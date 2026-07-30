use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, FileType, Metadata};
use cap_std::time::SystemTime;
use thiserror::Error;

use crate::{
    Completeness, Coverage, Manifest, ManifestEntry, ManifestError, ManifestNode,
    NativeRelativePath, NativeString, ObjectStore, Omission, OmissionReason, PathEncoding,
    PathError, SafetyObservations, StoreError,
};

const FILE_STABILITY_RETRIES: usize = 3;
const NAMESPACE_RETRIES: usize = 2;

/// The file type observed before applying an inclusion policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservedKind {
    Regular,
    Directory,
    Symlink,
    Unsupported,
}

/// A capture-scope decision supplied by a repository-aware policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeDecision {
    Include,
    Exclude,
    Boundary(OmissionReason),
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct ScopeError {
    pub message: String,
}

/// Policy boundary between generic filesystem capture and repository awareness.
pub trait ScopeClassifier {
    /// Classify one safely observed path.
    ///
    /// # Errors
    ///
    /// Returns [`ScopeError`] when policy evaluation cannot be completed.
    fn classify(
        &self,
        path: &NativeRelativePath,
        kind: ObservedKind,
    ) -> Result<ScopeDecision, ScopeError>;
}

/// Classifier that includes every observed node.
#[derive(Clone, Copy, Debug, Default)]
pub struct IncludeAll;

impl ScopeClassifier for IncludeAll {
    fn classify(
        &self,
        _path: &NativeRelativePath,
        _kind: ObservedKind,
    ) -> Result<ScopeDecision, ScopeError> {
        Ok(ScopeDecision::Include)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureLimits {
    pub max_files: u64,
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_files: 250_000,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            max_file_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptureOptions {
    pub limits: CaptureLimits,
    pub allow_degraded: bool,
    pub cross_mounts: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptureStatistics {
    pub regular_files: u64,
    pub symlinks: u64,
    pub empty_directories: u64,
    pub raw_bytes: u64,
    pub excluded_nodes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureResult {
    pub manifest: Manifest,
    pub statistics: CaptureStatistics,
}

/// Serial, capability-rooted filesystem capture.
#[derive(Debug)]
pub struct CaptureEngine<'store> {
    store: &'store ObjectStore,
    options: CaptureOptions,
}

impl<'store> CaptureEngine<'store> {
    #[must_use]
    pub const fn new(store: &'store ObjectStore, options: CaptureOptions) -> Self {
        Self { store, options }
    }

    /// Capture a root using a caller-provided inclusion classifier.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] for unsafe, unstable, unsupported, oversized, or inaccessible
    /// nodes. No manifest is returned unless the declared completeness is trustworthy.
    pub fn capture(
        &self,
        root: &Path,
        classifier: &impl ScopeClassifier,
    ) -> Result<CaptureResult, CaptureError> {
        let root_dir = Dir::open_ambient_dir(root, ambient_authority())
            .map_err(|source| CaptureError::Root { source })?;
        let root_metadata = root_dir
            .dir_metadata()
            .map_err(|source| CaptureError::Root { source })?;
        let root_device = device_id(&root_metadata);

        let mut last_namespace_error = None;
        for _ in 0..NAMESPACE_RETRIES {
            let mut attempt = CaptureAttempt {
                store: self.store,
                options: self.options,
                classifier,
                entries: Vec::new(),
                omissions: Vec::new(),
                statistics: CaptureStatistics::default(),
                hardlinks: HashMap::new(),
                next_hardlink_group: 1,
                root_device,
            };
            let root_path = NativeRelativePath::new(PathEncoding::host(), Vec::new())
                .map_err(CaptureError::Path)?;
            match attempt.capture_directory(&root_dir, &root_path, true) {
                Ok(()) => {
                    let completeness = if attempt.omissions.is_empty() {
                        Completeness::Complete
                    } else {
                        Completeness::Degraded
                    };
                    let manifest = Manifest::new(
                        PathEncoding::host(),
                        attempt.entries,
                        Coverage {
                            completeness,
                            omissions: attempt.omissions,
                        },
                    )?;
                    return Ok(CaptureResult {
                        manifest,
                        statistics: attempt.statistics,
                    });
                }
                Err(error @ CaptureError::UnstableNamespace { .. }) => {
                    last_namespace_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(
            last_namespace_error.unwrap_or(CaptureError::UnstableNamespace {
                path: NativeRelativePath::new(PathEncoding::host(), Vec::new())
                    .map_err(CaptureError::Path)?,
            }),
        )
    }
}

struct CaptureAttempt<'store, 'classifier, C> {
    store: &'store ObjectStore,
    options: CaptureOptions,
    classifier: &'classifier C,
    entries: Vec<ManifestEntry>,
    omissions: Vec<Omission>,
    statistics: CaptureStatistics,
    hardlinks: HashMap<FileIdentity, u64>,
    next_hardlink_group: u64,
    root_device: Option<u64>,
}

impl<C: ScopeClassifier> CaptureAttempt<'_, '_, C> {
    fn capture_directory(
        &mut self,
        directory: &Dir,
        relative: &NativeRelativePath,
        is_root: bool,
    ) -> Result<(), CaptureError> {
        let first_names = read_sorted_names(directory, relative)?;
        if first_names.is_empty() && !is_root {
            self.entries.push(ManifestEntry {
                path: relative.clone(),
                node: ManifestNode::EmptyDirectory,
                safety: SafetyObservations::default(),
            });
            self.statistics.empty_directories += 1;
        }

        for name in &first_names {
            let child_path = relative
                .join_host_component(name)
                .map_err(CaptureError::Path)?;
            let before = directory
                .symlink_metadata(name)
                .map_err(|source| CaptureError::Io {
                    path: child_path.clone(),
                    source,
                })?;
            let kind = observed_kind(before.file_type());
            match self.classifier.classify(&child_path, kind)? {
                ScopeDecision::Exclude => {
                    self.statistics.excluded_nodes += 1;
                }
                ScopeDecision::Boundary(reason) => {
                    self.degrade_or_fail(child_path, reason)?;
                }
                ScopeDecision::Include => {
                    if !self.options.cross_mounts {
                        if let (Some(device), Some(root_device)) =
                            (device_id(&before), self.root_device)
                        {
                            if root_device != device {
                                self.degrade_or_fail(child_path, OmissionReason::MountBoundary)?;
                                continue;
                            }
                        }
                    }
                    match kind {
                        ObservedKind::Regular => {
                            self.capture_regular(directory, name, &child_path)?;
                        }
                        ObservedKind::Directory => {
                            let child =
                                directory
                                    .open_dir(name)
                                    .map_err(|source| CaptureError::Io {
                                        path: child_path.clone(),
                                        source,
                                    })?;
                            let opened =
                                child.dir_metadata().map_err(|source| CaptureError::Io {
                                    path: child_path.clone(),
                                    source,
                                })?;
                            let after = directory.symlink_metadata(name).map_err(|source| {
                                CaptureError::Io {
                                    path: child_path.clone(),
                                    source,
                                }
                            })?;
                            if !same_identity(&before, &opened)
                                || !same_identity(&before, &after)
                                || !after.is_dir()
                            {
                                return Err(CaptureError::UnstableNamespace { path: child_path });
                            }
                            self.capture_directory(&child, &child_path, false)?;
                        }
                        ObservedKind::Symlink => {
                            self.capture_symlink(directory, name, &child_path)?;
                        }
                        ObservedKind::Unsupported => {
                            self.degrade_or_fail(child_path, OmissionReason::UnsupportedType)?;
                        }
                    }
                }
            }
        }

        let second_names = read_sorted_names(directory, relative)?;
        if first_names != second_names {
            return Err(CaptureError::UnstableNamespace {
                path: relative.clone(),
            });
        }
        Ok(())
    }

    fn capture_regular(
        &mut self,
        directory: &Dir,
        name: &OsString,
        path: &NativeRelativePath,
    ) -> Result<(), CaptureError> {
        for _ in 0..FILE_STABILITY_RETRIES {
            let before = directory
                .symlink_metadata(name)
                .map_err(|source| CaptureError::Io {
                    path: path.clone(),
                    source,
                })?;
            if !before.is_file() {
                continue;
            }
            if before.len() > self.options.limits.max_file_bytes {
                return Err(CaptureError::FileLimit {
                    path: path.clone(),
                    size: before.len(),
                    maximum: self.options.limits.max_file_bytes,
                });
            }

            let mut file = directory.open(name).map_err(|source| CaptureError::Io {
                path: path.clone(),
                source,
            })?;
            let opened = file.metadata().map_err(|source| CaptureError::Io {
                path: path.clone(),
                source,
            })?;
            if !opened.is_file() || !same_identity(&before, &opened) {
                continue;
            }

            let (object, raw_size) = self.store.put(&mut file)?;
            let after_open = file.metadata().map_err(|source| CaptureError::Io {
                path: path.clone(),
                source,
            })?;
            let after_path =
                directory
                    .symlink_metadata(name)
                    .map_err(|source| CaptureError::Io {
                        path: path.clone(),
                        source,
                    })?;
            if stable_file(&before, &opened, &after_open, &after_path) && raw_size == before.len() {
                self.record_regular(path.clone(), object, raw_size, &before)?;
                return Ok(());
            }
        }
        self.degrade_or_fail(path.clone(), OmissionReason::Unstable)
    }

    fn record_regular(
        &mut self,
        path: NativeRelativePath,
        object: crate::ObjectId,
        raw_size: u64,
        metadata: &Metadata,
    ) -> Result<(), CaptureError> {
        let next_count = self
            .statistics
            .regular_files
            .checked_add(1)
            .ok_or(CaptureError::CountOverflow)?;
        if next_count > self.options.limits.max_files {
            return Err(CaptureError::FileCountLimit {
                maximum: self.options.limits.max_files,
            });
        }
        let total = self
            .statistics
            .raw_bytes
            .checked_add(raw_size)
            .ok_or(CaptureError::ByteCountOverflow)?;
        if total > self.options.limits.max_total_bytes {
            return Err(CaptureError::TotalLimit {
                size: total,
                maximum: self.options.limits.max_total_bytes,
            });
        }

        let hardlink_group = hardlink_identity(metadata).map(|identity| {
            *self.hardlinks.entry(identity).or_insert_with(|| {
                let group = self.next_hardlink_group;
                self.next_hardlink_group += 1;
                group
            })
        });
        self.entries.push(ManifestEntry {
            path,
            node: ManifestNode::Regular {
                object,
                raw_size,
                unix_exec_bits: execute_bits(metadata),
                windows_readonly: None,
            },
            safety: SafetyObservations {
                hardlink_group,
                link_count: link_count(metadata),
                extended_metadata_present: false,
            },
        });
        self.statistics.regular_files = next_count;
        self.statistics.raw_bytes = total;
        Ok(())
    }

    fn capture_symlink(
        &mut self,
        directory: &Dir,
        name: &OsString,
        path: &NativeRelativePath,
    ) -> Result<(), CaptureError> {
        for _ in 0..FILE_STABILITY_RETRIES {
            let before = directory
                .symlink_metadata(name)
                .map_err(|source| CaptureError::Io {
                    path: path.clone(),
                    source,
                })?;
            if !before.file_type().is_symlink() {
                continue;
            }
            let target = directory
                .read_link_contents(name)
                .map_err(|source| CaptureError::Io {
                    path: path.clone(),
                    source,
                })?;
            let after = directory
                .symlink_metadata(name)
                .map_err(|source| CaptureError::Io {
                    path: path.clone(),
                    source,
                })?;
            if same_fingerprint(&before, &after) && after.file_type().is_symlink() {
                self.entries.push(ManifestEntry {
                    path: path.clone(),
                    node: ManifestNode::Symlink {
                        target: NativeString::from_host(target.as_os_str()),
                        windows_link_kind: None,
                        windows_substitute_name: None,
                        windows_reparse_flags: None,
                    },
                    safety: SafetyObservations::default(),
                });
                self.statistics.symlinks += 1;
                return Ok(());
            }
        }
        self.degrade_or_fail(path.clone(), OmissionReason::Unstable)
    }

    fn degrade_or_fail(
        &mut self,
        path: NativeRelativePath,
        reason: OmissionReason,
    ) -> Result<(), CaptureError> {
        if !self.options.allow_degraded {
            return Err(CaptureError::Incomplete { path, reason });
        }
        self.omissions.push(Omission { path, reason });
        Ok(())
    }
}

fn read_sorted_names(
    directory: &Dir,
    path: &NativeRelativePath,
) -> Result<Vec<OsString>, CaptureError> {
    let entries = directory.entries().map_err(|source| CaptureError::Io {
        path: path.clone(),
        source,
    })?;
    let mut names = entries
        .map(|entry| {
            entry
                .map(|value| value.file_name())
                .map_err(|source| CaptureError::Io {
                    path: path.clone(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.sort_by(|left, right| {
        NativeString::from_host(left)
            .bytes()
            .cmp(NativeString::from_host(right).bytes())
    });
    Ok(names)
}

fn observed_kind(file_type: FileType) -> ObservedKind {
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FileIdentity {
    first: u64,
    second: u64,
}

fn stable_file(
    before: &Metadata,
    opened: &Metadata,
    after_open: &Metadata,
    after_path: &Metadata,
) -> bool {
    before.is_file()
        && opened.is_file()
        && after_open.is_file()
        && after_path.is_file()
        && same_identity(before, opened)
        && same_identity(before, after_open)
        && same_identity(before, after_path)
        && same_fingerprint(before, opened)
        && same_fingerprint(before, after_open)
        && same_fingerprint(before, after_path)
}

fn same_fingerprint(left: &Metadata, right: &Metadata) -> bool {
    left.len() == right.len()
        && left.file_type() == right.file_type()
        && modified(left) == modified(right)
        && change_time(left) == change_time(right)
        && execute_bits(left) == execute_bits(right)
}

fn modified(metadata: &Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
}

#[cfg(unix)]
fn same_identity(left: &Metadata, right: &Metadata) -> bool {
    use cap_std::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_identity(left: &Metadata, right: &Metadata) -> bool {
    same_fingerprint(left, right)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn change_time(metadata: &Metadata) -> Option<(i64, i64)> {
    use cap_std::fs::MetadataExt as _;
    Some((metadata.ctime(), metadata.ctime_nsec()))
}

#[cfg(not(unix))]
fn change_time(_metadata: &Metadata) -> Option<(i64, i64)> {
    None
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn execute_bits(metadata: &Metadata) -> Option<u8> {
    use cap_std::fs::MetadataExt as _;
    let mode = metadata.mode();
    Some(
        u8::from(mode & 0o100 != 0) << 2
            | u8::from(mode & 0o010 != 0) << 1
            | u8::from(mode & 0o001 != 0),
    )
}

#[cfg(not(unix))]
fn execute_bits(_metadata: &Metadata) -> Option<u8> {
    None
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn device_id(metadata: &Metadata) -> Option<u64> {
    use cap_std::fs::MetadataExt as _;
    Some(metadata.dev())
}

#[cfg(not(unix))]
fn device_id(_metadata: &Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn hardlink_identity(metadata: &Metadata) -> Option<FileIdentity> {
    use cap_std::fs::MetadataExt as _;
    (metadata.nlink() > 1).then_some(FileIdentity {
        first: metadata.dev(),
        second: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn hardlink_identity(_metadata: &Metadata) -> Option<FileIdentity> {
    None
}

#[cfg(unix)]
fn link_count(metadata: &Metadata) -> u64 {
    use cap_std::fs::MetadataExt as _;
    metadata.nlink()
}

#[cfg(not(unix))]
fn link_count(_metadata: &Metadata) -> u64 {
    1
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("failed to open or inspect capture root: {source}")]
    Root { source: io::Error },
    #[error("I/O error at {path:?}: {source}")]
    Io {
        path: NativeRelativePath,
        source: io::Error,
    },
    #[error("namespace remained unstable at {path:?}")]
    UnstableNamespace { path: NativeRelativePath },
    #[error("capture cannot be complete at {path:?}: {reason:?}")]
    Incomplete {
        path: NativeRelativePath,
        reason: OmissionReason,
    },
    #[error("file {path:?} has {size} bytes, above limit {maximum}")]
    FileLimit {
        path: NativeRelativePath,
        size: u64,
        maximum: u64,
    },
    #[error("capture exceeds the file-count limit {maximum}")]
    FileCountLimit { maximum: u64 },
    #[error("capture has {size} bytes, above total limit {maximum}")]
    TotalLimit { size: u64, maximum: u64 },
    #[error("capture file count overflow")]
    CountOverflow,
    #[error("capture byte count overflow")]
    ByteCountOverflow,
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Scope(#[from] ScopeError),
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn captures_regular_symlink_and_empty_directory() {
        let worktree = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        fs::create_dir(worktree.path().join("empty")).unwrap();
        fs::write(worktree.path().join("file"), b"raw bytes").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("file", worktree.path().join("link")).unwrap();

        let store = ObjectStore::open(store_root.path()).unwrap();
        let result = CaptureEngine::new(&store, CaptureOptions::default())
            .capture(worktree.path(), &IncludeAll)
            .unwrap();
        assert_eq!(
            result.manifest.coverage().completeness,
            Completeness::Complete
        );
        assert!(
            result
                .manifest
                .entries()
                .iter()
                .any(|entry| matches!(entry.node, ManifestNode::Regular { .. }))
        );
        assert!(
            result
                .manifest
                .entries()
                .iter()
                .any(|entry| matches!(entry.node, ManifestNode::EmptyDirectory))
        );
        #[cfg(unix)]
        assert!(
            result
                .manifest
                .entries()
                .iter()
                .any(|entry| matches!(entry.node, ManifestNode::Symlink { .. }))
        );
    }

    #[test]
    fn policy_exclusion_is_not_a_degraded_omission() {
        struct ExcludeEverything;
        impl ScopeClassifier for ExcludeEverything {
            fn classify(
                &self,
                _path: &NativeRelativePath,
                _kind: ObservedKind,
            ) -> Result<ScopeDecision, ScopeError> {
                Ok(ScopeDecision::Exclude)
            }
        }

        let worktree = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        fs::write(worktree.path().join("ignored"), b"ignored").unwrap();
        let store = ObjectStore::open(store_root.path()).unwrap();
        let result = CaptureEngine::new(&store, CaptureOptions::default())
            .capture(worktree.path(), &ExcludeEverything)
            .unwrap();
        assert_eq!(
            result.manifest.coverage().completeness,
            Completeness::Complete
        );
        assert!(result.manifest.entries().is_empty());
    }
}
