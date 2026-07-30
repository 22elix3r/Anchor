#![no_main]

use anchor_core::wire::decode_path;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let _result = decode_path(bytes);
});
