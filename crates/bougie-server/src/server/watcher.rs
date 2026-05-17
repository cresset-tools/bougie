//! Per-project filesystem watcher. Spec: SERVER.md §7.2.
//!
//! Watches each project's `.bougie/conf.d/`, `composer.json`, and
//! (if present) `bougie.toml`. Events are coalesced under a 250 ms
//! debounce window per (project, kind) so a flurry of editor saves
//! collapses into one reload.
//!
//! Two reload paths:
//!
//! - **conf.d touch** → SIGUSR2 the master via [`PoolManager::reload_project`].
//! - **composer.json / bougie.toml touch** → recompute resolved PHP
//!   version. If unchanged, treat as a conf.d-style reload. If
//!   changed, drop every pool for the project so the next request
//!   spawns afresh against the new install.
//!
//! Out of scope: dynamic add/remove of `[[host]]` blocks (would
//! require AppState re-build; phase 6's control socket).

use eyre::{Result, WrapErr};
use notify::{RecursiveMode, Watcher as _};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use super::pool::PoolManager;
use bougie_fs::state::read_project_resolved;

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ChangeKind {
    /// `<project>/.bougie/conf.d/*.ini` was touched. Reload (SIGUSR2)
    /// every variant for the project.
    ConfD,
    /// `<project>/composer.json` or `<project>/bougie.toml` was
    /// touched. Recompute resolved version; restart on change,
    /// reload otherwise.
    VersionInput,
}

#[derive(Debug)]
struct PendingEvent {
    project: PathBuf,
    kind: ChangeKind,
}

/// Spawn the watcher + dispatch loop. Returns a handle bundling both
/// the notify watcher (kept alive to keep events flowing) and the
/// dispatch JoinHandle (aborted at shutdown).
pub fn start(
    projects: &[PathBuf],
    pool_manager: &Arc<PoolManager>,
) -> Result<WatcherHandle> {
    // Set up the path → project resolution table once. The notify
    // callback runs on its own thread; we need a synchronously-shared
    // mapping it can consult to label events without doing IO.
    let path_to_project = build_path_map(projects);

    let (tx, mut rx) = mpsc::unbounded_channel::<PendingEvent>();

    let path_to_project_for_cb = path_to_project.clone();
    let tx_for_cb = tx.clone();
    let mut watcher: notify::RecommendedWatcher = notify::recommended_watcher(
        move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !is_relevant(&event.kind) {
                return;
            }
            for path in &event.paths {
                if let Some((project, kind)) = classify(&path_to_project_for_cb, path) {
                    let _ = tx_for_cb.send(PendingEvent { project, kind });
                }
            }
        },
    )
    .wrap_err("creating notify watcher")?;

    let mut watched: Vec<PathBuf> = Vec::new();
    for project in projects {
        // Both `.bougie/conf.d/` (regular extensions) and
        // `.bougie/conf.d-debug/` (xdebug et al.) feed pool variants;
        // either changing means the running pools need fresh symlinks.
        for sub in [
            bougie_installer::conf_d::project_confd_dir(project),
            bougie_installer::conf_d::project_confd_debug_dir(project),
            bougie_installer::conf_d::project_confd_local_dir(project),
        ] {
            if sub.is_dir() {
                if let Err(e) = watcher.watch(&sub, RecursiveMode::Recursive) {
                    eprintln!(
                        "bougie server: failed to watch {}: {e}",
                        sub.display()
                    );
                    continue;
                }
                watched.push(sub);
            }
        }
        // Watch the parent directory of composer.json/bougie.toml
        // rather than the files themselves: many editors do
        // write-and-rename, which would invalidate a file-target
        // watch. The dispatch loop filters by basename.
        if let Err(e) = watcher.watch(project, RecursiveMode::NonRecursive) {
            eprintln!(
                "bougie server: failed to watch {}: {e}",
                project.display()
            );
        } else {
            watched.push(project.clone());
        }
    }

    let manager_for_task = Arc::clone(pool_manager);
    let dispatch = tokio::spawn(async move {
        // Per-(project, kind) debounce timers. When a new event for
        // the same key arrives during the window, we just push the
        // deadline out — coalescing the burst into one reload.
        let mut pending: HashMap<(PathBuf, ChangeKind), tokio::time::Instant> = HashMap::new();

        loop {
            // Either pull a new event, or fire whichever debounce
            // deadline lands first.
            let next_deadline = pending.values().min().copied();
            tokio::select! {
                evt = rx.recv() => {
                    let Some(evt) = evt else { break };
                    let key = (evt.project, evt.kind);
                    pending.insert(key, tokio::time::Instant::now() + DEBOUNCE_WINDOW);
                }
                () = sleep_until_opt(next_deadline) => {
                    let now = tokio::time::Instant::now();
                    let due: Vec<(PathBuf, ChangeKind)> = pending
                        .iter()
                        .filter(|(_, when)| **when <= now)
                        .map(|(k, _)| k.clone())
                        .collect();
                    for key in due {
                        pending.remove(&key);
                        let (project, kind) = key;
                        if let Err(e) = apply(&manager_for_task, &project, kind).await {
                            eprintln!(
                                "bougie server: reload failed for {} ({kind:?}): {e:#}",
                                project.display(),
                            );
                        }
                    }
                }
            }
        }
    });

    Ok(WatcherHandle { _watcher: watcher, _watched: watched, dispatch })
}

/// Decide whether a `notify` event represents an actual filesystem
/// change we need to react to. Access events (open, read, close-nowrite,
/// metadata-only) are filtered out — they don't change what php-fpm
/// would load, AND `build_variant_confd`'s own `read_dir` of the
/// watched directory triggers them, which would set up an infinite
/// reload loop:
///
///   request → spawn → read_dir → IN_OPEN → debounce → reload →
///   read_dir → IN_OPEN → debounce → reload → …
///
/// Only Create / Modify / Remove imply a real change to the conf.d
/// or composer.json contents.
fn is_relevant(kind: &notify::EventKind) -> bool {
    use notify::EventKind;
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Future that resolves at the given deadline, or never if `None`.
/// Used in `select!` so the dispatch loop blocks on the next event
/// when there's nothing pending.
async fn sleep_until_opt(when: Option<tokio::time::Instant>) {
    match when {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending::<()>().await,
    }
}

/// Per-project notify roots indexed for O(1) lookup. Each entry maps
/// a *prefix* path back to the canonical project root + the kind of
/// event a hit under that prefix represents.
type PathMap = Vec<(PathBuf, PathBuf, ChangeKind)>;

fn build_path_map(projects: &[PathBuf]) -> PathMap {
    let mut out = PathMap::new();
    for project in projects {
        out.push((bougie_installer::conf_d::project_confd_dir(project), project.clone(), ChangeKind::ConfD));
        out.push((bougie_installer::conf_d::project_confd_debug_dir(project), project.clone(), ChangeKind::ConfD));
        out.push((bougie_installer::conf_d::project_confd_local_dir(project), project.clone(), ChangeKind::ConfD));
        // Composer + bougie.toml share the version-input kind; we
        // distinguish their basenames inside `classify` so unrelated
        // files in the project root (README.md, src/, etc.) don't
        // trigger reloads.
        out.push((project.clone(), project.clone(), ChangeKind::VersionInput));
    }
    out
}

/// Map a notify event path to a `(project, kind)` pair if it's one we
/// care about. Returns `None` for noise (unrelated files in the
/// project root, hidden tempfiles, etc.).
fn classify(map: &PathMap, path: &Path) -> Option<(PathBuf, ChangeKind)> {
    // Walk longest-prefix first so `<project>/.bougie/conf.d/` wins
    // over `<project>/` when both match.
    let mut candidates: Vec<&(PathBuf, PathBuf, ChangeKind)> = map
        .iter()
        .filter(|(prefix, _, _)| path.starts_with(prefix))
        .collect();
    candidates.sort_by_key(|(p, _, _)| std::cmp::Reverse(p.as_os_str().len()));
    let (_, project, kind) = candidates.first()?;
    match kind {
        ChangeKind::ConfD => Some((project.clone(), ChangeKind::ConfD)),
        ChangeKind::VersionInput => {
            let basename = path.file_name()?.to_str()?;
            if basename == "composer.json" || basename == "bougie.toml" {
                Some((project.clone(), ChangeKind::VersionInput))
            } else {
                None
            }
        }
    }
}

async fn apply(
    manager: &Arc<PoolManager>,
    project: &Path,
    kind: ChangeKind,
) -> Result<()> {
    match kind {
        ChangeKind::ConfD => {
            let count = manager.reload_project(project).await?;
            if count > 0 {
                eprintln!(
                    "[pool_reload] project={} variants={count} reason=conf.d",
                    project.display()
                );
            }
        }
        ChangeKind::VersionInput => {
            // Recompute resolved. If the file disappeared mid-flight
            // (atomic-rename mid-write), the read fails — fall back
            // to a reload, which is safe either way.
            let new_resolved = read_project_resolved(project).ok();
            // Compare against any one of the pools' resolved version
            // — they all share it; if pools is empty, treat as reload.
            let pids = manager.pids().await;
            let existing = pids
                .iter()
                .find(|(k, _)| k.project == project)
                .map(|(k, _)| (k.version.clone(), k.flavor.clone()));
            let version_changed = match (&new_resolved, &existing) {
                (Some((nv, nf)), Some((ov, of))) => (nv, nf) != (ov, of),
                (None, _) | (_, None) => false,
            };
            if version_changed {
                let count = manager.restart_project(project).await;
                eprintln!(
                    "[pool_restart] project={} variants={count} reason=version-change",
                    project.display()
                );
            } else {
                let count = manager.reload_project(project).await.unwrap_or(0);
                if count > 0 {
                    eprintln!(
                        "[pool_reload] project={} variants={count} reason=composer-or-bougie-toml",
                        project.display()
                    );
                }
            }
        }
    }
    Ok(())
}

/// Returned from [`start`]; keep alive as long as the watcher should
/// run. `Drop` aborts the dispatch task and lets the underlying notify
/// watcher tear itself down.
#[derive(Debug)]
pub struct WatcherHandle {
    _watcher: notify::RecommendedWatcher,
    _watched: Vec<PathBuf>,
    dispatch: tokio::task::JoinHandle<()>,
}

impl WatcherHandle {
    pub fn abort(&self) {
        self.dispatch.abort();
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.dispatch.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_routes_confd_files() {
        let project = PathBuf::from("/p/myapp");
        let map = build_path_map(std::slice::from_ref(&project));
        let hit = classify(&map, &project.join(".bougie/conf.d/20-redis.ini"));
        assert_eq!(hit, Some((project, ChangeKind::ConfD)));
    }

    #[test]
    fn classify_routes_composer_json() {
        let project = PathBuf::from("/p/myapp");
        let map = build_path_map(std::slice::from_ref(&project));
        let hit = classify(&map, &project.join("composer.json"));
        assert_eq!(hit, Some((project, ChangeKind::VersionInput)));
    }

    #[test]
    fn classify_routes_bougie_toml() {
        let project = PathBuf::from("/p/myapp");
        let map = build_path_map(std::slice::from_ref(&project));
        let hit = classify(&map, &project.join("bougie.toml"));
        assert_eq!(hit, Some((project, ChangeKind::VersionInput)));
    }

    #[test]
    fn classify_ignores_unrelated_project_files() {
        let project = PathBuf::from("/p/myapp");
        let map = build_path_map(std::slice::from_ref(&project));
        assert!(classify(&map, Path::new("/p/myapp/README.md")).is_none());
        assert!(classify(&map, Path::new("/p/myapp/src/app.php")).is_none());
    }

    #[test]
    fn is_relevant_filters_access_events() {
        use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind};
        use notify::EventKind;
        // Access events come from us reading the directory — must not
        // trigger reloads.
        assert!(!is_relevant(&EventKind::Access(AccessKind::Open(AccessMode::Read))));
        assert!(!is_relevant(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
        assert!(!is_relevant(&EventKind::Access(AccessKind::Any)));
        // Real changes do.
        assert!(is_relevant(&EventKind::Create(CreateKind::File)));
        assert!(is_relevant(&EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any))));
        assert!(is_relevant(&EventKind::Remove(RemoveKind::File)));
        // Catch-all kinds we don't care about.
        assert!(!is_relevant(&EventKind::Any));
        assert!(!is_relevant(&EventKind::Other));
    }

    #[test]
    fn classify_distinguishes_confd_from_root() {
        let project = PathBuf::from("/p/myapp");
        let map = build_path_map(std::slice::from_ref(&project));
        // Longest-prefix wins: a path under conf.d is ConfD, not
        // VersionInput (which would also match the project-root
        // prefix).
        let hit = classify(&map, &project.join(".bougie/conf.d/30-xdebug.ini"));
        assert_eq!(hit.map(|(_, k)| k), Some(ChangeKind::ConfD));
    }
}
