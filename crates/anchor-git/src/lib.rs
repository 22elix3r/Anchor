//! Read-only Git awareness for Anchor.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anchor_core::{
    Manifest, ManifestNode, NativeRelativePath, NativeString, ObjectId, ObjectStore, ObservedKind,
    OmissionReason, ScopeClassifier, ScopeDecision, ScopeError, StoreError,
};
use thiserror::Error;

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
        let root = self
            .common_dir
            .join("anchor")
            .join("users")
            .join(principal_key())
            .join("v1");
        let worktree_key = if self.git_dir == self.common_dir {
            "main".to_owned()
        } else {
            let native = NativeString::from_host(self.git_dir.as_os_str());
            format!("wt-{}", short_hash(native.bytes()))
        };
        StoreLocation { root, worktree_key }
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
        let mut anchor_ignore = gix::ignore::Search::default();
        add_in_tree_ignore(
            &mut anchor_ignore,
            &self.worktree_root.join(".anchorignore"),
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
            anchor_ignore,
            case: self.ignore_case(),
            tracked: &self.tracked,
            submodules: &self.submodules,
            worktree_root: &self.worktree_root,
            store_relative,
        })
    }

    /// Compile a frozen ignore policy from the complete before manifest.
    ///
    /// Nested `.gitignore` and `.anchorignore` bytes are read from immutable Anchor objects, so
    /// later edits to those files do not change the session scope.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when an ignore source is missing, corrupt, or cannot be compiled.
    pub fn frozen_scope(
        &self,
        before: &Manifest,
        store: &ObjectStore,
        endpoint_tracked: &BTreeSet<NativeRelativePath>,
    ) -> Result<FrozenGitScope, GitError> {
        let mut git_search = gix::ignore::Search::default();
        let mut anchor_search = gix::ignore::Search::default();
        let parser = gix::ignore::search::Ignore::default();

        if let Some(global) = self.global_ignore_path()? {
            add_external_ignore(&mut git_search, &global)?;
        }
        add_external_ignore(
            &mut git_search,
            &self.common_dir.join("info").join("exclude"),
        )?;

        let mut in_tree_sources = before
            .entries()
            .iter()
            .filter_map(|entry| {
                let filename = entry.path.to_host_path().ok()?.file_name()?.to_owned();
                let kind = if filename == OsStr::new(".gitignore") {
                    IgnoreSourceKind::Git
                } else if filename == OsStr::new(".anchorignore") {
                    if entry.path.components().len() != 1 {
                        return None;
                    }
                    IgnoreSourceKind::Anchor
                } else {
                    return None;
                };
                let ManifestNode::Regular {
                    object, raw_size, ..
                } = &entry.node
                else {
                    return None;
                };
                Some((kind, entry.path.clone(), *object, *raw_size))
            })
            .collect::<Vec<_>>();
        in_tree_sources.sort_by(|left, right| {
            left.1
                .components()
                .len()
                .cmp(&right.1.components().len())
                .then_with(|| left.1.cmp(&right.1))
        });
        for (kind, path, object, raw_size) in in_tree_sources {
            let bytes = store.get(object, raw_size)?;
            let host = path
                .to_host_path()
                .map_err(|error| GitError::Ignore(error.to_string()))?;
            let search = match kind {
                IgnoreSourceKind::Git => &mut git_search,
                IgnoreSourceKind::Anchor => &mut anchor_search,
            };
            search.add_patterns_buffer(
                &bytes,
                self.worktree_root.join(host),
                Some(&self.worktree_root),
                parser,
            );
        }

        let mut tracked = self.tracked.clone();
        tracked.extend(endpoint_tracked.iter().cloned());
        Ok(FrozenGitScope {
            git_search,
            anchor_search,
            case: self.ignore_case(),
            tracked,
            submodules: self.submodules.clone(),
            worktree_root: self.worktree_root.clone(),
            store_relative: self
                .store_location()
                .root
                .strip_prefix(&self.worktree_root)
                .ok()
                .and_then(|path| NativeRelativePath::from_host_path(path).ok()),
        })
    }

    fn global_ignore_path(&self) -> Result<Option<PathBuf>, GitError> {
        if let Some(path) = self
            .repository
            .config_snapshot()
            .trusted_path("core.excludesFile")
            .map_err(|error| GitError::Ignore(error.to_string()))?
        {
            return Ok(Some(path));
        }
        Ok(gix::path::env::xdg_config("git/ignore", &mut |name| {
            std::env::var_os(name)
        }))
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

    /// Capture exact raw index bytes into Anchor's object store.
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
    anchor_ignore: gix::ignore::Search,
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
        if is_root_anchor_ignore(path) {
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
        let anchor_excluded = self
            .anchor_ignore
            .pattern_matching_relative_path(
                relative.as_ref(),
                Some(kind == ObservedKind::Directory),
                self.case,
            )
            .is_some_and(|matched| !matched.pattern.is_negative());
        Ok(if anchor_excluded {
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
    anchor_search: gix::ignore::Search,
    case: gix::glob::pattern::Case,
    tracked: BTreeSet<NativeRelativePath>,
    submodules: BTreeSet<NativeRelativePath>,
    worktree_root: PathBuf,
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
        if is_root_anchor_ignore(path) {
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
            if self.worktree_root.join(host).join(".git").exists() {
                return Ok(ScopeDecision::Boundary(OmissionReason::NestedRepository));
            }
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
        let anchor_excluded = self
            .anchor_search
            .pattern_matching_relative_path(relative.as_ref(), is_directory, self.case)
            .is_some_and(|matched| !matched.pattern.is_negative());
        Ok(if anchor_excluded {
            ScopeDecision::Exclude
        } else {
            ScopeDecision::Include
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreLocation {
    pub root: PathBuf,
    pub worktree_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeadState {
    Unborn { referent: Vec<u8> },
    Attached { referent: Vec<u8>, target: String },
    Detached { target: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexCapture {
    Absent,
    Present {
        object: ObjectId,
        raw_size: u64,
        summary: IndexSummary,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IgnoreSourceKind {
    Git,
    Anchor,
}

fn add_external_ignore(search: &mut gix::ignore::Search, path: &Path) -> Result<(), GitError> {
    if !path.exists() {
        return Ok(());
    }
    let bytes = read_stable_bounded(path, MAX_IGNORE_BYTES)?;
    search.add_patterns_buffer(
        &bytes,
        path.to_path_buf(),
        None,
        gix::ignore::search::Ignore::default(),
    );
    Ok(())
}

fn add_in_tree_ignore(
    search: &mut gix::ignore::Search,
    path: &Path,
    root: &Path,
) -> Result<(), GitError> {
    if !path.exists() {
        return Ok(());
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

fn is_root_anchor_ignore(path: &NativeRelativePath) -> bool {
    let expected = NativeRelativePath::from_host_path(Path::new(".anchorignore"));
    expected.is_ok_and(|expected| expected == *path)
}

fn is_below(path: &NativeRelativePath, parent: &NativeRelativePath) -> bool {
    path == parent || is_strict_descendant(path, parent)
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

#[cfg(not(unix))]
fn principal_key() -> String {
    let principal = std::env::var_os("USERNAME").unwrap_or_else(|| "unknown".into());
    let native = NativeString::from_host(principal.as_os_str());
    format!("p-{}", short_hash(native.bytes()))
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
    #[error("ignore source {path} has {size} bytes, above limit {maximum}")]
    IgnoreSourceTooLarge {
        path: PathBuf,
        size: u64,
        maximum: u64,
    },
    #[error("ignore source remained unstable at {0}")]
    UnstableIgnoreSource(PathBuf),
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
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_core::{CaptureEngine, CaptureOptions};

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
    fn frozen_scope_uses_before_ignore_bytes() {
        let root = unborn_repository();
        let store_root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".gitignore"), b"ignored\n").unwrap();
        fs::write(root.path().join(".anchorignore"), b"anchor-only\n").unwrap();
        fs::write(root.path().join("ignored"), b"not captured").unwrap();
        fs::write(root.path().join("anchor-only"), b"not captured").unwrap();
        fs::write(root.path().join("kept"), b"captured").unwrap();

        let context = GitContext::discover(root.path()).unwrap();
        let store = ObjectStore::open(store_root.path()).unwrap();
        let before = CaptureEngine::new(&store, CaptureOptions::default())
            .capture(root.path(), &context.live_scope().unwrap())
            .unwrap()
            .manifest;
        let names = before
            .entries()
            .iter()
            .map(|entry| entry.path.to_host_path().unwrap())
            .collect::<BTreeSet<_>>();
        assert!(names.contains(Path::new(".gitignore")));
        assert!(names.contains(Path::new(".anchorignore")));
        assert!(names.contains(Path::new("kept")));
        assert!(!names.contains(Path::new("ignored")));
        assert!(!names.contains(Path::new("anchor-only")));

        let frozen = context
            .frozen_scope(&before, &store, context.tracked_paths())
            .unwrap();
        fs::write(root.path().join(".gitignore"), b"").unwrap();
        fs::write(root.path().join(".anchorignore"), b"").unwrap();
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
                    &NativeRelativePath::from_host_path(Path::new("anchor-only")).unwrap(),
                    ObservedKind::Regular,
                )
                .unwrap(),
            ScopeDecision::Exclude
        );
    }
}
