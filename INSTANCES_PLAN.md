# Multi-instance services + port fallback — implementation plan

Working plan for two coupled capabilities:

1. **Port fallback** — when a service's fixed catalog port is already in
   use (the developer's own rabbitmq on 5672, a stray opensearch on
   9200), relocate to a free port instead of failing the start.
2. **Version-keyed service instances** — run more than one version of a
   service at once (`elasticsearch 7.x` alongside `8.x`), and different
   services that share a default port at once (`elasticsearch` alongside
   `opensearch`, both 9200).

These land together because they share one root change: a service stops
being a **singleton keyed by name** and becomes an **instance keyed by
`(name, version)`**, with its ports resolved at runtime rather than read
from a compile-time constant.

Phases are bottom-up; stop at any phase and the preceding work still has
value (Phase 0–1 give the instance model, Phase 2 adds fallback, Phase 3
makes it user-visible).

## Why this is bigger than "save a different port"

Everything about a service today is baked in at compile time and keyed
by name:

- The catalog is a `const CATALOG: &[CatalogEntry]`
  (`crates/bougie-daemon/src/daemon/catalog.rs`); each entry's port is a
  `Binding::Tcp { port }` literal (opensearch 9200 `:208`, rabbitmq 5672
  `:234`, mailpit 1025 `:257`, server 7080 `:276`).
- The supervisor holds `services: HashMap<&'static str, ManagedService>`
  (`supervisor.rs:297`), seeded once from the const catalog
  (`supervisor.rs:310-311`) — **one slot per name**, key borrowed
  `&'static str` from the catalog.
- The up IPC's `ServiceRequest { name, tenant }` (`ipc.rs:165`) **carries
  no version**; `spawn_service` runs `entry.tarball` — always the catalog
  default. A `[services] mariadb = "10.6"` pin never reaches the daemon
  as a running-version selector.
- Per-instance runtime state is name-only:
  `state/services/<name>/{data,run/<sock>,conf,log,tenants.json}`
  (`bougie-paths/src/lib.rs:376-405`).
- The tenant ledger (`tenants.rs`) records identity + `secrets` + `alloc`
  but **no port** — every connection value is re-derived at read time
  from the const catalog binding (`tenant_env.rs:50`).

The only thing already version-keyed is the **store**: extracted binaries
live at `store/<tarball>` = `store/mariadb-11.4.4`
(`store_layout.rs:12`), so multiple versions can be *installed* side by
side — just not *run* side by side. The store is telling us the model;
this plan extends it into the runtime state tree.

## Mental model

An **instance** is one running copy of a service, identified by
`Instance { name, version }`. Its canonical id is the **tarball name** —
`format!("{name}-{version}")`, e.g. `elasticsearch-8.13.4` — because
that's already how the store keys extracted binaries.

Everything per-instance keys off that identity:

```
state/services/<name>/<version>/
    ├── data/            datadir            (collision #1 for two versions)
    ├── run/<sock>       unix socket        (collision #2 for db/redis)
    ├── conf/            rendered config
    ├── log/<name>.log   service log
    ├── tenants.json     per-instance ledger
    └── endpoint.json    per-instance effective ports   ← "saved ports"
```

The **catalog stays keyed by name.** Binding *kind* (tcp vs socket),
client tools, tenancy strategy, and sandbox stance are
version-independent — the catalog entry supplies the shape. The
instance's resolved version only selects the tarball/binary and the
concrete port(s). `CatalogEntry.version` / `.tarball` become *defaults*
used when no version is pinned.

### What coexists, and what doesn't

| Scenario | Enabled by | Works? |
| --- | --- | --- |
| `redis` + `mariadb` (different names) | already true today | ✓ |
| `opensearch` + `elasticsearch` (different names, both default 9200/9300) | **port fallback** (version-keying irrelevant — different names already isolate state) | ✓ once an `elasticsearch` catalog entry exists |
| `elasticsearch 7.x` + `8.x` (same name, two versions) | **version-keying** (isolates state) **+ port fallback** (9200/9300 clash) | ✓ at the host level |
| One project wired to *both* ES 7.x and 8.x at once | — | ✗ by design (see env-prefix boundary) |

The env-connection vars are prefixed `BOUGIE_SERVICE_<NAME>_` — keyed by
**name, not version** (`tenant_env.rs:43`). So a single project is wired
to exactly one version per service name; the natural shape is *project A
pins 7.x, project B pins 8.x*, each project's env pointing at its own
instance. The offline resolver (below) encodes the same assumption. A
single project consuming two versions of one service would need
version-suffixed env vars (`BOUGIE_SERVICE_ELASTICSEARCH_8_PORT`) — an
explicit later extension, not the default; it complicates the common
case for a rare one.

## Design

### 1. `Instance` type + version-keyed paths *(largest mechanical surface)*

- New `Instance { name, version }` (owned) with `.tarball()` →
  `format!("{name}-{version}")` and a `.dir_segment()` for the version
  path component. Add an owned `InstanceId` for map keys (the tarball
  string, or a `(String, String)` newtype).
- Every `Paths::service_*(name)` helper gains a version and takes the
  instance (or `name, version`): `service_dir`, `service_data`,
  `service_run`, `service_conf`, `service_log`, `service_log_file`,
  `service_tenants` (`bougie-paths/src/lib.rs:376-405`). This is the
  broad-but-mechanical edit — both investigations found these threaded
  through the supervisor, ipc, tenant_env, diagnose, service/exec, and
  ide.
- `tarball_for(name, version)` replaces reads of the const
  `entry.tarball`; `store_layout::basedir` resolves against it
  (`store_layout.rs:32`).

### 2. Supervisor: name-singleton → instance-keyed + lazy creation

- `HashMap<&'static str, ManagedService>` → `HashMap<InstanceId,
  ManagedService>`. `ManagedService.name: &'static str` (`supervisor.rs:122`)
  and the mirrored `ServiceStatus.name` (`:194`) carry owned
  `name + version`. The `&'static str` → owned change ripples through
  `PendingHealth.name` (`:235`), tracing fields, and error strings — the
  single biggest churn in the plan.
- `Supervisor::new` can no longer pre-seed a Stopped slot per catalog
  entry (`:310-311`). **Instances are created lazily on first `up`.**
  This is a real lifecycle shift: today every service exists as a Stopped
  slot from birth; afterward the map holds only instances that have been
  requested. `status`/`down`/`restart` handle "unknown instance"
  gracefully.
- Backoff, health-miss, and restart bookkeeping are already
  per-`ManagedService`, so they become per-instance for free.

### 3. Version through the up IPC *(schema bump)*

- `ServiceRequest { name, tenant }` → `{ name, version, tenant }`
  (`ipc.rs:165`). The CLI resolves the project's pin (`[services]
  mariadb = "11.4"`, `ServicePin`) to a concrete version **before** the
  call; the daemon keys the instance by it.
- `down` / `restart` / `logs` / `env` / `status` args identify instances
  by `(name, version)`. Bump `SCHEMA_VERSION` (`ipc.rs`) — the wire shape
  of `ServiceRequest` changed (SERVICES.md §7 is v1; this is v2).

### 4. Per-instance port allocation + fallback *(the original ask)*

Add `endpoint.json` per instance:

```json
{ "primary": 9201, "extra": { "transport": 9301 } }
```

In `spawn_service`, replacing the current hard-fail branch
(`supervisor.rs:455-464`), for each TCP port the instance needs:

1. If `endpoint.json` records a port and it is still bindable → **reuse
   it** (sticky, so a restart doesn't strand config that cached the
   port).
2. Else if the catalog default is free → use it.
3. Else scan upward (`9200 → 9201 → …`, capped) for a free port.

Then persist `endpoint.json` and emit a **relocation warning** reusing
the holder attribution from `dfcab120`
(`ports::describe_holder`): *"port 9200 held by java (pid 4321);
elasticsearch-8.13.4 will use 9201 instead."* This **replaces** today's
hard error (`supervisor.rs:455-464`). The anti-masquerade property that
motivated `dfcab120` is *preserved and strengthened*: we never bind the
foreigner's port, we bind our own free one, so the health probe can't
accidentally pass against someone else's live instance.

Because the probe tests *actual* availability (`ports::port_in_use`,
`ports.rs:24`), it handles **cross-service** collisions with no
special-casing: whichever of opensearch/elasticsearch comes up second
sees 9200 held by its sibling and relocates.

Thread the effective port(s) into the sites that bypass `entry.binding`
today:

| Site | File | Field |
| --- | --- | --- |
| opensearch http | `supervisor.rs:1343` | `-Ehttp.port` |
| opensearch **transport** | `supervisor.rs:1329-1348` | **add** `-Etransport.port` (see below) |
| opensearch provisioning | `provisioners/opensearch.rs:25` | `OPENSEARCH_BASE_URL` |
| rabbitmq | `provisioners/rabbitmq.rs:310` | `RABBITMQ_NODE_PORT` (also feeds `rabbitmqctl`) |
| server | `supervisor.rs:1325-1326` + `provisioners/bougie_server.rs:365` | `--listen` + seeded `server.toml` |
| mailpit smtp + http | `supervisor.rs:1397-1398` | `--smtp` / `--listen` |
| health probe | `health.rs:98` (mailpit `:65`) | connect port |

### 5. Multi-port services

The single-field `Binding::Tcp { port }` models one port; several
services bind a second:

- **mailpit** — SMTP (catalog binding) + HTTP web UI 8025
  (`MAILPIT_HTTP_PORT`, `catalog.rs:22`), already surfaced as
  `_DASHBOARD_URL`.
- **opensearch / elasticsearch — transport 9300.** Even in
  `discovery.type=single-node`, the node binds the transport port
  (default 9300), and today's exec args pin only `-Ehttp.port=9200`
  (`supervisor.rs:1343`) — transport is left unmanaged. So two search
  engines up at once collide on **9300** as well as 9200. Fix: add
  `-Etransport.port=<effective>` to the exec args and carry it in the
  `endpoint.json` `extra` map.
- **rabbitmq — epmd 4369.** Shared Erlang port-mapper, *reusable* (a
  stale epmd is fine — see the test note in `dfcab120`). Leave on 4369 in
  v1; relocate via `ERL_EPMD_PORT` only if real conflicts surface. Note
  it in `endpoint.json` for diagnose visibility but don't fail on it.

The `extra` map generalizes all of these: each named secondary port runs
the same free/sticky/scan allocation as the primary.

### 6. Offline resolution via ledger-scan *(no resolver off-daemon)*

`bougie run` env, `service credentials`, PhpStorm datasources, and
diagnose run **without the daemon** and today read
`state/services/<name>/tenants.json` + derive the port from the const
catalog (`tenant_env.rs`, `credentials.rs:131`, `exec.rs:246`,
`ide.rs`). With per-instance state they must find *which version's*
ledger + endpoint to use.

Rule: **scan `state/services/<name>/*/tenants.json` for this project's
row.** The parent directory names the version, so the scan lands on the
correct instance's ledger *and* its `endpoint.json` for the effective
ports. Deterministic, offline, needs no version resolver. It finds
exactly one row per service name (the one-version-per-project-per-name
assumption above); finding two would be the ambiguity that
version-suffixed env vars would later resolve.

`tenant_service_env` (`tenant_env.rs:37`) gains the effective endpoint as
an input instead of reading `entry.binding`; the `_HOST/_PORT/_URL/_DSN/
_DASHBOARD_URL` construction reads from it. The catalog default is the
fallback when no instance has ever recorded an endpoint (fresh /
never-upped).

### 7. Server carve-out

The dev `server` (`catalog.rs:268`) is a **shared singleton by design** —
one server multiplexes every project's `<name>.bougie.run` host. It does
**not** get versioned per project; its "version" is the running bougie
binary (`current_exe()`). It *does* participate in **port fallback**
(7080 can be squatted) but stays a single instance. Concretely: `server`
keeps a name-only state path and a single supervisor slot; only its port
resolves through `endpoint.json`.

### 8. Migration

Existing users have provisioned tenants + (large) datadirs under
`state/services/<name>/…`. On daemon start, one-time **rename** legacy
`state/services/<name>/{data,conf,run,log,tenants.json}` into
`state/services/<name>/<default-version>/`, guarded by a marker file so
it runs once. Rename (not copy) — datadirs are big. `server` is exempt
(stays name-only).

## Supply prerequisites (tracked separately)

The runtime model here works regardless of the index, but it has nothing
to *choose between* until the supply side offers it:

- **Multiple versions per service in the index.** The catalog pins one
  version per name today; multi-version selection needs the index
  (`php-build-standalone`, `SERVICES.md`) to publish several tarballs per
  service and a resolver to map a pin → concrete version. Coordinate
  upstream.
- **An `elasticsearch` catalog entry.** Only `opensearch` exists.
  Elasticsearch is a separate product (opensearch is a 7.10 fork), so
  it's a new `CatalogEntry` + index tarball + provisioner — the
  provisioner can largely clone opensearch's since the ES 7.x REST
  surface is near-identical. Orthogonal to this plan; the plan makes it
  *coexist* once added.

## Phases

**Status (2026-07-08):** Phases 0–4 landed; the first real multi-version
service (`mysql` 8.4 + 8.0) runs end-to-end. Phase 3's offline ledger-scan
is done for the connection-critical consumers (`tenant_env` / `bougie run`
env / `credentials` / client-exec wiring / `projects list` / default-tenant
derivation / `down` / daemon-restart restore) via
`tenants::instance_versions`; `service status`, `diagnose`, and `ide`
datasource writing still resolve the catalog **default** version (display /
IDE-nicety only, tracked as remaining Phase 3). Phase 5 (SERVICES.md, full
migration validation) outstanding.

| Phase | Deliverable | Status |
| --- | --- | --- |
| 0 | `Instance`/`InstanceId` type, version-keyed `Paths` helpers, `endpoint.json` model + allocator (unit-tested), legacy-state migration. No behavior change — default version only. | ✅ |
| 1 | Version through the up IPC (`ServiceRequest.version`, schema bump), instance-keyed supervisor map, lazy instance creation, per-instance spawn/health/restart from the resolved tarball. | ✅ |
| 2 | Per-instance port allocation + fallback; effective ports threaded into every exec/provisioner/health site (incl. the new `-Etransport.port`); hard-fail → warn+relocate. | ✅ |
| 3 | Offline + CLI: ledger-scan resolution in `tenant_env`/`credentials`/run-env/ide; `service status`, diagnose, and `service catalog` show version + effective ports. | 🟡 core done; status/diagnose/ide display still default-version |
| 4 | Version resolution + multi-version catalog/index (coordinate `SERVICES.md`); pin → concrete version in the CLI. | ✅ `mysql` entry (8.4/8.0), constraint-based `resolve_service_version`, `mysqld --initialize-insecure` provisioner, version-aware sandbox; real 2-version smoke `phase25` (gated `BOUGIE_SKIP_REAL_MYSQL`) |
| 5 | Docs (`SERVICES.md`), tests, migration validation. | ⬜ |

## Testing

Testing is first-class here, not an afterthought, for one specific
reason: this change **inverts an assumption the current service suites
are built on** (start-fails-on-occupied-port) *and* introduces
concurrency (two instances at once) that nothing tests today. The
guiding principle is **fakes-first**: every bit of new logic —
version-keyed paths, instance-keyed supervisor, the port allocator,
`endpoint.json`, ledger-scan, migration — must be provable *without*
downloading a 274 MB opensearch tarball + JVM. Real-service suites stay
as thin smoke coverage on top.

### How service tests work today (the ground we build on)

- **Harness** (`tests/common/mod.rs`): `TestEnv` = isolated temp
  `BOUGIE_HOME`/`BOUGIE_CACHE` + an `assert_cmd` builder driving the real
  `bougie` binary; `TestProject` seeds a `composer.json` whose basename
  fixes the derived tenant.
- **Fakes vs real.** `tests/fixtures/fake_redis.rs` is a ~80-line
  stand-in that binds `--unixsocket` and speaks just enough RESP to pass
  the health probe — but it's **socket-only; there is no fake TCP
  service.** Real services (opensearch/mariadb/mailpit/rabbitmq) use
  `tests/common/*_fixture.rs` to download the real tarball (sha-pinned,
  cached in `~/.cache/bougie-test-fixtures/`), gated per-platform
  (`compile_error!` off-target) and by `BOUGIE_SKIP_REAL_*`.
- **Staging trick.** A fake is substituted by copying its binary to the
  catalog's expected store path (`store/redis-8.6.3/bin/redis-server`,
  `phase14:29`); the daemon then spawns it as "redis". **There is no
  catalog test-injection seam** — the catalog is a hard `const`
  (`catalog.rs:158`, `find` at `:328`). So tests can't invent new service
  *names*, but they *can* stage a fake at any `store/<name>-<version>/`
  the resolved instance points to.

### Layer 1 — Unit tests (fast, every CI run, no network)

Colocated `#[cfg(test)]`, extending the existing blocks in
`ports.rs`/`tenant_env.rs`/`catalog.rs`/`tenants.rs`:

- **Allocator** (new `endpoint.rs`): default-free → uses default;
  default-taken → scans upward; **sticky** (recorded port still free →
  reused; recorded port now taken → re-scan); **multi-port** (primary +
  `extra` allocated independently); scan cap → error. Use the
  bind-`:0`-then-drop trick already in `ports.rs` tests to source
  guaranteed-free/held ports deterministically.
- **`Instance`/`InstanceId`**: `.tarball()`/`.dir_segment()` derivation;
  path helpers yield `state/services/<name>/<version>/…`.
- **`endpoint.json`**: serde round-trip; missing file → catalog-default
  fallback; unknown keys tolerated (forward-compat).
- **`tenant_service_env` with an injected endpoint**: HOST/PORT/URL/DSN
  reflect a *relocated* port, not the const; the existing byte-stability
  assertions still hold.
- **Ledger-scan resolution**: over a `state/services/<name>/{v1,v2}/`
  layout, find the project's instance; exactly-one invariant; zero-match
  → catalog default; two-match → the defined error.
- **Migration**: legacy `state/services/<name>/…` → renamed under
  `<default-version>/`; idempotent via marker; tenants + datadir survive;
  `server` exempt.

### Layer 2 — Fake TCP service *(the key new test infra)*

Generalize `fake_redis.rs` into a `fake-tcp-service` bin (new `[[bin]]`,
`required-features = ["test-fixtures"]`) that parses the `--listen`/
`-Ehttp.port`/`--smtp` shapes the daemon renders, binds those ports,
answers a trivial line/HTTP health probe, and waits for SIGTERM. Stage it
at `store/<name>-<version>/…` for an existing TCP service name and drive
versions through the new IPC `version` field — **no catalog change
needed.** This makes the whole feature testable in-process, fast, deterministic:

- **Port relocation (the original ask):** hold an in-process squatter on
  the catalog port, `up`, assert start *succeeds*, `endpoint.json` +
  `status` report the relocated port, the relocation warning is emitted,
  and `service.env` / `credentials` hand out the new port.
- **Two-version coexistence (the ES 7.x+8.x shape):** request one TCP
  service at two versions (fakes staged at `store/<name>-A/` and
  `…-B/`); assert **distinct datadirs**, **distinct relocated ports**,
  **independent tenant ledgers**, and that each project's env points at
  its own instance.
- **Sticky across restart:** `up`, note the port, restart the daemon,
  `up` again → same port from `endpoint.json`.
- **Lazy lifecycle:** an instance appears in `status` only after it's
  requested (the singleton-slots-from-birth behavior is gone).
- **Cross-service collision (ES+OS shape):** covered *mechanically* now
  via squatter-on-default-port (the allocator path is identical whether
  the squatter is a foreign process or a sibling service); a genuine
  two-managed-services-sharing-9200 test waits on a real `elasticsearch`
  catalog entry.

### Layer 3 — Real-service suites (rework, don't expand)

`phase18/19/21/22` download real tarballs and encode the
**start-fails-on-occupied-port** assumption that `dfcab120` added and
Phase 2 removes:

- `common::wait_for_port_free` between daemon handoffs and the
  `BOUGIE_SKIP_REAL_SERVER` / masquerade gates exist *because* a stray
  listener made `up` fail. Rework them to **assert relocation** instead:
  bring the service up with its catalog port squatted, assert it lands on
  a relocated port (learned from `status`/`endpoint.json`) and still
  provisions its own tenant. Net simplification — deletes the
  wait-for-JVM/Erlang-teardown dance and a class of flakiness.
- Keep **one real happy-path smoke per service** on the default (free)
  port — unchanged fidelity that the fakes don't provide (real RESP,
  real cluster health, real AMQP).
- Add **one real two-version smoke** (e.g. opensearch on two published
  versions) — but this is **gated on the supply prereq** (index must ship
  a 2nd version). Until then it's `#[ignore]`d with the reason inline;
  the Layer-2 fake carries the two-version mechanics in CI.

### Layer 4 — CI, platforms, determinism

- **The real-service suites already run *unconditionally* in CI.**
  `ci.yml`'s ubuntu + macos jobs run a plain `cargo test --features
  test-fixtures` and set **no `BOUGIE_SKIP_REAL_*` vars** — so every push
  today downloads real mariadb/opensearch(274 MB)/rabbitmq/mailpit and
  boots real JVMs/Erlang, serialized behind per-binary mutexes. There is
  no separate real-services job and no opt-in gate. `test-fixtures` gates
  only `fake-redis`, not the real suites.
- **CI-cost trap to avoid.** A naive multi-instance test that boots
  *two real* instances would double an already-heavy leg on every PR.
  The fakes-first design is what keeps this in check: **all two-instance
  and relocation mechanics run on the fake-TCP bin (no download)**;
  relocation smokes on real services only add a cheap in-process squatter
  (one extra `TcpListener`), never a second real boot; and the real
  two-version smoke stays `#[ignore]`/opt-in. **Decision to make:** take
  the opportunity to carve the real-service suites into a **dedicated
  opt-in CI job** (or wire the existing `BOUGIE_SKIP_REAL_*` gates into
  the default job), so the fast unit+fake tiers gate every PR and the
  slow real tier runs on a schedule / label. Recommended — the refactor
  is the natural moment, and it de-risks the added instances.
- **Unix-gated E2E, cross-platform units.** Service supervision is
  Unix-only, so the daemon E2E stay Unix-gated (windows is build-only +
  `windows_smoke`); the pure `Instance`/path/allocator logic compiles
  everywhere and gets `windows_smoke` lib coverage.
- **No `daemon start` seam.** The daemon auto-spawns on the first IPC
  call; the only subcommands are `status`/`stop`/`version`, and each
  real-service suite reimplements its own `stop_daemon()`. The
  sticky-across-restart test therefore drives *stop → next `up`
  auto-respawns*, not an explicit restart. If instance identity changes
  daemon lifecycle, note there's no single harness seam — a shared
  `common::stop_daemon` is worth extracting as part of this.
- **No race-dependent tests.** The pre-spawn probe is *advisory* — the
  child's bind is authoritative, so there's an inherent TOCTOU (probe
  says free → someone grabs it → child hits EADDRINUSE). The design
  guarantee is "**a lost race degrades to a backoff retry that re-scans,
  never a wedge**"; tests assert that retry path via the existing backoff
  machinery rather than trying to win/lose a race. All port tests use
  dedicated in-process squatters held for the test's lifetime and
  `:0`-sourced free ports — never `sleep`; poll `status`/`endpoint.json`.
- **Happy-path assertions survive; only relocation tests move.** When the
  default port is free (the CI norm), the allocator records the default
  in `endpoint.json`, so existing `:9200`/`:7080` env + URL assertions
  still hold. What breaks is anywhere a test *forces* a conflict — those
  become "assert the relocated value," read from `status`/`endpoint.json`
  rather than a literal.

### Phase-by-phase test gates

Each phase lands with its own tests green before the next builds on it:

| Phase | Test gate |
| --- | --- |
| 0 | Existing suite is the **oracle** — Phase 0 is a pure refactor, so all of `phase12–22` pass unchanged except path-shape assertions. New units: `Instance`/paths, `endpoint.json`, migration. |
| 1 | Fake-TCP two-instance + lazy-lifecycle E2E; IPC schema round-trip (v2 `ServiceRequest`). |
| 2 | Allocator units; fake-TCP relocation + sticky + cross-collision; reworked real-service relocation smokes. |
| 3 | Ledger-scan units; offline `credentials`/`run`-env/diagnose report relocated ports (extend `phase13_services_offline` + `service_credentials`). |
| 4 | Real two-version smoke un-`#[ignore]`d once the index publishes a 2nd version. |

## Decided

- **Server: port-fallback only, never versioned** — it's the shared dev
  server.
- **No strict-mode knob for v1** — the relocation warning already tells
  you what happened; a `BOUGIE_STRICT_PORTS` opt-out is easy to add later
  if CI wants hard-fail.
- **Instance key = tarball-exact (patch-level)**, matching the store;
  addressed by the user's minor-version pin.
- **One version per project per service name** — encoded in both the env
  prefix and the offline ledger-scan; version-suffixed env vars are a
  later opt-in, not the default.
