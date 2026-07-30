#![no_main]

use fence_session::fuzzing::decode_session_record;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| decode_session_record(bytes));
