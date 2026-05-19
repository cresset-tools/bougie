# Live autoloader in bougie-server

Working plan to make autoloader updates invisible in the dev loop:
bougie-server owns an in-memory autoloader model per project,
bootstraps it via the equivalent of `bougie composer dump-autoloader -a -o`
on server start, and patches it incrementally as the existing
filesystem watcher reports user-code changes. Tracks #108. Supersedes
the persistent-cache framing originally proposed in this document.

## Context

`bougie composer dump-autoloader -o` on a large project (~94k files)
takes ~2 s warm, ~3.2 s cold; ~89% of CPU is in syscalls (samply
profile in #107). Today's dev-loop UX is "save file, run
dump-autoloader, wait 2 s, refresh browser."

bougie-server is a long-lived process. It already watches
`<project>/.bougie/conf.d/`, `composer.json`, and `bougie.toml`
(SERVER.md §7.2; impl: `crates/bougie-server/src/server/watcher.rs`),
and serves HTTP via axum, dispatching either to a PHP-FPM pool over
FastCGI (`server/router.rs::serve_php`) or to a static file
(`serve_static`). Extending it to watch user autoload roots and patch
the classmap in-memory removes the dump-autoloader step entirely.
The dev saves a file; before their browser-refresh request lands, the
classmap is current.

This is `[[project_autoloader_incremental_and_autoreload.md]]`'s
PRs β + γ, with PR α (a persistent per-file cache) explicitly
skipped: the server is the source of truth during its lifetime, so
no cache file is needed.

## Mental model

One `Autoloader` per project, owned by the server. State:

- The full task list (same shape as `collect.rs:163`'s task construction
  builds today; lines 234-377).
- Per task, a `BTreeMap<rel_path, Vec<class>>` recording every kept
  file's emitted class list under that task's `NamespaceFilter`.
- The merged `BTreeMap<class, path_expr>` ready to emit
  (`autoload_classmap.php`).
- Header hashes (`composer.lock` content-hash, normalized root
  autoload-config hash, exclude-patterns hash, flags) for detecting
  config drift.

Three flows:

1. **Bootstrap** (server start, lockfile change, autoload-config
   change): build the task list, run a full parallel scan, populate
   per-file maps, merge, write `autoload_*.php` atomically.
2. **Patch** (notify event under a watched user-code root): re-read
   the dirty file, recompute its classes, diff against its prior
   per-file entry, update the task's map, recompute the merged
   BTreeMap, rewrite `autoload_classmap.php`.
3. **Tear down** (lockfile content-hash change): drop the Autoloader
   and re-bootstrap.

## Bootstrap flow

On server start, **before** the project's HTTP front becomes ready:

1. Read `composer.lock` + root manifest as `dump_autoload` does
   today (`lib.rs:135-136`).
2. Run the task-construction pass from `collect::classmap`
   (`collect.rs:234-377`).
3. For each task, drive `scan::scan` over `scan_root` with the
   task's filter and exclude set — same code as today — but capture
   the per-file emission as `BTreeMap<rel_path, Vec<class>>` instead
   of folding into a flat `Vec<(class, PathBuf)>`. A small change to
   `scan/mod.rs:40`'s return shape; the cleaner/finder/filter
   internals are unchanged.
4. Merge across tasks with the existing first-seen-wins
   (`collect.rs:407-414`) into the per-Autoloader merged BTreeMap.
5. Emit every autoload file via the existing `emit::*` paths
   (`lib.rs:178-209`): `autoload_classmap.php`, `autoload_psr4.php`,
   `autoload_namespaces.php`, `autoload_files.php` (if any),
   `autoload.php`, `autoload_real.php`, `autoload_static.php`,
   plus the vendored composer runtime files. All via `write_atomic`
   (`lib.rs:280`).
6. Flip the project's readiness flag in `AppState`.

Bootstrap is always `-a -o` (classmap-authoritative + optimize):
every class lives in the classmap, no PSR-* runtime file_exists
fallback, fastest request handling. The block-on-bootstrap discipline
makes authoritative mode safe — no request is served against an
incomplete classmap.

For a large project: ~2 s. For small projects: tens of milliseconds.

## Startup page

While a project's readiness flag is `false`, `router.rs::dispatch`
short-circuits requests for that host to a 503 response carrying an
embedded HTML page (`include_str!` of an asset under
`crates/bougie-server/src/server/`). The page is dev-mode UX only:

- `<meta http-equiv="refresh" content="1">` so the browser polls.
- Friendly message naming the project and what's happening.
- No CSS framework, no JS — single static `&'static str`.

Slot it next to `not_found` / `forbidden` / `internal_error` in
`router.rs:521-530` as `fn starting(project: &Path) -> Response`.

## Patch flow

`watcher.rs` already debounces notify events per `(project, kind)`
on a 250 ms window. We add `ChangeKind::UserCode` with a 50 ms window
(devs notice longer than that on save → refresh) and batched paths.

Per settled batch, for each dirty path:

- **Modified**: locate the task whose `scan_root` contains the path
  (longest-prefix match across all tasks). If excluded by the task's
  `ExcludePatterns`, drop. Otherwise read+clean+find+filter via the
  same code as bootstrap; replace that task's `per_file[rel_path]`
  with the new class list.
- **Deleted**: drop the entry from every task it appeared under.
- **Created**: same handling as Modified — locate the owning task,
  scan, insert.

After applying the batch:

1. Rebuild the merged `BTreeMap<class, path_expr>` by re-running the
   first-seen-wins merge across all task `per_file` maps in task
   order. Cheap — ~5 ms across 94k entries.
2. Emit `autoload_classmap.php` via `write_atomic`. The other
   `autoload_*.php` files only change on bootstrap (PSR-4 / PSR-0 /
   files come from the lockfile + root manifest, not user code).
3. (Optional, PR3) Nudge fpm to `opcache_reset` the project's pool.

Per-edit total: ~10 ms (debounce 50 ms not included in the work
window — it overlaps with the dev's hand moving to the browser).

## Lockfile / autoload-config change flow

Two new `ChangeKind`s in the watcher:

- `ChangeKind::Lockfile` — `composer.lock` touched. Read it, hash the
  content; if the hash differs from the live Autoloader's header,
  flip the project's readiness flag to `false`, re-bootstrap, flip
  back. During the rebuild window, requests get the startup page
  again.
- The existing `ChangeKind::VersionInput` (composer.json /
  bougie.toml) already triggers a pool restart on PHP-version change.
  Extend it to also recompute the normalized autoload-config hash;
  on mismatch, re-bootstrap the Autoloader.

`vendor/` is **not watched**. Invariant: vendor only changes when
`composer install` runs, and `composer install` rewrites
`composer.lock`, which we DO watch. Hand-edits to vendor files work
for the common case (devs poke at method bodies to debug a library)
because existing class → file mappings stay stable. The exotic case
— adding a new class to a vendor file — is invisible until the next
install or server restart. Document it; offer `bougie server reload
<project>` (control socket; out of scope here) as an escape valve.

Path-repo packages (`dist.type: path` in `composer.lock`) get their
`scan_root`s added to the user-code watcher set — they behave like
root autoload paths for invalidation.

## `Autoloader` API

New struct in `bougie-autoloader`:

```rust
pub struct Autoloader {
    project_root: PathBuf,
    tasks: Vec<TaskState>,
    merged: BTreeMap<String, String>,   // class -> path_expr
    header: AutoloadHeader,             // lock hash, autoload-config hash,
                                        // exclude-patterns hash, flags
}

struct TaskState {
    task: TaskKey,                       // origin, scan_root, install_abs,
                                         // filter, needs_vendor_exclude
    exclude: ExcludePatterns,            // precompiled for this task
    per_file: BTreeMap<PathBuf, Vec<String>>, // rel_path -> emitted classes
}

impl Autoloader {
    pub fn bootstrap(req: &DumpRequest<'_>) -> Result<Self, DumpError>;
    pub fn apply_changed_path(&mut self, abs_path: &Path) -> Result<bool, DumpError>;
    pub fn apply_deleted_path(&mut self, abs_path: &Path) -> Result<bool, DumpError>;
    pub fn emit(&self) -> Result<(), DumpError>;
    pub fn header_matches(&self, req: &DumpRequest<'_>) -> bool;
    pub fn user_code_roots(&self) -> impl Iterator<Item = &Path>;
}
```

`apply_*` return `Ok(true)` iff the merged map actually changed (so
the caller can skip the emit when an edit didn't move the classmap —
e.g. a comment-only change). They're no-ops returning `Ok(false)`
for paths outside any `scan_root`.

`dump_autoload` (`lib.rs:134`) becomes a thin wrapper:

```rust
pub fn dump_autoload(req: &DumpRequest<'_>) -> Result<(), DumpError> {
    let loader = Autoloader::bootstrap(req)?;
    loader.emit()
}
```

CLI semantics unchanged.

## Server integration

`crates/bougie-server/src/server/watcher.rs` extensions:

- `ChangeKind::UserCode { paths: Vec<PathBuf> }` — batches a set of
  paths per debounced fire (unlike ConfD / VersionInput which carry
  no payload).
- `ChangeKind::Lockfile`.
- 50 ms debounce window for UserCode.
- `build_path_map` extended at bootstrap time with each project's
  `Autoloader::user_code_roots()` (root autoload `scan_root`s +
  path-repo package `scan_root`s).
- `classify` returns `UserCode { paths: vec![path] }` for hits under
  those roots, filtered by `.php` / `.inc` extension (mirroring
  `scan/walker.rs::DEFAULT_EXTENSIONS`) to keep noise out.
- Dispatch coalescing: when a `UserCode` key already exists in
  `pending`, append the new path to its set rather than replacing.

New `AutoloaderManager` in `bougie-server`:

- Holds `HashMap<PathBuf, Arc<tokio::sync::Mutex<Autoloader>>>`
  keyed by canonical project root.
- Per-project readiness flag in `AppState` (or co-located here).
- Bootstrap runs on the tokio runtime at server start; readiness
  flips when each project's bootstrap returns.
- On `UserCode` batch: lock the project's mutex, apply each path,
  emit if `any(changed)`.
- On `Lockfile` / `VersionInput` re-bootstrap: flip readiness off,
  rebuild, swap in the new Autoloader, flip readiness on.

`crates/bougie-server/src/server/run.rs`:

- Construct an `AutoloaderManager`; for each project, build a
  `DumpRequest` from `composer.json` / `composer.lock`, kick off
  `Autoloader::bootstrap`, store in the manager.
- Pass the manager into `AppState`.
- The watcher's `start` call gets the manager so its dispatch loop
  can route `UserCode` / `Lockfile` events to it.

`crates/bougie-server/src/server/router.rs::dispatch`:

- After host resolution, before forwarding to `serve_php`, check the
  project's readiness flag. Not ready → return `starting(project)`.

## Phasing

Three PRs.

1. **PR1 — `Autoloader` refactor.** Carve `Autoloader::bootstrap` /
   `apply_changed_path` / `apply_deleted_path` / `emit` out of the
   existing `dump_autoload` machinery. Pure refactor; no behavior
   change for CLI users. Introduces `TaskState` + per-file class-list
   storage. Adds the apply-path code that PR2 needs.
2. **PR2 — server-resident autoloader.** `AutoloaderManager` in
   `bougie-server`; bootstrap on server start; readiness gate in
   `router.rs::dispatch`; `starting` response; `ChangeKind::UserCode`
   + `ChangeKind::Lockfile` in `watcher.rs`; dispatch loop wires
   events through the manager.
3. **PR3 (optional) — opcache reset.** Touch / signal fpm pool after
   each emit. Standard dev `opcache.revalidate_freq=0` makes this a
   convenience, not a requirement.

## Critical files

- `crates/bougie-autoloader/src/lib.rs` — `Autoloader` struct,
  re-route `dump_autoload` through it. `DumpRequest` unchanged.
- `crates/bougie-autoloader/src/collect.rs` — task construction
  (`:234-377`) and the merge (`:407-414`) get extracted into pieces
  the patch flow can drive on a single file's worth of input.
- `crates/bougie-autoloader/src/scan/mod.rs` — `scan()` continues to
  drive full-task scans on bootstrap; new `scan_one(path, &task, &exclude)`
  for the patch path runs the same cleaner+finder+filter pipeline on
  one file.
- `crates/bougie-autoloader/src/scan/walker.rs` — unchanged.
- `crates/bougie-server/src/server/watcher.rs` — new ChangeKinds,
  expanded `build_path_map`, 50 ms UserCode debounce, batched-path
  coalescing.
- `crates/bougie-server/src/server/run.rs` — `AutoloaderManager`
  construction, bootstrap orchestration.
- `crates/bougie-server/src/server/router.rs` — readiness gate +
  `starting()` response, slotted next to `not_found` / `forbidden`
  at `router.rs:521-530`.
- `crates/bougie-server/src/server/` — embedded startup HTML asset.

## Reused utilities

- `collect::canonical` (`collect.rs:212` and similar) — TaskKey path
  normalization.
- `write_atomic` (`bougie-autoloader/src/lib.rs:280`) — emit writes.
- `scan::finder::find_classes` + `scan::cleaner` + `scan::filter` —
  per-file extraction in the patch path uses the same code as
  bootstrap.
- `notify::RecommendedWatcher` + debounce dispatch in
  `bougie-server/src/server/watcher.rs` — extend, don't replace.
- `md5::Md5` (already imported at `collect.rs:9`) — header hashes.
- `lock::read_lock` / `lock::read_root_manifest` (`lib.rs:135-136`) —
  drive lockfile + autoload-config hashes.
- `axum::response::Response` + the `plain_response` helper in
  `router.rs` — startup-page response shape.

## Verification

Unit tests in `bougie-autoloader`:

- For every existing fixture under `tests/fixtures/`:
  bootstrap → emit → assert byte-equivalent to today's
  `dump_autoload` (reuses the harness from `byte_equivalence.rs:65`).
- For every fixture: bootstrap → mutate one PHP file (add a class
  / remove a class / no-op edit) → `apply_changed_path` → re-emit
  → assert bytes match a fresh bootstrap with the mutation in place.
- Ambiguity replay: two files declare class `Foo`; bootstrap (first
  wins); `apply_deleted_path(first)`; assert the second now wins on
  re-emit.
- `apply_changed_path` on a path outside any `scan_root` returns
  `Ok(false)` and doesn't mutate state.
- Path-repo package scan_roots appear in `user_code_roots()`.
- `apply_changed_path` on a comment-only edit returns `Ok(false)`.

Integration tests in `bougie-server`:

- Server bootstraps an Autoloader on start; readiness flips after.
- HTTP request during bootstrap returns 503 with the startup page
  (assert `<meta http-equiv="refresh">` substring in body).
- After ready, requests forward to fpm normally.
- File touched under a root autoload `scan_root` triggers a
  debounced re-emit within 100 ms; `autoload_classmap.php` mtime
  advances and the new class is present.
- `composer.lock` content change triggers a re-bootstrap; readiness
  flickers `true → false → true` and the new classmap reflects the
  lock change.

Manual perf check (large project, ~94k files):

- Server boot → first project ready: ~2 s (block-on-bootstrap).
- Save src/Foo.php → `autoload_classmap.php` mtime advance:
  <100 ms wall-clock (50 ms debounce + ~10 ms work).
- Steady-state HTTP request after save: 0 ms autoloader overhead
  (rewrite has already landed before the next request).

## Risks

- **Server-only optimization.** CLI `bougie composer dump-autoloader`
  (no server) keeps today's full-scan path. CI/scripted dumps cost
  ~2 s on a large project. Acceptable — CI is infrequent.
- **Server-crash latency.** If the server dies between a save and
  the next bootstrap, the just-saved class isn't in
  `autoload_classmap.php` yet. Next bootstrap (~2 s) fixes it. The
  startup page surfaces this honestly.
- **Hand-edits to vendor.** Adding a new class to a vendor file is
  invisible until `composer install` or a server restart. Document;
  offer a manual reload as an escape valve.
- **Opcache staleness.** Without `opcache.revalidate_freq=0`, fpm
  workers serve a stale classmap for up to 2 s after each save.
  Standard dev php.ini sets this to 0 already; document, optionally
  PR3.
- **Ambiguity correctness after a deletion.** If files A and B both
  declared `Foo` and A is deleted, the patch flow must re-resolve
  to B by walking remaining `per_file` maps in task order. Without
  the `per_file` storage we'd silently keep A's stale `path_expr`.
  PR1 carries the mandatory ambiguity fixture.
- **Multi-project memory.** One Autoloader per project ≈ ~20 MiB
  for a ~94k-file project. A dev server with 5-10 projects
  costs ~100-200 MiB. Acceptable.
- **`NamespaceFilter` evolution.** Any field that affects
  `scan::scan` output must also affect single-file extraction so
  `apply_changed_path` produces the same set as bootstrap.
  Compile-time check on the discriminant + a doc reminder at the
  `NamespaceFilter` definition.
- **Watcher churn from editor tempfiles.** Editors do
  write-and-rename, which fires `Create` + `Remove` events near the
  real save. The 50 ms debounce + the `.php` / `.inc` extension
  filter in `classify` are usually enough, but worth eyeballing in
  PR2 against the editors the team uses (vim, VS Code, JetBrains).

## Out of scope

- Persistent on-disk cache. Server is the source of truth; bootstrap
  on every server start is cheap enough.
- CLI dump-autoloader speedups. Today's full-scan path is fine for
  CI / scripted invocations.
- File-watching `vendor/` for autoloader correctness. `composer.lock`
  watch covers the "vendor changed via composer install" case;
  hand-edits are documented exotic.
- Opt-in `vendor/` watcher for `opcache_invalidate`. Feasible as a
  follow-on PR — a ~94k-file project's vendor consumes ~10-15k inotify watches,
  well under modern Linux defaults (`fs.inotify.max_user_watches`
  is 524288 on Ubuntu 22+). Would let devs run `revalidate_freq=2`
  instead of the default `0`, with the watcher pre-invalidating
  touched files via a FastCGI call to fpm. Orthogonal to the
  classmap (vendor stays frozen-at-install); needs a config flag
  (off by default), event-storm suppression during lockfile
  re-bootstrap, and graceful fall-through if `inotify_add_watch`
  hits the user's limit.
- `scannedFiles` cross-task dedup (Composer parity). Orthogonal,
  separate issue.
- Cross-pool autoload sharing (e.g. SHM between fpm workers). PHP's
  opcache already does this; no new mechanism needed.
- Control-socket commands (`bougie server reload <project>` etc.).
  Out of scope for this plan; once the control socket exists it'll
  expose a one-liner to bust readiness and re-bootstrap.
