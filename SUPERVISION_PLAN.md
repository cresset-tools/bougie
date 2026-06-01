# Service supervision: cgroup-v2 backend + fallback

Status: design. No implementation yet beyond the existing process-group +
`PR_SET_PDEATHSIG` floor (shipped in `bougie-babysit`).

## Problem

bougied supervises each service through a `bougie-babysit` shim that:

1. puts the service in its own process group (`setpgid(0,0)`), and
2. `killpg`s that group on the stop paths — SIGTERM, control-socket EOF
   (bougied died), or service self-exit — escalating SIGTERM → SIGKILL
   after a grace window.
3. (new) sets `PR_SET_PDEATHSIG = SIGKILL` so a babysit that dies
   *without* running cleanup (OOM killer, `kill -9`) still takes its
   direct service child down.

This is correct for well-behaved services but leaks in two cases the
process-group model fundamentally can't cover:

- **Process-group escapees.** A process that `setsid()`s / daemonizes
  leaves the group, so `killpg` never reaches it. The live offender is
  Erlang's `epmd`, which rabbitmq spawns as `epmd -daemon`; it routinely
  survives `bougie down` and lingers (observed: `epmd -daemon` alive with
  no bougied). `PR_SET_PDEATHSIG` only signals the babysit's *direct*
  child, so it doesn't catch escapees either.
- **Crash orphans with lost state.** The supervisor tracks each service's
  pgid only in memory (`Service::service_pgid: Option<i32>` in
  `supervisor.rs`). If bougied itself dies, the babysit's socket-EOF path
  normally cleans up — but if babysit *also* died abnormally, the group
  is orphaned and there is no on-disk record for a fresh bougied to find
  and reap it. The next `bougie up` then races a stale instance (port /
  datadir conflicts).

Foreground mode (every service launched non-daemonized — see
`render_exec_args`) is a prerequisite and is already satisfied for all
services except the `epmd` escapee. It is necessary but not sufficient:
it keeps the *main* service in the group; it does nothing about a child
that deliberately leaves it.

## Goal

Reliably terminate a service's **entire** process subtree — escapees
included — on stop, on bougied crash recovery, and on startup-reap of
leftovers, **without requiring root**, while degrading gracefully on
hosts where the necessary primitives aren't available.

## Why cgroup v2

A cgroup is a kernel-tracked membership set that processes can't escape by
forking/`setsid`. cgroup v2 exposes:

- `cgroup.procs` — move a process (and thereafter its descendants) in.
- `cgroup.kill` (kernel ≥ 5.14) — write `1` to SIGKILL **every** member
  atomically, escapees included. This is the primitive `killpg` wishes it
  were.
- `cgroup.freeze` — fallback on < 5.14: freeze, enumerate `cgroup.procs`,
  kill each, thaw.

cgroups persist as directories, so a fresh bougied can enumerate leftover
service cgroups and reap them on startup — closing the crash-orphan hole
that the in-memory pgid can't.

`cgroup.kill` / `cgroup.procs` / `cgroup.freeze` are **core** interface
files: they work even with **no controllers delegated**, so we need no
cpu/memory/pids delegation just to kill reliably.

## Availability — NOT universal

Rootless cgroup management requires *all* of:

- **Linux** (macOS/Windows: no cgroups; macOS is a real target → must
  fall back).
- **cgroup v2 unified** (`statfs` magic `0x63677270` on `/sys/fs/cgroup`).
  v1 / hybrid → no unprivileged delegation.
- **A delegated, writable cgroup** — normally a logind
  `user@$UID.service` subtree. Absent under: non-systemd distros
  (OpenRC/runit), cron / non-PAM `ssh` / service-user contexts (no
  `user@$UID.service`), un-delegated containers, admin-disabled
  delegation.
- For `cgroup.kill`: **kernel ≥ 5.14** (else freeze+kill fallback).

Confirmed working unprivileged on the current dev box (kernel 6.19,
cgroup2fs, `user@1000.service` writable with `cgroup.kill`, delegated
controllers `cpu memory pids`, `mkdir` of a sub-cgroup succeeded). But a
meaningful fraction of users won't have this, so cgroups must be an
*opportunistic* backend, never the only one.

### Fallback matrix

| Environment                                   | Teardown mechanism                         |
|-----------------------------------------------|--------------------------------------------|
| Linux + v2 delegated + kernel ≥ 5.14          | `cgroup.kill` (best)                       |
| Linux + v2 delegated + kernel < 5.14          | `cgroup.freeze` + kill `cgroup.procs`     |
| Linux, no v2 / no delegation                  | process group + `PR_SET_PDEATHSIG`        |
| macOS                                         | process group (no pdeathsig equivalent)    |

The process-group + `PR_SET_PDEATHSIG` path stays as the portable floor.
cgroups *augment*, never replace it; both can run together as
defense-in-depth.

## Design

### Capability probe (bougied startup, once)

Produce a `SupervisionBackend` enum:

```
enum SupervisionBackend {
    CgroupKill   { base: PathBuf },   // <base> is our writable delegated cgroup
    CgroupFreeze { base: PathBuf },   // kernel < 5.14
    ProcessGroup,                     // current behaviour
}
```

Probe steps:

1. `statfs("/sys/fs/cgroup")` == `CGROUP2_SUPER_MAGIC`? else `ProcessGroup`.
2. Read `/proc/self/cgroup` → `0::<path>`; `base = /sys/fs/cgroup<path>`.
   Use **bougied's own** cgroup as the base — bougied may itself run
   inside a scope; never hardcode `user@$UID.service`.
3. Try `mkdir "<base>/bougie.probe.<pid>"`; if it fails → `ProcessGroup`.
4. `cgroup.kill` present in the probe dir → `CgroupKill`; else
   `cgroup.freeze` present → `CgroupFreeze`; else `ProcessGroup`.
   `rmdir` the probe.

### cgroup layout

```
<base>/                         # bougied's own cgroup (has bougied's pid)
  bougie.svc/                   # parent for service leaves
    <name>/                     # one LEAF per service (redis, opensearch, …)
      cgroup.procs              # babysit pid written here → service inherits
      cgroup.kill
```

- **Leaves only** hold processes. We do **not** enable controllers in
  `cgroup.subtree_control`, so v2's "no internal processes" rule never
  bites (it only constrains cgroups with controllers enabled) and bougied
  can keep living in `<base>` while owning `bougie.svc/*`. If we later
  want cgroup *limits* (memory caps etc.), bougied must first relocate
  itself into its own leaf (`<base>/bougie.svc/daemon`) before enabling
  controllers — out of scope here.
- Leaf name keys off the service name (+ tenant where the service is
  multi-tenant). Startup-reap kills/`rmdir`s everything under
  `bougie.svc/` since there is at most one bougied per user.

### Joining the cgroup

bougied creates `<base>/bougie.svc/<name>/` *before* spawning the babysit,
then passes the leaf path to the babysit (new `--cgroup <path>` flag).
The babysit, in `pre_exec` (before its existing `setpgid`), writes its own
pid into `<leaf>/cgroup.procs` (`write("0\n")` moves the caller). The
service inherited via the babysit's exec is then a member, and so is every
descendant — including anything that later `setsid`s. `setpgid` +
`PR_SET_PDEATHSIG` stay in place so the fallback path is always armed too.

### Teardown

`cleanup_group` / the supervisor's reaper grow a backend-aware step:

- `CgroupKill`: `write("1\n", "<leaf>/cgroup.kill")`, then `rmdir` the
  (now-empty) leaf. One syscall, atomic, catches escapees.
- `CgroupFreeze`: `write("1\n", cgroup.freeze)`, read `cgroup.procs`,
  SIGKILL each, `write("0\n", cgroup.freeze)`, `rmdir`.
- `ProcessGroup`: today's `killpg` TERM→grace→KILL.

Where cgroups are active, the babysit still does its graceful
SIGTERM→grace first (lets services flush), then the cgroup kill as the
"nothing escapes" backstop.

### Startup reap

On bougied start, if backend is a cgroup variant: enumerate
`<base>/bougie.svc/*`, and for each, `cgroup.kill` + `rmdir`. This is the
crash-orphan recovery the in-memory pgid can't provide. (No PID-reuse
guard needed — cgroup identity is the directory, not a recyclable pid.)

## Alternative considered: systemd transient scopes

Instead of manual cgroupfs, ask the **user** systemd over D-Bus
(`StartTransientUnit`, the API behind `systemd-run --user --scope`) to
create one transient scope per service. Pros: systemd owns delegation,
naming, and cleanup; `systemctl --user stop` / scope GC handles orphans.
Cons: adds a D-Bus dependency; same availability ceiling as manual
delegation (needs systemd-user anyway); less control over exact lifecycle.
Decision: **prefer the manual cgroupfs backend** — fewer deps, bougie
already does low-level process management, and it works against any
delegated v2 subtree, systemd-managed or not. Revisit D-Bus only if the
manual path hits a delegation wall.

## Implementation phases

1. **Probe + backend type.** `bougie-daemon`: capability probe →
   `SupervisionBackend`, plumbed into the supervisor. No behaviour change
   yet (still `ProcessGroup`). Unit-test the probe against synthetic
   cgroup trees.
2. **Leaf lifecycle.** Create/destroy leaf cgroups around service spawn;
   `--cgroup` flag on babysit + `pre_exec` join. Service still also gets
   pgroup/pdeathsig.
3. **cgroup teardown + reap.** Wire `cgroup.kill` (+ freeze fallback) into
   `cleanup_group` and the supervisor reaper; add startup reap.
4. **rabbitmq/epmd validation.** Confirm `bougie down rabbitmq` leaves no
   `epmd`. (Once cgroups catch it, the separate epmd-taming idea becomes
   optional.)

## Testing

- Probe unit tests over fabricated `/sys/fs/cgroup`-shaped temp trees
  (statfs can't be faked → factor the magic check behind a trait/arg).
- Integration test mirroring `crates/bougie/tests/babysit_shim.rs`: spawn
  a service that `setsid`s a child *out of* the process group, assert
  `killpg` would miss it but `cgroup.kill` reaps it. **Gate on the runtime
  probe** — skip (with a logged reason, never a silent pass) when no
  writable delegated v2 cgroup is present, since CI runners may lack
  delegation.
- Startup-reap test: leave a populated leaf behind, start a fresh
  supervisor, assert the leaf is killed + removed.

## Risks / open questions

- **Base discovery in odd contexts** (bougied under `systemd-run`, nested
  containers): `/proc/self/cgroup` is authoritative; the writable-mkdir
  probe is the real gate.
- **Controllers later wanted** (memory caps): requires relocating bougied
  into its own leaf first (no-internal-processes rule). Deferred.
- **`rmdir` timing**: a cgroup is only removable once empty; `cgroup.kill`
  is synchronous w.r.t. signal delivery but reaping is async — `rmdir`
  may need a brief retry loop.
- **Interaction with sandbox-run**: services already run under Landlock
  (Linux) via `pre_exec`; cgroup join is another `pre_exec` step and must
  be ordered before the sandbox lockdown if the sandbox would block
  `/sys/fs/cgroup` writes.

## Related

- `crates/bougie-babysit/src/lib.rs` — pgroup + `PR_SET_PDEATHSIG` floor.
- `crates/bougie-daemon/src/daemon/supervisor.rs` — spawn, in-memory
  `service_pgid`, the existing group reaper.
- Memory: services-foreground-mode + pdeathsig directive.

When shipped, delete this file (repo convention: shipped plans live in
git history).
