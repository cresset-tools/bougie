use crate::cli::OutputFormat;
use crate::config::load_project;
use crate::errors::BougieError;
use crate::index::{
    build_verifier,
    fetch::{fetch_root, fetch_section},
    wire::Section,
};
use crate::install::{host_to_dirname, DEFAULT_INDEX_URL};
use crate::list_format::{
    pad_spaces, write_styled, FLAVOR_STYLE, SEP_STYLE, Suffix, TARGET_STYLE, VERSION_STYLE,
};
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

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub only_installed: bool,
    pub only_available: bool,
    pub all_versions: bool,
    pub all_platforms: bool,
    pub show_urls: bool,
}

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub schema_version: u32,
    pub items: Vec<Row>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Row {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub php_minor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flavor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub status: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.items.is_empty() {
            writeln!(w, "no extensions known")?;
            return Ok(());
        }
        let pad = self.items.iter().map(plain_key_len).max().unwrap_or(0);
        for row in &self.items {
            write_key(w, row)?;
            pad_spaces(w, plain_key_len(row), pad)?;
            write!(w, "  ")?;
            // `--show-urls` upgrades the suffix from status tags to the
            // dim URL. The unifying mental model: green path when
            // resolved on-disk, dim URL/placeholder when remote, dim
            // status when neither applies. Ext rows currently have no
            // resolved path (the .so location isn't carried through
            // the index data), so paths never appear here.
            let tag_refs: Vec<&str> = row.status.to_vec();
            let suffix = match &row.url {
                Some(u) => Suffix::Url(u.as_str()),
                None => Suffix::Status(&tag_refs),
            };
            suffix.write(w)?;
            writeln!(w)?;
        }
        Ok(())
    }
}

fn plain_key_len(row: &Row) -> usize {
    let mut n = row.name.len();
    if let Some(v) = &row.version {
        n += 1 + v.len();
    }
    if let Some(m) = &row.php_minor {
        n += 4 + m.replace('.', "").len(); // "+php"
    }
    if let Some(f) = &row.flavor {
        n += 1 + f.len();
    }
    if let Some(t) = &row.target {
        n += 1 + t.len();
    }
    n
}

fn write_key(w: &mut dyn Write, row: &Row) -> io::Result<()> {
    // Bold name — same role as the bold version in php list: the
    // primary identifier of the row.
    write_styled(w, VERSION_STYLE, &row.name)?;
    if let Some(v) = &row.version {
        write_styled(w, SEP_STYLE, "-")?;
        write_styled(w, TARGET_STYLE, v)?;
    }
    if let Some(m) = &row.php_minor {
        write_styled(w, SEP_STYLE, "+php")?;
        write_styled(w, TARGET_STYLE, &m.replace('.', ""))?;
    }
    if let Some(f) = &row.flavor {
        write_styled(w, SEP_STYLE, "-")?;
        write_styled(w, FLAVOR_STYLE, f)?;
    }
    if let Some(t) = &row.target {
        write_styled(w, SEP_STYLE, "-")?;
        write_styled(w, TARGET_STYLE, t)?;
    }
    Ok(())
}

/// `bougie ext list` — combine the project's required/installed extensions
/// with what the index advertises for the active or selected target(s).
///
/// Status semantics per CLI.md §3.2.3:
/// - `installed`  — `.so` is on disk under the project's resolved interpreter
///   AND the index advertises it.
/// - `shipped`    — `.so` is on disk but the index has no section for the
///   name (i.e. it ships bundled with the interpreter).
/// - `available`  — published in the index for the listed target.
/// - `local-only` — present on disk, the index has a section for the name,
///   but no non-yanked artifact for the project's resolved
///   `(php_minor, flavor)`.
/// - `required`   — listed in `composer.json`'s `require.ext-*`. Tag, not
///   exclusive.
pub fn run(format: OutputFormat, field: Option<&str>, opts: Options) -> Result<ExitCode> {
    if opts.only_installed && opts.only_available {
        return Err(BougieError::Resolution {
            kind: "list".into(),
            detail: "--only-installed and --only-available are mutually exclusive".into(),
        }
        .into());
    }

    let project_root = std::env::current_dir()?;
    let project = load_project(&project_root)?;
    let required: BTreeSet<String> = project
        .composer
        .map(|c| c.require_extensions.into_iter().collect())
        .unwrap_or_default();

    let resolved = read_project_resolved(&project_root).ok();
    // Always compute the installed set, even under --only-available:
    // the filter restricts which rows we keep, but each retained row
    // should still carry its accurate disk-state markers. Hiding the
    // `installed` tag forces the user to cross-reference a second
    // command (`bougie ext list --only-installed`) to find what they
    // already have.
    let installed: BTreeSet<String> = list_installed(&project_root, resolved.as_ref())?;

    let mut rows: Vec<Row> = Vec::new();

    if opts.only_installed {
        // Disk-only view, no index fetch.
        let mut names: BTreeSet<String> = installed.iter().cloned().collect();
        names.extend(required.iter().cloned());
        for name in names {
            let mut status = Vec::new();
            if required.contains(&name) {
                status.push("required");
            }
            if installed.contains(&name) {
                status.push("installed");
            }
            rows.push(Row {
                name,
                version: None,
                php_minor: None,
                flavor: None,
                target: None,
                status,
                url: None,
            });
        }
    } else {
        let host = Triple::detect()?;
        let host_str = host.to_string();
        let url = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());
        let paths = Paths::from_env()?;
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|e| BougieError::Network {
                operation: "building HTTP client".into(),
                detail: e.to_string(),
            })?;
        let cache_root = paths.cache_index(&host_to_dirname(&url));
        let fetched = fetch_root(&client, &url, &cache_root, build_verifier)?;

        let targets: Vec<String> = if opts.all_platforms {
            fetched.root.targets.keys().cloned().collect()
        } else {
            vec![host_str.clone()]
        };

        let need_section_fetch = opts.all_versions || opts.show_urls;
        let mut available_names_host: BTreeSet<String> = BTreeSet::new();
        // Whether we found a non-yanked artifact matching the project's
        // (php_minor, flavor). Used to distinguish `installed` vs `local-only`.
        let mut matches_resolved: BTreeSet<String> = BTreeSet::new();

        for target_str in &targets {
            let Some(target_entry) = fetched.root.targets.get(target_str) else {
                continue;
            };
            let target_label = if opts.all_platforms {
                Some(target_str.clone())
            } else {
                None
            };
            let is_host = target_str == &host_str;

            for section_name in target_entry.sections.keys() {
                let Some(ext_name) = section_name.strip_prefix(EXTENSION_PREFIX) else {
                    continue;
                };
                if is_host {
                    available_names_host.insert(ext_name.to_owned());
                }

                if need_section_fetch {
                    let section_ref = &target_entry.sections[section_name];
                    let section = fetch_section(
                        &client,
                        &url,
                        &cache_root,
                        &fetched.root.version,
                        target_str,
                        section_name,
                        &section_ref.sha256,
                    )?;
                    if opts.all_versions {
                        for art in &section.artifacts {
                            if art.yanked {
                                continue;
                            }
                            if is_host && resolved_matches(art, resolved.as_ref()) {
                                matches_resolved.insert(ext_name.to_owned());
                            }
                            push_index_row(
                                &mut rows,
                                ext_name,
                                Some(&art.version),
                                art.php_minor.as_deref(),
                                Some(&art.flavor),
                                target_label.as_deref(),
                                is_host,
                                resolved.as_ref(),
                                &required,
                                &installed,
                                opts.show_urls.then(|| build_manifest_url(&url, &art.manifest.path)),
                                art,
                            );
                        }
                    } else if let Some(art) = pick_latest(&section, resolved.as_ref()) {
                        if is_host && resolved_matches(art, resolved.as_ref()) {
                            matches_resolved.insert(ext_name.to_owned());
                        }
                        push_index_row(
                            &mut rows,
                            ext_name,
                            None,
                            None,
                            None,
                            target_label.as_deref(),
                            is_host,
                            resolved.as_ref(),
                            &required,
                            &installed,
                            opts.show_urls.then(|| build_manifest_url(&url, &art.manifest.path)),
                            art,
                        );
                    }
                } else {
                    // Cheap path: section keys only. We can't tell whether
                    // a non-yanked artifact exists for the project's
                    // resolved (php_minor, flavor) without fetching the
                    // section, so assume it does — `installed` if also on
                    // disk, `available` otherwise.
                    if is_host && installed.contains(ext_name) {
                        matches_resolved.insert(ext_name.to_owned());
                    }
                    push_index_row_cheap(
                        &mut rows,
                        ext_name,
                        target_label.as_deref(),
                        is_host,
                        &required,
                        &installed,
                    );
                }
            }
        }

        // Tack on names the user `require`s that the index does not advertise.
        for name in required.difference(&available_names_host) {
            if installed.contains(name) {
                continue;
            }
            rows.push(Row {
                name: name.clone(),
                version: None,
                php_minor: None,
                flavor: None,
                target: None,
                status: vec!["required"],
                url: None,
            });
        }

        // shipped: on disk, name has no section in the index (interpreter
        // bundle). local-only: on disk, section exists but no matching
        // artifact for the project's resolved (php_minor, flavor).
        for name in &installed {
            let in_index = available_names_host.contains(name);
            let matched = matches_resolved.contains(name);
            if in_index && matched {
                // already pushed by push_index_row with `installed` tag.
                continue;
            }
            let mut status = Vec::new();
            if required.contains(name) {
                status.push("required");
            }
            if in_index {
                status.push("local-only");
            } else {
                status.push("shipped");
            }
            rows.push(Row {
                name: name.clone(),
                version: None,
                php_minor: None,
                flavor: None,
                target: None,
                status,
                url: None,
            });
        }
    }

    if opts.only_available {
        // "Only rows the index advertises" — installed *or* not. Keeps
        // the `installed` marker on rows that are on disk so the user
        // sees coverage at a glance without re-running with a different
        // flag. Excludes `shipped` (bundled with the interpreter, never
        // in the index) and `local-only` (in the index but no artifact
        // matches the project's resolved php_minor + flavor).
        rows.retain(|r| r.status.contains(&"available"));
    }

    rows.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.target.cmp(&b.target))
            .then_with(|| a.php_minor.cmp(&b.php_minor))
            .then_with(|| a.flavor.cmp(&b.flavor))
            .then_with(|| a.version.cmp(&b.version))
    });

    let result = ListResult { schema_version: 1, items: rows };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
fn push_index_row(
    rows: &mut Vec<Row>,
    name: &str,
    version: Option<&str>,
    php_minor: Option<&str>,
    flavor: Option<&str>,
    target: Option<&str>,
    is_host: bool,
    resolved: Option<&(String, String)>,
    required: &BTreeSet<String>,
    installed: &BTreeSet<String>,
    url: Option<String>,
    artifact: &crate::index::wire::Artifact,
) {
    let mut status = Vec::new();
    if required.contains(name) {
        status.push("required");
    }
    let installed_for_this_row = is_host
        && installed.contains(name)
        && resolved_matches(artifact, resolved);
    if installed_for_this_row {
        status.push("installed");
    }
    status.push("available");
    rows.push(Row {
        name: name.to_owned(),
        version: version.map(str::to_owned),
        php_minor: php_minor.map(str::to_owned),
        flavor: flavor.map(str::to_owned),
        target: target.map(str::to_owned),
        status,
        url,
    });
}

fn push_index_row_cheap(
    rows: &mut Vec<Row>,
    name: &str,
    target: Option<&str>,
    is_host: bool,
    required: &BTreeSet<String>,
    installed: &BTreeSet<String>,
) {
    let mut status = Vec::new();
    if required.contains(name) {
        status.push("required");
    }
    if is_host && installed.contains(name) {
        status.push("installed");
    }
    status.push("available");
    rows.push(Row {
        name: name.to_owned(),
        version: None,
        php_minor: None,
        flavor: None,
        target: target.map(str::to_owned),
        status,
        url: None,
    });
}

fn resolved_matches(
    art: &crate::index::wire::Artifact,
    resolved: Option<&(String, String)>,
) -> bool {
    let Some((v, f)) = resolved else {
        return true;
    };
    if art.flavor != *f {
        return false;
    }
    let mut parts = v.split('.');
    let Some(major) = parts.next() else { return true };
    let Some(minor) = parts.next() else { return true };
    let target_minor = format!("{major}.{minor}");
    art.php_minor.as_deref().is_none_or(|m| m == target_minor)
}

fn build_manifest_url(host_base: &str, manifest_path: &str) -> String {
    let base = host_base.trim_end_matches('/');
    if manifest_path.starts_with('/') {
        format!("{base}{manifest_path}")
    } else {
        format!("{base}/{manifest_path}")
    }
}

fn pick_latest<'a>(
    section: &'a Section,
    resolved: Option<&(String, String)>,
) -> Option<&'a crate::index::wire::Artifact> {
    let mut best: Option<(&crate::index::wire::Artifact, Vec<u32>)> = None;
    for art in &section.artifacts {
        if art.yanked {
            continue;
        }
        if !resolved_matches(art, resolved) {
            continue;
        }
        let parsed = parse_version_components(&art.version);
        match &best {
            None => best = Some((art, parsed)),
            Some((_, prev)) if parsed > *prev => best = Some((art, parsed)),
            _ => {}
        }
    }
    best.map(|(art, _)| art)
}

fn parse_version_components(s: &str) -> Vec<u32> {
    s.split('.').filter_map(|c| c.parse().ok()).collect()
}

/// Union of two enablement signals: extensions bundled with the PHP
/// install (under `<install>/lib/extensions/<api>/<name>.so`) and
/// extensions the project has explicitly enabled via a bougie-written
/// conf.d fragment (`<project>/.bougie/conf.d/20-<name>.ini`).
///
/// Skipping the conf.d half would silently drop every `bougie ext add`
/// from `--only-installed` and from the `installed` marker under
/// `--only-available`, defeating the at-a-glance coverage view.
fn list_installed(
    project_root: &Path,
    resolved: Option<&(String, String)>,
) -> Result<BTreeSet<String>> {
    let mut names: BTreeSet<String> = BTreeSet::new();

    // Bundled: shipped with the PHP install. Only present once synced.
    if let Some((version, flavor)) = resolved {
        let paths = Paths::from_env()?;
        let ext_root = paths
            .installs()
            .join(format!("{version}-{flavor}"))
            .join("lib")
            .join("extensions");
        for api_dir in dir_entries(&ext_root) {
            for so in dir_entries(&api_dir.path()) {
                if let Some(stem) = stem_if_ext(&so.path(), "so") {
                    names.insert(stem.to_owned());
                }
            }
        }
    }

    // User-installed: `bougie ext add` writes `20-<name>.ini` (CLI.md
    // §6.2). The `00-XX-*.ini` files are bundled-conf mirrors and
    // surface via the path above.
    for entry in dir_entries(&project_root.join(".bougie").join("conf.d")) {
        let p = entry.path();
        if let Some(stem) = stem_if_ext(&p, "ini")
            && let Some(name) = stem.strip_prefix("20-")
        {
            names.insert(name.to_owned());
        }
    }

    Ok(names)
}

/// Iterate a directory's entries, treating a missing directory as
/// empty. Read errors yield nothing — these scans tolerate noise.
fn dir_entries(p: &Path) -> impl Iterator<Item = std::fs::DirEntry> {
    std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
}

/// `Some(stem)` if `p` ends in `.<ext>`. Both the extension match and
/// the stem extraction must succeed, and the stem must be UTF-8 —
/// matches the prior inline check exactly.
fn stem_if_ext<'a>(p: &'a Path, ext: &str) -> Option<&'a str> {
    if p.extension().and_then(|e| e.to_str()) != Some(ext) {
        return None;
    }
    p.file_stem().and_then(|s| s.to_str())
}
