//! Shared helpers for the latency-targeted backend benchmarks.
//!
//! Each benchmark in this directory drives the full `Backend::process`
//! pipeline through `chezmoi_modify_manager::inner_main`. We deliberately
//! avoid exposing internal APIs: the public entry point is the contract
//! a regression would break in real use, so timing it gives a faithful
//! ceiling for "configure this file with chmm".
//!
//! Conventions:
//!
//! * Each bench writes a single-file modify script (directives + inline
//!   body separated by `---`) into a `tempfile::TempDir`. Setup is
//!   one-shot; the benchmarked closure only re-runs the merge.
//! * `sample_size(20)` keeps `cargo bench` cheap enough for CI. The
//!   intent is regression detection, not statistical glamour.
//! * Targets quoted in the per-bench file headers are developer-laptop
//!   ceilings — e.g. dotfile-sized plist merge < 1 ms. A future change
//!   that pushes a workload past its ceiling is what these benches are
//!   meant to catch.

use camino::Utf8PathBuf;
use chezmoi_modify_manager::ChmmArgs;
use chezmoi_modify_manager::inner_main;
use std::io::Cursor;

/// Standard directive prefix for plist `merge shallow` benches.
/// Captures the common script shape (language + merge) and keeps
/// per-bench duplication down to the source-body string.
#[allow(dead_code)] // not every bench file imports this constant
pub(crate) const PLIST_SHALLOW_DIRECTIVES: &str = "#!/bin/sh\nlanguage plist\nmerge shallow\n";

/// Sample count shared by the latency-targeted benches. Twenty samples is
/// enough to detect regressions; full sample count burns CI minutes for
/// no extra signal at this latency scale.
#[allow(dead_code)]
pub(crate) const BENCH_SAMPLE_SIZE: usize = 20;

/// One-shot benchmark fixture: a temp directory holding a modify script
/// (with optional inline body) plus, optionally, a sidecar source file.
///
/// Holding the `TempDir` keeps the directory alive for the duration of
/// the bench; dropping the fixture cleans up.
pub(crate) struct ProcessFixture {
    _dir: tempfile::TempDir,
    pub(crate) script_path: Utf8PathBuf,
}

impl ProcessFixture {
    /// Build a fixture with an inline single-file script.
    ///
    /// `directives` is the text above the `---` divider; `body` is the
    /// inline source bytes below it. The bench can then call
    /// `run_process(&fixture, live_bytes)` to time one merge.
    pub(crate) fn inline(directives: &str, body: &[u8]) -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let dir_path: Utf8PathBuf = dir
            .path()
            .to_path_buf()
            .try_into()
            .expect("tempdir is valid utf-8");
        let script_path = dir_path.join("modify_bench");

        let mut content = Vec::with_capacity(directives.len() + 4 + body.len());
        content.extend_from_slice(directives.as_bytes());
        if !directives.ends_with('\n') {
            content.push(b'\n');
        }
        content.extend_from_slice(b"---\n");
        content.extend_from_slice(body);
        std::fs::write(&script_path, &content).expect("write modify script");

        Self {
            _dir: dir,
            script_path,
        }
    }
}

/// Drive the `Backend::process` pipeline for `fixture` against the
/// supplied live bytes. Returns the merged output so the caller can
/// `black_box` it — without that, LLVM is free to elide everything.
pub(crate) fn run_process(fixture: &ProcessFixture, live_bytes: &[u8]) -> Vec<u8> {
    let mut stdout: Vec<u8> = Vec::with_capacity(live_bytes.len());
    let mut status: Vec<u8> = Vec::new();
    inner_main(
        ChmmArgs::Process(fixture.script_path.clone()),
        || Cursor::new(live_bytes),
        || &mut stdout,
        || &mut status,
    )
    .expect("inner_main process succeeded");
    stdout
}
