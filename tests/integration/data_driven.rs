//! Data & config driven tests.

use camino::Utf8PathBuf;
use chezmoi_modify_manager::ChmmArgs;
use chezmoi_modify_manager::inner_main;
use chezmoi_modify_manager::run_filter;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::io::BufReader;
use std::io::Read;

/// Fixtures gated behind a separate `#[ignore]`d test (so they can land
/// before the implementation does).
const PENDING_FIXTURES: &[&str] = &[];

/// Find all the test cases. By default this excludes [`PENDING_FIXTURES`];
/// pass `include_pending = true` to opt in (used by the gated test).
fn find_test_cases(include_pending: bool) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let mut path: Utf8PathBuf = std::env::var("CARGO_MANIFEST_DIR")?.into();
    path.push("tests");
    path.push("data");

    let mut results = vec![];
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let path: Utf8PathBuf = entry.path().try_into().expect("Path isn't valid UTF-8");
        if !path.is_file() {
            continue;
        }
        if path.extension() == Some("tmpl") {
            let stem = path.file_stem().unwrap_or("");
            let pending = PENDING_FIXTURES.contains(&stem);
            if pending != include_pending {
                continue;
            }
            results.push(path);
        }
    }
    Ok(results)
}

/// Variant of a `.tmpl` test case to run.
enum FixtureKind {
    /// Drive the merge (`Backend::process`) end-to-end via `inner_main`.
    Process {
        sys: Utf8PathBuf,
        expected: Utf8PathBuf,
    },
    /// Drive the re-add filter (`Backend::filter`) directly.
    Filter {
        live: Utf8PathBuf,
        expected_source: Utf8PathBuf,
    },
}

/// Detect what to run for a `.tmpl` fixture.
///
/// * `<name>.sys.<ext>` + `<name>.expected.<ext>` — process (merge) fixture.
/// * `<name>.live.<ext>` + `<name>.expected-source.<ext>` — filter (re-add)
///   fixture.
fn fixture_kind(test_case: &Utf8PathBuf) -> FixtureKind {
    for ext in ["ini", "xml"] {
        let sys = test_case.with_extension(format!("sys.{ext}"));
        let expected = test_case.with_extension(format!("expected.{ext}"));
        if sys.is_file() && expected.is_file() {
            return FixtureKind::Process { sys, expected };
        }
        let live = test_case.with_extension(format!("live.{ext}"));
        let expected_source = test_case.with_extension(format!("expected-source.{ext}"));
        if live.is_file() && expected_source.is_file() {
            return FixtureKind::Filter {
                live,
                expected_source,
            };
        }
    }
    panic!(
        "no fixture pair found for {test_case}; expected one of \
         <name>.sys.<ext>+<name>.expected.<ext> (process) or \
         <name>.live.<ext>+<name>.expected-source.<ext> (filter); \
         supported extensions are ini, xml"
    );
}

fn read_all(path: &Utf8PathBuf) -> Vec<u8> {
    let mut buf: Vec<u8> = vec![];
    File::open(path)
        .unwrap_or_else(|e| panic!("failed to open fixture {path}: {e}"))
        .read_to_end(&mut buf)
        .unwrap_or_else(|e| panic!("failed to read fixture {path}: {e}"));
    buf
}

fn run_fixture(test_case: &Utf8PathBuf) {
    match fixture_kind(test_case) {
        FixtureKind::Process { sys, expected } => {
            let expected_data = read_all(&expected);
            let mut stdout: Vec<u8> = vec![];
            let mut status: Vec<u8> = vec![];

            inner_main(
                ChmmArgs::Process(test_case.clone()),
                || {
                    BufReader::new(
                        File::open(&sys)
                            .unwrap_or_else(|e| panic!("failed to open sys fixture {sys}: {e}")),
                    )
                },
                || &mut stdout,
                || &mut status,
            )
            .unwrap_or_else(|e| panic!("inner_main failed for {test_case}: {e}"));
            assert_eq!(
                String::from_utf8(stdout),
                String::from_utf8(expected_data),
                "test case {test_case}"
            );
            assert_eq!(status, b"", "test case {test_case}");
        }
        FixtureKind::Filter {
            live,
            expected_source,
        } => {
            let live_bytes = read_all(&live);
            let expected_data = read_all(&expected_source);
            let actual = run_filter(test_case, &live_bytes)
                .unwrap_or_else(|e| panic!("run_filter failed for {test_case}: {e}"));
            assert_eq!(
                String::from_utf8(actual),
                String::from_utf8(expected_data),
                "filter test case {test_case}"
            );
        }
    }
}

#[test]
fn test_data() {
    for test_case in find_test_cases(false).unwrap() {
        run_fixture(&test_case);
    }
}

/// Pending-feature fixture suite. Empty after the `ignore path` re-add
/// behaviour landed; kept as a hook for future feature gating.
#[test]
fn test_data_pending() {
    // PENDING_FIXTURES is currently empty; iterating over zero matches
    // keeps the hook live so adding a new entry above immediately runs.
    for test_case in find_test_cases(true).unwrap() {
        run_fixture(&test_case);
    }
}
