#![no_main]

use fence_core::object::decode_object_envelope_for_fuzzing;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let _ = decode_object_envelope_for_fuzzing(bytes);
});
