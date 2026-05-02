//! Property-based tests for the byte-span patcher.
//!
//! For any source buffer and any set of non-overlapping `Patch`es,
//! `apply_patches` is order-insensitive: every permutation of the patch
//! list must produce the same byte output. The production code achieves
//! this by sorting in descending start-offset order before applying;
//! proptest exercises arbitrary input orderings to confirm the contract
//! generalises.

use super::patch::Patch;
use super::patch::apply_patches;
use proptest::prelude::*;

/// Generator: a 32-byte source plus 1..=8 non-overlapping replacement
/// patches.
///
/// `apply_patches` explicitly allows *touching* spans — `b.start == a.end`
/// is fine, only `b.start < a.end` is an overlap. The previous generator
/// used `hash_set` of distinct offsets which made touching spans
/// unreachable; a regression that flipped `>` to `>=` in the overlap
/// check would not have been caught. We instead pick lengths and gaps
/// independently so each pair has `start[i] < end[i]` (rules out
/// zero-length spans, which the patcher rejects when they share an
/// offset with another span) while still permitting `start[i+1] ==
/// end[i]` between consecutive pairs.
fn source_and_patches_strategy() -> impl Strategy<Value = (Vec<u8>, Vec<Patch>)> {
    let source = proptest::collection::vec(any::<u8>(), 32..=32);
    let count = 1usize..=8;
    (source, count).prop_flat_map(|(src, n)| {
        let len = src.len();
        // Distribute `len` across `n` patch lengths (1..) and `n + 1`
        // gaps (0.., gap == 0 produces a touching pair). Drawing each
        // independently and rescaling keeps the cursor within [0, len]
        // while letting both the touching case (gap = 0) and arbitrary
        // gaps (gap > 0) appear with non-trivial probability.
        let lengths = proptest::collection::vec(1usize..=4, n);
        let gaps = proptest::collection::vec(0usize..=4, n + 1);
        let replacements =
            proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..=4), n);
        (Just(src), lengths, gaps, replacements).prop_map(move |(src, lengths, gaps, reps)| {
            // Total budget consumed by spans + gaps must fit in
            // `len`. Scale uniformly if it exceeds.
            let total: usize = lengths.iter().sum::<usize>() + gaps.iter().sum::<usize>();
            let (lengths, gaps) = if total > len {
                let scale = |v: &[usize]| -> Vec<usize> {
                    v.iter().map(|x| (x * len) / total.max(1)).collect()
                };
                let mut lens = scale(&lengths);
                // Re-floor any zero lengths back to 1 — we want
                // every span non-empty per the constraint above.
                for l in &mut lens {
                    if *l == 0 {
                        *l = 1;
                    }
                }
                (lens, scale(&gaps))
            } else {
                (lengths, gaps)
            };
            let mut cursor = 0usize;
            let mut patches = Vec::with_capacity(reps.len());
            for (i, replacement) in reps.into_iter().enumerate() {
                cursor = (cursor + gaps[i]).min(len);
                let start = cursor;
                let end = (start + lengths[i]).min(len);
                if end <= start {
                    // Out of room — drop the remaining patches
                    // rather than synthesise a zero-length one.
                    break;
                }
                cursor = end;
                patches.push(Patch {
                    span: start..end,
                    replacement,
                });
            }
            (src, patches)
        })
    })
}

fn config() -> proptest::test_runner::Config {
    proptest::test_runner::Config {
        cases: crate::test_support::PROPTEST_CASES,
        failure_persistence: None,
        ..proptest::test_runner::Config::default()
    }
}

proptest! {
    #![proptest_config(config())]

    /// For non-overlapping patches over the same source, applying them in
    /// any input order produces the same output bytes (the patcher
    /// internally sorts by descending start offset, but proptest picks
    /// adversarial orderings to confirm the property holds in general).
    #[test]
    fn patch_composition_is_order_independent(
        (source, patches) in source_and_patches_strategy(),
        seed in any::<u64>(),
    ) {
        let baseline = apply_patches(&source, patches.clone())
            .expect("non-overlapping patches must apply cleanly");

        // Use a tiny LCG seeded from `seed` to permute `patches`. We
        // can't pull in `rand` here, but a deterministic Fisher-Yates
        // suffices.
        let mut shuffled = patches;
        let mut state = seed | 1;
        for i in (1..shuffled.len()).rev() {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let j = (state >> 33) as usize % (i + 1);
            shuffled.swap(i, j);
        }

        let permuted = apply_patches(&source, shuffled)
            .expect("permuted non-overlapping patches must still apply");
        prop_assert_eq!(baseline, permuted);
    }
}
