# Error visibility — implementation plan

Working plan for making bougie's failures legible — to the maintainer
reading the telemetry dashboard, to the user reading stderr, and to
`bougie diagnose` assembling a report after the fact.

**Status: Phases 0–2 and 4 done; Phase 3 (usage lane) remaining.**
Phases 0–2 shipped in bougie-v0.47.0 (released off-branch, only
#485); collector deployed with the failure panel + 0.47 vocab — the
rollout gate is closed. Phase 4 (2026-07-10): the single
`last-failure.json` slot became a ring under `telemetry/failures/`
with consecutive identical failures collapsing into a repeat counter
(motivated by a real 1,555-event `watch`-loop burst that overwrote
its own evidence), `bougie diagnose --last`, an "earlier failures"
report section, and category-keyed stderr hints (network, service).
Remaining: Phase 3, Phase 5 (measure `other` share dropping), plus
data-driven iteration on `new`/`composer` verb failures once the new
categories start reporting; collector-side, consider an
install-weighted failure view so one runaway machine can't dominate
the panel.

Motivating data (2026-07-09): the collected command events show
**17 `other` outcomes against 71 `ok`** — roughly one invocation in
five fails, and nearly every failure is uncategorized. The cause is
structural, not a bug: `outcome_for_error`
(`bougie-telemetry/src/event.rs`) recognizes exactly the ten typed
`BougieError` variants, and the rest of the codebase errors via ad-hoc
eyre — **663 `bail!`/`eyre!` sites** across the workspace (209 in
`bougie/src/commands/` alone) versus **16 typed `BougieError`
constructions** in the bin. The taxonomy covers the plumbing lanes
(index signatures, hash mismatches, self-update) that rarely fire, and
misses the everyday failures (no project in cwd, bad config, PHP not
resolvable, service startup, unsupported-on-Windows) that actually do.

Two facts about the existing data worth keeping in mind:

- **`other` is real failures, not usage noise.** clap parse errors
  call `exit()` inside `Cli::parse` before the recorder exists
  (`main.rs`), so mistyped flags produce *no event at all*. The
  `usage` vocab entry is reserved but never emitted today.
- **`exit_code` adds nothing for these events.** Untyped errors all
  map to exit code 1 (`bougie-errors/src/lib.rs::exit_code_for`), so
  the only discriminators the collector has for an `other` event are
  `name` and `install_id`.

## Design principles

- **The privacy contract is fixed.** A failed command's entire wire
  payload stays "category label + exit code" — closed vocabulary, no
  free text (TELEMETRY.md). Better visibility means *finer categories*
  and *better local capture*, never message content on the wire.
- **Data-driven widening.** New categories are chosen from the
  per-command breakdown of real `other` events (Phase 0), not from
  guessing which of the 663 sites matter. We type the load-bearing
  sites in the offending commands; we do not convert 663 call sites.
- **Type at the boundary, classify in one place.** Leaf crates that
  own a failure domain (fetch → network, config → config) construct
  typed errors at their public boundary; `outcome_for_error` stays the
  single classification point. `bougie-telemetry` gains no new heavy
  deps for downcasting — `std::io::Error` is free, and HTTP errors are
  typed where reqwest already is a dep (bougie-fetch, backends).
- **Exit codes are public API.** Every new variant gets a distinct,
  documented exit code; codes are never reused or renumbered.
- **Collector-first rollout.** The collector validates
  `outcome` against the vocab imported from the *published*
  `bougie-telemetry` crate. Any vocab widening ships in order:
  publish crate → bump collector dep + deploy (cresset-tools/infra) →
  release the client that emits the new labels. Same ordering rule
  diagnose v2 followed for `schema_version: 2`.

## Phase 0 — measure what we already have (collector-side only)

No client change; can start today against live data.

1. **Breakdown query/panel: `other` by command `name`.** Which verbs
   produce the 17? Also split by `install_id` count — 17 failures
   from 17 installs is a product problem; 17 from one install is one
   user's broken loop (hour-truncated `ts` + `invocation` help here).
2. **Failure-rate panel per command over time**, with `other` share
   highlighted. This is the metric later phases move; capture the
   baseline before shipping anything.
3. Deliverable: a ranked list of (command, other-count, distinct
   installs) that picks the Phase 2 category set.

Repo: cresset-tools/infra (dashboard lives with the collector).

### Findings (2026-07-09, live query)

| command | `other` events | distinct installs |
|---|---|---|
| `start` | 5 | **3 — every failing install** |
| `new` | 4 | 2 |
| `composer` | 3 | 2 |
| `patches` | 2 | 1 |
| `tool` / `projects` / `add` | 1 each | 1 each |

- 17 `other` from **3 installs** (of 6 total), all interactive, zero
  CI, versions 0.43.0–0.46.0. Not one broken loop — but this is
  dogfood-scale data: install `0ae69521` (11/17 events) is the dev box
  this analysis ran on. Shapes generalize, absolute rates don't.
- The single `filesystem` event confirms the typed lane works
  end-to-end.
- Ground truth from the dev box's local `last-failure.json`: a
  `bougie-run --help` shim failure whose chain roots in
  `std::io::Error` ("No such file or directory") — Phase 1's
  chain-walk would already classify it. It also postdates all 11 of
  that install's telemetry events, i.e. the single slot had been
  overwritten 11 times — Phase 4's ring is not hypothetical.
- Phase 2 consequences: `start` failing on every install makes
  `service`/`subprocess`/recipe failures the top typing target;
  `new` (scaffold/starter lane) and `composer` next. No Windows
  events → drop the `unsupported` label idea for now; no
  php-selection evidence yet → keep folded until data says otherwise.

## Phase 1 — chain-walking classification (client-only, no wire change)

Reclassify a chunk of `other` into *existing* vocabulary. No new
labels, so no collector coordination; ships in any client release.

1. `outcome_for_error` walks `err.chain()` (today it downcasts the
   report root only, which sees through `wrap_err` layers but not
   through errors stringified early):
   - any `BougieError` in the chain → its category (as today);
   - else any `std::io::Error` in the chain → `filesystem`.
2. Audit the fetch/metadata boundaries for reqwest errors escaping
   untyped (`bougie-composer/src/metadata.rs` and friends) and wrap
   them in `BougieError::Network` at the boundary, where the main
   `bougie-fetch`/`bougie-index`/backend paths already do.
3. Fix the anti-pattern where a typed error is flattened into a
   string (`eyre!("{e}")`) before propagating — those lose their
   category irrecoverably. Grep-audit, small diff.
4. Unit tests: chain-wrapped `BougieError` still classifies; io-rooted
   chains classify as `filesystem`; TELEMETRY.md gets a one-line note
   that `filesystem`/`network` now also cover chain-rooted causes.

### Implementation notes (2026-07-09)

- `reqwest` was already a bougie-telemetry dep (flush client), so the
  network downcast added no dependency. Check order is network before
  io because a real transport error carries an io root in the same
  chain (covered by a refused-connection test).
- `std::io::Error::source()` *skips* a custom boxed inner error and
  returns its source — an io::Error wrapping another error hides that
  error's type from chain-walking entirely. Don't wrap non-io causes
  in io::Error.
- Boundary conversions (`map_err(|e| eyre!("…: {e}"))` →
  `wrap_err_with`) done in the Phase-0-implicated CLI-reachable lanes:
  bougie-patches apply (io sites), bougie-tool receipt/list/run,
  bougie-fs lock acquisition, resolver composer.json *read* +
  autoload-dump (DumpError exposes its io root via `source()`).
  ~120 further stringification sites remain, mostly parse/domain
  errors whose category doesn't exist until Phase 2 — convert them
  with their variants.
- **`bougie-daemon` sites are useless to convert for telemetry**:
  bougied is a separate process; its errors reach the CLI over the
  control socket as strings. Typing `start`/`service` failures
  (Phase 2) must happen in the CLI's daemon-client layer, mapping
  protocol error kinds to `BougieError` variants — the wire already
  carries a kind marker (e.g. `(service_start_failed)`).

## Phase 2 — widen the taxonomy (wire-contract change, data-driven)

The core phase. Final category list comes from Phase 0; expected
candidates, mapped to where they'd be constructed:

| candidate label | failure family | typed where |
|---|---|---|
| `no-project` | no composer.json/bougie.toml around cwd | project-locate helpers (`bougie-paths::project`, `failure::project_root_near` callers) |
| `config` | unparseable/invalid composer.json, bougie.toml, server.toml | `bougie-config`, `bougie-composer` model boundary |
| `php-selection` | no matching PHP/extension for the requested constraint | `bougie-resolver`, `bougie-installer` |
| `service` | daemon/service lifecycle failures (start, health, socket) | `bougie-daemon` boundary in `commands/service` |
| `subprocess` | a spawned tool/php that bougie itself required failed | `bougie-tool`, run/exec plumbing |
| `unsupported` | Windows unsupported-feature path (`unsupported_on_windows`) | `bougie/src/lib.rs` |

Mechanics per label:

1. New `BougieError` variant with a distinct exit code; extend the
   `each_variant_has_distinct_code` test and the exit-code table in
   docs.
2. Add the label to `OUTCOME_VOCAB` + the TELEMETRY.md outcome row
   (public contract).
3. Convert the handful of load-bearing construction sites in the
   commands Phase 0 implicated. Everything else stays eyre and keeps
   falling through — the goal is `other` < ~5% of failures, not zero.
4. Rollout ordering per the design principle: publish
   `bougie-telemetry`, bump + deploy collector, then release the
   client. (Allowlist widening = dep bump — the established flow.)

## Phase 3 — the usage lane

Make mistyped invocations visible instead of invisible.

1. `main.rs`: `Cli::try_parse()` instead of `parse()`. On
   `ErrorKind::DisplayHelp`/`DisplayVersion`, print and exit as clap
   would — not recorded (help is not an error, and recording it is
   noise). On real parse errors, spool a command event with
   `outcome: "usage"`, `exit_code: 2` (clap's convention), name
   best-effort: the first token if it matches `COMMAND_VOCAB`, else
   `unknown`. Then print clap's rendered error and exit with its code.
2. Recorder today is constructed inside `bougie::run` *after* parse;
   the usage lane needs a slim spool-only path that doesn't require a
   parsed `Cli` (no consent prompt from this path — mode-gated spool
   only, same as `Recorder::init` semantics).
3. `usage` is already in `OUTCOME_VOCAB` and the collector contract —
   no vocab change, ships independently of Phase 2.

## Phase 4 — local visibility: failure lane + stderr hints

The unscrubbed detail already stays local by design; make it more
useful there.

1. **Failure ring instead of single slot.** `last-failure.json` keeps
   only the newest failure; a repeated flake overwrites the
   interesting one. Keep a small ring (say 10) under
   `<cache>/telemetry/failures/`, same schema, local-only. `bougie
   diagnose` shows the recent set (count + categories + first lines),
   not just the latest.
2. **Category-aware stderr hints.** `report_error` appends one static
   hint line keyed on the category where a specific next step exists:
   `lock-held` → the holding pid is in the message, `network` →
   offline/proxy pointer, `no-project` → `bougie init`/cd hint,
   `service` → `bougie service logs <name>`. One line, no essays;
   uv-style.
3. `bougie diagnose --last` (or a section in the default report):
   print the recorded chain(s) locally without assembling/sending a
   full report.

## Phase 5 — docs + measurement

1. TELEMETRY.md: updated outcome table (it is the public contract),
   plus the exit-code table if it doesn't exist as a doc yet.
2. Watch the Phase 0 dashboard: success = `other` share of failures
   drops below ~5% and the failure-rate-per-command panel becomes
   actionable. Iterate Phase 2's category set if a new cluster forms.
3. Delete this file when shipped.

## Explicit non-goals / decided against

- **No free-text or fingerprints on the command-event wire.** Crash
  fingerprints exist in the crash lane with scrubbing guarantees;
  the command lane stays label+code.
- **No blanket conversion of eyre sites.** Typing all 663 sites is
  churn without payoff; the chain-walk + boundary typing covers the
  mass, the data picks the rest.
- **Ok-lane nonzero exits stay `ok`/0.** `lib.rs` documents this
  deliberately (verb-specific soft-failure codes like `composer
  audit`'s advisory exit are not errors). Revisit only if Phase 0
  shows a need to see child-process failure rates from `run`/`make` —
  that would be a separate, argued-for change to a documented
  decision.

## Open questions

- `php-selection` vs folding into `resolution`: distinct populations
  (toolchain resolve vs composer dep solve) argue for distinct labels;
  decide when Phase 0 data shows whether the volume justifies it.
- Does `unsupported` earn a label, or is Windows volume too small to
  matter yet? (`os` field already isolates it — a dashboard cut may
  suffice without a vocab entry.)
- Ring size / retention for Phase 4 failures dir (10 files? 7 days?).
