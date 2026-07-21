//! `bougie make [task]` — walk a project recipe's DAG, applying
//! freshness. A bare `bougie make` lists the available tasks;
//! `bougie start` runs the `start` task (the project umbrella).
//! See RECIPES.md.

use bougie_cli::OutputFormat;
use crate::commands::sync;
use bougie_output::output::{emit, Render};
use bougie_recipe::{
    builtin::{detect_from_text, load_builtin, BUILTINS},
    dag::Dag,
    merge_with_builtin, parse,
    run::pinned_bougie_dir,
    run_task, Recipe, RunOptions, TaskOutcome, TaskStatus,
};
use eyre::{eyre, Result, WrapErr};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Default, Clone)]
pub struct MakeOptions {
    pub task: Option<String>,
    pub list: bool,
    pub dry_run: bool,
    pub explain: bool,
    pub no_sync: bool,
    pub no_builtin: bool,
    pub no_team: bool,
    pub recipe: Option<String>,
    pub print: bool,
}

/// Which layer a merged task's effective definition came from — shown in
/// `--list` so a dev sees which `test` they're about to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskSource {
    Builtin,
    Team,
    Local,
}

impl TaskSource {
    fn label(self) -> &'static str {
        match self {
            TaskSource::Builtin => "built-in",
            TaskSource::Team => "team",
            TaskSource::Local => "local",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TaskListing {
    pub name: String,
    pub source: TaskSource,
}

#[derive(Debug, Serialize)]
pub struct MakeResult {
    pub schema_version: u32,
    pub recipe: String,
    pub task: String,
    pub steps: Vec<StepResult>,
}

#[derive(Debug, Serialize)]
pub struct StepResult {
    pub name: String,
    pub status: &'static str,
    pub reason: String,
}

impl Render for MakeResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "recipe `{}` task `{}` — {} step(s)",
            self.recipe,
            self.task,
            self.steps.len()
        )?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub schema_version: u32,
    pub recipe: String,
    pub tasks: Vec<TaskListing>,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "recipe: {}", self.recipe)?;
        let width = self.tasks.iter().map(|t| t.name.len()).max().unwrap_or(0);
        for t in &self.tasks {
            writeln!(w, "  {:<width$}  ({})", t.name, t.source.label())?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct PrintResult {
    pub schema_version: u32,
    pub recipe: String,
    pub toml: String,
}

impl Render for PrintResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        w.write_all(self.toml.as_bytes())
    }
}

pub fn run(format: OutputFormat, opts: MakeOptions) -> Result<ExitCode> {
    let project_root = std::env::current_dir().wrap_err("getting current directory")?;
    let task_opt = opts.task.clone();

    let (recipe_name, recipe, sources) = load_merged_recipe(&project_root, &opts)?;

    // A bare `bougie make` lists the available tasks (like `just`); to
    // bring the whole project up use `bougie start`. `--list` forces the
    // same listing even with a task named. Each is tagged with its origin
    // layer (built-in / team / local) so a dev sees which one wins.
    if opts.list || (task_opt.is_none() && !opts.print) {
        let tasks: Vec<TaskListing> = recipe
            .tasks
            .keys()
            .map(|name| TaskListing {
                name: name.clone(),
                source: sources.get(name).copied().unwrap_or(TaskSource::Builtin),
            })
            .collect();
        emit(
            format,
            &ListResult { schema_version: 1, recipe: recipe_name, tasks },
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    if opts.print {
        let mut buf = String::new();
        for (name, def) in &recipe.tasks {
            writeln!(buf, "[task.{name}]").expect("writing to String");
            if !def.deps.is_empty() {
                writeln!(
                    buf,
                    "deps = [{}]",
                    def.deps
                        .iter()
                        .map(|d| format!("{d:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .expect("writing to String");
            }
            if let Some(c) = &def.creates {
                if c.len() == 1 {
                    writeln!(buf, "creates = {:?}", c[0]).expect("writing to String");
                } else {
                    writeln!(
                        buf,
                        "creates = [{}]",
                        c.iter().map(|p| format!("{p:?}")).collect::<Vec<_>>().join(", ")
                    )
                    .expect("writing to String");
                }
            }
            if let Some(c) = &def.check {
                writeln!(buf, "check = {c:?}").expect("writing to String");
            }
            if let Some(r) = &def.run {
                writeln!(buf, "run = \"\"\"\n{r}\n\"\"\"").expect("writing to String");
            }
            buf.push('\n');
        }
        emit(
            format,
            &PrintResult { schema_version: 1, recipe: recipe_name, toml: buf },
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    // After the list/print early returns a task is guaranteed present; the
    // `unwrap_or_default` branch is unreachable (and avoids a panic path).
    let task_name = task_opt.unwrap_or_default();

    // Sync prologue (RECIPES.md §5).
    if !opts.no_sync && !opts.dry_run && !opts.explain {
        sync::run(
            &project_root,
            format,
            false,
            false,
            None,
            None,
            bougie_cli::PhpPrefArgs::default(),
            bougie_composer_resolver::ResolutionStrategy::Highest,
            bougie_composer_resolver::PlatformIgnore::default(),
        )?;
    } else if opts.dry_run || opts.explain {
        eprintln!(
            "[sync]     {}",
            if opts.no_sync {
                "skipped (--no-sync)"
            } else {
                "would run `bougie sync`"
            }
        );
    }

    let dag = Dag::build(&recipe, &task_name)
        .map_err(|e| eyre!("recipe error: {e}"))?;

    // Pin recipe `bougie …` invocations to *this* executable so a recipe
    // never shells out to a different-versioned bougie (which would
    // restart the daemon it's driving and tear its services down).
    let bougie_dir = pinned_bougie_dir(&project_root);
    let run_opts = RunOptions {
        project_root,
        dry_run: opts.dry_run,
        explain: opts.explain,
        bougie_dir,
    };

    let mut steps: Vec<StepResult> = Vec::new();
    let outcomes = run_task(&dag, &run_opts, |outcome: &TaskOutcome| {
        let tag = match outcome.status {
            TaskStatus::Ran => "ran",
            TaskStatus::Skipped => "skip",
            TaskStatus::Failed => "fail",
        };
        eprintln!("[{:8}] {} — {}", outcome.name, tag, outcome.reason);
    })?;

    for o in outcomes {
        steps.push(StepResult {
            name: o.name,
            status: match o.status {
                TaskStatus::Ran => "ran",
                TaskStatus::Skipped => "skipped",
                TaskStatus::Failed => "failed",
            },
            reason: o.reason,
        });
    }

    if task_name == "start" && !opts.dry_run && !opts.explain {
        print_start_hints(&recipe_name, &run_opts.project_root);
    }

    emit(
        format,
        &MakeResult {
            schema_version: 1,
            recipe: recipe_name,
            task: task_name,
            steps,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Print a friendly "ready" banner after `bougie start` (i.e. the
/// `start` task) completes. Pulls live service env from the daemon so
/// the URL reflects the actual allocated hostname/port; recipe-specific
/// extras (Magento admin credentials) are added based on the resolved
/// recipe name. Best-effort: silent when the daemon isn't reachable or
/// the server tenant hasn't been provisioned.
fn print_start_hints(recipe_name: &str, project_root: &std::path::Path) {
    let env: std::collections::BTreeMap<String, String> =
        crate::commands::service::recipe_env_for_project(project_root)
            .into_iter()
            .collect();
    let hostname = env.get("BOUGIE_SERVICE_SERVER_HOSTNAME").cloned();
    let port = env.get("BOUGIE_SERVICE_SERVER_PORT").cloned();
    let url = match (&hostname, &port) {
        (Some(h), Some(p)) => Some(format!("http://{h}:{p}/")),
        _ => env.get("BOUGIE_SERVICE_SERVER_URL").cloned(),
    };
    if url.is_none() && recipe_name != "magento" {
        return;
    }

    eprintln!();
    eprintln!("Ready.");
    if let Some(u) = &url {
        eprintln!("  URL:      {u}");
    }
    if recipe_name == "magento" {
        if let (Some(u), Some(front)) = (&url, read_magento_backend_frontname(project_root)) {
            eprintln!("  Admin:    {u}{front}");
        }
        eprintln!("  User:     admin");
        eprintln!("  Password: admin123");
    }
}

/// Best-effort extract of `backend.frontName` from `app/etc/env.php`.
/// The file is generated by Magento's `setup:install`, which writes the
/// array with PHP's var_export-ish style; a regex over the literal is
/// robust enough for the post-install banner. Returns None when the
/// file is missing or the key isn't found.
fn read_magento_backend_frontname(project_root: &std::path::Path) -> Option<String> {
    let env_php = project_root.join("app/etc/env.php");
    let text = std::fs::read_to_string(&env_php).ok()?;
    let (_, after) = text.split_once("'backend'")?;
    let (_, after) = after.split_once("'frontName'")?;
    let (_, after) = after.split_once("=>")?;
    let after = after.trim_start();
    let quote = after.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let rest = &after[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Read a recipe name pinned in composer.json `extra.bougie.recipe`.
/// Written by `bougie init --starter` from the manifest's `recipe` field so a
/// producer can name the recipe explicitly instead of relying on detection.
fn configured_recipe(composer_text: Option<&str>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(composer_text?).ok()?;
    v.get("extra")?
        .get("bougie")?
        .get("recipe")?
        .as_str()
        .map(str::to_string)
}

/// Convert a manifest-sourced team task into a `bougie-recipe` `TaskDef`. sconce
/// serves `deps`/`creates` as arrays; an empty `creates` maps to `None` so the
/// task isn't freshness-gated on nothing.
fn team_task_to_def(task: crate::commands::team::ManifestRecipeTask) -> bougie_recipe::TaskDef {
    bougie_recipe::TaskDef {
        deps: task.deps,
        creates: if task.creates.is_empty() {
            None
        } else {
            Some(task.creates)
        },
        check: task.check,
        run: task.run,
    }
}

/// Resolve the effective recipe per RECIPES.md §4: pick a builtin by
/// honouring `--recipe <name>`, then a pinned `extra.bougie.recipe`, then
/// sniffing composer.json — then fold the team's manifest tasks over the
/// builtin, and the project's `bougie.toml` tasks over that. Precedence is
/// **built-in < team < local**: a team task overrides the framework default,
/// but a project's own same-named task still wins. `--no-builtin` /
/// `--no-team` drop a layer. Returns the merged recipe plus, per task, which
/// layer its winning definition came from (for `--list`).
fn load_merged_recipe(
    project_root: &PathBuf,
    opts: &MakeOptions,
) -> Result<(String, Recipe, BTreeMap<String, TaskSource>)> {
    let composer_path = project_root.join("composer.json");
    let composer_text = if composer_path.exists() {
        Some(std::fs::read_to_string(&composer_path).wrap_err_with(|| {
            format!("reading {}", composer_path.display())
        })?)
    } else {
        None
    };

    // Recipe precedence: explicit `--recipe` flag > a recipe pinned in the
    // project's composer.json `extra.bougie.recipe` (e.g. written by
    // `--starter` from the manifest's `recipe` field) > composer.json
    // auto-detection.
    let chosen = match &opts.recipe {
        Some(name) => name.clone(),
        None => configured_recipe(composer_text.as_deref())
            .unwrap_or_else(|| detect_from_text(composer_text.as_deref()).to_string()),
    };
    if !BUILTINS.iter().any(|(n, _)| *n == chosen) {
        return Err(eyre!(
            "unknown builtin recipe `{chosen}`. Available: {}",
            BUILTINS
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let builtin = if opts.no_builtin {
        Recipe::default()
    } else {
        load_builtin(&chosen).expect("detected recipe should resolve to a builtin")
    };

    // Team-shared tasks from the cached manifest (empty for a non-team project
    // or with `--no-team`). Trusted like the private packages / seed recipe the
    // team already runs — `--list`/`--print` surface them for visibility.
    let team = if opts.no_team {
        Recipe::default()
    } else {
        Recipe {
            tasks: crate::commands::team::cached_recipes(project_root)
                .into_iter()
                .map(|(name, task)| (name, team_task_to_def(task)))
                .collect(),
        }
    };

    let bougie_toml = project_root.join("bougie.toml");
    let local = if bougie_toml.exists() {
        let text = std::fs::read_to_string(&bougie_toml)
            .wrap_err_with(|| format!("reading {}", bougie_toml.display()))?;
        parse(&text)?
    } else {
        Recipe::default()
    };

    // Provenance: the highest layer defining a task is the one that wins, so
    // record built-in, then overwrite with team, then local.
    let mut sources = BTreeMap::new();
    for name in builtin.tasks.keys() {
        sources.insert(name.clone(), TaskSource::Builtin);
    }
    for name in team.tasks.keys() {
        sources.insert(name.clone(), TaskSource::Team);
    }
    for name in local.tasks.keys() {
        sources.insert(name.clone(), TaskSource::Local);
    }

    // built-in < team < local (later merge wins per task name).
    let merged = merge_with_builtin(merge_with_builtin(builtin, team), local);
    if merged.tasks.is_empty() {
        return Err(eyre!(
            "no tasks defined: --no-builtin was set and neither the team manifest nor \
             bougie.toml has any [task.*] tables"
        ));
    }
    Ok((chosen, merged, sources))
}

#[cfg(test)]
mod tests {
    use super::{configured_recipe, team_task_to_def};
    use crate::commands::team::ManifestRecipeTask;

    #[test]
    fn configured_recipe_reads_extra_bougie_recipe() {
        let json = r#"{"extra":{"bougie":{"recipe":"magento"}}}"#;
        assert_eq!(configured_recipe(Some(json)).as_deref(), Some("magento"));
    }

    #[test]
    fn configured_recipe_absent_when_unset() {
        assert_eq!(configured_recipe(Some(r#"{"extra":{"bougie":{}}}"#)), None);
        assert_eq!(configured_recipe(Some("{}")), None);
        assert_eq!(configured_recipe(None), None);
    }

    #[test]
    fn team_task_maps_fields_and_empty_creates_to_none() {
        // A leaf command task: empty creates → no freshness gate (None).
        let leaf = team_task_to_def(ManifestRecipeTask {
            run: Some("phpunit".into()),
            check: None,
            deps: vec![],
            creates: vec![],
        });
        assert_eq!(leaf.run.as_deref(), Some("phpunit"));
        assert_eq!(leaf.creates, None);
        assert!(leaf.deps.is_empty());

        // deps + check + creates pass through; non-empty creates → Some.
        let full = team_task_to_def(ManifestRecipeTask {
            run: Some("build".into()),
            check: Some("test -f x".into()),
            deps: vec!["vendor".into(), "services".into()],
            creates: vec!["build/out".into()],
        });
        assert_eq!(full.deps, vec!["vendor", "services"]);
        assert_eq!(full.check.as_deref(), Some("test -f x"));
        assert_eq!(full.creates, Some(vec!["build/out".to_string()]));
    }
}
