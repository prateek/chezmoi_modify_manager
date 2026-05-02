//! Single-file mode parsing.
//!
//! A modify script can carry its source data inline. The first column-0
//! `---` line separates the directives (above) from the body (below).
//!
//! See the RFC at `docs/src/dev/xml_support_rfc.md` for the design.

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use camino::Utf8Path;
use camino::Utf8PathBuf;

use crate::backend::Language;

/// A modify script — either purely directives (sidecar mode) or directives
/// plus an inline body (single-file mode).
///
/// The directives portion is always UTF-8. The body, when present, is held
/// as raw bytes so that binary payloads (e.g. a future binary-plist
/// rejection diagnostic) can flow through without panicking.
#[derive(Debug, Clone)]
pub(crate) struct Script {
    /// The path of the script file on disk. Used for diagnostics and for
    /// resolving sibling source files in sidecar mode.
    pub(crate) script_path: Utf8PathBuf,
    /// The directive section: everything above the divider (or the whole
    /// file when no divider is present). Includes the trailing newline that
    /// precedes the divider, when one exists.
    pub(crate) directives: String,
    /// The inline body, if any. `None` in sidecar mode. Held as raw bytes;
    /// callers convert to `&str` themselves when the language requires text
    /// (e.g. INI, JSON, XML).
    pub(crate) body: Option<Vec<u8>>,
}

impl Script {
    /// Split raw modify-script content on the first column-0 `---` line.
    ///
    /// `raw` is treated as bytes. The directives portion (above the divider,
    /// or the whole file in sidecar mode) is required to be valid UTF-8;
    /// the body may be arbitrary bytes.
    ///
    /// Divider rules:
    ///
    /// * The divider is exactly `---` followed by `\n`, `\r\n`, or EOF.
    /// * It must be at column 0: either the start of the file or directly
    ///   after a `\n`.
    /// * Only the *first* such line is the divider; subsequent `---` lines
    ///   are body content.
    /// * The divider line itself is not included in either `directives` or
    ///   `body`.
    /// * `directives` includes the trailing newline that precedes the
    ///   divider (when present).
    /// * `body` does not include the leading newline of the divider; it
    ///   begins at the first byte after the `---\n` (or `---\r\n`).
    pub(crate) fn parse(raw: &[u8], script_path: &Utf8Path) -> anyhow::Result<Self> {
        let bytes = raw;
        let mut search_from = 0usize;

        // First pass: any column-0 `---` line that sits inside an open
        // `{{ ... }}` template block (possibly multi-line) is a phantom
        // divider hazard and is rejected. The same-line variant is
        // also caught here. We check the full raw input rather than the
        // post-split `directives` because the divider scan would
        // otherwise grab the offending `---` and put it in body before
        // we get a chance to look.
        reject_templated_triple_dash_in_raw(bytes, script_path)?;

        let (directives_bytes, body) = loop {
            // Find the next `---` candidate at column 0.
            let candidate = if search_from == 0 {
                if bytes.starts_with(b"---") {
                    Some(0usize)
                } else {
                    find_at_column_zero(bytes, 0)
                }
            } else {
                find_at_column_zero(bytes, search_from)
            };
            let Some(start) = candidate else {
                // No divider: pure sidecar mode.
                break (raw, None);
            };

            // Validate that it is exactly three dashes: the next byte must
            // be `\n`, `\r`, or EOF (i.e. not `-`, not anything else).
            let after = start + 3;
            let post = bytes.get(after).copied();
            match post {
                None => {
                    // `---` at EOF, with no trailing newline. Body is empty.
                    break (&raw[..start], Some(Vec::new()));
                }
                Some(b'\n') => {
                    let body_start = after + 1;
                    break (&raw[..start], Some(raw[body_start..].to_vec()));
                }
                Some(b'\r') => {
                    if bytes.get(after + 1).copied() == Some(b'\n') {
                        let body_start = after + 2;
                        break (&raw[..start], Some(raw[body_start..].to_vec()));
                    }
                    // `---\r` without `\n` — not a recognised divider
                    // line terminator. Treat as body content; advance.
                    search_from = start + 1;
                    continue;
                }
                Some(_) => {
                    // Not exactly three dashes (e.g. `----`) or trailing
                    // garbage. Not a divider; advance past this candidate.
                    search_from = start + 1;
                    continue;
                }
            }
        };

        let directives = std::str::from_utf8(directives_bytes)
            .with_context(|| format!("{script_path}: directives are not valid UTF-8"))?
            .to_owned();

        Ok(Self {
            script_path: script_path.to_owned(),
            directives,
            body,
        })
    }

    pub(crate) fn is_inline(&self) -> bool {
        self.body.is_some()
    }
}

/// Reject chezmoi-template constructs that could synthesise a phantom
/// divider after templating.
///
/// Two flavours:
///
/// 1. **Same-line:** a literal `---` and a `{{ ... }}` template on the
///    same line (e.g. `{{ if eq .os "linux" }}---{{ end }}`).
/// 2. **Multi-line control block:** a chezmoi control action
///    (`{{ if }}`, `{{ range }}`, `{{ with }}`, `{{ define }}`,
///    `{{ block }}`) that has not yet closed with a matching
///    `{{ end }}`, followed by a column-0 `---`. Adversarial example:
///    ```text
///    {{ if true }}
///    ---
///    {{ end }}
///    ```
///    The middle line would otherwise be selected as the divider
///    despite being template-conditional. Track control-block state
///    across lines so this case is caught.
///
/// We also track raw `{{` / `}}` balance for the case where an action
/// itself spans lines (e.g. `{{\nif true\n}}`). A `---` at column 0
/// while either kind of block is open is rejected.
///
/// Errors with the 1-based line number of the offending line.
fn reject_templated_triple_dash_in_raw(raw: &[u8], script_path: &Utf8Path) -> anyhow::Result<()> {
    // Non-UTF-8 raw input means the directives region (which must be
    // UTF-8) will fail downstream with a clearer error. We have nothing
    // useful to scan for in the malformed-byte case, so succeed here
    // and let the canonical UTF-8 check report.
    let Ok(text) = std::str::from_utf8(raw) else {
        return Ok(());
    };

    let mut delim_depth: usize = 0;
    let mut control_depth: usize = 0;
    let mut block_started_line: usize = 0;
    // When we enter a `{{` (possibly spanning multiple lines), buffer
    // every byte of the action body until the matching `}}` so that a
    // control keyword on an *intermediate* line — not just on the
    // opening or closing line — is still classified. Without this, an
    // adversarial input like
    //
    // ```
    // {{
    // if eq .os "linux"
    // }}
    // ---
    // ```
    //
    // sees the closing-line prefix as empty, never increments
    // `control_depth`, and lets a phantom `---` slip through.
    let mut action_buffer = String::new();

    for (idx, line) in text.split('\n').enumerate() {
        let lineno = idx + 1;

        // Same-line check: the historical rule. Any line that has both
        // `{{...}}` and `---` is rejected, regardless of block depth.
        if line.contains("{{") && line.contains("}}") && line.contains("---") {
            return Err(anyhow!(
                "{script_path}: line {lineno}: chezmoi template `{{{{ ... }}}}` co-occurs \
                 with a literal `---` on the same line; this could produce a phantom \
                 divider after templating. Move the divider out of the template, or \
                 rewrite so that the line does not contain both."
            ));
        }

        // Multi-line check: a column-0 `---` while either delimiters
        // are unbalanced or a control structure is still open is the
        // phantom-divider hazard.
        if (delim_depth > 0 || control_depth > 0) && line == "---" {
            let open = block_started_line;
            return Err(anyhow!(
                "{script_path}: line {lineno}: literal `---` at column 0 sits inside an \
                 open chezmoi template block opened on line {open}; the divider scan \
                 would treat this as the directives/body separator despite being \
                 template-conditional. Close the template block before the divider, or \
                 move the divider out of it."
            ));
        }

        // Walk the line balancing `{{` against `}}` and counting
        // chezmoi control actions (`if`, `range`, `with`, `define`,
        // `block`) against their matching `end`. We work on the action
        // body — the substring between each `{{` and `}}` — so spurious
        // matches inside a template literal value are unlikely.
        //
        // `delim_depth` tracks open `{{` across lines: a `{{` that
        // opened on a previous line continues to suppress action
        // classification until its matching `}}` is found, even if
        // that `}}` lives several lines later. Without the cross-line
        // accounting, an action that legitimately spans lines (e.g.
        // `{{\n  if true\n}}`) would never see its closing `}}` and
        // would erroneously veto every subsequent `---` line.
        let bytes = line.as_bytes();
        let mut i = 0;
        // If we entered this line already inside a `{{ ... }}` action
        // (opened on a previous line), the entire prefix up to the
        // first `}}` is action body. Capture that prefix and consume
        // through the closing `}}` before we resume normal scanning.
        if delim_depth > 0 {
            let mut closing = None;
            while i + 1 < bytes.len() {
                if bytes[i] == b'}' && bytes[i + 1] == b'}' {
                    closing = Some(i);
                    break;
                }
                i += 1;
            }
            if let Some(close_at) = closing {
                // Append the in-action prefix from this line, then
                // classify the *full* spanned action body (which may
                // include control keywords from intermediate lines).
                action_buffer.push_str(&line[..close_at]);
                classify_action(
                    &action_buffer,
                    &mut control_depth,
                    lineno,
                    &mut block_started_line,
                );
                action_buffer.clear();
                delim_depth = delim_depth.saturating_sub(1);
                i = close_at + 2;
            } else {
                // Still inside a `{{` block; entire line is action
                // body. Capture it (with a newline separator so token
                // splitting still finds keywords on subsequent lines)
                // and continue.
                action_buffer.push_str(line);
                action_buffer.push('\n');
                continue;
            }
        }
        while i < bytes.len() {
            if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
                if delim_depth == 0 && control_depth == 0 {
                    block_started_line = lineno;
                }
                delim_depth += 1;
                i += 2;
                // Find the matching `}}` (on this line) and inspect the
                // action body for control keywords.
                let action_start = i;
                while i + 1 < bytes.len() && !(bytes[i] == b'}' && bytes[i + 1] == b'}') {
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    let action = &line[action_start..i];
                    classify_action(action, &mut control_depth, lineno, &mut block_started_line);
                    delim_depth = delim_depth.saturating_sub(1);
                    i += 2;
                } else {
                    // Action opens here but its closing `}}` is on a
                    // later line. Buffer everything from `action_start`
                    // to end-of-line so an intermediate-line keyword
                    // still reaches `classify_action` when the action
                    // closes.
                    action_buffer.clear();
                    action_buffer.push_str(&line[action_start..]);
                    action_buffer.push('\n');
                    break;
                }
            } else {
                i += 1;
            }
        }
    }
    Ok(())
}

/// Inspect a chezmoi action body (the text between `{{` and `}}`) and
/// adjust the control-block depth accordingly. We look at the first
/// non-trim-marker token: `if`/`range`/`with`/`define`/`block` open a
/// new block, `end` closes the most recently opened one. `else` /
/// `else if` neither open nor close.
fn classify_action(
    body: &str,
    control_depth: &mut usize,
    lineno: usize,
    block_started_line: &mut usize,
) {
    let trimmed = body.trim_start_matches('-').trim_end_matches('-').trim();
    let first = trimmed.split_whitespace().next().unwrap_or("");
    match first {
        "if" | "range" | "with" | "define" | "block" => {
            if *control_depth == 0 {
                *block_started_line = lineno;
            }
            *control_depth += 1;
        }
        "end" => {
            *control_depth = control_depth.saturating_sub(1);
        }
        _ => {}
    }
}

/// Find the byte offset of the next `---` at column 0 (i.e. directly
/// preceded by `\n`), starting from `from`. Returns the offset of the first
/// `-`.
///
/// Backed by the `memchr` crate so that the inner newline scan is SIMD-
/// accelerated; total work is O(n) in the haystack length even when
/// dividers are absent.
fn find_at_column_zero(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= bytes.len() {
        // We need to find a `\n` such that the next three bytes are `---`.
        let rel = memchr::memchr(b'\n', &bytes[i..])?;
        let nl = i + rel;
        let after_nl = nl + 1;
        if after_nl + 3 <= bytes.len() && &bytes[after_nl..after_nl + 3] == b"---" {
            return Some(after_nl);
        }
        i = nl + 1;
    }
    None
}

/// The detected inline body format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InlineFormat {
    Ini,
    Json,
    Xml,
}

/// Detect the format of an inline body, based on the declared language.
///
/// `body` is raw bytes; format detection works on the byte stream (the
/// `bplist00` magic check, the first non-whitespace char check). Callers
/// that need to feed the body into a UTF-8 parser (INI/JSON/XML) should
/// validate UTF-8 themselves, at the boundary.
///
/// * `language ini` — always `InlineFormat::Ini`.
/// * `language xml` — body must start (after ASCII whitespace) with `<` or
///   be empty; otherwise an error.
/// * `language plist` — first non-whitespace `{` or `[` → JSON; `<` → XML;
///   anything else is an error. Binary plist (magic `bplist00`) is rejected.
pub(crate) fn detect_inline_format(
    language: Language,
    body: &[u8],
    script_path: &Utf8Path,
) -> anyhow::Result<InlineFormat> {
    // Strip a leading UTF-8 BOM (`EF BB BF`) before classifying. Some
    // editors and macOS tooling add it; without the strip, a body
    // beginning with BOM + `{` would fail format detection with a
    // confusing "expected `<` or `{`" diagnostic.
    let body = strip_utf8_bom(body);
    match language {
        Language::Ini => Ok(InlineFormat::Ini),
        Language::Xml => {
            let trimmed = body.trim_ascii_start();
            if trimmed.is_empty() {
                return Ok(InlineFormat::Xml);
            }
            if trimmed[0] == b'<' {
                Ok(InlineFormat::Xml)
            } else {
                let preview = byte_preview(trimmed, 16);
                Err(anyhow!(
                    "{script_path}: xml body: expected `<` (XML markup), got {preview:?}"
                ))
            }
        }
        Language::Plist => {
            // Reject binary plist on the *raw* body — leading whitespace
            // would not appear in a real binary plist anyway, but be
            // explicit about the magic.
            if body.starts_with(b"bplist00") {
                bail!(
                    "{script_path}: plist body: binary plist is not allowed inline; use a \
                     sidecar source"
                );
            }
            let trimmed = body.trim_ascii_start();
            if let Some(&first) = trimmed.first() {
                match first {
                    b'{' | b'[' => Ok(InlineFormat::Json),
                    b'<' => Ok(InlineFormat::Xml),
                    _ => {
                        let preview = byte_preview(trimmed, 16);
                        Err(anyhow!(
                            "{script_path}: plist body: expected JSON object/array or XML, got \
                             {preview:?}"
                        ))
                    }
                }
            } else {
                Err(anyhow!(
                    "{script_path}: plist body: expected JSON object/array or XML, got empty \
                     body"
                ))
            }
        }
    }
}

/// Skip a leading UTF-8 BOM if present.
fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

/// Take up to `n` bytes for a diagnostic preview. Lossy UTF-8 conversion
/// keeps non-ASCII payloads readable in error messages without panicking.
fn byte_preview(bytes: &[u8], n: usize) -> String {
    let take = bytes.len().min(n);
    String::from_utf8_lossy(&bytes[..take]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;
    use pretty_assertions::assert_eq;

    fn p() -> &'static Utf8Path {
        Utf8Path::new("modify_test.tmpl")
    }

    #[test]
    fn divider_at_start_splits() {
        let raw = b"---\nbody-line\n";
        let s = Script::parse(raw, p()).unwrap();
        assert_eq!(s.directives, "");
        assert_eq!(s.body.as_deref(), Some(&b"body-line\n"[..]));
        assert!(s.is_inline());
    }

    #[test]
    fn divider_after_directives() {
        let raw = b"language ini\nsource auto\n---\n[section]\nkey=val\n";
        let s = Script::parse(raw, p()).unwrap();
        assert_eq!(s.directives, "language ini\nsource auto\n");
        assert_eq!(s.body.as_deref(), Some(&b"[section]\nkey=val\n"[..]));
    }

    #[test]
    fn no_divider_is_sidecar() {
        let raw = b"language ini\nsource auto\n";
        let s = Script::parse(raw, p()).unwrap();
        assert_eq!(s.directives.as_bytes(), raw);
        assert!(s.body.is_none());
        assert!(!s.is_inline());
    }

    #[test]
    fn crlf_divider() {
        let raw = b"language ini\r\n---\r\nbody\r\n";
        let s = Script::parse(raw, p()).unwrap();
        assert_eq!(s.directives, "language ini\r\n");
        assert_eq!(s.body.as_deref(), Some(&b"body\r\n"[..]));
    }

    #[test]
    fn divider_must_be_column_zero() {
        // `  ---` is indented; not a divider.
        let raw = b"language ini\n  ---\nstill directives\n";
        let s = Script::parse(raw, p()).unwrap();
        assert_eq!(s.directives.as_bytes(), raw);
        assert!(s.body.is_none());
    }

    #[test]
    fn divider_must_be_exactly_three_dashes() {
        let raw = b"language ini\n----\nnot a divider\n";
        let s = Script::parse(raw, p()).unwrap();
        assert_eq!(s.directives.as_bytes(), raw);
        assert!(s.body.is_none());
    }

    #[test]
    fn subsequent_divider_is_body_content() {
        let raw = b"language ini\n---\nfirst body line\n---\nstill body\n";
        let s = Script::parse(raw, p()).unwrap();
        assert_eq!(s.directives, "language ini\n");
        assert_eq!(
            s.body.as_deref(),
            Some(&b"first body line\n---\nstill body\n"[..])
        );
    }

    #[test]
    fn body_at_eof_no_trailing_newline_ok() {
        // `---` at EOF, no trailing newline, body is empty.
        let raw = b"language ini\n---";
        let s = Script::parse(raw, p()).unwrap();
        assert_eq!(s.directives, "language ini\n");
        assert_eq!(s.body.as_deref(), Some(&b""[..]));
    }

    #[test]
    fn body_can_be_non_utf8() {
        // The directives portion is UTF-8, but the body may be arbitrary
        // bytes (this is the slice-7 binary-plist diagnostic path).
        let mut raw = b"language plist\n---\n".to_vec();
        raw.extend_from_slice(b"bplist00\xff\xfe\x00\x01");
        let s = Script::parse(&raw, p()).unwrap();
        assert_eq!(s.directives, "language plist\n");
        assert_eq!(s.body.as_deref(), Some(&b"bplist00\xff\xfe\x00\x01"[..]));
    }

    #[test]
    fn directives_must_be_utf8() {
        // Invalid UTF-8 in the directives region is an error.
        let raw: Vec<u8> = vec![0xff, 0xfe, b'\n'];
        let err = Script::parse(&raw, p()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not valid UTF-8"), "msg={msg}");
    }

    #[test]
    fn comment_with_three_dashes_is_not_divider() {
        // A `# ---` line is a comment (column 0 is `#`), not a divider.
        let raw = b"language ini\n# ---\nsource auto\n";
        let s = Script::parse(raw, p()).unwrap();
        assert_eq!(s.directives.as_bytes(), raw);
        assert!(s.body.is_none(), "should remain in sidecar mode");
    }

    #[test]
    fn mixed_crlf_lf_in_same_file() {
        // Directives use LF, body uses CRLF — splits correctly.
        let raw = b"language ini\nsource auto\n---\nbody\r\nmore\r\n";
        let s = Script::parse(raw, p()).unwrap();
        assert_eq!(s.directives, "language ini\nsource auto\n");
        assert_eq!(s.body.as_deref(), Some(&b"body\r\nmore\r\n"[..]));
    }

    #[test]
    fn bom_at_file_start_is_preserved() {
        // We do not strip a UTF-8 BOM. The BOM byte sequence appears
        // verbatim in `directives`. Document this behaviour: a BOM at the
        // start of the directives region is *not* supported by the
        // directive parser (which expects a directive keyword), so the
        // overall parse will error downstream — but `Script::parse` itself
        // succeeds since the bytes are valid UTF-8.
        let mut raw = Vec::new();
        raw.extend_from_slice("\u{FEFF}".as_bytes());
        raw.extend_from_slice(b"source auto\n");
        let s = Script::parse(&raw, p()).unwrap();
        assert!(s.directives.starts_with('\u{FEFF}'));
        assert!(s.body.is_none());
    }

    #[test]
    fn templated_triple_dash_rejected() {
        // A chezmoi-template block in the directives region containing a
        // literal `---` is rejected with the line number of the offending
        // `{{`.
        let raw = b"language ini\n{{ if eq .os \"linux\" }}---{{ end }}\nsource auto\n";
        let err = Script::parse(raw, p()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("line 2"), "msg={msg}");
        assert!(
            msg.contains("template") || msg.contains("phantom divider"),
            "msg={msg}"
        );
    }

    #[test]
    fn templated_multiline_block_rejects_phantom_divider() {
        // Adversarial multi-line template: the `{{ if true }}` opens on
        // line 1, an unguarded column-0 `---` sits on line 2, and the
        // matching `{{ end }}` closes on line 3. Without multi-line
        // tracking, the divider scan would adopt line 2 as the
        // directives/body separator, despite that line being template-
        // conditional.
        let raw = b"{{ if true }}\n---\n{{ end }}\nlanguage ini\n";
        let err = Script::parse(raw, p()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("line 2"), "msg={msg}");
        assert!(
            msg.contains("template") || msg.contains("phantom") || msg.contains("inside"),
            "msg={msg}"
        );
    }

    #[test]
    fn templated_opener_block_phantom_divider_rejected() {
        // Each control-block opener (`range`, `with`, `define`, `block`)
        // must be treated like `if`: a column-0 `---` while the block is
        // open is a phantom-divider hazard. chezmoi's `{{ block }}`
        // action is included for parity with the other openers.
        let cases: &[(&str, &[u8])] = &[
            (
                "range",
                b"{{ range $x := .y }}\n---\n{{ end }}\nlanguage ini\n",
            ),
            ("with", b"{{ with .x }}\n---\n{{ end }}\nlanguage ini\n"),
            (
                "define",
                b"{{ define \"foo\" }}\n---\n{{ end }}\nlanguage ini\n",
            ),
            (
                "block",
                b"{{ block \"foo\" . }}\n---\n{{ end }}\nlanguage ini\n",
            ),
        ];
        for (name, raw) in cases {
            let err = Script::parse(raw, p())
                .err()
                .unwrap_or_else(|| panic!("expected error for opener {name}"));
            let msg = format!("{err}");
            assert!(msg.contains("line 2"), "opener={name} msg={msg}");
            assert!(
                msg.contains("template") || msg.contains("phantom") || msg.contains("inside"),
                "opener={name} msg={msg}"
            );
        }
    }

    #[test]
    fn templated_nested_blocks_phantom_divider_rejected() {
        // Two nested openers on one line, then a column-0 `---` while
        // both are still open. The scan must keep depth > 0 and reject.
        let raw = b"{{ if a }}{{ if b }}\n---\n{{ end }}{{ end }}\nlanguage ini\n";
        let err = Script::parse(raw, p()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("line 2"), "msg={msg}");
        assert!(
            msg.contains("template") || msg.contains("phantom") || msg.contains("inside"),
            "msg={msg}"
        );
    }

    #[test]
    fn templated_unbalanced_end_handled_gracefully() {
        // Bare `{{ end }}` with no opener exercises the saturating_sub
        // branch in `classify_action` (depth was 0, stays 0). The
        // overall parse must succeed (no phantom-divider hazard) and
        // any parse error must come from the directive parser, not the
        // scanner.
        let raw = b"language ini\n{{ end }}\nsource auto\n";
        // `Script::parse` itself only runs the divider/template scan;
        // it does not invoke the directive parser. So the unbalanced
        // `{{ end }}` should not raise here.
        let _ = Script::parse(raw, p()).unwrap();
    }

    #[test]
    fn detect_inline_format_ini() {
        let r = detect_inline_format(Language::Ini, b"[s]\nk=v\n", p()).unwrap();
        assert_eq!(r, InlineFormat::Ini);
    }

    #[test]
    fn detect_inline_format_json_object() {
        let r = detect_inline_format(Language::Plist, b"  \n{\"a\": 1}", p()).unwrap();
        assert_eq!(r, InlineFormat::Json);
    }

    #[test]
    fn detect_inline_format_json_array() {
        let r = detect_inline_format(Language::Plist, b"[1, 2, 3]", p()).unwrap();
        assert_eq!(r, InlineFormat::Json);
    }

    #[test]
    fn detect_inline_format_xml_plist() {
        let r = detect_inline_format(
            Language::Plist,
            b"<?xml version=\"1.0\"?>\n<plist></plist>",
            p(),
        )
        .unwrap();
        assert_eq!(r, InlineFormat::Xml);
    }

    #[test]
    fn detect_inline_format_plist_rejects_binary() {
        let err = detect_inline_format(Language::Plist, b"bplist00\x00", p()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("binary plist"), "msg={msg}");
        assert!(msg.contains("modify_test.tmpl"), "msg={msg}");
    }

    #[test]
    fn divider_scan_is_linear_on_large_input() {
        // Sanity-check: a 64KiB body with no divider must parse near
        // instantaneously. Asserts <50 ms to catch a regression to a
        // pathological scan; the SIMD-accelerated `memchr` path completes
        // in microseconds in practice.
        let mut raw: Vec<u8> = Vec::with_capacity(64 * 1024 + 32);
        raw.extend_from_slice(b"language ini\n");
        // Many `\n` boundaries to exercise the inner loop, none followed
        // by `---`.
        for _ in 0..(64 * 1024 / 32) {
            raw.extend_from_slice(b"abcdefghijklmnopqrstuvwxyz01234\n");
        }
        let start = std::time::Instant::now();
        let s = Script::parse(&raw, p()).unwrap();
        let elapsed = start.elapsed();
        assert!(s.body.is_none(), "expected sidecar mode (no divider)");
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "divider scan took {elapsed:?}, expected <50ms",
        );
    }

    #[test]
    fn detect_inline_format_strips_bom_before_json_check() {
        // A UTF-8 BOM (`EF BB BF`) preceding a JSON object body must
        // not break format detection. Previous behaviour:
        // the BOM was the first non-whitespace byte and the matcher
        // emitted a confusing "expected `<` or `{`" diagnostic.
        // Regression for round-2 fix #7.
        let mut body = Vec::new();
        body.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        body.extend_from_slice(b"{\"a\": 1}");
        let r = detect_inline_format(Language::Plist, &body, p()).unwrap();
        assert_eq!(r, InlineFormat::Json);

        // Same with leading whitespace after the BOM.
        let mut body = Vec::new();
        body.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        body.extend_from_slice(b"   \n[1, 2]");
        let r = detect_inline_format(Language::Plist, &body, p()).unwrap();
        assert_eq!(r, InlineFormat::Json);
    }

    #[test]
    fn templated_action_spanning_lines_followed_by_legitimate_divider() {
        // An action that spans lines (`{{\n  if true\n}}`) closes on
        // line 3, the matching `{{ end }}` closes the control block on
        // line 5, and the divider on line 7 is genuinely outside the
        // template. Previous behaviour: `delim_depth` never decremented
        // because the closing `}}` lived on a different line from its
        // opener, so the divider scan permanently rejected any later
        // `---`. New behaviour: cross-line `delim_depth` accounting
        // correctly closes the action and the legitimate divider
        // succeeds.
        let raw = b"{{\n  if true\n}}\nlanguage ini\n{{ end }}\n\n---\nbody-content\n";
        let s = Script::parse(raw, p()).expect(
            "multi-line `{{ ... }}` action followed by an outside-the-block divider must succeed",
        );
        assert_eq!(s.body.as_deref(), Some(&b"body-content\n"[..]));
    }

    #[test]
    fn templated_action_with_control_keyword_on_intermediate_line_rejected_phantom_divider() {
        // Round-3 regression: with `{{` and `}}` on different lines, the
        // closing-line prefix passed to `classify_action` was empty, so
        // an `if` keyword on an intermediate line never incremented
        // `control_depth`. A subsequent column-0 `---` then became the
        // divider despite being inside an open template control block.
        // We now buffer the full action body across lines and classify
        // the spanned text, catching the keyword wherever it appears.
        let raw = b"{{\nif eq .os \"linux\"\n}}\n---\nbody\n{{ end }}\n";
        let err = Script::parse(raw, p()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("template") || msg.contains("phantom") || msg.contains("inside"),
            "expected phantom-divider rejection, got: {msg}"
        );
        // The opening `{{` is on line 1; the diagnostic should flag the
        // `---` on line 4 (or at minimum mention an open block).
        assert!(msg.contains("line 4"), "msg={msg}");
    }

    #[test]
    fn detect_inline_format_xml_must_be_xml() {
        let err = detect_inline_format(Language::Xml, b"not xml", p()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("xml body"), "msg={msg}");
        assert!(msg.contains("modify_test.tmpl"), "msg={msg}");
    }
}
