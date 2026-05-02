//! Latency target: **linear in path length, not in tree size**.
//!
//! Workload: a `plist::Value::Dictionary` 100 levels deep, with
//! breadth 10 at every level. We resolve a path
//! `Outer.Inner.Inner.....Inner.Leaf` via a `set path` directive.
//!
//! Why bench the full pipeline? Resolve is `pub(crate)` and exposing
//! it just for benches would leak an internal API. Instead we
//! parametrise depth: criterion's per-input report makes the per-step
//! cost obvious — if doubling depth doesn't roughly double the time
//! (after subtracting a constant decode/encode floor), the resolver
//! has accidentally become tree-size-dependent.
//!
//! No absolute ms ceiling here; the contract is the **shape** of the
//! curve. A future change that makes resolution `O(tree_size)` will
//! show up as a step at the largest input even though the tree size
//! is held constant — so this catches the regression class the user
//! asked about.

use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::black_box;
use criterion::criterion_group;
use criterion::criterion_main;
use plist::Dictionary;
use plist::Value;

#[path = "common/mod.rs"]
mod common;

use common::BENCH_SAMPLE_SIZE;
use common::ProcessFixture;
use common::run_process;

const BREADTH: usize = 10;
/// The fixed total tree depth. Each iteration's resolve walks `depth`
/// of these levels — see `path_for_depth`.
const TREE_DEPTH: usize = 100;
/// The target leaf is named distinctly so the path is unambiguous.
const LEAF_KEY: &str = "Leaf";

/// Build a `Dictionary` with `BREADTH` siblings at every level, recursing
/// to the requested depth. The branch named "Inner" is the one our path
/// will follow; the others exist purely to bulk up the tree so a
/// regression that scans siblings shows up as super-linear scaling.
fn build_deep_tree(depth: usize) -> Value {
    if depth == 0 {
        // Leaf level: a dict containing the named leaf scalar plus
        // sibling distractors.
        let mut d = Dictionary::new();
        d.insert(
            LEAF_KEY.to_string(),
            Value::String("leaf_value".to_string()),
        );
        for i in 0..(BREADTH - 1) {
            d.insert(
                format!("sibling_leaf_{i}"),
                Value::String(format!("sv_{i}")),
            );
        }
        return Value::Dictionary(d);
    }
    let mut d = Dictionary::new();
    // The "Inner" branch is the one our path follows.
    d.insert("Inner".to_string(), build_deep_tree(depth - 1));
    // BREADTH - 1 distractors at every level — these are the "10 each
    // level" the user asked for. Their sub-trees are intentionally
    // shallow stubs (a string), not full clones; cloning the full deep
    // tree breadth-times would be exponential and tell us nothing.
    for i in 0..(BREADTH - 1) {
        d.insert(
            format!("sibling_branch_{i}"),
            Value::String(format!("sb_{i}")),
        );
    }
    Value::Dictionary(d)
}

/// Build the live binary plist for the chosen tree depth.
fn build_live(depth: usize) -> Vec<u8> {
    // The top-level "Outer" wrapper matches the user's path shape:
    // `Outer.Inner.....Leaf`. A leading dict-key must exist at the
    // top level for `set path` to walk into.
    let mut top = Dictionary::new();
    top.insert("Outer".to_string(), build_deep_tree(depth));
    let mut out = Vec::new();
    Value::Dictionary(top)
        .to_writer_binary(&mut out)
        .expect("encode deep live binary plist");
    out
}

/// Construct the path string that resolves to the leaf at the
/// requested walk-depth. Walking `depth` levels means `depth - 1`
/// `Inner` segments after the initial `Outer.Inner`, then `.Leaf`.
fn path_for_depth(depth: usize) -> String {
    let mut s = String::from("Outer");
    for _ in 0..depth {
        s.push_str(".Inner");
    }
    s.push('.');
    s.push_str(LEAF_KEY);
    s
}

fn bench_path_resolve(c: &mut Criterion) {
    // Build the deep live tree once. The same live bytes are reused
    // across walk-depths — the bench varies *path length*, not tree
    // shape, which is exactly the property we want to characterise.
    let live = build_live(TREE_DEPTH);

    let mut group = c.benchmark_group("path_resolve");
    group.sample_size(BENCH_SAMPLE_SIZE);

    // Sweep depth through {1, 10, 50, 100}. The output table makes
    // per-step cost obvious: divide (t_100 - t_1) by 99 to get the
    // amortised per-segment cost.
    for &depth in &[1usize, 10, 50, 100] {
        let path = path_for_depth(depth);
        // Use single-quotes around the value so the path itself can
        // be embedded inside a double-quoted directive string. The
        // grammar accepts both quote styles.
        let directives = format!(
            "#!/bin/sh\nlanguage plist\nmerge shallow\nset path \"{path}\" \
             \"new_leaf\"\n",
        );
        // Empty source body: `merge shallow` with no source keys is a
        // no-op on the live tree, so the only mutating work is the
        // `set path` walk. This isolates resolve cost from merge cost.
        let fixture = ProcessFixture::inline(&directives, b"{}");

        // Sanity: the merge must not error and must produce a
        // non-empty binary plist for any depth in our sweep.
        let initial = run_process(&fixture, &live);
        assert!(
            !initial.is_empty(),
            "set path at depth {depth} produced empty output"
        );

        group.bench_with_input(
            BenchmarkId::new("set_path_walk_depth", depth),
            &depth,
            |b, _| {
                b.iter(|| {
                    let out = run_process(&fixture, black_box(&live));
                    black_box(out);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_path_resolve);
criterion_main!(benches);
