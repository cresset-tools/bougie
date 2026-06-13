# CLI_REORG_PLAN.md

Reorganize the top-level CLI so the core workflow is discoverable and the
"how do I start my project?" question has exactly one answer. Staged so each
phase ships behind conventional commits without breaking muscle memory.

Status: **Phases 1–3 done** (branch `feat/cli-reorg`); Phase 4 pending.
Delete this file once all phases ship.

Note: the existing `phase1x_services_*` integration tests still drive the
deprecated top-level `bougie up`/`down` aliases (they print a stderr notice
but forward fine). Migrate them to `bougie services up`/`down` in the same
commit that *removes* the aliases — until then they double as forwarding
coverage.

## Goals

1. One umbrella lifecycle verb: `bougie start` / `bougie stop` (ddev/Lando
   parity — the convention our Magento-agency buyers already know).
2. Stop overloading "start": `make` becomes a pure task runner, not a second
   spelling of `start`.
3. Granular service control lives under `bougie services` again; top-level
   `up`/`down` go away (as aliases, then removed).
4. Flag and vocabulary consistency across the lifecycle verbs.
5. A `--help` that reads as a curated core, not a flat list of 20 verbs.

## The problem being solved

Today four verbs all sound like "start my stuff" and nothing disambiguates:
`start` / `make` (run the recipe), `up` (services only), `server` (HTTP only),
`sync` (deps only). `bougie start` is documented as "a zero-arg alias for
`bougie make start`", which makes it look redundant. And `up`/`down` were
promoted to top-level (commit 62babc8-era), but `bougie up` only touches
services while reading like it brings up the whole project.

Resolution: `start`/`stop` is the **umbrella** (sync + services + server +
recipe); everything else is a narrower knob underneath it.

---

## Key fact: the recipe DAG is *already* the umbrella

Don't re-derive this in code. The builtin recipes
(`crates/bougie-recipe/recipes/*.toml`) already orchestrate the full bring-up:
the `start` task depends on `vendor` (`run = "bougie sync"`), `services`
(`bougie services add … && bougie up --detach …`), `install`, `reindex`, and
finishes with `bougie up --detach server`. So `bougie start` ≡ `bougie make
start` *because the recipe composes sync + services + server already*.

Therefore the umbrella is **"run the recipe's `start` task"**, not a new
hand-rolled `sync → up → server → recipe` sequence (that would double-run every
step). The job of Phase 1 is to make `start`/`stop` first-class *verbs* and stop
framing `start` as a mere alias — not to re-implement orchestration.

## Phase 1 — `start` / `stop` as first-class lifecycle verbs

**CLI (`crates/bougie-cli/src/lib.rs`)**
- Add `Command::Start` and `Command::Stop` variants.
  - `start`: thin wrapper over the recipe's `start` task. Carry the
    recipe-relevant `make` flags so it's a real verb, not a stub: `--no-sync`,
    `--no-builtin`, `--recipe <NAME>`, `--dry-run`, `--explain`. Unix-only
    (the recipe/services stack is), same `#[cfg]` split as `make`.
  - `stop`: new teardown — no recipe equivalent today. Flags `--purge`
    (forwarded to the services teardown) and an optional `names` positional so
    `bougie stop redis` works symmetrically with `start`.
- Keep `Command::Make` but **drop its `start` alias** (`#[command(alias =
  "start")]`, ~line 361) and change no-arg behavior: bare `bougie make` lists
  tasks (like `just`) instead of running `start`. Update its doc comment so it
  no longer claims equivalence with `bougie start`.

**Dispatch (`crates/bougie/src/lib.rs`)**
- `Command::Start` → `commands::make::run` with `task: Some("start")` and the
  forwarded flags. This is exactly what `init`'s `start_project`
  (`commands/init.rs:203`) already does — factor that into the new
  `commands::start::run` and have `init` call it.
- `Command::Stop` → dispatched inline to `services::down::run` for all
  declared services. The `server` tenant comes down as one of those declared
  services; a *global* `server stop` is deliberately NOT run — it would tear
  down hosting for every other project sharing the daemon. `--purge` plumbs
  through to `down`. No separate `stop` module needed (it's a one-liner); if a
  recipe later grows a `stop` task, add a module then.
- Both Unix-only; reuse the `unsupported_on_windows` pattern.

**`init`/`new` `--start`** (lib.rs ~85, ~109; `commands/init.rs:203`): repoint
to `commands::start::run` so there's a single start path. Behavior is
unchanged (still `make start`), just deduplicated.

**Tests**: `start` runs the `start` task and honors `--no-sync`/`--dry-run`;
`stop` brings declared services down and stops the server. Reuse the services
up/down + make test harnesses.

**Commit**: `feat(cli): promote 'start'/'stop' to first-class lifecycle verbs`

---

## Phase 2 — move `up`/`down` back under `services`

The impls already live at `commands::services::up`/`down` — this is a CLI-shape
change, not a logic move.

- Add `Up`/`Down` arms to `ServicesCommand` (mirror the current top-level
  field sets: `up { names, detach }`, `down { names, purge }`).
- Make the top-level `Command::Up`/`Command::Down` **hidden deprecated
  aliases** (`#[command(hide = true)]`) that print a one-line "use `bougie
  services up`" notice and forward, for one release.
- Dispatch (`crates/bougie/src/lib.rs` lines ~184–190 and the
  `ServicesCommand` match block ~502–540): route the new `services up/down`
  arms; keep the deprecated top-level arms forwarding.
- **Update the builtin recipes** (`crates/bougie-recipe/recipes/*.toml`): they
  shell out to `bougie up --detach …`. Rewrite to `bougie services up
  --detach …`. The deprecated alias keeps them working in the interim, but the
  recipes are ours — move them to the new spelling in the same commit. Grep
  for `bougie up`/`bougie down` across `recipes/` and any docs/fixtures.

**Commit**: `feat(cli): move 'up'/'down' back under 'bougie services'`
Then a follow-up release later: `feat(cli)!: remove deprecated top-level up/down`.

---

## Phase 3 — flag + vocabulary consistency

Small, mechanical, high-signal.

- **Attach/detach**: `server`'s `ServeArgs::no_attach` (lib.rs ~523) vs `up`'s
  `-d/--detach`. Standardize on `-d/--detach` everywhere; keep `--no-attach` as
  a hidden alias on `server` for one release.
- **`--purge` overload**: three meanings today —
  - `down --purge` → destroy tenant data (real)
  - `services remove --purge` → reserved no-op (lib.rs ~404)
  - `projects purge` → deprovision (real)
  Decide: either implement `services remove --purge` to mean the same
  "destroy tenant data" as `down --purge`, or drop the flag until it does
  something. No silent no-op flags in the core surface.
- **`server stop` vs `services down server`**: investigated — these are NOT
  duplicates. `server stop` shuts the *shared* dev server down for every
  project; `services down server` only deprovisions *this* project's host
  tenant. Left both; the help text already states the distinction. No dedup.

**Done.** `--no-attach` → `-d/--detach` (alias kept); `services remove --purge`
now runs the real deprovision path; `server stop` left as-is (not a dup).

**Commit**: `refactor(cli): standardize lifecycle flags (detach, purge)`

---

## Phase 4 — help grouping / curated core

20 flat subcommands is the discoverability problem in miniature. uv groups its
`--help`; clap can't group subcommands natively, but `next_help_heading` +
deliberate variant ordering approximates it.

Target grouping:
- **Project**: `init`, `new`, `start`, `stop`, `sync`, `run`, `make`
- **Dependencies**: `add`, `remove`, `lock`, `tree`, `outdated`, `composer`,
  `ext`
- **Toolchain**: `php`, `tool`
- **Services & serving**: `services`, `server`
- **Admin**: `cache`, `self`, `projects`

Open question: `projects` was deliberately promoted to top-level this branch.
It's low-frequency tenant admin — fits "Admin" above, but reverting placement
is a separate call. Leave top-level for now; just group it under Admin.

**Commit**: `docs(cli): group --help into project/deps/toolchain/services/admin`

---

## Out of scope (revisit later)

- Whether `make` should be renamed entirely (e.g. folded into `run <task>`).
  Keeping `make` for now; only its `start` overlap is removed.
- Reverting `projects` from top-level.
- Any change to the `composer …` compat surface — it's deliberately
  Composer-shaped and stays as-is.

## Migration / compat summary

| Old | New | Transition |
|-----|-----|-----------|
| `bougie start` (= make start) | `bougie start` (umbrella) | superset, no break |
| `bougie make` (ran start) | `bougie make` (lists tasks) | **behavior change** — call out in CHANGELOG |
| `bougie up` | `bougie services up` | hidden alias 1 release, then removed |
| `bougie down` | `bougie services down` | hidden alias 1 release, then removed |
| `server --no-attach` | `server -d/--detach` | hidden alias 1 release |

The `bougie make` no-arg change is the only hard behavior break; everything
else is additive + aliased. Gate it on a `feat!` so release-please flags it.
