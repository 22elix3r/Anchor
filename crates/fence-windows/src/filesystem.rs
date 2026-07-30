use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt as _;
use std::os::windows::io::{AsRawHandle, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_NO_MORE_FILES, GENERIC_READ, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_ENCRYPTED, FILE_ATTRIBUTE_OFFLINE,
    FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_CASE_SENSITIVE_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo, FileBasicInfo,
    FileCaseSensitiveInfo, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo,
    FileStandardInfo, FileStreamInfo, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    OPEN_EXISTING, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::{
    FILE_CS_FLAG_CASE_SENSITIVE_DIR, IO_REPARSE_TAG_SYMLINK,
};

use crate::{VerbatimPath, WindowsError, io_error};

const DIRECTORY_BUFFER: usize = 64 * 1024;
const INITIAL_STREAM_BUFFER: usize = 64 * 1024;
const MAX_STREAM_BUFFER: usize = 1024 * 1024;
const MAX_REPARSE_BUFFER: usize = 16 * 1024;
const FILE_NAME_NORMALIZED: u32 = 0;

/// Stable identity returned by `FileIdInfo`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileIdentity {
    pub volume_serial: u64,
    pub file_id: [u8; 16],
}

/// The supported structural kind of a Windows node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    RegularFile,
    Directory,
    ReparsePoint,
}

/// Identity-relevant metadata read from an open handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeMetadata {
    pub identity: FileIdentity,
    pub kind: NodeKind,
    pub size: u64,
    pub allocation_size: u64,
    pub link_count: u32,
    pub attributes: u32,
    pub reparse_tag: Option<u32>,
    pub creation_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
}

impl NodeMetadata {
    /// Whether the node carries the Windows read-only attribute.
    #[must_use]
    pub const fn is_readonly(self) -> bool {
        self.attributes & FILE_ATTRIBUTE_READONLY != 0
    }

    /// Whether the node is a directory according to its file attributes.
    #[must_use]
    pub const fn has_directory_attribute(self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// Whether reading the node may recall externally tiered data.
    #[must_use]
    pub const fn may_recall_data(self) -> bool {
        self.attributes
            & (FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
    }

    /// Whether the node is encrypted by EFS.
    #[must_use]
    pub const fn is_efs_encrypted(self) -> bool {
        self.attributes & FILE_ATTRIBUTE_ENCRYPTED != 0
    }
}

/// One entry returned by handle-based directory enumeration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub name: OsString,
    pub file_id: [u8; 16],
    pub attributes: u32,
    pub reparse_tag: Option<u32>,
    pub size: u64,
    pub allocation_size: u64,
    pub creation_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
}

/// One stream attached to a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamInfo {
    pub name: OsString,
    pub size: u64,
    pub allocation_size: u64,
}

impl StreamInfo {
    /// Whether this is the unnamed default data stream.
    pub fn is_default_data_stream(&self) -> bool {
        self.name == OsStr::new("::$DATA")
    }
}

/// The two names and flags stored in a standard symbolic-link reparse point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolicLinkData {
    pub substitute_name: OsString,
    pub print_name: OsString,
    pub flags: u32,
}

/// A reparse point classified without following it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReparseKind {
    SymbolicLink(SymbolicLinkData),
    Other(u32),
}

/// A pinned filesystem root.
#[derive(Debug)]
pub struct RootHandle {
    directory: DirectoryHandle,
}

impl RootHandle {
    /// Open a selected root, resolve that selected root once, then reopen its final location
    /// without following any descendant reparse points.
    ///
    /// # Errors
    ///
    /// Returns an error unless the final node is an ordinary directory.
    pub fn open(path: &Path) -> Result<Self, WindowsError> {
        let requested = VerbatimPath::new(path)?;
        let followed = open_raw(
            &requested,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_FLAG_BACKUP_SEMANTICS,
        )?;
        let final_path = final_path(&followed)?;
        let followed_metadata = metadata(&followed)?;
        let handle = open_raw(
            &final_path,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        let metadata = metadata(&handle)?;
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

    /// Borrow the pinned root directory.
    pub const fn directory(&self) -> &DirectoryHandle {
        &self.directory
    }

    /// Consume the root and return its directory handle.
    pub fn into_directory(self) -> DirectoryHandle {
        self.directory
    }
}

/// A pinned ordinary directory used as a no-follow namespace boundary.
#[derive(Debug)]
pub struct DirectoryHandle {
    pub(crate) handle: OwnedHandle,
    pub(crate) path: VerbatimPath,
    pub(crate) metadata: NodeMetadata,
}

impl DirectoryHandle {
    /// Metadata for the pinned directory.
    pub const fn metadata(&self) -> NodeMetadata {
        self.metadata
    }

    /// Enumerate raw names, 128-bit identities, attributes, and reparse tags from this handle.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed native records or filesystem query failure.
    pub fn entries(&self) -> Result<Vec<DirectoryEntry>, WindowsError> {
        enumerate(&self.handle)
    }

    /// Whether this directory uses per-directory case-sensitive lookup semantics.
    ///
    /// # Errors
    ///
    /// Returns an error if the filesystem cannot report case semantics.
    pub fn is_case_sensitive(&self) -> Result<bool, WindowsError> {
        let info: FILE_CASE_SENSITIVE_INFO =
            query_fixed(&self.handle, FileCaseSensitiveInfo, "FileCaseSensitiveInfo")?;
        Ok(info.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0)
    }

    /// Open an enumerated child without following a reparse point and verify its identity.
    ///
    /// # Errors
    ///
    /// Returns `IdentityChanged` if the child was replaced after enumeration.
    pub fn open_child(&self, entry: &DirectoryEntry) -> Result<NodeHandle, WindowsError> {
        let path = self.path.join_name(&entry.name)?;
        let is_directory = entry.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        let access = if is_directory {
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES
        } else {
            GENERIC_READ | FILE_READ_ATTRIBUTES
        };
        let share = if is_directory {
            FILE_SHARE_READ | FILE_SHARE_WRITE
        } else {
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        };
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | if is_directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                0
            };
        let handle = open_raw(&path, access, share, flags)?;
        let metadata = metadata(&handle)?;
        if metadata.identity.volume_serial != self.metadata.identity.volume_serial
            || metadata.identity.file_id != entry.file_id
        {
            return Err(WindowsError::IdentityChanged);
        }
        Ok(NodeHandle {
            handle,
            path,
            metadata,
        })
    }

    /// Open an exact currently enumerated child without following a reparse point.
    ///
    /// # Errors
    ///
    /// Returns `IdentityChanged` when the name is absent or changes before it is opened.
    pub fn open_named_child(&self, name: &OsStr) -> Result<NodeHandle, WindowsError> {
        let entry = self
            .entries()?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(WindowsError::IdentityChanged)?;
        self.open_child(&entry)
    }
}

/// A child opened without following a reparse point.
#[derive(Debug)]
pub struct NodeHandle {
    handle: OwnedHandle,
    path: VerbatimPath,
    metadata: NodeMetadata,
}

impl NodeHandle {
    /// Metadata read from the open node.
    pub const fn metadata(&self) -> NodeMetadata {
        self.metadata
    }

    /// Query the reparse payload without following it.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed data or a native query failure.
    pub fn reparse_kind(&self) -> Result<Option<ReparseKind>, WindowsError> {
        if self.metadata.kind != NodeKind::ReparsePoint {
            return Ok(None);
        }
        query_reparse(&self.handle).map(Some)
    }

    /// Enumerate all streams attached to this node.
    ///
    /// # Errors
    ///
    /// Returns an error if the filesystem returns malformed or unbounded stream information.
    pub fn streams(&self) -> Result<Vec<StreamInfo>, WindowsError> {
        query_streams(&self.handle)
    }

    /// Re-read identity-relevant metadata from the same handle.
    ///
    /// # Errors
    ///
    /// Returns an error when the filesystem query fails.
    pub fn refresh_metadata(&self) -> Result<NodeMetadata, WindowsError> {
        metadata(&self.handle)
    }

    /// Clone an ordinary file handle for streaming reads while retaining the pinned identity
    /// handle for the post-read verification.
    ///
    /// # Errors
    ///
    /// Returns an error for non-files or when the operating system cannot duplicate the handle.
    pub fn try_clone_file(&self) -> Result<File, WindowsError> {
        if self.metadata.kind != NodeKind::RegularFile {
            return Err(WindowsError::Malformed("regular file handle"));
        }
        let cloned = self.handle.try_clone().map_err(|source| WindowsError::Io {
            operation: "DuplicateHandle",
            source,
        })?;
        Ok(File::from(cloned))
    }

    /// Reopen the node by its current name and require the same identity.
    ///
    /// # Errors
    ///
    /// Returns `IdentityChanged` when the path no longer resolves to this node.
    pub fn verify_path_identity(&self) -> Result<(), WindowsError> {
        let is_directory = self.metadata.has_directory_attribute();
        let reopened = open_raw(
            &self.path,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_OPEN_REPARSE_POINT
                | if is_directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        )?;
        if metadata(&reopened)?.identity != self.metadata.identity {
            return Err(WindowsError::IdentityChanged);
        }
        Ok(())
    }

    /// Turn an ordinary directory node into a pinned directory handle.
    ///
    /// # Errors
    ///
    /// Returns an error for files and all reparse points.
    pub fn into_directory(self) -> Result<DirectoryHandle, WindowsError> {
        if self.metadata.kind != NodeKind::Directory {
            return Err(WindowsError::NotDirectory);
        }
        Ok(DirectoryHandle {
            handle: self.handle,
            path: self.path,
            metadata: self.metadata,
        })
    }

    /// Consume an ordinary file handle for streaming reads.
    ///
    /// # Errors
    ///
    /// Returns an error for directories and reparse points.
    pub fn into_file(self) -> Result<File, WindowsError> {
        if self.metadata.kind != NodeKind::RegularFile {
            return Err(WindowsError::Malformed("regular file handle"));
        }
        Ok(File::from(self.handle))
    }
}

pub(crate) fn open_raw(
    path: &VerbatimPath,
    access: u32,
    share: u32,
    flags: u32,
) -> Result<OwnedHandle, WindowsError> {
    // SAFETY: `path` owns a NUL-terminated buffer for the duration of the call. Null security
    // attributes and template handles are permitted, and every successful handle is immediately
    // transferred to `OwnedHandle`.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            share,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io_error("CreateFileW"));
    }
    // SAFETY: `CreateFileW` returned a new, valid, owned handle and no other owner exists.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
}

pub(crate) fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle().cast()
}

fn final_path(handle_value: &OwnedHandle) -> Result<VerbatimPath, WindowsError> {
    let flags = FILE_NAME_NORMALIZED | VOLUME_NAME_DOS;
    // SAFETY: Passing a null output pointer with length zero is the documented sizing query.
    let required =
        unsafe { GetFinalPathNameByHandleW(raw_handle(handle_value), ptr::null_mut(), 0, flags) };
    if required == 0 {
        return Err(io_error("GetFinalPathNameByHandleW"));
    }
    let capacity = usize::try_from(required).map_err(|_| WindowsError::TooLarge("final path"))? + 1;
    if capacity >= 32_767 {
        return Err(WindowsError::TooLarge("final path"));
    }
    let mut buffer = vec![0_u16; capacity];
    // SAFETY: `buffer` is writable for `capacity` UTF-16 units and the handle remains valid.
    let written = unsafe {
        GetFinalPathNameByHandleW(
            raw_handle(handle_value),
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).map_err(|_| WindowsError::TooLarge("final path"))?,
            flags,
        )
    };
    if written == 0
        || usize::try_from(written)
            .ok()
            .is_none_or(|len| len >= buffer.len())
    {
        return Err(io_error("GetFinalPathNameByHandleW"));
    }
    buffer.truncate(usize::try_from(written).expect("validated above"));
    VerbatimPath::from_final_units(buffer).map_err(WindowsError::from)
}

pub(crate) fn final_path_for_mutation(handle: &OwnedHandle) -> Result<VerbatimPath, WindowsError> {
    final_path(handle)
}

fn metadata(handle: &OwnedHandle) -> Result<NodeMetadata, WindowsError> {
    let identity: FILE_ID_INFO = query_fixed(handle, FileIdInfo, "FileIdInfo")?;
    let basic: FILE_BASIC_INFO = query_fixed(handle, FileBasicInfo, "FileBasicInfo")?;
    let standard: FILE_STANDARD_INFO = query_fixed(handle, FileStandardInfo, "FileStandardInfo")?;
    let tagged: FILE_ATTRIBUTE_TAG_INFO =
        query_fixed(handle, FileAttributeTagInfo, "FileAttributeTagInfo")?;
    let is_reparse = tagged.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    let kind = if is_reparse {
        NodeKind::ReparsePoint
    } else if standard.Directory {
        NodeKind::Directory
    } else {
        NodeKind::RegularFile
    };
    Ok(NodeMetadata {
        identity: FileIdentity {
            volume_serial: identity.VolumeSerialNumber,
            file_id: identity.FileId.Identifier,
        },
        kind,
        size: nonnegative(standard.EndOfFile, "file size")?,
        allocation_size: nonnegative(standard.AllocationSize, "allocation size")?,
        link_count: standard.NumberOfLinks,
        attributes: basic.FileAttributes,
        reparse_tag: is_reparse.then_some(tagged.ReparseTag),
        creation_time: basic.CreationTime,
        last_write_time: basic.LastWriteTime,
        change_time: basic.ChangeTime,
    })
}

pub(crate) fn metadata_for_system(handle: &OwnedHandle) -> Result<NodeMetadata, WindowsError> {
    metadata(handle)
}

fn query_fixed<T: Default>(
    handle_value: &OwnedHandle,
    class: i32,
    operation: &'static str,
) -> Result<T, WindowsError> {
    let mut value = T::default();
    // SAFETY: `value` is valid writable storage of exactly the size passed to the API, and the
    // handle remains owned for the duration of the call.
    let success = unsafe {
        GetFileInformationByHandleEx(
            raw_handle(handle_value),
            class,
            ptr::from_mut(&mut value).cast::<c_void>(),
            u32::try_from(size_of::<T>()).expect("Win32 structures fit in u32"),
        )
    };
    if success == 0 {
        return Err(io_error(operation));
    }
    Ok(value)
}

fn enumerate(handle_value: &OwnedHandle) -> Result<Vec<DirectoryEntry>, WindowsError> {
    let mut output = Vec::new();
    let mut first = true;
    loop {
        let mut buffer = vec![0_u8; DIRECTORY_BUFFER];
        let class = if first {
            FileIdExtdDirectoryRestartInfo
        } else {
            FileIdExtdDirectoryInfo
        };
        first = false;
        // SAFETY: `buffer` is writable for the supplied length and the directory handle remains
        // valid. The parser validates every returned offset and length before dereferencing.
        let success = unsafe {
            GetFileInformationByHandleEx(
                raw_handle(handle_value),
                class,
                buffer.as_mut_ptr().cast::<c_void>(),
                u32::try_from(buffer.len()).expect("bounded buffer fits u32"),
            )
        };
        if success == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == i32::try_from(ERROR_NO_MORE_FILES).ok() {
                break;
            }
            return Err(WindowsError::Io {
                operation: "FileIdExtdDirectoryInfo",
                source: error,
            });
        }
        parse_directory_buffer(&buffer, &mut output)?;
    }
    Ok(output)
}

fn parse_directory_buffer(
    buffer: &[u8],
    output: &mut Vec<DirectoryEntry>,
) -> Result<(), WindowsError> {
    const HEADER: usize = 88;
    let mut offset = 0_usize;
    loop {
        let header_end = offset
            .checked_add(HEADER)
            .ok_or(WindowsError::Malformed("directory information"))?;
        if header_end > buffer.len() {
            return Err(WindowsError::Malformed("directory information"));
        }
        let next = read_u32(buffer, offset)?;
        let creation_time = read_i64(buffer, offset + 8)?;
        let last_write_time = read_i64(buffer, offset + 24)?;
        let change_time = read_i64(buffer, offset + 32)?;
        let size = nonnegative(read_i64(buffer, offset + 40)?, "directory entry size")?;
        let allocation_size =
            nonnegative(read_i64(buffer, offset + 48)?, "directory allocation size")?;
        let attributes = read_u32(buffer, offset + 56)?;
        let name_length = usize::try_from(read_u32(buffer, offset + 60)?)
            .map_err(|_| WindowsError::Malformed("directory filename"))?;
        let reparse_tag = read_u32(buffer, offset + 68)?;
        let mut file_id = [0_u8; 16];
        file_id.copy_from_slice(
            buffer
                .get(offset + 72..offset + 88)
                .ok_or(WindowsError::Malformed("directory file ID"))?,
        );
        let name = read_wide(buffer, header_end, name_length, "directory filename")?;
        if name != OsStr::new(".") && name != OsStr::new("..") {
            output.push(DirectoryEntry {
                name,
                file_id,
                attributes,
                reparse_tag: (attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
                    .then_some(reparse_tag),
                size,
                allocation_size,
                creation_time,
                last_write_time,
                change_time,
            });
        }
        if next == 0 {
            return Ok(());
        }
        let next = usize::try_from(next)
            .map_err(|_| WindowsError::Malformed("directory information offset"))?;
        if next < HEADER
            || offset
                .checked_add(next)
                .is_none_or(|next| next >= buffer.len())
        {
            return Err(WindowsError::Malformed("directory information offset"));
        }
        offset += next;
    }
}

fn query_streams(handle_value: &OwnedHandle) -> Result<Vec<StreamInfo>, WindowsError> {
    let mut size = INITIAL_STREAM_BUFFER;
    loop {
        let mut buffer = vec![0_u8; size];
        // SAFETY: `buffer` is valid writable storage for the supplied bounded length.
        let success = unsafe {
            GetFileInformationByHandleEx(
                raw_handle(handle_value),
                FileStreamInfo,
                buffer.as_mut_ptr().cast::<c_void>(),
                u32::try_from(buffer.len()).expect("bounded buffer fits u32"),
            )
        };
        if success != 0 {
            return parse_stream_buffer(&buffer);
        }
        let error = std::io::Error::last_os_error();
        let code = error.raw_os_error();
        if code == i32::try_from(ERROR_MORE_DATA).ok()
            || code == i32::try_from(ERROR_INSUFFICIENT_BUFFER).ok()
        {
            size = size
                .checked_mul(2)
                .filter(|next| *next <= MAX_STREAM_BUFFER)
                .ok_or(WindowsError::TooLarge("stream information"))?;
            continue;
        }
        return Err(WindowsError::Io {
            operation: "FileStreamInfo",
            source: error,
        });
    }
}

fn parse_stream_buffer(buffer: &[u8]) -> Result<Vec<StreamInfo>, WindowsError> {
    const HEADER: usize = 24;
    let mut output = Vec::new();
    let mut offset = 0_usize;
    loop {
        let header_end = offset
            .checked_add(HEADER)
            .ok_or(WindowsError::Malformed("stream information"))?;
        if header_end > buffer.len() {
            return Err(WindowsError::Malformed("stream information"));
        }
        let next = read_u32(buffer, offset)?;
        let name_length = usize::try_from(read_u32(buffer, offset + 4)?)
            .map_err(|_| WindowsError::Malformed("stream name"))?;
        let size = nonnegative(read_i64(buffer, offset + 8)?, "stream size")?;
        let allocation_size =
            nonnegative(read_i64(buffer, offset + 16)?, "stream allocation size")?;
        output.push(StreamInfo {
            name: read_wide(buffer, header_end, name_length, "stream name")?,
            size,
            allocation_size,
        });
        if next == 0 {
            return Ok(output);
        }
        let next = usize::try_from(next)
            .map_err(|_| WindowsError::Malformed("stream information offset"))?;
        if next < HEADER
            || offset
                .checked_add(next)
                .is_none_or(|next| next >= buffer.len())
        {
            return Err(WindowsError::Malformed("stream information offset"));
        }
        offset += next;
    }
}

fn query_reparse(handle_value: &OwnedHandle) -> Result<ReparseKind, WindowsError> {
    let mut buffer = vec![0_u8; MAX_REPARSE_BUFFER];
    let mut returned = 0_u32;
    // SAFETY: The input buffer is absent as required by FSCTL_GET_REPARSE_POINT. `buffer` is
    // writable for its exact length and `returned` is a valid output pointer.
    let success = unsafe {
        DeviceIoControl(
            raw_handle(handle_value),
            FSCTL_GET_REPARSE_POINT,
            ptr::null(),
            0,
            buffer.as_mut_ptr().cast::<c_void>(),
            u32::try_from(buffer.len()).expect("reparse buffer fits u32"),
            ptr::from_mut(&mut returned),
            ptr::null_mut(),
        )
    };
    if success == 0 {
        return Err(io_error("FSCTL_GET_REPARSE_POINT"));
    }
    buffer
        .truncate(usize::try_from(returned).map_err(|_| WindowsError::Malformed("reparse data"))?);
    parse_reparse_buffer(&buffer)
}

fn parse_reparse_buffer(buffer: &[u8]) -> Result<ReparseKind, WindowsError> {
    if buffer.len() < 8 {
        return Err(WindowsError::Malformed("reparse data"));
    }
    let tag = read_u32(buffer, 0)?;
    let data_length = usize::from(read_u16(buffer, 4)?);
    if data_length
        .checked_add(8)
        .is_none_or(|end| end > buffer.len())
    {
        return Err(WindowsError::Malformed("reparse data length"));
    }
    if tag != IO_REPARSE_TAG_SYMLINK {
        return Ok(ReparseKind::Other(tag));
    }
    if data_length < 12 || buffer.len() < 20 {
        return Err(WindowsError::Malformed("symbolic-link reparse data"));
    }
    let substitute_offset = usize::from(read_u16(buffer, 8)?);
    let substitute_length = usize::from(read_u16(buffer, 10)?);
    let print_offset = usize::from(read_u16(buffer, 12)?);
    let print_length = usize::from(read_u16(buffer, 14)?);
    let flags = read_u32(buffer, 16)?;
    let path_buffer = &buffer[20..8 + data_length];
    Ok(ReparseKind::SymbolicLink(SymbolicLinkData {
        substitute_name: read_wide(
            path_buffer,
            substitute_offset,
            substitute_length,
            "symbolic-link substitute name",
        )?,
        print_name: read_wide(
            path_buffer,
            print_offset,
            print_length,
            "symbolic-link print name",
        )?,
        flags,
    }))
}

fn read_u16(buffer: &[u8], offset: usize) -> Result<u16, WindowsError> {
    let bytes: [u8; 2] = buffer
        .get(offset..offset + 2)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(WindowsError::Malformed("native record"))?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(buffer: &[u8], offset: usize) -> Result<u32, WindowsError> {
    let bytes: [u8; 4] = buffer
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(WindowsError::Malformed("native record"))?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_i64(buffer: &[u8], offset: usize) -> Result<i64, WindowsError> {
    let bytes: [u8; 8] = buffer
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(WindowsError::Malformed("native record"))?;
    Ok(i64::from_le_bytes(bytes))
}

fn read_wide(
    buffer: &[u8],
    offset: usize,
    byte_length: usize,
    name: &'static str,
) -> Result<OsString, WindowsError> {
    if byte_length % 2 != 0 {
        return Err(WindowsError::Malformed(name));
    }
    let end = offset
        .checked_add(byte_length)
        .ok_or(WindowsError::Malformed(name))?;
    let bytes = buffer
        .get(offset..end)
        .ok_or(WindowsError::Malformed(name))?;
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Ok(OsString::from_wide(&units))
}

fn nonnegative(value: i64, name: &'static str) -> Result<u64, WindowsError> {
    u64::try_from(value).map_err(|_| WindowsError::Malformed(name))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;

    use super::*;

    #[test]
    fn enumerates_and_reopens_exact_file_identity() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file.txt"), b"fence").unwrap();
        fs::create_dir(temp.path().join("directory")).unwrap();

        let root = RootHandle::open(temp.path()).unwrap();
        let entries = root.directory().entries().unwrap();
        assert_eq!(entries.len(), 2);
        for entry in entries {
            let node = root.directory().open_child(&entry).unwrap();
            assert_eq!(node.metadata().identity.file_id, entry.file_id);
            node.verify_path_identity().unwrap();
        }
    }

    #[test]
    fn detects_named_streams() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("streams.txt");
        fs::write(&path, b"default").unwrap();
        let mut named = File::create(format!("{}:fence", path.display())).unwrap();
        named.write_all(b"named").unwrap();
        drop(named);

        let root = RootHandle::open(temp.path()).unwrap();
        let entry = root
            .directory()
            .entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.name == "streams.txt")
            .unwrap();
        let streams = root
            .directory()
            .open_child(&entry)
            .unwrap()
            .streams()
            .unwrap();
        assert!(streams.iter().any(StreamInfo::is_default_data_stream));
        assert!(
            streams
                .iter()
                .any(|stream| !stream.is_default_data_stream())
        );
    }

    #[test]
    fn classifies_symbolic_link_without_reading_target() {
        use std::os::windows::fs::symlink_file;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("sentinel");
        fs::write(&target, b"outside").unwrap();
        let link = temp.path().join("outside-link");
        if let Err(error) = symlink_file(&target, &link) {
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("cannot create symlink fixture: {error}");
        }

        let root = RootHandle::open(temp.path()).unwrap();
        let entry = root.directory().entries().unwrap().pop().unwrap();
        let node = root.directory().open_child(&entry).unwrap();
        assert!(matches!(
            node.reparse_kind().unwrap(),
            Some(ReparseKind::SymbolicLink(_))
        ));
        assert_eq!(fs::read(&target).unwrap(), b"outside");
    }
}
