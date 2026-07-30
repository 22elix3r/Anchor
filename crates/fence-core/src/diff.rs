use std::collections::{BTreeMap, BTreeSet};

use crate::{Manifest, ManifestEntry, ManifestNode, NativeRelativePath, NativeString, ObjectId};

/// Path-level classification derived from two immutable manifests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    TypeChanged,
    ModeChanged,
    SymlinkTargetChanged,
    Renamed,
}

/// One deterministic path-level change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestChange {
    pub kind: ChangeKind,
    pub path: NativeRelativePath,
    pub previous_path: Option<NativeRelativePath>,
    pub before: Option<ManifestEntry>,
    pub after: Option<ManifestEntry>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ManifestDiff {
    pub changes: Vec<ManifestChange>,
}

impl ManifestDiff {
    #[must_use]
    pub fn between(before: &Manifest, after: &Manifest) -> Self {
        let before_by_path = before
            .entries()
            .iter()
            .map(|entry| (&entry.path, entry))
            .collect::<BTreeMap<_, _>>();
        let after_by_path = after
            .entries()
            .iter()
            .map(|entry| (&entry.path, entry))
            .collect::<BTreeMap<_, _>>();

        let removed = before_by_path
            .keys()
            .filter(|path| !after_by_path.contains_key(*path))
            .copied()
            .collect::<Vec<_>>();
        let added = after_by_path
            .keys()
            .filter(|path| !before_by_path.contains_key(*path))
            .copied()
            .collect::<Vec<_>>();

        let mut removed_by_fingerprint: BTreeMap<NodeFingerprint, Vec<&NativeRelativePath>> =
            BTreeMap::new();
        let mut added_by_fingerprint: BTreeMap<NodeFingerprint, Vec<&NativeRelativePath>> =
            BTreeMap::new();
        for path in &removed {
            removed_by_fingerprint
                .entry(NodeFingerprint::from(&before_by_path[*path].node))
                .or_default()
                .push(path);
        }
        for path in &added {
            added_by_fingerprint
                .entry(NodeFingerprint::from(&after_by_path[*path].node))
                .or_default()
                .push(path);
        }

        let mut renamed_from = BTreeSet::new();
        let mut renamed_to = BTreeSet::new();
        let mut changes = Vec::new();
        for (fingerprint, sources) in &removed_by_fingerprint {
            let Some(destinations) = added_by_fingerprint.get(fingerprint) else {
                continue;
            };
            if sources.len() == 1 && destinations.len() == 1 {
                let source = sources[0];
                let destination = destinations[0];
                renamed_from.insert(source);
                renamed_to.insert(destination);
                changes.push(ManifestChange {
                    kind: ChangeKind::Renamed,
                    path: destination.clone(),
                    previous_path: Some(source.clone()),
                    before: Some(before_by_path[source].clone()),
                    after: Some(after_by_path[destination].clone()),
                });
            }
        }

        for path in removed {
            if !renamed_from.contains(path) {
                changes.push(ManifestChange {
                    kind: ChangeKind::Removed,
                    path: path.clone(),
                    previous_path: None,
                    before: Some(before_by_path[path].clone()),
                    after: None,
                });
            }
        }
        for path in added {
            if !renamed_to.contains(path) {
                changes.push(ManifestChange {
                    kind: ChangeKind::Added,
                    path: path.clone(),
                    previous_path: None,
                    before: None,
                    after: Some(after_by_path[path].clone()),
                });
            }
        }
        for (path, before_entry) in &before_by_path {
            let Some(after_entry) = after_by_path.get(path) else {
                continue;
            };
            if before_entry.node != after_entry.node {
                changes.push(ManifestChange {
                    kind: classify_change(&before_entry.node, &after_entry.node),
                    path: (*path).clone(),
                    previous_path: None,
                    before: Some((*before_entry).clone()),
                    after: Some((*after_entry).clone()),
                });
            }
        }
        changes.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| change_order(left.kind).cmp(&change_order(right.kind)))
        });
        Self { changes }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

fn classify_change(before: &ManifestNode, after: &ManifestNode) -> ChangeKind {
    match (before, after) {
        (
            ManifestNode::Regular {
                object: before_object,
                unix_exec_bits: before_mode,
                windows_readonly: before_readonly,
                ..
            },
            ManifestNode::Regular {
                object: after_object,
                unix_exec_bits: after_mode,
                windows_readonly: after_readonly,
                ..
            },
        ) if before_object == after_object
            && (before_mode != after_mode || before_readonly != after_readonly) =>
        {
            ChangeKind::ModeChanged
        }
        (ManifestNode::Regular { .. }, ManifestNode::Regular { .. }) => ChangeKind::Modified,
        (ManifestNode::Symlink { .. }, ManifestNode::Symlink { .. }) => {
            ChangeKind::SymlinkTargetChanged
        }
        _ => ChangeKind::TypeChanged,
    }
}

fn change_order(kind: ChangeKind) -> u8 {
    match kind {
        ChangeKind::Added => 1,
        ChangeKind::Removed => 2,
        ChangeKind::Modified => 3,
        ChangeKind::TypeChanged => 4,
        ChangeKind::ModeChanged => 5,
        ChangeKind::SymlinkTargetChanged => 6,
        ChangeKind::Renamed => 7,
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum NodeFingerprint {
    Regular(ObjectId, Option<u8>, Option<bool>),
    Symlink(
        NativeString,
        Option<crate::WindowsSymlinkKind>,
        Option<NativeString>,
        Option<u32>,
    ),
    EmptyDirectory,
}

impl From<&ManifestNode> for NodeFingerprint {
    fn from(node: &ManifestNode) -> Self {
        match node {
            ManifestNode::Regular {
                object,
                unix_exec_bits,
                windows_readonly,
                ..
            } => Self::Regular(*object, *unix_exec_bits, *windows_readonly),
            ManifestNode::Symlink {
                target,
                windows_link_kind,
                windows_substitute_name,
                windows_reparse_flags,
            } => Self::Symlink(
                target.clone(),
                *windows_link_kind,
                windows_substitute_name.clone(),
                *windows_reparse_flags,
            ),
            ManifestNode::EmptyDirectory => Self::EmptyDirectory,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Completeness, Coverage, ManifestEntry, PathEncoding, SafetyObservations};

    use super::*;

    fn path(name: &[u8]) -> NativeRelativePath {
        NativeRelativePath::new(PathEncoding::UnixBytes, vec![name.to_vec()]).unwrap()
    }

    fn file(name: &[u8], bytes: &[u8], mode: u8) -> ManifestEntry {
        ManifestEntry {
            path: path(name),
            node: ManifestNode::Regular {
                object: ObjectId::from_raw(bytes),
                raw_size: u64::try_from(bytes.len()).unwrap(),
                unix_exec_bits: Some(mode),
                windows_readonly: None,
            },
            safety: SafetyObservations::default(),
        }
    }

    fn manifest(entries: Vec<ManifestEntry>) -> Manifest {
        Manifest::new(
            PathEncoding::UnixBytes,
            entries,
            Coverage {
                completeness: Completeness::Complete,
                omissions: Vec::new(),
            },
        )
        .unwrap()
    }

    #[test]
    fn detects_unique_exact_rename() {
        let before = manifest(vec![file(b"old", b"same", 0)]);
        let after = manifest(vec![file(b"new", b"same", 0)]);
        let diff = ManifestDiff::between(&before, &after);
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].kind, ChangeKind::Renamed);
        assert_eq!(diff.changes[0].previous_path, Some(path(b"old")));
        assert_eq!(diff.changes[0].path, path(b"new"));
    }

    #[test]
    fn duplicate_content_is_not_guessed_as_a_rename() {
        let before = manifest(vec![file(b"a", b"same", 0), file(b"b", b"same", 0)]);
        let after = manifest(vec![file(b"c", b"same", 0), file(b"d", b"same", 0)]);
        let diff = ManifestDiff::between(&before, &after);
        assert_eq!(
            diff.changes
                .iter()
                .filter(|change| change.kind == ChangeKind::Renamed)
                .count(),
            0
        );
    }

    #[test]
    fn separates_mode_from_content_changes() {
        let before = manifest(vec![file(b"file", b"same", 0)]);
        let after = manifest(vec![file(b"file", b"same", 1)]);
        assert_eq!(
            ManifestDiff::between(&before, &after).changes[0].kind,
            ChangeKind::ModeChanged
        );
    }
}
