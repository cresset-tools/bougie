//! Built-in service catalog. See SERVICES.md §2.
//!
//! Phase 2 ships the data layer: entries, lookup, and shape needed by
//! `bougie service {add,remove,list,catalog}`. The full
//! exec/sandbox/provisioner machinery lands in Phase 3 (redis) and
//! later phases (mariadb, opensearch, rabbitmq, bougie server).

use serde::Serialize;

/// Mailpit's SMTP listener port. This is the catalog `binding` — the
/// endpoint PHP apps connect to to send mail, so it's the port the
/// supervisor health-probes (mirrors rabbitmq exposing its AMQP port,
/// not its management UI). Matches Mailpit's upstream default so the
/// familiar `127.0.0.1:1025` keeps working.
pub const MAILPIT_SMTP_PORT: u16 = 1025;

/// Mailpit's HTTP web-UI / REST-API port. The single-endpoint
/// [`Binding`] can't model a second port, so this rides alongside:
/// hard-coded into the supervisor's exec args and surfaced to apps as
/// `BOUGIE_SERVICE_MAILPIT_DASHBOARD_URL` (cf. rabbitmq's unmonitored
/// management UI on 15672). Matches Mailpit's upstream default.
pub const MAILPIT_HTTP_PORT: u16 = 8025;

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
    /// Every version bougie knows how to run for this service, newest
    /// first by convention. `version` (the default) must appear here.
    /// Single-element for single-version services; `mysql` carries both
    /// 8.4 and 8.0 so two projects can pin different majors and run them
    /// side by side. The pin resolver intersects a project's partial or
    /// range pin against this compiled-in set; the index stays
    /// authoritative at fetch time (an exact pin naming a version the
    /// index ships but this list omits is passed through untouched).
    pub versions: &'static [&'static str],
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
    /// Whether `bougie service add <name>` accepts this entry.
    /// `false` for runtime-only deps that ride along transitively.
    pub user_facing: bool,
    /// One-line summary used by `bougie service catalog` (text form).
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
    /// Client-side companion tools shipped in the same tarball
    /// (mariadb/mysqldump, redis-cli, rabbitmqctl, …). Exposed to
    /// users as `vendor/bougie/bin/` shims and via
    /// `bougie service exec`; the CLI wires each invocation to the
    /// project's tenant. Names must be unique across the whole
    /// catalog — the shim dispatches on argv[0] basename alone.
    pub clients: &'static [ClientTool],
}

/// One user-facing client binary inside a service tarball.
///
/// `name` is what users type (and the shim link name); `path` is the
/// binary's location relative to the tarball root. Several names may
/// map to the same path (`mysqldump` and `mariadb-dump` both resolve
/// to `bin/mariadb-dump` — the tarball's own `mysql*` compat symlinks
/// aren't guaranteed to survive repacking, so aliases point at the
/// canonical binary).
#[derive(Debug, Clone, Serialize)]
pub struct ClientTool {
    /// Basename users invoke.
    pub name: &'static str,
    /// Path inside the extracted tarball (e.g. `bin/mariadb`).
    pub path: &'static str,
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
    /// Same database+user+GRANT tenant model as [`Tenancy::Mariadb`],
    /// but bootstrapped with `mysqld --initialize-insecure` (`MySQL` has
    /// no `mariadb-install-db`) and provisioned by connecting as the
    /// passwordless `root@localhost` the insecure init leaves behind.
    /// Users get the `MySQL` default `caching_sha2_password` plugin, which
    /// works unchanged on both 8.0 and 8.4 (unlike `mysql_native_password`,
    /// which 8.4 disables by default).
    Mysql,
    /// Allocate logical DB number 0..15.
    Redis,
    /// Create index template `<t>-*`.
    Opensearch,
    /// `rabbitmqctl add_vhost <t>; add_user <t> <pw> …`.
    Rabbitmq,
    /// Shared global mail sink — no real per-project isolation.
    /// Records a bare ledger row so `bougie projects list` and the
    /// `service.env` injection see the tenant; every project shares the
    /// one Mailpit instance (its `--tenant-id` is a single-value
    /// startup flag, not a per-connection selector, so multi-tenant
    /// isolation isn't possible against one shared instance).
    Mailpit,
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
        versions: &["8.6.3"],
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
        clients: &[ClientTool { name: "redis-cli", path: "bin/redis-cli" }],
    },
    CatalogEntry {
        name: "mariadb",
        // 11.4.4 matches the tag published by the bougie index today;
        // bump when the index ships a newer 11.4.x.
        version: "11.4.4",
        versions: &["11.4.4"],
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
        // Aliases target the mariadb-native binaries directly: the
        // tarball's own `mysql* → mariadb-*` compat symlinks aren't
        // guaranteed to survive repacking. The curated set is the
        // day-to-day trio; everything else in `bin/` stays reachable
        // via `bougie service exec`.
        clients: &[
            ClientTool { name: "mariadb", path: "bin/mariadb" },
            ClientTool { name: "mysql", path: "bin/mariadb" },
            ClientTool { name: "mariadb-dump", path: "bin/mariadb-dump" },
            ClientTool { name: "mysqldump", path: "bin/mariadb-dump" },
            ClientTool { name: "mariadb-admin", path: "bin/mariadb-admin" },
            ClientTool { name: "mysqladmin", path: "bin/mariadb-admin" },
        ],
    },
    CatalogEntry {
        name: "mysql",
        // 8.4 LTS is the default; 8.0 rides alongside so a project stuck
        // on the older series can pin `mysql = "8.0"` and run it beside a
        // sibling project's 8.4. Both are published by the bougie index
        // (`tool/mysql`); bump the patch levels when it ships newer.
        version: "8.4.10",
        versions: &["8.4.10", "8.0.46"],
        tarball: "mysql-8.4.10",
        binary: "bin/mysqld",
        binding: Binding::UnixSocket { sockname: "mysql.sock" },
        tenancy: Tenancy::Mysql,
        requires: &[],
        after: &[],
        runtime_deps: &[],
        user_facing: true,
        summary: "MySQL (8.4 LTS / 8.0); one database + user per project tenant.",
        sandbox: SandboxKind::Strict,
        // No curated client shims: mariadb already owns the catalog-wide
        // `mysql`/`mysqldump`/`mysqladmin` names (the argv[0] shim
        // dispatches on basename alone, so a name can back only one
        // service). MySQL's own `bin/mysql` client stays reachable via
        // `bougie service exec mysql -- mysql …`. Per-project client
        // resolution is a follow-up (INSTANCES_PLAN, client-collision).
        clients: &[],
    },
    CatalogEntry {
        name: "opensearch",
        version: "2.19.5",
        versions: &["2.19.5"],
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
        // No curated clients: opensearch has no interactive CLI; the
        // maintenance tools (opensearch-plugin, …) stay reachable via
        // `bougie service exec`.
        clients: &[],
    },
    CatalogEntry {
        name: "rabbitmq",
        version: "4.2.6",
        versions: &["4.2.6"],
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
        clients: &[
            ClientTool { name: "rabbitmqctl", path: "sbin/rabbitmqctl" },
            ClientTool { name: "rabbitmq-diagnostics", path: "sbin/rabbitmq-diagnostics" },
            ClientTool { name: "rabbitmq-plugins", path: "sbin/rabbitmq-plugins" },
        ],
    },
    CatalogEntry {
        name: "mailpit",
        version: "1.30.2",
        versions: &["1.30.2"],
        tarball: "mailpit-1.30.2",
        // Single static Go binary — the index lays it out at
        // `install/bin/mailpit`, same `bin/` convention as redis.
        binary: "bin/mailpit",
        // SMTP is the service endpoint apps connect to; the web UI on
        // MAILPIT_HTTP_PORT rides alongside (see the port consts).
        binding: Binding::Tcp { port: MAILPIT_SMTP_PORT },
        tenancy: Tenancy::Mailpit,
        requires: &[],
        after: &[],
        runtime_deps: &[],
        user_facing: true,
        summary: "Mailpit SMTP test server; shared mail sink with a web UI on :8025.",
        sandbox: SandboxKind::Strict,
        clients: &[],
    },
    CatalogEntry {
        name: "server",
        // Bougie's own version isn't pinned here — `binary` resolves
        // to the running bougie executable via `current_exe()` at
        // spawn time. The version string surfaces in `bougie service
        // catalog` output for completeness.
        version: env!("CARGO_PKG_VERSION"),
        versions: &[env!("CARGO_PKG_VERSION")],
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
        clients: &[],
    },
    // ---------- Runtime-only deps ----------
    CatalogEntry {
        name: "jdk",
        version: "21.0.11+10",
        versions: &["21.0.11+10"],
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
        clients: &[],
    },
    CatalogEntry {
        name: "erlang",
        version: "27.3.4.11",
        versions: &["27.3.4.11"],
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
        clients: &[],
    },
];

// -------------------- lookup --------------------

/// Look up an entry by name. `None` if unknown.
pub fn find(name: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.name == name)
}

/// The catalog default version for a service name, or `""` if unknown.
///
/// The single-instance stand-in used while the runtime state tree is
/// version-keyed but the request/IPC layer doesn't yet carry a resolved
/// version (Phase 1a). Once instances are threaded end-to-end the
/// resolved version comes from the caller and this is only a fallback
/// for name-only paths (offline consumers before the ledger-scan lands).
#[must_use]
pub fn default_version(name: &str) -> &'static str {
    find(name).map_or("", |e| e.version)
}

/// Look up a client tool by its user-facing name across the whole
/// catalog. `None` if no service ships a client under that name.
/// Client names are unique catalog-wide (asserted in tests) — the
/// argv[0] shim dispatches on the bare basename.
pub fn find_client(name: &str) -> Option<(&'static CatalogEntry, &'static ClientTool)> {
    CATALOG.iter().find_map(|e| {
        e.clients
            .iter()
            .find(|c| c.name == name)
            .map(|c| (e, c))
    })
}

/// Subset of the catalog `bougie service add` will accept. Excludes
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
        assert!(find("mysql").is_some());
        assert!(find("opensearch").is_some());
        assert!(find("rabbitmq").is_some());
        assert!(find("mailpit").is_some());
        assert!(find("server").is_some());
    }

    #[test]
    fn every_entry_lists_its_default_version() {
        // The pin resolver treats `versions` as the known set and
        // `version` as the fallback; the default must be resolvable
        // from the set, and no entry may ship an empty list.
        for e in CATALOG {
            assert!(!e.versions.is_empty(), "{} has no versions", e.name);
            assert!(
                e.versions.contains(&e.version),
                "{}: default `{}` not in versions {:?}",
                e.name,
                e.version,
                e.versions
            );
        }
    }

    #[test]
    fn mysql_ships_two_majors_with_its_own_tenancy() {
        let m = find("mysql").unwrap();
        assert!(m.user_facing);
        assert!(matches!(m.tenancy, Tenancy::Mysql));
        // Both published majors present; 8.4 is the default.
        assert_eq!(m.version, "8.4.10");
        assert!(m.versions.contains(&"8.4.10"));
        assert!(m.versions.contains(&"8.0.46"));
        // Unix socket, like mariadb — coexists by version-keyed socket
        // path, not a port.
        assert!(matches!(m.binding, Binding::UnixSocket { .. }));
        // No curated clients (mariadb owns the `mysql*` shim names).
        assert!(m.clients.is_empty());
    }

    #[test]
    fn mailpit_is_a_user_facing_tcp_service() {
        let mp = find("mailpit").unwrap();
        assert!(mp.user_facing);
        assert!(matches!(mp.tenancy, Tenancy::Mailpit));
        // The catalog binding tracks the SMTP port — the one the
        // supervisor health-probes and apps connect to.
        assert!(
            matches!(mp.binding, Binding::Tcp { port } if port == MAILPIT_SMTP_PORT),
            "{:?}",
            mp.binding
        );
        // Mailpit ships as a single static binary, no runtime deps.
        assert!(mp.runtime_deps.is_empty());
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
    fn client_names_are_unique_catalog_wide() {
        // The argv[0] shim dispatches on the bare basename, so two
        // services shipping a client under the same name would be
        // ambiguous.
        let mut names: Vec<_> = CATALOG
            .iter()
            .flat_map(|e| e.clients.iter().map(|c| c.name))
            .collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate client tool names");
    }

    #[test]
    fn client_names_do_not_shadow_shim_roles() {
        // vendor/bougie/bin/ already carries the php/composer/unzip
        // shims; a client under one of those names would fight the
        // existing argv[0] roles.
        for reserved in ["php", "php-fpm", "composer", "unzip", "bougied", "bougie-babysit"] {
            assert!(
                find_client(reserved).is_none(),
                "client name `{reserved}` shadows a shim role"
            );
        }
    }

    #[test]
    fn find_client_resolves_curated_tools() {
        let (svc, tool) = find_client("mysqldump").unwrap();
        assert_eq!(svc.name, "mariadb");
        assert_eq!(tool.path, "bin/mariadb-dump");

        let (svc, tool) = find_client("redis-cli").unwrap();
        assert_eq!(svc.name, "redis");
        assert_eq!(tool.path, "bin/redis-cli");

        let (svc, _) = find_client("rabbitmqctl").unwrap();
        assert_eq!(svc.name, "rabbitmq");

        assert!(find_client("psql").is_none());
        assert!(find_client("").is_none());
    }

    #[test]
    fn mariadb_aliases_share_canonical_paths() {
        // `mysql`/`mysqldump`/`mysqladmin` are aliases of the
        // mariadb-native binaries, not separate tarball paths.
        for (alias, canonical) in [
            ("mysql", "mariadb"),
            ("mysqldump", "mariadb-dump"),
            ("mysqladmin", "mariadb-admin"),
        ] {
            let (_, a) = find_client(alias).unwrap();
            let (_, c) = find_client(canonical).unwrap();
            assert_eq!(a.path, c.path, "{alias} should target {canonical}'s path");
        }
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
        for name in ["opensearch", "rabbitmq", "mailpit", "server"] {
            let e = find(name).unwrap();
            assert!(
                matches!(e.binding, Binding::Tcp { .. }),
                "{name} should bind a TCP port"
            );
        }
    }
}
