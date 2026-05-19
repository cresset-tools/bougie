# Live autoloader in bougie-server

Working plan to make autoloader updates invisible in the dev loop:
bougie-server owns an in-memory autoloader model per project,
bootstrapped lazily on the project's first HTTP request via the
equivalent of `bougie composer dump-autoloader -a -o`. The filesystem
watcher is armed for the project's user-code roots **before** the
bootstrap scan begins, so saves that happen during the warm-up
window are queued and drained into the fresh state the moment
bootstrap finishes — no event is lost. Tracks #108. Supersedes the
persistent-cache framing originally proposed in this document.

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
(`serve_static`). It also lazy-starts fpm pools on first request to
a project. We extend the same lazy-on-first-request pattern to
autoload: server start does no per-project work, and the first
request to each project triggers a background bootstrap of the
in-memory `Autoloader`.

The on-disk autoload that fpm reads is never absent. `bougie composer
install` emits a fast **unoptimized** dump by default (PSR-4 / PSR-0
maps + the explicit `classmap` autoload entries from the lockfile,
no PSR-root scanning — sub-100 ms). That unoptimized autoload
handles every request via PSR-4 `file_exists` fallback until the
in-memory bootstrap finishes, at which point the server atomically
swaps in the optimized + classmap-authoritative
`autoload_classmap.php`.

This is `[[project_autoloader_incremental_and_autoreload.md]]`'s
PRs β + γ, with PR α (a persistent per-file cache) explicitly
skipped: the server is the source of truth during its lifetime, the
composer-install-time unoptimized autoload covers the cold path, and
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

Lifecycle states per project, transitioned in this order:

1. **Cold** (server start). No in-memory Autoloader. fpm serves
   against the unoptimized on-disk autoload emitted by composer
   install.
2. **Warming** (first request landed). Watcher is armed for the
   project's user-code roots and events buffer into a queue.
   Bootstrap scan runs on the tokio runtime. fpm continues to serve
   against the unoptimized on-disk autoload — no request blocks on
   the scan.
3. **Live**. Bootstrap done, buffered events drained, optimized +
   authoritative `autoload_classmap.php` swapped in atomically.
   Subsequent saves take the incremental-patch fast path.

Three flows:

1. **Lazy bootstrap** (first request to a project, lockfile change,
   autoload-config change): arm the watcher with event buffering,
   run the full parallel scan, drain buffered events into the fresh
   per-file maps, merge, write `autoload_*.php` atomically.
2. **Patch** (notify event under a watched user-code root, Live
   state only): re-read the dirty file, recompute its classes, diff
   against its prior per-file entry, update the task's map,
   recompute the merged BTreeMap, rewrite `autoload_classmap.php`.
3. **Re-bootstrap** (lockfile content-hash change): transition the
   project back to Warming — watcher buffer reactivates, bootstrap
   re-runs, swap on completion. Requests continue serving against
   the on-disk unoptimized autoload that the just-completed
   `composer install` re-emitted.

## Lazy bootstrap on first request

**CLI side.** `bougie composer install` emits an **unoptimized** dump
by default (no `-o`, no `-a`) by passing `optimize: false,
classmap_authoritative: false` to `dump_autoload`. This produces
`autoload_psr4.php`, `autoload_namespaces.php`, an unoptimized
(lockfile-only) `autoload_classmap.php`, `autoload.php`,
`autoload_real.php`, and `autoload_static.php`. PSR-4 `file_exists`
fallback at runtime resolves every class without a PSR-root scan.
Cost: sub-100 ms.

**Server side.** Server start does **no** per-project autoload work.
fpm pools and autoload bootstraps are both lazy on first traffic.

When the first HTTP request for a project lands in
`router.rs::dispatch`, the manager's `ensure_bootstrap(project)` is
called. It returns immediately if the project is already Warming or
Live. On a Cold → Warming transition:

1. Parse `composer.lock` + root manifest (sub-ms via
   `lock::read_lock` + `lock::read_root_manifest` —
   `lib.rs:135-136`).
2. Compute `user_code_roots`: root autoload `scan_root`s plus
   path-repo package `scan_root`s.
3. Call `notify::Watcher::watch` for each root and switch the
   manager entry to `Warming { buffer: vec![] }`. **From this moment
   on, any FS event under a watched root is appended to the
   per-project buffer** instead of taking the live-patch path (which
   requires a Live `Autoloader`).
4. Spawn a tokio task running `Autoloader::bootstrap(req)`.
5. **Return from `ensure_bootstrap`.** The request that triggered
   the warm-up dispatches to fpm normally and is not blocked on the
   scan.

The bootstrap task:

1. Runs the task-construction pass from `collect::classmap`
   (`collect.rs:234-377`).
2. For each task, drives `scan::scan` over `scan_root` — but
   capturing per-file emission as `BTreeMap<rel_path, Vec<class>>`
   instead of folding into a flat `Vec<(class, PathBuf)>`. A small
   change to `scan/mod.rs:40`'s return shape; the
   cleaner/finder/filter internals are unchanged.
3. Merges across tasks with the existing first-seen-wins
   (`collect.rs:407-414`) into the per-Autoloader merged BTreeMap.
4. Acquires the project's manager mutex. Drains the per-project
   event buffer into the fresh state by calling `apply_changed_path`
   / `apply_deleted_path` for each buffered path. Idempotent: an
   event for a file the scan already saw in its post-save state
   re-runs the same single-file extraction and writes the same
   per-file map entry.
5. Emits every autoload file via the existing `emit::*` paths
   (`lib.rs:178-209`) — atomically via `write_atomic`
   (`lib.rs:280`).
6. Swaps the manager entry to `Live(Autoloader)`. Subsequent events
   take the live-patch flow directly.

Bootstrap is `-a -o` (classmap-authoritative + optimize).
Authoritative mode is safe because the watcher armed *before* the
scan guarantees that the swapped-in classmap is equivalent to "the
scan plus every save since the scan began" — no incomplete-classmap
window for new classes.

For a ~94k-file project: ~2 s. For small projects: tens of ms.

## No startup gate

`router.rs::dispatch` does **not** gate on Autoloader state. The
on-disk unoptimized autoload (emitted by `composer install`) is
always a valid autoload — every class resolves via PSR-4
`file_exists`. The warm-up window between first request and Live
state is invisible to the dev: fpm serves normally, and once the
scan completes the optimize+authoritative classmap is atomically
swapped in for subsequent requests.

If a project has no `vendor/autoload.php` at all (fresh checkout
that was never installed), the existing fpm dispatch surfaces
whatever PHP error PHP itself produces. That's a pre-existing
condition, not something this plan creates.

## Patch flow

`watcher.rs` already debounces notify events per `(project, kind)`
on a 250 ms window. We add `ChangeKind::UserCode` with a 50 ms
window (devs notice longer than that on save → refresh) and batched
paths.

When a `UserCode` batch settles, the dispatch loop locks the
project's manager state and chooses:

- **Cold**: shouldn't happen — the watcher only watches user-code
  roots that were registered during a Cold → Warming transition.
  Defensive: drop.
- **Warming**: append the paths to the buffer. They'll be drained
  by the bootstrap task before it transitions to Live.
- **Live**: take the live-patch path below.

Per settled batch in Live state, for each dirty path:

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

- `ChangeKind::Lockfile` — `composer.lock` touched. Read it, hash
  the content; if the hash differs from the live Autoloader's
  header, transition the project back to Warming (re-activate the
  buffer), spawn `Autoloader::bootstrap`, swap on completion.
  Requests continue serving against the on-disk unoptimized
  autoload that `composer install` just re-emitted.
- The existing `ChangeKind::VersionInput` (composer.json /
  bougie.toml) already triggers a pool restart on PHP-version
  change. Extend it to also recompute the normalized autoload-config
  hash; on mismatch, transition back to Warming as above.

No readiness gate, no startup HTML page, no request rejection
during the swap window. Same uninterrupted-traffic property as the
initial lazy bootstrap.

Re-bootstrap re-runs `user_code_roots()` from the new lockfile +
manifest. If the set has changed (e.g. a new path-repo package), the
manager registers any new roots with `notify::Watcher::watch` before
the buffer-drain step, and drops roots that no longer apply.

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
}

// Free function — the manager calls this BEFORE bootstrap so it can
// arm the watcher first.
pub fn user_code_roots(req: &DumpRequest<'_>) -> Result<Vec<PathBuf>, DumpError>;
```

`apply_*` return `Ok(true)` iff the merged map actually changed (so
the caller can skip the emit when an edit didn't move the classmap —
e.g. a comment-only change). They're no-ops returning `Ok(false)`
for paths outside any `scan_root`.

`user_code_roots` lives outside `impl Autoloader` because the
manager needs the roots to arm the watcher *before* spawning the
heavy bootstrap. It reads only the lockfile + root manifest — sub-ms.

`dump_autoload` (`lib.rs:134`) becomes a thin wrapper:

```rust
pub fn dump_autoload(req: &DumpRequest<'_>) -> Result<(), DumpError> {
    let loader = Autoloader::bootstrap(req)?;
    loader.emit()
}
```

CLI semantics unchanged. The CLI install path passes `optimize:
false, classmap_authoritative: false` for its post-install dump; the
`-a -o` invocation is now server-internal.

## Server integration

`crates/bougie-server/src/server/watcher.rs` extensions:

- `ChangeKind::UserCode { paths: Vec<PathBuf> }` — batches a set of
  paths per debounced fire (unlike ConfD / VersionInput which carry
  no payload).
- `ChangeKind::Lockfile`.
- 50 ms debounce window for UserCode.
- `build_path_map` is extended **dynamically** when a project
  transitions Cold → Warming: the project's `user_code_roots()` are
  added, and `notify::Watcher::watch` is called for each. The
  initial server-start `build_path_map` covers only ConfD /
  VersionInput / Lockfile as today.
- `classify` returns `UserCode { paths: vec![path] }` for hits under
  those roots, filtered by `.php` / `.inc` extension (mirroring
  `scan/walker.rs::DEFAULT_EXTENSIONS`) to keep noise out.
- Dispatch coalescing: when a `UserCode` key already exists in
  `pending`, append the new path to its set rather than replacing.

New `AutoloaderManager` in `bougie-server`:

- Holds `HashMap<PathBuf, Arc<tokio::sync::Mutex<ProjectState>>>`
  keyed by canonical project root, where:

  ```rust
  enum ProjectState {
      Cold,
      Warming { buffer: Vec<UserCodeEvent> },
      Live(Autoloader),
  }
  ```

- `ensure_bootstrap(project)`: idempotent. On a Cold entry: parse
  lockfile + manifest, compute `user_code_roots`, register them with
  the watcher (`notify::Watcher::watch` for each), switch the entry
  to `Warming { buffer: vec![] }`, spawn the bootstrap task. Returns
  immediately. **Order matters: the watcher must observe a
  not-yet-armed → armed transition that completes before the
  bootstrap task starts the scan, so saves during the scan can only
  fall into the buffer or the scan itself, never into the gap.**
- Bootstrap task: builds the `Autoloader`, acquires the project
  mutex, drains the `Warming` buffer into it via `apply_*`, emits,
  swaps the state to `Live(Autoloader)`.
- On `UserCode` batch from the watcher: lock the project mutex; if
  `Warming`, append to buffer; if `Live`, apply each path and emit
  if `any(changed)`.
- On `Lockfile` / `VersionInput` re-bootstrap: lock the mutex; if
  `Live`, take the inner `Autoloader` out and replace with `Warming
  { buffer: vec![] }`; re-compute `user_code_roots` and register any
  new ones; spawn a new bootstrap task.

`crates/bougie-server/src/server/run.rs`:

- Construct an `AutoloaderManager` with one `Cold` entry per
  project. No bootstrap on server start.
- Pass the manager into `AppState`.
- Pass the manager into `watcher::start` so the dispatch loop can
  route `UserCode` / `Lockfile` events to it.

`crates/bougie-server/src/server/router.rs::dispatch`:

- After host resolution, before forwarding to `serve_php`, call
  `manager.ensure_bootstrap(project)`. Non-blocking — fires the
  warm-up if Cold, returns immediately. The request itself proceeds
  to fpm unconditionally.

## Phasing

Three PRs.

1. **PR1 — `Autoloader` refactor.** Carve `Autoloader::bootstrap` /
   `apply_changed_path` / `apply_deleted_path` / `emit` and the
   free-function `user_code_roots` out of the existing
   `dump_autoload` machinery. Pure refactor; no behavior change for
   CLI users. Introduces `TaskState` + per-file class-list storage.
   Adds the apply-path code that PR2 needs. Also lands the change
   to `bougie composer install` to default to unoptimized
   (`optimize: false, classmap_authoritative: false`).
2. **PR2 — server-resident autoloader.** `AutoloaderManager` in
   `bougie-server`; `ProjectState::{Cold, Warming, Live}`;
   `ensure_bootstrap` called from `router.rs::dispatch`;
   `ChangeKind::UserCode` + `ChangeKind::Lockfile` in `watcher.rs`
   with dynamic per-project `notify::Watcher::watch`; event
   buffering during Warming; dispatch loop wires events through the
   manager.
3. **PR3 (optional) — opcache reset.** Touch / signal fpm pool after
   each emit. Standard dev `opcache.revalidate_freq=0` makes this a
   convenience, not a requirement.

## Critical files

- `crates/bougie-autoloader/src/lib.rs` — `Autoloader` struct,
  re-route `dump_autoload` through it. `DumpRequest` unchanged.
- `crates/bougie-autoloader/src/collect.rs` — task construction
  (`:234-377`) and the merge (`:407-414`) get extracted into pieces
  the patch flow can drive on a single file's worth of input;
  `user_code_roots` shares the early portion of this code.
- `crates/bougie-autoloader/src/scan/mod.rs` — `scan()` continues to
  drive full-task scans on bootstrap; new `scan_one(path, &task,
  &exclude)` for the patch path runs the same cleaner+finder+filter
  pipeline on one file.
- `crates/bougie-autoloader/src/scan/walker.rs` — unchanged.
- `crates/bougie-server/src/server/watcher.rs` — new ChangeKinds,
  dynamic per-project `notify::Watcher::watch` calls during Cold →
  Warming, 50 ms UserCode debounce, batched-path coalescing.
- `crates/bougie-server/src/server/run.rs` — `AutoloaderManager`
  construction (all `Cold`), no bootstrap on start.
- `crates/bougie-server/src/server/router.rs` — `ensure_bootstrap`
  call slotted into `dispatch` before forwarding.
- (CLI) `bougie composer install` path — pass `optimize: false,
  classmap_authoritative: false` to `dump_autoload`.

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
- `lock::read_lock` / `lock::read_root_manifest` (`lib.rs:135-136`)
  — drive lockfile + autoload-config hashes; also called early
  during Cold → Warming to compute `user_code_roots` before the
  heavy scan.

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
- `apply_changed_path` on a comment-only edit returns `Ok(false)`.
- `user_code_roots(req)` returns root autoload `scan_root`s plus
  path-repo package `scan_root`s for a fixture with both.

Integration tests in `bougie-server`:

- Server starts. No project bootstraps; `AutoloaderManager` entries
  are all `Cold`.
- First HTTP request to a project: returns whatever fpm produces
  immediately (against the on-disk unoptimized autoload) AND
  triggers a background bootstrap; the project's state transitions
  Cold → Warming → Live within ~2 s.
- **Save during Warming (load-bearing test).** Inject a 1 s delay
  into the bootstrap task. Fire `ensure_bootstrap`. While the task
  is sleeping, write a new PHP file under a watched `scan_root`.
  Assert: the notify event lands in the project's Warming buffer
  (not the live-patch path); after the bootstrap task finishes, the
  drained buffer applies the path; the resulting Live
  `autoload_classmap.php` contains the new class. This is the
  watcher-before-scan ordering invariant.
- File touched under a root autoload `scan_root` in Live state
  triggers a debounced re-emit within 100 ms;
  `autoload_classmap.php` mtime advances and the new class is present.
- `composer.lock` content change triggers a re-bootstrap; the
  project transitions Live → Warming → Live without any request
  failing during the swap; the new classmap reflects the lock
  change.

Manual perf check (large project, ~94k files):

- Server boot: <50 ms regardless of project count.
- First request to project: returns at fpm's normal latency.
  Background bootstrap completes within ~2 s of first request.
- Save src/Foo.php → `autoload_classmap.php` mtime advance:
  <100 ms wall-clock (50 ms debounce + ~10 ms work).
- Steady-state HTTP request after save: 0 ms autoloader overhead.

## Risks

- **Server-only optimization.** CLI `bougie composer dump-autoloader`
  (no server) keeps today's full-scan path. CI/scripted dumps cost
  ~2 s on a large project. Acceptable — CI is infrequent.
- **First-request latency during Warming.** Requests that arrive
  during Warming dispatch to fpm against the unoptimized on-disk
  autoload, which uses PSR-4 `file_exists` resolution — slightly
  slower per-class than authoritative classmap. Only visible for
  requests during the first ~2 s after first traffic. Same latency
  shape as the existing lazy fpm-pool start.
- **Authoritative vs PSR-4 fallback disagreement.** For a project
  with genuine class-name ambiguity (the same class declared in two
  files reachable by PSR-4), PSR-4 `file_exists` order can resolve
  differently from authoritative first-seen-wins. The swap is
  observable. This is a pre-existing project bug; document, don't
  paper over.
- **Hand-edits to vendor.** Adding a new class to a vendor file is
  invisible until `composer install` or a server restart. Document;
  offer a manual reload as an escape valve.
- **Opcache staleness.** Without `opcache.revalidate_freq=0`, fpm
  workers serve a stale classmap for up to 2 s after each save.
  Standard dev php.ini sets this to 0 already; document, optionally
  PR3.
- **Ambiguity correctness after a deletion.** If files A and B both
  declared `Foo` and A is deleted, the patch flow must re-resolve to
  B by walking remaining `per_file` maps in task order. Without the
  `per_file` storage we'd silently keep A's stale `path_expr`. PR1
  carries the mandatory ambiguity fixture.
- **Multi-project memory.** One Autoloader per project ≈ ~20 MiB
  at ~94k classes. Cost is lazy — only Warming/Live projects pay
  it. A dev server with 5 active projects costs ~100 MiB.
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
- **Gap between Cold and watcher-armed.** A save in the brief
  window after `ensure_bootstrap` reads the lockfile but before
  `notify::Watcher::watch` returns is not buffered — but the
  subsequent bootstrap scan reads files at scan time, so the
  post-save content lands in the scan output regardless. The
  watcher-before-scan ordering only needs to cover events arriving
  *during* the scan; events before the watcher arms are captured
  by the scan itself.

## Out of scope

- Persistent on-disk cache. Composer install's unoptimized dump
  covers the cold-start case; the server is the source of truth
  during its lifetime.
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
