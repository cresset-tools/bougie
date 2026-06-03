//! Freshness rules per RECIPES.md §3.

use super::dag::split_deps;
use super::parser::{Recipe, TaskDef};
use super::run::{build_sh, RunOptions};
use eyre::{Result, WrapErr};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::SystemTime;

/// Outcome of a freshness check: should this recipe run, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Run(String),
    Skip(String),
}

impl Verdict {
    pub fn should_run(&self) -> bool {
        matches!(self, Verdict::Run(_))
    }
    pub fn reason(&self) -> &str {
        match self {
            Verdict::Run(r) | Verdict::Skip(r) => r,
        }
    }
}

/// Per-task freshness state computed during the walk. Tracks each
/// task's effective mtime (the oldest `creates` mtime) and whether
/// `check` gated this task to clean.
#[derive(Debug, Clone, Default)]
pub struct WalkState {
    /// mtimes of named tasks' `creates` (oldest wins per §2 schema).
    pub task_mtime: HashMap<String, Option<SystemTime>>,
    /// Tasks whose `check` exited 0 — propagate-clean per §3.
    pub check_clean: HashMap<String, bool>,
    /// Tasks whose freshness verdict was Run. A phony (no-`creates`) dep
    /// always runs but records no mtime, so dirtiness can only reach its
    /// dependents through this set (make's phony-prerequisite rule).
    pub dirty: HashSet<String>,
}

/// Decide whether a task's recipe should run.
pub fn evaluate(
    recipe: &Recipe,
    _name: &str,
    task: &TaskDef,
    state: &WalkState,
    opts: &RunOptions,
) -> Result<Verdict> {
    let project_root = opts.project_root.as_path();
    if let Some(check) = &task.check {
        let status = run_check(check, opts)?;
        if status {
            return Ok(Verdict::Skip("check ✓ — skipping".to_string()));
        }
        return Ok(Verdict::Run("check failed → running".to_string()));
    }

    let Some(creates) = &task.creates else {
        return Ok(Verdict::Run("phony task — always runs".into()));
    };

    let oldest = oldest_mtime(creates, project_root)?;
    let Some(our_mtime) = oldest else {
        return Ok(Verdict::Run(format!(
            "{} missing → running",
            creates.first().map_or("<creates>", String::as_str)
        )));
    };

    let (named, files) = split_deps(recipe, &task.deps);

    for file in files {
        let p = project_root.join(file);
        match newest_mtime(&p)? {
            Some(t) if t > our_mtime => {
                return Ok(Verdict::Run(format!("{file} newer than {}", display_creates(creates))));
            }
            // None: a non-existent file dep is "infinitely old"; treat
            // as clean rather than re-running endlessly.
            _ => {}
        }
    }

    for dep_name in named {
        // `check`-gated deps don't propagate dirtiness (§3).
        if state.check_clean.get(dep_name).copied().unwrap_or(false) {
            continue;
        }
        // A dep that ran is dirty → this task must run too. This is the
        // only signal that catches a phony dep (no `creates`, so no
        // mtime to compare); it also covers a `creates` dep that was
        // regenerated this walk regardless of clock granularity.
        if state.dirty.contains(dep_name) {
            return Ok(Verdict::Run(format!(
                "dependency `{dep_name}` ran → running"
            )));
        }
        if let Some(Some(dep_mtime)) = state.task_mtime.get(dep_name)
            && *dep_mtime > our_mtime {
                return Ok(Verdict::Run(format!(
                    "task `{dep_name}` newer than {}",
                    display_creates(creates)
                )));
            }
    }

    Ok(Verdict::Skip(format!(
        "{} up to date — skipping",
        display_creates(creates)
    )))
}

/// After a successful run, touch every directory `creates` so its
/// mtime reflects "this recipe just finished" (POSIX doesn't update
/// directory mtime for nested changes).
pub fn touch_directories(creates: &[String], project_root: &Path) -> Result<()> {
    for c in creates {
        let p = project_root.join(c);
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            touch(&p)?;
        }
    }
    Ok(())
}

pub fn read_mtime_of_creates(
    creates: &[String],
    project_root: &Path,
) -> Result<Option<SystemTime>> {
    oldest_mtime(creates, project_root)
}

fn oldest_mtime(creates: &[String], project_root: &Path) -> Result<Option<SystemTime>> {
    let mut oldest: Option<SystemTime> = None;
    for c in creates {
        let p = project_root.join(c);
        match newest_mtime(&p)? {
            None => return Ok(None),
            Some(t) => {
                oldest = Some(match oldest {
                    Some(prev) if prev < t => prev,
                    _ => t,
                });
            }
        }
    }
    Ok(oldest)
}

fn newest_mtime(path: &Path) -> Result<Option<SystemTime>> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(eyre::Report::new(e)
                .wrap_err(format!("stat {}", path.display())));
        }
    };
    Ok(Some(meta.modified().wrap_err_with(|| {
        format!("reading mtime of {}", path.display())
    })?))
}

fn touch(path: &Path) -> Result<()> {
    use rustix::fs::{utimensat, AtFlags, Timestamps};
    use rustix::fs::CWD;
    let now = rustix::fs::Timespec {
        tv_sec: 0,
        tv_nsec: rustix::fs::UTIME_NOW,
    };
    let omit = rustix::fs::Timespec {
        tv_sec: 0,
        tv_nsec: rustix::fs::UTIME_OMIT,
    };
    let ts = Timestamps {
        last_access: omit,
        last_modification: now,
    };
    utimensat(CWD, path, &ts, AtFlags::empty())
        .wrap_err_with(|| format!("touching {}", path.display()))?;
    Ok(())
}

fn run_check(script: &str, opts: &RunOptions) -> Result<bool> {
    // Same env as a recipe `run` step (PATH-pinned bougie +
    // BOUGIE_SERVICE_*) so a `check` that shells out to `bougie …`
    // resolves it and reflects real state instead of always "failing"
    // with "bougie: not found" and re-running the task.
    let status = build_sh(script, opts)
        .status()
        .wrap_err("running check script")?;
    Ok(status.success())
}

fn display_creates(creates: &[String]) -> String {
    creates.join(", ")
}
