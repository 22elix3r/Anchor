//! Read-only Git awareness for Fence.
//!
//! This crate is published as an implementation layer for the `fence` CLI. Its
//! Rust API is prerelease and may change between `0.1.0-alpha.N` versions.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
use fence_core::NativeString;
use fence_core::{
    NativeRelativePath, ObjectId, ObjectStore, ObservedKind, OmissionReason, ScopeClassifier,
    ScopeDecision, ScopeError, StoreError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod policy;

pub use policy::{
    ExternalIgnoreOrigin, ExternalIgnorePolicy, FrozenGitPolicy, FrozenIgnoreFile,
    FrozenIgnoreSource, PolicyBlob, PolicyDrift,
};

const MAX_INDEX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_IGNORE_BYTES: u64 = 16 * 1024 * 1024;
const INDEX_READ_RETRIES: usize = 3;

/// A discovered non-bare Git worktree and its read-only repository handle.
pub struct GitContext {
    repository: gix::Repository,
    worktree_root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    index_path: PathBuf,
    store_location: StoreLocation,
    tracked: BTreeSet<NativeRelativePath>,
    submodules: BTreeSet<NativeRelativePath>,
}

impl std::fmt::Debug for GitContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GitContext")
            .field("worktree_root", &self.worktree_root)
            .field("git_dir", &self.git_dir)
            .field("common_dir", &self.common_dir)
            .field("index_path", &self.index_path)
            .field("tracked_paths", &self.tracked.len())
            .field("submodules", &self.submodules.len())
            .finish_non_exhaustive()
    }
}

impl GitContext {
    /// Discover the effective repository from an invocation directory.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when discovery fails, the repository is bare, or index paths cannot
    /// be represented safely on the host.
    pub fn discover(invocation_directory: &Path) -> Result<Self, GitError> {
        let repository = gix::discover(invocation_directory)
            .map_err(|error| GitError::Discover(error.to_string()))?;
        if repository.is_bare() {
            return Err(GitError::BareRepository);
        }
        let worktree_root = repository
            .workdir()
            .ok_or(GitError::MissingWorktree)?
            .to_path_buf();
        let git_dir = repository.git_dir().to_path_buf();
        let common_dir = repository.common_dir().to_path_buf();
        let index_path = repository.index_path();
        let store_location = default_store_location(&git_dir, &common_dir)?;

        let index = repository
            .index_or_empty()
            .map_err(|error| GitError::Index(error.to_string()))?;
        let mut tracked = BTreeSet::new();
        let mut submodules = BTreeSet::new();
        for entry in index.entries() {
            let path = git_index_path(entry.path(&index))?;
            tracked.insert(path.clone());
            if entry.mode == gix::index::entry::Mode::COMMIT {
                submodules.insert(path);
            }
        }

        Ok(Self {
            repository,
            worktree_root,
            git_dir,
            common_dir,
            index_path,
            store_location,
            tracked,
            submodules,
        })
    }

    #[must_use]
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    #[must_use]
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    #[must_use]
    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    #[must_use]
    pub fn tracked_paths(&self) -> &BTreeSet<NativeRelativePath> {
        &self.tracked
    }

    /// Construct the default per-user shared store location and per-worktree namespace.
    #[must_use]
    pub fn store_location(&self) -> StoreLocation {
        self.store_location.clone()
    }

    /// Return whether an untrusted manifest path could address Git metadata, Fence's store,
    /// or a frozen repository boundary.
    ///
    /// This is a mutation guard, not an inclusion rule. It intentionally rejects every
    /// component spelling of `.git` under ASCII case folding, even on a case-sensitive
    /// filesystem.
    #[must_use]
    pub fn is_protected_mutation_path(
        &self,
        policy: &FrozenGitPolicy,
        path: &NativeRelativePath,
    ) -> bool {
        let dot_git = NativeRelativePath::from_host_path(Path::new(".git")).ok();
        if dot_git.as_ref().is_some_and(|dot_git| {
            let expected = &dot_git.components()[0];
            path.components()
                .iter()
                .any(|component| component_ascii_eq(component, expected))
        }) {
            return true;
        }
        let fold_case = policy.ignore_case || cfg!(windows);
        let frozen_boundaries = policy
            .submodules
            .iter()
            .chain(&policy.nested_repositories)
            .chain(policy.store_relative.iter());
        if frozen_boundaries
            .into_iter()
            .any(|boundary| is_below_with_case(path, boundary, fold_case))
        {
            return true;
        }
        [
            self.git_dir.as_path(),
            self.common_dir.as_path(),
            self.store_location.root.as_path(),
        ]
        .into_iter()
        .filter_map(|protected| protected.strip_prefix(&self.worktree_root).ok())
        .filter_map(|protected| NativeRelativePath::from_host_path(protected).ok())
        .any(|protected| is_below_with_case(path, &protected, fold_case))
    }

    /// Build a live Git-compatible inclusion classifier.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] if the ignore stack cannot be assembled.
    pub fn live_scope(&self) -> Result<GitScope<'_>, GitError> {
        let index = self
            .repository
            .index_or_empty()
            .map_err(|error| GitError::Index(error.to_string()))?;
        let excludes = self
            .repository
            .excludes(
                &index,
                None,
                gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
            )
            .map_err(|error| GitError::Ignore(error.to_string()))?;
        let mut fence_ignore = gix::ignore::Search::default();
        add_in_tree_ignore(
            &mut fence_ignore,
            &self.worktree_root.join(".fenceignore"),
            &self.worktree_root,
        )?;
        let store_relative = self
            .store_location()
            .root
            .strip_prefix(&self.worktree_root)
            .ok()
            .and_then(|path| NativeRelativePath::from_host_path(path).ok());
        Ok(GitScope {
            excludes: RefCell::new(excludes),
            fence_ignore,
            case: self.ignore_case(),
            tracked: &self.tracked,
            submodules: &self.submodules,
            worktree_root: &self.worktree_root,
            store_relative,
        })
    }

    fn ignore_case(&self) -> gix::glob::pattern::Case {
        if self
            .repository
            .config_snapshot()
            .boolean("core.ignoreCase")
            .unwrap_or(false)
        {
            gix::glob::pattern::Case::Fold
        } else {
            gix::glob::pattern::Case::Sensitive
        }
    }

    /// Capture endpoint HEAD, operation, object-format, and sparse/index mode.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when HEAD cannot be read consistently.
    pub fn repository_state(&self) -> Result<RepositoryState, GitError> {
        let head = self
            .repository
            .head()
            .map_err(|error| GitError::Head(error.to_string()))?;
        let referent = head.referent_name().map(|name| name.as_bstr().to_vec());
        let target = head.id().map(|id| id.to_string());
        let head = if head.is_unborn() {
            HeadState::Unborn {
                referent: referent.ok_or(GitError::InvalidHead)?,
            }
        } else if head.is_detached() {
            HeadState::Detached {
                target: target.ok_or(GitError::InvalidHead)?,
            }
        } else {
            HeadState::Attached {
                referent: referent.ok_or(GitError::InvalidHead)?,
                target: target.ok_or(GitError::InvalidHead)?,
            }
        };
        let repository_state = self.repository.state();
        let operation = repository_state
            .as_ref()
            .map_or(OperationState::None, operation_state);
        let config = self.repository.config_snapshot();
        let index = self
            .repository
            .index_or_empty()
            .map_err(|error| GitError::Index(error.to_string()))?;
        Ok(RepositoryState {
            head,
            operation,
            object_hash: format!("{:?}", self.repository.object_hash()),
            ignore_case: config.boolean("core.ignoreCase").unwrap_or(false),
            sparse_checkout: config.boolean("core.sparseCheckout").unwrap_or(false),
            sparse_index: index
                .entries()
                .iter()
                .any(|entry| entry.mode == gix::index::entry::Mode::DIR),
            split_index: index.link().is_some(),
        })
    }

    /// Capture exact raw index bytes into Fence's object store.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] for an active Git index lock, unstable bytes, parse failure, size
    /// violation, or object-store failure.
    pub fn capture_index(&self, store: &ObjectStore) -> Result<IndexCapture, GitError> {
        let lock_path = self.index_path.with_extension("lock");
        if lock_path.exists() {
            return Err(GitError::IndexLocked(lock_path));
        }
        if !self.index_path.exists() {
            return Ok(IndexCapture::Absent);
        }

        for _ in 0..INDEX_READ_RETRIES {
            let before = fs::metadata(&self.index_path)?;
            if before.len() > MAX_INDEX_BYTES {
                return Err(GitError::IndexTooLarge(before.len()));
            }
            let bytes = fs::read(&self.index_path)?;
            let after = fs::metadata(&self.index_path)?;
            if lock_path.exists()
                || before.len() != after.len()
                || before.modified().ok() != after.modified().ok()
                || u64::try_from(bytes.len()).ok() != Some(before.len())
            {
                continue;
            }

            let parsed = self
                .repository
                .open_index()
                .map_err(|error| GitError::Index(error.to_string()))?;
            let check = fs::read(&self.index_path)?;
            if check != bytes || lock_path.exists() {
                continue;
            }
            let mut parsed_tracked = BTreeSet::new();
            let mut parsed_submodules = BTreeSet::new();
            for entry in parsed.entries() {
                let path = git_index_path(entry.path(&parsed))?;
                parsed_tracked.insert(path.clone());
                if entry.mode == gix::index::entry::Mode::COMMIT {
                    parsed_submodules.insert(path);
                }
            }
            if parsed_tracked != self.tracked || parsed_submodules != self.submodules {
                return Err(GitError::StaleIndexContext);
            }
            let object = store.put_bytes(&bytes)?;
            let summary = IndexSummary {
                version: parsed.version() as u32,
                entries: u64::try_from(parsed.entries().len())
                    .map_err(|_| GitError::IndexTooLarge(before.len()))?,
                conflicts: parsed.entries().iter().any(|entry| entry.stage_raw() != 0),
                intent_to_add: parsed.entries().iter().any(|entry| {
                    entry
                        .flags
                        .contains(gix::index::entry::Flags::INTENT_TO_ADD)
                }),
                skip_worktree: parsed.entries().iter().any(|entry| {
                    entry
                        .flags
                        .contains(gix::index::entry::Flags::SKIP_WORKTREE)
                }),
                sparse_index: parsed
                    .entries()
                    .iter()
                    .any(|entry| entry.mode == gix::index::entry::Mode::DIR),
                split_index: parsed.link().is_some(),
                checksum_present: parsed.checksum().is_some(),
            };
            return Ok(IndexCapture::Present {
                object,
                raw_size: before.len(),
                summary,
            });
        }
        Err(GitError::UnstableIndex(self.index_path.clone()))
    }
}

/// Live classifier used for the before capture.
pub struct GitScope<'repository> {
    excludes: RefCell<gix::AttributeStack<'repository>>,
    fence_ignore: gix::ignore::Search,
    case: gix::glob::pattern::Case,
    tracked: &'repository BTreeSet<NativeRelativePath>,
    submodules: &'repository BTreeSet<NativeRelativePath>,
    worktree_root: &'repository Path,
    store_relative: Option<NativeRelativePath>,
}

impl ScopeClassifier for GitScope<'_> {
    fn classify(
        &self,
        path: &NativeRelativePath,
        kind: ObservedKind,
    ) -> Result<ScopeDecision, ScopeError> {
        if is_root_dot_git(path)
            || self
                .store_relative
                .as_ref()
                .is_some_and(|root| is_below(path, root))
        {
            return Ok(ScopeDecision::Exclude);
        }
        if is_root_fence_ignore(path) {
            return Ok(ScopeDecision::Include);
        }
        if self.submodules.contains(path) {
            return Ok(ScopeDecision::Exclude);
        }
        if self.tracked.contains(path)
            || (kind == ObservedKind::Directory
                && self
                    .tracked
                    .iter()
                    .any(|tracked| is_strict_descendant(tracked, path)))
        {
            return Ok(ScopeDecision::Include);
        }
        if kind == ObservedKind::Directory {
            let host = path.to_host_path().map_err(scope_error)?;
            let nested_marker = self.worktree_root.join(host).join(".git");
            if nested_marker.exists() {
                return Ok(ScopeDecision::Boundary(OmissionReason::NestedRepository));
            }
        }

        let host = path.to_host_path().map_err(scope_error)?;
        let mode = match kind {
            ObservedKind::Directory => Some(gix::index::entry::Mode::DIR),
            ObservedKind::Symlink => Some(gix::index::entry::Mode::SYMLINK),
            ObservedKind::Regular | ObservedKind::Unsupported => None,
        };
        let excluded = self
            .excludes
            .borrow_mut()
            .at_path(&host, mode)
            .map_err(scope_error)?
            .is_excluded();
        if excluded {
            return Ok(ScopeDecision::Exclude);
        }
        let relative = gix::path::try_into_bstr(host).map_err(scope_error)?;
        let fence_excluded = self
            .fence_ignore
            .pattern_matching_relative_path(
                relative.as_ref(),
                Some(kind == ObservedKind::Directory),
                self.case,
            )
            .is_some_and(|matched| !matched.pattern.is_negative());
        Ok(if fence_excluded {
            ScopeDecision::Exclude
        } else {
            ScopeDecision::Include
        })
    }
}

/// Immutable Git-compatible policy used for the after and restore-time captures.
#[derive(Clone, Debug)]
pub struct FrozenGitScope {
    git_search: gix::ignore::Search,
    fence_search: gix::ignore::Search,
    case: gix::glob::pattern::Case,
    tracked: BTreeSet<NativeRelativePath>,
    submodules: BTreeSet<NativeRelativePath>,
    nested_repositories: BTreeSet<NativeRelativePath>,
    store_relative: Option<NativeRelativePath>,
}

impl ScopeClassifier for FrozenGitScope {
    fn classify(
        &self,
        path: &NativeRelativePath,
        kind: ObservedKind,
    ) -> Result<ScopeDecision, ScopeError> {
        if is_root_dot_git(path)
            || self
                .store_relative
                .as_ref()
                .is_some_and(|root| is_below(path, root))
        {
            return Ok(ScopeDecision::Exclude);
        }
        if is_root_fence_ignore(path) {
            return Ok(ScopeDecision::Include);
        }
        if self.submodules.contains(path) {
            return Ok(ScopeDecision::Exclude);
        }
        if self.tracked.contains(path)
            || (kind == ObservedKind::Directory
                && self
                    .tracked
                    .iter()
                    .any(|tracked| is_strict_descendant(tracked, path)))
        {
            return Ok(ScopeDecision::Include);
        }
        if kind == ObservedKind::Directory && self.nested_repositories.contains(path) {
            return Ok(ScopeDecision::Boundary(OmissionReason::NestedRepository));
        }

        let host = path.to_host_path().map_err(scope_error)?;
        let relative = gix::path::try_into_bstr(host).map_err(scope_error)?;
        let is_directory = Some(kind == ObservedKind::Directory);
        let git_excluded = self
            .git_search
            .pattern_matching_relative_path(relative.as_ref(), is_directory, self.case)
            .is_some_and(|matched| !matched.pattern.is_negative());
        if git_excluded {
            return Ok(ScopeDecision::Exclude);
        }
        let fence_excluded = self
            .fence_search
            .pattern_matching_relative_path(relative.as_ref(), is_directory, self.case)
            .is_some_and(|matched| !matched.pattern.is_negative());
        Ok(if fence_excluded {
            ScopeDecision::Exclude
        } else {
            ScopeDecision::Include
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreLocation {
    pub root: PathBuf,
    pub trusted_parent: PathBuf,
    pub relative_root: PathBuf,
    pub legacy_root: PathBuf,
    pub worktree_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct RepositoryState {
    pub head: HeadState,
    pub operation: OperationState,
    pub object_hash: String,
    pub ignore_case: bool,
    pub sparse_checkout: bool,
    pub sparse_index: bool,
    pub split_index: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HeadState {
    Unborn { referent: Vec<u8> },
    Attached { referent: Vec<u8>, target: String },
    Detached { target: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OperationState {
    None,
    ApplyMailbox,
    ApplyMailboxRebase,
    Bisect,
    CherryPick,
    CherryPickSequence,
    Merge,
    Rebase,
    RebaseInteractive,
    Revert,
    RevertSequence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IndexCapture {
    Absent,
    Present {
        object: ObjectId,
        raw_size: u64,
        summary: IndexSummary,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct IndexSummary {
    pub version: u32,
    pub entries: u64,
    pub conflicts: bool,
    pub intent_to_add: bool,
    pub skip_worktree: bool,
    pub sparse_index: bool,
    pub split_index: bool,
    pub checksum_present: bool,
}

fn add_in_tree_ignore(
    search: &mut gix::ignore::Search,
    path: &Path,
    root: &Path,
) -> Result<(), GitError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() {
        return Err(GitError::UnsupportedIgnoreSource(path.to_path_buf()));
    }
    let bytes = read_stable_bounded(path, MAX_IGNORE_BYTES)?;
    search.add_patterns_buffer(
        &bytes,
        path.to_path_buf(),
        Some(root),
        gix::ignore::search::Ignore::default(),
    );
    Ok(())
}

fn read_stable_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, GitError> {
    for _ in 0..INDEX_READ_RETRIES {
        let before = fs::metadata(path)?;
        if before.len() > maximum {
            return Err(GitError::IgnoreSourceTooLarge {
                path: path.to_path_buf(),
                size: before.len(),
                maximum,
            });
        }
        let bytes = fs::read(path)?;
        let after = fs::metadata(path)?;
        if before.len() == after.len()
            && before.modified().ok() == after.modified().ok()
            && u64::try_from(bytes.len()).ok() == Some(before.len())
        {
            return Ok(bytes);
        }
    }
    Err(GitError::UnstableIgnoreSource(path.to_path_buf()))
}

fn git_index_path(path: &gix::bstr::BStr) -> Result<NativeRelativePath, GitError> {
    let native =
        gix::path::try_from_bstr(path).map_err(|_| GitError::InvalidIndexPath(path.to_vec()))?;
    NativeRelativePath::from_host_path(native.as_ref())
        .map_err(|error| GitError::UnsafeIndexPath(error.to_string()))
}

fn operation_state(state: &gix::state::InProgress) -> OperationState {
    match state {
        gix::state::InProgress::ApplyMailbox => OperationState::ApplyMailbox,
        gix::state::InProgress::ApplyMailboxRebase => OperationState::ApplyMailboxRebase,
        gix::state::InProgress::Bisect => OperationState::Bisect,
        gix::state::InProgress::CherryPick => OperationState::CherryPick,
        gix::state::InProgress::CherryPickSequence => OperationState::CherryPickSequence,
        gix::state::InProgress::Merge => OperationState::Merge,
        gix::state::InProgress::Rebase => OperationState::Rebase,
        gix::state::InProgress::RebaseInteractive => OperationState::RebaseInteractive,
        gix::state::InProgress::Revert => OperationState::Revert,
        gix::state::InProgress::RevertSequence => OperationState::RevertSequence,
    }
}

fn is_root_dot_git(path: &NativeRelativePath) -> bool {
    let expected = NativeRelativePath::from_host_path(Path::new(".git"));
    expected.is_ok_and(|expected| expected == *path)
}

fn is_root_fence_ignore(path: &NativeRelativePath) -> bool {
    let expected = NativeRelativePath::from_host_path(Path::new(".fenceignore"));
    expected.is_ok_and(|expected| expected == *path)
}

fn is_below(path: &NativeRelativePath, parent: &NativeRelativePath) -> bool {
    path == parent || is_strict_descendant(path, parent)
}

fn is_below_with_case(
    path: &NativeRelativePath,
    parent: &NativeRelativePath,
    fold_ascii_case: bool,
) -> bool {
    if path.components().len() < parent.components().len() {
        return false;
    }
    path.components()
        .iter()
        .zip(parent.components())
        .all(|(left, right)| {
            left == right
                || (fold_ascii_case
                    && left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| left.eq_ignore_ascii_case(right)))
        })
}

fn component_ascii_eq(component: &[u8], expected: &[u8]) -> bool {
    component.len() == expected.len()
        && component
            .iter()
            .zip(expected)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn is_strict_descendant(path: &NativeRelativePath, parent: &NativeRelativePath) -> bool {
    path.components().len() > parent.components().len()
        && path.components().starts_with(parent.components())
}

fn scope_error(error: impl std::fmt::Display) -> ScopeError {
    ScopeError {
        message: error.to_string(),
    }
}

#[cfg(unix)]
fn principal_key() -> String {
    format!("u{}", rustix::process::geteuid().as_raw())
}

#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)]
fn default_store_location(git_dir: &Path, common_dir: &Path) -> Result<StoreLocation, GitError> {
    let relative_root = PathBuf::from("fence")
        .join("users")
        .join(principal_key())
        .join("v1");
    let root = common_dir.join(&relative_root);
    let legacy_root = common_dir
        .join("anchor")
        .join("users")
        .join(principal_key())
        .join("v1");
    let worktree_key = if git_dir == common_dir {
        "main".to_owned()
    } else {
        let native = NativeString::from_host(git_dir.as_os_str());
        format!("wt-{}", short_hash(native.bytes()))
    };
    Ok(StoreLocation {
        root,
        trusted_parent: common_dir.to_path_buf(),
        relative_root,
        legacy_root,
        worktree_key,
    })
}

#[cfg(windows)]
fn default_store_location(git_dir: &Path, common_dir: &Path) -> Result<StoreLocation, GitError> {
    let common_identity = fence_windows::RootHandle::open(common_dir)?
        .directory()
        .metadata()
        .identity;
    let mut repository_identity = common_identity.volume_serial.to_le_bytes().to_vec();
    repository_identity.extend_from_slice(&common_identity.file_id);
    let trusted_parent = fence_windows::local_app_data()?;
    let relative_root = PathBuf::from("Fence")
        .join("stores")
        .join("v1")
        .join(format!("repo-{}", short_hash(&repository_identity)));
    let root = trusted_parent.join(&relative_root);
    let legacy_root = trusted_parent
        .join("Anchor")
        .join("stores")
        .join("v1")
        .join(format!("repo-{}", short_hash(&repository_identity)));
    let worktree_key = if git_dir == common_dir {
        "main".to_owned()
    } else {
        let identity = fence_windows::RootHandle::open(git_dir)?
            .directory()
            .metadata()
            .identity;
        let mut bytes = identity.volume_serial.to_le_bytes().to_vec();
        bytes.extend_from_slice(&identity.file_id);
        format!("wt-{}", short_hash(&bytes))
    };
    Ok(StoreLocation {
        root,
        trusted_parent,
        relative_root,
        legacy_root,
        worktree_key,
    })
}

fn short_hash(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    hash.as_bytes()[..12]
        .iter()
        .fold(String::with_capacity(24), |mut output, byte| {
            use std::fmt::Write as _;
            write!(output, "{byte:02x}").expect("writing to String cannot fail");
            output
        })
}

#[derive(Debug, Error)]
pub enum GitError {
    #[error("Git repository discovery failed: {0}")]
    Discover(String),
    #[error("bare repositories do not have a capturable worktree")]
    BareRepository,
    #[error("repository does not expose a worktree")]
    MissingWorktree,
    #[error("Git index error: {0}")]
    Index(String),
    #[error("Git ignore policy error: {0}")]
    Ignore(String),
    #[error("frozen Git policy schema {0} is not supported")]
    UnsupportedPolicySchema(u16),
    #[error("invalid frozen Git policy: {0}")]
    InvalidFrozenPolicy(String),
    #[error("ignore source {path} has {size} bytes, above limit {maximum}")]
    IgnoreSourceTooLarge {
        path: PathBuf,
        size: u64,
        maximum: u64,
    },
    #[error("ignore source remained unstable at {0}")]
    UnstableIgnoreSource(PathBuf),
    #[error("ignore source is not a readable regular file: {0}")]
    UnsupportedIgnoreSource(PathBuf),
    #[error("inclusion-policy discovery exceeded its {0}-entry safety limit")]
    PolicyScanLimitExceeded(usize),
    #[error("inclusion-policy namespace remained unstable at {0:?}")]
    UnstablePolicyNamespace(NativeRelativePath),
    #[error("Git HEAD error: {0}")]
    Head(String),
    #[error("Git HEAD state is internally inconsistent")]
    InvalidHead,
    #[error("Git index path cannot be represented on this host: {0:?}")]
    InvalidIndexPath(Vec<u8>),
    #[error("unsafe Git index path: {0}")]
    UnsafeIndexPath(String),
    #[error("Git index lock exists at {0}")]
    IndexLocked(PathBuf),
    #[error("Git index has {0} bytes, above the capture limit")]
    IndexTooLarge(u64),
    #[error("Git index remained unstable at {0}")]
    UnstableIndex(PathBuf),
    #[error("Git index paths changed after repository discovery; retry the operation")]
    StaleIndexContext,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[cfg(windows)]
    #[error(transparent)]
    Windows(#[from] fence_windows::WindowsError),
}

#[cfg(test)]
mod tests {
    use super::*;
    fn unborn_repository() -> tempfile::TempDir {
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
    fn discovers_unborn_repository_without_git_executable() {
        let root = unborn_repository();
        let context = GitContext::discover(root.path()).unwrap();
        assert_eq!(context.worktree_root(), root.path());
        assert!(matches!(
            context.repository_state().unwrap().head,
            HeadState::Unborn { .. }
        ));
        assert!(matches!(
            context
                .capture_index(&ObjectStore::open(root.path().join("store")).unwrap())
                .unwrap(),
            IndexCapture::Absent
        ));
    }

    #[test]
    fn scope_always_excludes_root_git_metadata() {
        let root = unborn_repository();
        let context = GitContext::discover(root.path()).unwrap();
        let scope = context.live_scope().unwrap();
        let dot_git = NativeRelativePath::from_host_path(Path::new(".git")).unwrap();
        assert_eq!(
            scope.classify(&dot_git, ObservedKind::Directory).unwrap(),
            ScopeDecision::Exclude
        );
    }

    #[test]
    fn mutation_guard_rejects_git_metadata_and_frozen_boundaries() {
        let root = unborn_repository();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::create_dir(root.path().join("nested").join(".git")).unwrap();
        let context = GitContext::discover(root.path()).unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(store_root.path()).unwrap();
        let policy = context.capture_frozen_policy(&store).unwrap();

        for path in [".git/config", ".GIT/config", "nested/file"] {
            let path = NativeRelativePath::from_host_path(Path::new(path)).unwrap();
            assert!(
                context.is_protected_mutation_path(&policy, &path),
                "{path:?}"
            );
        }
        let ordinary = NativeRelativePath::from_host_path(Path::new("src/lib.rs")).unwrap();
        assert!(!context.is_protected_mutation_path(&policy, &ordinary));
    }

    #[test]
    fn frozen_policy_uses_captured_ignore_bytes() {
        let root = unborn_repository();
        let store_root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".gitignore"), b"ignored\n").unwrap();
        fs::write(root.path().join(".fenceignore"), b"fence-only\n").unwrap();
        fs::write(root.path().join("ignored"), b"not captured").unwrap();
        fs::write(root.path().join("fence-only"), b"not captured").unwrap();
        fs::write(root.path().join("kept"), b"captured").unwrap();

        let context = GitContext::discover(root.path()).unwrap();
        let store = ObjectStore::open(store_root.path()).unwrap();
        let policy = context.capture_frozen_policy(&store).unwrap();
        let frozen = policy
            .compile(&store, root.path(), context.tracked_paths())
            .unwrap();
        fs::write(root.path().join(".gitignore"), b"").unwrap();
        fs::write(root.path().join(".fenceignore"), b"").unwrap();
        assert_eq!(
            frozen
                .classify(
                    &NativeRelativePath::from_host_path(Path::new("ignored")).unwrap(),
                    ObservedKind::Regular,
                )
                .unwrap(),
            ScopeDecision::Exclude
        );
        assert_eq!(
            frozen
                .classify(
                    &NativeRelativePath::from_host_path(Path::new("fence-only")).unwrap(),
                    ObservedKind::Regular,
                )
                .unwrap(),
            ScopeDecision::Exclude
        );
    }

    #[test]
    fn endpoint_tracked_paths_override_the_frozen_ignore_match() {
        let root = unborn_repository();
        let store_root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".gitignore"), b"generated\n").unwrap();
        fs::write(root.path().join("generated"), b"content").unwrap();
        let context = GitContext::discover(root.path()).unwrap();
        let store = ObjectStore::open(store_root.path()).unwrap();
        let policy = context.capture_frozen_policy(&store).unwrap();
        let generated = NativeRelativePath::from_host_path(Path::new("generated")).unwrap();
        let endpoint_tracked = BTreeSet::from([generated.clone()]);
        let scope = policy
            .compile(&store, root.path(), &endpoint_tracked)
            .unwrap();
        assert_eq!(
            scope.classify(&generated, ObservedKind::Regular).unwrap(),
            ScopeDecision::Include
        );
        assert!(
            !policy
                .drift_from(&context.capture_frozen_policy(&store).unwrap())
                .any()
        );
    }

    #[test]
    #[cfg(unix)]
    fn root_fenceignore_symlink_is_refused_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = unborn_repository();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"secret-pattern\n").unwrap();
        symlink(outside.path(), root.path().join(".fenceignore")).unwrap();
        let context = GitContext::discover(root.path()).unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(store_root.path()).unwrap();
        assert!(matches!(
            context.capture_frozen_policy(&store),
            Err(GitError::UnsupportedIgnoreSource(path))
                if path == root.path().join(".fenceignore")
        ));
    }
}
