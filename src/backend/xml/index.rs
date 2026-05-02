//! Index of elements and attributes built from a [`tokens`](super::tokens)
//! token stream.
//!
//! The index is the lookup structure that path resolution uses. It is
//! built once per source-or-live document and never mutated; mutations
//! flow through the byte-span patcher in [`super::patch`] instead.
//!
//! Element span semantics: for an element written as `<foo attr="v">…</foo>`,
//! the recorded `span` covers the whole thing — from the leading `<` of
//! the opener to the trailing `>` of the closer. For a self-closing
//! `<foo/>` it covers `<foo/>`. This is the byte range a `remove path`
//! directive consumes when targeting an element.
//!
//! Text spans collected on an element correspond to non-CDATA `Text`
//! tokens that are direct children of the element. Whitespace counts.

use super::tokens::ElementEndKind;
use super::tokens::Token;
use anyhow::anyhow;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttrIndex {
    /// Attribute name as it appears in the source (with prefix retained).
    pub(crate) name: String,
    /// Byte span of the attribute name.
    #[allow(dead_code)] // kept for future diagnostics; resolve uses `name`.
    pub(crate) name_span: Range<usize>,
    /// Byte span of the value, *not* including the surrounding quotes.
    pub(crate) value_span: Range<usize>,
    /// Byte span of the whole attribute (including quotes and `=`).
    pub(crate) span: Range<usize>,
}

/// Origin of a direct-text span on an element: a regular `Text` token, or
/// an entire `<![CDATA[...]]>` block. The patcher needs to know which kind
/// it is targeting because:
///
/// * a `set path "/elem/text()"` against a CDATA span would replace the
///   inner content while leaving stray `<![CDATA[ ... ]]>` brackets
///   (corrupt output) — we reject such a set; and
/// * mixed-content detection ignores attribute-only tokens but considers
///   both flavours of text run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextKind {
    /// A regular text run (possibly containing entity references). The
    /// span is the verbatim source bytes.
    Text,
    /// A `<![CDATA[...]]>` block. The span covers the whole construct
    /// including the `<![CDATA[` and `]]>` brackets.
    Cdata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextRun {
    pub(crate) kind: TextKind,
    pub(crate) span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ElementIndex {
    /// Element name (with namespace prefix retained).
    pub(crate) name: String,
    /// Byte span of the element name in the opener.
    pub(crate) name_span: Range<usize>,
    /// Full element span from opener `<` through closer `>` (inclusive).
    pub(crate) span: Range<usize>,
    /// Attributes on this element.
    pub(crate) attrs: Vec<AttrIndex>,
    /// Direct child elements, as indices into the parent `Vec<ElementIndex>`.
    pub(crate) children: Vec<usize>,
    /// Direct text runs (regular `Text` tokens or `<![CDATA[...]]>`
    /// blocks) between this element's opener and closer that are not
    /// inside any child element.
    pub(crate) text_spans: Vec<TextRun>,
    /// Index of the parent element, or `None` for the root.
    pub(crate) parent: Option<usize>,
}

/// Build the element index from `tokens`.
///
/// Returns the `Vec<ElementIndex>` and the index of the root element.
/// Errors if there is no root element (e.g. an empty document).
pub(crate) fn build(tokens: &[Token]) -> anyhow::Result<(Vec<ElementIndex>, usize)> {
    let mut elements: Vec<ElementIndex> = Vec::new();
    // Stack of `(elem_idx, opener_start)` for the currently-open elements.
    let mut stack: Vec<usize> = Vec::new();
    let mut root: Option<usize> = None;

    for tok in tokens {
        match tok {
            Token::ElementStart {
                name,
                name_span,
                span,
            } => {
                let idx = elements.len();
                elements.push(ElementIndex {
                    name: name.clone(),
                    name_span: name_span.clone(),
                    // Opener-only for now; adjusted when the close arrives.
                    span: span.start..span.end,
                    attrs: Vec::new(),
                    children: Vec::new(),
                    text_spans: Vec::new(),
                    parent: stack.last().copied(),
                });
                if let Some(&parent) = stack.last() {
                    elements[parent].children.push(idx);
                } else if root.is_none() {
                    root = Some(idx);
                }
                stack.push(idx);
            }
            Token::Attribute {
                name,
                name_span,
                value_span,
                span,
            } => {
                let cur = stack
                    .last()
                    .copied()
                    .ok_or_else(|| anyhow!("attribute outside any element"))?;
                elements[cur].attrs.push(AttrIndex {
                    name: name.clone(),
                    name_span: name_span.clone(),
                    value_span: value_span.clone(),
                    span: span.clone(),
                });
            }
            Token::ElementEnd { kind, span } => match kind {
                ElementEndKind::Open => {
                    // Just terminates the start tag. Element span is updated
                    // at close time.
                }
                ElementEndKind::Empty => {
                    let idx = stack
                        .pop()
                        .ok_or_else(|| anyhow!("element-end with empty stack"))?;
                    elements[idx].span.end = span.end;
                }
                ElementEndKind::Close => {
                    let idx = stack
                        .pop()
                        .ok_or_else(|| anyhow!("element-end with empty stack"))?;
                    elements[idx].span.end = span.end;
                }
            },
            Token::Text { span } => {
                if let Some(&cur) = stack.last() {
                    elements[cur].text_spans.push(TextRun {
                        kind: TextKind::Text,
                        span: span.clone(),
                    });
                }
                // Text outside any element (whitespace between
                // declaration and root) is ignored.
            }
            Token::Cdata { span, .. } => {
                if let Some(&cur) = stack.last() {
                    // Treat CDATA as direct text content for indexing
                    // purposes. The recorded span covers the whole
                    // `<![CDATA[...]]>` construct (including brackets);
                    // mixed-content detection in `resolve` uses this to
                    // refuse `set path` directives that would otherwise
                    // overwrite the inner content while leaving the
                    // brackets behind.
                    elements[cur].text_spans.push(TextRun {
                        kind: TextKind::Cdata,
                        span: span.clone(),
                    });
                }
            }
            Token::Comment { .. } | Token::ProcessingInstruction { .. } | Token::Doctype { .. } => {
                // Not represented in the element/attr index.
            }
        }
    }

    let root = root.ok_or_else(|| anyhow!("XML: document has no root element"))?;
    Ok((elements, root))
}

#[cfg(test)]
mod tests {
    use super::super::tokens::tokenize;
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn nested_elements_indexed() {
        let src = "<root><a/><b><c/></b></root>";
        let tokens = tokenize(src).unwrap();
        let (elems, root) = build(&tokens).unwrap();
        assert_eq!(elems[root].name, "root");
        // Three children of root? No, two: <a/> and <b>.
        assert_eq!(elems[root].children.len(), 2);
        let b_idx = elems[root].children[1];
        assert_eq!(elems[b_idx].name, "b");
        assert_eq!(elems[b_idx].children.len(), 1);
        let c_idx = elems[b_idx].children[0];
        assert_eq!(elems[c_idx].name, "c");
        assert_eq!(elems[c_idx].parent, Some(b_idx));
    }

    #[test]
    fn text_content_collected() {
        let src = "<root>hello <child>world</child>!</root>";
        let tokens = tokenize(src).unwrap();
        let (elems, root) = build(&tokens).unwrap();
        // root has two direct text spans: "hello " and "!"
        let root = &elems[root];
        assert_eq!(root.text_spans.len(), 2);
        let child = &elems[root.children[0]];
        assert_eq!(child.text_spans.len(), 1);
        assert_eq!(&src[child.text_spans[0].span.clone()], "world");
        assert_eq!(child.text_spans[0].kind, TextKind::Text);
    }

    #[test]
    fn namespace_prefixed_names_retained() {
        let src = r#"<ns:root ns:attr="v"><ns:child/></ns:root>"#;
        let tokens = tokenize(src).unwrap();
        let (elems, root) = build(&tokens).unwrap();
        assert_eq!(elems[root].name, "ns:root");
        assert_eq!(elems[root].attrs[0].name, "ns:attr");
        let child_idx = elems[root].children[0];
        assert_eq!(elems[child_idx].name, "ns:child");
    }

    #[test]
    fn element_span_covers_full_extent() {
        let src = "<root><a/></root>";
        let tokens = tokenize(src).unwrap();
        let (elems, root) = build(&tokens).unwrap();
        assert_eq!(&src[elems[root].span.clone()], "<root><a/></root>");
        let a_idx = elems[root].children[0];
        assert_eq!(&src[elems[a_idx].span.clone()], "<a/>");
    }

    #[test]
    fn empty_document_errors() {
        let err = build(&[]).unwrap_err();
        assert!(err.to_string().contains("no root"));
    }
}
