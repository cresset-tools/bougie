//! Shared text-mode formatting for `bougie php list`, `composer list`,
//! and `ext list`. Modeled on `uv python list`: bold version, cyan
//! prefix/flavor, dim separators/targets, green installed paths, dim
//! `<download available>` placeholders.
//!
//! Only the text renderer uses this — JSON output is untouched.

use anstyle::{AnsiColor, Style};
use std::io::{self, Write};
use std::path::Path;

pub const VERSION_STYLE: Style = Style::new().bold();
pub const PREFIX_STYLE: Style = AnsiColor::Cyan.on_default();
pub const FLAVOR_STYLE: Style = AnsiColor::Cyan.on_default();
pub const TARGET_STYLE: Style = Style::new().dimmed();
pub const SEP_STYLE: Style = Style::new().dimmed();
pub const PATH_STYLE: Style = AnsiColor::Green.on_default();
pub const PLACEHOLDER_STYLE: Style = Style::new().dimmed();
pub const STATUS_STYLE: Style = Style::new().dimmed();

/// The left-hand column of a list row.
///
/// Lengths are computed from the unstyled string so padding lines up
/// regardless of whether ANSI escapes are emitted.
#[derive(Debug, Default)]
pub struct KeyParts<'a> {
    /// Cyan, no separator after it. e.g. `"php-"`, `"redis"`.
    pub prefix: Option<&'a str>,
    /// Bold. Required.
    pub version: &'a str,
    /// Dim. Joined with a dim `-`. Used for triple strings.
    pub target: Option<&'a str>,
    /// Cyan. Joined with a dim `-`. e.g. `"nts"`, `"stable"`.
    pub flavor: Option<&'a str>,
}

impl KeyParts<'_> {
    pub fn plain_len(&self) -> usize {
        let mut n = 0;
        if let Some(p) = self.prefix {
            n += p.len();
        }
        n += self.version.len();
        if let Some(t) = self.target {
            n += 1 + t.len();
        }
        if let Some(f) = self.flavor {
            n += 1 + f.len();
        }
        n
    }

    pub fn write(&self, w: &mut dyn Write) -> io::Result<()> {
        if let Some(p) = self.prefix {
            write!(
                w,
                "{}{}{}",
                PREFIX_STYLE.render(),
                p,
                PREFIX_STYLE.render_reset()
            )?;
        }
        write!(
            w,
            "{}{}{}",
            VERSION_STYLE.render(),
            self.version,
            VERSION_STYLE.render_reset()
        )?;
        if let Some(t) = self.target {
            write!(
                w,
                "{}-{}{}",
                TARGET_STYLE.render(),
                t,
                TARGET_STYLE.render_reset()
            )?;
        }
        if let Some(f) = self.flavor {
            write!(w, "{}-{}", SEP_STYLE.render(), SEP_STYLE.render_reset())?;
            write!(
                w,
                "{}{}{}",
                FLAVOR_STYLE.render(),
                f,
                FLAVOR_STYLE.render_reset()
            )?;
        }
        Ok(())
    }
}

/// Right-hand column: what the row resolves to.
#[derive(Debug)]
pub enum Suffix<'a> {
    /// Installed on disk. Rendered green.
    Path(&'a Path),
    /// Index URL (when `--show-urls` is set). Rendered dim.
    Url(&'a str),
    /// `<download available>` placeholder. Rendered dim.
    Placeholder,
    /// Comma-joined status tags (ext list rows that have neither path nor URL).
    Status(&'a [&'a str]),
}

impl Suffix<'_> {
    pub fn write(&self, w: &mut dyn Write) -> io::Result<()> {
        match self {
            Suffix::Path(p) => write!(
                w,
                "{}{}{}",
                PATH_STYLE.render(),
                p.display(),
                PATH_STYLE.render_reset()
            ),
            Suffix::Url(u) => write!(
                w,
                "{}{}{}",
                PLACEHOLDER_STYLE.render(),
                u,
                PLACEHOLDER_STYLE.render_reset()
            ),
            Suffix::Placeholder => write!(
                w,
                "{}<download available>{}",
                PLACEHOLDER_STYLE.render(),
                PLACEHOLDER_STYLE.render_reset()
            ),
            Suffix::Status(tags) => write!(
                w,
                "{}{}{}",
                STATUS_STYLE.render(),
                tags.join(", "),
                STATUS_STYLE.render_reset()
            ),
        }
    }
}

/// Pad with spaces from `plain_len` up to `pad`. ANSI escapes have
/// zero display width, so columns align as long as callers pass the
/// unstyled length.
pub fn pad_spaces(w: &mut dyn Write, plain_len: usize, pad: usize) -> io::Result<()> {
    for _ in plain_len..pad {
        write!(w, " ")?;
    }
    Ok(())
}

/// Write `text` wrapped in `style`'s SGR sequence.
pub fn write_styled(w: &mut dyn Write, style: Style, text: &str) -> io::Result<()> {
    write!(w, "{}{}{}", style.render(), text, style.render_reset())
}

/// Optional dimmed parenthetical tacked onto the end (e.g. channel
/// label like `(stable)`).
pub fn write_tail_note(w: &mut dyn Write, note: &str) -> io::Result<()> {
    write!(
        w,
        " {}({}){}",
        STATUS_STYLE.render(),
        note,
        STATUS_STYLE.render_reset()
    )
}

/// Render one row: key, two spaces, suffix, optional tail note, newline.
pub fn write_row(
    w: &mut dyn Write,
    key: &KeyParts<'_>,
    pad: usize,
    suffix: &Suffix<'_>,
    tail: Option<&str>,
) -> io::Result<()> {
    key.write(w)?;
    let plain = key.plain_len();
    for _ in plain..pad {
        write!(w, " ")?;
    }
    write!(w, "  ")?;
    suffix.write(w)?;
    if let Some(t) = tail {
        write_tail_note(w, t)?;
    }
    writeln!(w)
}
