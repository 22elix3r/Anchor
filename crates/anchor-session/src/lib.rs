//! Durable session lifecycle and interactive command execution.

mod maintenance;
mod restore;

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anchor_core::{
    CaptureEngine, CaptureOptions, CaptureStatistics, Manifest, ManifestError, ManifestId,
    NativeString, ObjectStore, StoreError,
};
use anchor_git::{GitContext, GitError, IndexCapture, RepositoryState};
use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;
use uuid::Uuid;

pub use maintenance::{
    DoctorReport, GarbageCollectionReport, MaintenanceError, MaintenanceService,
};
pub use restore::{IndexRestoreResult, RestoreApplyResult, RestoreError, RestoreService};

const SESSION_TAG: u64 = 0x4153_4553;
const SESSION_SCHEMA: u16 = 1;
const MAX_SESSION_BYTES: usize = 16 * 1024 * 1024;
const ENDPOINT_RETRIES: usize = 2;

/// Stable public identifier for one captured command window.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl SessionId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    pub id: SessionId,
    pub command: Vec<NativeString>,
    pub invocation_directory: NativeString,
    pub worktree_root: NativeString,
    pub worktree_key: String,
    pub before: EndpointSnapshot,
    pub after: Option<EndpointSnapshot>,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub exit: Option<ExitRecord>,
    pub state: SessionState,
    pub failure: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EndpointSnapshot {
    pub manifest: ManifestId,
    pub index: IndexCapture,
    pub repository: RepositoryState,
    pub statistics: SnapshotStatistics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotStatistics {
    pub regular_files: u64,
    pub symlinks: u64,
    pub empty_directories: u64,
    pub raw_bytes: u64,
    pub excluded_nodes: u64,
}

impl From<CaptureStatistics> for SnapshotStatistics {
    fn from(value: CaptureStatistics) -> Self {
        Self {
            regular_files: value.regular_files,
            symlinks: value.symlinks,
            empty_directories: value.empty_directories,
            raw_bytes: value.raw_bytes,
            excluded_nodes: value.excluded_nodes,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Timestamp {
    pub seconds: u64,
    pub nanoseconds: u32,
}

impl Timestamp {
    fn now() -> Result<Self, SessionError> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| SessionError::ClockBeforeEpoch)?;
        Ok(Self {
            seconds: duration.as_secs(),
            nanoseconds: duration.subsec_nanos(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExitRecord {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub success: bool,
}

impl ExitRecord {
    fn from_status(status: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt as _;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            signal,
            success: status.success(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionState {
    BeforeSnapshotComplete,
    ChildRunning,
    CapturingAfter,
    Completed,
    Interrupted,
    LaunchFailed,
    AfterSnapshotFailed,
}

impl SessionState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Interrupted | Self::LaunchFailed | Self::AfterSnapshotFailed
        )
    }
}

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub invocation_directory: PathBuf,
    pub command: Vec<OsString>,
    pub capture_options: CaptureOptions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunResult {
    pub session_id: SessionId,
    pub exit: ExitRecord,
    pub state: SessionState,
    pub after_failure: Option<String>,
}

impl RunResult {
    #[must_use]
    pub const fn process_exit_code(&self) -> i32 {
        if let Some(code) = self.exit.code {
            return code;
        }
        if let Some(signal) = self.exit.signal {
            return 128 + signal;
        }
        1
    }
}

/// Persistent store containing immutable manifests and mutable session state.
#[derive(Debug)]
pub struct SessionStore {
    root: PathBuf,
    worktree_key: String,
    objects: ObjectStore,
}

impl SessionStore {
    /// Open a store namespace for one worktree.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the private layout cannot be created.
    pub fn open(
        root: impl AsRef<Path>,
        worktree_key: impl Into<String>,
    ) -> Result<Self, SessionError> {
        let root = root.as_ref().to_path_buf();
        let worktree_key = worktree_key.into();
        let objects = ObjectStore::open(&root)?;
        private_directory(&root.join("manifests").join("b3"))?;
        private_directory(&root.join("sessions").join(&worktree_key))?;
        private_directory(&root.join("locks"))?;
        Ok(Self {
            root,
            worktree_key,
            objects,
        })
    }

    #[must_use]
    pub const fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Publish and verify an immutable manifest.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] for encoding, I/O, collision, or integrity failures.
    pub fn put_manifest(&self, manifest: &Manifest) -> Result<ManifestId, SessionError> {
        let id = manifest.id()?;
        let bytes = manifest.encode()?;
        let path = self.manifest_path(id);
        let parent = path.parent().ok_or(SessionError::InvalidLayout)?;
        private_directory(parent)?;
        if path.exists() {
            let existing = self.load_manifest(id)?;
            if existing == *manifest {
                return Ok(id);
            }
            return Err(SessionError::ManifestCollision(id));
        }
        let mut file = NamedTempFile::new_in(parent)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        match file.persist_noclobber(&path) {
            Ok(file) => {
                file.sync_all()?;
                Ok(id)
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                let existing = self.load_manifest(id)?;
                if existing == *manifest {
                    Ok(id)
                } else {
                    Err(SessionError::ManifestCollision(id))
                }
            }
            Err(error) => Err(error.error.into()),
        }
    }

    /// Load a manifest and verify its content-derived identity.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] for malformed, missing, oversized, or corrupt data.
    pub fn load_manifest(&self, id: ManifestId) -> Result<Manifest, SessionError> {
        let bytes = bounded_read(&self.manifest_path(id), 256 * 1024 * 1024)?;
        let manifest = Manifest::decode(&bytes)?;
        if manifest.id()? != id {
            return Err(SessionError::ManifestIdentityMismatch(id));
        }
        Ok(manifest)
    }

    /// Atomically persist the current state of a session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] for invalid namespaces, encoding, size, or I/O failures.
    pub fn save_session(&self, session: &Session) -> Result<(), SessionError> {
        if session.worktree_key != self.worktree_key {
            return Err(SessionError::WrongWorktreeNamespace);
        }
        let wire = SessionWire::from_session(session);
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&wire, &mut bytes)
            .map_err(|error| SessionError::Encode(error.to_string()))?;
        if bytes.len() > MAX_SESSION_BYTES {
            return Err(SessionError::SessionTooLarge);
        }
        let mut file = AtomicWriteFile::open(self.session_path(session.id))?;
        file.write_all(&bytes)?;
        file.commit()?;
        Ok(())
    }

    /// Load and validate one versioned session record.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] for malformed, missing, oversized, or unsupported data.
    pub fn load_session(&self, id: SessionId) -> Result<Session, SessionError> {
        let bytes = bounded_read(&self.session_path(id), MAX_SESSION_BYTES)?;
        let wire: SessionWire = ciborium::de::from_reader(Cursor::new(bytes))
            .map_err(|error| SessionError::Decode(error.to_string()))?;
        wire.into_session()
    }

    /// Load all sessions in this worktree namespace, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] if any retained session cannot be read safely.
    pub fn list_sessions(&self) -> Result<Vec<Session>, SessionError> {
        let directory = self.root.join("sessions").join(&self.worktree_key);
        let mut sessions = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "cbor")
            {
                let bytes = bounded_read(&entry.path(), MAX_SESSION_BYTES)?;
                let wire: SessionWire = ciborium::de::from_reader(Cursor::new(bytes))
                    .map_err(|error| SessionError::Decode(error.to_string()))?;
                sessions.push(wire.into_session()?);
            }
        }
        sessions.sort_by_key(|session| session.started_at.seconds);
        sessions.reverse();
        Ok(sessions)
    }

    fn manifest_path(&self, id: ManifestId) -> PathBuf {
        let hex = id.to_string();
        self.root
            .join("manifests")
            .join("b3")
            .join(&hex[..2])
            .join(format!("{}.cbor", &hex[2..]))
    }

    fn session_path(&self, id: SessionId) -> PathBuf {
        self.root
            .join("sessions")
            .join(&self.worktree_key)
            .join(format!("{id}.cbor"))
    }

    pub(crate) fn acquire_active_lock(&self) -> Result<ActiveLock, SessionError> {
        let path = self
            .root
            .join("locks")
            .join(format!("{}.active.lock", self.worktree_key));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        fs4::FileExt::try_lock(&file)
            .map_err(|error| SessionError::ActiveSession(error.to_string()))?;
        Ok(ActiveLock { file })
    }
}

pub(crate) struct ActiveLock {
    file: File,
}

impl Drop for ActiveLock {
    fn drop(&mut self) {
        let _result = fs4::FileExt::unlock(&self.file);
    }
}

/// Execute commands while holding one worktree session lock.
#[derive(Debug, Default)]
pub struct SessionRunner;

impl SessionRunner {
    /// Capture a command window and return its exact process outcome.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] before launch when the safe before-snapshot cannot be completed,
    /// or after launch for process-management and persistence failures. An after-capture failure
    /// is persisted and returned in [`RunResult`] so the child outcome remains available.
    #[allow(clippy::too_many_lines)]
    pub fn run(request: &RunRequest) -> Result<RunResult, SessionError> {
        if request.command.is_empty() {
            return Err(SessionError::EmptyCommand);
        }
        #[cfg(windows)]
        return Err(SessionError::PlatformCaptureUnsupported);

        #[cfg(not(windows))]
        {
            let before_context = GitContext::discover(&request.invocation_directory)?;
            reject_unsupported_repository(&before_context.repository_state()?)?;
            let location = before_context.store_location();
            let store = SessionStore::open(&location.root, location.worktree_key)?;
            let _active_lock = store.acquire_active_lock()?;

            let before = capture_live_endpoint(&before_context, &store, request.capture_options)?;
            let before_manifest = store.load_manifest(before.manifest)?;
            let frozen_scope = before_context.frozen_scope(
                &before_manifest,
                store.objects(),
                before_context.tracked_paths(),
            )?;
            let id = SessionId::new();
            let mut session = Session {
                id,
                command: request
                    .command
                    .iter()
                    .map(|value| NativeString::from_host(value))
                    .collect(),
                invocation_directory: NativeString::from_host(
                    request.invocation_directory.as_os_str(),
                ),
                worktree_root: NativeString::from_host(before_context.worktree_root().as_os_str()),
                worktree_key: store.worktree_key.clone(),
                before,
                after: None,
                started_at: Timestamp::now()?,
                finished_at: None,
                exit: None,
                state: SessionState::BeforeSnapshotComplete,
                failure: None,
            };
            store.save_session(&session)?;

            let interrupted = install_interrupt_flag()?;
            let mut command = Command::new(&request.command[0]);
            command
                .args(&request.command[1..])
                .current_dir(&request.invocation_directory)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    session.state = SessionState::LaunchFailed;
                    session.failure = Some(error.to_string());
                    session.finished_at = Some(Timestamp::now()?);
                    store.save_session(&session)?;
                    return Err(SessionError::ChildSpawn {
                        session_id: id,
                        source: error,
                    });
                }
            };
            session.state = SessionState::ChildRunning;
            store.save_session(&session)?;

            let status = loop {
                match child.wait() {
                    Ok(status) => break status,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(SessionError::ChildWait(error)),
                }
            };
            let exit = ExitRecord::from_status(status);
            session.exit = Some(exit);
            session.state = SessionState::CapturingAfter;
            store.save_session(&session)?;

            let after_result = (|| {
                let after_context = GitContext::discover(&request.invocation_directory)?;
                capture_frozen_endpoint(
                    &after_context,
                    &store,
                    request.capture_options,
                    frozen_scope,
                )
            })();
            session.finished_at = Some(Timestamp::now()?);
            match after_result {
                Ok(after) => {
                    session.after = Some(after);
                    session.state = if interrupted.load(Ordering::SeqCst) {
                        SessionState::Interrupted
                    } else {
                        SessionState::Completed
                    };
                    session.failure = None;
                }
                Err(error) => {
                    session.state = SessionState::AfterSnapshotFailed;
                    session.failure = Some(error.to_string());
                }
            }
            store.save_session(&session)?;
            Ok(RunResult {
                session_id: id,
                exit,
                state: session.state,
                after_failure: session.failure,
            })
        }
    }
}

fn reject_unsupported_repository(state: &RepositoryState) -> Result<(), SessionError> {
    if state.sparse_checkout || state.sparse_index {
        return Err(SessionError::SparseRepositoryUnsupported);
    }
    if state.split_index {
        return Err(SessionError::SplitIndexUnsupported);
    }
    Ok(())
}

fn capture_live_endpoint(
    context: &GitContext,
    store: &SessionStore,
    options: CaptureOptions,
) -> Result<EndpointSnapshot, SessionError> {
    for _ in 0..ENDPOINT_RETRIES {
        let repository_before = context.repository_state()?;
        reject_unsupported_repository(&repository_before)?;
        let index_before = context.capture_index(store.objects())?;
        let scope = context.live_scope()?;
        let capture = CaptureEngine::new(store.objects(), options)
            .capture(context.worktree_root(), &scope)?;
        let index_after = context.capture_index(store.objects())?;
        let repository_after = context.repository_state()?;
        if repository_before == repository_after && index_before == index_after {
            return Ok(EndpointSnapshot {
                manifest: store.put_manifest(&capture.manifest)?,
                index: index_after,
                repository: repository_after,
                statistics: capture.statistics.into(),
            });
        }
    }
    Err(SessionError::UnstableRepositoryEndpoint)
}

fn capture_frozen_endpoint(
    after_context: &GitContext,
    store: &SessionStore,
    options: CaptureOptions,
    mut scope: anchor_git::FrozenGitScope,
) -> Result<EndpointSnapshot, SessionError> {
    scope.include_tracked(after_context.tracked_paths().iter().cloned());
    for _ in 0..ENDPOINT_RETRIES {
        let repository_before = after_context.repository_state()?;
        reject_unsupported_repository(&repository_before)?;
        let index_before = after_context.capture_index(store.objects())?;
        let capture = CaptureEngine::new(store.objects(), options)
            .capture(after_context.worktree_root(), &scope)?;
        let index_after = after_context.capture_index(store.objects())?;
        let repository_after = after_context.repository_state()?;
        if repository_before == repository_after && index_before == index_after {
            return Ok(EndpointSnapshot {
                manifest: store.put_manifest(&capture.manifest)?,
                index: index_after,
                repository: repository_after,
                statistics: capture.statistics.into(),
            });
        }
    }
    Err(SessionError::UnstableRepositoryEndpoint)
}

fn install_interrupt_flag() -> Result<Arc<AtomicBool>, SessionError> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGQUIT,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&flag))?;
    }
    #[cfg(windows)]
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag))?;
    Ok(flag)
}

fn bounded_read(path: &Path, maximum: usize) -> Result<Vec<u8>, SessionError> {
    let metadata = fs::metadata(path)?;
    let length = usize::try_from(metadata.len()).map_err(|_| SessionError::SessionTooLarge)?;
    if length > maximum {
        return Err(SessionError::SessionTooLarge);
    }
    let bytes = fs::read(path)?;
    if bytes.len() != length {
        return Err(SessionError::UnstableMetadata(path.to_path_buf()));
    }
    Ok(bytes)
}

fn private_directory(path: &Path) -> Result<(), SessionError> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct SessionWire(
    u64,
    u16,
    SessionId,
    Vec<NativeStringWire>,
    NativeStringWire,
    NativeStringWire,
    String,
    EndpointSnapshot,
    Option<EndpointSnapshot>,
    Timestamp,
    Option<Timestamp>,
    Option<ExitRecord>,
    SessionState,
    Option<String>,
);

impl SessionWire {
    fn from_session(session: &Session) -> Self {
        Self(
            SESSION_TAG,
            SESSION_SCHEMA,
            session.id,
            session
                .command
                .iter()
                .map(NativeStringWire::from_native)
                .collect(),
            NativeStringWire::from_native(&session.invocation_directory),
            NativeStringWire::from_native(&session.worktree_root),
            session.worktree_key.clone(),
            session.before.clone(),
            session.after.clone(),
            session.started_at,
            session.finished_at,
            session.exit,
            session.state,
            session.failure.clone(),
        )
    }

    fn into_session(self) -> Result<Session, SessionError> {
        if self.0 != SESSION_TAG {
            return Err(SessionError::WrongTag);
        }
        if self.1 != SESSION_SCHEMA {
            return Err(SessionError::UnsupportedSchema(self.1));
        }
        Ok(Session {
            id: self.2,
            command: self
                .3
                .into_iter()
                .map(NativeStringWire::into_native)
                .collect::<Result<_, _>>()?,
            invocation_directory: self.4.into_native()?,
            worktree_root: self.5.into_native()?,
            worktree_key: self.6,
            before: self.7,
            after: self.8,
            started_at: self.9,
            finished_at: self.10,
            exit: self.11,
            state: self.12,
            failure: self.13,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct NativeStringWire(u8, serde_bytes::ByteBuf);

impl NativeStringWire {
    fn from_native(value: &NativeString) -> Self {
        Self(
            match value.encoding() {
                anchor_core::PathEncoding::UnixBytes => 1,
                anchor_core::PathEncoding::WindowsWtf16Le => 2,
            },
            serde_bytes::ByteBuf::from(value.bytes().to_vec()),
        )
    }

    fn into_native(self) -> Result<NativeString, SessionError> {
        let encoding = match self.0 {
            1 => anchor_core::PathEncoding::UnixBytes,
            2 => anchor_core::PathEncoding::WindowsWtf16Le,
            value => return Err(SessionError::InvalidPathEncoding(value)),
        };
        NativeString::new(encoding, self.1.into_vec()).map_err(SessionError::Path)
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("the command after `--` is empty")]
    EmptyCommand,
    #[error("another Anchor session is active in this worktree: {0}")]
    ActiveSession(String),
    #[error("session {session_id} could not launch its child: {source}")]
    ChildSpawn {
        session_id: SessionId,
        source: io::Error,
    },
    #[error("failed while waiting for the child process: {0}")]
    ChildWait(io::Error),
    #[error("repository or index changed repeatedly during endpoint capture")]
    UnstableRepositoryEndpoint,
    #[error("sparse checkout and sparse indexes are refused until scope parity is proven")]
    SparseRepositoryUnsupported,
    #[error("split indexes are refused until shared-index dependencies are captured")]
    SplitIndexUnsupported,
    #[error(
        "Windows capture is refused until reparse-point containment and ACL handling are proven"
    )]
    PlatformCaptureUnsupported,
    #[error("system clock is before the Unix epoch")]
    ClockBeforeEpoch,
    #[error("invalid storage layout")]
    InvalidLayout,
    #[error("session belongs to a different worktree namespace")]
    WrongWorktreeNamespace,
    #[error("session record exceeds its encoded size limit")]
    SessionTooLarge,
    #[error("session or manifest changed while it was read: {0}")]
    UnstableMetadata(PathBuf),
    #[error("session record has the wrong type tag")]
    WrongTag,
    #[error("session schema {0} is unsupported")]
    UnsupportedSchema(u16),
    #[error("native string has unknown encoding {0}")]
    InvalidPathEncoding(u8),
    #[error("manifest object {0} has a colliding encoding")]
    ManifestCollision(ManifestId),
    #[error("manifest object {0} failed identity verification")]
    ManifestIdentityMismatch(ManifestId),
    #[error("session encoding failed: {0}")]
    Encode(String),
    #[error("session decoding failed: {0}")]
    Decode(String),
    #[error(transparent)]
    Capture(#[from] anchor_core::CaptureError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Path(#[from] anchor_core::PathError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn records_before_and_after_around_a_command() {
        let root = repository();
        let script = root.path().join("make-change.sh");
        fs::write(&script, b"#!/bin/sh\nprintf after > changed\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let command = if cfg!(windows) {
            vec![
                OsString::from("cmd"),
                OsString::from("/C"),
                OsString::from("echo after>changed"),
            ]
        } else {
            vec![script.into_os_string()]
        };
        let result = SessionRunner::run(&RunRequest {
            invocation_directory: root.path().to_path_buf(),
            command,
            capture_options: CaptureOptions::default(),
        })
        .unwrap();
        assert_eq!(result.state, SessionState::Completed);
        assert!(result.exit.success);

        let context = GitContext::discover(root.path()).unwrap();
        let location = context.store_location();
        let store = SessionStore::open(location.root, location.worktree_key).unwrap();
        let session = store.load_session(result.session_id).unwrap();
        assert!(session.after.is_some());
        assert_ne!(
            session.before.manifest,
            session.after.as_ref().unwrap().manifest
        );
    }

    #[test]
    fn session_wire_round_trips_non_utf8_command_on_unix() {
        let root = repository();
        let context = GitContext::discover(root.path()).unwrap();
        let location = context.store_location();
        let store = SessionStore::open(location.root, location.worktree_key).unwrap();
        let endpoint = capture_live_endpoint(&context, &store, CaptureOptions::default()).unwrap();
        #[cfg(unix)]
        let argument = {
            use std::os::unix::ffi::OsStringExt as _;
            NativeString::from_host(OsString::from_vec(vec![0xff]).as_os_str())
        };
        #[cfg(not(unix))]
        let argument = NativeString::from_host(OsString::from("argument").as_os_str());
        let session = Session {
            id: SessionId::new(),
            command: vec![argument],
            invocation_directory: NativeString::from_host(root.path().as_os_str()),
            worktree_root: NativeString::from_host(root.path().as_os_str()),
            worktree_key: store.worktree_key.clone(),
            before: endpoint,
            after: None,
            started_at: Timestamp::now().unwrap(),
            finished_at: None,
            exit: None,
            state: SessionState::BeforeSnapshotComplete,
            failure: None,
        };
        store.save_session(&session).unwrap();
        assert_eq!(store.load_session(session.id).unwrap(), session);
    }
}
