//! Unit tests for the plist backend.

use super::*;
use crate::config::Script;
use camino::Utf8Path;
use indoc::indoc;
use plist::Dictionary;
use plist::Value;
use pretty_assertions::assert_eq;

fn parse_script(text: &str) -> Script {
    Script::parse(text.as_bytes(), Utf8Path::new("modify_test.plist.tmpl")).unwrap()
}

fn dict_from(pairs: &[(&str, Value)]) -> Dictionary {
    let mut d = Dictionary::new();
    for (k, v) in pairs {
        d.insert((*k).to_owned(), v.clone());
    }
    d
}

// ---- merge_shallow ---------------------------------------------------------

#[test]
fn merge_shallow_overrides_top_level_keys() {
    let source = dict_from(&[
        ("a", Value::Integer(1.into())),
        ("b", Value::Integer(2.into())),
    ]);
    let mut live = dict_from(&[
        ("a", Value::Integer(10.into())),
        ("c", Value::Integer(30.into())),
    ]);
    merge::merge_shallow(&mut live, &source, &[]);

    // Values: a is overridden, c is preserved, b is added.
    assert_eq!(live.get("a"), Some(&Value::Integer(1.into())));
    assert_eq!(live.get("b"), Some(&Value::Integer(2.into())));
    assert_eq!(live.get("c"), Some(&Value::Integer(30.into())));

    // Order: existing key positions preserved, new keys appended.
    let order: Vec<&str> = live.keys().map(String::as_str).collect();
    assert_eq!(order, vec!["a", "c", "b"]);
}

#[test]
fn merge_shallow_replaces_nested_dicts_wholesale() {
    let source_inner = dict_from(&[("p", Value::Integer(1.into()))]);
    let live_inner = dict_from(&[("q", Value::Integer(2.into()))]);
    let source = dict_from(&[("x", Value::Dictionary(source_inner))]);
    let mut live = dict_from(&[("x", Value::Dictionary(live_inner))]);

    merge::merge_shallow(&mut live, &source, &[]);

    let Some(Value::Dictionary(inner)) = live.get("x") else {
        panic!("x must be a dict");
    };
    assert_eq!(inner.get("p"), Some(&Value::Integer(1.into())));
    // Inner `q` is gone — shallow replaces wholesale.
    assert!(inner.get("q").is_none());
}

// ---- merge_deep ------------------------------------------------------------

#[test]
fn merge_deep_recurses_dicts() {
    let source_inner = dict_from(&[("p", Value::Integer(1.into()))]);
    let live_inner = dict_from(&[("q", Value::Integer(2.into()))]);
    let source = dict_from(&[("x", Value::Dictionary(source_inner))]);
    let mut live = dict_from(&[("x", Value::Dictionary(live_inner))]);

    merge::merge_deep(&mut live, &source, &[]).unwrap();

    let Some(Value::Dictionary(inner)) = live.get("x") else {
        panic!("x must be a dict");
    };
    assert_eq!(inner.get("p"), Some(&Value::Integer(1.into())));
    assert_eq!(inner.get("q"), Some(&Value::Integer(2.into())));
}

#[test]
fn merge_deep_arrays_replace() {
    let source = dict_from(&[("x", Value::Array(vec![Value::Integer(9.into())]))]);
    let mut live = dict_from(&[(
        "x",
        Value::Array(vec![
            Value::Integer(1.into()),
            Value::Integer(2.into()),
            Value::Integer(3.into()),
        ]),
    )]);

    merge::merge_deep(&mut live, &source, &[]).unwrap();

    let Some(Value::Array(arr)) = live.get("x") else {
        panic!("x must be an array");
    };
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0], Value::Integer(9.into()));
}

#[test]
fn merge_deep_scalar_overrides() {
    let source = dict_from(&[("x", Value::String("new".into()))]);
    let mut live = dict_from(&[("x", Value::String("old".into()))]);
    merge::merge_deep(&mut live, &source, &[]).unwrap();
    assert_eq!(live.get("x"), Some(&Value::String("new".into())));
}

// ---- JSON → plist conversion ----------------------------------------------

#[test]
fn json_to_plist_value_round_trip() {
    let json = serde_json::json!({
        "s": "hello",
        "i": 42,
        "b": true,
        "arr": [1, 2, 3],
        "obj": {"inner": "yes"},
    });
    let value = json::json_to_plist(&json).unwrap();

    // Encode as XML, decode again, expect logical equality.
    let mut buf = Vec::new();
    value.to_writer_xml(&mut buf).unwrap();
    let decoded = Value::from_reader(std::io::Cursor::new(&buf)).unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn json_null_rejected() {
    let json = serde_json::json!({"x": null});
    let err = json::json_to_plist(&json).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("null"), "msg={msg}");
}

#[test]
fn json_float_becomes_real() {
    let json = serde_json::json!(3.5);
    let value = json::json_to_plist(&json).unwrap();
    assert!(matches!(value, Value::Real(_)));
}

// ---- end-to-end: inline body ----------------------------------------------

fn run_process(script_text: &str, live_bytes: &[u8]) -> Vec<u8> {
    let script = parse_script(script_text);
    let backend = PlistBackend;
    let mut stdin = std::io::Cursor::new(live_bytes.to_vec());
    let mut stdout = Vec::<u8>::new();
    backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap();
    stdout
}

fn xml_plist_dict(pairs: &[(&str, Value)]) -> Vec<u8> {
    let mut buf = Vec::new();
    Value::Dictionary(dict_from(pairs))
        .to_writer_xml(&mut buf)
        .unwrap();
    buf
}

fn binary_plist_dict(pairs: &[(&str, Value)]) -> Vec<u8> {
    let mut buf = Vec::new();
    Value::Dictionary(dict_from(pairs))
        .to_writer_binary(&mut buf)
        .unwrap();
    buf
}

#[test]
fn inline_json_body_merges_correctly() {
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        ---
        {"themeId": "tokyo-night", "timeoutSeconds": 60}
    "#};
    let live = xml_plist_dict(&[
        ("themeId", Value::String("default".into())),
        ("otherKey", Value::Boolean(true)),
    ]);

    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(
        dict.get("themeId"),
        Some(&Value::String("tokyo-night".into()))
    );
    assert_eq!(dict.get("otherKey"), Some(&Value::Boolean(true)));
    assert_eq!(dict.get("timeoutSeconds"), Some(&Value::Integer(60.into())));
}

#[test]
fn binary_plist_input_decodes() {
    let script = indoc! {r#"
        language plist
        ---
        {"a": 1}
    "#};
    let live = binary_plist_dict(&[
        ("a", Value::Integer(0.into())),
        ("b", Value::Integer(2.into())),
    ]);
    let out = run_process(script, &live);
    // Default output is binary — magic should be present.
    assert!(out.starts_with(b"bplist00"), "expected bplist00 magic");
    // Decode to confirm merge applied.
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("a"), Some(&Value::Integer(1.into())));
    assert_eq!(dict.get("b"), Some(&Value::Integer(2.into())));
}

#[test]
fn output_xml_produces_xml() {
    let script = indoc! {r#"
        language plist
        output xml
        ---
        {"a": 1}
    "#};
    let live = xml_plist_dict(&[("a", Value::Integer(0.into()))]);
    let out = run_process(script, &live);
    let s = std::str::from_utf8(&out).unwrap();
    assert!(s.contains("<?xml"), "expected XML output, got: {s}");
    assert!(s.contains("<plist"), "expected XML plist tag, got: {s}");
}

#[test]
fn output_binary_default_starts_with_bplist00() {
    let script = indoc! {r#"
        language plist
        ---
        {"a": 1}
    "#};
    let live = xml_plist_dict(&[("a", Value::Integer(0.into()))]);
    let out = run_process(script, &live);
    assert!(
        out.starts_with(b"bplist00"),
        "expected bplist00 magic, got first bytes: {:?}",
        &out[..out.len().min(16)]
    );
}

// ---- sidecar resolution ----------------------------------------------------

#[test]
fn sidecar_json_alias() {
    // Build a temp dir layout with only `.src.json` present and confirm the
    // resolver picks it up.
    let dir = tempfile::tempdir().unwrap();
    let dir_path = camino::Utf8Path::from_path(dir.path()).unwrap();
    let script_path = dir_path.join("modify_foo.plist.tmpl");
    let json_path = dir_path.join("foo.plist.src.json");
    std::fs::write(&script_path, "language plist\nsource auto-path\n").unwrap();
    std::fs::write(&json_path, r#"{"a": 1}"#).unwrap();

    let raw = std::fs::read(&script_path).unwrap();
    let script = Script::parse(&raw, &script_path).unwrap();
    let cfg = config::parse_for_merge(&script).unwrap();
    let resolved = cfg.source_path(&script_path).unwrap();
    assert_eq!(resolved.as_ref(), json_path.as_path());
}

#[test]
fn sidecar_both_plist_and_json_errors() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = camino::Utf8Path::from_path(dir.path()).unwrap();
    let script_path = dir_path.join("modify_foo.plist.tmpl");
    let plist_path = dir_path.join("foo.plist.src.plist");
    let json_path = dir_path.join("foo.plist.src.json");
    std::fs::write(&script_path, "language plist\nsource auto-path\n").unwrap();
    std::fs::write(&plist_path, b"<plist><dict/></plist>").unwrap();
    std::fs::write(&json_path, r#"{}"#).unwrap();

    let raw = std::fs::read(&script_path).unwrap();
    let script = Script::parse(&raw, &script_path).unwrap();
    let cfg = config::parse_for_merge(&script).unwrap();
    let err = cfg.source_path(&script_path).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("both present") && msg.contains("remove one"),
        "msg={msg}"
    );
}

// ---- output directive validation ------------------------------------------

#[test]
fn output_directive_rejected_for_ini() {
    let script = parse_script(indoc! {r#"
        language ini
        output xml
        source auto-path
    "#});
    let err = config::parse_for_merge(&script).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("`output`") && msg.contains("plist"),
        "msg={msg}"
    );
}

#[test]
fn duplicate_output_rejected() {
    let script = parse_script(indoc! {r#"
        language plist
        output xml
        output binary
        ---
        {}
    "#});
    let err = config::parse_for_merge(&script).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("duplicate `output`"),
        "msg={msg}"
    );
}

// ---- path-level directives (slice 8) -------------------------------------

#[test]
fn ignore_path_skips_top_level_key_during_shallow_merge() {
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "timeoutSeconds"
        ---
        {"themeId": "tokyo-night", "timeoutSeconds": 60}
    "#};
    let live = xml_plist_dict(&[
        ("themeId", Value::String("default".into())),
        ("timeoutSeconds", Value::Integer(120.into())),
    ]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(
        dict.get("themeId"),
        Some(&Value::String("tokyo-night".into()))
    );
    // The live value (120) is preserved, not overwritten by source (60).
    assert_eq!(
        dict.get("timeoutSeconds"),
        Some(&Value::Integer(120.into()))
    );
}

#[test]
fn ignore_path_missing_in_source_errors() {
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "noSuchKey"
        ---
        {"a": 1}
    "#});
    let backend = PlistBackend;
    let mut stdin = std::io::Cursor::new(xml_plist_dict(&[("a", Value::Integer(0.into()))]));
    let mut stdout: Vec<u8> = vec![];
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing key `noSuchKey`"),
        "unexpected error: {msg}"
    );
}

#[test]
fn remove_path_deletes_top_level_key() {
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        remove path "RecentDocuments"
        ---
        {"a": 1}
    "#};
    let live = xml_plist_dict(&[
        ("a", Value::Integer(0.into())),
        (
            "RecentDocuments",
            Value::Array(vec![Value::String("/tmp/foo".into())]),
        ),
    ]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("a"), Some(&Value::Integer(1.into())));
    assert!(dict.get("RecentDocuments").is_none());
}

#[test]
fn ignore_path_for_ini_rejected_at_parse_time() {
    let script = parse_script(indoc! {r#"
        language ini
        source auto-path
        ignore path "X"
    "#});
    let err = config::parse_for_merge(&script).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("matcher `path` is not valid for language ini"),
        "unexpected error: {msg}"
    );
}

#[test]
fn wildcard_selector_rejected_for_transform() {
    // Slice 10 lifts wildcard for ignore/add:hide/add:remove/remove. It
    // remains rejected for `transform` (single-target) and the future
    // `set` directive.
    let script = parse_script(indoc! {r#"
        language plist
        transform path "Accounts[*].Password" json-encode
        ---
        {}
    "#});
    let err = config::parse_for_merge(&script).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("requires a single target") && msg.contains("`[*]`"),
        "unexpected error: {msg}"
    );
}

#[test]
fn predicate_selector_accepted_at_parse_time() {
    // Predicates are single-match and therefore valid wherever paths
    // are. Parse should succeed; the integration test exercises the
    // runtime semantics.
    let script = parse_script(indoc! {r#"
        language plist
        ignore path "Accounts[name=\"main\"].Password"
        ---
        {}
    "#});
    config::parse_for_merge(&script).unwrap();
}

#[test]
fn add_hide_path_replaces_string_with_hidden() {
    let script = parse_script(indoc! {r#"
        language plist
        add:hide path "Accounts.Password"
        ---
        {}
    "#});
    let backend = PlistBackend;

    // Build the live plist as XML.
    let inner = dict_from(&[("Password", Value::String("s3cr3t".into()))]);
    let live = {
        let mut buf = Vec::new();
        Value::Dictionary(dict_from(&[("Accounts", Value::Dictionary(inner))]))
            .to_writer_xml(&mut buf)
            .unwrap();
        buf
    };
    let out = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap();
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let Value::Dictionary(top) = &value else {
        panic!("not a dict");
    };
    let Some(Value::Dictionary(accounts)) = top.get("Accounts") else {
        panic!("missing Accounts");
    };
    assert_eq!(
        accounts.get("Password"),
        Some(&Value::String("HIDDEN".into()))
    );
}

#[test]
fn add_hide_path_on_dict_errors() {
    let script = parse_script(indoc! {r#"
        language plist
        add:hide path "Accounts"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let inner = dict_from(&[("k", Value::Integer(1.into()))]);
    let live = {
        let mut buf = Vec::new();
        Value::Dictionary(dict_from(&[("Accounts", Value::Dictionary(inner))]))
            .to_writer_xml(&mut buf)
            .unwrap();
        buf
    };
    let err = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("only valid on") && msg.contains("dictionary"),
        "unexpected error: {msg}"
    );
}

#[test]
fn add_remove_path_deletes_dict_entry() {
    let script = parse_script(indoc! {r#"
        language plist
        add:remove path "secret"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[
        ("a", Value::Integer(1.into())),
        ("secret", Value::String("don't share".into())),
    ]);
    let out = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap();
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("a"), Some(&Value::Integer(1.into())));
    assert!(dict.get("secret").is_none());
}

#[test]
fn filter_preserves_xml_format() {
    let script = parse_script(indoc! {r#"
        language plist
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("a", Value::Integer(1.into()))]);
    let out = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap();
    assert!(
        std::str::from_utf8(&out).unwrap().starts_with("<?xml"),
        "expected XML output preserved"
    );
}

#[test]
fn filter_preserves_binary_format() {
    let script = parse_script(indoc! {r#"
        language plist
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = binary_plist_dict(&[("a", Value::Integer(1.into()))]);
    let out = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap();
    assert!(out.starts_with(b"bplist00"), "expected binary preserved");
}

// ---- wildcard / predicate end-to-end (slice 10) ----------------------------

fn account_dict(name: &str, password: &str) -> Value {
    Value::Dictionary(dict_from(&[
        ("name", Value::String(name.into())),
        ("Password", Value::String(password.into())),
    ]))
}

#[test]
fn ignore_path_wildcard_skips_each_match() {
    // Source has new Password values for every Accounts entry, but the
    // ignore directive preserves live's Password values.
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "Accounts[*].Password"
        ---
        {"Accounts": [{"name": "a", "Password": "src-a"}, {"name": "b", "Password": "src-b"}]}
    "#};
    let live = xml_plist_dict(&[(
        "Accounts",
        Value::Array(vec![
            account_dict("a", "live-a"),
            account_dict("b", "live-b"),
        ]),
    )]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    let Some(Value::Array(arr)) = dict.get("Accounts") else {
        panic!("expected Accounts array");
    };
    assert_eq!(arr.len(), 2);
    let Value::Dictionary(d0) = &arr[0] else {
        panic!()
    };
    let Value::Dictionary(d1) = &arr[1] else {
        panic!()
    };
    assert_eq!(d0.get("Password"), Some(&Value::String("live-a".into())));
    assert_eq!(d1.get("Password"), Some(&Value::String("live-b".into())));
}

#[test]
fn add_hide_path_wildcard_replaces_each_string() {
    let script = parse_script(indoc! {r#"
        language plist
        add:hide path "Accounts[*].Password"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let mut buf = Vec::new();
    Value::Dictionary(dict_from(&[(
        "Accounts",
        Value::Array(vec![
            account_dict("a", "s1"),
            account_dict("b", "s2"),
            account_dict("c", "s3"),
        ]),
    )]))
    .to_writer_xml(&mut buf)
    .unwrap();
    let out = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &buf)
        .unwrap();
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    let Some(Value::Array(arr)) = dict.get("Accounts") else {
        panic!()
    };
    for entry in arr {
        let Value::Dictionary(d) = entry else {
            panic!()
        };
        assert_eq!(d.get("Password"), Some(&Value::String("HIDDEN".into())));
    }
}

#[test]
fn add_hide_path_wildcard_one_non_string_errors_whole_directive() {
    // Second element's Password is an integer, not a string. The whole
    // directive must fail before any element is mutated.
    let script = parse_script(indoc! {r#"
        language plist
        add:hide path "Accounts[*].Password"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let mut buf = Vec::new();
    Value::Dictionary(dict_from(&[(
        "Accounts",
        Value::Array(vec![
            account_dict("a", "s1"),
            Value::Dictionary(dict_from(&[
                ("name", Value::String("b".into())),
                ("Password", Value::Integer(7.into())),
            ])),
            account_dict("c", "s3"),
        ]),
    )]))
    .to_writer_xml(&mut buf)
    .unwrap();
    let err = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &buf)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("only valid on") && msg.contains("integer"),
        "unexpected error: {msg}"
    );

    // And the failure was atomic: re-running with the same input still
    // sees the original strings (we can verify only by parsing the live
    // bytes — the backend doesn't mutate `buf`).
    let still_value = Value::from_reader(std::io::Cursor::new(&buf)).unwrap();
    let Value::Dictionary(d) = &still_value else {
        panic!()
    };
    let Some(Value::Array(arr)) = d.get("Accounts") else {
        panic!()
    };
    let Value::Dictionary(d0) = &arr[0] else {
        panic!()
    };
    assert_eq!(d0.get("Password"), Some(&Value::String("s1".into())));
}

#[test]
fn add_remove_path_wildcard_clears_array() {
    let script = parse_script(indoc! {r#"
        language plist
        add:remove path "Items[*]"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[(
        "Items",
        Value::Array(vec![
            Value::String("a".into()),
            Value::String("b".into()),
            Value::String("c".into()),
        ]),
    )]);
    let out = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap();
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    // Array shell remains; all elements removed.
    let Some(Value::Array(arr)) = dict.get("Items") else {
        panic!("expected Items array");
    };
    assert!(arr.is_empty(), "expected empty array, got {arr:?}");
}

#[test]
fn predicate_ignore_keeps_one_account_unchanged() {
    // Source brings new passwords; the predicate-ignored entry keeps
    // its live password while siblings adopt source values.
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "Accounts[name=\"main\"].Password"
        ---
        {"Accounts": [{"name": "main", "Password": "src-main"}, {"name": "other", "Password": "src-other"}]}
    "#};
    let live = xml_plist_dict(&[(
        "Accounts",
        Value::Array(vec![
            account_dict("other", "live-other"),
            account_dict("main", "live-main"),
        ]),
    )]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    let Some(Value::Array(arr)) = dict.get("Accounts") else {
        panic!()
    };
    // After merge, the array is source's order: main is index 0.
    let Value::Dictionary(d_main) = &arr[0] else {
        panic!()
    };
    assert_eq!(d_main.get("name"), Some(&Value::String("main".into())));
    assert_eq!(
        d_main.get("Password"),
        Some(&Value::String("live-main".into())),
        "predicate-ignored Password should be preserved from live",
    );
    let Value::Dictionary(d_other) = &arr[1] else {
        panic!()
    };
    assert_eq!(d_other.get("name"), Some(&Value::String("other".into())));
    assert_eq!(
        d_other.get("Password"),
        Some(&Value::String("src-other".into())),
        "non-ignored entry should adopt source's Password",
    );
}

#[test]
fn predicate_zero_match_in_source_errors_at_process() {
    // The predicate is well-formed but matches no source element → the
    // ignore-path validation fails with a clear message.
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "Accounts[name=\"nope\"].Password"
        ---
        {"Accounts": [{"name": "main", "Password": "x"}]}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("Accounts", Value::Array(vec![account_dict("main", "live")]))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout: Vec<u8> = vec![];
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("matched no elements") && msg.contains("ignore path"),
        "unexpected error: {msg}"
    );
}

// ---- set path (slice 12) -------------------------------------------------

#[test]
fn set_path_string_replaces_text() {
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "TileSize" "48"
        ---
        {"TileSize": "32"}
    "#};
    let live = xml_plist_dict(&[("TileSize", Value::String("16".into()))]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("TileSize"), Some(&Value::String("48".into())));
}

#[test]
fn set_path_integer_with_type_tag() {
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "TileSize" integer "48"
        ---
        {"TileSize": 32}
    "#};
    let live = xml_plist_dict(&[("TileSize", Value::Integer(16.into()))]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("TileSize"), Some(&Value::Integer(48.into())));
}

#[test]
fn set_path_integer_accepts_u64_above_i64_max() {
    // `plist::Integer` spans the full i64..=u64 range; the JSON import
    // path round-trips u64::MAX via `as_unsigned()`. The DSL `set path
    // ... integer "<lit>"` must do the same so a literal above i64::MAX
    // survives the decode → set round-trip without losing data.
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "Big" integer "18446744073709551615"
        ---
        {"Big": 0}
    "#};
    let live = xml_plist_dict(&[("Big", Value::Integer(0i64.into()))]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    let actual = dict
        .get("Big")
        .and_then(Value::as_unsigned_integer)
        .expect("Big should decode as an unsigned integer");
    assert_eq!(actual, u64::MAX);
}

#[test]
fn set_path_integer_without_type_tag_inferred() {
    // No type tag — the existing scalar's type (integer) is used.
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "TileSize" "48"
        ---
        {"TileSize": 32}
    "#};
    let live = xml_plist_dict(&[("TileSize", Value::Integer(16.into()))]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("TileSize"), Some(&Value::Integer(48.into())));
}

#[test]
fn set_path_type_mismatch_errors() {
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "TileSize" string "48"
        ---
        {"TileSize": 32}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("TileSize", Value::Integer(16.into()))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout: Vec<u8> = vec![];
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("type mismatch") && msg.contains("string") && msg.contains("integer"),
        "unexpected error: {msg}"
    );
}

#[test]
fn set_path_on_bool_errors() {
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "flag" "true"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("flag", Value::Boolean(false))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout: Vec<u8> = vec![];
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("non-scalar") && msg.contains("boolean"),
        "unexpected error: {msg}"
    );
}

#[test]
fn set_path_real_value_with_decimal() {
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "ratio" real "1.5"
        ---
        {"ratio": 2.0}
    "#};
    let live = xml_plist_dict(&[("ratio", Value::Real(0.5))]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("ratio"), Some(&Value::Real(1.5)));
}

#[test]
fn set_path_data_base64_decode() {
    // base64 "AQID" -> bytes [1, 2, 3]
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "blob" data "AQID"
        ---
        {}
    "#};
    let live = xml_plist_dict(&[("blob", Value::Data(vec![0]))]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("blob"), Some(&Value::Data(vec![1, 2, 3])));
}

#[test]
fn set_path_date_iso8601() {
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "ts" date "2026-01-02T03:04:05Z"
        ---
        {}
    "#};
    // Build a live with a date scalar (any date) so the type infers.
    let initial = plist::Date::from_xml_format("2020-01-01T00:00:00Z").expect("valid initial date");
    let live = xml_plist_dict(&[("ts", Value::Date(initial))]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    let Some(Value::Date(d)) = dict.get("ts") else {
        panic!("expected date");
    };
    assert_eq!(d.to_xml_format(), "2026-01-02T03:04:05Z");
}

#[test]
fn set_path_invalid_integer_errors() {
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "n" integer "not-a-number"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("n", Value::Integer(0.into()))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout: Vec<u8> = vec![];
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invalid integer literal"),
        "unexpected error: {msg}"
    );
}

#[test]
fn set_path_predicate_match_single() {
    // Predicates are single-match and therefore valid for `set path`.
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "Accounts[name=\"main\"].Password" "new-secret"
        ---
        {"Accounts": [{"name": "main", "Password": "src"}, {"name": "other", "Password": "src2"}]}
    "#};
    let live = xml_plist_dict(&[(
        "Accounts",
        Value::Array(vec![
            account_dict("main", "live-main"),
            account_dict("other", "live-other"),
        ]),
    )]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    let Some(Value::Array(arr)) = dict.get("Accounts") else {
        panic!("expected Accounts array");
    };
    // After source-shallow merge, source order wins: main is index 0.
    let Value::Dictionary(d_main) = &arr[0] else {
        panic!()
    };
    assert_eq!(
        d_main.get("Password"),
        Some(&Value::String("new-secret".into())),
        "predicate-set Password should be the literal value"
    );
}

#[test]
fn set_path_wildcard_rejected() {
    // `[*]` is reserved for multi-match directives; `set path` requires
    // a single target so the parser-level validator rejects it.
    let script = parse_script(indoc! {r#"
        language plist
        set path "Items[*]" "x"
        ---
        {}
    "#});
    let err = config::parse_for_merge(&script).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("requires a single target") && msg.contains("`[*]`"),
        "unexpected error: {msg}"
    );
}

// ---- bug fix regressions ---------------------------------------------------

#[test]
fn filter_ignore_path_removes_value() {
    // `ignore path` on the re-add path must remove the addressed value
    // so that secrets pinned to the live side don't leak through
    // `chezmoi re-add` into the source-controlled tree. The validator
    // used to accept the directive but drop it before filtering.
    let script = parse_script(indoc! {r#"
        language plist
        ignore path "Accounts[*].Password"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[(
        "Accounts",
        Value::Array(vec![
            account_dict("a", "secret-a"),
            account_dict("b", "secret-b"),
        ]),
    )]);
    let out = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap();
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    let Some(Value::Array(arr)) = dict.get("Accounts") else {
        panic!("expected Accounts array")
    };
    for entry in arr {
        let Value::Dictionary(d) = entry else {
            panic!()
        };
        assert!(
            d.get("Password").is_none(),
            "Password should be removed by `ignore path` re-add filter, got {entry:?}",
        );
    }
}

#[test]
fn ignore_path_redacted_value_does_not_leak_to_filtered_output() {
    // Security-relevant concern: a `Password` that the user is
    // ignoring during merge must NOT survive into the filtered
    // (re-add) output either. Otherwise `chezmoi re-add` happily
    // commits the very secret the ignore directive was meant to keep
    // local.
    let script = parse_script(indoc! {r#"
        language plist
        ignore path "Password"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[
        ("name", Value::String("alice".into())),
        ("Password", Value::String("super-secret".into())),
    ]);
    let out = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap();
    // The encoded plist must not embed the literal secret anywhere.
    assert!(
        !out.windows(b"super-secret".len())
            .any(|w| w == b"super-secret"),
        "secret leaked into filtered output: {out:?}",
    );
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert!(
        dict.get("Password").is_none(),
        "Password key should be dropped"
    );
    assert_eq!(dict.get("name"), Some(&Value::String("alice".into())));
}

#[test]
fn ignore_path_predicate_with_reordered_arrays() {
    // Adversarial: live and source arrays both contain a `main`
    // account, but at different indices and with different siblings.
    // The predicate must align by dict identity so the live `main`
    // password lands on the merged `main`, *not* on whichever sibling
    // happens to share the same array index.
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "Accounts[name=\"main\"].Password"
        ---
        {"Accounts": [{"name": "work", "Password": "src-work"}, {"name": "main", "Password": "src-main"}]}
    "#};
    let live = xml_plist_dict(&[(
        "Accounts",
        Value::Array(vec![account_dict("main", "live-main")]),
    )]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    let Some(Value::Array(arr)) = dict.get("Accounts") else {
        panic!()
    };
    assert_eq!(arr.len(), 2);
    // Source order: work first (idx 0), main second (idx 1).
    let Value::Dictionary(work) = &arr[0] else {
        panic!()
    };
    assert_eq!(work.get("name"), Some(&Value::String("work".into())));
    assert_eq!(
        work.get("Password"),
        Some(&Value::String("src-work".into())),
        "work should keep its source password — only main is ignored",
    );
    let Value::Dictionary(main) = &arr[1] else {
        panic!()
    };
    assert_eq!(main.get("name"), Some(&Value::String("main".into())));
    assert_eq!(
        main.get("Password"),
        Some(&Value::String("live-main".into())),
        "predicate-aligned ignore must restore live's main password to merged main",
    );
}

#[test]
fn ignore_path_wildcard_length_mismatch_errors() {
    // With `[*]` and arrays of different shape between live and source,
    // there is no positional alignment that preserves the user's
    // intent — error explicitly rather than silently smear values.
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "Accounts[*].Password"
        ---
        {"Accounts": [{"name": "a", "Password": "src-a"}, {"name": "b", "Password": "src-b"}, {"name": "c", "Password": "src-c"}]}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("Accounts", Value::Array(vec![account_dict("a", "live-a")]))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout = Vec::<u8>::new();
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("differ in length") || msg.contains("different match"),
        "unexpected error: {msg}",
    );
}

#[test]
fn looks_like_xml_plist_with_bom() {
    // UTF-8 BOM (`EF BB BF`) at the start of an XML plist used to
    // break the heuristic and force a binary re-encode on round-trip.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(b"<?xml version=\"1.0\"?>\n<plist><dict/></plist>\n");
    assert!(
        looks_like_xml_plist(&bytes),
        "BOM-prefixed XML plist should be detected as XML",
    );
}

#[test]
fn merge_top_level_empty_live_treated_as_empty_dict() {
    // Empty stdin (no live file on disk yet) is silently coerced to an
    // empty top-level dict so a fresh apply can populate every source
    // key. A non-empty but non-dict live still errors.
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        ---
        {"a": 1, "b": "two"}
    "#};
    let out = run_process(script, b"");
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("a"), Some(&Value::Integer(1.into())));
    assert_eq!(dict.get("b"), Some(&Value::String("two".into())));
}
// ---- merge_top_level: non-dict bail branches ------------------------------

fn try_run_process(script_text: &str, live_bytes: &[u8]) -> Result<Vec<u8>, anyhow::Error> {
    let script = parse_script(script_text);
    let backend = PlistBackend;
    let mut stdin = std::io::Cursor::new(live_bytes.to_vec());
    let mut stdout = Vec::<u8>::new();
    backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .map(|()| stdout)
}

#[test]
fn top_level_array_source_errors() {
    // A JSON inline body whose root is an array (not an object) must
    // be rejected by `merge_top_level` with a "must be a top-level
    // dictionary" diagnostic.
    let script = indoc! {r#"
        language plist
        ---
        [1, 2, 3]
    "#};
    let live = xml_plist_dict(&[("a", Value::Integer(1.into()))]);
    let err = try_run_process(script, &live).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("top-level dictionary"),
        "unexpected error: {msg}"
    );
}

#[test]
fn top_level_array_live_errors() {
    // A live plist whose top-level value is an `<array>` must be
    // rejected. We build a binary plist whose root is an array: the
    // live decode itself succeeds, then `merge_top_level` rejects.
    let script = indoc! {r#"
        language plist
        ---
        {}
    "#};
    let live = {
        let mut buf = Vec::new();
        Value::Array(vec![Value::Integer(1.into())])
            .to_writer_binary(&mut buf)
            .unwrap();
        buf
    };
    let err = try_run_process(script, &live).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("top-level dictionary"),
        "unexpected error: {msg}"
    );
}

// ---- set path data/date error paths ---------------------------------------

#[test]
fn set_path_invalid_base64_errors() {
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "X" data "not-base64"
        ---
        {}
    "#};
    // Live tree has X bound to <data> so the inferred set type matches.
    let live = xml_plist_dict(&[("X", Value::Data(b"abc".to_vec()))]);
    let err = try_run_process(script, &live).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("base64"), "unexpected error: {msg}");
}

#[test]
fn set_path_invalid_iso8601_errors() {
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "X" date "garbage"
        ---
        {}
    "#};
    // Live tree has X bound to <date> so the inferred set type matches.
    let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
        \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
        <plist version=\"1.0\">\n<dict>\n<key>X</key>\n\
        <date>2023-01-02T03:04:05Z</date>\n</dict>\n</plist>\n";
    let err = try_run_process(script, xml.as_bytes()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ISO-8601") || msg.contains("date"),
        "unexpected error: {msg}"
    );
}

// ---- compare_paths_desc length tiebreak -----------------------------------

#[test]
fn remove_path_descending_sort_handles_nested_arrays() {
    // Multiple paths matched by `[*]` traversal include both
    // `Items[0].sub[1]` and `Items[0]`. The sort must order the deeper
    // (longer) path first so the deeper path is processed before its
    // ancestor.
    use super::compare_paths_desc;
    use crate::path::plist::PlistPath;
    let deeper: PlistPath = "Items[0].sub[1]".parse().unwrap();
    let shallower: PlistPath = "Items[0]".parse().unwrap();
    let mut v = [shallower.clone(), deeper.clone()];
    v.sort_by(compare_paths_desc);
    assert_eq!(v[0], deeper, "deeper path must sort first");
    assert_eq!(v[1], shallower);
}

// ---- round-2 reviewer-found correctness regressions ----------------------

#[test]
fn ignore_path_multi_segment_dict_under_merge_shallow_preserves_live() {
    // Multi-segment dict-key ignore (`Outer.Inner`) under
    // `merge shallow`: the source overwrites the entire `Outer` dict,
    // so without a post-merge restore for length>1 dict paths the
    // ignore would silently no-op and source's value would leak
    // through. Regression for round-2 fix #1.
    let script = indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "Outer.Inner"
        ---
        {"Outer": {"Inner": "clobber", "Other": "src-other"}}
    "#};
    let live = xml_plist_dict(&[(
        "Outer",
        Value::Dictionary(dict_from(&[("Inner", Value::String("keep".into()))])),
    )]);
    let out = run_process(script, &live);
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    let Some(Value::Dictionary(outer)) = dict.get("Outer") else {
        panic!("expected Outer dict")
    };
    assert_eq!(
        outer.get("Inner"),
        Some(&Value::String("keep".into())),
        "ignore path Outer.Inner must preserve live's value under merge shallow",
    );
    // Sibling (`Outer.Other`) still receives source's value — only
    // the ignored leaf is preserved.
    assert_eq!(
        outer.get("Other"),
        Some(&Value::String("src-other".into())),
        "non-ignored sibling should take source's value",
    );
}

#[test]
fn ignore_path_predicate_zero_match_in_live_errors() {
    // Predicate matches no live element: there is no value to restore.
    // The previous behaviour silently left the merged tree (=source's
    // value) in place, defeating the directive's intent. Regression
    // for round-2 fix #2.
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "Accounts[name=\"main\"].Password"
        ---
        {"Accounts": [{"name": "main", "Password": "src-main"}]}
    "#});
    let backend = PlistBackend;
    // Live's Accounts has no `main` entry — predicate matches zero.
    let live = xml_plist_dict(&[(
        "Accounts",
        Value::Array(vec![account_dict("other", "live-other")]),
    )]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout = Vec::<u8>::new();
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("zero elements"),
        "expected zero-match error, got: {msg}",
    );
}

#[test]
fn ignore_path_predicate_ambiguous_live_errors() {
    // Predicate matches multiple live elements: ambiguous. Regression
    // for round-2 fix #2.
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "Accounts[name=\"main\"].Password"
        ---
        {"Accounts": [{"name": "main", "Password": "src-main"}]}
    "#});
    let backend = PlistBackend;
    // Live's Accounts has two `main` entries — predicate is
    // ambiguous on the live side.
    let live = xml_plist_dict(&[(
        "Accounts",
        Value::Array(vec![
            account_dict("main", "live-1"),
            account_dict("main", "live-2"),
        ]),
    )]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout = Vec::<u8>::new();
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ambiguous") || msg.contains("2 elements"),
        "expected ambiguous-match error, got: {msg}",
    );
}

#[test]
fn set_path_data_with_xml_special_chars_in_literal() {
    // The previous implementation built an XML string
    // `<plist><data>{literal}</data></plist>` and parsed it. A literal
    // containing `<`, `&`, or `]]>` corrupted (or got rejected with
    // an XML-parse error pretending to be a base64 error). The new
    // path uses the `base64` crate directly and surfaces a clean
    // diagnostic. Regression for round-2 fix #3.
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "X" data "<not-base64>"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("X", Value::Data(b"abc".to_vec()))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout = Vec::<u8>::new();
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invalid base64"),
        "expected base64 error, got: {msg}",
    );
}

#[test]
fn filter_empty_live_treated_as_empty_dict_plist() {
    // `process` accepts empty stdin (slice-7 fix); `filter` should be
    // consistent — first-run re-add of a not-yet-existing live file.
    // Regression for round-2 fix #5.
    let script = parse_script(indoc! {r#"
        language plist
        ignore path "Password"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let out = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), b"")
        .expect("empty live must be coerced to empty dict, not error");
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert!(dict.is_empty(), "expected empty dict, got {dict:?}");
}

#[test]
fn ignore_path_shape_mismatch_live_dict_source_array_no_op() {
    // Defensive guard for `restore_ignore_in_parallel`: if live and the
    // merged tree have incompatible shapes at some path segment,
    // degrade to a no-op rather than a hard `bail!`. The previous
    // arms ("expected dictionary at segment", "expected array at
    // segment `[n]`", "expected array at `[*]`") were over-strict —
    // source's shape determines the merged tree, and a live tree
    // that happens to disagree is benign (there is nothing to
    // restore anyway). This test exercises the function directly with
    // a hand-crafted shape disagreement that the high-level `process`
    // pipeline cannot easily reproduce, since the path-validator
    // pre-checks source shapes. Regression for round-2 fix #6.
    use crate::path::plist::PlistPath;

    let live = Value::Dictionary(dict_from(&[(
        "Accounts",
        Value::Dictionary(dict_from(&[("Password", Value::String("live-pwd".into()))])),
    )]));
    // Pretend the post-merge tree mirrors source's array shape (i.e.
    // source replaced live's dict with an array of strings).
    let mut merged = Value::Dictionary(dict_from(&[(
        "Accounts",
        Value::Array(vec![Value::String("from-source".into())]),
    )]));
    let path: PlistPath = "Accounts.Password".parse().unwrap();
    // Must not error: shape mismatch is a no-op, leaving merged
    // unchanged.
    restore_ignore_in_parallel(&live, &mut merged, &path, 0)
        .expect("shape mismatch must degrade to no-op, not error");
    let Value::Dictionary(d) = &merged else {
        panic!()
    };
    let Some(Value::Array(a)) = d.get("Accounts") else {
        panic!("merged must remain unchanged")
    };
    assert_eq!(a.len(), 1);
}

// ---- restore_ignore_in_parallel shape-mismatch handling -------------------
//
// These tests drive `restore_ignore_in_parallel` directly to exercise the
// three shape-mismatch arms (Key/Index/Wildcard). The earlier round
// surfaced these as confusing hard errors when the public pipeline could
// not in practice produce the divergence; the correctness fix degrades
// them to no-ops, which these assertions pin in place.

#[test]
fn restore_ignore_arr_vs_dict_type_mismatch_at_n_segment() {
    use crate::path::plist::PlistPath;
    let live = Value::Dictionary(dict_from(&[(
        "Accounts",
        Value::Array(vec![Value::String("live-pw".into())]),
    )]));
    let merged_before = Value::Dictionary(dict_from(&[(
        "Accounts",
        Value::Dictionary(dict_from(&[("x", Value::Integer(1.into()))])),
    )]));
    let mut merged = merged_before.clone();
    let path: PlistPath = "Accounts[0]".parse().unwrap();
    restore_ignore_in_parallel(&live, &mut merged, &path, 0)
        .expect("shape mismatch must degrade to no-op, not error");
    assert_eq!(merged, merged_before, "merged tree must be unchanged");
}

#[test]
fn restore_ignore_dict_vs_arr_type_mismatch_at_key_segment() {
    use crate::path::plist::PlistPath;
    let live = Value::Dictionary(dict_from(&[("Password", Value::String("live-pw".into()))]));
    let merged_before = Value::Array(vec![Value::Integer(1.into())]);
    let mut merged = merged_before.clone();
    let path: PlistPath = "Password".parse().unwrap();
    restore_ignore_in_parallel(&live, &mut merged, &path, 0)
        .expect("shape mismatch must degrade to no-op, not error");
    assert_eq!(merged, merged_before, "merged tree must be unchanged");
}

#[test]
fn restore_ignore_wildcard_against_non_array() {
    use crate::path::plist::PlistPath;
    let live = Value::Dictionary(dict_from(&[(
        "Accounts",
        Value::Array(vec![Value::String("a".into()), Value::String("b".into())]),
    )]));
    let merged_before = Value::Dictionary(dict_from(&[(
        "Accounts",
        Value::Dictionary(dict_from(&[("x", Value::Integer(1.into()))])),
    )]));
    let mut merged = merged_before.clone();
    let path: PlistPath = "Accounts[*]".parse().unwrap();
    restore_ignore_in_parallel(&live, &mut merged, &path, 0)
        .expect("shape mismatch must degrade to no-op, not error");
    assert_eq!(merged, merged_before, "merged tree must be unchanged");
}

// ---- set path: non-scalar targets -----------------------------------------

#[test]
fn set_path_on_array_target_errors() {
    // The merged tree at `Items` is an array — `set path` is replace-
    // only and only valid on scalars (no type-tag inference, must
    // error).
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "Items" "x"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[(
        "Items",
        Value::Array(vec![Value::Integer(1.into()), Value::Integer(2.into())]),
    )]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout: Vec<u8> = vec![];
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("non-scalar") && msg.contains("array"),
        "unexpected error: {msg}",
    );
}

#[test]
fn set_path_on_dict_target_errors() {
    // The merged tree at `outer` is a dict — `set path` is replace-only
    // and only valid on scalars; ensures the non-scalar guard rejects
    // dict targets explicitly.
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        set path "outer" "x"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let inner = dict_from(&[("a", Value::Integer(1.into()))]);
    let live = xml_plist_dict(&[("outer", Value::Dictionary(inner))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout: Vec<u8> = vec![];
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("non-scalar") && msg.contains("dictionary"),
        "unexpected error: {msg}",
    );
}

// ---- add:hide type rejection ----------------------------------------------

#[test]
fn add_hide_on_integer_value_errors() {
    // `add:hide` is only valid on `<string>` or `<data>` values.
    let script = parse_script(indoc! {r#"
        language plist
        add:hide path "n"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("n", Value::Integer(42.into()))]);
    let err = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("only valid on") && msg.contains("integer"),
        "unexpected error: {msg}",
    );
}

#[test]
fn add_hide_on_dict_value_errors() {
    // Mirror of `add_hide_path_on_dict_errors` above with the
    // canonical task-spec name; ensures the `add:hide` type guard
    // rejects dict targets explicitly.
    let script = parse_script(indoc! {r#"
        language plist
        add:hide path "Accounts"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let inner = dict_from(&[("k", Value::Integer(1.into()))]);
    let live = xml_plist_dict(&[("Accounts", Value::Dictionary(inner))]);
    let err = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("only valid on") && msg.contains("dictionary"),
        "unexpected error: {msg}",
    );
}

// ---- JSON ↔ plist depth boundary precision --------------------------------

#[test]
fn json_to_plist_depth_at_127_succeeds() {
    // 127 nested arrays (innermost number sits at depth 127, which is
    // strictly less than MAX_DEPTH = 128) must succeed.
    let mut value = serde_json::Value::Number(1.into());
    for _ in 0..127 {
        value = serde_json::Value::Array(vec![value]);
    }
    json::json_to_plist(&value).expect("depth 127 should succeed");
}

#[test]
fn json_to_plist_depth_at_128_errors() {
    // 128 nested arrays (innermost number at depth 128) must error
    // because `depth >= MAX_DEPTH` triggers the limit.
    let mut value = serde_json::Value::Number(1.into());
    for _ in 0..128 {
        value = serde_json::Value::Array(vec![value]);
    }
    let err = json::json_to_plist(&value).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("depth") && msg.contains("128"),
        "unexpected error: {msg}",
    );
}

#[test]
fn plist_to_json_depth_at_127_succeeds() {
    let mut value = Value::Integer(1.into());
    for _ in 0..127 {
        value = Value::Array(vec![value]);
    }
    transforms::plist_to_json(&value).expect("depth 127 should succeed");
}

#[test]
fn plist_to_json_depth_at_128_errors() {
    let mut value = Value::Integer(1.into());
    for _ in 0..128 {
        value = Value::Array(vec![value]);
    }
    let err = transforms::plist_to_json(&value).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("depth") && msg.contains("128"),
        "unexpected error: {msg}",
    );
}

// ---- round-3 regressions --------------------------------------------------

#[test]
fn filter_ignore_path_plist_ambiguous_predicate_errors() {
    // Round-3 regression: an `ignore path` whose predicate matches more
    // than one element on the live side must propagate the resolution
    // error rather than silently no-op'ing. The previous
    // `let Ok(...) else continue` swallowed every kind of resolve
    // failure, including ambiguous-match — and that meant a duplicate
    // `name` would silently let every Password through `chezmoi re-add`.
    let script = parse_script(indoc! {r#"
        language plist
        ignore path "Accounts[name=\"dup\"].Password"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[(
        "Accounts",
        Value::Array(vec![
            account_dict("dup", "secret-1"),
            account_dict("dup", "secret-2"),
        ]),
    )]);
    let err = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("matched 2 elements"),
        "expected ambiguous-match error, got: {msg}"
    );
    // Directive context is still attached so the user can find which
    // line triggered it.
    assert!(
        msg.contains("ignore path"),
        "expected directive context: {msg}"
    );
}

#[test]
fn filter_ignore_path_plist_no_match_is_silent_noop() {
    // Round-3 regression: an `ignore path` that addresses a key absent
    // from this particular live file is a no-op, not an error. Output
    // must round-trip the live tree unchanged (modulo encoder rewrite).
    let script = parse_script(indoc! {r#"
        language plist
        ignore path "Missing"
        ---
        {}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("present", Value::String("kept".into()))]);
    let out = backend
        .filter(&script, Utf8Path::new("modify_test.plist.tmpl"), &live)
        .expect("absent ignore path is a legitimate no-op");
    let value = Value::from_reader(std::io::Cursor::new(&out)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("present"), Some(&Value::String("kept".into())));
}

#[test]
fn merge_deep_depth_limit_errors_at_128() {
    // Round-3 regression: `merge_deep_at` recursed without a depth
    // bound. A binary plist with thousands of nested dicts (which
    // `plist::Value::from_reader` happily decodes) would overflow the
    // stack. We now error cleanly at the same MAX_DEPTH as JSON
    // conversion.
    let mut nested = Value::Dictionary(Dictionary::new());
    for _ in 0..200 {
        let mut outer = Dictionary::new();
        outer.insert("inner".to_string(), nested);
        nested = Value::Dictionary(outer);
    }
    let mut live_dict = Dictionary::new();
    live_dict.insert("root".to_string(), nested.clone());
    let mut source_dict = Dictionary::new();
    source_dict.insert("root".to_string(), nested);

    let err = merge::merge_deep(&mut live_dict, &source_dict, &[]).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("depth") && msg.contains("128"),
        "expected clean depth-limit error, got: {msg}"
    );
}

#[test]
fn process_ignore_path_typo_errors() {
    // Round-3 regression: a literal `ignore path` referring to a key
    // missing from source must error (typo detection), not silently
    // no-op. With the not-found marker added in this round, the
    // existing `?`-propagation in `process` now correctly surfaces it.
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "Missing"
        ---
        {"present": 1}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("present", Value::Integer(0.into()))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout: Vec<u8> = vec![];
    let err = backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing key `Missing`"),
        "expected missing-key error: {msg}"
    );
}

#[test]
fn process_ignore_path_wildcard_zero_match_is_noop() {
    // Round-3 regression complement: a wildcard `[*]` against an empty
    // array is *not* a typo — matching zero elements is meaningful and
    // the directive trivially does nothing. `process` must succeed.
    let script = parse_script(indoc! {r#"
        language plist
        merge shallow
        output xml
        ignore path "Items[*].x"
        ---
        {"Items": []}
    "#});
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("Items", Value::Array(vec![]))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout: Vec<u8> = vec![];
    backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .expect("wildcard against empty array is a legitimate no-op");
}

#[test]
fn process_inline_json_with_bom_decodes_correctly() {
    // Round-3 regression: round-2 added BOM stripping in
    // `detect_inline_format` but the end-to-end `decode_source` →
    // `decode_json_bytes` pipeline wasn't tested. Build a script whose
    // body begins with the UTF-8 BOM (`EF BB BF`) followed by a JSON
    // object and verify the merged output reflects the source value.
    let mut directives = Vec::new();
    directives.extend_from_slice(
        b"language plist\nmerge shallow\noutput xml\nset path \"k\" string \"override\"\n---\n",
    );
    let mut body = Vec::new();
    body.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    body.extend_from_slice(br#"{"k": "src-value"}"#);
    let mut raw = directives;
    raw.extend_from_slice(&body);

    let script = Script::parse(&raw, Utf8Path::new("modify_test.plist.tmpl")).unwrap();
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("k", Value::String("live-value".into()))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout: Vec<u8> = vec![];
    backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .expect("BOM-prefixed inline JSON body must decode");
    let value = Value::from_reader(std::io::Cursor::new(&stdout)).unwrap();
    let dict = value.into_dictionary().unwrap();
    // `set path` after the merge replaces `k` regardless; the test
    // proves the JSON parsed (otherwise process would have errored
    // before reaching `set path`).
    assert_eq!(dict.get("k"), Some(&Value::String("override".into())));
}

#[test]
fn set_path_data_full_byte_range_round_trip() {
    // Round-3 regression: the round-2 base64 fix used
    // `base64::engine::general_purpose::STANDARD` directly. Existing
    // tests only round-tripped `"AQID"` (no `+`/`/`/`=` characters),
    // which would not catch a broken alphabet table. Encode the full
    // 0..=255 byte range and verify it decodes back to the same bytes
    // through the `set path "X" data "..."` path.
    use base64::Engine as _;
    let original: Vec<u8> = (0u8..=255).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&original);
    let script_text = format!(
        "language plist\nmerge shallow\noutput xml\nset path \"blob\" data \"{encoded}\"\n---\n{{}}\n"
    );
    let script = parse_script(&script_text);
    let backend = PlistBackend;
    let live = xml_plist_dict(&[("blob", Value::Data(vec![0]))]);
    let mut stdin = std::io::Cursor::new(live);
    let mut stdout: Vec<u8> = vec![];
    backend
        .process(
            &script,
            Utf8Path::new("modify_test.plist.tmpl"),
            &mut stdin,
            &mut stdout,
        )
        .expect("full-byte-range base64 should decode cleanly");
    let value = Value::from_reader(std::io::Cursor::new(&stdout)).unwrap();
    let dict = value.into_dictionary().unwrap();
    assert_eq!(dict.get("blob"), Some(&Value::Data(original)));
}
