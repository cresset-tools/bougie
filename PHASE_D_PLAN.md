# Phase D — VCS / source installs

Status: **in-flight** (scope locked 2026-07-21). The last major gap from
`RESOLVER_PLAN.md`: native git dependency support, end to end. Closes the
VCS-repository ignore tracked at `bougie-composer-resolver/src/update.rs`
(the `Some("vcs" | "github" | "git" | …) => Ok(())` follow-up).

Delete this file once shipped (repo convention — plans live in git history).

## Goal

Resolve versions from a `{type:vcs}` git repository **and** install a package
from a git `source` — with no upstream Composer and no registry in the loop.
After Phase D, a project can `require` / `update` / `lock` / `install` a
private-git or fork dependency using only bougie.

## Scope (locked)

- **Tier 2** — both halves of the problem:
  1. *Install* a package whose lock already pins a git `source` (works even
     when the lock was produced by upstream Composer).
  2. *Resolve* — `require`/`add`/`update`/`lock` discover versions from a
     `{type:vcs}` git repo (read `composer.json` at each tag/branch), solve,
     and write a `source` lock entry.
- **Git only.** Repo types `git`, `vcs` (→git), `github`, `gitlab`,
  `bitbucket`; transport https + ssh. Covers >99% of real PHP VCS deps.
- **Shell out to the user's `git`** (Composer-identical). Free ssh /
  credential-helper / proxy handling; a `git`-on-PATH dependency incurred
  only when a VCS dep is actually used.

### Not in v1 (seams left in place)

- `--prefer-source` — installing from git source when a dist exists
  (contributor/editable-checkout workflow). Tier 3; the
  `InstallOptions`/`ResolutionStrategy` seam is noted below so it drops in
  later.
- **Host dist-API optimization** — Composer will fetch a GitHub/GitLab/
  Bitbucket *zipball* for a tag instead of cloning. We always clone the
  source in v1 (simpler, works without host API tokens). Revisit as perf.
- `hg` / `svn` / `fossil` and `package` / `artifact` repository types.
- **Sandboxing the git subprocess** — `git` needs network + ssh-agent +
  credential-helper passthrough; Composer doesn't sandbox it, and the
  sandbox-by-default invariant covers *bougied-spawned services*, not
  CLI-driven subprocesses. Revisit only if the CLI ever sandboxes its own
  children.

## Why this is mostly wiring

The three layers are pre-shaped for it, and the **path-repository**
implementation (`update/path_repo.rs` + `seed_path_candidates` +
`RepoKind::Path` + the four `is_path()` short-circuits) is a near-exact
structural template — the only genuinely new machinery is git-ref
discovery/checkout.

- **Lock model already carries `source`.** `LockSource { kind, url,
  reference, mirrors }` at `bougie-composer/src/lockfile.rs:953`, on
  `LockPackage.source` (`:800`). Doc comment already says *"Phase D will use
  this when we add git-clone-as-source-install."* Note `LockSource.reference`
  is a **required** `String` (unlike `LockDist.reference: Option`) — a source
  always carries a checkout ref, so install can rely on it.
- **The lock writer needs no change.** Solved packages copy their cached
  `LockPackage` verbatim (`lock_package_for` `update.rs:786`, assembled at
  `update.rs:4428-4436`); a `source` block is written iff the seeded
  `LockPackage` carried one. So writing a git `source` entry is purely a
  matter of the driver producing `source: Some(..), dist: None`.

## Design

### Git operations — system git over a cached bare mirror

New module `crates/bougie-composer-resolver/src/vcs/` with a `git()` helper
extending the existing shape at `update/path_repo.rs:271-282`
(`Command::new("git").arg("-C")…`).

- **Cache:** a bare mirror per repo at `$BOUGIE_CACHE/composer-vcs/<sanitized-url>`,
  via a new `Paths::cache_composer_vcs()` one-liner alongside
  `cache_composer_dist()` (`bougie-paths/src/lib.rs:306`). Composer uses the
  same bare-clone-cache convention.
- **Metadata read (no working tree):** `git ls-remote --tags --heads` to
  enumerate refs, then `git -C <mirror> show <ref>:composer.json` (cat-file)
  to read each ref's manifest. One `fetch`/`remote update` per repo per
  resolve; immutable tags cached across runs, mutable `dev-*` branches
  refreshed each lock.
- **Install materialize:** from the mirror, populate `vendor_dest` and
  `git checkout <reference>` (the exact commit sha). No working history kept.
- **git-missing** raises a clear `bougie-errors` variant (exit-code mapped),
  only when a VCS dep is actually reached.

### Version model

- **tag** → normalized semver via `composer_semver` (strip leading `v`,
  skip non-semver tags — Composer parity).
- **branch** → `dev-<branch>`, honoring existing branch-alias support
  (`composer_semver::version::is_branch_alias`).
- The chosen ref's resolved **commit sha** becomes `LockSource.reference`.

### Resolution (Phase D2)

Mirror the path-repo path exactly:

- `RepoKind::Vcs(VcsRepoConfig)` added to `metadata.rs:101`; `is_vcs()`
  alongside `is_path()` (`metadata.rs:255`); parse the git-family types in
  `parse_repo_entry` (`update.rs:2335`, replacing the silent `Ok(())`).
- `seed_vcs_candidates` — sibling of `seed_path_candidates` (`update.rs:725`):
  `ls-remote` → per-ref `composer.json` → `Vec<LockPackage>` (one per ref,
  each carrying `require*`/`autoload` and `source: Some(git,url,sha)`,
  `dist: None`). Seed into `cache` (`update.rs:235`), record `vcs_owned_names`
  (cf. `path_owned_names` `update.rs:354`), register provide/replace virtuals.
- Extend the four `is_path()` short-circuits to also cover VCS-owned names:
  `discover_repos` (`update.rs:698`), `load_real_candidates` (`:1116`),
  `fetch_one` (`:1192`), prefetch BFS (`:1878`).
- Insert the seed step between `discover_repos` (`update.rs:4315`) and
  `pre_fetch_closure` (`:4331`).
- `versions_for` / `choose_version` / `get_dependencies` unchanged — they
  read the seeded cache transparently.

### Install (Phase D1)

- `source_urls()` accessor near `dist_urls()` (`lockfile.rs:1063`), with
  mirror-substitution parity.
- Split the `installable` filter (`orchestrate.rs:249-253`) into a dist-set
  and a source-set.
- Replace the source-only hard error (`orchestrate.rs:1201-1209`) with a
  **source-materializer** pass running parallel to the dist fetch (extend the
  two-phase `rayon` pipeline at `downloader.rs:148-178`, or a sibling clone
  pass), producing a populated `vendor_dest`.
- The post-materialize pipeline — patches, `extra.map` deploy, autoload dump,
  bin proxies (`orchestrate.rs:438-557`) — rejoins **unchanged**.

## Phases (each independently shippable)

- **D0 — git plumbing + cache.** `vcs/` module, `cache_composer_vcs()`,
  `ls-remote` / `show` / clone / checkout primitives, git-probe error.
  Unit-tested against local `file://` repos. No behavior change.
- **D1 — install locked source** *(early real-world value)*. Materializer
  replaces the hard error; `source_urls()`; installable split. Unblocks
  `install`/`sync` for any lock already pinning a git `source` (private forks,
  satis-less repos, Mirasvit-style vendors) — including locks produced by
  upstream Composer. Independent of D2.
- **D2 — VCS repository resolution.** `RepoKind::Vcs`, `seed_vcs_candidates`,
  `is_path`→`is_vcs` short-circuits, seed-step wiring. `require`/`add`/
  `update`/`lock` of a `{type:vcs}` package end-to-end; writes the `source`
  lock entries D1 installs.
- **D3 — auth + polish.** Private-repo auth (`bougie login` token overlay →
  git credential helper / token-in-URL), error taxonomy (git-missing / auth /
  ref-not-found), upgrade the `hg`/`svn`/`fossil`/`package`/`artifact` silent
  ignore to a warning, docs.

Order: **D0 → D1 → D2 → D3.** D1 and D2 both depend only on D0; D1 first for
the earliest usable slice.

## Critical files (anchors on `main`)

| Concern | Anchor | Action |
|---|---|---|
| Source coordinates in lock | `bougie-composer/src/lockfile.rs:953` `LockSource` | Add `source_urls()` near `dist_urls()` `:1063` |
| Git shell-out pattern | `bougie-composer-resolver/src/update/path_repo.rs:271-282` | Reuse shape in new `src/vcs/git.rs` |
| VCS cache dir | `bougie-paths/src/lib.rs:306` `cache_composer_dist` | Add `cache_composer_vcs` sibling |
| Repo type recognition | `update.rs:2335-2343` (ignore); enum `metadata.rs:101-105`; `is_path` `metadata.rs:255` | `RepoKind::Vcs`, parse git-family, `is_vcs()` |
| Candidate seeding | `seed_path_candidates` `update.rs:725`; `cache` `:235`; `path_owned_names` `:354` | Add `seed_vcs_candidates` + `vcs_owned_names` |
| Network short-circuits | `is_path()` at `update.rs:698`, `:1116`, `:1192`, `:1878` | Also skip VCS-owned names |
| Seed-step wiring | `solve_into_lock_packages` `update.rs:4315→4325→4331` | Insert seed between discover & prefetch |
| Lock entry (source) | `lock_package_for` `update.rs:786` → `:4428-4436` | **No change** — source flows from seeded `LockPackage` |
| Install hard error | `orchestrate.rs:1201-1209` (preflight) | Replace with git-clone materializer |
| Install-set filter | `orchestrate.rs:249-253` | Split dist-set vs source-set |
| Parallel materialize | `downloader.rs:148-178` (rayon two-phase) | Add clone+checkout producing `vendor_dest` |
| Post-materialize rejoin | `orchestrate.rs:438-557` | Unchanged |
| prefer-source seam (v-next) | `InstallOptions` `orchestrate.rs:44`; strategy flags `bougie-cli/src/lib.rs:206` | Left in place for Tier 3 |

## Testing

- **Hermetic, no network.** Build ephemeral git repos in a tempdir
  (`git init`, commit a `composer.json`, `git tag v1.2.3`) and reference them
  by `file://` — deterministic and offline. Guard with skip-if-no-git (CI
  runners already ship git).
- **D0 unit:** `ls-remote` enumerates tags+branches; `show <ref>:composer.json`
  reads the manifest; clone+checkout lands the correct tree at a ref.
- **D2:** `seed_vcs_candidates` over a `file://` fixture → versions
  discovered, `require`/`provide`/`replace` parsed, `source` lock entry
  written; solve picks highest-in-range; `dev-<branch>` + branch-alias cases.
- **D1:** `install_from_lock` over a lock with a `file://` `source` → vendor
  tree present at the pinned sha, autoload/bin rejoin, and a mixed
  path+dist+source lock in one project.
- **Cross-check (optional):** commit a tiny fixture repo bundle and compare
  resolution against Composer, following the existing frozen `cross-check`
  pattern (`crates/bougie/tests/composer_cross_check.rs`).
- **Windows parity is near-free:** git-for-windows on PATH; `file://`
  fixtures + checkout are cross-platform. Add a `windows_smoke` case.
- No new heavy dependencies (git is shelled out), so the `unsafe_code`
  allowlist and sandbox invariants are untouched.

## Open questions

1. **Private-git auth (D3).** Rely purely on the user's git credential
   helper / ssh-agent (zero-config, Composer-like), or *also* inject
   `bougie login` tokens into the clone URL for CI? *Lean:* credential-helper
   first; token-injection as an opt-in overlay (reuses the existing
   `bougie login` repositories merge).
2. **Ref-read strategy.** Single bare mirror + `git show <ref>:composer.json`
   per ref (one fetch/repo), vs. Composer's per-ref reads. *Lean:* bare
   mirror + cat-file.
3. **Mutable-branch cache TTL.** Re-fetch `dev-*` branches every resolve vs.
   cache with an explicit `--refresh`. *Lean:* refresh mutable refs each lock,
   cache immutable tags across runs.
