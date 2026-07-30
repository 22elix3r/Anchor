#![no_main]

use std::collections::BTreeSet;

use fence_core::{Manifest, RestorePlan};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let third = bytes.len() / 3;
    let (base, tail) = bytes.split_at(third);
    let (session, current) = tail.split_at(third);
    let (Ok(base), Ok(session), Ok(current)) = (
        Manifest::decode(base),
        Manifest::decode(session),
        Manifest::decode(current),
    ) else {
        return;
    };
    let _ = RestorePlan::calculate(&base, &session, &current, &BTreeSet::new());
});
