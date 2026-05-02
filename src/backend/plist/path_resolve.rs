//! Resolve a [`PlistPath`] against a [`plist::Value`] tree.
//!
//! Used by the path-level plist directives (`ignore path`, `remove path`,
//! `add:hide path`, `add:remove path`).
//!
//! Two flavours of API are exposed:
//!
//! * **Single-resolve** (`resolve`, `resolve_mut`, `set`, `remove`): the
//!   selector must address exactly one node. `[*]` is rejected (multi-match
//!   doesn't fit the single-target shape); `[key="value"]` is supported and
//!   must match exactly one element of the addressed array.
//! * **Multi-resolve** (`resolve_all_paths`): expands `[*]` segments into
//!   concrete `Index(N)` paths. Predicate segments resolve to their single
//!   match (also as `Index(N)`). The caller iterates the returned concrete
//!   paths and applies its directive to each via the single-resolve API.

use super::util::value_kind;
use crate::path::plist::PlistPath;
use crate::path::plist::Segment;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use plist::Value;

/// Wraps a not-found error so callers can distinguish it from genuine
/// resolution failures (ambiguous predicates, shape mismatches) via
/// [`is_not_found`]. The wrapper's `Display` defers to the inner
/// message, so the user-visible output is unchanged.
///
/// The filter path swallows not-found errors (a missing live entry is
/// a legitimate no-op for `ignore path`) but propagates everything else.
#[derive(Debug)]
pub(crate) struct NotFoundError(pub(crate) anyhow::Error);

impl std::fmt::Display for NotFoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Use anyhow's full-chain rendering so contexts added by callers
        // ("`ignore path \"X\"` (live)") still appear when we propagate.
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NotFoundError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// Tag a not-found error so callers can downcast to [`NotFoundError`]
/// and distinguish it from ambiguous-/shape-mismatch errors.
fn not_found(err: anyhow::Error) -> anyhow::Error {
    anyhow!(NotFoundError(err))
}

/// Did a resolution failure originate from a missing target rather than
/// from an ambiguity or shape mismatch? Used by the re-add filter to
/// silently no-op truly-absent targets while still surfacing real
/// configuration errors (e.g. predicate `[key=...]` matches multiple
/// elements).
pub(crate) fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain()
        .any(<dyn std::error::Error + 'static>::is::<NotFoundError>)
}

/// Format the prefix of a path up to and including `idx` segments. Used in
/// error messages so the user knows where in the path the problem occurred.
fn fmt_prefix(path: &PlistPath, idx: usize) -> String {
    PlistPath {
        segments: path.segments.iter().take(idx + 1).cloned().collect(),
    }
    .to_string()
}

/// Resolve `path` against `root`, producing a reference to the addressed
/// value. The selector must address exactly one node — `[*]` is rejected,
/// and `[key="value"]` must match exactly one array element.
pub(crate) fn resolve<'a>(root: &'a Value, path: &PlistPath) -> Result<&'a Value> {
    let mut current: &Value = root;
    for (i, seg) in path.segments.iter().enumerate() {
        current = step(current, seg, path, i)?;
    }
    Ok(current)
}

/// Mutable counterpart to [`resolve`].
pub(crate) fn resolve_mut<'a>(root: &'a mut Value, path: &PlistPath) -> Result<&'a mut Value> {
    let mut current: &mut Value = root;
    for (i, seg) in path.segments.iter().enumerate() {
        current = step_mut(current, seg, path, i)?;
    }
    Ok(current)
}

fn step<'a>(node: &'a Value, seg: &Segment, path: &PlistPath, idx: usize) -> Result<&'a Value> {
    match (seg, node) {
        (Segment::Key(name), Value::Dictionary(d)) => d.get(name).ok_or_else(|| {
            not_found(anyhow!(
                "plist path {path}: missing key `{name}` at {prefix}",
                prefix = fmt_prefix(path, idx)
            ))
        }),
        (Segment::Index(n), Value::Array(arr)) => arr.get(*n).ok_or_else(|| {
            not_found(anyhow!(
                "plist path {path}: index {n} out of bounds (len {len}) at {prefix}",
                len = arr.len(),
                prefix = fmt_prefix(path, idx)
            ))
        }),
        (Segment::Predicate { key, value }, Value::Array(arr)) => {
            let n = predicate_match(arr, key, value, path, idx)?;
            Ok(&arr[n])
        }
        (Segment::Key(_), other) => Err(anyhow!(
            "plist path {path}: dotted continuation against {kind} at {prefix}",
            kind = value_kind(other),
            prefix = fmt_prefix(path, idx)
        )),
        (Segment::Index(_), other) => Err(anyhow!(
            "plist path {path}: index `[N]` against {kind} at {prefix}",
            kind = value_kind(other),
            prefix = fmt_prefix(path, idx)
        )),
        (Segment::Predicate { .. }, other) => Err(anyhow!(
            "plist path {path}: predicate `[key=...]` against {kind} at {prefix}",
            kind = value_kind(other),
            prefix = fmt_prefix(path, idx)
        )),
        (Segment::Wildcard, _) => bail!(
            "plist path {path}: `[*]` selector cannot resolve to a single node at {prefix}",
            prefix = fmt_prefix(path, idx)
        ),
    }
}

fn step_mut<'a>(
    node: &'a mut Value,
    seg: &Segment,
    path: &PlistPath,
    idx: usize,
) -> Result<&'a mut Value> {
    match seg {
        Segment::Key(name) => match node {
            Value::Dictionary(d) => {
                let prefix = fmt_prefix(path, idx);
                d.get_mut(name).ok_or_else(|| {
                    not_found(anyhow!(
                        "plist path {path}: missing key `{name}` at {prefix}"
                    ))
                })
            }
            other => Err(anyhow!(
                "plist path {path}: dotted continuation against {kind} at {prefix}",
                kind = value_kind(other),
                prefix = fmt_prefix(path, idx)
            )),
        },
        Segment::Index(n) => match node {
            Value::Array(arr) => {
                let len = arr.len();
                let prefix = fmt_prefix(path, idx);
                arr.get_mut(*n).ok_or_else(|| {
                    not_found(anyhow!(
                        "plist path {path}: index {n} out of bounds (len {len}) at {prefix}"
                    ))
                })
            }
            other => Err(anyhow!(
                "plist path {path}: index `[N]` against {kind} at {prefix}",
                kind = value_kind(other),
                prefix = fmt_prefix(path, idx)
            )),
        },
        Segment::Predicate { key, value } => match node {
            Value::Array(arr) => {
                let n = predicate_match(arr, key, value, path, idx)?;
                Ok(&mut arr[n])
            }
            other => Err(anyhow!(
                "plist path {path}: predicate `[key=...]` against {kind} at {prefix}",
                kind = value_kind(other),
                prefix = fmt_prefix(path, idx)
            )),
        },
        Segment::Wildcard => bail!(
            "plist path {path}: `[*]` selector cannot resolve to a single node at {prefix}",
            prefix = fmt_prefix(path, idx)
        ),
    }
}

/// Find the unique array index whose element is a dict containing
/// `<key>key</key>` with `<string>value</string>`. Errors if zero or more
/// than one element matches. Non-string `<key>` values silently fail to
/// match (RFC: predicate equality is string-only).
fn predicate_match(
    arr: &[Value],
    key: &str,
    value: &str,
    path: &PlistPath,
    idx: usize,
) -> Result<usize> {
    let mut matches: Vec<usize> = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        if let Value::Dictionary(d) = item
            && let Some(Value::String(s)) = d.get(key)
            && s == value
        {
            matches.push(i);
        }
    }
    let prefix = fmt_prefix(path, idx);
    match matches.as_slice() {
        [single] => Ok(*single),
        [] => Err(not_found(anyhow!(
            "plist path {path}: predicate `[{key}={value:?}]` matched no elements at {prefix}"
        ))),
        many => bail!(
            "plist path {path}: predicate `[{key}={value:?}]` matched {n} elements at {prefix} \
             (must match exactly one)",
            n = many.len()
        ),
    }
}

/// Expand any `[*]` and `[key="value"]` segments in `path` into concrete
/// `Index(N)` paths. The returned paths address single nodes and can be
/// fed back through [`resolve`], [`resolve_mut`], [`set`], or [`remove`].
///
/// Wildcards expand to one concrete path per current array element.
/// Predicates resolve to the single matching index (errors on zero or
/// multiple matches, same as the single-resolve API).
///
/// If the path contains no `[*]` and no predicates, the result is a
/// one-element vector containing a clone of `path`. The caller can treat
/// that as the single-target case uniformly.
pub(crate) fn resolve_all_paths(root: &Value, path: &PlistPath) -> Result<Vec<PlistPath>> {
    let mut out: Vec<PlistPath> = Vec::new();
    expand(
        root,
        path,
        0,
        &mut Vec::with_capacity(path.segments.len()),
        &mut out,
    )?;
    Ok(out)
}

fn expand(
    node: &Value,
    path: &PlistPath,
    idx: usize,
    acc: &mut Vec<Segment>,
    out: &mut Vec<PlistPath>,
) -> Result<()> {
    if idx == path.segments.len() {
        out.push(PlistPath {
            segments: acc.clone(),
        });
        return Ok(());
    }
    let seg = &path.segments[idx];
    match seg {
        Segment::Wildcard => match node {
            Value::Array(arr) => {
                for (i, item) in arr.iter().enumerate() {
                    acc.push(Segment::Index(i));
                    expand(item, path, idx + 1, acc, out)?;
                    acc.pop();
                }
                Ok(())
            }
            other => Err(anyhow!(
                "plist path {path}: `[*]` against {kind} at {prefix}",
                kind = value_kind(other),
                prefix = fmt_prefix(path, idx)
            )),
        },
        Segment::Predicate { key, value } => match node {
            Value::Array(arr) => {
                let n = predicate_match(arr, key, value, path, idx)?;
                acc.push(Segment::Index(n));
                expand(&arr[n], path, idx + 1, acc, out)?;
                acc.pop();
                Ok(())
            }
            other => Err(anyhow!(
                "plist path {path}: predicate `[key=...]` against {kind} at {prefix}",
                kind = value_kind(other),
                prefix = fmt_prefix(path, idx)
            )),
        },
        Segment::Key(_) | Segment::Index(_) => {
            // Reuse the single-step descent for these — they don't
            // branch.
            let next = step(node, seg, path, idx)?;
            acc.push(seg.clone());
            expand(next, path, idx + 1, acc, out)?;
            acc.pop();
            Ok(())
        }
    }
}

/// Remove the value addressed by `path` and return it. The selector must
/// address exactly one node (no `[*]`); use [`resolve_all_paths`] +
/// `remove` per concrete path for multi-match removals.
pub(crate) fn remove(root: &mut Value, path: &PlistPath) -> Result<Value> {
    if path.segments.is_empty() {
        bail!("plist path is empty; cannot remove the root");
    }
    // Navigate to the parent node. If the path has one segment, the parent
    // is `root` itself.
    let (last, parents) = path.segments.split_last().expect("non-empty");
    let mut current = root;
    for (i, seg) in parents.iter().enumerate() {
        current = step_mut(current, seg, path, i)?;
    }
    let parent_idx = parents.len();
    match (last, current) {
        (Segment::Key(name), Value::Dictionary(d)) => d.remove(name).ok_or_else(|| {
            not_found(anyhow!(
                "plist path {path}: missing key `{name}` at {prefix}",
                prefix = fmt_prefix(path, parent_idx)
            ))
        }),
        (Segment::Index(n), Value::Array(arr)) => {
            let len = arr.len();
            if *n >= len {
                return Err(not_found(anyhow!(
                    "plist path {path}: index {n} out of bounds (len {len}) at {prefix}",
                    prefix = fmt_prefix(path, parent_idx)
                )));
            }
            Ok(arr.remove(*n))
        }
        (Segment::Predicate { key, value }, Value::Array(arr)) => {
            let n = predicate_match(arr, key, value, path, parent_idx)?;
            Ok(arr.remove(n))
        }
        (Segment::Key(_), other) => Err(anyhow!(
            "plist path {path}: dotted continuation against {kind} at {prefix}",
            kind = value_kind(other),
            prefix = fmt_prefix(path, parent_idx)
        )),
        (Segment::Index(_), other) => Err(anyhow!(
            "plist path {path}: index `[N]` against {kind} at {prefix}",
            kind = value_kind(other),
            prefix = fmt_prefix(path, parent_idx)
        )),
        (Segment::Predicate { .. }, other) => Err(anyhow!(
            "plist path {path}: predicate `[key=...]` against {kind} at {prefix}",
            kind = value_kind(other),
            prefix = fmt_prefix(path, parent_idx)
        )),
        (Segment::Wildcard, _) => bail!(
            "plist path {path}: `[*]` not allowed in single-target `remove`; expand via \
             resolve_all_paths first"
        ),
    }
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

    #[test]
    fn resolve_top_level_string() {
        let root = dict(&[("a", Value::String("hi".into()))]);
        let v = resolve(&root, &pp("a")).unwrap();
        assert_eq!(v, &Value::String("hi".into()));
    }

    #[test]
    fn resolve_nested_dict() {
        let inner = dict(&[("inner", Value::Integer(7.into()))]);
        let root = dict(&[("outer", inner)]);
        let v = resolve(&root, &pp("outer.inner")).unwrap();
        assert_eq!(v, &Value::Integer(7.into()));
    }

    #[test]
    fn resolve_array_index() {
        let arr = Value::Array(vec![
            Value::Integer(1.into()),
            Value::Integer(2.into()),
            Value::Integer(3.into()),
        ]);
        let root = dict(&[("nums", arr)]);
        let v = resolve(&root, &pp("nums[1]")).unwrap();
        assert_eq!(v, &Value::Integer(2.into()));
    }

    #[test]
    fn resolve_chained_indices() {
        let inner = Value::Array(vec![Value::Integer(10.into()), Value::Integer(20.into())]);
        let outer = Value::Array(vec![Value::Integer(0.into()), inner]);
        let root = dict(&[("m", outer)]);
        let v = resolve(&root, &pp("m[1][0]")).unwrap();
        assert_eq!(v, &Value::Integer(10.into()));
    }

    #[test]
    fn resolve_quoted_key() {
        let root = dict(&[("com.apple.dock", Value::Integer(36.into()))]);
        let v = resolve(&root, &pp(r#""com.apple.dock""#)).unwrap();
        assert_eq!(v, &Value::Integer(36.into()));
    }

    #[test]
    fn error_index_against_dict() {
        let root = dict(&[("a", Value::Integer(1.into()))]);
        // Path "a[0]" — `[0]` against the integer at `a`.
        let err = resolve(&root, &pp("a[0]")).unwrap_err().to_string();
        assert!(
            err.contains("`[N]` against integer"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_dot_against_array() {
        let root = dict(&[("arr", Value::Array(vec![Value::Integer(1.into())]))]);
        // Path "arr.x" — dotted continuation against array.
        let err = resolve(&root, &pp("arr.x")).unwrap_err().to_string();
        assert!(
            err.contains("dotted continuation against array"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_walk_into_scalar() {
        let root = dict(&[("a", Value::String("hi".into()))]);
        // Path "a.b" — walking into a string scalar.
        let err = resolve(&root, &pp("a.b")).unwrap_err().to_string();
        assert!(
            err.contains("dotted continuation against string"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_missing_key() {
        let root = dict(&[("a", Value::Integer(1.into()))]);
        let err = resolve(&root, &pp("missing")).unwrap_err().to_string();
        assert!(
            err.contains("missing key `missing`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_missing_index() {
        let root = dict(&[("arr", Value::Array(vec![Value::Integer(1.into())]))]);
        let err = resolve(&root, &pp("arr[5]")).unwrap_err().to_string();
        assert!(
            err.contains("index 5 out of bounds"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn remove_dict_entry() {
        let mut root = dict(&[
            ("a", Value::Integer(1.into())),
            ("b", Value::Integer(2.into())),
        ]);
        let removed = remove(&mut root, &pp("a")).unwrap();
        assert_eq!(removed, Value::Integer(1.into()));
        let Value::Dictionary(d) = &root else {
            panic!("not a dict")
        };
        assert!(d.get("a").is_none());
        assert!(d.get("b").is_some());
    }

    #[test]
    fn remove_array_element() {
        let mut root = dict(&[(
            "arr",
            Value::Array(vec![
                Value::Integer(1.into()),
                Value::Integer(2.into()),
                Value::Integer(3.into()),
            ]),
        )]);
        let removed = remove(&mut root, &pp("arr[1]")).unwrap();
        assert_eq!(removed, Value::Integer(2.into()));
        let v = resolve(&root, &pp("arr")).unwrap();
        let Value::Array(arr) = v else {
            panic!("not an array")
        };
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], Value::Integer(1.into()));
        assert_eq!(arr[1], Value::Integer(3.into()));
    }

    // ---- wildcard expansion (slice 10) ------------------------------------

    fn make_accounts(passwords: &[&str]) -> Value {
        let mut arr = Vec::new();
        for (i, p) in passwords.iter().enumerate() {
            arr.push(dict(&[
                ("name", Value::String(format!("acc{i}"))),
                ("Password", Value::String((*p).into())),
            ]));
        }
        dict(&[("Accounts", Value::Array(arr))])
    }

    #[test]
    fn wildcard_resolves_all_array_elements() {
        let root = dict(&[(
            "Items",
            Value::Array(vec![
                Value::Integer(1.into()),
                Value::Integer(2.into()),
                Value::Integer(3.into()),
            ]),
        )]);
        let paths = resolve_all_paths(&root, &pp("Items[*]")).unwrap();
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0].to_string(), "Items[0]");
        assert_eq!(paths[1].to_string(), "Items[1]");
        assert_eq!(paths[2].to_string(), "Items[2]");
    }

    #[test]
    fn wildcard_chained_with_subkey() {
        let root = make_accounts(&["a", "b", "c"]);
        let paths = resolve_all_paths(&root, &pp("Accounts[*].Password")).unwrap();
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0].to_string(), "Accounts[0].Password");
        assert_eq!(paths[1].to_string(), "Accounts[1].Password");
        assert_eq!(paths[2].to_string(), "Accounts[2].Password");
    }

    #[test]
    fn wildcard_against_non_array_errors() {
        let root = dict(&[("x", Value::Integer(7.into()))]);
        let err = resolve_all_paths(&root, &pp("x[*]"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`[*]` against integer"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn wildcard_in_dict_position_errors() {
        // Defensive: grammar should already keep `[*]` from being a valid
        // dict-key segment (it's bracket-only). But if a path is built
        // by hand with Wildcard against a dict, expansion errors.
        let root = dict(&[("d", dict(&[("a", Value::Integer(1.into()))]))]);
        let path = PlistPath {
            segments: vec![Segment::Key("d".into()), Segment::Wildcard],
        };
        let err = resolve_all_paths(&root, &path).unwrap_err().to_string();
        assert!(
            err.contains("`[*]` against dictionary"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn wildcard_empty_array_returns_no_paths() {
        let root = dict(&[("Items", Value::Array(vec![]))]);
        let paths = resolve_all_paths(&root, &pp("Items[*]")).unwrap();
        assert!(paths.is_empty());
    }

    #[test]
    fn wildcard_in_single_resolve_errors() {
        // resolve / resolve_mut / remove reject `[*]` directly.
        let root = dict(&[("Items", Value::Array(vec![Value::Integer(1.into())]))]);
        let err = resolve(&root, &pp("Items[*]")).unwrap_err().to_string();
        assert!(
            err.contains("`[*]` selector cannot resolve to a single node"),
            "unexpected error: {err}"
        );
    }

    // ---- predicate (slice 10) -------------------------------------------

    #[test]
    fn predicate_matches_one_dict() {
        let root = make_accounts(&["a", "b", "c"]);
        // Match acc1 → index 1.
        let v = resolve(&root, &pp(r#"Accounts[name="acc1"].Password"#)).unwrap();
        assert_eq!(v, &Value::String("b".into()));
        // The expanded path also reports Index(1).
        let paths = resolve_all_paths(&root, &pp(r#"Accounts[name="acc1"].Password"#)).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].to_string(), "Accounts[1].Password");
    }

    #[test]
    fn predicate_zero_match_errors() {
        let root = make_accounts(&["a", "b"]);
        let err = resolve(&root, &pp(r#"Accounts[name="nope"].Password"#))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("matched no elements"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn predicate_multi_match_errors() {
        // Two array entries with the same `name` value.
        let arr = Value::Array(vec![
            dict(&[
                ("name", Value::String("dup".into())),
                ("Password", Value::String("a".into())),
            ]),
            dict(&[
                ("name", Value::String("dup".into())),
                ("Password", Value::String("b".into())),
            ]),
        ]);
        let root = dict(&[("Accounts", arr)]);
        let err = resolve(&root, &pp(r#"Accounts[name="dup"].Password"#))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("matched 2 elements"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn predicate_against_non_array_errors() {
        let root = dict(&[("Accounts", dict(&[("name", Value::String("x".into()))]))]);
        let err = resolve(&root, &pp(r#"Accounts[name="x"].Password"#))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("predicate `[key=...]` against dictionary"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn predicate_against_array_of_non_dicts_errors() {
        // Array of strings: predicate cannot match (no `<key>` children).
        let root = dict(&[(
            "Accounts",
            Value::Array(vec![Value::String("a".into()), Value::String("b".into())]),
        )]);
        let err = resolve(&root, &pp(r#"Accounts[name="a"].Password"#))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("matched no elements"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn predicate_with_non_string_key_value_no_match() {
        // The element has `<key>name</key><integer>42</integer>`, and the
        // predicate is `[name="42"]`. RFC: predicate equality is string-only
        // — non-string `<key>` values silently fail to match.
        let arr = Value::Array(vec![dict(&[
            ("name", Value::Integer(42.into())),
            ("Password", Value::String("x".into())),
        ])]);
        let root = dict(&[("Accounts", arr)]);
        let err = resolve(&root, &pp(r#"Accounts[name="42"].Password"#))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("matched no elements"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn predicate_remove_drops_single_element() {
        let arr = Value::Array(vec![
            dict(&[
                ("name", Value::String("keep".into())),
                ("Password", Value::String("k".into())),
            ]),
            dict(&[
                ("name", Value::String("kill".into())),
                ("Password", Value::String("d".into())),
            ]),
        ]);
        let mut root = dict(&[("Accounts", arr)]);
        let removed = remove(&mut root, &pp(r#"Accounts[name="kill"]"#)).unwrap();
        let Value::Dictionary(d) = removed else {
            panic!("expected dict");
        };
        assert_eq!(d.get("name"), Some(&Value::String("kill".into())));
        // `keep` is the only remaining entry.
        let v = resolve(&root, &pp("Accounts")).unwrap();
        let Value::Array(a) = v else {
            panic!("not array")
        };
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn no_array_segments_returns_one_path() {
        let root = dict(&[("a", dict(&[("b", Value::Integer(1.into()))]))]);
        let paths = resolve_all_paths(&root, &pp("a.b")).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].to_string(), "a.b");
    }
}
