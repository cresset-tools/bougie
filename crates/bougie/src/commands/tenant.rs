//! Shared project → tenant-identity derivation.
//!
//! A "tenant" is a project's slot inside the shared dev services — the
//! mariadb database, rabbitmq vhost, opensearch index prefix, redis db,
//! and the `<tenant>.bougie.run` server hostname. Both the `service up`
//! provisioning path (`service::up`) and the `bougie server` project
//! verb (`commands::server`) derive it, and they must agree: `server
//! open` / `server logs` re-derive the name to find an already-running
//! project, so the function has to be stable and consistent.
//!
//! The name is the **sanitized project directory basename**, not the
//! `composer.json` `name`: every project skeleton shares one composer
//! name (`mage-os/project-community-edition`, `laravel/laravel`,
//! `symfony/skeleton`, …), so composer-name-first collapsed *distinct
//! projects onto one tenant* — they ended up sharing a database, vhost,
//! and hostname. The directory name is the signal that actually
//! distinguishes them.
//!
//! Uniqueness + stability come from a durable per-project record plus the
//! on-disk tenant ledgers:
//! 1. If this project already owns a live tenant (a ledger row), reuse it
//!    verbatim (so DB names and hostnames survive `up`/`down`/`up`).
//! 2. Otherwise, if we already committed to a name for this project,
//!    reuse it. The name is cached under
//!    `$BOUGIE_HOME/state/projects/<hash>/tenant`, so it outlives the
//!    ledger row that `down --purge` deletes. Without this, re-derivation
//!    could *drift*: a same-basename sibling project appearing or
//!    disappearing between runs would flip the basename/hash decision,
//!    stranding the DB credentials baked into the project's
//!    `app/etc/env.php` and breaking auth (the user is provisioned under
//!    one name, Magento connects under the other).
//! 3. Otherwise use the basename.
//! 4. If a *different* project already holds that basename, append a
//!    short hash of the canonical path to disambiguate.
//!
//! Steps 3–4 run only once: the chosen name is persisted, so steps 1–2
//! answer every later call.

use std::path::Path;

#[cfg(unix)]
use bougie_daemon::daemon::catalog::{self, Tenancy};
#[cfg(unix)]
use bougie_daemon::daemon::tenants::Tenant;
use bougie_paths::Paths;

/// Normalise an arbitrary label into a tenant slug: lowercase ASCII
/// alphanumerics kept, everything else (slashes, dashes, dots) → `_`.
///
/// Leading/trailing `_` are trimmed: the tenant becomes a DNS label via
/// `<tenant>.bougie.run` (with `_` rendered as `-`), and a label may not
/// begin or end with `-`. A directory like `.tmpAbC` or `my-project-`
/// would otherwise produce an invalid hostname. Falls back to `project`
/// when nothing alphanumeric survives.
pub fn sanitize_tenant(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => out.push(c.to_ascii_lowercase()),
            _ => out.push('_'),
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Every provisioned tenant across all multi-tenant services, as
/// `(service_name, tenant)`. Best-effort: a missing, unreadable, or
/// partially-written ledger contributes nothing rather than failing
/// derivation. Empty on platforms with no daemon (the standalone
/// Windows server), which collapses derivation to the plain basename.
#[cfg(unix)]
pub fn load_all_tenants(paths: &Paths) -> Vec<(String, Tenant)> {
    let mut out = Vec::new();
    for entry in catalog::CATALOG {
        // Runtime-only deps (jdk, erlang) have no tenant ledger.
        if matches!(entry.tenancy, Tenancy::None) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(paths.service_tenants(entry.name)) else {
            continue;
        };
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(t) = serde_json::from_str::<Tenant>(line) {
                out.push((entry.name.to_string(), t));
            }
        }
    }
    out
}

/// Derive the default tenant identity for `project_root`. See the module
/// docs for the rationale; the rules are live-ledger reuse → durable
/// cache → basename → hash on collision, with the chosen name persisted
/// so it stays stable across a `down --purge` that wipes the ledger row.
#[cfg(unix)]
pub fn derive_default_tenant(project_root: &Path, paths: &Paths) -> String {
    let canonical = canonical_path(project_root);
    let existing = load_all_tenants(paths);
    let cached = read_tenant_cache(paths, &canonical);
    let (name, persist) = resolve_tenant(project_root, &canonical, &existing, cached.as_deref());
    if persist {
        write_tenant_cache(paths, &canonical, &name);
    }
    name
}

/// Pure resolution policy, split from I/O so it can be unit-tested with a
/// synthetic ledger + cache. Returns the tenant name and whether the
/// caller should persist it to the durable cache.
///
/// A live ledger row is authoritative when present (and we still signal a
/// persist, to backfill the cache for projects provisioned before it
/// existed). A cached name wins over re-derivation, so the identity can't
/// drift when a same-basename sibling project appears or disappears.
#[cfg(unix)]
fn resolve_tenant(
    project_root: &Path,
    canonical: &Path,
    existing: &[(String, Tenant)],
    cached: Option<&str>,
) -> (String, bool) {
    if let Some((_, t)) = existing.iter().find(|(_, t)| t.project == canonical) {
        return (t.tenant.clone(), true);
    }
    if let Some(name) = cached {
        return (name.to_string(), false);
    }
    (derive_from_ledger(project_root, canonical, existing), true)
}

/// `$BOUGIE_HOME/state/projects/<hash>/tenant` — the durable record of the
/// tenant name this project committed to. Lives outside `vendor/` and
/// outside the service ledgers, so it survives both a `vendor` wipe and a
/// `down --purge`.
#[cfg(unix)]
fn tenant_cache_path(paths: &Paths, canonical: &Path) -> std::path::PathBuf {
    paths.project_state_dir(canonical).join("tenant")
}

/// Read the cached tenant name, if any. Best-effort: a missing, empty, or
/// unreadable file simply means "not cached".
#[cfg(unix)]
fn read_tenant_cache(paths: &Paths, canonical: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(tenant_cache_path(paths, canonical)).ok()?;
    let name = raw.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Persist the chosen tenant name. Best-effort: a write failure only
/// costs stability across a future purge, so it must never break
/// derivation. No-ops when the file already records the same name.
#[cfg(unix)]
fn write_tenant_cache(paths: &Paths, canonical: &Path, tenant: &str) {
    if read_tenant_cache(paths, canonical).as_deref() == Some(tenant) {
        return;
    }
    let path = tenant_cache_path(paths, canonical);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{tenant}\n"));
}

/// On platforms with no daemon (the standalone Windows server) there are
/// no tenant ledgers, so derivation collapses to the sanitized basename —
/// no reuse/collision logic to run.
#[cfg(not(unix))]
pub fn derive_default_tenant(project_root: &Path, _paths: &Paths) -> String {
    sanitize_tenant(
        project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project"),
    )
}

/// The collision/reuse logic split out from I/O so it can be unit
/// tested with a synthetic ledger.
#[cfg(unix)]
fn derive_from_ledger(
    project_root: &Path,
    canonical: &Path,
    existing: &[(String, Tenant)],
) -> String {
    // (1) Stability: this project already has a tenant somewhere — reuse
    //     it verbatim so its DB name / hostname don't move under it.
    if let Some((_, t)) = existing.iter().find(|(_, t)| t.project == canonical) {
        return t.tenant.clone();
    }

    // (2) The sanitized directory basename.
    let base = sanitize_tenant(
        project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project"),
    );

    // (3) Disambiguate only if a *different* project already holds it.
    let taken_by_other = existing
        .iter()
        .any(|(_, t)| t.tenant == base && t.project != canonical);
    if taken_by_other {
        return format!("{base}_{}", short_hash(canonical));
    }
    base
}

/// Canonicalize for stable identity + ledger comparison (the ledger
/// stores canonical paths). Falls back to the path as-given when it
/// can't be resolved (e.g. it no longer exists).
#[cfg(unix)]
fn canonical_path(p: &Path) -> std::path::PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Short, stable hex digest of the canonical project path, used only to
/// disambiguate same-basename projects. FNV-1a — no cryptographic
/// strength needed, just a deterministic 24-bit (6 hex) tag.
#[cfg(unix)]
fn short_hash(p: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in p.as_os_str().as_encoded_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:06x}", h & 0x00ff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_normalises_slash_and_dash() {
        assert_eq!(sanitize_tenant("acme/blog"), "acme_blog");
        assert_eq!(sanitize_tenant("My-Project"), "my_project");
        assert_eq!(sanitize_tenant("ACME"), "acme");
    }

    #[test]
    fn sanitize_trims_leading_trailing_separators_for_valid_hostnames() {
        // `.tmpAbC` (tempdir / hidden dir) and trailing dashes would
        // otherwise yield `-tmpabc.bougie.run` / `foo-.bougie.run`,
        // which the host validator rejects.
        assert_eq!(sanitize_tenant(".tmpAbC"), "tmpabc");
        assert_eq!(sanitize_tenant("my-project-"), "my_project");
        assert_eq!(sanitize_tenant("__weird__"), "weird");
        // Nothing alphanumeric survives → fallback.
        assert_eq!(sanitize_tenant("///"), "project");
        assert_eq!(sanitize_tenant(""), "project");
    }
}

// Ledger-backed derivation only exists on Unix (no daemon elsewhere), so
// its tests use `Tenant`/`derive_from_ledger` and are gated to match.
#[cfg(all(test, unix))]
mod ledger_tests {
    use super::*;
    use std::path::PathBuf;

    fn tenant(name: &str, project: &str) -> (String, Tenant) {
        // Tenant has no public constructor with a preset tenant string,
        // so go through JSON (it's Deserialize).
        let t: Tenant = serde_json::from_str(&format!(
            "{{\"schema_version\":1,\"tenant\":\"{name}\",\"project\":\"{project}\",\"created_at\":\"2026-06-05T00:00:00Z\"}}"
        ))
        .unwrap();
        ("mariadb".to_string(), t)
    }

    #[test]
    fn basename_used_not_composer_name_for_a_fresh_project() {
        let p = PathBuf::from("/home/u/dev/nebula");
        assert_eq!(derive_from_ledger(&p, &p, &[]), "nebula");
    }

    #[test]
    fn two_distinct_skeleton_projects_do_not_collide() {
        // Both are mage-os/project-community-edition, but their dirs
        // differ → distinct tenants (the whole point).
        let a = PathBuf::from("/home/u/dev/mageos-rma");
        let b = PathBuf::from("/home/u/dev/nebula");
        assert_eq!(derive_from_ledger(&a, &a, &[]), "mageos_rma");
        assert_eq!(derive_from_ledger(&b, &b, &[]), "nebula");
    }

    #[test]
    fn existing_project_tenant_is_reused_for_stability() {
        let p = PathBuf::from("/home/u/dev/shop");
        // Already provisioned under a legacy name → keep it.
        let ledger = vec![tenant("legacy_name", "/home/u/dev/shop")];
        assert_eq!(derive_from_ledger(&p, &p, &ledger), "legacy_name");
    }

    #[test]
    fn same_basename_different_project_gets_hash_suffix() {
        let mine = PathBuf::from("/home/u/a/shop");
        let other = "/home/u/b/shop";
        let ledger = vec![tenant("shop", other)];
        let got = derive_from_ledger(&mine, &mine, &ledger);
        assert!(got.starts_with("shop_"), "got {got}");
        assert_ne!(got, "shop");
        // Deterministic.
        assert_eq!(got, derive_from_ledger(&mine, &mine, &ledger));
    }

    #[test]
    fn no_suffix_when_same_project_already_holds_the_basename() {
        // The "collision" is the project itself (reuse path), not a
        // different one — must not suffix.
        let p = PathBuf::from("/home/u/dev/shop");
        let ledger = vec![tenant("shop", "/home/u/dev/shop")];
        assert_eq!(derive_from_ledger(&p, &p, &ledger), "shop");
    }

    #[test]
    fn live_ledger_row_is_authoritative_and_persisted() {
        // A live row wins even over a (stale) cached value, and we still
        // signal a persist so the cache gets corrected/backfilled.
        let p = PathBuf::from("/home/u/dev/shop");
        let ledger = vec![tenant("legacy_name", "/home/u/dev/shop")];
        let (name, persist) = resolve_tenant(&p, &p, &ledger, Some("stale_cache"));
        assert_eq!(name, "legacy_name");
        assert!(persist);
    }

    #[test]
    fn cached_name_survives_a_ledger_drift() {
        // The crux of the fix: this project's ledger row is gone (post
        // `down --purge`) and a *different* project now holds the bare
        // basename. Fresh derivation would drift to `shop_<hash>` and
        // strand the credentials baked into env.php; the cache pins the
        // original name and reports no rewrite needed.
        let mine = PathBuf::from("/home/u/a/shop");
        let ledger = vec![tenant("shop", "/home/u/b/shop")];
        assert_ne!(
            derive_from_ledger(&mine, &mine, &ledger),
            "shop",
            "without the cache, derivation drifts"
        );
        let (name, persist) = resolve_tenant(&mine, &mine, &ledger, Some("shop"));
        assert_eq!(name, "shop");
        assert!(!persist, "a cache hit needs no rewrite");
    }

    #[test]
    fn fresh_project_derives_and_persists() {
        let p = PathBuf::from("/home/u/dev/nebula");
        let (name, persist) = resolve_tenant(&p, &p, &[], None);
        assert_eq!(name, "nebula");
        assert!(persist, "a freshly derived name must be cached");
    }
}
