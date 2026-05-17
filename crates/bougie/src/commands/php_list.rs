use bougie_cli::OutputFormat;
use bougie_errors::BougieError;
use bougie_index::{
    build_verifier,
    fetch::{fetch_root, fetch_section},
};
use bougie_installer::install::{host_to_dirname, DEFAULT_INDEX_URL};
use bougie_output::list_format::{write_row, KeyParts, Suffix};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use bougie_version::request::{parse_request, Flavor, Request, VersionLike};
use bougie_fs::store::{install_dir, list_installed};
use bougie_platform::target::Triple;
use bougie_version::version::{PartialVersion, Version};
use eyre::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const SECTION_NAME: &str = "interpreter/php";

#[derive(Debug, Clone, Copy)]
pub struct Options<'a> {
    pub request: Option<&'a str>,
    pub only_installed: bool,
    pub only_available: bool,
    pub all_versions: bool,
    pub all_platforms: bool,
    pub all_arches: bool,
    pub show_urls: bool,
}

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub schema_version: u32,
    pub items: Vec<Row>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Row {
    pub version: String,
    pub flavor: String,
    pub target: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.items.is_empty() {
            writeln!(w, "no PHP interpreters installed")?;
            return Ok(());
        }
        let host = Triple::detect().ok().map(|t| t.to_string());
        let multi_target = self
            .items
            .iter()
            .any(|r| Some(&r.target) != host.as_ref());
        let keys: Vec<KeyParts<'_>> = self.items.iter().map(|r| row_key(r, multi_target)).collect();
        let pad = keys.iter().map(KeyParts::plain_len).max().unwrap_or(0);
        for (row, key) in self.items.iter().zip(keys.iter()) {
            let suffix = match (&row.path, &row.url) {
                (Some(p), _) => Suffix::Path(p.as_path()),
                (None, Some(u)) => Suffix::Url(u.as_str()),
                (None, None) => Suffix::Placeholder,
            };
            write_row(w, key, pad, &suffix, None)?;
        }
        Ok(())
    }
}

fn row_key(row: &Row, multi_target: bool) -> KeyParts<'_> {
    if multi_target {
        KeyParts {
            prefix: Some("php-"),
            version: &row.version,
            target: Some(&row.target),
            flavor: Some(&row.flavor),
        }
    } else {
        KeyParts {
            prefix: None,
            version: &row.version,
            target: None,
            flavor: Some(&row.flavor),
        }
    }
}

pub fn run(format: OutputFormat, opts: Options<'_>) -> Result<ExitCode> {
    if opts.only_installed && opts.only_available {
        return Err(BougieError::Resolution {
            kind: "list".into(),
            detail: "--only-installed and --only-available are mutually exclusive".into(),
        }
        .into());
    }

    let paths = Paths::from_env()?;
    let host = Triple::detect()?;
    let host_str = host.to_string();
    let request = opts
        .request
        .map(parse_request)
        .transpose()?;

    let mut rows: Vec<Row> = Vec::new();

    if !opts.only_available {
        for (version_str, flavor_str) in list_installed(&paths)? {
            let Ok(version) = version_str.parse::<Version>() else {
                continue;
            };
            let Some(flavor) = parse_flavor(&flavor_str) else {
                continue;
            };
            rows.push(Row {
                version: version.to_string(),
                flavor: flavor.to_string(),
                target: host_str.clone(),
                status: "installed",
                path: Some(install_dir(&paths, version, flavor)),
                url: None,
            });
        }
    }

    if !opts.only_installed {
        let available = fetch_available(&paths, &host, &opts)?;
        for row in available {
            // Don't double-count: skip rows whose (version, flavor, target)
            // already appears as installed.
            if rows
                .iter()
                .any(|r| r.version == row.version && r.flavor == row.flavor && r.target == row.target)
            {
                continue;
            }
            rows.push(row);
        }
    }

    if let Some(req) = &request {
        rows.retain(|r| matches_request(r, req));
    }

    if !opts.all_versions {
        rows = collapse_to_latest_per_minor(rows);
    }

    rows.sort_by(|a, b| {
        let av: Version = a.version.parse().unwrap_or(Version::new(0, 0, 0));
        let bv: Version = b.version.parse().unwrap_or(Version::new(0, 0, 0));
        bv.cmp(&av)
            .then_with(|| a.flavor.cmp(&b.flavor))
            .then_with(|| a.target.cmp(&b.target))
    });

    let result = ListResult { schema_version: 1, items: rows };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn fetch_available(paths: &Paths, host: &Triple, opts: &Options<'_>) -> Result<Vec<Row>> {
    let host_str = host.to_string();
    let url = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| BougieError::Network {
            operation: "building HTTP client".into(),
            detail: e.to_string(),
        })?;
    let cache_root = paths.cache_index(&host_to_dirname(&url));
    let fetched = fetch_root(&client, &url, &cache_root, build_verifier)?;

    let host_os_env = (host.os, host.env);
    let mut targets: Vec<&String> = Vec::new();
    for triple_str in fetched.root.targets.keys() {
        if opts.all_platforms {
            targets.push(triple_str);
        } else if opts.all_arches {
            // Match host's OS+libc, vary arch.
            if let Some(t) = parse_triple(triple_str)
                && (t.os, t.env) == host_os_env
            {
                targets.push(triple_str);
            }
        } else if triple_str == &host_str {
            targets.push(triple_str);
        }
    }

    let mut rows: Vec<Row> = Vec::new();
    for triple_str in targets {
        let target_entry = match fetched.root.targets.get(triple_str) {
            Some(t) => t,
            None => continue,
        };
        let Some(section_ref) = target_entry.sections.get(SECTION_NAME) else {
            continue;
        };
        let section = fetch_section(
            &client,
            &url,
            &cache_root,
            &fetched.root.version,
            triple_str,
            SECTION_NAME,
            &section_ref.sha256,
        )?;
        for art in &section.artifacts {
            if art.yanked {
                continue;
            }
            let manifest_url = if opts.show_urls {
                Some(format!(
                    "{}{}",
                    url.trim_end_matches('/'),
                    if art.manifest.path.starts_with('/') {
                        art.manifest.path.clone()
                    } else {
                        format!("/{}", art.manifest.path)
                    }
                ))
            } else {
                None
            };
            rows.push(Row {
                version: art.version.clone(),
                flavor: art.flavor.clone(),
                target: triple_str.clone(),
                status: "available",
                path: None,
                url: manifest_url,
            });
        }
    }
    Ok(rows)
}

fn parse_flavor(s: &str) -> Option<Flavor> {
    match s {
        "nts" => Some(Flavor::Nts),
        "nts-debug" => Some(Flavor::NtsDebug),
        "zts" => Some(Flavor::Zts),
        "zts-debug" => Some(Flavor::ZtsDebug),
        _ => None,
    }
}

fn matches_request(row: &Row, req: &Request) -> bool {
    let Ok(row_v) = row.version.parse::<Version>() else {
        return false;
    };
    match req {
        Request::VersionLike { spec, flavor } => {
            if let Some(f) = flavor
                && row.flavor != f.as_str()
            {
                return false;
            }
            match spec {
                VersionLike::Version(pv) => version_matches_partial(row_v, *pv),
                VersionLike::Constraint(c) => c.satisfies(row_v),
            }
        }
        Request::FullTag { version, target, flavor } => {
            if &row.target != target {
                return false;
            }
            if let Some(f) = flavor
                && row.flavor != f.as_str()
            {
                return false;
            }
            version_matches_partial(row_v, *version)
        }
        // Path / Name requests don't make sense as filters here.
        Request::Path(_) | Request::Name(_) => false,
    }
}

fn version_matches_partial(v: Version, pv: PartialVersion) -> bool {
    if v.major != pv.major {
        return false;
    }
    if let Some(m) = pv.minor
        && v.minor != m
    {
        return false;
    }
    if let Some(p) = pv.patch
        && v.patch != p
    {
        return false;
    }
    true
}

/// Collapse `available` rows to the highest patch per (minor, flavor, target).
/// `installed` rows are always kept.
fn collapse_to_latest_per_minor(rows: Vec<Row>) -> Vec<Row> {
    let mut latest: BTreeMap<(u32, u32, String, String), Row> = BTreeMap::new();
    let mut kept: Vec<Row> = Vec::new();
    for row in rows {
        if row.status == "installed" {
            kept.push(row);
            continue;
        }
        let Ok(v) = row.version.parse::<Version>() else {
            continue;
        };
        let key = (v.major, v.minor, row.flavor.clone(), row.target.clone());
        match latest.get(&key) {
            Some(existing) => {
                let existing_v: Version =
                    existing.version.parse().unwrap_or(Version::new(0, 0, 0));
                if v > existing_v {
                    latest.insert(key, row);
                }
            }
            None => {
                latest.insert(key, row);
            }
        }
    }
    kept.extend(latest.into_values());
    kept
}

fn parse_triple(s: &str) -> Option<Triple> {
    use bougie_platform::target::{Arch, Env, Os, Vendor};
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() < 3 {
        return None;
    }
    let arch = match parts[0] {
        "x86_64" => Arch::X86_64,
        "aarch64" => Arch::Aarch64,
        _ => return None,
    };
    let vendor = match parts[1] {
        "unknown" => Vendor::Unknown,
        "apple" => Vendor::Apple,
        _ => return None,
    };
    let os = match parts[2] {
        "linux" => Os::Linux,
        "darwin" => Os::Darwin,
        _ => return None,
    };
    let env = if parts.len() >= 4 {
        match parts[3] {
            "gnu" => Some(Env::Gnu),
            "musl" => Some(Env::Musl),
            _ => return None,
        }
    } else {
        None
    };
    Some(Triple { arch, vendor, os, env })
}
