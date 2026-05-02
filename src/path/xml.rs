//! XML path grammar — narrow `XPath` subset.
//!
//! See `docs/src/dev/xml_support_rfc.md` (section "XML path grammar") for the
//! authoritative description. In short:
//!
//! * Paths are absolute and start with `/`.
//! * Element names match `[A-Za-z_][A-Za-z0-9_-]*`. A `prefix:` is reserved
//!   shape (no resolution performed in this slice — the whole literal
//!   `prefix:Element` is stored as the step name).
//! * Element predicates may carry exactly one attribute filter:
//!   `Element[@name="value"]`. The value is a double-quoted string with `\"`
//!   and `\\` escapes.
//! * The path may end with an attribute target (`/@attr`) or a text target
//!   (`/text()`); both must be terminal.
//! * No descendant search (`//`), no functions other than `text()`, no
//!   namespace URI resolution, no boolean predicates, no index selectors.
//!
//! Implementation notes:
//!
//! * Built on `winnow`. The combinators consume the input cursor (`&mut &str`)
//!   directly so reporting precise byte offsets is just `original.len() -
//!   remaining.len()`.
//! * Each rejection-with-message is signalled by `ErrMode::Cut(...)` so
//!   `alt`/`opt` cannot silently swallow it. The wrapper at the `FromStr` layer
//!   translates the cursor position and the topmost context label into a
//!   pleasant `ParseError`.

use std::fmt;
use std::str::FromStr;
use winnow::Parser;
use winnow::combinator::opt;
use winnow::error::AddContext;
use winnow::error::ContextError;
use winnow::error::ErrMode;
use winnow::error::StrContext;
use winnow::error::StrContextValue;
use winnow::prelude::*;
use winnow::token::take_while;

/// A parsed XML path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XmlPath {
    /// One step per path segment.
    pub(crate) steps: Vec<Step>,
    /// What the final selector targets.
    pub(crate) target: Target,
}

/// A single path segment: an element name with an optional attribute
/// predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) name: String,
    /// At most one attribute predicate: `Element[@name="value"]`.
    pub(crate) attr_predicate: Option<(String, String)>,
}

/// What the path's final selector targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// `/path/to/Element` — selects the element itself.
    Element,
    /// `/path/.../@attr` — selects an attribute on the final element.
    Attribute(String),
    /// `/path/.../text()` — selects the text content of the final element.
    Text,
}

/// Error type for XML path parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError {
    /// The original input.
    pub(crate) input: String,
    /// Byte offset into `input` where the offending span begins.
    pub(crate) offset: usize,
    /// Human-readable message describing what went wrong.
    pub(crate) message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "XML path parse error at byte {}: {}\n  in: {}\n      {}^",
            self.offset,
            self.message,
            self.input,
            " ".repeat(self.offset)
        )
    }
}

impl std::error::Error for ParseError {}

impl FromStr for XmlPath {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let original_len = s.len();
        let mut input = s;
        match xml_path.parse_next(&mut input) {
            Ok(path) => {
                if input.is_empty() {
                    Ok(path)
                } else {
                    let offset = original_len - input.len();
                    Err(ParseError {
                        input: s.to_owned(),
                        offset,
                        message: format!("unexpected trailing input: {input:?}"),
                    })
                }
            }
            Err(err) => {
                let offset = original_len - input.len();
                Err(ParseError {
                    input: s.to_owned(),
                    offset,
                    message: render_err(&err),
                })
            }
        }
    }
}

impl fmt::Display for XmlPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for step in &self.steps {
            write!(f, "/{}", step.name)?;
            if let Some((attr, value)) = &step.attr_predicate {
                write!(f, "[@{}=\"{}\"]", attr, escape_attr_value(value))?;
            }
        }
        match &self.target {
            Target::Element => Ok(()),
            Target::Attribute(name) => write!(f, "/@{name}"),
            Target::Text => write!(f, "/text()"),
        }
    }
}

/// Escape a quoted attribute value's interior for canonical Display output.
fn escape_attr_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            other => out.push(other),
        }
    }
    out
}

/// Pull a human-friendly description out of a winnow `ContextError`. We pick
/// the first `Description` we find, since our combinators only ever attach
/// one such label per rejection site.
fn render_err(err: &ErrMode<ContextError>) -> String {
    let ctx = match err {
        ErrMode::Backtrack(c) | ErrMode::Cut(c) => c,
        ErrMode::Incomplete(_) => return "incomplete input".to_owned(),
    };
    for c in ctx.context() {
        if let StrContext::Expected(StrContextValue::Description(desc)) = c {
            return (*desc).to_owned();
        }
    }
    "parse error".to_owned()
}

// ---------------------------------------------------------------------------
// Top-level path parser
// ---------------------------------------------------------------------------

/// Parse a complete XML path. Consumes the entire input on success.
fn xml_path(i: &mut &str) -> ModalResult<XmlPath> {
    // Reject descendant search early for a clearer message.
    if i.starts_with("//") {
        return Err(cut(i, "descendant search '//' is not supported"));
    }

    // Leading slash is required.
    if !consume_char(i, '/') {
        return Err(cut(i, "absolute path must start with '/'"));
    }

    // Empty path: nothing after '/'.
    if i.is_empty() {
        return Err(cut(i, "empty path: at least one step required"));
    }

    // First segment must be an element step.
    let first = element_step.parse_next(i)?;
    let mut steps = vec![first];

    // Remaining segments: each begins with '/'.
    let target = loop {
        if i.is_empty() {
            break Target::Element;
        }
        if !consume_char(i, '/') {
            return Err(cut(i, "expected '/' between path segments"));
        }

        if i.starts_with('@') {
            // Attribute target.
            consume_char(i, '@');
            let name = identifier_with_prefix(i, "attribute name after '@'")?;
            if !i.is_empty() {
                return Err(cut(i, "attribute target must be terminal"));
            }
            break Target::Attribute(name);
        } else if i.starts_with("text()") {
            *i = &i["text()".len()..];
            if !i.is_empty() {
                return Err(cut(i, "text() target must be terminal"));
            }
            break Target::Text;
        } else if i.is_empty() {
            return Err(cut(i, "trailing '/' without a segment"));
        }
        steps.push(element_step.parse_next(i)?);
    };

    Ok(XmlPath { steps, target })
}

// ---------------------------------------------------------------------------
// Combinators (placed at the bottom of the module per project convention).
// ---------------------------------------------------------------------------

/// Parse a single element step: name with optional `[@attr="value"]`.
fn element_step(i: &mut &str) -> ModalResult<Step> {
    let name = element_name(i)?;
    let attr_predicate = opt(predicate).parse_next(i)?;

    // A second `[` would mean a multi-predicate form.
    if i.starts_with('[') {
        return Err(cut(i, "at most one predicate per element step"));
    }

    Ok(Step {
        name,
        attr_predicate,
    })
}

/// Parse an element name: `Identifier` or `Prefix:Identifier`.
fn element_name(i: &mut &str) -> ModalResult<String> {
    let first = identifier(i, "element name")?;
    if consume_char(i, ':') {
        let local = identifier(i, "local name after ':' in 'prefix:local'")?;
        Ok(format!("{first}:{local}"))
    } else {
        Ok(first.to_owned())
    }
}

/// Parse `[@name="value"]`. Returns the attribute name and the unescaped
/// value.
fn predicate(i: &mut &str) -> ModalResult<(String, String)> {
    if !i.starts_with('[') {
        // Soft fail (Backtrack) — this is the `opt` case.
        return Err(ErrMode::Backtrack(ContextError::new()));
    }
    consume_char(i, '[');

    // Index predicate: explicit reject.
    if let Some(c) = i.chars().next()
        && c.is_ascii_digit()
    {
        return Err(cut(i, "index predicates are not supported"));
    }

    if !consume_char(i, '@') {
        return Err(cut(i, "predicate must be of the form '@name=\"value\"'"));
    }

    let name = identifier_with_prefix(i, "attribute name in predicate")?;

    if !consume_char(i, '=') {
        return Err(cut(i, "expected '=' between attribute name and value"));
    }

    let value = if i.starts_with('"') {
        quoted_string(i)?
    } else if i.starts_with('\'') {
        single_quoted_string(i)?
    } else {
        return Err(cut(i, "attribute value must be a double-quoted string"));
    };

    if !i.starts_with(']') {
        return Err(cut(i, "only one attribute permitted in predicate"));
    }
    consume_char(i, ']');

    Ok((name, value))
}

/// Parse a single identifier matching `[A-Za-z_][A-Za-z0-9_-]*`. The `what`
/// argument is the user-facing description for diagnostics.
fn identifier<'s>(i: &mut &'s str, what: &'static str) -> ModalResult<&'s str> {
    let starts_with_id = matches!(i.chars().next(), Some(c) if is_identifier_start(c));
    if !starts_with_id {
        return Err(cut(i, what));
    }
    take_while(1.., is_identifier_continue).parse_next(i)
}

/// Parse `Identifier` or `Identifier:Identifier`.
fn identifier_with_prefix(i: &mut &str, what: &'static str) -> ModalResult<String> {
    let first = identifier(i, what)?.to_owned();
    if consume_char(i, ':') {
        let local = identifier(i, "local name after ':'")?;
        Ok(format!("{first}:{local}"))
    } else {
        Ok(first)
    }
}

/// Parse a double-quoted string with `\"` and `\\` escapes.
fn quoted_string(i: &mut &str) -> ModalResult<String> {
    delimited_string(i, '"')
}

/// Single-quoted variant of [`quoted_string`]. Mirrors the `XPath`
/// convention where either quote may delimit a predicate value. Inside
/// `'...'` only `\\` and `\'` are escapes; `"` needs no escaping.
fn single_quoted_string(i: &mut &str) -> ModalResult<String> {
    delimited_string(i, '\'')
}

/// Shared implementation for [`quoted_string`] and [`single_quoted_string`].
/// Inside the delimited run only `\\` and `\<delim>` are recognised
/// escapes; the unescaped delimiter terminates the string.
fn delimited_string(i: &mut &str, delim: char) -> ModalResult<String> {
    let opening_msg: &'static str = if delim == '"' {
        "expected opening '\"'"
    } else {
        "expected opening `'`"
    };
    let closing_msg: &'static str = if delim == '"' {
        "unterminated string: missing closing '\"'"
    } else {
        "unterminated string: missing closing `'`"
    };
    let escape_msg: &'static str = if delim == '"' {
        "invalid escape: only \\\\ and \\\" are supported"
    } else {
        "invalid escape: only \\\\ and \\' are supported"
    };

    if !consume_char(i, delim) {
        return Err(cut(i, opening_msg));
    }
    let mut out = String::new();
    loop {
        match i.chars().next() {
            None => return Err(cut(i, closing_msg)),
            Some(c) if c == delim => {
                *i = &i[c.len_utf8()..];
                return Ok(out);
            }
            Some('\\') => {
                *i = &i['\\'.len_utf8()..];
                match i.chars().next() {
                    Some(c) if c == delim => {
                        out.push(delim);
                        *i = &i[c.len_utf8()..];
                    }
                    Some('\\') => {
                        out.push('\\');
                        *i = &i['\\'.len_utf8()..];
                    }
                    _ => {
                        return Err(cut(i, escape_msg));
                    }
                }
            }
            Some(c) => {
                out.push(c);
                *i = &i[c.len_utf8()..];
            }
        }
    }
}

/// Build a `Cut` error attached at the current cursor position. The cursor is
/// not advanced.
fn cut(i: &&str, message: &'static str) -> ErrMode<ContextError> {
    let checkpoint = i.checkpoint();
    let err = ContextError::new().add_context(
        i,
        &checkpoint,
        StrContext::Expected(StrContextValue::Description(message)),
    );
    ErrMode::Cut(err)
}

/// Try to consume a single ASCII char; return whether it was consumed.
fn consume_char(i: &mut &str, c: char) -> bool {
    if i.starts_with(c) {
        *i = &i[c.len_utf8()..];
        true
    } else {
        false
    }
}

fn is_identifier_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_identifier_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn parse(s: &str) -> Result<XmlPath, ParseError> {
        s.parse::<XmlPath>()
    }

    #[test]
    fn parse_simple_element_path() {
        let p = parse("/config/window").unwrap();
        assert_eq!(
            p,
            XmlPath {
                steps: vec![
                    Step {
                        name: "config".into(),
                        attr_predicate: None,
                    },
                    Step {
                        name: "window".into(),
                        attr_predicate: None,
                    },
                ],
                target: Target::Element,
            }
        );
    }

    #[test]
    fn parse_attribute_target() {
        let p = parse("/config/window/@width").unwrap();
        assert_eq!(p.steps.len(), 2);
        assert_eq!(p.target, Target::Attribute("width".into()));
    }

    #[test]
    fn parse_text_target() {
        let p = parse("/config/title/text()").unwrap();
        assert_eq!(p.steps.len(), 2);
        assert_eq!(p.target, Target::Text);
    }

    #[test]
    fn parse_attribute_predicate() {
        let p = parse(r#"/gui/Action[@name="open"]/@shortcut"#).unwrap();
        assert_eq!(p.steps.len(), 2);
        assert_eq!(
            p.steps[1],
            Step {
                name: "Action".into(),
                attr_predicate: Some(("name".into(), "open".into())),
            }
        );
        assert_eq!(p.target, Target::Attribute("shortcut".into()));
    }

    #[test]
    fn parse_namespace_prefix() {
        let p = parse("/x:foo/y:bar/@z:baz").unwrap();
        assert_eq!(
            p,
            XmlPath {
                steps: vec![
                    Step {
                        name: "x:foo".into(),
                        attr_predicate: None,
                    },
                    Step {
                        name: "y:bar".into(),
                        attr_predicate: None,
                    },
                ],
                target: Target::Attribute("z:baz".into()),
            }
        );
    }

    #[test]
    fn predicate_with_single_quotes_no_escape() {
        // XPath convention: either quote may delimit a predicate value.
        // Single-quoted form lets the user avoid `\"` escaping inside
        // the surrounding `"..."` path string.
        let dq = parse(r#"/Accounts/Item[@name="main"]/@password"#).unwrap();
        let sq = parse(r#"/Accounts/Item[@name='main']/@password"#).unwrap();
        assert_eq!(sq, dq);

        // The other quote needs no escape inside the single-quoted form.
        let p = parse(r#"/Element[@name='he said "hi"']"#).unwrap();
        assert_eq!(
            p.steps[0].attr_predicate,
            Some(("name".into(), r#"he said "hi""#.into()))
        );

        // Within single quotes, `\'` and `\\` are still escapes.
        let p = parse(r"/Element[@name='it\'s fine']").unwrap();
        assert_eq!(
            p.steps[0].attr_predicate,
            Some(("name".into(), "it's fine".into()))
        );
    }

    #[test]
    fn parse_quoted_value_with_escapes() {
        let p = parse(r#"/Element[@name="he said \"hi\""]"#).unwrap();
        assert_eq!(
            p.steps[0].attr_predicate,
            Some(("name".into(), r#"he said "hi""#.into()))
        );
    }

    #[test]
    fn parse_quoted_value_with_backslash() {
        let p = parse(r#"/Element[@path="C:\\foo\\bar"]"#).unwrap();
        assert_eq!(
            p.steps[0].attr_predicate,
            Some(("path".into(), r"C:\foo\bar".into()))
        );
    }

    #[test]
    fn roundtrip_via_display() {
        let cases = [
            "/gui/ActionProperties/Action[@name=\"open\"]/@shortcut",
            "/config/window/@width",
            "/config/title/text()",
            "/x:foo/y:bar/@z:baz",
            r#"/Element[@name="he said \"hi\""]"#,
            r#"/Element[@path="C:\\foo"]"#,
            "/single",
        ];
        for s in cases {
            let parsed = parse(s).unwrap_or_else(|e| panic!("parse failed for {s:?}: {e}"));
            assert_eq!(parsed.to_string(), s, "round-trip differs for {s:?}");
        }
    }

    #[test]
    fn error_missing_leading_slash() {
        let err = parse("gui/Action").unwrap_err();
        assert_eq!(err.offset, 0);
        assert!(err.message.contains("absolute"));
    }

    #[test]
    fn error_empty_path() {
        let err = parse("/").unwrap_err();
        assert_eq!(err.offset, 1);
        assert!(err.message.contains("empty"));
    }

    #[test]
    fn error_unterminated_string() {
        let err = parse(r#"/Element[@name="unclosed]"#).unwrap_err();
        // Offset lands at end-of-input where the closing quote was expected.
        assert_eq!(err.offset, r#"/Element[@name="unclosed]"#.len());
        assert!(err.message.contains("unterminated"));
    }

    #[test]
    fn error_unquoted_predicate_value() {
        let err = parse(r#"/Element[@name=value]"#).unwrap_err();
        let prefix = r#"/Element[@name="#;
        assert_eq!(err.offset, prefix.len());
        assert!(err.message.contains("double-quoted"));
    }

    #[test]
    fn error_multi_attribute_predicate() {
        let err = parse(r#"/Element[@name="x" @other="y"]"#).unwrap_err();
        // After `"x"` we expected `]`.
        let prefix = r#"/Element[@name="x""#;
        assert_eq!(err.offset, prefix.len());
        assert!(err.message.contains("one attribute"));
    }

    #[test]
    fn error_descendant_search() {
        let err = parse("//Element").unwrap_err();
        assert_eq!(err.offset, 0);
        assert!(err.message.contains("descendant"));
    }

    #[test]
    fn error_index_predicate() {
        let err = parse("/Element[1]").unwrap_err();
        let prefix = "/Element[";
        assert_eq!(err.offset, prefix.len());
        assert!(err.message.to_lowercase().contains("index"));
    }

    #[test]
    fn error_text_not_terminal() {
        let err = parse("/Element/text()/more").unwrap_err();
        let prefix = "/Element/text()";
        assert_eq!(err.offset, prefix.len());
        assert!(err.message.to_lowercase().contains("terminal"));
    }

    #[test]
    fn error_attribute_not_terminal() {
        let err = parse("/Element/@foo/more").unwrap_err();
        let prefix = "/Element/@foo";
        assert_eq!(err.offset, prefix.len());
        assert!(err.message.to_lowercase().contains("terminal"));
    }
}
