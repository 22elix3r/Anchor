use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anchor_core::{Manifest, ManifestNode, ObjectId};
use anchor_git::{GitContext, IndexCapture};
use thiserror::Error;

use crate::restore::scan_transactions;
use crate::{SessionError, SessionState, SessionStore};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorReport {
    pub sessions: u64,
    pub deleted_sessions: u64,
    pub incomplete_sessions: u64,
    pub manifests_verified: u64,
    pub objects_verified: u64,
    pub transactions: u64,
    pub transactions_needing_recovery: u64,
    pub unfinished_transactions: u64,
    pub repository_drift_from_latest: bool,
    pub store_private: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GarbageCollectionReport {
    pub dry_run: bool,
    pub manifests_removed: u64,
    pub objects_removed: u64,
    pub bytes_reclaimed: u64,
}

#[derive(Debug, Default)]
pub struct MaintenanceService;

impl MaintenanceService {
    /// Verify all data reachable from retained worktree sessions.
    ///
    /// # Errors
    ///
    /// Returns [`MaintenanceError`] immediately for corrupt session, manifest, or object data.
    pub fn doctor(
        store: &SessionStore,
        context: &GitContext,
    ) -> Result<DoctorReport, MaintenanceError> {
        let _lease = store.acquire_store_read_lease()?;
        let current_sessions = store.list_sessions()?;
        let deleted_sessions = store.list_deleted_sessions()?;
        let sessions = store.list_all_retained_sessions()?;
        let transactions = scan_transactions(store.root())?;
        let mut manifests = BTreeSet::new();
        let mut objects = BTreeSet::new();
        let mut incomplete = 0_u64;
        for session in &sessions {
            if !session.state.is_terminal()
                || matches!(
                    session.state,
                    SessionState::AfterSnapshotFailed | SessionState::LaunchFailed
                )
            {
                incomplete += 1;
            }
            manifests.insert(session.before.manifest);
            collect_index_object(&session.before.index, &mut objects);
            if let Some(after) = &session.after {
                manifests.insert(after.manifest);
                collect_index_object(&after.index, &mut objects);
            }
        }
        for id in &manifests {
            let manifest = store.load_manifest(*id)?;
            collect_manifest_objects(&manifest, &mut objects);
        }
        for (id, raw_size) in &objects {
            store.objects().verify(*id, *raw_size)?;
        }
        let repository_drift_from_latest = current_sessions.first().is_some_and(|session| {
            session.after.as_ref().is_some_and(|after| {
                context.repository_state().ok().as_ref() != Some(&after.repository)
            })
        });
        Ok(DoctorReport {
            sessions: u64::try_from(sessions.len()).unwrap_or(u64::MAX),
            deleted_sessions: u64::try_from(deleted_sessions.len()).unwrap_or(u64::MAX),
            incomplete_sessions: incomplete,
            manifests_verified: u64::try_from(manifests.len()).unwrap_or(u64::MAX),
            objects_verified: u64::try_from(objects.len()).unwrap_or(u64::MAX),
            transactions: transactions.total,
            transactions_needing_recovery: transactions.needs_recovery,
            unfinished_transactions: transactions.unfinished,
            repository_drift_from_latest,
            store_private: store_is_private(store.root())?,
        })
    }

    /// Sweep immutable manifests and objects unreachable from retained sessions.
    ///
    /// # Errors
    ///
    /// Returns [`MaintenanceError`] without deleting anything when retained metadata is corrupt.
    /// An exclusive common-store lease excludes snapshot publication, readers, and restoration in
    /// every linked worktree sharing this store.
    pub fn gc(
        store: &SessionStore,
        dry_run: bool,
    ) -> Result<GarbageCollectionReport, MaintenanceError> {
        let _lease = store.acquire_store_write_lease()?;
        let transactions = scan_transactions(store.root())?;
        if transactions.total != transactions.complete {
            return Err(MaintenanceError::UnresolvedTransactions {
                needs_recovery: transactions.needs_recovery,
                unfinished: transactions.unfinished,
            });
        }
        let sessions = store.list_all_retained_sessions()?;
        let mut reachable_manifests = BTreeSet::new();
        let mut reachable_objects = BTreeSet::new();
        for session in &sessions {
            reachable_manifests.insert(session.before.manifest);
            collect_index_object(&session.before.index, &mut reachable_objects);
            if let Some(after) = &session.after {
                reachable_manifests.insert(after.manifest);
                collect_index_object(&after.index, &mut reachable_objects);
            }
        }
        for id in &reachable_manifests {
            let manifest = store.load_manifest(*id)?;
            collect_manifest_objects(&manifest, &mut reachable_objects);
        }
        for (id, raw_size) in &reachable_objects {
            store.objects().verify(*id, *raw_size)?;
        }

        let mut report = GarbageCollectionReport {
            dry_run,
            manifests_removed: 0,
            objects_removed: 0,
            bytes_reclaimed: 0,
        };
        sweep_files(
            &store.root().join("manifests").join("b3"),
            "cbor",
            |path| {
                !reachable_manifests
                    .iter()
                    .any(|id| store.manifest_path(*id) == path)
            },
            dry_run,
            &mut report.manifests_removed,
            &mut report.bytes_reclaimed,
        )?;
        sweep_files(
            &store.root().join("objects").join("b3"),
            "zst",
            |path| {
                !reachable_objects
                    .iter()
                    .any(|(id, _)| store.objects().object_path(*id) == path)
            },
            dry_run,
            &mut report.objects_removed,
            &mut report.bytes_reclaimed,
        )?;
        Ok(report)
    }
}

fn collect_index_object(index: &IndexCapture, objects: &mut BTreeSet<(ObjectId, u64)>) {
    if let IndexCapture::Present {
        object, raw_size, ..
    } = index
    {
        objects.insert((*object, *raw_size));
    }
}

fn collect_manifest_objects(manifest: &Manifest, objects: &mut BTreeSet<(ObjectId, u64)>) {
    for entry in manifest.entries() {
        if let ManifestNode::Regular {
            object, raw_size, ..
        } = entry.node
        {
            objects.insert((object, raw_size));
        }
    }
}

fn sweep_files(
    root: &Path,
    extension: &str,
    unreachable: impl Fn(&Path) -> bool,
    dry_run: bool,
    count: &mut u64,
    bytes: &mut u64,
) -> Result<(), MaintenanceError> {
    if !root.exists() {
        return Ok(());
    }
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == extension)
                && unreachable(&entry.path())
            {
                let size = entry.metadata()?.len();
                *count = count.saturating_add(1);
                *bytes = bytes.saturating_add(size);
                if !dry_run {
                    fs::remove_file(entry.path())?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
#[allow(clippy::verbose_bit_mask)]
fn store_is_private(root: &Path) -> Result<bool, MaintenanceError> {
    use std::os::unix::fs::PermissionsExt as _;
    Ok(fs::metadata(root)?.permissions().mode() & 0o077 == 0)
}

#[cfg(not(unix))]
fn store_is_private(_root: &Path) -> Result<bool, MaintenanceError> {
    Ok(false)
}

#[derive(Debug, Error)]
pub enum MaintenanceError {
    #[error(
        "garbage collection is refused while restore transactions are unresolved ({needs_recovery} need recovery, {unfinished} unfinished)"
    )]
    UnresolvedTransactions {
        needs_recovery: u64,
        unfinished: u64,
    },
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Restore(#[from] crate::RestoreError),
    #[error(transparent)]
    Store(#[from] anchor_core::StoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    #[cfg(not(windows))]
    use std::ffi::OsString;

    use anchor_core::{
        Completeness, Coverage, Manifest, ManifestEntry, ManifestNode, NativeRelativePath,
        PathEncoding, SafetyObservations,
    };

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

    #[test]
    #[cfg(not(windows))]
    fn doctor_verifies_reachable_data_and_gc_keeps_it() {
        let root = repository();
        fs::write(root.path().join("file"), b"before").unwrap();
        SessionRunner::run(&RunRequest {
            invocation_directory: root.path().to_path_buf(),
            command: change_command(),
            capture_policy: CapturePolicy::default(),
        })
        .unwrap();
        let context = GitContext::discover(root.path()).unwrap();
        let location = context.store_location();
        let store = SessionStore::open(location.root, location.worktree_key).unwrap();
        let doctor = MaintenanceService::doctor(&store, &context).unwrap();
        assert_eq!(doctor.sessions, 1);
        assert!(doctor.objects_verified >= 2);
        #[cfg(unix)]
        assert!(doctor.store_private);
        let gc = MaintenanceService::gc(&store, false).unwrap();
        assert_eq!(gc.objects_removed, 0);
        assert_eq!(gc.manifests_removed, 0);
    }

    #[test]
    fn gc_reclaims_an_unreferenced_object() {
        let root = repository();
        let context = GitContext::discover(root.path()).unwrap();
        let location = context.store_location();
        let store = SessionStore::open(location.root, location.worktree_key).unwrap();
        let object = store.objects().put_bytes(b"orphan").unwrap();
        assert!(store.objects().object_path(object).exists());
        let preview = MaintenanceService::gc(&store, true).unwrap();
        assert_eq!(preview.objects_removed, 1);
        assert!(store.objects().object_path(object).exists());
        let swept = MaintenanceService::gc(&store, false).unwrap();
        assert_eq!(swept.objects_removed, 1);
        assert!(!store.objects().object_path(object).exists());
    }

    #[test]
    #[cfg(not(windows))]
    fn gc_marks_sessions_from_every_linked_worktree_namespace() {
        let root = repository();
        fs::write(root.path().join("file"), b"before").unwrap();
        let result = SessionRunner::run(&RunRequest {
            invocation_directory: root.path().to_path_buf(),
            command: change_command(),
            capture_policy: CapturePolicy::default(),
        })
        .unwrap();
        let context = GitContext::discover(root.path()).unwrap();
        let location = context.store_location();
        let store = SessionStore::open(&location.root, &location.worktree_key).unwrap();
        let other = SessionStore::open(&location.root, "wt-other").unwrap();
        let object = other.objects().put_bytes(b"only-other-worktree").unwrap();
        let manifest = Manifest::new(
            PathEncoding::UnixBytes,
            vec![ManifestEntry {
                path: NativeRelativePath::new(PathEncoding::UnixBytes, vec![b"other".to_vec()])
                    .unwrap(),
                node: ManifestNode::Regular {
                    object,
                    raw_size: 19,
                    unix_exec_bits: Some(0),
                    windows_readonly: None,
                },
                safety: SafetyObservations::default(),
            }],
            Coverage {
                completeness: Completeness::Complete,
                omissions: Vec::new(),
            },
        )
        .unwrap();
        let manifest_id = other.put_manifest(&manifest).unwrap();
        let mut session = store.load_session(result.session_id).unwrap();
        session.id = crate::SessionId::new();
        session.worktree_key = "wt-other".to_owned();
        session.before.manifest = manifest_id;
        session.after = None;
        session.state = SessionState::BeforeSnapshotComplete;
        other.save_session(&session).unwrap();

        let report = MaintenanceService::gc(&store, false).unwrap();
        assert!(other.objects().object_path(object).exists());
        assert!(other.load_manifest(manifest_id).is_ok());
        assert_eq!(report.objects_removed, 0);
    }

    #[cfg(not(windows))]
    fn change_command() -> Vec<OsString> {
        vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("printf after > file"),
        ]
    }
}
