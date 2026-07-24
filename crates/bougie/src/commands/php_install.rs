use bougie_cli::OutputFormat;
use bougie_installer::baseline::{BaselineFilter, parse_without};
use bougie_installer::install::{
    BaselineReport, InstalledPhp, PreinstallReport, install_baseline_into, install_php,
    preinstall_into,
};
use bougie_output::changelog::{
    ChangeKind, plural, write_change, write_change_detail, write_summary,
};
use bougie_output::list_format::writeln_dim;
use bougie_output::output::{Render, emit, verbose};
use bougie_paths::Paths;
use bougie_platform::target::Triple;
use bougie_resolver::ResolveOptions;
use bougie_version::request::{Flavor, Request, VersionLike, parse_request};
use bougie_version::version::PartialVersion;
use composer_semver::Constraint;
use eyre::{Result, eyre};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub schema_version: u32,
    /// The requests as the user typed them, in order. Empty when
    /// `bougie php install` ran bare (install the latest). Drives the
    /// uv-style "already installed" wording when nothing changed.
    pub requests: Vec<String>,
    pub installed: Vec<InstallEntry>,
    /// Wall-clock of the whole install batch, in milliseconds.
    pub elapsed_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct InstallEntry {
    /// The index tag identifying this installation —
    /// `php-<version>-<target>-<flavor>`. uv's installation key.
    pub key: String,
    pub version: String,
    pub flavor: String,
    pub path: PathBuf,
    pub already_present: bool,
    /// Names of baseline extensions installed alongside this
    /// interpreter (CLI.md §3.5.1.1). Empty when `--bare`.
    pub baseline: Vec<String>,
    /// Per-name failure detail for baseline extensions that didn't
    /// install. The interpreter is still considered installed; the
    /// next `bougie sync` retries.
    pub baseline_failed: Vec<BaselineFailure>,
    /// Names of extensions pre-downloaded into the store but not
    /// enabled (currently: xdebug). Empty when `--bare`.
    pub preinstalled: Vec<String>,
    /// Per-name failure detail for preinstall, matching `baseline_failed`.
    pub preinstall_failed: Vec<BaselineFailure>,
}

#[derive(Debug, Serialize)]
pub struct BaselineFailure {
    pub name: String,
    pub reason: String,
}

impl InstallEntry {
    /// Anything worth printing under this entry's row: the two failure
    /// lists always, the installed-set listings only under `--verbose`.
    fn has_details(&self) -> bool {
        !self.baseline_failed.is_empty()
            || !self.preinstall_failed.is_empty()
            || (verbose() && (!self.baseline.is_empty() || !self.preinstalled.is_empty()))
    }

    fn write_details(&self, w: &mut dyn Write) -> io::Result<()> {
        // The baseline set is ~two dozen names and is re-affirmed on
        // every install, so it's `--verbose` detail rather than news.
        // Failures are always news: the interpreter is usable but
        // incomplete until the next `bougie sync` retries.
        if verbose() && !self.baseline.is_empty() {
            write_change_detail(w, &format!("baseline: {}", self.baseline.join(", ")))?;
        }
        for failure in &self.baseline_failed {
            write_change_detail(
                w,
                &format!(
                    "baseline failed: {} — {} (next `bougie sync` will retry)",
                    failure.name, failure.reason
                ),
            )?;
        }
        if verbose() && !self.preinstalled.is_empty() {
            write_change_detail(
                w,
                &format!(
                    "pre-downloaded (inactive): {}",
                    self.preinstalled.join(", ")
                ),
            )?;
        }
        for failure in &self.preinstall_failed {
            write_change_detail(
                w,
                &format!(
                    "preinstall failed: {} — {} (next `bougie sync` will retry)",
                    failure.name, failure.reason
                ),
            )?;
        }
        Ok(())
    }
}

/// uv's `python install` shape: one dimmed summary line, then a green
/// `+` row per installation that actually landed. Versions that were
/// already on disk aren't news, so they get no row — only the
/// "already installed" message when *nothing* changed.
impl Render for InstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let added: Vec<&InstallEntry> = self
            .installed
            .iter()
            .filter(|e| !e.already_present)
            .collect();

        if let [only] = added.as_slice() {
            write_summary(
                w,
                "Installed",
                &format!("PHP {}", only.version),
                self.elapsed_ms,
            )?;
        } else if added.is_empty() {
            match self.requests.as_slice() {
                [] => writeln_dim(
                    w,
                    "PHP is already installed. Use `bougie php install <request>` to install another version.",
                )?,
                [one] => writeln_dim(w, &format!("{one} is already installed"))?,
                _ => writeln_dim(w, "All requested versions already installed")?,
            }
        } else {
            let n = added.len();
            write_summary(
                w,
                "Installed",
                &format!("{n} {}", plural(n as u64, "version", "versions")),
                self.elapsed_ms,
            )?;
        }

        for entry in &self.installed {
            if entry.already_present {
                // No `+` row for an untouched interpreter — but if its
                // baseline reported something, anchor that detail to a
                // key so the reader knows which install it belongs to.
                if !entry.has_details() {
                    continue;
                }
                writeln_dim(w, &format!(" = {} (already installed)", entry.key))?;
            } else {
                write_change(w, ChangeKind::Added, &entry.key, None)?;
            }
            entry.write_details(w)?;
        }
        Ok(())
    }
}

pub fn run(
    format: OutputFormat,
    request_strs: &[String],
    flavor_arg: Option<&str>,
    bare: bool,
    without: &[String],
) -> Result<ExitCode> {
    let flavor = match flavor_arg {
        Some(s) => Some(parse_flavor(s)?),
        None => None,
    };
    let baseline_filter = resolve_baseline_filter(bare, without)?;
    let paths = Paths::from_env()?;

    let requests: Vec<Request> = if request_strs.is_empty() {
        vec![default_latest_request()]
    } else {
        request_strs
            .iter()
            .map(|s| parse_request(s))
            .collect::<Result<_>>()?
    };

    // Installs always target the host, so one detection covers the
    // whole batch. The triple is the middle field of the index tag we
    // report as each entry's key.
    let target = Triple::detect()?.to_string();
    let started = Instant::now();
    let mut installed = Vec::with_capacity(requests.len());
    for request in &requests {
        let info: InstalledPhp = install_php(&paths, request, flavor, ResolveOptions::default())?;
        let php_minor = PartialVersion {
            major: info.version.major,
            minor: Some(info.version.minor),
            patch: None,
        };
        // Baseline install runs *after* install_php returns so the
        // global lock has been released — install_extension acquires
        // the same lock and nesting would deadlock. install.rs
        // documents this constraint on install_baseline_into.
        let report: BaselineReport = install_baseline_into(
            &paths,
            &info.install_path,
            php_minor,
            info.flavor,
            &baseline_filter,
            ResolveOptions::default(),
        );
        // Pre-download (without enabling) extensions like xdebug so
        // the first server-side debug request doesn't stall on a
        // download. Skipped under `--bare` so that flag still
        // produces a minimal install.
        let preinstall: PreinstallReport = if matches!(baseline_filter, BaselineFilter::None) {
            PreinstallReport::default()
        } else {
            preinstall_into(
                &paths,
                &info.install_path,
                php_minor,
                info.flavor,
                ResolveOptions::default(),
            )
        };
        installed.push(InstallEntry {
            key: install_key(&info.version.to_string(), &target, &info.flavor.to_string()),
            version: info.version.to_string(),
            flavor: info.flavor.to_string(),
            path: info.install_path,
            already_present: info.already_present,
            baseline: report.installed,
            baseline_failed: report
                .failed
                .into_iter()
                .map(|(name, reason)| BaselineFailure { name, reason })
                .collect(),
            preinstalled: preinstall.installed,
            preinstall_failed: preinstall
                .failed
                .into_iter()
                .map(|(name, reason)| BaselineFailure { name, reason })
                .collect(),
        });
    }

    let result = InstallResult {
        schema_version: 1,
        requests: request_strs.to_vec(),
        installed,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// The index tag for one interpreter — `php-8.3.12-<target>-nts`. Same
/// shape the index publishes (`manifest.tag`) and what `bougie php list`
/// shows in its multi-target mode, so the key a user sees on install is
/// the key they can look up afterwards.
fn install_key(version: &str, target: &str, flavor: &str) -> String {
    format!("php-{version}-{target}-{flavor}")
}

fn resolve_baseline_filter(bare: bool, without: &[String]) -> Result<BaselineFilter> {
    if bare && !without.is_empty() {
        // clap's conflicts_with usually catches this, but the resolver
        // is the second line of defense — callers pass slices directly
        // from tests.
        return Err(eyre!("--bare and --without are mutually exclusive"));
    }
    if bare {
        return Ok(BaselineFilter::None);
    }
    parse_without(without).map_err(|m| eyre!("{m}"))
}

/// `*` — match anything (highest non-yanked overall). Used when the
/// user runs `bougie php install` with no argument.
fn default_latest_request() -> Request {
    Request::VersionLike {
        spec: VersionLike::Constraint(
            Constraint::parse("*").expect("static constraint string is valid"),
        ),
        flavor: None,
    }
}

fn parse_flavor(s: &str) -> Result<Flavor> {
    Ok(match s {
        "nts" => Flavor::Nts,
        "nts-debug" => Flavor::NtsDebug,
        "zts" => Flavor::Zts,
        "zts-debug" => Flavor::ZtsDebug,
        other => return Err(eyre!("unknown flavor: {other}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str, already_present: bool) -> InstallEntry {
        InstallEntry {
            key: install_key(version, "x86_64-unknown-linux-gnu", "nts"),
            version: version.into(),
            flavor: "nts".into(),
            path: PathBuf::from(format!("/store/php/{version}-nts")),
            already_present,
            baseline: vec!["bcmath".into(), "intl".into()],
            baseline_failed: Vec::new(),
            preinstalled: vec!["xdebug".into()],
            preinstall_failed: Vec::new(),
        }
    }

    fn result(requests: &[&str], installed: Vec<InstallEntry>) -> InstallResult {
        InstallResult {
            schema_version: 1,
            requests: requests.iter().map(|s| (*s).to_string()).collect(),
            installed,
            elapsed_ms: 3420.0,
        }
    }

    /// Render to text with the SGR codes dropped — what a user sees on
    /// a non-color terminal, where `emit`'s `AutoStream` strips them.
    fn render(result: &InstallResult) -> String {
        let mut buf = Vec::new();
        result.render_text(&mut buf).unwrap();
        let styled = String::from_utf8(buf).unwrap();
        let mut out = String::with_capacity(styled.len());
        let mut in_escape = false;
        for c in styled.chars() {
            match c {
                '\u{1b}' => in_escape = true,
                'm' if in_escape => in_escape = false,
                _ if in_escape => {}
                _ => out.push(c),
            }
        }
        out
    }

    #[test]
    fn multiple_installs_render_uv_style() {
        let out = render(&result(
            &["8.2", "8.3", "8.4"],
            vec![
                entry("8.2.29", false),
                entry("8.3.24", false),
                entry("8.4.11", false),
            ],
        ));
        assert_eq!(
            out,
            "Installed 3 versions in 3.42s\n \
             + php-8.2.29-x86_64-unknown-linux-gnu-nts\n \
             + php-8.3.24-x86_64-unknown-linux-gnu-nts\n \
             + php-8.4.11-x86_64-unknown-linux-gnu-nts\n"
        );
    }

    #[test]
    fn single_install_names_the_version() {
        let out = render(&result(&["8.3"], vec![entry("8.3.24", false)]));
        assert_eq!(
            out,
            "Installed PHP 8.3.24 in 3.42s\n + php-8.3.24-x86_64-unknown-linux-gnu-nts\n"
        );
    }

    #[test]
    fn already_present_versions_get_no_row() {
        let out = render(&result(
            &["8.2", "8.3"],
            vec![entry("8.2.29", true), entry("8.3.24", false)],
        ));
        assert_eq!(
            out,
            "Installed PHP 8.3.24 in 3.42s\n + php-8.3.24-x86_64-unknown-linux-gnu-nts\n"
        );
    }

    #[test]
    fn nothing_to_do_wording_follows_the_request_count() {
        let bare = render(&result(&[], vec![entry("8.3.24", true)]));
        assert_eq!(
            bare,
            "PHP is already installed. Use `bougie php install <request>` to install another version.\n"
        );

        let one = render(&result(&["8.3"], vec![entry("8.3.24", true)]));
        assert_eq!(one, "8.3 is already installed\n");

        let many = render(&result(
            &["8.2", "8.3"],
            vec![entry("8.2.29", true), entry("8.3.24", true)],
        ));
        assert_eq!(many, "All requested versions already installed\n");
    }

    #[test]
    fn baseline_failures_stay_visible_under_their_row() {
        let mut fresh = entry("8.3.24", false);
        fresh.baseline_failed.push(BaselineFailure {
            name: "intl".into(),
            reason: "download failed".into(),
        });
        let mut stale = entry("8.2.29", true);
        stale.baseline_failed.push(BaselineFailure {
            name: "gd".into(),
            reason: "no manifest".into(),
        });

        let out = render(&result(&["8.2", "8.3"], vec![stale, fresh]));
        assert_eq!(
            out,
            "Installed PHP 8.3.24 in 3.42s\n\
             \x20= php-8.2.29-x86_64-unknown-linux-gnu-nts (already installed)\n\
             \x20  baseline failed: gd — no manifest (next `bougie sync` will retry)\n\
             \x20+ php-8.3.24-x86_64-unknown-linux-gnu-nts\n\
             \x20  baseline failed: intl — download failed (next `bougie sync` will retry)\n"
        );
    }

    #[test]
    fn install_key_matches_the_index_tag() {
        assert_eq!(
            install_key("8.3.24", "aarch64-apple-darwin", "zts"),
            "php-8.3.24-aarch64-apple-darwin-zts"
        );
    }

    #[test]
    fn baseline_filter_defaults_to_all() {
        match resolve_baseline_filter(false, &[]).unwrap() {
            BaselineFilter::All => {}
            other => panic!("expected All, got {other:?}"),
        }
    }

    #[test]
    fn bare_flag_disables_set() {
        match resolve_baseline_filter(true, &[]).unwrap() {
            BaselineFilter::None => {}
            other => panic!("expected None, got {other:?}"),
        }
    }

    #[test]
    fn without_excludes_named() {
        match resolve_baseline_filter(false, &["opcache".into(), "readline".into()]).unwrap() {
            BaselineFilter::Without(set) => {
                assert!(set.contains("opcache"));
                assert!(set.contains("readline"));
                assert!(!set.contains("calendar"));
            }
            other => panic!("expected Without(..), got {other:?}"),
        }
    }

    #[test]
    fn bare_and_without_conflict() {
        assert!(resolve_baseline_filter(true, &["opcache".into()]).is_err());
    }
}
