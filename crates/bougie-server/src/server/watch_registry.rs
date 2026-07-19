//! Shared filesystem-watch state.
//!
//! Two consumers mutate this concurrently:
//!
//! - [`super::watcher::start`] arms the initial conf.d / composer.json
//!   / bougie.toml / composer.lock watches and runs the dispatch loop
//!   that translates notify events into `ChangeKind` routings.
//! - [`super::autoloader_manager::AutoloaderManager`] arms additional
//!   per-project user-code roots dynamically on Cold → Warming. The
//!   ordering invariant ("watcher armed before scan starts") requires
//!   the manager to extend this map *before* spawning its bootstrap
//!   task.
//!
//! The notify callback runs on its own thread and consults the
//! `path_map` synchronously; we use `std::sync::RwLock` so reads from
//! the callback don't block on the tokio-side mutators (writes are
//! infrequent — one per Cold→Warming transition).

use notify::{RecursiveMode, Watcher as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, RwLock};

/// A user-code scan root the manager tracks. Newtype around
/// `PathBuf` so the `classify` lookup table can distinguish it from
/// the project-root entry (which would otherwise be a longer-prefix
/// match for paths under the project that aren't autoload-relevant).
///
/// `armed` records whether an OS-level recursive watch is currently
/// attached. A root that doesn't exist yet (e.g. Magento's
/// `generated/code` before the first compile, or after a `cache:clean`
/// wipes the whole `generated/` tree) is still *recorded* — so
/// `classify` can match ancestor/descendant events against it — but
/// left unarmed until the directory appears. [`resync_user_code_arming`]
/// flips the bit and attaches/detaches the watch as the directory winks
/// in and out.
///
/// [`resync_user_code_arming`]: WatchRegistry::resync_user_code_arming
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCodeRoot {
    pub project: PathBuf,
    pub root: PathBuf,
    pub armed: bool,
}

/// Every prefix the dispatch loop's `classify` consults to label
/// incoming events. Walk longest-prefix first — `<project>/vendor/bougie/
/// conf.d/` wins over `<project>/` when both match.
#[derive(Debug, Default)]
pub struct PathMap {
    pub confd: Vec<(PathBuf, PathBuf)>,         // (prefix, project)
    pub version_input: Vec<PathBuf>,            // project root (filtered by basename)
    pub user_code: Vec<UserCodeRoot>,
}

impl PathMap {
    pub fn push_confd(&mut self, prefix: PathBuf, project: PathBuf) {
        self.confd.push((prefix, project));
    }
    pub fn push_version_input(&mut self, project: PathBuf) {
        self.version_input.push(project);
    }
    pub fn push_user_code_root(&mut self, project: PathBuf, root: PathBuf, armed: bool) {
        self.user_code.push(UserCodeRoot {
            project,
            root,
            armed,
        });
    }
    pub fn contains_user_code_root(&self, root: &Path) -> bool {
        self.user_code.iter().any(|u| u.root == root)
    }
    /// Whether the static (conf.d / version-input) watches for
    /// `project` have already been recorded.
    pub fn contains_project(&self, project: &Path) -> bool {
        self.version_input.iter().any(|p| p == project)
            || self.confd.iter().any(|(_, p)| p == project)
    }
}

/// Shared registry. Cloned (as an `Arc`) into both the watcher
/// dispatch loop and the autoloader manager so they observe the same
/// path map.
///
/// The notify watcher itself is wrapped in an `Option` so the
/// registry can be constructed *before* the watcher exists: the
/// callback closure handed to `notify::recommended_watcher` needs an
/// `Arc<WatchRegistry>` to consult `path_map`, but the watcher
/// can't exist until the callback is built. The startup sequence is:
///
/// 1. `WatchRegistry::new()` — both fields empty.
/// 2. Build `notify::recommended_watcher(|ev| consult registry)`.
/// 3. `registry.install_watcher(watcher)` — registry is fully populated.
/// 4. `arm_user_code_roots` becomes callable from the manager.
pub struct WatchRegistry {
    /// The notify watcher. `notify::RecommendedWatcher` is `Send` but
    /// its `watch` method is sync; we use a plain `std::sync::Mutex`.
    pub(crate) watcher: StdMutex<Option<notify::RecommendedWatcher>>,
    pub(crate) path_map: RwLock<PathMap>,
}

impl Default for WatchRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WatchRegistry {
    pub fn new() -> Self {
        Self {
            watcher: StdMutex::new(None),
            path_map: RwLock::new(PathMap::default()),
        }
    }

    /// Install the notify watcher once it's been constructed. Called
    /// from [`super::watcher::start`] after the initial static watches
    /// are armed; this hands ownership of the OS resources into the
    /// registry so subsequent `arm_user_code_roots` calls extend the
    /// same watcher.
    ///
    /// # Panics
    ///
    /// Panics if the watcher mutex is poisoned — i.e. a previous
    /// holder panicked.
    pub fn install_watcher(&self, watcher: notify::RecommendedWatcher) {
        *self.watcher.lock().expect("notify watcher poisoned") = Some(watcher);
    }

    /// Arm a project's user-code roots with the notify watcher and
    /// record them in the path map. Idempotent per `(project, root)`
    /// pair so a Lockfile re-bootstrap can call this without
    /// double-watching directories that were already armed.
    ///
    /// # Panics
    ///
    /// Panics if either the watcher mutex or the path-map `RwLock`
    /// is poisoned — i.e. a previous holder panicked. Bougie treats
    /// lock poisoning as unrecoverable since recovering would mean
    /// running with possibly-torn state.
    pub fn arm_user_code_roots(
        &self,
        project: &Path,
        roots: &[PathBuf],
    ) -> notify::Result<()> {
        let mut watcher_guard = self.watcher.lock().expect("notify watcher poisoned");
        let mut map = self.path_map.write().expect("path map poisoned");
        for root in roots {
            if map.contains_user_code_root(root) {
                continue;
            }
            // Record every root, even one that doesn't exist yet, so
            // `classify` can match ancestor/descendant events against it
            // (e.g. Magento's `generated/code` before the first compile).
            // Only attach an OS watch when the directory exists; absent
            // roots stay unarmed and get armed later by
            // `resync_user_code_arming` once they appear. Tests construct
            // a registry without a real watcher (`watcher_guard` is None)
            // and drive FS events synthetically — record but leave unarmed.
            let armed = match watcher_guard.as_mut() {
                Some(watcher) if root.is_dir() => {
                    watcher.watch(root, RecursiveMode::Recursive)?;
                    true
                }
                _ => false,
            };
            map.push_user_code_root(project.to_path_buf(), root.clone(), armed);
        }
        Ok(())
    }

    /// Arm the static per-project watches — the conf.d prefixes
    /// (recursive) and the project root (non-recursive, for
    /// composer.json / bougie.toml / composer.lock) — and record them
    /// in the path map. Called from [`super::watcher::start`] for
    /// every project in `server.toml` at boot, and from the control
    /// socket's `reload-config` handler for projects that enter the
    /// host map while the server is running. Without the latter, a
    /// project registered mid-run would never get its conf.d /
    /// composer.json edits picked up — `bougie ext add` after
    /// `bougie server` would leave the running pool on the old
    /// extension set until it idled out.
    ///
    /// Idempotent per project: an already-recorded project returns an
    /// empty vec. conf.d prefixes that don't exist are skipped, same
    /// as at boot (`conf.d-local/` only appears on the first local
    /// `.so` install). A registry without an installed watcher (tests,
    /// or a server whose watcher failed to start) records paths
    /// without attaching OS watches.
    ///
    /// Returns the paths newly watched.
    ///
    /// # Panics
    ///
    /// Panics if the watcher mutex or the path-map `RwLock` is
    /// poisoned — i.e. a previous holder panicked.
    pub fn arm_project_watches(&self, project: &Path, confd_dirs: &[PathBuf]) -> Vec<PathBuf> {
        let mut watcher_guard = self.watcher.lock().expect("notify watcher poisoned");
        let mut map = self.path_map.write().expect("path map poisoned");
        if map.contains_project(project) {
            return Vec::new();
        }
        let mut watched: Vec<PathBuf> = Vec::new();
        for sub in confd_dirs {
            if !sub.is_dir() {
                continue;
            }
            if let Some(w) = watcher_guard.as_mut()
                && let Err(e) = w.watch(sub, RecursiveMode::Recursive)
            {
                eprintln!("bougie server: failed to watch {}: {e}", sub.display());
                continue;
            }
            map.push_confd(sub.clone(), project.to_path_buf());
            watched.push(sub.clone());
        }
        // Parent of composer.json / bougie.toml / composer.lock so
        // editors' write-and-rename doesn't invalidate the watch.
        let root_watch = match watcher_guard.as_mut() {
            Some(w) => match w.watch(project, RecursiveMode::NonRecursive) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!(
                        "bougie server: failed to watch {}: {e}",
                        project.display()
                    );
                    false
                }
            },
            None => true,
        };
        if root_watch {
            map.push_version_input(project.to_path_buf());
            watched.push(project.to_path_buf());
        }
        watched
    }

    /// Re-evaluate every recorded user-code root for `project` against
    /// the current on-disk state, attaching a recursive watch to roots
    /// that have just appeared and detaching the watch from roots whose
    /// directory has vanished. Returns the roots whose armed state
    /// flipped — the caller (the autoloader manager) reconciles each via
    /// [`Autoloader::rescan_root`] so a freshly-created `generated/code`
    /// is scanned in (and a wiped one pruned) regardless of which leaf
    /// events the watcher delivered.
    ///
    /// Cheap (a handful of `is_dir` probes per project) and idempotent,
    /// so the manager can call it on every user-code batch.
    ///
    /// [`Autoloader::rescan_root`]: bougie_autoloader::Autoloader::rescan_root
    ///
    /// # Panics
    ///
    /// Panics if the watcher mutex or the path-map `RwLock` is poisoned.
    pub fn resync_user_code_arming(&self, project: &Path) -> Vec<PathBuf> {
        let mut watcher_guard = self.watcher.lock().expect("notify watcher poisoned");
        let mut map = self.path_map.write().expect("path map poisoned");
        let mut flipped: Vec<PathBuf> = Vec::new();
        for u in map.user_code.iter_mut().filter(|u| u.project == project) {
            let exists = u.root.is_dir();
            if exists && !u.armed {
                if let Some(watcher) = watcher_guard.as_mut()
                    && let Err(e) = watcher.watch(&u.root, RecursiveMode::Recursive)
                {
                    eprintln!(
                        "bougie server: failed to (re)arm watch on {}: {e}",
                        u.root.display()
                    );
                    continue;
                }
                u.armed = true;
                flipped.push(u.root.clone());
            } else if !exists && u.armed {
                // The tree was removed out-of-band. The OS watch may
                // already be gone (inotify drops it on delete); unwatch
                // is best-effort so a recreate re-arms cleanly.
                if let Some(watcher) = watcher_guard.as_mut() {
                    let _ = watcher.unwatch(&u.root);
                }
                u.armed = false;
                flipped.push(u.root.clone());
            }
        }
        flipped
    }

    /// Snapshot view for the dispatch loop's `classify`. Returns a
    /// read guard; callers should drop it promptly so concurrent
    /// `arm_user_code_roots` writers don't starve.
    ///
    /// # Panics
    ///
    /// Panics if the path-map `RwLock` is poisoned (see
    /// [`Self::arm_user_code_roots`] for the rationale).
    pub fn path_map(&self) -> std::sync::RwLockReadGuard<'_, PathMap> {
        self.path_map.read().expect("path map poisoned")
    }
}

impl std::fmt::Debug for WatchRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchRegistry").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_project_watches_records_existing_confd_and_root() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("myapp");
        let confd = project.join("vendor/bougie/conf.d");
        std::fs::create_dir_all(&confd).unwrap();
        let missing = project.join("vendor/bougie/conf.d-debug");

        let reg = WatchRegistry::new();
        let armed = reg.arm_project_watches(&project, &[confd.clone(), missing.clone()]);
        assert!(armed.contains(&confd));
        assert!(armed.contains(&project));
        assert!(!armed.contains(&missing));

        let map = reg.path_map();
        assert!(map.confd.iter().any(|(p, proj)| p == &confd && proj == &project));
        assert!(!map.confd.iter().any(|(p, _)| p == &missing));
        assert!(map.version_input.contains(&project));
        assert!(map.contains_project(&project));
    }

    #[test]
    fn arm_project_watches_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("myapp");
        let confd = project.join("vendor/bougie/conf.d");
        std::fs::create_dir_all(&confd).unwrap();

        let reg = WatchRegistry::new();
        assert!(!reg.arm_project_watches(&project, std::slice::from_ref(&confd)).is_empty());
        assert!(reg.arm_project_watches(&project, std::slice::from_ref(&confd)).is_empty());

        let map = reg.path_map();
        assert_eq!(map.confd.iter().filter(|(_, p)| p == &project).count(), 1);
        assert_eq!(map.version_input.iter().filter(|p| *p == &project).count(), 1);
    }
}
