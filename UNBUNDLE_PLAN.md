# bougie — tool tarball unbundling plan

Working plan for splitting the bundled C-library closure out of tool
tarballs (mariadb, redis, mkcert, erlang) and splitting embedded
sibling-tool trees out of higher-level tool tarballs (opensearch's
bundled JDK, rabbitmq's embedded erlang).

The spec context lives in
[`cresset-tools/php-build-standalone/DISTRIBUTION.md`](https://github.com/cresset-tools/php-build-standalone/blob/main/DISTRIBUTION.md)
(object kinds, manifest shape) and
[`SERVICES.md`](https://github.com/cresset-tools/php-build-standalone/blob/main/SERVICES.md)
(catalog model). This document is the build order for the bougie-side
client changes plus the wire-format additions both sides need to
agree on before the corresponding php-build-standalone tarball.nix
changes land.

The dedup-determinism precheck has already been done against the
current `flake.nix`: every consumer of `deps.openssl`/`deps.zlib`/
`deps.ncurses`/`deps.libedit` etc. resolves to the same Nix derivation
and thus the same `<name>-<version>-<8hex>` storeName. Confirmed
empirically by inspecting `result-tarball/.../install/store/` for PHP
8.5.6, `pbs-tree-11.4.10` (mariadb), `pbs-tree-8.6.3` (redis),
`pbs-tree-27.3.4.11` (erlang), and `pbs-tree-1.4.4` (mkcert) — every
overlap hash-matches.

## Scope

In scope:

1. **C-lib closure split for `kind=tool` artifacts.** mariadb, redis,
   mkcert, and erlang stop carrying their `install/store/<lib>-…/`
   subtree; the libraries publish as their own store-path blobs
   (kind-3 per DISTRIBUTION.md §1) and the tool manifests grow a
   non-empty `closure[]`.
2. **Tool-on-tool dependency mechanism.** OpenSearch drops its
   embedded `install/jdk/` tree. RabbitMQ drops its embedded
   `install/erlang/` tree. Both grow a new `requires_tools[]` field
   pointing at the standalone `jdk` / `erlang` tool tarballs.
3. **Bougie client changes** to fetch closure peers and recursively
   install tool dependencies on `bougie services up`-time
   auto-install and on any future `bougie tool install` path.

Explicitly out of scope:

- The PHP interpreter tarball layout. PHP keeps shipping
  `install/store/<lib>-…/` bundled (separate, deferred decision).
- Splitting opensearch's bundled plugin set into separate blobs.
  Plugins are core-version-pinned and used by no other tool, so
  there's no dedup win.
- Splitting JDK or erlang internal libs (`libjvm`, `lib/erlang/lib/`)
  into the shared store. They're not reusable.

## Wire-format additions

Both deltas live in `src/index/wire.rs`. The schema version stays at
`1`: empty `closure[]` and absent `requires_tools[]` are how today's
tool manifests already render, so a client that handles the new
fields stays backward-compatible with the pre-split artifacts.

### 1. `closure[]` becomes meaningful for `kind=tool`

Already a `Vec<Closure>` on `Manifest`; today every tool emits `[]`.
After the split, mariadb's manifest will populate it like extension
manifests already do. No struct change needed.

### 2. `requires_tools[]` is new

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequiresTool {
    /// Catalog short name of the depended-on tool (e.g. "jdk", "erlang").
    pub name: String,
    /// Exact upstream version of the depended-on tool. Pinned, not a range.
    pub version: String,
    /// Full tag of the depended-on artifact (e.g. "jdk-21.0.11_10-…-default").
    /// Identifies one row in the depended-on tool's section.
    pub tag: String,
    /// Manifest URL — absolute, same convention as Closure.url. The
    /// index publisher substitutes {INDEX_BASE}/{BLOB_BASE} at publish
    /// time so the client never has to reconstruct paths.
    pub manifest_url: String,
    /// Path relative to the outer tool's install root where the inner
    /// tool's install root must be linked. Example: opensearch sets
    /// link_into = "jdk" so its scripts find `${ES_HOME}/jdk/bin/java`.
    /// Empty string means "do not link" (rare; reserved for the case
    /// where the outer tool only needs the inner to be installed,
    /// not symlinked at a known path).
    pub link_into: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    // … existing fields …
    #[serde(default)]
    pub requires_tools: Vec<RequiresTool>,
}
```

`RequiresTool::validate()` mirrors `Closure::validate()`: reject empty
name, empty version, non-absolute manifest_url, and `link_into`
containing `..` / starting with `/`.

### Note on inner-manifest verification

The wire format deliberately omits `manifest_sha256` on `RequiresTool`
entries. Reason: at tarball.nix build time, the inner tool's
substituted manifest sha256 isn't known yet — index.nix substitutes
`{BLOB_BASE}` during publish, which changes the bytes. Computing it
ahead of time would require a two-pass substitution in `index.nix`
(stage all manifests → hash them → re-substitute cross-references →
re-hash), which is real work.

For v1 the client falls back to verifying the inner manifest via the
**section row** for `<inner-tool>` — section rows already carry the
authoritative `manifest.sha256` for every manifest in the publish.
The recursive walk fetches the index root → inner tool's section →
picks the row whose `tag` matches the outer manifest's
`requires_tools[].tag` → uses the section row's sha256 to verify the
manifest body. One extra section fetch per recursive step, but
section files are small and cache-friendly.

If a future wire-format revision wants to skip the section round-trip
on requires-tools resolution, add `manifest_sha256` then and grow
index.nix to do the two-pass substitution.

## Server-side layout (recap)

The on-the-wire layout doesn't change beyond what DISTRIBUTION.md
already specifies. Just two existing facets become populated:

```
https://index.bougie.tools/
  versions/<V>/targets/<T>/
    sections/
      tool/<name>.json                       # existing; closure-aware after this
      store-path/<name>.json                 # existing; gains tool-only deps
    manifests/
      tool/<name>/<ver>/<tag>.json           # closure[] + requires_tools[] populated
      store-path/<name>/<ver>/<hash>.json    # existing
```

`blobs.bougie.tools/blobs/<sha[0:2]>/<sha>` is unchanged — every
artifact stays content-addressed at the blob layer.

## Client-side layout (recap)

Tools already live under `$BOUGIE_HOME/store/<tarball>` today
(`store_fetch::fetch_blocking` extracts via `paths.store().join(entry.tarball)`),
which is also where the C-lib store paths live. That's the right
shape for content-addressed dedup; we keep it.

After the split:

```
$BOUGIE_HOME/store/
  openssl-3.5.6-99hgd6kn/lib/...             # one copy; referenced by PHP,
                                              # mariadb, redis, erlang, …
  zlib-1.3.2-jbmj2bcm/lib/...
  ncurses-6.6-grpadm5y/lib/...

  mariadb-11.4.10/                            # outer tool, no embedded store/
    bin/mariadbd
    lib/...
    store/
      openssl-3.5.6-99hgd6kn -> ../../openssl-3.5.6-99hgd6kn   # peer symlinks
      zlib-1.3.2-jbmj2bcm    -> ../../zlib-1.3.2-jbmj2bcm
      ncurses-6.6-grpadm5y   -> ../../ncurses-6.6-grpadm5y
      libedit-…              -> ../../libedit-…
      pcre2-…                -> ../../pcre2-…
      libxcrypt-…            -> ../../libxcrypt-…              # Linux only

  jdk-21.0.11_10/                             # tool tarball; stays self-contained
    bin/java
    lib/server/libjvm.so
    …

  opensearch-2.19.5/
    bin/opensearch
    lib/...
    plugins/...
    jdk -> ../jdk-21.0.11_10                  # link_into target

  erlang-27.3.4.11/
    bin/erl
    lib/erlang/lib/…
    store/                                    # erlang's own closure
      openssl-3.5.6-99hgd6kn -> ../../openssl-3.5.6-99hgd6kn
      zlib-1.3.2-jbmj2bcm    -> ../../zlib-1.3.2-jbmj2bcm
      ncurses-6.6-grpadm5y   -> ../../ncurses-6.6-grpadm5y

  rabbitmq-4.2.6/
    sbin/rabbitmq-server
    plugins/...
    erlang -> ../erlang-27.3.4.11             # link_into target
```

RPATH inside `mariadb-11.4.10/bin/mariadbd` continues to be
`$ORIGIN/../store/openssl-3.5.6-99hgd6kn/lib`. It resolves through
the per-entry peer symlink into the global pool — no patchelf at
install time, no per-platform branching, same mechanism extensions
already use.

OpenSearch's `bin/opensearch` continues to invoke `${ES_HOME}/jdk/bin/java`
(where `ES_HOME` is the unpacked opensearch dir). The
`jdk -> ../jdk-21.0.11_10` symlink absorbs the redirection. No
patching of opensearch scripts; no env-var wiring at run time.

## Existing bougie code we lean on

- `src/index/wire.rs::Closure` — closure entry struct and
  `validate()`. The shape we need for tools matches it exactly.
- `src/index/wire.rs::Manifest` — already deserializes `closure[]`
  for every kind; just needs `requires_tools[]` added.
- `src/install.rs::install_extension` — the canonical closure-aware
  install flow. Walks `manifest.closure[]`, fetches missing blobs
  into `paths.store()`, calls `materialize_closure_peer` for each.
  We port this loop into the tool install path.
- `src/install.rs::materialize_closure_peer(install_root, name, version, hash)`
  — creates `<install_root>/store/<name>-<version>-<hash>` as a
  relative symlink to `../../<name>-<version>-<hash>`. Already
  idempotent and refuses to overwrite a regular file. Reused as-is.
- `src/install.rs::store_dir_for_closure` — `$BOUGIE_HOME/store/<name>-<version>-<hash>`.
- `src/daemon/store_fetch.rs::ensure_tarball` — the existing tool
  auto-fetch entry point invoked by `dispatch_up`. Currently fetches
  one blob and stops; needs to grow the closure walk + the recursive
  `requires_tools` walk.
- `src/daemon/store_fetch.rs::pick_pinned_artifact` — section row
  selector, already version-pin aware. Reused for the recursive lookup.
- `src/daemon/store_layout.rs::basedir` — resolves
  `$BOUGIE_HOME/store/<tarball>` (with hash-suffix fallback). Already
  the right primitive for finding both outer and inner tools.
- `src/fetch.rs::extract_tar_zst` — `strip_prefix: "install"` strips
  the leading `install/` directory the tarballs ship under. Same
  call site works for closure-stripped tool tarballs (their tree
  still ships under `install/`, just without `install/store/`).
- `src/lock::ExclusiveGuard` — global lock around store mutations.
  Wrap the recursive install in one acquire to avoid lock-thrash
  across the tool + closure peers + sibling tools.

## New / modified bougie modules

```
src/index/wire.rs                    # +RequiresTool struct, +Manifest.requires_tools
src/daemon/store_fetch.rs            # +closure walk, +requires_tools recursive walk
src/daemon/store_layout.rs           # +link_into symlink creation helper
src/install.rs                       # +install_tool() public fn, refactored from
                                     #  install_extension's closure loop
```

Filenames map 1:1 to existing modules; no new files needed. The
ext/tool split is purely at the function boundary.

## Phase 0 — Wire format (0.5 day)

**Outcome:** Adding `requires_tools` to a manifest does not break
existing parsers; downloading a manifest with non-empty `closure[]`
on a `kind=tool` artifact parses and validates.

- Add `RequiresTool` struct and `Manifest.requires_tools` field with
  `#[serde(default)]`. Add `validate()`. Wire it into `Manifest::validate()`.
- Update the `wire.rs` round-trip tests with one `kind=tool` fixture
  carrying a populated `closure[]` and one with a populated
  `requires_tools[]`. Cover the rejection cases (relative
  manifest_url, `..` in link_into, etc.).

No behavior change in the daemon or CLI yet — this phase just makes
the parser tolerant.

## Phase 1 — Closure walk for `kind=tool` (1–2 days)

**Outcome:** Installing a tool whose manifest has non-empty
`closure[]` (mariadb, redis, mkcert, erlang) fetches the closure
blobs, lays out peer symlinks, and runs the tool from
`$BOUGIE_HOME/store/<tool>/` without any libraries being missing.

- Extract the closure loop body from `install::install_extension`
  (the part between fetching the main blob and writing conf.d) into
  a private helper `install_closure_peers(manifest, install_root, paths, bar)`.
  No semantics change — it just becomes callable from two sites.
- Extend `store_fetch::fetch_blocking` to call
  `install_closure_peers` after the main tarball extraction. The
  install root is `paths.store().join(entry.tarball)` (already
  computed; pass it in).
- Each closure entry is fetched via `fetch_blob` with
  `strip_prefix: "<storeName>"`. Same convention as
  `install::install_extension` uses for closure tarballs today —
  reuse the exact code path.
- Surface progress through the existing `DownloadBar::hidden`
  (daemon has no terminal). One total-byte estimate up front
  (`manifest.blob.size + sum(closure.size)`), one `set_current`
  per closure entry.
- Tests:
  - Unit: `install_closure_peers` over a manifest with three
    closure entries; assert three symlinks created at expected
    paths.
  - Integration: stub index server returns a mariadb manifest with
    a six-entry closure; assert end-to-end that `bougie services up`
    starts mariadbd and that `readelf -d` resolves every NEEDED
    through the peer symlink chain.

After this phase, the php-build-standalone side can switch the
tarball.nix for mariadb/redis/mkcert to drop `install/store/` and
emit non-empty closures. Old self-bundled tarballs keep working
(empty closure → loop is a no-op).

## Phase 2 — `requires_tools` resolver (2 days)

**Outcome:** Installing opensearch via the daemon's auto-fetch
recursively installs the JDK at the version pinned in its manifest,
creates the `opensearch-…/jdk` link, and `bin/opensearch` finds Java.

- Add `install_required_tool(paths, requires_tool, client, host, bar)`
  in `daemon/store_fetch.rs`. It:
  1. Resolves the requires_tool to an install location:
     `paths.store().join(format!("{}-{}", requires_tool.name, requires_tool.version))`.
  2. If that path already exists as a directory, returns early —
     the inner tool is already installed.
  3. Otherwise fetches `requires_tool.manifest_url`, validates the
     body against the inner tool's section-row sha256 (resolved via
     the same target's `tool/<inner-name>` section), then runs the
     same blob + closure flow as Phase 1.
  4. The inner tool's own `requires_tools[]` is walked recursively.
     Cycle prevention is a visited-set keyed by `(name, version)` —
     in practice no cycles exist (tool deps form a DAG: opensearch →
     jdk; rabbitmq → erlang), but the check is cheap insurance.
- Add `store_layout::create_link_into(outer_root, link_into, inner_root)`
  helper. Creates `outer_root/<link_into>` as a relative symlink to
  `inner_root` resolved against `outer_root`. Idempotent and
  refuses to overwrite a regular file (mirrors
  `materialize_closure_peer`'s posture).
  - When `link_into` is empty, skip the symlink (the "installed but
    not linked at a fixed path" case).
- Extend `store_fetch::fetch_blocking` to walk
  `manifest.requires_tools[]` *after* the main blob is extracted and
  closure peers are laid down. For each entry: install the inner
  tool, then call `create_link_into`.
- Global lock: hold one `ExclusiveGuard` across the whole recursive
  walk. The inner installs need to write to `$BOUGIE_HOME/store/`,
  and concurrent `bougie services up` for a different service can't
  be allowed to half-install jdk under us. Tests confirm the lock
  is recursive-safe (single guard, no nested acquire).
- Tests:
  - Unit: `create_link_into` creates correct relative target;
    refuses to clobber; handles `link_into = ""`.
  - Unit: `install_required_tool` early-returns when target dir
    exists.
  - Integration: stub index returns opensearch manifest with
    `requires_tools=[jdk@21.0.11+10]`. After install, assert
    `$BOUGIE_HOME/store/opensearch-2.19.5/jdk` resolves to
    `$BOUGIE_HOME/store/jdk-21.0.11_10/` and that
    `bin/opensearch --version` exits 0.

After this phase, the php-build-standalone side can switch the
opensearch and rabbitmq tarball.nix to drop their embedded tool
trees.

## Phase 3 — Catalog cross-check (0.5 day)

**Outcome:** The compiled-in catalog's `requires = […]` for
opensearch/rabbitmq stays consistent with the manifest's
`requires_tools[]` published by the index.

- At daemon startup (or first `bougie services up`), walk every
  catalog entry; for each `requires_tools` resolution, log a warning
  if the upstream manifest's `requires_tools` set differs from the
  catalog's `requires` list. Doesn't fail — the manifest is the
  source of truth at install time — but flags drift so we notice if
  the catalog table goes stale.
- The catalog can keep `requires` (startup-ordering relationship,
  see `SERVICES.md §2`) and the manifest can keep `requires_tools`
  (filesystem-co-installation relationship). They overlap in
  practice but answer different questions and should both exist.

## Phase 4 — `bougie services add` end-to-end UX polish (0.5 day)

**Outcome:** A user running `bougie services add opensearch` and
then `bougie services up` gets a clean experience: progress bars
account for all blobs, errors point at the right link, and the JSON
output schema for `up` carries the resolved closure inventory.

- Wire the recursive `RequiresTool` walk into the progress bar:
  the planned-bytes total grows as each inner manifest is fetched.
- The `ServicesUpResult` (existing struct, used by `bougie services
  up --format json-v1`) gains a `dependencies: Vec<ResolvedDep>`
  field per service, listing the resolved tool dependencies and
  closure entries that were either fetched or already on disk.
  Schema-version-bump to `2` for `ServicesUpResult` only (everything
  else stays at `1`).
- Error mapping: a 404 on a requires_tool manifest_url surfaces as
  "service `opensearch` depends on `jdk-21.0.11+10` which the index
  doesn't publish; the bougie catalog and the index are out of sync".
  Same template `pick_pinned_artifact` uses today.

## End-to-end walkthrough: `bougie services add opensearch`

This is the test case the implementation must pass. Numbered steps
trace through the existing + new code paths.

### State before

- Fresh `$BOUGIE_HOME` with no `installs/`, no `store/` contents.
- No project config.
- Index publishes:
  - `tool/opensearch/2.19.5/<tag>.json` with
    `requires_tools = [{name: "jdk", version: "21.0.11+10", tag: "…", link_into: "jdk", …}]`
    and `closure = []` (opensearch's only dep is the JDK).
  - `tool/jdk/21.0.11+10/<tag>.json` with `closure = []`,
    `requires_tools = []`.
  - The two manifests' blob URLs resolve under `blobs.bougie.tools`.

### Sequence

1. **`bougie services add opensearch`** (CLI side, `src/commands/services/add.rs`)
   - Validates `opensearch` against the catalog: `user_facing == true`. ✓
   - Locates project root or creates `bougie.toml`. Adds
     `[services] opensearch = "*"`.
   - Exits. No install yet.

2. **`bougie services up opensearch`** (CLI invokes daemon IPC)
   - CLI side: connects to `bougied.sock`, sends `Up { services: ["opensearch"] }`.
   - Daemon side (`dispatch_up`):
     - Looks up `opensearch` in the catalog.
     - Calls `ensure_tarball(paths, entry)` (`daemon/store_fetch.rs`).

3. **`ensure_tarball` for opensearch**
   - `store_layout::basedir(paths, entry)` → not found.
   - Spawns the blocking fetch task.
   - Acquires `ExclusiveGuard` on `paths.global_lock()`.
   - Fetches index root, `tool/opensearch.json` section, picks the
     `2.19.5` artifact via `pick_pinned_artifact`.
   - Fetches `tool/opensearch/2.19.5/<tag>.json` manifest.
   - Reads `manifest.blob`, `manifest.closure`, `manifest.requires_tools`.

4. **Main blob fetch (existing path)**
   - `fetch_blob` streams the tarball into `$BOUGIE_CACHE/blobs/<sha>.partial`,
     verifies sha256, extracts to `$BOUGIE_HOME/store/opensearch-2.19.5/`
     stripping `install/`.
   - Tarball contains `bin/opensearch`, `lib/...`, `plugins/...` —
     **no `jdk/` directory** (that's what we're verifying).

5. **Closure walk (Phase 1 path)**
   - `manifest.closure` is empty for opensearch. Loop is a no-op.

6. **`requires_tools` walk (Phase 2 path)**
   - One entry: `jdk-21.0.11+10`.
   - `install_required_tool(paths, entry, client, host, bar)`:
     - Computes inner install root:
       `$BOUGIE_HOME/store/jdk-21.0.11+10`. Not present.
     - Fetches the manifest at `entry.manifest_url`, verifies against
       the section-row sha256 for jdk in this target's
       `tool/jdk.json` section.
     - Validates: `jdk` manifest has `closure=[]`, `requires_tools=[]`.
     - Recurses into the same install flow:
       - `fetch_blob` for the JDK tarball → `$BOUGIE_HOME/store/jdk-21.0.11+10/`.
       - Empty closure, empty requires_tools.
   - Back in the opensearch context:
     `create_link_into("opensearch-2.19.5", "jdk", "jdk-21.0.11+10")`
     → creates `$BOUGIE_HOME/store/opensearch-2.19.5/jdk` →
     `../jdk-21.0.11+10`.

7. **Lock release; control returns to `dispatch_up`**
   - Daemon spawns `bin/opensearch` per the catalog `exec_args`.
   - Opensearch's bootstrap reads `JAVA_HOME` (or computes
     `$ES_HOME/jdk/bin/java`); finds it through the symlink.
   - Readiness probe per the catalog `health` entry succeeds.
   - IPC response: `Up::Ok { services: [{ name: "opensearch", state: "running", … }] }`.

8. **`--format json-v1` output (Phase 4)**

   ```json
   {
     "schema_version": 2,
     "services": [
       {
         "name": "opensearch",
         "version": "2.19.5",
         "state": "running",
         "dependencies": [
           { "kind": "tool",       "name": "jdk", "version": "21.0.11+10",
             "fetched": true,  "install_path": "store/jdk-21.0.11+10" }
         ]
       }
     ]
   }
   ```

### Second-run state

A subsequent `bougie services add redis` + `bougie services up redis`:

- Fetches `redis-8.6.3` tarball.
- Walks `closure[]`: `openssl-3.5.6-99hgd6kn`, `zlib-1.3.2-jbmj2bcm`.
- **Neither blob is fetched** — they're already on disk from a
  later opensearch install (in this scenario) or any earlier PHP
  install. `materialize_closure_peer` just creates the peer symlinks
  inside `store/redis-8.6.3/store/`.

The `fetched: false` reports in the JSON output prove dedup is
working from the user's vantage point.

## Server-side checklist (php-build-standalone)

This list lives here for cross-reference; the actual changes happen
in the sibling repo. Order matters: ship phases 0+1 of the bougie
client first so the new manifests have something that can read them.

For each tool, three files:

```
shared/tree.nix                  # add splitClosure flag or split into
                                 # tree-core + tree-closure outputs
tools/<tool>/<tool>.nix          # consume tree split
tools/<tool>/tarball.nix         # drop install/store/ from the
                                 # produced tarball, populate
                                 # closure[] from bundledDeps
shared/index.nix                 # ensure each tool's closure libs
                                 # surface in the store-path section
```

For opensearch + rabbitmq, additional changes:

```
tools/opensearch/opensearch.nix  # stop merging jdkTree at install/jdk/
tools/opensearch/tarball.nix     # populate requires_tools[] = [jdk-tag]
                                 # drop jdk from bundled_libraries

tools/rabbitmq/rabbitmq.nix      # stop merging erlangTree at install/erlang/
tools/rabbitmq/tarball.nix       # populate requires_tools[] = [erlang-tag]
                                 # drop erlang/openssl/zlib/ncurses from
                                 # bundled_libraries
```

A smoke-test extension in `scripts/smoke-test-tarball.sh` should lay
out the required `store/` peer symlinks (mimicking what
`materialize_closure_peer` does on the client side) before exec'ing
the tool binary. Otherwise CI on the php-build-standalone side can't
verify the produced artifact in isolation.

## Risks

- **Recursive install error reporting.** A `requires_tools` chain
  failure (e.g., the inner manifest 404s) needs to attribute the
  failure to the outer tool the user asked for, not just to the
  inner tag. The error template in Phase 4 covers this; verify in
  integration tests.
- **`link_into` collisions.** If two different `requires_tools`
  entries on the same outer manifest both specify `link_into: "jdk"`,
  the second would clobber the first. The validator in
  `Manifest::validate()` should reject duplicate `link_into` values
  on a single manifest.
- **Concurrent `bougie services up` for different services that
  share a closure peer.** Solved by the global `ExclusiveGuard`;
  smoke-test it (Phase 1 integration test) with two parallel daemon
  IPCs.
- **Stale catalog tarball-name vs index `tag`.** Today
  `store_layout::basedir` accepts a `<tarball>-` prefix to absorb
  hash-suffixed publish names. The recursive lookup keys on
  `<name>-<version>` from `RequiresTool`, so it doesn't depend on
  the catalog's `tarball` field at all — but the outer tool still
  does, so the cross-check in Phase 3 should flag mismatches
  proactively.
- **Old self-bundled tarballs.** Pre-split tarballs continue to be
  reachable by their URLs in older index snapshots; a user pinned
  to an old version will get the bundled form. That's fine: the
  install flow with empty `closure[]` and empty `requires_tools[]`
  is exactly the existing behaviour. No flag day.

## Rough time estimate

- Phase 0: 0.5 day
- Phase 1: 1.5 days
- Phase 2: 2 days
- Phase 3: 0.5 day
- Phase 4: 0.5 day
- Server-side (separate, parallelizable after Phase 1 lands): 2 days

Total bougie-side: ~5 days of focused work. The leverage from
existing closure-peer machinery is doing most of the heavy lifting.

## Open decisions deferred to implementation time

- **Manifest schema version bump.** Adding `requires_tools` with
  `#[serde(default)]` is backward-compatible at parse time, so
  `schema = 1` can stay. If a future change isn't backward-compatible,
  bump then. Decision: keep schema=1 for now.
- **`tool` install command surface.** Today tools are only installed
  implicitly by `bougie services up`. A future `bougie tool install
  jdk` or similar would surface the recursive install for direct
  use. Out of scope for this plan; the install primitives this plan
  introduces are the prerequisite.
- **Pruning unused store paths.** `bougie cache prune` and friends
  currently consider store paths reachable from installed PHP
  versions. After this lands, store paths reachable from installed
  *tools* must also count as live. Tracked separately; the GC pass
  is a one-line change to the existing reachability walk.
- **`bundled_libraries` field semantics.** With the split, this
  field on tool manifests collapses to `{ <tool>: <version> }`
  (just the tool itself) for most tools. Worth deciding whether to
  drop it on `kind=tool` entirely (the closure already enumerates
  what's bundled) or keep it for the few cases that ship inline
  upstream-internal libs (jdk's `lib/server/libjvm.so`, etc.). Keep
  for now; revisit if it gets confusing.
