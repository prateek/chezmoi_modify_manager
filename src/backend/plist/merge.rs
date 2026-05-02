//! Default merges for the plist backend.
//!
//! `merge_shallow` (the default) overrides each top-level key from source
//! onto live. `merge_deep` recurses on dict-vs-dict; arrays at any depth
//! replace wholesale; scalars override.
//!
//! `ignore path` directives (slice 8) are honoured here: when the merge
//! step's current path matches one of the supplied paths, the source-side
//! value is *not* written to live.

use crate::path::plist::PlistPath;
use crate::path::plist::Segment;
use anyhow::Result;
use anyhow::anyhow;
use plist::Dictionary;
use plist::Value;

/// Maximum nesting depth accepted during a deep-merge recursion. Mirrors
/// [`super::json::MAX_DEPTH`] — the JSON↔plist conversion is bounded, but
/// without an analogous bound here a binary plist with thousands of
/// nested dicts (which the `plist` crate happily decodes) would overflow
/// the stack inside `merge_deep_at`.
const MAX_DEPTH: usize = super::json::MAX_DEPTH;

/// Shallow merge: for each key in `source`, set `live[key] = source[key]`.
///
/// Existing live keys keep their position (replaced in place); new keys
/// are appended in source order. Live-only keys pass through unchanged.
/// Top-level keys whose name matches an `ignore_paths` entry of length 1
/// are skipped, leaving live's value untouched.
pub(super) fn merge_shallow(
    live: &mut Dictionary,
    source: &Dictionary,
    ignore_paths: &[PlistPath],
) {
    for (key, val) in source {
        if is_ignored(ignore_paths, &[key.as_str()]) {
            continue;
        }
        // `plist::Dictionary` preserves insertion order. `insert` either
        // updates an existing key in place (preserving its position) or
        // appends at the tail.
        live.insert(key.clone(), val.clone());
    }
}

/// Deep merge. Same as `merge_shallow` at the top, except both-dict
/// entries recurse. Arrays replace wholesale; scalars override.
///
/// Bounded by [`MAX_DEPTH`]: pathological input (thousands of nested
/// dicts in a binary plist) errors cleanly instead of overflowing the
/// stack.
pub(super) fn merge_deep(
    live: &mut Dictionary,
    source: &Dictionary,
    ignore_paths: &[PlistPath],
) -> Result<()> {
    merge_deep_at(live, source, &mut Vec::new(), ignore_paths, 0)
}

fn merge_deep_at(
    live: &mut Dictionary,
    source: &Dictionary,
    path_so_far: &mut Vec<String>,
    ignore_paths: &[PlistPath],
    depth: usize,
) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err(anyhow!(
            "plist deep merge nesting depth exceeds limit of {MAX_DEPTH}; refusing to recurse \
             further"
        ));
    }
    for (key, src_val) in source {
        path_so_far.push(key.clone());
        let cur: Vec<&str> = path_so_far.iter().map(String::as_str).collect();
        if is_ignored(ignore_paths, &cur) {
            path_so_far.pop();
            continue;
        }
        match (live.get_mut(key), src_val) {
            (Some(Value::Dictionary(live_inner)), Value::Dictionary(src_inner)) => {
                merge_deep_at(live_inner, src_inner, path_so_far, ignore_paths, depth + 1)?;
            }
            _ => {
                live.insert(key.clone(), src_val.clone());
            }
        }
        path_so_far.pop();
    }
    Ok(())
}

/// Returns `true` if the dotted dict-key path `current` exactly equals any
/// entry in `ignore_paths`. Only `Segment::Key` paths can match (the
/// merge spine never crosses arrays in slice 8). Wildcard/predicate
/// segments are rejected at config-parse time and so cannot appear here.
fn is_ignored(ignore_paths: &[PlistPath], current: &[&str]) -> bool {
    'outer: for p in ignore_paths {
        if p.segments.len() != current.len() {
            continue;
        }
        for (seg, name) in p.segments.iter().zip(current.iter()) {
            match seg {
                Segment::Key(k) if k == name => {}
                _ => continue 'outer,
            }
        }
        return true;
    }
    false
}
