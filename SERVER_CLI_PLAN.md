# bougie server — CLI redesign plan

Redesign of the `bougie server` **command surface**. The engine
(`bougie-server`: FastCGI, pools, xdebug routing, watcher, control
socket, TLS) is shipped per `SERVER_PLAN.md` and is *not* changing.
This plan changes only how the surface is exposed so the common case —
"serve the project I'm standing in" — is a zero-config, one-word command.

## Problem

The server engine is multi-tenant by `Host:` header on one shared port
(`127.0.0.1:7080`), with `<name>.bougie.run` as a public wildcard →
loopback (no `/etc/hosts` needed) and `.bougie.test` as the opt-in
`/etc/hosts` path. The daemon already does all host-registration
plumbing (`bougie up server` → writes a `[[host]]` block + reloads via
control socket).

But the CLI exposes the wrong front door:

1. **`server run --config P` is a primitive wearing the top-level name.**
   `--config` is mandatory with no project default, so `bougie server`
   — what a user naturally types in their project — just errors. The
   `SERVER_PLAN` helper commands meant to make this usable were dropped;
   the mandatory `--config` they justified stayed.
2. **Two competing models for "serve my app"** — manual
   `server run --config` vs managed `up server` — with no shared
   vocabulary and nothing to tell you which you want.
3. **`server list --config` is the only introspection** and it can't
   show live pool state without a hand-supplied config path. No
   `status`, `open`, `logs`.

## Locked design decisions

- **`bougie server` is a project-facing verb over the shared daemon**
  (Valet model, leaning into the existing arch). Windows (no bougied)
  falls back to a standalone foreground server.
- **Ctrl-C detaches** — the shared server keeps running (it may host
  other projects). Explicit teardown via `bougie server stop`.
- **Dedicated `server` subcommands** for observability (`status`,
  `logs`, `open`) — richer than the generic services verbs because they
  read the control socket's host/pool detail.

## New command surface

```
bougie server [NAME] [--open] [--tls] [--no-attach] [--no-sync]
    THE everyday command. In a project (Unix):
      1. locate project root (error if none)
      2. implicit `bougie sync` unless --no-sync
      3. ensure bougied is up + the `server` service is up; provision
         this project as a tenant → `[[host]]` for <NAME>.bougie.run,
         control-socket reload (i.e. `up server`, scoped to this project)
      4. print a URL banner: http(s)://<name>.bougie.run:<port>
      5. unless --no-attach, stream THIS project's request log in the
         foreground. Ctrl-C detaches (server keeps running); prints
         "still serving at <url>; stop with `bougie server stop`".
      6. --open also opens the browser.
    NAME overrides the auto hostname (label only; `.bougie.run` appended,
    or a full `*.bougie.test` host when /etc/hosts mode is on).
    Windows: no daemon — runs a standalone foreground php-cgi server for
    this one project on --listen; Ctrl-C stops it.

  run --config P [--listen ADDR] [--log-format FMT]
    Unchanged low-level primitive: the actual server process against an
    explicit multi-host server.toml, foreground, no daemon. What bougied
    spawns and what CI/power users invoke. --config stays MANDATORY here
    (a default makes no sense for a multi-host process).

  status [--format json-v1]
    Live host + pool table via the control socket: hostname, project,
    pool variant(s), state, idle age, last-served, PID. Falls back to
    "configured hosts (server not running)" from the daemon's server.toml
    when the socket is absent. Replaces `list --config`. (`list` kept as
    a hidden alias for one release.)

  open [NAME]
    Resolve the current project's (or NAME's) URL and open it in the
    default browser. Hints to run `bougie server` if not registered.

  stop
    Stop the shared dev server (= `bougie down server`). Message notes
    this stops hosting for ALL projects, since the server is shared.

  logs [-f] [-n N]
    Tail the server's request log. In a project, defaults to this
    project's host; otherwise all hosts. Mirrors `services logs`.

  tls {install, uninstall}        # unchanged
  hosts apply [--config P]        # --config now OPTIONAL → daemon's server.toml
```

### What changes vs today
- `bougie server` (no subcommand) now *does something* (the friendly
  verb) instead of erroring.
- `--config` demoted from mandatory to optional **everywhere except
  `run`**. When omitted, synthesize the host block from the project.
- `server list --config` → `server status` (live, control-socket
  backed); `list` becomes a deprecated alias.
- New: `status`, `open`, `stop`, `logs`.

### Relationship to `bougie up`
- `bougie up [server]` — bring up declared services (incl. server) in
  the background, attach to the *combined* multi-service log. General.
- `bougie server` — the web-server-focused, single-project, URL-first
  foreground sibling: `up server` + URL banner + per-host log attach +
  optional browser. Same daemon, same host registration — **no second
  runtime.**

## Config synthesis (the zero-config core)

When `--config` is absent, build an in-memory `ServerConfig` with one
`HostBlock` for the current project. Resolution order:

1. CLI flags (`NAME`, `--tls`)
2. project `bougie.toml` `[server]` — `hostname`, `root`, `index`,
   `try_files`, `rewrite`, `tls` (declarative per-project pinning)
3. derived defaults — hostname from dir / composer `name`; web root via
   the existing `resolve_web_root` (public/ → pub/ → web/ → .)

On Unix this synthesized block is handed to the daemon's provisioner
(same path as `up server`), not written to a user file. On Windows it
feeds the standalone `run` process directly.

The `skeleton()` / `add_host` / `remove_host` / `validate_host`
machinery already in `bougie-server::server::config` is reused — no new
config writer needed.

## Port note (Unix)

The shared server has one listener (`:7080`); `NAME` differentiates by
`Host:` header, so the friendly verb does **not** take `--listen` on
Unix (changing the shared port affects every project). `--listen` is
valid only on `run` and on the Windows standalone path. The resolved
port is surfaced in the URL banner.

## Implementation phases

**Phase 1 — CLI + dispatch reshape. ✅ DONE.** Reworked `ServerCommand`
in `bougie-cli`: `Server(ServerArgs)` with a flattened default-action
`ServeArgs` + `args_conflicts_with_subcommands`; added `Status`, `Open`,
`Stop`, `Logs`; kept `Run` (mandatory `--config`), `Tls`, `Hosts`
(`--config` now optional); `List` → hidden alias of `Status`. All server
dispatch moved into `crates/bougie/src/commands/server.rs`.

**Phase 2 — config synthesis. ✅ DONE.** On Unix the host block is
synthesized *by the daemon's provisioner* (the same path `up server`
uses), so the CLI just sends `service.up`. For the standalone path the
CLI synthesizes the host itself via cross-platform `sanitize_tenant` /
`derive_default_tenant` / `resolve_web_root` (mirroring the daemon) and
reuses `config::add_host_with_rewrites` (which seeds from a skeleton and
is idempotent) to write the one-host `server.toml`.

**Phase 3 — the project verb (Unix). ✅ DONE.** `bougie server` →
locate root → optional sync (`--no-sync`) → `service.up` for the
`server` service (bypassing `up`'s declared-only gate, since the daemon
validates against the catalog) → URL banner (`<tenant>.bougie.run:<port>`,
port read from the daemon's `server.toml`) → optional `--open` browser
launch → foreground attach to the server log, detaching on Ctrl-C.
`--format json-v1` / non-TTY implicitly detach.

**Phase 4 — observability. PARTIAL.** `open` (browser launch), `stop`
(= down server), `logs` (server-service tail) are wired. Remaining:
`status` live control-socket host/pool table (today it shows the
config-only view via `helpers::list`), and **per-host** log filtering
for `serve`/`logs` (today they attach to the whole `server` service
stream — fine for single-user dev, but not yet scoped to one project).

**Phase 5 — Windows fallback. ✅ DONE (compile-verified; runtime
untested here).** `bougie server` on non-Unix → `serve_standalone`:
locate project → optional sync → synthesize the host into the
bougied-managed `server.toml` path → run the engine in the foreground
(`run::run`); Ctrl-C stops (no daemon to detach into). `open` and
`open_url` were made cross-platform too (`cmd /C start` on Windows).
`serve_standalone` is defined cross-platform so it type-checks on Unix
(dead code there) — local `cargo check` for a Windows target is blocked
by a C dep (`aws-lc-sys` needs mingw-gcc), so real Windows compilation
is left to CI (`-p bougie`). No `--listen` on the friendly verb; custom
ports/configs go through `server run --config`.

**Phase 6 — docs. ✅ DONE.** `CLAUDE.md` CLI surface refreshed; this
plan kept until Windows runtime is confirmed in CI, then delete it (per
repo convention). `SERVER.md` upstream is the engine contract and is
unaffected by this CLI-only redesign.

## Risks / open details

- **Per-host log filtering** — the combined-log attach exists; needs a
  host filter to scope to one project. Confirm the log stream carries a
  host tag.
- **Detach UX** — Ctrl-C must cleanly detach the attach without sending
  a signal to the shared server process group. The `up` attach already
  solved foreground-attach-to-managed-service; reuse its detach path.
- **`open` on headless/CI** — no browser; degrade to printing the URL.
- **clap default-subcommand ergonomics** — `bougie server NAME` where
  `NAME` could collide with a subcommand name (`status`, `open`, …).
  Reserve those words / document that a project literally named `status`
  must use `bougie server --name status`.
