//! Latency target: **< 1 ms** on a developer laptop.
//!
//! Workload: a representative dotfile-sized plist:
//!
//! * top-level dict with ~100 keys mixing string/integer/bool/real;
//! * one nested dict with ~20 keys;
//! * one array with ~10 elements.
//!
//! The benchmarked path is the full `Backend::process` pipeline:
//! decode source JSON, decode live binary plist, run `merge shallow`,
//! re-encode binary plist. If a refactor pushes this past 1 ms, this
//! bench will fail the eyeball test in CI — that's the regression we
//! want to catch.

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

/// Build the dotfile-sized live plist as a `Value` tree, then encode to
/// binary so the benchmarked pipeline pays the realistic decode cost.
fn build_live_dotfile() -> Vec<u8> {
    let mut top = Dictionary::new();
    // ~100 top-level keys, mixed scalar types.
    for i in 0..70 {
        top.insert(format!("string_key_{i}"), Value::String(format!("val_{i}")));
    }
    for i in 0..15 {
        top.insert(format!("int_key_{i}"), Value::Integer(i64::from(i).into()));
    }
    for i in 0..10 {
        top.insert(format!("bool_key_{i}"), Value::Boolean(i % 2 == 0));
    }
    for i in 0..5 {
        // f64::from(i) is precise for the small ints we use here.
        top.insert(format!("real_key_{i}"), Value::Real(f64::from(i) + 0.5));
    }
    // One nested dict, ~20 keys.
    let mut nested = Dictionary::new();
    for i in 0..20 {
        nested.insert(format!("nested_{i}"), Value::String(format!("nv_{i}")));
    }
    top.insert("nested".to_string(), Value::Dictionary(nested));
    // One array with ~10 elements.
    let arr: Vec<Value> = (0..10).map(|i| Value::String(format!("arr_{i}"))).collect();
    top.insert("array".to_string(), Value::Array(arr));

    let mut out = Vec::new();
    Value::Dictionary(top)
        .to_writer_binary(&mut out)
        .expect("encode live binary plist");
    out
}

/// Build a JSON source body that overrides ~10 of the live keys. The
/// shape is intentionally small relative to the live tree — a typical
/// `modify_*` dotfile script overrides a handful of keys, not the
/// majority.
fn build_source_json() -> Vec<u8> {
    use std::fmt::Write as _;
    let mut s = String::from("{\n");
    for i in 0..8 {
        // `write!` to a `String` is infallible; the `Result` is
        // discarded explicitly to satisfy `format_push_string`.
        let _ = writeln!(s, "  \"string_key_{i}\": \"override_{i}\",");
    }
    s.push_str("  \"int_key_0\": 999,\n");
    s.push_str("  \"bool_key_0\": false\n");
    s.push_str("}\n");
    s.into_bytes()
}

fn bench_plist_dotfile(c: &mut Criterion) {
    let live = build_live_dotfile();
    let source = build_source_json();
    let fixture = ProcessFixture::inline(PLIST_SHALLOW_DIRECTIVES, &source);

    // Sanity-check that the pipeline actually does something before we
    // time it. A bench that silently no-ops would happily report 0ns.
    let initial = run_process(&fixture, &live);
    assert!(!initial.is_empty(), "plist process produced empty output");

    let mut group = c.benchmark_group("plist_merge");
    // 20 samples is enough to detect regressions; full sample count
    // burns CI minutes for no extra signal at this latency scale.
    group.sample_size(BENCH_SAMPLE_SIZE);
    group.bench_function("dotfile_target_lt_1ms", |b| {
        b.iter(|| {
            // black_box on inputs so LLVM can't constant-fold the live
            // bytes; black_box on output so the merge isn't elided.
            let out = run_process(&fixture, black_box(&live));
            black_box(out);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_plist_dotfile);
criterion_main!(benches);
