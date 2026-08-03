//! Safety-reviewed native Unix filesystem primitives used by Fence.
//!
//! This crate quarantines the small amount of Unix FFI that cannot be expressed
//! through the safe APIs used by `fence-core`.
//!
//! This implementation crate's Rust API is prerelease and may change between
//! `0.1.0-alpha.N` versions.

#![cfg(target_os = "macos")]

use std::ffi::{c_int, c_uint, c_void};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr;

const ACL_TYPE_EXTENDED: c_uint = 256;
const ACL_FIRST_ENTRY: c_int = 0;

#[repr(C)]
struct Acl {
    _private: [u8; 0],
}

#[repr(C)]
struct AclEntry {
    _private: [u8; 0],
}

type AclHandle = *mut Acl;
type AclEntryHandle = *mut AclEntry;

unsafe extern "C" {
    fn acl_get_fd_np(fd: c_int, acl_type: c_uint) -> AclHandle;
    fn acl_get_entry(acl: AclHandle, entry_id: c_int, entry: *mut AclEntryHandle) -> c_int;
    fn acl_free(object: *mut c_void) -> c_int;
}

struct OwnedAcl(AclHandle);

impl Drop for OwnedAcl {
    fn drop(&mut self) {
        // SAFETY: `OwnedAcl` is constructed only from a non-null value returned
        // by `acl_get_fd_np`, owns that value, and invokes `acl_free` once.
        let _ = unsafe { acl_free(self.0.cast()) };
    }
}

/// Report whether an already-open file or directory has an extended macOS ACL.
///
/// The query is descriptor-based, so it cannot be redirected through a path
/// replacement between worktree verification and metadata inspection.
pub fn extended_acl_present(file: BorrowedFd<'_>) -> io::Result<bool> {
    // SAFETY: `file` is a live borrowed descriptor for the duration of the
    // call, and `ACL_TYPE_EXTENDED` is the value defined by macOS
    // `<sys/acl.h>`.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(false);
        }
        return Err(error);
    }
    let acl = OwnedAcl(acl);
    let mut entry = ptr::null_mut();
    // SAFETY: `acl` remains live through the call and `entry` points to writable
    // storage for the borrowed entry pointer. Fence does not retain it.
    match unsafe { acl_get_entry(acl.0, ACL_FIRST_ENTRY, &raw mut entry) } {
        0 => Ok(true),
        _ => {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::fd::AsFd;
    use std::process::Command;

    use super::extended_acl_present;

    #[test]
    fn distinguishes_empty_and_extended_acl() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("node");
        File::create(&path).unwrap();

        let file = File::open(&path).unwrap();
        assert!(!extended_acl_present(file.as_fd()).unwrap());
        drop(file);

        let status = Command::new("/bin/chmod")
            .args(["+a", "everyone deny delete"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());

        let file = File::open(&path).unwrap();
        assert!(extended_acl_present(file.as_fd()).unwrap());
    }
}
