use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::{Completeness, Manifest, ManifestEntry, ManifestNode, NativeRelativePath};

/// A fully calculated, side-effect-free restoration decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestorePlan {
    pub outcomes: Vec<PathRestore>,
}

impl RestorePlan {
    /// Calculate an inverse session transformation over selected paths.
    ///
    /// An empty selection means all paths changed between `base` and `session`.
    ///
    /// # Errors
    ///
    /// Returns [`RestorePlanError`] if any input is degraded or uses another path encoding.
    pub fn calculate(
        base: &Manifest,
        session: &Manifest,
        current: &Manifest,
        selected: &BTreeSet<NativeRelativePath>,
    ) -> Result<Self, RestorePlanError> {
        for (name, manifest) in [("base", base), ("session", session), ("current", current)] {
            if manifest.coverage().completeness == Completeness::Degraded {
                return Err(RestorePlanError::DegradedInput(name));
            }
        }
        if base.path_encoding() != session.path_encoding()
            || base.path_encoding() != current.path_encoding()
        {
            return Err(RestorePlanError::MixedPathEncoding);
        }

        let base = entries(base);
        let session = entries(session);
        let current = entries(current);
        let mut paths = base
            .keys()
            .chain(session.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        if selected.is_empty() {
            paths.retain(|path| base.get(path) != session.get(path));
        } else {
            paths.retain(|path| selected.contains(path));
        }
        let outcomes = paths
            .into_iter()
            .map(|path| {
                let base_entry = base.get(&path).copied();
                let session_entry = session.get(&path).copied();
                let current_entry = current.get(&path).copied();
                PathRestore {
                    path,
                    outcome: decide(base_entry, session_entry, current_entry),
                }
            })
            .collect();
        Ok(Self { outcomes })
    }

    #[must_use]
    pub fn can_apply(&self) -> bool {
        self.outcomes
            .iter()
            .all(|item| !matches!(item.outcome, RestoreOutcome::Conflict(_)))
    }

    pub fn writes(&self) -> impl Iterator<Item = &PathRestore> {
        self.outcomes
            .iter()
            .filter(|item| matches!(item.outcome, RestoreOutcome::Write(_)))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathRestore {
    pub path: NativeRelativePath,
    pub outcome: RestoreOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestoreOutcome {
    /// Install this base-derived entry, or remove the path when `None`.
    Write(Option<ManifestEntry>),
    NoChange(NoChangeReason),
    Conflict(Box<RestoreConflict>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoChangeReason {
    NoSessionChange,
    AlreadyRestored,
    PostSessionDeletionSuperseded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreConflict {
    pub reason: ConflictReason,
    pub base: Option<ManifestEntry>,
    pub session: Option<ManifestEntry>,
    pub current: Option<ManifestEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictReason {
    SessionAdditionDrifted,
    SessionDeletionRecreated,
    OpaqueContentDrifted,
    SymlinkTargetDrifted,
    TypeDrifted,
    ModeDrifted,
    TextMergeOverlaps,
    TextMergeUnsupported,
    TextMergeTooLarge,
}

fn decide(
    base: Option<&ManifestEntry>,
    session: Option<&ManifestEntry>,
    current: Option<&ManifestEntry>,
) -> RestoreOutcome {
    if same_node(base, session) {
        return RestoreOutcome::NoChange(NoChangeReason::NoSessionChange);
    }
    if same_node(current, base) {
        return RestoreOutcome::NoChange(NoChangeReason::AlreadyRestored);
    }
    if same_node(current, session) {
        return RestoreOutcome::Write(base.cloned());
    }

    match (base, session, current) {
        (None, Some(session), Some(current)) => conflict(
            ConflictReason::SessionAdditionDrifted,
            None,
            Some(session),
            Some(current),
        ),
        (Some(base), None, None) => RestoreOutcome::Write(Some(base.clone())),
        (Some(base), None, Some(current)) => conflict(
            ConflictReason::SessionDeletionRecreated,
            Some(base),
            None,
            Some(current),
        ),
        (None | Some(_), Some(_), None) => {
            RestoreOutcome::NoChange(NoChangeReason::PostSessionDeletionSuperseded)
        }
        (Some(base), Some(session), Some(current)) => decide_present(base, session, current),
        (None, None, _) => RestoreOutcome::NoChange(NoChangeReason::NoSessionChange),
    }
}

fn decide_present(
    base: &ManifestEntry,
    session: &ManifestEntry,
    current: &ManifestEntry,
) -> RestoreOutcome {
    match (&base.node, &session.node, &current.node) {
        (
            ManifestNode::Regular {
                object: base_object,
                raw_size: base_size,
                unix_exec_bits: base_mode,
            },
            ManifestNode::Regular {
                object: session_object,
                unix_exec_bits: session_mode,
                ..
            },
            ManifestNode::Regular {
                object: current_object,
                raw_size: current_size,
                unix_exec_bits: current_mode,
            },
        ) => {
            let desired_mode = match invert_scalar(*base_mode, *session_mode, *current_mode) {
                ScalarDecision::Preserve(value) | ScalarDecision::Restore(value) => value,
                ScalarDecision::Conflict => {
                    return conflict(
                        ConflictReason::ModeDrifted,
                        Some(base),
                        Some(session),
                        Some(current),
                    );
                }
            };
            if base_object == session_object {
                if desired_mode == *current_mode {
                    return RestoreOutcome::NoChange(NoChangeReason::AlreadyRestored);
                }
                return RestoreOutcome::Write(Some(ManifestEntry {
                    path: current.path.clone(),
                    node: ManifestNode::Regular {
                        object: *current_object,
                        raw_size: *current_size,
                        unix_exec_bits: desired_mode,
                    },
                    safety: current.safety.clone(),
                }));
            }
            if current_object == session_object {
                return RestoreOutcome::Write(Some(ManifestEntry {
                    path: base.path.clone(),
                    node: ManifestNode::Regular {
                        object: *base_object,
                        raw_size: *base_size,
                        unix_exec_bits: desired_mode,
                    },
                    safety: base.safety.clone(),
                }));
            }
            conflict(
                ConflictReason::OpaqueContentDrifted,
                Some(base),
                Some(session),
                Some(current),
            )
        }
        (
            ManifestNode::Symlink { .. },
            ManifestNode::Symlink { .. },
            ManifestNode::Symlink { .. },
        ) => conflict(
            ConflictReason::SymlinkTargetDrifted,
            Some(base),
            Some(session),
            Some(current),
        ),
        _ => conflict(
            ConflictReason::TypeDrifted,
            Some(base),
            Some(session),
            Some(current),
        ),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScalarDecision<T> {
    Preserve(T),
    Restore(T),
    Conflict,
}

fn invert_scalar<T: Copy + Eq>(base: T, session: T, current: T) -> ScalarDecision<T> {
    if base == session {
        ScalarDecision::Preserve(current)
    } else if current == session {
        ScalarDecision::Restore(base)
    } else if current == base {
        ScalarDecision::Preserve(current)
    } else {
        ScalarDecision::Conflict
    }
}

fn same_node(left: Option<&ManifestEntry>, right: Option<&ManifestEntry>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.node == right.node,
        _ => false,
    }
}

fn entries(manifest: &Manifest) -> BTreeMap<NativeRelativePath, &ManifestEntry> {
    manifest
        .entries()
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect()
}

fn conflict(
    reason: ConflictReason,
    base: Option<&ManifestEntry>,
    session: Option<&ManifestEntry>,
    current: Option<&ManifestEntry>,
) -> RestoreOutcome {
    RestoreOutcome::Conflict(Box::new(RestoreConflict {
        reason,
        base: base.cloned(),
        session: session.cloned(),
        current: current.cloned(),
    }))
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RestorePlanError {
    #[error("{0} manifest is degraded and cannot be used for safe restoration")]
    DegradedInput(&'static str),
    #[error("restore manifests use different path encoding families")]
    MixedPathEncoding,
}

#[cfg(test)]
mod tests {
    use crate::{Coverage, ObjectId, PathEncoding, SafetyObservations};

    use super::*;

    fn path() -> NativeRelativePath {
        NativeRelativePath::new(PathEncoding::UnixBytes, vec![b"file".to_vec()]).unwrap()
    }

    fn file(bytes: &[u8], mode: u8) -> ManifestEntry {
        ManifestEntry {
            path: path(),
            node: ManifestNode::Regular {
                object: ObjectId::from_raw(bytes),
                raw_size: u64::try_from(bytes.len()).unwrap(),
                unix_exec_bits: Some(mode),
            },
            safety: SafetyObservations::default(),
        }
    }

    fn manifest(entry: Option<ManifestEntry>) -> Manifest {
        Manifest::new(
            PathEncoding::UnixBytes,
            entry.into_iter().collect(),
            Coverage {
                completeness: Completeness::Complete,
                omissions: Vec::new(),
            },
        )
        .unwrap()
    }

    fn outcome(
        base: Option<ManifestEntry>,
        session: Option<ManifestEntry>,
        current: Option<ManifestEntry>,
    ) -> RestoreOutcome {
        RestorePlan::calculate(
            &manifest(base),
            &manifest(session),
            &manifest(current),
            &BTreeSet::new(),
        )
        .unwrap()
        .outcomes
        .into_iter()
        .next()
        .unwrap()
        .outcome
    }

    #[test]
    fn restores_only_when_current_matches_session() {
        let base = file(b"before", 0);
        let session = file(b"session", 0);
        assert_eq!(
            outcome(Some(base.clone()), Some(session.clone()), Some(session)),
            RestoreOutcome::Write(Some(base))
        );
    }

    #[test]
    fn refuses_drifted_opaque_content() {
        let RestoreOutcome::Conflict(conflict) = outcome(
            Some(file(b"before", 0)),
            Some(file(b"session", 0)),
            Some(file(b"later", 0)),
        ) else {
            panic!("expected conflict");
        };
        assert_eq!(conflict.reason, ConflictReason::OpaqueContentDrifted);
    }

    #[test]
    fn never_deletes_a_drifted_session_addition() {
        let RestoreOutcome::Conflict(conflict) =
            outcome(None, Some(file(b"session", 0)), Some(file(b"later", 0)))
        else {
            panic!("expected conflict");
        };
        assert_eq!(conflict.reason, ConflictReason::SessionAdditionDrifted);
    }

    #[test]
    fn preserves_post_session_deletion() {
        assert_eq!(
            outcome(Some(file(b"before", 0)), Some(file(b"session", 0)), None),
            RestoreOutcome::NoChange(NoChangeReason::PostSessionDeletionSuperseded)
        );
    }

    #[test]
    fn inverts_mode_while_preserving_current_content() {
        let result = outcome(
            Some(file(b"same", 0)),
            Some(file(b"same", 1)),
            Some(file(b"later", 1)),
        );
        let RestoreOutcome::Write(Some(entry)) = result else {
            panic!("expected write");
        };
        let ManifestNode::Regular {
            object,
            unix_exec_bits,
            ..
        } = entry.node
        else {
            panic!("expected regular file");
        };
        assert_eq!(object, ObjectId::from_raw(b"later"));
        assert_eq!(unix_exec_bits, Some(0));
    }
}
