//! Minimal git plumbing for VCS/source installs (`RESOLVER_PLAN.md`
//! Phase D). bougie shells out to the user's `git` — exactly what
//! Composer does — so ssh keys, credential helpers, and proxies work with
//! zero extra configuration. A `git`-on-PATH dependency is incurred only
//! when a project actually has a git `source` to install.
//!
//! Repositories are cached as bare `--mirror` clones under
//! `$BOUGIE_CACHE/composer-vcs/<sanitized-url>` and refreshed only when a
//! wanted revision is missing, so a pinned-sha install from a warm cache
//! makes no network round-trip.

use std::path::{Path, PathBuf};
use std::process::Command;

use bougie_paths::Paths;
use eyre::{eyre, Context, Result};
use rustc_hash::FxHasher;

/// Fail early with an actionable message when `git` isn't on PATH. Called
/// once before any source install so the error is a single clear line
/// rather than one per package.
pub fn ensure_git_available() -> Result<()> {
    Command::new("git")
        .arg("--version")
        .output()
        .map_err(|e| {
            eyre!(
                "installing a package from a git `source` needs `git` on PATH, \
                 but it could not be run ({e}). Install git, or install this \
                 project's git dependencies with upstream Composer."
            )
        })
        .and_then(|out| {
            if out.status.success() {
                Ok(())
            } else {
                Err(eyre!("`git --version` failed; is git installed correctly?"))
            }
        })
}

/// Clone `url` at `reference` into `dest`, using (and refreshing) the
/// shared bare mirror. `dest` is wiped first so the checkout is pristine
/// (mirrors the dist extractor, which also clears its target). The
/// resulting tree keeps its `.git` with `origin` pointed at the real
/// `url`, so a source-installed package stays a usable git checkout.
pub fn install_source(paths: &Paths, url: &str, reference: &str, dest: &Path) -> Result<()> {
    let mirror = ensure_mirror(paths, url, reference)
        .wrap_err_with(|| format!("preparing git mirror for {url}"))?;

    // A `git clone` refuses a non-empty target, so remove any prior tree
    // and let clone recreate the directory itself.
    let _ = std::fs::remove_dir_all(dest);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }

    run_git(
        None,
        &[
            "clone".as_ref(),
            "--quiet".as_ref(),
            mirror.as_os_str(),
            dest.as_os_str(),
        ],
    )
    .wrap_err_with(|| format!("cloning {} into {}", mirror.display(), dest.display()))?;

    run_git(Some(dest), &["checkout".as_ref(), "--quiet".as_ref(), "--detach".as_ref(), reference.as_ref()])
        .wrap_err_with(|| format!("checking out {reference} for {url}"))?;

    // Point origin back at the real remote (the clone set it to the local
    // mirror). Best-effort: a failure here doesn't affect the files.
    let _ = run_git(Some(dest), &["remote".as_ref(), "set-url".as_ref(), "origin".as_ref(), url.as_ref()]);

    Ok(())
}

/// Ensure a bare `--mirror` clone of `url` exists and contains `reference`.
/// Clones on first use; otherwise fetches only when the wanted revision is
/// absent (so a warm cache with the pinned sha stays offline).
fn ensure_mirror(paths: &Paths, url: &str, reference: &str) -> Result<PathBuf> {
    let cache_root = paths.cache_composer_vcs();
    std::fs::create_dir_all(&cache_root)
        .wrap_err_with(|| format!("creating {}", cache_root.display()))?;
    let mirror = cache_root.join(mirror_dir_name(url));

    if mirror.join("HEAD").is_file() {
        // Fetch only if the wanted commit isn't already present.
        if !reference.is_empty() && !commit_present(&mirror, reference) {
            run_git(Some(&mirror), &["remote".as_ref(), "update".as_ref(), "--prune".as_ref()])
                .wrap_err_with(|| format!("refreshing mirror for {url}"))?;
        }
    } else {
        let _ = std::fs::remove_dir_all(&mirror);
        run_git(None, &["clone".as_ref(), "--mirror".as_ref(), "--quiet".as_ref(), url.as_ref(), mirror.as_os_str()])
            .wrap_err_with(|| format!("mirroring {url}"))?;
    }
    Ok(mirror)
}

/// `true` when `reference` resolves to a commit object in the mirror.
fn commit_present(mirror: &Path, reference: &str) -> bool {
    run_git(Some(mirror), &["cat-file".as_ref(), "-e".as_ref(), format!("{reference}^{{commit}}").as_ref()]).is_ok()
}

/// A filesystem-safe, collision-resistant directory name for a repo URL:
/// the sanitized URL plus a short hash so two URLs that sanitize alike
/// (e.g. `:` vs `/`) never share a mirror.
fn mirror_dir_name(url: &str) -> String {
    use std::hash::Hasher;
    let mut h = FxHasher::default();
    h.write(url.as_bytes());
    let sanitized: String = url
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    // Keep the tail (the repo name is the informative part) bounded.
    let tail: String = sanitized.chars().rev().take(60).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{tail}-{:016x}", h.finish())
}

/// Run `git [-C cwd] <args...>`, mapping a non-zero exit into an error
/// that carries git's stderr.
fn run_git(cwd: Option<&Path>, args: &[&std::ffi::OsStr]) -> Result<()> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);
    let out = cmd
        .output()
        .map_err(|e| eyre!("could not run git: {e} (is git on PATH?)"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(eyre!("git {} failed: {}", render_args(args), stderr.trim()))
    }
}

fn render_args(args: &[&std::ffi::OsStr]) -> String {
    args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests;
