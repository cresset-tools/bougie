//! `bougie repo {add,remove}` — declare where a project's packages come from,
//! in its committed `composer.json` (the stock-Composer-visible place, so
//! teammates, CI and plain `composer` see the same repositories).
//!
//! A repository is a Composer URL; a Bougie Repo shows the command to run on
//! its index page. Credentials are not handled here: a private repository
//! takes a token via `bougie login` or Composer's own `auth.json`.

use std::path::Path;
use std::process::ExitCode;

use bougie_cli::{OutputFormat, RepoCommand};
use bougie_composer::lockfile::{RepositoriesApplied, add_repositories, remove_repository};
use eyre::{Result, bail};
use serde_json::json;

pub fn run(format: OutputFormat, cmd: RepoCommand) -> Result<ExitCode> {
    let root = std::env::current_dir()?;
    match cmd {
        RepoCommand::Add { url } => add(format, &root, &url),
        RepoCommand::Remove { url } => remove(format, &root, &url),
    }
}

fn add(format: OutputFormat, root: &Path, url: &str) -> Result<ExitCode> {
    let url = normalize_url(url)?;
    require_composer_json(root)?;
    let applied = add_repositories(root, std::slice::from_ref(&url))?;
    report(format, "add", &url, &applied, |done| {
        if done {
            format!("Added {url} to composer.json")
        } else {
            format!("{url} is already in composer.json")
        }
    })
}

fn remove(format: OutputFormat, root: &Path, url: &str) -> Result<ExitCode> {
    let url = normalize_url(url)?;
    require_composer_json(root)?;
    let applied = remove_repository(root, &url)?;
    report(format, "remove", &url, &applied, |done| {
        if done {
            format!("Removed {url} from composer.json")
        } else {
            format!("{url} is not in composer.json")
        }
    })
}

fn report(
    format: OutputFormat,
    action: &str,
    url: &str,
    applied: &RepositoriesApplied,
    message: impl Fn(bool) -> String,
) -> Result<ExitCode> {
    let changed = applied.added > 0;
    if format == OutputFormat::JsonV1 {
        let out = json!({ "action": action, "url": url, "changed": changed });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("{}", message(changed));
    }
    Ok(ExitCode::SUCCESS)
}

/// An http(s) URL without a trailing slash, so `…/` and `…` are the same entry.
fn normalize_url(raw: &str) -> Result<String> {
    let url = raw.trim().trim_end_matches('/');
    let Some((scheme, rest)) = url.split_once("://") else {
        bail!("`{raw}` is not a URL: give the full address, like https://acme.bougierepo.com");
    };
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        bail!("`{raw}` is not an http(s) URL");
    }
    if rest.is_empty() {
        bail!("`{raw}` has no host");
    }
    Ok(url.to_owned())
}

fn require_composer_json(root: &Path) -> Result<()> {
    if !root.join("composer.json").is_file() {
        bail!(
            "no composer.json in {}: run this in a project, or create one with `bougie init`",
            root.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn urls_lose_their_trailing_slash_and_must_be_http() {
        assert_eq!(
            normalize_url(" https://repo.example.org/ ").unwrap(),
            "https://repo.example.org"
        );
        assert_eq!(
            normalize_url("https://host/acme/web").unwrap(),
            "https://host/acme/web"
        );
        assert!(normalize_url("repo.example.org").is_err());
        assert!(normalize_url("ftp://repo.example.org").is_err());
        assert!(normalize_url("https://").is_err());
    }
}
