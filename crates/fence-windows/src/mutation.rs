use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, NtCreateFile,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, HANDLE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_DELETE_CHILD, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_INFO_EX,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
    FILE_WRITE_ATTRIBUTES, FileDispositionInfoEx, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_SYMLINK;

use crate::filesystem::{metadata_for_system, open_raw, raw_handle};
use crate::{DirectoryHandle, NodeKind, SymbolicLinkData, VerbatimPath, WindowsError, io_error};

const MAX_REPARSE_BUFFER: usize = 16 * 1024;
const MUTATION_DIRECTORY_ACCESS: u32 = FILE_LIST_DIRECTORY
    | FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | FILE_DELETE_CHILD
    | FILE_TRAVERSE
    | FILE_READ_ATTRIBUTES;

/// A pinned ordinary root opened with the rights needed for handle-relative restoration.
#[derive(Debug)]
pub struct MutationRoot {
    directory: DirectoryHandle,
}

impl MutationRoot {
    /// Resolve the selected root once, then reopen it as a non-reparse mutation boundary.
    ///
    /// # Errors
    ///
    /// Returns an error unless the final selected node is an ordinary writable directory.
    pub fn open(path: &Path) -> Result<Self, WindowsError> {
        let requested = VerbatimPath::new(path)?;
        let followed = open_raw(
            &requested,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_FLAG_BACKUP_SEMANTICS,
        )?;
        let final_path = crate::filesystem::final_path_for_mutation(&followed)?;
        let followed_metadata = metadata_for_system(&followed)?;
        let handle = open_raw(
            &final_path,
            MUTATION_DIRECTORY_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        let metadata = metadata_for_system(&handle)?;
        if metadata.kind != NodeKind::Directory {
            return Err(WindowsError::NotDirectory);
        }
        if metadata.identity != followed_metadata.identity {
            return Err(WindowsError::IdentityChanged);
        }
        Ok(Self {
            directory: DirectoryHandle {
                handle,
                path: final_path,
                metadata,
            },
        })
    }

    /// Borrow the pinned mutation directory.
    pub const fn directory(&self) -> &DirectoryHandle {
        &self.directory
    }

    /// Consume the root directory handle.
    pub fn into_directory(self) -> DirectoryHandle {
        self.directory
    }
}

impl DirectoryHandle {
    /// Adopt an already-open ordinary directory as a pinned mutation boundary.
    ///
    /// This is used by capability-based callers that have already resolved and retained the
    /// directory. The final path is diagnostic only; subsequent operations remain handle-relative.
    ///
    /// # Errors
    ///
    /// Refuses non-directory and reparse-point handles or malformed final paths.
    pub fn from_directory_file(file: File) -> Result<DirectoryHandle, WindowsError> {
        let handle = OwnedHandle::from(file);
        let path = crate::filesystem::final_path_for_mutation(&handle)?;
        let metadata = metadata_for_system(&handle)?;
        if metadata.kind != NodeKind::Directory {
            return Err(WindowsError::NotDirectory);
        }
        Ok(DirectoryHandle {
            handle,
            path,
            metadata,
        })
    }

    /// Duplicate this pinned mutation directory handle.
    ///
    /// # Errors
    ///
    /// Returns an error if Windows cannot duplicate the underlying handle.
    pub fn try_clone_mutation(&self) -> Result<DirectoryHandle, WindowsError> {
        let handle = self.handle.try_clone().map_err(|source| WindowsError::Io {
            operation: "DuplicateHandle",
            source,
        })?;
        Ok(DirectoryHandle {
            handle,
            path: self.path.clone(),
            metadata: self.metadata,
        })
    }

    /// Open an exact ordinary child directory with mutation rights.
    ///
    /// # Errors
    ///
    /// Refuses missing names, identity races, non-directories, and reparse points.
    pub fn open_mutation_directory(&self, name: &OsStr) -> Result<DirectoryHandle, WindowsError> {
        let entry = self
            .entries()?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(WindowsError::IdentityChanged)?;
        if entry.reparse_tag.is_some() || entry.attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(WindowsError::NotDirectory);
        }
        let handle = nt_create_relative(
            &self.handle,
            name,
            MUTATION_DIRECTORY_ACCESS,
            0,
            FILE_OPEN,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;
        let metadata = metadata_for_system(&handle)?;
        if metadata.kind != NodeKind::Directory || metadata.identity.file_id != entry.file_id {
            return Err(WindowsError::IdentityChanged);
        }
        Ok(DirectoryHandle {
            handle,
            path: self.path.join_name(name)?,
            metadata,
        })
    }

    /// Create a new ordinary file relative to this pinned directory.
    ///
    /// # Errors
    ///
    /// Fails if the exact name exists or is unsafe.
    pub fn create_new_file(&self, name: &OsStr) -> Result<File, WindowsError> {
        let handle = nt_create_relative(
            &self.handle,
            name,
            GENERIC_READ | GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            FILE_ATTRIBUTE_NORMAL,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;
        Ok(File::from(handle))
    }

    /// Create a new empty ordinary directory relative to this pinned directory.
    ///
    /// # Errors
    ///
    /// Fails if the exact name exists or is unsafe.
    pub fn create_new_directory(&self, name: &OsStr) -> Result<(), WindowsError> {
        nt_create_relative(
            &self.handle,
            name,
            GENERIC_READ | GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            FILE_ATTRIBUTE_NORMAL,
            FILE_CREATE,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;
        Ok(())
    }

    /// Create an exact standard symbolic-link reparse point relative to this pinned directory.
    ///
    /// # Errors
    ///
    /// Fails for existing names, unsupported flags, malformed targets, or unsupported volumes.
    pub fn create_symbolic_link(
        &self,
        name: &OsStr,
        data: &SymbolicLinkData,
        directory: bool,
    ) -> Result<(), WindowsError> {
        if data.flags > 1 {
            return Err(WindowsError::Malformed("symbolic-link flags"));
        }
        let handle = nt_create_relative(
            &self.handle,
            name,
            GENERIC_READ | GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            FILE_ATTRIBUTE_NORMAL,
            FILE_CREATE,
            (if directory {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            }) | FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;
        let payload = encode_symbolic_link(data)?;
        let mut returned = 0_u32;
        // SAFETY: `payload` is a validated standard symbolic-link reparse buffer and the newly
        // created handle is opened for data writes without following reparse points.
        if unsafe {
            DeviceIoControl(
                raw_handle(&handle),
                FSCTL_SET_REPARSE_POINT,
                payload.as_ptr().cast::<c_void>(),
                u32::try_from(payload.len())
                    .map_err(|_| WindowsError::TooLarge("symbolic-link reparse data"))?,
                ptr::null_mut(),
                0,
                ptr::from_mut(&mut returned),
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io_error("FSCTL_SET_REPARSE_POINT"));
        }
        Ok(())
    }

    /// Rename an exact child within this pinned directory without replacement.
    ///
    /// # Errors
    ///
    /// Fails if the source changed or the target name already exists.
    pub fn rename_child_noreplace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> Result<(), WindowsError> {
        let source = self.open_named_for_delete(source)?;
        rename_handle(&source, &self.handle, destination, false)
    }

    /// Atomically replace one exact child with another child from this pinned directory.
    ///
    /// # Errors
    ///
    /// Fails if the source changed, either name is unsafe, or Windows refuses replacement.
    pub fn replace_child(&self, source: &OsStr, destination: &OsStr) -> Result<(), WindowsError> {
        let source = self.open_named_for_delete(source)?;
        rename_handle(&source, &self.handle, destination, true)
    }

    /// Delete an exact child through a no-follow handle.
    ///
    /// # Errors
    ///
    /// Fails if the node changed, is a non-empty directory, or is held without delete sharing.
    pub fn remove_child(&self, name: &OsStr) -> Result<(), WindowsError> {
        let child = self.open_named_for_delete(name)?;
        let disposition = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        };
        let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>())
            .map_err(|_| WindowsError::TooLarge("disposition information"))?;
        // SAFETY: `disposition` is initialized and `child` was opened with DELETE access.
        if unsafe {
            SetFileInformationByHandle(
                raw_handle(&child),
                FileDispositionInfoEx,
                ptr::from_ref(&disposition).cast::<c_void>(),
                size,
            )
        } == 0
        {
            return Err(io_error(
                "SetFileInformationByHandle(FileDispositionInfoEx)",
            ));
        }
        Ok(())
    }

    fn open_named_for_delete(&self, name: &OsStr) -> Result<OwnedHandle, WindowsError> {
        let entry = self
            .entries()?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(WindowsError::IdentityChanged)?;
        let options = if entry.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        };
        let handle = nt_create_relative(
            &self.handle,
            name,
            DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            0,
            FILE_OPEN,
            options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;
        if metadata_for_system(&handle)?.identity.file_id != entry.file_id {
            return Err(WindowsError::IdentityChanged);
        }
        Ok(handle)
    }
}

fn nt_create_relative(
    root: &OwnedHandle,
    name: &OsStr,
    access: u32,
    attributes: u32,
    disposition: u32,
    options: u32,
) -> Result<OwnedHandle, WindowsError> {
    // Validate the component using the same rules as capture before constructing native data.
    let _validated = VerbatimPath::new(Path::new(r"C:\"))?.join_name(name)?;
    let units = name.encode_wide().collect::<Vec<_>>();
    let byte_len = u16::try_from(
        units
            .len()
            .checked_mul(2)
            .ok_or(WindowsError::TooLarge("relative filename"))?,
    )
    .map_err(|_| WindowsError::TooLarge("relative filename"))?;
    let unicode = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: units.as_ptr().cast_mut(),
    };
    let object = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>())
            .map_err(|_| WindowsError::TooLarge("object attributes"))?,
        RootDirectory: raw_handle(root),
        ObjectName: ptr::from_ref(&unicode),
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle: HANDLE = ptr::null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: All descriptors point to initialized live stack/storage data for the duration of
    // the call, and a successful output handle is immediately wrapped by `OwnedHandle`.
    let result = unsafe {
        NtCreateFile(
            ptr::from_mut(&mut handle),
            access | SYNCHRONIZE,
            ptr::from_ref(&object),
            ptr::from_mut(&mut status),
            ptr::null(),
            attributes,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            disposition,
            options,
            ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(WindowsError::NtStatus {
            operation: "NtCreateFile",
            status: result,
        });
    }
    // SAFETY: Successful NtCreateFile returned a newly owned kernel handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
}

fn rename_handle(
    source: &OwnedHandle,
    destination_directory: &OwnedHandle,
    destination: &OsStr,
    replace: bool,
) -> Result<(), WindowsError> {
    let _validated = VerbatimPath::new(Path::new(r"C:\"))?.join_name(destination)?;
    let units = destination.encode_wide().collect::<Vec<_>>();
    let name_bytes = units
        .len()
        .checked_mul(2)
        .ok_or(WindowsError::TooLarge("rename filename"))?;
    // Windows requires the buffer to contain the complete declared structure plus the variable
    // filename bytes.
    let total = size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(name_bytes)
        .ok_or(WindowsError::TooLarge("rename information"))?;
    let mut buffer = vec![0_usize; total.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: The aligned buffer has room for the fixed header and every filename unit.
    unsafe {
        ptr::write(info, FILE_RENAME_INFORMATION::default());
        (*info).Anonymous.ReplaceIfExists = replace;
        (*info).RootDirectory = raw_handle(destination_directory);
        (*info).FileNameLength =
            u32::try_from(name_bytes).map_err(|_| WindowsError::TooLarge("rename filename"))?;
        ptr::copy_nonoverlapping(
            units.as_ptr(),
            ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            units.len(),
        );
    }
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: `buffer` is a valid variable-sized FILE_RENAME_INFORMATION, both rooted handles
    // remain live, and the source was opened with DELETE access.
    let result = unsafe {
        NtSetInformationFile(
            raw_handle(source),
            ptr::from_mut(&mut status),
            buffer.as_ptr().cast::<c_void>(),
            u32::try_from(total).map_err(|_| WindowsError::TooLarge("rename information"))?,
            FileRenameInformation,
        )
    };
    if result < 0 {
        return Err(WindowsError::NtStatus {
            operation: "NtSetInformationFile(FileRenameInformation)",
            status: result,
        });
    }
    Ok(())
}

fn encode_symbolic_link(data: &SymbolicLinkData) -> Result<Vec<u8>, WindowsError> {
    let substitute = data.substitute_name.encode_wide().collect::<Vec<_>>();
    let print = data.print_name.encode_wide().collect::<Vec<_>>();
    let substitute_bytes = substitute
        .len()
        .checked_mul(2)
        .ok_or(WindowsError::TooLarge("symbolic-link target"))?;
    let print_bytes = print
        .len()
        .checked_mul(2)
        .ok_or(WindowsError::TooLarge("symbolic-link target"))?;
    let total = 20_usize
        .checked_add(substitute_bytes)
        .and_then(|value| value.checked_add(print_bytes))
        .ok_or(WindowsError::TooLarge("symbolic-link reparse data"))?;
    if total > MAX_REPARSE_BUFFER {
        return Err(WindowsError::TooLarge("symbolic-link reparse data"));
    }
    let mut output = vec![0_u8; total];
    output[0..4].copy_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
    output[4..6].copy_from_slice(
        &u16::try_from(total - 8)
            .map_err(|_| WindowsError::TooLarge("symbolic-link reparse data"))?
            .to_le_bytes(),
    );
    output[10..12].copy_from_slice(
        &u16::try_from(substitute_bytes)
            .map_err(|_| WindowsError::TooLarge("symbolic-link target"))?
            .to_le_bytes(),
    );
    output[12..14].copy_from_slice(
        &u16::try_from(substitute_bytes)
            .map_err(|_| WindowsError::TooLarge("symbolic-link target"))?
            .to_le_bytes(),
    );
    output[14..16].copy_from_slice(
        &u16::try_from(print_bytes)
            .map_err(|_| WindowsError::TooLarge("symbolic-link target"))?
            .to_le_bytes(),
    );
    output[16..20].copy_from_slice(&data.flags.to_le_bytes());
    for (index, unit) in substitute.iter().chain(&print).enumerate() {
        let offset = 20 + index * 2;
        output[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn creates_renames_and_removes_relative_to_pinned_directory() {
        let root = tempfile::tempdir().unwrap();
        let mutation = MutationRoot::open(root.path()).unwrap();
        let directory = mutation.directory();
        let mut file = directory.create_new_file(OsStr::new("stage")).unwrap();
        file.write_all(b"exact bytes").unwrap();
        file.sync_all().unwrap();
        drop(file);
        directory
            .rename_child_noreplace(OsStr::new("stage"), OsStr::new("final"))
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("final")).unwrap(),
            b"exact bytes"
        );
        directory.remove_child(OsStr::new("final")).unwrap();
        assert!(!root.path().join("final").exists());
    }

    #[test]
    fn refuses_to_replace_an_existing_rename_target() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("source"), b"source").unwrap();
        std::fs::write(root.path().join("target"), b"target").unwrap();
        let mutation = MutationRoot::open(root.path()).unwrap();
        assert!(
            mutation
                .directory()
                .rename_child_noreplace(OsStr::new("source"), OsStr::new("target"))
                .is_err()
        );
        assert_eq!(
            std::fs::read(root.path().join("source")).unwrap(),
            b"source"
        );
        assert_eq!(
            std::fs::read(root.path().join("target")).unwrap(),
            b"target"
        );
    }

    #[test]
    fn atomically_replaces_an_existing_rename_target() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("source"), b"new").unwrap();
        std::fs::write(root.path().join("target"), b"old").unwrap();
        let mutation = MutationRoot::open(root.path()).unwrap();
        mutation
            .directory()
            .replace_child(OsStr::new("source"), OsStr::new("target"))
            .unwrap();

        assert!(!root.path().join("source").exists());
        assert_eq!(std::fs::read(root.path().join("target")).unwrap(), b"new");
    }

    #[test]
    fn refuses_reparse_points_during_mutating_descent() {
        use std::os::windows::fs::symlink_dir;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        if let Err(error) = symlink_dir(outside.path(), &link) {
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("failed to create test symlink: {error}");
        }
        let mutation = MutationRoot::open(root.path()).unwrap();
        assert!(
            mutation
                .directory()
                .open_mutation_directory(OsStr::new("link"))
                .is_err()
        );
    }
}
