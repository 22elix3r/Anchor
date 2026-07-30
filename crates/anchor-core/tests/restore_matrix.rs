use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;

use anchor_core::{
    Completeness, ConflictReason, Coverage, Manifest, ManifestEntry, ManifestNode,
    MetadataObservation, NativeRelativePath, NativeString, NoChangeReason, ObjectId, PathEncoding,
    RestoreOutcome, RestorePlan, SafetyObservations,
};

#[derive(Clone, Debug)]
enum State {
    Absent,
    File(&'static [u8], u8),
    Symlink(&'static str),
    EmptyDirectory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Expected {
    WriteAbsent,
    WriteFile(&'static [u8], u8),
    WriteSymlink(&'static str),
    WriteEmptyDirectory,
    NoChange(NoChangeReason),
    Conflict(ConflictReason),
}

struct Case {
    id: &'static str,
    base: State,
    session: State,
    current: State,
    expected: Expected,
}

#[test]
#[allow(clippy::too_many_lines)]
fn base_session_current_decision_matrix() {
    let mut cases = vec![
        case(
            "regular_modified_unchanged_after",
            file(b"base", 0),
            file(b"session", 0),
            file(b"session", 0),
            Expected::WriteFile(b"base", 0),
        ),
        case(
            "regular_modified_already_restored",
            file(b"base", 0),
            file(b"session", 0),
            file(b"base", 0),
            Expected::NoChange(NoChangeReason::AlreadyRestored),
        ),
        case(
            "regular_no_session_change_preserves_current",
            file(b"base", 0),
            file(b"base", 0),
            file(b"later", 0),
            Expected::NoChange(NoChangeReason::NoSessionChange),
        ),
        case(
            "regular_post_session_content_conflicts",
            file(b"base", 0),
            file(b"session", 0),
            file(b"later", 0),
            Expected::Conflict(ConflictReason::OpaqueContentDrifted),
        ),
        case(
            "regular_added_unchanged_after",
            State::Absent,
            file(b"session", 0),
            file(b"session", 0),
            Expected::WriteAbsent,
        ),
        case(
            "regular_added_then_modified",
            State::Absent,
            file(b"session", 0),
            file(b"later", 0),
            Expected::Conflict(ConflictReason::SessionAdditionDrifted),
        ),
        case(
            "regular_added_then_deleted",
            State::Absent,
            file(b"session", 0),
            State::Absent,
            Expected::NoChange(NoChangeReason::AlreadyRestored),
        ),
        case(
            "regular_deleted_still_absent",
            file(b"base", 0),
            State::Absent,
            State::Absent,
            Expected::WriteFile(b"base", 0),
        ),
        case(
            "regular_deleted_then_recreated",
            file(b"base", 0),
            State::Absent,
            file(b"later", 0),
            Expected::Conflict(ConflictReason::SessionDeletionRecreated),
        ),
        case(
            "regular_deleted_then_recreated_as_base",
            file(b"base", 0),
            State::Absent,
            file(b"base", 0),
            Expected::NoChange(NoChangeReason::AlreadyRestored),
        ),
        case(
            "regular_mode_only",
            file(b"same", 0),
            file(b"same", 1),
            file(b"same", 1),
            Expected::WriteFile(b"same", 0),
        ),
        case(
            "regular_content_and_mode",
            file(b"base", 0),
            file(b"session", 1),
            file(b"session", 1),
            Expected::WriteFile(b"base", 0),
        ),
        case(
            "regular_post_content_with_session_mode",
            file(b"same", 0),
            file(b"same", 1),
            file(b"later", 1),
            Expected::WriteFile(b"later", 0),
        ),
        case(
            "regular_binary_is_exact_when_endpoint_matches",
            file(b"\0base\xff", 0),
            file(b"\0session\xfe", 0),
            file(b"\0session\xfe", 0),
            Expected::WriteFile(b"\0base\xff", 0),
        ),
        case(
            "regular_binary_third_state_conflicts",
            file(b"\0base\xff", 0),
            file(b"\0session\xfe", 0),
            file(b"\0later\xfd", 0),
            Expected::Conflict(ConflictReason::OpaqueContentDrifted),
        ),
        case(
            "empty_file_modified",
            file(b"", 0),
            file(b"session", 0),
            file(b"session", 0),
            Expected::WriteFile(b"", 0),
        ),
        case(
            "missing_final_newline_is_byte_exact",
            file(b"base", 0),
            file(b"session\n", 0),
            file(b"session\n", 0),
            Expected::WriteFile(b"base", 0),
        ),
        case(
            "symlink_added",
            State::Absent,
            symlink("target"),
            symlink("target"),
            Expected::WriteAbsent,
        ),
        case(
            "symlink_deleted",
            symlink("target"),
            State::Absent,
            State::Absent,
            Expected::WriteSymlink("target"),
        ),
        case(
            "symlink_target_changed",
            symlink("base-target"),
            symlink("session-target"),
            symlink("session-target"),
            Expected::WriteSymlink("base-target"),
        ),
        case(
            "symlink_target_changed_again",
            symlink("base-target"),
            symlink("session-target"),
            symlink("later-target"),
            Expected::Conflict(ConflictReason::SymlinkTargetDrifted),
        ),
        case(
            "symlink_replaced_by_file_after_session",
            symlink("base-target"),
            symlink("session-target"),
            file(b"later", 0),
            Expected::Conflict(ConflictReason::TypeDrifted),
        ),
        case(
            "symlink_absolute_target_is_opaque",
            symlink("/base"),
            symlink("/session"),
            symlink("/session"),
            Expected::WriteSymlink("/base"),
        ),
        case(
            "empty_directory_added",
            State::Absent,
            State::EmptyDirectory,
            State::EmptyDirectory,
            Expected::WriteAbsent,
        ),
        case(
            "empty_directory_deleted",
            State::EmptyDirectory,
            State::Absent,
            State::Absent,
            Expected::WriteEmptyDirectory,
        ),
        case(
            "empty_directory_recreated_as_file",
            State::EmptyDirectory,
            State::Absent,
            file(b"later", 0),
            Expected::Conflict(ConflictReason::SessionDeletionRecreated),
        ),
        case(
            "file_to_empty_directory_type_change",
            file(b"base", 0),
            State::EmptyDirectory,
            State::EmptyDirectory,
            Expected::WriteFile(b"base", 0),
        ),
        case(
            "type_changed_again_after_session",
            file(b"base", 0),
            State::EmptyDirectory,
            symlink("later"),
            Expected::Conflict(ConflictReason::TypeDrifted),
        ),
        case(
            "post_session_deletion_supersedes_file_change",
            file(b"base", 0),
            file(b"session", 0),
            State::Absent,
            Expected::NoChange(NoChangeReason::PostSessionDeletionSuperseded),
        ),
    ];

    // Unix executable bits have more than two states. Windows readonly metadata
    // is boolean, so it cannot represent an opaque third metadata state.
    #[cfg(unix)]
    cases.push(case(
        "regular_third_mode_conflicts",
        file(b"same", 0),
        file(b"same", 1),
        file(b"same", 2),
        Expected::Conflict(ConflictReason::ModeDrifted),
    ));

    assert_eq!(
        cases.len(),
        if cfg!(unix) { 30 } else { 29 },
        "update the declared matrix size"
    );
    let mut seen = BTreeSet::new();
    for case in cases {
        assert!(seen.insert(case.id), "duplicate matrix case {}", case.id);
        let outcome = calculate(&case.base, &case.session, &case.current);
        assert_outcome(case.id, outcome, case.expected);
    }
}

fn case(id: &'static str, base: State, session: State, current: State, expected: Expected) -> Case {
    Case {
        id,
        base,
        session,
        current,
        expected,
    }
}

fn file(bytes: &'static [u8], mode: u8) -> State {
    State::File(bytes, mode)
}

fn symlink(target: &'static str) -> State {
    State::Symlink(target)
}

fn calculate(base: &State, session: &State, current: &State) -> RestoreOutcome {
    let base = manifest(base);
    let session = manifest(session);
    let current = manifest(current);
    let selected = BTreeSet::from([path()]);
    let plan = RestorePlan::calculate(&base, &session, &current, &selected).unwrap();
    assert_eq!(plan.outcomes.len(), 1);
    plan.outcomes.into_iter().next().unwrap().outcome
}

fn manifest(state: &State) -> Manifest {
    let entries = match entry(state) {
        Some(entry) => vec![entry],
        None => Vec::new(),
    };
    Manifest::new(
        PathEncoding::host(),
        entries,
        Coverage {
            completeness: Completeness::Complete,
            omissions: Vec::new(),
        },
    )
    .unwrap()
}

fn entry(state: &State) -> Option<ManifestEntry> {
    let (node, safety) = match state {
        State::Absent => return None,
        State::File(bytes, mode) => {
            let (unix_exec_bits, windows_readonly) = platform_metadata(*mode);
            (
                ManifestNode::Regular {
                    object: ObjectId::from_raw(bytes),
                    raw_size: u64::try_from(bytes.len()).unwrap(),
                    unix_exec_bits,
                    windows_readonly,
                },
                SafetyObservations {
                    hardlink_group: None,
                    link_count: 1,
                    extended_metadata: MetadataObservation::Absent,
                },
            )
        }
        State::Symlink(target) => (
            ManifestNode::Symlink {
                target: NativeString::from_host(OsStr::new(target)),
                windows_link_kind: None,
                windows_substitute_name: None,
                windows_reparse_flags: None,
            },
            SafetyObservations::default(),
        ),
        State::EmptyDirectory => (
            ManifestNode::EmptyDirectory,
            SafetyObservations {
                extended_metadata: MetadataObservation::Absent,
                ..SafetyObservations::default()
            },
        ),
    };
    Some(ManifestEntry {
        path: path(),
        node,
        safety,
    })
}

fn path() -> NativeRelativePath {
    NativeRelativePath::from_host_path(Path::new("node")).unwrap()
}

fn assert_outcome(id: &str, actual: RestoreOutcome, expected: Expected) {
    match (actual, expected) {
        (RestoreOutcome::Write(None), Expected::WriteAbsent) => {}
        (
            RestoreOutcome::Write(Some(entry)),
            Expected::WriteFile(expected_bytes, expected_mode),
        ) => {
            let ManifestNode::Regular {
                object,
                raw_size,
                unix_exec_bits: actual_unix_exec_bits,
                windows_readonly: actual_windows_readonly,
                ..
            } = entry.node
            else {
                panic!("{id}: expected regular write");
            };
            assert_eq!(object, ObjectId::from_raw(expected_bytes), "{id}");
            assert_eq!(raw_size, expected_bytes.len() as u64, "{id}");
            let (expected_unix_exec_bits, expected_windows_readonly) =
                platform_metadata(expected_mode);
            assert_eq!(actual_unix_exec_bits, expected_unix_exec_bits, "{id}");
            assert_eq!(actual_windows_readonly, expected_windows_readonly, "{id}");
        }
        (RestoreOutcome::Write(Some(entry)), Expected::WriteSymlink(expected_target)) => {
            let ManifestNode::Symlink { target, .. } = entry.node else {
                panic!("{id}: expected symlink write");
            };
            assert_eq!(
                target,
                NativeString::from_host(OsStr::new(expected_target)),
                "{id}"
            );
        }
        (RestoreOutcome::Write(Some(entry)), Expected::WriteEmptyDirectory) => {
            assert!(
                matches!(entry.node, ManifestNode::EmptyDirectory),
                "{id}: expected empty-directory write"
            );
        }
        (RestoreOutcome::NoChange(actual), Expected::NoChange(expected)) => {
            assert_eq!(actual, expected, "{id}");
        }
        (RestoreOutcome::Conflict(actual), Expected::Conflict(expected)) => {
            assert_eq!(actual.reason, expected, "{id}");
        }
        (actual, expected) => panic!("{id}: got {actual:?}, expected {expected:?}"),
    }
}

#[cfg(unix)]
const fn platform_metadata(mode: u8) -> (Option<u8>, Option<bool>) {
    (Some(mode), None)
}

#[cfg(windows)]
const fn platform_metadata(mode: u8) -> (Option<u8>, Option<bool>) {
    (None, Some(mode != 0))
}
