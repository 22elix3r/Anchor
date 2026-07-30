use std::ffi::{OsString, c_void};
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::ptr;

use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetKernelObjectSecurity, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, SetKernelObjectSecurity, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

use crate::filesystem::open_raw;
use crate::{NodeKind, VerbatimPath, WindowsError, io_error};

const MAX_NATIVE_STRING: usize = 32_767;

/// Return the current user's non-roaming application-data directory through the Known Folder API.
///
/// # Errors
///
/// Returns an error if the shell cannot resolve the folder or returns malformed native data.
pub fn local_app_data() -> Result<PathBuf, WindowsError> {
    let mut raw = ptr::null_mut();
    // SAFETY: `raw` is a valid output pointer. A null token requests the current user and the
    // returned allocation is released with `CoTaskMemFree`.
    let result = unsafe {
        SHGetKnownFolderPath(
            ptr::from_ref(&FOLDERID_LocalAppData),
            0,
            ptr::null_mut(),
            ptr::from_mut(&mut raw),
        )
    };
    if result < 0 {
        return Err(WindowsError::NativeStatus {
            operation: "SHGetKnownFolderPath",
            status: result,
        });
    }
    if raw.is_null() {
        return Err(WindowsError::Malformed("known-folder path"));
    }
    let units = bounded_wide(raw.cast_const(), "known-folder path");
    // SAFETY: `SHGetKnownFolderPath` allocated this pointer with the COM task allocator.
    unsafe { CoTaskMemFree(raw.cast::<c_void>()) };
    Ok(PathBuf::from(OsString::from_wide(&units?)))
}

/// Replace one ordinary directory's DACL with a protected current-user-and-SYSTEM ACL.
///
/// The directory is opened without following a reparse point and the ACL is applied to that
/// pinned handle, closing the path-replacement race.
///
/// # Errors
///
/// Returns an error for reparse points, non-directories, token failures, or ACL failures.
pub fn harden_private_directory(path: &Path) -> Result<(), WindowsError> {
    let handle = open_private_directory(path, READ_CONTROL | WRITE_DAC)?;

    let sid = current_user_sid_string()?;
    let sddl = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{sid})");
    let wide = sddl
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut descriptor = ptr::null_mut();
    // SAFETY: `wide` is NUL terminated and `descriptor` is a valid output pointer. The returned
    // descriptor is owned by LocalAlloc and released below.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            ptr::from_mut(&mut descriptor),
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        ));
    }
    // SAFETY: Both the pinned handle and converted self-relative descriptor remain valid.
    let applied = unsafe {
        SetKernelObjectSecurity(
            handle.as_raw_handle().cast(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    // SAFETY: The conversion API returned this LocalAlloc allocation.
    unsafe { LocalFree(descriptor.cast()) };
    if applied == 0 {
        return Err(io_error("SetKernelObjectSecurity"));
    }
    Ok(())
}

/// Check that a directory has Fence's exact protected current-user-and-SYSTEM DACL.
///
/// # Errors
///
/// Returns an error if the path is a reparse point or its security descriptor cannot be read.
pub fn private_directory_is_hardened(path: &Path) -> Result<bool, WindowsError> {
    let handle = open_private_directory(path, READ_CONTROL)?;
    let mut required = 0_u32;
    // SAFETY: This is the documented sizing call with a null descriptor destination.
    unsafe {
        GetKernelObjectSecurity(
            handle.as_raw_handle().cast(),
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            0,
            ptr::from_mut(&mut required),
        );
    }
    if required == 0 {
        return Err(io_error("GetKernelObjectSecurity"));
    }
    let words = usize::try_from(required)
        .map_err(|_| WindowsError::TooLarge("security descriptor"))?
        .div_ceil(size_of::<usize>());
    let mut descriptor = vec![0_usize; words];
    // SAFETY: The aligned output buffer is writable for at least `required` bytes.
    if unsafe {
        GetKernelObjectSecurity(
            handle.as_raw_handle().cast(),
            DACL_SECURITY_INFORMATION,
            descriptor.as_mut_ptr().cast::<c_void>(),
            required,
            ptr::from_mut(&mut required),
        )
    } == 0
    {
        return Err(io_error("GetKernelObjectSecurity"));
    }
    let actual = canonical_dacl_sddl(descriptor.as_mut_ptr().cast::<c_void>())?;
    let expected_input = format!(
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{})",
        current_user_sid_string()?
    );
    let wide = expected_input
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut expected_descriptor = ptr::null_mut();
    // SAFETY: `wide` is NUL terminated and the returned descriptor is released below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            ptr::from_mut(&mut expected_descriptor),
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        ));
    }
    let expected = canonical_dacl_sddl(expected_descriptor);
    // SAFETY: The conversion API returned this LocalAlloc allocation.
    unsafe { LocalFree(expected_descriptor.cast()) };
    Ok(actual == expected?)
}

fn canonical_dacl_sddl(descriptor: *mut c_void) -> Result<String, WindowsError> {
    let mut sddl = ptr::null_mut();
    // SAFETY: The caller provides a live security descriptor and the output is released below.
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            ptr::from_mut(&mut sddl),
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io_error(
            "ConvertSecurityDescriptorToStringSecurityDescriptorW",
        ));
    }
    let units = bounded_wide(sddl.cast_const(), "security descriptor string");
    // SAFETY: The conversion API returned this LocalAlloc string.
    unsafe { LocalFree(sddl.cast()) };
    String::from_utf16(&units?).map_err(|_| WindowsError::Malformed("security descriptor string"))
}

fn open_private_directory(path: &Path, access: u32) -> Result<OwnedHandle, WindowsError> {
    let path = VerbatimPath::new(path)?;
    let handle = open_raw(
        &path,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    if crate::filesystem::metadata_for_system(&handle)?.kind != NodeKind::Directory {
        return Err(WindowsError::PrivateDirectoryReparse);
    }
    Ok(handle)
}

fn current_user_sid_string() -> Result<String, WindowsError> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `token` is a valid output pointer and the pseudo process handle is always valid.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, ptr::from_mut(&mut token)) } == 0
    {
        return Err(io_error("OpenProcessToken"));
    }
    // SAFETY: `OpenProcessToken` returned a newly owned handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token.cast()) };
    let mut required = 0_u32;
    // SAFETY: This is the documented sizing call with a null destination.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle().cast(),
            TokenUser,
            ptr::null_mut(),
            0,
            ptr::from_mut(&mut required),
        );
    }
    if required == 0 {
        return Err(io_error("GetTokenInformation"));
    }
    let words = usize::try_from(required)
        .map_err(|_| WindowsError::TooLarge("token user"))?
        .div_ceil(size_of::<usize>());
    let mut storage = vec![0_usize; words];
    // SAFETY: `storage` is aligned and writable for at least `required` bytes.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle().cast(),
            TokenUser,
            storage.as_mut_ptr().cast::<c_void>(),
            required,
            ptr::from_mut(&mut required),
        )
    } == 0
    {
        return Err(io_error("GetTokenInformation"));
    }
    // SAFETY: A successful TokenUser query initialized a TOKEN_USER at the aligned buffer start.
    let sid = unsafe { storage.as_ptr().cast::<TOKEN_USER>().read().User.Sid };
    let mut string_sid = ptr::null_mut();
    // SAFETY: `sid` points into the live token information buffer and the output is valid.
    if unsafe { ConvertSidToStringSidW(sid, ptr::from_mut(&mut string_sid)) } == 0 {
        return Err(io_error("ConvertSidToStringSidW"));
    }
    let units = bounded_wide(string_sid.cast_const(), "SID string");
    // SAFETY: `ConvertSidToStringSidW` allocated the returned string with LocalAlloc.
    unsafe { LocalFree(string_sid.cast()) };
    String::from_utf16(&units?).map_err(|_| WindowsError::Malformed("SID string"))
}

fn bounded_wide(pointer: *const u16, label: &'static str) -> Result<Vec<u16>, WindowsError> {
    for length in 0..MAX_NATIVE_STRING {
        // SAFETY: Both callers provide a valid NUL-terminated Windows API allocation. The scan is
        // bounded by the maximum native path/string length accepted by this crate.
        if unsafe { *pointer.add(length) } == 0 {
            // SAFETY: The preceding bounded scan established readable initialized units.
            return Ok(unsafe { std::slice::from_raw_parts(pointer, length) }.to_vec());
        }
    }
    Err(WindowsError::TooLarge(label))
}

/// A Windows Job object that terminates its assigned process tree if the wrapper exits.
#[derive(Debug)]
pub struct KillOnCloseJob {
    handle: OwnedHandle,
}

impl KillOnCloseJob {
    /// Create an unnamed job configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    ///
    /// # Errors
    ///
    /// Returns an error if the job cannot be created or configured.
    pub fn new() -> Result<Self, WindowsError> {
        // SAFETY: Null attributes and name request a new private job object.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw.is_null() {
            return Err(io_error("CreateJobObjectW"));
        }
        // SAFETY: The API returned an owned job handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let limits_size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(|_| WindowsError::TooLarge("job limits"))?;
        // SAFETY: `limits` is initialized and its exact structure size is supplied.
        let success = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle().cast(),
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limits).cast::<c_void>(),
                limits_size,
            )
        };
        if success == 0 {
            return Err(io_error("SetInformationJobObject"));
        }
        Ok(Self { handle })
    }

    /// Assign a spawned child to the kill-on-close job.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows refuses nested-job assignment.
    pub fn assign(&self, child: &Child) -> Result<(), WindowsError> {
        // SAFETY: Both handles remain owned and valid for the duration of the call.
        if unsafe {
            AssignProcessToJobObject(
                self.handle.as_raw_handle().cast(),
                child.as_raw_handle().cast(),
            )
        } == 0
        {
            return Err(io_error("AssignProcessToJobObject"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn known_local_app_data_is_absolute() {
        assert!(local_app_data().unwrap().is_absolute());
    }

    #[test]
    fn hardens_an_ordinary_directory_without_losing_access() {
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("private");
        fs::create_dir(&private).unwrap();
        harden_private_directory(&private).unwrap();
        assert!(private_directory_is_hardened(&private).unwrap());
        fs::write(private.join("probe"), b"private").unwrap();
        assert_eq!(fs::read(private.join("probe")).unwrap(), b"private");
    }

    #[test]
    fn refuses_a_directory_symlink_as_private_storage() {
        use std::os::windows::fs::symlink_dir;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let link = root.path().join("link");
        fs::create_dir(&target).unwrap();
        if let Err(error) = symlink_dir(&target, &link) {
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("failed to create test symlink: {error}");
        }
        assert!(matches!(
            harden_private_directory(&link),
            Err(WindowsError::PrivateDirectoryReparse)
        ));
    }
}
