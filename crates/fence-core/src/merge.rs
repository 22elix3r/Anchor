use std::cmp::{max, min};
use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextMergeLimits {
    pub max_input_bytes: u64,
    pub max_output_bytes: u64,
}

impl Default for TextMergeLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 8 * 1024 * 1024,
            max_output_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextMergeResult {
    Clean(Vec<u8>),
    Conflict(TextMergeConflict),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextMergeConflict {
    InputTooLarge,
    OutputTooLarge,
    NotUtf8,
    ContainsNul,
    OverlappingEdits,
}

/// Merge an inverse session edit into current text without conflict markers.
///
/// The merge ancestor is `session`, the inverse target is `base`, and the post-session side is
/// `current`. Only non-overlapping line edits are combined. A conflict is structured and never
/// embedded into the returned bytes.
///
/// # Errors
///
/// Returns [`TextMergeError`] only for internal range conversion or accounting failures.
pub fn inverse_three_way_text_merge(
    base: &[u8],
    session: &[u8],
    current: &[u8],
    limits: TextMergeLimits,
) -> Result<TextMergeResult, TextMergeError> {
    if [base, session, current].iter().any(|bytes| {
        u64::try_from(bytes.len()).map_or(true, |length| length > limits.max_input_bytes)
    }) {
        return Ok(TextMergeResult::Conflict(TextMergeConflict::InputTooLarge));
    }
    if base
        .iter()
        .chain(session)
        .chain(current)
        .any(|byte| *byte == 0)
    {
        return Ok(TextMergeResult::Conflict(TextMergeConflict::ContainsNul));
    }
    let (Ok(base), Ok(session), Ok(current)) = (
        std::str::from_utf8(base),
        std::str::from_utf8(session),
        std::str::from_utf8(current),
    ) else {
        return Ok(TextMergeResult::Conflict(TextMergeConflict::NotUtf8));
    };

    let inverse = calculate_edits(session, base)?;
    let post_session = calculate_edits(session, current)?;
    let mut combined = inverse.clone();
    for current_edit in post_session {
        let mut duplicate = false;
        for inverse_edit in &inverse {
            if inverse_edit == &current_edit {
                duplicate = true;
                break;
            }
            if edits_overlap(inverse_edit, &current_edit) {
                return Ok(TextMergeResult::Conflict(
                    TextMergeConflict::OverlappingEdits,
                ));
            }
        }
        if !duplicate {
            combined.push(current_edit);
        }
    }
    combined.sort_by(|left, right| {
        left.ancestor
            .start
            .cmp(&right.ancestor.start)
            .then_with(|| left.ancestor.end.cmp(&right.ancestor.end))
    });

    let offsets = line_offsets(session.as_bytes());
    let mut output = Vec::new();
    let mut cursor = 0_usize;
    for edit in combined {
        let start = offset(&offsets, edit.ancestor.start)?;
        let end = offset(&offsets, edit.ancestor.end)?;
        if start < cursor {
            return Ok(TextMergeResult::Conflict(
                TextMergeConflict::OverlappingEdits,
            ));
        }
        output.extend_from_slice(&session.as_bytes()[cursor..start]);
        output.extend_from_slice(&edit.replacement);
        cursor = end;
        if u64::try_from(output.len()).map_or(true, |length| length > limits.max_output_bytes) {
            return Ok(TextMergeResult::Conflict(TextMergeConflict::OutputTooLarge));
        }
    }
    output.extend_from_slice(&session.as_bytes()[cursor..]);
    if u64::try_from(output.len()).map_or(true, |length| length > limits.max_output_bytes) {
        return Ok(TextMergeResult::Conflict(TextMergeConflict::OutputTooLarge));
    }
    Ok(TextMergeResult::Clean(output))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Edit {
    ancestor: Range<usize>,
    replacement: Vec<u8>,
}

fn calculate_edits(ancestor: &str, variant: &str) -> Result<Vec<Edit>, TextMergeError> {
    let input = InternedInput::new(ancestor, variant);
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let variant_offsets = line_offsets(variant.as_bytes());
    diff.hunks()
        .map(|hunk| {
            let after_start =
                usize::try_from(hunk.after.start).map_err(|_| TextMergeError::Range)?;
            let after_end = usize::try_from(hunk.after.end).map_err(|_| TextMergeError::Range)?;
            let start = offset(&variant_offsets, after_start)?;
            let end = offset(&variant_offsets, after_end)?;
            Ok(Edit {
                ancestor: usize::try_from(hunk.before.start).map_err(|_| TextMergeError::Range)?
                    ..usize::try_from(hunk.before.end).map_err(|_| TextMergeError::Range)?,
                replacement: variant.as_bytes()[start..end].to_vec(),
            })
        })
        .collect()
}

fn edits_overlap(left: &Edit, right: &Edit) -> bool {
    match (left.ancestor.is_empty(), right.ancestor.is_empty()) {
        (true, true) => left.ancestor.start == right.ancestor.start,
        (true, false) => {
            left.ancestor.start > right.ancestor.start && left.ancestor.start < right.ancestor.end
        }
        (false, true) => {
            right.ancestor.start > left.ancestor.start && right.ancestor.start < left.ancestor.end
        }
        (false, false) => {
            max(left.ancestor.start, right.ancestor.start)
                < min(left.ancestor.end, right.ancestor.end)
        }
    }
}

fn line_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    if offsets.last().copied() != Some(bytes.len()) {
        offsets.push(bytes.len());
    }
    offsets
}

fn offset(offsets: &[usize], line: usize) -> Result<usize, TextMergeError> {
    offsets.get(line).copied().ok_or(TextMergeError::Range)
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TextMergeError {
    #[error("text diff produced an invalid line range")]
    Range,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combines_disjoint_inverse_and_post_session_edits() {
        let result = inverse_three_way_text_merge(
            b"one\nbase\ntail\n",
            b"one\nsession\ntail\n",
            b"current-prefix\nsession\ntail\n",
            TextMergeLimits::default(),
        )
        .unwrap();
        assert_eq!(
            result,
            TextMergeResult::Clean(b"current-prefix\nbase\ntail\n".to_vec())
        );
    }

    #[test]
    fn refuses_overlapping_edits_without_markers() {
        let result = inverse_three_way_text_merge(
            b"one\nbase\n",
            b"one\nsession\n",
            b"one\npost-session\n",
            TextMergeLimits::default(),
        )
        .unwrap();
        assert_eq!(
            result,
            TextMergeResult::Conflict(TextMergeConflict::OverlappingEdits)
        );
    }

    #[test]
    fn combines_insertions_at_adjacent_boundaries() {
        let result = inverse_three_way_text_merge(
            b"inverse\nsame\n",
            b"same\n",
            b"same\ncurrent\n",
            TextMergeLimits::default(),
        )
        .unwrap();
        assert_eq!(
            result,
            TextMergeResult::Clean(b"inverse\nsame\ncurrent\n".to_vec())
        );
    }

    #[test]
    fn refuses_binary_and_oversized_input() {
        assert_eq!(
            inverse_three_way_text_merge(b"a\0", b"b\0", b"c\0", TextMergeLimits::default())
                .unwrap(),
            TextMergeResult::Conflict(TextMergeConflict::ContainsNul)
        );
        assert_eq!(
            inverse_three_way_text_merge(
                b"aa",
                b"bb",
                b"cc",
                TextMergeLimits {
                    max_input_bytes: 1,
                    max_output_bytes: 10,
                },
            )
            .unwrap(),
            TextMergeResult::Conflict(TextMergeConflict::InputTooLarge)
        );
    }

    #[test]
    fn preserves_missing_final_newline() {
        let result = inverse_three_way_text_merge(
            b"base",
            b"session",
            b"prefix\nsession",
            TextMergeLimits::default(),
        )
        .unwrap();
        assert_eq!(result, TextMergeResult::Clean(b"prefix\nbase".to_vec()));
    }
}
