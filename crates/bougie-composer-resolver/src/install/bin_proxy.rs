//! Composer-compatible `vendor/bin/` proxy script generation.
//!
//! Matches Composer 2.8.12's `BinaryInstaller` behavior: PHP binaries
//! get a PHP proxy that sets `$GLOBALS['_composer_bin_dir']` and
//! `$GLOBALS['_composer_autoload_path']`; non-PHP binaries get a
//! shell proxy that resolves paths and sets `COMPOSER_RUNTIME_BIN_DIR`.

use std::path::Path;

use bougie_composer::lockfile::LockPackage;

#[derive(Debug, Clone, Default)]
pub(super) struct BinSummary {
    pub bins_installed: u32,
    pub warnings: Vec<String>,
}

pub(super) fn install_bin_proxies(
    project_root: &Path,
    packages: &[&LockPackage],
) -> BinSummary {
    let bin_dir = project_root.join("vendor/bin");
    let mut summary = BinSummary::default();

    for pkg in packages {
        if pkg.bin.is_empty() {
            continue;
        }
        for bin in &pkg.bin {
            let target = project_root
                .join("vendor")
                .join(&pkg.name)
                .join(bin);
            let link_name = match Path::new(bin).file_name() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => {
                    summary.warnings.push(format!(
                        "skipped bin `{bin}` for {}: invalid path",
                        pkg.name,
                    ));
                    continue;
                }
            };

            if !target.exists() {
                summary.warnings.push(format!(
                    "skipped installation of bin `{bin}` for package {}: \
                     file not found in package",
                    pkg.name,
                ));
                continue;
            }
            if target.is_dir() {
                summary.warnings.push(format!(
                    "skipped installation of bin `{bin}` for package {}: \
                     found a directory at that path",
                    pkg.name,
                ));
                continue;
            }

            let link_path = bin_dir.join(&link_name);
            if link_path.exists() && !is_composer_proxy(&link_path) {
                summary.warnings.push(format!(
                    "skipped installation of bin `{bin}` for package {}: \
                     name conflicts with an existing file",
                    pkg.name,
                ));
                continue;
            }

            if let Err(e) = std::fs::create_dir_all(&bin_dir) {
                summary.warnings.push(format!(
                    "skipped bin `{bin}` for {}: cannot create vendor/bin: {e}",
                    pkg.name,
                ));
                continue;
            }

            let bin_type = detect_bin_type(&target);
            let is_phpunit = pkg.name == "phpunit/phpunit"
                && bin == "phpunit";
            let proxy_content = match bin_type {
                BinType::PhpPlain => generate_php_proxy(&pkg.name, bin, false, is_phpunit),
                BinType::PhpShebang => generate_php_proxy(&pkg.name, bin, true, is_phpunit),
                BinType::Other => generate_shell_proxy(&pkg.name, bin),
            };

            if let Err(e) = std::fs::write(&link_path, proxy_content) {
                summary.warnings.push(format!(
                    "failed to write bin proxy for {}: {e}",
                    pkg.name,
                ));
                continue;
            }

            set_executable(&link_path);
            set_executable(&target);
            summary.bins_installed += 1;
        }
    }
    summary
}

#[allow(dead_code)]
pub(super) fn remove_bin_proxies(project_root: &Path, packages: &[&LockPackage]) {
    let bin_dir = project_root.join("vendor/bin");
    if !bin_dir.is_dir() {
        return;
    }
    for pkg in packages {
        for bin in &pkg.bin {
            let Some(name) = Path::new(bin).file_name() else { continue };
            let link = bin_dir.join(name);
            if link.exists() || link.symlink_metadata().is_ok() {
                let _ = std::fs::remove_file(&link);
            }
            let bat = bin_dir.join(format!("{}.bat", name.to_string_lossy()));
            if bat.exists() {
                let _ = std::fs::remove_file(&bat);
            }
        }
    }
    if bin_dir.is_dir() && is_dir_empty(&bin_dir) {
        let _ = std::fs::remove_dir(&bin_dir);
    }
}

fn is_dir_empty(path: &Path) -> bool {
    path.read_dir().is_ok_and(|mut d| d.next().is_none())
}

// --- Binary type detection ---

enum BinType {
    PhpPlain,
    PhpShebang,
    Other,
}

fn detect_bin_type(path: &Path) -> BinType {
    let Ok(content) = read_head(path, 500) else {
        return BinType::PhpPlain;
    };
    let s = String::from_utf8_lossy(&content);
    let has_shebang = s.starts_with("#!");
    if has_shebang {
        let after_shebang = s.find('\n')
            .map_or("", |pos| &s[pos + 1..]);
        let is_php = after_shebang
            .trim_start_matches(['\r', '\n', '\t', ' '])
            .starts_with("<?php");
        if is_php {
            return BinType::PhpShebang;
        }
        return BinType::Other;
    }
    let trimmed = s.trim_start_matches(['\r', '\n', '\t', ' ']);
    if trimmed.starts_with("<?php") {
        BinType::PhpPlain
    } else {
        BinType::Other
    }
}

fn read_head(path: &Path, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max_bytes];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

// --- Proxy script generation ---

fn generate_php_proxy(
    package_name: &str,
    bin_path: &str,
    has_shebang: bool,
    is_phpunit: bool,
) -> String {
    let rel = format!("/{package_name}/{bin_path}");
    let bin_path_comment = format!("../{package_name}/{bin_path}");

    let mut lines: Vec<String> = Vec::new();
    lines.push("#!/usr/bin/env php".into());
    lines.push("<?php".into());
    lines.push(String::new());
    lines.push("/**".into());
    lines.push(" * Proxy PHP file generated by Composer".into());
    lines.push(" *".into());
    if has_shebang {
        lines.push(format!(" * This file includes the referenced bin path ({bin_path_comment})"));
        lines.push(" * using a stream wrapper to prevent the shebang from being output on PHP<8".into());
    } else {
        lines.push(format!(" * This file includes the referenced bin path ({bin_path_comment})"));
    }
    lines.push(" *".into());
    lines.push(" * @generated".into());
    lines.push(" */".into());
    lines.push(String::new());
    lines.push("namespace Composer;".into());
    lines.push(String::new());
    lines.push("$GLOBALS['_composer_bin_dir'] = __DIR__;".into());
    lines.push(format!(
        "$GLOBALS['_composer_autoload_path'] = __DIR__ . '/..'.'/autoload.php';"
    ));

    if is_phpunit {
        lines.push(format!(
            "$GLOBALS['__PHPUNIT_ISOLATION_EXCLUDE_LIST'] = \
             $GLOBALS['__PHPUNIT_ISOLATION_BLACKLIST'] = \
             array(realpath(__DIR__ . '/..'.'{rel}'));"
        ));
    }

    if has_shebang {
        lines.push(String::new());
        lines.push("if (PHP_VERSION_ID < 80000) {".into());
        lines.push("    if (!class_exists('Composer\\BinProxyWrapper')) {".into());
        lines.push(BIN_PROXY_WRAPPER_CLASS.into());
        lines.push("    }".into());
        lines.push(String::new());
        lines.push("    if (".into());
        lines.push("        (function_exists('stream_get_wrappers') && in_array('phpvfscomposer', stream_get_wrappers(), true))".into());
        lines.push("        || (function_exists('stream_wrapper_register') && stream_wrapper_register('phpvfscomposer', 'Composer\\BinProxyWrapper'))".into());
        lines.push("    ) {".into());
        lines.push(format!(
            "        return include(\"phpvfscomposer://\" . __DIR__ . '/..'.'{rel}');"
        ));
        lines.push("    }".into());
        lines.push("}".into());
    }

    lines.push(String::new());
    lines.push(format!("return include __DIR__ . '/..'.'{rel}';"));
    lines.push(String::new());

    lines.join("\n")
}

const BIN_PROXY_WRAPPER_CLASS: &str = r#"        /**
         * @internal
         */
        final class BinProxyWrapper
        {
            private $handle;
            private $position;
            private $realpath;

            public function stream_open($path, $mode, $options, &$opened_path)
            {
                // get rid of phpvfscomposer:// prefix for __FILE__ & __DIR__ resolution
                $opened_path = substr($path, 17);
                $this->realpath = realpath($opened_path) ?: $opened_path;
                $opened_path = 'phpvfscomposer://'.$this->realpath;
                $this->handle = fopen($this->realpath, $mode);
                $this->position = 0;

                return (bool) $this->handle;
            }

            public function stream_read($count)
            {
                $data = fread($this->handle, $count);

                if ($this->position === 0) {
                    $data = preg_replace('{^#!.*\r?\n}', '', $data);
                }
                $data = str_replace('__DIR__', var_export(dirname($this->realpath), true), $data);
                $data = str_replace('__FILE__', var_export($this->realpath, true), $data);

                $this->position += strlen($data);

                return $data;
            }

            public function stream_cast($castAs)
            {
                return $this->handle;
            }

            public function stream_close()
            {
                fclose($this->handle);
            }

            public function stream_lock($operation)
            {
                return $operation ? flock($this->handle, $operation) : true;
            }

            public function stream_seek($offset, $whence)
            {
                if (0 === fseek($this->handle, $offset, $whence)) {
                    $this->position = ftell($this->handle);
                    return true;
                }

                return false;
            }

            public function stream_tell()
            {
                return $this->position;
            }

            public function stream_eof()
            {
                return feof($this->handle);
            }

            public function stream_stat()
            {
                return array();
            }

            public function stream_set_option($option, $arg1, $arg2)
            {
                return true;
            }

            public function url_stat($path, $flags)
            {
                $path = substr($path, 17);
                if (file_exists($path)) {
                    return stat($path);
                }

                return false;
            }
        }"#;

fn generate_shell_proxy(package_name: &str, bin_path: &str) -> String {
    let bin_file = Path::new(bin_path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let bin_dir_rel = Path::new(bin_path)
        .parent()
        .filter(|p| *p != Path::new(""))
        .map(|p| format!("'../{package_name}/{}'", p.display()))
        .unwrap_or_else(|| format!("'../{package_name}'"));

    format!(
        r#"#!/usr/bin/env sh

# Support bash to support `source` with fallback on $0 if this does not run with bash
# https://stackoverflow.com/a/35006505/6512
selfArg="$BASH_SOURCE"
if [ -z "$selfArg" ]; then
    selfArg="$0"
fi

self=$(realpath "$selfArg" 2> /dev/null)
if [ -z "$self" ]; then
    self="$selfArg"
fi

dir=$(cd "${{self%[/\\]*}}" > /dev/null; cd {bin_dir_rel} && pwd)

if [ -d /proc/cygdrive ]; then
    case $(which php) in
        $(readlink -n /proc/cygdrive)/*)
            # We are in Cygwin using Windows php, so the path must be translated
            dir=$(cygpath -m "$dir");
            ;;
    esac
fi

export COMPOSER_RUNTIME_BIN_DIR="$(cd "${{self%[/\\]*}}" > /dev/null; pwd)"

# If bash is sourcing this file, we have to source the target as well
bashSource="$BASH_SOURCE"
if [ -n "$bashSource" ]; then
    if [ "$bashSource" != "$0" ]; then
        source "${{dir}}/{bin_file}" "$@"
        return
    fi
fi

exec "${{dir}}/{bin_file}" "$@"
"#
    )
}

fn is_composer_proxy(path: &Path) -> bool {
    let Ok(content) = read_head(path, 200) else {
        return false;
    };
    let s = String::from_utf8_lossy(&content);
    s.contains("Proxy PHP file generated by Composer")
        || s.contains("COMPOSER_RUNTIME_BIN_DIR")
        || path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink())
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = path.metadata() {
        let mode = meta.permissions().mode() | 0o111;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

#[cfg(test)]
mod tests;
