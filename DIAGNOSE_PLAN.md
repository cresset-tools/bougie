# bougie diagnose v2 — implementation plan

Working plan for making `bougie diagnose` reports rich enough to
actually diagnose service failures, putting the user in an `$EDITOR`
over the exact bytes that ship, and giving the maintainer a real
viewer for uploaded reports.

**Status: phases 1–3 implemented (2026-07-06), pending review.**
Phase 4 (collector ingest v2 + `/admin` viewer, cresset-tools/infra)
is the remaining work — and it must deploy **before** phases 1–3
ship in a release: the live collector rejects `schema_version: 2`.

Implementation note: the phase-1 pre-start probe surfaced a real
test-suite defect — with a developer's own stack running, the
`phase18`/`phase19`/`phase21` E2E suites' health probes were
*masquerade-passing* against the real opensearch/server/rabbitmq on
the shared catalog ports (and provisioning test tenants into them).
The probe now fails those runs fast and crisply; the suites gained
`wait_for_port_free` daemon handoffs and a `BOUGIE_SKIP_REAL_SERVER`
gate to match their siblings.

Motivating incident: a colleague's `bougie start` failed because the
rabbitmq port was already taken. The uploaded report contained only
the generic eyre chain — `bougied: rabbitmq: service `rabbitmq`
exited during startup (status 1); check `bougie service logs
rabbitmq` for the reason (service_start_failed)`. The actual
`Address already in use` line lived in
`$BOUGIE_HOME/state/services/rabbitmq/log/rabbitmq.log` on the
colleague's machine and never left it. On the receiving side, reading
the report meant ssh-ing into the telemetry box and poking
`diagnose_reports.body` with the sqlite CLI.

Three failures, three fixes, phased below:

1. the *error itself* should carry the cause (daemon-side),
2. the *report* should carry the surrounding evidence — logs, service
   state, port occupancy (client-side),
3. the *maintainer* should be able to read reports without ssh
   (collector-side), and the *user* should get an editor pass before
   anything is sent.

## Design principles

- **Correspondence, not telemetry.** Everything in TELEMETRY.md's
  diagnose paragraph stays true: independent of consent mode and
  `DO_NOT_TRACK`, never sent without explicit per-report
  confirmation, review before send. The new report is bigger, so the
  review mechanism grows up: from "scroll the terminal" to "edit the
  file".
- **What you edit is what ships.** The report becomes a Markdown
  document and that document — post-editor — is the payload,
  verbatim. No structured field survives outside it except fixed
  machine facts (bougie version, os/arch/libc) that are also visible
  in the text. Redacting a line in the editor removes it from the
  wire, full stop.
- **Offline-first, never mutate.** Collection reads disk state
  (`Paths` + `bougie_config::load_project` + log files). The daemon
  control socket is used only if `bougied.sock` is already live —
  diagnose must **never autospawn bougied** (today
  `connect_with_autospawn` in `commands/service/client.rs:133` would;
  diagnose gets a non-spawning connect). A diagnose run leaves the
  machine byte-identical.
- **Degrade section-by-section.** No daemon → service states read
  "unknown (daemon not running)" but log tails still ship. No
  project → services/ports sections are omitted with a one-line note.
  Windows (no services stack) → those sections are absent, the rest
  of diagnose works as today.

## The report, v2

Markdown, one document, sections in this order. Sample of the
motivating case:

```markdown
# bougie diagnostic report

## your notes

_(describe what you were doing — anything you type here is sent along)_

## environment

bougie 0.43.2 (a92fca7f) on linux-x86_64 (gnu)
telemetry mode: enabled
BOUGIE_* env set (names only): BOUGIE_HOME
project: ~/work/shop
declared services: mariadb 11.4, redis *, rabbitmq *, mailpit *
disk free: 41.2 GiB ($BOUGIE_HOME), 41.2 GiB (cache)

## last failure

command:   bougie start
category:  service_start_failed (exit 74)
error:     bougied: rabbitmq: service `rabbitmq` exited during startup (status 1); …

## daemon

bougied: running (pid 51023)

### bougied.log (last 200 lines)

```log
2026-07-06T09:14:02Z WARN service `rabbitmq` exited during startup (status 1)
…
```

## services

### rabbitmq (declared: *) — failed to start, gave up after 10 attempts

binding: 127.0.0.1:5672 — **in use by another process** (beam.smp, pid 4321)

#### rabbitmq.log (last 200 lines)

```log
BOOT FAILED: Address already in use (os error 98) — 127.0.0.1:5672
…
```

### mariadb (declared: 11.4) — running, healthy

binding: unix socket state/services/mariadb/run/mariadb.sock — present

#### mariadb.log (last 200 lines)
…

## ports

| port | wanted by | probe   | holder (best effort)        |
|------|-----------|---------|-----------------------------|
| 5672 | rabbitmq  | in use  | beam.smp (pid 4321)         |
| 4369 | rabbitmq (epmd) | in use | epmd (pid 4310)        |
| 3306 | mariadb   | free    | —                           |
```

Section sources, all mapped and reachable today:

- **environment** — as v1, plus: project root (home-folded) from
  `locate_project_root` (`commands/server.rs:244`), declared services
  from `bougie_config::load_project(...).bougie.services` (offline;
  same enumeration `service/up.rs:73-78` uses), and free-space for
  `$BOUGIE_HOME` / cache (statvfs via a tiny helper; skip on error).
- **last failure / re-run** — unchanged from v1.
- **daemon** — liveness = does `paths.bougied_sock()` accept a
  connect (with a short timeout, no autospawn, no retry). Log tail
  from `paths.bougied_log()` (`bougie-paths/src/lib.rs:334`), which
  the CLI already points bougied's stderr at
  (`service/client.rs:343-351`).
- **services** — one subsection per *declared* service. Live state
  (running / unhealthy / failed / restart counts) comes from the
  daemon `status` IPC call **only when the socket is already live**;
  that state is memory-only in `Supervisor` and unrecoverable
  otherwise — the section then says so. Log tail via
  `bougie_daemon::daemon::logs::tail_lines`
  (`daemon/logs.rs:148-193`, already handles missing files) on
  `state/services/<name>/log/<name>.log`. That path join currently
  exists twice (`supervisor.rs:463`, `ipc.rs:519`); rather than add a
  third copy, hoist it to `Paths::service_log_file(name)` and point
  all three at it. If the live log is shorter than the budget, do
  *not* walk into `.log.1` — the tail of the live file is the recent
  story; rotated files are a rabbit hole.
- **ports** — driven by the service catalog's `Binding` table
  (`daemon/catalog.rs`), not a blind system scan: for every declared
  service with `Binding::Tcp { port }`, probe by attempting
  `TcpListener::bind(("127.0.0.1", port))` — `EADDRINUSE` = in use.
  rabbitmq additionally probes epmd's 4369 (the known escapee; a
  small static extra-ports list in the diagnose collector, not a
  catalog change). `Binding::UnixSocket` reports whether the socket
  file exists. "In use" is only a *conflict* when our service isn't
  the one running — join against the live status when available.
  On Linux, enrich with holder attribution: parse
  `/proc/net/tcp{,6}` for LISTEN (state 0A), map socket inode → pid
  by scanning this user's `/proc/*/fd`, name from `/proc/<pid>/comm`.
  Std-only, best-effort, same-user visibility. On macOS the probe
  column ships without a holder (no `/proc`; we do not shell out to
  `lsof`).
- **server** — when the project is registered with the shared dev
  server, the vhost name and a host-filtered tail of
  `state/services/server/log/server.log` (the server is catalog
  service `server`; filter mirrors `server logs`,
  `commands/server.rs:413-428`).

### Scrubbing

Two passes over every log tail and free-text section, applied before
the draft is written (so the editor already shows scrubbed text):

1. **Home folding** — as today.
2. **Known-credential scrub** — bougie *knows* every secret it
   minted or read: tenant passwords from each
   `state/services/*/tenants.json` ledger, and tokens/passwords from
   the `auth.json` files bougie already consumes. Collect all values
   (≥ 6 chars), replace occurrences with `«redacted:tenant-password»`
   / `«redacted:auth-token»`. Exact-value replacement — no regex
   guessing.

The editor pass is the backstop for everything we can't know about.

### Size budget

Collector `MAX_BODY` is 256 KiB today; the diagnose route gets its
own 1 MiB limit (phase 4). Client-side budget, enforced regardless:

- per log tail: 200 lines via `tail_lines`, then byte-capped at
  16 KiB (drop oldest lines first, note the truncation in-section);
- re-run stderr: 64 KiB (unchanged);
- whole report: hard cap 384 KiB — if over, shrink the largest log
  sections proportionally and say so.

Worst realistic case (6 services + daemon + server + 64 KiB rerun)
lands around 230 KiB pre-JSON-escaping; comfortably inside both caps.

### LastFailure v2

`failure.rs` gains `schema: 2` with one new field: `project_dir`
(the cwd at failure time, if a project root was found). Diagnose run
later from anywhere still finds the right project's services. Loader
keeps accepting schema 1 (field just absent). A `--project <dir>`
flag on diagnose overrides both cwd and the recorded dir.

## The editor pass

Interactive default (`stdin` and `stdout` are TTYs): after assembly
and scrubbing, write the draft to
`<cache>/telemetry/diagnose-draft-<ts>.md` (mode 0600) with a
git-style instruction header:

```
# bougie diagnose — review before sending
# Everything BELOW the scissors line is exactly what will be sent.
# Edit freely: redact anything private, add context under "your notes".
# Delete everything (or save an empty report) to abort.
# ------------------------ >8 ------------------------
# bougie diagnostic report
…
```

Open `$VISUAL` → `$EDITOR` → platform default (`vi`; `notepad` on
Windows), via `sh -c '<editor> "$file"'` on Unix so editor values
with flags work. Then:

- editor exits non-zero → abort, keep the draft file, print its path;
- strip through the scissors line; result empty/whitespace → abort
  ("empty report, nothing sent");
- otherwise show `report is 38 KiB (412 lines) — send to the bougie
  developers? [y/N]` — still defaulting to **no**; an editor save is
  a review, not a consent.

Flag matrix (all existing flags keep their meaning):

| invocation            | editor | confirm | notes                          |
|-----------------------|--------|---------|--------------------------------|
| `diagnose`            | yes    | y/N     | new interactive default        |
| `diagnose --no-edit`  | no     | y/N     | v1 behavior: print full report |
| `diagnose --yes`      | no     | no      | CI/scripts; sends pristine     |
| `diagnose --yes --edit` | yes  | no      | edit, then send on save        |
| `diagnose --issue`    | yes    | —       | see below                      |
| no TTY                | no     | as v1   | refuses without `--yes`        |

`--issue` no longer dumps a large report to stdout: it writes
`./bougie-diagnose.md` (post-editor) and prints the path plus the
new-issue URL. GitHub issue bodies cap at 65,536 chars, so the hint
says "attach or trim" when the file is bigger than that.

The draft file is deleted after a successful send or a clean abort;
it is deliberately *kept* on editor failure so nothing typed is lost.

## Wire format v2

```json
{
  "schema_version": 2,
  "bougie_version": "0.43.2",
  "build_sha": "a92fca7f0",
  "os": "linux", "arch": "x86_64", "libc": "gnu",
  "report_md": "# bougie diagnostic report\n…"
}
```

`report_md` is authoritative and user-edited. The envelope fields
exist so the viewer's list column doesn't need to parse Markdown; all
of them are fixed machine facts of the same sensitivity class as the
User-Agent, and each also appears inside the visible report text.
Nothing else structured — deliberately: any structured field that
duplicated report content would survive an in-editor redaction.

## Phases

Bottom-up; each phase has standalone value.

### Phase 1 — the error itself carries the cause (bougie-daemon) ✅ shipped

Would alone have made the motivating report actionable, even at
schema v1.

- **Pre-start bind probe.** Before spawning a service whose catalog
  binding is `Tcp { port }`, try binding it. Occupied → fail fast
  with a crisp error: `port 5672 is already in use by another
  process (beam.smp, pid 4321) — rabbitmq cannot start`, holder via
  the same Linux `/proc` attribution helper (shared in a small
  module, used by both the supervisor and diagnose's ports section).
  This replaces the current "exited during startup (status 1)"
  mystery for the whole conflict class.
- **Log excerpt in the health-wait failure.** The two generic
  messages in `wait_for_health` (`supervisor.rs:1444`, `:1457`)
  append the last ~15 lines of the service's log. Those lines then
  flow into the eyre chain → `LastFailure.chain` → every diagnose
  report and every terminal, for free.

Tests: fixture service that squats a port (bind a listener in the
test, start the fake service against it); assert the error names the
port; fake service that prints a distinctive line and exits — assert
the line appears in the `service_start_failed` message.

### Phase 2 — report v2 collection (bougie client) ✅ shipped

New `commands/diagnose/` module split: `collect.rs` (sections),
`render.rs` (markdown), `scrub.rs`, existing flow in `mod.rs`.

- Section collectors per "The report, v2" above; services/daemon/
  ports collectors are `#[cfg(unix)]`.
- `Paths::service_log_file(name)` hoist (3 call sites).
- Non-spawning IPC connect: factor `client::try_connect` out of
  `connect_with_autospawn`; diagnose uses it with a 500 ms timeout
  for the one `status` call.
- Scrub pass + budgets.
- `LastFailure` schema 2 (`project_dir`), `--project` flag.

Ships before phase 3 without a collector change: the collector's
diagnose shape-check is loose (`schema_version == 1` is all it
verifies), so richer v1 JSON already ingests — but stays under
256 KiB until phase 4's limit bump deploys, so phase 2 temporarily
halves the per-log budget (8 KiB) if it ships first. Simpler: deploy
the phase-4 limit bump (one-line infra PR) ahead of everything.

Tests (extend `tests/diagnose.rs` `Env`): fake project with
`bougie.toml` `[services]`; planted log files under
`state/services/rabbitmq/log/`; assert tails + truncation marker;
plant `tenants.json` with a password that also appears in the log →
assert `«redacted:tenant-password»` and absence of the raw value;
assert no `bougied.sock`/process exists after a run (never-spawn);
ports section against a real bound listener.

### Phase 3 — editor pass + markdown payload (bougie client) ✅ shipped

- Draft file + scissors header + `$VISUAL`/`$EDITOR` resolution +
  abort semantics per "The editor pass".
- Flags `--edit` / `--no-edit`; `--issue` writes
  `./bougie-diagnose.md`.
- Upload switches to wire v2 (`report_md`).
- TELEMETRY.md diagnose paragraph rewritten (contents list, editor
  flow, unchanged retention/deletion promises). Not a consent-version
  event — diagnose is not telemetry and every send remains
  individually confirmed.

Tests: `EDITOR` pointed at a shell script that (a) appends a note →
assert it arrives in `report_md`; (b) deletes a planted fake-secret
line → assert absent on the wire; (c) truncates to empty → assert
abort + no request (wiremock `expect(0)`); scissors header never
ships; editor exiting 1 → abort, draft file survives.

### Phase 4 — collector v2 + viewer (cresset-tools/infra)

Ingest:

- `/v1/diagnose` accepts `schema_version` 1 *and* 2; v2 requires
  `report_md: String`. Route-scoped `DefaultBodyLimit` of 1 MiB
  (batch stays at 256 KiB); nginx `client_max_body_size` 1m on the
  vhost. This limit bump is deployable on day one, ahead of phases
  2–3.
- Storage unchanged: `diagnose_reports(id, received_at, body)`, body
  = full JSON. 180-day retention unchanged.

Viewer — the collector grows an `/admin/` surface, HTML in the same
single-file style as `dashboard.html`:

- `GET /admin/diagnose` — table: id, received (UTC), version,
  platform, summary (first `command:` line grepped from `report_md`
  at render time — so an in-editor redaction also redacts the list).
- `GET /admin/diagnose/{id}` — v2: envelope header + `report_md` in
  a `<pre>` (no markdown rendering; monospace is the right reading
  mode for logs). v1 rows: pretty-printed JSON.
- `GET /admin/diagnose/{id}/raw` — the stored JSON, for download.
- `POST /admin/diagnose/{id}/delete` — honors TELEMETRY.md's
  "deleted on request by report id" without sqlite gymnastics.

Auth is nginx's, not the collector's: a `location /admin/` block with
`basicAuthFile = /var/lib/nginx/diagnose-htpasswd`, provisioned once
by hand on the box (`echo "jelle:$(openssl passwd -apr1)" > …`) —
the flake auto-upgrades from a public repo, so the secret can't live
in it. The collector listens on loopback only; anything reaching
`/admin/` came through nginx's auth. `/admin/` responses set
`cache-control: no-store`. The public dashboard and `/data.json`
continue to expose zero diagnose data.

Tests: v1 + v2 ingest accepted, unknown schema rejected, >1 MiB
rejected; list renders the summary from `report_md`; delete removes
the row; `/admin` responses carry `no-store`.

## Rollout order

1. infra: limit bump + v2 ingest + viewer (phase 4) — deploy; accepts
   old and new clients from then on.
2. bougie: phase 1 (`fix(daemon): name the port conflict…`) — its own
   PR, independent value.
3. bougie: phase 2, then phase 3 (or one PR if review-sized).

No coordination hazard in either direction: old clients keep sending
v1 forever; the collector keeps accepting it.

## Decided questions

- **Markdown-authoritative payload** over structured JSON + comment
  field: editing structured JSON in `$EDITOR` is hostile, and any
  parallel structured copy would defeat in-editor redaction.
- **Confirm stays `[y/N]`** after the editor: a save is a review,
  not a consent.
- **No autospawn from diagnose**, ever — a diagnostic tool that
  changes the system it's diagnosing poisons its own report.
- **nginx basic auth** for the viewer over a collector-side token:
  zero auth code in the collector, browser-native, and the secret
  stays off the public flake repo.
- **No external tools** (`lsof`, `ss`) for port attribution — std +
  `/proc` on Linux, probe-only elsewhere.
- **Live log tail only**, no rotated generations — recency is the
  diagnostic signal; the budget is better spent across services.
