//! Per-running-server filesystem layout.
//!
//! Unix: `$XDG_RUNTIME_DIR/bougie/server/<project-hash>/`. Spec:
//! SERVER.md §7.3. tmpfs-backed on systemd systems, wiped at logout,
//! 0700 by default. Falls back to `/tmp/bougie-server-<uid>` when
//! `XDG_RUNTIME_DIR` is unset.
//!
//! Windows: `%LOCALAPPDATA%\bougie\server\<project-hash>\` — same
//! layout shape; `LocalAppData` is per-user, not roamed, which mirrors
//! the Unix-runtime-dir guarantees we care about (private, machine-
//! local). Falls back to `%TEMP%\bougie\bougie\server\` when LOCALAPPDATA
//! is somehow unset.
//!
//! `pool_socket()` only makes sense on Unix (php-fpm listens on a unix
//! socket); on Windows the per-pool transport is TCP loopback, picked
//! at spawn time and tracked inside [`super::pool::Pool`]. The path
//! helper stays Unix-only.

use eyre::{Result, WrapErr};
use std::path::{Path, PathBuf};

/// Re-exported from `bougie-paths` so the server and the rest of the
/// codebase agree on the per-project hash. Used here for the runtime
/// root layout and orphan pruning.
pub use bougie_paths::project_hash;

/// Per-server-instance directory layout. Constructed once at server
/// startup and threaded through everything that needs to spawn FPM or
/// open a unix socket.
#[derive(Debug, Clone)]
pub struct ServerPaths {
    runtime_root: PathBuf,
}

impl ServerPaths {
    /// Resolve from environment.
    pub fn from_env() -> Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self::from_xdg_runtime_dir(std::env::var_os("XDG_RUNTIME_DIR")))
        }
        #[cfg(windows)]
        {
            Ok(Self::from_local_app_data(std::env::var_os("LOCALAPPDATA")))
        }
    }

    /// Pure resolver for the Unix code path. Exposed for tests so they
    /// can exercise the fallback without mutating process-global state.
    #[cfg(unix)]
    pub fn from_xdg_runtime_dir(xdg: Option<std::ffi::OsString>) -> Self {
        let root = xdg.map_or_else(fallback_root, PathBuf::from);
        Self { runtime_root: root.join("bougie").join("server") }
    }

    /// Pure resolver for the Windows code path. Falls back to `%TEMP%`
    /// when `LOCALAPPDATA` is somehow unset (rare; both are populated
    /// by the standard user profile setup).
    #[cfg(windows)]
    pub fn from_local_app_data(lad: Option<std::ffi::OsString>) -> Self {
        let root = lad
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("TEMP")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(r"C:\Windows\Temp"))
                    .join("bougie")
            });
        Self { runtime_root: root.join("bougie").join("server") }
    }

    /// Construct with an explicit root. For tests.
    pub fn from_root(root: PathBuf) -> Self {
        Self { runtime_root: root }
    }

    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    /// `<runtime_root>/<project-hash>/`. Created on demand by callers.
    pub fn project_dir(&self, project: &Path) -> PathBuf {
        self.runtime_root.join(project_hash(project))
    }

    /// Pool unix socket (Unix only). SERVER.md §7.3:
    /// `$XDG_RUNTIME_DIR/bougie/server/<project-hash>/<variant>.sock`.
    #[cfg(unix)]
    pub fn pool_socket(&self, project: &Path, variant: &str) -> PathBuf {
        self.project_dir(project).join(format!("{variant}.sock"))
    }

    /// Generated php-fpm config file (Unix only).
    #[cfg(unix)]
    pub fn pool_conf(&self, project: &Path, variant: &str) -> PathBuf {
        self.project_dir(project).join(format!("{variant}.conf"))
    }

    /// Variant conf.d directory the pool's `PHP_INI_SCAN_DIR` points at:
    /// `<runtime_root>/<project-hash>/<variant>.confd/`.
    pub fn pool_confd(&self, project: &Path, variant: &str) -> PathBuf {
        self.project_dir(project).join(format!("{variant}.confd"))
    }

    /// Control socket (Unix): `<runtime_root>/control.sock`.
    #[cfg(unix)]
    pub fn control_socket(&self) -> PathBuf {
        self.runtime_root.join("control.sock")
    }

    /// Discovery file that holds the named-pipe name of the running
    /// server (Windows). Stored as plain text — one line, the pipe
    /// name (`\\.\pipe\bougie-server-<hash>`). The control socket itself
    /// is a named pipe, not a filesystem entry, so we need a side
    /// channel for `bougie server list` to find it.
    #[cfg(windows)]
    pub fn control_pipe_discovery(&self) -> PathBuf {
        self.runtime_root.join("control.pipe")
    }

    /// Remove every per-project subdirectory of `runtime_root` whose
    /// 12-hex name doesn't match any of `keep`. Called at server
    /// startup (with `keep` = current `[[host]]` projects) to clear
    /// orphans from prior runs that exited abnormally, and at
    /// shutdown (with `keep` = `&[]`) to wipe everything this server
    /// owned. Non-fatal: a failure to prune is logged but doesn't
    /// abort startup or shutdown.
    ///
    /// Files directly under `runtime_root` (notably `control.sock` on
    /// Unix or `control.pipe` on Windows) are left alone — this only
    /// touches subdirs.
    pub fn prune_project_dirs(&self, keep: &[std::path::PathBuf]) -> Vec<(PathBuf, String)> {
        use std::collections::HashSet;
        let mut errors = Vec::new();
        let entries = match std::fs::read_dir(&self.runtime_root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return errors,
            Err(e) => {
                errors.push((self.runtime_root.clone(), e.to_string()));
                return errors;
            }
        };
        let keep_hashes: HashSet<String> = keep.iter().map(|p| project_hash(p)).collect();
        for entry in entries.flatten() {
            let path = entry.path();
            // Skip files (control.sock and stray detritus).
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // Defensive: only touch dirs that look like a project
            // hash (12 lowercase-hex chars). Anything else might be
            // a future bougie artifact we don't recognise yet.
            if !is_project_hash_name(name) {
                continue;
            }
            if keep_hashes.contains(name) {
                continue;
            }
            if let Err(e) = std::fs::remove_dir_all(&path) {
                errors.push((path, e.to_string()));
            }
        }
        errors
    }
}

fn is_project_hash_name(name: &str) -> bool {
    name.len() == 12 && name.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[cfg(unix)]
fn fallback_root() -> PathBuf {
    let uid = rustix::process::geteuid().as_raw();
    PathBuf::from(format!("/tmp/bougie-server-{uid}"))
}

/// Stringify a path for embedding in CGI/FastCGI params (`SCRIPT_FILENAME`,
/// `DOCUMENT_ROOT`, etc). Windows-specific: strips the `\\?\` extended-
/// length prefix that `Path::canonicalize` returns, because PHP on
/// Windows opens that literal as a verbatim filename rather than
/// resolving it via the Win32 path normalizer — `\\?\C:\…` becomes a
/// "file not found" 404 from php-cgi. On Unix it's just
/// `path.display().to_string()`.
pub fn cgi_path_string(p: &Path) -> String {
    #[cfg(unix)]
    {
        p.display().to_string()
    }
    #[cfg(windows)]
    {
        let s = p.display().to_string();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            // UNC verbatim form: \\?\UNC\server\share\... → \\server\share\...
            format!(r"\\{rest}")
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            rest.to_owned()
        } else {
            s
        }
    }
}

/// Create a directory with the tightest sensible permissions for the
/// platform. Unix: mode 0700 (the runtime dir contains per-project
/// state and leaks paths + ini fragment contents through readdir).
/// Windows: default ACL — `%LOCALAPPDATA%` is already per-user and
/// inherited permissions land on subdirectories.
pub fn create_dir_0700(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .wrap_err_with(|| format!("creating {}", path.display()))?;
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(path)
            .wrap_err_with(|| format!("creating {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn project_hash_is_12_hex_chars() {
        let h = project_hash(Path::new("/tmp"));
        assert_eq!(h.len(), 12);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn project_hash_is_deterministic() {
        let a = project_hash(Path::new("/tmp"));
        let b = project_hash(Path::new("/tmp"));
        assert_eq!(a, b);
    }

    #[test]
    fn project_hash_differs_for_distinct_paths() {
        let a = project_hash(Path::new("/tmp"));
        let b = project_hash(Path::new("/var"));
        assert_ne!(a, b);
    }

    #[test]
    fn project_hash_canonicalizes() {
        // Equivalent paths (via a trailing `.`) produce the same hash
        // once canonicalized.
        let td = TempDir::new().unwrap();
        let a = project_hash(td.path());
        let with_dot = td.path().join(".");
        let b = project_hash(&with_dot);
        assert_eq!(a, b);
    }

    #[cfg(unix)]
    #[test]
    fn paths_layout_matches_spec() {
        let p = ServerPaths::from_root(PathBuf::from("/run/user/1000/bougie/server"));
        let project = Path::new("/some/where");
        let hash = project_hash(project);
        assert_eq!(
            p.project_dir(project),
            PathBuf::from(format!("/run/user/1000/bougie/server/{hash}"))
        );
        assert_eq!(
            p.pool_socket(project, "normal"),
            PathBuf::from(format!("/run/user/1000/bougie/server/{hash}/normal.sock"))
        );
        assert_eq!(
            p.pool_conf(project, "normal"),
            PathBuf::from(format!("/run/user/1000/bougie/server/{hash}/normal.conf"))
        );
        assert_eq!(
            p.pool_confd(project, "normal"),
            PathBuf::from(format!("/run/user/1000/bougie/server/{hash}/normal.confd"))
        );
        assert_eq!(
            p.control_socket(),
            PathBuf::from("/run/user/1000/bougie/server/control.sock")
        );
    }

    #[test]
    fn prune_removes_stale_subdirs_and_keeps_known_ones() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("bougie/server");
        std::fs::create_dir_all(&root).unwrap();
        let sp = ServerPaths::from_root(root.clone());

        let project = td.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let live_hash = project_hash(&project);
        std::fs::create_dir_all(root.join(&live_hash)).unwrap();
        std::fs::create_dir_all(root.join("aaaaaaaaaaaa")).unwrap();
        std::fs::create_dir_all(root.join("bbbbbbbbbbbb")).unwrap();
        // Control socket file lives directly under root — must survive.
        std::fs::write(root.join("control.sock"), b"").unwrap();
        // Non-hash-shaped dir — should be left alone (future bougie
        // artifact we don't recognise yet).
        std::fs::create_dir_all(root.join("not-a-hash")).unwrap();

        let errs = sp.prune_project_dirs(&[project.clone()]);
        assert!(errs.is_empty(), "{errs:?}");

        assert!(root.join(&live_hash).exists());
        assert!(!root.join("aaaaaaaaaaaa").exists());
        assert!(!root.join("bbbbbbbbbbbb").exists());
        assert!(root.join("control.sock").exists());
        assert!(root.join("not-a-hash").exists());
    }

    #[test]
    fn prune_with_empty_keep_wipes_all_hash_subdirs() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("bougie/server");
        std::fs::create_dir_all(&root).unwrap();
        let sp = ServerPaths::from_root(root.clone());
        std::fs::create_dir_all(root.join("aaaaaaaaaaaa")).unwrap();
        std::fs::create_dir_all(root.join("bbbbbbbbbbbb")).unwrap();
        std::fs::write(root.join("control.sock"), b"").unwrap();

        let errs = sp.prune_project_dirs(&[]);
        assert!(errs.is_empty(), "{errs:?}");

        assert!(!root.join("aaaaaaaaaaaa").exists());
        assert!(!root.join("bbbbbbbbbbbb").exists());
        assert!(root.join("control.sock").exists());
    }

    #[test]
    fn prune_with_missing_runtime_root_is_noop() {
        let sp = ServerPaths::from_root(PathBuf::from("/nonexistent-bougie-test-path"));
        let errs = sp.prune_project_dirs(&[]);
        assert!(errs.is_empty());
    }

    #[test]
    fn is_project_hash_name_accepts_12_lower_hex() {
        assert!(is_project_hash_name("0123456789ab"));
        assert!(is_project_hash_name("aaaaaaaaaaaa"));
        assert!(!is_project_hash_name("0123456789AB")); // uppercase rejected
        assert!(!is_project_hash_name("0123456789a")); // too short
        assert!(!is_project_hash_name("0123456789abc")); // too long
        assert!(!is_project_hash_name("ghijklmnopqr")); // non-hex
        assert!(!is_project_hash_name("control.sock"));
    }

    #[cfg(unix)]
    #[test]
    fn xdg_runtime_dir_is_honored() {
        let p = ServerPaths::from_xdg_runtime_dir(Some(
            std::ffi::OsString::from("/tmp/xdg-fixture"),
        ));
        assert_eq!(p.runtime_root(), Path::new("/tmp/xdg-fixture/bougie/server"));
    }

    #[cfg(unix)]
    #[test]
    fn missing_xdg_runtime_dir_falls_back_to_tmp() {
        let p = ServerPaths::from_xdg_runtime_dir(None);
        let s = p.runtime_root().to_string_lossy();
        assert!(s.starts_with("/tmp/bougie-server-"));
        assert!(s.ends_with("/bougie/server"));
    }

    #[cfg(windows)]
    #[test]
    fn local_app_data_is_honored() {
        let p = ServerPaths::from_local_app_data(Some(
            std::ffi::OsString::from(r"C:\Users\test\AppData\Local"),
        ));
        assert_eq!(
            p.runtime_root(),
            Path::new(r"C:\Users\test\AppData\Local\bougie\server")
        );
    }

    #[cfg(windows)]
    #[test]
    fn missing_local_app_data_falls_back_to_temp() {
        let p = ServerPaths::from_local_app_data(None);
        let s = p.runtime_root().to_string_lossy();
        // Either honors %TEMP% or the hard-coded fallback; both end in
        // \bougie\server.
        assert!(s.ends_with(r"bougie\server"));
    }

    #[cfg(windows)]
    #[test]
    fn cgi_path_string_strips_verbatim_prefix() {
        let p = Path::new(r"\\?\C:\Users\jelle\proj\index.php");
        assert_eq!(cgi_path_string(p), r"C:\Users\jelle\proj\index.php");
    }

    #[cfg(windows)]
    #[test]
    fn cgi_path_string_strips_verbatim_unc_prefix() {
        let p = Path::new(r"\\?\UNC\server\share\index.php");
        assert_eq!(cgi_path_string(p), r"\\server\share\index.php");
    }

    #[cfg(windows)]
    #[test]
    fn cgi_path_string_leaves_non_verbatim_alone() {
        let p = Path::new(r"C:\Users\jelle\proj\index.php");
        assert_eq!(cgi_path_string(p), r"C:\Users\jelle\proj\index.php");
    }

    #[cfg(unix)]
    #[test]
    fn cgi_path_string_unix_is_display() {
        let p = Path::new("/srv/app/index.php");
        assert_eq!(cgi_path_string(p), "/srv/app/index.php");
    }
}
