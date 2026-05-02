//! JSON → `plist::Value` conversion for plist source files written as JSON.
//!
//! Per the RFC, JSON authoring loses access to plist-only types
//! (`<data>` and `<date>`); there is no way to opt into producing them
//! from JSON in slice 7. This module therefore only ever emits the
//! mappable shapes: bool, integer, real, string, array, dictionary.
//!
//! `null` has no plist counterpart and is rejected with a hint.
//!
//! Recursion is bounded by [`MAX_DEPTH`] so adversarial deeply-nested
//! input fails with a clear error instead of overflowing the stack. The
//! reverse direction (`plist_to_json` in [`super::transforms`]) applies
//! the same limit.

use anyhow::Result;
use anyhow::anyhow;
use plist::Dictionary;
use plist::Value;
use serde_json::Value as J;

/// Maximum nesting depth accepted during JSON ↔ plist conversion.
/// Applied symmetrically by [`super::transforms::plist_to_json`].
pub(super) const MAX_DEPTH: usize = 128;

/// Convert a `serde_json::Value` to an equivalent `plist::Value`.
pub(super) fn json_to_plist(value: &J) -> Result<Value> {
    json_to_plist_at(value, 0)
}

fn json_to_plist_at(value: &J, depth: usize) -> Result<Value> {
    if depth >= MAX_DEPTH {
        return Err(anyhow!(
            "JSON nesting depth exceeds limit of {MAX_DEPTH}; refusing to recurse further"
        ));
    }
    match value {
        J::Null => Err(anyhow!(
            "plist has no null type; remove the key, or use `.src.plist` if you need a non-JSON \
             type"
        )),
        J::Bool(b) => Ok(Value::Boolean(*b)),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i.into()))
            } else if let Some(u) = n.as_u64() {
                Ok(Value::Integer(u.into()))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Real(f))
            } else {
                Err(anyhow!("unsupported JSON number: {n}"))
            }
        }
        J::String(s) => Ok(Value::String(s.clone())),
        J::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(json_to_plist_at(item, depth + 1)?);
            }
            Ok(Value::Array(out))
        }
        J::Object(map) => {
            let mut dict = Dictionary::new();
            for (k, v) in map {
                dict.insert(k.clone(), json_to_plist_at(v, depth + 1)?);
            }
            Ok(Value::Dictionary(dict))
        }
    }
}
