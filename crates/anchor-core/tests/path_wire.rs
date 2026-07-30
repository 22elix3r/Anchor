use anchor_core::wire::{decode_path, encode_path};
use anchor_core::{NativeRelativePath, PathEncoding};
use proptest::collection::vec;
use proptest::prelude::*;

fn unix_component() -> impl Strategy<Value = Vec<u8>> {
    vec(
        any::<u8>().prop_filter("no separators or NUL", |byte| !matches!(*byte, 0 | b'/')),
        1..64,
    )
    .prop_filter("not dot traversal", |bytes| bytes != b"." && bytes != b"..")
}

fn windows_component() -> impl Strategy<Value = Vec<u8>> {
    vec(
        any::<u16>().prop_filter("no separators or NUL", |unit| {
            !matches!(*unit, 0 | 0x2f | 0x3a | 0x5c)
        }),
        1..32,
    )
    .prop_filter("not dot traversal", |units| {
        units != &[u16::from(b'.')] && units != &[u16::from(b'.'), u16::from(b'.')]
    })
    .prop_map(|units| units.into_iter().flat_map(u16::to_le_bytes).collect())
}

proptest! {
    #[test]
    fn unix_path_wire_round_trip(components in vec(unix_component(), 0..16)) {
        let path = NativeRelativePath::new(PathEncoding::UnixBytes, components).unwrap();
        let encoded = encode_path(&path).unwrap();
        prop_assert_eq!(decode_path(&encoded).unwrap(), path);
    }

    #[test]
    fn windows_path_wire_round_trip(components in vec(windows_component(), 0..16)) {
        let path = NativeRelativePath::new(PathEncoding::WindowsWtf16Le, components).unwrap();
        let encoded = encode_path(&path).unwrap();
        prop_assert_eq!(decode_path(&encoded).unwrap(), path);
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_unix_host_round_trip() {
    use std::os::unix::ffi::OsStringExt;

    let original = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![0xff, b'a']));
    let native = NativeRelativePath::from_host_path(&original).unwrap();
    assert_eq!(native.to_host_path().unwrap(), original);
}
