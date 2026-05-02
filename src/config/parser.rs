//! Defines the winnow parser for the config file format.
//!
//! TODO (round-3 review, deferred): runtime path-resolution errors lack
//! the `{script_path}:{line}` prefix that parse-time errors carry. The
//! outer `inner_main` wraps with `{file_name}: backend processing
//! failed`, so the user sees the file but not the offending line. To fix
//! cleanly, store the source line number alongside each parsed
//! directive (e.g. a `Spanned<T>` newtype on
//! `Config::{plist,xml}.{ignore,remove,set_path,transforms}`) so the
//! merge/filter pipeline can prefix the directive's line in
//! `with_context(...)`. This is a structural change; deferred from
//! round-3 because the surface area is too broad to mix with other
//! correctness fixes.
use crate::backend::Language;
use crate::config::MergeMode;
use crate::config::PlistOutput;
use crate::path::plist::PlistPath;
use crate::path::xml::XmlPath;
use std::collections::HashMap;
use std::str::FromStr;
use winnow::ascii::escaped;
use winnow::ascii::space1;
use winnow::combinator::alt;
use winnow::combinator::delimited;
use winnow::combinator::opt;
use winnow::combinator::preceded;
use winnow::combinator::separated;
use winnow::error::StrContext;
use winnow::prelude::*;
use winnow::token::take_till;
use winnow::token::take_until;

/// A directive in the config file
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Directive {
    /// Whitespace, ignore
    WS,
    /// A source path
    Source(String),
    /// Automatic source localisation (via environment variable)
    SourceAutoEnv,
    #[doc(hidden)]
    /// Automatic source localisation (via relative path)
    ///
    /// This is used internally by the integration tests, but doesn't actually
    /// work with real chezmoi
    SourceAutoPath,
    /// We shouldn't warn on multiple regular expressions matching the same
    /// section + key.
    NoWarnMultipleKeyMatches,
    /// An ignore directive
    Ignore(Matcher),
    /// A transform directive (INI)
    Transform(Matcher, String, HashMap<String, String>),
    /// A plist transform directive (slice 9). Kept separate from
    /// [`Directive::Transform`] because the INI variant requires
    /// section/key matchers and dispatches to the INI transform engine,
    /// while the plist variant addresses a single `plist::Value` node.
    PlistTransform {
        path: PlistPath,
        kind: PlistTransform,
    },
    /// Set a key in a section to a specific value
    Set {
        section: String,
        key: String,
        value: String,
        separator: Option<String>,
    },
    /// `set path "..." [<type>] "<literal>"` — replace-only set on an
    /// XML attribute/text or a plist scalar value (slice 12). The
    /// optional `type_tag` is rejected for XML and used to constrain
    /// the existing type for plist.
    SetPath {
        path: PathMatcher,
        value: String,
        type_tag: Option<PlistTypeTag>,
    },
    /// Remove everything matching a specific matcher
    Remove(Matcher),
    /// On add: remove everything matching a specific matcher
    AddRemove(Matcher),
    /// On add: hide the value of everything matching a specific matcher
    AddHide(Matcher),
    /// Language / backend selector
    Language(Language),
    /// Merge-mode selector (currently only meaningful for plist)
    Merge(MergeMode),
    /// Output format selector (plist-only)
    Output(PlistOutput),
}

/// A plist scalar type tag (slice 12). Used in
/// `set path "..." <type> "<value>"` to constrain the existing
/// scalar's type. The XML `set path` form rejects type tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlistTypeTag {
    String,
    Integer,
    Real,
    Data,
    Date,
}

impl PlistTypeTag {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "string" => Some(Self::String),
            "integer" => Some(Self::Integer),
            "real" => Some(Self::Real),
            "data" => Some(Self::Data),
            "date" => Some(Self::Date),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Real => "real",
            Self::Data => "data",
            Self::Date => "date",
        }
    }
}

/// The path-matcher form (XML or plist) carried by `set path` (slice 12).
/// Mirrors the dispatch already done by [`match_path`] for `ignore` /
/// `add:hide` / `add:remove` / `remove`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathMatcher {
    Xml(XmlPath),
    Plist(PlistPath),
}

/// A plist transform primitive (slice 9). Each variant carries the
/// already-validated arguments needed to apply the transform at process
/// time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlistTransform {
    /// `join-lines` — array of strings to a single newline-joined string.
    JoinLines,
    /// `json-encode` — encode the addressed value as canonical JSON.
    JsonEncode,
    /// `data-encode` — encode the addressed value as canonical JSON, then
    /// wrap the resulting UTF-8 bytes as `plist::Value::Data` (i.e. emit
    /// as a `<data>` element). Used by apps that store JSON blobs as
    /// base64-encoded `<data>` rather than `<string>`.
    DataEncode,
    /// `flatten-keys prefix="..." [json-encode-values | data-encode-values]`
    /// — lift inner dict entries to top-level keys named
    /// `<prefix><inner>`. Optionally JSON-encodes each value (string),
    /// or JSON-then-data-wraps each value (`<data>`). The two
    /// `*-values` flags are mutually exclusive.
    FlattenKeys {
        prefix: String,
        json_encode_values: bool,
        data_encode_values: bool,
    },
}

/// The different ways things can be matched.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Matcher {
    /// Match a whole section (exact name)
    Section(String),
    /// Match a whole section (regex)
    SectionRegex(String),
    /// Match exact section and key names
    Literal(String, String),
    /// Match section and key names using regexes
    Regex(String, String),
    /// Match a node by plist key-path (slice 8). Only valid for
    /// `language plist`; rejected at config-parse time for `language ini`.
    PlistPath(PlistPath),
    /// Match a node by XML path (slices 4–6). Only valid for
    /// `language xml`; rejected at config-parse time for other languages.
    XmlPath(XmlPath),
}

/// Top level parser for the config file
pub(super) fn parse_config(i: &mut &str) -> ModalResult<Vec<Directive>> {
    let alternatives = (
        comment.context(StrContext::Label("comment")),
        chezmoi_template.context(StrContext::Label("chezmoi template")),
        language.context(StrContext::Label("language")),
        merge.context(StrContext::Label("merge")),
        output.context(StrContext::Label("output")),
        source.context(StrContext::Label("source")),
        no_warn_multiple_key_matches.context(StrContext::Label("no-warn-multiple-key-matches")),
        ignore.context(StrContext::Label("ignore")),
        transform_plist.context(StrContext::Label("transform path")),
        transform.context(StrContext::Label("transform")),
        set_path.context(StrContext::Label("set path")),
        set.context(StrContext::Label("set")),
        remove.context(StrContext::Label("remove")),
        add_remove.context(StrContext::Label("add:remove")),
        add_hide.context(StrContext::Label("add:hide")),
        "".map(|_| Directive::WS)
            .context(StrContext::Label("whitespace")), // Blank lines
    );
    (separated(0.., alt(alternatives), newline), opt(newline))
        .map(|(val, _)| val)
        .parse_next(i)
}

/// A newline (LF, CR or CRLF)
fn newline(i: &mut &str) -> ModalResult<()> {
    alt(("\r\n", "\n", "\r")).void().parse_next(i)
}

/// A comment
fn comment(i: &mut &str) -> ModalResult<Directive> {
    ('#', take_till(0.., ['\n', '\r']))
        .void()
        .map(|()| Directive::WS)
        .parse_next(i)
}

/// A chezmoi template. Ignored when re-adding
fn chezmoi_template(i: &mut &str) -> ModalResult<Directive> {
    delimited("{{", take_until(0.., "}}"), "}}")
        .void()
        .map(|()| Directive::WS)
        .parse_next(i)
}

/// A source statement
fn source(i: &mut &str) -> ModalResult<Directive> {
    // To support working on the raw templated files before chezmoi processes
    // them we parse to the end of the line, instead of end of the quotation
    // mark.
    (
        "source",
        space1,
        alt((
            "auto-path".map(|_| Directive::SourceAutoPath),
            "auto".map(|_| Directive::SourceAutoEnv),
            quoted_string_nl.map(Directive::Source),
        )),
    )
        .map(|(_, _, result)| result)
        .parse_next(i)
}

/// A language directive: `language ini|xml|plist`.
fn language(i: &mut &str) -> ModalResult<Directive> {
    (
        "language",
        space1,
        alt((
            "ini".map(|_| Language::Ini),
            "xml".map(|_| Language::Xml),
            "plist".map(|_| Language::Plist),
        )),
    )
        .map(|(_, _, lang)| Directive::Language(lang))
        .parse_next(i)
}

/// A merge directive: `merge shallow|deep`.
fn merge(i: &mut &str) -> ModalResult<Directive> {
    (
        "merge",
        space1,
        alt((
            "shallow".map(|_| MergeMode::Shallow),
            "deep".map(|_| MergeMode::Deep),
        )),
    )
        .map(|(_, _, mode)| Directive::Merge(mode))
        .parse_next(i)
}

/// An output directive: `output xml|binary`.
fn output(i: &mut &str) -> ModalResult<Directive> {
    (
        "output",
        space1,
        alt((
            "xml".map(|_| PlistOutput::Xml),
            "binary".map(|_| PlistOutput::Binary),
        )),
    )
        .map(|(_, _, fmt)| Directive::Output(fmt))
        .parse_next(i)
}

fn no_warn_multiple_key_matches(i: &mut &str) -> ModalResult<Directive> {
    "no-warn-multiple-key-matches"
        .map(|_| Directive::NoWarnMultipleKeyMatches)
        .parse_next(i)
}

/// An ignore statement
fn ignore(i: &mut &str) -> ModalResult<Directive> {
    ("ignore", space1, matcher)
        .map(|(_, _, pattern)| Directive::Ignore(pattern))
        .parse_next(i)
}

fn set(i: &mut &str) -> ModalResult<Directive> {
    (
        "set",
        space1,
        quoted_string,
        space1,
        quoted_string,
        space1,
        quoted_string,
        opt((space1, "separator=", quoted_string).map(|(_, _, v)| v)),
    )
        .map(
            |(_, _, section, _, key, _, value, separator)| Directive::Set {
                section,
                key,
                value,
                separator,
            },
        )
        .parse_next(i)
}

/// `set path "..." [<type>] "<literal>"` (slice 12).
///
/// Two forms are accepted:
///
/// * `set path "<path>" "<literal>"` — no type tag.
/// * `set path "<path>" <type> "<literal>"` — `<type>` is one of
///   `string`/`integer`/`real`/`data`/`date`. Only valid for plist; the
///   XML backend rejects type tags at config-eval time.
///
/// Like [`match_path`], the path string dispatches to [`XmlPath`] or
/// [`PlistPath`] based on whether it starts with `/`. Whether the chosen
/// matcher matches the active `language` is validated later.
///
/// The `set path` prefix is cut so that argument errors propagate as
/// parse-time diagnostics rather than silently backtracking into the
/// INI [`set`] parser.
fn set_path(i: &mut &str) -> ModalResult<Directive> {
    use winnow::error::ContextError;
    use winnow::error::ErrMode;
    use winnow::error::FromExternalError;

    let _ = ("set", space1, "path", space1).parse_next(i)?;
    let raw_path = quoted_string.parse_next(i)?;
    let path = if raw_path.starts_with('/') {
        match XmlPath::from_str(&raw_path) {
            Ok(p) => PathMatcher::Xml(p),
            Err(e) => return Err(ErrMode::Cut(ContextError::from_external_error(i, e))),
        }
    } else {
        match PlistPath::from_str(&raw_path) {
            Ok(p) => PathMatcher::Plist(p),
            Err(e) => return Err(ErrMode::Cut(ContextError::from_external_error(i, e))),
        }
    };

    let _ = space1.parse_next(i)?;

    // Either a type tag followed by a quoted value, or a quoted value directly.
    let (type_tag, value) = if i.starts_with('"') {
        let value = quoted_string.parse_next(i)?;
        (None, value)
    } else {
        let tag_str = take_till(1.., [' ', '\t', '\r', '\n']).parse_next(i)?;
        let tag = PlistTypeTag::parse(tag_str).ok_or_else(|| {
            ErrMode::Cut(ContextError::from_external_error(
                i,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "unknown plist type tag `{tag_str}`; expected one of \
                         string/integer/real/data/date"
                    ),
                ),
            ))
        })?;
        let _ = space1.parse_next(i)?;
        let value = quoted_string.parse_next(i)?;
        (Some(tag), value)
    };

    Ok(Directive::SetPath {
        path,
        value,
        type_tag,
    })
}

fn remove(i: &mut &str) -> ModalResult<Directive> {
    ("remove", space1, matcher)
        .map(|(_, _, matcher)| Directive::Remove(matcher))
        .parse_next(i)
}

fn add_remove(i: &mut &str) -> ModalResult<Directive> {
    ("add:remove", space1, matcher)
        .map(|(_, _, matcher)| Directive::AddRemove(matcher))
        .parse_next(i)
}

fn add_hide(i: &mut &str) -> ModalResult<Directive> {
    ("add:hide", space1, matcher)
        .map(|(_, _, matcher)| Directive::AddHide(matcher))
        .parse_next(i)
}

/// A transform statement
fn transform(i: &mut &str) -> ModalResult<Directive> {
    (
        "transform",
        space1,
        matcher_transform,
        space1,
        take_till(1.., [' ', '\r', '\n']),
        opt(preceded(space1, separated(0.., transform_arg, space1))),
    )
        .map(|(_, _, pattern, _, transform, args)| {
            Directive::Transform(pattern, transform.to_owned(), args.unwrap_or_default())
        })
        .parse_next(i)
}

/// One argument to a transformer on the form `arg="value"`
fn transform_arg(i: &mut &str) -> ModalResult<(String, String)> {
    (take_till(1.., [' ', '=']), '=', quoted_string)
        .map(|(key, _, value)| (key.to_owned(), value))
        .parse_next(i)
}

/// One flag-style argument (no `=value`): `flag-name`. Used by
/// `transform path` for boolean primitives like `json-encode-values`.
fn transform_flag(i: &mut &str) -> ModalResult<String> {
    take_till(1.., [' ', '=', '\r', '\n'])
        .map(str::to_owned)
        .parse_next(i)
}

/// Either a `key="value"` arg or a bare flag. The plist `transform`
/// surface allows mixing them on the same line.
fn transform_plist_arg(i: &mut &str) -> ModalResult<(String, Option<String>)> {
    alt((
        transform_arg.map(|(k, v)| (k, Some(v))),
        transform_flag.map(|k| (k, None)),
    ))
    .parse_next(i)
}

/// `transform path "..." <name> [<key>="<value>" | <flag>]*`.
///
/// A separate, plist-only directive that resolves a single
/// [`PlistPath`] node and applies a named primitive. Cut at the
/// `transform path` prefix so that argument errors propagate as
/// parse-time diagnostics rather than backtracking into the INI
/// `transform` parser.
fn transform_plist(i: &mut &str) -> ModalResult<Directive> {
    use winnow::error::ContextError;
    use winnow::error::ErrMode;
    use winnow::error::FromExternalError;

    let _ = ("transform", space1, "path", space1).parse_next(i)?;
    let raw_path = quoted_string.parse_next(i)?;
    let path = PlistPath::from_str(&raw_path)
        .map_err(|e| ErrMode::Cut(ContextError::from_external_error(i, e)))?;
    let _ = space1.parse_next(i)?;
    let name = take_till(1.., [' ', '\r', '\n']).parse_next(i)?;
    let args: Vec<(String, Option<String>)> = opt(preceded(
        space1,
        separated(0.., transform_plist_arg, space1),
    ))
    .parse_next(i)?
    .unwrap_or_default();
    let kind = build_plist_transform(name, &args).map_err(|e| {
        ErrMode::Cut(ContextError::from_external_error(
            i,
            std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        ))
    })?;
    Ok(Directive::PlistTransform { path, kind })
}

/// Build a [`PlistTransform`] from a parsed name and arg list, validating
/// argument names, required args, and flag compatibility per the slice 9
/// surface. Returns a human-readable error suitable for embedding in a
/// parse-time diagnostic.
fn build_plist_transform(
    name: &str,
    args: &[(String, Option<String>)],
) -> Result<PlistTransform, String> {
    match name {
        "join-lines" => {
            if let Some((k, _)) = args.first() {
                return Err(format!(
                    "transform `join-lines` takes no arguments; unexpected `{k}`"
                ));
            }
            Ok(PlistTransform::JoinLines)
        }
        "json-encode" => {
            if let Some((k, _)) = args.first() {
                return Err(format!(
                    "transform `json-encode` takes no arguments; unexpected `{k}`"
                ));
            }
            Ok(PlistTransform::JsonEncode)
        }
        "data-encode" => {
            if let Some((k, _)) = args.first() {
                return Err(format!(
                    "transform `data-encode` takes no arguments; unexpected `{k}`"
                ));
            }
            Ok(PlistTransform::DataEncode)
        }
        "flatten-keys" => {
            let mut prefix: Option<String> = None;
            let mut json_encode_values = false;
            let mut data_encode_values = false;
            for (k, v) in args {
                match (k.as_str(), v) {
                    ("prefix", Some(val)) => {
                        if prefix.is_some() {
                            return Err(
                                "transform `flatten-keys`: duplicate `prefix=` argument".into()
                            );
                        }
                        prefix = Some(val.clone());
                    }
                    ("json-encode-values", None) => {
                        json_encode_values = true;
                    }
                    ("data-encode-values", None) => {
                        data_encode_values = true;
                    }
                    ("prefix", None) => {
                        return Err(
                            "transform `flatten-keys`: `prefix` requires a `=\"value\"`".into()
                        );
                    }
                    ("json-encode-values", Some(_)) => {
                        return Err(
                            "transform `flatten-keys`: `json-encode-values` is a flag, not a `key=value`"
                                .into(),
                        );
                    }
                    ("data-encode-values", Some(_)) => {
                        return Err(
                            "transform `flatten-keys`: `data-encode-values` is a flag, not a `key=value`"
                                .into(),
                        );
                    }
                    (other, _) => {
                        return Err(format!(
                            "transform `flatten-keys`: unknown argument `{other}`"
                        ));
                    }
                }
            }
            if json_encode_values && data_encode_values {
                return Err(
                    "transform `flatten-keys`: `json-encode-values` and `data-encode-values` \
                     are mutually exclusive"
                        .into(),
                );
            }
            let prefix = prefix
                .ok_or_else(|| "transform `flatten-keys` requires `prefix=\"...\"`".to_string())?;
            Ok(PlistTransform::FlattenKeys {
                prefix,
                json_encode_values,
                data_encode_values,
            })
        }
        other => Err(format!("unknown plist transform `{other}`")),
    }
}

/// Matcher for a section
fn match_section_regex(i: &mut &str) -> ModalResult<Matcher> {
    ("section", space1, "regex", space1, quoted_string)
        .map(|(_, _, _, _, section)| Matcher::SectionRegex(section))
        .parse_next(i)
}

/// Matcher for a section
fn match_section(i: &mut &str) -> ModalResult<Matcher> {
    ("section", space1, quoted_string)
        .map(|(_, _, section)| Matcher::Section(section))
        .parse_next(i)
}

/// Matcher for a regex
fn match_regex(i: &mut &str) -> ModalResult<Matcher> {
    ("regex", space1, quoted_string, space1, quoted_string)
        .map(|(_, _, section, _, key)| Matcher::Regex(section, key))
        .parse_next(i)
}

/// Literal matcher
fn match_literal(i: &mut &str) -> ModalResult<Matcher> {
    (quoted_string, space1, quoted_string)
        .map(|(section, _, key)| Matcher::Literal(section, key))
        .parse_next(i)
}

/// Path matcher: `path "..."`. The string is parsed as either an
/// [`XmlPath`] or a [`PlistPath`], chosen by syntactic shape:
///
/// * If the string starts with `/`, it is parsed as an [`XmlPath`].
///   This unambiguously identifies XPath-shaped paths because plist key
///   paths never start with `/`.
/// * Otherwise it is parsed as a [`PlistPath`].
///
/// Whether the chosen matcher is valid for the active `language` is
/// validated later at config-eval time.
fn match_path(i: &mut &str) -> ModalResult<Matcher> {
    use winnow::error::ErrMode;
    use winnow::error::FromExternalError;
    let (_, _, raw) = ("path", space1, quoted_string).parse_next(i)?;
    if raw.starts_with('/') {
        match XmlPath::from_str(&raw) {
            Ok(p) => Ok(Matcher::XmlPath(p)),
            Err(e) => Err(ErrMode::Cut(
                winnow::error::ContextError::from_external_error(i, e),
            )),
        }
    } else {
        match PlistPath::from_str(&raw) {
            Ok(p) => Ok(Matcher::PlistPath(p)),
            Err(e) => Err(ErrMode::Cut(
                winnow::error::ContextError::from_external_error(i, e),
            )),
        }
    }
}

/// All valid matchers
fn matcher(i: &mut &str) -> ModalResult<Matcher> {
    alt((
        match_section_regex,
        match_section,
        match_regex,
        match_path,
        match_literal,
    ))
    .parse_next(i)
}

/// The valid matchers for a transformer
fn matcher_transform(i: &mut &str) -> ModalResult<Matcher> {
    alt((match_regex, match_literal)).parse_next(i)
}

/// Quoted string value
fn quoted_string(i: &mut &str) -> ModalResult<String> {
    delimited(
        '"',
        escaped(
            take_till(1.., ['"', '\\']),
            '\\',
            alt(("\\".value("\\"), "\"".value("\""), "n".value("\n"))),
        ),
        '"',
    )
    .parse_next(i)
}

/// Quoted string ending in newline value
fn quoted_string_nl(i: &mut &str) -> ModalResult<String> {
    delimited(
        '"',
        escaped(
            take_till(1.., ['\n', '\r', '\\']),
            '\\',
            alt(("\\".value("\\"), "\"".value("\""), "n".value("\n"))),
        ),
        alt(('\n', '\r')),
    )
    // Trim any trailing ws and "
    .map(|mut v: String| {
        while v.ends_with(['\r', '\n', ' ', '\t']) {
            v.pop();
        }
        if v.ends_with('"') {
            v.pop();
        }
        v
    })
    .parse_next(i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    #[test]
    fn check_quoted_string() {
        let (rem, out) = quoted_string.parse_peek("\"test \\\" \\\\input\"").unwrap();
        assert_eq!(rem, "");
        assert_eq!(out, "test \" \\input");

        let res = quoted_string.parse_peek("\"invalid");
        assert!(res.is_err());
    }

    #[test]
    fn check_quoted_string_nl() {
        let (rem, out) = quoted_string_nl
            .parse_peek("\"test \\\" \\\\input\"\n")
            .unwrap();
        assert_eq!(rem, "");
        assert_eq!(out, "test \" \\input");

        let (rem, out) = quoted_string_nl.parse_peek("\"a \" b\"\n").unwrap();
        assert_eq!(rem, "");
        assert_eq!(out, "a \" b");

        let res = quoted_string_nl.parse_peek("\"invalid");
        assert!(res.is_err());
    }

    #[test]
    fn check_chezmoi_raw_source() {
        let input = concat!(
            r#"source "{{ .chezmoi.sourceDir }}/{{ .chezmoi.sourceFile | trimSuffix ".tmpl" | replace "modify_" "" }}.src.ini""#,
            "\n"
        );
        let (rem, out) = source.parse_peek(input).unwrap();
        assert_eq!(rem, "");
        assert!(
            matches!(out, Directive::Source(s) if s == r#"{{ .chezmoi.sourceDir }}/{{ .chezmoi.sourceFile | trimSuffix ".tmpl" | replace "modify_" "" }}.src.ini"#)
        );

        // Trailing WS should be OK too
        let input = concat!(
            r#"source "{{ .chezmoi.sourceDir }}/{{ .chezmoi.sourceFile | trimSuffix ".tmpl" | replace "modify_" "" }}.src.ini"  "#,
            "\t  \n"
        );
        let (rem, out) = source.parse_peek(input).unwrap();
        assert_eq!(rem, "");
        assert!(
            matches!(out, Directive::Source(s) if s == r#"{{ .chezmoi.sourceDir }}/{{ .chezmoi.sourceFile | trimSuffix ".tmpl" | replace "modify_" "" }}.src.ini"#)
        );
    }

    #[test]
    fn check_matcher() {
        let (rem, out) = matcher.parse_peek("section \"my-section\"").unwrap();
        assert_eq!(rem, "");
        assert!(matches!(out, Matcher::Section(s) if s == "my-section"));

        let (rem, out) = matcher.parse_peek("\"my-section\" \"my-key\"").unwrap();
        assert_eq!(rem, "");
        assert!(matches!(out, Matcher::Literal(s, k) if s == "my-section" && k == "my-key"));

        let (rem, out) = matcher
            .parse_peek("regex \"my-section.*\" \"my-key.*\"")
            .unwrap();
        assert_eq!(rem, "");
        assert!(matches!(out, Matcher::Regex(s, k) if s == "my-section.*" && k == "my-key.*"));
    }

    #[test]
    fn check_transform_arg() {
        let (rem, out) = transform_arg.parse_peek("aaa=\"bbb\"").unwrap();
        assert_eq!(rem, "");
        assert_eq!(out.0, "aaa");
        assert_eq!(out.1, "bbb");
    }

    #[test]
    fn check_transform() {
        // Test winnow parser
        let (rem, out) = transform
            .parse_peek("transform regex \"s.*\" \"k.*\" transform-name arg1=\"a\" arg2=\"b\"")
            .unwrap();

        assert_eq!(rem, "");
        assert_eq!(
            out,
            Directive::Transform(
                Matcher::Regex("s.*".into(), "k.*".into()),
                "transform-name".into(),
                HashMap::from([("arg1".into(), "a".into()), ("arg2".into(), "b".into())]),
            )
        );
    }

    #[test]
    fn check_transform_no_args() {
        // Test winnow parser
        let (rem, out) = transform
            .parse_peek("transform regex \"s.*\" \"k.*\" transform-name")
            .unwrap();

        assert_eq!(rem, "");
        assert_eq!(
            out,
            Directive::Transform(
                Matcher::Regex("s.*".into(), "k.*".into()),
                "transform-name".into(),
                HashMap::new(),
            )
        );
    }

    const FULL_EXAMPLE: &str = indoc! {r#"
    #!/path
    source auto

    ignore section "c"
    ignore "a" "b"
    transform "d" "e" unsorted-list separator=","
    transform "f g" "h" keyring service="srv" user="usr"
    transform "a" "b" kde-shortcut

    ignore regex "a.*" "b.*"
    transform regex "d.*" "e.*" kde-shortcut
    # Random comment

    # Test adding
    add:hide "f g" "h"
    add:remove regex "quux.*" "eh?"
    add:remove section "very secret"
    add:hide section "somewhat secret"
    "#};

    #[test]
    fn test_parse() {
        let out = parse_config.parse(FULL_EXAMPLE).unwrap();

        // Get rid of whitespace, we don't care about those
        let out: Vec<_> = out.into_iter().filter(|v| *v != Directive::WS).collect();

        assert_eq!(
            out,
            vec![
                Directive::SourceAutoEnv,
                Directive::Ignore(Matcher::Section("c".into())),
                Directive::Ignore(Matcher::Literal("a".into(), "b".into())),
                Directive::Transform(
                    Matcher::Literal("d".into(), "e".into()),
                    "unsorted-list".into(),
                    HashMap::from([("separator".into(), ",".into())])
                ),
                Directive::Transform(
                    Matcher::Literal("f g".into(), "h".into()),
                    "keyring".into(),
                    HashMap::from([
                        ("service".into(), "srv".into()),
                        ("user".into(), "usr".into())
                    ])
                ),
                Directive::Transform(
                    Matcher::Literal("a".into(), "b".into()),
                    "kde-shortcut".into(),
                    HashMap::new()
                ),
                Directive::Ignore(Matcher::Regex("a.*".into(), "b.*".into())),
                Directive::Transform(
                    Matcher::Regex("d.*".into(), "e.*".into()),
                    "kde-shortcut".into(),
                    HashMap::new()
                ),
                Directive::AddHide(Matcher::Literal("f g".into(), "h".into())),
                Directive::AddRemove(Matcher::Regex("quux.*".into(), "eh?".into())),
                Directive::AddRemove(Matcher::Section("very secret".into())),
                Directive::AddHide(Matcher::Section("somewhat secret".into())),
            ]
        );
    }

    #[test]
    fn parse_language_ini() {
        let (rem, out) = language.parse_peek("language ini").unwrap();
        assert_eq!(rem, "");
        assert_eq!(out, Directive::Language(Language::Ini));
    }

    #[test]
    fn parse_language_xml() {
        let (rem, out) = language.parse_peek("language xml").unwrap();
        assert_eq!(rem, "");
        assert_eq!(out, Directive::Language(Language::Xml));
    }

    #[test]
    fn parse_language_plist() {
        let (rem, out) = language.parse_peek("language plist").unwrap();
        assert_eq!(rem, "");
        assert_eq!(out, Directive::Language(Language::Plist));
    }

    #[test]
    fn parse_merge_shallow() {
        let (rem, out) = merge.parse_peek("merge shallow").unwrap();
        assert_eq!(rem, "");
        assert_eq!(out, Directive::Merge(MergeMode::Shallow));
    }

    #[test]
    fn parse_merge_deep() {
        let (rem, out) = merge.parse_peek("merge deep").unwrap();
        assert_eq!(rem, "");
        assert_eq!(out, Directive::Merge(MergeMode::Deep));
    }

    #[test]
    fn language_unknown_value_errors() {
        // Top-level parse should fail for an unknown language value.
        let res = parse_config.parse("language toml\n");
        assert!(res.is_err(), "expected parse error, got: {res:?}");
    }

    #[test]
    fn merge_unknown_value_errors() {
        let res = parse_config.parse("merge funky\n");
        assert!(res.is_err(), "expected parse error, got: {res:?}");
    }

    #[test]
    fn parse_transform_plist_join_lines() {
        let (rem, out) = transform_plist
            .parse_peek("transform path \"browserHostWhitelist\" join-lines")
            .unwrap();
        assert_eq!(rem, "");
        match out {
            Directive::PlistTransform { path, kind } => {
                assert_eq!(path.to_string(), "browserHostWhitelist");
                assert_eq!(kind, PlistTransform::JoinLines);
            }
            other => panic!("unexpected directive: {other:?}"),
        }
    }

    #[test]
    fn parse_transform_plist_flatten_keys() {
        let (rem, out) = transform_plist
            .parse_peek(
                "transform path \"shortcuts\" flatten-keys prefix=\"shortcut.\" \
                 json-encode-values",
            )
            .unwrap();
        assert_eq!(rem, "");
        match out {
            Directive::PlistTransform { path, kind } => {
                assert_eq!(path.to_string(), "shortcuts");
                assert_eq!(
                    kind,
                    PlistTransform::FlattenKeys {
                        prefix: "shortcut.".into(),
                        json_encode_values: true,
                        data_encode_values: false,
                    }
                );
            }
            other => panic!("unexpected directive: {other:?}"),
        }
    }

    #[test]
    fn parse_transform_plist_unknown_name_errors() {
        let res = parse_config.parse("transform path \"x\" no-such-thing\n");
        assert!(res.is_err(), "expected parse error, got: {res:?}");
    }

    #[test]
    fn parse_transform_plist_flatten_keys_missing_prefix_errors() {
        let res = parse_config.parse("transform path \"x\" flatten-keys\n");
        assert!(res.is_err(), "expected parse error, got: {res:?}");
    }

    #[test]
    fn parse_transform_plist_join_lines_rejects_flag() {
        let res = parse_config.parse("transform path \"x\" join-lines json-encode-values\n");
        assert!(res.is_err(), "expected parse error, got: {res:?}");
    }

    #[test]
    fn parse_transform_plist_json_encode_rejects_flag() {
        let res = parse_config.parse("transform path \"x\" json-encode json-encode-values\n");
        assert!(res.is_err(), "expected parse error, got: {res:?}");
    }

    #[test]
    fn parse_transform_plist_data_encode() {
        let (rem, out) = transform_plist
            .parse_peek("transform path \"customPrompts\" data-encode")
            .unwrap();
        assert_eq!(rem, "");
        match out {
            Directive::PlistTransform { path, kind } => {
                assert_eq!(path.to_string(), "customPrompts");
                assert_eq!(kind, PlistTransform::DataEncode);
            }
            other => panic!("unexpected directive: {other:?}"),
        }
    }

    #[test]
    fn parse_transform_plist_flatten_keys_data_encode_values() {
        let (rem, out) = transform_plist
            .parse_peek(
                "transform path \"shortcuts\" flatten-keys prefix=\"shortcut.\" \
                 data-encode-values",
            )
            .unwrap();
        assert_eq!(rem, "");
        match out {
            Directive::PlistTransform { kind, .. } => {
                assert_eq!(
                    kind,
                    PlistTransform::FlattenKeys {
                        prefix: "shortcut.".into(),
                        json_encode_values: false,
                        data_encode_values: true,
                    }
                );
            }
            other => panic!("unexpected directive: {other:?}"),
        }
    }

    #[test]
    fn parse_transform_plist_flatten_keys_mutex_flags_rejected() {
        let res = parse_config.parse(
            "transform path \"x\" flatten-keys prefix=\"p.\" json-encode-values \
             data-encode-values\n",
        );
        assert!(res.is_err(), "expected parse error, got: {res:?}");
    }

    #[test]
    fn test_parse_newlines() {
        let out = parse_config
            .parse(
                "source auto\rsource \"foo\"\r\nignore section \"bar\"\nignore section \
                 \"quux\"\r\n",
            )
            .unwrap();

        // Get rid of whitespace, we don't care about those
        let out: Vec<_> = out.into_iter().filter(|v| *v != Directive::WS).collect();

        assert_eq!(
            out,
            vec![
                Directive::SourceAutoEnv,
                Directive::Source("foo".into()),
                Directive::Ignore(Matcher::Section("bar".into())),
                Directive::Ignore(Matcher::Section("quux".into()))
            ]
        );
    }
}
