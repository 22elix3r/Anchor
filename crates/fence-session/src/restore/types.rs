use fence_core::{ConflictReason, ManifestId, NativeRelativePath, NoChangeReason, ObjectId};

use crate::SessionId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestoreApplyResult {
    Applied {
        session_id: SessionId,
        path: NativeRelativePath,
        merged: bool,
    },
    TextMergeAvailable {
        session_id: SessionId,
        path: NativeRelativePath,
        current_object: ObjectId,
        current_raw_size: u64,
        merged_object: ObjectId,
        merged_raw_size: u64,
    },
    NoChange {
        reason: NoChangeReason,
    },
    Conflict {
        reason: ConflictReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexRestoreResult {
    Applied,
    NoChange,
    Conflict,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TextMergeMode {
    #[default]
    Disabled,
    Preview,
    Apply {
        expected_object: ObjectId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WholeRestoreMode {
    Preview,
    Apply { expected_current: ManifestId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WholeRestoreResult {
    Preview {
        current_manifest: ManifestId,
        writes: u64,
        no_changes: u64,
    },
    Applied {
        paths: u64,
    },
    Conflicts {
        conflicts: Vec<WholeRestoreConflict>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WholeRestoreConflict {
    pub path: NativeRelativePath,
    pub reason: ConflictReason,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TransactionRecoveryReport {
    pub rolled_back: Vec<String>,
    pub completed: Vec<String>,
    pub skipped_other_worktrees: u64,
}
