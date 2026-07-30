use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
use cap_std::ambient_authority;
#[cfg(not(windows))]
use cap_std::fs::{Dir, Metadata};
use fence_core::{
    NativeRelativePath, NativeString, ObjectId, ObjectStore, ObservedKind, OmissionReason,
    ScopeClassifier, ScopeDecision,
};
#[cfg(windows)]
use fence_windows::{DirectoryEntry, DirectoryHandle, NodeHandle, NodeKind, RootHandle};
use serde::{Deserialize, Serialize};

use super::{FrozenGitScope, GitContext, GitError, MAX_IGNORE_BYTES, read_stable_bounded};

const FROZEN_POLICY_SCHEMA: u16 = 1;
const MAX_POLICY_PATH_SETS: usize = 1_000_000;
const MAX_POLICY_SCAN_ENTRIES: usize = 1_000_000;
const MAX_POLICY_SOURCES: usize = 250_000;

/// Immutable bytes for one ignore source.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyBlob {
    pub object: ObjectId,
    pub raw_size: u64,
}

/// File state for an external policy source.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FrozenIgnoreFile {
    Absent,
    Regular(PolicyBlob),
    Directory,
    Symlink,
    Unsupported,
}

/// How Git selected an external ignore source.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExternalIgnoreOrigin {
    CoreExcludesFile,
    XdgDefault,
    GitCommonInfoExclude,
}

/// Exact selection, path, and bytes for an external Git exclusion source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExternalIgnorePolicy {
    pub origin: ExternalIgnoreOrigin,
    pub path: Option<NativeString>,
    pub file: FrozenIgnoreFile,
}

/// Exact bytes and repository-relative location of one in-tree ignore source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FrozenIgnoreSource {
    pub path: NativeRelativePath,
    pub blob: PolicyBlob,
}

/// Complete immutable inclusion policy for all filesystem endpoints of one session.
///
/// The source vectors are already in matching order. Deserialization is followed by
/// [`FrozenGitPolicy::validate`] before use.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FrozenGitPolicy {
    schema: u16,
    pub ignore_case: bool,
    pub global_ignore: ExternalIgnorePolicy,
    pub info_exclude: ExternalIgnorePolicy,
    pub gitignore_sources: Vec<FrozenIgnoreSource>,
    #[serde(rename = "root_anchorignore")]
    pub root_fenceignore: FrozenIgnoreFile,
    pub base_tracked: BTreeSet<NativeRelativePath>,
    pub submodules: BTreeSet<NativeRelativePath>,
    pub nested_repositories: BTreeSet<NativeRelativePath>,
    pub store_relative: Option<NativeRelativePath>,
}

/// Difference between the frozen policy and an endpoint observation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct PolicyDrift {
    pub ignore_sources_changed: bool,
    pub ignore_case_changed: bool,
    pub tracked_paths_changed: bool,
    pub submodule_boundaries_changed: bool,
    pub nested_repository_boundaries_changed: bool,
}

impl PolicyDrift {
    #[must_use]
    pub const fn any(self) -> bool {
        self.ignore_sources_changed
            || self.ignore_case_changed
            || self.tracked_paths_changed
            || self.submodule_boundaries_changed
            || self.nested_repository_boundaries_changed
    }

    #[must_use]
    pub const fn capture_boundary_changed(self) -> bool {
        self.submodule_boundaries_changed || self.nested_repository_boundaries_changed
    }
}

impl FrozenGitPolicy {
    /// Validate an untrusted persistent policy before compiling it.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] for unsupported schemas, invalid source order, or impossible source
    /// states.
    pub fn validate(&self) -> Result<(), GitError> {
        if self.schema != FROZEN_POLICY_SCHEMA {
            return Err(GitError::UnsupportedPolicySchema(self.schema));
        }
        if self.global_ignore.path.is_none() && self.global_ignore.file != FrozenIgnoreFile::Absent
        {
            return Err(GitError::InvalidFrozenPolicy(
                "an unselected global ignore cannot contain file state".to_owned(),
            ));
        }
        if self.info_exclude.path.is_none() {
            return Err(GitError::InvalidFrozenPolicy(
                "the common info/exclude path is missing".to_owned(),
            ));
        }
        if self.global_ignore.origin == ExternalIgnoreOrigin::GitCommonInfoExclude
            || self.info_exclude.origin != ExternalIgnoreOrigin::GitCommonInfoExclude
        {
            return Err(GitError::InvalidFrozenPolicy(
                "external ignore origins are inconsistent".to_owned(),
            ));
        }
        validate_ignore_file(self.global_ignore.file)?;
        validate_ignore_file(self.info_exclude.file)?;
        validate_ignore_file(self.root_fenceignore)?;
        if self.gitignore_sources.len() > MAX_POLICY_SOURCES
            || self.base_tracked.len() > MAX_POLICY_PATH_SETS
            || self.submodules.len() > MAX_POLICY_PATH_SETS
            || self.nested_repositories.len() > MAX_POLICY_PATH_SETS
        {
            return Err(GitError::InvalidFrozenPolicy(
                "frozen policy exceeds its entry-count limit".to_owned(),
            ));
        }
        if self
            .gitignore_sources
            .iter()
            .any(|source| source.blob.raw_size > MAX_IGNORE_BYTES)
        {
            return Err(GitError::InvalidFrozenPolicy(
                "in-tree ignore source exceeds its raw-byte limit".to_owned(),
            ));
        }
        if !self
            .gitignore_sources
            .windows(2)
            .all(|pair| source_order(&pair[0].path, &pair[1].path).is_lt())
        {
            return Err(GitError::InvalidFrozenPolicy(
                "in-tree ignore sources are duplicated or out of order".to_owned(),
            ));
        }
        Ok(())
    }

    /// Compile the persisted bytes without consulting live ignore files or Git configuration.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] if the policy or any referenced object is invalid.
    pub fn compile(
        &self,
        store: &ObjectStore,
        worktree_root: &Path,
        endpoint_tracked: &BTreeSet<NativeRelativePath>,
    ) -> Result<FrozenGitScope, GitError> {
        self.validate()?;
        let mut git_search = gix::ignore::Search::default();
        let mut fence_search = gix::ignore::Search::default();
        let parser = gix::ignore::search::Ignore::default();

        add_external_policy(&mut git_search, &self.global_ignore, store, parser)?;
        add_external_policy(&mut git_search, &self.info_exclude, store, parser)?;
        for source in &self.gitignore_sources {
            let bytes = store.get(source.blob.object, source.blob.raw_size)?;
            let host = source
                .path
                .to_host_path()
                .map_err(|error| GitError::InvalidFrozenPolicy(error.to_string()))?;
            git_search.add_patterns_buffer(
                &bytes,
                worktree_root.join(host),
                Some(worktree_root),
                parser,
            );
        }
        if let FrozenIgnoreFile::Regular(blob) = self.root_fenceignore {
            let bytes = store.get(blob.object, blob.raw_size)?;
            fence_search.add_patterns_buffer(
                &bytes,
                worktree_root.join(".fenceignore"),
                Some(worktree_root),
                parser,
            );
        }

        let mut tracked = self.base_tracked.clone();
        tracked.extend(endpoint_tracked.iter().cloned());
        Ok(FrozenGitScope {
            git_search,
            fence_search,
            case: if self.ignore_case {
                gix::glob::pattern::Case::Fold
            } else {
                gix::glob::pattern::Case::Sensitive
            },
            tracked,
            submodules: self.submodules.clone(),
            nested_repositories: self.nested_repositories.clone(),
            store_relative: self.store_relative.clone(),
        })
    }

    #[must_use]
    pub fn drift_from(&self, observed: &Self) -> PolicyDrift {
        PolicyDrift {
            ignore_sources_changed: self.global_ignore != observed.global_ignore
                || self.info_exclude != observed.info_exclude
                || self.gitignore_sources != observed.gitignore_sources
                || self.root_fenceignore != observed.root_fenceignore,
            ignore_case_changed: self.ignore_case != observed.ignore_case,
            tracked_paths_changed: self.base_tracked != observed.base_tracked,
            submodule_boundaries_changed: self.submodules != observed.submodules,
            nested_repository_boundaries_changed: self.nested_repositories
                != observed.nested_repositories,
        }
    }

    #[must_use]
    pub fn referenced_objects(&self) -> BTreeSet<(ObjectId, u64)> {
        let mut objects = BTreeSet::new();
        collect_file_object(self.global_ignore.file, &mut objects);
        collect_file_object(self.info_exclude.file, &mut objects);
        collect_file_object(self.root_fenceignore, &mut objects);
        objects.extend(
            self.gitignore_sources
                .iter()
                .map(|source| (source.blob.object, source.blob.raw_size)),
        );
        objects
    }
}

impl GitContext {
    /// Freeze every source that affects Fence's inclusion policy.
    ///
    /// The scan is read-only and stores source bytes as normal immutable Fence objects. Callers
    /// must repeat the scan after the before capture and require exact equality before launching
    /// the child.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when a source is unreadable, unstable, oversized, or cannot be
    /// classified losslessly.
    pub fn capture_frozen_policy(&self, store: &ObjectStore) -> Result<FrozenGitPolicy, GitError> {
        let scope = self.live_scope()?;
        self.observe_frozen_policy(store, &scope)
    }

    /// Observe live policy sources while walking the namespace selected by `scope`.
    ///
    /// This is used after the initial freeze to report policy drift without changing the scope.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] for unstable or unreadable sources and unsafe paths.
    pub fn observe_frozen_policy(
        &self,
        store: &ObjectStore,
        scope: &impl ScopeClassifier,
    ) -> Result<FrozenGitPolicy, GitError> {
        let global_ignore = self.capture_global_ignore(store)?;
        let info_path = self.common_dir.join("info").join("exclude");
        let info_exclude = ExternalIgnorePolicy {
            origin: ExternalIgnoreOrigin::GitCommonInfoExclude,
            path: Some(NativeString::from_host(info_path.as_os_str())),
            file: capture_external_file(&info_path, store)?,
        };
        let (gitignore_sources, root_fenceignore, nested_repositories) =
            self.scan_in_tree_policy(store, scope)?;
        Ok(FrozenGitPolicy {
            schema: FROZEN_POLICY_SCHEMA,
            ignore_case: matches!(self.ignore_case(), gix::glob::pattern::Case::Fold),
            global_ignore,
            info_exclude,
            gitignore_sources,
            root_fenceignore,
            base_tracked: self.tracked.clone(),
            submodules: self.submodules.clone(),
            nested_repositories,
            store_relative: self
                .store_location
                .root
                .strip_prefix(&self.worktree_root)
                .ok()
                .and_then(|path| NativeRelativePath::from_host_path(path).ok()),
        })
    }

    fn capture_global_ignore(&self, store: &ObjectStore) -> Result<ExternalIgnorePolicy, GitError> {
        if let Some(path) = self
            .repository
            .config_snapshot()
            .trusted_path("core.excludesFile")
            .map_err(|error| GitError::Ignore(error.to_string()))?
        {
            return Ok(ExternalIgnorePolicy {
                origin: ExternalIgnoreOrigin::CoreExcludesFile,
                path: Some(NativeString::from_host(path.as_os_str())),
                file: capture_external_file(&path, store)?,
            });
        }
        let path = gix::path::env::xdg_config("git/ignore", &mut |name| std::env::var_os(name));
        Ok(match path {
            Some(path) => ExternalIgnorePolicy {
                origin: ExternalIgnoreOrigin::XdgDefault,
                path: Some(NativeString::from_host(path.as_os_str())),
                file: capture_external_file(&path, store)?,
            },
            None => ExternalIgnorePolicy {
                origin: ExternalIgnoreOrigin::XdgDefault,
                path: None,
                file: FrozenIgnoreFile::Absent,
            },
        })
    }

    fn scan_in_tree_policy(
        &self,
        store: &ObjectStore,
        scope: &impl ScopeClassifier,
    ) -> Result<
        (
            Vec<FrozenIgnoreSource>,
            FrozenIgnoreFile,
            BTreeSet<NativeRelativePath>,
        ),
        GitError,
    > {
        #[cfg(not(windows))]
        {
            scan_in_tree_policy_capability(&self.worktree_root, store, scope)
        }
        #[cfg(windows)]
        {
            scan_in_tree_policy_windows(&self.worktree_root, store, scope)
        }
    }
}

#[cfg(not(windows))]
fn scan_in_tree_policy_capability(
    worktree_root: &Path,
    store: &ObjectStore,
    scope: &impl ScopeClassifier,
) -> Result<
    (
        Vec<FrozenIgnoreSource>,
        FrozenIgnoreFile,
        BTreeSet<NativeRelativePath>,
    ),
    GitError,
> {
    let root = Dir::open_ambient_dir(worktree_root, ambient_authority())?;
    let root_path = NativeRelativePath::from_host_path(Path::new(""))
        .map_err(|error| GitError::InvalidFrozenPolicy(error.to_string()))?;
    let mut pending = vec![(root_path, root)];
    let mut sources = Vec::new();
    let mut fenceignore = FrozenIgnoreFile::Absent;
    let mut nested = BTreeSet::new();
    let mut scanned_entries = 0_usize;

    while let Some((directory_path, directory)) = pending.pop() {
        let first_names = policy_directory_names(&directory)?;
        scanned_entries = scanned_entries.saturating_add(first_names.len());
        if scanned_entries > MAX_POLICY_SCAN_ENTRIES {
            return Err(GitError::PolicyScanLimitExceeded(MAX_POLICY_SCAN_ENTRIES));
        }
        for name in &first_names {
            let path = directory_path
                .join_host_component(name)
                .map_err(|error| GitError::InvalidFrozenPolicy(error.to_string()))?;
            let before = directory.symlink_metadata(name)?;
            let kind = observed_cap_kind(before.file_type());

            if name == OsStr::new(".gitignore") && kind == ObservedKind::Regular {
                sources.push(FrozenIgnoreSource {
                    path: path.clone(),
                    blob: capture_capability_policy_file(
                        &directory,
                        name,
                        &worktree_root.join(
                            path.to_host_path().map_err(|error| {
                                GitError::InvalidFrozenPolicy(error.to_string())
                            })?,
                        ),
                        store,
                    )?,
                });
            }
            if directory_path.is_root() && name == OsStr::new(".fenceignore") {
                if kind != ObservedKind::Regular {
                    return Err(GitError::UnsupportedIgnoreSource(
                        worktree_root.join(".fenceignore"),
                    ));
                }
                fenceignore = FrozenIgnoreFile::Regular(capture_capability_policy_file(
                    &directory,
                    name,
                    &worktree_root.join(".fenceignore"),
                    store,
                )?);
            }

            match scope
                .classify(&path, kind)
                .map_err(|error| GitError::Ignore(error.to_string()))?
            {
                ScopeDecision::Include if kind == ObservedKind::Directory => {
                    let child = directory.open_dir(name)?;
                    let opened = child.dir_metadata()?;
                    let after = directory.symlink_metadata(name)?;
                    if !same_cap_identity(&before, &opened)
                        || !same_cap_identity(&before, &after)
                        || !after.is_dir()
                    {
                        return Err(GitError::UnstablePolicyNamespace(path));
                    }
                    if capability_git_marker_exists(&child)? {
                        nested.insert(path.clone());
                    }
                    pending.push((path, child));
                }
                ScopeDecision::Boundary(OmissionReason::NestedRepository) => {
                    nested.insert(path);
                }
                ScopeDecision::Include | ScopeDecision::Exclude | ScopeDecision::Boundary(_) => {}
            }
        }
        if first_names != policy_directory_names(&directory)? {
            return Err(GitError::UnstablePolicyNamespace(directory_path));
        }
    }
    sources.sort_by(|left, right| source_order(&left.path, &right.path));
    Ok((sources, fenceignore, nested))
}

#[cfg(not(windows))]
fn policy_directory_names(directory: &Dir) -> Result<Vec<std::ffi::OsString>, GitError> {
    let mut names = directory
        .entries()?
        .map(|entry| entry.map(|entry| entry.file_name()).map_err(GitError::Io))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort_by(|left, right| {
        NativeString::from_host(left)
            .bytes()
            .cmp(NativeString::from_host(right).bytes())
    });
    Ok(names)
}

#[cfg(not(windows))]
fn capture_capability_policy_file(
    directory: &Dir,
    name: &OsStr,
    diagnostic_path: &Path,
    store: &ObjectStore,
) -> Result<PolicyBlob, GitError> {
    for _ in 0..super::INDEX_READ_RETRIES {
        let before = directory.symlink_metadata(name)?;
        if !before.is_file() {
            continue;
        }
        if before.len() > MAX_IGNORE_BYTES {
            return Err(GitError::IgnoreSourceTooLarge {
                path: diagnostic_path.to_path_buf(),
                size: before.len(),
                maximum: MAX_IGNORE_BYTES,
            });
        }
        let mut file = directory.open(name)?;
        let opened = file.metadata()?;
        if !opened.is_file() || !same_cap_identity(&before, &opened) {
            continue;
        }
        let mut bytes = Vec::with_capacity(usize::try_from(before.len()).unwrap_or(0));
        file.by_ref()
            .take(MAX_IGNORE_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)?;
        let after = directory.symlink_metadata(name)?;
        if same_cap_file_state(&before, &opened)
            && same_cap_file_state(&before, &after)
            && u64::try_from(bytes.len()).ok() == Some(before.len())
        {
            return Ok(PolicyBlob {
                object: store.put_bytes(&bytes)?,
                raw_size: before.len(),
            });
        }
    }
    Err(GitError::UnstableIgnoreSource(
        diagnostic_path.to_path_buf(),
    ))
}

#[cfg(unix)]
fn same_cap_identity(left: &Metadata, right: &Metadata) -> bool {
    use cap_std::fs::MetadataExt as _;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(all(not(unix), not(windows)))]
fn same_cap_identity(left: &Metadata, right: &Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

#[cfg(not(windows))]
fn same_cap_file_state(left: &Metadata, right: &Metadata) -> bool {
    same_cap_identity(left, right)
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

#[cfg(not(windows))]
fn observed_cap_kind(file_type: cap_std::fs::FileType) -> ObservedKind {
    if file_type.is_file() {
        ObservedKind::Regular
    } else if file_type.is_dir() {
        ObservedKind::Directory
    } else if file_type.is_symlink() {
        ObservedKind::Symlink
    } else {
        ObservedKind::Unsupported
    }
}

#[cfg(not(windows))]
fn capability_git_marker_exists(directory: &Dir) -> Result<bool, GitError> {
    match directory.symlink_metadata(".git") {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn scan_in_tree_policy_windows(
    worktree_root: &Path,
    store: &ObjectStore,
    scope: &impl ScopeClassifier,
) -> Result<
    (
        Vec<FrozenIgnoreSource>,
        FrozenIgnoreFile,
        BTreeSet<NativeRelativePath>,
    ),
    GitError,
> {
    let root = RootHandle::open(worktree_root)?.into_directory();
    let root_path = NativeRelativePath::from_host_path(Path::new(""))
        .map_err(|error| GitError::InvalidFrozenPolicy(error.to_string()))?;
    let mut pending = vec![(root_path, root)];
    let mut sources = Vec::new();
    let mut fenceignore = FrozenIgnoreFile::Absent;
    let mut nested = BTreeSet::new();
    let mut scanned_entries = 0_usize;

    while let Some((directory_path, directory)) = pending.pop() {
        let first = sorted_windows_policy_entries(&directory)?;
        scanned_entries = scanned_entries.saturating_add(first.len());
        if scanned_entries > MAX_POLICY_SCAN_ENTRIES {
            return Err(GitError::PolicyScanLimitExceeded(MAX_POLICY_SCAN_ENTRIES));
        }
        for entry in &first {
            let path = directory_path
                .join_host_component(&entry.name)
                .map_err(|error| GitError::InvalidFrozenPolicy(error.to_string()))?;
            let kind = windows_policy_kind(entry);
            if entry.name == OsStr::new(".gitignore") && kind == ObservedKind::Regular {
                let node = directory.open_child(entry)?;
                sources.push(FrozenIgnoreSource {
                    path: path.clone(),
                    blob: capture_windows_policy_file(
                        &node,
                        &worktree_root.join(
                            path.to_host_path().map_err(|error| {
                                GitError::InvalidFrozenPolicy(error.to_string())
                            })?,
                        ),
                        store,
                    )?,
                });
            }
            if directory_path.is_root() && entry.name == OsStr::new(".fenceignore") {
                if kind != ObservedKind::Regular {
                    return Err(GitError::UnsupportedIgnoreSource(
                        worktree_root.join(".fenceignore"),
                    ));
                }
                let node = directory.open_child(entry)?;
                fenceignore = FrozenIgnoreFile::Regular(capture_windows_policy_file(
                    &node,
                    &worktree_root.join(".fenceignore"),
                    store,
                )?);
            }
            match scope
                .classify(&path, kind)
                .map_err(|error| GitError::Ignore(error.to_string()))?
            {
                ScopeDecision::Include if kind == ObservedKind::Directory => {
                    let child = directory.open_child(entry)?.into_directory()?;
                    if child
                        .entries()?
                        .iter()
                        .any(|entry| entry.name == OsStr::new(".git"))
                    {
                        nested.insert(path.clone());
                    }
                    pending.push((path, child));
                }
                ScopeDecision::Boundary(OmissionReason::NestedRepository) => {
                    nested.insert(path);
                }
                ScopeDecision::Include | ScopeDecision::Exclude | ScopeDecision::Boundary(_) => {}
            }
        }
        if first != sorted_windows_policy_entries(&directory)? {
            return Err(GitError::UnstablePolicyNamespace(directory_path));
        }
    }
    sources.sort_by(|left, right| source_order(&left.path, &right.path));
    Ok((sources, fenceignore, nested))
}

#[cfg(windows)]
fn sorted_windows_policy_entries(
    directory: &DirectoryHandle,
) -> Result<Vec<DirectoryEntry>, GitError> {
    let mut entries = directory.entries()?;
    entries.sort_by(|left, right| {
        NativeString::from_host(&left.name)
            .bytes()
            .cmp(NativeString::from_host(&right.name).bytes())
    });
    Ok(entries)
}

#[cfg(windows)]
fn windows_policy_kind(entry: &DirectoryEntry) -> ObservedKind {
    if entry.reparse_tag.is_some() {
        ObservedKind::Symlink
    } else if entry.attributes & 0x10 != 0 {
        ObservedKind::Directory
    } else {
        ObservedKind::Regular
    }
}

#[cfg(windows)]
fn capture_windows_policy_file(
    node: &NodeHandle,
    diagnostic_path: &Path,
    store: &ObjectStore,
) -> Result<PolicyBlob, GitError> {
    for _ in 0..super::INDEX_READ_RETRIES {
        let before = node.refresh_metadata()?;
        if before.kind != NodeKind::RegularFile {
            continue;
        }
        if before.size > MAX_IGNORE_BYTES {
            return Err(GitError::IgnoreSourceTooLarge {
                path: diagnostic_path.to_path_buf(),
                size: before.size,
                maximum: MAX_IGNORE_BYTES,
            });
        }
        let mut file = node.try_clone_file()?;
        let mut bytes = Vec::with_capacity(usize::try_from(before.size).unwrap_or(0));
        file.by_ref()
            .take(MAX_IGNORE_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)?;
        let after = node.refresh_metadata()?;
        if before == after
            && u64::try_from(bytes.len()).ok() == Some(before.size)
            && node.verify_path_identity().is_ok()
        {
            return Ok(PolicyBlob {
                object: store.put_bytes(&bytes)?,
                raw_size: before.size,
            });
        }
    }
    Err(GitError::UnstableIgnoreSource(
        diagnostic_path.to_path_buf(),
    ))
}

fn add_external_policy(
    search: &mut gix::ignore::Search,
    source: &ExternalIgnorePolicy,
    store: &ObjectStore,
    parser: gix::ignore::search::Ignore,
) -> Result<(), GitError> {
    let (Some(path), FrozenIgnoreFile::Regular(blob)) = (&source.path, source.file) else {
        return Ok(());
    };
    let path = PathBuf::from(
        path.to_host()
            .map_err(|error| GitError::InvalidFrozenPolicy(error.to_string()))?,
    );
    let bytes = store.get(blob.object, blob.raw_size)?;
    search.add_patterns_buffer(&bytes, path, None, parser);
    Ok(())
}

fn capture_external_file(path: &Path, store: &ObjectStore) -> Result<FrozenIgnoreFile, GitError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FrozenIgnoreFile::Absent);
        }
        Err(error) => return Err(error.into()),
    };
    let file_type = metadata.file_type();
    if file_type.is_file() {
        return capture_regular_policy_file(path, store).map(FrozenIgnoreFile::Regular);
    }
    Err(GitError::UnsupportedIgnoreSource(path.to_path_buf()))
}

fn capture_regular_policy_file(path: &Path, store: &ObjectStore) -> Result<PolicyBlob, GitError> {
    let bytes = read_stable_bounded(path, MAX_IGNORE_BYTES)?;
    Ok(PolicyBlob {
        object: store.put_bytes(&bytes)?,
        raw_size: u64::try_from(bytes.len()).map_err(|_| GitError::IgnoreSourceTooLarge {
            path: path.to_path_buf(),
            size: u64::MAX,
            maximum: MAX_IGNORE_BYTES,
        })?,
    })
}

fn source_order(left: &NativeRelativePath, right: &NativeRelativePath) -> std::cmp::Ordering {
    left.components()
        .len()
        .cmp(&right.components().len())
        .then_with(|| left.cmp(right))
}

fn collect_file_object(file: FrozenIgnoreFile, objects: &mut BTreeSet<(ObjectId, u64)>) {
    if let FrozenIgnoreFile::Regular(blob) = file {
        objects.insert((blob.object, blob.raw_size));
    }
}

fn validate_ignore_file(file: FrozenIgnoreFile) -> Result<(), GitError> {
    match file {
        FrozenIgnoreFile::Absent => Ok(()),
        FrozenIgnoreFile::Regular(blob) if blob.raw_size <= MAX_IGNORE_BYTES => Ok(()),
        FrozenIgnoreFile::Regular(_) => Err(GitError::InvalidFrozenPolicy(
            "external ignore source exceeds its raw-byte limit".to_owned(),
        )),
        FrozenIgnoreFile::Directory | FrozenIgnoreFile::Symlink | FrozenIgnoreFile::Unsupported => {
            Err(GitError::InvalidFrozenPolicy(
                "unsupported external ignore source state".to_owned(),
            ))
        }
    }
}
