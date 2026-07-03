//! `bougie tool run [--php] [--with] [--no-project]
//! <vendor/name>[@<constraint>] args...`.
//!
//! Thin dispatcher. The heavy lifting (cache key, persistent-match
//! lookup, cache materialisation, exec) lives in
//! `bougie_tool::run::run`. Callbacks come from
//! `super::tool_callbacks` so the wiring is shared with install /
//! inject / upgrade.
//!
//! `command` arrives as one `trailing_var_arg` list: the first element
//! is the tool package, everything after is forwarded to the tool
//! verbatim (no `--` needed — bougie's own options must precede the
//! package).
//!
//! Unless `--no-project` is passed, the surrounding PHP project (if
//! any) contributes context via `super::tool_project::detect`: its
//! PHP narrows interpreter selection (tool ∩ project, tool winning on
//! conflict) and its required/inferred extensions join the effective
//! extension set. See `tool_project.rs` for the derivation rules.
//! This is unique to the ephemeral lane — `bougie tool install` stays
//! project-blind.

use bougie_cli::OutputFormat;
use bougie_paths::Paths;
use bougie_tool::install::InstallContext;
use bougie_tool::{request, run};
use eyre::{Result, eyre};
use std::ffi::OsString;
use std::process::ExitCode;

pub fn run(
    _format: OutputFormat,
    php_spec: Option<&str>,
    with: &[String],
    no_project: bool,
    mut command: Vec<OsString>,
) -> Result<ExitCode> {
    // clap enforces `required = true`, so `command` is non-empty; keep a
    // defensive check rather than indexing.
    if command.is_empty() {
        return Err(eyre!("missing tool package — usage: bgx [OPTIONS] <PACKAGE> [ARGS]..."));
    }
    let package_os = command.remove(0);
    let package = package_os
        .to_str()
        .ok_or_else(|| eyre!("tool package name is not valid UTF-8: {package_os:?}"))?;
    let args = command;
    let paths = Paths::from_env()?;
    let req = request::parse(package)?;
    let project = if no_project {
        None
    } else {
        super::tool_project::detect()
    };
    let resolve_lock: &bougie_tool::install::LockResolver = &|paths, project_root| {
        super::composer_update::resolve_and_write_lock(paths, project_root, bougie_composer_resolver::ResolutionStrategy::Highest).map(|_| ())
    };
    let php_installer = super::tool_callbacks::php_installer();
    let classifier = super::tool_callbacks::extension_classifier();
    let ext_installer = super::tool_callbacks::extension_installer();
    let tool_requires = super::tool_callbacks::tool_requires_fetcher();
    let php_baseline = super::tool_callbacks::baseline_ensurer();
    let ctx = InstallContext {
        paths: &paths,
        resolve_lock,
        php_installer: php_installer.as_ref(),
        classifier: classifier.as_ref(),
        ext_installer: ext_installer.as_ref(),
        tool_requires: tool_requires.as_ref(),
        php_baseline: php_baseline.as_ref(),
    };
    // `run::run` returns `Infallible` on Unix because it execve's
    // into PHP. The `Result` carries only the prep-time / execve
    // failure mode.
    let _: std::convert::Infallible =
        run::run(&ctx, &req, php_spec, with, project.as_ref(), args)?;
    unreachable!("tool run execve never returns on success");
}
