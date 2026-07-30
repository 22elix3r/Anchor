use std::io::Write as _;
use std::path::Path;

use anchor_core::{ManifestEntry, ManifestNode, NativeRelativePath, ObjectId};
use anchor_git::IndexCapture;
use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::RestoreError;
use crate::SessionId;
use crate::restore_plan::{PlanSafety, RestorePlanId};

pub(super) const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
pub(super) const BATCH_JOURNAL_TAG: u64 = 0x414e_4348_4f52_424a;
pub(super) const FILE_JOURNAL_SCHEMA: u16 = 5;
pub(super) const BATCH_JOURNAL_SCHEMA: u16 = 4;

pub(super) fn save_journal(path: &Path, journal: &RestoreJournal) -> Result<(), RestoreError> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(journal, &mut bytes)
        .map_err(|error| RestoreError::Journal(error.to_string()))?;
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&bytes)?;
    file.commit()?;
    Ok(())
}

pub(super) fn save_index_journal(
    path: &Path,
    journal: &IndexRestoreJournal,
) -> Result<(), RestoreError> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(journal, &mut bytes)
        .map_err(|error| RestoreError::Journal(error.to_string()))?;
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&bytes)?;
    file.commit()?;
    Ok(())
}

pub(super) fn save_batch_journal(
    path: &Path,
    journal: &BatchRestoreJournal,
) -> Result<(), RestoreError> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(journal, &mut bytes)
        .map_err(|error| RestoreError::Journal(error.to_string()))?;
    if bytes.len() > usize::try_from(MAX_JOURNAL_BYTES).unwrap_or(usize::MAX) {
        return Err(RestoreError::JournalTooLarge(path.to_path_buf()));
    }
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&bytes)?;
    file.commit()?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct RestoreJournal {
    pub(super) schema: u16,
    pub(super) session_id: SessionId,
    #[serde(default)]
    pub(super) plan_id: Option<RestorePlanId>,
    #[serde(default)]
    pub(super) transaction_id: Option<Uuid>,
    pub(super) path: NativeRelativePath,
    pub(super) stage_name: String,
    #[serde(default)]
    pub(super) backup_name: Option<String>,
    #[serde(default)]
    pub(super) worktree_root: Option<anchor_core::NativeString>,
    #[serde(default)]
    pub(super) worktree_key: Option<String>,
    #[serde(default)]
    pub(super) expected: Option<JournalPresence>,
    #[serde(default)]
    pub(super) desired: Option<JournalPresence>,
    pub(super) state: JournalState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct IndexRestoreJournal {
    pub(super) schema: u16,
    pub(super) session_id: SessionId,
    #[serde(default)]
    pub(super) plan_id: Option<RestorePlanId>,
    #[serde(default)]
    pub(super) transaction_id: Option<Uuid>,
    #[serde(default)]
    pub(super) backup_name: Option<String>,
    #[serde(default)]
    pub(super) worktree_key: Option<String>,
    #[serde(default)]
    pub(super) index_path: Option<anchor_core::NativeString>,
    #[serde(default)]
    pub(super) expected: Option<IndexCapture>,
    #[serde(default)]
    pub(super) desired: Option<IndexCapture>,
    pub(super) state: JournalState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct BatchRestoreJournal {
    pub(super) tag: u64,
    pub(super) schema: u16,
    pub(super) session_id: SessionId,
    #[serde(default)]
    pub(super) plan_id: Option<RestorePlanId>,
    #[serde(default)]
    pub(super) transaction_id: Option<Uuid>,
    pub(super) worktree_root: anchor_core::NativeString,
    pub(super) worktree_key: String,
    pub(super) state: BatchJournalState,
    pub(super) items: Vec<BatchJournalItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct BatchJournalItem {
    pub(super) path: NativeRelativePath,
    pub(super) stage_name: String,
    pub(super) backup_name: String,
    pub(super) expected: JournalPresence,
    pub(super) desired: JournalPresence,
    pub(super) state: BatchItemState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) enum BatchJournalState {
    Prepared,
    Evacuating,
    Installing,
    Verified,
    Cleaning,
    CleanupComplete,
    Complete,
    NeedsRecovery,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) enum BatchItemState {
    Prepared,
    Staged,
    Evacuated,
    Installed,
    Verified,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum JournalNode {
    Regular {
        object: ObjectId,
        raw_size: u64,
        unix_exec_bits: Option<u8>,
    },
    Symlink {
        target: anchor_core::NativeString,
        windows_link_kind: Option<anchor_core::WindowsSymlinkKind>,
    },
    EmptyDirectory,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum JournalPresence {
    Absent,
    /// Legacy representation from journal schemas through file v4 / batch v2.
    Present(JournalNode),
    PresentV2 {
        node: JournalNode,
        safety: PlanSafety,
    },
}

impl JournalPresence {
    pub(super) fn from_entry(entry: Option<&ManifestEntry>) -> Self {
        entry.map_or(Self::Absent, |entry| Self::PresentV2 {
            node: JournalNode::from_entry(entry),
            safety: PlanSafety::from_observations(&entry.safety),
        })
    }

    pub(super) fn to_entry(&self, path: &NativeRelativePath) -> Option<ManifestEntry> {
        match self {
            Self::Absent => None,
            Self::Present(node) => Some(node.to_entry(path)),
            Self::PresentV2 { node, safety } => {
                let mut entry = node.to_entry(path);
                entry.safety = safety.to_observations();
                Some(entry)
            }
        }
    }
}

impl JournalNode {
    pub(super) fn from_entry(entry: &ManifestEntry) -> Self {
        match &entry.node {
            ManifestNode::Regular {
                object,
                raw_size,
                unix_exec_bits,
                ..
            } => Self::Regular {
                object: *object,
                raw_size: *raw_size,
                unix_exec_bits: *unix_exec_bits,
            },
            ManifestNode::Symlink {
                target,
                windows_link_kind,
                ..
            } => Self::Symlink {
                target: target.clone(),
                windows_link_kind: *windows_link_kind,
            },
            ManifestNode::EmptyDirectory => Self::EmptyDirectory,
        }
    }

    fn to_entry(&self, path: &NativeRelativePath) -> ManifestEntry {
        let node = match self {
            Self::Regular {
                object,
                raw_size,
                unix_exec_bits,
            } => ManifestNode::Regular {
                object: *object,
                raw_size: *raw_size,
                unix_exec_bits: *unix_exec_bits,
                windows_readonly: None,
            },
            Self::Symlink {
                target,
                windows_link_kind,
            } => ManifestNode::Symlink {
                target: target.clone(),
                windows_link_kind: *windows_link_kind,
                windows_substitute_name: None,
                windows_reparse_flags: None,
            },
            Self::EmptyDirectory => ManifestNode::EmptyDirectory,
        };
        ManifestEntry {
            path: path.clone(),
            node,
            safety: anchor_core::SafetyObservations::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) enum JournalState {
    Prepared,
    Evacuated,
    Installed,
    Verified,
    Complete,
    NeedsRecovery,
    RolledBack,
}
