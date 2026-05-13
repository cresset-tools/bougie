# bougie server — implementation plan

Working plan for shipping `bougie server` as specified in
[`cresset-tools/php-build-standalone/SERVER.md`](https://github.com/cresset-tools/php-build-standalone/blob/main/SERVER.md).

The spec is the contract; this doc is the build order. Phases are bottom-up
so each ends in a runnable/shippable state. Stop at any phase and the
preceding work still has value.

## Approach

Bottom-up: skeleton → static → PHP → xdebug routing → lifecycle → opt-ins.
Each phase has a single verifiable outcome. No phase requires the next to
be useful.

v1 = phases 0–6. v1.x = phase 7 (TLS).

## Existing bougie code we lean on

- `src/cli.rs` — clap derive entry. New `ServerCommand` subcommand enum
  matches the shape of `ExtCommand` / `PhpCommand`.
- `src/install::Paths` — resolves `<install>/bin/php-fpm` (and `/bin/php`)
  for a given (version, flavor).
- `src/shim` — project root location and `.bougie/state/resolved` reading;
  `locate_project_root()`, `read_project_resolved()`.
- `src/conf_d` — listing of `.bougie/conf.d/*.ini` fragments. Reused for
  variant generation in phase 3.
- `src/index/wire::LoadDirective` — needed to identify the extension name
  carried in each conf.d fragment (for the `debug_only_extensions` filter).
- Global `--format text|json-v1` plumbing and the existing JSON event
  schema (CLI.md §9).

## New module layout (under `src/commands/server/`)

```
mod.rs              # ServerCommand enum + dispatch
config.rs           # server.toml parse + helper writes
run.rs              # foreground entry point (phase 1+)
router.rs           # Host: matching, pool selection
static_files.rs     # try_files, mime, path-safety
fastcgi.rs          # FCGI protocol (binary; ~500 LOC)
pool.rs             # spawn, idle-out, LRU cap, reload
conf_d.rs           # variant conf.d generation (normal/xdebug)
hosts.rs            # /etc/hosts sentinel-block management
control.rs          # control socket (phase 6, optional v1)
tls.rs              # mkcert wiring + rustls config (phase 7)
```

Module names mirror SERVER.md §11. Filenames map 1:1 — easy to grep
between the spec and the code.

## New crate dependencies

| Crate | Phase | Why |
|---|---|---|
| `tokio` (with `rt-multi-thread`, `net`, `process`, `signal`, `fs`) | 1 | async runtime |
| `axum` | 1 | HTTP server on top of hyper |
| `hyper` / `hyper-util` | 1 | already a transitive of axum; explicit body work |
| `tower` / `tower-http` | 1 | request/response middleware (logging, body limits) |
| `tracing` + `tracing-subscriber` (with `env-filter`, `json`) | 1 | structured logs, text + json-v1 |
| `mime_guess` | 1 | static-file Content-Type |
| `notify` | 4 | conf.d / composer.json watcher |
| `serde` + `toml` + `serde_json` | 0 | already present, reused |
| `sha2` | 2 | `<project-hash>` for runtime paths |
| `rustls` + `tokio-rustls` + `rustls-pemfile` | 7 | TLS listener |

FastCGI: implemented in-tree (`fastcgi.rs`), no client crate. Single
transport (unix socket), narrow param set, well-specified protocol.

---

## Phase 0 — Config & helper commands (1–2 days)

**Goal**: `bougie server add/remove/list` mutate
`$XDG_CONFIG_HOME/bougie/server.toml`. No HTTP yet.

**Files**:
- `src/cli.rs` — add `ServerCommand` enum: `Add`, `Remove`, `List`,
  (placeholder) `Run`.
- `src/commands/server/mod.rs` — dispatch.
- `src/commands/server/config.rs` — `ServerConfig` struct mirroring
  SERVER.md §4.2 schema; load/save with humantime for `idle_pool_timeout`.

**Verification**:
- `bougie server add myapp /tmp/myapp` produces a `[[host]]` block.
- `bougie server list --format json-v1` emits NDJSON matching CLI.md §9.
- Config round-trip preserves comments and ordering where feasible
  (use `toml_edit`, not bare `toml`).

**Tests**: parse fixtures, helper-mutation idempotency, `--format json-v1`
schema fields present.

---

## Phase 1 — Static HTTP server, single host (2–3 days)

**Goal**: foreground HTTP server that serves static files from configured
hosts. No PHP.

**Files**:
- `src/commands/server/run.rs` — tokio runtime, signal handling
  (SIGINT/SIGTERM graceful shutdown), listener bind.
- `src/commands/server/router.rs` — Host header lookup → `HostConfig`;
  404 with `bougie: unknown host` on miss.
- `src/commands/server/static_files.rs` — try_files placeholder
  expansion (`$uri`, `$is_args`, `$args`), filesystem probe, mime
  guess, `Cache-Control: no-cache`, path-traversal guard
  (canonicalize-under-root).

**Tracing wiring**: `tracing-subscriber` set up by `run.rs` based on
`--format` and `RUST_LOG`. One request-completion span per request with
the SERVER.md §9 field set; logged at INFO.

**Verification**:
- Start `bougie server` against a fixture project with `index.html`.
- `curl http://myapp.bougie.run:7080/` → 200, `Cache-Control: no-cache`.
- `curl http://myapp.bougie.run:7080/../../etc/passwd` → 403.
- SIGINT during a hanging request → graceful 5s shutdown then exit.

**Tests**: integration test using a real listener on `127.0.0.1:0`, two
host fixtures, ten request scenarios (existing file, missing file,
directory→index, traversal attempt, etc.).

---

## Phase 2 — FastCGI + single normal pool per project (4–5 days)

**Goal**: serve PHP requests via php-fpm. One pool per (project,
version), always "normal" variant. No xdebug yet.

**Files**:
- `src/commands/server/fastcgi.rs` — binary FCGI client over
  `UnixStream`. Records: `BEGIN_REQUEST`, `PARAMS`, `STDIN`, `STDOUT`,
  `STDERR`, `END_REQUEST`. Supports KEEP_CONN=0 (one conn per request)
  for v1; multiplexing deferred.
- `src/commands/server/conf_d.rs` — variant directory builder. For
  phase 2 only the "normal" variant; symlink to source fragments
  excluding `debug_only_extensions` (default `["xdebug"]`).
- `src/commands/server/pool.rs` — `Pool` state machine: `Starting →
  Healthy → Reaping`. Spawn `<install>/bin/php-fpm -y <conf> -p
  <prefix> -F` under tokio; capture stderr; emit `[fpm:<host>:<variant>]`
  prefix.
- `<project-hash>` derivation: first 12 chars of
  `hex(sha256(canonical_project_path))`.
- Health probe: `FCGI_GET_VALUES` with 2s timeout before first dispatch.

**Static + FCGI integration**: extend
`router.rs` so `try_files` falls through to FCGI dispatch when the
resolved file is `.php`. Populate FCGI params per nginx convention:
`SCRIPT_FILENAME`, `SCRIPT_NAME`, `PATH_INFO`, `QUERY_STRING`,
`REQUEST_METHOD`, `REQUEST_URI`, `DOCUMENT_ROOT`, `REMOTE_ADDR`,
`SERVER_NAME`, `SERVER_PROTOCOL`, `HTTPS` (unset in v1), `HTTP_*` for
every request header.

**Verification**:
- `bougie ext add xdebug` then `bougie server` (xdebug installed but
  not yet routed — phase 3 territory).
- `<project>/public/index.php` containing `<?php phpinfo();` →
  `curl http://myapp.bougie.run:7080/` returns rendered phpinfo.
- 502 with `bougie: php-fpm failed to start` if php-fpm config is broken
  (captured stderr included in the response body in `text` mode, in the
  event in `json-v1` mode).
- Headers + cookies round-trip correctly (POST forms, multi-value
  headers, large bodies via chunked STDIN frames).

**Tests**:
- FCGI protocol unit tests: serialize/parse each record type, fragment
  boundaries, oversized records.
- Integration: fixture project with one .php file, full request cycle.

---

## Phase 3 — xdebug pool + per-request routing (2–3 days)

**Goal**: the headline feature. `XDEBUG_SESSION` cookie / trigger
dispatches to a separate pool variant with xdebug loaded.

**Files**:
- `src/commands/server/conf_d.rs` — extend to also build the "xdebug"
  variant (full conf.d, no exclusion).
- `src/commands/server/router.rs` — `XdebugTrigger` detector:
  inspects `XDEBUG_SESSION` cookie, `XDEBUG_TRIGGER` cookie,
  `XDEBUG_SESSION_START` + `XDEBUG_TRIGGER` query params,
  `X-Bougie-Force-Xdebug` header.
- `src/commands/server/pool.rs` — second pool entry per project,
  keyed by `(project, version, variant)`.
- Always set `X-Bougie-Pool: normal|xdebug` on the response.

**Verification**:
- `curl http://myapp.bougie.run:7080/` → `X-Bougie-Pool: normal`.
- `curl -b XDEBUG_SESSION=1 http://...` → `X-Bougie-Pool: xdebug`, and
  PHP `xdebug_is_debugger_active()` returns true inside the script.
- Both pools coexist; switching cookies on consecutive requests routes
  correctly without restarts.

**Tests**: trigger-detection unit table (one row per cookie/query/header
combination, both presence and absence), router-level pool-selection
test.

---

## Phase 4 — Pool lifecycle (3–4 days)

**Goal**: idle-out, LRU cap, file-watch reload, PHP-version-change
restart.

**Files**:
- `src/commands/server/pool.rs`:
  - Per-pool `last_served_at: Instant`. A background task per server
    scans every ~10s; pools idle > `idle_pool_timeout` get SIGTERM →
    `waitpid` → cleanup.
  - LRU eviction when `len() >= max_concurrent_pools`: evict oldest
    `last_served_at` (idle) pool first.
- `src/commands/server/watcher.rs` (new) — `notify::RecommendedWatcher`
  wrapping `<project>/.bougie/conf.d/`,
  `<project>/composer.json`, optional `<project>/bougie.toml`.
  Debounce 250ms. On event:
  - composer.json or bougie.toml changed → recompute resolved PHP
    version. If changed, full pool restart (SIGTERM, respawn lazily).
  - `.bougie/conf.d/` changed → regenerate variant dirs, SIGUSR2 pool.

**Verification**:
- Idle a project for `idle_pool_timeout`; observe `pool_idle_out` event,
  pool gone from process list.
- Bring up `max_concurrent_pools + 1` hosts; oldest reaps with
  `pool_evicted` event.
- `bougie ext add redis` while server is running → next request loads
  redis (`pool_reload` event observed; SIGUSR2 sent).
- Change `composer.json` `"php"` constraint to a different patch; pool
  restarts with new version on next request.

**Tests**: pool state-machine unit tests with mocked clock; watcher
integration with `tempfile`.

---

## Phase 5 — /etc/hosts management (1–2 days)

**Goal**: opt-in fallback for users on DNS-rebinding-protected networks.

**Files**:
- `src/commands/server/hosts.rs`:
  - `bougie server hosts add <name>` — append to bougie's view in
    `$BOUGIE_HOME/state/hosts.json` (no /etc/hosts write yet).
  - `bougie server hosts remove <name>` — same.
  - `bougie server hosts apply` — read view, rewrite the sentinel
    block in /etc/hosts atomically (tempfile + rename). When invoked
    without root, prints `sudo bougie server hosts apply` and exits 1.
  - Sentinel block: `# BEGIN bougie ... # END bougie`. Only mutate
    inside. Preserve all other lines verbatim. Preserve permissions
    and ownership.
  - Hostname validation: `^[a-z0-9][a-z0-9-]{0,62}\.bougie\.test$` (or
    `.bougie.run` when used for local override).

**Verification**:
- `bougie server hosts add myapp` then `bougie server hosts list`
  shows it.
- `sudo bougie server hosts apply` writes the block; running it again
  is a no-op (idempotent).
- `bougie server hosts remove myapp` and re-apply removes the line.
- Pre-existing /etc/hosts content outside the sentinel block is
  preserved bit-for-bit.

**Tests**: with a writable tempfile standing in for /etc/hosts, exercise
add/remove/apply round trips and corruption resistance (sentinel
removed, missing END marker, etc.).

---

## Phase 6 — Control socket + live status (1 day)

**Goal**: `bougie server list` reports live pool state when a server is
running.

**Files**:
- `src/commands/server/control.rs`:
  - Server-side listener at
    `$XDG_RUNTIME_DIR/bougie/server/control.sock` (mode 0600).
  - Single-line JSON request, single-line JSON response. Methods:
    `status` (returns active pools, idle ages, host counts),
    `reload` (triggers conf.d regeneration without waiting for the
    watcher).
  - Client-side: `list` and an internal `reload` helper.

**Verification**:
- `bougie server` running in one terminal, `bougie server list` in
  another shows live pool table.
- Without a running server, `list` falls back to config-only output.

**Tests**: a fixture `bougie server` started in a child task; client
requests over the socket.

---

## Phase 7 — TLS via bougie-distributed mkcert (v1.x, separate cycle)

**Goal**: HTTPS support via the kind=tool mkcert artifact.

**Prerequisites in php-build-standalone**:
- `tools/mkcert/` directory: `mkcert.nix`, `build-mkcert.sh` (calls
  Go build with `CGO_ENABLED=0`). Pin v1.4.6.
- `shared/index.nix` already supports `kind="tool"` after the
  mariadb rename.
- Cross-platform tarballs (linux x86_64 glibc + musl, macOS arm64),
  manifest path `tools/mkcert/1.4.6/<target>/<tag>.json`.

**Bougie-side files**:
- `src/commands/server/tls.rs`:
  - On first TLS need: ensure mkcert is in the store
    (`$BOUGIE_HOME/store/mkcert-<version>-<hash>/bin/mkcert`),
    fetch if absent via existing index machinery.
  - Run `CAROOT=$BOUGIE_HOME/tls/mkcert-ca/ mkcert -install`; capture
    output for the user.
  - Write `$BOUGIE_HOME/state/server-tls.json` marker.
  - `bougie server tls install` / `bougie server tls uninstall` CLI.
- `src/commands/server/run.rs` — when `[server].listen` resolves to a
  TLS-bound address, also start a `rustls`-wrapped axum listener with
  per-host SNI cert lookup.
- Per-host cert cache at `$BOUGIE_CACHE/server/tls/<hostname>/` —
  lazy issuance via `mkcert -cert-file ... -key-file ... <hostname>`,
  refresh inside the 30-day-from-expiry window.

**Verification**:
- `bougie server tls install` succeeds; browser trusts certs from
  bougie's CA after.
- `https://myapp.bougie.run:7443/` loads without warning.
- `bougie server tls uninstall` removes the CA cleanly.

---

## Cross-cutting concerns

**Logging**: every per-request span emits a single completion event
matching SERVER.md §9 schema. `tracing-subscriber` switches between
the `text` formatter (one-line ANSI) and a custom JSON formatter that
embeds `schema_version: 1` and the field set spec'd. Background events
(`pool_start`, `pool_idle_out`, `pool_reload`, `pool_crash`,
`pool_evicted`) follow the same schema but with type set accordingly.

**Errors**: align with CLI.md §8 exit codes. Server-mode-specific
codes (502 backend, 504 backend timeout, etc.) are HTTP-layer; the
exit code only matters for fatal startup errors (port in use, config
invalid, missing PHP install).

**Config validation at startup**: parse server.toml, resolve every
`[[host]].project` to an extracted PHP install (refusing to start
if a project lacks `bougie sync`), pre-warm `<project-hash>` paths.
Surface all problems before binding the listener.

**`bougie sync` integration**: a project must have a resolved PHP
version + conf.d for `bougie server` to route to it. Document; do not
auto-sync. (Possibly add a `--auto-sync` flag in a later iteration.)

**Process management on shutdown**: SIGINT/SIGTERM → stop accepting
new connections → drain in-flight requests (5s ceiling) → SIGTERM
every spawned php-fpm master → `waitpid` each → exit. SIGKILL fallback
after another 3s.

---

## Risks

- **FastCGI implementation cost**: 500 lines is optimistic. Real cost
  is more like 800–1200 including the param encoding, fragment
  splitting, and chunked stdin handling. If pulling a maintained crate
  saves a week, reconsider that call in phase 2.
- **macOS path watching**: `notify`'s FSEvents backend has known
  coalescing quirks. Debounce + rescan-on-event is the standard
  workaround; test on macOS in CI.
- **php-fpm signal handling**: SIGUSR2 reload semantics differ subtly
  between major php-fpm versions. Verify against every PHP version in
  bougie's matrix during phase 4.
- **Pool startup time**: cold-start latency adds to first-request
  perceived latency. Health-probe timeout (2s) sets the upper bound on
  user-visible delay; consider extending if php-fpm boot is heavier
  than expected on cold caches.
- **Hostname conflicts**: two `[[host]]` blocks with identical
  hostnames is an error at startup. Aliases (`[[host.alias]]`) must be
  globally unique too.

---

## Rough time estimate

| Phase | Estimate | Cumulative |
|---|---|---|
| 0. Config & helpers | 1–2 d | 2 d |
| 1. Static HTTP | 2–3 d | 5 d |
| 2. FastCGI + normal pool | 4–5 d | 10 d |
| 3. Xdebug routing | 2–3 d | 13 d |
| 4. Pool lifecycle | 3–4 d | 17 d |
| 5. /etc/hosts | 1–2 d | 19 d |
| 6. Control socket | 1 d | 20 d |
| **v1 total** | **14–20 days** | |
| 7. TLS (v1.x) | 4–5 d + index work | |

Days here are focused-coding days, not calendar days. Real schedule
depends on intercurrent index work, php-build-standalone tarball
updates, and how often the FastCGI implementation surprises us.

---

## Open decisions deferred to implementation time

- **FCGI keep-alive / multiplexing**: v1 ships one connection per
  request. Revisit if cold-connect overhead dominates a perf trace.
- **Body size limits**: hardcode a 32 MB cap for both directions in
  v1; expose as `[server].max_body_size` later if anyone hits it.
- **HTTP/2**: axum supports it via `hyper-util`; defer turning it on
  until TLS lands (h2-cleartext is rare and adds complexity for no
  user-visible benefit in v1).
- **systemd-user integration**: if users want `bougie server` as a
  background service, document a sample unit file in v1.1 rather than
  shipping one.
