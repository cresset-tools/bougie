use bougie_cli::OutputFormat;
use bougie_composer::{fetch, parse_request, resolve_request, ComposerRequest};
use bougie_errors::BougieError;
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct UninstallResult {
    pub schema_version: u32,
    pub removed: PathBuf,
}

impl Render for UninstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "removed {}", self.removed.display())
    }
}

pub fn run(format: OutputFormat, request_str: &str) -> Result<ExitCode> {
    let request = parse_request(request_str)?;
    let paths = Paths::from_env()?;
    let dest = locate_install_dir(&paths, &request)?;
    if !dest.exists() {
        return Err(BougieError::Resolution {
            kind: "composer/uninstall".into(),
            detail: format!("no install directory at {}", dest.display()),
        }
        .into());
    }
    std::fs::remove_dir_all(&dest).map_err(|e| BougieError::Filesystem {
        operation: format!("removing {}", dest.display()),
        detail: e.to_string(),
    })?;
    let result = UninstallResult { schema_version: 1, removed: dest };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn locate_install_dir(paths: &Paths, request: &ComposerRequest) -> Result<PathBuf> {
    match request {
        ComposerRequest::Exact(v) => Ok(paths.composer_root().join(v)),
        ComposerRequest::Channel(_) | ComposerRequest::Partial(_) => {
            // Resolve via the channels snapshot so the user can write
            // `bougie composer uninstall lts` (or `2.2`, `stable`, ...)
            // and remove whichever exact version that currently points at.
            let client = fetch::build_client()?;
            let channels = fetch::fetch_channels(&client, paths)?;
            let resolved = resolve_request(&channels, request)?;
            Ok(paths.composer_root().join(resolved.version))
        }
        ComposerRequest::Path(p) => {
            let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            if !canon.starts_with(paths.composer_root()) {
                return Err(eyre!(
                    "path {} is not under {}",
                    p.display(),
                    paths.composer_root().display()
                ));
            }
            // The path may point at the phar or at the version dir.
            if canon.is_dir() {
                Ok(canon)
            } else {
                canon
                    .parent()
                    .map(Path::to_path_buf)
                    .ok_or_else(|| eyre!("path {} has no parent", p.display()))
            }
        }
    }
}

use std::path::Path;
