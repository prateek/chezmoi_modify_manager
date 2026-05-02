//! Tokenisation wrapper around [`xmlparser::Tokenizer`].
//!
//! The XML backend uses byte-span patching: it must be able to map every
//! token back to its byte offset in the original document. `xmlparser`
//! already exposes byte spans on each token, so this module is mostly a
//! thin shim. It adds a few project-level checks the underlying parser
//! does not perform:
//!
//! * Element start/end stack must match.
//! * Each element's attribute names must be unique.
//! * Entity references must close with a matching `;`. Named entities
//!   beyond the five XML built-ins (e.g. `&nbsp;`, `&copy;`) are
//!   accepted and passed through verbatim — KDE config files and the
//!   like rely on this. The byte-span patcher never needs to decode an
//!   entity inside a span it patches; preservation is sound.
//!
//! The output is an owned `Vec<Token>` with byte ranges into the original
//! source. We deliberately drop `xmlparser`'s borrowed `StrSpan` shape so
//! callers do not need to thread a lifetime through the index.

use anyhow::Context;
use anyhow::anyhow;
use std::collections::HashSet;
use std::ops::Range;

/// A single XML token, as observed in the source byte stream.
///
/// All ranges are byte offsets into the original source string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    /// An `<element` opener (without its trailing `>` or `/>`).
    ElementStart {
        /// Full name as it appears in the source, including any prefix
        /// (`ns:local`).
        name: String,
        /// Byte span of the name.
        name_span: Range<usize>,
        /// Byte span of the entire element-start token (`<name`).
        span: Range<usize>,
    },
    /// An attribute attached to the most recent `ElementStart`.
    Attribute {
        name: String,
        name_span: Range<usize>,
        /// Byte span of the value, *not* including the surrounding quotes.
        value_span: Range<usize>,
        /// Byte span of the whole attribute token, including the equals
        /// sign and the surrounding quotes.
        span: Range<usize>,
    },
    /// `>` or `/>` closing the start tag, or `</name>` closing an element.
    ElementEnd {
        kind: ElementEndKind,
        /// Byte span of just the closing token (`>`, `/>` or `</name>`).
        span: Range<usize>,
    },
    /// Free text between elements.
    Text { span: Range<usize> },
    /// `<![CDATA[...]]>` text. The value span is the inner content; the
    /// outer span includes the `<![CDATA[` and `]]>`.
    Cdata {
        text_span: Range<usize>,
        span: Range<usize>,
    },
    /// `<!-- ... -->`. We never edit comments; we keep them in the index
    /// only for completeness.
    Comment { span: Range<usize> },
    /// `<?xml ... ?>` declaration or `<?target ...?>` processing instruction.
    ProcessingInstruction { span: Range<usize> },
    /// `<!DOCTYPE ... >` token (start, body or end). Treated as opaque.
    Doctype { span: Range<usize> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElementEndKind {
    /// `>` — the start tag finishes, content follows.
    Open,
    /// `/>` — the element is self-closing.
    Empty,
    /// `</name>` — closing tag of a previously opened element.
    Close,
}

/// Tokenise `source`, returning the full token stream.
///
/// Performs project-level validation in addition to whatever
/// `xmlparser` checks itself.
pub(crate) fn tokenize(source: &str) -> anyhow::Result<Vec<Token>> {
    let mut out: Vec<Token> = Vec::new();
    // Stack of open element names, used to verify well-formedness.
    let mut open_stack: Vec<String> = Vec::new();
    // Attribute-name set for the element currently being opened
    // (between `ElementStart` and the next non-`Attribute` token).
    let mut attr_names: HashSet<String> = HashSet::new();
    let mut in_attrs = false;

    let tokenizer = xmlparser::Tokenizer::from(source);
    for tok in tokenizer {
        let tok = tok.with_context(|| "XML tokenisation failed")?;
        match tok {
            xmlparser::Token::Declaration { span, .. } => {
                out.push(Token::ProcessingInstruction { span: span.range() });
            }
            xmlparser::Token::ProcessingInstruction { span, .. } => {
                out.push(Token::ProcessingInstruction { span: span.range() });
            }
            xmlparser::Token::Comment { span, .. } => {
                out.push(Token::Comment { span: span.range() });
            }
            xmlparser::Token::DtdStart { span, .. }
            | xmlparser::Token::EmptyDtd { span, .. }
            | xmlparser::Token::DtdEnd { span }
            | xmlparser::Token::EntityDeclaration { span, .. } => {
                out.push(Token::Doctype { span: span.range() });
            }
            xmlparser::Token::ElementStart {
                prefix,
                local,
                span,
            } => {
                let name = qualified_name(prefix.as_str(), local.as_str());
                let name_start = if prefix.as_str().is_empty() {
                    local.start()
                } else {
                    prefix.start()
                };
                let name_end = local.end();
                in_attrs = true;
                attr_names.clear();
                open_stack.push(name.clone());
                out.push(Token::ElementStart {
                    name,
                    name_span: name_start..name_end,
                    span: span.range(),
                });
            }
            xmlparser::Token::Attribute {
                prefix,
                local,
                value,
                span,
            } => {
                if !in_attrs {
                    return Err(anyhow!(
                        "XML: attribute at byte {} appears outside an element-start",
                        span.start()
                    ));
                }
                let name = qualified_name(prefix.as_str(), local.as_str());
                if !attr_names.insert(name.clone()) {
                    return Err(anyhow!(
                        "XML: duplicate attribute `{}` at byte {}",
                        name,
                        span.start()
                    ));
                }
                // Validate any character/entity references in the value.
                check_entities(value.as_str(), value.start())?;
                let name_start = if prefix.as_str().is_empty() {
                    local.start()
                } else {
                    prefix.start()
                };
                let name_end = local.end();
                out.push(Token::Attribute {
                    name,
                    name_span: name_start..name_end,
                    value_span: value.range(),
                    span: span.range(),
                });
            }
            xmlparser::Token::ElementEnd { end, span } => {
                in_attrs = false;
                attr_names.clear();
                let kind = match end {
                    xmlparser::ElementEnd::Open => ElementEndKind::Open,
                    xmlparser::ElementEnd::Empty => {
                        // `<name/>` — pop the matching opener.
                        if open_stack.pop().is_none() {
                            return Err(anyhow!(
                                "XML: stray `/>` at byte {} with no open element",
                                span.start()
                            ));
                        }
                        ElementEndKind::Empty
                    }
                    xmlparser::ElementEnd::Close(prefix, local) => {
                        let name = qualified_name(prefix.as_str(), local.as_str());
                        match open_stack.pop() {
                            Some(opener) if opener == name => {}
                            Some(opener) => {
                                return Err(anyhow!(
                                    "XML: mismatched element end at byte {}: expected `</{}>`, \
                                     got `</{}>`",
                                    span.start(),
                                    opener,
                                    name,
                                ));
                            }
                            None => {
                                return Err(anyhow!(
                                    "XML: stray `</{}>` at byte {} with no open element",
                                    name,
                                    span.start()
                                ));
                            }
                        }
                        ElementEndKind::Close
                    }
                };
                out.push(Token::ElementEnd {
                    kind,
                    span: span.range(),
                });
            }
            xmlparser::Token::Text { text } => {
                check_entities(text.as_str(), text.start())?;
                out.push(Token::Text { span: text.range() });
            }
            xmlparser::Token::Cdata { text, span } => {
                out.push(Token::Cdata {
                    text_span: text.range(),
                    span: span.range(),
                });
            }
        }
    }

    if let Some(name) = open_stack.last() {
        return Err(anyhow!("XML: unclosed element `<{name}>`"));
    }

    Ok(out)
}

fn qualified_name(prefix: &str, local: &str) -> String {
    if prefix.is_empty() {
        local.to_owned()
    } else {
        format!("{prefix}:{local}")
    }
}

/// Verify that every entity reference in `text` closes with a matching
/// `;`. Returns an error pointing at the offending byte offset (in the
/// *original* source) on failure. `text_start` is the byte offset of
/// `text` within the source so that the error span is meaningful.
///
/// We deliberately accept any named entity, not just the five XML
/// built-ins (`amp`, `lt`, `gt`, `quot`, `apos`) or numeric character
/// references: real-world configuration files (e.g. KDE's `.xml`)
/// routinely contain named entities like `&nbsp;` or `&copy;`. The
/// byte-span patcher never needs to decode an entity that lives inside
/// a span it replaces — it preserves the bytes verbatim — so a
/// permissive parser is sound. Built-in entities are decoded only for
/// attribute-value equality in predicates ([`super::resolve`]); unknown
/// named entities simply don't decode and so don't match.
fn check_entities(text: &str, text_start: usize) -> anyhow::Result<()> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            i += 1;
            continue;
        }
        let start = i;
        // Find the terminating `;`.
        let Some(end_rel) = bytes[i + 1..].iter().position(|&b| b == b';') else {
            return Err(anyhow!(
                "XML: unterminated entity reference at byte {}",
                text_start + start
            ));
        };
        // Validate the name between `&` and `;`. We accept the same
        // shapes the XML spec allows for *references* (a strict subset
        // of `Name` is sufficient for our predicate-decode and for KDE-
        // style configs):
        //   * Numeric:  `&#[0-9]+;`         — decimal char ref
        //   * Hex:      `&#x[0-9A-Fa-f]+;`  — hex char ref
        //   * Named:    `&[A-Za-z_][A-Za-z0-9._-]*;`
        // Anything else (empty name, whitespace inside the name, names
        // beginning with a digit, etc.) is rejected with a byte offset
        // pointing at the `&`.
        let name = &bytes[i + 1..i + 1 + end_rel];
        if !is_valid_entity_name(name) {
            return Err(anyhow!(
                "XML: malformed entity reference at byte {} (name `{}` is not a valid \
                 entity-reference name)",
                text_start + start,
                String::from_utf8_lossy(name)
            ));
        }
        i += 1 + end_rel + 1;
    }
    Ok(())
}

/// Whether the bytes between `&` and `;` form a syntactically valid
/// entity-reference name (numeric, hex, or named). The check is
/// deliberately restrictive: real-world workloads only ever use names
/// matching these shapes, and a permissive fallback would let
/// adversarial inputs (`&;`, `&\n;`, `&1abc;`) sneak through and
/// silently fail predicate equality later.
fn is_valid_entity_name(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    if name[0] == b'#' {
        // Numeric or hex char reference.
        let rest = &name[1..];
        if rest.is_empty() {
            return false;
        }
        if rest[0] == b'x' || rest[0] == b'X' {
            let hex = &rest[1..];
            !hex.is_empty() && hex.iter().all(u8::is_ascii_hexdigit)
        } else {
            rest.iter().all(u8::is_ascii_digit)
        }
    } else {
        // Named entity: starts with letter or underscore, then
        // `[A-Za-z0-9._-]*`. We deliberately exclude the colon — XML
        // spec allows it as a NameStartChar but real entity references
        // never carry namespace prefixes.
        let first = name[0];
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return false;
        }
        name[1..]
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_tokenises() {
        let src = r#"<?xml version="1.0"?><root a="1"><child/>text</root>"#;
        let toks = tokenize(src).unwrap();
        // Decl + ElementStart(root) + Attr(a) + Open + ElementStart(child) + Empty + Text + Close
        assert!(matches!(toks[0], Token::ProcessingInstruction { .. }));
        assert!(matches!(toks[1], Token::ElementStart { .. }));
        assert!(matches!(toks[2], Token::Attribute { .. }));
        assert!(matches!(
            toks[3],
            Token::ElementEnd {
                kind: ElementEndKind::Open,
                ..
            }
        ));
    }

    #[test]
    fn mismatched_element_errors() {
        let src = "<a></b>";
        let err = tokenize(src).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mismatched") || msg.to_lowercase().contains("expected"));
    }

    #[test]
    fn duplicate_attribute_errors() {
        let src = r#"<a x="1" x="2"/>"#;
        let err = tokenize(src).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("duplicate"), "got: {msg}");
    }

    #[test]
    fn unclosed_element_errors() {
        let src = "<a><b></b>";
        let err = tokenize(src).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unclosed") || msg.contains("Unclosed"));
    }

    #[test]
    fn unterminated_entity_errors() {
        // A `&` without a closing `;` is genuinely malformed and should
        // still be rejected.
        let src = r#"<a x="&nbsp"/>"#;
        let err = tokenize(src).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unterminated"), "got: {msg}");
    }

    #[test]
    fn named_entity_passthrough() {
        // Named entities beyond the five XML built-ins (e.g. `&nbsp;`,
        // `&copy;`) must tokenise without error. KDE config files
        // commonly contain these.
        let src = r#"<config><label>Copyright &copy; 2024 &nbsp;</label></config>"#;
        let _ = tokenize(src).unwrap();
    }

    #[test]
    fn builtin_entities_ok() {
        let src = r#"<a x="&amp;&lt;&gt;&quot;&apos;">&amp;</a>"#;
        let _ = tokenize(src).unwrap();
    }

    #[test]
    fn numeric_char_ref_ok() {
        let src = r#"<a x="&#65;&#x41;"/>"#;
        let _ = tokenize(src).unwrap();
    }

    // ---- round-2 fix #4: tightened entity-name validation ---------------

    #[test]
    fn entity_with_empty_name_rejected() {
        // `&;` has zero characters between the `&` and `;`. Previous
        // behaviour silently accepted it and let `decode_attr_value`
        // pass `&` through literally, which then mismatched any
        // predicate equality unexpectedly. Now rejected with a byte-
        // offset diagnostic.
        let src = r#"<a x="&;"/>"#;
        let err = tokenize(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("malformed entity"),
            "expected malformed-entity diagnostic, got: {msg}",
        );
    }

    #[test]
    fn entity_with_newline_in_name_rejected() {
        // A newline (or any non-name char) inside the entity name.
        // Real-world XML never contains this; reject explicitly.
        let src = "<a x=\"&\n;\"/>";
        let err = tokenize(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("malformed entity"),
            "expected malformed-entity diagnostic, got: {msg}",
        );
    }

    #[test]
    fn entity_with_invalid_first_char_rejected() {
        // Named entity must start with letter or underscore. `&1abc;`
        // starts with a digit; reject.
        let src = r#"<a x="&1abc;"/>"#;
        let err = tokenize(src).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("malformed entity"),
            "expected malformed-entity diagnostic, got: {msg}",
        );
    }

    #[test]
    fn namespace_prefix_retained() {
        let src = r#"<ns:root ns:attr="v"/>"#;
        let toks = tokenize(src).unwrap();
        match &toks[0] {
            Token::ElementStart { name, .. } => assert_eq!(name, "ns:root"),
            other => panic!("unexpected: {other:?}"),
        }
        match &toks[1] {
            Token::Attribute { name, .. } => assert_eq!(name, "ns:attr"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
