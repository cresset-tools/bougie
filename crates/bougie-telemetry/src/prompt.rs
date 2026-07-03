//! First-run consent prompt (the in-binary fallback surface).
//!
//! The installer consent block covers `curl | sh`; this covers every
//! other channel (`cargo install`, docker, direct downloads, Windows).
//! It appears only when *nothing* has decided the mode — never over an
//! explicit setting, `DO_NOT_TRACK`, CI, or a non-tty — and at most
//! [`MAX_ATTEMPTS`] times, after which the mode is written `off` and
//! the question never returns.

use crate::clock::UtcHour;
use crate::ids;
use crate::mode::{self, Mode};
use std::fs;
use std::io::{IsTerminal as _, Write as _};
use std::path::Path;

/// Skipped/EOF prompt attempts before we stop asking forever.
pub const MAX_ATTEMPTS: u32 = 3;

/// Everything the prompt decision depends on, gathered by the caller
/// so the rule itself stays table-testable.
#[allow(
    clippy::struct_excessive_bools,
    reason = "a bag of independent yes/no gates is exactly what this is; a state machine would obscure the table-testable rule"
)]
#[derive(Debug, Clone, Copy)]
pub struct Gates {
    /// Text-mode, not `--quiet` (json consumers never see prompts).
    pub interactive_format: bool,
    /// Inside a `bougie run` shim context (`BOUGIE_PROJECT_ROOT` set).
    pub in_run_shim: bool,
    pub ci: bool,
    pub stdin_tty: bool,
    pub stderr_tty: bool,
    /// Mode source is unset (no env, no DNT, no valid mode file).
    pub prompt_eligible: bool,
    pub attempts: u32,
}

pub fn should_prompt(g: Gates) -> bool {
    g.interactive_format
        && !g.in_run_shim
        && !g.ci
        && g.stdin_tty
        && g.stderr_tty
        && g.prompt_eligible
        && g.attempts < MAX_ATTEMPTS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    /// EOF, interrupt, or an unclassifiable reply: no consent recorded,
    /// attempt counter bumped.
    Skipped,
}

/// Enter means yes (`[Y/n]`); anything starting with `n` is no;
/// anything else (including EOF, `None`) is a skip, never a consent.
pub fn classify_answer(line: Option<&str>) -> Answer {
    match line {
        Some(raw) => {
            let ans = raw.trim().to_ascii_lowercase();
            if ans.is_empty() || ans.starts_with('y') {
                Answer::Yes
            } else if ans.starts_with('n') {
                Answer::No
            } else {
                Answer::Skipped
            }
        }
        None => Answer::Skipped,
    }
}

/// The disclosure block, shared verbatim with the installer snippets
/// and `telemetry on` — one wording everywhere.
pub const DISCLOSURE: &str = "\
bougie can send anonymous usage statistics and crash reports to the
bougie developers. This never includes project names, package names,
paths, or IP addresses, and nothing is sent without your consent.
Details + full field list: https://bougie.tools/telemetry";

/// Show the consent prompt when every gate passes; record the answer.
/// All I/O is best-effort — a broken terminal must not break the
/// command that triggered the prompt.
pub fn maybe_prompt(interactive_format: bool) {
    let Ok(config_dir) = bougie_paths::config_dir() else { return };
    let mode_file = config_dir.join("telemetry");
    let state = mode::resolve_from_env(Some(&mode_file));
    let gates = Gates {
        interactive_format,
        in_run_shim: std::env::var_os("BOUGIE_PROJECT_ROOT").is_some(),
        ci: mode::is_ci(),
        stdin_tty: std::io::stdin().is_terminal(),
        stderr_tty: std::io::stderr().is_terminal(),
        prompt_eligible: state.prompt_eligible(),
        attempts: read_attempts(&config_dir),
    };
    if !should_prompt(gates) {
        return;
    }

    eprintln!();
    eprintln!("{DISCLOSURE}");
    eprintln!();
    eprint!("  Enable anonymous telemetry? [Y/n] ");
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    let read = match std::io::stdin().read_line(&mut line) {
        Ok(n) if n > 0 => Some(line.as_str()),
        _ => None,
    };
    let date = UtcHour::now().date();
    match classify_answer(read) {
        Answer::Yes => {
            if mode::write_file(&mode_file, Mode::On, &date).is_ok() {
                let _ = ids::read_or_mint(&config_dir);
                eprintln!("telemetry enabled — inspect events anytime with: bougie telemetry log");
            }
        }
        Answer::No => {
            if mode::write_file(&mode_file, Mode::Off, &date).is_ok() {
                eprintln!("ok — telemetry is off. Enable later with: bougie telemetry on");
            }
        }
        Answer::Skipped => {
            let attempts = gates.attempts + 1;
            write_attempts(&config_dir, attempts);
            if attempts >= MAX_ATTEMPTS {
                // Third strike: stop asking forever.
                let _ = mode::write_file(&mode_file, Mode::Off, &date);
            }
        }
    }
    eprintln!();
}

fn attempts_file(config_dir: &Path) -> std::path::PathBuf {
    config_dir.join("telemetry-prompt-attempts")
}

fn read_attempts(config_dir: &Path) -> u32 {
    fs::read_to_string(attempts_file(config_dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_attempts(config_dir: &Path, attempts: u32) {
    if fs::create_dir_all(config_dir).is_ok() {
        let _ = fs::write(attempts_file(config_dir), attempts.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_gates() -> Gates {
        Gates {
            interactive_format: true,
            in_run_shim: false,
            ci: false,
            stdin_tty: true,
            stderr_tty: true,
            prompt_eligible: true,
            attempts: 0,
        }
    }

    #[test]
    fn prompt_only_when_every_gate_passes() {
        assert!(should_prompt(open_gates()));
        assert!(!should_prompt(Gates { interactive_format: false, ..open_gates() }));
        assert!(!should_prompt(Gates { in_run_shim: true, ..open_gates() }));
        assert!(!should_prompt(Gates { ci: true, ..open_gates() }));
        assert!(!should_prompt(Gates { stdin_tty: false, ..open_gates() }));
        assert!(!should_prompt(Gates { stderr_tty: false, ..open_gates() }));
        assert!(!should_prompt(Gates { prompt_eligible: false, ..open_gates() }));
        assert!(!should_prompt(Gates { attempts: MAX_ATTEMPTS, ..open_gates() }));
    }

    #[test]
    fn enter_means_yes_and_only_explicit_n_declines() {
        assert_eq!(classify_answer(Some("\n")), Answer::Yes);
        assert_eq!(classify_answer(Some("")), Answer::Yes);
        assert_eq!(classify_answer(Some("y")), Answer::Yes);
        assert_eq!(classify_answer(Some("Yes")), Answer::Yes);
        assert_eq!(classify_answer(Some("n")), Answer::No);
        assert_eq!(classify_answer(Some("NO")), Answer::No);
        assert_eq!(classify_answer(Some("maybe")), Answer::Skipped);
        assert_eq!(classify_answer(None), Answer::Skipped);
    }

    #[test]
    fn attempts_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(read_attempts(tmp.path()), 0);
        write_attempts(tmp.path(), 2);
        assert_eq!(read_attempts(tmp.path()), 2);
    }
}
