//! Property-based tests for the plist backend.
//!
//! Two families of properties live here:
//!
//! 1. `merge_shallow` invariants: every source key must end up in `live`
//!    with the source value; every live-only key must be untouched; key
//!    counts and ordering must follow the slice-7 contract.
//! 2. JSON↔plist round-trip on representable values: any JSON value with
//!    no `null` and no NaN/Inf reals survives `json_to_plist` →
//!    `plist_to_json` unchanged, modulo numeric coercion. Generators
//!    explicitly classify each leaf number as integer-valued or
//!    fractional so the property does not need to reason about subtype
//!    drift.

use super::json::json_to_plist;
use super::merge::merge_deep;
use super::merge::merge_shallow;
use super::transforms::plist_to_json;
use plist::Dictionary;
use plist::Value;
use proptest::collection::vec;
use proptest::prelude::*;
use serde_json::Number;
use serde_json::Value as J;

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

fn key_strategy() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,3}".prop_map(String::from)
}

/// Small JSON leaf strategy. NaN/Inf and `null` are excluded by
/// construction. Each numeric leaf is drawn as an integer-or-fraction
/// kind so the round-trip property knows which plist subtype to expect
/// after conversion.
fn json_leaf_strategy() -> impl Strategy<Value = J> {
    prop_oneof![
        Just(J::Bool(true)),
        Just(J::Bool(false)),
        any::<i32>().prop_map(|n| J::Number(Number::from(n))),
        // Restrict floats to a finite, deterministic range and force at
        // least a fractional part so they round-trip as `Real`.
        (-1_000i32..=1_000, 1u32..=999).prop_map(|(int_part, frac)| {
            let f = f64::from(int_part) + f64::from(frac) / 1000.0;
            // `from_f64` returns None only for NaN/Inf, which our range
            // excludes.
            J::Number(Number::from_f64(f).expect("finite by construction"))
        }),
        "[ -~]{0,8}".prop_map(J::String),
    ]
}

/// Recursive JSON strategy: leaves at the bottom, arrays and objects up
/// to a bounded depth so the test stays fast.
fn json_value_strategy() -> impl Strategy<Value = J> {
    json_leaf_strategy().prop_recursive(
        3,  // max depth
        16, // max total nodes
        4,  // max children per collection
        |inner| {
            prop_oneof![
                vec(inner.clone(), 0..=3).prop_map(J::Array),
                vec((key_strategy(), inner), 0..=3).prop_map(|kvs| {
                    let mut map = serde_json::Map::new();
                    for (k, v) in kvs {
                        map.insert(k, v);
                    }
                    J::Object(map)
                }),
            ]
        },
    )
}

/// Strategy for a `plist::Dictionary` of small scalar values. Used by
/// the merge invariants — we only need shallow merge to behave
/// correctly, so values are intentionally simple to keep failure output
/// readable.
fn small_value_strategy() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<bool>().prop_map(Value::Boolean),
        any::<i32>().prop_map(|n| Value::Integer(i64::from(n).into())),
        "[a-zA-Z0-9]{0,4}".prop_map(Value::String),
    ]
}

fn dict_strategy() -> impl Strategy<Value = Dictionary> {
    vec((key_strategy(), small_value_strategy()), 0..=6).prop_map(|kvs| {
        let mut d = Dictionary::new();
        for (k, v) in kvs {
            d.insert(k, v);
        }
        d
    })
}

/// Recursive plist `Value` strategy with a bounded depth/breadth budget.
/// Used by `merge_deep_invariants` so we get nested dicts on both the
/// `live` and `source` sides — without nesting, `merge_deep` would only
/// exercise the top-level shallow path.
fn nested_value_strategy() -> impl Strategy<Value = Value> {
    let leaf = small_value_strategy();
    // depth ≤ 4, breadth ≤ 5 per task spec.
    leaf.prop_recursive(4, 32, 5, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..=3).prop_map(Value::Array),
            vec((key_strategy(), inner), 0..=4).prop_map(|kvs| {
                let mut d = Dictionary::new();
                for (k, v) in kvs {
                    d.insert(k, v);
                }
                Value::Dictionary(d)
            }),
        ]
    })
}

fn nested_dict_strategy() -> impl Strategy<Value = Dictionary> {
    vec((key_strategy(), nested_value_strategy()), 0..=5).prop_map(|kvs| {
        let mut d = Dictionary::new();
        for (k, v) in kvs {
            d.insert(k, v);
        }
        d
    })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

fn config() -> proptest::test_runner::Config {
    proptest::test_runner::Config {
        cases: crate::test_support::PROPTEST_CASES,
        failure_persistence: None,
        ..proptest::test_runner::Config::default()
    }
}

proptest! {
    #![proptest_config(config())]

    /// `merge_shallow` overwrites every source key, leaves live-only keys
    /// alone, never deletes, and never introduces a `null`-equivalent.
    #[test]
    fn merge_shallow_invariants(live in dict_strategy(), source in dict_strategy()) {
        let live_before = live.clone();
        let mut merged = live;
        merge_shallow(&mut merged, &source, &[]);

        // (a) every source key now equals source[k]
        for (k, v) in &source {
            let got = merged
                .get(k)
                .ok_or_else(|| TestCaseError::fail(format!("missing source key {k:?} after merge")))?;
            prop_assert_eq!(got, v, "source key {} mismatched", k);
        }
        // (b) keys only in live retain their original value
        for (k, v) in &live_before {
            if !source.contains_key(k) {
                let got = merged.get(k).ok_or_else(|| {
                    TestCaseError::fail(format!("live-only key {k:?} was deleted"))
                })?;
                prop_assert_eq!(got, v, "live-only key {} was changed", k);
            }
        }
        // (c) result key count = |source| + |live - source|
        let live_only = live_before
            .keys()
            .filter(|k| !source.contains_key(k.as_str()))
            .count();
        prop_assert_eq!(merged.len(), source.len() + live_only);
        // (d) no key was deleted from live
        for k in live_before.keys() {
            prop_assert!(merged.contains_key(k), "key {:?} was deleted", k);
        }
    }

    /// Slice-7 ordering contract: existing live keys keep their original
    /// position; brand-new source keys are appended in source order.
    #[test]
    fn merge_shallow_preserves_order(live in dict_strategy(), source in dict_strategy()) {
        let live_keys_before: Vec<String> = live.keys().cloned().collect();
        let mut merged = live.clone();
        merge_shallow(&mut merged, &source, &[]);

        let merged_keys: Vec<String> = merged.keys().cloned().collect();

        // Existing live keys retain their relative positions in `merged`.
        let live_positions_in_merged: Vec<usize> = live_keys_before
            .iter()
            .map(|k| merged_keys.iter().position(|m| m == k).expect("present"))
            .collect();
        let mut sorted = live_positions_in_merged.clone();
        sorted.sort_unstable();
        prop_assert_eq!(
            live_positions_in_merged,
            sorted,
            "live keys reordered after merge"
        );

        // New keys (in source but not in live) appear in source order at
        // the tail of the merged dict.
        let new_keys_source_order: Vec<String> = source
            .keys()
            .filter(|k| !live.contains_key(k.as_str()))
            .cloned()
            .collect();
        let new_keys_merged_order: Vec<String> = merged_keys
            .iter()
            .filter(|k| !live.contains_key(k.as_str()))
            .cloned()
            .collect();
        prop_assert_eq!(new_keys_source_order, new_keys_merged_order);
    }

    /// `merge_deep` invariants on nested dict trees:
    ///
    /// * Source-side scalars and arrays overwrite the live value at that
    ///   key (the deep merge only recurses into dict-vs-dict pairs).
    /// * Live-only keys pass through unchanged.
    /// * Where both sides have a `Dictionary` at the same top-level key,
    ///   every source child key is reachable in the merged subdict — a
    ///   1-level recursive check sufficient to catch a regression that
    ///   stopped recursing.
    /// * Idempotence: applying `merge_deep` twice equals applying it once.
    #[test]
    fn merge_deep_invariants(live in nested_dict_strategy(), source in nested_dict_strategy()) {
        let live_before = live.clone();
        let mut merged = live;
        merge_deep(&mut merged, &source, &[])
            .map_err(|e| TestCaseError::fail(format!("merge_deep errored: {e}")))?;

        // (a) source scalars/arrays overwrite live's value at that key.
        // (Dict-vs-dict recurses; we check that case below.)
        for (k, src_v) in &source {
            match (live_before.get(k), src_v) {
                (Some(Value::Dictionary(_)), Value::Dictionary(_)) => {
                    // recursive case — checked separately
                }
                _ => {
                    let got = merged.get(k).ok_or_else(|| {
                        TestCaseError::fail(format!("source key {k:?} missing after merge"))
                    })?;
                    prop_assert_eq!(got, src_v, "source key {} not overwritten", k);
                }
            }
        }

        // (b) live-only keys retain their value.
        for (k, v) in &live_before {
            if !source.contains_key(k) {
                let got = merged.get(k).ok_or_else(|| {
                    TestCaseError::fail(format!("live-only key {k:?} was deleted"))
                })?;
                prop_assert_eq!(got, v, "live-only key {} was changed", k);
            }
        }

        // (c) dict-vs-dict recursion: every source child key must be
        // present in the merged subdict (1-level check).
        for (k, src_v) in &source {
            if let (Some(Value::Dictionary(live_sub)), Value::Dictionary(src_sub)) =
                (live_before.get(k), src_v)
            {
                let merged_sub = match merged.get(k) {
                    Some(Value::Dictionary(d)) => d,
                    other => {
                        return Err(TestCaseError::fail(format!(
                            "expected Dictionary at {k:?} after merge, got {other:?}"
                        )));
                    }
                };
                for (ck, cv) in src_sub {
                    match (live_sub.get(ck), cv) {
                        (Some(Value::Dictionary(_)), Value::Dictionary(_)) => {
                            // deeper dict-vs-dict — child must exist
                            prop_assert!(
                                merged_sub.contains_key(ck),
                                "child key {} missing under {}", ck, k
                            );
                        }
                        _ => {
                            let got = merged_sub.get(ck).ok_or_else(|| {
                                TestCaseError::fail(format!(
                                    "child source key {ck:?} missing under {k:?}"
                                ))
                            })?;
                            prop_assert_eq!(got, cv, "child {} under {} not overwritten", ck, k);
                        }
                    }
                }
            }
        }

        // (d) idempotence: a second merge over the same source is a no-op.
        let once = merged.clone();
        merge_deep(&mut merged, &source, &[])
            .map_err(|e| TestCaseError::fail(format!("second merge_deep errored: {e}")))?;
        prop_assert_eq!(once, merged, "merge_deep is not idempotent");
    }

    /// JSON → plist → JSON is the identity on values that contain no
    /// `null` and no NaN/Inf reals (the converter rejects both). Numeric
    /// subtype is preserved by construction in the generator.
    #[test]
    fn json_plist_round_trip(j in json_value_strategy()) {
        let p = json_to_plist(&j)
            .map_err(|e| TestCaseError::fail(format!("json_to_plist failed: {e}")))?;
        let back = plist_to_json(&p)
            .map_err(|e| TestCaseError::fail(format!("plist_to_json failed: {e}")))?;
        prop_assert_eq!(j, back);
    }
}
