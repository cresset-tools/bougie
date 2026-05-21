//! Server-resident autoloader manager.
//!
//! One `Autoloader` per project, owned by the server. Lifecycle:
//!
//! 1. **Cold** (server start). No work has been done for this project.
//!    fpm serves against whatever on-disk autoload `composer install`
//!    emitted — the unoptimized PSR-4 fallback.
//! 2. **Warming** (first request landed). The notify watcher is armed
//!    for the project's user-code roots **before** the bootstrap scan
//!    starts. Filesystem events that arrive during the scan land in
//!    the per-project buffer instead of taking the live-patch path
//!    (which requires a Live `Autoloader`). fpm continues to serve
//!    against the on-disk autoload — no request blocks on the scan.
//! 3. **Live**. Bootstrap completed, buffered events drained, the
//!    optimized + authoritative `autoload_classmap.php` swapped in
//!    atomically. Subsequent saves take the patch fast path.
//!
//! The watcher-before-scan ordering is load-bearing: it's what
//! guarantees the swapped-in classmap is equivalent to "the scan plus
//! every save since the scan began". See
//! `INCREMENTAL_AUTOLOADER_PLAN.md`'s "Save during Warming" test.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bougie_autoloader::{user_code_roots, Autoloader, DumpRequest};
use tokio::sync::Mutex;

use super::watch_registry::WatchRegistry;

/// One project's autoloader lifecycle state. The outer `Arc<Mutex<>>`
/// in [`AutoloaderManager`] lets concurrent requests for the same
/// project share one bootstrap (the second arrival sees `Warming` or
/// `Live` and returns immediately).
#[derive(Debug)]
enum ProjectState {
    Cold,
    Warming(WarmingState),
    Live(Box<Autoloader>),
    /// Bootstrap failed (e.g. malformed composer.lock). The reason is
    /// logged at transition time; the lockfile-watcher event resets
    /// to Warming when the user fixes their input. We keep the unit
    /// payload (vs `Cold`) so concurrent requests don't re-spawn a
    /// bootstrap for a project we already know is broken.
    Failed,
}

/// Events that arrived during a bootstrap scan and need to be folded
/// into the fresh `Autoloader` before it goes Live.
#[derive(Debug, Default)]
struct WarmingState {
    /// Created / modified paths the scan may or may not have picked
    /// up. Drained by re-running the single-file extract on each path
    /// in arrival order.
    changed: Vec<PathBuf>,
    /// Deleted paths the scan can't have picked up. Drained by
    /// dropping the corresponding `per_file` entries.
    deleted: Vec<PathBuf>,
}

/// Server-wide autoloader bookkeeping. One entry per project root;
/// every entry starts `Cold` and transitions on the first request to
/// that project (lazy bootstrap).
pub struct AutoloaderManager {
    states: Mutex<HashMap<PathBuf, Arc<Mutex<ProjectState>>>>,
    /// Shared filesystem-watch registry. The manager arms new roots
    /// here during Cold → Warming so the watcher captures saves during
    /// the bootstrap window.
    registry: Arc<WatchRegistry>,
}

impl AutoloaderManager {
    /// Construct a manager with every project entry in `Cold`. The
    /// registry is shared with the watcher dispatch loop so events
    /// can be routed back to `handle_user_code` / `handle_lockfile`.
    pub fn new(projects: &[PathBuf], registry: Arc<WatchRegistry>) -> Self {
        let mut states = HashMap::new();
        for p in projects {
            states.insert(p.clone(), Arc::new(Mutex::new(ProjectState::Cold)));
        }
        Self {
            states: Mutex::new(states),
            registry,
        }
    }

    /// Lazily kick off (or no-op if already running / done) the
    /// bootstrap scan for `project`. Idempotent: concurrent first
    /// requests to the same project see the same Warming/Live state
    /// and return immediately. Does not block on the scan — the
    /// background tokio task drives that, and fpm serves against the
    /// on-disk autoload in the meantime.
    pub async fn ensure_bootstrap(self: &Arc<Self>, project: &Path) {
        let slot = {
            let mut guard = self.states.lock().await;
            Arc::clone(
                guard
                    .entry(project.to_path_buf())
                    .or_insert_with(|| Arc::new(Mutex::new(ProjectState::Cold))),
            )
        };

        let mut state = slot.lock().await;
        if !matches!(*state, ProjectState::Cold) {
            return;
        }

        // **Order matters.** Compute user-code roots and arm the
        // watcher first; only then flip state to Warming and spawn the
        // bootstrap task. A save that lands between arming and the
        // scan starting is captured by the scan itself (the scan reads
        // files at scan time); a save during the scan lands in the
        // Warming buffer; a save after Live takes the patch path. The
        // gap is the "before the watcher arms" window — see the plan
        // doc's "Gap between Cold and watcher-armed" risk.
        let req = bootstrap_request(project);
        let roots = match user_code_roots(&req) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "bougie server: user_code_roots failed for {}: {e}",
                    project.display()
                );
                *state = ProjectState::Failed;
                return;
            }
        };
        if let Err(e) = self.registry.arm_user_code_roots(project, &roots) {
            eprintln!(
                "bougie server: arming watcher for {} failed: {e}",
                project.display()
            );
            *state = ProjectState::Failed;
            return;
        }
        *state = ProjectState::Warming(WarmingState::default());

        let project_owned = project.to_path_buf();
        let slot_for_task = Arc::clone(&slot);
        tokio::spawn(async move {
            run_bootstrap(slot_for_task, project_owned).await;
        });
    }

    /// Fold a batch of user-code filesystem events into the live
    /// classmap. Routed from the watcher dispatch loop after debounce.
    ///
    /// Warming-state projects buffer the events; the bootstrap task
    /// drains the buffer before going Live. Live-state projects apply
    /// the patches inline and re-emit `autoload_classmap.php`.
    pub async fn handle_user_code(&self, project: &Path, paths: Vec<PathBuf>) {
        let Some(slot) = self.slot_for(project).await else {
            return;
        };
        let mut state = slot.lock().await;
        match &mut *state {
            ProjectState::Cold | ProjectState::Failed => {
                // No watcher should be armed for a Cold project, but
                // defensively drop the event — better than running a
                // bootstrap from inside the event-dispatch loop, which
                // would block other projects' events behind the scan.
            }
            ProjectState::Warming(buf) => {
                for p in paths {
                    buf.changed.push(p);
                }
            }
            ProjectState::Live(loader) => {
                let mut any = false;
                for p in &paths {
                    match loader.apply_changed_path(p) {
                        Ok(true) => any = true,
                        Ok(false) => {}
                        Err(e) => eprintln!(
                            "bougie server: autoloader patch failed for {}: {e:#}",
                            p.display()
                        ),
                    }
                }
                if any {
                    if let Err(e) = loader.emit() {
                        eprintln!(
                            "bougie server: autoloader re-emit failed for {}: {e:#}",
                            project.display()
                        );
                    }
                }
            }
        }
    }

    /// Fold deletion events the same way as [`Self::handle_user_code`]
    /// but routed through `apply_deleted_path`. Kept as a separate
    /// entry point so the watcher can call us with the right shape
    /// without a discriminator field.
    pub async fn handle_user_code_deleted(&self, project: &Path, paths: Vec<PathBuf>) {
        let Some(slot) = self.slot_for(project).await else {
            return;
        };
        let mut state = slot.lock().await;
        match &mut *state {
            ProjectState::Cold | ProjectState::Failed => {}
            ProjectState::Warming(buf) => {
                for p in paths {
                    buf.deleted.push(p);
                }
            }
            ProjectState::Live(loader) => {
                let mut any = false;
                for p in &paths {
                    match loader.apply_deleted_path(p) {
                        Ok(true) => any = true,
                        Ok(false) => {}
                        Err(e) => eprintln!(
                            "bougie server: autoloader delete-patch failed for {}: {e:#}",
                            p.display()
                        ),
                    }
                }
                if any {
                    if let Err(e) = loader.emit() {
                        eprintln!(
                            "bougie server: autoloader re-emit failed for {}: {e:#}",
                            project.display()
                        );
                    }
                }
            }
        }
    }

    /// `composer.lock` changed → re-bootstrap. The fresh
    /// `composer install` that produced the change already re-emitted
    /// the on-disk unoptimized autoload, so requests in flight keep
    /// resolving classes through PSR-4 fallback while the new
    /// bootstrap runs.
    pub async fn handle_lockfile(self: &Arc<Self>, project: &Path) {
        let Some(slot) = self.slot_for(project).await else {
            return;
        };
        {
            let mut state = slot.lock().await;
            // If we're Live, take the loader out and replace with
            // Warming. If we're already Warming, the in-flight
            // bootstrap will pick up the new lockfile on retry — but
            // we don't have retry plumbing yet; just leave it. The
            // common case is a Live → Warming transition.
            match &*state {
                ProjectState::Live(_) | ProjectState::Failed | ProjectState::Cold => {
                    *state = ProjectState::Warming(WarmingState::default());
                }
                ProjectState::Warming(_) => return,
            }
        }

        // Re-arm user_code_roots in case the lockfile change added a
        // new path-repo package or autoload directive.
        let req = bootstrap_request(project);
        if let Ok(roots) = user_code_roots(&req) {
            if let Err(e) = self.registry.arm_user_code_roots(project, &roots) {
                eprintln!(
                    "bougie server: re-arming watcher for {} failed: {e:#}",
                    project.display()
                );
            }
        }

        let project_owned = project.to_path_buf();
        tokio::spawn(async move {
            run_bootstrap(slot, project_owned).await;
        });
    }

    /// Read-only probe used by tests to assert state transitions
    /// without exposing the inner enum.
    #[cfg(test)]
    pub async fn state_label(&self, project: &Path) -> Option<&'static str> {
        let slot = self.slot_for(project).await?;
        let s = slot.lock().await;
        Some(match &*s {
            ProjectState::Cold => "cold",
            ProjectState::Warming(_) => "warming",
            ProjectState::Live(_) => "live",
            ProjectState::Failed => "failed",
        })
    }

    async fn slot_for(&self, project: &Path) -> Option<Arc<Mutex<ProjectState>>> {
        let guard = self.states.lock().await;
        guard.get(project).map(Arc::clone)
    }
}

impl std::fmt::Debug for AutoloaderManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoloaderManager").finish_non_exhaustive()
    }
}

/// Default `DumpRequest` for a server-driven bootstrap: optimize +
/// classmap-authoritative, dev autoload included (this is a dev
/// server), no APCu autoload (it's a runtime nicety, not relevant to
/// the in-memory model).
fn bootstrap_request(project: &Path) -> DumpRequest<'_> {
    DumpRequest {
        project_root: project,
        optimize: true,
        classmap_authoritative: true,
        no_dev: false,
        apcu_autoloader: false,
        apcu_prefix: None,
        autoloader_suffix: None,
    }
}

/// Bootstrap task body. Runs the scan, drains the Warming buffer into
/// the fresh `Autoloader`, emits, and flips state to Live.
async fn run_bootstrap(slot: Arc<Mutex<ProjectState>>, project: PathBuf) {
    // `Autoloader::bootstrap` does sync I/O + rayon. Push it onto the
    // blocking pool so the dispatch executor can keep accepting other
    // events while this project warms up. `DumpRequest` borrows the
    // project path, so the blocking closure owns the `PathBuf` and
    // rebuilds the request inside.
    let project_for_task = project.clone();
    let bootstrap_result = tokio::task::spawn_blocking(move || {
        let req = bootstrap_request(&project_for_task);
        Autoloader::bootstrap(&req)
    })
    .await;

    let mut loader = match bootstrap_result {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => {
            eprintln!(
                "bougie server: bootstrap failed for {}: {e}",
                project.display()
            );
            let mut state = slot.lock().await;
            *state = ProjectState::Failed;
            return;
        }
        Err(e) => {
            eprintln!(
                "bougie server: bootstrap task panicked for {}: {e}",
                project.display()
            );
            let mut state = slot.lock().await;
            *state = ProjectState::Failed;
            return;
        }
    };

    // Drain the Warming buffer into the fresh loader. Hold the lock
    // for the drain so a concurrent watcher event either lands in the
    // buffer before we drain (covered) or arrives after we go Live
    // and takes the patch path directly (also covered).
    let mut state = slot.lock().await;
    let buffer = match &mut *state {
        ProjectState::Warming(buf) => std::mem::take(buf),
        // Either the project was forcibly re-bootstrapped while we
        // were scanning (concurrent lockfile change), or it's in an
        // unexpected state. Drop the just-built loader; the more
        // recent transition will spawn its own bootstrap.
        _ => return,
    };

    for path in &buffer.changed {
        if let Err(e) = loader.apply_changed_path(path) {
            eprintln!(
                "bougie server: drain-changed failed for {}: {e:#}",
                path.display()
            );
        }
    }
    for path in &buffer.deleted {
        if let Err(e) = loader.apply_deleted_path(path) {
            eprintln!(
                "bougie server: drain-deleted failed for {}: {e:#}",
                path.display()
            );
        }
    }

    if let Err(e) = loader.emit() {
        eprintln!(
            "bougie server: initial emit failed for {}: {e:#}",
            project.display()
        );
        *state = ProjectState::Failed;
        return;
    }

    *state = ProjectState::Live(Box::new(loader));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::watch_registry::WatchRegistry;
    use std::time::Duration;

    /// Write a self-contained composer project under `dir`: root
    /// composer.json with a `App\` PSR-4 mapping at `src/`, an empty
    /// composer.lock, and a starter `src/Foo.php`. The bootstrap path
    /// reads only these three files (plus walks `src/`), so this is
    /// the minimum viable fixture for the manager's state-machine
    /// tests.
    fn write_minimal_project(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("composer.json"),
            br#"{"name":"test/it","autoload":{"psr-4":{"App\\":"src/"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("composer.lock"),
            br#"{"content-hash":"abc","packages":[],"packages-dev":[]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("src/Foo.php"),
            b"<?php\n\nnamespace App;\n\nclass Foo {}\n",
        )
        .unwrap();
    }

    /// Poll the manager's state label until it matches `target` or
    /// `timeout` elapses. Used in place of arbitrary sleep delays so
    /// fast CI machines don't wait pointlessly and slow ones don't
    /// flake.
    async fn wait_for_state(
        manager: &Arc<AutoloaderManager>,
        project: &Path,
        target: &str,
        timeout: Duration,
    ) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if manager.state_label(project).await.as_deref() == Some(target) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "timeout waiting for state {target}; last = {:?}",
                    manager.state_label(project).await
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn temp() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "bougie-server-am-{nanos}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn ensure_bootstrap_transitions_cold_to_live() {
        let dir = temp();
        write_minimal_project(&dir);
        let registry = Arc::new(WatchRegistry::new());
        let manager = Arc::new(AutoloaderManager::new(
            std::slice::from_ref(&dir),
            registry,
        ));

        assert_eq!(manager.state_label(&dir).await.as_deref(), Some("cold"));

        manager.ensure_bootstrap(&dir).await;

        // ensure_bootstrap returns immediately after flipping to
        // Warming; the background task drives the rest. The transient
        // Warming state may be too fast to observe — just wait for Live.
        wait_for_state(&manager, &dir, "live", Duration::from_secs(5)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ensure_bootstrap_is_idempotent() {
        let dir = temp();
        write_minimal_project(&dir);
        let registry = Arc::new(WatchRegistry::new());
        let manager = Arc::new(AutoloaderManager::new(
            std::slice::from_ref(&dir),
            registry,
        ));

        // Concurrent ensure_bootstrap calls must not spawn two
        // bootstrap tasks — the second arrival should see Warming or
        // Live and return without touching anything.
        let m1 = Arc::clone(&manager);
        let d1 = dir.clone();
        let m2 = Arc::clone(&manager);
        let d2 = dir.clone();
        let h1 = tokio::spawn(async move { m1.ensure_bootstrap(&d1).await });
        let h2 = tokio::spawn(async move { m2.ensure_bootstrap(&d2).await });
        let _ = tokio::join!(h1, h2);

        wait_for_state(&manager, &dir, "live", Duration::from_secs(5)).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handle_user_code_patches_live_classmap() {
        let dir = temp();
        write_minimal_project(&dir);
        let registry = Arc::new(WatchRegistry::new());
        let manager = Arc::new(AutoloaderManager::new(
            std::slice::from_ref(&dir),
            registry,
        ));

        manager.ensure_bootstrap(&dir).await;
        wait_for_state(&manager, &dir, "live", Duration::from_secs(5)).await;

        // Drop a new PHP file under the watched scan root and route
        // it through the patch path.
        let new_file = dir.join("src/Bar.php");
        std::fs::write(&new_file, b"<?php\n\nnamespace App;\n\nclass Bar {}\n").unwrap();
        manager.handle_user_code(&dir, vec![new_file.clone()]).await;

        let map_path = dir.join("vendor/composer/autoload_classmap.php");
        let map = std::fs::read_to_string(&map_path).unwrap_or_default();
        assert!(
            map.contains("'App\\\\Bar'"),
            "expected App\\Bar in classmap after patch — got:\n{map}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handle_lockfile_re_bootstraps() {
        let dir = temp();
        write_minimal_project(&dir);
        let registry = Arc::new(WatchRegistry::new());
        let manager = Arc::new(AutoloaderManager::new(
            std::slice::from_ref(&dir),
            registry,
        ));

        manager.ensure_bootstrap(&dir).await;
        wait_for_state(&manager, &dir, "live", Duration::from_secs(5)).await;

        // Mutate composer.lock + composer.json so the new bootstrap
        // visibly differs (add `App\Extra` scoped at a new src dir).
        std::fs::create_dir_all(dir.join("extra")).unwrap();
        std::fs::write(
            dir.join("composer.json"),
            br#"{"name":"test/it","autoload":{"psr-4":{"App\\":"src/","App\\Extra\\":"extra/"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("composer.lock"),
            br#"{"content-hash":"def","packages":[],"packages-dev":[]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("extra/Bonus.php"),
            b"<?php\n\nnamespace App\\Extra;\n\nclass Bonus {}\n",
        )
        .unwrap();

        manager.handle_lockfile(&dir).await;
        wait_for_state(&manager, &dir, "live", Duration::from_secs(5)).await;

        let map_path = dir.join("vendor/composer/autoload_classmap.php");
        let map = std::fs::read_to_string(&map_path).unwrap_or_default();
        assert!(
            map.contains("'App\\\\Extra\\\\Bonus'"),
            "expected App\\Extra\\Bonus after lockfile re-bootstrap — got:\n{map}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handle_user_code_buffers_during_warming() {
        // Inject events while the project is Warming (before the
        // bootstrap task transitions to Live). Verify the buffer
        // drains correctly and the post-Live classmap includes the
        // path that was injected during the warm-up window — the
        // load-bearing test for the watcher-before-scan ordering.
        let dir = temp();
        write_minimal_project(&dir);
        let registry = Arc::new(WatchRegistry::new());
        let manager = Arc::new(AutoloaderManager::new(
            std::slice::from_ref(&dir),
            registry,
        ));

        // Set state to Warming manually so the test isn't racing the
        // real bootstrap task. ensure_bootstrap is intentionally
        // skipped here — we drive the state machine by hand.
        let slot = {
            let mut guard = manager.states.lock().await;
            Arc::clone(guard.get_mut(&dir).unwrap())
        };
        {
            let mut state = slot.lock().await;
            *state = ProjectState::Warming(WarmingState::default());
        }

        // Write a new file and route it through. handle_user_code
        // must append to the buffer rather than try to patch.
        let new_file = dir.join("src/Buffered.php");
        std::fs::write(
            &new_file,
            b"<?php\n\nnamespace App;\n\nclass Buffered {}\n",
        )
        .unwrap();
        manager
            .handle_user_code(&dir, vec![new_file.clone()])
            .await;

        // Verify the buffer holds the event.
        {
            let state = slot.lock().await;
            match &*state {
                ProjectState::Warming(buf) => {
                    assert_eq!(buf.changed, vec![new_file.clone()]);
                }
                other => panic!("expected Warming, got {other:?}"),
            }
        }

        // Now run the bootstrap task; the drain step must apply the
        // buffered path so the final Live classmap has Buffered.
        run_bootstrap(slot, dir.clone()).await;

        let map_path = dir.join("vendor/composer/autoload_classmap.php");
        let map = std::fs::read_to_string(&map_path).unwrap_or_default();
        assert!(
            map.contains("'App\\\\Buffered'"),
            "expected App\\Buffered after warming-drain — got:\n{map}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
