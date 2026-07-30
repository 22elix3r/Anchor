use std::path::Path;

use anchor_windows::{
    DirectoryEntry, DirectoryHandle, NodeHandle, NodeKind, ReparseKind, RootHandle, WindowsError,
};

use super::{
    CaptureEngine, CaptureError, CaptureResult, CaptureStatistics, Completeness, Coverage,
    FILE_STABILITY_RETRIES, Manifest, ManifestEntry, ManifestNode, MetadataObservation,
    NAMESPACE_RETRIES, NativeRelativePath, NativeString, ObjectStore, ObservedKind, Omission,
    OmissionReason, PathEncoding, SafetyObservations, ScopeClassifier, ScopeDecision,
};
use crate::WindowsSymlinkKind;

pub(super) fn capture(
    engine: &CaptureEngine<'_>,
    root: &Path,
    classifier: &impl ScopeClassifier,
) -> Result<CaptureResult, CaptureError> {
    let mut last_namespace_error = None;
    for _ in 0..NAMESPACE_RETRIES {
        let root = RootHandle::open(root)?;
        let mut attempt = WindowsCapture {
            store: engine.store,
            engine,
            classifier,
            entries: Vec::new(),
            omissions: Vec::new(),
            statistics: CaptureStatistics::default(),
        };
        let root_path = NativeRelativePath::new(PathEncoding::WindowsWtf16Le, Vec::new())?;
        match attempt.capture_directory(root.directory(), &root_path, true) {
            Ok(()) => {
                let completeness = if attempt.omissions.is_empty() {
                    Completeness::Complete
                } else {
                    Completeness::Degraded
                };
                let manifest = Manifest::new(
                    PathEncoding::WindowsWtf16Le,
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
            path: NativeRelativePath::new(PathEncoding::WindowsWtf16Le, Vec::new())?,
        }),
    )
}

struct WindowsCapture<'a, C> {
    store: &'a ObjectStore,
    engine: &'a CaptureEngine<'a>,
    classifier: &'a C,
    entries: Vec<ManifestEntry>,
    omissions: Vec<Omission>,
    statistics: CaptureStatistics,
}

impl<C: ScopeClassifier> WindowsCapture<'_, C> {
    fn capture_directory(
        &mut self,
        directory: &DirectoryHandle,
        relative: &NativeRelativePath,
        is_root: bool,
    ) -> Result<(), CaptureError> {
        if directory.is_case_sensitive()? {
            self.degrade_or_fail(relative.clone(), OmissionReason::CaseSemanticsUnknown)?;
            return Ok(());
        }
        let first = sorted_entries(directory, relative)?;
        if first.is_empty() && !is_root {
            self.entries.push(ManifestEntry {
                path: relative.clone(),
                node: ManifestNode::EmptyDirectory,
                safety: SafetyObservations {
                    extended_metadata: MetadataObservation::Unavailable,
                    ..SafetyObservations::default()
                },
            });
            self.statistics.empty_directories = self
                .statistics
                .empty_directories
                .checked_add(1)
                .ok_or(CaptureError::CountOverflow)?;
        }

        for entry in &first {
            let child_path = relative.join_host_component(&entry.name)?;
            let observed = observed_kind(entry);
            match self.classifier.classify(&child_path, observed)? {
                ScopeDecision::Exclude => {
                    self.statistics.excluded_nodes = self
                        .statistics
                        .excluded_nodes
                        .checked_add(1)
                        .ok_or(CaptureError::CountOverflow)?;
                }
                ScopeDecision::Boundary(reason) => {
                    self.degrade_or_fail(child_path, reason)?;
                }
                ScopeDecision::Include => {
                    let child = directory
                        .open_child(entry)
                        .map_err(|error| map_namespace_error(child_path.clone(), error))?;
                    match child.metadata().kind {
                        NodeKind::RegularFile => self.capture_regular(&child, &child_path)?,
                        NodeKind::Directory => {
                            let child = child.into_directory()?;
                            self.capture_directory(&child, &child_path, false)?;
                        }
                        NodeKind::ReparsePoint => self.capture_reparse(&child, &child_path)?,
                    }
                }
            }
        }

        if first != sorted_entries(directory, relative)? {
            return Err(CaptureError::UnstableNamespace {
                path: relative.clone(),
            });
        }
        Ok(())
    }

    fn capture_regular(
        &mut self,
        node: &NodeHandle,
        path: &NativeRelativePath,
    ) -> Result<(), CaptureError> {
        let initial = node.metadata();
        if initial.link_count > 1 {
            return self.degrade_or_fail(path.clone(), OmissionReason::HardlinkTopology);
        }
        if initial.is_efs_encrypted() {
            return self.degrade_or_fail(path.clone(), OmissionReason::EncryptedFile);
        }
        if initial.may_recall_data() {
            return self.degrade_or_fail(path.clone(), OmissionReason::CloudPlaceholder);
        }
        if node
            .streams()?
            .iter()
            .any(|stream| !stream.is_default_data_stream())
        {
            return self.degrade_or_fail(path.clone(), OmissionReason::AlternateDataStream);
        }
        self.check_limits_before_read(path, initial.size)?;

        for _ in 0..FILE_STABILITY_RETRIES {
            let before = node.refresh_metadata()?;
            if before.kind != NodeKind::RegularFile || before.identity != initial.identity {
                continue;
            }
            let mut file = node.try_clone_file()?;
            let (object, raw_size) = self.store.put(&mut file)?;
            let after = node.refresh_metadata()?;
            if before == after && raw_size == before.size && node.verify_path_identity().is_ok() {
                self.entries.push(ManifestEntry {
                    path: path.clone(),
                    node: ManifestNode::Regular {
                        object,
                        raw_size,
                        unix_exec_bits: None,
                        windows_readonly: Some(before.is_readonly()),
                    },
                    safety: SafetyObservations {
                        hardlink_group: None,
                        link_count: u64::from(before.link_count),
                        extended_metadata: MetadataObservation::Unavailable,
                    },
                });
                self.statistics.regular_files = self
                    .statistics
                    .regular_files
                    .checked_add(1)
                    .ok_or(CaptureError::CountOverflow)?;
                self.statistics.raw_bytes = self
                    .statistics
                    .raw_bytes
                    .checked_add(raw_size)
                    .ok_or(CaptureError::ByteCountOverflow)?;
                return Ok(());
            }
        }
        self.degrade_or_fail(path.clone(), OmissionReason::Unstable)
    }

    fn capture_reparse(
        &mut self,
        node: &NodeHandle,
        path: &NativeRelativePath,
    ) -> Result<(), CaptureError> {
        let before = node.metadata();
        let Some(kind) = node.reparse_kind()? else {
            return Err(CaptureError::UnstableNamespace { path: path.clone() });
        };
        let ReparseKind::SymbolicLink(link) = kind else {
            return self.degrade_or_fail(path.clone(), OmissionReason::UnsupportedReparsePoint);
        };
        if link.flags > 1 {
            return self.degrade_or_fail(path.clone(), OmissionReason::UnsupportedReparsePoint);
        }
        let after = node.refresh_metadata()?;
        if before != after || node.verify_path_identity().is_err() {
            return self.degrade_or_fail(path.clone(), OmissionReason::Unstable);
        }
        self.entries.push(ManifestEntry {
            path: path.clone(),
            node: ManifestNode::Symlink {
                target: NativeString::from_host(&link.print_name),
                windows_link_kind: Some(if before.has_directory_attribute() {
                    WindowsSymlinkKind::Directory
                } else {
                    WindowsSymlinkKind::File
                }),
                windows_substitute_name: Some(NativeString::from_host(&link.substitute_name)),
                windows_reparse_flags: Some(link.flags),
            },
            safety: SafetyObservations {
                extended_metadata: MetadataObservation::Unavailable,
                ..SafetyObservations::default()
            },
        });
        self.statistics.symlinks = self
            .statistics
            .symlinks
            .checked_add(1)
            .ok_or(CaptureError::CountOverflow)?;
        Ok(())
    }

    fn check_limits_before_read(
        &self,
        path: &NativeRelativePath,
        raw_size: u64,
    ) -> Result<(), CaptureError> {
        if raw_size > self.engine.options.limits.max_file_bytes {
            return Err(CaptureError::FileLimit {
                path: path.clone(),
                size: raw_size,
                maximum: self.engine.options.limits.max_file_bytes,
            });
        }
        let files = self
            .statistics
            .regular_files
            .checked_add(1)
            .ok_or(CaptureError::CountOverflow)?;
        if files > self.engine.options.limits.max_files {
            return Err(CaptureError::FileCountLimit {
                maximum: self.engine.options.limits.max_files,
            });
        }
        let total = self
            .statistics
            .raw_bytes
            .checked_add(raw_size)
            .ok_or(CaptureError::ByteCountOverflow)?;
        if total > self.engine.options.limits.max_total_bytes {
            return Err(CaptureError::TotalLimit {
                size: total,
                maximum: self.engine.options.limits.max_total_bytes,
            });
        }
        Ok(())
    }

    fn degrade_or_fail(
        &mut self,
        path: NativeRelativePath,
        reason: OmissionReason,
    ) -> Result<(), CaptureError> {
        if !self.engine.options.allow_degraded {
            return Err(CaptureError::Incomplete { path, reason });
        }
        self.omissions.push(Omission { path, reason });
        Ok(())
    }
}

fn sorted_entries(
    directory: &DirectoryHandle,
    path: &NativeRelativePath,
) -> Result<Vec<DirectoryEntry>, CaptureError> {
    let mut entries = directory
        .entries()
        .map_err(|error| map_namespace_error(path.clone(), error))?;
    entries.sort_by(|left, right| {
        NativeString::from_host(&left.name)
            .bytes()
            .cmp(NativeString::from_host(&right.name).bytes())
    });
    Ok(entries)
}

fn observed_kind(entry: &DirectoryEntry) -> ObservedKind {
    if entry.reparse_tag.is_some() {
        ObservedKind::Symlink
    } else if entry.attributes & 0x10 != 0 {
        ObservedKind::Directory
    } else {
        ObservedKind::Regular
    }
}

fn map_namespace_error(path: NativeRelativePath, error: WindowsError) -> CaptureError {
    if matches!(error, WindowsError::IdentityChanged) {
        CaptureError::UnstableNamespace { path }
    } else {
        CaptureError::Windows(error)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{CaptureLimits, CaptureOptions, IncludeAll};

    #[test]
    fn captures_readonly_regular_file() {
        let worktree = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let path = worktree.path().join("read-only.txt");
        fs::write(&path, b"windows bytes").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();

        let store = ObjectStore::open(store_root.path()).unwrap();
        let result = CaptureEngine::new(
            &store,
            CaptureOptions {
                limits: CaptureLimits::default(),
                allow_degraded: false,
                cross_mounts: false,
            },
        )
        .capture(worktree.path(), &IncludeAll)
        .unwrap();
        assert!(matches!(
            result.manifest.entries()[0].node,
            ManifestNode::Regular {
                windows_readonly: Some(true),
                ..
            }
        ));
    }

    #[test]
    fn refuses_named_data_stream() {
        let worktree = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        fs::write(worktree.path().join("file.txt"), b"default").unwrap();
        fs::write(worktree.path().join("file.txt:secret"), b"stream").unwrap();

        let store = ObjectStore::open(store_root.path()).unwrap();
        let error = CaptureEngine::new(&store, CaptureOptions::default())
            .capture(worktree.path(), &IncludeAll)
            .unwrap_err();
        assert!(matches!(
            error,
            CaptureError::Incomplete {
                reason: OmissionReason::AlternateDataStream,
                ..
            }
        ));
    }
}
