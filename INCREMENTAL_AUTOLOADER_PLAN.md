# Incremental classmap cache for `dump-autoloader`

Working plan for caching `bougie composer dump-autoloader -o` output
between invocations so that an unchanged subtree doesn't get rescanned.
Follow-on to `AUTOLOADER_PLAN.md`; tracked in issue #108. Complementary
to issue #107 (`--trust-psr4-filenames`): #107 cuts the cost of a full
rebuild, this plan cuts how often a full rebuild is needed.

## Context

`bougie composer dump-autoloader -o` rescans every PHP file under
`vendor/` on each invocation. On a large project (~94k files) that's
~2s warm-cache and ~3.2s cold, with ~89% of CPU in
`open` / `read` / `getdirentries` syscalls (samply profile in #107).

The dev-server flow regenerates the autoloader after every file edit.
Paying that 2s when one file changed and 94,099 didn't is the cost we
want to eliminate.

Cache location: `vendor/composer/.bougie-classmap-cache.bin`. Living
under `vendor/` means `rm -rf vendor && bougie composer install`
naturally resets it — no separate "how do I clear the cache" knob.

## Mental model

The current pipeline (`crates/bougie-autoloader/src/collect.rs:163`)
builds a list of `Task`s (each = one scan root + namespace filter),
runs `scan::scan` over each task in parallel, then sequentially merges
the per-task `(class, path_expr)` vectors with first-seen-wins.

The cache is a **replay tape of the per-task scan output**, not a
snapshot of the merged classmap. We store, per task, every file we
visited with its mtime, size, and the class names we extracted under
that task's filter. On re-dump:

1. Header-hash the inputs (`composer.lock`, root manifest's autoload
   block, exclude patterns, flags). On mismatch, full rebuild.
2. Build the current task list exactly as today.
3. For each task that's in the cache: either reuse all its file
   records (no path under that task is dirty) or rescan only the
   changed files.
4. Synthesize the per-task `Vec<(class, path_expr)>` from cached +
   newly-scanned records, feed it into the existing merge at
   `collect.rs:407-414`.
5. Write the updated cache.

Because we replay each task's emission, ambiguity (first-seen wins
across tasks) is correct by construction: if the winning file
disappears, the merge naturally picks the next emission.

## Cache schema

```rust
// crates/bougie-autoloader/src/cache/
struct CacheHeader {
    schema_version: u32,            // bumps on any layout change
    bougie_version: String,         // env!("CARGO_PKG_VERSION")
    reference_composer_version: String,
    composer_lock_hash: [u8; 16],
    root_autoload_hash: [u8; 16],
    no_dev: bool,
    optimize: bool,
    classmap_authoritative: bool,
    exclude_patterns_hash: [u8; 16],
}

struct TaskKey {
    origin_pkg: Option<String>,     // None = root
    scan_root: PathBuf,             // canonicalized
    install_abs: PathBuf,
    filter: NamespaceFilterKey,     // serializable mirror of NamespaceFilter
    needs_vendor_exclude: bool,
}

struct FileRecord {
    rel_path: PathBuf,              // forward-slash, relative to scan_root
    mtime_ns: i128,
    size: u64,
    classes: Vec<String>,           // post-filter, in walker order
}

struct DirRecord {
    rel_dir: PathBuf,
    mtime_ns: i128,
    child_count: u32,
}

struct TaskCache {
    task: TaskKey,
    files: Vec<FileRecord>,         // sorted by rel_path → walker order
    dirs: Vec<DirRecord>,
}

struct Cache {
    header: CacheHeader,
    tasks: Vec<TaskCache>,
}
```

Codec: **rkyv** (zero-copy decode) is the target — the dev-loop goal
is sub-100ms warm-no-change re-dumps, and validating a ~10 MB cache
file in single-digit ms matters. Fallback to bincode if rkyv adds too
much friction to the schema during PR1.

Atomic write: reuse `write_atomic` at `crates/bougie-autoloader/src/lib.rs:280`
(rename-based, already in use for every emitted PHP file).

## Hash inputs

- `composer_lock_hash`: md5 of `composer.lock` bytes (cheap; the file is small).
- `root_autoload_hash`: hash of the **canonical, parsed-and-normalized**
  form of root `composer.json`'s `autoload`, `autoload-dev`, `config.platform`,
  `config.autoloader-suffix`. Built from `lock::read_root_manifest`'s output —
  whitespace / key-order changes in `composer.json` must not invalidate the
  cache.
- `exclude_patterns_hash`: union of `exclude-from-classmap` across packages
  + root in lockfile order — the same input that feeds `ExcludePatterns::build`
  at `collect.rs:220`.

`md-5` is already a dep via `collect.rs:9`.

## Invalidation flows

Both driven from a new `collect::classmap_incremental(req, cache_path) -> Vec<ClassmapEntry>`.

**Watcher-driven** (`dirty_paths: Some(&[PathBuf])`):
- For each task, intersect `dirty_paths` with `scan_root`.
- For each dirty path:
  - exists, mtime+size unchanged → keep cached record;
  - exists, mtime or size changed → re-read, re-clean, re-find, replace;
  - gone → drop record;
  - new (no prior record) → walk the smallest containing dir and add records.
- Tasks with empty intersection are reused as-is.

**Self-driven** (CLI `--incremental`, `dirty_paths: None`):
- For each task, walk `scan_root` via `fs::read_dir` (NOT walkdir's
  full recursive iterator), recursing only into directories whose
  mtime differs from cached `DirRecord.mtime_ns`. Inside a matching
  dir we still stat each file (content edits don't bump parent dir
  mtime), but skip `open` + `read` + parse for matching files.
- On a mismatched dir, diff the live entry list against cached
  records and recurse / add / remove as needed.

## API additions

`DumpRequest` (`crates/bougie-autoloader/src/lib.rs:60`) gains:

```rust
pub incremental: bool,                       // default false
pub dirty_paths: Option<&'a [PathBuf]>,      // None = self-driven
```

`dump_autoload` (`lib.rs:134`) calls `collect::classmap_incremental`
when `incremental == true`. `psr4`, `psr0`, `files` collectors stay
unchanged — they're already cheap and don't need caching.

CLI: add `--incremental` to `crates/bougie/src/commands/composer_dump_autoloader.rs:71`.
Off by default for byte-equivalence safety.

Dev server: `bougie-server` / `bougie-daemon` already runs the file
watcher and is the same binary as the CLI. It accumulates dirty paths
between dumps and calls `bougie_autoloader::dump_autoload` directly
with `incremental: true` and `dirty_paths: Some(&paths)`. No subprocess.

## Walker integration

Add `scan::walker::enumerate_with_meta(root, exts) -> Vec<(PathBuf, i128, u64)>`
returning `(path, mtime_ns, size)` for every kept file. The cold-cache
path uses this so PR1 doesn't double-stat. Symlink handling unchanged:
`follow_links(true)` stays, mtime read via `fs::metadata` (follows
symlinks, matches walkdir semantics).

`TaskKey.scan_root` and `install_abs` continue through `collect::canonical`
(used at `collect.rs:212, 245, 258, 294, …`), so cache keys stay
stable across runs even when the project root sits behind a symlink.

## Phasing

Five PRs, each independently reviewable. Composer's cross-task
`scannedFiles` dedup is **deferred to a separate issue** — orthogonal
and would muddy ambiguity-replay testing.

1. **PR1 — cache write-only.** New `cache/` module + types + rkyv (or
   bincode) dep. After every `dump_autoload`, write
   `vendor/composer/.bougie-classmap-cache.bin`. No read-back yet.
   `enumerate_with_meta` lands here so the cold-cache path doesn't
   regress.
2. **PR2 — self-driven invalidation.** Implement
   `collect::classmap_incremental` for `dirty_paths: None`. Wire
   `DumpRequest::incremental` and the CLI `--incremental` flag.
   Bench against a large project: expect sub-100ms warm-no-change.
3. **PR3 — watcher-driven invalidation.** Implement the
   `dirty_paths: Some(...)` branch. Subset of PR2's machinery plus
   a fast intersection path.
4. **PR4 — dev-server integration.** Plumb the existing watcher in
   `bougie-server` / `bougie-daemon` into a debounced `dirty_paths`
   vec. Call `dump_autoload` in-process. No autoloader changes.
5. **PR5 (optional) — race-tolerance hardening.** Files may be
   deleted/recreated between a watcher event and the re-dump. Make
   per-path errors local (skip + invalidate that record) rather than
   aborting the dump.

## Critical files

- `crates/bougie-autoloader/src/lib.rs` — `DumpRequest` fields, `dump_autoload` branch.
- `crates/bougie-autoloader/src/collect.rs` — new `classmap_incremental`;
  task construction (lines 234-377) is the source of truth for `TaskKey`s;
  merge (lines 407-414) stays.
- `crates/bougie-autoloader/src/scan/mod.rs` — `scan()` still drives
  full-task scans when a whole task is dirty.
- `crates/bougie-autoloader/src/scan/walker.rs` — add `enumerate_with_meta`.
- `crates/bougie-autoloader/src/cache/` — new module (mod, header, entry, codec).
- `crates/bougie/src/commands/composer_dump_autoloader.rs` — `--incremental` CLI plumbing.
- `crates/bougie-server/`, `crates/bougie-daemon/` (PR4) — watcher → `DumpRequest` glue.

## Reused utilities

- `write_atomic` (`bougie-autoloader/src/lib.rs:280`) — cache writes.
- `collect::canonical` (`collect.rs:212` and similar) — `TaskKey` path normalization.
- `md5::Md5` (already imported at `collect.rs:9`) — hash fields.
- `lock::read_lock` / `lock::read_root_manifest` (`lib.rs:135-136`) — drive header hashes.
- `rayon::prelude` (`collect.rs:10`) — parallel rescan of dirty files inside a task.

## Verification

Unit tests in `crates/bougie-autoloader/src/cache/`:

- Header round-trip + every-field-flipped mismatch detection.
- Corrupt cache (truncated, wrong magic, bad `schema_version`) →
  graceful fall-through to full scan.
- `TaskKey` equality across canonicalize variants (test inside a
  tempdir symlink).
- Ambiguity replay: build cache with two tasks claiming the same
  class, delete first task's file on disk, run incremental, assert
  second now wins.

Integration tests under `crates/bougie-autoloader/tests/`:

- `incremental_byte_equivalence.rs`: every existing fixture under
  `tests/fixtures/` runs `dump_autoload` twice (second with
  `incremental: true`); assert second invocation produces byte-identical
  output to first. Reuses the harness from `byte_equivalence.rs:65`.
- New fixture `incremental-edit/`: dump → mutate one PHP file to
  declare a new class → incremental dump → assert classmap contains
  the new class.
- New fixture `incremental-delete/`: dump → delete a uniquely-named
  class's file → incremental dump → assert class removed.
- New fixture `incremental-ambiguous/`: two files declare class `Foo`;
  dump (first wins); delete first; incremental dump; assert second
  now wins.
- New fixture `incremental-lockfile-change/`: dump → bump
  `composer.lock` content-hash → incremental dump → assert full
  rebuild happened.

Manual perf check (not CI), a large project:

- Warm, no changes: <100ms (vs ~2s today).
- Warm, one file edited: <100ms.
- Cold (no cache): within 10% of current cold-cache wall —
  `enumerate_with_meta` must not regress.

## Risks

- **Ambiguity correctness.** Without per-task per-file class-list
  storage, deletion of a winner silently keeps the wrong record.
  Mitigation: mandatory `incremental-ambiguous` fixture in PR2.
- **Cache corruption.** Every read path returns `Result`; on any
  error log at debug and fall through to full scan, then overwrite
  the cache. Atomic write via `write_atomic`. Under `vendor/` so
  `composer install` resets it.
- **Cold-cache regression.** `enumerate_with_meta` should be free on
  macOS (`getdirentries64` already returns d_type / size) but may
  need an extra `fstatat` per file on Linux ext4. Bench in PR1.
- **Hash drift.** Root manifest hash MUST be the canonical
  parsed-and-normalized form, not raw JSON bytes. Whitespace /
  key-order changes in `composer.json` must not invalidate the cache.
- **`NamespaceFilter` evolution.** Any added field that affects
  `scan::scan` output must be reflected in `TaskKey.filter` and the
  `schema_version`. Add a doc reminder at the `NamespaceFilter`
  definition and a compile-time check (e.g. `mem::size_of` assertion
  on the discriminant variants) so the next person to extend the
  type can't silently invalidate the cache.

## Out of scope

- `scannedFiles` cross-task dedup (Composer parity): orthogonal,
  separate issue.
- Background / asynchronous cache writes: cache write is on the
  critical path of `dump_autoload`. Async write would need careful
  failure-mode design and isn't required to hit the perf goal.
- Distributed / network caches: not relevant for the dev-loop use
  case; the cache is per-checkout.
