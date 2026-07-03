//! `bougie diagnose` — user-initiated diagnostic reports.
//!
//! Deliberately *not* telemetry: independent of the consent mode and
//! `DO_NOT_TRACK` (a user deliberately mailing a report is
//! correspondence, not tracking), never triggered by anything but
//! this verb, and every send requires explicit confirmation —
//! interactive `[y/N]` defaulting to **no**, or the `--yes` flag.
//!
//! The report keeps error messages and package names (that detail is
//! the point); the safeguard is review — the full payload is printed
//! before the question — plus a courtesy pass that folds the home
//! directory into `~`.

use crate::failure::{self, LastFailure};
use bougie_cli::OutputFormat;
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Write};
use std::process::ExitCode;

/// Default diagnose endpoint; `BOUGIE_DIAGNOSE_URL` overrides.
pub const DEFAULT_ENDPOINT: &str = "https://telemetry.bougie.tools/v1/diagnose";

const NEW_ISSUE_URL: &str = "https://github.com/cresset-tools/bougie/issues/new";

/// Keep only the last 64 KiB of a re-run's stderr.
const STDERR_TAIL_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize)]
struct RerunCapture {
    argv: Vec<String>,
    exit_code: Option<i32>,
    stderr_tail: String,
}

#[derive(Debug, Serialize)]
struct DiagnoseReport {
    schema_version: u32,
    bougie_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_sha: Option<&'static str>,
    os: &'static str,
    arch: &'static str,
    libc: &'static str,
    telemetry_mode: &'static str,
    /// *Names* of the BOUGIE_* variables set — never their values.
    bougie_env_names: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<LastFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rerun: Option<RerunCapture>,
}

impl Render for DiagnoseReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "# bougie diagnostic report")?;
        writeln!(w)?;
        writeln!(
            w,
            "bougie {} ({}) on {}-{} ({})",
            self.bougie_version,
            self.build_sha.unwrap_or("no build sha"),
            self.os,
            self.arch,
            self.libc,
        )?;
        writeln!(w, "telemetry mode: {}", self.telemetry_mode)?;
        if self.bougie_env_names.is_empty() {
            writeln!(w, "BOUGIE_* env: none")?;
        } else {
            writeln!(w, "BOUGIE_* env set (names only): {}", self.bougie_env_names.join(", "))?;
        }
        if let Some(f) = &self.failure {
            writeln!(w)?;
            writeln!(w, "## last failure")?;
            writeln!(w)?;
            writeln!(w, "command:   {}", f.argv.join(" "))?;
            writeln!(w, "category:  {} (exit {})", f.category, f.exit_code)?;
            for (i, msg) in f.chain.iter().enumerate() {
                if i == 0 {
                    writeln!(w, "error:     {msg}")?;
                } else {
                    writeln!(w, "caused by: {msg}")?;
                }
            }
        }
        if let Some(r) = &self.rerun {
            writeln!(w)?;
            writeln!(w, "## re-run with debug logging")?;
            writeln!(w)?;
            writeln!(w, "command: {}", r.argv.join(" "))?;
            writeln!(w, "exit:    {}", r.exit_code.map_or("killed".into(), |c| c.to_string()))?;
            writeln!(w, "stderr (tail):")?;
            writeln!(w, "```")?;
            for line in r.stderr_tail.lines() {
                writeln!(w, "{line}")?;
            }
            writeln!(w, "```")?;
        }
        Ok(())
    }
}

pub fn run(
    format: OutputFormat,
    issue: bool,
    yes: bool,
    args: &[OsString],
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;

    let rerun = if args.is_empty() { None } else { Some(rerun_capture(args)?) };
    let failure = failure::load(paths.cache());
    if failure.is_none() && rerun.is_none() {
        eprintln!(
            "nothing to report yet: run the failing command first, or reproduce one now with\n\
             `bougie diagnose -- <bougie args>`"
        );
        return Ok(ExitCode::FAILURE);
    }

    let report = build_report(failure, rerun);

    // Review comes first, always: the full payload, exactly as it
    // would be sent.
    emit(format, &report)?;

    if issue {
        eprintln!();
        eprintln!("paste the report above into a new issue: {NEW_ISSUE_URL}");
        return Ok(ExitCode::SUCCESS);
    }

    let interactive =
        matches!(format, OutputFormat::Text) && std::io::stdin().is_terminal();
    let proceed = if yes {
        true
    } else if interactive {
        eprintln!();
        eprint!("Send this report to the bougie developers? [y/N] ");
        io::stderr().flush().ok();
        let mut line = String::new();
        io::stdin().read_line(&mut line).map_err(|e| eyre::eyre!("reading confirmation: {e}"))?;
        let ans = line.trim().to_ascii_lowercase();
        ans == "y" || ans == "yes"
    } else {
        eprintln!(
            "not sending: no terminal for confirmation. Re-run with --yes to upload, or --issue \
             to prepare a GitHub issue instead."
        );
        return Ok(ExitCode::FAILURE);
    };
    if !proceed {
        eprintln!("not sent. (Use --issue for the GitHub route.)");
        return Ok(ExitCode::SUCCESS);
    }

    let id = upload(&report)?;
    eprintln!("report uploaded: {id}");
    eprintln!("referencing it in an issue helps: {NEW_ISSUE_URL}");
    Ok(ExitCode::SUCCESS)
}

fn build_report(failure: Option<LastFailure>, rerun: Option<RerunCapture>) -> DiagnoseReport {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty());
    let tilde = |s: &str| match &home {
        Some(h) => s.replace(h.as_str(), "~"),
        None => s.to_owned(),
    };
    let failure = failure.map(|mut f| {
        f.argv = f.argv.iter().map(|a| tilde(a)).collect();
        f.chain = f.chain.iter().map(|c| tilde(c)).collect();
        f
    });
    let rerun = rerun.map(|mut r| {
        r.argv = r.argv.iter().map(|a| tilde(a)).collect();
        r.stderr_tail = tilde(&r.stderr_tail);
        r
    });
    let mode_file = bougie_paths::telemetry_mode_file().ok();
    let mut env_names: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("BOUGIE_"))
        .collect();
    env_names.sort();
    DiagnoseReport {
        schema_version: 1,
        bougie_version: env!("CARGO_PKG_VERSION"),
        build_sha: bougie_cli::BUILD_SHA,
        os: bougie_telemetry::event::os(),
        arch: bougie_telemetry::event::arch(),
        libc: bougie_telemetry::event::libc(),
        telemetry_mode: bougie_telemetry::mode::resolve_from_env(mode_file.as_deref())
            .mode
            .as_str(),
        bougie_env_names: env_names,
        failure,
        rerun,
    }
}

/// Re-run a bougie command with debug logging and capture its stderr
/// tail. The child runs to completion — the user asked for this
/// reproduction explicitly.
fn rerun_capture(args: &[OsString]) -> Result<RerunCapture> {
    let exe = std::env::current_exe().wrap_err("locating current bougie binary")?;
    eprintln!("re-running with BOUGIE_LOG=debug …");
    let output = std::process::Command::new(&exe)
        .args(args)
        .env("BOUGIE_LOG", "debug")
        .stdin(std::process::Stdio::null())
        .output()
        .wrap_err("re-running bougie for diagnosis")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail_start = stderr.len().saturating_sub(STDERR_TAIL_BYTES);
    // Snap to a char boundary going forward.
    let tail = stderr
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= tail_start)
        .map_or("", |i| &stderr[i..]);
    let mut command_line = vec!["bougie".to_owned()];
    command_line.extend(args.iter().map(|a| a.to_string_lossy().into_owned()));
    Ok(RerunCapture {
        argv: command_line,
        exit_code: output.status.code(),
        stderr_tail: tail.to_owned(),
    })
}

fn upload(report: &DiagnoseReport) -> Result<String> {
    let url = std::env::var("BOUGIE_DIAGNOSE_URL")
        .unwrap_or_else(|_| DEFAULT_ENDPOINT.to_owned());
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(format!("bougie/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .wrap_err("building http client")?;
    let response = client
        .post(&url)
        .json(report)
        .send()
        .wrap_err("uploading diagnostic report")?;
    if !response.status().is_success() {
        return Err(eyre::eyre!("collector answered {} for {url}", response.status()));
    }
    // `{"id": "diag-…"}` on success; tolerate anything else.
    let id = response
        .json::<serde_json::Value>()
        .ok()
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "(no id returned)".to_owned());
    Ok(id)
}
