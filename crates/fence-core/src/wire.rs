use std::io::Cursor;

use ciborium::value::Value;
use thiserror::Error;

use crate::{NativeRelativePath, PathEncoding, PathError};

const PATH_RECORD_TAG: u64 = 0x4150;
const PATH_WIRE_VERSION: u64 = 1;
const MAX_ENCODED_PATH_RECORD: usize = 2 * 1_048_576;

/// Encode a native relative path using the stable path-record v1 CBOR layout.
///
/// # Errors
///
/// Returns [`WireError`] if serialization fails or the encoded record exceeds its limit.
pub fn encode_path(path: &NativeRelativePath) -> Result<Vec<u8>, WireError> {
    let encoding = match path.encoding() {
        PathEncoding::UnixBytes => 1_u64,
        PathEncoding::WindowsWtf16Le => 2,
    };
    let components = path
        .components()
        .iter()
        .cloned()
        .map(Value::Bytes)
        .collect();
    let value = Value::Array(vec![
        Value::Integer(PATH_RECORD_TAG.into()),
        Value::Integer(PATH_WIRE_VERSION.into()),
        Value::Integer(encoding.into()),
        Value::Array(components),
    ]);

    let mut output = Vec::new();
    ciborium::ser::into_writer(&value, &mut output)
        .map_err(|error| WireError::Encode(error.to_string()))?;
    if output.len() > MAX_ENCODED_PATH_RECORD {
        return Err(WireError::EncodedTooLarge);
    }
    Ok(output)
}

/// Decode and validate a stable path-record v1 CBOR value.
///
/// # Errors
///
/// Returns [`WireError`] for malformed, unsupported, oversized, or unsafe records.
pub fn decode_path(bytes: &[u8]) -> Result<NativeRelativePath, WireError> {
    if bytes.len() > MAX_ENCODED_PATH_RECORD {
        return Err(WireError::EncodedTooLarge);
    }

    let value: Value = ciborium::de::from_reader(Cursor::new(bytes))
        .map_err(|error| WireError::Decode(error.to_string()))?;
    let Value::Array(fields) = value else {
        return Err(WireError::Shape);
    };
    let [tag, version, encoding, components] = fields.as_slice() else {
        return Err(WireError::Shape);
    };

    if value_as_u64(tag)? != PATH_RECORD_TAG {
        return Err(WireError::WrongTag);
    }
    let version = value_as_u64(version)?;
    if version != PATH_WIRE_VERSION {
        return Err(WireError::UnsupportedVersion(version));
    }
    let encoding = match value_as_u64(encoding)? {
        1 => PathEncoding::UnixBytes,
        2 => PathEncoding::WindowsWtf16Le,
        value => return Err(WireError::UnknownEncoding(value)),
    };
    let Value::Array(components) = components else {
        return Err(WireError::Shape);
    };
    if components.len() > NativeRelativePath::MAX_COMPONENTS {
        return Err(WireError::Path(PathError::TooManyComponents));
    }
    let mut decoded = Vec::with_capacity(components.len());
    for component in components {
        let Value::Bytes(component) = component else {
            return Err(WireError::Shape);
        };
        decoded.push(component.clone());
    }
    NativeRelativePath::new(encoding, decoded).map_err(WireError::Path)
}

/// Stable, domain-separated identity of the encoded path record.
pub fn path_record_id(encoded: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"anchor:path:v1\0");
    hasher.update(encoded);
    *hasher.finalize().as_bytes()
}

fn value_as_u64(value: &Value) -> Result<u64, WireError> {
    let Value::Integer(integer) = value else {
        return Err(WireError::Shape);
    };
    u64::try_from(*integer).map_err(|_| WireError::Shape)
}

#[derive(Debug, Error)]
pub enum WireError {
    #[error("failed to encode CBOR: {0}")]
    Encode(String),
    #[error("failed to decode CBOR: {0}")]
    Decode(String),
    #[error("record does not match the required path-v1 shape")]
    Shape,
    #[error("record has the wrong type tag")]
    WrongTag,
    #[error("unsupported path record version {0}")]
    UnsupportedVersion(u64),
    #[error("unknown path encoding tag {0}")]
    UnknownEncoding(u64),
    #[error("encoded path record exceeds its size limit")]
    EncodedTooLarge,
    #[error(transparent)]
    Path(#[from] PathError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_is_deterministic() {
        let path = NativeRelativePath::new(
            PathEncoding::UnixBytes,
            vec![b"src".to_vec(), b"main.rs".to_vec()],
        )
        .unwrap();
        let first = encode_path(&path).unwrap();
        let second = encode_path(&path).unwrap();
        assert_eq!(first, second);
        assert_eq!(decode_path(&first).unwrap(), path);
        assert_eq!(
            hex(&path_record_id(&first)),
            "e0d0fbae7a53813ebb9b9eb1877e2db99c5a59cd14d09ad7342883f3845d1b2a"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;

        bytes.iter().fold(
            String::with_capacity(bytes.len() * 2),
            |mut output, byte| {
                write!(output, "{byte:02x}").unwrap();
                output
            },
        )
    }
}
