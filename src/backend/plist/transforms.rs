//! Source-side transforms for the plist backend (slice 9).
//!
//! Transforms run between source decode and the default merge. They mutate
//! the source `plist::Value` tree in place; the live tree is untouched.
//! Each transform addresses a single node by [`PlistPath`].
//!
//! Four primitives ship today (slice 9 + follow-ups):
//!
//! * `join-lines` — `Array` of `String` becomes a single `String`
//!   joined with `\n`.
//! * `json-encode` — any value becomes a `String` containing canonical
//!   JSON. `<data>` and `<date>` cannot be encoded and error.
//! * `data-encode` — like `json-encode`, but the canonical JSON UTF-8
//!   bytes are wrapped as `plist::Value::Data` (i.e. emitted as a
//!   `<data>` element). Useful for apps that store JSON-shaped values
//!   as `<data>` blobs in plist preferences rather than `<string>`.
//! * `flatten-keys prefix="..." [json-encode-values | data-encode-values]`
//!   — addressed value must be a `Dictionary`. Each
//!   `(inner_key, inner_value)` is lifted into the addressed key's
//!   *parent* dict (root if at the top level) under the new key
//!   `<prefix><inner_key>`; the original key is removed. With
//!   `json-encode-values`, each lifted value is JSON-encoded; with
//!   `data-encode-values`, each lifted value is JSON-encoded then wrapped
//!   as `<data>`. The two flags are mutually exclusive (rejected at
//!   parse time).
//!
//! Transforms run in declaration order. A `flatten-keys` transform that
//! introduces new sibling keys can be followed by other transforms
//! addressing those keys.

use crate::config::PlistTransform;
use crate::path::plist::PlistPath;
use crate::path::plist::Segment;
use anyhow::Result;
use anyhow::anyhow;
use plist::Value;
use serde_json::Value as J;

use super::path_resolve;
use super::util::value_kind;

/// Apply a single transform to `source`. Errors carry the path so the
/// caller can prefix with file role (always source).
pub(super) fn apply(source: &mut Value, path: &PlistPath, kind: &PlistTransform) -> Result<()> {
    match kind {
        PlistTransform::JoinLines => apply_join_lines(source, path),
        PlistTransform::JsonEncode => apply_json_encode(source, path),
        PlistTransform::DataEncode => apply_data_encode(source, path),
        PlistTransform::FlattenKeys {
            prefix,
            json_encode_values,
            data_encode_values,
        } => apply_flatten_keys(
            source,
            path,
            prefix,
            *json_encode_values,
            *data_encode_values,
        ),
    }
}

fn apply_join_lines(source: &mut Value, path: &PlistPath) -> Result<()> {
    let target = path_resolve::resolve_mut(source, path)
        .map_err(|e| anyhow!("transform `join-lines` path \"{path}\": {e}"))?;
    let arr = match target {
        Value::Array(a) => a,
        other => {
            return Err(anyhow!(
                "transform `join-lines` path \"{path}\": expected array of strings, got {kind}",
                kind = value_kind(other)
            ));
        }
    };
    let mut joined = String::new();
    for (i, item) in arr.iter().enumerate() {
        match item {
            Value::String(s) => {
                if i > 0 {
                    joined.push('\n');
                }
                joined.push_str(s);
            }
            other => {
                return Err(anyhow!(
                    "transform `join-lines` path \"{path}\": array element {i} is {kind}, \
                     expected string",
                    kind = value_kind(other)
                ));
            }
        }
    }
    *target = Value::String(joined);
    Ok(())
}

fn apply_json_encode(source: &mut Value, path: &PlistPath) -> Result<()> {
    let target = path_resolve::resolve_mut(source, path)
        .map_err(|e| anyhow!("transform `json-encode` path \"{path}\": {e}"))?;
    let encoded = encode_value_as_json_string(target)
        .map_err(|e| anyhow!("transform `json-encode` path \"{path}\": {e}"))?;
    *target = Value::String(encoded);
    Ok(())
}

fn apply_data_encode(source: &mut Value, path: &PlistPath) -> Result<()> {
    let target = path_resolve::resolve_mut(source, path)
        .map_err(|e| anyhow!("transform `data-encode` path \"{path}\": {e}"))?;
    let encoded = encode_value_as_json_string(target)
        .map_err(|e| anyhow!("transform `data-encode` path \"{path}\": {e}"))?;
    *target = Value::Data(encoded.into_bytes());
    Ok(())
}

/// Encode a `plist::Value` subtree as a canonical JSON string. Shared by
/// `json-encode`, `data-encode`, and `flatten-keys *-values`.
fn encode_value_as_json_string(value: &Value) -> Result<String> {
    let json = plist_to_json(value)?;
    Ok(serde_json::to_string(&json)?)
}

fn apply_flatten_keys(
    source: &mut Value,
    path: &PlistPath,
    prefix: &str,
    json_encode_values: bool,
    data_encode_values: bool,
) -> Result<()> {
    // The addressed value's *parent* dict is the lift target. For a
    // top-level path like `shortcuts`, the parent is the root dict. For
    // a nested path like `Outer.Inner`, the parent is the dict at
    // `Outer`. Predicates / wildcards / array indices don't make sense
    // here — the addressed key must be a `Segment::Key`, and the parent
    // must already be a `Dictionary`.
    let (last, parents) = path
        .segments
        .split_last()
        .ok_or_else(|| anyhow!("transform `flatten-keys` path is empty"))?;
    let key = match last {
        Segment::Key(k) => k.clone(),
        _ => {
            return Err(anyhow!(
                "transform `flatten-keys` path \"{path}\": addressed segment must be a key \
                 (`flatten-keys` does not accept array index, wildcard, or predicate selectors)"
            ));
        }
    };

    // Walk to the parent dict.
    let parent_path = PlistPath {
        segments: parents.to_vec(),
    };
    let parent_node: &mut Value = if parents.is_empty() {
        source
    } else {
        path_resolve::resolve_mut(source, &parent_path)
            .map_err(|e| anyhow!("transform `flatten-keys` path \"{path}\": {e}"))?
    };
    let parent_dict = match parent_node {
        Value::Dictionary(d) => d,
        other => {
            return Err(anyhow!(
                "transform `flatten-keys` path \"{path}\": parent of `{key}` is {kind}, expected \
                 dictionary",
                kind = value_kind(other)
            ));
        }
    };

    // Remove the addressed entry so we own the inner dict.
    let removed = parent_dict
        .remove(&key)
        .ok_or_else(|| anyhow!("transform `flatten-keys` path \"{path}\": missing key `{key}`"))?;
    let inner_dict = match removed {
        Value::Dictionary(d) => d,
        other => {
            // Restore the original value before erroring so subsequent
            // transforms see a consistent tree (defensive — `process`
            // surfaces the error to the caller anyway).
            let kind = value_kind(&other);
            parent_dict.insert(key.clone(), other);
            return Err(anyhow!(
                "transform `flatten-keys` path \"{path}\": addressed value is {kind}, expected \
                 dictionary"
            ));
        }
    };
    for (inner_key, inner_val) in inner_dict {
        let new_key = format!("{prefix}{inner_key}");
        let final_val = if json_encode_values {
            let encoded = encode_value_as_json_string(&inner_val).map_err(|e| {
                anyhow!(
                    "transform `flatten-keys` path \"{path}\" json-encode-values for inner key \
                     `{inner_key}`: {e}"
                )
            })?;
            Value::String(encoded)
        } else if data_encode_values {
            let encoded = encode_value_as_json_string(&inner_val).map_err(|e| {
                anyhow!(
                    "transform `flatten-keys` path \"{path}\" data-encode-values for inner key \
                     `{inner_key}`: {e}"
                )
            })?;
            Value::Data(encoded.into_bytes())
        } else {
            inner_val
        };
        parent_dict.insert(new_key, final_val);
    }
    Ok(())
}

/// Convert a `plist::Value` to a `serde_json::Value`. `Data`, `Date`, and
/// `Uid` have no canonical JSON representation and error out. `Real`
/// values that are not finite (NaN/inf) error because JSON has no
/// representation for them either. Nesting deeper than
/// [`super::json::MAX_DEPTH`] is rejected so adversarial input cannot
/// blow the stack.
pub(super) fn plist_to_json(v: &Value) -> Result<J> {
    plist_to_json_at(v, 0)
}

fn plist_to_json_at(v: &Value, depth: usize) -> Result<J> {
    if depth >= super::json::MAX_DEPTH {
        return Err(anyhow!(
            "plist nesting depth exceeds limit of {limit}; refusing to recurse further",
            limit = super::json::MAX_DEPTH,
        ));
    }
    Ok(match v {
        Value::String(s) => J::String(s.clone()),
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(i) => {
            if let Some(n) = i.as_signed() {
                J::Number(n.into())
            } else if let Some(n) = i.as_unsigned() {
                J::Number(n.into())
            } else {
                return Err(anyhow!("integer out of JSON range"));
            }
        }
        Value::Real(r) => {
            let n = serde_json::Number::from_f64(*r)
                .ok_or_else(|| anyhow!("real {r} cannot be encoded as JSON (NaN or inf)"))?;
            J::Number(n)
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(plist_to_json_at(item, depth + 1)?);
            }
            J::Array(out)
        }
        Value::Dictionary(d) => {
            let mut map = serde_json::Map::with_capacity(d.len());
            for (k, val) in d {
                map.insert(k.clone(), plist_to_json_at(val, depth + 1)?);
            }
            J::Object(map)
        }
        Value::Data(_) => return Err(anyhow!("cannot encode plist <data> as JSON")),
        Value::Date(_) => return Err(anyhow!("cannot encode plist <date> as JSON")),
        Value::Uid(_) => return Err(anyhow!("cannot encode plist <uid> as JSON")),
        _ => return Err(anyhow!("cannot encode unknown plist value as JSON")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Dictionary;
    use pretty_assertions::assert_eq;

    fn pp(s: &str) -> PlistPath {
        s.parse().unwrap()
    }

    fn dict(pairs: &[(&str, Value)]) -> Value {
        let mut d = Dictionary::new();
        for (k, v) in pairs {
            d.insert((*k).to_owned(), v.clone());
        }
        Value::Dictionary(d)
    }

    // ---- join-lines -------------------------------------------------------

    #[test]
    fn join_lines_array_of_strings() {
        let mut root = dict(&[(
            "browserHostWhitelist",
            Value::Array(vec![
                Value::String("a.example".into()),
                Value::String("b.example".into()),
                Value::String("c.example".into()),
            ]),
        )]);
        apply(
            &mut root,
            &pp("browserHostWhitelist"),
            &PlistTransform::JoinLines,
        )
        .unwrap();
        let v = path_resolve::resolve(&root, &pp("browserHostWhitelist")).unwrap();
        assert_eq!(v, &Value::String("a.example\nb.example\nc.example".into()));
    }

    #[test]
    fn join_lines_non_string_errors() {
        let mut root = dict(&[(
            "list",
            Value::Array(vec![Value::String("a".into()), Value::Integer(7.into())]),
        )]);
        let err = apply(&mut root, &pp("list"), &PlistTransform::JoinLines).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("element 1") && msg.contains("integer"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn join_lines_non_array_errors() {
        let mut root = dict(&[("x", Value::String("hello".into()))]);
        let err = apply(&mut root, &pp("x"), &PlistTransform::JoinLines).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expected array of strings") && msg.contains("string"),
            "unexpected error: {msg}"
        );
    }

    // ---- json-encode -----------------------------------------------------

    #[test]
    fn json_encode_dict() {
        let inner = dict(&[
            ("a", Value::Integer(1.into())),
            ("b", Value::String("x".into())),
        ]);
        let mut root = dict(&[("sidebar", inner)]);
        apply(&mut root, &pp("sidebar"), &PlistTransform::JsonEncode).unwrap();
        let v = path_resolve::resolve(&root, &pp("sidebar")).unwrap();
        let Value::String(s) = v else {
            panic!("expected string, got {v:?}");
        };
        // serde_json preserves insertion order for `Map<String, Value>` here
        // because we feed it in that order.
        assert_eq!(s, r#"{"a":1,"b":"x"}"#);
    }

    #[test]
    fn json_encode_array() {
        let mut root = dict(&[(
            "items",
            Value::Array(vec![
                Value::Integer(1.into()),
                Value::Integer(2.into()),
                Value::Integer(3.into()),
            ]),
        )]);
        apply(&mut root, &pp("items"), &PlistTransform::JsonEncode).unwrap();
        let v = path_resolve::resolve(&root, &pp("items")).unwrap();
        assert_eq!(v, &Value::String("[1,2,3]".into()));
    }

    #[test]
    fn json_encode_scalar() {
        let mut root = dict(&[("n", Value::Integer(42.into()))]);
        apply(&mut root, &pp("n"), &PlistTransform::JsonEncode).unwrap();
        let v = path_resolve::resolve(&root, &pp("n")).unwrap();
        assert_eq!(v, &Value::String("42".into()));
    }

    // ---- data-encode -----------------------------------------------------

    #[test]
    fn data_encode_dict_yields_data_blob() {
        let inner = dict(&[
            ("title", Value::String("Precise".into())),
            ("id", Value::String("A1".into())),
        ]);
        let mut root = dict(&[("customPrompts", Value::Array(vec![inner]))]);
        apply(&mut root, &pp("customPrompts"), &PlistTransform::DataEncode).unwrap();
        let v = path_resolve::resolve(&root, &pp("customPrompts")).unwrap();
        let Value::Data(bytes) = v else {
            panic!("expected <data>, got {v:?}");
        };
        let s = std::str::from_utf8(bytes).unwrap();
        assert_eq!(s, r#"[{"id":"A1","title":"Precise"}]"#);
    }

    #[test]
    fn data_encode_scalar() {
        let mut root = dict(&[("n", Value::Integer(42.into()))]);
        apply(&mut root, &pp("n"), &PlistTransform::DataEncode).unwrap();
        let v = path_resolve::resolve(&root, &pp("n")).unwrap();
        let Value::Data(bytes) = v else {
            panic!("expected <data>, got {v:?}");
        };
        assert_eq!(std::str::from_utf8(bytes).unwrap(), "42");
    }

    #[test]
    fn data_encode_existing_data_errors() {
        let mut root = dict(&[("blob", Value::Data(vec![1, 2, 3]))]);
        let err = apply(&mut root, &pp("blob"), &PlistTransform::DataEncode).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("<data>"), "unexpected error: {msg}");
    }

    #[test]
    fn flatten_keys_with_data_encode_values() {
        // Real-world shape: `shortcut.<name>` keys whose values are
        // `<data>` blobs of the JSON-encoded inner dict.
        let nested = dict(&[
            ("key", Value::String("j".into())),
            ("control", Value::Boolean(true)),
        ]);
        let inner = dict(&[("focusDown", nested)]);
        let mut root = dict(&[("shortcuts", inner)]);
        let kind = PlistTransform::FlattenKeys {
            prefix: "shortcut.".into(),
            json_encode_values: false,
            data_encode_values: true,
        };
        apply(&mut root, &pp("shortcuts"), &kind).unwrap();
        let Value::Dictionary(d) = &root else {
            panic!("not a dict");
        };
        let v = d.get("shortcut.focusDown").expect("lifted key missing");
        let Value::Data(bytes) = v else {
            panic!("expected <data>, got {v:?}");
        };
        assert_eq!(
            std::str::from_utf8(bytes).unwrap(),
            r#"{"control":true,"key":"j"}"#
        );
    }

    #[test]
    fn json_encode_data_errors() {
        let mut root = dict(&[("blob", Value::Data(vec![1, 2, 3]))]);
        let err = apply(&mut root, &pp("blob"), &PlistTransform::JsonEncode).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("<data>"),
            "expected error to name <data>, got: {msg}"
        );
    }

    // ---- flatten-keys ----------------------------------------------------

    #[test]
    fn flatten_keys_lifts_inner_dict() {
        let inner = dict(&[
            ("open", Value::String("Cmd-O".into())),
            ("save", Value::String("Cmd-S".into())),
        ]);
        let mut root = dict(&[("shortcuts", inner), ("other", Value::Integer(1.into()))]);
        let kind = PlistTransform::FlattenKeys {
            prefix: "shortcut.".into(),
            json_encode_values: false,
            data_encode_values: false,
        };
        apply(&mut root, &pp("shortcuts"), &kind).unwrap();
        let Value::Dictionary(d) = &root else {
            panic!("not a dict");
        };
        assert_eq!(d.get("shortcut.open"), Some(&Value::String("Cmd-O".into())));
        assert_eq!(d.get("shortcut.save"), Some(&Value::String("Cmd-S".into())));
        assert!(d.get("shortcuts").is_none());
        assert_eq!(d.get("other"), Some(&Value::Integer(1.into())));
    }

    #[test]
    fn flatten_keys_with_json_encode_values() {
        let nested = dict(&[("x", Value::Integer(7.into()))]);
        let inner = dict(&[("first", nested), ("second", Value::Integer(2.into()))]);
        let mut root = dict(&[("data", inner)]);
        let kind = PlistTransform::FlattenKeys {
            prefix: "d.".into(),
            json_encode_values: true,
            data_encode_values: false,
        };
        apply(&mut root, &pp("data"), &kind).unwrap();
        let Value::Dictionary(d) = &root else {
            panic!("not a dict");
        };
        assert_eq!(d.get("d.first"), Some(&Value::String(r#"{"x":7}"#.into())));
        assert_eq!(d.get("d.second"), Some(&Value::String("2".into())));
    }

    #[test]
    fn flatten_keys_deletes_original_parent_key() {
        let inner = dict(&[("k", Value::Integer(1.into()))]);
        let mut root = dict(&[("parent", inner)]);
        let kind = PlistTransform::FlattenKeys {
            prefix: "p.".into(),
            json_encode_values: false,
            data_encode_values: false,
        };
        apply(&mut root, &pp("parent"), &kind).unwrap();
        let Value::Dictionary(d) = &root else {
            panic!("not a dict");
        };
        assert!(d.get("parent").is_none(), "parent key not removed");
        assert!(d.get("p.k").is_some());
    }

    #[test]
    fn flatten_keys_at_nested_path() {
        // `Outer.Inner` flattens into `Outer` (NOT root). Sibling keys at
        // `Outer` are preserved; root keys are untouched.
        let inner = dict(&[
            ("open", Value::String("Cmd-O".into())),
            ("save", Value::String("Cmd-S".into())),
        ]);
        let outer = dict(&[("Inner", inner), ("sibling", Value::Integer(1.into()))]);
        let mut root = dict(&[("Outer", outer), ("rootKey", Value::Integer(99.into()))]);
        let kind = PlistTransform::FlattenKeys {
            prefix: "shortcut.".into(),
            json_encode_values: false,
            data_encode_values: false,
        };
        apply(&mut root, &pp("Outer.Inner"), &kind).unwrap();

        // Lifted keys land on `Outer`, not on root.
        let outer_v = path_resolve::resolve(&root, &pp("Outer")).unwrap();
        let Value::Dictionary(d) = outer_v else {
            panic!("not a dict")
        };
        assert_eq!(d.get("shortcut.open"), Some(&Value::String("Cmd-O".into())));
        assert_eq!(d.get("shortcut.save"), Some(&Value::String("Cmd-S".into())));
        assert!(d.get("Inner").is_none(), "Inner key not removed");
        assert_eq!(d.get("sibling"), Some(&Value::Integer(1.into())));

        // Root keys untouched.
        let Value::Dictionary(rd) = &root else {
            panic!("not a dict")
        };
        assert!(rd.get("shortcut.open").is_none(), "lifted to wrong level");
        assert_eq!(rd.get("rootKey"), Some(&Value::Integer(99.into())));
    }

    #[test]
    fn flatten_keys_at_root_unchanged() {
        // Existing root-level case keeps working: `shortcuts` at the top
        // level still lifts into the root dict.
        let inner = dict(&[
            ("open", Value::String("Cmd-O".into())),
            ("save", Value::String("Cmd-S".into())),
        ]);
        let mut root = dict(&[("shortcuts", inner), ("other", Value::Integer(1.into()))]);
        let kind = PlistTransform::FlattenKeys {
            prefix: "shortcut.".into(),
            json_encode_values: false,
            data_encode_values: false,
        };
        apply(&mut root, &pp("shortcuts"), &kind).unwrap();
        let Value::Dictionary(d) = &root else {
            panic!("not a dict");
        };
        assert_eq!(d.get("shortcut.open"), Some(&Value::String("Cmd-O".into())));
        assert_eq!(d.get("shortcut.save"), Some(&Value::String("Cmd-S".into())));
        assert!(d.get("shortcuts").is_none());
        assert_eq!(d.get("other"), Some(&Value::Integer(1.into())));
    }

    #[test]
    fn flatten_keys_into_array_errors() {
        // The path's parent is an array element (not a dict). The
        // addressed segment is `Item[0]` — index, not a key — so we
        // reject before even resolving the parent. (Covers the
        // "addressed segment must be a key" guard.)
        let mut root = dict(&[(
            "Items",
            Value::Array(vec![dict(&[("inner", Value::Integer(1.into()))])]),
        )]);
        let kind = PlistTransform::FlattenKeys {
            prefix: "p.".into(),
            json_encode_values: false,
            data_encode_values: false,
        };
        let err = apply(&mut root, &pp("Items[0]"), &kind).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("addressed segment must be a key"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn flatten_keys_parent_is_array_errors() {
        // The addressed key is keyed (`elem`), but its parent walks
        // through an array index — the parent of the final key is
        // therefore a dict (the array element). We construct a case
        // where the parent really is an array by giving the parent
        // path an array-typed segment that resolves as the immediate
        // parent.
        //
        // Simplest concrete shape: the lookup `Outer.Inner` where
        // `Outer` is an array (not dict). Resolving the parent fails
        // because `Outer` is an array, not a dict.
        let mut root = dict(&[(
            "Outer",
            Value::Array(vec![dict(&[("Inner", Value::Integer(1.into()))])]),
        )]);
        let kind = PlistTransform::FlattenKeys {
            prefix: "p.".into(),
            json_encode_values: false,
            data_encode_values: false,
        };
        let err = apply(&mut root, &pp("Outer.Inner"), &kind).unwrap_err();
        let msg = format!("{err}");
        // The descent error from `path_resolve` flags the dotted
        // continuation against an array.
        assert!(
            msg.contains("dotted continuation against array")
                || msg.contains("expected dictionary"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn flatten_keys_value_not_dict_errors() {
        let mut root = dict(&[("x", Value::String("nope".into()))]);
        let kind = PlistTransform::FlattenKeys {
            prefix: "p.".into(),
            json_encode_values: false,
            data_encode_values: false,
        };
        let err = apply(&mut root, &pp("x"), &kind).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expected dictionary") && msg.contains("string"),
            "unexpected error: {msg}"
        );
    }

    // ---- ordering / no-match --------------------------------------------

    #[test]
    fn transforms_run_in_declaration_order() {
        // Two transforms: first `flatten-keys` lifts inner keys, then
        // `json-encode` encodes one of the resulting top-level entries.
        let inner = dict(&[
            ("a", Value::String("hello".into())),
            ("b", Value::Integer(2.into())),
        ]);
        let mut root = dict(&[("group", inner)]);

        // 1) flatten group → top-level keys p.a / p.b.
        apply(
            &mut root,
            &pp("group"),
            &PlistTransform::FlattenKeys {
                prefix: "p.".into(),
                json_encode_values: false,
                data_encode_values: false,
            },
        )
        .unwrap();
        // 2) json-encode the lifted "p.b" key.
        apply(&mut root, &pp(r#""p.b""#), &PlistTransform::JsonEncode).unwrap();

        let Value::Dictionary(d) = &root else {
            panic!("not a dict");
        };
        assert_eq!(d.get("p.a"), Some(&Value::String("hello".into())));
        assert_eq!(d.get("p.b"), Some(&Value::String("2".into())));
    }

    #[test]
    fn transform_path_no_match_errors() {
        let mut root = dict(&[("a", Value::Integer(1.into()))]);
        let err = apply(&mut root, &pp("nope"), &PlistTransform::JoinLines).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("missing key `nope`"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn flatten_keys_wildcard_addressed_segment_errors() {
        // The path's last segment must be a `Key`. A trailing `[*]`
        // (wildcard) is rejected with a clear error that names the
        // disallowed selector kinds.
        let mut root = dict(&[(
            "Items",
            Value::Array(vec![Value::Integer(1.into()), Value::Integer(2.into())]),
        )]);
        let kind = PlistTransform::FlattenKeys {
            prefix: "p.".into(),
            json_encode_values: false,
            data_encode_values: false,
        };
        let err = apply(&mut root, &pp("Items[*]"), &kind).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("addressed segment must be a key"),
            "unexpected error: {msg}",
        );
    }
}
