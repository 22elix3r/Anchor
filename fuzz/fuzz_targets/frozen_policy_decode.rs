#![no_main]

use fence_session::fuzzing::decode_frozen_policy_record;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| decode_frozen_policy_record(bytes));
