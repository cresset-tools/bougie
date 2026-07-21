//! `bougie login <URL>` — authenticate against a sconce Composer registry via
//! the OAuth 2.0 device authorization grant (RFC 8628), then auto-provision the
//! project's Composer `repositories`.
//!
//! The CLI starts a flow, prints a short user code and opens the dashboard, then
//! polls until a signed-in team member approves it in the browser. On approval
//! the registry mints an **org-scoped read token** — one credential that
//! authenticates every repository served under that host — which we persist to
//! bougie's own credential store keyed by origin (`host[:port]`), exactly where
//! the resolver looks for it. No browser callback, no local listener: the CLI
//! only ever makes outbound requests, so it works over SSH and in containers.
//!
//! With a token in hand we then discover which repositories it can access
//! (`GET /api/v1/repos`) and provision them so the dev doesn't paste URLs:
//! by default into a `vendor/bougie/repositories.json` overlay (bougie's own
//! gitignored project tree) the resolver merges at resolve time; with
//! `--composer-json`, into the committed `composer.json` so teammates on stock
//! Composer see them too. Provisioning is
//! best-effort — the token is already stored, and re-running is idempotent, so a
//! discovery/write hiccup warns rather than failing the login.

use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// Where `bougie login` writes the discovered `repositories`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionMode {
    /// Default: bougie's `vendor/bougie/repositories.json` overlay (gitignored).
    Overlay,
    /// `--composer-json`: the committed `composer.json` (stock-Composer-visible).
    ComposerJson,
    /// `--no-provision`: only store the token.
    Skip,
}

impl ProvisionMode {
    #[must_use]
    pub fn from_flags(no_provision: bool, composer_json: bool) -> Self {
        if no_provision {
            Self::Skip
        } else if composer_json {
            Self::ComposerJson
        } else {
            Self::Overlay
        }
    }
}

/// The registry's response to `POST /oauth/device`.
#[derive(Debug, Deserialize)]
struct DeviceStart {
    device_code: String,
    user_code: String,
    /// Where the human approves (dashboard).
    verification_uri: String,
    /// Same, pre-filled with the code — preferred for the browser hand-off.
    #[serde(default)]
    verification_uri_complete: Option<String>,
    /// Seconds the flow stays valid.
    expires_in: u64,
    /// Minimum seconds between polls.
    #[serde(default)]
    interval: Option<u64>,
}

/// The registry's response to `GET /api/v1/repos`.
#[derive(Debug, Deserialize)]
struct RepoListResponse {
    #[serde(default)]
    repositories: Vec<RepoRef>,
}

#[derive(Debug, Deserialize)]
struct RepoRef {
    url: String,
}

/// What provisioning did (or why it didn't), for output.
#[derive(Debug, Serialize)]
struct ProvisionSummary {
    /// `overlay`, `composer.json`, or `skipped`.
    target: String,
    /// Repositories newly written (0 = all already present, or skipped).
    added: usize,
    /// Repositories the token can access (context; 0 when skipped).
    available: usize,
    /// Human note when skipped or degraded.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// Structured result of a successful login.
#[derive(Debug, Serialize)]
struct LoginResult {
    schema_version: u32,
    /// The origin the token was stored against (`host[:port]`).
    host: String,
    /// Path of the credential store written.
    stored_at: String,
    provision: ProvisionSummary,
}

impl Render for LoginResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "Signed in to {}. Token stored in {}.",
            self.host, self.stored_at
        )?;
        let p = &self.provision;
        if p.added > 0 {
            writeln!(
                w,
                "Provisioned {} repositor{} into {}.",
                p.added,
                if p.added == 1 { "y" } else { "ies" },
                p.target
            )?;
        } else if let Some(note) = &p.note {
            writeln!(w, "{note}")?;
        } else if p.available > 0 {
            writeln!(w, "Repositories already up to date ({}).", p.target)?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, url: &str, mode: ProvisionMode) -> Result<ExitCode> {
    let base = url.trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(eyre!(
            "registry URL must start with http:// or https:// (got `{url}`)"
        ));
    }
    let host = bougie_composer_resolver::metadata::auth_origin(base);

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("bougie/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .wrap_err("building http client")?;

    // 1. Start the flow.
    let start: DeviceStart = client
        .post(format!("{base}/oauth/device"))
        .send()
        .wrap_err_with(|| format!("contacting {base} to start login"))?
        .error_for_status()
        .wrap_err(
            "the registry rejected the login request — check the URL points at a sconce instance",
        )?
        .json()
        .wrap_err("parsing the login response")?;

    // 2. Point the user at the approval page (and try to open it for them).
    let verify = start
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&start.verification_uri);
    eprintln!(
        "To finish signing in, open\n\n    {}\n\nand confirm this code: {}\n",
        start.verification_uri, start.user_code
    );
    if crate::commands::server::open_url(verify).is_ok() {
        eprintln!("(opened your browser)\n");
    }
    eprintln!("Waiting for approval…");

    // 3. Poll the token endpoint until approved, denied, or expired. The server
    // enforces the deadline (`expired_token`); the local one is a backstop.
    let deadline = Instant::now() + Duration::from_secs(start.expires_in);
    let mut interval = Duration::from_secs(start.interval.unwrap_or(5).max(1));
    let token: String = loop {
        std::thread::sleep(interval);
        if Instant::now() >= deadline {
            return Err(eyre!(
                "login timed out before it was approved — run `bougie login` again"
            ));
        }
        let resp = client
            .post(format!("{base}/oauth/device/token"))
            .json(&serde_json::json!({ "device_code": start.device_code }))
            .send()
            .wrap_err("polling for login approval")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().unwrap_or_default();
        if status.is_success() {
            break body
                .get("access_token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| eyre!("login approved but no token was returned"))?
                .to_owned();
        }
        match body.get("error").and_then(serde_json::Value::as_str) {
            Some("authorization_pending") => {}
            // RFC 8628: back off an extra 5s and keep polling.
            Some("slow_down") => interval += Duration::from_secs(5),
            Some("access_denied") => return Err(eyre!("login was denied in the browser")),
            Some("expired_token") => {
                return Err(eyre!(
                    "the login request expired before approval — run `bougie login` again"
                ));
            }
            Some(other) => return Err(eyre!("login failed: {other}")),
            None => return Err(eyre!("login failed with HTTP {status}")),
        }
    };

    // 4 + 5: store the token where the resolver reads it, then auto-provision.
    finish(format, &client, base, &host, &token, mode)
}

/// Non-interactive **CI** login: exchange the workflow's OIDC JWT for a
/// short-lived read token via `POST /oauth/ci` (zero stored secret), then store
/// + provision exactly like the device flow. The registry's `read` CI policy on
/// `repository` decides the token's scope — an org-scoped policy yields a token
/// valid for every private repo in the org, which is what `bougie sync` needs.
pub fn run_ci(
    format: OutputFormat,
    url: &str,
    repository: &str,
    audience: &str,
    mode: ProvisionMode,
) -> Result<ExitCode> {
    let base = url.trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(eyre!(
            "registry URL must start with http:// or https:// (got `{url}`)"
        ));
    }
    if repository.is_empty() {
        return Err(eyre!("--ci requires --repository <org/repo>"));
    }
    let host = bougie_composer_resolver::metadata::auth_origin(base);

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("bougie/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .wrap_err("building http client")?;

    let jwt = acquire_ci_jwt(&client, audience)?;
    let token = ci_exchange(&client, base, repository, &jwt)?;
    finish(format, &client, base, &host, &token, mode)
}

/// Store the read token where the resolver looks (keyed by origin), then
/// best-effort auto-provision the project's Composer repositories. Shared by the
/// device flow and the CI exchange.
fn finish(
    format: OutputFormat,
    client: &reqwest::blocking::Client,
    base: &str,
    host: &str,
    token: &str,
    mode: ProvisionMode,
) -> Result<ExitCode> {
    let path = bougie_composer_resolver::update::write_bougie_bearer(host, token)
        .wrap_err("storing the login token")?;
    let provision = provision(client, base, token, mode);
    let result = LoginResult {
        schema_version: 1,
        host: host.to_owned(),
        stored_at: path.display().to_string(),
        provision,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Acquire the CI platform's OIDC JWT for the read exchange. Preference order:
/// `$SCONCE_ID_TOKEN` (GitLab's `id_tokens:`, or any pre-fetched JWT), then
/// GitHub Actions' token service (`$ACTIONS_ID_TOKEN_REQUEST_URL/_TOKEN` with
/// `audience`). Errors with setup guidance when neither is available.
fn acquire_ci_jwt(client: &reqwest::blocking::Client, audience: &str) -> Result<String> {
    if let Ok(jwt) = std::env::var("SCONCE_ID_TOKEN") {
        if !jwt.trim().is_empty() {
            return Ok(jwt);
        }
    }
    let req_url = std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").unwrap_or_default();
    let req_token = std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").unwrap_or_default();
    if req_url.is_empty() || req_token.is_empty() {
        return Err(eyre!(
            "no CI OIDC token available. On GitHub Actions grant `permissions: id-token: write`; \
             on GitLab add an `id_tokens:` block exposing $SCONCE_ID_TOKEN. (Or set \
             $SCONCE_ID_TOKEN to a pre-fetched JWT.)"
        ));
    }
    #[derive(Deserialize)]
    struct IdTokenResponse {
        value: String,
    }
    let resp: IdTokenResponse = client
        .get(&req_url)
        .query(&[("audience", audience)])
        .bearer_auth(&req_token)
        .send()
        .wrap_err("requesting a GitHub Actions OIDC token")?
        .error_for_status()
        .wrap_err("the GitHub Actions OIDC token request failed")?
        .json()
        .wrap_err("parsing the GitHub Actions OIDC token response")?;
    Ok(resp.value)
}

/// Trade the OIDC JWT for a short-lived read token at `POST /oauth/ci`.
fn ci_exchange(
    client: &reqwest::blocking::Client,
    base: &str,
    repository: &str,
    jwt: &str,
) -> Result<String> {
    let resp = client
        .post(format!("{base}/oauth/ci"))
        .json(&serde_json::json!({ "repository": repository, "jwt": jwt }))
        .send()
        .wrap_err("contacting the registry for the CI token exchange")?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(eyre!(
            "the registry rejected the CI OIDC exchange (401). Check that {repository} has a \
             `read` CI policy whose issuer/audience/claims match this workflow — an admin adds \
             one with `sconce ci-policy add {repository} --capability read --scope org …`."
        ));
    }
    let body: serde_json::Value = resp
        .error_for_status()
        .wrap_err("the CI token exchange failed")?
        .json()
        .wrap_err("parsing the CI token exchange response")?;
    body.get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| eyre!("the exchange returned no access_token"))
}

/// Discover the repositories the token can access and write them into the
/// project, per `mode`. Best-effort: every failure path degrades to a note (the
/// token is already stored, and re-running `bougie login` re-provisions).
fn provision(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    mode: ProvisionMode,
) -> ProvisionSummary {
    let target = match mode {
        ProvisionMode::Overlay => "overlay",
        ProvisionMode::ComposerJson => "composer.json",
        ProvisionMode::Skip => "skipped",
    };
    let skipped = |note: &str| ProvisionSummary {
        target: target.to_owned(),
        added: 0,
        available: 0,
        note: Some(note.to_owned()),
    };

    if mode == ProvisionMode::Skip {
        return skipped("Skipped repository provisioning (--no-provision).");
    }

    // Only provision inside a project — `login` is often run once, globally.
    let Ok(cwd) = std::env::current_dir() else {
        return skipped("Not in a project — skipped repository provisioning.");
    };
    let Some(root) = crate::failure::project_root_near(&cwd) else {
        return skipped(
            "Not in a project — run `bougie login` inside a project to auto-configure its \
             repositories.",
        );
    };

    // Record which registry this project is wired to, durably (outside
    // `vendor/`), so `bougie start` can re-provision the overlay after a
    // `rm -rf vendor`. Only for the overlay sink — a committed composer.json
    // survives a wipe on its own. Best-effort: never affects login.
    if mode == ProvisionMode::Overlay {
        crate::commands::team::write_record(&root, base);
    }

    // Discover the repos this token can see — the git-remote-keyed team manifest
    // when the project has an `origin` remote, else the login org's repos.
    let urls = match crate::commands::team::discover_repo_urls(client, base, token, &root) {
        Ok(urls) => urls,
        Err(e) => {
            return skipped(&format!(
                "Signed in, but couldn't auto-configure repositories ({e}). The token still works."
            ));
        }
    };
    if urls.is_empty() {
        return skipped("Signed in. The token grants no repositories yet.");
    }
    let available = urls.len();

    let added = match mode {
        ProvisionMode::Overlay => {
            match bougie_composer_resolver::update::write_repositories_overlay(&root, &urls) {
                Ok((_path, added)) => added,
                Err(e) => {
                    return skipped(&format!("Signed in, but writing the overlay failed: {e}"));
                }
            }
        }
        ProvisionMode::ComposerJson => {
            match bougie_composer::lockfile::add_repositories(&root, &urls) {
                Ok(applied) => applied.added,
                Err(e) => {
                    return skipped(&format!(
                        "Signed in, but updating composer.json failed: {e}"
                    ));
                }
            }
        }
        ProvisionMode::Skip => 0,
    };

    ProvisionSummary {
        target: target.to_owned(),
        added,
        available,
        note: None,
    }
}

/// `GET {base}/api/v1/repos` with the token → the repositories' Composer URLs.
/// Shared with `bougie start`'s team-mode self-heal ([`crate::commands::team`]),
/// which re-runs discovery against the recorded registry after a `vendor/` wipe.
pub(crate) fn fetch_repo_urls(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
) -> Result<Vec<String>> {
    let resp = client
        .get(format!("{base}/api/v1/repos"))
        .bearer_auth(token)
        .send()
        .wrap_err("requesting repository list")?;
    if !resp.status().is_success() {
        return Err(eyre!(
            "registry answered {} for /api/v1/repos",
            resp.status()
        ));
    }
    let list: RepoListResponse = resp.json().wrap_err("parsing repository list")?;
    Ok(list.repositories.into_iter().map(|r| r.url).collect())
}

#[cfg(test)]
mod tests {
    use super::{ProvisionMode, run_ci};
    use bougie_cli::OutputFormat;

    #[test]
    fn run_ci_rejects_bad_url_and_missing_repository() {
        // Bad scheme is caught before any network I/O.
        let err = run_ci(OutputFormat::Text, "ftp://x", "acme/app", "sconce", ProvisionMode::Skip)
            .unwrap_err()
            .to_string();
        assert!(err.contains("http://"), "{err}");

        // A valid URL but no repository → the `--repository` guard, also before I/O.
        let err = run_ci(OutputFormat::Text, "https://x", "", "sconce", ProvisionMode::Skip)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--repository"), "{err}");
    }
}
