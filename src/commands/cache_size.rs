use crate::cli::OutputFormat;
use crate::output::{emit, Render};
use crate::paths::Paths;
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct SizeResult {
    pub schema_version: u32,
    pub cache_bytes: u64,
    pub store_bytes: u64,
    pub installs_bytes: u64,
    pub total_bytes: u64,
}

impl Render for SizeResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "cache    {}", human(self.cache_bytes))?;
        writeln!(w, "store    {}", human(self.store_bytes))?;
        writeln!(w, "installs {}", human(self.installs_bytes))?;
        writeln!(w, "total    {}", human(self.total_bytes))
    }
}

pub fn run(format: OutputFormat, field: Option<&str>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let cache = dir_size(paths.cache());
    let store = dir_size(&paths.store());
    let installs = dir_size(&paths.installs());
    let result = SizeResult {
        schema_version: 1,
        cache_bytes: cache,
        store_bytes: store,
        installs_bytes: installs,
        total_bytes: cache.saturating_add(store).saturating_add(installs),
    };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn dir_size(p: &Path) -> u64 {
    if !p.exists() {
        return 0;
    }
    walkdir::WalkDir::new(p)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter_map(|e| std::fs::metadata(e.path()).ok())
        .filter(std::fs::Metadata::is_file)
        .map(|m| m.len())
        .sum()
}

fn human(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    #[allow(clippy::cast_precision_loss)]
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}
