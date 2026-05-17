//! In-process `unzip` shim for Composer's `ZipDownloader`.
//!
//! Composer prefers a PATH `unzip` over PHP's `ZipArchive` because it's
//! faster on large packages and preserves Unix mode bits and symlinks.
//! Its discovery is a Symfony `ExecutableFinder` PATH lookup with no
//! probe; its sole invocation shape (Linux/macOS) is
//!
//! ```text
//! unzip -qq <file> -d <dir>
//! ```
//!
//! See `src/Composer/Downloader/ZipDownloader.php` in composer/composer.
//! This module accepts that exact shape (plus `-q`, `-o`, `-n`, `-l`,
//! `-p` defensively), delegates extraction to the `zip` crate's built-in
//! `ZipArchive::extract` — which handles mode bits, symlinks, and
//! traversal-safety — and rejects unknown flags loudly.

use eyre::{eyre, Result};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// Entry point for the `Role::Unzip` arm of `shim::exec`.
pub fn run(args: Vec<OsString>) -> Result<ExitCode> {
    let parsed = parse(args)?;
    trace_invocation(&parsed);
    match parsed {
        Parsed::Extract { file, dest } => extract(&file, &dest),
        Parsed::List(file) => list(&file),
        Parsed::Pipe(file) => pipe(&file),
    }
}

#[derive(Debug)]
enum Parsed {
    Extract { file: PathBuf, dest: PathBuf },
    List(PathBuf),
    Pipe(PathBuf),
}

fn parse(args: Vec<OsString>) -> Result<Parsed> {
    let mut list_mode = false;
    let mut pipe_mode = false;
    let mut dest: Option<PathBuf> = None;
    let mut positional: Vec<PathBuf> = Vec::new();

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let s = arg
            .to_str()
            .ok_or_else(|| eyre!("unzip: non-UTF-8 argument: {:?}", arg))?;
        match s {
            // `-q`/`-qq` — quiet flags. We're always quiet anyway (no
            // entry-by-entry stdout). `-o` — overwrite-without-prompt;
            // ZipArchive::extract overwrites unconditionally, so it's a
            // no-op. All three are accepted purely so Composer's exact
            // invocation works.
            "-q" | "-qq" | "-o" => {}
            // Never-overwrite. Composer never passes this; reject so
            // a future caller doesn't silently get the wrong behavior.
            "-n" => return Err(eyre!("unzip: `-n` (never overwrite) is not supported")),
            "-l" => list_mode = true,
            "-p" => pipe_mode = true,
            "-d" => {
                let d = iter
                    .next()
                    .ok_or_else(|| eyre!("unzip: `-d` requires an argument"))?;
                dest = Some(PathBuf::from(d));
            }
            "--" => {
                for rest in iter.by_ref() {
                    positional.push(PathBuf::from(rest));
                }
                break;
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(eyre!(
                    "unzip: unsupported flag `{flag}` (bougie ships a Composer-targeted subset of unzip)"
                ));
            }
            _ => positional.push(PathBuf::from(s)),
        }
    }

    let file = positional
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("unzip: missing zipfile argument"))?;

    if list_mode && pipe_mode {
        return Err(eyre!("unzip: `-l` and `-p` are mutually exclusive"));
    }
    if list_mode {
        return Ok(Parsed::List(file));
    }
    if pipe_mode {
        return Ok(Parsed::Pipe(file));
    }
    Ok(Parsed::Extract {
        file,
        dest: dest.unwrap_or_else(|| PathBuf::from(".")),
    })
}

fn extract(file: &std::path::Path, dest: &std::path::Path) -> Result<ExitCode> {
    let fh = File::open(file).map_err(|e| eyre!("unzip: opening {}: {e}", file.display()))?;
    let mut archive =
        zip::ZipArchive::new(fh).map_err(|e| eyre!("unzip: reading {}: {e}", file.display()))?;
    std::fs::create_dir_all(dest)
        .map_err(|e| eyre!("unzip: creating {}: {e}", dest.display()))?;
    archive
        .extract(dest)
        .map_err(|e| eyre!("unzip: extracting {} -> {}: {e}", file.display(), dest.display()))?;
    Ok(ExitCode::SUCCESS)
}

fn list(file: &std::path::Path) -> Result<ExitCode> {
    let fh = File::open(file).map_err(|e| eyre!("unzip: opening {}: {e}", file.display()))?;
    let mut archive =
        zip::ZipArchive::new(fh).map_err(|e| eyre!("unzip: reading {}: {e}", file.display()))?;
    let mut out = io::stdout().lock();
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| eyre!("unzip: reading entry {i}: {e}"))?;
        writeln!(out, "{}", entry.name())?;
    }
    Ok(ExitCode::SUCCESS)
}

fn pipe(file: &std::path::Path) -> Result<ExitCode> {
    let fh = File::open(file).map_err(|e| eyre!("unzip: opening {}: {e}", file.display()))?;
    let mut archive =
        zip::ZipArchive::new(fh).map_err(|e| eyre!("unzip: reading {}: {e}", file.display()))?;
    let mut out = io::stdout().lock();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| eyre!("unzip: reading entry {i}: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        io::copy(&mut entry, &mut out)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// If `BOUGIE_TRACE_UNZIP` is set, append a one-line record of the
/// invocation to the file it names. Used by integration tests to assert
/// that Composer actually shelled out to our shim rather than falling
/// back to PHP's `ZipArchive`. Best-effort: tracing failures don't fail
/// the extraction.
fn trace_invocation(parsed: &Parsed) {
    let Some(path) = std::env::var_os("BOUGIE_TRACE_UNZIP") else {
        return;
    };
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let line = match parsed {
        Parsed::Extract { file, dest } => {
            format!("extract {} -> {}\n", file.display(), dest.display())
        }
        Parsed::List(f) => format!("list {}\n", f.display()),
        Parsed::Pipe(f) => format!("pipe {}\n", f.display()),
    };
    let _ = f.write_all(line.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn osv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(|s| OsString::from(*s)).collect()
    }

    #[test]
    fn parses_composer_invocation() {
        let p = parse(osv(&["-qq", "/tmp/pkg.zip", "-d", "/tmp/out"])).unwrap();
        match p {
            Parsed::Extract { file, dest } => {
                assert_eq!(file, PathBuf::from("/tmp/pkg.zip"));
                assert_eq!(dest, PathBuf::from("/tmp/out"));
            }
            other => panic!("expected extract, got {other:?}"),
        }
    }

    #[test]
    fn parses_dest_before_file() {
        let p = parse(osv(&["-d", "/tmp/out", "-qq", "/tmp/pkg.zip"])).unwrap();
        match p {
            Parsed::Extract { file, dest } => {
                assert_eq!(file, PathBuf::from("/tmp/pkg.zip"));
                assert_eq!(dest, PathBuf::from("/tmp/out"));
            }
            other => panic!("expected extract, got {other:?}"),
        }
    }

    #[test]
    fn defaults_dest_to_cwd() {
        let p = parse(osv(&["pkg.zip"])).unwrap();
        match p {
            Parsed::Extract { dest, .. } => assert_eq!(dest, PathBuf::from(".")),
            other => panic!("expected extract, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_flag() {
        let err = parse(osv(&["-x", "pkg.zip"])).unwrap_err();
        assert!(err.to_string().contains("unsupported flag"));
    }

    #[test]
    fn requires_zipfile() {
        let err = parse(osv(&["-qq"])).unwrap_err();
        assert!(err.to_string().contains("missing zipfile"));
    }

    #[test]
    fn list_mode_recognized() {
        let p = parse(osv(&["-l", "pkg.zip"])).unwrap();
        assert!(matches!(p, Parsed::List(_)));
    }
}
