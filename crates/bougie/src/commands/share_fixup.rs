//! Framework-specific "share mode" fixups for `bougie share`.
//!
//! A shared store is reached over public HTTPS at `<slug>.bougie.show`, but its
//! own config still points at the loopback `*.bougie.run` host — so absolute
//! URLs (assets, links, redirects) come out wrong unless the framework is told
//! about the share host. The relay overwrites `X-Forwarded-Host`/`Proto` with
//! trusted values, and the dev server passes them through to PHP as
//! `$_SERVER['HTTP_X_FORWARDED_*']`; a fixup turns that into correct URLs.
//!
//! Generic by design — most frameworks (Laravel/Symfony with trusted proxies,
//! static sites) already honour `X-Forwarded-*`, so the default adapter is a
//! **no-op**. Magento is the one that builds absolute URLs from its stored
//! `base_url` and needs intervention; it's the first adapter, and others slot
//! in behind [`detect`].
//!
//! The Magento adapter is **request-aware**: it injects a small, sentinel-
//! marked block into `app/etc/env.php` that overrides `base_url` *only when the
//! request carries this share's `X-Forwarded-Host`*. So the public share gets
//! the share URL while normal loopback dev is untouched — and a stale block
//! left by a killed `bougie share` is inert (it never matches a live header),
//! so it can't strand the store. env.php's `system` config is authoritative
//! (it's what `config:set --lock-env` writes) and is read every request, so it
//! reliably drives the URL builder.

use std::path::Path;

use eyre::{Result, WrapErr, eyre};

/// A framework-specific share fixup. Both operations are idempotent.
trait ShareFixup {
    /// Short name for logs (`"magento"`, `"none"`).
    fn name(&self) -> &'static str;
    /// Prepare the store to serve correct URLs for `share_host`.
    fn apply(&self, project: &Path, share_host: &str) -> Result<()>;
    /// Undo any changes. Safe if never applied / already reverted.
    fn revert(&self, project: &Path) -> Result<()>;
}

/// Detect + apply the right fixup. Returns the adapter name that ran.
pub(crate) fn apply(project: &Path, share_host: &str) -> Result<&'static str> {
    let fx = detect(project);
    fx.apply(project, share_host)?;
    Ok(fx.name())
}

/// Detect + revert. Best-effort, idempotent.
pub(crate) fn revert(project: &Path) -> Result<()> {
    detect(project).revert(project)
}

/// Pick the fixup for a project. Default = no-op.
fn detect(project: &Path) -> Box<dyn ShareFixup> {
    if is_magento(project) {
        Box::new(Magento)
    } else {
        Box::new(NoOp)
    }
}

fn is_magento(project: &Path) -> bool {
    project.join("bin/magento").is_file() && project.join("app/etc/env.php").is_file()
}

/// Frameworks that honour `X-Forwarded-*` (or static sites): nothing to do —
/// the relay's headers already produce correct URLs.
struct NoOp;
impl ShareFixup for NoOp {
    fn name(&self) -> &'static str {
        "none"
    }
    fn apply(&self, _: &Path, _: &str) -> Result<()> {
        Ok(())
    }
    fn revert(&self, _: &Path) -> Result<()> {
        Ok(())
    }
}

struct Magento;
impl ShareFixup for Magento {
    fn name(&self) -> &'static str {
        "magento"
    }

    fn apply(&self, project: &Path, share_host: &str) -> Result<()> {
        let env_php = project.join("app/etc/env.php");
        let src = std::fs::read_to_string(&env_php)
            .wrap_err_with(|| format!("reading {}", env_php.display()))?;
        let injected = inject(&src, share_host)
            .wrap_err_with(|| format!("preparing share base_url override in {}", env_php.display()))?;
        write_atomically(&env_php, &injected)
    }

    fn revert(&self, project: &Path) -> Result<()> {
        let env_php = project.join("app/etc/env.php");
        let Ok(src) = std::fs::read_to_string(&env_php) else {
            return Ok(()); // gone / unreadable — nothing to undo
        };
        if let Some(clean) = strip(&src) {
            write_atomically(&env_php, &clean)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// env.php surgery (pure — unit-tested below).
//
// We prepend a sentinel-marked PHP function and wrap the file's top-level
// `return <array>;` in a call to it: `return __bougie_share_base_url(<array>);`.
// The function overrides `system/default/web/*/base_url` (+ secure/offloader/
// cookie) with the share URL only when `HTTP_X_FORWARDED_HOST` equals this
// share's host — otherwise it returns the config untouched.
// ---------------------------------------------------------------------------

const SENTINEL_BEGIN: &str = "// bougie:share-fixup:begin";
const SENTINEL_END: &str = "// bougie:share-fixup:end";
const WRAP_CALL: &str = "return __bougie_share_base_url(";
const RETURN_OPENERS: [&str; 3] = ["return array (", "return array(", "return ["];

/// Reject anything that couldn't be a hostname, so it can't break out of the
/// single-quoted PHP string literal we bake it into.
fn validate_host(host: &str) -> Result<()> {
    if host.is_empty()
        || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(eyre!("refusing to inject an unexpected share host: {host:?}"));
    }
    Ok(())
}

fn fixup_block(share_host: &str) -> String {
    format!(
        r"{SENTINEL_BEGIN} — auto-managed by `bougie share`; safe to delete
if (!function_exists('__bougie_share_base_url')) {{
    function __bougie_share_base_url(array $config): array {{
        $host = '{share_host}';
        if (($_SERVER['HTTP_X_FORWARDED_HOST'] ?? '') !== $host) {{
            return $config; // not this share's request → leave config untouched
        }}
        $url = 'https://' . $host . '/';
        $config['system']['default']['web']['unsecure']['base_url'] = $url;
        $config['system']['default']['web']['secure']['base_url'] = $url;
        $config['system']['default']['web']['secure']['use_in_frontend'] = '1';
        $config['system']['default']['web']['secure']['use_in_adminhtml'] = '1';
        $config['system']['default']['web']['secure']['offloader_header'] = 'X-Forwarded-Proto';
        $config['system']['default']['web']['cookie']['cookie_domain'] = '';
        return $config;
    }}
}}
{SENTINEL_END}
"
    )
}

/// Inject the request-aware `base_url` override. Idempotent: strips any prior
/// block first, so re-applying (e.g. after a crashed share) is clean.
fn inject(src: &str, share_host: &str) -> Result<String> {
    validate_host(share_host)?;
    let cleaned = strip(src).unwrap_or_else(|| src.to_string());
    if !cleaned.trim_start().starts_with("<?php") {
        return Err(eyre!("env.php must start with `<?php`"));
    }
    let opener = RETURN_OPENERS
        .into_iter()
        .find(|p| cleaned.contains(p))
        .ok_or_else(|| eyre!("env.php has no top-level `return [...]`"))?;
    let ret_pos = cleaned.find(opener).expect("opener present");

    let before = &cleaned[..ret_pos];
    let from_return = &cleaned[ret_pos..];
    // `return array (` → `return __bougie_share_base_url(array (`
    let wrapped = from_return.replacen("return ", WRAP_CALL, 1);
    // Close the wrapper call just before the statement-terminating `;`.
    let semi = wrapped
        .rfind(';')
        .ok_or_else(|| eyre!("env.php's `return` has no terminating `;`"))?;
    let mut region = String::with_capacity(wrapped.len() + 1);
    region.push_str(&wrapped[..semi]);
    region.push(')');
    region.push_str(&wrapped[semi..]);

    Ok(format!("{before}{}{region}", fixup_block(share_host)))
}

/// Remove a previously-injected block, returning the cleaned source — or `None`
/// if no fixup is present. Host-agnostic, so it also cleans a stale block.
fn strip(src: &str) -> Option<String> {
    let begin = src.find(SENTINEL_BEGIN)?;
    let end_marker = src.find(SENTINEL_END)?;
    let after_end = end_marker + SENTINEL_END.len();
    // Drop through the end-of-line after the end sentinel.
    let block_end = src[after_end..]
        .find('\n')
        .map_or(src.len(), |n| after_end + n + 1);

    let mut out = String::with_capacity(src.len());
    out.push_str(&src[..begin]);
    out.push_str(&src[block_end..]);

    // Unwrap the return only if the wrapper call is actually there.
    if !out.contains(WRAP_CALL) {
        return Some(out);
    }
    let mut out = out.replacen(WRAP_CALL, "return ", 1);
    // Remove the extra `)` we inserted before the final `;`.
    if let Some(semi) = out.rfind(';')
        && semi > 0
        && out.as_bytes()[semi - 1] == b')'
    {
        out.replace_range(semi - 1..semi, "");
    }
    Some(out)
}

/// Write `contents` to `path` atomically (temp-in-same-dir + rename),
/// preserving the original file's permissions.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| eyre!("{} has no parent dir", path.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".env.php.bougie-")
        .tempfile_in(dir)
        .wrap_err_with(|| format!("creating temp file in {}", dir.display()))?;
    tmp.write_all(contents.as_bytes()).wrap_err("writing env.php override")?;
    tmp.flush().ok();
    // Preserve the original mode so php-fpm can still read it.
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(tmp.path(), meta.permissions());
    }
    tmp.persist(path).map_err(|e| eyre!("replacing {}: {}", path.display(), e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: &str = "<?php\nreturn [\n    'db' => ['x' => 1],\n];\n";
    const LONG: &str = "<?php\nreturn array (\n  'db' => \n  array (\n    'x' => 1,\n  ),\n);\n";
    const HOST: &str = "myshop-a1b2.bougie.show";

    #[test]
    fn inject_then_strip_round_trips_short_array() {
        let injected = inject(SHORT, HOST).unwrap();
        assert!(injected.contains(SENTINEL_BEGIN));
        assert!(injected.contains("return __bougie_share_base_url(["));
        assert!(injected.trim_end().ends_with("]);"));
        assert_eq!(strip(&injected).unwrap(), SHORT);
    }

    #[test]
    fn inject_then_strip_round_trips_long_array() {
        let injected = inject(LONG, HOST).unwrap();
        assert!(injected.contains("return __bougie_share_base_url(array ("));
        assert!(injected.trim_end().ends_with("));"));
        assert_eq!(strip(&injected).unwrap(), LONG);
    }

    #[test]
    fn inject_is_idempotent() {
        let once = inject(SHORT, HOST).unwrap();
        let twice = inject(&once, HOST).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn reinject_with_a_new_host_replaces_the_old() {
        let first = inject(SHORT, "old-slug.bougie.show").unwrap();
        let second = inject(&first, "new-slug.bougie.show").unwrap();
        assert!(second.contains("new-slug.bougie.show"));
        assert!(!second.contains("old-slug.bougie.show"));
        assert_eq!(strip(&second).unwrap(), SHORT);
    }

    #[test]
    fn strip_is_none_when_absent() {
        assert!(strip(SHORT).is_none());
    }

    #[test]
    fn host_baked_and_gated_on_forwarded_host() {
        let injected = inject(SHORT, HOST).unwrap();
        assert!(injected.contains("$host = 'myshop-a1b2.bougie.show';"));
        assert!(injected.contains("$_SERVER['HTTP_X_FORWARDED_HOST']"));
        assert!(injected.contains("'https://' . $host . '/'"));
    }

    #[test]
    fn rejects_a_host_that_could_break_the_literal() {
        assert!(inject(SHORT, "evil'; system('x'); //").is_err());
        assert!(inject(SHORT, "").is_err());
    }

    #[test]
    fn errors_on_a_file_without_a_return() {
        assert!(inject("<?php\n$x = 1;\n", HOST).is_err());
    }
}
