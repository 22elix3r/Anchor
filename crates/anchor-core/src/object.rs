use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tempfile::NamedTempFile;
use thiserror::Error;

const OBJECT_MAGIC: &[u8; 8] = b"ANCHOBJ1";
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
        let root = root.as_ref().to_path_buf();
        create_private_dir(&root)?;
        create_private_dir(&root.join("objects"))?;
        create_private_dir(&root.join("objects").join("b3"))?;
        Ok(Self {
            root,
            compression_level,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Store all raw bytes from a reader.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for input I/O, compression, durability, or collision failures.
    pub fn put(&self, mut reader: impl Read) -> Result<(ObjectId, u64), StoreError> {
        let staging = self.root.join("objects").join("b3");
        let mut temp = NamedTempFile::new_in(&staging)?;
        temp.as_file_mut().write_all(&[0_u8; HEADER_LEN])?;

        let mut hasher = blake3::Hasher::new();
        let mut raw_len = 0_u64;
        {
            let mut encoder =
                zstd::stream::write::Encoder::new(temp.as_file_mut(), self.compression_level)?;
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
        temp.as_file_mut().seek(SeekFrom::Start(0))?;
        temp.as_file_mut().write_all(&encode_header(id, raw_len))?;
        temp.as_file_mut().sync_all()?;

        let final_path = self.object_path(id);
        let parent = final_path.parent().ok_or(StoreError::InvalidStoreLayout)?;
        create_private_dir(parent)?;

        match temp.persist_noclobber(&final_path) {
            Ok(file) => {
                file.sync_all()?;
                sync_directory(parent)?;
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                drop(error.file);
                self.verify(id, raw_len)?;
            }
            Err(error) => return Err(StoreError::Io(error.error)),
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
        let mut file = File::open(self.object_path(id))?;
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
        let hex = id.to_hex();
        self.root
            .join("objects")
            .join("b3")
            .join(&hex[..2])
            .join(format!("{}.zst", &hex[2..]))
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

fn hex_nibble(byte: u8) -> Result<u8, StoreError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(StoreError::InvalidObjectId),
    }
}

fn create_private_dir(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg_attr(windows, allow(clippy::unnecessary_wraps))]
fn sync_directory(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(windows)]
    {
        let _ = path;
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid object ID")]
    InvalidObjectId,
    #[error("object exceeds the supported length")]
    ObjectTooLarge,
    #[error("invalid store directory layout")]
    InvalidStoreLayout,
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
}
