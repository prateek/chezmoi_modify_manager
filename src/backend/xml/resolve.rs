//! Resolve a parsed [`XmlPath`] against an [`ElementIndex`] tree to a
//! concrete byte-span target.
//!
//! Path semantics (per RFC §"XML path grammar"):
//!
//! * The first step matches the root element by name.
//! * Each subsequent step descends to a direct child element matching by
//!   name and (optionally) attribute predicate.
//! * The terminal target is the element itself, one of its attributes,
//!   or its direct text content.
//!
//! Scalar selectors must match exactly one node. Zero or multiple
//! matches both error.

use super::index::ElementIndex;
use super::index::TextKind;
use crate::path::xml::Step;
use crate::path::xml::Target;
use crate::path::xml::XmlPath;
use anyhow::anyhow;
use std::ops::Range;

/// Wraps a not-found resolver error so the re-add filter can
/// distinguish it from ambiguous-selector or shape-mismatch failures
/// (those must propagate; otherwise an unintentionally ambiguous
/// directive silently leaks values it was supposed to redact).
///
/// `Display` defers to the inner anyhow chain so the user-visible
/// message is unchanged when we propagate.
#[derive(Debug)]
pub(crate) struct NotFoundError(pub(crate) anyhow::Error);

impl std::fmt::Display for NotFoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NotFoundError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

fn not_found(err: anyhow::Error) -> anyhow::Error {
    anyhow!(NotFoundError(err))
}

/// `true` if `err` was raised because the addressed XML node could not be
/// found (vs. ambiguous selector or shape mismatch).
pub(crate) fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain()
        .any(<dyn std::error::Error + 'static>::is::<NotFoundError>)
}

/// What an [`XmlPath`] resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedTarget {
    /// `/path/to/Element` — the whole element span.
    Element { idx: usize, span: Range<usize> },
    /// `/path/to/Element/@attr` — an attribute value (without quotes) and
    /// the attribute's full token span.
    Attribute {
        elem_idx: usize,
        value_span: Range<usize>,
        full_attr_span: Range<usize>,
    },
    /// `/path/to/Element/text()` — the direct-text region.
    ///
    /// The RFC declares mixed content (text *and* child elements
    /// interleaved) out of scope; the resolver refuses such elements
    /// rather than silently overwriting children. CDATA-only text spans
    /// are surfaced with `is_cdata = true` so that `set path` can refuse
    /// (replacing the inner content would leave dangling
    /// `<![CDATA[ ... ]]>` brackets).
    Text {
        elem_idx: usize,
        span: Range<usize>,
        /// `true` iff the (single) text run is a `<![CDATA[...]]>` block.
        is_cdata: bool,
    },
}

/// Look up `path` in `elements`/`root`, returning the addressed target.
pub(crate) fn resolve(
    path: &XmlPath,
    elements: &[ElementIndex],
    root: usize,
    source: &str,
) -> anyhow::Result<ResolvedTarget> {
    // Step 0: match the root by name + predicate.
    let first_step = path
        .steps
        .first()
        .ok_or_else(|| anyhow!("XML path has no steps"))?;
    if elements[root].name != first_step.name {
        return Err(not_found(anyhow!(
            "XML path: root element `{}` does not match first step `{}`",
            elements[root].name,
            first_step.name
        )));
    }
    if let Some((attr, val)) = &first_step.attr_predicate
        && !attr_matches(&elements[root], attr, val, source)
    {
        return Err(not_found(anyhow!(
            "XML path: root element `{}` does not satisfy predicate [@{}=\"{}\"]",
            first_step.name,
            attr,
            val
        )));
    }

    // Walk the remaining steps through children.
    let mut current = root;
    for (depth, step) in path.steps.iter().enumerate().skip(1) {
        let matches: Vec<usize> = elements[current]
            .children
            .iter()
            .copied()
            .filter(|&child| element_matches(&elements[child], step, source))
            .collect();
        match matches.as_slice() {
            [] => {
                return Err(not_found(anyhow!(
                    "XML path: no element matches step `{}` (segment {} of {})",
                    step_to_string(step),
                    depth + 1,
                    path.steps.len()
                )));
            }
            [only] => current = *only,
            many => {
                return Err(anyhow!(
                    "XML path: step `{}` matches {} elements; selector must be unique \
                     (segment {} of {})",
                    step_to_string(step),
                    many.len(),
                    depth + 1,
                    path.steps.len()
                ));
            }
        }
    }

    // Final step: dispatch on the path target.
    match &path.target {
        Target::Element => Ok(ResolvedTarget::Element {
            idx: current,
            span: elements[current].span.clone(),
        }),
        Target::Attribute(name) => {
            let element = &elements[current];
            let mut hits = element
                .attrs
                .iter()
                .enumerate()
                .filter(|(_, a)| a.name == *name);
            let (_, attr) = hits.next().ok_or_else(|| {
                not_found(anyhow!(
                    "XML path: element `{}` has no attribute `{}`",
                    element.name,
                    name
                ))
            })?;
            if hits.next().is_some() {
                // Should be impossible — `tokens` rejects duplicate
                // attributes — but guard for completeness.
                return Err(anyhow!(
                    "XML path: element `{}` has multiple `{}` attributes",
                    element.name,
                    name
                ));
            }
            Ok(ResolvedTarget::Attribute {
                elem_idx: current,
                value_span: attr.value_span.clone(),
                full_attr_span: attr.span.clone(),
            })
        }
        Target::Text => {
            let element = &elements[current];
            // Reject mixed content. The RFC declares it out of scope;
            // returning a span that covers child elements would silently
            // corrupt the document on a `set path`/`add:hide`/`ignore`
            // patch.
            //
            // Mixed content is anything where:
            //   * the element has direct child elements (text + elements
            //     interleaved), OR
            //   * the element has more than one direct text run (which
            //     can only happen if a child element split them, even if
            //     that child is gone the spans tell us so).
            if !element.children.is_empty() || element.text_spans.len() > 1 {
                return Err(anyhow!(
                    "text() target on mixed-content element is not supported \
                     (element `{}` mixes text and child elements)",
                    element.name
                ));
            }
            let run = match element.text_spans.as_slice() {
                [] => {
                    return Err(not_found(anyhow!(
                        "XML path: element `{}` has no text content",
                        element.name
                    )));
                }
                [only] => only,
                _ => unreachable!("mixed content is rejected above"),
            };
            Ok(ResolvedTarget::Text {
                elem_idx: current,
                span: run.span.clone(),
                is_cdata: matches!(run.kind, TextKind::Cdata),
            })
        }
    }
}

fn element_matches(element: &ElementIndex, step: &Step, source: &str) -> bool {
    if element.name != step.name {
        return false;
    }
    match &step.attr_predicate {
        None => true,
        Some((attr, val)) => attr_matches(element, attr, val, source),
    }
}

fn attr_matches(element: &ElementIndex, attr_name: &str, expected: &str, source: &str) -> bool {
    element
        .attrs
        .iter()
        .find(|a| a.name == attr_name)
        .is_some_and(|a| {
            let raw = &source[a.value_span.clone()];
            decode_attr_value(raw) == *expected
        })
}

/// Decode the five XML built-in entity references in an attribute value.
///
/// The `xmlparser` token gives us the raw byte slice between the quotes
/// — including any `&amp;` / `&lt;` etc. For predicate equality we want
/// the *logical* value, which means decoding those references.
fn decode_attr_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(end_rel) = bytes[i + 1..].iter().position(|&b| b == b';')
        {
            let name = &raw[i + 1..i + 1 + end_rel];
            let decoded = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                other if other.starts_with('#') => decode_numeric_charref(other),
                _ => None,
            };
            if let Some(c) = decoded {
                out.push(c);
                i += 1 + end_rel + 1;
                continue;
            }
        }
        // Push one UTF-8 char without panicking on multi-byte boundaries.
        let ch = raw[i..].chars().next().expect("non-empty by loop guard");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn decode_numeric_charref(name: &str) -> Option<char> {
    let rest = name.strip_prefix('#')?;
    let code = if let Some(hex) = rest.strip_prefix('x').or_else(|| rest.strip_prefix('X')) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        rest.parse::<u32>().ok()?
    };
    char::from_u32(code)
}

fn step_to_string(step: &Step) -> String {
    match &step.attr_predicate {
        None => step.name.clone(),
        Some((a, v)) => format!(r#"{}[@{}="{}"]"#, step.name, a, v),
    }
}

#[cfg(test)]
mod tests {
    use super::super::index::build;
    use super::super::tokens::tokenize;
    use super::*;
    use std::str::FromStr;

    fn doc(src: &str) -> (Vec<ElementIndex>, usize, String) {
        let tokens = tokenize(src).unwrap();
        let (elems, root) = build(&tokens).unwrap();
        (elems, root, src.to_owned())
    }

    fn p(s: &str) -> XmlPath {
        XmlPath::from_str(s).unwrap()
    }

    #[test]
    fn resolve_attribute() {
        let (els, root, src) = doc(r#"<config><window width="800" height="600"/></config>"#);
        let target = resolve(&p("/config/window/@width"), &els, root, &src).unwrap();
        match target {
            ResolvedTarget::Attribute { value_span, .. } => {
                assert_eq!(&src[value_span], "800");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn resolve_element() {
        let (els, root, src) = doc("<root><a><b/></a></root>");
        let target = resolve(&p("/root/a"), &els, root, &src).unwrap();
        match target {
            ResolvedTarget::Element { span, .. } => {
                assert_eq!(&src[span], "<a><b/></a>");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn resolve_text() {
        let (els, root, src) = doc("<root><title>Hello, world!</title></root>");
        let target = resolve(&p("/root/title/text()"), &els, root, &src).unwrap();
        match target {
            ResolvedTarget::Text {
                span,
                is_cdata: false,
                ..
            } => {
                assert_eq!(&src[span], "Hello, world!");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn predicate_match() {
        let src = r#"<gui><Action name="open" key="x"/><Action name="close" key="y"/></gui>"#;
        let (els, root, source) = doc(src);
        let target = resolve(&p(r#"/gui/Action[@name="open"]/@key"#), &els, root, &source).unwrap();
        match target {
            ResolvedTarget::Attribute { value_span, .. } => {
                assert_eq!(&source[value_span], "x");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn predicate_match_with_entity() {
        // Predicate value `He said "hi"` should match `name="He said &quot;hi&quot;"`.
        let src = r#"<gui><Action name="He said &quot;hi&quot;" key="X"/></gui>"#;
        let (els, root, source) = doc(src);
        let target = resolve(
            &p(r#"/gui/Action[@name="He said \"hi\""]/@key"#),
            &els,
            root,
            &source,
        )
        .unwrap();
        match target {
            ResolvedTarget::Attribute { value_span, .. } => {
                assert_eq!(&source[value_span], "X");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn zero_matches_errors() {
        let (els, root, src) = doc("<root><a/></root>");
        let err = resolve(&p("/root/missing"), &els, root, &src).unwrap_err();
        assert!(err.to_string().contains("no element"));
    }

    #[test]
    fn multiple_matches_errors() {
        let (els, root, src) = doc("<root><a/><a/></root>");
        let err = resolve(&p("/root/a"), &els, root, &src).unwrap_err();
        assert!(err.to_string().contains("matches"));
    }

    #[test]
    fn missing_attribute_errors() {
        let (els, root, src) = doc("<root><a x=\"1\"/></root>");
        let err = resolve(&p("/root/a/@y"), &els, root, &src).unwrap_err();
        assert!(err.to_string().contains("no attribute"));
    }

    #[test]
    fn missing_text_errors() {
        let (els, root, src) = doc("<root><a/></root>");
        let err = resolve(&p("/root/a/text()"), &els, root, &src).unwrap_err();
        assert!(err.to_string().contains("text"));
    }

    #[test]
    fn root_name_mismatch_errors() {
        let (els, root, src) = doc("<root><a/></root>");
        let err = resolve(&p("/notroot/a"), &els, root, &src).unwrap_err();
        assert!(err.to_string().contains("root"));
    }

    #[test]
    fn text_target_with_intervening_comment_rejected() {
        // A `<!-- ... -->` between two text runs splits the direct-text
        // sequence into two separate runs, which qualifies as mixed
        // content for the purposes of `text()` resolution.
        let (els, root, src) = doc("<root><title>start<!-- comment -->end</title></root>");
        let err = resolve(&p("/root/title/text()"), &els, root, &src).unwrap_err();
        assert!(
            err.to_string().contains("mixed"),
            "expected mixed-content rejection, got: {err}"
        );
    }

    #[test]
    fn text_target_on_element_only_errors_no_text_content() {
        // The element has only a child element, no direct text. The
        // mixed-content check (children non-empty) fires before the
        // empty-text check.
        let (els, root, src) = doc("<root><title><b>only-bold</b></title></root>");
        let err = resolve(&p("/root/title/text()"), &els, root, &src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("mixed") || msg.contains("no text"),
            "expected mixed-content or no-text-content rejection, got: {msg}"
        );
    }

    #[test]
    fn text_target_two_cdata_blocks_rejected() {
        // Two `<![CDATA[...]]>` blocks back-to-back produce two text
        // runs and so trigger mixed-content rejection.
        let (els, root, src) = doc("<root><note><![CDATA[a]]><![CDATA[b]]></note></root>");
        let err = resolve(&p("/root/note/text()"), &els, root, &src).unwrap_err();
        assert!(
            err.to_string().contains("mixed"),
            "expected mixed-content rejection, got: {err}"
        );
    }
}
