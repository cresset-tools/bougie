# Service health checks (protocol-aware, startup + continuous)

## Problem

When a service fails to start, or starts and then crash-loops, this is
invisible to someone running `bougie up` / `bougie start`:

- The **startup readiness probe** (`supervisor::wait_for_health`) is a bare
  TCP/Unix-socket *connect*. "Listening" ≠ "ready": opensearch answers on
  9200 long before the cluster is green; mariadb's socket can accept during
  crash recovery. So a connect probe can declare a not-yet-usable service
  `Running`.
- There is **no continuous health check**. Once `Running`, the only thing
  watched is process aliveness (`check_all` → `child.try_wait()`). A process
  that is *alive but wedged* reads as `Running` forever — never surfaced,
  never restarted.

## Goal

Per-service **protocol-aware** health probes, run **at startup** (gate the
`Running` transition on real readiness) **and continuously** (catch
wedged-but-alive services), feeding the existing failure-count / backoff /
give-up machinery, and surfaced in `bougie services status`.

The protocol-aware logic already exists, scattered across the provisioners —
this consolidates it and adds the continuous loop.

## Per-service probe

| Service | Probe | Reuses |
|---|---|---|
| redis | RESP `PING` over the unix socket → expect `+PONG` | redis provisioner's RESP pattern (it already speaks RESP, ships no `redis-cli`) |
| mariadb | `bin/mariadb --socket=… -e "SELECT 1"` exit 0 | `mariadb_client_binary` + `run_sql` |
| opensearch | HTTP `GET /_cluster/health` → `status != "red"` | `http_client` + `wait_for_cluster` |
| rabbitmq | `sbin/rabbitmqctl status --quiet` exit 0 | `ctl_binary` + `build_ctl_env` |
| mailpit | HTTP `GET :8025/` → 2xx (web UI; SMTP has no cheap probe) | reqwest (daemon dep) |
| server | HTTP `GET :7080/` → any `< 500` (HTTP layer up, host need not exist) | reqwest |
| jdk, erlang | none (`Binding::None`, runtime-only) | — |

Each is bounded by a per-probe timeout so a hung probe can't wedge the loop.

## Implementation

### 1. `daemon/health.rs` (new) — the dispatcher
`pub async fn probe(name: &str, paths: &Paths) -> Result<()>`, name-dispatched
(idiomatic here — cf. `health_timeout_for`, `sidecar_for`). Generic HTTP
probes (mailpit/server) live inline; protocol-specific ones delegate to a new
`pub(crate) async fn health(...)` on each provisioner module. Fallback for
anything without a richer probe = today's binding-connect (moved here from
`wait_for_health`).

### 2. Startup readiness
`wait_for_health` loops on `health::probe(name, paths)` instead of the bare
connect (keeping the early child-exit short-circuit + the `bougie services
logs` hint). Net effect: `up`/`start` now fail — clearly — when a service
comes up but isn't actually ready.

### 3. Continuous probing
New `ManagedService` fields: `health_misses: u32`, `next_health_at:
Option<Instant>`, `health_inflight: bool`, `last_health_ok: Option<Instant>`.

New `ServiceState::Unhealthy` — a live process (has a child, behaves like
`Running` for stop/snapshot/idempotence) that is failing its probe. Added to
every `matches!(state, Running | HealthChecking | Starting)` site.

New `Supervisor` methods:
- `health_due() -> Vec<&'static str>` — `Running`/`Unhealthy` services with a
  real probe whose `next_health_at` is due and not already in-flight; marks
  them in-flight.
- `record_health(name, ok) -> HealthOutcome` — clears in-flight, reschedules.
  ok → misses 0, `Unhealthy`→`Running`. fail below threshold →
  `Running`→`Unhealthy`. fail at threshold → `Breach`.
- `fail_unhealthy(name)` (async) — a breach is wedged-but-alive: tear the
  group down (reuse `stop`'s teardown) and schedule a backoff restart exactly
  like the crash arm of `check_all` (`FAILURE_RESET_THRESHOLD`,
  `compute_backoff`, give up past `MAX_RESTART_ATTEMPTS`). The existing `due`
  collection then respawns it.

Ticker (`daemon.rs`): after `check_all`, also `health_due()` and spawn each
probe **off the lock** (same discipline as start — a probe must never block
the lock); on `Breach`, `fail_unhealthy`.

Cadence: `HEALTH_INTERVAL` ≈ 5s; `HEALTH_FAILURE_THRESHOLD` ≈ 3 consecutive
misses before a breach (≈15s of sustained failure — avoids flapping on a
transient blip).

### 4. Surfacing
- `ServiceStatus` (snapshot) gains `health_misses` + `health_threshold`
  (the latter only when actively missing, so the CLI can render `2/3`).
- `services/status.rs` `render_text` now prints state + `failure_count` +
  `next_restart_ms` + health (today it drops all three). e.g.
  `opensearch  unhealthy  (health check failing 2/3)` /
  `redis  failed  restart in 4s (attempt 3/10)` /
  `mariadb  failed  gave up after 10 attempts — see 'bougie services logs mariadb'`.

## Out of scope (follow-ons)
- Live "⚠ crashed, restart 3/10" lines interleaved into `bougie up`'s log
  attach (needs a push event over the log stream).
- A post-`start` settle window / final health summary.

Delete this file once shipped (repo convention).
