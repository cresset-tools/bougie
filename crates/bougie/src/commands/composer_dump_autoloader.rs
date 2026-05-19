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

#[allow(clippy::fn_params_excessive_bools)] // names line up with Composer CLI flags 1:1
pub fn run(
    format: OutputFormat,
    working_dir: Option<PathBuf>,
    optimize: bool,
    classmap_authoritative: bool,
    no_dev: bool,
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
