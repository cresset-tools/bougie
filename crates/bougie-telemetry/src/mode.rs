//! Consent-mode resolution: `off` / `local` / `on`.
//!
//! Precedence (first match wins):
//! 1. `DO_NOT_TRACK` set and truthy → off (also suppresses prompts).
//! 2. `BOUGIE_TELEMETRY=off|local|on` (aliases `1`/`true` → on,
//!    `0`/`false` → off, same convention as `BOUGIE_SYSTEM_PHP`).
//!    Never writes the file. An explicit env `on` overrides CI
//!    detection — that's the lever for telemetry from owned runners.
//! 3. The mode file (single shell-writable line:
//!    `<mode> <yyyy-mm-dd> <consent-version>`). A stale consent
//!    version on `on` behaves as unset so the user is re-asked.
//! 4. Unset → off, prompt-eligible.
//!
//! The mode-file format is a contract with the installer consent
//! snippets (`scripts/install-consent.{sh,ps1}`), which write it from
//! plain shell — keep it a single trivially-formattable line.

use std::fs;
use std::io;
use std::path::Path;

/// Bumped only when the *scope* of collection expands. An `on` recorded
/// under an older version stops uploads and re-prompts.
pub const CONSENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Local,
    On,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Local => "local",
            Self::On => "on",
        }
    }

    /// Parse a mode token, accepting the truthy/falsy aliases.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "on" | "1" | "true" => Some(Self::On),
            "off" | "0" | "false" => Some(Self::Off),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
}

/// Where the effective mode came from — surfaced by `telemetry status`
/// and used to decide prompt eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    DoNotTrack,
    Env,
    File,
    /// No signal at all (or a stale consent version): behaves as `off`
    /// and is the only state in which a consent prompt may appear.
    Unset,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DoNotTrack => "DO_NOT_TRACK",
            Self::Env => "BOUGIE_TELEMETRY",
            Self::File => "mode file",
            Self::Unset => "unset",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModeState {
    pub mode: Mode,
    pub source: Source,
    /// Consent date recorded in the mode file, when that's the source.
    pub consent_date: Option<String>,
    /// Consent version recorded in the mode file, when that's the source.
    pub consent_version: Option<u32>,
}

impl ModeState {
    /// The consent prompt may only appear when nothing has decided the
    /// mode — never over an explicit off/local/on, DNT, or env setting.
    pub fn prompt_eligible(&self) -> bool {
        matches!(self.source, Source::Unset)
    }
}

/// Pure resolver over the three inputs; env reads live in the caller so
/// this stays table-testable (the `Paths::resolve` convention).
pub fn resolve(
    do_not_track: Option<&str>,
    env_mode: Option<&str>,
    file_contents: Option<&str>,
) -> ModeState {
    if do_not_track.is_some_and(dnt_truthy) {
        return ModeState {
            mode: Mode::Off,
            source: Source::DoNotTrack,
            consent_date: None,
            consent_version: None,
        };
    }
    // An unparseable env value is treated as unset rather than off:
    // a typo shouldn't silently flip a recorded consent either way.
    if let Some(mode) = env_mode.and_then(Mode::parse) {
        return ModeState {
            mode,
            source: Source::Env,
            consent_date: None,
            consent_version: None,
        };
    }
    if let Some(parsed) = file_contents.and_then(parse_file_line) {
        // `on` under an older consent version behaves as unset:
        // uploads stop and the next interactive command re-prompts
        // with the delta. Narrowing never bumps, so off/local keep.
        if parsed.mode == Mode::On
            && parsed.consent_version.is_none_or(|v| v < CONSENT_VERSION)
        {
            return ModeState {
                mode: Mode::Off,
                source: Source::Unset,
                consent_date: parsed.consent_date,
                consent_version: parsed.consent_version,
            };
        }
        return parsed;
    }
    ModeState {
        mode: Mode::Off,
        source: Source::Unset,
        consent_date: None,
        consent_version: None,
    }
}

/// `DO_NOT_TRACK` semantics per the (Wayback-archived) spec: presence
/// with any value other than an explicit falsy counts.
fn dnt_truthy(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
}

fn parse_file_line(contents: &str) -> Option<ModeState> {
    let line = contents.lines().next()?.trim();
    let mut parts = line.split_ascii_whitespace();
    let mode = Mode::parse(parts.next()?)?;
    let consent_date = parts.next().map(str::to_owned);
    let consent_version = parts.next().and_then(|v| v.parse().ok());
    Some(ModeState { mode, source: Source::File, consent_date, consent_version })
}

/// Render the single-line mode-file format the installer snippets also
/// write: `<mode> <yyyy-mm-dd> <consent-version>`.
pub fn format_line(mode: Mode, date: &str) -> String {
    format!("{} {} {}\n", mode.as_str(), date, CONSENT_VERSION)
}

/// Read the mode file; `None` for missing/unreadable (fail-soft).
pub fn read_file(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Write the mode file, creating parent dirs.
pub fn write_file(path: &Path, mode: Mode, date: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format_line(mode, date))
}

/// Resolve the effective mode from the real process environment plus
/// the mode file at `path` (if any).
pub fn resolve_from_env(mode_file: Option<&Path>) -> ModeState {
    let dnt = std::env::var("DO_NOT_TRACK").ok();
    let env_mode = std::env::var("BOUGIE_TELEMETRY").ok();
    let file = mode_file.and_then(read_file);
    resolve(dnt.as_deref(), env_mode.as_deref(), file.as_deref())
}

/// Conservative CI sniff: the de-facto `CI` variable plus the major
/// vendor markers. With the mode unset, CI is silently off and never
/// prompted; an explicit `BOUGIE_TELEMETRY=on` still wins (it is
/// checked earlier in the precedence order).
pub fn is_ci() -> bool {
    const MARKERS: &[&str] = &[
        "CI",
        "GITHUB_ACTIONS",
        "GITLAB_CI",
        "TRAVIS",
        "CIRCLECI",
        "JENKINS_URL",
        "TEAMCITY_VERSION",
        "BUILDKITE",
        "DRONE",
        "APPVEYOR",
        "TF_BUILD",
    ];
    MARKERS.iter().any(|m| {
        std::env::var_os(m).is_some_and(|v| !v.is_empty() && v != "0" && v != "false")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dnt: Option<&str>, env: Option<&str>, file: Option<&str>) -> ModeState {
        resolve(dnt, env, file)
    }

    #[test]
    fn unset_is_off_and_prompt_eligible() {
        let s = state(None, None, None);
        assert_eq!(s.mode, Mode::Off);
        assert!(s.prompt_eligible());
    }

    #[test]
    fn do_not_track_beats_everything() {
        let s = state(Some("1"), Some("on"), Some("on 2026-07-03 1"));
        assert_eq!(s.mode, Mode::Off);
        assert_eq!(s.source, Source::DoNotTrack);
        assert!(!s.prompt_eligible());
    }

    #[test]
    fn do_not_track_falsy_values_ignored() {
        for v in ["0", "false", "", "  "] {
            let s = state(Some(v), None, Some("on 2026-07-03 1"));
            assert_eq!(s.mode, Mode::On, "DO_NOT_TRACK={v:?}");
        }
    }

    #[test]
    fn env_beats_file_and_accepts_aliases() {
        for (v, expect) in [
            ("on", Mode::On),
            ("1", Mode::On),
            ("true", Mode::On),
            ("ON", Mode::On),
            ("off", Mode::Off),
            ("0", Mode::Off),
            ("false", Mode::Off),
            ("local", Mode::Local),
        ] {
            let s = state(None, Some(v), Some("off 2026-01-01 1"));
            assert_eq!(s.mode, expect, "BOUGIE_TELEMETRY={v:?}");
            assert_eq!(s.source, Source::Env);
        }
    }

    #[test]
    fn env_typo_falls_through_to_file() {
        let s = state(None, Some("yes-please"), Some("local 2026-01-01 1"));
        assert_eq!(s.mode, Mode::Local);
        assert_eq!(s.source, Source::File);
    }

    #[test]
    fn file_round_trips() {
        let line = format_line(Mode::On, "2026-07-03");
        let s = state(None, None, Some(&line));
        assert_eq!(s.mode, Mode::On);
        assert_eq!(s.source, Source::File);
        assert_eq!(s.consent_date.as_deref(), Some("2026-07-03"));
        assert_eq!(s.consent_version, Some(CONSENT_VERSION));
    }

    #[test]
    fn stale_consent_version_on_behaves_unset() {
        let s = state(None, None, Some("on 2025-01-01 0"));
        assert_eq!(s.mode, Mode::Off);
        assert!(s.prompt_eligible());
        // ... but a stale `off` keeps: narrowing never re-prompts.
        let s = state(None, None, Some("off 2025-01-01 0"));
        assert_eq!(s.mode, Mode::Off);
        assert!(!s.prompt_eligible());
    }

    #[test]
    fn missing_version_on_behaves_unset() {
        let s = state(None, None, Some("on 2025-01-01"));
        assert!(s.prompt_eligible());
    }

    #[test]
    fn garbage_file_is_unset() {
        let s = state(None, None, Some("banana\n"));
        assert!(s.prompt_eligible());
    }
}
