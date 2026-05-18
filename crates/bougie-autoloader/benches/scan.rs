//! Microbenchmarks for the classmap scan pipeline.
//!
//! These are correctness-of-perf guardrails for the hot inner loops
//! (`cleaner`, `finder`). They run on synthetic PHP source built at
//! bench-time so the corpus is self-contained — no fixture or
//! external `vendor/` tree required.
//!
//! Larger fixture-driven end-to-end benches (real `vendor/` trees,
//! `dump_autoload` wall time) arrive once the stress fixtures land.

use std::hint::black_box;

use bougie_autoloader::bench_api::{clean, find_classes};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

const SMALL_FILE: &[u8] = br"<?php

namespace Acme\Demo;

class Foo
{
    public function bar(): string
    {
        return 'hello';
    }
}
";

fn pad_source(base: &[u8], target_bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target_bytes + base.len());
    out.extend_from_slice(base);
    // Repeat method bodies (which contain strings + comments — work
    // for the cleaner) until we cross the target size.
    let filler = br"
    // line comment with class-shaped word: class
    public function method_{N}(): string
    {
        $heredoc = <<<EOT
class Hidden {}
EOT;
        return 'class Stringly { /* class */ }';
    }
";
    let mut idx = 0;
    while out.len() < target_bytes {
        // Replace `{N}` token so PHP would parse (it doesn't matter
        // for the scanner — the regex extracts class declarations and
        // method names aren't classes).
        let mut chunk = filler.to_vec();
        let placeholder = b"{N}";
        if let Some(pos) = chunk
            .windows(placeholder.len())
            .position(|w| w == placeholder)
        {
            let n = idx.to_string();
            chunk.splice(pos..pos + placeholder.len(), n.bytes());
        }
        out.extend_from_slice(&chunk);
        idx += 1;
    }
    // Close the trailing class brace so the source ends parseable-ish.
    out.extend_from_slice(b"}\n");
    out
}

fn cleaner_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("cleaner");
    for &size in &[1_024, 8 * 1_024, 64 * 1_024] {
        let src = pad_source(SMALL_FILE, size);
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(src.len()), &src, |b, src| {
            b.iter(|| {
                let cleaned = clean(black_box(src));
                black_box(cleaned);
            });
        });
    }
    group.finish();
}

fn finder_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("finder");
    for &size in &[1_024, 8 * 1_024, 64 * 1_024] {
        let src = pad_source(SMALL_FILE, size);
        group.throughput(Throughput::Bytes(src.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(src.len()), &src, |b, src| {
            b.iter(|| {
                let classes = find_classes(black_box(src));
                black_box(classes);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, cleaner_throughput, finder_throughput);
criterion_main!(benches);
