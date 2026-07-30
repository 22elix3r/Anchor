#![no_main]

use fence_session::fuzzing::decode_journal_records;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| decode_journal_records(bytes));
