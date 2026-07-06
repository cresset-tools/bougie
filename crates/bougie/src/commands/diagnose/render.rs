//! Markdown rendering. The Markdown IS the payload: what this module
//! produces (post-editor) uploads verbatim as `report_md`, so the
//! text form and the wire form can never disagree.

use super::collect::{DiagnoseReport, LOG_TAIL_LINES};
use bougie_output::output::Render;
use std::fmt::Write as _;
use std::io::{self, Write};

/// Log excerpts are fenced with four backticks so a log line that
/// itself contains a triple-backtick fence can't break out.
const FENCE: &str = "````";

pub fn to_markdown(report: &DiagnoseReport) -> String {
    let mut md = String::new();
    let _ = write_markdown(&mut md, report);
    md
}

impl Render for DiagnoseReport {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        w.write_all(to_markdown(self).as_bytes())
    }
}

#[allow(clippy::too_many_lines)]
fn write_markdown(md: &mut String, r: &DiagnoseReport) -> std::fmt::Result {
    writeln!(md, "# bougie diagnostic report")?;

    writeln!(md)?;
    writeln!(md, "## your notes")?;
    writeln!(md)?;
    writeln!(
        md,
        "_(describe what you were doing — anything you type here is sent along)_"
    )?;

    writeln!(md)?;
    writeln!(md, "## environment")?;
    writeln!(md)?;
    writeln!(
        md,
        "bougie {} ({}) on {}-{} ({})",
        r.bougie_version,
        r.build_sha.unwrap_or("no build sha"),
        r.os,
        r.arch,
        r.libc,
    )?;
    writeln!(md, "telemetry mode: {}", r.telemetry_mode)?;
    if r.bougie_env_names.is_empty() {
        writeln!(md, "BOUGIE_* env: none")?;
    } else {
        writeln!(
            md,
            "BOUGIE_* env set (names only): {}",
            r.bougie_env_names.join(", ")
        )?;
    }
    if let Some(p) = &r.project {
        writeln!(md, "project: {}", p.root)?;
        if p.declared_services.is_empty() {
            writeln!(md, "declared services: none")?;
        } else {
            let list: Vec<String> = p
                .declared_services
                .iter()
                .map(|s| format!("{} {}", s.name, s.pin))
                .collect();
            writeln!(md, "declared services: {}", list.join(", "))?;
        }
        if let Some(err) = &p.config_error {
            writeln!(md, "project config error: {err}")?;
        }
        match (&p.disk_free_home, &p.disk_free_cache) {
            (Some(h), Some(c)) => writeln!(md, "disk free: {h} ($BOUGIE_HOME), {c} (cache)")?,
            (Some(h), None) => writeln!(md, "disk free: {h} ($BOUGIE_HOME)")?,
            (None, Some(c)) => writeln!(md, "disk free: {c} (cache)")?,
            (None, None) => {}
        }
    }

    if let Some(f) = &r.failure {
        writeln!(md)?;
        writeln!(md, "## last failure")?;
        writeln!(md)?;
        writeln!(md, "command:   {}", f.argv.join(" "))?;
        writeln!(md, "category:  {} (exit {})", f.category, f.exit_code)?;
        for (i, msg) in f.chain.iter().enumerate() {
            if i == 0 {
                writeln!(md, "error:     {msg}")?;
            } else {
                writeln!(md, "caused by: {msg}")?;
            }
        }
    }

    if let Some(rr) = &r.rerun {
        writeln!(md)?;
        writeln!(md, "## re-run with debug logging")?;
        writeln!(md)?;
        writeln!(md, "command: {}", rr.argv.join(" "))?;
        writeln!(
            md,
            "exit:    {}",
            rr.exit_code.map_or("killed".into(), |c| c.to_string())
        )?;
        writeln!(md, "stderr (tail):")?;
        writeln!(md, "{FENCE}")?;
        for line in rr.stderr_tail.lines() {
            writeln!(md, "{line}")?;
        }
        writeln!(md, "{FENCE}")?;
    }

    if let Some(d) = &r.daemon {
        writeln!(md)?;
        writeln!(md, "## daemon")?;
        writeln!(md)?;
        writeln!(
            md,
            "bougied: {}",
            if d.running { "running" } else { "not running" }
        )?;
        write_log(md, "bougied.log", &d.log_tail)?;
    }

    if !r.services.is_empty() {
        writeln!(md)?;
        writeln!(md, "## services")?;
        for s in &r.services {
            writeln!(md)?;
            let heading = match (&s.state, &s.status_note) {
                (Some(state), Some(note)) => format!(" — {state} ({note})"),
                (Some(state), None) => format!(" — {state}"),
                (None, _) => " — state unknown (daemon not running)".to_owned(),
            };
            writeln!(md, "### {} (declared: {}){heading}", s.name, s.pin)?;
            writeln!(md)?;
            writeln!(md, "binding: {}", s.binding)?;
            write_sublog(md, &format!("{}.log", s.name), &s.log_tail)?;
        }
    }

    if let Some(srv) = &r.server {
        writeln!(md)?;
        writeln!(md, "## server")?;
        writeln!(md)?;
        writeln!(md, "host: {}", srv.host)?;
        write_log(md, "server.log (filtered to this host)", &srv.log_tail)?;
    }

    if !r.ports.is_empty() {
        writeln!(md)?;
        writeln!(md, "## ports")?;
        writeln!(md)?;
        writeln!(md, "| port | wanted by | probe | holder (best effort) |")?;
        writeln!(md, "|------|-----------|-------|----------------------|")?;
        for p in &r.ports {
            let wanted = match p.purpose {
                Some(purpose) => format!("{} ({purpose})", p.service),
                None => p.service.clone(),
            };
            let probe = match (p.in_use, p.expected) {
                (false, _) => "free",
                (true, true) => "in use (this service)",
                (true, false) => "**in use — conflict**",
            };
            let holder = p.holder.as_deref().unwrap_or("-");
            writeln!(md, "| {} | {wanted} | {probe} | {holder} |", p.port)?;
        }
    }
    Ok(())
}

fn write_log(md: &mut String, title: &str, lines: &[String]) -> std::fmt::Result {
    if lines.is_empty() {
        return Ok(());
    }
    writeln!(md)?;
    writeln!(md, "### {title} (last {LOG_TAIL_LINES} lines)")?;
    write_fenced(md, lines)
}

fn write_sublog(md: &mut String, title: &str, lines: &[String]) -> std::fmt::Result {
    writeln!(md)?;
    if lines.is_empty() {
        writeln!(md, "#### {title}: no log file")?;
        return Ok(());
    }
    writeln!(md, "#### {title} (last {LOG_TAIL_LINES} lines)")?;
    write_fenced(md, lines)
}

fn write_fenced(md: &mut String, lines: &[String]) -> std::fmt::Result {
    writeln!(md)?;
    writeln!(md, "{FENCE}log")?;
    for line in lines {
        writeln!(md, "{line}")?;
    }
    writeln!(md, "{FENCE}")?;
    Ok(())
}
