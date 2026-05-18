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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let project = PathBuf::from(args.next().ok_or(
        "usage: dump_bench <project-root> [iters]\n\
         project-root must contain composer.json, composer.lock, and a populated vendor/",
    )?);
    let iters: usize = args.next().as_deref().unwrap_or("5").parse()?;

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
    copy_dir(&project, &work_root)?;
    let guard = Cleanup(work_root.clone());

    println!("iterations: {iters} (first is warmup)\n");

    let bougie_summary = run_bougie(&guard.0, iters)?;

    let composer = locate_composer();
    let composer_summary = match composer {
        Some(cmd) => {
            println!();
            Some(run_composer(&cmd, &guard.0, iters)?)
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

fn run_bougie(project: &Path, iters: usize) -> Result<Summary, Box<dyn std::error::Error>> {
    println!("== bougie dump_autoload ==");
    let req = DumpRequest {
        project_root: project,
        optimize: false,
        classmap_authoritative: false,
        no_dev: false,
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

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if s.is_dir() {
            copy_dir(&s, &d)?;
        } else {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}
