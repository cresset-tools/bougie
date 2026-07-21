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

use bougie_errors::BougieError;
use bougie_paths::Paths;
use eyre::{Context, Result};
use rustc_hash::FxHasher;

/// A [`BougieError::Vcs`] for a `git` binary that couldn't be run at all.
fn git_missing(operation: &str) -> eyre::Report {
    BougieError::Vcs {
        operation: operation.to_owned(),
        url: String::new(),
        detail: "`git` could not be run".to_owned(),
        hint: "install git and make sure it's on PATH — bougie shells out to your \
               system git for VCS/source packages, or install this project's git \
               dependencies with upstream Composer"
            .to_owned(),
    }
    .into()
}

/// Classify a non-zero `git` exit from its stderr into a
/// [`BougieError::Vcs`] with an actionable hint: private-repo auth,
/// a missing ref, an unreachable repo, or a generic fallback.
fn classify_git(operation: &str, url: &str, stderr: &str) -> eyre::Report {
    let s = stderr.to_ascii_lowercase();
    let hint = if s.contains("authentication failed")
        || s.contains("could not read username")
        || s.contains("could not read password")
        || s.contains("permission denied")
        || s.contains("access denied")
        || s.contains("403")
        || s.contains("terminal prompts disabled")
    {
        "the repository is private or the credentials are wrong. bougie uses your \
         system git — configure a git credential helper or ssh key, or run `bougie login`"
    } else if s.contains("couldn't find remote ref")
        || s.contains("did not match any")
        || s.contains("reference is not a tree")
        || s.contains("unable to read tree")
        || s.contains("not a valid object name")
        || s.contains("bad object")
        || s.contains("pathspec")
    {
        "the requested commit/ref is not on the remote — the lock may point at an \
         unpushed or force-pushed commit; re-run `bougie update` to re-resolve"
    } else if s.contains("repository not found")
        || s.contains("could not read from remote repository")
        || s.contains("not found")
    {
        "the repository could not be reached — check the URL and your network/access"
    } else {
        "see git's output above"
    };
    BougieError::Vcs {
        operation: operation.to_owned(),
        url: url.to_owned(),
        detail: stderr.trim().to_owned(),
        hint: hint.to_owned(),
    }
    .into()
}

/// Fail early with an actionable message when `git` isn't on PATH. Called
/// once before any source install so the error is a single clear line
/// rather than one per package.
pub fn ensure_git_available() -> Result<()> {
    match Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        _ => Err(git_missing("invocation")),
    }
}

/// Clone `url` at `reference` into `dest`, using (and refreshing) the
/// shared bare mirror. `dest` is wiped first so the checkout is pristine
/// (mirrors the dist extractor, which also clears its target). The
/// resulting tree keeps its `.git` with `origin` pointed at the real
/// `url`, so a source-installed package stays a usable git checkout.
pub fn install_source(paths: &Paths, url: &str, reference: &str, dest: &Path) -> Result<()> {
    let mirror = ensure_mirror(paths, url, reference)?;

    // A `git clone` refuses a non-empty target, so remove any prior tree
    // and let clone recreate the directory itself.
    let _ = std::fs::remove_dir_all(dest);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }

    run_git(
        "clone",
        url,
        None,
        &["clone".as_ref(), "--quiet".as_ref(), mirror.as_os_str(), dest.as_os_str()],
    )?;

    run_git(
        "checkout",
        url,
        Some(dest),
        &["checkout".as_ref(), "--quiet".as_ref(), "--detach".as_ref(), reference.as_ref()],
    )?;

    // Point origin back at the real remote (the clone set it to the local
    // mirror). Best-effort: a failure here doesn't affect the files.
    let _ = run_git(
        "set-url",
        url,
        Some(dest),
        &["remote".as_ref(), "set-url".as_ref(), "origin".as_ref(), url.as_ref()],
    );

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
            run_git("fetch", url, Some(&mirror), &["remote".as_ref(), "update".as_ref(), "--prune".as_ref()])?;
        }
    } else {
        let _ = std::fs::remove_dir_all(&mirror);
        run_git("clone", url, None, &["clone".as_ref(), "--mirror".as_ref(), "--quiet".as_ref(), url.as_ref(), mirror.as_os_str()])?;
    }
    Ok(mirror)
}

/// `true` when `reference` resolves to a commit object in the mirror.
fn commit_present(mirror: &Path, reference: &str) -> bool {
    run_git("cat-file", "", Some(mirror), &["cat-file".as_ref(), "-e".as_ref(), format!("{reference}^{{commit}}").as_ref()]).is_ok()
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

/// Run `git [-C cwd] <args...>`, mapping a spawn failure to a
/// git-not-installed error and a non-zero exit to a classified
/// [`BougieError::Vcs`] (`operation`/`url` label the failing step).
fn run_git(
    operation: &str,
    url: &str,
    cwd: Option<&Path>,
    args: &[&std::ffi::OsStr],
) -> Result<()> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);
    let out = cmd.output().map_err(|_| git_missing(operation))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(classify_git(operation, url, &String::from_utf8_lossy(&out.stderr)))
    }
}

/// Like [`run_git`] but returns captured stdout on success.
fn git_output(
    operation: &str,
    url: &str,
    cwd: Option<&Path>,
    args: &[&std::ffi::OsStr],
) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);
    let out = cmd.output().map_err(|_| git_missing(operation))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(classify_git(operation, url, &String::from_utf8_lossy(&out.stderr)))
    }
}

/// Ensure a fully-refreshed bare `--mirror` clone of `url`: clone on first
/// use, otherwise `git remote update --prune` to pick up new tags and
/// branches. Used for metadata discovery (which wants current refs),
/// vs. [`install_source`]'s cache which fetches only when a pinned commit
/// is missing.
pub fn refresh_mirror(paths: &Paths, url: &str) -> Result<PathBuf> {
    let cache_root = paths.cache_composer_vcs();
    std::fs::create_dir_all(&cache_root)
        .wrap_err_with(|| format!("creating {}", cache_root.display()))?;
    let mirror = cache_root.join(mirror_dir_name(url));
    if mirror.join("HEAD").is_file() {
        run_git("fetch", url, Some(&mirror), &["remote".as_ref(), "update".as_ref(), "--prune".as_ref()])?;
    } else {
        let _ = std::fs::remove_dir_all(&mirror);
        run_git("clone", url, None, &["clone".as_ref(), "--mirror".as_ref(), "--quiet".as_ref(), url.as_ref(), mirror.as_os_str()])?;
    }
    Ok(mirror)
}

/// A discovered git ref (tag or branch) with the commit it resolves to.
#[derive(Debug, Clone)]
pub struct GitRef {
    /// Short name — the tag (`v1.2.3`) or branch (`main`).
    pub name: String,
    /// Commit sha the ref points to (annotated tags are dereferenced to
    /// their commit).
    pub sha: String,
    pub is_tag: bool,
}

/// Enumerate a mirror's tags and branches. Annotated tags are
/// dereferenced to the commit they point at (`*objectname`).
pub fn list_refs(mirror: &Path) -> Result<Vec<GitRef>> {
    let out = git_output(
        "list-refs",
        "",
        Some(mirror),
        &[
            "for-each-ref".as_ref(),
            "--format=%(objectname) %(*objectname) %(refname)".as_ref(),
            "refs/tags".as_ref(),
            "refs/heads".as_ref(),
        ],
    )?;
    let text = String::from_utf8_lossy(&out);
    let mut refs = Vec::new();
    for line in text.lines() {
        // `%(objectname) %(*objectname) %(refname)`; the middle field is
        // empty (→ two spaces) for branches and lightweight tags.
        let mut it = line.splitn(3, ' ');
        let obj = it.next().unwrap_or("");
        let deref = it.next().unwrap_or("");
        let refname = it.next().unwrap_or("");
        let sha = if deref.is_empty() { obj } else { deref };
        if sha.is_empty() {
            continue;
        }
        let (is_tag, name) = if let Some(n) = refname.strip_prefix("refs/tags/") {
            (true, n)
        } else if let Some(n) = refname.strip_prefix("refs/heads/") {
            (false, n)
        } else {
            continue;
        };
        refs.push(GitRef { name: name.to_owned(), sha: sha.to_owned(), is_tag });
    }
    Ok(refs)
}

/// Read a file's bytes at a given commit without a working tree
/// (`git show <sha>:<path>`). Returns `Ok(None)` when the path doesn't
/// exist at that ref (or can't be read) so the caller can skip it.
pub fn read_file_at(mirror: &Path, sha: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let spec = format!("{sha}:{path}");
    let out = Command::new("git")
        .arg("-C")
        .arg(mirror)
        .arg("show")
        .arg(&spec)
        .output()
        .map_err(|_| git_missing("show"))?;
    if out.status.success() {
        Ok(Some(out.stdout))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
