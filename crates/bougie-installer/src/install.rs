//! Orchestrates a PHP interpreter installation: refresh index, resolve,
//! fetch + extract. Shared by `bougie php install` and `bougie sync`.

use bougie_backend;
use crate::baseline::{BaselineFilter, BASELINE_EXTENSIONS};
#[cfg(not(target_os = "windows"))]
use crate::baseline::{skip_for_platform, PREINSTALLED_EXTENSIONS};
use bougie_errors::BougieError;
use bougie_fetch::{fetch_blob, BlobSpec, DownloadBar, Hash};
// Closure-peer tarballs are bougie-index-only; the consuming code is
// `cfg(not(target_os = "windows"))` so the import has to match.
#[cfg(not(target_os = "windows"))]
use bougie_fetch::ArchiveKind;
use bougie_index::wire::LoadDirective;
use bougie_fs::lock::ExclusiveGuard;
use bougie_paths::Paths;
use bougie_version::request::{Flavor, Request};
use bougie_resolver::ResolveOptions;
use bougie_fs::store::install_dir;
use bougie_platform::target::Triple;
use bougie_version::version::{PartialVersion, Version};
use eyre::{eyre, Result, WrapErr};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_INDEX_URL: &str = "https://index.bougie.tools";
const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Debug, Clone)]
pub struct InstalledPhp {
    pub version: Version,
    pub flavor: Flavor,
    pub install_path: PathBuf,
    pub already_present: bool,
    pub frozen_warning: bool,
}

pub fn install_php(
    paths: &Paths,
    request: &Request,
    flavor_override: Option<Flavor>,
    opts: ResolveOptions,
) -> Result<InstalledPhp> {
    let target = Triple::detect()?;
    let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());

    let (spec, in_request_flavor) = match request {
        Request::VersionLike { spec, flavor } => (spec.clone(), *flavor),
        Request::FullTag { .. } | Request::Path(_) | Request::Name(_) => {
            return Err(eyre!(
                "this request shape is not supported by `php install`; use a version or constraint"
            ));
        }
    };
    let flavor = pick_flavor(in_request_flavor, flavor_override)?;

    // Lock the global store before mutating anything.
    let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;

    let backend = bougie_backend::select(&target, &host, paths)?;
    let recipe = backend.resolve_php(&spec, flavor, opts)?;
    let dest = install_dir(paths, recipe.version, recipe.flavor);
    let already_present = dest.exists();
    if !already_present {
        // Interpreter is a monolithic blob (no closure walk), so the
        // bar grows by exactly the tarball's size. A pre-`size`
        // publisher emits `size: 0` which `add_planned` silently
        // drops; the bar still ticks bytes received but can't fill.
        // windows.php.net publishes a string-shaped `size` field
        // that we don't parse, so the Windows backend always falls
        // through that path.
        let bar = DownloadBar::new("downloading");
        bar.add_planned(recipe.blob.size);
        bar.set_current(format!("php-{}", recipe.version));
        let cache_blobs = paths.cache_blobs();
        backend.fetch_into(&recipe.blob, &dest, &cache_blobs, &bar)?;
        bar.finish();
    }

    Ok(InstalledPhp {
        version: recipe.version,
        flavor: recipe.flavor,
        install_path: dest,
        already_present,
        frozen_warning: recipe.frozen_warning,
    })
}

fn pick_flavor(in_request: Option<Flavor>, flag: Option<Flavor>) -> Result<Flavor> {
    match (in_request, flag) {
        (Some(a), Some(b)) if a != b => Err(BougieError::Resolution {
            kind: "flavor".into(),
            detail: format!(
                "request encodes flavor `{a}` but --flavor=`{b}` was passed; remove one or make them agree"
            ),
        }
        .into()),
        (Some(f), _) | (None, Some(f)) => Ok(f),
        (None, None) => Ok(Flavor::Nts),
    }
}

/// One installed extension, as produced by [`install_extension`].
#[derive(Debug, Clone)]
pub struct InstalledExt {
    pub name: String,
    pub version: Version,
    pub flavor: Flavor,
    pub php_minor: PartialVersion,
    /// Store directory the tarball was extracted into.
    pub store_path: PathBuf,
    /// Absolute path to the `.so` inside [`Self::store_path`], suitable
    /// for the right-hand side of the conf.d `extension=` /
    /// `zend_extension=` directive.
    pub so_path: PathBuf,
    /// Whether the conf.d fragment emits `extension=` or `zend_extension=`.
    pub load: LoadDirective,
    pub already_present: bool,
    pub frozen_warning: bool,
    /// Extra directories that need to be on PATH at run-time so the
    /// extension's dependent DLLs resolve. Empty on every Unix
    /// installation (closures + RPATH handle deps there) and on every
    /// single-DLL Windows PECL extension. Used by Windows imagick:
    /// its ZIP bundles `CORE_RL_*.dll` (link-time) and `IM_MOD_RL_*.dll`
    /// (codec modules) alongside `php_imagick.dll`, and the store dir
    /// is the only directory that has the whole set. `bougie run`
    /// reads the `; bougie-path: <dir>` comments these emit into the
    /// conf.d fragment and prepends each dir to PATH.
    pub path_extras: Vec<PathBuf>,
}

/// Install a PHP extension into the content-addressed store.
///
/// Mirrors [`install_php`] but targets the extension's own section
/// (`extension/<name>`) and uses [`resolve_extension`] for selection.
/// The destination is `$BOUGIE_HOME/store/ext-<name>-<version>+php<minor>-<flavor>-<sha8>/`,
/// content-addressed by the first 8 hex chars of the blob sha256 so
/// different ABI builds of the same `<name>-<version>` don't collide.
///
/// Single-extension entry point: creates its own [`DownloadBar`] for
/// this call. Use [`install_extension_with_bar`] when multiple
/// extensions are being installed in a loop and you want one bar
/// across all of them.
pub fn install_extension(
    paths: &Paths,
    name: &str,
    version_pin: Option<&str>,
    php_minor: PartialVersion,
    flavor: Flavor,
    opts: ResolveOptions,
) -> Result<InstalledExt> {
    let bar = DownloadBar::new("downloading");
    let out = install_extension_with_bar(paths, name, version_pin, php_minor, flavor, opts, &bar);
    bar.finish();
    out
}

/// Same as [`install_extension`] but draws progress against a caller-
/// owned bar so the orchestrator (e.g. [`install_baseline_into`],
/// [`preinstall_into`], `sync::install_required_extensions`) can show
/// a single combined bar across many extensions instead of one bar
/// per artifact.
pub fn install_extension_with_bar(
    paths: &Paths,
    name: &str,
    version_pin: Option<&str>,
    php_minor: PartialVersion,
    flavor: Flavor,
    opts: ResolveOptions,
    bar: &DownloadBar,
) -> Result<InstalledExt> {
    let target = Triple::detect()?;
    let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());

    let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;

    let backend = bougie_backend::select(&target, &host, paths)?;
    let recipe = backend.resolve_extension(name, php_minor, flavor, version_pin, opts)?;

    let sha8: String = recipe.blob.sha256.chars().take(8).collect();
    let php_minor_label = format!("php{}{}", php_minor.major, php_minor.minor.unwrap_or(0));
    let dirname = format!(
        "ext-{}-{}+{php_minor_label}-{flavor}-{sha8}",
        recipe.name, recipe.version,
    );
    let dest = paths.store().join(&dirname);
    let already_present = dest.exists();

    // Grow the shared bar's planned total by this extension's bytes:
    // the main `.so`/`.dll` blob (if not already on disk) plus every
    // closure entry whose store_path is missing. Sizes come from the
    // recipe so we don't pay a HEAD round-trip per file. A `size: 0`
    // — from an older publisher or from windows.php.net (which doesn't
    // advertise size on the PECL surface) — contributes nothing, and
    // the bar still ticks bytes received but can't fill for that
    // artifact.
    if !already_present {
        bar.add_planned(recipe.blob.size);
    }
    #[cfg(not(target_os = "windows"))]
    plan_closure_bytes(paths, &recipe.closure, bar);

    if !already_present {
        let blob_spec = BlobSpec {
            url: &recipe.blob.url,
            hash: Hash::sha256(&recipe.blob.sha256),
            partial_dir: &paths.cache_blobs(),
            dest: &dest,
            strip_prefix: &recipe.blob.strip_prefix,
            archive: recipe.blob.archive,
        };
        bar.set_current(format!("{}-{}", recipe.name, recipe.version));
        fetch_blob(backend.client(), &blob_spec, bar)?;
    }

    // Walk the bundled-C-lib closure and fetch any store-paths the
    // consumer doesn't have yet. Mandatory for bougie-index ext: the
    // `.so` was built with `$ORIGIN/../../../store/<storeName>/lib`
    // RPATHs that assume an install-shaped layout (matching the
    // interpreter tarball, whose `<install>/store/<storeName>/lib`
    // directly resolves). Without these tarballs *and* the
    // corresponding `store/` peer inside the ext root, dlopen falls
    // back to the system loader and surfaces errors like
    // `libicuuc.so.77: cannot open shared object file` for intl or
    // `libcurl: undefined symbol: ENGINE_init` for curl.
    //
    // Run unconditionally on Unix — `dest.exists()` only tells us the
    // .so blob is present; the closure may still be partial from an
    // earlier bougie release that didn't walk it. Skipped on Windows:
    // the windows.php.net backend always returns an empty closure
    // (DLL deps ride inside the PECL ZIP) and the symlink machinery
    // is unix-only anyway.
    #[cfg(not(target_os = "windows"))]
    install_closure_peers(
        backend.client(),
        paths,
        &recipe.closure,
        &recipe.name,
        // The backend doesn't track an artifact tag string; the
        // `<name>-<version>+php<minor>-<flavor>` dirname is the next
        // best stable identifier for error context.
        &dirname,
        &dest,
        bar,
    )?;

    let so_path = dest.join(&recipe.artifact_rel);
    if !so_path.exists() {
        return Err(eyre!(
            "extracted ext bundle is missing the declared artifact at {} \
             — blob {} may be corrupt or the recipe is wrong",
            so_path.display(),
            recipe.blob.url,
        ));
    }

    let path_extras = if recipe.needs_store_on_path {
        vec![dest.clone()]
    } else {
        Vec::new()
    };

    Ok(InstalledExt {
        name: recipe.name,
        version: recipe.version,
        flavor,
        php_minor,
        store_path: dest,
        so_path,
        load: recipe.load,
        already_present,
        frozen_warning: recipe.frozen_warning,
        path_extras,
    })
}

/// Outcome of [`install_local_so`]. Same shape as [`InstalledExt`]
/// for the bits the conf.d writer needs, minus the index-derived
/// `version` / `frozen_warning` fields (a local .so has no version).
#[derive(Debug, Clone)]
pub struct InstalledLocalExt {
    pub name: String,
    pub store_path: PathBuf,
    /// Absolute path to the copied `.so` inside `store_path`.
    pub so_path: PathBuf,
    /// `true` if the destination directory already existed — the .so
    /// at this path matches the user-provided bytes (sha-addressed).
    pub already_present: bool,
}

/// Copy a user-provided `.so` into the content-addressed store under
/// `ext-<name>-local+php<minor>-<flavor>-<sha8>/<name>.so`. The sha
/// is over the .so bytes, so a different file produces a different
/// dest; an identical re-add (even with the source renamed) is a
/// no-op. The on-disk basename is normalised to `<name>.so` so the
/// store layout is stable across different source filenames.
///
/// Does NOT walk a closure (there's no manifest) and does NOT touch
/// composer.json — callers (today: `bougie ext add <path>.so`) pair
/// this with [`crate::conf_d::write_local_ext_fragment`].
pub fn install_local_so(
    paths: &Paths,
    name: &str,
    source_so: &Path,
    php_minor: PartialVersion,
    flavor: Flavor,
) -> Result<InstalledLocalExt> {
    let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;

    let bytes = std::fs::read(source_so)
        .wrap_err_with(|| format!("reading {}", source_so.display()))?;
    let sha = {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(&bytes);
        digest.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let sha8: String = sha.chars().take(8).collect();

    let php_minor_label = format!("php{}{}", php_minor.major, php_minor.minor.unwrap_or(0));
    let dirname = format!("ext-{name}-local+{php_minor_label}-{flavor}-{sha8}");
    let dest_dir = paths.store().join(&dirname);
    // Canonical on-disk filename — independent of how the user named
    // the source. Keeps `dest_so.exists()` a reliable "is the install
    // complete" check even after `cp tideways.so tw.so && ext add tw.so`.
    let canonical_basename = format!("{name}.so");
    let dest_so = dest_dir.join(&canonical_basename);

    if dest_so.exists() {
        return Ok(InstalledLocalExt {
            name: name.to_string(),
            store_path: dest_dir,
            so_path: dest_so,
            already_present: true,
        });
    }

    // Tempdir-then-rename for atomicity: a partial copy under a
    // dest_dir that already-exists check would later mistake for a
    // complete install.
    let tmp = paths.store().join(format!(".{dirname}.incoming"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)
        .wrap_err_with(|| format!("creating {}", tmp.display()))?;
    let tmp_so = tmp.join(&canonical_basename);
    std::fs::write(&tmp_so, &bytes)
        .wrap_err_with(|| format!("writing {}", tmp_so.display()))?;
    // If a prior run left an empty/partial dest_dir behind (e.g. a
    // basename mismatch from an older bougie that keyed dirs only on
    // sha), clear it so the atomic rename can land. Same global lock
    // means we're the only writer here.
    if dest_dir.exists() {
        std::fs::remove_dir_all(&dest_dir)
            .wrap_err_with(|| format!("clearing stale {}", dest_dir.display()))?;
    }
    std::fs::rename(&tmp, &dest_dir)
        .wrap_err_with(|| format!("renaming {} -> {}", tmp.display(), dest_dir.display()))?;

    Ok(InstalledLocalExt {
        name: name.to_string(),
        store_path: dest_dir,
        so_path: dest_so,
        already_present: false,
    })
}

/// Outcome of [`install_baseline_into`]. `installed` and `failed`
/// together cover every name the filter admitted; `skipped` is empty
/// here and carried only so the JSON shape stays stable when the
/// caller passes [`BaselineFilter::None`] or a narrowed `Only` (the
/// difference between "didn't try" vs "tried and failed" matters for
/// CI dashboards).
#[derive(Debug, Default, Clone)]
pub struct BaselineReport {
    pub installed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Install every baseline extension admitted by `filter` into the
/// content-addressed store, then write a conf.d fragment under
/// `<install_root>/etc/php/conf.d/<NN>-<name>.ini` so the interpreter
/// auto-loads them. The numeric prefix mirrors `php-build-standalone`'s
/// `php/build-php.sh` convention so that `pdo_*` loads after `pdo` and
/// `mysqli` / `sqlite3` / `pgsql` load after their PDO siblings — see
/// [`conf_d_prefix_for`]. Errors per extension are downgraded to
/// entries in [`BaselineReport::failed`].
///
/// Before iterating, any pre-existing bougie-baseline-managed fragments
/// in `conf_d` are deleted. This keeps a re-run idempotent across
/// changes to [`conf_d_prefix_for`] or to [`BASELINE_EXTENSIONS`]: an
/// old `20-pdo_mysql.ini` from a previous bougie release won't linger
/// alongside the new `35-pdo_mysql.ini`, which would otherwise
/// trigger `undefined symbol: pdo_dbh_ce` (pdo_mysql loaded before
/// pdo) or `Module "pdo_mysql" is already loaded` (both fragments
/// loaded in sequence).
///
/// This is intentionally separate from [`install_php`] and must be
/// called *after* `install_php` returns: both functions acquire the
/// same global lock, and nesting them would deadlock.
pub fn install_baseline_into(
    paths: &Paths,
    install_root: &Path,
    php_minor: PartialVersion,
    flavor: Flavor,
    filter: &BaselineFilter,
    resolve_opts: ResolveOptions,
) -> BaselineReport {
    // Windows takes a different route: baseline extensions either ship
    // statically built into `php.exe` (mysqlnd, dom, simplexml, xml*,
    // tokenizer, readline, …) or land as DLLs in
    // `<install>/bin/ext/php_*.dll` (opcache, exif, ffi, fileinfo, ftp,
    // gettext, shmop, sockets, sysvshm, …). There's no per-extension
    // tarball to fetch — the windows.php.net interpreter ZIP already
    // carries everything. See [`install_baseline_from_bundled_windows`].
    #[cfg(target_os = "windows")]
    {
        let _ = (paths, php_minor, flavor, resolve_opts);
        return install_baseline_from_bundled_windows(install_root, filter);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut report = BaselineReport::default();
        let conf_d = install_root.join("etc").join("php").join("conf.d");
        if let Err(e) = std::fs::create_dir_all(&conf_d) {
            // If we can't even create conf.d, every entry will fail
            // the same way — record one synthetic failure and bail.
            report.failed.push((
                "<conf.d>".into(),
                format!("creating {}: {e}", conf_d.display()),
            ));
            return report;
        }

        if let Err(e) = clean_stale_baseline_fragments(&conf_d) {
            // Non-fatal: leave the loop a chance to overwrite each
            // canonical-prefix file. Surface as a synthetic failure so
            // CI dashboards see something happened.
            report
                .failed
                .push(("<conf.d-cleanup>".into(), format!("{e:#}")));
        }

        // One shared download bar across the whole baseline loop. The
        // bar's planned-total grows as each extension's manifest reveals
        // its blob+closure sizes, so the user sees a single combined
        // bar instead of one per extension.
        let bar = DownloadBar::new("downloading");
        for &name in BASELINE_EXTENSIONS {
            if !filter.includes(name) {
                continue;
            }
            if skip_for_platform(name) {
                // gettext on macOS is the canonical case — Apple's
                // libc has no real libintl, so php-build-standalone
                // emits no gettext.so on Darwin and the index has no
                // entry to fetch. Silently skip; the conf.d cleanup
                // loop above won't try to delete a fragment that was
                // never written.
                continue;
            }
            match install_extension_with_bar(
                paths,
                name,
                None,
                php_minor,
                flavor,
                resolve_opts,
                &bar,
            ) {
                Ok(installed) => {
                    if let Err(e) = write_install_conf_d(&conf_d, &installed) {
                        report
                            .failed
                            .push((name.into(), format!("writing conf.d: {e:#}")));
                    } else {
                        report.installed.push(name.into());
                    }
                }
                Err(e) => report.failed.push((name.into(), format!("{e:#}"))),
            }
        }
        bar.finish();
        report
    }
}

/// Windows baseline path: for every name in [`BASELINE_EXTENSIONS`]
/// that has a corresponding `<install>/bin/ext/php_<name>.dll`, write a
/// conf.d fragment with an absolute `extension=` (or `zend_extension=`,
/// for opcache) directive pointing at the bundled DLL. PHP on Windows
/// accepts absolute paths for both directives.
///
/// Extensions that aren't present as DLLs are silently skipped — they
/// either ship statically built into `php.exe` (e.g. `dom`, `mysqlnd`,
/// `readline`, the xml family — `php -m` already lists them after a
/// bare interpreter install) or have no Windows equivalent at all
/// (`posix`, `sysvmsg`, `sysvsem`). Distinguishing the two would need
/// a hardcoded "built-in on Windows" table; for now the user-visible
/// outcome — "the extension is loadable" or "it isn't, and there's
/// nothing bougie can do about it" — is the same.
#[cfg(target_os = "windows")]
fn install_baseline_from_bundled_windows(
    install_root: &Path,
    filter: &BaselineFilter,
) -> BaselineReport {
    let mut report = BaselineReport::default();
    let conf_d = install_root.join("etc").join("php").join("conf.d");
    if let Err(e) = std::fs::create_dir_all(&conf_d) {
        report.failed.push((
            "<conf.d>".into(),
            format!("creating {}: {e}", conf_d.display()),
        ));
        return report;
    }
    if let Err(e) = clean_stale_baseline_fragments(&conf_d) {
        report
            .failed
            .push(("<conf.d-cleanup>".into(), format!("{e:#}")));
    }

    let ext_dir = install_root.join("bin").join("ext");
    // The Windows baseline is the union of `BASELINE_EXTENSIONS` and
    // `WINDOWS_DLL_BASELINE_EXTRAS`. The latter holds extensions that
    // are static-into-php.so on the Linux build (so they sit in
    // `BUILTIN_EXTENSIONS` and never reach this loop on Unix) but
    // ride along as DLLs in the Windows ZIP — openssl is the canonical
    // case. The filter applies symmetrically so `--without openssl`
    // opts out of the ride-along too.
    for &name in BASELINE_EXTENSIONS
        .iter()
        .chain(crate::baseline::WINDOWS_DLL_BASELINE_EXTRAS.iter())
    {
        if !filter.includes(name) {
            continue;
        }
        let dll = ext_dir.join(format!("php_{name}.dll"));
        if !dll.exists() {
            // Built-in to php.exe, or not available on Windows.
            continue;
        }
        // opcache is the only baseline ext that's a Zend extension on
        // Windows; everything else uses the regular `extension=` form.
        let load = if name == "opcache" {
            LoadDirective::ZendExtension
        } else {
            LoadDirective::Extension
        };
        match write_bundled_baseline_fragment(&conf_d, name, &dll, load) {
            Ok(()) => report.installed.push(name.into()),
            Err(e) => report
                .failed
                .push((name.into(), format!("writing conf.d: {e:#}"))),
        }
    }
    report
}

/// Atomic write of a baseline conf.d fragment pointing at a bundled
/// `php_<name>.dll`. Mirrors [`write_install_conf_d`]'s tempfile +
/// rename dance so a `kill -9` mid-write can't wedge the next `php`
/// invocation, and uses the same [`BASELINE_FRAGMENT_HEADER`] so
/// [`clean_stale_baseline_fragments`] picks it up on the next
/// re-install.
#[cfg(target_os = "windows")]
fn write_bundled_baseline_fragment(
    conf_d: &Path,
    name: &str,
    dll: &Path,
    load: LoadDirective,
) -> Result<()> {
    let prefix = conf_d_prefix_for(name);
    let path = conf_d.join(format!("{prefix}-{name}.ini"));
    let body = format!(
        "{BASELINE_FRAGMENT_HEADER} {name} bundled\n\
         {directive}={dll}\n",
        directive = load.ini_directive(),
        dll = crate::conf_d::format_ini_path(dll),
    );
    let mut tf = tempfile::NamedTempFile::new_in(conf_d)
        .wrap_err_with(|| format!("creating tempfile in {}", conf_d.display()))?;
    tf.as_file_mut()
        .write_all(body.as_bytes())
        .wrap_err_with(|| format!("writing {}", tf.path().display()))?;
    tf.as_file_mut()
        .sync_all()
        .wrap_err_with(|| format!("fsyncing {}", tf.path().display()))?;
    tf.persist(&path)
        .map_err(|e| eyre!("renaming temp to {}: {e}", path.display()))?;
    Ok(())
}

/// Outcome of [`preinstall_into`]. Same shape as
/// [`BaselineReport`] so the two can be surfaced side-by-side in the
/// `bougie php install` JSON output.
#[derive(Debug, Default, Clone)]
pub struct PreinstallReport {
    pub installed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Pre-download (but don't activate) every extension in
/// [`PREINSTALLED_EXTENSIONS`] for the given interpreter. Each `.so`
/// lands in the content-addressed store under `paths.store()` — same
/// location `install_extension` would use anyway — so a later
/// `bougie ext add xdebug` (or the server's lazy activation path)
/// finds it already on disk and only writes a conf.d fragment.
///
/// `_install_root` is currently unused (no conf.d fragment is written
/// here) but kept in the signature for symmetry with
/// [`install_baseline_into`] and to leave room for future
/// per-interpreter bookkeeping. Errors are per-name and non-fatal —
/// caller surfaces them as warnings, same pattern as
/// [`BaselineReport`].
pub fn preinstall_into(
    paths: &Paths,
    _install_root: &Path,
    php_minor: PartialVersion,
    flavor: Flavor,
    resolve_opts: ResolveOptions,
) -> PreinstallReport {
    // The current preinstall set is just `xdebug`, which is a PECL
    // extension. PECL via windows.php.net is Phase 4b — until that
    // lands the preinstall step would always fail on Windows with
    // `unknown host target`. No-op so the install path stays quiet;
    // Phase 4b restores the preinstall behavior.
    #[cfg(target_os = "windows")]
    {
        let _ = (paths, _install_root, php_minor, flavor, resolve_opts);
        return PreinstallReport::default();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut report = PreinstallReport::default();
        // Same single-bar pattern as install_baseline_into.
        let bar = DownloadBar::new("downloading");
        for &name in PREINSTALLED_EXTENSIONS {
            match install_extension_with_bar(
                paths,
                name,
                None,
                php_minor,
                flavor,
                resolve_opts,
                &bar,
            ) {
                Ok(_) => report.installed.push(name.into()),
                Err(e) => report.failed.push((name.into(), format!("{e:#}"))),
            }
        }
        bar.finish();
        report
    }
}

/// Numeric conf.d prefix for a baseline (or user) extension. Mirrors
/// `php-build-standalone`'s `php/build-php.sh` numbering so that PHP's
/// alphabetic conf.d scan honors load-order dependencies:
///
/// - `35-pdo_*.ini` loads after `30-pdo.ini` (the core PDO base must
///   be initialized before any driver can register against it).
/// - `40-mysqli.ini` / `40-sqlite3.ini` / `40-pgsql.ini` load after
///   the `35-pdo_*` siblings, matching the conventional grouping
///   even though `mysqli` doesn't have a hard pdo dependency.
/// - Everything else loads at `20-` — right after `10-opcache.ini`
///   but before any user `50-*.ini` overrides.
pub fn conf_d_prefix_for(name: &str) -> u32 {
    if name.starts_with("pdo_") {
        35
    } else if matches!(name, "mysqli" | "sqlite3" | "pgsql") {
        40
    } else {
        20
    }
}

const BASELINE_FRAGMENT_HEADER: &str = "; managed by bougie — baseline extension";

/// Delete any `.ini` fragment under `conf_d` that starts with the
/// bougie-baseline marker. Run before re-writing baseline fragments
/// so a prefix change between bougie releases doesn't leave orphans
/// (see [`install_baseline_into`] docstring for why this matters).
///
/// Only files whose first line begins with [`BASELINE_FRAGMENT_HEADER`]
/// are removed — user-authored `15-mytunables.ini` and shipped
/// interpreter fragments stay put.
fn clean_stale_baseline_fragments(conf_d: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(conf_d) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(eyre!("reading {}: {e}", conf_d.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("ini") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        if body.starts_with(BASELINE_FRAGMENT_HEADER) {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Write `<NN>-<name>.ini` under `<install>/etc/php/conf.d/` referencing
/// the content-addressed store path of the just-installed `.so`. The
/// `<NN>` prefix is chosen by [`conf_d_prefix_for`] so the
/// load-order dependencies PHP build conventions encode in the prefix
/// (`30-pdo` → `35-pdo_*` → `40-mysqli/sqlite3/pgsql`) are preserved.
///
/// Unix-only: the Windows baseline path uses [`write_bundled_baseline_fragment`]
/// instead (no [`InstalledExt`] to feed off — the DLL is shipped inside
/// the interpreter ZIP, not as a content-addressed store entry).
#[cfg(not(target_os = "windows"))]
fn write_install_conf_d(conf_d: &Path, installed: &InstalledExt) -> Result<()> {
    let prefix = conf_d_prefix_for(&installed.name);
    let path = conf_d.join(format!("{prefix}-{}.ini", installed.name));
    let body = format!(
        "{header} {name} {version}\n\
         {directive}={so}\n",
        header = BASELINE_FRAGMENT_HEADER,
        name = installed.name,
        version = installed.version,
        directive = installed.load.ini_directive(),
        so = installed.so_path.display(),
    );
    // Use a tempfile+rename for the same atomicity reasons as
    // conf_d::write_ext_fragment; baseline install happens under the
    // global lock so concurrent writers are already serialized, but
    // a partial-write left behind by a kill -9 would still wedge the
    // next `php` invocation.
    let mut tf = tempfile::NamedTempFile::new_in(conf_d)
        .wrap_err_with(|| format!("creating tempfile in {}", conf_d.display()))?;
    tf.as_file_mut()
        .write_all(body.as_bytes())
        .wrap_err_with(|| format!("writing {}", tf.path().display()))?;
    tf.as_file_mut()
        .sync_all()
        .wrap_err_with(|| format!("fsyncing {}", tf.path().display()))?;
    tf.persist(&path)
        .map_err(|e| eyre!("renaming temp to {}: {e}", path.display()))?;
    Ok(())
}

/// Walk the closure list, fetching every missing shared store entry
/// and materializing the install-shaped `store/<closureName>` peer
/// symlinks inside `install_root`. Idempotent: closure entries already
/// on disk are skipped; existing peer symlinks are left alone.
///
/// Used from two sites with the same semantics:
///
/// - [`install_extension_with_bar`] (extension installs): the .so was
///   built with `$ORIGIN/../../../store/<storeName>/lib` RPATHs that
///   need both the global shared-store entry AND the per-ext
///   `store/<storeName>` peer.
/// - [`crate::daemon::store_fetch::fetch_blocking`] (tool installs):
///   tool tarballs (mariadb, redis, …) have the same RPATH shape, so
///   the same machinery applies. Phase 1 of `UNBUNDLE_PLAN.md` flips
///   these tarballs from "ships its closure under `install/store/`"
///   to "publishes a non-empty `closure[]` and relies on this peer
///   layout"; this helper is the client-side hinge.
///
/// `label` shows up in the progress bar (the name the caller would
/// normally pass to `bar.set_current`); `tag` shows up in
/// closure-fetch error messages (mirrors what `manifest.tag` carries
/// for an index manifest — caller picks whatever string identifies
/// the outer artifact unambiguously).
///
/// Caller is responsible for pre-planning bytes on the bar via
/// [`plan_closure_bytes`] if a visible progress total is wanted. The
/// daemon path uses [`DownloadBar::hidden`] so it skips planning.
///
/// Windows doesn't ship bougie-index closure tarballs (it pulls
/// extensions from windows.php.net's PECL surface where dependent DLLs
/// ride inside the same ZIP), so this whole machinery is `cfg(not(
/// target_os = "windows"))`. The daemon code path that also calls
/// this is itself `cfg(unix)`, which is a subset.
#[cfg(not(target_os = "windows"))]
pub fn install_closure_peers(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    closure: &[bougie_backend::ClosureRef],
    label: &str,
    tag: &str,
    install_root: &Path,
    bar: &DownloadBar,
) -> Result<()> {
    for entry in closure {
        let store_path =
            store_dir_for_closure(paths, &entry.name, &entry.version, &entry.hash);
        if !store_path.exists() {
            // Closure tarballs wrap their contents in `<storeName>/`
            // per shared/tarball-store-path.nix; strip it so the
            // tarball's `<storeName>/lib/lib*.so` lands at
            // `<store_path>/lib/lib*.so` (matching the interpreter's
            // `<install>/store/<storeName>/lib/lib*.so` layout the
            // RPATHs were compiled to expect).
            let storename = format!("{}-{}-{}", entry.name, entry.version, entry.hash);
            let blob_spec = BlobSpec {
                url: &entry.url,
                hash: Hash::sha256(&entry.sha256),
                partial_dir: &paths.cache_blobs(),
                dest: &store_path,
                strip_prefix: &storename,
                archive: ArchiveKind::TarZst,
            };
            bar.set_current(format!("{label} ({})", entry.name));
            fetch_blob(client, &blob_spec, bar).wrap_err_with(|| {
                format!(
                    "fetching closure entry `{}-{}-{}` for {tag}",
                    entry.name, entry.version, entry.hash,
                )
            })?;
        }
        materialize_closure_peer(install_root, &entry.name, &entry.version, &entry.hash)
            .wrap_err_with(|| {
                format!(
                    "linking closure peer `{}-{}-{}` for {tag}",
                    entry.name, entry.version, entry.hash,
                )
            })?;
    }
    Ok(())
}

/// Grow `bar`'s planned-total by the byte size of every closure entry
/// whose `store_path` is missing. Cheap stat-only loop; intended to be
/// called immediately before the main blob fetch so the caller can
/// show an accurate aggregate from the first byte.
#[cfg(not(target_os = "windows"))]
pub fn plan_closure_bytes(
    paths: &Paths,
    closure: &[bougie_backend::ClosureRef],
    bar: &DownloadBar,
) {
    for entry in closure {
        let store_path =
            store_dir_for_closure(paths, &entry.name, &entry.version, &entry.hash);
        if !store_path.exists() {
            bar.add_planned(entry.size);
        }
    }
}

/// `$BOUGIE_HOME/store/<name>-<version>-<hash>/` for a closure entry.
/// Thin wrapper over [`bougie_fs::store::store_dir`] kept here so callers
/// don't need to import the store module just to compute closure
/// destinations.
#[cfg(not(target_os = "windows"))]
fn store_dir_for_closure(paths: &Paths, name: &str, version: &str, hash: &str) -> PathBuf {
    bougie_fs::store::store_dir(paths, name, version, hash)
}

/// Create the install-shaped `store/<closureName>` peer inside an
/// extension's content-addressed root, as a relative symlink back at
/// the actual shared-store entry.
///
/// The interpreter tarball ships its closure under
/// `<install>/store/<storeName>/` so an extension `.so`'s
/// `$ORIGIN/../../../store/<storeName>/lib` RPATH resolves directly.
/// Standalone-installed extensions don't get that layout for free
/// (their root is just `$BOUGIE_HOME/store/ext-<name>-…/`, with no
/// `store/` peer), so we synthesize it: from inside the ext root,
/// `store/<closureName>` symlinks two levels up to the shared-store
/// sibling, which materializes the path the `.so` was compiled to
/// expect.
///
/// The symlink target is a *relative* `../../<closureName>`, so the
/// whole `$BOUGIE_HOME` tree relocates if the user moves it. Already-
/// correct symlinks are left alone; conflicting plain files would
/// indicate corruption and surface as an error.
#[cfg(not(target_os = "windows"))]
fn materialize_closure_peer(ext_root: &Path, name: &str, version: &str, hash: &str) -> Result<()> {
    let dirname = format!("{name}-{version}-{hash}");
    let store_peer = ext_root.join("store");
    std::fs::create_dir_all(&store_peer)
        .wrap_err_with(|| format!("creating {}", store_peer.display()))?;
    let link = store_peer.join(&dirname);
    let target = PathBuf::from("../..").join(&dirname);

    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            // Already a symlink — leave it. If a stale link points
            // elsewhere, deletion-then-recreate would be racy without
            // the lock we already hold, but the existing link is
            // accurate by construction (same closure entry hash).
            return Ok(());
        }
        Ok(_) => {
            return Err(eyre!(
                "{} exists but isn't a symlink — refusing to overwrite",
                link.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(eyre!("stat {}: {e}", link.display())),
    }
    symlink_dir(&target, &link)
        .wrap_err_with(|| format!("symlinking {} → {}", link.display(), target.display()))?;
    Ok(())
}

/// Cross-platform directory symlink, used by [`materialize_closure_peer`].
/// Both arms compile out on Windows builds because the only caller is
/// part of the bougie-index code path that's gated to
/// `cfg(not(target_os = "windows"))`.
///
/// Unix: `std::os::unix::fs::symlink` (no perm requirement).
#[cfg(unix)]
#[cfg(not(target_os = "windows"))]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

pub use bougie_index::host_to_dirname;

/// Read a project's already-resolved PHP (minor + flavor) so callers
/// that need to install an extension against the project's pinned
/// runtime don't have to re-run the constraint resolver. Errors when
/// `bougie sync` hasn't recorded a resolved PHP yet — the recovery
/// path is to run sync.
pub fn resolved_php_for_ext_install(project_root: &Path) -> Result<(PartialVersion, Flavor)> {
    let (version_str, flavor_str) = bougie_fs::state::read_project_resolved(project_root)
        .wrap_err(
            "project's resolved PHP isn't recorded yet — run `bougie sync` (or drop --no-sync) first",
        )?;
    let version = version_str
        .parse::<Version>()
        .map_err(|e| eyre!("malformed .bougie/state/resolved: {version_str:?}: {e}"))?;
    let flavor = match flavor_str.as_str() {
        "nts" => Flavor::Nts,
        "nts-debug" => Flavor::NtsDebug,
        "zts" => Flavor::Zts,
        "zts-debug" => Flavor::ZtsDebug,
        other => return Err(eyre!("malformed .bougie/state/resolved flavor: {other:?}")),
    };
    let php_minor = PartialVersion {
        major: version.major,
        minor: Some(version.minor),
        patch: None,
    };
    Ok((php_minor, flavor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_flavor_resolves_consistent_pair() {
        assert_eq!(
            pick_flavor(Some(Flavor::Zts), Some(Flavor::Zts)).unwrap(),
            Flavor::Zts
        );
        assert_eq!(
            pick_flavor(Some(Flavor::Zts), None).unwrap(),
            Flavor::Zts
        );
        assert_eq!(
            pick_flavor(None, Some(Flavor::ZtsDebug)).unwrap(),
            Flavor::ZtsDebug
        );
        assert_eq!(pick_flavor(None, None).unwrap(), Flavor::Nts);
    }

    #[test]
    fn pick_flavor_rejects_conflict() {
        assert!(pick_flavor(Some(Flavor::Nts), Some(Flavor::Zts)).is_err());
    }


    #[test]
    fn conf_d_prefix_handles_pdo_drivers_and_db_drivers() {
        assert_eq!(conf_d_prefix_for("pdo_mysql"), 35);
        assert_eq!(conf_d_prefix_for("pdo_sqlite"), 35);
        assert_eq!(conf_d_prefix_for("pdo_pgsql"), 35);
        assert_eq!(conf_d_prefix_for("mysqli"), 40);
        assert_eq!(conf_d_prefix_for("sqlite3"), 40);
        assert_eq!(conf_d_prefix_for("pgsql"), 40);
        assert_eq!(conf_d_prefix_for("mbstring"), 20);
        assert_eq!(conf_d_prefix_for("curl"), 20);
        assert_eq!(conf_d_prefix_for("redis"), 20);
    }

    #[test]
    fn clean_stale_baseline_fragments_removes_only_marked_files() {
        let td = tempfile::TempDir::new().unwrap();
        let conf_d = td.path();
        // Baseline-managed: should go.
        std::fs::write(
            conf_d.join("20-pdo_mysql.ini"),
            format!("{BASELINE_FRAGMENT_HEADER} pdo_mysql 1.0\nextension=x\n"),
        )
        .unwrap();
        std::fs::write(
            conf_d.join("20-mbstring.ini"),
            format!("{BASELINE_FRAGMENT_HEADER} mbstring 1.0\nextension=y\n"),
        )
        .unwrap();
        // Shipped by the PHP build — bare extension= line, must stay.
        std::fs::write(conf_d.join("35-pdo_pgsql.ini"), "extension=pdo_pgsql\n").unwrap();
        // User tunable, must stay.
        std::fs::write(conf_d.join("15-mytunables.ini"), "memory_limit=2G\n").unwrap();

        clean_stale_baseline_fragments(conf_d).unwrap();

        assert!(!conf_d.join("20-pdo_mysql.ini").exists());
        assert!(!conf_d.join("20-mbstring.ini").exists());
        assert!(conf_d.join("35-pdo_pgsql.ini").exists());
        assert!(conf_d.join("15-mytunables.ini").exists());
    }

    #[test]
    fn clean_stale_baseline_fragments_on_missing_dir_is_ok() {
        let td = tempfile::TempDir::new().unwrap();
        clean_stale_baseline_fragments(&td.path().join("does-not-exist")).unwrap();
    }

    // The whole closure-peer test family covers the bougie-index code
    // path that's `cfg(not(target_os = "windows"))`. Gate the tests
    // the same way so they don't reference symbols that don't exist
    // on Windows builds.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn materialize_closure_peer_creates_relative_symlink() {
        let td = tempfile::TempDir::new().unwrap();
        // Set up a fake shared store with the closure entry…
        let store = td.path();
        std::fs::create_dir_all(store.join("libcurl-8.20.0-abcdef01/lib")).unwrap();
        let ext_root = store.join("ext-curl-8.5.6+php85-nts-deadbeef");
        std::fs::create_dir_all(&ext_root).unwrap();

        materialize_closure_peer(&ext_root, "libcurl", "8.20.0", "abcdef01").unwrap();

        let link = ext_root.join("store/libcurl-8.20.0-abcdef01");
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink());
        // Resolves to the real shared-store entry.
        let target = std::fs::read_link(&link).unwrap();
        assert_eq!(target, PathBuf::from("../../libcurl-8.20.0-abcdef01"));
        assert!(std::fs::canonicalize(&link).unwrap().ends_with("libcurl-8.20.0-abcdef01"));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn materialize_closure_peer_is_idempotent() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path();
        std::fs::create_dir_all(store.join("libcurl-8.20.0-abcdef01")).unwrap();
        let ext_root = store.join("ext-curl-x");
        std::fs::create_dir_all(&ext_root).unwrap();
        materialize_closure_peer(&ext_root, "libcurl", "8.20.0", "abcdef01").unwrap();
        // Second call must not error or duplicate.
        materialize_closure_peer(&ext_root, "libcurl", "8.20.0", "abcdef01").unwrap();
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn materialize_closure_peer_refuses_to_overwrite_regular_file() {
        let td = tempfile::TempDir::new().unwrap();
        let ext_root = td.path().join("ext-x");
        std::fs::create_dir_all(ext_root.join("store")).unwrap();
        // A plain file occupies the path we'd want to symlink to.
        std::fs::write(ext_root.join("store/libcurl-1.0-aaaa"), "junk").unwrap();
        let err = materialize_closure_peer(&ext_root, "libcurl", "1.0", "aaaa").unwrap_err();
        assert!(err.to_string().contains("isn't a symlink"), "got: {err}");
    }

    // ---------- install_closure_peers (Phase 1) ----------

    /// Build a closure list of `entries`, with URLs pointing at
    /// `https://example.invalid/...` so any attempted network fetch
    /// would fail noisily — the test must pre-populate every
    /// `store_path` so the helper's fetch branch is never taken.
    #[cfg(not(target_os = "windows"))]
    fn closure_refs(entries: &[(&str, &str, &str)]) -> Vec<bougie_backend::ClosureRef> {
        entries
            .iter()
            .enumerate()
            .map(|(i, (name, ver, hash))| bougie_backend::ClosureRef {
                name: (*name).to_string(),
                version: (*ver).to_string(),
                hash: (*hash).to_string(),
                sha256: format!("{:0>64}", i),
                url: format!("https://example.invalid/{name}.tar.zst"),
                size: 0,
            })
            .collect()
    }

    #[cfg(not(target_os = "windows"))]
    fn pre_create_store_entry(paths: &Paths, name: &str, ver: &str, hash: &str) {
        // Pretend the closure blob has already been extracted into the
        // global store. The helper's fetch branch should skip; only
        // materialize_closure_peer should run.
        std::fs::create_dir_all(
            paths
                .store()
                .join(format!("{name}-{ver}-{hash}"))
                .join("lib"),
        )
        .unwrap();
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn install_closure_peers_creates_one_symlink_per_entry() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        // Three closure entries — a small but representative tool
        // closure (openssl + zlib + ncurses is exactly what redis and
        // mariadb need).
        let entries = [
            ("openssl", "3.5.6", "99c0f6e8"),
            ("zlib", "1.3.2", "jbmj2bcm"),
            ("ncurses", "6.6", "grpadm5y"),
        ];
        for (n, v, h) in &entries {
            pre_create_store_entry(&paths, n, v, h);
        }
        let closure = closure_refs(&entries);

        let install_root = paths.store().join("mariadb-11.4.10");
        std::fs::create_dir_all(&install_root).unwrap();

        let client = reqwest::blocking::Client::new();
        let bar = DownloadBar::hidden();
        install_closure_peers(&client, &paths, &closure, "mariadb", "mariadb-11.4.10", &install_root, &bar)
            .unwrap();

        for (n, v, h) in &entries {
            let link = install_root.join("store").join(format!("{n}-{v}-{h}"));
            let meta = std::fs::symlink_metadata(&link)
                .unwrap_or_else(|e| panic!("expected symlink at {}: {e}", link.display()));
            assert!(meta.file_type().is_symlink(), "{} not a symlink", link.display());
            let target = std::fs::read_link(&link).unwrap();
            assert_eq!(target, PathBuf::from("../..").join(format!("{n}-{v}-{h}")));
            // And it actually resolves into the global store.
            let canon = std::fs::canonicalize(&link).unwrap();
            assert!(canon.ends_with(format!("{n}-{v}-{h}")), "canon = {}", canon.display());
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn install_closure_peers_is_idempotent() {
        // Second invocation must not error (the existing peer symlinks
        // are left alone) and must not double-link.
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let entries = [("openssl", "3.5.6", "99c0f6e8")];
        pre_create_store_entry(&paths, "openssl", "3.5.6", "99c0f6e8");
        let closure = closure_refs(&entries);

        let install_root = paths.store().join("redis-8.6.3");
        std::fs::create_dir_all(&install_root).unwrap();
        let client = reqwest::blocking::Client::new();
        let bar = DownloadBar::hidden();
        install_closure_peers(&client, &paths, &closure, "redis", "redis-8.6.3", &install_root, &bar)
            .unwrap();
        install_closure_peers(&client, &paths, &closure, "redis", "redis-8.6.3", &install_root, &bar)
            .unwrap();

        // Exactly one entry under store/, still a symlink.
        let entries: Vec<_> = std::fs::read_dir(install_root.join("store"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn install_closure_peers_noop_on_empty_closure() {
        // Pre-split tool tarballs ship empty closure[]; the helper must
        // be a no-op so the daemon's fetch_blocking can call it
        // unconditionally without breaking old artifacts. This is the
        // backward-compat hinge in UNBUNDLE_PLAN.md §"Phase 1".
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let closure = closure_refs(&[]);
        let install_root = paths.store().join("mariadb-11.4.10");
        std::fs::create_dir_all(&install_root).unwrap();

        let client = reqwest::blocking::Client::new();
        let bar = DownloadBar::hidden();
        install_closure_peers(&client, &paths, &closure, "mariadb", "mariadb-11.4.10", &install_root, &bar)
            .unwrap();
        // No `store/` subdir is created when there's nothing to link.
        assert!(!install_root.join("store").exists());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn plan_closure_bytes_only_counts_missing() {
        // The bar's planned total reflects what we'll actually need to
        // download. If half the closure is already on disk from an
        // earlier install (the dedup happy path), planning must skip
        // those entries — otherwise the bar overshoots and never fills.
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        // Only openssl is already on disk.
        pre_create_store_entry(&paths, "openssl", "3.5.6", "99c0f6e8");

        // Hand-build a closure list where each entry advertises a
        // distinct non-zero size so we can assert exactly which one
        // was counted.
        let closure = vec![
            bougie_backend::ClosureRef {
                name: "openssl".into(),
                version: "3.5.6".into(),
                hash: "99c0f6e8".into(),
                sha256: format!("{:0>64}", 0),
                url: "https://x/o".into(),
                size: 1_000,
            },
            bougie_backend::ClosureRef {
                name: "zlib".into(),
                version: "1.3.2".into(),
                hash: "jbmj2bcm".into(),
                sha256: format!("{:0>64}", 1),
                url: "https://x/z".into(),
                size: 500,
            },
        ];

        let bar = DownloadBar::hidden();
        plan_closure_bytes(&paths, &closure, &bar);
        // openssl (1000) is on disk → skipped. zlib (500) is missing →
        // counted. So planned == 500.
        assert_eq!(bar.planned(), 500, "expected only the missing entry to be counted");
    }
}
