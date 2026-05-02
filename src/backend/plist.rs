//! Plist backend.
//!
//! Implements decode of XML/binary plist and JSON source, the default
//! `merge shallow` (and `merge deep`), and encode of either binary plist
//! (default) or XML plist via `output xml`.
//!
//! Slice 8 adds path-level directives:
//!
//! * `ignore path "X"` — skip merging source[X], leaving live[X] untouched.
//! * `remove path "X"` — delete X from the merged tree.
//! * `add:hide path "X"` — re-add filter: replace live[X] with the literal
//!   string `HIDDEN`.
//! * `add:remove path "X"` — re-add filter: delete live[X].
//!
//! Slice 10 lifts the `[*]` and `[key="value"]` restrictions for the four
//! directives above. `[*]` expands to one match per current array element;
//! `[key="value"]` resolves to the single matching element of an array of
//! dicts. `set path` (slice 12) and `transform path` continue to require a
//! single target. See `docs/src/dev/xml_support_rfc.md`.

use crate::backend::Backend;
use crate::config;
use crate::config::PlistOutput;
use crate::config::PlistTypeTag;
use crate::path::plist::PlistPath;
use crate::path::plist::Segment;
use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use camino::Utf8Path;
use std::io::Read;
use std::io::Write;

mod json;
mod merge;
mod path_resolve;
mod transforms;
mod util;

use util::value_kind;

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod tests;

pub(crate) struct PlistBackend;

impl Backend for PlistBackend {
    fn process(
        &self,
        script: &config::Script,
        script_path: &Utf8Path,
        stdin: &mut dyn Read,
        stdout: &mut dyn Write,
    ) -> anyhow::Result<()> {
        let cfg = config::parse_for_merge(script)
            .with_context(|| format!("Failed to parse {script_path}"))?;

        // 1) Decode the source.
        let mut source_value = decode_source(&cfg, script, script_path)?;

        // 1a) Apply transforms in declaration order. Transforms only affect
        //     the source side; the live tree is left untouched.
        for (path, kind) in &cfg.plist.transforms {
            transforms::apply(&mut source_value, path, kind)
                .with_context(|| format!("`transform path \"{path}\"` (source)"))?;
        }

        // 2) Decode the live plist from stdin.
        let mut live_bytes = Vec::new();
        stdin
            .read_to_end(&mut live_bytes)
            .context("Failed to read live plist from stdin")?;
        let mut live_value = if live_bytes.is_empty() {
            // Treat an empty live (no stdin from chezmoi) as an empty
            // top-level dictionary. This lets `merge shallow` /
            // `merge deep` insert every source key into a fresh file.
            // Non-empty live data that isn't a top-level dict is still
            // rejected by `merge_top_level` below — only the *empty*
            // case is coerced.
            plist::Value::Dictionary(plist::Dictionary::new())
        } else {
            plist::Value::from_reader(std::io::Cursor::new(&live_bytes))
                .context("Failed to decode live plist from stdin")?
        };

        // 3) Validate that every `ignore path` selector resolves in
        //    source. This catches typos: an ignore-path that names no
        //    source node would silently no-op otherwise.
        for path in &cfg.plist.ignore {
            path_resolve::resolve_all_paths(&source_value, path)
                .with_context(|| format!("`ignore path \"{path}\"` (source)"))?;
        }

        // For ignore paths that traverse arrays — or that have more than
        // one dict-key segment — we copy live values into the post-merge
        // tree. The default `merge_shallow` only skip-writes single-
        // segment top-level dict-key ignores, and even `merge_deep` is
        // bypassed under shallow mode for multi-segment paths, so the
        // post-merge restore is required to honour the directive. Under
        // deep merge it is idempotent (the live value at the path was
        // already preserved during recursion). Snapshot the pre-merge
        // live tree now so we can refer to it after merging.
        let live_pre_merge = if cfg.plist.ignore.iter().any(path_needs_post_merge_restore) {
            Some(live_value.clone())
        } else {
            None
        };

        // 4) Apply the default merge, honouring `ignore path` selectors
        //    that name a single dict-key path.
        merge_top_level(
            &mut live_value,
            source_value,
            cfg.merge_mode,
            &cfg.plist.ignore,
        )?;

        // 4a) Restore live values for ignore paths that traversed
        //     arrays. The walk descends through both the pre-merge live
        //     snapshot and the merged tree in parallel: predicate
        //     segments resolve independently in each tree (so the
        //     captured value is keyed by predicate identity, not by
        //     position), and `[*]` segments require both arrays to have
        //     the same length so per-index alignment is meaningful.
        if let Some(live_snapshot) = live_pre_merge {
            for path in &cfg.plist.ignore {
                if !path_needs_post_merge_restore(path) {
                    continue;
                }
                restore_ignore_in_parallel(&live_snapshot, &mut live_value, path, 0)
                    .with_context(|| format!("`ignore path \"{path}\"` (restoring)"))?;
            }
        }

        // 5) Apply `remove path` directives on the merged tree.
        for path in &cfg.plist.remove {
            let concrete = path_resolve::resolve_all_paths(&live_value, path)
                .with_context(|| format!("`remove path \"{path}\"`"))?;
            if concrete.is_empty() {
                bail!("`remove path \"{path}\"`: selector matched no nodes");
            }
            // Sort descending by trailing index so that array removals
            // don't shift earlier targets. (Path-level operations resolve
            // before any patches apply; removing several Items[*] entries
            // by descending index is the simplest way to honour that.)
            let mut concrete = concrete;
            concrete.sort_by(compare_paths_desc);
            for cp in &concrete {
                path_resolve::remove(&mut live_value, cp)
                    .with_context(|| format!("`remove path \"{path}\"` (at {cp})"))?;
            }
        }

        // 5b) Apply `set path` directives on the merged tree.
        //     Replace-only: the path must already resolve to a scalar
        //     value of a settable type. Type tag, when present, must
        //     match the existing scalar's type. `[*]` is rejected at
        //     parse time; predicates are accepted (single-match).
        for (path, literal, type_tag) in &cfg.plist.set_path {
            apply_set_path(&mut live_value, path, literal, *type_tag)
                .with_context(|| format!("`set path \"{path}\"`"))?;
        }

        // 6) Encode based on output format.
        encode(&live_value, cfg.output_format, stdout)?;
        Ok(())
    }

    fn filter(
        &self,
        script: &config::Script,
        _script_path: &Utf8Path,
        live_contents: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        let cfg = config::parse_for_add(script)?;

        // The `output` directive is consulted only for `process`; the re-add
        // path preserves the format of the live file on disk.
        let _ = cfg.output_format;

        // Match `process`'s slice-7 fix: empty / whitespace-only live
        // contents (no live file on disk yet) are coerced to an empty
        // top-level dict. Without this, `Backend::filter` errored where
        // `Backend::process` succeeded, which is inconsistent and makes
        // first-run re-add awkward. XML keeps its strict policy in
        // `XmlBackend::filter` — empty XML genuinely is malformed and
        // there is no analogous "empty" sentinel.
        if live_contents.iter().all(u8::is_ascii_whitespace) {
            // Re-add of an empty live file: nothing to filter; emit an
            // empty XML plist so chezmoi diffs cleanly.
            let mut out = Vec::new();
            plist::Value::Dictionary(plist::Dictionary::new())
                .to_writer_xml(&mut out)
                .context("Failed to encode empty plist for re-add")?;
            return Ok(out);
        }
        let was_xml = looks_like_xml_plist(live_contents);
        let mut value = plist::Value::from_reader(std::io::Cursor::new(live_contents))
            .context("Failed to decode live plist for re-add")?;

        // Apply `add:hide path` directives. The RFC restricts hide to
        // `<string>` and `<data>` values; reject other types explicitly.
        // For `[*]` selectors, every matched element must satisfy the
        // type constraint; one bad element fails the whole directive
        // before any element is mutated.
        for path in &cfg.plist.add_hide {
            let concrete = path_resolve::resolve_all_paths(&value, path)
                .with_context(|| format!("`add:hide path \"{path}\"` (live)"))?;
            if concrete.is_empty() {
                bail!("`add:hide path \"{path}\"` (live): selector matched no nodes");
            }
            // Pre-flight check: every concrete target must already be a
            // string or data value.
            for cp in &concrete {
                let target = path_resolve::resolve(&value, cp)
                    .with_context(|| format!("`add:hide path \"{path}\"` (live, at {cp})"))?;
                if !matches!(target, plist::Value::String(_) | plist::Value::Data(_)) {
                    return Err(anyhow!(
                        "`add:hide path \"{path}\"` is only valid on `<string>` or `<data>` \
                         values; got {kind} at {cp}",
                        kind = value_kind(target)
                    ));
                }
            }
            for cp in &concrete {
                let target = path_resolve::resolve_mut(&mut value, cp)
                    .with_context(|| format!("`add:hide path \"{path}\"` (live, at {cp})"))?;
                *target = plist::Value::String("HIDDEN".to_string());
            }
        }

        // Apply `add:remove path` directives. For `[*]`, removing array
        // elements is done back-to-front so earlier indices are still
        // valid as we go.
        for path in &cfg.plist.add_remove {
            let mut concrete = path_resolve::resolve_all_paths(&value, path)
                .with_context(|| format!("`add:remove path \"{path}\"` (live)"))?;
            if concrete.is_empty() {
                bail!("`add:remove path \"{path}\"` (live): selector matched no nodes");
            }
            concrete.sort_by(compare_paths_desc);
            for cp in &concrete {
                path_resolve::remove(&mut value, cp)
                    .with_context(|| format!("`add:remove path \"{path}\"` (live, at {cp})"))?;
            }
        }

        // Apply `ignore path` directives. The RFC's "Re-add filtering
        // algorithm" mandates removing the addressed span so that values
        // intentionally kept on the live side (e.g. a Password) do not
        // leak through `chezmoi re-add` into the source-controlled tree.
        // Missing nodes are tolerated here: an `ignore path` referring to
        // an entry that simply does not exist in this particular live
        // file is a no-op rather than an error — a reasonable filter
        // declaration may address keys that this machine hasn't written
        // yet. *Other* resolution errors (ambiguous predicate, shape
        // mismatch) propagate; otherwise a typo'd directive could leave a
        // secret in place because the filter silently bailed before
        // removing it.
        for path in &cfg.plist.ignore {
            let mut concrete = match path_resolve::resolve_all_paths(&value, path) {
                Ok(c) => c,
                Err(e) if path_resolve::is_not_found(&e) => continue,
                Err(e) => {
                    return Err(e.context(format!("`ignore path \"{path}\"` (live, re-add)")));
                }
            };
            concrete.sort_by(compare_paths_desc);
            for cp in &concrete {
                path_resolve::remove(&mut value, cp)
                    .with_context(|| format!("`ignore path \"{path}\"` (live, re-add, at {cp})"))?;
            }
        }

        let mut out = Vec::new();
        if was_xml {
            value
                .to_writer_xml(&mut out)
                .context("Failed to encode live plist as XML for re-add")?;
        } else {
            value
                .to_writer_binary(&mut out)
                .context("Failed to encode live plist as binary for re-add")?;
        }
        Ok(out)
    }
}

/// Decode the source for a plist merge, from either an inline body or a
/// sidecar file.
fn decode_source(
    cfg: &config::Config<ini_merge::mutations::Mutations>,
    script: &config::Script,
    script_path: &Utf8Path,
) -> anyhow::Result<plist::Value> {
    if script.is_inline() {
        let body = script.body.as_deref().unwrap_or(&[]);
        let format = config::detect_inline_format(cfg.language, body, script_path)?;
        match format {
            config::InlineFormat::Json => decode_json_bytes(body),
            config::InlineFormat::Xml => plist::Value::from_reader(std::io::Cursor::new(body))
                .context("Failed to decode inline XML plist body"),
            config::InlineFormat::Ini => Err(anyhow!(
                "{script_path}: language plist: inline body must be JSON or XML, got INI"
            )),
        }
    } else {
        let src_path = cfg
            .source_path(script_path)
            .context("Failed to get source path")?;
        let bytes = std::fs::read(src_path.as_std_path())
            .with_context(|| format!("Failed to open source file at: {src_path}"))?;
        // Pick decoder based on the resolved extension.
        let is_json = src_path
            .as_str()
            .to_ascii_lowercase()
            .ends_with(".src.json");
        if is_json {
            decode_json_bytes(&bytes)
        } else {
            plist::Value::from_reader(std::io::Cursor::new(&bytes))
                .with_context(|| format!("Failed to decode plist source at: {src_path}"))
        }
    }
}

fn decode_json_bytes(bytes: &[u8]) -> anyhow::Result<plist::Value> {
    // Strip a leading UTF-8 BOM (`EF BB BF`) before decoding. Round-2
    // added BOM-tolerance to `detect_inline_format`, but the format
    // detector doesn't reshape the body it returns — without the strip
    // here, a BOM-prefixed inline JSON body still failed at
    // `serde_json::from_slice` with a confusing "expected value at
    // line 1 column 1". Mirror the same strip on the sidecar `.src.json`
    // path; macOS tooling and some editors add a BOM that should never
    // reach the JSON parser.
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let v: serde_json::Value =
        serde_json::from_slice(bytes).context("Failed to decode JSON source as JSON")?;
    json::json_to_plist(&v)
}

/// Apply the configured top-level merge. The source must be a top-level
/// dictionary (the operation is dictionary-keyed).
fn merge_top_level(
    live: &mut plist::Value,
    source: plist::Value,
    mode: config::MergeMode,
    ignore_paths: &[PlistPath],
) -> anyhow::Result<()> {
    let source_dict = match source {
        plist::Value::Dictionary(d) => d,
        other => {
            return Err(anyhow!(
                "plist source must be a top-level dictionary; got {kind}",
                kind = value_kind(&other)
            ));
        }
    };
    let live_dict = match live {
        plist::Value::Dictionary(d) => d,
        other => {
            return Err(anyhow!(
                "live plist must be a top-level dictionary; got {kind}",
                kind = value_kind(other)
            ));
        }
    };
    match mode {
        config::MergeMode::Shallow => {
            merge::merge_shallow(live_dict, &source_dict, ignore_paths);
            Ok(())
        }
        config::MergeMode::Deep => merge::merge_deep(live_dict, &source_dict, ignore_paths),
    }
}

/// `true` if `path` requires a post-merge restoration pass.
///
/// The legacy slice-8 in-merge skip-write only covers single-segment
/// top-level dict-key ignores (and only under `merge_shallow`). For
/// every other case — array-traversing paths, or multi-segment dict-key
/// paths under `merge_shallow` — the merge step has already overwritten
/// live's value with source's, so we replay the pre-merge live snapshot
/// onto the merged tree. Under `merge_deep`, multi-segment dict-key
/// paths are also honoured during recursion (`is_ignored` checks at
/// each depth), so the restoration is idempotent there.
fn path_needs_post_merge_restore(path: &PlistPath) -> bool {
    let has_array = path.segments.iter().any(|s| {
        matches!(
            s,
            Segment::Index(_) | Segment::Wildcard | Segment::Predicate { .. }
        )
    });
    // Multi-segment dict-key paths are silently no-op'd by
    // `merge_shallow`'s top-level `is_ignored` check, so restore is
    // needed for any path of length > 1 — not just array-traversing
    // ones.
    has_array || path.segments.len() > 1
}

/// Walk `path` through `live` and `merged` in parallel and copy the live
/// value at each leaf into the merged tree.
///
/// The two trees may have arrays of different shape: at a predicate
/// segment we resolve the matching dict identity in each tree
/// independently, so a `[name="main"]` predicate finds `main` wherever
/// it lives in each side. `[*]` segments demand matching array lengths
/// (we have no way to align scalars otherwise) and zip element-wise. A
/// missing intermediate node on the live side is treated as a no-op for
/// that branch — there is nothing to restore.
fn restore_ignore_in_parallel(
    live: &plist::Value,
    merged: &mut plist::Value,
    path: &PlistPath,
    idx: usize,
) -> anyhow::Result<()> {
    if idx == path.segments.len() {
        *merged = live.clone();
        return Ok(());
    }
    let seg = &path.segments[idx];
    match seg {
        Segment::Key(name) => {
            // If the merged tree doesn't have this key (e.g. a `remove
            // path` ran later, or merging dropped it), nothing to do.
            let live_val = match live {
                plist::Value::Dictionary(d) => d.get(name),
                _ => None,
            };
            let Some(live_val) = live_val else {
                return Ok(());
            };
            // If the merged tree's shape at this segment differs from
            // the path (e.g. live has a dict but source produced an
            // array, so the merged tree mirrors source), there is
            // nothing to restore — degrade to a no-op rather than
            // erroring. The directive's intent ("don't take from
            // source") is moot when source's shape doesn't even
            // resemble the addressed path.
            let plist::Value::Dictionary(merged_dict) = merged else {
                return Ok(());
            };
            let Some(merged_val) = merged_dict.get_mut(name) else {
                return Ok(());
            };
            restore_ignore_in_parallel(live_val, merged_val, path, idx + 1)
        }
        Segment::Index(n) => {
            let live_val = match live {
                plist::Value::Array(a) => a.get(*n),
                _ => None,
            };
            let Some(live_val) = live_val else {
                return Ok(());
            };
            // Same rationale as the dict-key arm above: shape mismatch
            // between live and merged at this segment is a no-op.
            let plist::Value::Array(merged_arr) = merged else {
                return Ok(());
            };
            let Some(merged_val) = merged_arr.get_mut(*n) else {
                return Ok(());
            };
            restore_ignore_in_parallel(live_val, merged_val, path, idx + 1)
        }
        Segment::Predicate { key, value } => {
            // Live side is the source of truth for "what to restore".
            // A predicate that fails to identify a unique element on
            // the live side is a hard error: the directive's intent
            // ("don't take from source — keep live's value") cannot be
            // honoured when there is no unique live value to restore.
            // If the live tree's shape is fundamentally incompatible
            // (not an array at all), degrade to a no-op — there is
            // nothing to restore from a non-array, and the merged tree
            // already mirrors source.
            if !matches!(live, plist::Value::Array(_)) {
                return Ok(());
            }
            let live_idx = match predicate_match(live, key, value) {
                PredicateMatch::Unique(i) => i,
                PredicateMatch::None => bail!(
                    "ignore path predicate `[{key}=\"{value}\"]` matched zero elements in live; \
                     no value to restore"
                ),
                PredicateMatch::Many(n) => bail!(
                    "ignore path predicate `[{key}=\"{value}\"]` matched {n} elements in live; \
                     ambiguous (predicate must identify exactly one element)"
                ),
            };
            // The merged side may legitimately drop the dict (e.g. a
            // later `remove path` removed it, or source didn't carry
            // it). Treat absence in merged as a no-op.
            let PredicateMatch::Unique(merged_idx) = predicate_match(merged, key, value) else {
                return Ok(());
            };
            let plist::Value::Array(la) = live else {
                unreachable!("checked above");
            };
            let plist::Value::Array(ma) = merged else {
                return Ok(());
            };
            restore_ignore_in_parallel(&la[live_idx], &mut ma[merged_idx], path, idx + 1)
        }
        Segment::Wildcard => {
            let plist::Value::Array(live_arr) = live else {
                return Ok(());
            };
            // Shape mismatch (merged is not an array): no-op. Wildcard
            // alignment cannot be defined; the directive's intent is
            // moot because source produced a non-array at this path.
            let plist::Value::Array(merged_arr) = merged else {
                return Ok(());
            };
            if live_arr.len() != merged_arr.len() {
                bail!(
                    "ignore path: live and merged arrays at `[*]` differ in length ({live} vs \
                     {merged}); cannot align wildcard ignore",
                    live = live_arr.len(),
                    merged = merged_arr.len(),
                );
            }
            for (le, me) in live_arr.iter().zip(merged_arr.iter_mut()) {
                restore_ignore_in_parallel(le, me, path, idx + 1)?;
            }
            Ok(())
        }
    }
}

/// Result of a predicate scan against a candidate array.
enum PredicateMatch {
    /// Not an array, or zero matches.
    None,
    /// Exactly one element matched.
    Unique(usize),
    /// Two or more elements matched (count is the total).
    Many(usize),
}

/// Scan `node` for array elements that are dicts whose `key` value
/// equals `value` (string compare). The caller distinguishes zero,
/// one and many — `restore_ignore_in_parallel` uses zero/many to
/// produce explicit errors on the live side rather than silently
/// leaving merged (i.e. source) values in place.
fn predicate_match(node: &plist::Value, key: &str, value: &str) -> PredicateMatch {
    let plist::Value::Array(arr) = node else {
        return PredicateMatch::None;
    };
    let mut hits: Vec<usize> = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        if let plist::Value::Dictionary(d) = item
            && let Some(plist::Value::String(s)) = d.get(key)
            && s == value
        {
            hits.push(i);
        }
    }
    match hits.len() {
        0 => PredicateMatch::None,
        1 => PredicateMatch::Unique(hits[0]),
        n => PredicateMatch::Many(n),
    }
}

/// Apply a single `set path` directive (slice 12). The path must
/// already resolve to a scalar value; this is replace-only. If
/// `type_tag` is supplied, the existing value's type must match.
fn apply_set_path(
    root: &mut plist::Value,
    path: &PlistPath,
    literal: &str,
    type_tag: Option<PlistTypeTag>,
) -> anyhow::Result<()> {
    let target = path_resolve::resolve_mut(root, path)?;

    // Determine the target type. If a tag was supplied it must match
    // the existing scalar's type. If omitted, we infer from the
    // existing value.
    let actual_kind = value_kind(target);
    let inferred = match target {
        plist::Value::String(_) => Some(PlistTypeTag::String),
        plist::Value::Integer(_) => Some(PlistTypeTag::Integer),
        plist::Value::Real(_) => Some(PlistTypeTag::Real),
        plist::Value::Data(_) => Some(PlistTypeTag::Data),
        plist::Value::Date(_) => Some(PlistTypeTag::Date),
        _ => None,
    };
    let effective = match (type_tag, inferred) {
        (Some(tag), Some(have)) if tag == have => tag,
        (Some(tag), Some(have)) => {
            return Err(anyhow!(
                "type mismatch: declared `{decl}` but existing scalar is `{have}`",
                decl = tag.name(),
                have = have.name(),
            ));
        }
        (Some(_) | None, None) => {
            return Err(anyhow!(
                "set path on non-scalar value not supported: {actual_kind} (slice 12 supports \
                 only string/integer/real/data/date)"
            ));
        }
        (None, Some(have)) => have,
    };

    let new_value = match effective {
        PlistTypeTag::String => plist::Value::String(literal.to_string()),
        PlistTypeTag::Integer => {
            // `plist::Integer` natively spans the full i64..=u64 range; the
            // JSON-import path uses `as_unsigned()` to round-trip values
            // above i64::MAX. Mirror that here so a literal in
            // (i64::MAX, u64::MAX] survives without losing data — try
            // signed first (the common case), then unsigned.
            let int = if let Ok(n) = literal.parse::<i64>() {
                plist::Integer::from(n)
            } else if let Ok(n) = literal.parse::<u64>() {
                plist::Integer::from(n)
            } else {
                return Err(anyhow!(
                    "invalid integer literal `{literal}`: out of i64/u64 range"
                ));
            };
            plist::Value::Integer(int)
        }
        PlistTypeTag::Real => {
            let parsed: f64 = literal
                .parse()
                .map_err(|e| anyhow!("invalid real literal `{literal}`: {e}"))?;
            plist::Value::Real(parsed)
        }
        PlistTypeTag::Data => {
            // Decode the base64 literal directly. The previous round-
            // tripping through `<plist><data>{literal}</data></plist>`
            // silently corrupted any literal containing XML-special
            // characters (`<`, `&`, `]]>` etc.) — they were either
            // escaped, ignored, or rejected by the XML parser depending
            // on placement. Use the `base64` crate so an invalid
            // literal errors cleanly with an actionable diagnostic.
            //
            // The plist `<data>` reader strips ASCII whitespace before
            // decoding (per Apple convention); mirror that with the
            // `Indifferent` general-purpose decoder so multi-line
            // base64 literals continue to work.
            use base64::Engine as _;
            // `STANDARD` accepts the standard alphabet; we strip ASCII
            // whitespace to match the plist `<data>` reader's
            // permissive behaviour.
            let cleaned: String = literal
                .chars()
                .filter(|c| !c.is_ascii_whitespace())
                .collect();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(cleaned.as_bytes())
                .map_err(|e| anyhow!("invalid base64 data `{literal}`: {e}"))?;
            plist::Value::Data(bytes)
        }
        PlistTypeTag::Date => {
            let date = plist::Date::from_xml_format(literal)
                .map_err(|e| anyhow!("invalid ISO-8601 date `{literal}`: {e}"))?;
            plist::Value::Date(date)
        }
    };
    *target = new_value;
    Ok(())
}

/// Compare two concrete paths so that "later" array indices sort first.
/// Used to remove array elements back-to-front.
fn compare_paths_desc(a: &PlistPath, b: &PlistPath) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // Find the first differing segment.
    for (sa, sb) in a.segments.iter().zip(b.segments.iter()) {
        match (sa, sb) {
            (Segment::Index(ia), Segment::Index(ib)) => match ib.cmp(ia) {
                Ordering::Equal => continue,
                ord => return ord,
            },
            // Non-Index segments compare equal so we don't reorder
            // structurally distinct paths against each other.
            _ => continue,
        }
    }
    // Longer paths first so deepest arrays are removed before their
    // ancestors.
    b.segments.len().cmp(&a.segments.len())
}

fn encode(value: &plist::Value, format: PlistOutput, w: &mut dyn Write) -> anyhow::Result<()> {
    match format {
        PlistOutput::Binary => {
            value
                .to_writer_binary(w)
                .context("Failed to encode plist as binary")?;
        }
        PlistOutput::Xml => {
            value
                .to_writer_xml(w)
                .context("Failed to encode plist as XML")?;
        }
    }
    Ok(())
}

/// Heuristic: does `bytes` look like an XML plist? Used to preserve
/// on-disk format on the re-add path.
///
/// Strips a leading UTF-8 BOM (`EF BB BF`) before checking — some
/// editors and macOS tooling add it, and without the strip the file
/// would be misclassified as binary and re-encoded on round-trip.
fn looks_like_xml_plist(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let trimmed = bytes.trim_ascii_start();
    trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<plist")
}
