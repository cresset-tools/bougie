//! Per-project filesystem watcher. Spec: SERVER.md §7.2.
//!
//! Watches each project's `.bougie/conf.d/`, `composer.json`,
//! `bougie.toml`, `composer.lock`, and the per-project autoload
//! scan-roots the [`AutoloaderManager`] arms dynamically. Events are
//! coalesced under a per-(project, kind) debounce window so a flurry
//! of editor saves collapses into one action.
//!
//! Three reload paths:
//!
//! - **conf.d touch** → SIGUSR2 the master via [`PoolManager::reload_project`].
//! - **composer.json / bougie.toml touch** → recompute resolved PHP
//!   version. If unchanged, treat as a conf.d-style reload. If
//!   changed, drop every pool for the project so the next request
//!   spawns afresh against the new install.
//! - **composer.lock touch** or **user-code save** → routed to
//!   [`AutoloaderManager`] so the in-memory classmap stays in sync
//!   with the on-disk source tree. See
//!   `INCREMENTAL_AUTOLOADER_PLAN.md`.

use eyre::{Result, WrapErr};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use super::autoloader_manager::AutoloaderManager;
use super::pool::PoolManager;
use super::watch_registry::{PathMap, WatchRegistry};
use bougie_fs::state::read_project_resolved;

const DEBOUNCE_CONFD: Duration = Duration::from_millis(250);
const DEBOUNCE_VERSION_INPUT: Duration = Duration::from_millis(250);
const DEBOUNCE_LOCKFILE: Duration = Duration::from_millis(250);
/// Devs notice longer than ~50 ms between save and reload — pick a
/// window short enough to feel instant but long enough to coalesce
/// an editor's write-and-rename flurry.
const DEBOUNCE_USER_CODE: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ChangeKind {
    /// `<project>/.bougie/conf.d/*.ini` was touched. Reload (SIGUSR2)
    /// every variant for the project.
    ConfD,
    /// `<project>/composer.json` or `<project>/bougie.toml` was
    /// touched. Recompute resolved version; restart on change,
    /// reload otherwise.
    VersionInput,
    /// `<project>/composer.lock` was touched. Route to
    /// [`AutoloaderManager::handle_lockfile`] so the in-memory model
    /// re-bootstraps against the new dependency set.
    Lockfile,
}

#[derive(Debug, Clone)]
enum PendingEvent {
    Touch { project: PathBuf, kind: ChangeKind },
    UserCodeChange { project: PathBuf, path: PathBuf, deleted: bool },
}

/// Spawn the watcher + dispatch loop. The returned [`WatcherHandle`]
/// keeps the notify watcher alive and aborts the dispatch task on
/// drop. The same registry the watcher built is plumbed back into
/// the [`AutoloaderManager`] so user-code roots can be armed
/// dynamically on first request.
pub fn start(
    projects: &[PathBuf],
    pool_manager: &Arc<PoolManager>,
    autoloader_manager: &Arc<AutoloaderManager>,
    registry: &Arc<WatchRegistry>,
) -> Result<WatcherHandle> {
    let (tx, mut rx) = mpsc::unbounded_channel::<PendingEvent>();

    let tx_for_cb = tx.clone();
    let registry_for_cb = Arc::clone(registry);
    // The notify callback runs on its own thread; we plumb a sync
    // handle into it. `classify` takes a read guard on the path map
    // for each event — short reads + many writes-from-arm = RwLock.
    let mut watcher: notify::RecommendedWatcher = notify::recommended_watcher(
        move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !is_relevant(&event.kind) {
                return;
            }
            let deleted = matches!(event.kind, notify::EventKind::Remove(_));
            let map = registry_for_cb.path_map();
            for path in &event.paths {
                for ev in classify(&map, path, deleted) {
                    let _ = tx_for_cb.send(ev);
                }
            }
        },
    )
    .wrap_err("creating notify watcher")?;

    // Build the initial path map and watch the static prefixes
    // (conf.d, project root for composer.json/bougie.toml/composer.lock).
    // user_code_roots are armed lazily via `WatchRegistry::arm_user_code_roots`
    // when the autoloader manager flips a project Cold → Warming.
    let mut watched: Vec<PathBuf> = Vec::new();
    {
        use notify::{RecursiveMode, Watcher as _};
        for project in projects {
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
                    registry.with_path_map_mut(|map| {
                        map.push_confd(sub.clone(), project.clone());
                    });
                    watched.push(sub);
                }
            }
            // Parent of composer.json / bougie.toml / composer.lock so
            // editors' write-and-rename doesn't invalidate the watch.
            if let Err(e) = watcher.watch(project, RecursiveMode::NonRecursive) {
                eprintln!(
                    "bougie server: failed to watch {}: {e}",
                    project.display()
                );
            } else {
                registry.with_path_map_mut(|map| map.push_version_input(project.clone()));
                watched.push(project.clone());
            }
        }
    }

    // Hand the constructed watcher to the registry. Subsequent
    // `arm_user_code_roots` calls (from the autoloader manager) will
    // extend it.
    registry.install_watcher(watcher);

    let pool_manager = Arc::clone(pool_manager);
    let autoloader_manager = Arc::clone(autoloader_manager);
    let dispatch = tokio::spawn(async move {
        run_dispatch(&mut rx, &pool_manager, &autoloader_manager).await;
    });

    Ok(WatcherHandle {
        registry: Arc::clone(registry),
        _watched: watched,
        dispatch,
    })
}

/// Per-(project, kind) debounce timers + user-code batched path sets.
/// `pending_user_code` maps a project to (`paths_changed`, `paths_deleted`)
/// + the next-fire instant; the merge is path-set, not timer-merge,
/// because devs may touch many files inside one window.
async fn run_dispatch(
    rx: &mut mpsc::UnboundedReceiver<PendingEvent>,
    pools: &Arc<PoolManager>,
    autoloader: &Arc<AutoloaderManager>,
) {
    let mut pending: HashMap<(PathBuf, ChangeKind), tokio::time::Instant> = HashMap::new();
    let mut pending_user_code: HashMap<PathBuf, UserCodeBatch> = HashMap::new();

    loop {
        let next_deadline = soonest_deadline(&pending, &pending_user_code);
        tokio::select! {
            evt = rx.recv() => {
                let Some(evt) = evt else { break };
                match evt {
                    PendingEvent::Touch { project, kind } => {
                        let window = debounce_window(&kind);
                        pending.insert((project, kind), tokio::time::Instant::now() + window);
                    }
                    PendingEvent::UserCodeChange { project, path, deleted } => {
                        let batch = pending_user_code.entry(project).or_default();
                        if deleted {
                            batch.deleted.push(path);
                        } else {
                            batch.changed.push(path);
                        }
                        batch.deadline = tokio::time::Instant::now() + DEBOUNCE_USER_CODE;
                    }
                }
            }
            () = sleep_until_opt(next_deadline) => {
                let now = tokio::time::Instant::now();

                let due_touches: Vec<(PathBuf, ChangeKind)> = pending
                    .iter()
                    .filter(|(_, when)| **when <= now)
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in due_touches {
                    pending.remove(&key);
                    let (project, kind) = key;
                    apply_touch(pools, autoloader, &project, kind).await;
                }

                let due_user_code: Vec<PathBuf> = pending_user_code
                    .iter()
                    .filter(|(_, b)| b.deadline <= now)
                    .map(|(p, _)| p.clone())
                    .collect();
                for project in due_user_code {
                    let Some(batch) = pending_user_code.remove(&project) else {
                        continue;
                    };
                    if !batch.changed.is_empty() {
                        autoloader.handle_user_code(&project, batch.changed).await;
                    }
                    if !batch.deleted.is_empty() {
                        autoloader
                            .handle_user_code_deleted(&project, batch.deleted)
                            .await;
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct UserCodeBatch {
    changed: Vec<PathBuf>,
    deleted: Vec<PathBuf>,
    deadline: tokio::time::Instant,
}

impl Default for UserCodeBatch {
    fn default() -> Self {
        Self {
            changed: Vec::new(),
            deleted: Vec::new(),
            // `tokio::time::Instant` has no `Default`; use a sentinel
            // in the past so a freshly-initialised batch fires
            // immediately if (defensively) it's never updated. The
            // event-arrival path always sets a concrete deadline
            // before the batch is observed by the select! loop.
            deadline: tokio::time::Instant::now(),
        }
    }
}

fn soonest_deadline(
    touches: &HashMap<(PathBuf, ChangeKind), tokio::time::Instant>,
    user_code: &HashMap<PathBuf, UserCodeBatch>,
) -> Option<tokio::time::Instant> {
    let touch_min = touches.values().min().copied();
    let uc_min = user_code.values().map(|b| b.deadline).min();
    match (touch_min, uc_min) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn debounce_window(kind: &ChangeKind) -> Duration {
    match kind {
        ChangeKind::ConfD => DEBOUNCE_CONFD,
        ChangeKind::VersionInput => DEBOUNCE_VERSION_INPUT,
        ChangeKind::Lockfile => DEBOUNCE_LOCKFILE,
    }
}

/// Decide whether a `notify` event represents an actual filesystem
/// change we need to react to. Access events (open, read,
/// close-nowrite, metadata-only) are filtered out — they don't change
/// what php-fpm would load, AND `build_variant_confd`'s own `read_dir`
/// of the watched directory triggers them, which would set up an
/// infinite reload loop.
fn is_relevant(kind: &notify::EventKind) -> bool {
    use notify::EventKind;
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

async fn sleep_until_opt(when: Option<tokio::time::Instant>) {
    match when {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending::<()>().await,
    }
}

/// Map a notify event path to one or more outgoing events. Multiple
/// returns are possible: a user-code path that also lives under the
/// project's static watches would produce two — but conf.d and
/// version-input are filtered by basename inside `classify`, so the
/// only way to double-fire is if a user-code root literally contains
/// `composer.json` (extremely unusual; we accept the redundant
/// Lockfile/VersionInput fire as harmless).
fn classify(map: &PathMap, path: &Path, deleted: bool) -> Vec<PendingEvent> {
    let mut out: Vec<PendingEvent> = Vec::new();

    // User-code roots first. Longest-prefix match across user-code
    // entries; nested user-code roots shouldn't happen in practice
    // (each scan_root is distinct), but pick the longest to be safe.
    let mut uc_candidates: Vec<&super::watch_registry::UserCodeRoot> = map
        .user_code
        .iter()
        .filter(|u| path.starts_with(&u.root))
        .collect();
    uc_candidates.sort_by_key(|u| std::cmp::Reverse(u.root.as_os_str().len()));
    // For deletes we don't filter on extension — a recursive directory
    // delete on macOS often surfaces as a single Remove event for the
    // directory (no per-file follow-ups), so the .php files inside it
    // would never be dropped from the in-memory classmap. Pass the dir
    // path through and let `apply_deleted_path` drop every entry under it.
    if let Some(best) = uc_candidates.first()
        && (deleted || has_php_or_inc_extension(path)) {
            out.push(PendingEvent::UserCodeChange {
                project: best.project.clone(),
                path: path.to_path_buf(),
                deleted,
            });
        }

    // conf.d
    let mut confd_candidates: Vec<&(PathBuf, PathBuf)> = map
        .confd
        .iter()
        .filter(|(prefix, _)| path.starts_with(prefix))
        .collect();
    confd_candidates.sort_by_key(|(p, _)| std::cmp::Reverse(p.as_os_str().len()));
    if let Some((_, project)) = confd_candidates.first() {
        out.push(PendingEvent::Touch {
            project: (*project).clone(),
            kind: ChangeKind::ConfD,
        });
        return out;
    }

    // version-input / lockfile — basename-filtered. Walk longest-
    // prefix so a future per-host nested layout still routes to its
    // own project.
    let mut vi_candidates: Vec<&PathBuf> = map
        .version_input
        .iter()
        .filter(|prefix| path.starts_with(prefix))
        .collect();
    vi_candidates.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
    if let Some(project) = vi_candidates.first()
        && let Some(basename) = path.file_name().and_then(|s| s.to_str()) {
            match basename {
                "composer.json" | "bougie.toml" => out.push(PendingEvent::Touch {
                    project: (*project).clone(),
                    kind: ChangeKind::VersionInput,
                }),
                "composer.lock" => out.push(PendingEvent::Touch {
                    project: (*project).clone(),
                    kind: ChangeKind::Lockfile,
                }),
                _ => {}
            }
        }

    out
}

fn has_php_or_inc_extension(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("php") || e.eq_ignore_ascii_case("inc"))
}

async fn apply_touch(
    pools: &Arc<PoolManager>,
    autoloader: &Arc<AutoloaderManager>,
    project: &Path,
    kind: ChangeKind,
) {
    match kind {
        ChangeKind::ConfD => {
            match pools.reload_project(project).await {
                Ok(count) if count > 0 => eprintln!(
                    "[pool_reload] project={} variants={count} reason=conf.d",
                    project.display()
                ),
                Ok(_) => {}
                Err(e) => eprintln!(
                    "bougie server: reload failed for {} (ConfD): {e:#}",
                    project.display()
                ),
            }
        }
        ChangeKind::VersionInput => apply_version_input(pools, project).await,
        ChangeKind::Lockfile => {
            autoloader.handle_lockfile(project).await;
        }
    }
}

async fn apply_version_input(pools: &Arc<PoolManager>, project: &Path) {
    let new_resolved = read_project_resolved(project).ok();
    let pids = pools.pids().await;
    let existing = pids
        .iter()
        .find(|(k, _)| k.project == project)
        .map(|(k, _)| (k.version.clone(), k.flavor.clone()));
    let version_changed = match (&new_resolved, &existing) {
        (Some((nv, nf)), Some((ov, of))) => (nv, nf) != (ov, of),
        (None, _) | (_, None) => false,
    };
    if version_changed {
        let count = pools.restart_project(project).await;
        eprintln!(
            "[pool_restart] project={} variants={count} reason=version-change",
            project.display()
        );
    } else {
        let count = pools.reload_project(project).await.unwrap_or(0);
        if count > 0 {
            eprintln!(
                "[pool_reload] project={} variants={count} reason=composer-or-bougie-toml",
                project.display()
            );
        }
    }
}

/// Returned from [`start`]; keep alive as long as the watcher should
/// run. `Drop` aborts the dispatch task and lets the underlying notify
/// watcher tear itself down when the registry's last `Arc` drops.
#[derive(Debug)]
pub struct WatcherHandle {
    registry: Arc<WatchRegistry>,
    _watched: Vec<PathBuf>,
    dispatch: tokio::task::JoinHandle<()>,
}

impl WatcherHandle {
    pub fn abort(&self) {
        self.dispatch.abort();
    }

    /// Read-only access used by tests + run.rs to keep the registry
    /// alive alongside the handle.
    pub fn registry(&self) -> &Arc<WatchRegistry> {
        &self.registry
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
    use crate::server::watch_registry::PathMap;

    fn map_for(project: &Path) -> PathMap {
        let mut m = PathMap::default();
        m.push_confd(
            bougie_installer::conf_d::project_confd_dir(project),
            project.to_path_buf(),
        );
        m.push_confd(
            bougie_installer::conf_d::project_confd_debug_dir(project),
            project.to_path_buf(),
        );
        m.push_confd(
            bougie_installer::conf_d::project_confd_local_dir(project),
            project.to_path_buf(),
        );
        m.push_version_input(project.to_path_buf());
        m
    }

    #[test]
    fn classify_routes_confd_files() {
        let project = PathBuf::from("/p/myapp");
        let map = map_for(&project);
        let evs = classify(&map, &project.join(".bougie/conf.d/20-redis.ini"), false);
        assert!(evs.iter().any(|e| matches!(
            e,
            PendingEvent::Touch { project: p, kind: ChangeKind::ConfD } if p == &project
        )));
    }

    #[test]
    fn classify_routes_composer_json() {
        let project = PathBuf::from("/p/myapp");
        let map = map_for(&project);
        let evs = classify(&map, &project.join("composer.json"), false);
        assert!(evs.iter().any(|e| matches!(
            e,
            PendingEvent::Touch { project: p, kind: ChangeKind::VersionInput } if p == &project
        )));
    }

    #[test]
    fn classify_routes_composer_lock() {
        let project = PathBuf::from("/p/myapp");
        let map = map_for(&project);
        let evs = classify(&map, &project.join("composer.lock"), false);
        assert!(evs.iter().any(|e| matches!(
            e,
            PendingEvent::Touch { project: p, kind: ChangeKind::Lockfile } if p == &project
        )));
    }

    #[test]
    fn classify_routes_user_code_php_file() {
        let project = PathBuf::from("/p/myapp");
        let mut map = map_for(&project);
        let user_root = project.join("src");
        map.push_user_code_root(project.clone(), user_root.clone());
        let evs = classify(&map, &user_root.join("Foo.php"), false);
        assert!(evs.iter().any(|e| matches!(
            e,
            PendingEvent::UserCodeChange { project: p, path: _, deleted: false } if p == &project
        )));
    }

    #[test]
    fn classify_ignores_user_code_non_php() {
        let project = PathBuf::from("/p/myapp");
        let mut map = map_for(&project);
        let user_root = project.join("src");
        map.push_user_code_root(project.clone(), user_root.clone());
        let evs = classify(&map, &user_root.join("README.md"), false);
        assert!(!evs.iter().any(|e| matches!(e, PendingEvent::UserCodeChange { .. })));
    }

    #[test]
    fn classify_routes_user_code_directory_delete() {
        // macOS FSEvents collapses a recursive rmdir into a single
        // Remove for the directory; classify must let that through so
        // the autoloader can drop every entry under it. Non-deleted
        // dir events stay filtered (a Modify on a directory isn't a
        // real file change).
        let project = PathBuf::from("/p/myapp");
        let mut map = map_for(&project);
        let user_root = project.join("src");
        map.push_user_code_root(project.clone(), user_root.clone());
        let dir = user_root.join("subdir");
        let evs_del = classify(&map, &dir, true);
        assert!(evs_del.iter().any(|e| matches!(
            e,
            PendingEvent::UserCodeChange { project: p, path: q, deleted: true }
                if p == &project && q == &dir
        )));
        let evs_change = classify(&map, &dir, false);
        assert!(!evs_change.iter().any(|e| matches!(e, PendingEvent::UserCodeChange { .. })));
    }

    #[test]
    fn classify_user_code_with_deleted_flag() {
        let project = PathBuf::from("/p/myapp");
        let mut map = map_for(&project);
        let user_root = project.join("src");
        map.push_user_code_root(project.clone(), user_root.clone());
        let evs = classify(&map, &user_root.join("Foo.php"), true);
        assert!(evs.iter().any(|e| matches!(
            e,
            PendingEvent::UserCodeChange { deleted: true, .. }
        )));
    }

    #[test]
    fn classify_ignores_unrelated_project_files() {
        let project = PathBuf::from("/p/myapp");
        let map = map_for(&project);
        assert!(classify(&map, &project.join("README.md"), false).is_empty());
        assert!(classify(&map, &project.join("src/app.php"), false).is_empty());
    }

    #[test]
    fn is_relevant_filters_access_events() {
        use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind};
        use notify::EventKind;
        assert!(!is_relevant(&EventKind::Access(AccessKind::Open(AccessMode::Read))));
        assert!(!is_relevant(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
        assert!(!is_relevant(&EventKind::Access(AccessKind::Any)));
        assert!(is_relevant(&EventKind::Create(CreateKind::File)));
        assert!(is_relevant(&EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Any
        ))));
        assert!(is_relevant(&EventKind::Remove(RemoveKind::File)));
        assert!(!is_relevant(&EventKind::Any));
        assert!(!is_relevant(&EventKind::Other));
    }
}
