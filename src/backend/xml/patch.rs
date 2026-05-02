//! Byte-span patcher.
//!
//! A [`Patch`] replaces a contiguous byte range of the source with a
//! replacement byte slice. [`apply_patches`] applies a batch of patches
//! in descending byte-offset order, which keeps earlier offsets valid as
//! later regions are rewritten.
//!
//! Patches must not overlap. For our directives that is naturally true:
//! every directive resolves to a disjoint byte span (either an
//! attribute, an element, or a text region).

use anyhow::anyhow;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Patch {
    pub(crate) span: Range<usize>,
    pub(crate) replacement: Vec<u8>,
}

/// Apply the batch of `patches` to `source`, returning the rewritten
/// bytes.
///
/// Errors if two patches overlap.
pub(crate) fn apply_patches(source: &[u8], mut patches: Vec<Patch>) -> anyhow::Result<Vec<u8>> {
    // Sort by descending start so we can mutate in place from the end of
    // the buffer toward the beginning without invalidating earlier
    // offsets.
    patches.sort_by_key(|p| std::cmp::Reverse(p.span.start));

    // Overlap check after sorting: each patch's `end` must be `<=` the
    // next patch's `start`. (Sorted descending, so "the next" is the one
    // we already saw — which had a *larger* start.)
    for window in patches.windows(2) {
        let a = &window[0];
        let b = &window[1];
        // a has the larger start; b has the smaller start.
        if b.span.end > a.span.start {
            return Err(anyhow!(
                "XML patches overlap: spans {}..{} and {}..{}",
                b.span.start,
                b.span.end,
                a.span.start,
                a.span.end
            ));
        }
    }

    let mut out = source.to_vec();
    for patch in patches {
        if patch.span.end > out.len() {
            return Err(anyhow!(
                "XML patch span {}..{} exceeds source length {}",
                patch.span.start,
                patch.span.end,
                out.len()
            ));
        }
        out.splice(patch.span.clone(), patch.replacement.iter().copied());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn descending_order_application() {
        let src = b"abcdefghij";
        let patches = vec![
            Patch {
                span: 1..3,
                replacement: b"XX".to_vec(),
            },
            Patch {
                span: 6..8,
                replacement: b"YY".to_vec(),
            },
        ];
        let out = apply_patches(src, patches).unwrap();
        assert_eq!(out, b"aXXdefYYij");
    }

    #[test]
    fn replacement_byte_count_changes() {
        let src = b"abcdefghij";
        let patches = vec![
            Patch {
                span: 1..3,
                replacement: b"".to_vec(),
            },
            Patch {
                span: 6..8,
                replacement: b"WIDE".to_vec(),
            },
        ];
        let out = apply_patches(src, patches).unwrap();
        // "abcdefghij" -> remove 1..3 ("bc") -> "adefghij"
        // BUT: descending order means the later patch (6..8) goes first.
        // After replacing "gh" with "WIDE": "abcdefWIDEij"
        // Then remove 1..3 from original *positions* -> "aWIDEefij"... wait
        // we're operating on positions in the *original* buffer, applied
        // in descending order. After patch1 (6..8 -> WIDE): "abcdefWIDEij"
        // Then patch2 (1..3 -> ""): "adefWIDEij"
        assert_eq!(out, b"adefWIDEij");
    }

    #[test]
    fn empty_patch_list_returns_source() {
        let src = b"hello".to_vec();
        let out = apply_patches(&src, Vec::new()).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn out_of_bounds_rejected() {
        let src = b"abc";
        let patches = vec![Patch {
            span: 0..10,
            replacement: vec![],
        }];
        let err = apply_patches(src, patches).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn apply_patches_adjacent_spans_allowed_at_boundary() {
        // Spans `1..3` and `3..5` are adjacent (touch but do not
        // overlap). Both must apply.
        let src = b"abcdefgh";
        let patches = vec![
            Patch {
                span: 1..3,
                replacement: b"XX".to_vec(),
            },
            Patch {
                span: 3..5,
                replacement: b"YY".to_vec(),
            },
        ];
        let out = apply_patches(src, patches).unwrap();
        assert_eq!(out, b"aXXYYfgh");
    }

    #[test]
    fn single_patch_at_zero_offset() {
        // Patch starting at offset 0 must apply correctly.
        let src = b"hello";
        let patches = vec![Patch {
            span: 0..1,
            replacement: b"H".to_vec(),
        }];
        let out = apply_patches(src, patches).unwrap();
        assert_eq!(out, b"Hello");
    }

    #[test]
    fn single_patch_at_eof() {
        // Empty insertion at EOF (span len..len) must apply.
        let src = b"hello";
        let patches = vec![Patch {
            span: src.len()..src.len(),
            replacement: b"!".to_vec(),
        }];
        let out = apply_patches(src, patches).unwrap();
        assert_eq!(out, b"hello!");
    }

    #[test]
    fn apply_patches_overlapping_spans_errors() {
        // Two truly overlapping spans must error deterministically.
        let src = b"abcdefgh";
        let patches = vec![
            Patch {
                span: 1..4,
                replacement: vec![],
            },
            Patch {
                span: 3..6,
                replacement: vec![],
            },
        ];
        let err = apply_patches(src, patches).unwrap_err();
        assert!(
            err.to_string().contains("overlap"),
            "expected overlap error, got: {err}"
        );
    }
}
