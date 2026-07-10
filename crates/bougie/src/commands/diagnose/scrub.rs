//! Known-credential scrubbing + home folding.
//!
//! bougie *knows* every secret it minted or read: tenant passwords
//! from the per-service ledgers and composer auth material. Those are
//! replaced by exact value before the draft is ever rendered — no
//! regex guessing. The `$EDITOR` pass is the backstop for anything
//! this can't know about.

use bougie_paths::Paths;
use std::path::Path;

const MIN_SECRET_LEN: usize = 6;

#[derive(Debug)]
pub struct Scrubber {
    home: Option<String>,
    /// `(value, label)` — replaced with `«redacted:<label>»`.
    secrets: Vec<(String, &'static str)>,
}

impl Scrubber {
    pub fn from_env(paths: &Paths, project_root: Option<&Path>) -> Self {
        let mut secrets = Vec::new();

        // Tenant ledgers: state/services/<name>/<version>/tenants.json is
        // JSON Lines; every value under `secrets` is a generated
        // credential. Instances are version-keyed, so descend one level
        // (per-version subdir) below each service name.
        if let Ok(name_entries) = std::fs::read_dir(paths.services_dir()) {
            for name_entry in name_entries.flatten() {
                let Ok(version_entries) = std::fs::read_dir(name_entry.path()) else {
                    continue;
                };
                for version_entry in version_entries.flatten() {
                    let Ok(text) =
                        std::fs::read_to_string(version_entry.path().join("tenants.json"))
                    else {
                        continue;
                    };
                    for line in text.lines() {
                        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                            continue;
                        };
                        if let Some(map) = v.get("secrets").and_then(serde_json::Value::as_object) {
                            for val in map.values().filter_map(serde_json::Value::as_str) {
                                push_secret(&mut secrets, val, "tenant-secret");
                            }
                        }
                    }
                }
            }
        }

        // Composer auth: every leaf string in COMPOSER_AUTH and the
        // project's auth.json. Over-scrubbing a username is fine;
        // leaking a token is not.
        if let Ok(raw) = std::env::var("COMPOSER_AUTH") {
            collect_json_strings(&raw, &mut secrets, "auth-token");
        }
        if let Some(root) = project_root
            && let Ok(raw) = std::fs::read_to_string(root.join("auth.json"))
        {
            collect_json_strings(&raw, &mut secrets, "auth-token");
        }

        let home = std::env::var("HOME").ok().filter(|h| !h.is_empty());
        Self { home, secrets }
    }

    /// Exact-value credential replacement, then home folding (in that
    /// order — a secret containing the home path must not be split by
    /// the fold first).
    pub fn scrub(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (value, label) in &self.secrets {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), &format!("«redacted:{label}»"));
            }
        }
        if let Some(h) = &self.home {
            out = out.replace(h.as_str(), "~");
        }
        out
    }
}

fn push_secret(secrets: &mut Vec<(String, &'static str)>, value: &str, label: &'static str) {
    // Too-short values would shred unrelated text ("root", "dev", …).
    if value.len() >= MIN_SECRET_LEN && !secrets.iter().any(|(v, _)| v == value) {
        secrets.push((value.to_owned(), label));
    }
}

/// Every leaf string in a JSON document (`auth.json` /
/// `COMPOSER_AUTH` shape: nested maps of host → credentials).
fn collect_json_strings(raw: &str, secrets: &mut Vec<(String, &'static str)>, label: &'static str) {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(raw) else {
        return;
    };
    let mut stack = vec![&root];
    while let Some(v) = stack.pop() {
        match v {
            serde_json::Value::String(s) => push_secret(secrets, s, label),
            serde_json::Value::Array(items) => stack.extend(items),
            serde_json::Value::Object(map) => stack.extend(map.values()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scrubber_with(home: Option<&str>, secrets: &[(&str, &'static str)]) -> Scrubber {
        Scrubber {
            home: home.map(str::to_owned),
            secrets: secrets.iter().map(|(v, l)| ((*v).to_owned(), *l)).collect(),
        }
    }

    #[test]
    fn replaces_secrets_then_folds_home() {
        let s = scrubber_with(Some("/home/dev"), &[("hunter2secret", "tenant-secret")]);
        let scrubbed = s.scrub("mysql -phunter2secret --socket /home/dev/x.sock");
        assert_eq!(
            scrubbed,
            "mysql -p«redacted:tenant-secret» --socket ~/x.sock"
        );
    }

    #[test]
    fn short_values_are_not_collected() {
        let mut secrets = Vec::new();
        push_secret(&mut secrets, "root", "tenant-secret");
        push_secret(&mut secrets, "longenough", "tenant-secret");
        assert_eq!(secrets.len(), 1);
    }

    #[test]
    fn ledger_secrets_are_loaded_from_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root: PathBuf = tmp.path().into();
        let paths = Paths::new(root.clone(), root.clone());
        // Version-keyed layout: state/services/<name>/<version>/tenants.json.
        let dir = paths.service_dir("mariadb", "11.4.4");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tenants.json"),
            r#"{"schema_version":1,"tenant":"shop","project":"/p","created_at":"t","secrets":{"password":"s3cretpw123"}}"#,
        )
        .unwrap();
        let s = Scrubber::from_env(&paths, None);
        let scrubbed = s.scrub("access denied for shop using s3cretpw123");
        assert!(!scrubbed.contains("s3cretpw123"), "{scrubbed}");
        assert!(scrubbed.contains("«redacted:tenant-secret»"), "{scrubbed}");
    }

    #[test]
    fn auth_json_leaves_are_scrubbed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root: PathBuf = tmp.path().into();
        std::fs::write(
            root.join("auth.json"),
            r#"{"http-basic":{"repo.example.com":{"username":"jane","password":"tok_abc12345"}}}"#,
        )
        .unwrap();
        let paths = Paths::new(root.clone(), root.clone());
        let s = Scrubber::from_env(&paths, Some(&root));
        let scrubbed = s.scrub("GET https://repo.example.com with tok_abc12345");
        assert!(!scrubbed.contains("tok_abc12345"), "{scrubbed}");
    }
}
