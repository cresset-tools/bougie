//! uv-style change reporting: a dimmed one-line summary followed by one
//! `+` / `-` / `~` row per affected installation.
//!
//! Modeled on `uv python install`, which prints:
//!
//! ```text
//! Installed 3 versions in 3.42s
//!  + cpython-3.10.14-macos-aarch64-none
//!  + cpython-3.11.9-macos-aarch64-none
//!  + cpython-3.12.4-macos-aarch64-none
//! ```
//!
//! The summary verb and subject are the caller's ("Installed",
//! "3 versions"); the row key is whatever identifies one installation —
//! for bougie that's the index tag, `php-8.3.12-<target>-<flavor>`.
//!
//! Only text mode uses this — JSON output is untouched.

use anstyle::{AnsiColor, Style};
use std::io::{self, Write};

use crate::list_format::{DIM_STYLE, write_styled};

/// Bold, for the subject of a summary line and for a row's key.
pub const KEY_STYLE: Style = Style::new().bold();
/// Dim parenthetical tacked onto a row (uv puts executables here).
pub const NOTE_STYLE: Style = Style::new().dimmed();

/// What happened to one installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Newly installed. Green `+`.
    Added,
    /// Removed from the install tree. Red `-`.
    Removed,
    /// Replaced in place (upgrade / reinstall). Yellow `~`.
    Reinstalled,
}

impl ChangeKind {
    /// The single-character marker uv uses for this kind.
    pub fn marker(self) -> &'static str {
        match self {
            ChangeKind::Added => "+",
            ChangeKind::Removed => "-",
            ChangeKind::Reinstalled => "~",
        }
    }

    fn style(self) -> Style {
        match self {
            ChangeKind::Added => AnsiColor::Green.on_default(),
            ChangeKind::Removed => AnsiColor::Red.on_default(),
            ChangeKind::Reinstalled => AnsiColor::Yellow.on_default(),
        }
    }
}

/// `Installed 3 versions in 3.42s` — dim line, bold subject.
///
/// `verb` is the past-tense action ("Installed", "Removed"), `subject`
/// the thing it applied to ("3 versions", "PHP 8.3.12"), and
/// `elapsed_ms` the wall-clock of the operation.
pub fn write_summary(
    w: &mut dyn Write,
    verb: &str,
    subject: &str,
    elapsed_ms: f64,
) -> io::Result<()> {
    write_styled(w, DIM_STYLE, &format!("{verb} "))?;
    write_styled(w, DIM_STYLE.bold(), subject)?;
    write_styled(w, DIM_STYLE, &format!(" in {}", fmt_elapsed(elapsed_ms)))?;
    writeln!(w)
}

/// ` + php-8.3.12-<target>-nts (note)` — one row under a summary line.
///
/// Leading space, colored marker, bold key, optional dim parenthetical.
pub fn write_change(
    w: &mut dyn Write,
    kind: ChangeKind,
    key: &str,
    note: Option<&str>,
) -> io::Result<()> {
    write!(w, " ")?;
    write_styled(w, kind.style(), kind.marker())?;
    write!(w, " ")?;
    write_styled(w, KEY_STYLE, key)?;
    if let Some(note) = note {
        write_styled(w, NOTE_STYLE, &format!(" ({note})"))?;
    }
    writeln!(w)
}

/// A detail line indented under a [`write_change`] row, printed dim.
pub fn write_change_detail(w: &mut dyn Write, text: &str) -> io::Result<()> {
    write_styled(w, DIM_STYLE, &format!("   {text}"))?;
    writeln!(w)
}

/// Format an elapsed millisecond count uv-style: sub-millisecond keeps
/// two decimals (`0.55ms`), whole milliseconds print as integers
/// (`14ms`), and a second or more switches to `1.23s`.
pub fn fmt_elapsed(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2}s", ms / 1000.0)
    } else if ms >= 1.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{ms:.2}ms")
    }
}

/// Pick the singular or plural noun for `n`.
pub fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(f: impl Fn(&mut Vec<u8>) -> io::Result<()>) -> String {
        // The command path wraps stdout in an `anstream` adapter that
        // strips SGR codes off a non-color destination; here we strip
        // by hand so the assertions read as the user's plain output.
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        let styled = String::from_utf8(buf).unwrap();
        let mut out = String::with_capacity(styled.len());
        let mut in_escape = false;
        for c in styled.chars() {
            match c {
                '\u{1b}' => in_escape = true,
                'm' if in_escape => in_escape = false,
                _ if in_escape => {}
                _ => out.push(c),
            }
        }
        out
    }

    #[test]
    fn summary_reads_like_uv() {
        let out = plain(|w| write_summary(w, "Installed", "3 versions", 3420.0));
        assert_eq!(out, "Installed 3 versions in 3.42s\n");
    }

    #[test]
    fn change_row_is_space_marker_key() {
        let out = plain(|w| {
            write_change(
                w,
                ChangeKind::Added,
                "php-8.3.12-x86_64-unknown-linux-gnu-nts",
                None,
            )
        });
        assert_eq!(out, " + php-8.3.12-x86_64-unknown-linux-gnu-nts\n");
    }

    #[test]
    fn change_row_carries_a_note() {
        let out = plain(|w| write_change(w, ChangeKind::Removed, "php-8.2.1-nts", Some("stale")));
        assert_eq!(out, " - php-8.2.1-nts (stale)\n");
    }

    #[test]
    fn markers_match_uv() {
        assert_eq!(ChangeKind::Added.marker(), "+");
        assert_eq!(ChangeKind::Removed.marker(), "-");
        assert_eq!(ChangeKind::Reinstalled.marker(), "~");
    }

    #[test]
    fn elapsed_scales_by_magnitude() {
        assert_eq!(fmt_elapsed(0.5), "0.50ms");
        assert_eq!(fmt_elapsed(14.2), "14ms");
        assert_eq!(fmt_elapsed(1234.0), "1.23s");
    }

    #[test]
    fn plural_switches_on_one() {
        assert_eq!(plural(1, "version", "versions"), "version");
        assert_eq!(plural(2, "version", "versions"), "versions");
    }
}
