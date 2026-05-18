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
//! up front and run `dump_autoload` against the copy. This keeps the
//! original tree clean and means a `cargo run --example dump_bench`
//! can't accidentally pollute a fixture or a real working repo.

use std::path::{Path, PathBuf};
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

    println!("dump_autoload @ {}", guard.0.display());
    println!("iterations: {iters} (first is warmup)");

    let req = DumpRequest {
        project_root: &guard.0,
        optimize: false,
        classmap_authoritative: false,
        no_dev: false,
    };

    let mut samples: Vec<Duration> = Vec::with_capacity(iters);
    for i in 0..iters {
        let start = Instant::now();
        dump_autoload(&req)?;
        let elapsed = start.elapsed();
        let label = if i == 0 { " (warmup)" } else { "" };
        println!("  iter {i:>2}: {:>10.3?}{label}", elapsed);
        samples.push(elapsed);
    }

    // Summary excludes the warmup iteration.
    let mut measured: Vec<Duration> = samples.into_iter().skip(1).collect();
    if measured.is_empty() {
        return Ok(());
    }
    measured.sort();
    let min = measured.first().copied().unwrap();
    let max = measured.last().copied().unwrap();
    let median = measured[measured.len() / 2];

    println!();
    println!("summary (excluding warmup):");
    println!("  min:    {:>10.3?}", min);
    println!("  median: {:>10.3?}", median);
    println!("  max:    {:>10.3?}", max);

    Ok(())
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
