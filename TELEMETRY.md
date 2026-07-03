# Telemetry

bougie can collect **anonymous, opt-in** usage statistics and crash
reports. Nothing is ever sent without your explicit consent, and this
document is the complete contract: every field bougie can upload is
listed here, the collector rejects anything not on these tables, and
schema changes happen by public pull request against this file.

The short version:

- **Opt-in.** You are asked once — by the installer, or on first
  interactive run. Enter accepts, `n` declines, and non-interactive
  environments (CI, docker, pipes) are never prompted and default to
  off.
- **Anonymous.** No project names, no package names, no paths, no
  usernames or hostnames, no IP addresses stored, no machine
  fingerprinting. The only identifier is a random UUID minted at
  consent, rotatable with `bougie telemetry reset`.
- **Inspectable.** `bougie telemetry log` prints exactly the events
  that would be uploaded, byte for byte. `bougie telemetry local`
  records locally without ever uploading.
- **First-party.** Events go to `https://telemetry.bougie.tools`,
  operated by the bougie maintainers. No third-party analytics
  service is involved.

## Consent and modes

The mode is one of:

| mode | records locally | uploads |
| --- | --- | --- |
| `off` | no | no |
| `local` | yes | no |
| `on` | yes | yes (batched, detached, deprioritized) |

Resolution order (first match wins):

1. `DO_NOT_TRACK=1` → off (also suppresses every prompt).
2. `BOUGIE_TELEMETRY=off|local|on` (aliases: `1`/`true` = on,
   `0`/`false` = off). An explicit `on` here wins even in CI — the
   deliberate lever for telemetry from your own runners or images.
3. The mode file: `~/.config/bougie/telemetry` on Linux/macOS
   (respecting `XDG_CONFIG_HOME`), `%APPDATA%\bougie\telemetry` on
   Windows. One line: `<mode> <yyyy-mm-dd> <consent-version>`.
4. Nothing set → off. This is the only state in which bougie may ask.

The first-run prompt appears at most three times (only on a real
terminal, in text mode, outside CI); after the third non-answer,
`off` is recorded and the question never returns.

If the *scope* of collection ever expands, the consent version bumps:
a recorded `on` under an older version stops uploading and you are
asked again. Reductions in scope never re-ask.

## What is collected

One NDJSON line per event. Envelope fields on every event:

| field | values | notes |
| --- | --- | --- |
| `schema` | `1` | wire-schema version |
| `event` | `command` \| `crash` | |
| `ts` | RFC 3339 UTC, **truncated to the hour** | sub-hour timing is never recorded |
| `install_id` | UUIDv4, or `"unset"` | minted only at consent; `telemetry reset` rotates it |
| `invocation` | UUIDv4 | per-process, never stored, correlates one run's events |
| `bougie_version` | semver | |
| `build_sha` | 9 hex chars | omitted when built without git (crates.io builds) |
| `os` | `linux` \| `macos` \| `windows` \| `other` | |
| `arch` | `x86_64` \| `aarch64` \| `other` | |
| `libc` | `gnu` \| `musl` \| `none` | |
| `ci` | boolean | CI environments only ever upload after an explicit opt-in |
| `install_method` | `installer` \| `cargo` \| `docker` \| `unknown` | |

`command` event fields:

| field | values |
| --- | --- |
| `name` | the subcommand verb (`sync`, `add`, `composer`, `server`, …) — a closed set; nested subcommands collapse to the parent verb |
| `duration_ms` | integer |
| `outcome` | `ok`, or an error *category*: `network`, `index-signature`, `manifest-hash`, `blob-hash`, `resolution`, `unknown-target`, `yanked`, `lock-held`, `filesystem`, `self-update`, `other` |
| `exit_code` | integer |

The category label and exit code are the *entire* error payload — no
error messages, no offending package, no URL, no path.

Commands that materialize a project (`sync` and the verbs built on
it) may additionally attach:

| field | values | notes |
| --- | --- | --- |
| `resolve_ms` / `vendor_ms` | integer | phase wall-clock |
| `packages_installed` | integer | freshly installed this run |
| `php_version` | `8.4`-style minor | never the patch level |
| `php_flavor` | lowercase token | closed set defined by the bougie index |
| `php_source` | `managed` \| `system` | |
| `extensions` | names from a fixed list in `bougie-telemetry/src/probe.rs` | anything else (private/local extensions) is dropped, not sent |
| `services` | subset of the service catalog (`mariadb`, `redis`, `opensearch`, `rabbitmq`, `mailpit`, `mkcert`, `server`) | |
| `direct_deps` / `total_deps` | bucket: `0`, `1-5`, `6-15`, `16-40`, `41-100`, `100+` | never a raw dependency count |

The `php_*`, `extensions`, `services`, and `*_deps` fields describe a
project's shape, so they ship **at most once per project per week**
(tracked by a local marker file; no project identifier is uploaded to
do this).

`crash` event fields (release builds only; one per crash signature
per day):

| field | values | notes |
| --- | --- | --- |
| `command` | same closed verb set as `command.name` | |
| `fingerprint` | 16 hex chars | `sha256` of the frame list — groups identical crashes |
| `frames` | ≤ 40 entries | symbol names kept **only** for bougie/Rust-runtime code (`bougie…`, `std::…`, `core::…`, `alloc::…`); everything else collapses to `[external]`. Stripped release binaries yield `+0x…` module-relative offsets instead — meaningless without the matching build artifact |
| `message` | ≤ 200 chars, scrubbed | anything path-shaped, anything containing your home directory, and quoted spans longer than 12 chars are replaced with `[redacted]` before the message leaves the process. Standard Rust panic messages ("index out of bounds: …") survive |

## What is never collected

Project names, directory paths, Composer package names, git remotes,
hostnames, usernames, environment variables, locale, full-resolution
timestamps, IP addresses (see below), MAC addresses or any
machine-derived identifier, and the contents of any request bougie
makes on your behalf.

## Transport and server-side handling

Events append to a local spool (`<cache>/telemetry/spool/`, capped at
1 MiB / 30 days) and upload later in gzip batches from a detached
child process running at the lowest scheduling priority — never
in-band with your command, never delaying your prompt.

At the collector:

- **IP addresses are used in memory for rate limiting only and are
  never written to storage.**
- Events failing this document's field tables are dropped.
- Raw events are pruned after 400 days; aggregates are kept.

## Inspecting, disabling, erasing

```
bougie telemetry              # status: mode, source, spool, ids
bougie telemetry log          # exactly what would be uploaded
bougie telemetry on|off|local
bougie telemetry reset        # rotate install id + purge spool
```

To have uploaded data deleted, open an issue on
`cresset-tools/bougie` (or mail the maintainer) quoting your
`install_id` — that id is only on your machine, so only you can make
that request.

## Network calls that are not telemetry

For transparency, bougie also talks to the network for its actual
job, independent of any telemetry setting: Packagist (or your
configured Composer repositories) for package metadata and dists, the
bougie index for PHP builds, and the release mirror for self-updates.
Those requests carry a `bougie/<version>` User-Agent and nothing
else. Composer's `notify-batch` download ping is **not** sent.
`bougie diagnose`-style bug reports (planned) will be their own
explicitly-confirmed, per-incident channel, independent of the
telemetry mode.
