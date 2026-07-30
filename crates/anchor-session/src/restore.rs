#[cfg(all(test, unix))]
use std::cell::Cell;
use std::collections::BTreeSet;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs;
use std::io;
#[cfg(unix)]
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use anchor_core::ObjectStore;
use anchor_core::{
    CaptureEngine, ConflictReason, ManifestEntry, ManifestId, ManifestNode, NativeRelativePath,
    NoChangeReason, ObjectId, ObservedKind, RestoreConflict, RestoreOutcome, RestorePlan,
    ScopeClassifier, ScopeDecision, ScopeError, TextMergeConflict, TextMergeLimits,
    TextMergeResult, inverse_three_way_text_merge,
};
use anchor_git::{GitContext, IndexCapture};
#[cfg(unix)]
use atomic_write_file::AtomicWriteFile;
#[cfg(unix)]
use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::{Dir, OpenOptions};
#[cfg(unix)]
use serde::{Deserialize, Serialize};
use thiserror::Error;
#[cfg(unix)]
use uuid::Uuid;

use crate::{SessionError, SessionId, SessionStore};

#[cfg(unix)]
const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
#[cfg(unix)]
const BATCH_JOURNAL_TAG: u64 = 0x414e_4348_4f52_424a;

#[cfg(all(test, unix))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BatchFaultPoint {
    Prepared,
    FirstStaged,
    Staged,
    FirstEvacuated,
    Evacuated,
    FirstInstalled,
    Installed,
    FirstVerified,
    Verified,
}

#[cfg(all(test, unix))]
thread_local! {
    static BATCH_FAULT_POINT: Cell<Option<BatchFaultPoint>> = const { Cell::new(None) };
}

#[cfg(all(test, unix))]
fn inject_batch_fault(point: BatchFaultPoint) {
    BATCH_FAULT_POINT.set(Some(point));
}

#[cfg(all(test, unix))]
fn maybe_inject_batch_fault(point: BatchFaultPoint) -> Result<(), RestoreError> {
    if BATCH_FAULT_POINT.get() == Some(point) {
        BATCH_FAULT_POINT.set(None);
        return Err(RestoreError::InjectedBatchCrash);
    }
    Ok(())
}

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

#[derive(Debug, Default)]
pub struct TransactionRecoveryService;

impl TransactionRecoveryService {
    /// Roll back interrupted schema-v3 restore transactions for this worktree.
    ///
    /// Recovery verifies stored paths against the retained session and fresh Git discovery,
    /// verifies every live/staged/backup node by bytes and type, and refuses any ambiguity.
    ///
    /// # Errors
    ///
    /// Returns [`RestoreError`] without overwriting a mismatched live path, legacy incomplete
    /// journal, or transaction belonging to an unverifiable worktree.
    pub fn recover(store: &SessionStore) -> Result<TransactionRecoveryReport, RestoreError> {
        let _store_lease = store.acquire_store_read_lease()?;
        let _lock = store.acquire_active_lock()?;
        recover_transactions(store)
    }
}

#[derive(Debug, Default)]
pub struct RestoreService;

impl RestoreService {
    /// Preview or apply every unambiguous included session-window inverse as one batch.
    ///
    /// Apply requires the exact current-manifest ID returned by a fresh preview. All desired
    /// outputs are staged before mutation; all current nodes are evacuated and retained until
    /// every installed target verifies.
    ///
    /// # Errors
    ///
    /// Returns [`RestoreError`] for incomplete sessions, drift, unsupported parent-tree
    /// reconstruction, a changed preview token, transaction failure, or corruption.
    #[allow(clippy::too_many_lines)]
    pub fn restore_all(
        store: &SessionStore,
        session_id: SessionId,
        mode: WholeRestoreMode,
    ) -> Result<WholeRestoreResult, RestoreError> {
        let _store_lease = store.acquire_store_read_lease()?;
        let _lock = store.acquire_active_lock()?;
        ensure_no_unresolved_transactions(store.root())?;
        let session = store.load_session(session_id)?;
        if !matches!(
            session.state,
            crate::SessionState::Completed | crate::SessionState::Interrupted
        ) {
            return Err(RestoreError::IncompleteSession);
        }
        let after = session
            .after
            .as_ref()
            .ok_or(RestoreError::IncompleteSession)?;
        if session.before.repository != after.repository {
            return Err(RestoreError::RepositoryChangedDuringSession);
        }
        let worktree = PathBuf::from(session.worktree_root.to_host()?);
        let context = GitContext::discover(&worktree)?;
        if context.repository_state()? != after.repository {
            return Err(RestoreError::RepositoryDrift);
        }
        let base = store.load_manifest(session.before.manifest)?;
        let endpoint = store.load_manifest(after.manifest)?;
        let frozen_scope = context.frozen_scope(&base, store.objects(), context.tracked_paths())?;
        let current_endpoint = crate::capture_frozen_endpoint(
            &context,
            store,
            session.capture_policy.capture_options(),
            frozen_scope,
        )?;
        let current = store.load_manifest(current_endpoint.manifest)?;
        if let WholeRestoreMode::Apply { expected_current } = mode {
            if current_endpoint.manifest != expected_current {
                return Err(RestoreError::WholePreviewChanged {
                    expected: expected_current,
                    actual: current_endpoint.manifest,
                });
            }
        }
        let plan = RestorePlan::calculate(&base, &endpoint, &current, &BTreeSet::new())?;
        let conflicts = plan
            .outcomes
            .iter()
            .filter_map(|item| match &item.outcome {
                RestoreOutcome::Conflict(conflict) => Some(WholeRestoreConflict {
                    path: item.path.clone(),
                    reason: conflict.reason,
                }),
                RestoreOutcome::Write(_) | RestoreOutcome::NoChange(_) => None,
            })
            .collect::<Vec<_>>();
        if !conflicts.is_empty() {
            return Ok(WholeRestoreResult::Conflicts { conflicts });
        }
        let writes = plan
            .outcomes
            .iter()
            .filter_map(|item| match &item.outcome {
                RestoreOutcome::Write(desired) => Some(BatchWrite {
                    path: item.path.clone(),
                    expected: current
                        .entries()
                        .binary_search_by(|entry| entry.path.cmp(&item.path))
                        .ok()
                        .map(|index| &current.entries()[index])
                        .cloned(),
                    desired: desired.clone(),
                }),
                RestoreOutcome::NoChange(_) | RestoreOutcome::Conflict(_) => None,
            })
            .collect::<Vec<_>>();
        let no_changes = plan.outcomes.len().saturating_sub(writes.len());
        if mode == WholeRestoreMode::Preview {
            return Ok(WholeRestoreResult::Preview {
                current_manifest: current_endpoint.manifest,
                writes: u64::try_from(writes.len()).unwrap_or(u64::MAX),
                no_changes: u64::try_from(no_changes).unwrap_or(u64::MAX),
            });
        }
        apply_batch(store, session_id, &worktree, &writes)?;
        Ok(WholeRestoreResult::Applied {
            paths: u64::try_from(writes.len()).unwrap_or(u64::MAX),
        })
    }

    /// Safely restore one path when the three-state planner can prove an inverse.
    ///
    /// # Errors
    ///
    /// Returns [`RestoreError`] before mutation for incomplete sessions, repository drift,
    /// unsupported node types, unsafe paths, or I/O failures. Once mutation begins, an immutable
    /// transaction journal and recoverable backup remain in the store until verification.
    pub fn restore_file(
        store: &SessionStore,
        session_id: SessionId,
        selected: NativeRelativePath,
    ) -> Result<RestoreApplyResult, RestoreError> {
        Self::restore_file_with_merge(store, session_id, selected, TextMergeMode::Disabled)
    }

    /// Preview or apply a conservative inverse text merge for one selected path.
    ///
    /// `Preview` publishes only an immutable merged object and never changes the worktree.
    /// `Apply` uses the same evacuation, verification, and no-replace transaction as exact
    /// restoration.
    ///
    /// # Errors
    ///
    /// Returns [`RestoreError`] under the same refusal rules as [`Self::restore_file`].
    #[allow(clippy::too_many_lines)]
    pub fn restore_file_with_merge(
        store: &SessionStore,
        session_id: SessionId,
        selected: NativeRelativePath,
        merge_mode: TextMergeMode,
    ) -> Result<RestoreApplyResult, RestoreError> {
        let _store_lease = store.acquire_store_read_lease()?;
        let _lock = store.acquire_active_lock()?;
        ensure_no_unresolved_transactions(store.root())?;
        let session = store.load_session(session_id)?;
        if !matches!(
            session.state,
            crate::SessionState::Completed | crate::SessionState::Interrupted
        ) {
            return Err(RestoreError::IncompleteSession);
        }
        let after = session
            .after
            .as_ref()
            .ok_or(RestoreError::IncompleteSession)?;
        if session.before.repository != after.repository {
            return Err(RestoreError::RepositoryChangedDuringSession);
        }
        let worktree = session.worktree_root.to_host()?;
        let worktree = PathBuf::from(worktree);
        let context = GitContext::discover(&worktree)?;
        if context.repository_state()? != after.repository {
            return Err(RestoreError::RepositoryDrift);
        }

        let base = store.load_manifest(session.before.manifest)?;
        let endpoint = store.load_manifest(after.manifest)?;
        let expected = endpoint
            .entries()
            .iter()
            .find(|entry| entry.path == selected)
            .or_else(|| base.entries().iter().find(|entry| entry.path == selected));
        let scope = SelectedScope {
            selected: selected.clone(),
            expected_kind: expected.map(|entry| node_kind(&entry.node)),
        };
        let current = CaptureEngine::new(store.objects(), session.capture_policy.capture_options())
            .capture(&worktree, &scope)?
            .manifest;
        let selected_set = BTreeSet::from([selected.clone()]);
        let plan = RestorePlan::calculate(&base, &endpoint, &current, &selected_set)?;
        let Some(item) = plan.outcomes.first() else {
            return Err(RestoreError::PathNotChanged);
        };
        match &item.outcome {
            RestoreOutcome::NoChange(reason) => {
                Ok(RestoreApplyResult::NoChange { reason: *reason })
            }
            RestoreOutcome::Conflict(conflict) => Ok(RestoreApplyResult::Conflict {
                reason: if conflict.reason == ConflictReason::OpaqueContentDrifted
                    && merge_mode != TextMergeMode::Disabled
                {
                    let merged = merge_regular_conflict(conflict, store)?;
                    match merged {
                        MergeResolution::Clean(candidate) => {
                            if merge_mode == TextMergeMode::Preview {
                                return Ok(RestoreApplyResult::TextMergeAvailable {
                                    session_id,
                                    path: selected,
                                    current_object: candidate.current_object,
                                    current_raw_size: candidate.current_raw_size,
                                    merged_object: candidate.merged_object,
                                    merged_raw_size: candidate.merged_raw_size,
                                });
                            }
                            if let TextMergeMode::Apply { expected_object } = merge_mode {
                                if candidate.merged_object != expected_object {
                                    return Err(RestoreError::MergePreviewChanged {
                                        expected: expected_object,
                                        actual: candidate.merged_object,
                                    });
                                }
                            }
                            let current_entry = current
                                .entries()
                                .iter()
                                .find(|entry| entry.path == selected);
                            apply_one(
                                store,
                                session_id,
                                &worktree,
                                &selected,
                                current_entry,
                                Some(&candidate.desired),
                            )?;
                            return Ok(RestoreApplyResult::Applied {
                                session_id,
                                path: selected,
                                merged: true,
                            });
                        }
                        MergeResolution::Conflict(reason) => reason,
                    }
                } else {
                    conflict.reason
                },
            }),
            RestoreOutcome::Write(desired) => {
                let current_entry = current
                    .entries()
                    .iter()
                    .find(|entry| entry.path == selected);
                apply_one(
                    store,
                    session_id,
                    &worktree,
                    &selected,
                    current_entry,
                    desired.as_ref(),
                )?;
                Ok(RestoreApplyResult::Applied {
                    session_id,
                    path: selected,
                    merged: false,
                })
            }
        }
    }

    /// Restore exact raw index bytes only when the index has not drifted after the session.
    ///
    /// # Errors
    ///
    /// Returns [`RestoreError`] for incomplete sessions, repository drift, split indexes,
    /// existing Git locks, corrupt stored bytes, or any failure of the no-replace transaction.
    pub fn restore_index(
        store: &SessionStore,
        session_id: SessionId,
    ) -> Result<IndexRestoreResult, RestoreError> {
        let _store_lease = store.acquire_store_read_lease()?;
        let _lock = store.acquire_active_lock()?;
        ensure_no_unresolved_transactions(store.root())?;
        let session = store.load_session(session_id)?;
        if !matches!(
            session.state,
            crate::SessionState::Completed | crate::SessionState::Interrupted
        ) {
            return Err(RestoreError::IncompleteSession);
        }
        let after = session
            .after
            .as_ref()
            .ok_or(RestoreError::IncompleteSession)?;
        if session.before.repository != after.repository {
            return Err(RestoreError::RepositoryChangedDuringSession);
        }
        if index_is_split(&session.before.index) || index_is_split(&after.index) {
            return Err(RestoreError::SplitIndexUnsupported);
        }
        if session.before.index == after.index {
            return Ok(IndexRestoreResult::NoChange);
        }
        let worktree = PathBuf::from(session.worktree_root.to_host()?);
        let context = GitContext::discover(&worktree)?;
        if context.repository_state()? != after.repository {
            return Err(RestoreError::RepositoryDrift);
        }
        let current = context.capture_index(store.objects())?;
        if current == session.before.index {
            return Ok(IndexRestoreResult::NoChange);
        }
        if current != after.index {
            return Ok(IndexRestoreResult::Conflict);
        }
        apply_index(
            store,
            session_id,
            context.index_path(),
            &after.index,
            &session.before.index,
        )?;
        Ok(IndexRestoreResult::Applied)
    }
}

struct MergeCandidate {
    desired: ManifestEntry,
    current_object: ObjectId,
    current_raw_size: u64,
    merged_object: ObjectId,
    merged_raw_size: u64,
}

enum MergeResolution {
    Clean(MergeCandidate),
    Conflict(ConflictReason),
}

fn merge_regular_conflict(
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
            unix_exec_bits: base_mode,
        },
        ManifestNode::Regular {
            object: session_object,
            raw_size: session_size,
            unix_exec_bits: session_mode,
        },
        ManifestNode::Regular {
            object: current_object,
            raw_size: current_size,
            unix_exec_bits: current_mode,
        },
    ) = (&base.node, &session.node, &current.node)
    else {
        return Ok(MergeResolution::Conflict(
            ConflictReason::TextMergeUnsupported,
        ));
    };
    let Some(desired_mode) = inverse_scalar(*base_mode, *session_mode, *current_mode) else {
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
            Ok(MergeResolution::Clean(MergeCandidate {
                desired: ManifestEntry {
                    path: current.path.clone(),
                    node: ManifestNode::Regular {
                        object: merged_object,
                        raw_size: merged_raw_size,
                        unix_exec_bits: desired_mode,
                    },
                    safety: current.safety.clone(),
                },
                current_object: *current_object,
                current_raw_size: *current_size,
                merged_object,
                merged_raw_size,
            }))
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

fn index_is_split(index: &IndexCapture) -> bool {
    matches!(
        index,
        IndexCapture::Present {
            summary: anchor_git::IndexSummary {
                split_index: true,
                ..
            },
            ..
        }
    )
}

struct SelectedScope {
    selected: NativeRelativePath,
    expected_kind: Option<ObservedKind>,
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
                    anchor_core::OmissionReason::UnsupportedType,
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

fn node_kind(node: &ManifestNode) -> ObservedKind {
    match node {
        ManifestNode::Regular { .. } => ObservedKind::Regular,
        ManifestNode::Symlink { .. } => ObservedKind::Symlink,
        ManifestNode::EmptyDirectory => ObservedKind::Directory,
    }
}

#[derive(Clone)]
struct BatchWrite {
    path: NativeRelativePath,
    expected: Option<ManifestEntry>,
    desired: Option<ManifestEntry>,
}

#[cfg(unix)]
fn apply_batch(
    store: &SessionStore,
    session_id: SessionId,
    worktree: &Path,
    writes: &[BatchWrite],
) -> Result<(), RestoreError> {
    if writes.is_empty() {
        return Ok(());
    }
    let transaction_id = Uuid::now_v7();
    let transaction_path = store
        .root()
        .join("transactions")
        .join(format!("batch-{transaction_id}"));
    private_transaction_dir(&transaction_path)?;
    let journal_path = transaction_path.join("journal.cbor");
    let mut journal = BatchRestoreJournal {
        tag: BATCH_JOURNAL_TAG,
        schema: 1,
        session_id,
        worktree_root: anchor_core::NativeString::from_host(worktree.as_os_str()),
        worktree_key: store.worktree_key.clone(),
        state: BatchJournalState::Prepared,
        items: writes
            .iter()
            .enumerate()
            .map(|(index, write)| BatchJournalItem {
                path: write.path.clone(),
                stage_name: format!(".anchor-stage-{transaction_id}-{index}"),
                backup_name: format!(".anchor-backup-{transaction_id}-{index}"),
                expected: JournalPresence::from_entry(write.expected.as_ref()),
                desired: JournalPresence::from_entry(write.desired.as_ref()),
                state: BatchItemState::Prepared,
            })
            .collect(),
    };
    save_batch_journal(&journal_path, &journal)?;
    #[cfg(test)]
    maybe_inject_batch_fault(BatchFaultPoint::Prepared)?;
    if let Err(error) = apply_batch_inner(store, worktree, writes, &mut journal, &journal_path) {
        #[cfg(test)]
        if matches!(error, RestoreError::InjectedBatchCrash) {
            return Err(error);
        }
        if journal.state == BatchJournalState::Verified {
            // Verified is the transaction commit point. Some backups may already be gone, so
            // recovery must finish cleanup rather than attempting to reconstruct the old tree.
            return Err(error);
        }
        journal.state = BatchJournalState::NeedsRecovery;
        save_batch_journal(&journal_path, &journal)?;
        if let Err(rollback) = rollback_batch(store, worktree, &journal) {
            return Err(RestoreError::BatchRollbackFailed {
                apply: error.to_string(),
                rollback: rollback.to_string(),
            });
        }
        journal.state = BatchJournalState::RolledBack;
        save_batch_journal(&journal_path, &journal)?;
        return Err(error);
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_batch(
    _store: &SessionStore,
    _session_id: SessionId,
    _worktree: &Path,
    _writes: &[BatchWrite],
) -> Result<(), RestoreError> {
    Err(RestoreError::PlatformMutationUnsupported)
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)]
fn apply_batch_inner(
    store: &SessionStore,
    worktree: &Path,
    writes: &[BatchWrite],
    journal: &mut BatchRestoreJournal,
    journal_path: &Path,
) -> Result<(), RestoreError> {
    for (index, write) in writes.iter().enumerate() {
        let (parent, _) = open_batch_parent(worktree, &write.path)?;
        let stage = OsString::from(&journal.items[index].stage_name);
        if let Some(desired) = &write.desired {
            stage_node(&parent, &stage, desired, store.objects())?;
        }
        journal.items[index].state = BatchItemState::Staged;
        save_batch_journal(journal_path, journal)?;
        #[cfg(test)]
        if index == 0 {
            maybe_inject_batch_fault(BatchFaultPoint::FirstStaged)?;
        }
    }
    #[cfg(test)]
    maybe_inject_batch_fault(BatchFaultPoint::Staged)?;

    journal.state = BatchJournalState::Evacuating;
    save_batch_journal(journal_path, journal)?;
    for (index, write) in writes.iter().enumerate() {
        let (parent, name) = open_batch_parent(worktree, &write.path)?;
        let backup = OsString::from(&journal.items[index].backup_name);
        if write.expected.is_some() {
            if !verify_node(&parent, &name, write.expected.as_ref(), store.objects())? {
                return Err(RestoreError::CurrentChanged);
            }
            rename_noreplace(&parent, &name, &parent, &backup)?;
            if !verify_node(&parent, &backup, write.expected.as_ref(), store.objects())? {
                return Err(RestoreError::CurrentChanged);
            }
            if let Some(ManifestEntry {
                node:
                    ManifestNode::Regular {
                        unix_exec_bits: Some(bits),
                        ..
                    },
                ..
            }) = &write.desired
            {
                let stage = OsString::from(&journal.items[index].stage_name);
                apply_stage_mode(&parent, &stage, &backup, *bits)?;
            }
        } else if !verify_node(&parent, &name, None, store.objects())? {
            return Err(RestoreError::CurrentChanged);
        }
        journal.items[index].state = BatchItemState::Evacuated;
        save_batch_journal(journal_path, journal)?;
        #[cfg(test)]
        if index == 0 {
            maybe_inject_batch_fault(BatchFaultPoint::FirstEvacuated)?;
        }
    }
    #[cfg(test)]
    maybe_inject_batch_fault(BatchFaultPoint::Evacuated)?;

    journal.state = BatchJournalState::Installing;
    save_batch_journal(journal_path, journal)?;
    for (index, write) in writes.iter().enumerate() {
        let (parent, name) = open_batch_parent(worktree, &write.path)?;
        if write.desired.is_some() {
            let stage = OsString::from(&journal.items[index].stage_name);
            rename_noreplace(&parent, &stage, &parent, &name)?;
        }
        journal.items[index].state = BatchItemState::Installed;
        save_batch_journal(journal_path, journal)?;
        #[cfg(test)]
        if index == 0 {
            maybe_inject_batch_fault(BatchFaultPoint::FirstInstalled)?;
        }
    }
    #[cfg(test)]
    maybe_inject_batch_fault(BatchFaultPoint::Installed)?;

    for (index, write) in writes.iter().enumerate() {
        let (parent, name) = open_batch_parent(worktree, &write.path)?;
        if !verify_node(&parent, &name, write.desired.as_ref(), store.objects())? {
            return Err(RestoreError::VerificationFailed);
        }
        journal.items[index].state = BatchItemState::Verified;
        save_batch_journal(journal_path, journal)?;
        #[cfg(test)]
        if index == 0 {
            maybe_inject_batch_fault(BatchFaultPoint::FirstVerified)?;
        }
    }
    journal.state = BatchJournalState::Verified;
    save_batch_journal(journal_path, journal)?;
    #[cfg(test)]
    maybe_inject_batch_fault(BatchFaultPoint::Verified)?;
    finish_batch(store, worktree, journal, journal_path)
}

#[cfg(unix)]
fn finish_batch(
    store: &SessionStore,
    worktree: &Path,
    journal: &mut BatchRestoreJournal,
    journal_path: &Path,
) -> Result<(), RestoreError> {
    for item in &journal.items {
        let desired = item.desired.to_entry(&item.path);
        let expected = item.expected.to_entry(&item.path);
        let (parent, name) = open_batch_parent(worktree, &item.path)?;
        if !verify_node(&parent, &name, desired.as_ref(), store.objects())? {
            return Err(RestoreError::RecoveryCurrentChanged);
        }
        let backup = validate_journal_temp_name(&item.backup_name, ".anchor-backup-")?;
        if parent.symlink_metadata(&backup).is_ok() {
            if !verify_node(&parent, &backup, expected.as_ref(), store.objects())? {
                return Err(RestoreError::RecoveryBackupMismatch);
            }
            remove_node(&parent, &backup, expected.as_ref())?;
        }
        let stage = validate_journal_temp_name(&item.stage_name, ".anchor-stage-")?;
        if parent.symlink_metadata(&stage).is_ok() {
            if desired.is_none()
                || !verify_node(&parent, &stage, desired.as_ref(), store.objects())?
            {
                return Err(RestoreError::RecoveryStageMismatch);
            }
            remove_node(&parent, &stage, desired.as_ref())?;
        }
    }
    journal.state = BatchJournalState::Complete;
    save_batch_journal(journal_path, journal)
}

#[cfg(unix)]
fn rollback_batch(
    store: &SessionStore,
    worktree: &Path,
    journal: &BatchRestoreJournal,
) -> Result<(), RestoreError> {
    for item in journal.items.iter().rev() {
        let expected = item.expected.to_entry(&item.path);
        let desired = item.desired.to_entry(&item.path);
        let (parent, name) = open_batch_parent(worktree, &item.path)?;
        let stage = validate_journal_temp_name(&item.stage_name, ".anchor-stage-")?;
        let backup = validate_journal_temp_name(&item.backup_name, ".anchor-backup-")?;
        rollback_node(
            &parent,
            &name,
            &stage,
            &backup,
            expected.as_ref(),
            desired.as_ref(),
            store.objects(),
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn open_batch_parent(
    worktree: &Path,
    path: &NativeRelativePath,
) -> Result<(Dir, OsString), RestoreError> {
    let host = path.to_host_path()?;
    let name = host
        .file_name()
        .ok_or(RestoreError::UnsafeRootPath)?
        .to_owned();
    let parent_path = host.parent().unwrap_or_else(|| Path::new(""));
    let root = Dir::open_ambient_dir(worktree, ambient_authority())?;
    let parent = if parent_path.as_os_str().is_empty() {
        root
    } else {
        root.open_dir(parent_path)
            .map_err(|_| RestoreError::BatchParentUnavailable(path.clone()))?
    };
    Ok((parent, name))
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)]
fn apply_one(
    store: &SessionStore,
    session_id: SessionId,
    worktree: &Path,
    path: &NativeRelativePath,
    expected: Option<&ManifestEntry>,
    desired: Option<&ManifestEntry>,
) -> Result<(), RestoreError> {
    let host = path.to_host_path()?;
    let name = host
        .file_name()
        .ok_or(RestoreError::UnsafeRootPath)?
        .to_owned();
    let parent_path = host.parent().unwrap_or_else(|| Path::new(""));
    let root = Dir::open_ambient_dir(worktree, ambient_authority())?;
    let parent = if parent_path.as_os_str().is_empty() {
        root.try_clone()?
    } else {
        root.open_dir(parent_path)?
    };

    let transaction_id = Uuid::now_v7();
    let transaction_path = store
        .root()
        .join("transactions")
        .join(transaction_id.to_string());
    private_transaction_dir(&transaction_path)?;
    let stage_name_text = format!(".anchor-stage-{transaction_id}");
    let stage_name = OsString::from(&stage_name_text);
    let backup_name_text = format!(".anchor-backup-{transaction_id}");
    let backup_name = OsString::from(&backup_name_text);
    let journal_path = transaction_path.join("journal.cbor");
    let mut journal = RestoreJournal {
        schema: 3,
        session_id,
        path: path.clone(),
        stage_name: stage_name_text,
        backup_name: Some(backup_name_text),
        worktree_root: Some(anchor_core::NativeString::from_host(worktree.as_os_str())),
        worktree_key: Some(store.worktree_key.clone()),
        expected: Some(JournalPresence::from_entry(expected)),
        desired: Some(JournalPresence::from_entry(desired)),
        state: JournalState::Prepared,
    };
    save_journal(&journal_path, &journal)?;

    if let Some(desired) = desired {
        stage_node(&parent, &stage_name, desired, store.objects())?;
    }

    let had_current = expected.is_some();
    if had_current {
        if let Err(error) = rename_noreplace(&parent, &name, &parent, &backup_name) {
            cleanup_stage(&parent, &stage_name, desired);
            return Err(RestoreError::Evacuation(error));
        }
        journal.state = JournalState::Evacuated;
        save_journal(&journal_path, &journal)?;
        if !verify_node(&parent, &backup_name, expected, store.objects())? {
            let rollback = rename_noreplace(&parent, &backup_name, &parent, &name);
            journal.state = JournalState::NeedsRecovery;
            save_journal(&journal_path, &journal)?;
            cleanup_stage(&parent, &stage_name, desired);
            return match rollback {
                Ok(()) => Err(RestoreError::CurrentChanged),
                Err(error) => Err(RestoreError::RollbackFailed(error)),
            };
        }
        if let Some(ManifestEntry {
            node:
                ManifestNode::Regular {
                    unix_exec_bits: Some(bits),
                    ..
                },
            ..
        }) = desired
        {
            apply_stage_mode(&parent, &stage_name, &backup_name, *bits)?;
        }
    } else if parent.symlink_metadata(&name).is_ok() {
        cleanup_stage(&parent, &stage_name, desired);
        return Err(RestoreError::CurrentChanged);
    }

    if desired.is_some() {
        if let Err(error) = rename_noreplace(&parent, &stage_name, &parent, &name) {
            let rollback = if had_current {
                rename_noreplace(&parent, &backup_name, &parent, &name)
            } else {
                Ok(())
            };
            journal.state = JournalState::NeedsRecovery;
            save_journal(&journal_path, &journal)?;
            return match rollback {
                Ok(()) => Err(RestoreError::Install(error)),
                Err(rollback) => Err(RestoreError::RollbackFailed(rollback)),
            };
        }
    }
    journal.state = JournalState::Installed;
    save_journal(&journal_path, &journal)?;
    if !verify_node(&parent, &name, desired, store.objects())? {
        journal.state = JournalState::NeedsRecovery;
        save_journal(&journal_path, &journal)?;
        return Err(RestoreError::VerificationFailed);
    }
    journal.state = JournalState::Verified;
    save_journal(&journal_path, &journal)?;

    if had_current {
        remove_node(&parent, &backup_name, expected)?;
    }
    journal.state = JournalState::Complete;
    save_journal(&journal_path, &journal)?;
    Ok(())
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)]
fn apply_index(
    store: &SessionStore,
    session_id: SessionId,
    index_path: &Path,
    expected: &IndexCapture,
    desired: &IndexCapture,
) -> Result<(), RestoreError> {
    let parent_path = index_path.parent().ok_or(RestoreError::UnsafeIndexPath)?;
    let name = index_path
        .file_name()
        .ok_or(RestoreError::UnsafeIndexPath)?;
    let lock_path = index_path.with_extension("lock");
    let lock_name = lock_path.file_name().ok_or(RestoreError::UnsafeIndexPath)?;
    let parent = Dir::open_ambient_dir(parent_path, ambient_authority())?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut lock_file = parent
        .open_with(lock_name, &options)
        .map_err(RestoreError::IndexLock)?;
    if !verify_index(&parent, name, expected, store.objects())? {
        drop(lock_file);
        let _cleanup = parent.remove_file(lock_name);
        return Err(RestoreError::CurrentIndexChanged);
    }
    if let IndexCapture::Present {
        object, raw_size, ..
    } = desired
    {
        store
            .objects()
            .copy_verified(*object, *raw_size, &mut lock_file)?;
    }
    lock_file.sync_all()?;
    drop(lock_file);

    let transaction_id = Uuid::now_v7();
    let transaction_path = store
        .root()
        .join("transactions")
        .join(format!("index-{transaction_id}"));
    private_transaction_dir(&transaction_path)?;
    let backup_name_text = format!(".anchor-index-backup-{transaction_id}");
    let backup_name = OsString::from(&backup_name_text);
    let journal_path = transaction_path.join("journal.cbor");
    let mut journal = IndexRestoreJournal {
        schema: 3,
        session_id,
        backup_name: Some(backup_name_text),
        worktree_key: Some(store.worktree_key.clone()),
        index_path: Some(anchor_core::NativeString::from_host(index_path.as_os_str())),
        expected: Some(expected.clone()),
        desired: Some(desired.clone()),
        state: JournalState::Prepared,
    };
    save_index_journal(&journal_path, &journal)?;

    let had_current = matches!(expected, IndexCapture::Present { .. });
    if had_current {
        if let Err(error) = rename_noreplace(&parent, name, &parent, &backup_name) {
            let _cleanup = parent.remove_file(lock_name);
            return Err(RestoreError::Evacuation(error));
        }
        journal.state = JournalState::Evacuated;
        save_index_journal(&journal_path, &journal)?;
        if !verify_index(&parent, &backup_name, expected, store.objects())? {
            let rollback = rename_noreplace(&parent, &backup_name, &parent, name);
            let _cleanup = parent.remove_file(lock_name);
            journal.state = JournalState::NeedsRecovery;
            save_index_journal(&journal_path, &journal)?;
            return match rollback {
                Ok(()) => Err(RestoreError::CurrentIndexChanged),
                Err(error) => Err(RestoreError::RollbackFailed(error)),
            };
        }
    } else if parent.symlink_metadata(name).is_ok() {
        let _cleanup = parent.remove_file(lock_name);
        return Err(RestoreError::CurrentIndexChanged);
    }

    if matches!(desired, IndexCapture::Present { .. }) {
        if let Err(error) = rename_noreplace(&parent, lock_name, &parent, name) {
            let rollback = if had_current {
                rename_noreplace(&parent, &backup_name, &parent, name)
            } else {
                Ok(())
            };
            journal.state = JournalState::NeedsRecovery;
            save_index_journal(&journal_path, &journal)?;
            return match rollback {
                Ok(()) => Err(RestoreError::Install(error)),
                Err(rollback) => Err(RestoreError::RollbackFailed(rollback)),
            };
        }
    } else {
        parent.remove_file(lock_name)?;
    }
    journal.state = JournalState::Installed;
    save_index_journal(&journal_path, &journal)?;
    if !verify_index(&parent, name, desired, store.objects())? {
        journal.state = JournalState::NeedsRecovery;
        save_index_journal(&journal_path, &journal)?;
        return Err(RestoreError::VerificationFailed);
    }
    journal.state = JournalState::Verified;
    save_index_journal(&journal_path, &journal)?;
    if had_current {
        parent.remove_file(&backup_name)?;
    }
    journal.state = JournalState::Complete;
    save_index_journal(&journal_path, &journal)?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_index(
    _store: &SessionStore,
    _session_id: SessionId,
    _index_path: &Path,
    _expected: &IndexCapture,
    _desired: &IndexCapture,
) -> Result<(), RestoreError> {
    Err(RestoreError::PlatformMutationUnsupported)
}

#[cfg(unix)]
fn verify_index(
    directory: &Dir,
    name: &OsStr,
    expected: &IndexCapture,
    objects: &ObjectStore,
) -> Result<bool, RestoreError> {
    match expected {
        IndexCapture::Absent => Ok(matches!(
            directory.symlink_metadata(name),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        )),
        IndexCapture::Present {
            object, raw_size, ..
        } => {
            let metadata = match directory.symlink_metadata(name) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if !metadata.is_file() {
                return Ok(false);
            }
            let mut file = directory.open(name)?;
            let (actual, size) = objects.put(&mut file)?;
            Ok(actual == *object && size == *raw_size)
        }
    }
}

#[cfg(not(unix))]
fn apply_one(
    _store: &SessionStore,
    _session_id: SessionId,
    _worktree: &Path,
    _path: &NativeRelativePath,
    _expected: Option<&ManifestEntry>,
    _desired: Option<&ManifestEntry>,
) -> Result<(), RestoreError> {
    Err(RestoreError::PlatformMutationUnsupported)
}

#[cfg(unix)]
fn rename_noreplace(
    source_dir: &Dir,
    source: &OsStr,
    destination_dir: &Dir,
    destination: &OsStr,
) -> io::Result<()> {
    Ok(rustix::fs::renameat_with(
        source_dir,
        Path::new(source),
        destination_dir,
        Path::new(destination),
        rustix::fs::RenameFlags::NOREPLACE,
    )?)
}

#[cfg(unix)]
fn stage_node(
    parent: &Dir,
    name: &OsStr,
    desired: &ManifestEntry,
    objects: &ObjectStore,
) -> Result<(), RestoreError> {
    match &desired.node {
        ManifestNode::Regular {
            object,
            raw_size,
            unix_exec_bits,
        } => {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = parent.open_with(name, &options)?;
            objects.copy_verified(*object, *raw_size, &mut file)?;
            file.sync_all()?;
            if let Some(bits) = unix_exec_bits {
                use cap_std::fs::{MetadataExt as _, PermissionsExt as _};
                let mode = file.metadata()?.mode();
                file.set_permissions(cap_std::fs::Permissions::from_mode(
                    (mode & !0o111) | execute_mode(*bits),
                ))?;
            }
        }
        ManifestNode::Symlink { target, .. } => {
            parent.symlink_contents(target.to_host()?, name)?;
        }
        ManifestNode::EmptyDirectory => {
            parent.create_dir(name)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn apply_stage_mode(
    parent: &Dir,
    stage: &OsStr,
    backup: &OsStr,
    execute_bits: u8,
) -> Result<(), RestoreError> {
    use cap_std::fs::{MetadataExt as _, PermissionsExt as _};
    let backup_mode = parent.symlink_metadata(backup)?.mode();
    parent.set_permissions(
        stage,
        cap_std::fs::Permissions::from_mode((backup_mode & !0o111) | execute_mode(execute_bits)),
    )?;
    Ok(())
}

#[cfg(unix)]
fn execute_mode(bits: u8) -> u32 {
    u32::from(bits & 0b100) << 4 | u32::from(bits & 0b010) << 2 | u32::from(bits & 0b001)
}

#[cfg(unix)]
fn verify_node(
    directory: &Dir,
    name: &OsStr,
    expected: Option<&ManifestEntry>,
    objects: &ObjectStore,
) -> Result<bool, RestoreError> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(expected.is_none()),
        Err(error) => return Err(error.into()),
    };
    let Some(expected) = expected else {
        return Ok(false);
    };
    match &expected.node {
        ManifestNode::Regular {
            object,
            raw_size,
            unix_exec_bits,
        } => {
            if !metadata.is_file() {
                return Ok(false);
            }
            let mut file = directory.open(name)?;
            let (actual, size) = objects.put(&mut file)?;
            if actual != *object || size != *raw_size {
                return Ok(false);
            }
            if let Some(expected_bits) = unix_exec_bits {
                use cap_std::fs::MetadataExt as _;
                let mode = metadata.mode();
                let actual_bits = u8::from(mode & 0o100 != 0) << 2
                    | u8::from(mode & 0o010 != 0) << 1
                    | u8::from(mode & 0o001 != 0);
                if actual_bits != *expected_bits {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        ManifestNode::Symlink { target, .. } => {
            if !metadata.file_type().is_symlink() {
                return Ok(false);
            }
            Ok(directory.read_link_contents(name)?.as_os_str() == target.to_host()?)
        }
        ManifestNode::EmptyDirectory => {
            if !metadata.is_dir() {
                return Ok(false);
            }
            let directory = directory.open_dir(name)?;
            Ok(directory.entries()?.next().is_none())
        }
    }
}

#[cfg(unix)]
fn cleanup_stage(parent: &Dir, name: &OsStr, desired: Option<&ManifestEntry>) {
    if let Some(desired) = desired {
        let _result = remove_node(parent, name, Some(desired));
    }
}

#[cfg(unix)]
fn remove_node(
    directory: &Dir,
    name: &OsStr,
    entry: Option<&ManifestEntry>,
) -> Result<(), RestoreError> {
    match entry.map(|entry| &entry.node) {
        Some(ManifestNode::EmptyDirectory) => directory.remove_dir(name)?,
        Some(ManifestNode::Regular { .. } | ManifestNode::Symlink { .. }) => {
            directory.remove_file(name)?;
        }
        None => {}
    }
    Ok(())
}

#[cfg(unix)]
fn private_transaction_dir(path: &Path) -> Result<(), RestoreError> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn save_journal(path: &Path, journal: &RestoreJournal) -> Result<(), RestoreError> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(journal, &mut bytes)
        .map_err(|error| RestoreError::Journal(error.to_string()))?;
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&bytes)?;
    file.commit()?;
    Ok(())
}

#[cfg(unix)]
fn save_index_journal(path: &Path, journal: &IndexRestoreJournal) -> Result<(), RestoreError> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(journal, &mut bytes)
        .map_err(|error| RestoreError::Journal(error.to_string()))?;
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&bytes)?;
    file.commit()?;
    Ok(())
}

#[cfg(unix)]
fn save_batch_journal(path: &Path, journal: &BatchRestoreJournal) -> Result<(), RestoreError> {
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

#[cfg(unix)]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RestoreJournal {
    schema: u16,
    session_id: SessionId,
    path: NativeRelativePath,
    stage_name: String,
    #[serde(default)]
    backup_name: Option<String>,
    #[serde(default)]
    worktree_root: Option<anchor_core::NativeString>,
    #[serde(default)]
    worktree_key: Option<String>,
    #[serde(default)]
    expected: Option<JournalPresence>,
    #[serde(default)]
    desired: Option<JournalPresence>,
    state: JournalState,
}

#[cfg(unix)]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct IndexRestoreJournal {
    schema: u16,
    session_id: SessionId,
    #[serde(default)]
    backup_name: Option<String>,
    #[serde(default)]
    worktree_key: Option<String>,
    #[serde(default)]
    index_path: Option<anchor_core::NativeString>,
    #[serde(default)]
    expected: Option<IndexCapture>,
    #[serde(default)]
    desired: Option<IndexCapture>,
    state: JournalState,
}

#[cfg(unix)]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BatchRestoreJournal {
    tag: u64,
    schema: u16,
    session_id: SessionId,
    worktree_root: anchor_core::NativeString,
    worktree_key: String,
    state: BatchJournalState,
    items: Vec<BatchJournalItem>,
}

#[cfg(unix)]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BatchJournalItem {
    path: NativeRelativePath,
    stage_name: String,
    backup_name: String,
    expected: JournalPresence,
    desired: JournalPresence,
    state: BatchItemState,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum BatchJournalState {
    Prepared,
    Evacuating,
    Installing,
    Verified,
    Complete,
    NeedsRecovery,
    RolledBack,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum BatchItemState {
    Prepared,
    Staged,
    Evacuated,
    Installed,
    Verified,
}

#[cfg(unix)]
#[derive(Clone, Debug, Serialize, Deserialize)]
enum JournalNode {
    Regular {
        object: ObjectId,
        raw_size: u64,
        unix_exec_bits: Option<u8>,
    },
    Symlink {
        target: anchor_core::NativeString,
        windows_link_kind: Option<u8>,
    },
    EmptyDirectory,
}

#[cfg(unix)]
#[derive(Clone, Debug, Serialize, Deserialize)]
enum JournalPresence {
    Absent,
    Present(JournalNode),
}

#[cfg(unix)]
impl JournalPresence {
    fn from_entry(entry: Option<&ManifestEntry>) -> Self {
        entry.map_or(Self::Absent, |entry| {
            Self::Present(JournalNode::from_entry(entry))
        })
    }

    fn to_entry(&self, path: &NativeRelativePath) -> Option<ManifestEntry> {
        match self {
            Self::Absent => None,
            Self::Present(node) => Some(node.to_entry(path)),
        }
    }
}

#[cfg(unix)]
impl JournalNode {
    fn from_entry(entry: &ManifestEntry) -> Self {
        match &entry.node {
            ManifestNode::Regular {
                object,
                raw_size,
                unix_exec_bits,
            } => Self::Regular {
                object: *object,
                raw_size: *raw_size,
                unix_exec_bits: *unix_exec_bits,
            },
            ManifestNode::Symlink {
                target,
                windows_link_kind,
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
            },
            Self::Symlink {
                target,
                windows_link_kind,
            } => ManifestNode::Symlink {
                target: target.clone(),
                windows_link_kind: *windows_link_kind,
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

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum JournalState {
    Prepared,
    Evacuated,
    Installed,
    Verified,
    Complete,
    NeedsRecovery,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TransactionSummary {
    pub total: u64,
    pub complete: u64,
    pub needs_recovery: u64,
    pub unfinished: u64,
}

#[cfg(unix)]
pub(crate) fn scan_transactions(root: &Path) -> Result<TransactionSummary, RestoreError> {
    let transactions = root.join("transactions");
    if !transactions.exists() {
        return Ok(TransactionSummary::default());
    }
    let mut summary = TransactionSummary::default();
    for entry in fs::read_dir(transactions)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        summary.total = summary.total.saturating_add(1);
        let journal_path = entry.path().join("journal.cbor");
        let metadata = match fs::metadata(&journal_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                summary.unfinished = summary.unfinished.saturating_add(1);
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.len() > MAX_JOURNAL_BYTES {
            return Err(RestoreError::JournalTooLarge(journal_path));
        }
        let bytes = fs::read(&journal_path)?;
        if let Ok(journal) =
            ciborium::de::from_reader::<BatchRestoreJournal, _>(Cursor::new(&bytes))
        {
            if journal.tag != BATCH_JOURNAL_TAG || journal.schema != 1 {
                return Err(RestoreError::Journal(format!(
                    "transaction {} has an unsupported batch journal",
                    entry.file_name().to_string_lossy()
                )));
            }
            match journal.state {
                BatchJournalState::Complete | BatchJournalState::RolledBack => {
                    summary.complete = summary.complete.saturating_add(1);
                }
                BatchJournalState::NeedsRecovery => {
                    summary.needs_recovery = summary.needs_recovery.saturating_add(1);
                }
                BatchJournalState::Prepared
                | BatchJournalState::Evacuating
                | BatchJournalState::Installing
                | BatchJournalState::Verified => {
                    summary.unfinished = summary.unfinished.saturating_add(1);
                }
            }
        } else if let Ok(journal) =
            ciborium::de::from_reader::<RestoreJournal, _>(Cursor::new(&bytes))
        {
            record_journal_state(&mut summary, journal.state);
        } else if let Ok(journal) =
            ciborium::de::from_reader::<IndexRestoreJournal, _>(Cursor::new(&bytes))
        {
            record_journal_state(&mut summary, journal.state);
        } else {
            return Err(RestoreError::Journal(format!(
                "transaction {} has an unknown journal record",
                entry.file_name().to_string_lossy()
            )));
        }
    }
    Ok(summary)
}

#[cfg(unix)]
fn record_journal_state(summary: &mut TransactionSummary, state: JournalState) {
    match state {
        JournalState::Complete | JournalState::RolledBack => {
            summary.complete = summary.complete.saturating_add(1);
        }
        JournalState::NeedsRecovery => {
            summary.needs_recovery = summary.needs_recovery.saturating_add(1);
        }
        JournalState::Prepared
        | JournalState::Evacuated
        | JournalState::Installed
        | JournalState::Verified => {
            summary.unfinished = summary.unfinished.saturating_add(1);
        }
    }
}

#[cfg(unix)]
fn recover_transactions(store: &SessionStore) -> Result<TransactionRecoveryReport, RestoreError> {
    let transactions = store.root().join("transactions");
    if !transactions.exists() {
        return Ok(TransactionRecoveryReport::default());
    }
    let mut report = TransactionRecoveryReport::default();
    for entry in fs::read_dir(transactions)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| RestoreError::UnsafeJournalName)?;
        let journal_path = entry.path().join("journal.cbor");
        let bytes = read_journal(&journal_path)?;
        if let Ok(mut journal) =
            ciborium::de::from_reader::<BatchRestoreJournal, _>(Cursor::new(&bytes))
        {
            if matches!(
                journal.state,
                BatchJournalState::Complete | BatchJournalState::RolledBack
            ) {
                continue;
            }
            if journal.worktree_key != store.worktree_key {
                report.skipped_other_worktrees = report.skipped_other_worktrees.saturating_add(1);
                continue;
            }
            let completed = journal.state == BatchJournalState::Verified;
            recover_batch_journal(store, &mut journal, &journal_path)?;
            if completed {
                report.completed.push(id);
            } else {
                report.rolled_back.push(id);
            }
            continue;
        }
        if let Ok(mut journal) = ciborium::de::from_reader::<RestoreJournal, _>(Cursor::new(&bytes))
        {
            if matches!(
                journal.state,
                JournalState::Complete | JournalState::RolledBack
            ) {
                continue;
            }
            let Some(worktree_key) = journal.worktree_key.as_deref() else {
                return Err(RestoreError::LegacyRecoveryUnsupported(id));
            };
            if worktree_key != store.worktree_key {
                report.skipped_other_worktrees = report.skipped_other_worktrees.saturating_add(1);
                continue;
            }
            recover_file_journal(store, &mut journal, &journal_path)?;
            report.rolled_back.push(id);
            continue;
        }
        if let Ok(mut journal) =
            ciborium::de::from_reader::<IndexRestoreJournal, _>(Cursor::new(&bytes))
        {
            if matches!(
                journal.state,
                JournalState::Complete | JournalState::RolledBack
            ) {
                continue;
            }
            let Some(worktree_key) = journal.worktree_key.as_deref() else {
                return Err(RestoreError::LegacyRecoveryUnsupported(id));
            };
            if worktree_key != store.worktree_key {
                report.skipped_other_worktrees = report.skipped_other_worktrees.saturating_add(1);
                continue;
            }
            recover_index_journal(store, &mut journal, &journal_path)?;
            report.rolled_back.push(id);
            continue;
        }
        return Err(RestoreError::Journal(format!(
            "transaction {id} has an unknown journal record"
        )));
    }
    Ok(report)
}

#[cfg(unix)]
fn recover_batch_journal(
    store: &SessionStore,
    journal: &mut BatchRestoreJournal,
    journal_path: &Path,
) -> Result<(), RestoreError> {
    if journal.tag != BATCH_JOURNAL_TAG || journal.schema != 1 {
        return Err(RestoreError::LegacyRecoveryUnsupported(
            journal_path.display().to_string(),
        ));
    }
    let worktree = validated_journal_worktree(store, journal.session_id, &journal.worktree_root)?;
    let mut paths = BTreeSet::new();
    let mut temporary_names = BTreeSet::new();
    for item in &journal.items {
        if !paths.insert(item.path.clone()) {
            return Err(RestoreError::BatchJournalDuplicatePath);
        }
        let stage = validate_journal_temp_name(&item.stage_name, ".anchor-stage-")?;
        let backup = validate_journal_temp_name(&item.backup_name, ".anchor-backup-")?;
        if !temporary_names.insert(stage) || !temporary_names.insert(backup) {
            return Err(RestoreError::UnsafeJournalName);
        }
    }
    if journal.state == BatchJournalState::Verified {
        finish_batch(store, &worktree, journal, journal_path)
    } else {
        rollback_batch(store, &worktree, journal)?;
        journal.state = BatchJournalState::RolledBack;
        save_batch_journal(journal_path, journal)
    }
}

#[cfg(not(unix))]
fn recover_transactions(_store: &SessionStore) -> Result<TransactionRecoveryReport, RestoreError> {
    Err(RestoreError::PlatformMutationUnsupported)
}

#[cfg(unix)]
fn read_journal(path: &Path) -> Result<Vec<u8>, RestoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(RestoreError::UnsafeJournalFile(path.to_path_buf()));
    }
    if metadata.len() > MAX_JOURNAL_BYTES {
        return Err(RestoreError::JournalTooLarge(path.to_path_buf()));
    }
    Ok(fs::read(path)?)
}

#[cfg(unix)]
fn recover_file_journal(
    store: &SessionStore,
    journal: &mut RestoreJournal,
    journal_path: &Path,
) -> Result<(), RestoreError> {
    if journal.schema != 3 {
        return Err(RestoreError::LegacyRecoveryUnsupported(
            journal_path.display().to_string(),
        ));
    }
    let worktree = validated_journal_worktree(
        store,
        journal.session_id,
        journal
            .worktree_root
            .as_ref()
            .ok_or(RestoreError::IncompleteRecoveryJournal)?,
    )?;
    let expected = journal
        .expected
        .as_ref()
        .ok_or(RestoreError::IncompleteRecoveryJournal)?
        .to_entry(&journal.path);
    let desired = journal
        .desired
        .as_ref()
        .ok_or(RestoreError::IncompleteRecoveryJournal)?
        .to_entry(&journal.path);
    let host = journal.path.to_host_path()?;
    let name = host.file_name().ok_or(RestoreError::UnsafeRootPath)?;
    let parent_path = host.parent().unwrap_or_else(|| Path::new(""));
    let root = Dir::open_ambient_dir(&worktree, ambient_authority())?;
    let parent = if parent_path.as_os_str().is_empty() {
        root.try_clone()?
    } else {
        root.open_dir(parent_path)?
    };
    let stage = validate_journal_temp_name(&journal.stage_name, ".anchor-stage-")?;
    let backup = validate_journal_temp_name(
        journal
            .backup_name
            .as_deref()
            .ok_or(RestoreError::IncompleteRecoveryJournal)?,
        ".anchor-backup-",
    )?;
    rollback_node(
        &parent,
        name,
        &stage,
        &backup,
        expected.as_ref(),
        desired.as_ref(),
        store.objects(),
    )?;
    journal.state = JournalState::RolledBack;
    save_journal(journal_path, journal)
}

#[cfg(unix)]
fn rollback_node(
    parent: &Dir,
    name: &OsStr,
    stage: &OsStr,
    backup: &OsStr,
    expected: Option<&ManifestEntry>,
    desired: Option<&ManifestEntry>,
    objects: &ObjectStore,
) -> Result<(), RestoreError> {
    let backup_exists = parent.symlink_metadata(backup).is_ok();
    if backup_exists {
        if expected.is_none() || !verify_node(parent, backup, expected, objects)? {
            return Err(RestoreError::RecoveryBackupMismatch);
        }
        if parent.symlink_metadata(name).is_ok() {
            if verify_node(parent, name, expected, objects)? {
                remove_node(parent, backup, expected)?;
            } else if desired.is_some() && verify_node(parent, name, desired, objects)? {
                remove_node(parent, name, desired)?;
                rename_noreplace(parent, backup, parent, name)?;
            } else {
                return Err(RestoreError::RecoveryCurrentChanged);
            }
        } else {
            rename_noreplace(parent, backup, parent, name)?;
        }
    } else if verify_node(parent, name, expected, objects)? {
        // The transaction never evacuated the original, or a previous recovery restored it.
    } else if expected.is_none()
        && desired.is_some()
        && verify_node(parent, name, desired, objects)?
    {
        remove_node(parent, name, desired)?;
    } else {
        return Err(RestoreError::RecoveryBackupMissing);
    }

    if parent.symlink_metadata(stage).is_ok() {
        if desired.is_some() && verify_node(parent, stage, desired, objects)? {
            remove_node(parent, stage, desired)?;
        } else {
            return Err(RestoreError::RecoveryStageMismatch);
        }
    }
    if !verify_node(parent, name, expected, objects)? {
        return Err(RestoreError::VerificationFailed);
    }
    Ok(())
}

#[cfg(unix)]
fn recover_index_journal(
    store: &SessionStore,
    journal: &mut IndexRestoreJournal,
    journal_path: &Path,
) -> Result<(), RestoreError> {
    if journal.schema != 3 {
        return Err(RestoreError::LegacyRecoveryUnsupported(
            journal_path.display().to_string(),
        ));
    }
    let session = store.load_session(journal.session_id)?;
    let worktree = PathBuf::from(session.worktree_root.to_host()?);
    let context = GitContext::discover(&worktree)?;
    validate_store_identity(store, &context)?;
    let recorded_index = PathBuf::from(
        journal
            .index_path
            .as_ref()
            .ok_or(RestoreError::IncompleteRecoveryJournal)?
            .to_host()?,
    );
    if recorded_index != context.index_path() {
        return Err(RestoreError::RecoveryPathMismatch);
    }
    let expected = journal
        .expected
        .as_ref()
        .ok_or(RestoreError::IncompleteRecoveryJournal)?;
    let desired = journal
        .desired
        .as_ref()
        .ok_or(RestoreError::IncompleteRecoveryJournal)?;
    let parent_path = recorded_index
        .parent()
        .ok_or(RestoreError::UnsafeIndexPath)?;
    let name = recorded_index
        .file_name()
        .ok_or(RestoreError::UnsafeIndexPath)?;
    let parent = Dir::open_ambient_dir(parent_path, ambient_authority())?;
    let backup = validate_journal_temp_name(
        journal
            .backup_name
            .as_deref()
            .ok_or(RestoreError::IncompleteRecoveryJournal)?,
        ".anchor-index-backup-",
    )?;
    let lock_path = recorded_index.with_extension("lock");
    let lock_name = lock_path.file_name().ok_or(RestoreError::UnsafeIndexPath)?;
    rollback_index_node(
        &parent,
        name,
        lock_name,
        &backup,
        expected,
        desired,
        store.objects(),
    )?;
    journal.state = JournalState::RolledBack;
    save_index_journal(journal_path, journal)
}

#[cfg(unix)]
fn rollback_index_node(
    parent: &Dir,
    name: &OsStr,
    lock_name: &OsStr,
    backup: &OsStr,
    expected: &IndexCapture,
    desired: &IndexCapture,
    objects: &ObjectStore,
) -> Result<(), RestoreError> {
    if parent.symlink_metadata(backup).is_ok() {
        if !verify_index(parent, backup, expected, objects)? {
            return Err(RestoreError::RecoveryBackupMismatch);
        }
        if parent.symlink_metadata(name).is_ok() {
            if verify_index(parent, name, expected, objects)? {
                parent.remove_file(backup)?;
            } else if verify_index(parent, name, desired, objects)? {
                parent.remove_file(name)?;
                rename_noreplace(parent, backup, parent, name)?;
            } else {
                return Err(RestoreError::RecoveryCurrentChanged);
            }
        } else {
            rename_noreplace(parent, backup, parent, name)?;
        }
    } else if verify_index(parent, name, expected, objects)? {
        // The original was never evacuated, or recovery already restored it.
    } else if matches!(expected, IndexCapture::Absent)
        && verify_index(parent, name, desired, objects)?
    {
        parent.remove_file(name)?;
    } else {
        return Err(RestoreError::RecoveryBackupMissing);
    }

    if let Ok(metadata) = parent.symlink_metadata(lock_name) {
        if !metadata.is_file() {
            return Err(RestoreError::RecoveryStageMismatch);
        }
        let stage_matches = match desired {
            IndexCapture::Present { .. } => verify_index(parent, lock_name, desired, objects)?,
            IndexCapture::Absent => metadata.len() == 0,
        };
        if !stage_matches {
            return Err(RestoreError::RecoveryStageMismatch);
        }
        parent.remove_file(lock_name)?;
    }
    if !verify_index(parent, name, expected, objects)? {
        return Err(RestoreError::VerificationFailed);
    }
    Ok(())
}

#[cfg(unix)]
fn validated_journal_worktree(
    store: &SessionStore,
    session_id: SessionId,
    recorded: &anchor_core::NativeString,
) -> Result<PathBuf, RestoreError> {
    let session = store.load_session(session_id)?;
    if &session.worktree_root != recorded {
        return Err(RestoreError::RecoveryPathMismatch);
    }
    let worktree = PathBuf::from(recorded.to_host()?);
    let context = GitContext::discover(&worktree)?;
    validate_store_identity(store, &context)?;
    Ok(worktree)
}

#[cfg(unix)]
fn validate_store_identity(store: &SessionStore, context: &GitContext) -> Result<(), RestoreError> {
    let location = context.store_location();
    if location.worktree_key != store.worktree_key
        || fs::canonicalize(location.root)? != fs::canonicalize(store.root())?
    {
        return Err(RestoreError::RecoveryPathMismatch);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_journal_temp_name(value: &str, prefix: &str) -> Result<OsString, RestoreError> {
    if !value.starts_with(prefix)
        || value.len() <= prefix.len()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err(RestoreError::UnsafeJournalName);
    }
    Ok(OsString::from(value))
}

#[cfg(not(unix))]
pub(crate) fn scan_transactions(_root: &Path) -> Result<TransactionSummary, RestoreError> {
    Ok(TransactionSummary::default())
}

pub(crate) fn ensure_no_unresolved_transactions(root: &Path) -> Result<(), RestoreError> {
    let summary = scan_transactions(root)?;
    if summary.total == summary.complete {
        Ok(())
    } else {
        Err(RestoreError::UnresolvedTransaction {
            needs_recovery: summary.needs_recovery,
            unfinished: summary.unfinished,
        })
    }
}

#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("session has no complete after-snapshot")]
    IncompleteSession,
    #[error("repository state changed during the session; automatic worktree restore is refused")]
    RepositoryChangedDuringSession,
    #[error("repository state has drifted since the session ended")]
    RepositoryDrift,
    #[error("selected path was not changed during the session")]
    PathNotChanged,
    #[error("split-index restoration is refused until shared-index dependencies are captured")]
    SplitIndexUnsupported,
    #[error("Git index path is unsafe")]
    UnsafeIndexPath,
    #[error("could not acquire Git index lock: {0}")]
    IndexLock(io::Error),
    #[error("the worktree root itself cannot be restored")]
    UnsafeRootPath,
    #[error("current path state changed before it could be safely evacuated")]
    CurrentChanged,
    #[error("current Git index changed before it could be safely evacuated")]
    CurrentIndexChanged,
    #[error("could not evacuate current path without replacement: {0}")]
    Evacuation(io::Error),
    #[error("could not install staged path without replacement: {0}")]
    Install(io::Error),
    #[error("rollback could not safely replace the evacuated path: {0}")]
    RollbackFailed(io::Error),
    #[error("installed path failed byte-for-byte verification")]
    VerificationFailed,
    #[error("merged file length cannot be represented safely")]
    MergedFileTooLarge,
    #[error(
        "clean merge changed since preview (expected {expected}, recalculated {actual}); review again"
    )]
    MergePreviewChanged {
        expected: ObjectId,
        actual: ObjectId,
    },
    #[error(
        "worktree changed since whole-restore preview (expected {expected}, captured {actual}); review again"
    )]
    WholePreviewChanged {
        expected: ManifestId,
        actual: ManifestId,
    },
    #[error("batch restore cannot reconstruct a missing parent for {0:?}")]
    BatchParentUnavailable(NativeRelativePath),
    #[error("batch restore failed ({apply}) and its automatic rollback also failed ({rollback})")]
    BatchRollbackFailed { apply: String, rollback: String },
    #[error("batch restore journal contains the same path more than once")]
    BatchJournalDuplicatePath,
    #[cfg(test)]
    #[error("injected batch crash")]
    InjectedBatchCrash,
    #[error("restore journal encoding failed: {0}")]
    Journal(String),
    #[error("restore journal exceeds its size limit: {0}")]
    JournalTooLarge(PathBuf),
    #[error("restore journal is not a regular file: {0}")]
    UnsafeJournalFile(PathBuf),
    #[error("restore journal contains an unsafe temporary name")]
    UnsafeJournalName,
    #[error("restore journal does not contain schema-v3 recovery data")]
    IncompleteRecoveryJournal,
    #[error("legacy interrupted transaction {0} cannot be recovered automatically")]
    LegacyRecoveryUnsupported(String),
    #[error("restore journal path or worktree identity does not match fresh repository discovery")]
    RecoveryPathMismatch,
    #[error("recovery backup does not match the byte-exact recorded current state")]
    RecoveryBackupMismatch,
    #[error("recovery backup is missing and the live path is not already restored")]
    RecoveryBackupMissing,
    #[error("live path changed after the interrupted restore; recovery refused")]
    RecoveryCurrentChanged,
    #[error("staged restore output does not match the journal")]
    RecoveryStageMismatch,
    #[error(
        "unresolved restore transactions block mutation ({needs_recovery} need recovery, {unfinished} unfinished)"
    )]
    UnresolvedTransaction {
        needs_recovery: u64,
        unfinished: u64,
    },
    #[error("automatic filesystem mutation is not yet supported on this platform")]
    PlatformMutationUnsupported,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Git(#[from] anchor_git::GitError),
    #[error(transparent)]
    Capture(#[from] anchor_core::CaptureError),
    #[error(transparent)]
    Plan(#[from] anchor_core::RestorePlanError),
    #[error(transparent)]
    Path(#[from] anchor_core::PlatformMismatch),
    #[error(transparent)]
    Store(#[from] anchor_core::StoreError),
    #[error(transparent)]
    TextMerge(#[from] anchor_core::TextMergeError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::OsString;

    use anchor_core::PathEncoding;

    use super::*;
    use crate::{CapturePolicy, RunRequest, SessionRunner};

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs").join("heads")).unwrap();
        fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
        root
    }

    fn run_change(root: &Path, script: &str) -> (SessionStore, SessionId) {
        let result = SessionRunner::run(&RunRequest {
            invocation_directory: root.to_path_buf(),
            command: vec![
                OsString::from("sh"),
                OsString::from("-c"),
                OsString::from(script),
            ],
            capture_policy: CapturePolicy::default(),
        })
        .unwrap();
        let context = GitContext::discover(root).unwrap();
        let location = context.store_location();
        (
            SessionStore::open(location.root, location.worktree_key).unwrap(),
            result.session_id,
        )
    }

    fn selected(name: &[u8]) -> NativeRelativePath {
        NativeRelativePath::new(PathEncoding::UnixBytes, vec![name.to_vec()]).unwrap()
    }

    #[test]
    fn restores_exact_session_modification_to_pre_session_bytes() {
        let root = repository();
        fs::write(root.path().join("file"), b"pre-existing human bytes").unwrap();
        let (store, session) = run_change(root.path(), "printf session > file");
        let result = RestoreService::restore_file(&store, session, selected(b"file")).unwrap();
        assert!(matches!(result, RestoreApplyResult::Applied { .. }));
        assert_eq!(
            fs::read(root.path().join("file")).unwrap(),
            b"pre-existing human bytes"
        );
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".anchor-backup-")
        }));
        let transactions = scan_transactions(store.root()).unwrap();
        assert_eq!(transactions.total, 1);
        assert_eq!(transactions.complete, 1);
    }

    #[test]
    fn previews_then_restores_multiple_paths_as_one_batch() {
        let root = repository();
        fs::write(root.path().join("alpha"), b"alpha-before").unwrap();
        fs::write(root.path().join("beta"), b"beta-before").unwrap();
        let (store, session) = run_change(
            root.path(),
            "printf alpha-session > alpha; printf beta-session > beta",
        );

        let preview =
            RestoreService::restore_all(&store, session, WholeRestoreMode::Preview).unwrap();
        let WholeRestoreResult::Preview {
            current_manifest,
            writes,
            no_changes,
        } = preview
        else {
            panic!("expected a whole-restore preview");
        };
        assert_eq!(writes, 2);
        assert_eq!(no_changes, 0);
        assert_eq!(
            fs::read(root.path().join("alpha")).unwrap(),
            b"alpha-session"
        );

        let applied = RestoreService::restore_all(
            &store,
            session,
            WholeRestoreMode::Apply {
                expected_current: current_manifest,
            },
        )
        .unwrap();
        assert_eq!(applied, WholeRestoreResult::Applied { paths: 2 });
        assert_eq!(
            fs::read(root.path().join("alpha")).unwrap(),
            b"alpha-before"
        );
        assert_eq!(fs::read(root.path().join("beta")).unwrap(), b"beta-before");
        let transactions = scan_transactions(store.root()).unwrap();
        assert_eq!(transactions.total, 1);
        assert_eq!(transactions.complete, 1);
    }

    #[test]
    fn whole_restore_refuses_every_path_when_one_path_conflicts() {
        let root = repository();
        fs::write(root.path().join("alpha"), b"alpha-before").unwrap();
        fs::write(root.path().join("beta"), b"beta-before").unwrap();
        let (store, session) = run_change(
            root.path(),
            "printf alpha-session > alpha; printf beta-session > beta",
        );
        fs::write(root.path().join("beta"), b"beta-post-session").unwrap();

        let result =
            RestoreService::restore_all(&store, session, WholeRestoreMode::Preview).unwrap();
        let WholeRestoreResult::Conflicts { conflicts } = result else {
            panic!("expected a structured whole-restore conflict");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, selected(b"beta"));
        assert_eq!(conflicts[0].reason, ConflictReason::OpaqueContentDrifted);
        assert_eq!(
            fs::read(root.path().join("alpha")).unwrap(),
            b"alpha-session"
        );
        assert_eq!(
            fs::read(root.path().join("beta")).unwrap(),
            b"beta-post-session"
        );
        assert!(!store.root().join("transactions").exists());
    }

    #[test]
    fn whole_restore_preview_token_detects_later_worktree_drift() {
        let root = repository();
        fs::write(root.path().join("file"), b"before").unwrap();
        let (store, session) = run_change(root.path(), "printf session > file");
        let preview =
            RestoreService::restore_all(&store, session, WholeRestoreMode::Preview).unwrap();
        let WholeRestoreResult::Preview {
            current_manifest, ..
        } = preview
        else {
            panic!("expected a whole-restore preview");
        };
        fs::write(root.path().join("unrelated"), b"later").unwrap();

        let error = RestoreService::restore_all(
            &store,
            session,
            WholeRestoreMode::Apply {
                expected_current: current_manifest,
            },
        )
        .unwrap_err();
        assert!(matches!(error, RestoreError::WholePreviewChanged { .. }));
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"session");
        assert_eq!(fs::read(root.path().join("unrelated")).unwrap(), b"later");
    }

    #[test]
    fn whole_restore_handles_an_exact_session_rename() {
        let root = repository();
        fs::write(root.path().join("old"), b"bytes").unwrap();
        let (store, session) = run_change(root.path(), "mv old new");
        let preview =
            RestoreService::restore_all(&store, session, WholeRestoreMode::Preview).unwrap();
        let WholeRestoreResult::Preview {
            current_manifest, ..
        } = preview
        else {
            panic!("expected a whole-restore preview");
        };

        RestoreService::restore_all(
            &store,
            session,
            WholeRestoreMode::Apply {
                expected_current: current_manifest,
            },
        )
        .unwrap();
        assert_eq!(fs::read(root.path().join("old")).unwrap(), b"bytes");
        assert!(!root.path().join("new").exists());
    }

    #[test]
    fn batch_recovery_survives_every_persisted_transaction_boundary() {
        for point in [
            BatchFaultPoint::Prepared,
            BatchFaultPoint::FirstStaged,
            BatchFaultPoint::Staged,
            BatchFaultPoint::FirstEvacuated,
            BatchFaultPoint::Evacuated,
            BatchFaultPoint::FirstInstalled,
            BatchFaultPoint::Installed,
            BatchFaultPoint::FirstVerified,
            BatchFaultPoint::Verified,
        ] {
            let root = repository();
            fs::write(root.path().join("alpha"), b"alpha-base").unwrap();
            fs::write(root.path().join("beta"), b"beta-base").unwrap();
            let (store, session) = run_change(
                root.path(),
                "printf alpha-session > alpha; printf beta-session > beta",
            );
            let WholeRestoreResult::Preview {
                current_manifest, ..
            } = RestoreService::restore_all(&store, session, WholeRestoreMode::Preview).unwrap()
            else {
                panic!("expected a whole-restore preview");
            };

            inject_batch_fault(point);
            let error = RestoreService::restore_all(
                &store,
                session,
                WholeRestoreMode::Apply {
                    expected_current: current_manifest,
                },
            )
            .unwrap_err();
            assert!(matches!(error, RestoreError::InjectedBatchCrash));
            let unresolved = scan_transactions(store.root()).unwrap();
            assert_eq!(unresolved.total, 1);
            assert_eq!(unresolved.complete, 0);
            assert_eq!(unresolved.unfinished, 1);

            let report = TransactionRecoveryService::recover(&store).unwrap();
            if point == BatchFaultPoint::Verified {
                assert_eq!(report.completed.len(), 1);
                assert!(report.rolled_back.is_empty());
                assert_eq!(fs::read(root.path().join("alpha")).unwrap(), b"alpha-base");
                assert_eq!(fs::read(root.path().join("beta")).unwrap(), b"beta-base");
            } else {
                assert_eq!(report.rolled_back.len(), 1);
                assert!(report.completed.is_empty());
                assert_eq!(
                    fs::read(root.path().join("alpha")).unwrap(),
                    b"alpha-session"
                );
                assert_eq!(fs::read(root.path().join("beta")).unwrap(), b"beta-session");
            }
            let terminal = scan_transactions(store.root()).unwrap();
            assert_eq!(terminal.total, terminal.complete);
            assert!(
                fs::read_dir(root.path()).unwrap().all(|entry| {
                    let name = entry.unwrap().file_name();
                    !name.to_string_lossy().starts_with(".anchor-")
                }),
                "recovery left a sibling temporary node after {point:?}"
            );
        }
    }

    #[test]
    fn batch_recovery_refuses_a_duplicate_path_in_an_untrusted_journal() {
        let root = repository();
        fs::write(root.path().join("file"), b"base").unwrap();
        let (store, session_id) = run_change(root.path(), "printf session > file");
        let session = store.load_session(session_id).unwrap();
        let base = store.load_manifest(session.before.manifest).unwrap();
        let after = store
            .load_manifest(session.after.as_ref().unwrap().manifest)
            .unwrap();
        let desired = JournalPresence::from_entry(base.entries().first());
        let expected = JournalPresence::from_entry(after.entries().first());
        let transaction_id = Uuid::now_v7();
        let transaction_path = store
            .root()
            .join("transactions")
            .join(format!("batch-{transaction_id}"));
        private_transaction_dir(&transaction_path).unwrap();
        let item = |suffix: usize| BatchJournalItem {
            path: selected(b"file"),
            stage_name: format!(".anchor-stage-{transaction_id}-{suffix}"),
            backup_name: format!(".anchor-backup-{transaction_id}-{suffix}"),
            expected: expected.clone(),
            desired: desired.clone(),
            state: BatchItemState::Prepared,
        };
        save_batch_journal(
            &transaction_path.join("journal.cbor"),
            &BatchRestoreJournal {
                tag: BATCH_JOURNAL_TAG,
                schema: 1,
                session_id,
                worktree_root: anchor_core::NativeString::from_host(root.path().as_os_str()),
                worktree_key: store.worktree_key.clone(),
                state: BatchJournalState::Prepared,
                items: vec![item(0), item(1)],
            },
        )
        .unwrap();

        let error = TransactionRecoveryService::recover(&store).unwrap_err();
        assert!(matches!(error, RestoreError::BatchJournalDuplicatePath));
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"session");
        let unresolved = scan_transactions(store.root()).unwrap();
        assert_eq!(unresolved.unfinished, 1);
    }

    #[test]
    fn batch_recovery_preserves_a_concurrent_creator() {
        let root = repository();
        fs::write(root.path().join("alpha"), b"alpha-base").unwrap();
        fs::write(root.path().join("beta"), b"beta-base").unwrap();
        let (store, session) = run_change(
            root.path(),
            "printf alpha-session > alpha; printf beta-session > beta",
        );
        let WholeRestoreResult::Preview {
            current_manifest, ..
        } = RestoreService::restore_all(&store, session, WholeRestoreMode::Preview).unwrap()
        else {
            panic!("expected a whole-restore preview");
        };
        inject_batch_fault(BatchFaultPoint::FirstEvacuated);
        let error = RestoreService::restore_all(
            &store,
            session,
            WholeRestoreMode::Apply {
                expected_current: current_manifest,
            },
        )
        .unwrap_err();
        assert!(matches!(error, RestoreError::InjectedBatchCrash));
        assert!(!root.path().join("alpha").exists());
        fs::write(root.path().join("alpha"), b"concurrent").unwrap();

        let error = TransactionRecoveryService::recover(&store).unwrap_err();
        assert!(matches!(error, RestoreError::RecoveryCurrentChanged));
        assert_eq!(fs::read(root.path().join("alpha")).unwrap(), b"concurrent");
        assert_eq!(fs::read(root.path().join("beta")).unwrap(), b"beta-session");
        let unresolved = scan_transactions(store.root()).unwrap();
        assert_eq!(unresolved.unfinished, 1);
    }

    #[test]
    fn refuses_to_overwrite_post_session_bytes() {
        let root = repository();
        fs::write(root.path().join("file"), b"before").unwrap();
        let (store, session) = run_change(root.path(), "printf session > file");
        fs::write(root.path().join("file"), b"post-session").unwrap();
        let result = RestoreService::restore_file(&store, session, selected(b"file")).unwrap();
        assert_eq!(
            result,
            RestoreApplyResult::Conflict {
                reason: ConflictReason::OpaqueContentDrifted
            }
        );
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"post-session");
    }

    #[test]
    fn previews_then_applies_a_clean_inverse_text_merge() {
        let root = repository();
        fs::write(root.path().join("file"), b"one\nbase\ntail\n").unwrap();
        let (store, session) = run_change(root.path(), "printf 'one\\nsession\\ntail\\n' > file");
        fs::write(root.path().join("file"), b"current\nsession\ntail\n").unwrap();

        let preview = RestoreService::restore_file_with_merge(
            &store,
            session,
            selected(b"file"),
            TextMergeMode::Preview,
        )
        .unwrap();
        assert!(matches!(
            preview,
            RestoreApplyResult::TextMergeAvailable { .. }
        ));
        assert_eq!(
            fs::read(root.path().join("file")).unwrap(),
            b"current\nsession\ntail\n"
        );
        let RestoreApplyResult::TextMergeAvailable { merged_object, .. } = preview else {
            unreachable!("preview variant was asserted above");
        };

        let applied = RestoreService::restore_file_with_merge(
            &store,
            session,
            selected(b"file"),
            TextMergeMode::Apply {
                expected_object: merged_object,
            },
        )
        .unwrap();
        assert!(matches!(
            applied,
            RestoreApplyResult::Applied { merged: true, .. }
        ));
        assert_eq!(
            fs::read(root.path().join("file")).unwrap(),
            b"current\nbase\ntail\n"
        );
    }

    #[test]
    fn overlapping_text_edits_remain_a_structured_conflict() {
        let root = repository();
        fs::write(root.path().join("file"), b"base\n").unwrap();
        let (store, session) = run_change(root.path(), "printf 'session\\n' > file");
        fs::write(root.path().join("file"), b"post-session\n").unwrap();

        let result = RestoreService::restore_file_with_merge(
            &store,
            session,
            selected(b"file"),
            TextMergeMode::Preview,
        )
        .unwrap();
        assert_eq!(
            result,
            RestoreApplyResult::Conflict {
                reason: ConflictReason::TextMergeOverlaps
            }
        );
        assert_eq!(
            fs::read(root.path().join("file")).unwrap(),
            b"post-session\n"
        );
    }

    #[test]
    fn refuses_when_clean_merge_changed_after_preview() {
        let root = repository();
        fs::write(root.path().join("file"), b"one\nbase\ntail\n").unwrap();
        let (store, session) = run_change(root.path(), "printf 'one\\nsession\\ntail\\n' > file");
        fs::write(root.path().join("file"), b"current\nsession\ntail\n").unwrap();
        let preview = RestoreService::restore_file_with_merge(
            &store,
            session,
            selected(b"file"),
            TextMergeMode::Preview,
        )
        .unwrap();
        let RestoreApplyResult::TextMergeAvailable { merged_object, .. } = preview else {
            unreachable!("expected a clean merge preview");
        };
        fs::write(root.path().join("file"), b"newer-current\nsession\ntail\n").unwrap();

        let error = RestoreService::restore_file_with_merge(
            &store,
            session,
            selected(b"file"),
            TextMergeMode::Apply {
                expected_object: merged_object,
            },
        )
        .unwrap_err();
        assert!(matches!(error, RestoreError::MergePreviewChanged { .. }));
        assert_eq!(
            fs::read(root.path().join("file")).unwrap(),
            b"newer-current\nsession\ntail\n"
        );
    }

    #[test]
    fn recovers_an_interrupted_evacuated_file_transaction() {
        let root = repository();
        fs::write(root.path().join("file"), b"base").unwrap();
        let (store, session_id) = run_change(root.path(), "printf session > file");
        let session = store.load_session(session_id).unwrap();
        let base = store.load_manifest(session.before.manifest).unwrap();
        let after = store
            .load_manifest(session.after.as_ref().unwrap().manifest)
            .unwrap();
        let desired = base.entries().first().unwrap();
        let expected = after.entries().first().unwrap();

        let transaction_id = Uuid::now_v7();
        let transaction_path = store
            .root()
            .join("transactions")
            .join(transaction_id.to_string());
        private_transaction_dir(&transaction_path).unwrap();
        let stage_text = format!(".anchor-stage-{transaction_id}");
        let backup_text = format!(".anchor-backup-{transaction_id}");
        let parent = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        stage_node(&parent, OsStr::new(&stage_text), desired, store.objects()).unwrap();
        rename_noreplace(
            &parent,
            OsStr::new("file"),
            &parent,
            OsStr::new(&backup_text),
        )
        .unwrap();
        let journal_path = transaction_path.join("journal.cbor");
        save_journal(
            &journal_path,
            &RestoreJournal {
                schema: 3,
                session_id,
                path: selected(b"file"),
                stage_name: stage_text.clone(),
                backup_name: Some(backup_text.clone()),
                worktree_root: Some(anchor_core::NativeString::from_host(
                    root.path().as_os_str(),
                )),
                worktree_key: Some(store.worktree_key.clone()),
                expected: Some(JournalPresence::Present(JournalNode::from_entry(expected))),
                desired: Some(JournalPresence::Present(JournalNode::from_entry(desired))),
                state: JournalState::Evacuated,
            },
        )
        .unwrap();

        let report = TransactionRecoveryService::recover(&store).unwrap();
        assert_eq!(report.rolled_back, vec![transaction_id.to_string()]);
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"session");
        assert!(!root.path().join(stage_text).exists());
        assert!(!root.path().join(backup_text).exists());
        let summary = scan_transactions(store.root()).unwrap();
        assert_eq!(summary.total, summary.complete);
    }

    #[test]
    fn recovery_finishes_cleanup_after_a_batch_commit_point() {
        let root = repository();
        fs::write(root.path().join("file"), b"base").unwrap();
        let (store, session_id) = run_change(root.path(), "printf session > file");
        let session = store.load_session(session_id).unwrap();
        let base = store.load_manifest(session.before.manifest).unwrap();
        let after = store
            .load_manifest(session.after.as_ref().unwrap().manifest)
            .unwrap();
        let desired = base.entries().first().unwrap();
        let expected = after.entries().first().unwrap();

        let transaction_id = Uuid::now_v7();
        let transaction_name = format!("batch-{transaction_id}");
        let transaction_path = store.root().join("transactions").join(&transaction_name);
        private_transaction_dir(&transaction_path).unwrap();
        let stage_text = format!(".anchor-stage-{transaction_id}-0");
        let backup_text = format!(".anchor-backup-{transaction_id}-0");
        let parent = Dir::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        stage_node(&parent, OsStr::new(&stage_text), desired, store.objects()).unwrap();
        rename_noreplace(
            &parent,
            OsStr::new("file"),
            &parent,
            OsStr::new(&backup_text),
        )
        .unwrap();
        rename_noreplace(
            &parent,
            OsStr::new(&stage_text),
            &parent,
            OsStr::new("file"),
        )
        .unwrap();
        let journal_path = transaction_path.join("journal.cbor");
        save_batch_journal(
            &journal_path,
            &BatchRestoreJournal {
                tag: BATCH_JOURNAL_TAG,
                schema: 1,
                session_id,
                worktree_root: anchor_core::NativeString::from_host(root.path().as_os_str()),
                worktree_key: store.worktree_key.clone(),
                state: BatchJournalState::Verified,
                items: vec![BatchJournalItem {
                    path: selected(b"file"),
                    stage_name: stage_text,
                    backup_name: backup_text.clone(),
                    expected: JournalPresence::Present(JournalNode::from_entry(expected)),
                    desired: JournalPresence::Present(JournalNode::from_entry(desired)),
                    state: BatchItemState::Verified,
                }],
            },
        )
        .unwrap();

        let report = TransactionRecoveryService::recover(&store).unwrap();
        assert_eq!(report.completed, vec![transaction_name]);
        assert!(report.rolled_back.is_empty());
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"base");
        assert!(!root.path().join(backup_text).exists());
        let summary = scan_transactions(store.root()).unwrap();
        assert_eq!(summary.total, summary.complete);
    }

    #[test]
    fn recovers_an_interrupted_installed_index_transaction() {
        let root = repository();
        let index_path = root.path().join(".git").join("index");
        let index = empty_v2_index();
        fs::write(&index_path, &index).unwrap();
        let (store, session_id) = run_change(root.path(), "rm .git/index");
        let session = store.load_session(session_id).unwrap();
        let expected = session.after.as_ref().unwrap().index.clone();
        let desired = session.before.index.clone();
        fs::write(&index_path, &index).unwrap();

        let transaction_id = Uuid::now_v7();
        let transaction_path = store
            .root()
            .join("transactions")
            .join(format!("index-{transaction_id}"));
        private_transaction_dir(&transaction_path).unwrap();
        let backup_text = format!(".anchor-index-backup-{transaction_id}");
        let journal_path = transaction_path.join("journal.cbor");
        save_index_journal(
            &journal_path,
            &IndexRestoreJournal {
                schema: 3,
                session_id,
                backup_name: Some(backup_text),
                worktree_key: Some(store.worktree_key.clone()),
                index_path: Some(anchor_core::NativeString::from_host(index_path.as_os_str())),
                expected: Some(expected),
                desired: Some(desired),
                state: JournalState::Installed,
            },
        )
        .unwrap();

        let report = TransactionRecoveryService::recover(&store).unwrap();
        assert_eq!(report.rolled_back, vec![format!("index-{transaction_id}")]);
        assert!(!index_path.exists());
        assert_eq!(scan_transactions(store.root()).unwrap().unfinished, 0);
    }

    #[test]
    fn removes_an_unchanged_session_addition() {
        let root = repository();
        let (store, session) = run_change(root.path(), "printf session > added");
        let result = RestoreService::restore_file(&store, session, selected(b"added")).unwrap();
        assert!(matches!(result, RestoreApplyResult::Applied { .. }));
        assert!(!root.path().join("added").exists());
    }

    #[test]
    fn restores_empty_directory_additions_and_deletions() {
        let added_root = repository();
        let (store, session) = run_change(added_root.path(), "mkdir added-empty");
        let result =
            RestoreService::restore_file(&store, session, selected(b"added-empty")).unwrap();
        assert!(matches!(result, RestoreApplyResult::Applied { .. }));
        assert!(!added_root.path().join("added-empty").exists());

        let deleted_root = repository();
        fs::create_dir(deleted_root.path().join("deleted-empty")).unwrap();
        let (store, session) = run_change(deleted_root.path(), "rmdir deleted-empty");
        let result =
            RestoreService::restore_file(&store, session, selected(b"deleted-empty")).unwrap();
        assert!(matches!(result, RestoreApplyResult::Applied { .. }));
        assert!(deleted_root.path().join("deleted-empty").is_dir());
    }

    #[test]
    fn restores_raw_index_only_after_exact_endpoint_match() {
        let root = repository();
        let index = empty_v2_index();
        fs::write(root.path().join(".git").join("index"), &index).unwrap();
        let (store, session) = run_change(root.path(), "rm .git/index");
        assert!(!root.path().join(".git").join("index").exists());
        assert_eq!(
            RestoreService::restore_index(&store, session).unwrap(),
            IndexRestoreResult::Applied
        );
        assert_eq!(
            fs::read(root.path().join(".git").join("index")).unwrap(),
            index
        );
    }

    #[test]
    fn index_is_rechecked_after_its_lock_is_acquired() {
        let root = repository();
        let index_path = root.path().join(".git").join("index");
        fs::write(&index_path, empty_v2_index()).unwrap();
        let context = GitContext::discover(root.path()).unwrap();
        let location = context.store_location();
        let store = SessionStore::open(location.root, location.worktree_key).unwrap();
        let expected = context.capture_index(store.objects()).unwrap();
        let drifted = b"post-check index bytes";
        fs::write(&index_path, drifted).unwrap();

        let error = apply_index(
            &store,
            SessionId::new(),
            &index_path,
            &expected,
            &IndexCapture::Absent,
        )
        .unwrap_err();
        assert!(matches!(error, RestoreError::CurrentIndexChanged));
        assert_eq!(fs::read(&index_path).unwrap(), drifted);
        assert!(!index_path.with_extension("lock").exists());
    }

    #[test]
    fn unfinished_transaction_blocks_new_mutations() {
        let root = tempfile::tempdir().unwrap();
        private_transaction_dir(&root.path().join("transactions").join("unfinished")).unwrap();
        let error = ensure_no_unresolved_transactions(root.path()).unwrap_err();
        assert!(matches!(
            error,
            RestoreError::UnresolvedTransaction {
                needs_recovery: 0,
                unfinished: 1
            }
        ));
    }

    fn empty_v2_index() -> Vec<u8> {
        let mut bytes = b"DIRC\0\0\0\x02\0\0\0\0".to_vec();
        bytes.extend_from_slice(&[
            0x39, 0xd8, 0x90, 0x13, 0x9e, 0xe5, 0x35, 0x6c, 0x7e, 0xf5, 0x72, 0x21, 0x6c, 0xeb,
            0xcd, 0x27, 0xaa, 0x41, 0xf9, 0xdf,
        ]);
        bytes
    }
}
