use crate::cli::OutputFormat;
use crate::config::load_project;
use crate::errors::BougieError;
use crate::index::{build_verifier, fetch::fetch_root};
use crate::install::{host_to_dirname, DEFAULT_INDEX_URL};
use crate::output::{emit, Render};
use crate::paths::Paths;
use crate::state::read_project_resolved;
use crate::target::Triple;
use eyre::Result;
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

const EXTENSION_PREFIX: &str = "extension/";

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub schema_version: u32,
    pub required: Vec<String>,
    pub installed: Vec<String>,
    pub available: Vec<String>,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let req: BTreeSet<&str> = self.required.iter().map(String::as_str).collect();
        let inst: BTreeSet<&str> = self.installed.iter().map(String::as_str).collect();
        let avail: BTreeSet<&str> = self.available.iter().map(String::as_str).collect();
        let union: BTreeSet<&str> = req
            .iter()
            .chain(inst.iter())
            .chain(avail.iter())
            .copied()
            .collect();
        if union.is_empty() {
            writeln!(w, "no extensions known")?;
            return Ok(());
        }
        let pad = union.iter().map(|n| n.len()).max().unwrap_or(0);
        for name in &union {
            let mut tags: Vec<&str> = Vec::with_capacity(3);
            if req.contains(name) {
                tags.push("required");
            }
            if inst.contains(name) {
                tags.push("installed");
            }
            if avail.contains(name) {
                tags.push("available");
            }
            writeln!(w, "{name:<pad$}  {}", tags.join(", "), pad = pad)?;
        }
        Ok(())
    }
}

/// `bougie ext list` — combine three views of extensions for the project:
/// required (composer.json `require` ext-* entries), installed (`.so`
/// files on disk for the synced PHP), and available (sections the index
/// advertises for the host target).
pub fn run(
    format: OutputFormat,
    field: Option<&str>,
    only_installed: bool,
    only_available: bool,
) -> Result<ExitCode> {
    let project_root = std::env::current_dir()?;
    let project = load_project(&project_root)?;
    let mut required: Vec<String> = project
        .composer
        .map(|c| c.require_extensions.into_iter().collect())
        .unwrap_or_default();
    required.sort();

    let installed = list_installed(&project_root)?;
    let available = if only_installed {
        Vec::new()
    } else {
        fetch_available()?
    };

    // Filter the union so the renderer's iteration matches the flag
    // semantics. Required is shown by default; --only-* flags suppress it.
    let (required, installed, available) = if only_installed {
        (Vec::new(), installed, Vec::new())
    } else if only_available {
        (Vec::new(), Vec::new(), available)
    } else {
        (required, installed, available)
    };

    let result = ListResult { schema_version: 1, required, installed, available };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn list_installed(project_root: &Path) -> Result<Vec<String>> {
    let Ok((version, flavor)) = read_project_resolved(project_root) else {
        return Ok(Vec::new());
    };
    let paths = Paths::from_env()?;
    let install = paths.installs().join(format!("{version}-{flavor}"));
    let ext_root = install.join("lib").join("extensions");
    if !ext_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut names: BTreeSet<String> = BTreeSet::new();
    for api_dir in std::fs::read_dir(&ext_root)? {
        let api_dir = api_dir?.path();
        if !api_dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&api_dir)? {
            let p = entry?.path();
            if p.extension().and_then(|s| s.to_str()) == Some("so")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            {
                names.insert(stem.to_owned());
            }
        }
    }
    Ok(names.into_iter().collect())
}

fn fetch_available() -> Result<Vec<String>> {
    let paths = Paths::from_env()?;
    let target = Triple::detect()?.to_string();
    let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());

    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| BougieError::Network {
            operation: "building HTTP client".into(),
            detail: e.to_string(),
        })?;
    let cache_root = paths.cache_index(&host_to_dirname(&host));
    let fetched = fetch_root(&client, &host, &cache_root, build_verifier)?;

    let target_entry = fetched.root.targets.get(&target).ok_or_else(|| {
        let advertised: Vec<String> = fetched.root.targets.keys().cloned().collect();
        BougieError::UnknownTarget {
            triple: target.clone(),
            hint: format!("the index at {host} advertises: {}", advertised.join(", ")),
        }
    })?;

    let mut names: Vec<String> = target_entry
        .sections
        .keys()
        .filter_map(|name| name.strip_prefix(EXTENSION_PREFIX).map(str::to_owned))
        .collect();
    names.sort();
    Ok(names)
}
