//! Latency target: **< 100 ms** on a developer laptop.
//!
//! Workload: a 10 000-key JSON object. We drive the full
//! `Backend::process` pipeline against an *empty* live plist so the
//! measured work is dominated by:
//!
//! * `serde_json::from_slice` of the source body,
//! * `json_to_plist` conversion,
//! * `merge_shallow` against an empty live dict (essentially a memcpy
//!   of the whole source dict into live),
//! * `Value::to_writer_binary` of the result.
//!
//! That's the hot path the user actually pays when they bootstrap a
//! fresh plist via `chmm` — empty live, large JSON source. A
//! regression in any of the four steps above shows up here.

use criterion::Criterion;
use criterion::black_box;
use criterion::criterion_group;
use criterion::criterion_main;

#[path = "common/mod.rs"]
mod common;

use common::BENCH_SAMPLE_SIZE;
use common::PLIST_SHALLOW_DIRECTIVES;
use common::ProcessFixture;
use common::run_process;

const KEYS: usize = 10_000;

fn build_source_json() -> Vec<u8> {
    use std::fmt::Write as _;
    // Pre-size the buffer: each line is ~30 bytes, so 10k keys ≈
    // 300 KB. Avoids dozens of reallocations during the build.
    let mut s = String::with_capacity(KEYS * 32);
    s.push_str("{\n");
    for i in 0..KEYS {
        let comma = if i + 1 == KEYS { "" } else { "," };
        let _ = writeln!(s, "  \"k_{i:05}\": \"v_{i:05}\"{comma}");
    }
    s.push_str("}\n");
    s.into_bytes()
}

fn bench_json_to_plist(c: &mut Criterion) {
    let source = build_source_json();
    let fixture = ProcessFixture::inline(PLIST_SHALLOW_DIRECTIVES, &source);

    // Empty stdin: chmm coerces it to an empty top-level dict, so
    // every source key becomes a fresh insert. This is the
    // bootstrap-a-new-plist path.
    let live: &[u8] = b"";

    // Sanity-check before timing: the encoded binary plist for 10k
    // short keys should comfortably exceed 100 KB.
    let initial = run_process(&fixture, live);
    assert!(
        initial.len() > 100_000,
        "encode produced surprisingly small output ({} bytes)",
        initial.len()
    );

    let mut group = c.benchmark_group("plist_encode");
    group.sample_size(BENCH_SAMPLE_SIZE);
    group.bench_function("json_10k_to_binary_target_lt_100ms", |b| {
        b.iter(|| {
            let out = run_process(&fixture, black_box(live));
            black_box(out);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_json_to_plist);
criterion_main!(benches);
