//! `bougie make [task]` — walk a project recipe's DAG, applying
//! freshness. `bougie start` is a thin alias for `bougie make start`.
//! See RECIPES.md.

use crate::cli::OutputFormat;
use crate::commands::sync;
use crate::output::{emit, Render};
use crate::recipe::{
    builtin::{detect_from_text, load_builtin, BUILTINS},
    dag::Dag,
    merge_with_builtin, parse,
    run::current_bougie_dir,
    run_task, Recipe, RunOptions, TaskOutcome, TaskStatus,
};
use eyre::{eyre, Result, WrapErr};
use serde::Serialize;
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
            writeln!(w, "  {}", t)?;
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
            buf.push_str(&format!("[task.{}]\n", name));
            if !def.deps.is_empty() {
                buf.push_str(&format!(
                    "deps = [{}]\n",
                    def.deps
                        .iter()
                        .map(|d| format!("{:?}", d))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if let Some(c) = &def.creates {
                if c.len() == 1 {
                    buf.push_str(&format!("creates = {:?}\n", c[0]));
                } else {
                    buf.push_str(&format!(
                        "creates = [{}]\n",
                        c.iter().map(|p| format!("{:?}", p)).collect::<Vec<_>>().join(", ")
                    ));
                }
            }
            if let Some(c) = &def.check {
                buf.push_str(&format!("check = {:?}\n", c));
            }
            if let Some(r) = &def.run {
                buf.push_str(&format!("run = \"\"\"\n{r}\n\"\"\"\n"));
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
        sync::run(format, false)?;
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

    let run_opts = RunOptions {
        project_root,
        dry_run: opts.dry_run,
        explain: opts.explain,
        bougie_dir: current_bougie_dir(),
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
