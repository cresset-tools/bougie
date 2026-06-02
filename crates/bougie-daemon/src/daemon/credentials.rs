//! Deterministic per-tenant credentials.
//!
//! Service passwords are *derived*, not randomized, so they survive a
//! `down` / `--purge` / re-provision cycle: the same project always
//! resolves to the same password. That keeps app config that captured
//! the password at install time — notably Magento's `app/etc/env.php` —
//! valid after the database is cleaned up and re-provisioned, instead of
//! drifting into `ACCESS DENIED`.
//!
//! `password = sha256(machine_secret ‖ service ‖ project)`. The
//! `machine_secret` is 32 random bytes generated once and stored `0600`
//! under `state/`; it's what makes the derived password unpredictable to
//! anything that doesn't already have local filesystem access (and a
//! bougie database is loopback-only regardless). Keyed by the canonical
//! project path, mirroring the project-keyed tenant model, so distinct
//! projects get distinct passwords.

use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Filename of the machine-local credential-derivation key under `state/`.
const KEY_FILE: &str = "credentials.key";
/// Key length in bytes.
const KEY_LEN: usize = 32;

/// Read (or create-once) the machine-local secret used to derive
/// credentials. 32 random bytes, mode `0600`.
fn machine_secret(paths: &Paths) -> Result<Vec<u8>> {
    let state = paths.state();
    let path = state.join(KEY_FILE);

    // Fast path: an existing, well-formed key.
    if let Ok(bytes) = std::fs::read(&path)
        && bytes.len() >= KEY_LEN
    {
        return Ok(bytes);
    }

    std::fs::create_dir_all(&state)
        .wrap_err_with(|| format!("creating {}", state.display()))?;

    let mut key = [0u8; KEY_LEN];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut key))
        .wrap_err("reading /dev/urandom for the credential key")?;

    // Atomic create-if-absent (`O_EXCL`). If a concurrent provisioner
    // won the race, read its key instead so every derivation agrees.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut f) => {
            f.write_all(&key)
                .wrap_err_with(|| format!("writing {}", path.display()))?;
            Ok(key.to_vec())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::read(&path).wrap_err_with(|| format!("reading {}", path.display()))
        }
        Err(e) => Err(e).wrap_err_with(|| format!("creating {}", path.display())),
    }
}

/// Derive the per-tenant password for `service` + `project`. Stable
/// across re-provisioning. 48 lowercase-hex chars, matching the previous
/// random format.
pub fn derive_password(paths: &Paths, service: &str, project: &Path) -> Result<String> {
    let secret = machine_secret(paths)?;
    // Canonicalize so `/p`, `/p/`, and symlinked spellings of the same
    // project derive the same password.
    let canon = project.canonicalize().unwrap_or_else(|_| project.to_path_buf());

    let mut h = Sha256::new();
    h.update(&secret);
    h.update(b"\0");
    h.update(service.as_bytes());
    h.update(b":");
    h.update(canon.as_os_str().as_encoded_bytes());
    let digest = h.finalize();

    // 24 bytes → 48 hex chars (same length as the old generator).
    Ok(hex_encode(&digest[..24]))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths_in(dir: &TempDir) -> Paths {
        Paths::new(dir.path().to_path_buf(), dir.path().join("cache"))
    }

    #[test]
    fn derivation_is_stable_for_same_inputs() {
        let td = TempDir::new().unwrap();
        let paths = paths_in(&td);
        let proj = td.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();

        let a = derive_password(&paths, "mariadb", &proj).unwrap();
        let b = derive_password(&paths, "mariadb", &proj).unwrap();
        assert_eq!(a, b, "same machine + service + project must derive the same password");
        assert_eq!(a.len(), 48);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn derivation_survives_key_already_present() {
        // Second call goes through the fast-path read of the key file.
        let td = TempDir::new().unwrap();
        let paths = paths_in(&td);
        let proj = td.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let first = derive_password(&paths, "mariadb", &proj).unwrap();
        assert!(paths.state().join(KEY_FILE).exists());
        let second = derive_password(&paths, "mariadb", &proj).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn distinct_projects_and_services_differ() {
        let td = TempDir::new().unwrap();
        let paths = paths_in(&td);
        let p1 = td.path().join("p1");
        let p2 = td.path().join("p2");
        std::fs::create_dir_all(&p1).unwrap();
        std::fs::create_dir_all(&p2).unwrap();

        let m1 = derive_password(&paths, "mariadb", &p1).unwrap();
        let m2 = derive_password(&paths, "mariadb", &p2).unwrap();
        let r1 = derive_password(&paths, "rabbitmq", &p1).unwrap();
        assert_ne!(m1, m2, "different projects → different passwords");
        assert_ne!(m1, r1, "different services → different passwords");
    }

    #[test]
    fn key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let paths = paths_in(&td);
        let proj = td.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let _ = derive_password(&paths, "mariadb", &proj).unwrap();
        let mode = std::fs::metadata(paths.state().join(KEY_FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credential key must be 0600");
    }
}
