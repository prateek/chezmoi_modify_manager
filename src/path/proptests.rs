//! Property-based tests for the XML and plist path grammars.
//!
//! For any randomly-generated `XmlPath` or `PlistPath`, parsing the result
//! of `Display` must reproduce the original AST. The generators emit values
//! restricted to the supported subset of each grammar; the existing
//! `FromStr` implementation is the source of truth for what is valid.

use super::plist::PlistPath;
use super::plist::Segment as PlistSegment;
use super::xml::Step as XmlStep;
use super::xml::Target as XmlTarget;
use super::xml::XmlPath;
use proptest::collection::vec;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// Identifier matching `[A-Za-z_][A-Za-z0-9_-]*` — the unquoted-name
/// alphabet shared by both grammars. Kept short to keep counterexamples
/// readable.
fn ident_strategy() -> impl Strategy<Value = String> {
    "[A-Za-z_][A-Za-z0-9_\\-]{0,4}".prop_map(String::from)
}

/// XML element name with an optional `prefix:` namespace, matching the
/// grammar's `Identifier | Prefix:Identifier` rule. Plain identifiers
/// remain in the mix so we still cover the unprefixed path.
fn prefixed_ident_strategy() -> impl Strategy<Value = String> {
    (ident_strategy(), ident_strategy()).prop_map(|(prefix, local)| format!("{prefix}:{local}"))
}

/// Predicate-value alphabet. Includes characters that exercise the escape
/// machinery (`"`, `'`, and `\`) so that the round-trip property is
/// sensitive to escape bugs in either quote style.
fn predicate_value_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-zA-Z0-9 _\"'\\\\]{0,6}")
        .expect("static regex")
        .prop_map(String::from)
}

fn xml_step_strategy() -> impl Strategy<Value = XmlStep> {
    let name_strategy = prop_oneof![ident_strategy(), prefixed_ident_strategy()];
    (
        name_strategy,
        proptest::option::of((ident_strategy(), predicate_value_strategy())),
    )
        .prop_map(|(name, attr_predicate)| XmlStep {
            name,
            attr_predicate,
        })
}

fn xml_target_strategy() -> impl Strategy<Value = XmlTarget> {
    prop_oneof![
        Just(XmlTarget::Element),
        ident_strategy().prop_map(XmlTarget::Attribute),
        Just(XmlTarget::Text),
    ]
}

fn xml_path_strategy() -> impl Strategy<Value = XmlPath> {
    (vec(xml_step_strategy(), 1..=5), xml_target_strategy())
        .prop_map(|(steps, target)| XmlPath { steps, target })
}

fn plist_segment_strategy() -> impl Strategy<Value = PlistSegment> {
    prop_oneof![
        ident_strategy().prop_map(PlistSegment::Key),
        (0usize..32).prop_map(PlistSegment::Index),
        Just(PlistSegment::Wildcard),
        (ident_strategy(), predicate_value_strategy())
            .prop_map(|(key, value)| { PlistSegment::Predicate { key, value } }),
    ]
}

fn plist_path_strategy() -> impl Strategy<Value = PlistPath> {
    // First segment must be a key (the grammar rejects bare `[N]`).
    (ident_strategy(), vec(plist_segment_strategy(), 0..=4)).prop_map(|(first, rest)| {
        let mut segments = Vec::with_capacity(1 + rest.len());
        segments.push(PlistSegment::Key(first));
        segments.extend(rest);
        PlistPath { segments }
    })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: crate::test_support::PROPTEST_CASES,
        // We rely on seed-based reproducibility, not the persistence file.
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config())]

    /// `parse(display(p)) == p` for any well-formed `XmlPath`.
    #[test]
    fn xml_path_display_parse_round_trip(p in xml_path_strategy()) {
        let rendered = p.to_string();
        let reparsed: XmlPath = rendered
            .parse()
            .map_err(|e| TestCaseError::fail(format!("parse failed for {rendered:?}: {e}")))?;
        prop_assert_eq!(p, reparsed);
    }

    /// `parse(display(p)) == p` for any well-formed `PlistPath`.
    #[test]
    fn plist_path_display_parse_round_trip(p in plist_path_strategy()) {
        let rendered = p.to_string();
        let reparsed: PlistPath = rendered
            .parse()
            .map_err(|e| TestCaseError::fail(format!("parse failed for {rendered:?}: {e}")))?;
        prop_assert_eq!(p, reparsed);
    }
}
