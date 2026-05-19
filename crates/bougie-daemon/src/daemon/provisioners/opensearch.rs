//! `OpenSearch` tenancy: per-tenant index template. SERVICES.md §3.3.
//!
//! Per-project tenant gets:
//!   - an index template named `<tenant>` matching `<tenant>-*`,
//!   - sole authority over indices created under that prefix.
//!
//! Auth model: this is a dev-only single-node opensearch with the
//! security plugin omitted (bougie-index ships `opensearch-min`).
//! There are no users/roles in v1 — tenant isolation is purely
//! convention-based (index-prefix). Real auth lands when bougie
//! starts treating opensearch as a multi-process catalog entry that
//! some users would run alongside a security-plugin install.

use crate::daemon::{store_layout, tenants::{self, Tenant}};
use bougie_paths::Paths;
use eyre::{eyre, Result, WrapErr};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::time::Instant;

/// Loopback URL the supervisor binds opensearch to. The catalog pins
/// the port at 9200 (`Binding::Tcp { port: 9200 }`); keep this in
/// lockstep if it ever moves.
const OPENSEARCH_BASE_URL: &str = "http://127.0.0.1:9200";

/// HTTP timeout for every provisioning call. Index-template PUT is
/// usually <100ms on local; the generous cap covers cluster-state
/// recovery on a cold start.
const PROVISION_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the cluster's `/` endpoint to start
/// responding after the supervisor's TCP-connect health probe wins.
/// The TCP probe completes the moment Netty binds; the cluster state
/// takes another second or two to reach "started" before HTTP works.
const PROVISION_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Opensearch pre-start hook. Idempotent — re-running just re-copies
/// the config dir.
///
/// Three things to set up before mariadb-style relative-path leaks
/// crash the JVM during sandbox start:
///
/// 1. **`<datadir>/tmp`** — pinned by `OPENSEARCH_TMPDIR`
///    (`supervisor::render_exec_env`). The sandbox hides `/tmp`.
/// 2. **`<datadir>/logs`** — the JVM's `-XX:ErrorFile=logs/hs_err...`
///    and friends in `jvm.options` are resolved relative to CWD by
///    the JVM at startup. CWD ends up at `OPENSEARCH_HOME` because
///    `bin/opensearch-env` does `cd "$OPENSEARCH_HOME"` near the end,
///    and that's the read-only store. We copy the entire `config/`
///    out to `<service_conf>/`, rewrite the path-bearing options to
///    absolute paths under `<datadir>/`, and point
///    `OPENSEARCH_PATH_CONF` at our copy (see `render_exec_env`).
/// 3. **`<datadir>/data`** — for `path.data` and the JVM's
///    `-XX:HeapDumpPath=data` (also relative).
pub async fn pre_start(paths: &Paths) -> Result<()> {
    let data = paths.service_data("opensearch");
    let conf = paths.service_conf("opensearch");
    for sub in ["tmp", "logs", "data"] {
        let p = data.join(sub);
        tokio::fs::create_dir_all(&p)
            .await
            .wrap_err_with(|| format!("creating {}", p.display()))?;
    }
    tokio::fs::create_dir_all(&conf)
        .await
        .wrap_err_with(|| format!("creating {}", conf.display()))?;

    let entry = crate::daemon::catalog::find("opensearch")
        .ok_or_else(|| eyre!("BUG: opensearch missing from catalog"))?;
    let basedir = store_layout::basedir(paths, entry)
        .wrap_err("resolving opensearch basedir")?;
    let src_config = basedir.join("config");
    if !tokio::fs::metadata(&src_config).await.map(|m| m.is_dir()).unwrap_or(false) {
        return Err(eyre!(
            "opensearch tarball missing config/ at {}",
            src_config.display()
        ));
    }
    copy_dir_tree(&src_config, &conf)
        .await
        .wrap_err_with(|| format!("copying config/ to {}", conf.display()))?;
    rewrite_jvm_options(&conf.join("jvm.options"), &data)
        .await
        .wrap_err_with(|| format!("rewriting {}", conf.join("jvm.options").display()))?;
    Ok(())
}

/// Recursive copy. opensearch's config/ has ~4 files + jvm.options.d/;
/// no symlinks, no hardlinks — a basic `read + write` walk is fine.
///
/// Recursive async fn requires boxing: we return a `BoxFuture` so the
/// function's future type doesn't have to contain itself.
fn copy_dir_tree<'a>(
    src: &'a Path,
    dst: &'a Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let mut rd = tokio::fs::read_dir(src)
            .await
            .wrap_err_with(|| format!("reading {}", src.display()))?;
        while let Some(entry) = rd.next_entry().await? {
            let from = entry.path();
            let to = dst.join(entry.file_name());
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                tokio::fs::create_dir_all(&to).await?;
                copy_dir_tree(&from, &to).await?;
            } else if ft.is_file() {
                tokio::fs::copy(&from, &to)
                    .await
                    .wrap_err_with(|| format!("copy {} → {}", from.display(), to.display()))?;
            }
            // Symlinks shouldn't appear in opensearch's config/. Skip.
        }
        Ok(())
    })
}

/// Replace `logs/...` and `data` relative paths in `jvm.options` with
/// absolute paths anchored at the per-service data dir. The file ships
/// with three problem lines:
///   - `-Xloggc:logs/gc.log`
///   - `-Xlog:gc*,...:file=logs/gc.log:...`
///   - `-XX:ErrorFile=logs/hs_err_pid%p.log`
///   - `-XX:HeapDumpPath=data`
async fn rewrite_jvm_options(path: &Path, data_dir: &Path) -> Result<()> {
    let logs = data_dir.join("logs");
    let data_sub = data_dir.join("data");
    let content = tokio::fs::read_to_string(path)
        .await
        .wrap_err_with(|| format!("reading {}", path.display()))?;
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        // Only touch *active* directives — keep comments verbatim so
        // future debug archaeology still works. The lines we care
        // about all start with `-` or with a JDK version range like
        // `9-:`, `9-:-Xlog:...`, `8:-Xloggc:...`.
        let trimmed = line.trim_start();
        let looks_active = trimmed.starts_with('-')
            || trimmed
                .split_once(':')
                .is_some_and(|(prefix, _)| prefix.chars().all(|c| c.is_ascii_digit() || c == '-'));

        if looks_active {
            let replaced = line
                .replace("logs/", &format!("{}/", logs.display()))
                .replace("HeapDumpPath=data", &format!("HeapDumpPath={}", data_sub.display()));
            out.push_str(&replaced);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    tokio::fs::write(path, out)
        .await
        .wrap_err_with(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Provision a tenant. Idempotent — repeated calls for the same
/// project re-use the existing index template.
pub async fn provision(tenants_path: &Path, tenant_name: &str, project: &Path) -> Result<Tenant> {
    let existing = tenants::load_all(tenants_path).await?;
    if let Some(existing_t) = existing.iter().find(|t| t.project == project) {
        return Ok(existing_t.clone());
    }

    if !is_safe_template_name(tenant_name) {
        return Err(eyre!(
            "opensearch: tenant name `{tenant_name}` contains characters disallowed in \
             an index-template name (must match `[a-z0-9_]+`); rename via \
             `bougie services add opensearch --tenant=...`"
        ));
    }

    wait_for_cluster(PROVISION_READY_TIMEOUT)
        .await
        .wrap_err("opensearch cluster never became HTTP-ready")?;

    put_index_template(tenant_name)
        .await
        .wrap_err_with(|| format!("provisioning opensearch tenant `{tenant_name}`"))?;

    let mut tenant = Tenant::new(tenant_name, project.to_path_buf());
    tenant
        .alloc
        .insert("index_prefix".into(), serde_json::json!(format!("{tenant_name}-")));
    tenants::append(tenants_path, &tenant).await?;
    Ok(tenant)
}

/// Release a tenant. With `purge`, deletes both the index template
/// and every index that matched `<tenant>-*`. Without `purge`, only
/// the local ledger entry goes away; opensearch state survives a
/// `services down` so a later `up` re-uses it (matches redis/mariadb).
pub async fn deprovision(tenants_path: &Path, tenant_name: &str, purge: bool) -> Result<()> {
    let existing = tenants::load_all(tenants_path).await?;
    let Some(_target) = existing.iter().find(|t| t.tenant == tenant_name).cloned() else {
        return Ok(());
    };
    if purge {
        if !is_safe_template_name(tenant_name) {
            return Err(eyre!(
                "opensearch: refusing to purge tenant with unsafe identifier `{tenant_name}`"
            ));
        }
        // Best-effort: the live cluster might be down (e.g. `down`
        // races against the supervisor stop), in which case the user
        // intends to discard the ledger entry regardless.
        let _ = delete_indices(tenant_name).await;
        let _ = delete_index_template(tenant_name).await;
    }
    tenants::rewrite(tenants_path, |t| t.tenant != tenant_name).await?;
    Ok(())
}

// -------------------- HTTP helpers --------------------

// Build only fails on TLS-config errors, which can't apply to our
// HTTP-only localhost client.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(PROVISION_HTTP_TIMEOUT)
            .build()
            .expect("reqwest::Client::builder() for HTTP-only localhost cannot fail")
    })
}

async fn put_index_template(tenant: &str) -> Result<()> {
    let body = serde_json::json!({
        "index_patterns": [format!("{tenant}-*")],
        // `priority` ensures user-specified templates with the
        // default priority of 0 don't outrank us by accident.
        "priority": 100,
        "template": {
            "settings": {
                // Default of 1 replica fails red on a single-node
                // cluster; force 0 so health stays green out of the
                // box.
                "index": {
                    "number_of_replicas": 0,
                    "number_of_shards": 1,
                }
            }
        },
        "_meta": {
            "owner": "bougie",
            "tenant": tenant,
        }
    });
    let url = format!("{OPENSEARCH_BASE_URL}/_index_template/{tenant}");
    let resp = http_client()
        .put(&url)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| eyre!("PUT {url}: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(eyre!("PUT {url} returned {status}: {text}"));
    }
    Ok(())
}

async fn delete_index_template(tenant: &str) -> Result<()> {
    let url = format!("{OPENSEARCH_BASE_URL}/_index_template/{tenant}");
    let resp = http_client()
        .delete(&url)
        .send()
        .await
        .map_err(|e| eyre!("DELETE {url}: {e}"))?;
    // 404 is fine — template might have been wiped already.
    if !(resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND) {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(eyre!("DELETE {url} returned {status}: {text}"));
    }
    Ok(())
}

/// `DELETE /<tenant>-*` removes every index the tenant created.
/// Opensearch refuses the wildcard unless the URL itself is allowed-
/// to-be-destructive — we send `?expand_wildcards=open,closed`
/// explicitly.
async fn delete_indices(tenant: &str) -> Result<()> {
    let url = format!("{OPENSEARCH_BASE_URL}/{tenant}-*?expand_wildcards=open,closed");
    let resp = http_client()
        .delete(&url)
        .send()
        .await
        .map_err(|e| eyre!("DELETE {url}: {e}"))?;
    if !(resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND) {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(eyre!("DELETE {url} returned {status}: {text}"));
    }
    Ok(())
}

/// Poll `GET /` until it returns 200. The supervisor's TCP-connect
/// probe is satisfied the moment Netty binds, but cluster bootstrap
/// keeps the HTTP layer rejecting requests for another second or two
/// while it initialises the cluster state.
async fn wait_for_cluster(timeout: Duration) -> Result<()> {
    let client = http_client();
    let deadline = Instant::now() + timeout;
    let url = format!("{OPENSEARCH_BASE_URL}/");
    loop {
        if let Ok(r) = client.get(&url).send().await {
            if r.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(eyre!(
                "opensearch HTTP root never returned 200 within {timeout:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Match `[a-z0-9_]+`. Opensearch's index template names accept a
/// broader set than mariadb identifiers but we lowercase + strip
/// non-word chars at the CLI layer already (composer "acme/blog" →
/// "acme_blog"); the cap here is defence-in-depth so a malformed
/// `extra.bougie.services.tenant` can't ship malicious URL paths.
fn is_safe_template_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_template_name_accepts_typical_tenants() {
        assert!(is_safe_template_name("acme_blog"));
        assert!(is_safe_template_name("blog_2026"));
        assert!(is_safe_template_name("a"));
    }

    #[test]
    fn safe_template_name_rejects_uppercase_and_metacharacters() {
        assert!(!is_safe_template_name(""));
        assert!(!is_safe_template_name("AcmeBlog"));
        assert!(!is_safe_template_name("foo bar"));
        assert!(!is_safe_template_name("foo/bar"));
        assert!(!is_safe_template_name("foo;DROP"));
        assert!(!is_safe_template_name(&"x".repeat(129)));
    }
}
