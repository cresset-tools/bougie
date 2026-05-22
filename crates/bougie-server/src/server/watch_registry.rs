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

/// A user-code scan root the manager has armed. Newtype around
/// `PathBuf` so the `classify` lookup table can distinguish it from
/// the project-root entry (which would otherwise be a longer-prefix
/// match for paths under the project that aren't autoload-relevant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCodeRoot {
    pub project: PathBuf,
    pub root: PathBuf,
}

/// Every prefix the dispatch loop's `classify` consults to label
/// incoming events. Walk longest-prefix first — `<project>/.bougie/
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
    pub fn push_user_code_root(&mut self, project: PathBuf, root: PathBuf) {
        self.user_code.push(UserCodeRoot { project, root });
    }
    pub fn contains_user_code_root(&self, root: &Path) -> bool {
        self.user_code.iter().any(|u| u.root == root)
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

    pub(crate) fn with_path_map_mut<R>(&self, f: impl FnOnce(&mut PathMap) -> R) -> R {
        let mut guard = self.path_map.write().expect("path map poisoned");
        f(&mut guard)
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
        let Some(watcher) = watcher_guard.as_mut() else {
            // Tests construct a registry without a real watcher.
            // Without a notifier, just record the roots in the path
            // map so classify() reports correctly — the test harness
            // drives FS events synthetically anyway.
            let mut map = self.path_map.write().expect("path map poisoned");
            for root in roots {
                if !map.contains_user_code_root(root) {
                    map.push_user_code_root(project.to_path_buf(), root.clone());
                }
            }
            return Ok(());
        };
        let mut map = self.path_map.write().expect("path map poisoned");
        for root in roots {
            if map.contains_user_code_root(root) {
                continue;
            }
            if !root.is_dir() {
                // Autoload directive may point at a path that doesn't
                // exist yet. The bootstrap scan will pick up an empty
                // result; future saves create the dir, and the parent
                // project-root watch surfaces the create event.
                continue;
            }
            watcher.watch(root, RecursiveMode::Recursive)?;
            map.push_user_code_root(project.to_path_buf(), root.clone());
        }
        Ok(())
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
