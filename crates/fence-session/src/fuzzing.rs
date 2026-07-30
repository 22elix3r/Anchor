//! Bounded parser entry points used only by the isolated cargo-fuzz workspace.

use std::io::Cursor;

/// Exercise every supported session schema decoder.
pub fn decode_session_record(bytes: &[u8]) {
    let _ = super::decode_session(bytes);
}

/// Exercise frozen-policy decoding, trailing-byte rejection, and semantic validation.
pub fn decode_frozen_policy_record(bytes: &[u8]) {
    if bytes.len() > 32 * 1024 * 1024 {
        return;
    }
    let mut cursor = Cursor::new(bytes);
    let Ok(policy) = ciborium::de::from_reader::<fence_git::FrozenGitPolicy, _>(&mut cursor) else {
        return;
    };
    if usize::try_from(cursor.position()).ok() == Some(bytes.len()) {
        let _ = policy.validate();
    }
}

/// Exercise restore-plan decoding and all semantic validation.
pub fn decode_restore_plan_record(bytes: &[u8]) {
    let _ = super::restore_plan::RestorePlanRecord::decode(bytes);
}

/// Exercise every Unix journal schema decoder with exact-input consumption.
pub fn decode_journal_records(bytes: &[u8]) {
    super::restore::fuzz_decode_journals(bytes);
}
