//! Plist key-path grammar — dict/array path with literal/wildcard/predicate
//! selectors.
//!
//! Grammar (per the XML/plist support RFC):
//!
//! * Dict keys are joined with `.`.
//! * Array indices use `[N]` (zero-based).
//! * `[*]` matches every element of the array at that position.
//! * `[key="value"]` matches the single element of an array of dicts whose
//!   `<key>key</key>` child has the given string value.
//! * Keys containing `.`, `[`, `]`, `"`, or whitespace must be double-quoted;
//!   inside quotes only `\"` and `\\` are escapes. Unquoted keys must match
//!   `[A-Za-z_][A-Za-z0-9_-]*`.
//! * A leading `.` is allowed but not required.
//! * The selector must name at least one key or index.
//! * The selector resolves to the *value* node bound to the final key/index.

use std::fmt;
use std::str::FromStr;

use winnow::Parser;
use winnow::combinator::alt;
use winnow::combinator::delimited;
use winnow::combinator::opt;
use winnow::combinator::preceded;
use winnow::combinator::repeat;
use winnow::error::AddContext;
use winnow::error::ContextError;
use winnow::error::ErrMode;
use winnow::error::StrContext;
use winnow::error::StrContextValue;
use winnow::stream::Stream as _;
use winnow::token::take_while;

/// A parsed plist key path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlistPath {
    pub(crate) segments: Vec<Segment>,
}

/// A single segment in a [`PlistPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// `.foo` or `."foo with spaces"` — a dict key.
    Key(String),
    /// `[3]` — a positional array index.
    Index(usize),
    /// `[*]` — multi-match across every element of an array.
    Wildcard,
    /// `[name="value"]` — single-match on an array of dicts whose
    /// `<key>name</key>` has string value `value`.
    Predicate { key: String, value: String },
}

impl FromStr for PlistPath {
    type Err = PlistPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Detect the `[+]` append token early so we can produce the
        // RFC-mandated message instead of a generic parse error.
        if let Some(idx) = s.find("[+]") {
            // Only flag a true, isolated `[+]` selector token. A literal
            // `+` inside a quoted predicate value is unrelated.
            if !inside_quotes(s, idx) {
                return Err(PlistPathError::AppendNotAllowed);
            }
        }

        let mut input = s;
        match path.parse_next(&mut input) {
            Ok(parsed) => {
                if !input.is_empty() {
                    return Err(PlistPathError::Trailing(input.to_owned()));
                }
                Ok(parsed)
            }
            Err(ErrMode::Backtrack(e) | ErrMode::Cut(e)) => {
                Err(PlistPathError::Parse(format!("{e}")))
            }
            Err(ErrMode::Incomplete(_)) => Err(PlistPathError::Parse("incomplete input".into())),
        }
    }
}

impl fmt::Display for PlistPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.segments.iter().enumerate() {
            match segment {
                Segment::Key(name) => {
                    if i > 0 {
                        f.write_str(".")?;
                    }
                    write_key(f, name)?;
                }
                Segment::Index(n) => write!(f, "[{n}]")?,
                Segment::Wildcard => f.write_str("[*]")?,
                Segment::Predicate { key, value } => {
                    write!(f, "[{key}=")?;
                    write_quoted(f, value)?;
                    f.write_str("]")?;
                }
            }
        }
        Ok(())
    }
}

/// Errors produced while parsing a [`PlistPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlistPathError {
    /// `[+]` is the append token used by `set`; not allowed in a path.
    AppendNotAllowed,
    /// Catch-all for low-level winnow errors.
    Parse(String),
    /// Trailing input after a successful parse.
    Trailing(String),
}

impl fmt::Display for PlistPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AppendNotAllowed => f.write_str("append not allowed in this position"),
            Self::Parse(inner) => write!(f, "invalid plist path: {inner}"),
            Self::Trailing(rest) => write!(f, "unexpected trailing input: {rest:?}"),
        }
    }
}

impl std::error::Error for PlistPathError {}

/// Returns `true` when `idx` lies inside a `"..."` or `'...'` quoted run
/// within `s`, honouring the `\\`, `\"`, and `\'` escapes documented for
/// plist paths. The currently active delimiter (if any) is tracked so a
/// `'` inside `"..."` (or vice versa) is treated as plain data, mirroring
/// the parser's behaviour.
fn inside_quotes(s: &str, idx: usize) -> bool {
    let mut active: Option<char> = None;
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        if i >= idx {
            return active.is_some();
        }
        match active {
            None => {
                if c == '"' || c == '\'' {
                    active = Some(c);
                }
            }
            Some(delim) => match c {
                '\\' => {
                    // Skip the next char (escape).
                    chars.next();
                }
                other if other == delim => active = None,
                _ => {}
            },
        }
    }
    active.is_some()
}

// ---- winnow parsers --------------------------------------------------------

type Stream<'a> = &'a str;

fn path(i: &mut Stream<'_>) -> winnow::ModalResult<PlistPath> {
    // Reject empty input up front so we get a precise error.
    if i.is_empty() {
        return Err(ErrMode::Cut(
            ContextError::new()
                .add_context(i, &i.checkpoint(), StrContext::Label("plist path"))
                .add_context(
                    i,
                    &i.checkpoint(),
                    StrContext::Expected(StrContextValue::Description("non-empty path")),
                ),
        ));
    }

    // Optional leading dot.
    let _: Option<()> = opt('.'.void()).parse_next(i)?;

    // First segment must be a key.
    let first = key_segment
        .context(StrContext::Label("first segment (dict key)"))
        .parse_next(i)?;

    // Remaining segments: any of (.key | [N] | [*] | [key="value"]).
    let rest: Vec<Segment> = repeat(0.., next_segment).parse_next(i)?;

    let mut segments = Vec::with_capacity(1 + rest.len());
    segments.push(Segment::Key(first));
    segments.extend(rest);
    Ok(PlistPath { segments })
}

fn next_segment(i: &mut Stream<'_>) -> winnow::ModalResult<Segment> {
    alt((dotted_key, bracketed)).parse_next(i)
}

fn dotted_key(i: &mut Stream<'_>) -> winnow::ModalResult<Segment> {
    preceded('.', key_segment.map(Segment::Key))
        .context(StrContext::Label("dot-key segment"))
        .parse_next(i)
}

fn bracketed(i: &mut Stream<'_>) -> winnow::ModalResult<Segment> {
    delimited('[', bracket_body, ']')
        .context(StrContext::Label("bracket segment"))
        .parse_next(i)
}

fn bracket_body(i: &mut Stream<'_>) -> winnow::ModalResult<Segment> {
    alt((wildcard, predicate, index_segment)).parse_next(i)
}

fn wildcard(i: &mut Stream<'_>) -> winnow::ModalResult<Segment> {
    '*'.value(Segment::Wildcard).parse_next(i)
}

fn index_segment(i: &mut Stream<'_>) -> winnow::ModalResult<Segment> {
    // Disallow `+`, signs, whitespace, comma — caller's `]` terminator catches
    // anything else.
    take_while(1.., |c: char| c.is_ascii_digit())
        .try_map(|digits: &str| digits.parse::<usize>().map(Segment::Index))
        .context(StrContext::Label("array index"))
        .parse_next(i)
}

fn predicate(i: &mut Stream<'_>) -> winnow::ModalResult<Segment> {
    (unquoted_key, '=', predicate_value)
        .map(|(k, _, v)| Segment::Predicate {
            key: k.to_owned(),
            value: v,
        })
        .context(StrContext::Label("predicate"))
        .parse_next(i)
}

/// Predicate value: accepts either `"..."` or `'...'` as the delimiter.
/// The chosen quote becomes the delimiter; the other quote needs no escape.
fn predicate_value(i: &mut Stream<'_>) -> winnow::ModalResult<String> {
    alt((quoted_string, single_quoted_string)).parse_next(i)
}

fn key_segment(i: &mut Stream<'_>) -> winnow::ModalResult<String> {
    alt((quoted_string, unquoted_key.map(str::to_owned))).parse_next(i)
}

fn unquoted_key<'a>(i: &mut Stream<'a>) -> winnow::ModalResult<&'a str> {
    // Must start with [A-Za-z_], then [A-Za-z0-9_-]*. Use a manual one-shot to
    // produce the right error when the first char is wrong (e.g. a digit).
    let cp = i.checkpoint();
    let bytes = i.as_bytes();
    if bytes.is_empty() {
        return Err(ErrMode::Backtrack(ContextError::new().add_context(
            i,
            &cp,
            StrContext::Expected(StrContextValue::Description("unquoted key")),
        )));
    }
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(ErrMode::Backtrack(ContextError::new().add_context(
            i,
            &cp,
            StrContext::Expected(StrContextValue::Description(
                "unquoted key starting with letter or underscore",
            )),
        )));
    }
    let end = bytes
        .iter()
        .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-'))
        .unwrap_or(bytes.len());
    let (matched, rest) = i.split_at(end);
    *i = rest;
    Ok(matched)
}

fn quoted_string(i: &mut Stream<'_>) -> winnow::ModalResult<String> {
    delimited_string(i, '"')
}

/// Single-quoted variant of [`quoted_string`]. Mirrors the `XPath`
/// convention where either quote may delimit a predicate value, the chosen
/// one becoming the delimiter. Inside `'...'` only `\\` and `\'` are
/// escapes; `"` needs no escaping.
fn single_quoted_string(i: &mut Stream<'_>) -> winnow::ModalResult<String> {
    delimited_string(i, '\'')
}

/// Shared implementation for [`quoted_string`] and [`single_quoted_string`].
/// Inside the delimited run only `\\` and `\<delim>` are recognised
/// escapes; the unescaped delimiter terminates the string.
fn delimited_string(i: &mut Stream<'_>, delim: char) -> winnow::ModalResult<String> {
    let cp = i.checkpoint();
    let opening_desc: &'static str = if delim == '"' {
        "opening `\"`"
    } else {
        "opening `'`"
    };
    let closing_desc: &'static str = if delim == '"' {
        "closing `\"`"
    } else {
        "closing `'`"
    };
    let escape_desc: &'static str = if delim == '"' {
        "valid escape (\\\" or \\\\)"
    } else {
        "valid escape (\\' or \\\\)"
    };

    if !i.starts_with(delim) {
        return Err(ErrMode::Backtrack(ContextError::new().add_context(
            i,
            &cp,
            StrContext::Expected(StrContextValue::Description(opening_desc)),
        )));
    }
    *i = &i[delim.len_utf8()..];

    let mut out = String::new();
    let mut closed = false;
    while let Some(c) = i.chars().next() {
        let len = c.len_utf8();
        if c == delim {
            *i = &i[len..];
            closed = true;
            break;
        }
        if c == '\\' {
            *i = &i[len..];
            let esc = i.chars().next().ok_or_else(|| {
                ErrMode::Cut(ContextError::new().add_context(
                    i,
                    &cp,
                    StrContext::Expected(StrContextValue::Description(
                        "escape character after `\\`",
                    )),
                ))
            })?;
            let esc_len = esc.len_utf8();
            if esc == delim {
                out.push(delim);
            } else if esc == '\\' {
                out.push('\\');
            } else {
                // Unknown escape → cut so we don't backtrack into a
                // misleading "missing closing quote" message.
                return Err(ErrMode::Cut(ContextError::new().add_context(
                    i,
                    &cp,
                    StrContext::Expected(StrContextValue::Description(escape_desc)),
                )));
            }
            *i = &i[esc_len..];
            continue;
        }
        out.push(c);
        *i = &i[len..];
    }
    if !closed {
        return Err(ErrMode::Cut(ContextError::new().add_context(
            i,
            &cp,
            StrContext::Expected(StrContextValue::Description(closing_desc)),
        )));
    }
    Ok(out)
}

// ---- Display helpers -------------------------------------------------------

fn key_is_simple(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn write_key(f: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
    if key_is_simple(name) {
        f.write_str(name)
    } else {
        write_quoted(f, name)
    }
}

fn write_quoted(f: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    f.write_str("\"")?;
    for c in value.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            other => f.write_fmt(format_args!("{other}"))?,
        }
    }
    f.write_str("\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn parse(s: &str) -> Result<PlistPath, PlistPathError> {
        s.parse::<PlistPath>()
    }

    #[test]
    fn parse_simple_dot_path() {
        let p = parse("NSGlobalDomain.AppleLanguages").unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("NSGlobalDomain".into()),
                Segment::Key("AppleLanguages".into()),
            ]
        );
    }

    #[test]
    fn parse_quoted_key_with_dot_in_name() {
        let p = parse(r#""com.apple.dock".tilesize"#).unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("com.apple.dock".into()),
                Segment::Key("tilesize".into()),
            ]
        );
    }

    #[test]
    fn parse_array_index() {
        let p = parse("Accounts[0].Password").unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Accounts".into()),
                Segment::Index(0),
                Segment::Key("Password".into()),
            ]
        );
    }

    #[test]
    fn parse_wildcard() {
        let p = parse("Accounts[*].Password").unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Accounts".into()),
                Segment::Wildcard,
                Segment::Key("Password".into()),
            ]
        );
    }

    #[test]
    fn parse_predicate() {
        let p = parse(r#"Accounts[name="main"].Password"#).unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Accounts".into()),
                Segment::Predicate {
                    key: "name".into(),
                    value: "main".into(),
                },
                Segment::Key("Password".into()),
            ]
        );
    }

    #[test]
    fn predicate_with_single_quotes_no_escape() {
        // XPath convention: either quote may delimit a predicate value.
        // Single-quoted form lets the user avoid `\"` escaping inside
        // the surrounding `"..."` path string.
        let dq = parse(r#"Accounts[name="main"].Password"#).unwrap();
        let sq = parse(r#"Accounts[name='main'].Password"#).unwrap();
        assert_eq!(sq, dq);

        // The other quote needs no escape inside the single-quoted form.
        let p = parse(r#"Accounts[name='he said "hi"'].Password"#).unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Accounts".into()),
                Segment::Predicate {
                    key: "name".into(),
                    value: r#"he said "hi""#.into(),
                },
                Segment::Key("Password".into()),
            ]
        );

        // Within single quotes, `\'` and `\\` are still escapes.
        let p = parse(r"Accounts[name='it\'s fine'].x").unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Accounts".into()),
                Segment::Predicate {
                    key: "name".into(),
                    value: "it's fine".into(),
                },
                Segment::Key("x".into()),
            ]
        );
    }

    #[test]
    fn predicate_single_quote_with_backslash_and_escaped_quote() {
        // Combination: a single-quoted predicate value containing both
        // an escaped backslash (`\\` → `\`) and an escaped quote
        // (`\'` → `'`). Verifies that the two escape sequences compose
        // correctly inside the same value.
        let p = parse(r"Accounts[name='a\\b\'c'].x").unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Accounts".into()),
                Segment::Predicate {
                    key: "name".into(),
                    value: r"a\b'c".into(),
                },
                Segment::Key("x".into()),
            ]
        );
    }

    #[test]
    fn parse_predicate_with_escaped_quote() {
        let p = parse(r#"Accounts[name="hello\"world"].x"#).unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Accounts".into()),
                Segment::Predicate {
                    key: "name".into(),
                    value: "hello\"world".into(),
                },
                Segment::Key("x".into()),
            ]
        );
    }

    #[test]
    fn parse_chained_indices() {
        let p = parse("Matrix[0][1]").unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Matrix".into()),
                Segment::Index(0),
                Segment::Index(1),
            ]
        );
    }

    #[test]
    fn parse_leading_dot() {
        let p = parse(".leadingDotIsAllowed").unwrap();
        assert_eq!(p.segments, vec![Segment::Key("leadingDotIsAllowed".into())]);
    }

    #[test]
    fn parse_quoted_with_brackets_and_spaces() {
        let p = parse(r#""Servers"[2]."Hidden Field""#).unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Servers".into()),
                Segment::Index(2),
                Segment::Key("Hidden Field".into()),
            ]
        );
    }

    #[test]
    fn parse_escaped_backslash_in_key() {
        let p = parse(r#""back\\slash".x"#).unwrap();
        assert_eq!(
            p.segments,
            vec![Segment::Key(r"back\slash".into()), Segment::Key("x".into()),]
        );
    }

    #[test]
    fn roundtrip_via_display() {
        // Round-trip: parse(input) → Display → parse must produce the same
        // AST. Canonical Display drops gratuitous quoting on simple keys (so
        // `"Servers"` renders as `Servers`) and a leading dot, so we don't
        // require byte-for-byte equality with the input.
        let cases = [
            "NSGlobalDomain.AppleLanguages[0]",
            r#""com.apple.dock".tilesize"#,
            "Accounts[0].Password",
            r#"Accounts[name="main"].Password"#,
            "Accounts[*].Password",
            r#""Servers"[2]."Hidden Field""#,
            ".leadingDotIsAllowed",
            r#"Accounts[name="hello\"world"].x"#,
        ];
        for input in cases {
            let parsed = parse(input).unwrap_or_else(|e| panic!("parse {input:?}: {e}"));
            let rendered = parsed.to_string();
            let reparsed =
                parse(&rendered).unwrap_or_else(|e| panic!("re-parse {rendered:?}: {e}"));
            assert_eq!(
                parsed, reparsed,
                "round-trip AST mismatch for {input:?} (rendered as {rendered:?})"
            );
            // And rendering the reparsed form must be a fixed point.
            assert_eq!(
                rendered,
                reparsed.to_string(),
                "Display is not idempotent for {input:?}"
            );
        }
    }

    #[test]
    fn display_canonical_forms() {
        // Document the canonical Display behaviour for the cases where the
        // input has redundant quoting / a leading dot.
        assert_eq!(parse(r#""Servers"[2]"#).unwrap().to_string(), "Servers[2]");
        assert_eq!(parse(".foo.bar").unwrap().to_string(), "foo.bar");
        // A key that requires quoting keeps its quotes.
        assert_eq!(
            parse(r#""com.apple.dock""#).unwrap().to_string(),
            r#""com.apple.dock""#
        );
    }

    #[test]
    fn display_leading_dot_is_canonicalised() {
        // Leading dot is accepted but the canonical Display form drops it.
        let p = parse(".leadingDotIsAllowed").unwrap();
        assert_eq!(p.to_string(), "leadingDotIsAllowed");
        // And we round-trip.
        assert_eq!(parse(&p.to_string()).unwrap(), p);
    }

    #[test]
    fn error_empty_path() {
        assert!(matches!(parse(""), Err(PlistPathError::Parse(_))));
        // Specifically: empty string should yield a clear error.
        let err = parse("").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("empty") || msg.contains("plist path"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn error_first_segment_index() {
        let err = parse("[0]").unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, PlistPathError::Parse(_)),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn error_double_dot() {
        assert!(parse("Foo..Bar").is_err());
    }

    #[test]
    fn error_digit_leading_unquoted() {
        assert!(parse("123Key").is_err());
    }

    #[test]
    fn error_whitespace_in_unquoted() {
        assert!(parse("Foo Bar").is_err());
    }

    #[test]
    fn error_unquoted_predicate_value() {
        assert!(parse(r#"Foo[name=value]"#).is_err());
    }

    #[test]
    fn error_unterminated_string() {
        assert!(parse(r#"Foo["unterminated"#).is_err());
        assert!(parse(r#""unterminated"#).is_err());
    }

    #[test]
    fn error_multi_index() {
        assert!(parse("Foo[1, 2]").is_err());
    }

    #[test]
    fn error_multi_attribute_predicate() {
        assert!(parse(r#"Foo[name="a", other="b"]"#).is_err());
    }

    #[test]
    fn error_empty_brackets() {
        assert!(parse("Foo[]").is_err());
    }

    #[test]
    fn error_append_token() {
        let err = parse("Foo[+]").unwrap_err();
        assert_eq!(err, PlistPathError::AppendNotAllowed);
        assert!(err.to_string().contains("append not allowed"));
    }

    #[test]
    fn predicate_value_with_literal_plus_is_not_append() {
        // `[+]` only matters as a stand-alone selector token. A `+` inside a
        // quoted predicate value is just data.
        let p = parse(r#"Foo[name="a+b"]"#).unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Foo".into()),
                Segment::Predicate {
                    key: "name".into(),
                    value: "a+b".into(),
                },
            ]
        );
    }

    #[test]
    fn predicate_value_with_literal_plus_is_not_append_single_quoted() {
        // The single-quoted form must follow the same rule as the
        // double-quoted one: a literal `[+]` inside `'...'` is data, not
        // the append selector. A regression in `inside_quotes` that only
        // tracked `"` would wrongly reject this input.
        let p = parse(r#"Foo[name='[+]']"#).unwrap();
        assert_eq!(
            p.segments,
            vec![
                Segment::Key("Foo".into()),
                Segment::Predicate {
                    key: "name".into(),
                    value: "[+]".into(),
                },
            ]
        );
    }

    #[test]
    fn quoted_key_round_trips_special_chars() {
        let p = parse(r#""a[b]c""#).unwrap();
        assert_eq!(p.segments, vec![Segment::Key("a[b]c".into())]);
        assert_eq!(p.to_string(), r#""a[b]c""#);
    }
}
