//! The `$EDITOR` review pass. The draft the user saves — everything
//! below the scissors line — is byte-for-byte the report that ships;
//! an in-editor redaction is authoritative because nothing structured
//! duplicates report content on the wire.

use bougie_paths::Paths;
use eyre::{Result, WrapErr, eyre};
use std::path::{Path, PathBuf};

/// git-convention scissors: everything above (and including) this
/// line is instructions, stripped before sending.
const SCISSORS: &str = "# ------------------------ >8 ------------------------";

const HEADER: &str = "\
# bougie diagnose — review before sending
# Everything BELOW the scissors line is exactly what will be sent.
# Edit freely: redact anything private, add context under \"your notes\".
# Delete everything (or save an empty report) to abort.
";

/// The on-disk draft. Held until the report is actually sent (or
/// written for `--issue`) so a failed upload never loses the user's
/// edits; `discard` removes it.
#[derive(Debug)]
pub struct Draft {
    path: PathBuf,
}

impl Draft {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn discard(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Open the report in `$VISUAL` / `$EDITOR`, returning the edited
/// report text and the kept draft, or `None` when the user aborted by
/// saving an empty report (the draft is removed then). An editor that
/// fails to launch or exits non-zero is an error; the draft file is
/// kept and named so nothing typed is lost.
pub fn edit(paths: &Paths, markdown: &str) -> Result<Option<(String, Draft)>> {
    let dir = paths.cache().join("telemetry");
    std::fs::create_dir_all(&dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("diagnose-draft-{}.md", std::process::id()));
    write_private(&path, &format!("{HEADER}{SCISSORS}\n{markdown}"))
        .wrap_err_with(|| format!("writing draft {}", path.display()))?;
    let draft = Draft { path };

    let editor = resolve_editor();
    let status = spawn_editor(&editor, draft.path()).wrap_err_with(|| {
        format!(
            "launching editor `{editor}` (draft kept at {})",
            draft.path().display()
        )
    })?;
    if !status.success() {
        return Err(eyre!(
            "editor `{editor}` exited with {status}; draft kept at {}",
            draft.path().display()
        ));
    }

    let edited = std::fs::read_to_string(draft.path())
        .wrap_err_with(|| format!("reading edited draft {}", draft.path().display()))?;
    let report = strip_header(&edited);
    if report.trim().is_empty() {
        draft.discard();
        return Ok(None);
    }
    Ok(Some((report, draft)))
}

/// Draft is user data with secrets-adjacent content: 0600 on Unix.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}

/// Everything after the scissors line; the whole text when the user
/// deleted the header themselves.
fn strip_header(edited: &str) -> String {
    match edited.find(SCISSORS) {
        Some(at) => {
            let after = &edited[at + SCISSORS.len()..];
            after.strip_prefix('\n').unwrap_or(after).to_owned()
        }
        None => edited.to_owned(),
    }
}

fn resolve_editor() -> String {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return v;
        }
    }
    if cfg!(windows) {
        "notepad".to_owned()
    } else {
        "vi".to_owned()
    }
}

/// Run via the shell on Unix so `EDITOR="code --wait"` works; direct
/// spawn on Windows.
fn spawn_editor(editor: &str, file: &Path) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(file)
            .status()
    }
    #[cfg(not(unix))]
    {
        std::process::Command::new(editor).arg(file).status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_header_takes_everything_after_scissors() {
        let text = format!("{HEADER}{SCISSORS}\n# report\nbody\n");
        assert_eq!(strip_header(&text), "# report\nbody\n");
    }

    #[test]
    fn strip_header_passes_headerless_text_through() {
        assert_eq!(strip_header("# report\n"), "# report\n");
    }
}
