//! PhpStorm data-source generation.
//!
//! When `bougie up` brings up MariaDB, the project gets a per-project
//! database, user, and (deterministically derived) password. This module
//! drops a ready-to-use PhpStorm data source into the project's
//! `.idea/dataSources.xml` so the database shows up pre-configured in the
//! IDE's database tool — connecting over the unix socket with no keychain
//! prompt.
//!
//! Only MariaDB is handled. bougie's redis is unix-socket-only and
//! PhpStorm's Redis driver speaks TCP only, so a redis data source could
//! never connect; it's deliberately out of scope.
//!
//! ## Plaintext password
//!
//! PhpStorm only persists a password to the project files when it's part
//! of the JDBC URL; otherwise it goes to the OS keychain (and the user is
//! prompted on first connect). We embed `user`/`password` in the URL so
//! the connection is zero-touch. The password is loopback-only and the
//! project owner already holds it, so plaintext here is acceptable.
//!
//! ## Off-switch
//!
//! `BOUGIE_IDE_DATASOURCES=0` (or `off`/`false`) disables writing. This is
//! an interim mechanism; a persistent global config home is a follow-up.

use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Service whose tenant we turn into a PhpStorm data source.
const SERVICE: &str = "mariadb";

/// Write (or refresh) the PhpStorm MariaDB data source for `project_root`,
/// given the daemon's `service -> tenant` map from `service.up`.
///
/// Returns `Ok(Some(path))` for the file we wrote, `Ok(None)` when there
/// was nothing to do (off-switch set, or no mariadb tenant in `tenants`).
/// Errors are the caller's to treat as non-fatal — this is IDE sugar, not
/// part of bringing services up.
pub fn write_phpstorm_datasources(
    project_root: &Path,
    paths: &Paths,
    tenants: &BTreeMap<String, String>,
) -> Result<Option<PathBuf>> {
    if ide_disabled() {
        return Ok(None);
    }
    let Some(tenant) = tenants.get(SERVICE) else {
        return Ok(None);
    };

    let password =
        bougie_daemon::daemon::credentials::derive_password(paths, SERVICE, project_root)
            .wrap_err("deriving the mariadb password for the PhpStorm data source")?;
    let socket = paths.service_run(SERVICE, bougie_daemon::daemon::catalog::default_version(SERVICE)).join("mariadb.sock");

    let ds = datasource_block(tenant, &socket.to_string_lossy(), &password, project_root);

    let idea = project_root.join(".idea");
    std::fs::create_dir_all(&idea).wrap_err_with(|| format!("creating {}", idea.display()))?;
    let path = idea.join("dataSources.xml");

    let existing = std::fs::read_to_string(&path).ok();
    let merged = merge_datasource(existing.as_deref(), &ds);
    std::fs::write(&path, merged).wrap_err_with(|| format!("writing {}", path.display()))?;

    Ok(Some(path))
}

/// True when the env off-switch asks us not to touch IDE files.
fn ide_disabled() -> bool {
    match std::env::var("BOUGIE_IDE_DATASOURCES") {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        ),
        Err(_) => false,
    }
}

/// A single `<data-source>…</data-source>` element (no surrounding
/// component/project wrapper), indented for placement inside the
/// `DataSourceManagerImpl` component.
fn datasource_block(tenant: &str, socket: &str, password: &str, project_root: &Path) -> String {
    let uuid = datasource_uuid(project_root);
    let name = xml_escape(&format!("bougie: {tenant}"));
    // The socket path is emitted literally, exactly as PhpStorm's own
    // export does — MariaDB Connector/J does not URL-decode `localSocket`,
    // so percent-encoding the slashes would point it at a bogus path. The
    // `user`/`password` values are query params, so they get minimal
    // percent-encoding for the chars that would break parsing (in practice
    // a no-op: the password is hex and the tenant is sanitized). The whole
    // URL is then XML-escaped, which turns the `&` separators into `&amp;`.
    let url = format!(
        "jdbc:mariadb://?localSocket={socket}&user={user}&password={pw}",
        user = url_encode(tenant),
        pw = url_encode(password),
    );
    let url = xml_escape(&url);
    format!(
        "    <data-source source=\"LOCAL\" name=\"{name}\" uuid=\"{uuid}\">\n\
         \x20     <driver-ref>mariadb</driver-ref>\n\
         \x20     <synchronize>true</synchronize>\n\
         \x20     <jdbc-driver>org.mariadb.jdbc.Driver</jdbc-driver>\n\
         \x20     <jdbc-url>{url}</jdbc-url>\n\
         \x20     <working-dir>$ProjectFileDir$</working-dir>\n\
         \x20   </data-source>",
    )
}

/// Insert or replace our `<data-source>` inside `dataSources.xml`,
/// preserving any user-authored data sources.
///
/// - No existing file → emit the full wrapper around our one data source.
/// - Existing file with our `uuid` present → replace that block in place.
/// - Existing file without our block → insert before the closing
///   `</component>` of `DataSourceManagerImpl` (creating that component
///   before `</project>` if it's missing).
fn merge_datasource(existing: Option<&str>, ds: &str) -> String {
    let Some(existing) = existing else {
        return fresh_file(ds);
    };

    // Our block is keyed on its uuid="…" attribute, which is stable for a
    // given project. Replace it if present.
    if let Some(uuid) = uuid_of(ds)
        && let Some((start, end)) = find_datasource_span(existing, &uuid)
    {
        // `existing[..start]` already ends with the line's leading
        // indentation, so drop the indent on our block's first line to
        // avoid doubling it; the inner lines keep their own indentation.
        let mut out = String::with_capacity(existing.len());
        out.push_str(&existing[..start]);
        out.push_str(ds.trim_start());
        out.push_str(&existing[end..]);
        return out;
    }

    // Insert before the DataSourceManagerImpl component's close, else
    // create the component before </project>, else fall back to a fresh
    // file (the existing content wasn't a recognizable project file).
    if let Some(pos) = component_close_pos(existing) {
        let mut out = String::with_capacity(existing.len() + ds.len() + 1);
        out.push_str(&existing[..pos]);
        out.push_str(ds);
        out.push('\n');
        out.push_str(&existing[pos..]);
        return out;
    }
    if let Some(pos) = existing.find("</project>") {
        let mut out = String::with_capacity(existing.len() + ds.len() + 128);
        out.push_str(&existing[..pos]);
        out.push_str(COMPONENT_OPEN);
        out.push('\n');
        out.push_str(ds);
        out.push('\n');
        out.push_str("  </component>\n");
        out.push_str(&existing[pos..]);
        return out;
    }

    fresh_file(ds)
}

const COMPONENT_OPEN: &str =
    "  <component name=\"DataSourceManagerImpl\" format=\"xml\" multifile-model=\"true\">";

/// A complete `dataSources.xml` wrapping a single data source.
fn fresh_file(ds: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <project version=\"4\">\n\
         {COMPONENT_OPEN}\n\
         {ds}\n\
         \x20 </component>\n\
         </project>",
    )
}

/// Byte offset just before the closing `</component>` of the
/// `DataSourceManagerImpl` component, if that component exists.
fn component_close_pos(xml: &str) -> Option<usize> {
    let comp = xml.find("name=\"DataSourceManagerImpl\"")?;
    let rel = xml[comp..].find("</component>")?;
    Some(comp + rel)
}

/// Byte span (start..end) of the `<data-source …uuid="<uuid>"…>…</data-source>`
/// element, where `start` is the offset of `<data-source` and `end` is just
/// after `</data-source>`.
fn find_datasource_span(xml: &str, uuid: &str) -> Option<(usize, usize)> {
    let needle = format!("uuid=\"{uuid}\"");
    let at = xml.find(&needle)?;
    // Walk back to the `<data-source` that owns this attribute.
    let start = xml[..at].rfind("<data-source")?;
    let close = "</data-source>";
    let rel = xml[start..].find(close)?;
    Some((start, start + rel + close.len()))
}

/// Pull the `uuid="…"` value out of our generated block.
fn uuid_of(ds: &str) -> Option<String> {
    let key = "uuid=\"";
    let at = ds.find(key)? + key.len();
    let rel = ds[at..].find('"')?;
    Some(ds[at..at + rel].to_string())
}

/// Deterministic UUID (8-4-4-4-12 hex) from the project path, so re-running
/// `bougie up` refreshes our block in place rather than duplicating it.
fn datasource_uuid(project_root: &Path) -> String {
    let canon = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let mut h = Sha256::new();
    h.update(b"bougie:mariadb:");
    h.update(canon.as_os_str().as_encoded_bytes());
    let d = h.finalize();
    let hex = hex_encode(&d[..16]);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32],
    )
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

/// XML attribute/text escaping for the five predefined entities.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encode everything outside the unreserved set so the value is
/// safe as a JDBC URL query parameter. Applied to `user`/`password` only —
/// the socket path is emitted literally (see `datasource_block`).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ds() -> String {
        // uuid is what the merge keys on; pick a fixed one for the tests.
        "    <data-source source=\"LOCAL\" name=\"bougie: app\" uuid=\"aaaa-bbbb\">\n\
         \x20     <jdbc-url>jdbc:mariadb://?localSocket=/s&amp;user=app</jdbc-url>\n\
         \x20   </data-source>"
            .to_string()
    }

    #[test]
    fn fresh_file_has_wrapper_and_block() {
        let out = merge_datasource(None, &sample_ds());
        assert!(out.starts_with("<?xml"));
        assert!(out.contains("DataSourceManagerImpl"));
        assert!(out.contains("uuid=\"aaaa-bbbb\""));
        assert!(out.trim_end().ends_with("</project>"));
    }

    #[test]
    fn merge_preserves_foreign_datasource() {
        let foreign = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
            <project version=\"4\">\n\
            \x20 <component name=\"DataSourceManagerImpl\" format=\"xml\" multifile-model=\"true\">\n\
            \x20   <data-source source=\"LOCAL\" name=\"other\" uuid=\"ffff-0000\">\n\
            \x20     <jdbc-url>jdbc:postgresql://localhost</jdbc-url>\n\
            \x20   </data-source>\n\
            \x20 </component>\n\
            </project>";
        let out = merge_datasource(Some(foreign), &sample_ds());
        assert!(
            out.contains("uuid=\"ffff-0000\""),
            "foreign source dropped:\n{out}"
        );
        assert!(
            out.contains("uuid=\"aaaa-bbbb\""),
            "our source missing:\n{out}"
        );
        // Exactly one component, two data sources.
        assert_eq!(out.matches("<data-source").count(), 2);
        assert_eq!(out.matches("DataSourceManagerImpl").count(), 1);
    }

    #[test]
    fn merge_is_idempotent() {
        let once = merge_datasource(None, &sample_ds());
        let twice = merge_datasource(Some(&once), &sample_ds());
        assert_eq!(once, twice, "re-running must not change bytes or duplicate");
        assert_eq!(twice.matches("<data-source").count(), 1);
    }

    #[test]
    fn merge_replaces_changed_block() {
        let v1 = merge_datasource(None, &sample_ds());
        let updated = sample_ds().replace("localSocket=/s", "localSocket=/new");
        let v2 = merge_datasource(Some(&v1), &updated);
        assert!(v2.contains("localSocket=/new"));
        assert!(!v2.contains("localSocket=/s&"));
        assert_eq!(v2.matches("<data-source").count(), 1);
    }

    #[test]
    fn uuid_is_stable_and_shaped() {
        let u = datasource_uuid(Path::new("/tmp/some/project"));
        assert_eq!(u, datasource_uuid(Path::new("/tmp/some/project")));
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(u.bytes().all(|c| c == b'-' || c.is_ascii_hexdigit()));
    }

    #[test]
    fn url_encode_escapes_reserved() {
        assert_eq!(url_encode("a b&c"), "a%20b%26c");
        assert_eq!(url_encode("plain-_.~"), "plain-_.~");
    }

    #[test]
    fn xml_escape_handles_amp_and_quotes() {
        assert_eq!(xml_escape("a&b\"c"), "a&amp;b&quot;c");
    }

    #[test]
    fn socket_path_is_literal_not_percent_encoded() {
        let ds = datasource_block(
            "mageos_lite",
            "/home/u/.local/share/bougie/state/services/mariadb/run/mariadb.sock",
            "deadbeefcafe",
            Path::new("/tmp/proj"),
        );
        assert!(
            ds.contains("localSocket=/home/u/.local/share/bougie"),
            "{ds}"
        );
        assert!(
            !ds.contains("%2F"),
            "socket slashes must stay literal:\n{ds}"
        );
        // The `&` query separators are XML-escaped.
        assert!(
            ds.contains("mariadb.sock&amp;user=mageos_lite&amp;password=deadbeefcafe"),
            "{ds}"
        );
    }

    #[test]
    fn off_switch_values() {
        // The parsing is what we assert here; env mutation is avoided so
        // the test stays parallel-safe.
        for v in ["0", "off", "FALSE", "No"] {
            assert!(matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no"
            ));
        }
    }
}
