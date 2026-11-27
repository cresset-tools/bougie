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

    std::fs::create_dir_all(&state).wrap_err_with(|| format!("creating {}", state.display()))?;

    let mut key = [0u8; KEY_LEN];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut key))
        .wrap_err("reading /dev/urandom for the credential key")?;

    // Create-if-absent that only ever publishes a *complete* key: write
    // and fsync a private temp file, then hard-link it to the real name.
    // link(2) fails with EEXIST if the key already exists, so exactly one
    // racer wins and every other one reads the winner's key. Creating the
    // real name with O_EXCL and writing afterwards (the old approach) let
    // a racer read the empty file in between, and a crash in between left
    // an empty key behind for good — every password then derived from an
    // empty, guessable secret.
    let tmp = write_tmp_key(&state, &key)?;
    let linked = std::fs::hard_link(&tmp, &path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(key.to_vec()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_existing_key(&path),
        Err(e) => Err(e).wrap_err_with(|| format!("creating {}", path.display())),
    }
}

/// Write `key` to a fresh `0600` temp file in `state` and fsync it, so
/// the bytes are durable before the link makes them visible. The name is
/// unique per process and call, and `create_new` skips past any leftover
/// from a crashed earlier process.
fn write_tmp_key(state: &Path, key: &[u8]) -> Result<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    loop {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = state.join(format!(".{KEY_FILE}.{}.{seq}.tmp", std::process::id()));
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).wrap_err_with(|| format!("creating {}", tmp.display())),
        };
        if let Err(e) = f.write_all(key).and_then(|()| f.sync_all()) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).wrap_err_with(|| format!("writing {}", tmp.display()));
        }
        return Ok(tmp);
    }
}

/// Read a key another process created. Never returns a short key: a key
/// file under `KEY_LEN` bytes was left by an older bougie that wrote the
/// key in place, either still mid-write (brief, so retry) or killed
/// mid-write (permanent). Using it would derive every password from a
/// guessable secret; replacing it silently would change the password of
/// every tenant already provisioned with it. Neither is safe to do
/// unattended, so the permanent case is an error that says what to do.
fn read_existing_key(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for attempt in 0..20 {
        bytes = std::fs::read(path).wrap_err_with(|| format!("reading {}", path.display()))?;
        if bytes.len() >= KEY_LEN {
            return Ok(bytes);
        }
        if attempt < 19 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    Err(eyre::eyre!(
        "credential key {} is truncated ({} of {KEY_LEN} bytes), most likely left by an \
         interrupted write. Passwords derived from it are not secret. Delete the file to \
         generate a new key; every derived password changes with it, so tenants and app \
         config that stored the old ones (Magento's app/etc/env.php) must be updated",
        path.display(),
        bytes.len()
    ))
}

/// Derive the per-tenant password for `service` + `project`. Stable
/// across re-provisioning. 48 lowercase-hex chars, matching the previous
/// random format.
pub fn derive_password(paths: &Paths, service: &str, project: &Path) -> Result<String> {
    let secret = machine_secret(paths)?;
    // Canonicalize so `/p`, `/p/`, and symlinked spellings of the same
    // project derive the same password.
    let canon = project
        .canonicalize()
        .unwrap_or_else(|_| project.to_path_buf());

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
        assert_eq!(
            a, b,
            "same machine + service + project must derive the same password"
        );
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
        let mode = std::fs::metadata(paths.state().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "credential key must be 0600");
    }

    /// Concurrent first use: every racer must get the same full-length
    /// key. With create-then-write, a loser could read the winner's
    /// still-empty file and derive passwords from an empty secret.
    #[test]
    fn concurrent_first_use_agrees_on_a_full_key() {
        for _ in 0..50 {
            let td = TempDir::new().unwrap();
            let paths = paths_in(&td);
            let keys: Vec<Vec<u8>> = std::thread::scope(|s| {
                let handles: Vec<_> = (0..8)
                    .map(|_| s.spawn(|| machine_secret(&paths).unwrap()))
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            for k in &keys {
                assert_eq!(k.len(), KEY_LEN, "a racer got a short key");
                assert_eq!(k, &keys[0], "racers disagree on the key");
            }
            assert_eq!(
                std::fs::read(paths.state().join(KEY_FILE)).unwrap(),
                keys[0]
            );
            let leftovers: Vec<_> = std::fs::read_dir(paths.state())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .filter(|n| n != KEY_FILE)
                .collect();
            assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        }
    }

    /// A key left truncated by an interrupted write must not be used
    /// (guessable passwords) nor silently replaced (changes every
    /// tenant's password) — it is an error naming the file.
    #[test]
    fn truncated_key_is_an_error_not_a_secret() {
        let td = TempDir::new().unwrap();
        let paths = paths_in(&td);
        std::fs::create_dir_all(paths.state()).unwrap();
        let path = paths.state().join(KEY_FILE);
        std::fs::write(&path, b"").unwrap();

        let err = machine_secret(&paths).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("truncated (0 of 32 bytes)"), "{msg}");
        assert!(msg.contains(KEY_FILE), "{msg}");
        // Left in place for the user to decide.
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }
}
