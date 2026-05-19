//! End-to-end timing harness for `dump_autoload`.
//!
//! Point it at any project that has `composer.json`, `composer.lock`,
//! and a populated `vendor/` (i.e. a post-`composer install` tree —
//! the classmap scanner needs the materialized vendor layout).
//!
//! Usage:
//!   cargo run --release --example dump_bench -- <project-root> [iters]
//!
//! Reports per-iteration wall time plus min / median / max. The first
//! iteration is treated as a warm-up (its time is reported but
//! excluded from the summary) because the OS page cache for vendor/
//! is what we actually want to measure against.
//!
//! The target project is **never mutated**: we copy it to a tempdir
//! up front and run against the copy. This keeps the original tree
//! clean and means `cargo run --example dump_bench` can't pollute a
//! fixture or a real working repo.
//!
//! Composer comparison: if `BOUGIE_COMPOSER` is set (path to a
//! composer binary or `.phar`), or if the repo's pinned phar is
//! present at `$REPO_ROOT/.cache/composer-2.8.12.phar`, the example
//! also times `composer dump-autoload` against the same staged copy
//! and prints a speedup ratio. PHP needs to be on PATH for the phar
//! to invoke (use `nix develop`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use bougie_autoloader::{dump_autoload, DumpRequest};

fn main() {
    if let Err(e) = run() {
        eprintln!("dump_bench: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let optimize = raw.iter().any(|a| a == "-o" || a == "--optimize");
    let mut positional = raw.iter().filter(|a| !a.starts_with('-'));
    let project = PathBuf::from(positional.next().ok_or(
        "usage: dump_bench <project-root> [iters] [-o|--optimize]\n\
         project-root must contain composer.json, composer.lock, and a populated vendor/",
    )?);
    let iters: usize = positional.next().map(String::as_str).unwrap_or("5").parse()?;

    if !project.join("composer.json").is_file() {
        return Err(format!("no composer.json at {}", project.display()).into());
    }
    if !project.join("composer.lock").is_file() {
        return Err(format!("no composer.lock at {}", project.display()).into());
    }
    if !project.join("vendor").is_dir() {
        return Err(format!(
            "no vendor/ at {} — run `composer install` first",
            project.display()
        )
        .into());
    }

    let work_root = std::env::temp_dir().join(format!(
        "bougie-dump-bench-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    println!("staging copy of {} → {}", project.display(), work_root.display());
    copy_dir(&project, &work_root)
        .map_err(|e| format!("staging copy {} → {} failed: {e}", project.display(), work_root.display()))?;
    let guard = Cleanup(work_root.clone());

    println!(
        "iterations: {iters} (first is warmup){}\n",
        if optimize { " — optimize=true" } else { "" }
    );

    let bougie_summary = run_bougie(&guard.0, iters, optimize)?;

    let composer = locate_composer();
    let composer_summary = match composer {
        Some(cmd) => {
            println!();
            Some(run_composer(&cmd, &guard.0, iters, optimize)?)
        }
        None => {
            println!(
                "\n(skipping composer comparison: set BOUGIE_COMPOSER=<path> to a composer\n\
                 binary or .phar, or drop the pinned phar at .cache/composer-2.8.12.phar)"
            );
            None
        }
    };

    if let Some(cs) = composer_summary {
        let ratio = cs.median.as_secs_f64() / bougie_summary.median.as_secs_f64();
        println!();
        println!("comparison (median):");
        println!("  bougie:   {:>10.3?}", bougie_summary.median);
        println!("  composer: {:>10.3?}", cs.median);
        println!("  speedup:  {ratio:.2}x");
    }

    Ok(())
}

struct Summary {
    median: Duration,
}

fn run_bougie(
    project: &Path,
    iters: usize,
    optimize: bool,
) -> Result<Summary, Box<dyn std::error::Error>> {
    println!("== bougie dump_autoload ==");
    let req = DumpRequest {
        project_root: project,
        optimize,
        classmap_authoritative: false,
        no_dev: false,
        apcu_autoloader: false,
        apcu_prefix: None,
        autoloader_suffix: None,
    };
    let mut samples = Vec::with_capacity(iters);
    for i in 0..iters {
        let start = Instant::now();
        dump_autoload(&req)?;
        let elapsed = start.elapsed();
        let tag = if i == 0 { " (warmup)" } else { "" };
        println!("  iter {i:>2}: {:>10.3?}{tag}", elapsed);
        samples.push(elapsed);
    }
    Ok(summarize(samples))
}

enum ComposerCmd {
    Phar(PathBuf),
    Bin(PathBuf),
}

fn locate_composer() -> Option<ComposerCmd> {
    if let Ok(p) = std::env::var("BOUGIE_COMPOSER") {
        let path = PathBuf::from(p);
        if !path.is_file() {
            eprintln!("BOUGIE_COMPOSER={} is not a file", path.display());
            return None;
        }
        return Some(if path.extension().is_some_and(|e| e == "phar") {
            ComposerCmd::Phar(path)
        } else {
            ComposerCmd::Bin(path)
        });
    }
    // Repo's pinned phar — useful when running from inside this checkout.
    let pinned = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.cache/composer-2.8.12.phar");
    if pinned.is_file() {
        return Some(ComposerCmd::Phar(pinned));
    }
    None
}

fn run_composer(
    cmd: &ComposerCmd,
    project: &Path,
    iters: usize,
    optimize: bool,
) -> Result<Summary, Box<dyn std::error::Error>> {
    let (label, mut base) = match cmd {
        ComposerCmd::Phar(p) => (
            format!("php {}", p.display()),
            {
                let mut c = Command::new("php");
                c.arg(p);
                c
            },
        ),
        ComposerCmd::Bin(p) => (p.display().to_string(), Command::new(p)),
    };
    base.args(["dump-autoload", "--no-interaction", "--no-scripts", "--quiet"]);
    if optimize {
        // `dump-autoload` accepts `--optimize` / `-o`; the
        // `--optimize-autoloader` long form only exists on `install`/
        // `update` because it's a hint at install time. Mirror that.
        base.arg("--optimize");
    }
    base.current_dir(project);

    // Smoke-check: bail early if the composer invocation fails so we
    // don't time five back-to-back failures.
    let probe = base
        .status()
        .map_err(|e| format!("failed to invoke composer ({label}): {e}"))?;
    if !probe.success() {
        return Err(format!("composer ({label}) exited with {probe}").into());
    }

    println!("== composer dump-autoload ({label}) ==");
    let mut samples = Vec::with_capacity(iters);
    for i in 0..iters {
        let start = Instant::now();
        let status = base.status()?;
        let elapsed = start.elapsed();
        if !status.success() {
            return Err(format!("composer iter {i} exited with {status}").into());
        }
        let tag = if i == 0 { " (warmup)" } else { "" };
        println!("  iter {i:>2}: {:>10.3?}{tag}", elapsed);
        samples.push(elapsed);
    }
    Ok(summarize(samples))
}

fn summarize(mut samples: Vec<Duration>) -> Summary {
    // Drop warmup.
    if !samples.is_empty() {
        samples.remove(0);
    }
    samples.sort();
    let median = samples
        .get(samples.len() / 2)
        .copied()
        .unwrap_or_default();
    let min = samples.first().copied().unwrap_or_default();
    let max = samples.last().copied().unwrap_or_default();
    println!();
    println!("  summary (excluding warmup):");
    println!("    min:    {:>10.3?}", min);
    println!("    median: {:>10.3?}", median);
    println!("    max:    {:>10.3?}", max);
    Summary { median }
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Recursive copy that preserves symlinks (doesn't follow them) and
/// tolerates per-entry errors. Every error carries the exact path
/// and operation that failed so the user can see why staging died
/// (or, with per-entry tolerance, what got skipped).
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| ctx(e, "create_dir_all", dst))?;
    let read = std::fs::read_dir(src).map_err(|e| ctx(e, "read_dir", src))?;
    for entry in read {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("  warn: read_dir entry under {}: {e}", src.display());
                continue;
            }
        };
        let s = entry.path();
        let d = dst.join(entry.file_name());

        // symlink_metadata = inspect the link itself rather than its
        // target. If we used metadata() we'd follow links and inherit
        // any errors from broken targets.
        let meta = match std::fs::symlink_metadata(&s) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("  warn: symlink_metadata {}: {e}", s.display());
                continue;
            }
        };

        let ft = meta.file_type();
        let result: std::io::Result<()> = if ft.is_symlink() {
            copy_symlink(&s, &d).map_err(|e| ctx(e, "copy_symlink", &s))
        } else if ft.is_dir() {
            copy_dir(&s, &d)
        } else if ft.is_file() {
            std::fs::copy(&s, &d)
                .map(|_| ())
                .map_err(|e| ctx(e, "copy", &s))
        } else {
            // FIFO / socket / device — rare under a vendor/ tree;
            // skip and report so it isn't silently invisible.
            eprintln!(
                "  warn: skipping {} (unsupported file type: {ft:?})",
                s.display()
            );
            Ok(())
        };
        if let Err(e) = result {
            eprintln!("  warn: {e}");
        }
    }
    Ok(())
}

/// Wrap an [`io::Error`] with the path and operation that produced it.
/// The Rust stdlib's bare `fs::copy` / `read_dir` / etc. errors don't
/// include the path — and "No such file or directory" alone is no
/// help when something deep in a vendor/ tree fails.
fn ctx(e: std::io::Error, op: &str, path: &Path) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("{op} {}: {e}", path.display()))
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    let target = std::fs::read_link(src)?;
    // If the destination already exists (e.g. recursive directory
    // already created), drop it before re-linking.
    let _ = std::fs::remove_file(dst);
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(not(unix))]
fn copy_symlink(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    // Skip — Windows symlink semantics differ enough that we'd want a
    // separate code path. The bench example doesn't currently target
    // Windows.
    Ok(())
}
