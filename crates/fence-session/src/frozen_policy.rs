use std::fmt;
use std::io::{self, Cursor, Write};

use fence_git::FrozenGitPolicy;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tempfile::NamedTempFile;

use crate::{SessionError, SessionStore, bounded_read, private_directory};

const MAX_POLICY_BYTES: usize = 32 * 1024 * 1024;

/// BLAKE3 identity of the canonical CBOR bytes for one complete frozen policy.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FrozenPolicyId([u8; 32]);

impl FrozenPolicyId {
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
        use fmt::Write as _;

        self.0
            .iter()
            .fold(String::with_capacity(64), |mut output, byte| {
                write!(output, "{byte:02x}").expect("writing to String cannot fail");
                output
            })
    }
}

impl fmt::Debug for FrozenPolicyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("FrozenPolicyId")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for FrozenPolicyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl Serialize for FrozenPolicyId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for FrozenPolicyId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = serde_bytes::ByteBuf::deserialize(deserializer)?;
        let bytes: [u8; 32] = bytes
            .as_ref()
            .try_into()
            .map_err(|_| serde::de::Error::custom("frozen-policy ID must contain 32 bytes"))?;
        Ok(Self(bytes))
    }
}

impl SessionStore {
    pub(crate) fn put_frozen_policy(
        &self,
        policy: &FrozenGitPolicy,
    ) -> Result<FrozenPolicyId, SessionError> {
        policy.validate()?;
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(policy, &mut bytes)
            .map_err(|error| SessionError::Encode(error.to_string()))?;
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(SessionError::FrozenPolicyTooLarge);
        }
        let id = FrozenPolicyId::from_bytes(*blake3::hash(&bytes).as_bytes());
        let path = self.frozen_policy_path(id);
        let parent = path.parent().ok_or(SessionError::InvalidLayout)?;
        private_directory(parent)?;
        if path.exists() {
            self.load_frozen_policy(id)?;
            return Ok(id);
        }
        let mut file = NamedTempFile::new_in(parent)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        match file.persist_noclobber(&path) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                self.load_frozen_policy(id)?;
            }
            Err(error) => return Err(error.error.into()),
        }
        Ok(id)
    }

    pub(crate) fn load_frozen_policy(
        &self,
        id: FrozenPolicyId,
    ) -> Result<FrozenGitPolicy, SessionError> {
        let bytes = bounded_read(&self.frozen_policy_path(id), MAX_POLICY_BYTES)?;
        if blake3::hash(&bytes).as_bytes() != id.as_bytes() {
            return Err(SessionError::FrozenPolicyIdentityMismatch(id));
        }
        let mut cursor = Cursor::new(&bytes);
        let policy: FrozenGitPolicy = ciborium::de::from_reader(&mut cursor)
            .map_err(|error| SessionError::Decode(error.to_string()))?;
        if usize::try_from(cursor.position()).ok() != Some(bytes.len()) {
            return Err(SessionError::FrozenPolicyTrailingBytes);
        }
        policy.validate()?;
        Ok(policy)
    }

    pub(crate) fn frozen_policy_path(&self, id: FrozenPolicyId) -> std::path::PathBuf {
        let hex = id.to_hex();
        self.root
            .join("policies")
            .join("b3")
            .join(&hex[..2])
            .join(format!("{}.cbor", &hex[2..]))
    }
}
