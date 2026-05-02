//! Describes configuration file format
use self::parser::Directive;
use self::parser::Matcher;
use self::parser::PathMatcher;
pub(crate) use self::parser::PlistTransform;
pub(crate) use self::parser::PlistTypeTag;
use crate::backend::Language;
use crate::path::plist::PlistPath;
use crate::path::xml::XmlPath;
use crate::transforms::Transform;
use anyhow::Context;
use anyhow::anyhow;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use ini_merge::filter::FilterAction;
use ini_merge::filter::FilterActions;
use ini_merge::filter::FilterActionsBuilder;
use ini_merge::mutations::Action;
use ini_merge::mutations::Mutations;
use ini_merge::mutations::MutationsBuilder;
use ini_merge::mutations::SectionAction;
use ini_merge::mutations::transforms;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Debug;
use std::str::FromStr;
use winnow::Parser;

mod parser;
mod script;

pub(crate) use script::InlineFormat;
pub(crate) use script::Script;
pub(crate) use script::detect_inline_format;

/// How a backend should merge source onto the live/system tree.
///
/// Currently only consulted by the plist backend (slice 7+); kept here so the
/// directive can be parsed and threaded through end-to-end now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MergeMode {
    /// Shallow top-level override (default).
    #[default]
    Shallow,
    /// Recursive dict merge; arrays replace wholesale; scalars override.
    Deep,
}

/// Output format for the plist backend. Plist-only directive.
///
/// Defaults to [`PlistOutput::Binary`] which matches what macOS apps and
/// `cfprefsd` write back anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PlistOutput {
    /// Binary plist (`bplist00` magic). Default.
    #[default]
    Binary,
    /// XML plist; useful for human-readable diffs.
    Xml,
}

/// Where to find the source file
#[derive(Debug)]
pub(crate) enum Source {
    /// Specific path for the source file.
    Path(Utf8PathBuf),
    /// Auto locate the source file based on `CHEZMOI_SOURCE_FILE`
    ///
    /// Requires chezmoi 2.46.1 or newer.
    AutoEnv,
    /// Auto locate the source file based on relative path.
    ///
    /// This is currently broken with chezmoi, but needed for integration
    /// tests.
    AutoPath,
    /// Inline source: the body of a single-file modify script.
    ///
    /// The plist/xml backends read the bytes from `Script::body` directly.
    Inline,
}

/// Path-level plist directives (slice 8). The fields are populated only
/// for `language plist`; for other languages they're empty.
#[derive(Debug, Default)]
pub(crate) struct PlistDirectives {
    /// `ignore path "X"` — applied during merge.
    pub(crate) ignore: Vec<PlistPath>,
    /// `remove path "X"` — applied after merge.
    pub(crate) remove: Vec<PlistPath>,
    /// `add:hide path "X"` — applied during re-add filtering.
    pub(crate) add_hide: Vec<PlistPath>,
    /// `add:remove path "X"` — applied during re-add filtering.
    pub(crate) add_remove: Vec<PlistPath>,
    /// `transform path "X" <kind>` — applied to the source tree
    /// pre-merge, in declaration order (slice 9).
    pub(crate) transforms: Vec<(PlistPath, PlistTransform)>,
    /// `set path "X" [<type>] "<literal>"` — slice 12, replace-only.
    /// Applied after merge / remove / ignore handling, before encoding.
    pub(crate) set_path: Vec<(PlistPath, String, Option<PlistTypeTag>)>,
}

/// Path-level XML directives (slices 4–6). Populated only for
/// `language xml`; for other languages the fields are empty.
#[derive(Debug, Default)]
pub(crate) struct XmlDirectives {
    /// `ignore path "X"` — applied during merge: the system span wins.
    pub(crate) ignore: Vec<XmlPath>,
    /// `remove path "X"` — applied during merge: the source span is
    /// deleted.
    pub(crate) remove: Vec<XmlPath>,
    /// `add:hide path "X"` — applied during re-add filtering.
    pub(crate) add_hide: Vec<XmlPath>,
    /// `add:remove path "X"` — applied during re-add filtering.
    pub(crate) add_remove: Vec<XmlPath>,
    /// `set path "X" "<literal>"` — slice 12, replace-only. Applied
    /// during merge: the addressed source span (attribute value or
    /// `text()` content) is replaced with the XML-escaped literal.
    pub(crate) set_path: Vec<(XmlPath, String)>,
}

/// The data from the config file
#[derive(Debug)]
pub(crate) struct Config<ActionType>
where
    ActionType: Debug,
{
    pub(crate) source: Source,
    pub(crate) mutations: ActionType,
    /// Selected language. Defaults to [`Language::Ini`] when omitted.
    pub(crate) language: Language,
    /// Selected merge mode. Defaults to [`MergeMode::Shallow`].
    ///
    /// Currently only consulted by the (not-yet-implemented) plist backend.
    pub(crate) merge_mode: MergeMode,
    /// Plist output format selector (plist backend only).
    pub(crate) output_format: PlistOutput,
    /// Path-level plist directives (slice 8). Empty for non-plist scripts.
    pub(crate) plist: PlistDirectives,
    /// Path-level XML directives (slices 4–6). Empty for non-xml scripts.
    pub(crate) xml: XmlDirectives,
}

impl<ActionType> Config<ActionType>
where
    ActionType: Debug,
{
    /// Compute the source path
    pub(crate) fn source_path(&self, script_path: &Utf8Path) -> anyhow::Result<Cow<'_, Utf8Path>> {
        let extension = self.language.sidecar_extension();
        match self.source {
            Source::Path(ref p) => Ok(Cow::Borrowed(p)),
            Source::AutoEnv => {
                let mut env_path: Utf8PathBuf = std::env::var("CHEZMOI_SOURCE_DIR")
                    .context("CHEZMOI_SOURCE_DIR not set")?
                    .into();
                env_path.push(
                    std::env::var("CHEZMOI_SOURCE_FILE")
                        .context(
                            "Environment variable CHEZMOI_SOURCE_FILE not set, \"source auto\" \
                             not supported (upgrade chezmoi)",
                        )?
                        .as_str(),
                );
                Ok(Cow::Owned(resolve_auto_source(
                    &env_path,
                    extension,
                    self.language,
                )?))
            }
            Source::AutoPath => Ok(Cow::Owned(resolve_auto_source(
                script_path,
                extension,
                self.language,
            )?)),
            Source::Inline => Err(anyhow!(
                "source_path called on inline script; the body of the script is the source"
            )),
        }
    }
}

/// Resolve `source auto-*` for the given language.
///
/// For `language plist`, the canonical sidecar extension is `.src.plist`,
/// but `.src.json` is accepted as an authoring alias. If both files exist
/// the lookup errors per the RFC.
fn resolve_auto_source(
    base_path: &Utf8Path,
    extension: &str,
    language: Language,
) -> anyhow::Result<Utf8PathBuf> {
    let primary = resolve_relative_path(base_path, extension)?;
    if language != Language::Plist {
        return Ok(primary);
    }
    let json_alias = resolve_relative_path(base_path, ".src.json")?;
    let primary_exists = primary.exists();
    let alias_exists = json_alias.exists();
    match (primary_exists, alias_exists) {
        (true, true) => Err(anyhow!(
            "{primary} and {json_alias} both present; remove one"
        )),
        (true, false) => Ok(primary),
        (false, true) => Ok(json_alias),
        (false, false) => Ok(primary),
    }
}

/// Resolve the data path relative to a known script path.
///
/// `extension` includes the leading dot, e.g. `".src.ini"` or `".src.xml"`.
fn resolve_relative_path(script_path: &Utf8Path, extension: &str) -> anyhow::Result<Utf8PathBuf> {
    let script_name = script_path
        .file_name()
        .ok_or_else(|| anyhow!("Failed to extract filename from {script_path}"))?;
    let intermediate_name = script_name.strip_prefix("modify_").unwrap_or(script_name);
    let data_name = intermediate_name
        .strip_suffix(".tmpl")
        .unwrap_or(intermediate_name)
        .to_string()
        + extension;
    Ok(script_path.with_file_name(data_name))
}

/// Create a transformer based on name
fn make_transformer(
    transform: &str,
    args: &HashMap<String, String>,
) -> anyhow::Result<transforms::TransformerDispatch> {
    Transform::from_str(transform)
        .map_err(|err| anyhow!("Invalid transform specified: {transform}: {err}"))?
        .construct(args)
}

/// Look at a parsed [`Script`] and extract just the `language` directive.
///
/// Used by the dispatcher in `lib.rs` and `add.rs` to pick a backend before
/// running the full per-mode parse. Returns [`Language::Ini`] if no
/// `language` directive is present. Errors on duplicates or on a malformed
/// directives section.
///
/// Implementation note: this is a thin wrapper over [`scan_language`], which
/// is also used by [`resolve_meta`] so that duplicate-language detection
/// lives in exactly one place.
pub(crate) fn peek_language(script: &Script) -> anyhow::Result<Language> {
    let result = parser::parse_config
        .parse(script.directives.as_str())
        .map_err(|e| anyhow::format_err!("{e}"))?;

    Ok(scan_language(&result, &script.directives, &script.script_path)?.unwrap_or_default())
}

/// Walk parsed directives looking for `language`, rejecting duplicates.
///
/// Returns `Ok(None)` when no `language` directive is present (caller
/// applies the default). Errors with a `{path}: line N:` prefix pointing at
/// the duplicate occurrence.
fn scan_language(
    directives: &[Directive],
    raw_directives: &str,
    script_path: &Utf8Path,
) -> anyhow::Result<Option<Language>> {
    let mut language: Option<Language> = None;
    let mut seen_count = 0usize;
    for directive in directives {
        if let Directive::Language(lang) = directive {
            seen_count += 1;
            if language.is_some() {
                let l = nth_keyword_line(raw_directives, "language", seen_count).unwrap_or(0);
                return Err(anyhow!(
                    "{script_path}: line {l}: duplicate `language` directives are not allowed"
                ));
            }
            language = Some(*lang);
        }
    }
    Ok(language)
}

/// Resolve the `source`/`language`/`merge` triple, given the parsed
/// directives and whether the script is inline. Used by both
/// [`parse_for_merge`] and [`parse_for_add`].
///
/// `script_path` is threaded through purely for diagnostics: every
/// rejection message is prefixed with `{path}: line N:` so the user can
/// locate the offending directive.
fn resolve_meta(
    directives: &[Directive],
    is_inline: bool,
    script_path: &Utf8Path,
    raw_directives: &str,
) -> anyhow::Result<(Source, Language, MergeMode, PlistOutput)> {
    // `language` is centralised in `scan_language` so that duplicate
    // detection lives in one place. The other directives are scanned
    // here for now (they're not consulted by `peek_language`).
    let language = scan_language(directives, raw_directives, script_path)?;

    let mut source: Option<Source> = None;
    let mut merge_mode: Option<MergeMode> = None;
    let mut output_format: Option<PlistOutput> = None;
    let mut source_count = 0usize;
    let mut merge_count = 0usize;
    let mut output_count = 0usize;

    // We don't have per-directive byte offsets from the parser, so we
    // approximate line numbers by scanning the raw directives text for
    // the Nth occurrence of `language `, `merge `, or `source ` at
    // column 0. This is good enough for diagnostics; it doesn't need to
    // be byte-perfect.
    let line = |keyword: &str, occurrence: usize| -> usize {
        nth_keyword_line(raw_directives, keyword, occurrence).unwrap_or(0)
    };

    for directive in directives {
        match directive {
            // `language` already consumed by `scan_language` above.
            Directive::Language(_) => {}
            Directive::Merge(mode) => {
                merge_count += 1;
                if merge_mode.is_some() {
                    let l = line("merge", merge_count);
                    return Err(anyhow!(
                        "{script_path}: line {l}: duplicate `merge` directives are not allowed"
                    ));
                }
                merge_mode = Some(*mode);
            }
            Directive::Output(fmt) => {
                output_count += 1;
                if output_format.is_some() {
                    let l = line("output", output_count);
                    return Err(anyhow!(
                        "{script_path}: line {l}: duplicate `output` directives are not allowed"
                    ));
                }
                output_format = Some(*fmt);
            }
            Directive::Source(_) | Directive::SourceAutoEnv | Directive::SourceAutoPath => {
                source_count += 1;
                let l = line("source", source_count);
                if is_inline {
                    return Err(anyhow!(
                        "{script_path}: line {l}: inline (single-file) modify scripts cannot \
                         specify a `source` directive: the body of the script is the source"
                    ));
                }
                if source.is_some() {
                    return Err(anyhow!(
                        "{script_path}: line {l}: duplicate `source` directives are not allowed"
                    ));
                }
                source = Some(match directive {
                    Directive::Source(s) => Source::Path(s.clone().into()),
                    Directive::SourceAutoEnv => Source::AutoEnv,
                    Directive::SourceAutoPath => Source::AutoPath,
                    _ => unreachable!(),
                });
            }
            _ => {}
        }
    }

    let source = if is_inline {
        Source::Inline
    } else {
        source.ok_or_else(|| anyhow!("{script_path}: no `source` directive found"))?
    };
    let language = language.unwrap_or_default();
    if output_format.is_some() && language != Language::Plist {
        let l = line("output", output_count);
        return Err(anyhow!(
            "{script_path}: line {l}: `output` directive is only valid for `language plist`"
        ));
    }
    Ok((
        source,
        language,
        merge_mode.unwrap_or_default(),
        output_format.unwrap_or_default(),
    ))
}

/// Find the 1-based line number of the `occurrence`-th occurrence of
/// `keyword` (followed by a space) at column 0 in `text`. Returns `None`
/// if there are fewer than that many occurrences.
fn nth_keyword_line(text: &str, keyword: &str, occurrence: usize) -> Option<usize> {
    let mut found = 0usize;
    for (idx, line) in text.split('\n').enumerate() {
        if line.starts_with(keyword) && line.as_bytes().get(keyword.len()).copied() == Some(b' ') {
            found += 1;
            if found == occurrence {
                return Some(idx + 1);
            }
        }
    }
    None
}

/// Parse directives for operation
pub(crate) fn parse_for_merge(script: &Script) -> anyhow::Result<Config<Mutations>> {
    let result = parser::parse_config
        .parse(script.directives.as_str())
        .map_err(|e| anyhow::format_err!("{e}"))?;

    let (source, language, merge_mode, output_format) = resolve_meta(
        &result,
        script.is_inline(),
        &script.script_path,
        &script.directives,
    )?;
    let mut builder = MutationsBuilder::new();
    let mut plist_d = PlistDirectives::default();
    let mut xml_d = XmlDirectives::default();

    // Build config object
    for directive in result {
        match directive {
            Directive::WS => (),
            // Not relevant for merging (slice-7 — handled by `parse_for_add`).
            Directive::AddRemove(matcher) | Directive::AddHide(matcher) => {
                // Path matchers must still be validated at config-parse time
                // even though they don't affect merge — so a typo is caught
                // even if you only ever run `process`.
                if let Matcher::PlistPath(p) = &matcher {
                    validate_plist_path(p, "add:hide/add:remove", language)?;
                } else if let Matcher::XmlPath(p) = &matcher {
                    validate_xml_path(p, "add:hide/add:remove", language)?;
                }
            }
            // Already consumed by `resolve_meta`.
            Directive::Source(_)
            | Directive::SourceAutoEnv
            | Directive::SourceAutoPath
            | Directive::Language(_)
            | Directive::Merge(_)
            | Directive::Output(_) => (),
            Directive::Ignore(Matcher::Section(section)) => {
                builder.add_section_literal_action(section, SectionAction::Ignore);
            }
            Directive::Ignore(Matcher::SectionRegex(section)) => {
                builder.add_section_regex_action(section, SectionAction::Ignore);
            }
            Directive::Ignore(Matcher::PlistPath(p)) => {
                validate_plist_path(&p, "ignore", language)?;
                plist_d.ignore.push(p);
            }
            Directive::Ignore(Matcher::XmlPath(p)) => {
                validate_xml_path(&p, "ignore", language)?;
                xml_d.ignore.push(p);
            }
            Directive::Ignore(matcher) => {
                add_merge_action(&mut builder, matcher, Action::Ignore);
            }
            Directive::Transform(matcher, transform, args) => {
                let t = make_transformer(&transform, &args)?;
                add_merge_action(&mut builder, matcher, Action::Transform(t));
            }
            Directive::PlistTransform { path, kind } => {
                if language != Language::Plist {
                    return Err(anyhow!(
                        "`transform path` is only valid for `language plist` (used in `transform \
                         path \"{path}\"`)"
                    ));
                }
                validate_plist_path(&path, "transform", language)?;
                plist_d.transforms.push((path, kind));
            }
            Directive::Set {
                section,
                key,
                value,
                separator,
            } => {
                // Set is a transform under the hood, but needs special support
                // to enable adding lines that don't exist. This is handled inside
                // the mutations builder.
                builder.add_setter(
                    section,
                    key,
                    &value,
                    &separator.unwrap_or_else(|| " = ".to_string()),
                );
            }
            Directive::SetPath {
                path,
                value,
                type_tag,
            } => {
                let (xml_path, plist_path) = match path {
                    PathMatcher::Xml(p) => (Some(p), None),
                    PathMatcher::Plist(p) => (None, Some(p)),
                };
                if let Some(p) = xml_path {
                    if language != Language::Xml {
                        return Err(anyhow!(
                            "matcher `path \"{p}\"` (XML path syntax) is only valid for \
                             `language xml` (used in `set path`)"
                        ));
                    }
                    if type_tag.is_some() {
                        return Err(anyhow!(
                            "`set path` for XML does not accept a type tag (used in `set path \
                             \"{p}\"`)"
                        ));
                    }
                    xml_d.set_path.push((p, value));
                } else if let Some(p) = plist_path {
                    validate_plist_path(&p, "set", language)?;
                    plist_d.set_path.push((p, value, type_tag));
                }
            }
            Directive::Remove(Matcher::Section(section)) => {
                builder.add_section_literal_action(section, SectionAction::Delete);
            }
            Directive::Remove(Matcher::PlistPath(p)) => {
                validate_plist_path(&p, "remove", language)?;
                plist_d.remove.push(p);
            }
            Directive::Remove(Matcher::XmlPath(p)) => {
                validate_xml_path(&p, "remove", language)?;
                xml_d.remove.push(p);
            }
            Directive::Remove(matcher) => {
                add_merge_action(&mut builder, matcher, Action::Delete);
            }
            Directive::NoWarnMultipleKeyMatches => {
                builder.warn_on_multiple_matches(false);
            }
        }
    }

    // `set path` runs after `ignore path` is restored in the merge
    // pipeline, so a directive of the form
    //
    //     ignore path "X"
    //     set path "X" "value"
    //
    // would silently let the `set` win. The author almost certainly did
    // not intend both — flag it as a config-time error.
    reject_ignore_set_overlap(&plist_d, &xml_d)?;

    Ok(Config {
        source,
        mutations: builder.build()?,
        language,
        merge_mode,
        output_format,
        plist: plist_d,
        xml: xml_d,
    })
}

/// Reject scripts that name the same path on both `ignore path` and
/// `set path` (per language). The two directives operate at different
/// stages of the merge pipeline; `set` would silently override `ignore`,
/// which is almost never what the author intended.
fn reject_ignore_set_overlap(
    plist_d: &PlistDirectives,
    xml_d: &XmlDirectives,
) -> anyhow::Result<()> {
    for ignore in &plist_d.ignore {
        for (set, _, _) in &plist_d.set_path {
            if ignore == set {
                return Err(anyhow!(
                    "path `{ignore}` appears in both `ignore path` and `set path`; \
                     `set` runs after `ignore` and would silently override it. Drop \
                     one of the two directives."
                ));
            }
        }
    }
    for ignore in &xml_d.ignore {
        for (set, _) in &xml_d.set_path {
            if ignore == set {
                return Err(anyhow!(
                    "path `{ignore}` appears in both `ignore path` and `set path`; \
                     `set` runs after `ignore` and would silently override it. Drop \
                     one of the two directives."
                ));
            }
        }
    }
    Ok(())
}

/// Parse directives for operation
pub(crate) fn parse_for_add(script: &Script) -> Result<Config<FilterActions>, anyhow::Error> {
    let result = parser::parse_config
        .parse(script.directives.as_str())
        .map_err(|e| anyhow::format_err!("{e}"))?;

    let (source, language, merge_mode, output_format) = resolve_meta(
        &result,
        script.is_inline(),
        &script.script_path,
        &script.directives,
    )?;
    let mut builder = FilterActionsBuilder::new();
    let mut plist_d = PlistDirectives::default();
    let mut xml_d = XmlDirectives::default();

    // Build config object
    for directive in result {
        match directive {
            Directive::WS => (),
            Directive::AddHide(Matcher::PlistPath(p)) => {
                validate_plist_path(&p, "add:hide", language)?;
                plist_d.add_hide.push(p);
            }
            Directive::AddHide(Matcher::XmlPath(p)) => {
                validate_xml_path(&p, "add:hide", language)?;
                xml_d.add_hide.push(p);
            }
            Directive::AddHide(matcher) => {
                add_filter_action(&mut builder, matcher, FilterAction::Replace("HIDDEN"));
            }
            Directive::AddRemove(Matcher::PlistPath(p)) => {
                validate_plist_path(&p, "add:remove", language)?;
                plist_d.add_remove.push(p);
            }
            Directive::AddRemove(Matcher::XmlPath(p)) => {
                validate_xml_path(&p, "add:remove", language)?;
                xml_d.add_remove.push(p);
            }
            Directive::Ignore(Matcher::PlistPath(p)) => {
                // The RFC's "Re-add filtering algorithm" mandates removal
                // of `ignore path` targets when filtering. Without this
                // step a secret captured by `ignore path` (e.g. a
                // `Password`) would slip through `chezmoi re-add` because
                // the merge-side skip-write does not run there.
                validate_plist_path(&p, "ignore", language)?;
                plist_d.ignore.push(p);
            }
            Directive::Ignore(Matcher::XmlPath(p)) => {
                // Same reasoning as the plist branch above: the re-add
                // filter must drop the addressed span so that ignored
                // values do not leak through.
                validate_xml_path(&p, "ignore", language)?;
                xml_d.ignore.push(p);
            }
            Directive::AddRemove(matcher) | Directive::Ignore(matcher) => {
                add_filter_action(&mut builder, matcher, FilterAction::Remove);
            }
            // Already consumed by `resolve_meta`.
            Directive::Source(_)
            | Directive::SourceAutoEnv
            | Directive::SourceAutoPath
            | Directive::Language(_)
            | Directive::Merge(_)
            | Directive::Output(_) => (),
            // Not relevant for filtering
            Directive::Set { .. } => (),
            Directive::SetPath {
                path,
                type_tag,
                value: _,
            } => {
                // `set path` does not participate in re-add filtering, but
                // we still validate at parse time so a typo or wrong-language
                // matcher fails early.
                match path {
                    PathMatcher::Xml(p) => {
                        if language != Language::Xml {
                            return Err(anyhow!(
                                "matcher `path \"{p}\"` (XML path syntax) is only valid for \
                                 `language xml` (used in `set path`)"
                            ));
                        }
                        if type_tag.is_some() {
                            return Err(anyhow!(
                                "`set path` for XML does not accept a type tag (used in `set \
                                 path \"{p}\"`)"
                            ));
                        }
                    }
                    PathMatcher::Plist(p) => {
                        validate_plist_path(&p, "set", language)?;
                    }
                }
            }
            Directive::Transform(_, _, _) => (),
            Directive::PlistTransform { path, .. } => {
                // Transforms only apply to source on the merge path; on the
                // re-add (filter) path the live file is read as-is. We still
                // validate at parse time so a typo doesn't slip through.
                if language != Language::Plist {
                    return Err(anyhow!(
                        "`transform path` is only valid for `language plist` (used in `transform \
                         path \"{path}\"`)"
                    ));
                }
                validate_plist_path(&path, "transform", language)?;
            }
            Directive::Remove(matcher) => {
                if let Matcher::PlistPath(p) = &matcher {
                    validate_plist_path(p, "remove", language)?;
                } else if let Matcher::XmlPath(p) = &matcher {
                    validate_xml_path(p, "remove", language)?;
                }
            }
            Directive::NoWarnMultipleKeyMatches => {
                builder.warn_on_multiple_matches(false);
            }
        }
    }

    Ok(Config {
        source,
        mutations: builder.build()?,
        language,
        merge_mode,
        output_format,
        plist: plist_d,
        xml: xml_d,
    })
}

/// Validate an XML path matcher at config-parse time: only valid for
/// `language xml`.
fn validate_xml_path(path: &XmlPath, directive: &str, language: Language) -> anyhow::Result<()> {
    if language != Language::Xml {
        return Err(anyhow!(
            "matcher `path \"{path}\"` (XML path syntax) is only valid for `language xml` \
             (used in `{directive} path`)"
        ));
    }
    Ok(())
}

/// Validate a plist path matcher at config-parse time:
///
/// * Reject `path` matchers in `language ini`.
/// * Reject `[*]` (multi-match) on directives that require a single
///   target — `transform` (slice 9) and the eventual `set` (slice 12).
///   Predicates are single-match and are always allowed.
fn validate_plist_path(
    path: &PlistPath,
    directive: &str,
    language: Language,
) -> anyhow::Result<()> {
    if language == Language::Ini {
        return Err(anyhow!(
            "matcher `path` is not valid for language ini (used in `{directive} path \"{path}\"`)"
        ));
    }
    use crate::path::plist::Segment;
    let single_target = matches!(directive, "transform" | "set");
    if single_target {
        for seg in &path.segments {
            if matches!(seg, Segment::Wildcard) {
                return Err(anyhow!(
                    "`{directive} path` requires a single target; `[*]` is not allowed \
                     (used in `{directive} path \"{path}\"`)"
                ));
            }
        }
    }
    Ok(())
}

fn add_merge_action(builder: &mut MutationsBuilder, matcher: Matcher, action: Action) {
    match matcher {
        Matcher::Section(_) => panic!("Section match not valid in add_merge_action()"),
        Matcher::SectionRegex(_) => panic!("SectionRegex match not valid in add_merge_action()"),
        Matcher::PlistPath(_) => panic!("PlistPath match handled separately in add_merge_action()"),
        Matcher::XmlPath(_) => panic!("XmlPath match handled separately in add_merge_action()"),
        Matcher::Literal(section, key) => {
            builder.add_literal_action(section, &key, action);
        }
        Matcher::Regex(section, key) => {
            builder.add_regex_action(&section, &key, action);
        }
    }
}

fn add_filter_action(builder: &mut FilterActionsBuilder, matcher: Matcher, action: FilterAction) {
    match matcher {
        Matcher::Section(section) => {
            builder.add_section_literal_action(section, action);
        }
        Matcher::SectionRegex(section) => {
            builder.add_section_regex_action(section, action);
        }
        Matcher::PlistPath(_) => {
            panic!("PlistPath match handled separately in add_filter_action()")
        }
        Matcher::XmlPath(_) => {
            panic!("XmlPath match handled separately in add_filter_action()")
        }
        Matcher::Literal(section, key) => {
            builder.add_literal_action(section, &key, action);
        }
        Matcher::Regex(section, key) => {
            builder.add_regex_action(&section, &key, action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    fn parse(script_text: &str) -> Script {
        Script::parse(script_text.as_bytes(), Utf8Path::new("test://script")).unwrap()
    }

    fn try_parse(script_text: &str) -> anyhow::Result<Script> {
        Script::parse(script_text.as_bytes(), Utf8Path::new("test://script"))
    }

    #[test]
    fn language_omitted_defaults_to_ini() {
        let script = parse(indoc! {r#"
            source auto
        "#});
        let cfg = parse_for_merge(&script).unwrap();
        assert_eq!(cfg.language, Language::Ini);
    }

    #[test]
    fn language_ini_explicit() {
        let script = parse(indoc! {r#"
            language ini
            source auto
        "#});
        let cfg = parse_for_merge(&script).unwrap();
        assert_eq!(cfg.language, Language::Ini);
    }

    #[test]
    fn duplicate_language_rejected() {
        let script = parse(indoc! {r#"
            language ini
            language xml
            source auto
        "#});
        let err = parse_for_merge(&script).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("duplicate `language`"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn auto_source_extension_xml() {
        let script = parse(indoc! {r#"
            language xml
            source auto-path
        "#});
        let cfg = parse_for_merge(&script).unwrap();
        assert_eq!(cfg.language, Language::Xml);
        let path = cfg
            .source_path(Utf8Path::new("/dotfiles/modify_foo.xml"))
            .unwrap();
        assert_eq!(path.as_ref(), Utf8Path::new("/dotfiles/foo.xml.src.xml"));
    }

    #[test]
    fn auto_source_extension_plist() {
        let script = parse(indoc! {r#"
            language plist
            source auto-path
        "#});
        let cfg = parse_for_merge(&script).unwrap();
        assert_eq!(cfg.language, Language::Plist);
        let path = cfg
            .source_path(Utf8Path::new("/dotfiles/modify_foo.plist"))
            .unwrap();
        assert_eq!(
            path.as_ref(),
            Utf8Path::new("/dotfiles/foo.plist.src.plist")
        );
    }

    #[test]
    fn xml_path_matcher_rejected_for_plist_language() {
        // An XPath-shaped string in `language plist` mode parses as
        // `XmlPath` but is rejected at config-eval because the language
        // is plist.
        let script = parse(indoc! {r#"
            language plist
            source auto-path
            ignore path "/Element/@attr"
        "#});
        let err = parse_for_merge(&script).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("language xml"), "unexpected error: {msg}");
    }

    #[test]
    fn duplicate_merge_rejected() {
        let script = parse(indoc! {r#"
            merge shallow
            merge deep
            source auto
        "#});
        let err = parse_for_merge(&script).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("duplicate `merge`"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("test://script"), "should include path: {msg}");
    }

    #[test]
    fn language_repeated_with_same_value_rejected() {
        // Two `language ini` directives — same value, but still a
        // duplicate. `peek_language` and `resolve_meta` both reject.
        let script = parse(indoc! {r#"
            language ini
            language ini
            source auto
        "#});
        let err = peek_language(&script).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("duplicate `language`"),
            "peek_language msg: {msg}"
        );
        let err = parse_for_merge(&script).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("duplicate `language`"),
            "parse_for_merge msg: {msg}"
        );
    }

    #[test]
    fn inline_with_merge_deep_threads_through() {
        // `language plist` + `merge deep` + JSON body. The plist backend
        // would reject the body at process time (slice 7+), but the
        // *config* parses successfully and `MergeMode::Deep` flows
        // through into the resulting `Config`.
        let script = parse(indoc! {r#"
            language plist
            merge deep
            ---
            {"a": 1}
        "#});
        let cfg = parse_for_merge(&script).unwrap();
        assert_eq!(cfg.language, Language::Plist);
        assert_eq!(cfg.merge_mode, MergeMode::Deep);
        assert!(matches!(cfg.source, Source::Inline));
    }

    #[test]
    fn config_ignore_and_set_path_on_same_path_errors() {
        // Round-3 regression: `set path` runs after `ignore path`'s
        // restore step in the merge pipeline, so naming the same path
        // on both directives silently lets `set` win. Almost certainly
        // not what the author intended; reject at config-eval.
        let script = parse(indoc! {r#"
            language plist
            source auto-path
            ignore path "X"
            set path "X" string "v"
        "#});
        let err = parse_for_merge(&script).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ignore path") && msg.contains("set path"),
            "expected directive overlap error, got: {msg}"
        );
    }

    #[test]
    fn config_ignore_and_set_path_on_same_xml_path_errors() {
        // Same as the plist case, but for `language xml`.
        let script = parse(indoc! {r#"
            language xml
            source auto-path
            ignore path "/cfg/window/@width"
            set path "/cfg/window/@width" "1920"
        "#});
        let err = parse_for_merge(&script).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ignore path") && msg.contains("set path"),
            "expected directive overlap error, got: {msg}"
        );
    }

    #[test]
    fn inline_with_explicit_source_auto_rejected() {
        // Inline scripts cannot specify `source auto-path` (or any source
        // directive); the body of the script is the source.
        let err = try_parse(indoc! {r#"
            language ini
            source auto-path
            ---
            [section]
            key=val
        "#})
        .and_then(|s| parse_for_merge(&s))
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("inline") && msg.contains("source"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("test://script"), "should include path: {msg}");
    }
}
