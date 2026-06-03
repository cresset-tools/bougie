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
        let verdict = evaluate(dag.recipe, name, task, &state, opts)?;
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
                    // Entered across the blocking `/bin/sh` step so a
                    // Ctrl-\ dump shows which recipe task is running when
                    // `bougie make`/`start` appears to hang.
                    let _task_span =
                        tracing::info_span!("recipe_task", task = name.as_str()).entered();
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
        // Propagate dirtiness: a task that ran (or would, in dry-run)
        // forces dependents to run, including phony deps that record no
        // mtime. Keyed off the verdict so dry-run/explain stay accurate.
        if verdict.should_run() {
            state.dirty.insert(name.clone());
        }
        let _ = ran;
    }

    Ok(outcomes)
}

fn execute(script: &str, opts: &RunOptions) -> Result<ExitStatus> {
    // NOTE: this `/bin/sh` runs in bougie's foreground process group and
    // has no SIGQUIT handler. A Ctrl-\ activity dump (see the bougie
    // crate's `debug_dump`) signals the whole group, so pressing it while
    // a recipe step is running kills this shell and surfaces as a bogus
    // `exit -1` for the step. It's a known interaction with the debug
    // dump, documented there — don't read a step's `exit -1` as a real
    // failure if a dump was fired mid-step.
    build_sh(script, opts)
        .status()
        .wrap_err("spawning /bin/sh for recipe step")
}

/// Build the `/bin/sh -e -c <script>` command shared by every recipe
/// step (`run`) *and* every freshness gate (`check`). Both must see the
/// same environment: bougie pinned on `PATH` (so a bare `bougie …`
/// resolves to *this* executable, never an installed one — see
/// [`pinned_bougie_dir`]) plus the `BOUGIE_SERVICE_*` injection. A
/// `check` that shells out to `bougie …` (e.g. `bougie run -- bin/magento
/// indexer:status`) would otherwise die with "bougie: not found", read as
/// a failed check, and re-run its task on every invocation.
pub(crate) fn build_sh(script: &str, opts: &RunOptions) -> std::process::Command {
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
    cmd
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

/// Build a directory to prepend to recipe children's `PATH` such that a
/// bare `bougie` resolves to *this* running executable — never some other
/// `bougie` that happens to be installed on `PATH`.
///
/// Recipes shell out to `bougie composer install`, `bougie up`,
/// `bougie run …` etc. If one of those resolved to a different-versioned
/// bougie, it would connect to (and forcibly restart) the daemon that
/// this process is driving, tearing its services down mid-recipe. So we
/// pin the executable explicitly: materialize a `bougie` symlink →
/// [`std::env::current_exe`] in a dedicated shim dir under the project's
/// `.bougie/state/` and return that dir. The symlink is refreshed each
/// run, so it always points at the bougie actually in use.
///
/// Falls back to the executable's own directory (the previous behavior)
/// if a symlink can't be created, so recipes still run in degraded
/// environments.
pub fn pinned_bougie_dir(project_root: &Path) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    #[cfg(unix)]
    {
        let dir = project_root.join(".bougie").join("state").join("bin");
        if std::fs::create_dir_all(&dir).is_ok() {
            let link = dir.join("bougie");
            // Refresh: a stale symlink (e.g. pointing at a since-replaced
            // binary) must not win. Remove then re-create.
            let _ = std::fs::remove_file(&link);
            if std::os::unix::fs::symlink(&exe, &link).is_ok() {
                return Some(dir);
            }
        }
    }
    exe.parent().map(Path::to_path_buf)
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

    #[cfg(unix)]
    #[test]
    fn check_script_resolves_bougie_via_pinned_path() {
        // Regression: a `check` that shells out to `bougie …` must see the
        // PATH-pinned bougie (like a `run` step does), not die with
        // "bougie: not found" and force the task to re-run every time.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // Fake `bougie` on the pinned dir: drops a sentinel and exits 0,
        // so a `check = "bougie"` passes only if PATH resolution worked.
        let bindir = root.join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let fake = bindir.join("bougie");
        std::fs::write(&fake, "#!/bin/sh\ntouch \"$(dirname \"$0\")/ran\"\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let r = parse(
            r#"
[task.gate]
check = "bougie"
run = "false"
"#,
        )
        .unwrap();
        let dag = Dag::build(&r, "gate").unwrap();
        let opts = RunOptions {
            project_root: root.clone(),
            dry_run: false,
            explain: false,
            bougie_dir: Some(bindir.clone()),
        };
        let out = run_task(&dag, &opts, |_| {}).unwrap();
        // check resolved `bougie` (sentinel written) and exited 0 → Skip.
        // Without the PATH pin the check would 127 and the task would Run
        // (its `run = "false"` would then fail the whole walk).
        assert!(bindir.join("ran").exists(), "pinned bougie should have run");
        assert_eq!(out[0].status, TaskStatus::Skipped);
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

    #[cfg(unix)]
    #[test]
    fn pinned_bougie_dir_symlinks_to_current_exe() {
        let dir = tempfile::tempdir().unwrap();
        let pinned = pinned_bougie_dir(dir.path()).expect("pinned dir");
        // Lives under the project's .bougie/state so it's ephemeral + gitignored.
        assert!(pinned.ends_with(".bougie/state/bin"), "{pinned:?}");
        let link = pinned.join("bougie");
        assert!(link.is_symlink(), "expected a `bougie` symlink at {link:?}");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::env::current_exe().unwrap(),
            "the shim must point at *this* executable"
        );
        // Refreshable: a second call leaves a valid link at the same dir.
        let again = pinned_bougie_dir(dir.path()).unwrap();
        assert_eq!(again, pinned);
        assert_eq!(
            std::fs::read_link(again.join("bougie")).unwrap(),
            std::env::current_exe().unwrap()
        );
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
