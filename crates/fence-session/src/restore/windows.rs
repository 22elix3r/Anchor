use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use atomic_write_file::AtomicWriteFile;
use fence_core::{
    CaptureEngine, CaptureOptions, Completeness, Coverage, Manifest, ManifestEntry, ManifestNode,
    NativeRelativePath, NativeString, ObjectStore, ObservedKind, ScopeClassifier, ScopeDecision,
    ScopeError, WindowsSymlinkKind,
};
use fence_git::{GitContext, IndexCapture};
use fence_windows::{DirectoryHandle, MutationRoot, SymbolicLinkData};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    BatchWrite, RestoreError, SessionId, SessionStore, TransactionRecoveryReport,
    TransactionSummary, same_optional_node, validate_restore_plan,
};
use crate::restore_plan::{PlanOperation, RestorePlanId};

const JOURNAL_TAG: u64 = 0x414e_4348_4f52_574a;
const JOURNAL_SCHEMA: u16 = 2;
const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    tag: u64,
    schema: u16,
    session_id: SessionId,
    #[serde(default)]
    plan_id: Option<RestorePlanId>,
    #[serde(default)]
    transaction_id: Option<Uuid>,
    worktree_root: NativeString,
    worktree_key: String,
    state: JournalState,
    items: Vec<JournalItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JournalItem {
    path: NativeRelativePath,
    stage_name: String,
    backup_name: String,
    expected: Presence,
    desired: Presence,
    state: ItemState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct IndexJournal {
    tag: u64,
    schema: u16,
    session_id: SessionId,
    #[serde(default)]
    plan_id: Option<RestorePlanId>,
    #[serde(default)]
    transaction_id: Option<Uuid>,
    worktree_key: String,
    index_path: NativeString,
    backup_name: String,
    expected: IndexCapture,
    desired: IndexCapture,
    state: JournalState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Presence {
    Absent,
    Present(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum JournalState {
    Prepared,
    Evacuating,
    Installing,
    Verified,
    Complete,
    NeedsRecovery,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum ItemState {
    Prepared,
    Staged,
    Evacuated,
    Installed,
    Verified,
}

pub(super) fn apply_one(
    store: &SessionStore,
    session_id: SessionId,
    plan_id: RestorePlanId,
    worktree: &Path,
    path: &NativeRelativePath,
    expected: Option<&ManifestEntry>,
    desired: Option<&ManifestEntry>,
) -> Result<(), RestoreError> {
    apply_batch(
        store,
        session_id,
        plan_id,
        worktree,
        &[BatchWrite {
            path: path.clone(),
            expected: expected.cloned(),
            desired: desired.cloned(),
        }],
    )
}

pub(super) fn apply_batch(
    store: &SessionStore,
    session_id: SessionId,
    plan_id: RestorePlanId,
    worktree: &Path,
    writes: &[BatchWrite],
) -> Result<(), RestoreError> {
    if writes.is_empty() {
        return Ok(());
    }
    let id = Uuid::now_v7();
    let transaction = store
        .root()
        .join("transactions")
        .join(format!("windows-batch-{id}"));
    private_transaction_dir(&transaction)?;
    let journal_path = transaction.join("journal.cbor");
    let items = writes
        .iter()
        .enumerate()
        .map(|(index, write)| {
            Ok(JournalItem {
                path: write.path.clone(),
                stage_name: format!(".fence-stage-{id}-{index}"),
                backup_name: format!(".fence-backup-{id}-{index}"),
                expected: Presence::from_entry(write.expected.as_ref())?,
                desired: Presence::from_entry(write.desired.as_ref())?,
                state: ItemState::Prepared,
            })
        })
        .collect::<Result<Vec<_>, RestoreError>>()?;
    let mut journal = Journal {
        tag: JOURNAL_TAG,
        schema: JOURNAL_SCHEMA,
        session_id,
        plan_id: Some(plan_id),
        transaction_id: Some(id),
        worktree_root: NativeString::from_host(worktree.as_os_str()),
        worktree_key: store.worktree_key.clone(),
        state: JournalState::Prepared,
        items,
    };
    save_journal(&journal_path, &journal)?;
    if let Err(error) = apply_inner(store, worktree, &mut journal, &journal_path) {
        if journal.state == JournalState::Verified {
            return Err(error);
        }
        journal.state = JournalState::NeedsRecovery;
        save_journal(&journal_path, &journal)?;
        if let Err(rollback) = rollback(store, worktree, &journal) {
            return Err(RestoreError::BatchRollbackFailed {
                apply: error.to_string(),
                rollback: rollback.to_string(),
            });
        }
        journal.state = JournalState::RolledBack;
        save_journal(&journal_path, &journal)?;
        return Err(error);
    }
    Ok(())
}

pub(super) fn apply_index(
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
    let id = Uuid::now_v7();
    let transaction = store
        .root()
        .join("transactions")
        .join(format!("windows-index-{id}"));
    private_transaction_dir(&transaction)?;
    let journal_path = transaction.join("journal.cbor");
    let mut journal = IndexJournal {
        tag: JOURNAL_TAG,
        schema: JOURNAL_SCHEMA,
        session_id,
        plan_id: Some(plan_id),
        transaction_id: Some(id),
        worktree_key: store.worktree_key.clone(),
        index_path: NativeString::from_host(index_path.as_os_str()),
        backup_name: format!(".fence-index-backup-{id}"),
        expected: expected.clone(),
        desired: desired.clone(),
        state: JournalState::Prepared,
    };
    save_index_journal(&journal_path, &journal)?;

    let root = MutationRoot::open(parent_path)?;
    let parent = root.directory();
    let mut lock = parent.create_new_file(lock_name)?;
    if let IndexCapture::Present {
        object, raw_size, ..
    } = desired
    {
        store
            .objects()
            .copy_verified(*object, *raw_size, &mut lock)?;
    }
    lock.sync_all()?;
    drop(lock);
    if !verify_index_node(parent, name, expected, store.objects())? {
        parent.remove_child(lock_name)?;
        return Err(RestoreError::CurrentIndexChanged);
    }

    let had_current = matches!(expected, IndexCapture::Present { .. });
    if had_current {
        parent.rename_child_noreplace(name, OsStr::new(&journal.backup_name))?;
        journal.state = JournalState::Evacuating;
        save_index_journal(&journal_path, &journal)?;
        if !verify_index_node(
            parent,
            OsStr::new(&journal.backup_name),
            expected,
            store.objects(),
        )? {
            journal.state = JournalState::NeedsRecovery;
            save_index_journal(&journal_path, &journal)?;
            return Err(RestoreError::CurrentIndexChanged);
        }
    }
    journal.state = JournalState::Installing;
    save_index_journal(&journal_path, &journal)?;
    if matches!(desired, IndexCapture::Present { .. }) {
        parent.rename_child_noreplace(lock_name, name)?;
    } else {
        parent.remove_child(lock_name)?;
    }
    if !verify_index_node(parent, name, desired, store.objects())? {
        journal.state = JournalState::NeedsRecovery;
        save_index_journal(&journal_path, &journal)?;
        return Err(RestoreError::VerificationFailed);
    }
    journal.state = JournalState::Verified;
    save_index_journal(&journal_path, &journal)?;
    if had_current {
        parent.remove_child(OsStr::new(&journal.backup_name))?;
    }
    journal.state = JournalState::Complete;
    save_index_journal(&journal_path, &journal)
}

fn apply_inner(
    store: &SessionStore,
    worktree: &Path,
    journal: &mut Journal,
    journal_path: &Path,
) -> Result<(), RestoreError> {
    let root = MutationRoot::open(worktree)?;
    for index in 0..journal.items.len() {
        let item = &journal.items[index];
        let desired = item.desired.to_entry(&item.path)?;
        if let Some(desired) = &desired {
            let (parent, _) = open_parent(root.directory(), &item.path)?;
            stage_node(
                &parent,
                OsStr::new(&item.stage_name),
                desired,
                store.objects(),
            )?;
            if !verify_alternate(
                worktree,
                &item.path,
                OsStr::new(&item.stage_name),
                Some(desired),
                store.objects(),
            )? {
                return Err(RestoreError::VerificationFailed);
            }
        }
        journal.items[index].state = ItemState::Staged;
        save_journal(journal_path, journal)?;
    }

    journal.state = JournalState::Evacuating;
    save_journal(journal_path, journal)?;
    for index in 0..journal.items.len() {
        let item = &journal.items[index];
        let expected = item.expected.to_entry(&item.path)?;
        if !verify_path(worktree, &item.path, expected.as_ref(), store.objects())? {
            return Err(RestoreError::CurrentChanged);
        }
        if expected.is_some() {
            let (parent, name) = open_parent(root.directory(), &item.path)?;
            parent.rename_child_noreplace(&name, OsStr::new(&item.backup_name))?;
            if !verify_alternate(
                worktree,
                &item.path,
                OsStr::new(&item.backup_name),
                expected.as_ref(),
                store.objects(),
            )? {
                return Err(RestoreError::CurrentChanged);
            }
        }
        journal.items[index].state = ItemState::Evacuated;
        save_journal(journal_path, journal)?;
    }

    journal.state = JournalState::Installing;
    save_journal(journal_path, journal)?;
    for index in 0..journal.items.len() {
        let item = &journal.items[index];
        if item.desired.is_present() {
            let (parent, name) = open_parent(root.directory(), &item.path)?;
            parent.rename_child_noreplace(OsStr::new(&item.stage_name), &name)?;
        }
        journal.items[index].state = ItemState::Installed;
        save_journal(journal_path, journal)?;
    }

    for index in 0..journal.items.len() {
        let item = &journal.items[index];
        let desired = item.desired.to_entry(&item.path)?;
        if !verify_path(worktree, &item.path, desired.as_ref(), store.objects())? {
            return Err(RestoreError::VerificationFailed);
        }
        journal.items[index].state = ItemState::Verified;
        save_journal(journal_path, journal)?;
    }
    journal.state = JournalState::Verified;
    save_journal(journal_path, journal)?;
    finish(store, worktree, journal, journal_path)
}

fn finish(
    store: &SessionStore,
    worktree: &Path,
    journal: &mut Journal,
    journal_path: &Path,
) -> Result<(), RestoreError> {
    let root = MutationRoot::open(worktree)?;
    for item in &journal.items {
        let expected = item.expected.to_entry(&item.path)?;
        let desired = item.desired.to_entry(&item.path)?;
        if !verify_path(worktree, &item.path, desired.as_ref(), store.objects())? {
            return Err(RestoreError::RecoveryCurrentChanged);
        }
        let (parent, _) = open_parent(root.directory(), &item.path)?;
        let backup = OsStr::new(&item.backup_name);
        if child_exists(&parent, backup)? {
            if !verify_alternate(
                worktree,
                &item.path,
                backup,
                expected.as_ref(),
                store.objects(),
            )? {
                return Err(RestoreError::RecoveryBackupMismatch);
            }
            parent.remove_child(backup)?;
        }
        let stage = OsStr::new(&item.stage_name);
        if child_exists(&parent, stage)? {
            if desired.is_none()
                || !verify_alternate(
                    worktree,
                    &item.path,
                    stage,
                    desired.as_ref(),
                    store.objects(),
                )?
            {
                return Err(RestoreError::RecoveryStageMismatch);
            }
            parent.remove_child(stage)?;
        }
    }
    journal.state = JournalState::Complete;
    save_journal(journal_path, journal)
}

fn rollback(store: &SessionStore, worktree: &Path, journal: &Journal) -> Result<(), RestoreError> {
    let root = MutationRoot::open(worktree)?;
    for item in journal.items.iter().rev() {
        let expected = item.expected.to_entry(&item.path)?;
        let desired = item.desired.to_entry(&item.path)?;
        let (parent, name) = open_parent(root.directory(), &item.path)?;
        let backup = OsStr::new(&item.backup_name);
        if child_exists(&parent, backup)? {
            if expected.is_none()
                || !verify_alternate(
                    worktree,
                    &item.path,
                    backup,
                    expected.as_ref(),
                    store.objects(),
                )?
            {
                return Err(RestoreError::RecoveryBackupMismatch);
            }
            if verify_path(worktree, &item.path, expected.as_ref(), store.objects())? {
                parent.remove_child(backup)?;
            } else if verify_path(worktree, &item.path, desired.as_ref(), store.objects())? {
                if desired.is_some() {
                    parent.remove_child(&name)?;
                }
                parent.rename_child_noreplace(backup, &name)?;
            } else if verify_path(worktree, &item.path, None, store.objects())? {
                parent.rename_child_noreplace(backup, &name)?;
            } else {
                return Err(RestoreError::RecoveryCurrentChanged);
            }
        } else if !verify_path(worktree, &item.path, expected.as_ref(), store.objects())? {
            if expected.is_none()
                && desired.is_some()
                && verify_path(worktree, &item.path, desired.as_ref(), store.objects())?
            {
                parent.remove_child(&name)?;
            } else {
                return Err(RestoreError::RecoveryBackupMissing);
            }
        }
        let stage = OsStr::new(&item.stage_name);
        if child_exists(&parent, stage)? {
            if desired.is_none()
                || !verify_alternate(
                    worktree,
                    &item.path,
                    stage,
                    desired.as_ref(),
                    store.objects(),
                )?
            {
                return Err(RestoreError::RecoveryStageMismatch);
            }
            parent.remove_child(stage)?;
        }
        if !verify_path(worktree, &item.path, expected.as_ref(), store.objects())? {
            return Err(RestoreError::VerificationFailed);
        }
    }
    Ok(())
}

pub(super) fn recover_transactions(
    store: &SessionStore,
) -> Result<TransactionRecoveryReport, RestoreError> {
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
        if let Ok(mut journal) = ciborium::de::from_reader::<Journal, _>(bytes.as_slice()) {
            validate_journal(store, &journal)?;
            if matches!(
                journal.state,
                JournalState::Complete | JournalState::RolledBack
            ) {
                continue;
            }
            if journal.worktree_key != store.worktree_key {
                report.skipped_other_worktrees = report.skipped_other_worktrees.saturating_add(1);
                continue;
            }
            let worktree = PathBuf::from(journal.worktree_root.to_host()?);
            if journal.state == JournalState::Verified {
                finish(store, &worktree, &mut journal, &journal_path)?;
                report.completed.push(id);
            } else {
                rollback(store, &worktree, &journal)?;
                journal.state = JournalState::RolledBack;
                save_journal(&journal_path, &journal)?;
                report.rolled_back.push(id);
            }
            continue;
        }
        let mut journal: IndexJournal = ciborium::de::from_reader(bytes.as_slice())
            .map_err(|error| RestoreError::Journal(error.to_string()))?;
        validate_index_journal(store, &journal)?;
        if matches!(
            journal.state,
            JournalState::Complete | JournalState::RolledBack
        ) {
            continue;
        }
        if journal.worktree_key != store.worktree_key {
            report.skipped_other_worktrees = report.skipped_other_worktrees.saturating_add(1);
            continue;
        }
        if journal.state == JournalState::Verified {
            finish_index(store, &mut journal, &journal_path)?;
            report.completed.push(id);
        } else {
            rollback_index(store, &journal)?;
            journal.state = JournalState::RolledBack;
            save_index_journal(&journal_path, &journal)?;
            report.rolled_back.push(id);
        }
    }
    Ok(report)
}

pub(super) fn scan_transactions(root: &Path) -> Result<TransactionSummary, RestoreError> {
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
        let path = entry.path().join("journal.cbor");
        if !path.exists() {
            summary.unfinished = summary.unfinished.saturating_add(1);
            continue;
        }
        let bytes = read_journal(&path)?;
        let state = ciborium::de::from_reader::<Journal, _>(bytes.as_slice())
            .map(|journal| journal.state)
            .or_else(|_| {
                ciborium::de::from_reader::<IndexJournal, _>(bytes.as_slice())
                    .map(|journal| journal.state)
            })
            .map_err(|error| RestoreError::Journal(error.to_string()))?;
        match state {
            JournalState::Complete | JournalState::RolledBack => {
                summary.complete = summary.complete.saturating_add(1);
            }
            JournalState::NeedsRecovery => {
                summary.needs_recovery = summary.needs_recovery.saturating_add(1);
            }
            JournalState::Prepared
            | JournalState::Evacuating
            | JournalState::Installing
            | JournalState::Verified => {
                summary.unfinished = summary.unfinished.saturating_add(1);
            }
        }
    }
    Ok(summary)
}

fn validate_index_journal(
    store: &SessionStore,
    journal: &IndexJournal,
) -> Result<(), RestoreError> {
    if journal.tag != JOURNAL_TAG
        || journal.schema != JOURNAL_SCHEMA
        || journal.worktree_key != store.worktree_key
    {
        return Err(RestoreError::RecoveryPathMismatch);
    }
    let plan_id = journal.plan_id.ok_or_else(|| {
        RestoreError::LegacyRecoveryUnsupported("Windows index journal".to_owned())
    })?;
    let transaction_id = journal.transaction_id.ok_or_else(|| {
        RestoreError::LegacyRecoveryUnsupported("Windows index journal".to_owned())
    })?;
    if journal.backup_name != format!(".fence-index-backup-{transaction_id}") {
        return Err(RestoreError::UnsafeJournalName);
    }
    let plan = validate_restore_plan(store, plan_id, journal.session_id)?;
    let PlanOperation::Index {
        index_path,
        expected,
        desired,
    } = &plan.operation
    else {
        return Err(RestoreError::RecoveryPlanMismatch);
    };
    if plan.session_id != journal.session_id
        || plan.worktree_key != journal.worktree_key
        || index_path != &journal.index_path
        || expected != &journal.expected
        || desired != &journal.desired
    {
        return Err(RestoreError::RecoveryPlanMismatch);
    }
    validate_temp_name(&journal.backup_name, ".fence-index-backup-")?;
    let session = store.load_session(journal.session_id)?;
    let worktree = PathBuf::from(session.worktree_root.to_host()?);
    let context = GitContext::discover(&worktree)?;
    let recorded = PathBuf::from(journal.index_path.to_host()?);
    if recorded != context.index_path() {
        return Err(RestoreError::RecoveryPathMismatch);
    }
    Ok(())
}

fn finish_index(
    store: &SessionStore,
    journal: &mut IndexJournal,
    journal_path: &Path,
) -> Result<(), RestoreError> {
    let index_path = PathBuf::from(journal.index_path.to_host()?);
    let parent_path = index_path.parent().ok_or(RestoreError::UnsafeIndexPath)?;
    let name = index_path
        .file_name()
        .ok_or(RestoreError::UnsafeIndexPath)?;
    let root = MutationRoot::open(parent_path)?;
    if !verify_index_node(root.directory(), name, &journal.desired, store.objects())? {
        return Err(RestoreError::RecoveryCurrentChanged);
    }
    let backup = OsStr::new(&journal.backup_name);
    if child_exists(root.directory(), backup)? {
        if !verify_index_node(root.directory(), backup, &journal.expected, store.objects())? {
            return Err(RestoreError::RecoveryBackupMismatch);
        }
        root.directory().remove_child(backup)?;
    }
    journal.state = JournalState::Complete;
    save_index_journal(journal_path, journal)
}

fn rollback_index(store: &SessionStore, journal: &IndexJournal) -> Result<(), RestoreError> {
    let index_path = PathBuf::from(journal.index_path.to_host()?);
    let parent_path = index_path.parent().ok_or(RestoreError::UnsafeIndexPath)?;
    let name = index_path
        .file_name()
        .ok_or(RestoreError::UnsafeIndexPath)?;
    let lock_path = index_path.with_extension("lock");
    let lock_name = lock_path.file_name().ok_or(RestoreError::UnsafeIndexPath)?;
    let root = MutationRoot::open(parent_path)?;
    let parent = root.directory();
    let backup = OsStr::new(&journal.backup_name);
    if child_exists(parent, backup)? {
        if !verify_index_node(parent, backup, &journal.expected, store.objects())? {
            return Err(RestoreError::RecoveryBackupMismatch);
        }
        if verify_index_node(parent, name, &journal.expected, store.objects())? {
            parent.remove_child(backup)?;
        } else if verify_index_node(parent, name, &journal.desired, store.objects())? {
            if matches!(journal.desired, IndexCapture::Present { .. }) {
                parent.remove_child(name)?;
            }
            parent.rename_child_noreplace(backup, name)?;
        } else if verify_index_node(parent, name, &IndexCapture::Absent, store.objects())? {
            parent.rename_child_noreplace(backup, name)?;
        } else {
            return Err(RestoreError::RecoveryCurrentChanged);
        }
    } else if !verify_index_node(parent, name, &journal.expected, store.objects())? {
        if matches!(journal.expected, IndexCapture::Absent)
            && verify_index_node(parent, name, &journal.desired, store.objects())?
        {
            parent.remove_child(name)?;
        } else {
            return Err(RestoreError::RecoveryBackupMissing);
        }
    }
    if child_exists(parent, lock_name)? {
        let valid = match journal.desired {
            IndexCapture::Present { .. } => {
                verify_index_node(parent, lock_name, &journal.desired, store.objects())?
            }
            IndexCapture::Absent => parent.open_named_child(lock_name)?.metadata().size == 0,
        };
        if !valid {
            return Err(RestoreError::RecoveryStageMismatch);
        }
        parent.remove_child(lock_name)?;
    }
    if !verify_index_node(parent, name, &journal.expected, store.objects())? {
        return Err(RestoreError::VerificationFailed);
    }
    Ok(())
}

fn verify_index_node(
    parent: &DirectoryHandle,
    name: &OsStr,
    expected: &IndexCapture,
    objects: &ObjectStore,
) -> Result<bool, RestoreError> {
    let entry = parent
        .entries()?
        .into_iter()
        .find(|entry| entry.name == name);
    match expected {
        IndexCapture::Absent => Ok(entry.is_none()),
        IndexCapture::Present {
            object, raw_size, ..
        } => {
            let Some(entry) = entry else {
                return Ok(false);
            };
            let node = parent.open_child(&entry)?;
            if node.metadata().kind != fence_windows::NodeKind::RegularFile {
                return Ok(false);
            }
            let mut file = node.try_clone_file()?;
            let (actual, size) = objects.put(&mut file)?;
            Ok(actual == *object && size == *raw_size)
        }
    }
}

fn validate_journal(store: &SessionStore, journal: &Journal) -> Result<(), RestoreError> {
    if journal.tag != JOURNAL_TAG || journal.schema != JOURNAL_SCHEMA {
        return Err(RestoreError::Journal(
            "unsupported Windows restore journal".to_owned(),
        ));
    }
    let session = store.load_session(journal.session_id)?;
    if session.worktree_root != journal.worktree_root {
        return Err(RestoreError::RecoveryPathMismatch);
    }
    let worktree = PathBuf::from(journal.worktree_root.to_host()?);
    let location = GitContext::discover(&worktree)?.store_location();
    if location.worktree_key != store.worktree_key
        || fs::canonicalize(location.root)? != fs::canonicalize(store.root())?
    {
        return Err(RestoreError::RecoveryPathMismatch);
    }
    let plan_id = journal.plan_id.ok_or_else(|| {
        RestoreError::LegacyRecoveryUnsupported("Windows worktree journal".to_owned())
    })?;
    let transaction_id = journal.transaction_id.ok_or_else(|| {
        RestoreError::LegacyRecoveryUnsupported("Windows worktree journal".to_owned())
    })?;
    let plan = validate_restore_plan(store, plan_id, journal.session_id)?;
    let PlanOperation::Worktree { items, .. } = &plan.operation else {
        return Err(RestoreError::RecoveryPlanMismatch);
    };
    if plan.session_id != journal.session_id
        || plan.worktree_root != journal.worktree_root
        || plan.worktree_key != journal.worktree_key
        || items.len() != journal.items.len()
    {
        return Err(RestoreError::RecoveryPlanMismatch);
    }
    let mut paths = BTreeSet::new();
    let mut names = BTreeSet::new();
    for (index, item) in journal.items.iter().enumerate() {
        let plan_item = &items[index];
        let expected = item.expected.to_entry(&item.path)?;
        let desired = item.desired.to_entry(&item.path)?;
        if plan_item.path != item.path
            || !same_optional_node(
                plan_item.expected.to_entry(&item.path).as_ref(),
                expected.as_ref(),
            )
            || !same_optional_node(
                plan_item.desired.to_entry(&item.path).as_ref(),
                desired.as_ref(),
            )
            || item.stage_name != format!(".fence-stage-{transaction_id}-{index}")
            || item.backup_name != format!(".fence-backup-{transaction_id}-{index}")
        {
            return Err(RestoreError::RecoveryPlanMismatch);
        }
        if !paths.insert(item.path.clone())
            || !names.insert(validate_temp_name(&item.stage_name, ".fence-stage-")?)
            || !names.insert(validate_temp_name(&item.backup_name, ".fence-backup-")?)
        {
            return Err(RestoreError::BatchJournalDuplicatePath);
        }
    }
    Ok(())
}

fn stage_node(
    parent: &DirectoryHandle,
    name: &OsStr,
    desired: &ManifestEntry,
    objects: &ObjectStore,
) -> Result<(), RestoreError> {
    match &desired.node {
        ManifestNode::Regular {
            object,
            raw_size,
            windows_readonly,
            ..
        } => {
            let mut file = parent.create_new_file(name)?;
            objects.copy_verified(*object, *raw_size, &mut file)?;
            file.sync_all()?;
            let mut permissions = file.metadata()?.permissions();
            permissions.set_readonly(windows_readonly.unwrap_or(false));
            file.set_permissions(permissions)?;
        }
        ManifestNode::Symlink {
            target,
            windows_link_kind,
            windows_substitute_name,
            windows_reparse_flags,
        } => {
            let kind = windows_link_kind.ok_or(RestoreError::PlatformMutationUnsupported)?;
            let data = SymbolicLinkData {
                substitute_name: windows_substitute_name
                    .as_ref()
                    .ok_or(RestoreError::PlatformMutationUnsupported)?
                    .to_host()?,
                print_name: target.to_host()?,
                flags: windows_reparse_flags.ok_or(RestoreError::PlatformMutationUnsupported)?,
            };
            parent.create_symbolic_link(name, &data, kind == WindowsSymlinkKind::Directory)?;
        }
        ManifestNode::EmptyDirectory => parent.create_new_directory(name)?,
    }
    Ok(())
}

fn open_parent(
    root: &DirectoryHandle,
    path: &NativeRelativePath,
) -> Result<(DirectoryHandle, OsString), RestoreError> {
    let host = path.to_host_path()?;
    let name = host
        .file_name()
        .ok_or(RestoreError::UnsafeRootPath)?
        .to_owned();
    let mut current = root.try_clone_mutation()?;
    for component in host.parent().unwrap_or_else(|| Path::new("")).components() {
        current = current.open_mutation_directory(component.as_os_str())?;
    }
    Ok((current, name))
}

fn verify_path(
    worktree: &Path,
    path: &NativeRelativePath,
    expected: Option<&ManifestEntry>,
    objects: &ObjectStore,
) -> Result<bool, RestoreError> {
    let scope = ExactScope { path: path.clone() };
    let capture =
        CaptureEngine::new(objects, CaptureOptions::default()).capture(worktree, &scope)?;
    let actual = capture
        .manifest
        .entries()
        .iter()
        .find(|entry| entry.path == *path);
    Ok(actual.map(|entry| &entry.node) == expected.map(|entry| &entry.node))
}

fn verify_alternate(
    worktree: &Path,
    original: &NativeRelativePath,
    alternate_name: &OsStr,
    expected: Option<&ManifestEntry>,
    objects: &ObjectStore,
) -> Result<bool, RestoreError> {
    let alternate = original
        .parent()
        .ok_or(RestoreError::UnsafeRootPath)?
        .join_host_component(alternate_name)?;
    let adjusted = expected.cloned().map(|mut entry| {
        entry.path = alternate.clone();
        entry
    });
    verify_path(worktree, &alternate, adjusted.as_ref(), objects)
}

fn child_exists(parent: &DirectoryHandle, name: &OsStr) -> Result<bool, RestoreError> {
    Ok(parent.entries()?.iter().any(|entry| entry.name == name))
}

struct ExactScope {
    path: NativeRelativePath,
}

impl ScopeClassifier for ExactScope {
    fn classify(
        &self,
        path: &NativeRelativePath,
        _kind: ObservedKind,
    ) -> Result<ScopeDecision, ScopeError> {
        if path == &self.path || self.path.components().starts_with(path.components()) {
            Ok(ScopeDecision::Include)
        } else {
            Ok(ScopeDecision::Exclude)
        }
    }
}

impl Presence {
    fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }

    fn from_entry(entry: Option<&ManifestEntry>) -> Result<Self, RestoreError> {
        let Some(entry) = entry else {
            return Ok(Self::Absent);
        };
        let manifest = Manifest::new(
            fence_core::PathEncoding::WindowsWtf16Le,
            vec![entry.clone()],
            Coverage {
                completeness: Completeness::Complete,
                omissions: Vec::new(),
            },
        )
        .map_err(|error| RestoreError::Journal(error.to_string()))?;
        Ok(Self::Present(manifest.encode().map_err(|error| {
            RestoreError::Journal(error.to_string())
        })?))
    }

    fn to_entry(&self, path: &NativeRelativePath) -> Result<Option<ManifestEntry>, RestoreError> {
        let Self::Present(bytes) = self else {
            return Ok(None);
        };
        let manifest =
            Manifest::decode(bytes).map_err(|error| RestoreError::Journal(error.to_string()))?;
        if manifest.entries().len() != 1 || manifest.entries()[0].path != *path {
            return Err(RestoreError::RecoveryPathMismatch);
        }
        Ok(Some(manifest.entries()[0].clone()))
    }
}

fn private_transaction_dir(path: &Path) -> Result<(), RestoreError> {
    fs::create_dir_all(path)?;
    fence_windows::harden_private_directory(path)?;
    Ok(())
}

fn save_journal(path: &Path, journal: &Journal) -> Result<(), RestoreError> {
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

fn save_index_journal(path: &Path, journal: &IndexJournal) -> Result<(), RestoreError> {
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

fn validate_temp_name(value: &str, prefix: &str) -> Result<OsString, RestoreError> {
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
