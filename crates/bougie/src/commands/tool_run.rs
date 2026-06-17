//! `bougie tool run <vendor/name>[@<constraint>] [--php] [--with] -- args...`.
//!
//! Thin dispatcher. The heavy lifting (cache key, persistent-match
//! lookup, cache materialisation, exec) lives in
//! `bougie_tool::run::run`. Callbacks come from
//! `super::tool_callbacks` so the wiring is shared with install /
//! inject / upgrade.

use bougie_cli::OutputFormat;
use bougie_paths::Paths;
use bougie_tool::install::InstallContext;
use bougie_tool::{request, run};
use eyre::Result;
use std::ffi::OsString;
use std::process::ExitCode;

pub fn run(
    _format: OutputFormat,
    package: &str,
    php_spec: Option<&str>,
    with: &[String],
    args: Vec<OsString>,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let req = request::parse(package)?;
    let resolve_lock: &bougie_tool::install::LockResolver = &|paths, project_root| {
        super::composer_update::resolve_and_write_lock(paths, project_root, bougie_composer_resolver::ResolutionStrategy::Highest).map(|_| ())
    };
    let php_installer = super::tool_callbacks::php_installer();
    let classifier = super::tool_callbacks::extension_classifier();
    let ext_installer = super::tool_callbacks::extension_installer();
    let php_requirement = super::tool_callbacks::required_php_fetcher();
    let php_baseline = super::tool_callbacks::baseline_ensurer();
    let ctx = InstallContext {
        paths: &paths,
        resolve_lock,
        php_installer: php_installer.as_ref(),
        classifier: classifier.as_ref(),
        ext_installer: ext_installer.as_ref(),
        php_requirement: php_requirement.as_ref(),
        php_baseline: php_baseline.as_ref(),
    };
    // `run::run` returns `Infallible` on Unix because it execve's
    // into PHP. The `Result` carries only the prep-time / execve
    // failure mode.
    let _: std::convert::Infallible = run::run(&ctx, &req, php_spec, with, args)?;
    unreachable!("tool run execve never returns on success");
}
