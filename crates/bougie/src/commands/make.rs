//! `bougie make [task]` — walk a project recipe's DAG, applying
//! freshness. `bougie start` is a thin alias for `bougie make start`.
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
    pub recipe: Option<String>,
    pub print: bool,
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
    pub tasks: Vec<String>,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "recipe: {}", self.recipe)?;
        for t in &self.tasks {
            writeln!(w, "  {t}")?;
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
    let task_name = opts.task.clone().unwrap_or_else(|| "start".into());

    let (recipe_name, recipe) = load_merged_recipe(&project_root, &opts)?;

    if opts.list {
        let mut tasks: Vec<String> = recipe.tasks.keys().cloned().collect();
        tasks.sort();
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

    // Sync prologue (RECIPES.md §5).
    if !opts.no_sync && !opts.dry_run && !opts.explain {
        sync::run(format, false, false)?;
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
        crate::commands::services::recipe_env_for_project(project_root)
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

/// Resolve the effective recipe per RECIPES.md §4: pick a builtin by
/// sniffing composer.json (or honour `--recipe <name>`), then merge
/// the project's `bougie.toml` recipe tables over it (or skip the
/// builtin entirely with `--no-builtin`).
fn load_merged_recipe(
    project_root: &PathBuf,
    opts: &MakeOptions,
) -> Result<(String, Recipe)> {
    let composer_path = project_root.join("composer.json");
    let composer_text = if composer_path.exists() {
        Some(std::fs::read_to_string(&composer_path).wrap_err_with(|| {
            format!("reading {}", composer_path.display())
        })?)
    } else {
        None
    };

    let chosen = match &opts.recipe {
        Some(name) => {
            if !BUILTINS.iter().any(|(n, _)| n == name) {
                return Err(eyre!(
                    "unknown builtin recipe `{name}`. Available: {}",
                    BUILTINS
                        .iter()
                        .map(|(n, _)| *n)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            name.clone()
        }
        None => detect_from_text(composer_text.as_deref()).to_string(),
    };

    let builtin = if opts.no_builtin {
        Recipe::default()
    } else {
        load_builtin(&chosen).expect("detected recipe should resolve to a builtin")
    };

    let bougie_toml = project_root.join("bougie.toml");
    let local = if bougie_toml.exists() {
        let text = std::fs::read_to_string(&bougie_toml)
            .wrap_err_with(|| format!("reading {}", bougie_toml.display()))?;
        parse(&text)?
    } else {
        Recipe::default()
    };

    let merged = merge_with_builtin(builtin, local);
    if merged.tasks.is_empty() {
        return Err(eyre!(
            "no tasks defined: --no-builtin was set and bougie.toml has no [task.*] tables"
        ));
    }
    Ok((chosen, merged))
}
