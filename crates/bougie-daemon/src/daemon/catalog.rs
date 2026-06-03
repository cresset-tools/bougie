//! Built-in service catalog. See SERVICES.md §2.
//!
//! Phase 2 ships the data layer: entries, lookup, and shape needed by
//! `bougie services {add,remove,list,catalog}`. The full
//! exec/sandbox/provisioner machinery lands in Phase 3 (redis) and
//! later phases (mariadb, opensearch, rabbitmq, bougie server).

use serde::Serialize;

/// A single service the supervisor knows how to manage.
///
/// All fields are `'static` so the catalog can live in `const CATALOG`
/// at compile time. Per-instance state (tenants, runtime config) lives
/// elsewhere — this is just the data the daemon needs to know which
/// tarball to fetch and how to bring the service up.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    /// Public service identifier. Stable across versions; never renamed.
    pub name: &'static str,
    /// Default upstream version when the project pin is `"*"` or absent.
    pub version: &'static str,
    /// Tarball identifier in the bougie index. Pinned per release.
    pub tarball: &'static str,
    /// Path to the main binary inside the extracted tarball
    /// (e.g. `bin/redis-server`, `sbin/rabbitmq-server`).
    pub binary: &'static str,
    /// Default binding — Unix socket where possible, TCP port only when
    /// the service can't speak a socket.
    pub binding: Binding,
    /// Tenancy provisioner used by Phase 3+. `None` means the entry
    /// has no user-facing tenants (used by runtime-only deps).
    pub tenancy: Tenancy,
    /// Other catalog entries this service requires at runtime
    /// (hard dep: failed dep cascades to Failed).
    pub requires: &'static [&'static str],
    /// Other catalog entries this service prefers to start after
    /// (soft dep: ordering hint only).
    pub after: &'static [&'static str],
    /// Tarballs that must be present in the store for this service to
    /// run, but which are NOT themselves supervised processes (e.g.
    /// `jdk` for opensearch, `erlang` for rabbitmq).
    pub runtime_deps: &'static [&'static str],
    /// Whether `bougie services add <name>` accepts this entry.
    /// `false` for runtime-only deps that ride along transitively.
    pub user_facing: bool,
    /// One-line summary used by `bougie services catalog` (text form).
    pub summary: &'static str,
    /// Sandbox stance. Default `Strict`: full Landlock/SBPL allowlist
    /// confinement. `LightHome` is the loose mode used by services
    /// that need normal user-level FS access (the bougie-server
    /// catalog entry spawns php-fpm, reads project files, writes
    /// `$XDG_RUNTIME_DIR`); it still blocks sensitive home subdirs
    /// (`~/.ssh`, `~/.aws`, `~/.gnupg`) on macOS via SBPL deny rules.
    /// On Linux it's effectively unsandboxed because Landlock is
    /// allow-list-only and can't compose a permissive baseline with
    /// fine-grained denies.
    pub sandbox: SandboxKind,
}

/// Sandbox stance for a catalog entry. See `CatalogEntry::sandbox`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    /// `ProtectSystem::Strict` + `ProtectHome::Yes` + explicit RW
    /// allowlist. Default for every tarball-shipped service.
    Strict,
    /// Permissive base + macOS-only deny rules for `~/.ssh` etc.
    /// Linux currently runs unsandboxed under this mode (Landlock
    /// limitation); revisit if/when we get LSM or eBPF hooks.
    LightHome,
}

/// How a service exposes itself to PHP clients. Unix socket is the
/// default — TCP only when the service can't speak a socket.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Binding {
    /// `\$BOUGIE_HOME/state/services/<name>/run/<sockname>`. The
    /// `sockname` is fixed per entry (e.g. `mariadb.sock`,
    /// `redis.sock`).
    UnixSocket { sockname: &'static str },
    /// 127.0.0.1:<port>. Never binds on a non-loopback address in v1.
    Tcp { port: u16 },
    /// Used by runtime-only deps that aren't reachable as services.
    None,
}

/// Tenancy strategy used by the per-service provisioner. Phase 2 just
/// names them; Phase 3+ wires the actual provisioner functions.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Tenancy {
    /// `CREATE DATABASE <t>; CREATE USER <t>@localhost …`.
    Mariadb,
    /// Allocate logical DB number 0..15.
    Redis,
    /// Create index template `<t>-*`.
    Opensearch,
    /// `rabbitmqctl add_vhost <t>; add_user <t> <pw> …`.
    Rabbitmq,
    /// Add `[[host]]` to server.toml; reload via control socket.
    BougieServer,
    /// Runtime-only deps (jdk, erlang) have no tenants.
    None,
}

// -------------------- v1 entries --------------------

/// Built-in v1 catalog. Order is irrelevant; lookup is by name.
///
/// Versions match the tarballs in `~/php-build-standalone/tools/`
/// (see commit log of that repo for release history). Bumped when the
/// index ships a newer tarball.
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        name: "redis",
        version: "8.6.3",
        tarball: "redis-8.6.3",
        binary: "bin/redis-server",
        binding: Binding::UnixSocket { sockname: "redis.sock" },
        tenancy: Tenancy::Redis,
        requires: &[],
        after: &[],
        runtime_deps: &[],
        user_facing: true,
        summary: "Redis in-memory data store; one tenant per logical DB (0..15).",
        sandbox: SandboxKind::Strict,
    },
    CatalogEntry {
        name: "mariadb",
        // 11.4.4 matches the tag published by the bougie index today;
        // bump when the index ships a newer 11.4.x.
        version: "11.4.4",
        tarball: "mariadb-11.4.4",
        binary: "bin/mariadbd",
        binding: Binding::UnixSocket { sockname: "mariadb.sock" },
        tenancy: Tenancy::Mariadb,
        requires: &[],
        after: &[],
        runtime_deps: &[],
        user_facing: true,
        summary: "MariaDB 11.4 LTS; one database + user per project tenant.",
        sandbox: SandboxKind::Strict,
    },
    CatalogEntry {
        name: "opensearch",
        version: "2.19.5",
        tarball: "opensearch-2.19.5",
        binary: "bin/opensearch",
        binding: Binding::Tcp { port: 9200 },
        tenancy: Tenancy::Opensearch,
        requires: &[],
        after: &[],
        // OpenSearch's tarball was unbundled upstream (php-build-standalone
        // UNBUNDLE_PLAN phases 0–2): instead of shipping a Temurin JDK
        // inside the tarball, its manifest now declares a `requires_tools[]`
        // entry for `jdk`, which the client resolves from the shared store
        // and links back to `install/jdk/` (where `bin/opensearch-env`
        // autoresolves `OPENSEARCH_HOME/jdk/bin/java`). Mirror that here so
        // the store-fetch drift audit stays quiet and the supervisor's
        // availability checks see the dep — same shape as rabbitmq→erlang.
        runtime_deps: &["jdk"],
        user_facing: true,
        summary: "OpenSearch 2.x search engine; per-tenant index template.",
        sandbox: SandboxKind::Strict,
    },
    CatalogEntry {
        name: "rabbitmq",
        version: "4.2.6",
        tarball: "rabbitmq-4.2.6",
        binary: "sbin/rabbitmq-server",
        binding: Binding::Tcp { port: 5672 },
        tenancy: Tenancy::Rabbitmq,
        requires: &[],
        after: &[],
        runtime_deps: &["erlang"],
        user_facing: true,
        summary: "RabbitMQ 4.x AMQP broker; per-tenant vhost + user.",
        sandbox: SandboxKind::Strict,
    },
    CatalogEntry {
        name: "server",
        // Bougie's own version isn't pinned here — `binary` resolves
        // to the running bougie executable via `current_exe()` at
        // spawn time. The version string surfaces in `bougie services
        // catalog` output for completeness.
        version: env!("CARGO_PKG_VERSION"),
        tarball: "",
        binary: "bougie",
        binding: Binding::Tcp { port: 7080 },
        tenancy: Tenancy::BougieServer,
        requires: &[],
        after: &[],
        runtime_deps: &[],
        user_facing: true,
        summary: "Bougie dev HTTP server with per-request xdebug routing.",
        // Server reads project files, spawns php-fpm masters, writes
        // `$XDG_RUNTIME_DIR/bougie/server/<project-hash>/` — the
        // things ProtectSystem::Strict actively blocks. Run it under
        // the loose stance with sensitive home subdirs denied
        // (macOS-only effect today; see `SandboxKind` docs).
        sandbox: SandboxKind::LightHome,
    },
    // ---------- Runtime-only deps ----------
    CatalogEntry {
        name: "jdk",
        version: "21.0.11+10",
        tarball: "jdk-21.0.11+10",
        binary: "bin/java",
        binding: Binding::None,
        tenancy: Tenancy::None,
        requires: &[],
        after: &[],
        runtime_deps: &[],
        user_facing: false,
        summary: "Eclipse Temurin JDK 21 (runtime dep of opensearch).",
        sandbox: SandboxKind::Strict,
    },
    CatalogEntry {
        name: "erlang",
        version: "27.3.4.11",
        tarball: "erlang-27.3.4.11",
        binary: "bin/erl",
        binding: Binding::None,
        tenancy: Tenancy::None,
        requires: &[],
        after: &[],
        runtime_deps: &[],
        user_facing: false,
        summary: "Erlang/OTP 27 runtime (runtime dep of rabbitmq).",
        sandbox: SandboxKind::Strict,
    },
];

// -------------------- lookup --------------------

/// Look up an entry by name. `None` if unknown.
pub fn find(name: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.name == name)
}

/// Subset of the catalog `bougie services add` will accept. Excludes
/// runtime-only deps.
pub fn user_facing() -> impl Iterator<Item = &'static CatalogEntry> {
    CATALOG.iter().filter(|e| e.user_facing)
}

/// Comma-separated list of user-facing names, for error messages.
pub fn user_facing_names() -> String {
    user_facing()
        .map(|e| e.name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_has_unique_name() {
        let mut names: Vec<_> = CATALOG.iter().map(|e| e.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate catalog names");
    }

    #[test]
    fn find_returns_known_entries() {
        assert!(find("redis").is_some());
        assert!(find("mariadb").is_some());
        assert!(find("opensearch").is_some());
        assert!(find("rabbitmq").is_some());
        assert!(find("server").is_some());
    }

    #[test]
    fn find_returns_none_for_unknown() {
        assert!(find("postgres").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn jdk_and_erlang_are_runtime_only() {
        assert!(!find("jdk").unwrap().user_facing);
        assert!(!find("erlang").unwrap().user_facing);
    }

    #[test]
    fn user_facing_excludes_runtime_deps() {
        let names: Vec<_> = user_facing().map(|e| e.name).collect();
        assert!(names.contains(&"redis"));
        assert!(names.contains(&"server"));
        assert!(!names.contains(&"jdk"));
        assert!(!names.contains(&"erlang"));
    }

    #[test]
    fn runtime_deps_reference_known_entries() {
        // Catch typos in runtime_deps lists at test time.
        for entry in CATALOG {
            for dep in entry.runtime_deps {
                assert!(
                    find(dep).is_some(),
                    "{}.runtime_deps references unknown {dep}",
                    entry.name
                );
            }
            for req in entry.requires {
                assert!(
                    find(req).is_some(),
                    "{}.requires references unknown {req}",
                    entry.name
                );
            }
            for aft in entry.after {
                assert!(
                    find(aft).is_some(),
                    "{}.after references unknown {aft}",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn opensearch_runtime_deps_include_jdk() {
        // OpenSearch was unbundled upstream: its manifest declares a
        // `requires_tools[]` entry for the Temurin JDK (linked back to
        // install/jdk/), so the catalog must mirror it under
        // runtime_deps — same as rabbitmq→erlang. Keeping this in sync
        // is what silences `store_fetch::audit_catalog_drift`.
        let os = find("opensearch").unwrap();
        assert!(os.runtime_deps.contains(&"jdk"), "{:?}", os.runtime_deps);
    }

    #[test]
    fn rabbitmq_runtime_deps_include_erlang() {
        let rmq = find("rabbitmq").unwrap();
        assert!(rmq.runtime_deps.contains(&"erlang"));
    }

    #[test]
    fn unix_socket_services_use_socket_binding() {
        for name in ["redis", "mariadb"] {
            let e = find(name).unwrap();
            assert!(
                matches!(e.binding, Binding::UnixSocket { .. }),
                "{name} should bind a unix socket"
            );
        }
    }

    #[test]
    fn tcp_services_use_loopback_port() {
        for name in ["opensearch", "rabbitmq", "server"] {
            let e = find(name).unwrap();
            assert!(
                matches!(e.binding, Binding::Tcp { .. }),
                "{name} should bind a TCP port"
            );
        }
    }
}
