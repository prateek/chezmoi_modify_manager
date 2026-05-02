//! Latency target: **< 100 ms** on a developer laptop.
//!
//! Workload: a synthetic large plist with ~10,000 top-level string
//! keys; the source JSON overrides ~50 of them.
//!
//! Benchmarked path: full `Backend::process` pipeline (decode JSON
//! source, decode binary live, `merge shallow`, encode binary). Useful
//! as a regression bound for users with very large `com.apple.*` style
//! preference files.

use criterion::Criterion;
use criterion::black_box;
use criterion::criterion_group;
use criterion::criterion_main;
use plist::Dictionary;
use plist::Value;

#[path = "common/mod.rs"]
mod common;

use common::BENCH_SAMPLE_SIZE;
use common::PLIST_SHALLOW_DIRECTIVES;
use common::ProcessFixture;
use common::run_process;

const TOP_KEYS: usize = 10_000;
const OVERRIDES: usize = 50;

fn build_live_large() -> Vec<u8> {
    let mut top = Dictionary::new();
    for i in 0..TOP_KEYS {
        // The string values are intentionally short; we want the
        // benchmark to spend its time on the merge/encode plumbing,
        // not on memcpy of long values.
        top.insert(format!("k_{i:05}"), Value::String(format!("v_{i:05}")));
    }
    let mut out = Vec::new();
    Value::Dictionary(top)
        .to_writer_binary(&mut out)
        .expect("encode large live binary plist");
    out
}

fn build_source_json_large() -> Vec<u8> {
    use std::fmt::Write as _;
    let mut s = String::from("{\n");
    for i in 0..OVERRIDES {
        // Override every 200th key — spread over the range so the
        // merge hits varied dict-bucket positions, not a single hot
        // run. Trailing comma handling is delicate; use a join-style
        // construction.
        let comma = if i + 1 == OVERRIDES { "" } else { "," };
        let key_idx = i * (TOP_KEYS / OVERRIDES);
        let _ = writeln!(s, "  \"k_{key_idx:05}\": \"override_{i}\"{comma}");
    }
    s.push_str("}\n");
    s.into_bytes()
}

fn bench_plist_large(c: &mut Criterion) {
    let live = build_live_large();
    let source = build_source_json_large();
    let fixture = ProcessFixture::inline(PLIST_SHALLOW_DIRECTIVES, &source);

    // Sanity check: 10k-key live should produce a substantial output.
    let initial = run_process(&fixture, &live);
    assert!(
        initial.len() > 1024,
        "large plist process produced unexpectedly small output ({} bytes)",
        initial.len()
    );

    let mut group = c.benchmark_group("plist_merge");
    group.sample_size(BENCH_SAMPLE_SIZE);
    group.bench_function("large_10k_keys_target_lt_100ms", |b| {
        b.iter(|| {
            let out = run_process(&fixture, black_box(&live));
            black_box(out);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_plist_large);
criterion_main!(benches);
