use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::{StoreFs, StoreFsError};

const OBJECT_MAGIC: &[u8; 8] = b"ANCHOBJ1";
const STORE_MARKER_NAME: &str = "fence-store";
const STORE_MARKER_BYTES: &[u8] = b"FENCE_STORE\n1\n";
const OBJECT_CODEC_ZSTD: u8 = 1;
const HEADER_LEN: usize = 56;
const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

/// BLAKE3 identity of uncompressed object bytes.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    #[must_use]
    pub fn from_raw(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn to_hex(self) -> String {
        use std::fmt::Write as _;

        self.0
            .iter()
            .fold(String::with_capacity(64), |mut output, byte| {
                write!(output, "{byte:02x}").expect("writing to String cannot fail");
                output
            })
    }

    /// Parse a hexadecimal object ID.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidObjectId`] unless the input is exactly 64 hex digits.
    pub fn from_hex(value: &str) -> Result<Self, StoreError> {
        if value.len() != 64 {
            return Err(StoreError::InvalidObjectId);
        }
        let mut output = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            output[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
        }
        Ok(Self(output))
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ObjectId")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl Serialize for ObjectId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = serde_bytes::ByteBuf::deserialize(deserializer)?;
        let bytes: [u8; 32] = bytes
            .as_ref()
            .try_into()
            .map_err(|_| serde::de::Error::custom("object ID must contain exactly 32 bytes"))?;
        Ok(Self(bytes))
    }
}

/// Immutable, compressed object storage rooted at a private directory.
#[derive(Debug)]
pub struct ObjectStore {
    root: PathBuf,
    filesystem: StoreFs,
    compression_level: i32,
}

impl ObjectStore {
    /// Open or create an object store.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if directories cannot be created or secured.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_compression(root, DEFAULT_COMPRESSION_LEVEL)
    }

    /// Open a store with an explicit Zstandard level.
    ///
    /// Object identity is independent of this value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if directories cannot be created or secured.
    pub fn open_with_compression(
        root: impl AsRef<Path>,
        compression_level: i32,
    ) -> Result<Self, StoreError> {
        let filesystem = StoreFs::open_ambient(root)?;
        Self::from_filesystem(filesystem, compression_level)
    }

    /// Open a store below an already trusted directory boundary.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for unsafe components, ownership, permissions, or I/O failures.
    pub fn open_beneath(
        trusted_parent: impl AsRef<Path>,
        relative_root: impl AsRef<Path>,
    ) -> Result<Self, StoreError> {
        let filesystem = StoreFs::open_beneath(trusted_parent, relative_root)?;
        Self::from_filesystem(filesystem, DEFAULT_COMPRESSION_LEVEL)
    }

    fn from_filesystem(filesystem: StoreFs, compression_level: i32) -> Result<Self, StoreError> {
        ensure_store_marker(&filesystem)?;
        filesystem.ensure_dir("objects/b3")?;
        let root = filesystem.root().to_path_buf();
        Ok(Self {
            root,
            filesystem,
            compression_level,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn filesystem(&self) -> &StoreFs {
        &self.filesystem
    }

    /// Store all raw bytes from a reader.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for input I/O, compression, durability, or collision failures.
    pub fn put(&self, mut reader: impl Read) -> Result<(ObjectId, u64), StoreError> {
        let mut temp = self.filesystem.temporary_file("objects/b3")?;
        temp.write_all(&[0_u8; HEADER_LEN])?;

        let mut hasher = blake3::Hasher::new();
        let mut raw_len = 0_u64;
        {
            let mut encoder = zstd::stream::write::Encoder::new(&mut temp, self.compression_level)?;
            let mut buffer = vec![0_u8; 1024 * 1024];
            loop {
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                raw_len = raw_len
                    .checked_add(u64::try_from(read).map_err(|_| StoreError::ObjectTooLarge)?)
                    .ok_or(StoreError::ObjectTooLarge)?;
                hasher.update(&buffer[..read]);
                encoder.write_all(&buffer[..read])?;
            }
            encoder.finish()?;
        }

        let id = ObjectId::from_bytes(*hasher.finalize().as_bytes());
        temp.seek(SeekFrom::Start(0))?;
        temp.write_all(&encode_header(id, raw_len))?;
        temp.file_mut().sync_all()?;

        let final_relative = Self::object_relative_path(id);
        let parent = final_relative
            .parent()
            .ok_or(StoreError::InvalidStoreLayout)?;
        let final_directory = self.filesystem.ensure_dir(parent)?;
        let destination = final_relative
            .file_name()
            .ok_or(StoreError::InvalidStoreLayout)?;

        match temp.persist_noclobber_in(&final_directory, destination) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                self.verify(id, raw_len)?;
            }
            Err(error) => return Err(StoreError::Io(error)),
        }
        Ok((id, raw_len))
    }

    /// Store a byte slice.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the immutable object cannot be safely published.
    pub fn put_bytes(&self, bytes: &[u8]) -> Result<ObjectId, StoreError> {
        self.put(bytes).map(|(id, _)| id)
    }

    /// Read and verify an object into memory with a caller-supplied raw-size limit.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for missing, malformed, oversized, or corrupt objects.
    pub fn get(&self, id: ObjectId, max_raw_len: u64) -> Result<Vec<u8>, StoreError> {
        let capacity = usize::try_from(max_raw_len.min(16 * 1024 * 1024))
            .map_err(|_| StoreError::ObjectTooLarge)?;
        let mut output = Vec::with_capacity(capacity);
        self.copy_verified(id, max_raw_len, &mut output)?;
        Ok(output)
    }

    /// Stream a verified object to a destination.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for missing, malformed, oversized, corrupt, or output failures.
    pub fn copy_verified(
        &self,
        id: ObjectId,
        max_raw_len: u64,
        mut output: impl Write,
    ) -> Result<u64, StoreError> {
        let relative = Self::object_relative_path(id);
        let mut file = self.filesystem.open_file(&relative)?;
        let opened_metadata = file.metadata()?;
        let header = read_header(&mut file)?;
        if header.id != id {
            return Err(StoreError::IdentityMismatch {
                expected: id,
                actual: header.id,
            });
        }
        if header.raw_len > max_raw_len {
            return Err(StoreError::LimitExceeded {
                declared: header.raw_len,
                maximum: max_raw_len,
            });
        }

        let mut decoder = zstd::stream::read::Decoder::new(file)?;
        let mut hasher = blake3::Hasher::new();
        let mut actual_len = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = decoder.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            actual_len = actual_len
                .checked_add(u64::try_from(read).map_err(|_| StoreError::ObjectTooLarge)?)
                .ok_or(StoreError::ObjectTooLarge)?;
            if actual_len > header.raw_len {
                return Err(StoreError::LengthMismatch {
                    declared: header.raw_len,
                    actual: actual_len,
                });
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        if actual_len != header.raw_len {
            return Err(StoreError::LengthMismatch {
                declared: header.raw_len,
                actual: actual_len,
            });
        }
        let actual_id = ObjectId::from_bytes(*hasher.finalize().as_bytes());
        if actual_id != id {
            return Err(StoreError::IdentityMismatch {
                expected: id,
                actual: actual_id,
            });
        }
        if !self
            .filesystem
            .file_identity_matches(relative, &opened_metadata)?
        {
            return Err(StoreError::UnsafeStoreFile(self.object_path(id)));
        }
        Ok(actual_len)
    }

    /// Verify an object's envelope, length, decompression, and content hash.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the object is absent, corrupt, or has an unexpected size.
    pub fn verify(&self, id: ObjectId, expected_raw_len: u64) -> Result<(), StoreError> {
        let actual = self.copy_verified(id, expected_raw_len, io::sink())?;
        if actual != expected_raw_len {
            return Err(StoreError::LengthMismatch {
                declared: expected_raw_len,
                actual,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn object_path(&self, id: ObjectId) -> PathBuf {
        self.root.join(Self::object_relative_path(id))
    }

    fn object_relative_path(id: ObjectId) -> PathBuf {
        let hex = id.to_hex();
        PathBuf::from("objects")
            .join("b3")
            .join(&hex[..2])
            .join(format!("{}.zst", &hex[2..]))
    }
}

fn ensure_store_marker(filesystem: &StoreFs) -> Result<(), StoreError> {
    match filesystem.read_bounded(STORE_MARKER_NAME, 64) {
        Ok(bytes) if bytes == STORE_MARKER_BYTES => return Ok(()),
        Ok(_) => return Err(StoreError::InvalidStoreMarker),
        Err(StoreFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if filesystem.open_dir("")?.entries()?.next().is_some() {
        return Err(StoreError::UnrecognizedStore);
    }
    let mut temporary = filesystem.temporary_file("")?;
    temporary.write_all(STORE_MARKER_BYTES)?;
    match temporary.persist_noclobber(STORE_MARKER_NAME) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let bytes = filesystem.read_bounded(STORE_MARKER_NAME, 64)?;
            if bytes == STORE_MARKER_BYTES {
                Ok(())
            } else {
                Err(StoreError::InvalidStoreMarker)
            }
        }
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Copy, Debug)]
struct ObjectHeader {
    id: ObjectId,
    raw_len: u64,
}

fn encode_header(id: ObjectId, raw_len: u64) -> [u8; HEADER_LEN] {
    let mut header = [0_u8; HEADER_LEN];
    header[..8].copy_from_slice(OBJECT_MAGIC);
    header[8] = OBJECT_CODEC_ZSTD;
    header[16..24].copy_from_slice(&raw_len.to_le_bytes());
    header[24..56].copy_from_slice(id.as_bytes());
    header
}

fn read_header(reader: &mut impl Read) -> Result<ObjectHeader, StoreError> {
    let mut header = [0_u8; HEADER_LEN];
    reader.read_exact(&mut header)?;
    if &header[..8] != OBJECT_MAGIC {
        return Err(StoreError::BadMagic);
    }
    if header[8] != OBJECT_CODEC_ZSTD {
        return Err(StoreError::UnknownCodec(header[8]));
    }
    if header[9..16].iter().any(|byte| *byte != 0) {
        return Err(StoreError::MalformedHeader);
    }
    let raw_len = u64::from_le_bytes(
        header[16..24]
            .try_into()
            .map_err(|_| StoreError::MalformedHeader)?,
    );
    let id = ObjectId::from_bytes(
        header[24..56]
            .try_into()
            .map_err(|_| StoreError::MalformedHeader)?,
    );
    Ok(ObjectHeader { id, raw_len })
}

#[cfg(feature = "fuzzing")]
/// Decode and verify one in-memory object envelope under a fixed raw-size limit.
///
/// # Errors
///
/// Returns [`StoreError`] for malformed, oversized, corrupt, or mismatched input.
pub fn decode_object_envelope_for_fuzzing(bytes: &[u8]) -> Result<(), StoreError> {
    const FUZZ_RAW_LIMIT: u64 = 16 * 1024 * 1024;

    let mut cursor = io::Cursor::new(bytes);
    let header = read_header(&mut cursor)?;
    if header.raw_len > FUZZ_RAW_LIMIT {
        return Err(StoreError::LimitExceeded {
            declared: header.raw_len,
            maximum: FUZZ_RAW_LIMIT,
        });
    }
    let mut decoder = zstd::stream::read::Decoder::new(cursor)?;
    let mut hasher = blake3::Hasher::new();
    let mut actual = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = decoder.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        actual = actual
            .checked_add(u64::try_from(read).map_err(|_| StoreError::ObjectTooLarge)?)
            .ok_or(StoreError::ObjectTooLarge)?;
        if actual > header.raw_len {
            return Err(StoreError::LengthMismatch {
                declared: header.raw_len,
                actual,
            });
        }
        hasher.update(&buffer[..read]);
    }
    if actual != header.raw_len {
        return Err(StoreError::LengthMismatch {
            declared: header.raw_len,
            actual,
        });
    }
    let actual_id = ObjectId::from_bytes(*hasher.finalize().as_bytes());
    if actual_id != header.id {
        return Err(StoreError::IdentityMismatch {
            expected: header.id,
            actual: actual_id,
        });
    }
    Ok(())
}

fn hex_nibble(byte: u8) -> Result<u8, StoreError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(StoreError::InvalidObjectId),
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    StoreFs(#[from] StoreFsError),
    #[cfg(windows)]
    #[error(transparent)]
    Windows(#[from] fence_windows::WindowsError),
    #[error("invalid object ID")]
    InvalidObjectId,
    #[error("object exceeds the supported length")]
    ObjectTooLarge,
    #[error("invalid store directory layout")]
    InvalidStoreLayout,
    #[error("store directory is nonempty but has no Fence product marker")]
    UnrecognizedStore,
    #[error("store has an invalid Fence product or layout marker")]
    InvalidStoreMarker,
    #[error("Fence store directory is a symlink or non-directory: {0}")]
    UnsafeStoreDirectory(PathBuf),
    #[cfg(unix)]
    #[error(
        "Fence store directory {path} belongs to uid {actual_uid}, expected effective uid {expected_uid}"
    )]
    StoreOwnershipMismatch {
        path: PathBuf,
        expected_uid: u32,
        actual_uid: u32,
    },
    #[error("Fence store directory permissions could not be restricted: {0}")]
    UnsafeStorePermissions(PathBuf),
    #[error("Fence store object is a symlink or non-file: {0}")]
    UnsafeStoreFile(PathBuf),
    #[error("object has an invalid magic header")]
    BadMagic,
    #[error("object uses unknown codec {0}")]
    UnknownCodec(u8),
    #[error("object header is malformed")]
    MalformedHeader,
    #[error("object declares {declared} bytes, exceeding limit {maximum}")]
    LimitExceeded { declared: u64, maximum: u64 },
    #[error("object declares {declared} bytes but decoded {actual}")]
    LengthMismatch { declared: u64, actual: u64 },
    #[error("object identity mismatch: expected {expected}, found {actual}")]
    IdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_id_hex_round_trip() {
        let id = ObjectId::from_raw(b"hello");
        assert_eq!(ObjectId::from_hex(&id.to_hex()).unwrap(), id);
    }

    #[test]
    fn compression_level_does_not_change_identity() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let first = ObjectStore::open_with_compression(first_root.path(), 1).unwrap();
        let second = ObjectStore::open_with_compression(second_root.path(), 9).unwrap();
        let bytes = b"the same raw bytes, regardless of compression";
        let first_id = first.put_bytes(bytes).unwrap();
        let second_id = second.put_bytes(bytes).unwrap();
        assert_eq!(first_id, second_id);
        assert_eq!(first.get(first_id, 1024).unwrap(), bytes);
        assert_eq!(second.get(second_id, 1024).unwrap(), bytes);
    }

    #[test]
    fn repeated_put_deduplicates() {
        let root = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(root.path()).unwrap();
        let first = store.put_bytes(b"duplicate").unwrap();
        let second = store.put_bytes(b"duplicate").unwrap();
        assert_eq!(first, second);
        assert_eq!(store.get(first, 1024).unwrap(), b"duplicate");
    }

    #[test]
    fn raw_limit_is_checked_before_decode_output() {
        let root = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(root.path()).unwrap();
        let id = store.put_bytes(&[42_u8; 1024]).unwrap();
        assert!(matches!(
            store.get(id, 100),
            Err(StoreError::LimitExceeded { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn store_root_symlink_is_refused() {
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = parent.path().join("store");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        assert!(ObjectStore::open(&link).is_err());
    }

    #[test]
    fn nonempty_unmarked_store_is_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("legacy-record"), b"opaque").unwrap();

        assert!(matches!(
            ObjectStore::open(root.path()),
            Err(StoreError::UnrecognizedStore)
        ));
    }

    #[test]
    fn invalid_store_marker_is_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(STORE_MARKER_NAME), b"not-fence").unwrap();

        assert!(matches!(
            ObjectStore::open(root.path()),
            Err(StoreError::InvalidStoreMarker)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn object_symlink_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let store = ObjectStore::open(root.path().join("store")).unwrap();
        let id = ObjectId::from_raw(b"outside");
        let path = store.object_path(id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();

        assert!(store.get(id, 1024).is_err());
    }
}
