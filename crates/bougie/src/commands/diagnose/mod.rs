//! `bougie diagnose` — user-initiated diagnostic reports.
//!
//! Deliberately *not* telemetry: independent of the consent mode and
//! `DO_NOT_TRACK` (a user deliberately mailing a report is
//! correspondence, not tracking), never triggered by anything but
//! this verb, and every send requires explicit confirmation —
//! interactive `[y/N]` defaulting to **no**, or the `--yes` flag.
//!
//! The report keeps error messages, package names, and service log
//! tails (that detail is the point); the safeguards are review and
//! scrubbing. Interactively the review is an `$EDITOR` pass over the
//! exact Markdown that ships — what the user saves is byte-for-byte
//! the payload (`report_md` in the schema-2 envelope), so an
//! in-editor redaction is authoritative. Before the draft is even
//! shown, known credentials (tenant secrets, composer auth) are
//! replaced and the home directory folded to `~` (see `scrub`).
//!
//! Collection is offline-first and never mutates: on-disk logs,
//! ledgers, and config are read directly; the daemon socket is used
//! only when `bougied` is already running (`client::try_call` — no
//! autospawn).

mod collect;
mod editor;
mod render;
mod scrub;

use crate::failure::{self, LastFailure};
use bougie_cli::OutputFormat;
use bougie_output::output::emit;
use bougie_paths::Paths;
use collect::RerunCapture;
use eyre::{Result, WrapErr};
use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Default diagnose endpoint; `BOUGIE_DIAGNOSE_URL` overrides.
pub const DEFAULT_ENDPOINT: &str = "https://telemetry.bougie.tools/v1/diagnose";

const NEW_ISSUE_URL: &str = "https://github.com/cresset-tools/bougie/issues/new";

/// Keep only the last 64 KiB of a re-run's stderr.
const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Hard cap on the rendered report. The per-log budgets keep a
/// realistic report far below this; the cap only bites on a
/// pathological failure chain. Also comfortably inside the
/// collector's diagnose body limit.
const REPORT_CAP_BYTES: usize = 384 * 1024;

/// GitHub rejects issue bodies past this; `--issue` hints at
/// attaching the file instead of pasting when the report is bigger.
const GITHUB_ISSUE_BODY_LIMIT: usize = 65_536;

/// File `--issue` writes into the current directory.
const ISSUE_FILE: &str = "bougie-diagnose.md";

// Mirrors the clap surface one-to-one; the flag set is the API.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct DiagnoseArgs {
    pub issue: bool,
    pub yes: bool,
    pub edit: bool,
    pub no_edit: bool,
    pub project: Option<PathBuf>,
    pub args: Vec<OsString>,
}

pub fn run(format: OutputFormat, args: DiagnoseArgs) -> Result<ExitCode> {
    let DiagnoseArgs {
        issue,
        yes,
        edit,
        no_edit,
        project,
        args,
    } = args;
    let paths = Paths::from_env()?;

    let rerun = if args.is_empty() {
        None
    } else {
        Some(rerun_capture(&args)?)
    };
    let failure = failure::load(paths.cache());
    if failure.is_none() && rerun.is_none() {
        eprintln!(
            "nothing to report yet: run the failing command first, or reproduce one now with\n\
             `bougie diagnose -- <bougie args>`"
        );
        return Ok(ExitCode::FAILURE);
    }

    let project_root = resolve_project_root(project.as_deref(), failure.as_ref());
    let scrubber = scrub::Scrubber::from_env(&paths, project_root.as_deref());
    let report = collect::collect(&paths, failure, rerun, project_root.as_deref(), &scrubber);
    let mut markdown = render::to_markdown(&report);
    cap_report(&mut markdown);

    let interactive = matches!(format, OutputFormat::Text)
        && io::stdin().is_terminal()
        && io::stdout().is_terminal();
    let use_editor = !no_edit && (edit || (interactive && !yes));

    let mut draft: Option<editor::Draft> = None;
    if use_editor {
        let Some((edited, kept)) = editor::edit(&paths, &markdown)? else {
            eprintln!("empty report — nothing sent.");
            return Ok(ExitCode::SUCCESS);
        };
        markdown = edited;
        draft = Some(kept);
    } else {
        // No editor pass → the terminal print is the review: the full
        // payload, exactly as it would be sent.
        emit(format, &report)?;
    }

    if issue {
        let out = Path::new(ISSUE_FILE);
        std::fs::write(out, &markdown).wrap_err_with(|| format!("writing {}", out.display()))?;
        if let Some(d) = draft {
            d.discard();
        }
        eprintln!();
        eprintln!("report written to {}", out.display());
        if markdown.len() > GITHUB_ISSUE_BODY_LIMIT {
            eprintln!(
                "(it exceeds GitHub's issue-body limit — attach the file to the issue instead \
                 of pasting it)"
            );
        }
        eprintln!("open a new issue: {NEW_ISSUE_URL}");
        return Ok(ExitCode::SUCCESS);
    }

    let proceed = if yes {
        true
    } else if interactive {
        confirm_send(&markdown)?
    } else {
        eprintln!(
            "not sending: no terminal for confirmation. Re-run with --yes to upload, or --issue \
             to prepare a GitHub issue instead."
        );
        return Ok(ExitCode::FAILURE);
    };
    if !proceed {
        eprintln!("not sent. (Use --issue for the GitHub route.)");
        if let Some(d) = &draft {
            eprintln!("your edited draft is kept at {}", d.path().display());
        }
        return Ok(ExitCode::SUCCESS);
    }

    match upload(&markdown) {
        Ok(id) => {
            if let Some(d) = draft {
                d.discard();
            }
            eprintln!("report uploaded: {id}");
            eprintln!("referencing it in an issue helps: {NEW_ISSUE_URL}");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            if let Some(d) = &draft {
                eprintln!(
                    "upload failed; your edited draft is kept at {}",
                    d.path().display()
                );
            }
            Err(e)
        }
    }
}

/// The `[y/N]` gate, still defaulting to **no** — an editor save is a
/// review, not a consent.
fn confirm_send(markdown: &str) -> Result<bool> {
    let kib = markdown.len().div_ceil(1024);
    let lines = markdown.lines().count();
    eprintln!();
    eprint!("report is {kib} KiB ({lines} lines) — send to the bougie developers? [y/N] ");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line).map_err(|e| eyre::eyre!("reading confirmation: {e}"))?;
    let ans = line.trim().to_ascii_lowercase();
    Ok(ans == "y" || ans == "yes")
}

/// The project the report is about: `--project` wins, then the
/// project around the cwd, then the one recorded with the last
/// failure (schema-2 `project_dir`).
fn resolve_project_root(flag: Option<&Path>, failure: Option<&LastFailure>) -> Option<PathBuf> {
    if let Some(p) = flag {
        return Some(std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
    }
    std::env::current_dir()
        .ok()
        .and_then(|cwd| failure::project_root_near(&cwd))
        .or_else(|| failure.and_then(|f| f.project_dir.clone()))
}

/// Truncate a pathologically large report at a char boundary, with a
/// visible marker. The section budgets make this a backstop, not a
/// working path.
fn cap_report(markdown: &mut String) {
    if markdown.len() <= REPORT_CAP_BYTES {
        return;
    }
    let mut cut = REPORT_CAP_BYTES;
    while !markdown.is_char_boundary(cut) {
        cut -= 1;
    }
    markdown.truncate(cut);
    markdown.push_str("\n\n_(report truncated at 384 KiB)_\n");
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

/// Schema-2 upload envelope. `report_md` — the user-reviewed (and
/// possibly user-edited) Markdown — is the report; the envelope
/// fields are fixed machine facts of the same sensitivity class as
/// the User-Agent, each also visible inside the report text. Nothing
/// else structured, deliberately: a structured copy of report content
/// would survive an in-editor redaction.
#[derive(Debug, serde::Serialize)]
struct UploadEnvelope<'a> {
    schema_version: u32,
    bougie_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_sha: Option<&'static str>,
    os: &'static str,
    arch: &'static str,
    libc: &'static str,
    report_md: &'a str,
}

fn upload(markdown: &str) -> Result<String> {
    let envelope = UploadEnvelope {
        schema_version: 2,
        bougie_version: env!("CARGO_PKG_VERSION"),
        build_sha: bougie_cli::BUILD_SHA,
        os: bougie_telemetry::event::os(),
        arch: bougie_telemetry::event::arch(),
        libc: bougie_telemetry::event::libc(),
        report_md: markdown,
    };
    let url = std::env::var("BOUGIE_DIAGNOSE_URL").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_owned());
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(format!("bougie/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .wrap_err("building http client")?;
    let response = client
        .post(&url)
        .json(&envelope)
        .send()
        .wrap_err("uploading diagnostic report")?;
    if !response.status().is_success() {
        return Err(eyre::eyre!(
            "collector answered {} for {url}",
            response.status()
        ));
    }
    // `{"id": "diag-…"}` on success; tolerate anything else.
    let id = response
        .json::<serde_json::Value>()
        .ok()
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "(no id returned)".to_owned());
    Ok(id)
}
