//! Latency target: **< 1 ms** on a developer laptop.
//!
//! Workload: a KDE-style XML config of ~50 KB (~500 elements, ~2000
//! attributes) with eight directives applied:
//!
//! * 5 × `ignore path "..."`
//! * 3 × `add:hide path "..."` (driven through `Backend::filter`,
//!   which is the re-add hot path; a regression here matters as much
//!   as one in `process`).
//!
//! Benchmarked path: `Backend::process` with the five `ignore` paths.
//! XML edits are byte-span replacements: aside from tokenisation and
//! index build, the work is essentially memcpy. The < 1 ms target
//! reflects that — anything substantially slower means we've added a
//! quadratic somewhere.

use criterion::Criterion;
use criterion::black_box;
use criterion::criterion_group;
use criterion::criterion_main;

#[path = "common/mod.rs"]
mod common;

use common::BENCH_SAMPLE_SIZE;
use common::ProcessFixture;
use common::run_process;

/// Generate a KDE-style XML config: a `<Config>` root containing many
/// `<Group>` elements, each with several `<Entry>` children that carry
/// a handful of attributes. Tuned so the rendered document is roughly
/// 50 KB with ~500 elements and ~2000 attributes.
fn build_kde_xml() -> String {
    // 90 groups × 5 entries each = 450 entries + 90 group elements +
    // 1 root = 541 elements. Each entry carries 4 attributes (name,
    // type, hidden, value); each group carries 1 (name). 4*450 + 90 =
    // 1890 attributes — close enough to the 2000 target.
    let mut s = String::with_capacity(60 * 1024);
    use std::fmt::Write as _;
    s.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    s.push('\n');
    s.push_str("<Config>\n");
    for g in 0..90 {
        let _ = writeln!(s, "  <Group name=\"Group{g:03}\">");
        for e in 0..5 {
            // Pad the value so we hit the size target without making
            // the document degenerate (lots of identical short values
            // would compress better than realistic XML).
            let _ = writeln!(
                s,
                "    <Entry name=\"key_{g:03}_{e}\" type=\"string\" hidden=\"false\" \
                 value=\"value_{g:03}_{e}_padding_xxxxxxxxxxxxxxxxxxxx\"/>",
            );
        }
        s.push_str("  </Group>\n");
    }
    s.push_str("</Config>\n");
    s
}

fn bench_xml_kde(c: &mut Criterion) {
    let live = build_kde_xml();
    let source = live.clone(); // identity source — we exercise patches, not diffs.
    assert!(
        live.len() > 40_000 && live.len() < 80_000,
        "KDE-style XML fixture is ~50KB; got {} bytes",
        live.len()
    );

    // 5 × `ignore path` and 3 × `add:hide path`. The bench drives
    // `process` (which honours `ignore path`); `add:hide` lives on
    // the re-add path but is included in the script to exercise the
    // parser path that real KDE configs would use.
    let directives = "#!/bin/sh\n\
        language xml\n\
        ignore path \"/Config/Group[@name=\\\"Group001\\\"]/Entry[@name=\\\"key_001_0\\\"]/@value\"\n\
        ignore path \"/Config/Group[@name=\\\"Group010\\\"]/Entry[@name=\\\"key_010_2\\\"]/@value\"\n\
        ignore path \"/Config/Group[@name=\\\"Group045\\\"]/Entry[@name=\\\"key_045_3\\\"]/@value\"\n\
        ignore path \"/Config/Group[@name=\\\"Group070\\\"]/Entry[@name=\\\"key_070_4\\\"]/@value\"\n\
        ignore path \"/Config/Group[@name=\\\"Group089\\\"]/Entry[@name=\\\"key_089_1\\\"]/@value\"\n\
        add:hide path \"/Config/Group[@name=\\\"Group002\\\"]/Entry[@name=\\\"key_002_0\\\"]/@value\"\n\
        add:hide path \"/Config/Group[@name=\\\"Group020\\\"]/Entry[@name=\\\"key_020_1\\\"]/@value\"\n\
        add:hide path \"/Config/Group[@name=\\\"Group080\\\"]/Entry[@name=\\\"key_080_2\\\"]/@value\"\n";

    let fixture = ProcessFixture::inline(directives, source.as_bytes());

    let initial = run_process(&fixture, live.as_bytes());
    assert!(
        initial.len() > 40_000,
        "XML process produced surprisingly small output ({} bytes)",
        initial.len()
    );

    let mut group = c.benchmark_group("xml_patch");
    group.sample_size(BENCH_SAMPLE_SIZE);
    group.bench_function("kde_50kb_target_lt_1ms", |b| {
        b.iter(|| {
            let out = run_process(&fixture, black_box(live.as_bytes()));
            black_box(out);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_xml_kde);
criterion_main!(benches);
