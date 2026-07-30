#![no_main]

use anchor_core::Manifest;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let _result = Manifest::decode(bytes);
});
