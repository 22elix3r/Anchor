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
    CaptureEngine, CaptureOptions, ConflictReason, ManifestEntry, ManifestNode, NativeRelativePath,
    NoChangeReason, ObservedKind, RestoreOutcome, RestorePlan, ScopeClassifier, ScopeDecision,
    ScopeError,
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
const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestoreApplyResult {
    Applied {
        session_id: SessionId,
        path: NativeRelativePath,
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

#[derive(Debug, Default)]
pub struct RestoreService;

impl RestoreService {
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
        let _store_lease = store.acquire_store_read_lease()?;
        let _lock = store.acquire_active_lock()?;
        ensure_no_unresolved_transactions(store.root())?;
        let session = store.load_session(session_id)?;
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
        if expected.is_some_and(|entry| matches!(entry.node, ManifestNode::EmptyDirectory)) {
            return Err(RestoreError::DirectoryUnsupported);
        }
        let scope = SelectedScope {
            selected: selected.clone(),
            expected_kind: expected.map(|entry| node_kind(&entry.node)),
        };
        let current = CaptureEngine::new(store.objects(), CaptureOptions::default())
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
                reason: conflict.reason,
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

#[cfg(unix)]
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
        schema: 2,
        session_id,
        path: path.clone(),
        stage_name: stage_name_text,
        backup_name: Some(backup_name_text),
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
        schema: 2,
        session_id,
        backup_name: Some(backup_name_text),
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
        ManifestNode::EmptyDirectory => return Err(RestoreError::DirectoryUnsupported),
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
        ManifestNode::EmptyDirectory => Err(RestoreError::DirectoryUnsupported),
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
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RestoreJournal {
    schema: u16,
    session_id: SessionId,
    path: NativeRelativePath,
    stage_name: String,
    #[serde(default)]
    backup_name: Option<String>,
    state: JournalState,
}

#[cfg(unix)]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct IndexRestoreJournal {
    schema: u16,
    session_id: SessionId,
    #[serde(default)]
    backup_name: Option<String>,
    state: JournalState,
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
        let state = ciborium::de::from_reader::<RestoreJournal, _>(Cursor::new(&bytes))
            .map(|journal| journal.state)
            .or_else(|_| {
                ciborium::de::from_reader::<IndexRestoreJournal, _>(Cursor::new(&bytes))
                    .map(|journal| journal.state)
            })
            .map_err(|error| RestoreError::Journal(error.to_string()))?;
        match state {
            JournalState::Complete => summary.complete = summary.complete.saturating_add(1),
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
    Ok(summary)
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
    #[error("empty-directory restoration is not enabled in the first safe mutation backend")]
    DirectoryUnsupported,
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
    #[error("restore journal encoding failed: {0}")]
    Journal(String),
    #[error("restore journal exceeds its size limit: {0}")]
    JournalTooLarge(PathBuf),
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
    Io(#[from] io::Error),
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::OsString;

    use anchor_core::{CaptureOptions, PathEncoding};

    use super::*;
    use crate::{RunRequest, SessionRunner};

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
            capture_options: CaptureOptions::default(),
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
    fn removes_an_unchanged_session_addition() {
        let root = repository();
        let (store, session) = run_change(root.path(), "printf session > added");
        let result = RestoreService::restore_file(&store, session, selected(b"added")).unwrap();
        assert!(matches!(result, RestoreApplyResult::Applied { .. }));
        assert!(!root.path().join("added").exists());
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
