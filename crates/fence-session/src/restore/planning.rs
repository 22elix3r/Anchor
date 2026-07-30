use fence_core::{
    ConflictReason, Manifest, ManifestEntry, ManifestNode, NativeRelativePath, ObjectId,
    ObservedKind, RestoreConflict, ScopeClassifier, ScopeDecision, ScopeError, TextMergeConflict,
    TextMergeLimits, TextMergeResult, inverse_three_way_text_merge,
};
use fence_git::{GitContext, IndexCapture};

use super::RestoreError;
use crate::SessionStore;

pub(super) struct MergeCandidate {
    pub(super) desired: ManifestEntry,
    pub(super) base_object: ObjectId,
    pub(super) session_object: ObjectId,
    pub(super) current_object: ObjectId,
    pub(super) current_raw_size: u64,
    pub(super) merged_object: ObjectId,
    pub(super) merged_raw_size: u64,
}

pub(super) enum MergeResolution {
    Clean(Box<MergeCandidate>),
    Conflict(ConflictReason),
}

pub(super) fn merge_regular_conflict(
    conflict: &RestoreConflict,
    store: &SessionStore,
) -> Result<MergeResolution, RestoreError> {
    let (Some(base), Some(session), Some(current)) =
        (&conflict.base, &conflict.session, &conflict.current)
    else {
        return Ok(MergeResolution::Conflict(
            ConflictReason::TextMergeUnsupported,
        ));
    };
    let (
        ManifestNode::Regular {
            object: base_object,
            raw_size: base_size,
            unix_exec_bits: base_unix_mode,
            windows_readonly: base_windows_readonly,
        },
        ManifestNode::Regular {
            object: session_object,
            raw_size: session_size,
            unix_exec_bits: session_unix_mode,
            windows_readonly: session_windows_readonly,
        },
        ManifestNode::Regular {
            object: current_object,
            raw_size: current_size,
            unix_exec_bits: current_unix_mode,
            windows_readonly: current_windows_readonly,
        },
    ) = (&base.node, &session.node, &current.node)
    else {
        return Ok(MergeResolution::Conflict(
            ConflictReason::TextMergeUnsupported,
        ));
    };
    let Some(desired_unix_mode) =
        inverse_scalar(*base_unix_mode, *session_unix_mode, *current_unix_mode)
    else {
        return Ok(MergeResolution::Conflict(ConflictReason::ModeDrifted));
    };
    let Some(desired_windows_readonly) = inverse_scalar(
        *base_windows_readonly,
        *session_windows_readonly,
        *current_windows_readonly,
    ) else {
        return Ok(MergeResolution::Conflict(ConflictReason::ModeDrifted));
    };
    let limits = TextMergeLimits::default();
    let base_bytes = store.objects().get(*base_object, *base_size)?;
    let session_bytes = store.objects().get(*session_object, *session_size)?;
    let current_bytes = store.objects().get(*current_object, *current_size)?;
    match inverse_three_way_text_merge(&base_bytes, &session_bytes, &current_bytes, limits)? {
        TextMergeResult::Clean(bytes) => {
            let merged_raw_size =
                u64::try_from(bytes.len()).map_err(|_| RestoreError::MergedFileTooLarge)?;
            let merged_object = store.objects().put_bytes(&bytes)?;
            Ok(MergeResolution::Clean(Box::new(MergeCandidate {
                desired: ManifestEntry {
                    path: current.path.clone(),
                    node: ManifestNode::Regular {
                        object: merged_object,
                        raw_size: merged_raw_size,
                        unix_exec_bits: desired_unix_mode,
                        windows_readonly: desired_windows_readonly,
                    },
                    safety: current.safety.clone(),
                },
                base_object: *base_object,
                session_object: *session_object,
                current_object: *current_object,
                current_raw_size: *current_size,
                merged_object,
                merged_raw_size,
            })))
        }
        TextMergeResult::Conflict(reason) => Ok(MergeResolution::Conflict(match reason {
            TextMergeConflict::OverlappingEdits => ConflictReason::TextMergeOverlaps,
            TextMergeConflict::InputTooLarge | TextMergeConflict::OutputTooLarge => {
                ConflictReason::TextMergeTooLarge
            }
            TextMergeConflict::NotUtf8 | TextMergeConflict::ContainsNul => {
                ConflictReason::TextMergeUnsupported
            }
        })),
    }
}

fn inverse_scalar<T: Copy + Eq>(base: T, session: T, current: T) -> Option<T> {
    if base == session || current == base {
        Some(current)
    } else if current == session {
        Some(base)
    } else {
        None
    }
}

pub(super) fn index_is_split(index: &IndexCapture) -> bool {
    matches!(
        index,
        IndexCapture::Present {
            summary: fence_git::IndexSummary {
                split_index: true,
                ..
            },
            ..
        }
    )
}

pub(super) struct SelectedScope {
    pub(super) selected: NativeRelativePath,
    pub(super) expected_kind: Option<ObservedKind>,
}

impl ScopeClassifier for SelectedScope {
    fn classify(
        &self,
        path: &NativeRelativePath,
        kind: ObservedKind,
    ) -> Result<ScopeDecision, ScopeError> {
        if path == &self.selected {
            if self.expected_kind.is_some_and(|expected| expected != kind) {
                return Ok(ScopeDecision::Boundary(
                    fence_core::OmissionReason::UnsupportedType,
                ));
            }
            return Ok(ScopeDecision::Include);
        }
        if self.selected.components().starts_with(path.components()) {
            return Ok(ScopeDecision::Include);
        }
        Ok(ScopeDecision::Exclude)
    }
}

pub(super) fn node_kind(node: &ManifestNode) -> ObservedKind {
    match node {
        ManifestNode::Regular { .. } => ObservedKind::Regular,
        ManifestNode::Symlink { .. } => ObservedKind::Symlink,
        ManifestNode::EmptyDirectory => ObservedKind::Directory,
    }
}

pub(super) fn validate_manifest_mutation_paths(
    context: &GitContext,
    policy: &fence_git::FrozenGitPolicy,
    manifest: &Manifest,
) -> Result<(), RestoreError> {
    for entry in manifest.entries() {
        validate_mutation_path(context, policy, &entry.path)?;
    }
    Ok(())
}

pub(super) fn validate_mutation_path(
    context: &GitContext,
    policy: &fence_git::FrozenGitPolicy,
    path: &NativeRelativePath,
) -> Result<(), RestoreError> {
    if context.is_protected_mutation_path(policy, path) {
        return Err(RestoreError::ProtectedWorktreePath(path.clone()));
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct BatchWrite {
    pub(super) path: NativeRelativePath,
    pub(super) expected: Option<ManifestEntry>,
    pub(super) desired: Option<ManifestEntry>,
}
