#[cfg(all(test, unix))]
use std::cell::Cell;
use std::collections::BTreeSet;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs;
use std::io;
#[cfg(unix)]
use std::io::Cursor;
#[cfg(all(test, unix))]
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use anchor_core::ObjectStore;
use anchor_core::{
    CaptureEngine, ConflictReason, Manifest, ManifestEntry, ManifestId, ManifestNode,
    MetadataObservation, NativeRelativePath, NoChangeReason, ObjectId, ObservedKind,
    RestoreConflict, RestoreOutcome, RestorePlan, ScopeClassifier, ScopeDecision, ScopeError,
    TextMergeConflict, TextMergeLimits, TextMergeResult, inverse_three_way_text_merge,
};
#[cfg(unix)]
use anchor_core::{
    observe_directory_extended_metadata, observe_extended_metadata,
    platform_managed_directory_metadata_equal, platform_managed_metadata_equal,
};
use anchor_git::{GitContext, IndexCapture};
#[cfg(unix)]
use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::{Dir, OpenOptions};
use thiserror::Error;
#[cfg(unix)]
use uuid::Uuid;

use crate::restore_plan::{
    PlanItem, PlanOperation, PlanPresence, PlanProof, RestorePlanId, RestorePlanRecord,
};
use crate::{SessionError, SessionId, SessionStore};

#[cfg(windows)]
#[path = "restore_windows.rs"]
mod windows;

#[cfg(unix)]
mod journal;
#[cfg(all(test, unix))]
use journal::JournalNode;
#[cfg(unix)]
use journal::{
    BATCH_JOURNAL_SCHEMA, BATCH_JOURNAL_TAG, BatchItemState, BatchJournalItem, BatchJournalState,
    BatchRestoreJournal, FILE_JOURNAL_SCHEMA, IndexRestoreJournal, JournalPresence, JournalState,
    MAX_JOURNAL_BYTES, RestoreJournal, save_batch_journal, save_index_journal, save_journal,
};

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

#[cfg(all(test, unix))]
fn pause_subprocess_at_boundary(boundary: &str) {
    if std::env::var_os("ANCHOR_CRASH_BOUNDARY").as_deref() != Some(OsStr::new(boundary)) {
        return;
    }
    let marker =
        std::env::var_os("ANCHOR_CRASH_MARKER").expect("crash helper requires ANCHOR_CRASH_MARKER");
    let mut file = fs::File::create(marker).expect("crash helper could not create marker");
    file.write_all(boundary.as_bytes())
        .expect("crash helper could not write marker");
    file.sync_all()
        .expect("crash helper could not synchronize marker");
    loop {
        std::thread::park_timeout(std::time::Duration::from_secs(1));
    }
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
        if session.frozen_policy.is_none() {
            return Err(RestoreError::LegacySessionWithoutFrozenPolicy);
        }
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
        let policy_id = session
            .frozen_policy
            .ok_or(RestoreError::LegacySessionWithoutFrozenPolicy)?;
        let frozen_policy = store.load_frozen_policy(policy_id)?;
        let base = store.load_manifest(session.before.manifest)?;
        let endpoint = store.load_manifest(after.manifest)?;
        validate_manifest_mutation_paths(&context, &frozen_policy, &base)?;
        validate_manifest_mutation_paths(&context, &frozen_policy, &endpoint)?;
        let current_endpoint = crate::capture_frozen_endpoint(
            &context,
            store,
            session.capture_policy.capture_options(),
            &frozen_policy,
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
        if !writes.is_empty() {
            let record = RestorePlanRecord::worktree(
                session_id,
                session.worktree_root.clone(),
                session.worktree_key.clone(),
                after.repository.clone(),
                session.before.manifest,
                after.manifest,
                current_endpoint.manifest,
                writes
                    .iter()
                    .map(|write| {
                        PlanItem::exact(
                            write.path.clone(),
                            write.expected.as_ref(),
                            write.desired.as_ref(),
                        )
                    })
                    .collect(),
            )?;
            let plan_id = store.put_restore_plan(&record)?;
            apply_batch(store, session_id, plan_id, &worktree, &writes)?;
        }
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
        if session.frozen_policy.is_none() {
            return Err(RestoreError::LegacySessionWithoutFrozenPolicy);
        }
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

        let policy_id = session
            .frozen_policy
            .ok_or(RestoreError::LegacySessionWithoutFrozenPolicy)?;
        let frozen_policy = store.load_frozen_policy(policy_id)?;
        let base = store.load_manifest(session.before.manifest)?;
        let endpoint = store.load_manifest(after.manifest)?;
        validate_manifest_mutation_paths(&context, &frozen_policy, &base)?;
        validate_manifest_mutation_paths(&context, &frozen_policy, &endpoint)?;
        validate_mutation_path(&context, &frozen_policy, &selected)?;
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
        let current_manifest = store.put_manifest(&current)?;
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
                            let record = RestorePlanRecord::worktree(
                                session_id,
                                session.worktree_root.clone(),
                                session.worktree_key.clone(),
                                after.repository.clone(),
                                session.before.manifest,
                                after.manifest,
                                current_manifest,
                                vec![PlanItem::merged(
                                    selected.clone(),
                                    current_entry,
                                    &candidate.desired,
                                    candidate.base_object,
                                    candidate.session_object,
                                    candidate.current_object,
                                    candidate.merged_object,
                                )],
                            )?;
                            let plan_id = store.put_restore_plan(&record)?;
                            apply_one(
                                store,
                                session_id,
                                plan_id,
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
                let record = RestorePlanRecord::worktree(
                    session_id,
                    session.worktree_root.clone(),
                    session.worktree_key.clone(),
                    after.repository.clone(),
                    session.before.manifest,
                    after.manifest,
                    current_manifest,
                    vec![PlanItem::exact(
                        selected.clone(),
                        current_entry,
                        desired.as_ref(),
                    )],
                )?;
                let plan_id = store.put_restore_plan(&record)?;
                apply_one(
                    store,
                    session_id,
                    plan_id,
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
        let record = RestorePlanRecord::index(
            session_id,
            session.worktree_root.clone(),
            session.worktree_key.clone(),
            after.repository.clone(),
            anchor_core::NativeString::from_host(context.index_path().as_os_str()),
            after.index.clone(),
            session.before.index.clone(),
        )?;
        let plan_id = store.put_restore_plan(&record)?;
        apply_index(
            store,
            session_id,
            plan_id,
            context.index_path(),
            &after.index,
            &session.before.index,
        )?;
        Ok(IndexRestoreResult::Applied)
    }
}

struct MergeCandidate {
    desired: ManifestEntry,
    base_object: ObjectId,
    session_object: ObjectId,
    current_object: ObjectId,
    current_raw_size: u64,
    merged_object: ObjectId,
    merged_raw_size: u64,
}

enum MergeResolution {
    Clean(Box<MergeCandidate>),
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

fn validate_manifest_mutation_paths(
    context: &GitContext,
    policy: &anchor_git::FrozenGitPolicy,
    manifest: &Manifest,
) -> Result<(), RestoreError> {
    for entry in manifest.entries() {
        validate_mutation_path(context, policy, &entry.path)?;
    }
    Ok(())
}

fn validate_mutation_path(
    context: &GitContext,
    policy: &anchor_git::FrozenGitPolicy,
    path: &NativeRelativePath,
) -> Result<(), RestoreError> {
    if context.is_protected_mutation_path(policy, path) {
        return Err(RestoreError::ProtectedWorktreePath(path.clone()));
    }
    Ok(())
}

#[derive(Clone)]
struct BatchWrite {
    pub(super) path: NativeRelativePath,
    pub(super) expected: Option<ManifestEntry>,
    pub(super) desired: Option<ManifestEntry>,
}

#[cfg(unix)]
fn apply_batch(
    store: &SessionStore,
    session_id: SessionId,
    plan_id: RestorePlanId,
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
        schema: BATCH_JOURNAL_SCHEMA,
        plan_id: Some(plan_id),
        transaction_id: Some(transaction_id),
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
    pause_subprocess_at_boundary("journal-created");
    #[cfg(test)]
    maybe_inject_batch_fault(BatchFaultPoint::Prepared)?;
    if let Err(error) = apply_batch_inner(store, worktree, writes, &mut journal, &journal_path) {
        #[cfg(test)]
        if matches!(error, RestoreError::InjectedBatchCrash) {
            return Err(error);
        }
        if matches!(
            journal.state,
            BatchJournalState::Verified
                | BatchJournalState::Cleaning
                | BatchJournalState::CleanupComplete
        ) {
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
    store: &SessionStore,
    session_id: SessionId,
    plan_id: RestorePlanId,
    worktree: &Path,
    writes: &[BatchWrite],
) -> Result<(), RestoreError> {
    windows::apply_batch(store, session_id, plan_id, worktree, writes)
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
        let (parent, current_name) = open_batch_parent(worktree, &write.path)?;
        let stage = OsString::from(&journal.items[index].stage_name);
        if let Some(desired) = &write.desired {
            stage_node(&parent, &stage, desired, store.objects())?;
            if !verify_node(&parent, &stage, Some(desired), store.objects())? {
                return Err(RestoreError::VerificationFailed);
            }
            if let Some(expected) = &write.expected {
                if !platform_metadata_survives_replace(
                    &parent,
                    &current_name,
                    expected,
                    &stage,
                    desired,
                )? {
                    return Err(RestoreError::VerificationFailed);
                }
            }
        }
        journal.items[index].state = BatchItemState::Staged;
        save_batch_journal(journal_path, journal)?;
        #[cfg(test)]
        if index == 0 {
            pause_subprocess_at_boundary("first-output-staged");
            maybe_inject_batch_fault(BatchFaultPoint::FirstStaged)?;
        }
    }
    #[cfg(test)]
    pause_subprocess_at_boundary("all-outputs-staged");
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
            pause_subprocess_at_boundary("first-current-evacuated");
            maybe_inject_batch_fault(BatchFaultPoint::FirstEvacuated)?;
        }
    }
    #[cfg(test)]
    pause_subprocess_at_boundary("all-current-evacuated");
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
            pause_subprocess_at_boundary("first-desired-installed");
            maybe_inject_batch_fault(BatchFaultPoint::FirstInstalled)?;
        }
    }
    #[cfg(test)]
    pause_subprocess_at_boundary("all-desired-installed");
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
            pause_subprocess_at_boundary("first-desired-verified");
            maybe_inject_batch_fault(BatchFaultPoint::FirstVerified)?;
        }
    }
    #[cfg(test)]
    pause_subprocess_at_boundary("all-desired-verified");
    journal.state = BatchJournalState::Verified;
    save_batch_journal(journal_path, journal)?;
    #[cfg(test)]
    pause_subprocess_at_boundary("commit-recorded");
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
    journal.state = BatchJournalState::Cleaning;
    save_batch_journal(journal_path, journal)?;
    #[cfg(test)]
    pause_subprocess_at_boundary("backup-cleanup-started");
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
    journal.state = BatchJournalState::CleanupComplete;
    save_batch_journal(journal_path, journal)?;
    #[cfg(test)]
    pause_subprocess_at_boundary("cleanup-completed");
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
    plan_id: RestorePlanId,
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
        schema: FILE_JOURNAL_SCHEMA,
        session_id,
        plan_id: Some(plan_id),
        transaction_id: Some(transaction_id),
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
        if !verify_node(&parent, &stage_name, Some(desired), store.objects())? {
            cleanup_stage(&parent, &stage_name, Some(desired));
            return Err(RestoreError::VerificationFailed);
        }
        if let Some(expected) = expected {
            if !platform_metadata_survives_replace(&parent, &name, expected, &stage_name, desired)?
            {
                cleanup_stage(&parent, &stage_name, Some(desired));
                return Err(RestoreError::VerificationFailed);
            }
        }
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
    plan_id: RestorePlanId,
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
        schema: 4,
        session_id,
        plan_id: Some(plan_id),
        transaction_id: Some(transaction_id),
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
    store: &SessionStore,
    session_id: SessionId,
    plan_id: RestorePlanId,
    index_path: &Path,
    expected: &IndexCapture,
    desired: &IndexCapture,
) -> Result<(), RestoreError> {
    windows::apply_index(store, session_id, plan_id, index_path, expected, desired)
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
    store: &SessionStore,
    session_id: SessionId,
    plan_id: RestorePlanId,
    worktree: &Path,
    path: &NativeRelativePath,
    expected: Option<&ManifestEntry>,
    desired: Option<&ManifestEntry>,
) -> Result<(), RestoreError> {
    windows::apply_one(
        store, session_id, plan_id, worktree, path, expected, desired,
    )
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
            ..
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
fn platform_metadata_survives_replace(
    parent: &Dir,
    current_name: &OsStr,
    current: &ManifestEntry,
    stage_name: &OsStr,
    staged: &ManifestEntry,
) -> Result<bool, RestoreError> {
    if current.safety.extended_metadata != MetadataObservation::PlatformManaged
        && staged.safety.extended_metadata != MetadataObservation::PlatformManaged
    {
        return Ok(true);
    }
    if current.safety.extended_metadata != MetadataObservation::PlatformManaged
        || staged.safety.extended_metadata != MetadataObservation::PlatformManaged
    {
        return Ok(false);
    }
    match (&current.node, &staged.node) {
        (ManifestNode::Regular { .. }, ManifestNode::Regular { .. }) => {
            let current = parent.open(current_name)?;
            let staged = parent.open(stage_name)?;
            Ok(platform_managed_metadata_equal(&current, &staged))
        }
        (ManifestNode::EmptyDirectory, ManifestNode::EmptyDirectory) => {
            let current = parent.open_dir(current_name)?;
            let staged = parent.open_dir(stage_name)?;
            Ok(platform_managed_directory_metadata_equal(&current, &staged))
        }
        _ => Ok(false),
    }
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
            ..
        } => {
            if !metadata.is_file() {
                return Ok(false);
            }
            let mut file = directory.open(name)?;
            let opened = file.metadata()?;
            {
                use cap_std::fs::MetadataExt as _;
                if metadata.dev() != opened.dev()
                    || metadata.ino() != opened.ino()
                    || opened.nlink() != expected.safety.link_count
                {
                    return Ok(false);
                }
            }
            let extended_before = observe_extended_metadata(&file);
            let (actual, size) = objects.put(&mut file)?;
            if actual != *object || size != *raw_size {
                return Ok(false);
            }
            let after = file.metadata()?;
            let extended_after = observe_extended_metadata(&file);
            {
                use cap_std::fs::MetadataExt as _;
                if opened.dev() != after.dev()
                    || opened.ino() != after.ino()
                    || after.nlink() != expected.safety.link_count
                    || extended_before != expected.safety.extended_metadata
                    || extended_after != expected.safety.extended_metadata
                {
                    return Ok(false);
                }
            }
            if let Some(expected_bits) = unix_exec_bits {
                use cap_std::fs::MetadataExt as _;
                let mode = after.mode();
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
            Ok(directory.entries()?.next().is_none()
                && observe_directory_extended_metadata(&directory)
                    == expected.safety.extended_metadata)
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
            if journal.tag != BATCH_JOURNAL_TAG || !matches!(journal.schema, 1..=4) {
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
                | BatchJournalState::Verified
                | BatchJournalState::Cleaning
                | BatchJournalState::CleanupComplete => {
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
            let completed = matches!(
                journal.state,
                BatchJournalState::Verified
                    | BatchJournalState::Cleaning
                    | BatchJournalState::CleanupComplete
            );
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

#[allow(clippy::too_many_lines)]
fn validate_restore_plan(
    store: &SessionStore,
    plan_id: RestorePlanId,
    session_id: SessionId,
) -> Result<RestorePlanRecord, RestoreError> {
    let plan = store.load_restore_plan(plan_id)?;
    if plan.session_id != session_id || plan.worktree_key != store.worktree_key {
        return Err(RestoreError::RecoveryPlanMismatch);
    }
    let session = store.load_session(session_id)?;
    let after = session
        .after
        .as_ref()
        .ok_or(RestoreError::IncompleteSession)?;
    if plan.worktree_root != session.worktree_root
        || plan.worktree_key != session.worktree_key
        || plan.repository != after.repository
    {
        return Err(RestoreError::RecoveryPlanMismatch);
    }
    let worktree = PathBuf::from(session.worktree_root.to_host()?);
    let context = GitContext::discover(&worktree)?;
    validate_store_identity(store, &context)?;
    let policy_id = session
        .frozen_policy
        .ok_or(RestoreError::LegacySessionWithoutFrozenPolicy)?;
    let frozen_policy = store.load_frozen_policy(policy_id)?;
    if context.repository_state()? != plan.repository {
        return Err(RestoreError::RepositoryDrift);
    }

    match &plan.operation {
        PlanOperation::Worktree {
            base_manifest,
            session_manifest,
            current_manifest,
            items,
        } => {
            for item in items {
                validate_mutation_path(&context, &frozen_policy, &item.path)?;
            }
            if *base_manifest != session.before.manifest || *session_manifest != after.manifest {
                return Err(RestoreError::RecoveryPlanMismatch);
            }
            let base = store.load_manifest(*base_manifest)?;
            let endpoint = store.load_manifest(*session_manifest)?;
            let current = store.load_manifest(*current_manifest)?;
            validate_manifest_mutation_paths(&context, &frozen_policy, &base)?;
            validate_manifest_mutation_paths(&context, &frozen_policy, &endpoint)?;
            validate_manifest_mutation_paths(&context, &frozen_policy, &current)?;
            let selected = items
                .iter()
                .map(|item| item.path.clone())
                .collect::<BTreeSet<_>>();
            let recalculated = RestorePlan::calculate(&base, &endpoint, &current, &selected)?;
            if recalculated.outcomes.len() != items.len() {
                return Err(RestoreError::RecoveryPlanMismatch);
            }
            for item in items {
                let current_entry = current
                    .entries()
                    .binary_search_by(|entry| entry.path.cmp(&item.path))
                    .ok()
                    .map(|index| &current.entries()[index]);
                if item.expected != PlanPresence::from_entry(current_entry) {
                    return Err(RestoreError::RecoveryPlanMismatch);
                }
                let calculated = recalculated
                    .outcomes
                    .iter()
                    .find(|outcome| outcome.path == item.path)
                    .ok_or(RestoreError::RecoveryPlanMismatch)?;
                match (&item.proof, &calculated.outcome) {
                    (PlanProof::Exact, RestoreOutcome::Write(desired))
                        if item.desired == PlanPresence::from_entry(desired.as_ref()) => {}
                    (
                        PlanProof::CleanTextMerge {
                            base,
                            session,
                            current,
                            merged,
                        },
                        RestoreOutcome::Conflict(conflict),
                    ) if conflict.reason == ConflictReason::OpaqueContentDrifted => {
                        let MergeResolution::Clean(candidate) =
                            merge_regular_conflict(conflict, store)?
                        else {
                            return Err(RestoreError::RecoveryPlanMismatch);
                        };
                        if candidate.base_object != *base
                            || candidate.session_object != *session
                            || candidate.current_object != *current
                            || candidate.merged_object != *merged
                            || item.desired != PlanPresence::from_entry(Some(&candidate.desired))
                        {
                            return Err(RestoreError::RecoveryPlanMismatch);
                        }
                    }
                    _ => return Err(RestoreError::RecoveryPlanMismatch),
                }
            }
        }
        PlanOperation::Index {
            index_path,
            expected,
            desired,
        } => {
            let recorded_index = index_path.to_host()?;
            if expected != &after.index
                || desired != &session.before.index
                || Path::new(&recorded_index) != context.index_path()
            {
                return Err(RestoreError::RecoveryPlanMismatch);
            }
        }
    }
    Ok(plan)
}

#[cfg(unix)]
fn recover_batch_journal(
    store: &SessionStore,
    journal: &mut BatchRestoreJournal,
    journal_path: &Path,
) -> Result<(), RestoreError> {
    if journal.tag != BATCH_JOURNAL_TAG || journal.schema != BATCH_JOURNAL_SCHEMA {
        return Err(RestoreError::LegacyRecoveryUnsupported(
            journal_path.display().to_string(),
        ));
    }
    let plan_id = journal.plan_id.ok_or_else(|| {
        RestoreError::LegacyRecoveryUnsupported(journal_path.display().to_string())
    })?;
    let transaction_id = journal.transaction_id.ok_or_else(|| {
        RestoreError::LegacyRecoveryUnsupported(journal_path.display().to_string())
    })?;
    validate_transaction_directory(journal_path, &format!("batch-{transaction_id}"))?;
    let plan = validate_restore_plan(store, plan_id, journal.session_id)?;
    let PlanOperation::Worktree { items, .. } = &plan.operation else {
        return Err(RestoreError::RecoveryPlanMismatch);
    };
    if items.len() != journal.items.len() {
        return Err(RestoreError::RecoveryPlanMismatch);
    }
    let worktree = validated_journal_worktree(store, journal.session_id, &journal.worktree_root)?;
    let mut paths = BTreeSet::new();
    let mut temporary_names = BTreeSet::new();
    for (index, item) in journal.items.iter().enumerate() {
        if !paths.insert(item.path.clone()) {
            return Err(RestoreError::BatchJournalDuplicatePath);
        }
        let plan_item = &items[index];
        if plan_item.path != item.path
            || !same_optional_node(
                plan_item.expected.to_entry(&item.path).as_ref(),
                item.expected.to_entry(&item.path).as_ref(),
            )
            || !same_optional_node(
                plan_item.desired.to_entry(&item.path).as_ref(),
                item.desired.to_entry(&item.path).as_ref(),
            )
        {
            return Err(RestoreError::RecoveryPlanMismatch);
        }
        let expected_stage = format!(".anchor-stage-{transaction_id}-{index}");
        let expected_backup = format!(".anchor-backup-{transaction_id}-{index}");
        if item.stage_name != expected_stage || item.backup_name != expected_backup {
            return Err(RestoreError::UnsafeJournalName);
        }
        let stage = validate_journal_temp_name(&item.stage_name, ".anchor-stage-")?;
        let backup = validate_journal_temp_name(&item.backup_name, ".anchor-backup-")?;
        if !temporary_names.insert(stage) || !temporary_names.insert(backup) {
            return Err(RestoreError::UnsafeJournalName);
        }
    }
    if matches!(
        journal.state,
        BatchJournalState::Verified
            | BatchJournalState::Cleaning
            | BatchJournalState::CleanupComplete
    ) {
        finish_batch(store, &worktree, journal, journal_path)
    } else {
        rollback_batch(store, &worktree, journal)?;
        journal.state = BatchJournalState::RolledBack;
        save_batch_journal(journal_path, journal)
    }
}

#[cfg(not(unix))]
fn recover_transactions(store: &SessionStore) -> Result<TransactionRecoveryReport, RestoreError> {
    windows::recover_transactions(store)
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
    if journal.schema != FILE_JOURNAL_SCHEMA {
        return Err(RestoreError::LegacyRecoveryUnsupported(
            journal_path.display().to_string(),
        ));
    }
    let plan_id = journal.plan_id.ok_or_else(|| {
        RestoreError::LegacyRecoveryUnsupported(journal_path.display().to_string())
    })?;
    let transaction_id = journal.transaction_id.ok_or_else(|| {
        RestoreError::LegacyRecoveryUnsupported(journal_path.display().to_string())
    })?;
    validate_transaction_directory(journal_path, &transaction_id.to_string())?;
    let plan = validate_restore_plan(store, plan_id, journal.session_id)?;
    let PlanOperation::Worktree { items, .. } = &plan.operation else {
        return Err(RestoreError::RecoveryPlanMismatch);
    };
    let [plan_item] = items.as_slice() else {
        return Err(RestoreError::RecoveryPlanMismatch);
    };
    if plan_item.path != journal.path {
        return Err(RestoreError::RecoveryPlanMismatch);
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
    if !same_optional_node(
        plan_item.expected.to_entry(&journal.path).as_ref(),
        expected.as_ref(),
    ) || !same_optional_node(
        plan_item.desired.to_entry(&journal.path).as_ref(),
        desired.as_ref(),
    ) {
        return Err(RestoreError::RecoveryPlanMismatch);
    }
    let host = journal.path.to_host_path()?;
    let name = host.file_name().ok_or(RestoreError::UnsafeRootPath)?;
    let parent_path = host.parent().unwrap_or_else(|| Path::new(""));
    let root = Dir::open_ambient_dir(&worktree, ambient_authority())?;
    let parent = if parent_path.as_os_str().is_empty() {
        root.try_clone()?
    } else {
        root.open_dir(parent_path)?
    };
    if journal.stage_name != format!(".anchor-stage-{transaction_id}")
        || journal.backup_name.as_deref() != Some(&format!(".anchor-backup-{transaction_id}"))
    {
        return Err(RestoreError::UnsafeJournalName);
    }
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

fn same_optional_node(left: Option<&ManifestEntry>, right: Option<&ManifestEntry>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.path == right.path && left.node == right.node && left.safety == right.safety
        }
        _ => false,
    }
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
    if journal.schema != 4 {
        return Err(RestoreError::LegacyRecoveryUnsupported(
            journal_path.display().to_string(),
        ));
    }
    let plan_id = journal.plan_id.ok_or_else(|| {
        RestoreError::LegacyRecoveryUnsupported(journal_path.display().to_string())
    })?;
    let transaction_id = journal.transaction_id.ok_or_else(|| {
        RestoreError::LegacyRecoveryUnsupported(journal_path.display().to_string())
    })?;
    validate_transaction_directory(journal_path, &format!("index-{transaction_id}"))?;
    let plan = validate_restore_plan(store, plan_id, journal.session_id)?;
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
    let PlanOperation::Index {
        index_path,
        expected: plan_expected,
        desired: plan_desired,
    } = &plan.operation
    else {
        return Err(RestoreError::RecoveryPlanMismatch);
    };
    if index_path
        != journal
            .index_path
            .as_ref()
            .ok_or(RestoreError::IncompleteRecoveryJournal)?
        || plan_expected != expected
        || plan_desired != desired
    {
        return Err(RestoreError::RecoveryPlanMismatch);
    }
    let parent_path = recorded_index
        .parent()
        .ok_or(RestoreError::UnsafeIndexPath)?;
    let name = recorded_index
        .file_name()
        .ok_or(RestoreError::UnsafeIndexPath)?;
    let parent = Dir::open_ambient_dir(parent_path, ambient_authority())?;
    if journal.backup_name.as_deref() != Some(&format!(".anchor-index-backup-{transaction_id}")) {
        return Err(RestoreError::UnsafeJournalName);
    }
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

#[cfg(not(unix))]
fn validate_store_identity(store: &SessionStore, context: &GitContext) -> Result<(), RestoreError> {
    let location = context.store_location();
    if location.worktree_key != store.worktree_key
        || std::fs::canonicalize(location.root)? != std::fs::canonicalize(store.root())?
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

#[cfg(unix)]
fn validate_transaction_directory(journal_path: &Path, expected: &str) -> Result<(), RestoreError> {
    let actual = journal_path
        .parent()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .ok_or(RestoreError::UnsafeJournalName)?;
    if actual != expected {
        return Err(RestoreError::UnsafeJournalName);
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn scan_transactions(root: &Path) -> Result<TransactionSummary, RestoreError> {
    windows::scan_transactions(root)
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
    #[error("session predates complete policy freezing and is review-only")]
    LegacySessionWithoutFrozenPolicy,
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
    #[error(
        "restore path addresses protected Git, Anchor-store, submodule, or nested-repository data: {0:?}"
    )]
    ProtectedWorktreePath(NativeRelativePath),
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
    #[error("restore journal is not semantically bound to the retained session restore plan")]
    RecoveryPlanMismatch,
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
    #[cfg(windows)]
    #[error(transparent)]
    Windows(#[from] anchor_windows::WindowsError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Git(#[from] anchor_git::GitError),
    #[error(transparent)]
    Capture(#[from] anchor_core::CaptureError),
    #[error(transparent)]
    Plan(#[from] anchor_core::RestorePlanError),
    #[error(transparent)]
    PlanRecord(#[from] crate::restore_plan::RestorePlanError),
    #[error(transparent)]
    Path(#[from] anchor_core::PlatformMismatch),
    #[error(transparent)]
    UnsafePath(#[from] anchor_core::PathError),
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
    use std::process::{Command, Stdio};

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
        assert!(
            matches!(result, RestoreApplyResult::Applied { .. }),
            "{result:?}"
        );
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
    #[ignore = "invoked only as the child of subprocess_crash_recovery_matrix"]
    fn subprocess_restore_crash_helper() {
        let root = PathBuf::from(
            std::env::var_os("ANCHOR_CRASH_WORKTREE")
                .expect("crash helper requires ANCHOR_CRASH_WORKTREE"),
        );
        let session = std::env::var("ANCHOR_CRASH_SESSION")
            .expect("crash helper requires ANCHOR_CRASH_SESSION")
            .parse()
            .expect("invalid crash helper session ID");
        let context = GitContext::discover(&root).unwrap();
        let location = context.store_location();
        let store = SessionStore::open(location.root, location.worktree_key).unwrap();
        let WholeRestoreResult::Preview {
            current_manifest, ..
        } = RestoreService::restore_all(&store, session, WholeRestoreMode::Preview).unwrap()
        else {
            panic!("expected whole-restore preview");
        };
        RestoreService::restore_all(
            &store,
            session,
            WholeRestoreMode::Apply {
                expected_current: current_manifest,
            },
        )
        .unwrap();
        panic!("crash helper passed its requested boundary without pausing");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn subprocess_crash_recovery_matrix() {
        use std::os::unix::fs::PermissionsExt as _;

        const BOUNDARIES: &[&str] = &[
            "journal-created",
            "first-output-staged",
            "all-outputs-staged",
            "first-current-evacuated",
            "all-current-evacuated",
            "first-desired-installed",
            "all-desired-installed",
            "first-desired-verified",
            "all-desired-verified",
            "commit-recorded",
            "backup-cleanup-started",
            "cleanup-completed",
        ];

        for boundary in BOUNDARIES {
            let root = repository();
            fs::write(root.path().join("alpha"), b"alpha-base").unwrap();
            fs::write(root.path().join("removed"), b"removed-base").unwrap();
            std::os::unix::fs::symlink("before-target", root.path().join("link")).unwrap();
            fs::create_dir(root.path().join("empty-removed")).unwrap();
            fs::write(root.path().join("mode"), b"mode-bytes").unwrap();
            fs::write(root.path().join("old"), b"rename-bytes").unwrap();
            let (store, session) = run_change(
                root.path(),
                "printf alpha-session > alpha; rm removed; printf added-session > added; \
                 ln -sfn after-target link; rmdir empty-removed; mkdir empty-added; \
                 chmod +x mode; mv old new",
            );
            let marker_dir = tempfile::tempdir().unwrap();
            let marker = marker_dir.path().join("durable-boundary");
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "restore::tests::subprocess_restore_crash_helper",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("ANCHOR_CRASH_BOUNDARY", boundary)
                .env("ANCHOR_CRASH_MARKER", &marker)
                .env("ANCHOR_CRASH_WORKTREE", root.path())
                .env("ANCHOR_CRASH_SESSION", session.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();

            let mut reached = false;
            for _ in 0..500 {
                if marker.exists() {
                    reached = true;
                    break;
                }
                if let Some(status) = child.try_wait().unwrap() {
                    panic!("crash helper exited at {boundary} with {status}");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(reached, "crash helper did not reach {boundary}");
            child.kill().unwrap();
            let status = child.wait().unwrap();
            assert!(
                !status.success(),
                "crash helper was not killed at {boundary}"
            );

            let report = TransactionRecoveryService::recover(&store).unwrap();
            let post_commit = matches!(
                *boundary,
                "commit-recorded" | "backup-cleanup-started" | "cleanup-completed"
            );
            if post_commit {
                assert_eq!(report.completed.len(), 1, "{boundary}");
                assert!(report.rolled_back.is_empty(), "{boundary}");
                assert_eq!(
                    fs::read(root.path().join("alpha")).unwrap(),
                    b"alpha-base",
                    "{boundary}"
                );
                assert_eq!(
                    fs::read(root.path().join("removed")).unwrap(),
                    b"removed-base",
                    "{boundary}"
                );
                assert!(!root.path().join("added").exists(), "{boundary}");
                assert_eq!(
                    fs::read_link(root.path().join("link")).unwrap(),
                    PathBuf::from("before-target"),
                    "{boundary}"
                );
                assert!(root.path().join("empty-removed").is_dir(), "{boundary}");
                assert!(!root.path().join("empty-added").exists(), "{boundary}");
                assert_eq!(
                    fs::metadata(root.path().join("mode"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o111,
                    0,
                    "{boundary}"
                );
                assert_eq!(
                    fs::read(root.path().join("old")).unwrap(),
                    b"rename-bytes",
                    "{boundary}"
                );
                assert!(!root.path().join("new").exists(), "{boundary}");
            } else {
                assert_eq!(report.rolled_back.len(), 1, "{boundary}");
                assert!(report.completed.is_empty(), "{boundary}");
                assert_eq!(
                    fs::read(root.path().join("alpha")).unwrap(),
                    b"alpha-session",
                    "{boundary}"
                );
                assert_eq!(
                    fs::read(root.path().join("added")).unwrap(),
                    b"added-session",
                    "{boundary}"
                );
                assert!(!root.path().join("removed").exists(), "{boundary}");
                assert_eq!(
                    fs::read_link(root.path().join("link")).unwrap(),
                    PathBuf::from("after-target"),
                    "{boundary}"
                );
                assert!(!root.path().join("empty-removed").exists(), "{boundary}");
                assert!(root.path().join("empty-added").is_dir(), "{boundary}");
                assert_ne!(
                    fs::metadata(root.path().join("mode"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o111,
                    0,
                    "{boundary}"
                );
                assert!(!root.path().join("old").exists(), "{boundary}");
                assert_eq!(
                    fs::read(root.path().join("new")).unwrap(),
                    b"rename-bytes",
                    "{boundary}"
                );
            }
            let terminal = scan_transactions(store.root()).unwrap();
            assert_eq!(terminal.total, terminal.complete, "{boundary}");
            assert!(
                fs::read_dir(root.path()).unwrap().all(|entry| {
                    let name = entry.unwrap().file_name();
                    !name.to_string_lossy().starts_with(".anchor-")
                }),
                "recovery left a sibling temporary node after {boundary}"
            );
        }
    }

    #[test]
    fn plan_bound_recovery_refuses_a_journal_authored_transformation() {
        let root = repository();
        fs::write(root.path().join("file"), b"base").unwrap();
        fs::write(root.path().join("unrelated"), b"keep").unwrap();
        let (store, session) = run_change(root.path(), "printf session > file");
        let WholeRestoreResult::Preview {
            current_manifest, ..
        } = RestoreService::restore_all(&store, session, WholeRestoreMode::Preview).unwrap()
        else {
            panic!("expected a whole-restore preview");
        };
        inject_batch_fault(BatchFaultPoint::Prepared);
        let error = RestoreService::restore_all(
            &store,
            session,
            WholeRestoreMode::Apply {
                expected_current: current_manifest,
            },
        )
        .unwrap_err();
        assert!(matches!(error, RestoreError::InjectedBatchCrash));

        let transaction = fs::read_dir(store.root().join("transactions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let journal_path = transaction.join("journal.cbor");
        let mut journal: BatchRestoreJournal =
            ciborium::de::from_reader(fs::read(&journal_path).unwrap().as_slice()).unwrap();
        journal.items[0].path = selected(b"unrelated");
        save_batch_journal(&journal_path, &journal).unwrap();

        let error = TransactionRecoveryService::recover(&store).unwrap_err();
        assert!(matches!(error, RestoreError::RecoveryPlanMismatch));
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"session");
        assert_eq!(fs::read(root.path().join("unrelated")).unwrap(), b"keep");
        assert_eq!(scan_transactions(store.root()).unwrap().unfinished, 1);
    }

    #[test]
    fn plan_bound_recovery_refuses_a_non_derived_temporary_name() {
        let root = repository();
        fs::write(root.path().join("file"), b"base").unwrap();
        let (store, session) = run_change(root.path(), "printf session > file");
        let WholeRestoreResult::Preview {
            current_manifest, ..
        } = RestoreService::restore_all(&store, session, WholeRestoreMode::Preview).unwrap()
        else {
            panic!("expected a whole-restore preview");
        };
        inject_batch_fault(BatchFaultPoint::Prepared);
        RestoreService::restore_all(
            &store,
            session,
            WholeRestoreMode::Apply {
                expected_current: current_manifest,
            },
        )
        .unwrap_err();

        let transaction = fs::read_dir(store.root().join("transactions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let journal_path = transaction.join("journal.cbor");
        let mut journal: BatchRestoreJournal =
            ciborium::de::from_reader(fs::read(&journal_path).unwrap().as_slice()).unwrap();
        journal.items[0].stage_name = ".anchor-stage-attacker".to_owned();
        save_batch_journal(&journal_path, &journal).unwrap();

        let error = TransactionRecoveryService::recover(&store).unwrap_err();
        assert!(matches!(error, RestoreError::UnsafeJournalName));
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"session");
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
                plan_id: None,
                transaction_id: None,
                worktree_root: anchor_core::NativeString::from_host(root.path().as_os_str()),
                worktree_key: store.worktree_key.clone(),
                state: BatchJournalState::Prepared,
                items: vec![item(0), item(1)],
            },
        )
        .unwrap();

        let error = TransactionRecoveryService::recover(&store).unwrap_err();
        assert!(matches!(error, RestoreError::LegacyRecoveryUnsupported(_)));
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
    fn refuses_an_unbound_interrupted_file_transaction() {
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
                plan_id: None,
                transaction_id: None,
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

        let error = TransactionRecoveryService::recover(&store).unwrap_err();
        assert!(matches!(error, RestoreError::LegacyRecoveryUnsupported(_)));
        assert!(!root.path().join("file").exists());
        assert!(root.path().join(stage_text).exists());
        assert!(root.path().join(backup_text).exists());
        let summary = scan_transactions(store.root()).unwrap();
        assert_eq!(summary.unfinished, 1);
    }

    #[test]
    fn refuses_an_unbound_batch_at_a_commit_point() {
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
                plan_id: None,
                transaction_id: None,
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

        let error = TransactionRecoveryService::recover(&store).unwrap_err();
        assert!(matches!(error, RestoreError::LegacyRecoveryUnsupported(_)));
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"base");
        assert!(root.path().join(backup_text).exists());
        let summary = scan_transactions(store.root()).unwrap();
        assert_eq!(summary.unfinished, 1);
    }

    #[test]
    fn refuses_an_unbound_interrupted_index_transaction() {
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
                plan_id: None,
                transaction_id: None,
                backup_name: Some(backup_text),
                worktree_key: Some(store.worktree_key.clone()),
                index_path: Some(anchor_core::NativeString::from_host(index_path.as_os_str())),
                expected: Some(expected),
                desired: Some(desired),
                state: JournalState::Installed,
            },
        )
        .unwrap();

        let error = TransactionRecoveryService::recover(&store).unwrap_err();
        assert!(matches!(error, RestoreError::LegacyRecoveryUnsupported(_)));
        assert_eq!(fs::read(&index_path).unwrap(), index);
        assert_eq!(scan_transactions(store.root()).unwrap().unfinished, 1);
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
    fn restore_refuses_git_metadata_before_selected_capture() {
        let root = repository();
        let (store, session) = run_change(root.path(), "printf session > added");
        let config_before = fs::read(root.path().join(".git").join("config")).unwrap();
        let selected = NativeRelativePath::from_host_path(Path::new(".git/config")).unwrap();

        let error = RestoreService::restore_file(&store, session, selected.clone()).unwrap_err();

        assert!(matches!(
            error,
            RestoreError::ProtectedWorktreePath(path) if path == selected
        ));
        assert_eq!(
            fs::read(root.path().join(".git").join("config")).unwrap(),
            config_before
        );
    }

    #[test]
    fn restore_refuses_a_hardlinked_regular_file() {
        use std::os::unix::fs::MetadataExt as _;

        let root = repository();
        fs::write(root.path().join("file"), b"base").unwrap();
        fs::hard_link(root.path().join("file"), root.path().join("alias")).unwrap();
        let (store, session) = run_change(root.path(), "printf session > file");
        let inode_before = fs::metadata(root.path().join("file")).unwrap().ino();

        let result = RestoreService::restore_file(&store, session, selected(b"file")).unwrap();

        assert!(matches!(
            result,
            RestoreApplyResult::Conflict {
                reason: ConflictReason::HardlinkTopology
            }
        ));
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"session");
        assert_eq!(fs::read(root.path().join("alias")).unwrap(), b"session");
        assert_eq!(
            fs::metadata(root.path().join("alias")).unwrap().ino(),
            inode_before
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn restore_refuses_a_regular_file_with_unmodeled_xattrs() {
        let root = repository();
        let path = root.path().join("file");
        fs::write(&path, b"base").unwrap();
        rustix::fs::setxattr(
            &path,
            "user.anchor-test",
            b"retain-me",
            rustix::fs::XattrFlags::empty(),
        )
        .unwrap();
        let (store, session) = run_change(root.path(), "printf session > file");

        let result = RestoreService::restore_file(&store, session, selected(b"file")).unwrap();

        assert!(matches!(
            result,
            RestoreApplyResult::Conflict {
                reason: ConflictReason::UnmodeledMetadataPresent
            }
        ));
        assert_eq!(fs::read(&path).unwrap(), b"session");
        let mut value = [0_u8; 32];
        let length = rustix::fs::getxattr(&path, "user.anchor-test", &mut value).unwrap();
        assert_eq!(&value[..length], b"retain-me");
    }

    #[test]
    fn restores_empty_directory_additions_and_deletions() {
        let added_root = repository();
        let (store, session) = run_change(added_root.path(), "mkdir added-empty");
        let result =
            RestoreService::restore_file(&store, session, selected(b"added-empty")).unwrap();
        assert!(
            matches!(result, RestoreApplyResult::Applied { .. }),
            "{result:?}"
        );
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
            RestorePlanId::from_bytes([0; 32]),
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

#[cfg(all(test, windows))]
mod windows_tests {
    use std::ffi::OsString;
    use std::fs;

    use anchor_core::NativeRelativePath;

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

    fn session_store(root: &Path) -> (SessionStore, PathBuf) {
        let context = GitContext::discover(root).unwrap();
        let location = context.store_location();
        (
            SessionStore::open(&location.root, location.worktree_key).unwrap(),
            location.root,
        )
    }

    #[test]
    fn restores_exact_session_bytes_through_native_transaction() {
        let root = repository();
        fs::write(root.path().join("file.txt"), b"before").unwrap();
        let result = SessionRunner::run(&RunRequest {
            invocation_directory: root.path().to_path_buf(),
            command: vec![
                OsString::from("cmd.exe"),
                OsString::from("/C"),
                OsString::from("> file.txt (echo session)"),
            ],
            capture_policy: CapturePolicy::default(),
        })
        .unwrap();
        let (store, store_root) = session_store(root.path());
        let path = NativeRelativePath::from_host_path(Path::new("file.txt")).unwrap();
        let restored = RestoreService::restore_file(&store, result.session_id, path);
        assert!(
            matches!(restored, Ok(RestoreApplyResult::Applied { .. })),
            "{restored:?}"
        );
        assert_eq!(fs::read(root.path().join("file.txt")).unwrap(), b"before");
        drop(store);
        fs::remove_dir_all(store_root).unwrap();
    }

    #[test]
    fn refuses_to_overwrite_post_session_windows_bytes() {
        let root = repository();
        fs::write(root.path().join("file.txt"), b"before").unwrap();
        let result = SessionRunner::run(&RunRequest {
            invocation_directory: root.path().to_path_buf(),
            command: vec![
                OsString::from("cmd.exe"),
                OsString::from("/C"),
                OsString::from("> file.txt (echo session)"),
            ],
            capture_policy: CapturePolicy::default(),
        })
        .unwrap();
        fs::write(root.path().join("file.txt"), b"post-session").unwrap();
        let (store, store_root) = session_store(root.path());
        let path = NativeRelativePath::from_host_path(Path::new("file.txt")).unwrap();
        assert!(matches!(
            RestoreService::restore_file(&store, result.session_id, path),
            Ok(RestoreApplyResult::Conflict { .. })
        ));
        assert_eq!(
            fs::read(root.path().join("file.txt")).unwrap(),
            b"post-session"
        );
        drop(store);
        fs::remove_dir_all(store_root).unwrap();
    }

    #[test]
    fn restores_raw_windows_index_only_after_exact_endpoint_match() {
        let root = repository();
        let index = empty_v2_index();
        fs::write(root.path().join(".git").join("index"), &index).unwrap();
        let result = SessionRunner::run(&RunRequest {
            invocation_directory: root.path().to_path_buf(),
            command: vec![
                OsString::from("cmd.exe"),
                OsString::from("/C"),
                OsString::from("del /Q .git\\index"),
            ],
            capture_policy: CapturePolicy::default(),
        })
        .unwrap();
        let (store, store_root) = session_store(root.path());
        assert_eq!(
            RestoreService::restore_index(&store, result.session_id).unwrap(),
            IndexRestoreResult::Applied
        );
        assert_eq!(
            fs::read(root.path().join(".git").join("index")).unwrap(),
            index
        );
        drop(store);
        fs::remove_dir_all(store_root).unwrap();
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
