//! `bougie composer dump-autoloader` — regenerate
//! `vendor/composer/autoload_*.php` against the current
//! `composer.lock`. Drop-in for upstream `composer dump-autoload`;
//! output is byte-equivalent to Composer 2.8.12 with the same flags.
//!
//! Resolves the working directory the same way Composer's
//! `--working-dir` flag does: explicit `-d` / `--working-dir` wins,
//! otherwise CWD. We don't walk upwards to find a project root — if
//! the user is in `src/`, they should `cd ..` or pass `-d`, same as
//! Composer. The directory is required to contain both
//! `composer.json` (read for `config.autoloader-suffix`) and
//! `composer.lock` (read for the package list + content-hash).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_autoloader::{dump_autoload, DumpRequest, PsrWarning};
use bougie_cli::OutputFormat;
use bougie_output::output::{emit, Render};
use eyre::{Context, Result};
use serde::Serialize;

/// Serializable mirror of [`bougie_autoloader::PsrWarning`] used in
/// `--format json-v1`. Kept as a separate struct so the JSON schema
/// is owned by this command crate (not by the autoloader library).
#[derive(Debug, Serialize)]
pub struct PsrWarningJson {
    pub class: String,
    pub path: String,
    /// 0 or 4. Matches the literal `psr-N` token in the rendered
    /// text warning.
    pub psr_version: u8,
}

impl From<&PsrWarning> for PsrWarningJson {
    fn from(w: &PsrWarning) -> Self {
        Self {
            class: w.class.clone(),
            path: w.relative_path.clone(),
            psr_version: w.psr_version,
        }
    }
}

/// JSON-v1 output shape. Echoes back the flags that were applied so a
/// scripted caller can confirm what bougie ran with (matters for
/// `--apcu-autoloader` without an explicit prefix — the result will
/// carry the random prefix that was generated). `class_count` and
/// `warnings` come from the dump itself: the size of the emitted
/// classmap and the PSR-noncompliance reports Composer would print.
#[derive(Debug, Serialize)]
pub struct DumpAutoloaderResult {
    pub schema_version: u32,
    pub project_root: PathBuf,
    pub optimize: bool,
    pub classmap_authoritative: bool,
    pub no_dev: bool,
    pub apcu_autoloader: bool,
    pub autoloader_suffix: Option<String>,
    pub class_count: usize,
    pub warnings: Vec<PsrWarningJson>,
}

/// ANSI magenta wrap for the final "Generated …" line. Stripped by
/// `anstream::AutoStream` when stdout isn't a TTY or `NO_COLOR` is
/// set, so we can hard-code the escape codes here.
const MAGENTA: &str = "\x1b[35m";
const RESET: &str = "\x1b[0m";

impl Render for DumpAutoloaderResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        // PSR-noncompliance warnings come out first, in scan order,
        // matching the interleave you'd see from upstream Composer.
        for warn in &self.warnings {
            writeln!(
                w,
                "Class {} located in {} does not comply with psr-{} autoloading standard. Skipping.",
                warn.class, warn.path, warn.psr_version
            )?;
        }
        // Composer's `Generated …` summary. Mode prefix depends on
        // `--optimize` / `--classmap-authoritative`; without either,
        // Composer prints `Generated autoload files containing N classes`.
        let mode = if self.classmap_authoritative {
            "optimized autoload files (authoritative)"
        } else if self.optimize {
            "optimized autoload files"
        } else {
            "autoload files"
        };
        writeln!(
            w,
            "{MAGENTA}Generated {mode} containing {} classes{RESET}",
            self.class_count
        )
    }
}

// Params line up with Composer's `dump-autoload` CLI flags 1:1.
#[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
pub fn run(
    format: OutputFormat,
    working_dir: Option<PathBuf>,
    optimize: bool,
    classmap_authoritative: bool,
    no_dev: bool,
    dev: bool,
    apcu_autoloader: bool,
    apcu_prefix: Option<String>,
    autoloader_suffix: Option<String>,
) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };

    require_file(&project_root, "composer.json")?;
    require_file(&project_root, "composer.lock")?;

    // An explicit `--apcu-autoloader-prefix` implies `--apcu-autoloader`
    // (Composer does the same — see commands/InstallCommand.php).
    let apcu_autoloader = apcu_autoloader || apcu_prefix.is_some();

    // Composer parity: with neither `--no-dev` nor `--dev`, the dump inherits
    // the dev mode of the installed tree recorded in
    // `vendor/composer/installed.json`. Without this, a dump on a
    // `--no-dev`-installed tree emits `files` autoload requires for dev-only
    // packages that aren't on disk, and the next PHP run fatals (issue #499).
    let no_dev = effective_no_dev(no_dev, dev, &project_root);

    let req = DumpRequest {
        project_root: &project_root,
        optimize,
        classmap_authoritative,
        no_dev,
        apcu_autoloader,
        apcu_prefix,
        autoloader_suffix: autoloader_suffix.clone(),
    };
    let report = dump_autoload(&req).wrap_err("dump_autoload failed")?;

    let result = DumpAutoloaderResult {
        schema_version: 1,
        project_root,
        optimize,
        classmap_authoritative,
        no_dev,
        apcu_autoloader,
        autoloader_suffix,
        class_count: report.class_count,
        warnings: report.warnings.iter().map(PsrWarningJson::from).collect(),
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Resolve the effective `no_dev` for the dump, matching Composer's
/// `dump-autoload`: an explicit `--no-dev` wins, then `--dev`, and with neither
/// the dev mode is inherited from the installed tree
/// ([`installed_dev_mode`]). An absent/mode-less `installed.json` keeps the
/// dev-included default (a pre-install dump behaves as before).
fn effective_no_dev(no_dev: bool, dev: bool, project_root: &Path) -> bool {
    if no_dev {
        true
    } else if dev {
        false
    } else {
        installed_dev_mode(project_root).is_some_and(|installed_dev| !installed_dev)
    }
}

/// The dev mode recorded in `vendor/composer/installed.json` (`"dev": bool`),
/// or `None` when the file is absent or doesn't record it — the shape a fresh
/// (pre-install) project takes.
fn installed_dev_mode(project_root: &Path) -> Option<bool> {
    let bytes = std::fs::read(project_root.join("vendor/composer/installed.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("dev").and_then(serde_json::Value::as_bool)
}

fn require_file(dir: &Path, name: &str) -> Result<()> {
    let path = dir.join(name);
    if !path.is_file() {
        return Err(eyre::eyre!(
            "{name} not found in {}",
            dir.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `vendor/composer/installed.json` with the given body under `root`.
    fn write_installed_json(root: &Path, body: &str) {
        let dir = root.join("vendor/composer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("installed.json"), body).unwrap();
    }

    #[test]
    fn explicit_flags_win_over_installed_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Installed dev:true, but --no-dev must still force dev off...
        write_installed_json(root, r#"{ "packages": [], "dev": true }"#);
        assert!(effective_no_dev(true, false, root));
        // ...and installed dev:false with --dev must force dev on.
        write_installed_json(root, r#"{ "packages": [], "dev": false }"#);
        assert!(!effective_no_dev(false, true, root));
    }

    #[test]
    fn inherits_installed_dev_mode_when_no_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Installed --no-dev (dev:false) → dump excludes dev (issue #499).
        write_installed_json(root, r#"{ "packages": [], "dev": false }"#);
        assert!(effective_no_dev(false, false, root));
        // Installed with dev (dev:true) → dump includes dev.
        write_installed_json(root, r#"{ "packages": [], "dev": true }"#);
        assert!(!effective_no_dev(false, false, root));
    }

    #[test]
    fn defaults_to_dev_included_without_installed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // No installed.json (pre-install dump) → keep the dev-included default.
        assert!(!effective_no_dev(false, false, root));
        // installed.json present but missing the `dev` field → same default.
        write_installed_json(root, r#"{ "packages": [] }"#);
        assert!(!effective_no_dev(false, false, root));
        assert_eq!(installed_dev_mode(root), None);
    }

    #[test]
    fn installed_dev_mode_reads_the_field() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_installed_json(root, r#"{ "packages": [], "dev": false }"#);
        assert_eq!(installed_dev_mode(root), Some(false));
        write_installed_json(root, r#"{ "packages": [], "dev": true }"#);
        assert_eq!(installed_dev_mode(root), Some(true));
    }
}
