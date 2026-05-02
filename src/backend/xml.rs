//! XML backend.
//!
//! Implements byte-span patching of XML documents per the RFC. The
//! backend tokenises the source and live (system) files, builds an
//! element index, and resolves directive paths into byte spans that the
//! patcher rewrites.
//!
//! Supported in this slice:
//!
//! * `ignore path "X"` — preserve the system value for an attribute or
//!   element. The selector must match exactly one node in both source
//!   and system.
//! * `remove path "X"` — delete the selected attribute, element, or
//!   text region from the source span.
//! * `add:hide path "X"` — at re-add time, replace the addressed
//!   attribute value (or element text content) with the literal string
//!   `HIDDEN`.
//! * `add:remove path "X"` — at re-add time, delete the addressed
//!   attribute or element span.

use crate::backend::Backend;
use crate::config;
use crate::path::xml::Target;
use crate::path::xml::XmlPath;
use anyhow::Context;
use anyhow::anyhow;
use camino::Utf8Path;
use std::io::Read;
use std::io::Write;

mod index;
mod patch;
mod resolve;
mod tokens;

#[cfg(test)]
mod proptests;

use index::ElementIndex;
use patch::Patch;
use patch::apply_patches;
use resolve::ResolvedTarget;
use resolve::is_not_found;
use resolve::resolve;

pub(crate) struct XmlBackend;

impl Backend for XmlBackend {
    fn process(
        &self,
        script: &config::Script,
        script_path: &Utf8Path,
        stdin: &mut dyn Read,
        stdout: &mut dyn Write,
    ) -> anyhow::Result<()> {
        let cfg = config::parse_for_merge(script)
            .with_context(|| format!("Failed to parse {script_path}"))?;

        // Read the source bytes (sidecar `.src.xml` or inline body).
        let source_bytes = read_source(&cfg, script, script_path)?;
        let source_str =
            std::str::from_utf8(&source_bytes).context("XML source file is not valid UTF-8")?;

        // Read the live/system file from stdin.
        let mut live_bytes = Vec::new();
        stdin
            .read_to_end(&mut live_bytes)
            .context("Failed to read live XML from stdin")?;
        let live_str =
            std::str::from_utf8(&live_bytes).context("Live XML (stdin) is not valid UTF-8")?;

        // Tokenise and index both documents.
        let source_tokens =
            tokens::tokenize(source_str).context("Failed to tokenise XML source")?;
        let (source_elems, source_root) =
            index::build(&source_tokens).context("Failed to build XML index for source")?;

        let live_tokens = tokens::tokenize(live_str).context("Failed to tokenise XML live file")?;
        let (live_elems, live_root) =
            index::build(&live_tokens).context("Failed to build XML index for live file")?;

        let mut patches: Vec<Patch> = Vec::new();

        // `ignore path` — replace the addressed source span with the
        // corresponding live bytes.
        for path in &cfg.xml.ignore {
            let source_target = resolve(path, &source_elems, source_root, source_str)
                .with_context(|| format!("`ignore path \"{path}\"` (source)"))?;
            let live_target = resolve(path, &live_elems, live_root, live_str)
                .with_context(|| format!("`ignore path \"{path}\"` (live)"))?;
            let patch = patch_for_ignore(path, &source_target, &live_target, live_str)?;
            patches.push(patch);
        }

        // `remove path` — replace the addressed source span with empty.
        for path in &cfg.xml.remove {
            let source_target = resolve(path, &source_elems, source_root, source_str)
                .with_context(|| format!("`remove path \"{path}\"` (source)"))?;
            let patch = patch_for_remove(&source_elems, source_str, source_target);
            patches.push(patch);
        }

        // `set path` — replace an attribute value or `text()` content
        // span with the XML-escaped literal. Element targets are
        // rejected (slice 12 is replace-only and scalar-only).
        for (path, literal) in &cfg.xml.set_path {
            let source_target = resolve(path, &source_elems, source_root, source_str)
                .with_context(|| format!("`set path \"{path}\"` (source)"))?;
            let patch = patch_for_set(path, &source_target, literal)?;
            patches.push(patch);
        }

        let merged = apply_patches(&source_bytes, patches)?;
        stdout
            .write_all(&merged)
            .context("Failed to write merged XML to stdout")?;
        Ok(())
    }

    fn filter(
        &self,
        script: &config::Script,
        _script_path: &Utf8Path,
        live_contents: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        let cfg = config::parse_for_add(script)?;

        let live_str = std::str::from_utf8(live_contents).context("Live XML is not valid UTF-8")?;
        let tokens = tokens::tokenize(live_str).context("Failed to tokenise XML live file")?;
        let (elems, root) =
            index::build(&tokens).context("Failed to build XML index for live file")?;

        let mut patches: Vec<Patch> = Vec::new();

        // `add:hide path` — replace attribute value or element text with HIDDEN.
        for path in &cfg.xml.add_hide {
            let target = resolve(path, &elems, root, live_str)
                .with_context(|| format!("`add:hide path \"{path}\"` (live)"))?;
            let patch = patch_for_add_hide(path, target)?;
            patches.push(patch);
        }

        // `add:remove path` — delete the addressed span (with adjacent
        // whitespace for attribute targets).
        for path in &cfg.xml.add_remove {
            let target = resolve(path, &elems, root, live_str)
                .with_context(|| format!("`add:remove path \"{path}\"` (live)"))?;
            let patch = patch_for_remove(&elems, live_str, target);
            patches.push(patch);
        }

        // `ignore path` — per the RFC's "Re-add filtering algorithm",
        // remove the addressed span at re-add time so that values
        // intentionally pinned to the live side (typically secrets) do
        // not leak through `chezmoi re-add` into the source tree. Treat
        // selectors that *cleanly* fail to resolve in this particular
        // live file as a no-op (an ignore directive may legitimately
        // address a key that hasn't been written yet on this machine).
        // Real failures — ambiguous selector, mixed-content shape
        // mismatch, etc. — must propagate; otherwise a directive that
        // looks correct but matches multiple elements would silently let
        // every secret through `chezmoi re-add`.
        for path in &cfg.xml.ignore {
            match resolve(path, &elems, root, live_str) {
                Ok(target) => {
                    let patch = patch_for_remove(&elems, live_str, target);
                    patches.push(patch);
                }
                Err(e) if is_not_found(&e) => {
                    // No-op: nothing to redact in this live file.
                }
                Err(e) => {
                    return Err(e.context(format!("`ignore path \"{path}\"` (live, re-add)")));
                }
            }
        }

        apply_patches(live_contents, patches)
    }
}

/// Read the source bytes for a merge from either an inline body or a
/// sidecar `.src.xml` file.
fn read_source(
    cfg: &config::Config<ini_merge::mutations::Mutations>,
    script: &config::Script,
    script_path: &Utf8Path,
) -> anyhow::Result<Vec<u8>> {
    if script.is_inline() {
        // Validate the inline format; XML language requires an XML body.
        let body = script.body.as_deref().unwrap_or(&[]);
        let format = config::detect_inline_format(cfg.language, body, script_path)?;
        match format {
            config::InlineFormat::Xml => Ok(body.to_vec()),
            other => Err(anyhow!(
                "{script_path}: language xml: inline body must be XML, got {other:?}"
            )),
        }
    } else {
        let src_path = cfg
            .source_path(script_path)
            .context("Failed to resolve source path")?;
        std::fs::read(src_path.as_std_path())
            .with_context(|| format!("Failed to open XML source at: {src_path}"))
    }
}

/// Build the patch for an `ignore path` directive.
///
/// For an attribute target, the patch span is the *value* of the source
/// attribute (sans quotes) and the replacement is the live attribute
/// value (sans quotes). Source quote style is preserved.
///
/// For an element target, the entire source element span is replaced
/// with the entire live element span.
///
/// For a text target, the source text region is replaced with the live
/// text region.
fn patch_for_ignore(
    path: &XmlPath,
    source_target: &ResolvedTarget,
    live_target: &ResolvedTarget,
    live_str: &str,
) -> anyhow::Result<Patch> {
    match (&path.target, source_target, live_target) {
        (
            Target::Attribute(_),
            ResolvedTarget::Attribute {
                value_span: source_span,
                ..
            },
            ResolvedTarget::Attribute {
                value_span: live_span,
                ..
            },
        ) => Ok(Patch {
            span: source_span.clone(),
            replacement: live_str[live_span.clone()].as_bytes().to_vec(),
        }),
        (
            Target::Element,
            ResolvedTarget::Element {
                span: source_span, ..
            },
            ResolvedTarget::Element {
                span: live_span, ..
            },
        ) => Ok(Patch {
            span: source_span.clone(),
            replacement: live_str[live_span.clone()].as_bytes().to_vec(),
        }),
        (
            Target::Text,
            ResolvedTarget::Text {
                span: source_span, ..
            },
            ResolvedTarget::Text {
                span: live_span, ..
            },
        ) => Ok(Patch {
            span: source_span.clone(),
            replacement: live_str[live_span.clone()].as_bytes().to_vec(),
        }),
        _ => Err(anyhow!(
            "`ignore path \"{path}\"`: source and live target shapes do not match"
        )),
    }
}

/// Build the patch for a `remove`-style directive (`remove path`,
/// `add:remove path`).
///
/// For attributes, also consume the leading whitespace before the
/// attribute so the surrounding tag does not end up with a double space
/// or a stray run of spaces. This is best-effort per RFC §"Re-add
/// filtering algorithm" — an awkward edit may leave one extra space.
fn patch_for_remove(elements: &[ElementIndex], source: &str, target: ResolvedTarget) -> Patch {
    match target {
        ResolvedTarget::Element { span, .. } => Patch {
            span,
            replacement: Vec::new(),
        },
        ResolvedTarget::Text { span, .. } => Patch {
            span,
            replacement: Vec::new(),
        },
        ResolvedTarget::Attribute {
            elem_idx,
            full_attr_span,
            ..
        } => {
            // Sweep leading whitespace inside the start tag back to the
            // previous non-whitespace byte. Don't sweep past the element
            // name (i.e. don't eat the space after `<elem`).
            let element = &elements[elem_idx];
            // The attribute's name span starts at full_attr_span.start;
            // we look backward from there.
            let bytes = source.as_bytes();
            // Lower bound: just past the element name end.
            let lower_bound = element.name_span.end;
            let mut start = full_attr_span.start;
            while start > lower_bound && matches!(bytes[start - 1], b' ' | b'\t' | b'\n' | b'\r') {
                start -= 1;
            }
            // Invariant: the loop stops at `lower_bound = name_span.end`,
            // so the swept range never crosses into the element name and
            // the surviving prefix `<name` stays intact.
            Patch {
                span: start..full_attr_span.end,
                replacement: Vec::new(),
            }
        }
    }
}

/// Build the patch for a `set path` directive: replace the attribute
/// value (sans quotes) or `text()` span with the XML-escaped literal.
/// Element targets are rejected — slice 12 is replace-only and only
/// targets scalar values. Likewise, `set path` against a `text()` span
/// that lives inside a `<![CDATA[...]]>` block is refused: replacing the
/// inner content would corrupt the document by leaving stale `<![CDATA[`
/// and `]]>` brackets around the new value.
fn patch_for_set(path: &XmlPath, target: &ResolvedTarget, literal: &str) -> anyhow::Result<Patch> {
    match target {
        ResolvedTarget::Attribute { value_span, .. } => Ok(Patch {
            span: value_span.clone(),
            replacement: xml_escape_attribute(literal).into_bytes(),
        }),
        ResolvedTarget::Text { is_cdata: true, .. } => Err(anyhow!(
            "`set path \"{path}\"` text() inside CDATA is not supported"
        )),
        ResolvedTarget::Text { span, .. } => Ok(Patch {
            span: span.clone(),
            replacement: xml_escape_text(literal).into_bytes(),
        }),
        ResolvedTarget::Element { .. } => Err(anyhow!(
            "`set path \"{path}\"` is only valid on an attribute or text() target; \
             setting an element value is not supported"
        )),
    }
}

/// Escape `&`, `<`, `>`, and `"` in an attribute value. Single quotes
/// are left as-is because attributes use double quotes throughout the
/// XML backend's emitted patches.
fn xml_escape_attribute(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape `&`, `<`, and `>` in element text content. Quotes do not
/// require escaping in `#PCDATA` but `>` is escaped defensively to
/// avoid accidental `]]>` sequences.
fn xml_escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Build the patch for an `add:hide path` directive: replace the
/// attribute value (sans quotes) or element text region with `HIDDEN`.
/// On an element target without text, this errors.
fn patch_for_add_hide(path: &XmlPath, target: ResolvedTarget) -> anyhow::Result<Patch> {
    match target {
        ResolvedTarget::Attribute { value_span, .. } => Ok(Patch {
            span: value_span,
            replacement: b"HIDDEN".to_vec(),
        }),
        ResolvedTarget::Text { span, .. } => Ok(Patch {
            span,
            replacement: b"HIDDEN".to_vec(),
        }),
        ResolvedTarget::Element { .. } => Err(anyhow!(
            "`add:hide path \"{path}\"` is only valid on an attribute or text() target"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    fn run_process(script_text: &str, sys_xml: &str, src_xml: Option<&str>) -> Vec<u8> {
        // Use a temp dir so that source-file lookup works.
        let tmp = tempfile::tempdir().unwrap();
        let dir: camino::Utf8PathBuf = tmp.path().to_path_buf().try_into().unwrap();
        let modify_path = dir.join("modify_test.xml");
        std::fs::write(&modify_path, script_text).unwrap();
        if let Some(src) = src_xml {
            std::fs::write(dir.join("test.xml.src.xml"), src).unwrap();
        }
        let raw = std::fs::read(&modify_path).unwrap();
        let script = config::Script::parse(&raw, &modify_path).unwrap();

        let backend = XmlBackend;
        let mut stdin = std::io::Cursor::new(sys_xml.as_bytes().to_vec());
        let mut stdout: Vec<u8> = Vec::new();
        backend
            .process(&script, &modify_path, &mut stdin, &mut stdout)
            .unwrap();
        stdout
    }

    fn run_filter(script_text: &str, live_xml: &str) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        let dir: camino::Utf8PathBuf = tmp.path().to_path_buf().try_into().unwrap();
        let modify_path = dir.join("modify_test.xml");
        std::fs::write(&modify_path, script_text).unwrap();
        let raw = std::fs::read(&modify_path).unwrap();
        let script = config::Script::parse(&raw, &modify_path).unwrap();

        let backend = XmlBackend;
        backend
            .filter(&script, &modify_path, live_xml.as_bytes())
            .unwrap()
    }

    #[test]
    fn process_ignore_attribute_keeps_system_value() {
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            ignore path "/config/window/@width"
        "#};
        let sys = r#"<config><window width="1024" height="600"/></config>"#;
        let src = r#"<config><window width="800" height="900"/></config>"#;
        let out = run_process(script, sys, Some(src));
        let s = String::from_utf8(out).unwrap();
        // width comes from system, but rest of source is preserved
        assert_eq!(s, r#"<config><window width="1024" height="900"/></config>"#);
    }

    #[test]
    fn process_remove_attribute() {
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            remove path "/config/window/@height"
        "#};
        let sys = r#"<config><window width="800" height="600"/></config>"#;
        let src = r#"<config><window width="800" height="600"/></config>"#;
        let out = run_process(script, sys, Some(src));
        let s = String::from_utf8(out).unwrap();
        // height is gone, leading whitespace consumed
        assert_eq!(s, r#"<config><window width="800"/></config>"#);
    }

    #[test]
    fn process_remove_element() {
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            remove path "/config/window"
        "#};
        let sys = r#"<config><window width="800"/><other/></config>"#;
        let src = r#"<config><window width="800"/><other/></config>"#;
        let out = run_process(script, sys, Some(src));
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, r#"<config><other/></config>"#);
    }

    #[test]
    fn filter_add_hide_attribute() {
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            add:hide path "/config/auth/@token"
        "#};
        let live = r#"<config><auth user="bob" token="s3cret"/></config>"#;
        let out = run_filter(script, live);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, r#"<config><auth user="bob" token="HIDDEN"/></config>"#);
    }

    #[test]
    fn filter_add_remove_attribute() {
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            add:remove path "/config/auth/@token"
        "#};
        let live = r#"<config><auth user="bob" token="s3cret"/></config>"#;
        let out = run_filter(script, live);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, r#"<config><auth user="bob"/></config>"#);
    }

    #[test]
    fn filter_add_remove_element() {
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            add:remove path "/config/auth"
        "#};
        let live = r#"<config><auth token="s"/><other/></config>"#;
        let out = run_filter(script, live);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, r#"<config><other/></config>"#);
    }

    #[test]
    fn process_preserves_formatting_outside_edited_spans() {
        // Whitespace, comments, attribute order outside edited spans
        // are preserved byte-for-byte.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            ignore path "/root/leaf/@v"
        "#};
        let sys = "<root>\n  <leaf v=\"NEW\"/>\n</root>\n";
        let src = "<root>\n  <!-- comment -->\n  <leaf v=\"OLD\"/>\n</root>\n";
        let out = run_process(script, sys, Some(src));
        let s = String::from_utf8(out).unwrap();
        // Only the value `OLD` -> `NEW` is rewritten.
        assert_eq!(
            s,
            "<root>\n  <!-- comment -->\n  <leaf v=\"NEW\"/>\n</root>\n"
        );
    }

    #[test]
    fn process_ambiguous_selector_errors() {
        // Two `<row>` siblings, no predicate — selector is ambiguous.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            ignore path "/table/row/@v"
        "#};
        let sys = r#"<table><row v="1"/><row v="2"/></table>"#;
        let src = r#"<table><row v="A"/><row v="B"/></table>"#;
        let tmp = tempfile::tempdir().unwrap();
        let dir: camino::Utf8PathBuf = tmp.path().to_path_buf().try_into().unwrap();
        let modify_path = dir.join("modify_test.xml");
        std::fs::write(&modify_path, script).unwrap();
        std::fs::write(dir.join("test.xml.src.xml"), src).unwrap();
        let raw = std::fs::read(&modify_path).unwrap();
        let parsed = config::Script::parse(&raw, &modify_path).unwrap();

        let backend = XmlBackend;
        let mut stdin = std::io::Cursor::new(sys.as_bytes().to_vec());
        let mut stdout: Vec<u8> = Vec::new();
        let err = backend
            .process(&parsed, &modify_path, &mut stdin, &mut stdout)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("matches"),
            "expected ambiguous-match error, got: {msg}"
        );
    }

    // ---- set path (slice 12) ---------------------------------------------

    #[test]
    fn set_attribute_value_replaces_span() {
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            set path "/cfg/window/@width" "1920"
        "#};
        let sys = r#"<cfg><window width="800" height="600"/></cfg>"#;
        let src = r#"<cfg><window width="800" height="600"/></cfg>"#;
        let out = run_process(script, sys, Some(src));
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, r#"<cfg><window width="1920" height="600"/></cfg>"#);
    }

    #[test]
    fn set_attribute_xml_escapes_value() {
        // The literal contains all four escape-required characters for
        // attribute values: `&`, `<`, `>`, `"`.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            set path "/cfg/window/@title" "A & B <C> \"x\""
        "#};
        let sys = r#"<cfg><window title="orig"/></cfg>"#;
        let src = r#"<cfg><window title="orig"/></cfg>"#;
        let out = run_process(script, sys, Some(src));
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            r#"<cfg><window title="A &amp; B &lt;C&gt; &quot;x&quot;"/></cfg>"#
        );
    }

    #[test]
    fn set_text_replaces_text_span() {
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            set path "/cfg/title/text()" "New <Title> & more"
        "#};
        let sys = r#"<cfg><title>Old</title></cfg>"#;
        let src = r#"<cfg><title>Old</title></cfg>"#;
        let out = run_process(script, sys, Some(src));
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            r#"<cfg><title>New &lt;Title&gt; &amp; more</title></cfg>"#
        );
    }

    #[test]
    fn set_path_missing_attr_errors() {
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            set path "/cfg/window/@missing" "x"
        "#};
        let sys = r#"<cfg><window width="800"/></cfg>"#;
        let src = r#"<cfg><window width="800"/></cfg>"#;
        let tmp = tempfile::tempdir().unwrap();
        let dir: camino::Utf8PathBuf = tmp.path().to_path_buf().try_into().unwrap();
        let modify_path = dir.join("modify_test.xml");
        std::fs::write(&modify_path, script).unwrap();
        std::fs::write(dir.join("test.xml.src.xml"), src).unwrap();
        let raw = std::fs::read(&modify_path).unwrap();
        let parsed = config::Script::parse(&raw, &modify_path).unwrap();
        let backend = XmlBackend;
        let mut stdin = std::io::Cursor::new(sys.as_bytes().to_vec());
        let mut stdout: Vec<u8> = Vec::new();
        let err = backend
            .process(&parsed, &modify_path, &mut stdin, &mut stdout)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("set path") && msg.contains("no attribute"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn set_path_against_element_target_errors() {
        // `set path` resolving to an element (no `/@attr` or `/text()`)
        // is invalid: slice 12 only supports scalar replacement.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            set path "/cfg/window" "value"
        "#};
        let sys = r#"<cfg><window/></cfg>"#;
        let src = r#"<cfg><window/></cfg>"#;
        let tmp = tempfile::tempdir().unwrap();
        let dir: camino::Utf8PathBuf = tmp.path().to_path_buf().try_into().unwrap();
        let modify_path = dir.join("modify_test.xml");
        std::fs::write(&modify_path, script).unwrap();
        std::fs::write(dir.join("test.xml.src.xml"), src).unwrap();
        let raw = std::fs::read(&modify_path).unwrap();
        let parsed = config::Script::parse(&raw, &modify_path).unwrap();
        let backend = XmlBackend;
        let mut stdin = std::io::Cursor::new(sys.as_bytes().to_vec());
        let mut stdout: Vec<u8> = Vec::new();
        let err = backend
            .process(&parsed, &modify_path, &mut stdin, &mut stdout)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("set path") && msg.contains("attribute or text()"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn process_missing_selector_errors() {
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            ignore path "/config/missing/@v"
        "#};
        let sys = r#"<config/>"#;
        let src = r#"<config/>"#;
        let tmp = tempfile::tempdir().unwrap();
        let dir: camino::Utf8PathBuf = tmp.path().to_path_buf().try_into().unwrap();
        let modify_path = dir.join("modify_test.xml");
        std::fs::write(&modify_path, script).unwrap();
        std::fs::write(dir.join("test.xml.src.xml"), src).unwrap();
        let raw = std::fs::read(&modify_path).unwrap();
        let parsed = config::Script::parse(&raw, &modify_path).unwrap();

        let backend = XmlBackend;
        let mut stdin = std::io::Cursor::new(sys.as_bytes().to_vec());
        let mut stdout: Vec<u8> = Vec::new();
        let err = backend
            .process(&parsed, &modify_path, &mut stdin, &mut stdout)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no element"), "got: {msg}");
    }

    // ---- bug fix regressions -------------------------------------------------

    #[test]
    fn filter_ignore_path_removes_attribute() {
        // `ignore path` on the re-add path must drop the addressed
        // attribute so a value pinned to the live side (typically a
        // secret) doesn't leak through `chezmoi re-add`.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            ignore path "/config/auth/@token"
        "#};
        let live = r#"<config><auth user="bob" token="s3cret"/></config>"#;
        let out = run_filter(script, live);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, r#"<config><auth user="bob"/></config>"#);
        assert!(!s.contains("s3cret"), "secret leaked through filter: {s}");
    }

    #[test]
    fn text_target_mixed_content_errors() {
        // The RFC declares mixed content out of scope. Targeting
        // `text()` on `<title>Hello <b>bold</b> world</title>` would
        // otherwise span the child `<b>` element and corrupt it on
        // patch — we now refuse such targets.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            add:hide path "/x/title/text()"
        "#};
        let live = r#"<x><title>Hello <b>bold</b> world</title></x>"#;
        let tmp = tempfile::tempdir().unwrap();
        let dir: camino::Utf8PathBuf = tmp.path().to_path_buf().try_into().unwrap();
        let modify_path = dir.join("modify_test.xml");
        std::fs::write(&modify_path, script).unwrap();
        let raw = std::fs::read(&modify_path).unwrap();
        let parsed = config::Script::parse(&raw, &modify_path).unwrap();
        let backend = XmlBackend;
        let err = backend
            .filter(&parsed, &modify_path, live.as_bytes())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("mixed-content") || msg.contains("mixed content"),
            "unexpected error: {msg}",
        );
    }

    #[test]
    fn kde_style_xml_with_named_entities_parses() {
        // Real KDE configuration XML routinely contains named entities
        // beyond the five XML built-ins (`&nbsp;`, `&copy;`, etc.).
        // The tokenizer must accept them rather than reject the whole
        // document. We verify by running an `ignore path` that touches
        // an unrelated attribute — the parse-and-resolve must succeed.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            ignore path "/config/label/@id"
        "#};
        let sys = r#"<config><label id="2">Copyright &copy; 2024</label></config>"#;
        let src = r#"<config><label id="1">Copyright &copy; 2024</label></config>"#;
        let out = run_process(script, sys, Some(src));
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("&copy;"),
            "named entity should be preserved verbatim: {s}",
        );
        assert!(s.contains(r#"id="2""#), "ignore should keep system value");
    }

    #[test]
    fn set_path_text_in_cdata_errors() {
        // `set path "/elem/text()"` against a span that lives inside a
        // `<![CDATA[...]]>` block would replace just the inner content
        // and leave dangling `<![CDATA[ ... ]]>` brackets — corrupt
        // output. We refuse explicitly.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            set path "/cfg/note/text()" "hello"
        "#};
        let sys = r#"<cfg><note><![CDATA[old]]></note></cfg>"#;
        let src = r#"<cfg><note><![CDATA[old]]></note></cfg>"#;
        let tmp = tempfile::tempdir().unwrap();
        let dir: camino::Utf8PathBuf = tmp.path().to_path_buf().try_into().unwrap();
        let modify_path = dir.join("modify_test.xml");
        std::fs::write(&modify_path, script).unwrap();
        std::fs::write(dir.join("test.xml.src.xml"), src).unwrap();
        let raw = std::fs::read(&modify_path).unwrap();
        let parsed = config::Script::parse(&raw, &modify_path).unwrap();
        let backend = XmlBackend;
        let mut stdin = std::io::Cursor::new(sys.as_bytes().to_vec());
        let mut stdout: Vec<u8> = Vec::new();
        let err = backend
            .process(&parsed, &modify_path, &mut stdin, &mut stdout)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("CDATA"), "unexpected error: {msg}",);
    }

    // ---- patch_for_remove whitespace sweep ------------------------------

    #[test]
    fn remove_first_attribute_after_name() {
        // The first attribute is immediately preceded by a single space
        // separating it from the element name. Removing it must not eat
        // the space adjacent to the name; the result still has well-formed
        // syntax.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            remove path "/a/@x"
        "#};
        let doc = r#"<a x="1" y="2"/>"#;
        let out = run_process(script, doc, Some(doc));
        let s = String::from_utf8(out).unwrap();
        // The leading space before `x` is consumed; the gap between the
        // element name and the next attribute is a single space.
        assert_eq!(s, r#"<a y="2"/>"#);
    }

    #[test]
    fn remove_attribute_with_tab_separator() {
        // A tab character before the attribute is treated as whitespace
        // and consumed by the sweep.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            remove path "/a/@y"
        "#};
        let doc = "<a x=\"1\"\ty=\"2\"/>";
        let out = run_process(script, doc, Some(doc));
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, r#"<a x="1"/>"#);
    }

    #[test]
    fn filter_ignore_path_xml_ambiguous_selector_errors() {
        // Round-3 regression: an `ignore path` whose selector matches
        // *more than one* element on the live side must surface as an
        // error rather than silently no-op'ing. The previous
        // `Err(_) => /* no-op */` swallowed the ambiguous-match error
        // and let every matching token leak through `chezmoi re-add`.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            ignore path "/users/user/@token"
        "#};
        let live = r#"<users><user token="s1"/><user token="s2"/></users>"#;
        let tmp = tempfile::tempdir().unwrap();
        let dir: camino::Utf8PathBuf = tmp.path().to_path_buf().try_into().unwrap();
        let modify_path = dir.join("modify_test.xml");
        std::fs::write(&modify_path, script).unwrap();
        let raw = std::fs::read(&modify_path).unwrap();
        let parsed = config::Script::parse(&raw, &modify_path).unwrap();
        let backend = XmlBackend;
        let err = backend
            .filter(&parsed, &modify_path, live.as_bytes())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("matches"),
            "expected ambiguous-match error, got: {msg}"
        );
        // The directive name is included in the propagated context.
        assert!(
            msg.contains("ignore path"),
            "expected directive context: {msg}"
        );
    }

    #[test]
    fn filter_ignore_path_xml_no_match_is_silent_noop() {
        // Round-3 regression: an `ignore path` that addresses an
        // element absent from this particular live file is a legitimate
        // no-op (the user may pin a key that hasn't been written on
        // this machine yet). Output is unchanged; no error is raised.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            ignore path "/config/auth/@token"
        "#};
        let live = r#"<config><other/></config>"#;
        let out = run_filter(script, live);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, live, "no-op expected when path is absent on live");
    }

    #[test]
    fn remove_attribute_with_newline_separator() {
        // Newline separators (multi-line attribute layout) must also be
        // swept.
        let script = indoc! {r#"
            #!/bin/sh
            language xml
            source auto-path
            remove path "/a/@y"
        "#};
        let doc = "<a x=\"1\"\n   y=\"2\"/>";
        let out = run_process(script, doc, Some(doc));
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, r#"<a x="1"/>"#);
    }
}
