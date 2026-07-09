//! `bougie login <URL>` — authenticate against a sconce Composer registry via
//! the OAuth 2.0 device authorization grant (RFC 8628).
//!
//! The CLI starts a flow, prints a short user code and opens the dashboard, then
//! polls until a signed-in team member approves it in the browser. On approval
//! the registry mints an **org-scoped read token** — one credential that
//! authenticates every repository served under that host — which we persist to
//! bougie's own credential store keyed by origin (`host[:port]`), exactly where
//! the resolver looks for it. No browser callback, no local listener: the CLI
//! only ever makes outbound requests, so it works over SSH and in containers.

use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

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

/// Structured result of a successful login.
#[derive(Debug, Serialize)]
struct LoginResult {
    schema_version: u32,
    /// The origin the token was stored against (`host[:port]`).
    host: String,
    /// Path of the credential store written.
    stored_at: String,
}

impl Render for LoginResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "Signed in to {}. Token stored in {}.",
            self.host, self.stored_at
        )
    }
}

pub fn run(format: OutputFormat, url: &str) -> Result<ExitCode> {
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
    loop {
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
            let token = body
                .get("access_token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| eyre!("login approved but no token was returned"))?;
            let path = bougie_composer_resolver::update::write_bougie_bearer(&host, token)
                .wrap_err("storing the login token")?;
            let result = LoginResult {
                schema_version: 1,
                host,
                stored_at: path.display().to_string(),
            };
            emit(format, &result)?;
            return Ok(ExitCode::SUCCESS);
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
    }
}
