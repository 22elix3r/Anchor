use std::collections::BTreeSet;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs;
use std::io;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use anchor_core::ObjectStore;
use anchor_core::{
    CaptureEngine, CaptureOptions, ConflictReason, ManifestEntry, ManifestNode, NativeRelativePath,
    NoChangeReason, ObservedKind, RestoreOutcome, RestorePlan, ScopeClassifier, ScopeDecision,
    ScopeError,
};
use anchor_git::GitContext;
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
        let _lock = store.acquire_active_lock()?;
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
    let transaction = Dir::open_ambient_dir(&transaction_path, ambient_authority())?;
    let stage_name_text = format!(".anchor-stage-{transaction_id}");
    let stage_name = OsString::from(&stage_name_text);
    let backup_name = OsStr::new("backup");
    let journal_path = transaction_path.join("journal.cbor");
    let mut journal = RestoreJournal {
        schema: 1,
        session_id,
        path: path.clone(),
        stage_name: stage_name_text,
        state: JournalState::Prepared,
    };
    save_journal(&journal_path, &journal)?;

    if let Some(desired) = desired {
        stage_node(&parent, &stage_name, desired, store.objects())?;
    }

    let had_current = expected.is_some();
    if had_current {
        if let Err(error) = rename_noreplace(&parent, &name, &transaction, backup_name) {
            cleanup_stage(&parent, &stage_name, desired);
            return Err(RestoreError::Evacuation(error));
        }
        journal.state = JournalState::Evacuated;
        save_journal(&journal_path, &journal)?;
        if !verify_node(&transaction, backup_name, expected, store.objects())? {
            let rollback = rename_noreplace(&transaction, backup_name, &parent, &name);
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
            apply_stage_mode(&parent, &stage_name, &transaction, backup_name, *bits)?;
        }
    } else if parent.symlink_metadata(&name).is_ok() {
        cleanup_stage(&parent, &stage_name, desired);
        return Err(RestoreError::CurrentChanged);
    }

    if desired.is_some() {
        if let Err(error) = rename_noreplace(&parent, &stage_name, &parent, &name) {
            let rollback = if had_current {
                rename_noreplace(&transaction, backup_name, &parent, &name)
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
        remove_node(&transaction, backup_name, expected)?;
    }
    journal.state = JournalState::Complete;
    save_journal(&journal_path, &journal)?;
    Ok(())
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
    transaction: &Dir,
    backup: &OsStr,
    execute_bits: u8,
) -> Result<(), RestoreError> {
    use cap_std::fs::{MetadataExt as _, PermissionsExt as _};
    let backup_mode = transaction.symlink_metadata(backup)?.mode();
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
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RestoreJournal {
    schema: u16,
    session_id: SessionId,
    path: NativeRelativePath,
    stage_name: String,
    state: JournalState,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum JournalState {
    Prepared,
    Evacuated,
    Installed,
    Verified,
    Complete,
    NeedsRecovery,
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
    #[error("the worktree root itself cannot be restored")]
    UnsafeRootPath,
    #[error("current path state changed before it could be safely evacuated")]
    CurrentChanged,
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
}
