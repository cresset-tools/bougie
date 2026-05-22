//! Recipe execution loop: walk the DAG, evaluate freshness, run.

use super::dag::Dag;
use super::freshness::{evaluate, read_mtime_of_creates, touch_directories, Verdict, WalkState};
use eyre::{eyre, Result, WrapErr};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Ran,
    Skipped,
    Failed,
}

#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub name: String,
    pub status: TaskStatus,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub project_root: PathBuf,
    pub dry_run: bool,
    pub explain: bool,
    /// Directory prepended to `PATH` for every child process. Set
    /// to the directory of `bougie`'s own executable so recipes can
    /// invoke `bougie …` without assuming an installed location.
    pub bougie_dir: Option<PathBuf>,
}

/// Walk `dag.order`, evaluate freshness, execute or skip each task.
/// Stops on the first failure.
pub fn run_task(
    dag: &Dag<'_>,
    opts: &RunOptions,
    mut emit: impl FnMut(&TaskOutcome),
) -> Result<Vec<TaskOutcome>> {
    let mut state = WalkState::default();
    let mut outcomes = Vec::new();

    for name in &dag.order {
        let task = &dag.recipe.tasks[name];
        let verdict = evaluate(dag.recipe, name, task, &state, &opts.project_root)?;
        let ran = match &verdict {
            Verdict::Skip(reason) => {
                let outcome = TaskOutcome {
                    name: name.clone(),
                    status: TaskStatus::Skipped,
                    reason: reason.clone(),
                };
                emit(&outcome);
                outcomes.push(outcome);
                // §3: check-clean propagates non-dirtiness — record
                // it so downstream tasks ignore us in their compare.
                if task.check.is_some() {
                    state.check_clean.insert(name.clone(), true);
                }
                false
            }
            Verdict::Run(reason) => {
                if opts.dry_run || opts.explain {
                    let outcome = TaskOutcome {
                        name: name.clone(),
                        status: TaskStatus::Ran,
                        reason: format!("would run — {reason}"),
                    };
                    emit(&outcome);
                    outcomes.push(outcome);
                    false
                } else {
                    let Some(script) = task.run.as_deref() else {
                        return Err(eyre!(
                            "task `{name}` needs to run but has no `run` script"
                        ));
                    };
                    match execute(script, opts) {
                        Ok(status) if status.success() => {
                            if let Some(creates) = &task.creates {
                                touch_directories(creates, &opts.project_root)?;
                            }
                            let outcome = TaskOutcome {
                                name: name.clone(),
                                status: TaskStatus::Ran,
                                reason: reason.clone(),
                            };
                            emit(&outcome);
                            outcomes.push(outcome);
                            true
                        }
                        Ok(status) => {
                            let outcome = TaskOutcome {
                                name: name.clone(),
                                status: TaskStatus::Failed,
                                reason: format!("exit {}", status.code().unwrap_or(-1)),
                            };
                            emit(&outcome);
                            outcomes.push(outcome.clone());
                            return Err(eyre!(
                                "task `{name}` failed: exit {}",
                                status.code().unwrap_or(-1)
                            ));
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        };

        // Record this task's effective mtime (oldest `creates`) so
        // downstream tasks can compare against it.
        if let Some(creates) = &task.creates {
            let m = read_mtime_of_creates(creates, &opts.project_root)?;
            state.task_mtime.insert(name.clone(), m);
        }
        let _ = ran;
    }

    Ok(outcomes)
}

fn execute(script: &str, opts: &RunOptions) -> Result<ExitStatus> {
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-e").arg("-c").arg(script);
    cmd.current_dir(&opts.project_root);
    if let Some(dir) = &opts.bougie_dir {
        let prev = std::env::var("PATH").unwrap_or_default();
        let joined = if prev.is_empty() {
            dir.display().to_string()
        } else {
            format!("{}:{prev}", dir.display())
        };
        cmd.env("PATH", joined);
    }
    // Inject `BOUGIE_SERVICE_*` env so recipes can write
    // `--db-host="$BOUGIE_SERVICE_MARIADB_SOCKET"` etc. (RECIPES.md
    // §2). Mirrors `bougie run`'s tenant-env handoff: best-effort
    // against a running daemon; silent when bougied is down (failing
    // recipe steps surface the real issue more clearly than a
    // wrapper error here would).
    for (k, v) in fetch_service_env_for_project(&opts.project_root) {
        cmd.env(k, v);
    }
    cmd.status().wrap_err("spawning /bin/sh for recipe step")
}

fn fetch_service_env_for_project(project: &std::path::Path) -> Vec<(String, String)> {
    // The IPC client to `bougied` shares wire types with the daemon
    // and lives in the top-level bougie crate. To keep the recipe
    // crate free of that dep, the bougie binary registers a provider
    // via `bougie_recipe::set_service_env_provider` at startup. When
    // unset (e.g. in unit tests), recipes still run — just without
    // the BOUGIE_SERVICE_* env injection.
    crate::service_env_provider()
        .map(|f| f(project))
        .unwrap_or_default()
}

/// Resolve the directory containing the running `bougie` binary, so
/// child processes can `exec bougie …` even when bougie isn't on PATH.
pub fn current_bougie_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn runs_phony_task_and_skips_when_creates_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let r = parse(r#"
[task.touch]
creates = "marker"
run = "touch marker"

[task.start]
deps = ["touch"]
run = "echo start"
"#)
        .unwrap();
        let dag = Dag::build(&r, "start").unwrap();
        let opts = RunOptions {
            project_root: root.clone(),
            dry_run: false,
            explain: false,
            bougie_dir: None,
        };
        let mut events = Vec::new();
        let out = run_task(&dag, &opts, |o| events.push(o.clone())).unwrap();
        let touch = out.iter().find(|o| o.name == "touch").unwrap();
        assert_eq!(touch.status, TaskStatus::Ran);
        assert!(root.join("marker").exists());

        // Second pass: `marker` exists and has no older deps → skip.
        let out2 = run_task(&dag, &opts, |_| {}).unwrap();
        let touch2 = out2.iter().find(|o| o.name == "touch").unwrap();
        assert_eq!(touch2.status, TaskStatus::Skipped);
    }

    #[test]
    fn check_gated_task_skips_and_does_not_propagate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("dep"), "x").unwrap();
        let r = parse(
            r#"
[task.a]
deps = ["dep"]
check = "true"
run = "false"

[task.b]
creates = "b.out"
deps = ["a"]
run = "touch b.out"
"#,
        )
        .unwrap();
        // Pre-create b.out so its mtime is fresh.
        std::fs::write(root.join("b.out"), "x").unwrap();
        let dag = Dag::build(&r, "b").unwrap();
        let opts = RunOptions {
            project_root: root,
            dry_run: false,
            explain: false,
            bougie_dir: None,
        };
        let out = run_task(&dag, &opts, |_| {}).unwrap();
        // `a` skips (check ✓). `b` should also skip because a's
        // check-clean status means it doesn't bump downstream dirtiness.
        assert_eq!(out[0].status, TaskStatus::Skipped);
        assert_eq!(out[1].status, TaskStatus::Skipped);
    }

    #[test]
    fn failed_task_stops_walk() {
        let dir = tempfile::tempdir().unwrap();
        let r = parse(
            r#"
[task.a]
run = "false"

[task.b]
deps = ["a"]
run = "echo never"
"#,
        )
        .unwrap();
        let dag = Dag::build(&r, "b").unwrap();
        let opts = RunOptions {
            project_root: dir.path().to_path_buf(),
            dry_run: false,
            explain: false,
            bougie_dir: None,
        };
        let err = run_task(&dag, &opts, |_| {}).unwrap_err();
        assert!(err.to_string().contains("`a` failed"));
    }
}
