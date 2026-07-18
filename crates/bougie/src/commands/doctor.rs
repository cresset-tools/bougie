//! `bougie doctor` — one command that verifies the whole dev setup, the
//! team-onboarding checkup: does the project config load, does the installed
//! PHP toolchain match the pin, are the declared services healthy, is the
//! team login still valid, and is the seeded database present (and current)?
//!
//! Read-only and side-effect free: every problem comes with the command that
//! fixes it, but doctor itself never installs, seeds, or logs in. Team checks
//! skip cleanly on a non-team project, so the same verb serves everyone. Exits
//! non-zero when any check **fails** (warnings don't), so CI/scripts can gate
//! on it.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use bougie_cli::{DoctorArgs, OutputFormat};
use bougie_composer_resolver::metadata::auth_origin;
use bougie_composer_resolver::update::read_bougie_bearer;
use bougie_config::{ProjectConfig, load_project};
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_version::request::VersionLike;
use bougie_version::version::Version;
use eyre::Result;
use serde::Serialize;

use super::db;
use super::service::config_mut::locate_project_root;
use super::team;

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned args from clap-parsed CLI"
)]
pub fn run(format: OutputFormat, args: DoctorArgs) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let project_root = locate_project_root()?;

    let project = load_project(&project_root);
    let mut checks = vec![project_check(&project)];
    checks.push(php_check(&project_root, project.as_ref().ok()));
    checks.push(services_check(&paths, project.as_ref().ok()));
    let (team_check, session) = login_check(&project_root, args.offline);
    checks.push(team_check);
    checks.push(snapshot_check(&paths, &project_root, session.as_ref(), args.offline));

    let report = DoctorReport {
        schema_version: 1,
        project_root: project_root.display().to_string(),
        failed: checks.iter().filter(|c| c.verdict == Verdict::Fail).count(),
        warned: checks.iter().filter(|c| c.verdict == Verdict::Warn).count(),
        checks,
    };
    emit(format, &report)?;
    Ok(if report.failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/// Does the project config parse? Everything else keys off it.
fn project_check(project: &Result<ProjectConfig>) -> Check {
    match project {
        Ok(p) => {
            let services = p.bougie.services.len();
            Check::ok("project", format!("config loads ({services} service(s) declared)"))
        }
        Err(e) => Check::fail("project", format!("config doesn't load: {e}"))
            .hint("fix bougie.toml / composer.json, then re-run"),
    }
}

/// Is the synced PHP toolchain present, and does it still satisfy the pin?
/// (Editing the pin after a sync leaves a stale interpreter behind.)
fn php_check(project_root: &Path, project: Option<&ProjectConfig>) -> Check {
    let Ok((version, flavor)) = bougie_fs::state::read_project_resolved(project_root) else {
        return Check::warn("php", "project not synced yet — no PHP resolved")
            .hint("run `bougie sync`");
    };
    if super::env::resolve_php_bin(project_root).is_none() {
        return Check::fail(
            "php",
            format!("resolved php {version} ({flavor}) is not installed"),
        )
        .hint("run `bougie sync`");
    }
    if let Some(project) = project {
        match super::sync::project_php_inputs(project_root, project) {
            Ok((spec, _)) if !resolved_matches_spec(&version, &spec) => {
                return Check::warn(
                    "php",
                    format!("installed php {version} no longer satisfies the project's PHP pin"),
                )
                .hint("run `bougie sync` to re-resolve");
            }
            _ => {}
        }
    }
    Check::ok("php", format!("php {version} ({flavor}) installed, matches the pin"))
}

/// Does the resolved `major.minor.patch` still satisfy the pinned spec?
/// Unparseable input degrades to "matches" — doctor should never false-alarm
/// on its own parsing.
fn resolved_matches_spec(resolved: &str, spec: &VersionLike) -> bool {
    let Ok(v) = resolved.parse::<Version>() else {
        return true;
    };
    match spec {
        VersionLike::Version(pv) => {
            v.major == pv.major
                && pv.minor.is_none_or(|m| v.minor == m)
                && pv.patch.is_none_or(|p| v.patch == p)
        }
        VersionLike::Constraint(c) => composer_semver::Version::parse(&v.to_string())
            .map(|cv| c.matches(&cv))
            .unwrap_or(true),
    }
}

/// Are the declared services running (per the live daemon)?
fn services_check(paths: &Paths, project: Option<&ProjectConfig>) -> Check {
    use super::service::client;

    #[derive(Debug, serde::Deserialize)]
    struct StatusReply {
        services: Vec<serde_json::Value>,
    }

    let declared: Vec<String> = project
        .map(|p| p.bougie.services.keys().cloned().collect())
        .unwrap_or_default();
    if declared.is_empty() {
        return Check::skip("services", "none declared");
    }

    // One read-only status round-trip, only if bougied is already up.
    let Some(reply) = client::try_call::<StatusReply>(paths, "status", serde_json::Value::Null)
    else {
        return Check::warn(
            "services",
            format!("bougied isn't running ({} service(s) declared)", declared.len()),
        )
        .hint("run `bougie up`");
    };
    let mut bad = Vec::new();
    for name in &declared {
        let state = reply
            .services
            .iter()
            .find(|v| v.get("name").and_then(serde_json::Value::as_str) == Some(name))
            .and_then(|v| v.get("state"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("not started");
        if !matches!(state, "running" | "health_checking" | "starting") {
            bad.push(format!("{name} ({state})"));
        }
    }
    if bad.is_empty() {
        Check::ok("services", format!("{} service(s) running", declared.len()))
    } else {
        Check::fail("services", format!("not healthy: {}", bad.join(", ")))
            .hint("run `bougie up` (then `bougie service logs <name>` if it fails again)")
    }
}

/// The registry credentials a valid login yields — carried into the snapshot
/// check so it doesn't re-resolve (or re-report) login problems.
struct Session {
    base: String,
    token: String,
}

/// Is the project wired to a team registry, and is the login token still
/// accepted? Freshness can only be proven by an authenticated round-trip (the
/// local store holds no expiry), so `--offline` downgrades to a presence check.
fn login_check(project_root: &Path, offline: bool) -> (Check, Option<Session>) {
    let Some(record) = team::read_record(project_root) else {
        return (Check::skip("team", "not a team project"), None);
    };
    let base = record.registry.trim_end_matches('/').to_string();
    let host = auth_origin(&base);
    let Some(token) = read_bougie_bearer(&host) else {
        return (
            Check::fail("team", format!("logged out of {host}"))
                .hint(format!("run `bougie login {base}`")),
            None,
        );
    };
    let session = Session { base: base.clone(), token };
    if offline {
        return (
            Check::ok("team", format!("login recorded for {host} (freshness not checked offline)")),
            Some(session),
        );
    }
    // A cheap authenticated GET: 2xx proves the token is still accepted.
    match authed_get(&format!("{base}/api/v1/repos"), &session.token) {
        Ok(status) if status.is_success() => (
            Check::ok("team", format!("logged in to {host} — token valid")),
            Some(session),
        ),
        Ok(status)
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN =>
        {
            (
                Check::fail("team", format!("the registry rejected the login token ({status})"))
                    .hint(format!("run `bougie login {base}`")),
                None,
            )
        }
        Ok(status) => (
            Check::warn("team", format!("the registry answered {status}")),
            Some(session),
        ),
        Err(e) => (
            Check::warn("team", format!("registry unreachable ({e:#})")),
            // Keep the session: the snapshot check will skip its own
            // round-trip but can still report local seed state.
            Some(session),
        ),
    }
}

fn authed_get(url: &str, token: &str) -> Result<reqwest::StatusCode> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("bougie/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .build()?;
    Ok(client.get(url).bearer_auth(token).send()?.status())
}

/// Is a production-shaped database seeded, and is it current? Reuses `db
/// status`'s machinery: the seed marker locally, the registry's
/// `latest/info` for the behind check.
fn snapshot_check(
    paths: &Paths,
    project_root: &Path,
    session: Option<&Session>,
    offline: bool,
) -> Check {
    let Some(snap) = team::cached_snapshot_ref(project_root) else {
        return Check::skip("snapshot", "no team snapshot source configured");
    };
    let marker = db::seed::read_seed_marker(&db::seed::seed_marker_path(paths, project_root));
    let Some(marker) = marker else {
        return Check::warn("snapshot", "database not seeded yet")
            .hint("run `bougie db seed` (or `bougie start`)");
    };
    let age = db::seed::human_age(db::seed::now_unix().saturating_sub(marker.seeded_at_unix));

    let staleness = if offline || session.is_none() {
        None
    } else if let Some(session) = session {
        let env = snap.env.as_deref().unwrap_or("production");
        let profile = snap.profile.as_deref().unwrap_or("full");
        db::status::fetch_latest_info(&session.base, &session.token, &snap.repo, env, profile)
            .ok()
            .flatten()
    } else {
        None
    };
    match (staleness, marker.digest.as_deref()) {
        (Some(info), Some(d)) if d == info.digest => {
            Check::ok("snapshot", format!("seeded {age}, up to date with the registry"))
        }
        (Some(info), Some(_)) => Check::warn(
            "snapshot",
            format!(
                "behind: the registry's latest was published {}",
                db::seed::human_age(
                    db::seed::now_unix().saturating_sub(info.created_at.max(0).unsigned_abs())
                )
            ),
        )
        .hint("run `bougie db refresh` (replaces local data)"),
        _ => Check::ok("snapshot", format!("seeded {age} (staleness not checked)")),
    }
}

// ---------------------------------------------------------------------------
// Report model + rendering
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl Verdict {
    /// The fixed-width text label, bracket-aligned in the report.
    fn label(self) -> &'static str {
        match self {
            Verdict::Ok => "[ ok ]",
            Verdict::Warn => "[warn]",
            Verdict::Fail => "[FAIL]",
            Verdict::Skip => "[skip]",
        }
    }
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    verdict: Verdict,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Verdict::Ok, detail)
    }
    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Verdict::Warn, detail)
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Verdict::Fail, detail)
    }
    fn skip(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Verdict::Skip, detail)
    }
    fn new(name: &'static str, verdict: Verdict, detail: impl Into<String>) -> Self {
        Check { name, verdict, detail: detail.into(), hint: None }
    }
    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    schema_version: u32,
    project_root: String,
    checks: Vec<Check>,
    warned: usize,
    failed: usize,
}

impl Render for DoctorReport {
    fn render_text(&self, w: &mut dyn Write) -> std::io::Result<()> {
        writeln!(w, "bougie doctor: {}", self.project_root)?;
        for c in &self.checks {
            writeln!(w, "  {} {:<9} {}", c.verdict.label(), c.name, c.detail)?;
            if let Some(hint) = &c.hint {
                writeln!(w, "         {:<9} → {hint}", "")?;
            }
        }
        let ok = self.checks.len() - self.warned - self.failed
            - self.checks.iter().filter(|c| c.verdict == Verdict::Skip).count();
        writeln!(
            w,
            "{ok} ok, {} warning(s), {} failure(s)",
            self.warned, self.failed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(report: &DoctorReport) -> String {
        let mut buf = Vec::new();
        report.render_text(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn report_renders_verdicts_hints_and_summary() {
        let report = DoctorReport {
            schema_version: 1,
            project_root: "/p".to_string(),
            checks: vec![
                Check::ok("project", "config loads"),
                Check::warn("php", "not synced yet").hint("run `bougie sync`"),
                Check::fail("team", "logged out").hint("run `bougie login`"),
                Check::skip("snapshot", "not a team project"),
            ],
            warned: 1,
            failed: 1,
        };
        let text = render(&report);
        assert!(text.contains("[ ok ] project"), "{text}");
        assert!(text.contains("[warn] php"), "{text}");
        assert!(text.contains("[FAIL] team"), "{text}");
        assert!(text.contains("[skip] snapshot"), "{text}");
        assert!(text.contains("→ run `bougie sync`"), "{text}");
        assert!(text.contains("1 ok, 1 warning(s), 1 failure(s)"), "{text}");
    }

    #[test]
    fn resolved_version_matches_partial_and_constraint_specs() {
        use bougie_version::request::{Request, parse_request};
        let spec = |s: &str| match parse_request(s).unwrap() {
            Request::VersionLike { spec, .. } => spec,
            _ => panic!("not a version-like request"),
        };
        // Partial versions: prefix match.
        assert!(resolved_matches_spec("8.4.10", &spec("8.4")));
        assert!(!resolved_matches_spec("8.3.9", &spec("8.4")));
        // Constraints: composer semantics.
        assert!(resolved_matches_spec("8.4.10", &spec("^8.3")));
        assert!(!resolved_matches_spec("7.4.33", &spec("^8.3")));
        // Unparseable resolved version → never false-alarm.
        assert!(resolved_matches_spec("garbage", &spec("8.4")));
    }
}
