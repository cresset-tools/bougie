# bougie telemetry — design plan

Opt-in, anonymous usage + crash telemetry with a first-party collector.
Consent is asked **at install time** (a consent block appended to the
dist installer when it's promoted to the `latest/` channel) with a
first-run prompt in the binary as the fallback, so the setting exists
before bougie ever does anything interesting.

Design borrows deliberately: Go's transparent-telemetry mode file +
approved-fields allowlist, Homebrew's "nothing is sent before the
notice" guarantee, Turborepo's Rust client shape, gh CLI's detached
flush subprocess. Prior-art notes at the bottom.

## Locked design decisions

- **Opt-in, `[Y/n]`, Enter = yes.** Nothing is ever sent without an
  explicit interactive answer. Non-interactive contexts (piped stdin,
  CI, docker build) never prompt and default to **off**. "Default yes
  on Enter" applies only to a human pressing Enter at a real prompt.
- **First-party collector only** — `https://telemetry.bougie.tools`,
  running on infra we already operate (same box family as
  `releases.bougie.tools`). No third-party analytics processor, ever.
  This is the GitLab-pledge posture and it keeps the privacy story one
  sentence long.
- **Tri-state mode, Go-style: `off` / `local` / `on`.** `local` spools
  events on disk but never uploads — it costs nothing extra because
  the transport is spool-then-flush anyway, and `bougie telemetry log`
  over the spool is the single best trust device we can ship ("see
  exactly what would be sent").
- **Five data lanes in v1:** command usage + outcome, platform +
  version, PHP ecosystem stats, performance internals, crash reports.
- **Never collected:** project names/paths, package names, hostnames,
  usernames, env contents, IP addresses (dropped at ingest),
  sub-hour timestamps.
- **Random install UUID, minted only after consent** (Angular model),
  rotatable/deletable via `bougie telemetry reset`. No machine-derived
  IDs (.NET's hashed-MAC is the most-criticized design in the field).
- **Allowlist-driven schema.** Every field and every enumerable value
  is listed in `TELEMETRY.md` (public, in-repo); the collector rejects
  anything not on the list, so the server *cannot* receive strings it
  doesn't already know (Go's approved-counters idea). Schema additions
  happen by public PR against `TELEMETRY.md`.
- **`DO_NOT_TRACK=1` honored** everywhere: suppresses the prompt,
  forces `off`. Cheap goodwill several tools refuse to pay; we pay it.
- **Telemetry must never fail or slow a command.** Spool writes are
  best-effort and swallowed (the contract already written on
  `bougie-output`'s event sink, output.rs:175); uploads happen in a
  detached process after the user has their prompt back, running at
  the lowest scheduling priority safely reachable (nice 19 /
  below-normal class).

## Consent surfaces

### 1. Installer consent block (the "early moment")

The dist-generated installer cannot host a prompt *natively*:
cargo-dist 0.32 has no hooks, no template overrides, and its only knob
is `install-success-msg` (axodotdev/cargo-dist#314, open since 2023).
But it doesn't need to — the generated script is safely **appendable**,
and the publish pipeline already has a bougie-owned step where that
can happen.

Verified against the live installer (53 KB, APP_VERSION 0.39.0): the
entire script is function definitions until the final line,
`download_binary_and_run_installer "$@" || exit 1`, and nothing
checksums the installer scripts (`sha256.sum` covers only the
archives; no `.sh.sha256` sidecar exists). `publish-mirror.yml`'s
"Promote installers to latest" step (:135-150) copies
`bougie-installer.{sh,ps1}` from staging to
`installers/bougie/latest/` — exactly the path the
`bougie.tools/install.sh` 301 serves today.

Plan:

- `scripts/install-consent.sh` in-repo: the consent block as a single
  POSIX-sh function + trailing call. Must be dash-compatible and
  `set -u`-safe (it runs inside the dist script's shell, which sets
  `set -u` and aliases `local`).
- The promote-to-latest step appends it to the **staged** copy before
  scp. Only the `latest/` channel — already a bougie-specific pointer,
  not a mirrored GitHub asset — carries the block; the versioned
  mirror paths and the GitHub release assets stay byte-identical
  pristine dist output, so nothing ever looks like a tampered mirror.
- **Tail guard:** before appending, CI asserts the staged installer's
  last line is exactly `download_binary_and_run_installer "$@" || exit 1`.
  If a dist upgrade reshapes the entrypoint (starts `exit`ing or
  `exec`ing on success), the release job fails loudly instead of
  silently shipping a prompt that never runs.
- No nginx change, no second download, README one-liner untouched.

Appended-block semantics (it runs only after a *successful* install —
on failure the `|| exit 1` fires first, which is what we want):

1. Skip entirely when any of: mode file already exists
   (reinstall/update — never re-ask), `BOUGIE_TELEMETRY` set,
   `DO_NOT_TRACK=1` (writes `off`), `CI` set, no tty available.
2. Print the consent block (wording below) and read the answer.
   `curl | sh` consumes stdin, so it reads from `/dev/tty` (rustup's
   pattern). No `/dev/tty` → leave mode unset (= off; the binary may
   ask later).
3. Write the mode file (format below) to
   `${XDG_CONFIG_HOME:-$HOME/.config}/bougie/telemetry`.

PowerShell parity: append `scripts/install-consent.ps1` to
`bougie-installer.ps1` in the same step — `irm … | iex` does *not*
consume stdin, so plain `Read-Host` works (mode file at
`%APPDATA%\bougie\telemetry`). Verify the ps1 entrypoint seam with the
same tail-guard idea.

Failure stance: if the consent block breaks on some exotic shell, the
answer is "unset", never "on" — and it must never affect the install's
exit status (the block swallows its own errors and always returns 0).

Rejected alternative — a standalone wrapper `install.sh` that prompts
and then fetches the untouched dist installer (rustup/rye style). It
keeps the installer pristine and can evolve independently of releases,
but costs a redirect flip (ops), a second download, and a second
script to maintain. Revisit only if the tail guard becomes a recurring
casualty of dist upgrades.

### 2. First-run prompt in the binary (fallback)

Covers every channel the installer block can't reach: `cargo install bougie`,
docker images, a future brew formula, direct artifact downloads. On
command entry, prompt **iff all of**:

- mode is unset (no file, no `BOUGIE_TELEMETRY`, no `DO_NOT_TRACK`);
- stdin *and* stderr are ttys; `--format text`; not `--quiet`;
- not CI (`CI` + the usual vendor vars, `is_ci`-style sniff);
- the invocation is a real top-level command — never on shim/argv0
  paths (`php`, `composer`, `bougie-babysit`, `bougied`), `bougie run`
  children, `__telemetry-flush`, or `bougie telemetry` itself;
- fewer than 3 prior prompt attempts (counter kept next to the mode
  file). On the 3rd skipped/declined-by-EOF attempt, write `off` and
  never ask again.

Prompt goes to **stderr** (stdout stays clean for `--format json-v1`),
answer read from stdin, Enter = yes, then the command proceeds
normally. Ctrl-C / EOF = no answer recorded, attempt counter bumped.

### 3. Mode file + env resolution

`<config>/bougie/telemetry` — deliberately next to the dist install
receipt (`~/.config/bougie/bougie-receipt.json`) rather than under
`$BOUGIE_HOME`, because the installer's consent block (plain sh) must
be able to compute the path without knowing bougie's path rules. New
`bougie-paths::config_dir()`: Unix `${XDG_CONFIG_HOME:-~/.config}/bougie`,
Windows `%APPDATA%\bougie`.

Single shell-writable line, Go-style:

```
on 2026-07-03 1
^   ^          ^ consent-schema version
|   | date consent was recorded
| off | local | on
```

Precedence (first match wins):

1. `DO_NOT_TRACK=1` → off (also suppresses prompts)
2. `BOUGIE_TELEMETRY=off|local|on` → as given (never writes the
   file). Truthy/falsy aliases accepted: `1`/`true` → on, `0`/`false`
   → off (same convention as `BOUGIE_SYSTEM_PHP`).
3. mode file
4. unset → off (+ prompt-eligible)

Note that CI detection sits *below* the env var: `BOUGIE_TELEMETRY=on`
(or `=1`) sends even when `CI` is set — CI gating only applies to the
unset case and to prompting. That's the deliberate lever for people
who want telemetry from their own runners/images.

**Consent versioning:** if we ever *expand* what an existing lane
collects or add a lane, bump the consent-schema version. A mode file
saying `on` with an older version behaves as **unset**: uploads stop,
and the next interactive command re-prompts with the delta explained
(Nuxt's `consentVersion` pattern). Narrowing collection never bumps.

CI posture: mode unset in CI = off, silently. Mode explicitly `on` —
whether via `BOUGIE_TELEMETRY=on`/`=1` in the environment or a mode
file baked into an image on purpose — → honored, events carry
`ci: true`. CI never suppresses an explicit opt-in; it only stops
bougie from prompting or defaulting anything on.

## What is collected — schema v1

One NDJSON event per line. Common envelope on every event:

| field | type / values | notes |
| --- | --- | --- |
| `schema` | `1` | wire schema, independent of consent version |
| `event` | `command` \| `crash` | |
| `ts` | RFC3339, **truncated to the hour** | no sub-hour resolution, ever |
| `install_id` | UUIDv4 | minted at consent; `telemetry reset` rotates |
| `invocation` | UUIDv4 | per-process; correlates `command`+`crash` from one run |
| `bougie_version` | semver string | from the release-please-stamped version |
| `build_sha` | 9-hex short git SHA, or absent | from the same git stamping `bougie-cli/build.rs` already does for `--version` |
| `os` / `arch` | `linux\|macos\|windows` / `x86_64\|aarch64` | |
| `libc` | `gnu\|musl\|none` | Windows → `none` |
| `ci` | bool | |
| `install_method` | `installer\|cargo\|docker\|unknown` | receipt-derived: dist receipt present → `installer`; `BOUGIE_IMAGE`-built containers set a marker → `docker` |

On `build_sha`: `bougie-cli/build.rs` already resolves
`git rev-parse --short=9 HEAD` into the `--version` string
(`0.6.4 (63c5f57d3 2026-05-08 <target>)`); it grows one extra
`cargo:rustc-env=BOUGIE_BUILD_SHA={sha}` line so telemetry reads it
via `option_env!` instead of parsing the display string. Kept as a
separate field rather than embedded in `bougie_version` so the
version stays an enumerable allowlist value and the SHA is
pattern-validated (`^[0-9a-f]{9}$`) like `install_id`. Absent exactly
when git metadata was unavailable at build time — crates.io tarballs,
so it correlates with `install_method: cargo`; dist-built release
binaries always carry it. The collector may optionally enrich by
cross-checking against the repo's known release SHAs.

### `command` event

| field | type / values |
| --- | --- |
| `name` | stable `command_name()` string (lib.rs:81-109) — closed set, allowlisted |
| `duration_ms` | u64 |
| `outcome` | `ok` \| error category (below) |
| `exit_code` | u8 |

Outcome categories map 1:1 from the `bougie-errors` taxonomy
(`exit_code_for`): `network` (10), `index-signature` (11),
`manifest-hash` (12), `blob-hash` (13), `resolution` (20),
`unknown-target` (21), `yanked` (22), `lock-held` (40),
`filesystem` (50), `self-update` (60), plus `usage` (clap, 2),
`panic` (101), `other` (1).

**Perf fields** (only on commands where they apply — `sync`, `lock`,
`add`, `composer install/update`, `up`): `resolve_ms`, `fetch_ms`,
`extract_ms`, `autoload_ms`, `download_bytes`, `packages_installed`
(count), `cache_hit_pct` (0–100, integer). Threaded out of the
resolver/installer via a small `TelemetryProbe` the command impls
already have natural access to; absent = field omitted.

**Ecosystem fields** (same commands, throttled): `php_version`
(minor only, `"8.4"`), `php_flavor`, `php_source` (`managed|system`),
`extensions` (names from the bougie index — closed vocabulary,
allowlisted), `services` (names from the service catalog — closed
vocabulary), `direct_deps` / `total_deps` (bucketed:
`0,1-5,6-15,16-40,41-100,100+`). Throttle: at most once per project
per 7 days, deduped **locally** against the existing
`$BOUGIE_HOME/state/projects/<hash>/` dir — the project hash never
leaves the machine.

### `crash` event

Written by a panic hook installed in `main.rs`'s worker thread (the
existing exit-101 path). Release builds only (dev builds skip). The
envelope's `build_sha` pins a crash to an exact commit, which is what
makes fingerprints and frame line-offsets trustworthy across builds;
when it's absent (crates.io builds) the release-please version→tag
mapping is the fallback.

| field | notes |
| --- | --- |
| `command` | same `command_name()` string |
| `fingerprint` | sha256 (first 16 hex) of the frame list — local dedupe: each fingerprint sent at most once per day |
| `frames` | ≤ 40 entries, `crate::module::function` **symbol names only** — file paths and absolute line numbers stripped; frames outside an allowlist of prefixes (`bougie*`, `sandbox_run`, `std`, `core`, `alloc`) collapsed to `[external]` |
| `message` | panic message after scrubbing, ≤ 200 chars |

Message scrubber (order matters): replace anything path-shaped
(`/…/…`, `X:\…`, `~/…`), anything containing the home dir, and any
quoted string longer than 12 chars with `[redacted]`; then truncate.
Standard panics ("index out of bounds: len 3 idx 7",
"called `unwrap()` on `None`") survive intact — those carry the value.
If scrubbing panics or the message is empty, send the fingerprint with
no message: frames alone are still actionable (Go ships stacks with no
message at all).

### Explicitly never collected

Project names, directory paths, Composer package names, git remotes,
hostnames, usernames, locale, env vars, full timestamps, IPs (see
collector), request/URL contents. This list goes verbatim into
`TELEMETRY.md` — a "what we do NOT collect" section is the single most
copied trust feature across every tool surveyed.

## Transport

**Spool, then flush. Never in-band.**

- Spool: append NDJSON to `$BOUGIE_CACHE/telemetry/spool/<yyyy-mm-dd>.ndjson`
  (cache root = transient by definition, correct home). Caps: 1 MiB /
  30 days, oldest dropped first. Append failures are swallowed.
  Spooling happens in `off` mode **not at all**, in `local`/`on` always.
- Flush (mode `on` only): at end of `bougie::run`, if the spool has
  \>64 KiB or the oldest entry is >24 h old, spawn a detached
  `bougie __telemetry-flush` (hidden subcommand, gh CLI's pattern:
  own process group/`setsid` on Unix, `CREATE_NO_WINDOW`+detached on
  Windows). The parent returns immediately — the user never waits.
- `__telemetry-flush`: reads spool, gzips, `POST
  https://telemetry.bougie.tools/v1/batch` (≤256 KiB per request,
  multiple requests if needed), `reqwest::blocking` (already a bin
  dep, same rustls config as self-update), 5 s timeout, UA
  `bougie/<version>`. 2xx → delete flushed files; anything else →
  leave them for next time (caps bound the damage). Holds a lock file
  so concurrent flushers no-op.
- **The flush deprioritizes itself before doing anything else.** Unix:
  `rustix::process::nice(19)` — rustix is already in the dependency
  graph (via tempfile) and wraps this safely; `nix` 0.30 has no
  priority API, and the workspace `unsafe_code = "deny"` policy rules
  out raw `libc`. Linux extra: best-effort write of `19` to
  `/proc/self/autogroup` — under autogroup scheduling (default on
  desktop distros) per-process nice only weighs within a session, and
  the detached flush *is* its own session; the autogroup file closes
  that gap with plain file I/O. Windows: the parent ORs
  `BELOW_NORMAL_PRIORITY_CLASS` into the same `creation_flags` word
  that already carries the detach/no-window flags — no child
  cooperation needed. All best-effort: a failed renice never skips
  the flush. Accepted side effect: at nice 19 on a busy machine the
  5 s timeout fires more readily — fine, the spool persists and the
  next flush retries. I/O priority (`ioprio_set`) is deliberately
  left alone: the flush reads ≤1 MiB, not worth a raw syscall under
  the unsafe policy.
- `BOUGIE_TELEMETRY_URL` overrides the endpoint (tests, dry-runs).

Out of scope for v1: the long-running daemons (`bougied`,
`bougie-babysit`) and all exec-passthrough shim paths emit nothing —
shims `exec()` away before any timing could be captured, and daemon
telemetry deserves its own consent thinking (if ever).

## Instrumentation choke point

Everything hangs off `bougie::run` (crates/bougie/src/lib.rs:112),
which already computes `command_name(&cli.command)` for the tracing
span at lib.rs:128:

```rust
let telemetry = bougie_telemetry::Recorder::init(&paths, command_name);  // reads mode, no I/O if off
let started = Instant::now();
let result = /* existing match cli.command */;
telemetry.record_command(started.elapsed(), classify(&result));          // spool append
telemetry.maybe_spawn_flush();                                           // detached, mode==on only
result
```

`classify` reuses `bougie_errors::exit_code_for`'s downcast to name
the category. The panic hook is registered in `main.rs` before the
worker thread spawns, writing a `crash` event through the same spool.
The first-run prompt also lives at `run()` entry, before the match,
gated as in §Consent-2.

## Collector (server side, separate deliverable)

Not in this repo — a small companion service (working name
`bougie-collector`), deployed next to the mirror origin. v1 spec:

- `POST /v1/batch`: gzip NDJSON, ≤256 KiB. Validates every event
  against the allowlist generated from `TELEMETRY.md`'s field tables
  (one source of truth; a codegen check in CI keeps the Rust event
  structs, the doc, and the collector schema in sync). Unknown
  field/value → line dropped. Always answers 204 — the client is
  never told to retry harder.
- **IP handling: in-memory rate limiting only, never written.** No
  geo lookup in v1. This goes in `TELEMETRY.md` in exactly those
  words — the gap in Go's privacy policy (IP handling unstated) is
  the one thing we do better on paper.
- Storage: SQLite + litestream backup, nightly aggregate rollups.
  Zero-ops at any plausible bougie scale (the entire Go fleet is
  ≤80 MB/week compressed; we are orders of magnitude below).
  Raw events pruned at 400 days; aggregates kept indefinitely.
  Schema written so a ClickHouse migration is mechanical if volume
  ever demands it.
- Later (phase 6): public aggregate dashboard at
  `bougie.tools/telemetry` — publishing the data is the strongest
  goodwill move in every case studied (Go, Homebrew, .NET).

## `bougie diagnose` — user-initiated deep reports

Telemetry answers *that* and *where* (category + version + platform
spikes); this answers *why*. When an error category spikes but the
content-free events can't explain it, the detail comes from affected
users who volunteer it — per-incident, shown-before-sent, explicitly
confirmed. Precedent: `brew gist-logs`, `flutter doctor`,
`composer diagnose`. Considered and rejected instead of this:
Sentry/sentry.io — a third-party processor breaks the first-party
pledge, error messages are the most identifying payloads we handle
(paths/URLs/package names in exactly the categories worth debugging),
and the SDK's whole value is context this design refuses to collect
automatically. (Self-hosted GlitchTip would fix only the first
objection; revisit only if diagnose proves insufficient.)

**Deliberately not telemetry.** Independent of the telemetry mode —
it works with telemetry `off`, because a user deliberately mailing a
report is correspondence, not tracking — and not subject to
`DO_NOT_TRACK`. It never runs automatically, nothing else sends
through its channel, and every send requires a fresh interactive
confirm (`[y/N]`, default **no** — outward-facing action).

Mechanics:

- On any `BougieError` failure, the `report_error` path (main.rs:108)
  also writes `$BOUGIE_CACHE/telemetry/last-failure.json` — local
  only, single slot (overwritten each failure), full unscrubbed error
  chain + argv + platform snapshot. Written regardless of telemetry
  mode: it's a local artifact of the same class as a log file, and
  it's what makes the workflow zero-effort after the fact. Error
  output for diagnosable categories (everything except `usage`) gains
  one trailing hint: ``hint: run `bougie diagnose` to assemble a
  shareable report``.
- `bougie diagnose` assembles a report from `last-failure.json` plus
  an environment summary (bougie version/`build_sha`, os/arch/libc,
  PHP version, and the *names* — never values — of any `BOUGIE_*`
  env vars set). `bougie diagnose -- <bougie args>` re-runs the given
  command with a debug-level tracing ring buffer and appends the log
  tail instead, for reproducible failures.
- The report passes the crash-lane scrubber as a courtesy pass
  (home-dir/path redaction) but *keeps* error messages and package
  names — the real safeguard is review: the full payload is printed
  (paged on a tty) and the user confirms before anything leaves the
  machine.
- On confirm: gzip POST to `/v1/diagnose` (≤256 KiB), which returns a
  short report id (`diag-a1b2c3`) to paste into a GitHub issue.
  `--issue` skips the upload entirely and produces a prefilled GitHub
  issue instead (report inlined as markdown). Diagnose reports are
  retained 180 days, deleted on request by id — documented in
  `TELEMETRY.md` with everything else.

## CLI surface

```
bougie telemetry              # alias for status
bougie telemetry status       # mode, consent date, install id, spool size, last flush
bougie telemetry on|off|local # writes the mode file; `on` from unset mints install_id
bougie telemetry log [-n N]   # print spooled events (the "see for yourself" command)
bougie telemetry reset        # rotate install_id, purge spool
```

Plus hidden `__telemetry-flush`. All subcommands implement `Render`
so `--format json-v1` works. `bougie telemetry on` prints the same
disclosure text as the prompts, then confirms.

Separately, top-level `bougie diagnose [--issue] [-- ARGS...]`
(§ above) — deliberately *not* a `telemetry` subcommand, because it
must be reachable and functional with telemetry off.

## Prompt wording

Installer and first-run use the same block (installer says "bougie
can…", first-run says "bougie would like to…"):

```
bougie can send anonymous usage statistics and crash reports to the
bougie developers. This never includes project names, package names,
paths, or IP addresses, and nothing is sent without your consent.
Details + full field list: https://bougie.tools/telemetry

  Enable anonymous telemetry? [Y/n]
```

Decline path always prints: `ok — telemetry is off. Enable later with:
bougie telemetry on`.

## Crate layout

New workspace member **`bougie-telemetry`** (29th crate):

- `mode.rs` — mode file parse/write, env precedence, consent version.
- `event.rs` — envelope + event structs (`serde`), the allowlist
  constants, bucket helpers.
- `spool.rs` — append/rotate/caps/iterate.
- `recorder.rs` — per-invocation entry point the bin talks to
  (`Recorder::init` / `record_command`).
- `scrub.rs` — crash frame filter + message redactor (heavily
  unit-tested; property tests that no `/`-rooted token survives).
- `flush.rs` — batch, gzip, POST (`reqwest` blocking + `flate2`).
- `spawn.rs` — parent-side flush trigger + detached spawn (Windows
  `creation_flags` live here).
- `prompt.rs` — tty detection, first-run gates, attempt counter.
- `probe.rs` — `TelemetryProbe` populated at the command layer for
  perf/ecosystem fields.

Deps: `serde`, `serde_json`, `uuid`, `flate2`, `reqwest` (blocking,
rustls — same feature set as the bin), `sha2`, and Unix-only `rustix`
(`process` feature, for the flush renice). Follows release-please
convention: `[workspace.dependencies]` entry with annotated
`version = "…" # x-release-please-version`. `bougie-paths` gains
`config_dir()`; `bougie-cli` gains the `Telemetry` subcommand enum.

## Privacy & legal posture (EU maintainer)

- **Consent is the lawful basis** (GDPR Art 6(1)(a)) and the same act
  satisfies ePrivacy Art 5(3) for both the stored mode file/UUID and
  the network calls — relevant because EDPB Guidelines 2/2023
  explicitly pull non-web software phone-home into Art 5(3) scope.
  Opt-in makes the Dutch-vs-elsewhere national variance moot.
- The install UUID is conservatively treated as personal data (EDPB
  01/2025 pseudonymisation guidance). Erasure story:
  `bougie telemetry reset` client-side, plus delete-by-install-id on
  request (documented contact in `TELEMETRY.md`).
- `TELEMETRY.md` (repo root, linked from README and both prompts, and
  the target of `bougie.tools/telemetry`) is the public contract:
  every field with allowed values, the never-collected list, IP-drop
  at ingest, retention numbers, how to inspect (`telemetry log`), how
  consent versioning works.
- Same doc discloses the *non-telemetry* network metadata bougie
  already emits (User-Agent to Packagist/index/mirror, Composer's
  `notify-batch` **not** being sent) — uv taught us that undocumented
  UA metadata gets called "hidden telemetry" eventually.

## Implementation plan

Bottom-up; each phase lands whole and inert until the next, one PR
per phase. Conventional-commit titles given per phase (release-please
requires them).

### Cross-cutting facts the implementation leans on

- **Dispatch wrap needs one mechanical refactor.** `run()`
  (lib.rs:112) *is* the match; to wrap it with timing/outcome, first
  extract the match body into `fn dispatch(cli: Cli) -> Result<ExitCode>`
  and have `run()` call it. `command_name()` (lib.rs:81-109) is a
  flat match returning `&'static str` — new `Command` variants force
  new arms at compile time, which keeps the telemetry name set and
  the dispatcher in sync for free.
- **`Paths` is resolved per-command, never centrally** (e.g.
  cache_clean.rs:23). The telemetry `Recorder` follows suit: its init
  calls `Paths::from_env()` itself and degrades to disabled on any
  error — telemetry must never surface a paths failure.
- **Prompt idiom is hand-rolled**: stderr prompt + `stderr().flush()`
  + `stdin().read_line()` + `stdin().is_terminal()` gate
  (starter.rs:289-318, projects.rs:347-375). No dialoguer, no
  `/dev/tty` anywhere — the binary's prompts read plain stdin (fine:
  unlike `curl | sh`, stdin *is* the terminal for a normal bougie
  invocation). Only the installer snippet needs `/dev/tty`.
- **Hidden subcommand precedent**: `#[command(hide = true, name =
  "tool-exec")] ToolExec` (bougie-cli lib.rs:383-393) — the template
  for `__telemetry-flush`.
- **Detach precedent**: parent spawns `current_exe()` with null stdio
  and doesn't wait (client.rs::spawn_daemon :310-329); the *child*
  calls `rustix::process::setsid()` (bougie-daemon daemon.rs:68).
  `rustix` is already a direct dep of the bin with the `process`
  feature — `nice()` comes from the same feature, no new dep for the
  bin. Windows `creation_flags` has **no precedent in the repo**;
  the flush spawner is its first use (`CREATE_NO_WINDOW |
  BELOW_NORMAL_PRIORITY_CLASS`, via
  `std::os::windows::process::CommandExt`).
- **Build SHA is per-crate at compile time.** `option_env!` resolves
  against the *consuming crate's* build env, so the daemon's
  `option_env!("BOUGIE_BUILD_HASH")` (ipc.rs:386) is `""` today —
  nothing sets it. The fix that helps both: `bougie-cli/build.rs`
  (which already computes the 9-char SHA) additionally emits
  `cargo:rustc-env=BOUGIE_BUILD_SHA={sha}`, and bougie-cli exports
  `pub const BUILD_SHA: Option<&'static str> =
  option_env!("BOUGIE_BUILD_SHA");`. The bin passes it into the
  Recorder — bougie-telemetry never deps on bougie-cli.
- **Third-party deps are per-crate** (root `[workspace.dependencies]`
  holds only internal crates). bougie-telemetry declares its own
  `serde`, `serde_json`, `sha2 = "0.11"`, `flate2 = "1"`, `reqwest`
  (blocking + rustls, mirror the bin's features), `uuid = { version
  = "1", features = ["v4"] }` (**new to the tree**), and a
  `[target.'cfg(unix)'.dependencies.rustix]` block modeled on
  bougie-scripts' nix block (Cargo.toml:19-24).
- **Flush lock**: `bougie_fs::lock::ExclusiveGuard::acquire(path,
  timeout)` (lock.rs:29) on a `telemetry.lock` next to the spool.
- **Test harness**: `tests/common/mod.rs::TestEnv` overrides
  `BOUGIE_HOME`/`BOUGIE_CACHE` via assert_cmd; telemetry tests
  additionally override `XDG_CONFIG_HOME` (mode file). Endpoint
  injection copies the `BOUGIE_PACKAGIST_BASE_URL` wiremock pattern
  (composer_update.rs:88-104) with `BOUGIE_TELEMETRY_URL`.
- **Release plumbing for a new crate**: workspace `members` entry +
  alphabetical `[workspace.dependencies]` line with the
  `# x-release-please-version` annotation. First crates.io publish of
  a brand-new crate name can trip the new-crate rate limit
  (crates-publish.yml is self-retrying, but expect one manual nudge).

### Phase 1 — core crate + command events, local-only

`feat(telemetry): bougie-telemetry crate, mode file, spool, telemetry CLI (local-only)`

New crate `crates/bougie-telemetry/`:

- `mode.rs` — `Mode {Off, Local, On}`; `resolve(env, file) -> ModeState`
  implementing the precedence table (§Consent-3) incl. `1/true/0/false`
  aliases and consent-version comparison; `write(mode_file, mode,
  date, version)`. Pure functions over injected env/content, tested
  like `Paths::resolve`.
- `event.rs` — envelope + `CommandEvent` structs (serde), outcome
  enum mapped from `bougie_errors` categories, bucket helpers.
  `schema = 1` constant. The field tables in `TELEMETRY.md` are
  written from these structs (phase 2); a unit test asserts the
  serialized field set matches a checked-in snapshot so schema drift
  is loud.
- `spool.rs` — append NDJSON line to
  `<cache>/telemetry/spool/<yyyy-mm-dd>.ndjson`, size/age caps with
  oldest-first pruning, iterate/drain. All fallible paths swallowed
  behind a `debug!` trace.
- `recorder.rs` — `Recorder::init(command_name, version, build_sha)`
  (does its own lenient `Paths::from_env`), `record_command(duration,
  outcome, exit_code)`.

Repo wiring:

- `bougie-paths`: free `pub fn config_dir() -> Result<PathBuf>` via
  `etcetera` (`Xdg` config dir on Unix = `${XDG_CONFIG_HOME:-~/.config}`,
  `Windows` strategy on Windows = `%APPDATA%`), joined with
  `"bougie"`; plus `telemetry_mode_file()`. Must byte-match what the
  sh/ps1 snippets compute — a doc-comment states that contract.
- `bougie-cli`: `Command::Telemetry(TelemetryCommand)` (`status`
  default, `on`, `off`, `local`, `log { n }`, `reset`); build.rs
  emits `BOUGIE_BUILD_SHA` + exports `BUILD_SHA`.
- `bougie` bin: extract `dispatch()`, wrap with Recorder;
  `commands/telemetry.rs` implementing the subcommands
  cache_clean.rs-style (`Serialize` result + `Render` + `emit`).
  `telemetry on` from unset mints the install UUID; `reset` rotates
  it and purges spool.
- Root Cargo.toml: member + annotated workspace-dep line.

No uploads exist yet: mode `on` behaves as `local` with a
`status`-visible note ("collector not yet enabled in this build" —
or simply: phase 1 treats on==local internally). No prompts.

Tests: mode-precedence table-driven unit tests; spool cap/prune unit
tests; one integration test (TestEnv + XDG_CONFIG_HOME) asserting a
failed `sync` in mode `local` produces a spooled `command` event with
`outcome: resolution`, and that `--format json-v1` stdout stays clean.
Windows: unit tests only (crate compiles in the `-p bougie` build).

### Phase 2 — flush, consent surfaces, collector → live

Three PRs, ordered; the release that ships 2c must not go out before
the collector is deployed.

**2a** `feat(telemetry): detached flush subprocess`

- `flush.rs` in the crate: drain spool → gzip → POST batches to
  `BOUGIE_TELEMETRY_URL` or the baked default; delete on 2xx; 5 s
  timeout; `ExclusiveGuard` around the drain.
- `spawn.rs`: parent-side trigger (spool >64 KiB or oldest >24 h,
  mode `on` only) spawning `current_exe()` + `__telemetry-flush`,
  null stdio; Unix child does `setsid()` + `nice(19)` +
  `/proc/self/autogroup` write; Windows spawner sets
  `creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)`.
- bougie-cli: hidden `#[command(hide = true, name =
  "__telemetry-flush")]` variant; bin: `commands/telemetry_flush.rs`.
- Tests: wiremock integration test — spool three events, run
  `__telemetry-flush` with `BOUGIE_TELEMETRY_URL` pointed at the mock,
  assert gzip NDJSON body, spool emptied on 200, retained on 500.
  Unix-gated; Windows compile-only.

**2b** `feat(telemetry): consent prompts + install-time consent block`

- First-run prompt in `run()` entry per §Consent-2 gates (attempt
  counter file next to the mode file); prompt/confirm in the
  starter.rs idiom. `telemetry on` prints the disclosure text.
- `scripts/install-consent.sh` (POSIX, `set -u`-safe, self-contained
  function + call, `/dev/tty` read) and `install-consent.ps1`
  (Read-Host).
- `publish-mirror.yml`: in "Promote installers to latest" (:135-150),
  before each scp: tail-guard (`tail -n1` must equal the dist
  entrypoint line, modeled on the existing missing-file guard at
  :144) then `cat` the snippet onto the staged copy. Versioned
  staging upload (:122-133) stays pristine.
- Shell tests: a `tests/install_consent.rs`-adjacent shell check (or
  CI step) running the snippet under `dash` with simulated tty
  answers via a pty helper — minimum: syntax-check with `dash -n` +
  a non-tty run asserting no mode file is written and exit 0.

**2c** `feat(telemetry): enable default collector endpoint` +
`docs: add TELEMETRY.md`

- Bake `https://telemetry.bougie.tools/v1/batch`, flip on==local off.
- `TELEMETRY.md` written from the event structs (field tables,
  never-collected list, IP/retention policy, inspection how-to);
  README link; prompt URL `bougie.tools/telemetry` → served copy.
- Out-of-repo gate: bougie-collector deployed (POST /v1/batch,
  allowlist validation, 204-always, IP drop, SQLite+litestream).
  Separate repo; its schema config is generated from this repo's
  `TELEMETRY.md` tables — a CI check here fails if structs and doc
  tables diverge.

### Phase 3 — ecosystem + perf enrichment

`feat(telemetry): ecosystem and perf fields on command events`

- `probe.rs`: `TelemetryProbe` populated at the *command layer* (not
  inside the resolver) — `sync`/`lock`/`add`/`composer_*` impls
  already hold the lock model (dep counts), the resolved PHP
  version/flavor/source, and wall-clock spans around
  resolve/fetch/autoload calls. No resolver-internal changes in this
  phase; `cache_hit_pct`/`download_bytes` come from
  resolver-returned stats if already surfaced, else deferred.
- Per-project 7-day throttle keyed on the existing
  `state/projects/<hash>/` dir (hash stays local): a
  `telemetry-last-snapshot` marker file.
- Extensions/services names validated against the closed
  vocabularies at event-build time (drop, don't send, anything
  unknown — e.g. a locally-built extension).

Consent v1 already discloses these lanes; no consent bump.

### Phase 4 — crash lane (then announce)

`feat(telemetry): crash reports with scrubbed backtraces`

- `scrub.rs`: frame filter (allowlist prefixes `bougie`,
  `sandbox_run`, `std`, `core`, `alloc`; others → `[external]`;
  symbols only, ≤40) + message redactor (path-shaped tokens, home
  dir, long quoted strings → `[redacted]`; ≤200 chars).
  Property-test: no `/`-rooted or drive-letter token survives.
- Panic hook: installed in `main()` on the **non-shim branch only**
  (after `role_from_argv0` returns None — bougied/babysit must not
  inherit it), chaining to the previous hook so the stderr panic
  message and the join-Err→101 contract are untouched. Command name
  reaches the hook via a `OnceLock<&'static str>` set right after
  `Cli::parse()` (command_name already returns `&'static str`).
  Frames via the `backtrace` crate (std's frame API is unstable);
  release builds only (`#[cfg(not(debug_assertions))]` + present
  `BUILD_SHA` or `install_method != cargo`).
- Local fingerprint dedupe (one send per fingerprint per day) via a
  small `crashes-seen` file next to the spool.
- Tests: a `test-fixtures`-gated `panic-probe` hidden test hook (or
  an integration test binary) that panics on demand; assert the
  spooled crash event has no path-like tokens and exit code is 101.

**v1 announce after this phase** — announcing before crash reporting
exists would mean a consent-version bump two weeks later.

### Phase 5 — `bougie diagnose`

`feat(diagnose): user-initiated diagnostic reports`

- `report_error` (main.rs:108) additionally writes
  `<cache>/telemetry/last-failure.json` (single slot) and appends the
  hint line for non-`usage` categories.
- `commands/diagnose.rs`: assemble from last-failure.json + env
  summary; `-- ARGS` re-run lane is a **subprocess** — spawn
  `current_exe()` with the args and `BOUGIE_LOG=debug`, capture the
  last 64 KiB of its stderr (no in-process ring-buffer layer needed;
  the repo has no log-capture infra to reuse, and a subprocess keeps
  it that way).
- Review-then-confirm via `emit_paged` (output.rs:70 — pager support
  exists) + the projects.rs `[y/N]` idiom; POST `/v1/diagnose` on
  yes, print report id; `--issue` renders markdown + a prefilled
  GitHub issue URL instead.
- Independent of mode by design → the send path here does **not**
  check mode/DNT, only the interactive confirm.

### Phase 6 — transparency extras (ongoing)

Public aggregate dashboard at `bougie.tools/telemetry`,
schema-change-by-PR process note in CONTRIBUTING, quarterly prune
review. Mostly collector-side; no bin changes expected.

### Risks / watch items

- **dist template drift** breaks the append seam → the 2b tail-guard
  turns that into a loud release failure; re-check after every
  `cargo-dist-version` bump (same ritual as the hand-edited workflow
  trigger).
- **Windows detach is greenfield** — first `creation_flags` in the
  repo; verify in the `windows_smoke` lane that a spawn-triggering
  command exits promptly and leaves no console window (manual check
  once; smoke test asserts exit only).
- **`uuid` is a new dependency** — trivial, but it's the first
  new-to-tree crate this feature adds; keep the rest to what the
  workspace already uses.
- **Panic-hook interaction with the 16 MiB worker thread**: the hook
  runs on the panicking thread; spool append must not allocate
  unboundedly (backtrace capture is the heavy part — cap frames
  before formatting).
- **crates.io first publish** of `bougie-telemetry` may need a manual
  retry on the new-crate rate limit.

## Prior art, condensed

| Tool | What we take | What we avoid |
| --- | --- | --- |
| Go (`gotelemetry`) | tri-state mode file, allowlist config, stack-symbol-only crashes, public data | weekly-counter architecture (event volume doesn't warrant it); their unstated IP policy |
| Homebrew | "nothing sent before notice" guarantee, no-ID mindset, 365-d retention precedent | opt-out default; third-party InfluxDB ingest |
| Angular / Nuxt | opt-in prompt UX, consent-version re-prompt, ID-minted-on-consent | GA4 backend |
| Turborepo | Rust client shape (`crates/turborepo-telemetry`), debug/inspect mode | Vercel-shared endpoint |
| gh CLI | detached flush subprocess, `=log` payload inspection | silent enable (the whole 2026 backlash) |
| .NET | public aggregate data | machine-derived IDs, collect-from-first-run |
| rustup | — | shipped opt-in telemetry nobody used, deleted it; our counter: prompt at install, not a buried command |

Backlash pattern across Audacity/GitLab/Go/Gatsby/gh: the default is
~80 % of the fight, third-party endpoints are an independent tripwire,
notice must precede the first send, minimization must be structural.
This design starts on the right side of all four.

## Open questions

- Collector hosting: same Hetzner box as the mirror origin vs. a
  separate VM (blast-radius isolation for an unauthenticated ingest
  endpoint). Leaning separate-small-VM.
- Docker images: bake `BOUGIE_TELEMETRY=off` into published images, or
  leave unset (same effect, less explicit)? Leaning explicit `off`.
- Does `bougie self update` preserve the prompt-attempt counter
  semantics across major consent-version bumps, or reset attempts to
  give the re-prompt three fresh chances? Leaning reset.
