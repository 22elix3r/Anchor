use std::os::fd::AsFd;

use crate::MetadataObservation;

const SMALL_XATTR_LIST: usize = 1024;
const MAX_XATTR_LIST: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const MAX_PLATFORM_LABEL: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const SELINUX_XATTR: &str = "security.selinux";

/// Observe whether an open node carries extended metadata Anchor does not reproduce.
#[must_use]
pub fn observe_extended_metadata(file: &impl AsFd) -> MetadataObservation {
    #[cfg(target_os = "linux")]
    {
        let Ok(names) = list_xattrs(file) else {
            return MetadataObservation::Unavailable;
        };
        if names.is_empty() {
            return MetadataObservation::Absent;
        }
        if names
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .all(|name| name == SELINUX_XATTR.as_bytes())
        {
            return MetadataObservation::PlatformManaged;
        }
        MetadataObservation::Present
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsFd;

        let Ok(names) = list_xattrs(file) else {
            return MetadataObservation::Unavailable;
        };
        if !names.is_empty() {
            return MetadataObservation::Present;
        }
        match anchor_unix::extended_acl_present(file.as_fd()) {
            Ok(true) => MetadataObservation::Present,
            Ok(false) => MetadataObservation::Absent,
            Err(_) => MetadataObservation::Unavailable,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = file;
        MetadataObservation::Unavailable
    }
}

/// Observe an already capability-rooted directory through a readable descriptor.
///
/// `cap_std::fs::Dir` may use an `O_PATH` descriptor on Linux, which cannot be
/// passed to `flistxattr`. Reopening `.` retains the capability boundary while
/// producing a descriptor suitable for metadata queries.
#[must_use]
pub fn observe_directory_extended_metadata(directory: &cap_std::fs::Dir) -> MetadataObservation {
    directory
        .open(".")
        .map_or(MetadataObservation::Unavailable, |file| {
            observe_extended_metadata(&file)
        })
}

/// Compare platform-managed metadata on two capability-rooted directories.
#[must_use]
pub fn platform_managed_directory_metadata_equal(
    left: &cap_std::fs::Dir,
    right: &cap_std::fs::Dir,
) -> bool {
    let Ok(left) = left.open(".") else {
        return false;
    };
    let Ok(right) = right.open(".") else {
        return false;
    };
    platform_managed_metadata_equal(&left, &right)
}

/// Compare platform-managed metadata that must survive a staged replacement.
///
/// This currently recognizes only the `SELinux` label. Any query ambiguity is unequal.
#[must_use]
pub fn platform_managed_metadata_equal(left: &impl AsFd, right: &impl AsFd) -> bool {
    #[cfg(target_os = "linux")]
    {
        read_xattr(left, SELINUX_XATTR)
            .is_some_and(|left| read_xattr(right, SELINUX_XATTR).is_some_and(|right| left == right))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (left, right);
        false
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn list_xattrs(file: &impl AsFd) -> Result<Vec<u8>, rustix::io::Errno> {
    let mut bytes = vec![0_u8; SMALL_XATTR_LIST];
    match rustix::fs::flistxattr(file, &mut bytes) {
        Ok(length) => {
            bytes.truncate(length);
            Ok(bytes)
        }
        Err(rustix::io::Errno::RANGE) => {
            bytes.resize(MAX_XATTR_LIST, 0);
            let length = rustix::fs::flistxattr(file, &mut bytes)?;
            bytes.truncate(length);
            Ok(bytes)
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn read_xattr(file: &impl AsFd, name: &str) -> Option<Vec<u8>> {
    let mut value = vec![0_u8; MAX_PLATFORM_LABEL];
    let length = rustix::fs::fgetxattr(file, name, &mut value).ok()?;
    value.truncate(length);
    Some(value)
}
