use crate::cli::OutputFormat;
use crate::composer::{parse_request, ComposerRequest};
use crate::errors::BougieError;
use crate::output::{emit, Render};
use crate::paths::Paths;
use crate::state::read_project_resolved_composer;
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct FindResult {
    pub schema_version: u32,
    pub path: PathBuf,
}

impl Render for FindResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", self.path.display())
    }
}

pub fn run(
    format: OutputFormat,
        request_str: Option<&str>,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let phar = match request_str {
        Some(s) => find_for_request(&paths, &parse_request(s)?)?,
        None => find_project_or_default(&paths)?,
    };
    let result = FindResult { schema_version: 1, path: phar };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn find_for_request(paths: &Paths, request: &ComposerRequest) -> Result<PathBuf> {
    match request {
        ComposerRequest::Path(p) => {
            let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            if canon.is_file() {
                Ok(canon)
            } else if canon.is_dir() {
                let phar = canon.join("composer.phar");
                if phar.is_file() {
                    Ok(phar)
                } else {
                    Err(eyre!("no composer.phar at {}", canon.display()))
                }
            } else {
                Err(eyre!("no such path: {}", p.display()))
            }
        }
        ComposerRequest::Exact(v) => {
            let phar = paths.composer_phar(v);
            if !phar.exists() {
                return Err(missing_install_error(&phar));
            }
            Ok(phar)
        }
        _ => find_project_or_default(paths),
    }
}

fn find_project_or_default(paths: &Paths) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    if let Ok(version) = read_project_resolved_composer(&cwd) {
        let phar = paths.composer_phar(&version);
        if phar.is_file() {
            return Ok(phar);
        }
    }
    // Fall back to the highest installed version on disk.
    highest_installed(paths)
}

fn highest_installed(paths: &Paths) -> Result<PathBuf> {
    let root = paths.composer_root();
    if !root.exists() {
        return Err(BougieError::Resolution {
            kind: "composer".into(),
            detail: "no composer installed; run `bougie composer install` first".into(),
        }
        .into());
    }
    let mut best: Option<String> = None;
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let phar = entry.path().join("composer.phar");
        if !phar.is_file() {
            continue;
        }
        match &best {
            None => best = Some(name.to_owned()),
            Some(b) if compare_versions(name, b) == std::cmp::Ordering::Greater => {
                best = Some(name.to_owned());
            }
            _ => {}
        }
    }
    let v = best.ok_or_else(|| BougieError::Resolution {
        kind: "composer".into(),
        detail: "no composer installed; run `bougie composer install` first".into(),
    })?;
    Ok(paths.composer_phar(&v))
}

fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u32> {
        s.split('.')
            .map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>())
            .filter_map(|p| p.parse::<u32>().ok())
            .collect()
    };
    parse(a).cmp(&parse(b))
}

fn missing_install_error(phar: &std::path::Path) -> eyre::Report {
    BougieError::Resolution {
        kind: "composer".into(),
        detail: format!(
            "no installed composer at {} — run `bougie composer install` first",
            phar.display()
        ),
    }
    .into()
}
