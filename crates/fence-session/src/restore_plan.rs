use std::fmt;
use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io::{self, Cursor, Write};
use std::path::PathBuf;

use fence_core::{
    ManifestEntry, ManifestId, ManifestNode, MetadataObservation, NativeRelativePath, NativeString,
    ObjectId, SafetyObservations, WindowsSymlinkKind,
};
use fence_git::{IndexCapture, RepositoryState};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::{SessionError, SessionId, SessionStore, bounded_read, private_directory};

const PLAN_TAG: u64 = 0x4152_504c_414e_5631;
const PLAN_SCHEMA: u16 = 2;
const MAX_PLAN_BYTES: usize = 32 * 1024 * 1024;
const MAX_PLAN_ITEMS: usize = 250_000;

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RestorePlanId([u8; 32]);

impl RestorePlanId {
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

impl fmt::Debug for RestorePlanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RestorePlanId")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for RestorePlanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl Serialize for RestorePlanId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for RestorePlanId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = serde_bytes::ByteBuf::deserialize(deserializer)?;
        let bytes: [u8; 32] = bytes
            .as_ref()
            .try_into()
            .map_err(|_| serde::de::Error::custom("restore-plan ID must contain 32 bytes"))?;
        Ok(Self(bytes))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RestorePlanRecord {
    tag: u64,
    schema: u16,
    pub session_id: SessionId,
    pub worktree_root: NativeString,
    pub worktree_key: String,
    pub repository: RepositoryState,
    pub operation: PlanOperation,
}

impl RestorePlanRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn worktree(
        session_id: SessionId,
        worktree_root: NativeString,
        worktree_key: String,
        repository: RepositoryState,
        base_manifest: ManifestId,
        session_manifest: ManifestId,
        current_manifest: ManifestId,
        items: Vec<PlanItem>,
    ) -> Result<Self, RestorePlanError> {
        let record = Self {
            tag: PLAN_TAG,
            schema: PLAN_SCHEMA,
            session_id,
            worktree_root,
            worktree_key,
            repository,
            operation: PlanOperation::Worktree {
                base_manifest,
                session_manifest,
                current_manifest,
                items,
            },
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn index(
        session_id: SessionId,
        worktree_root: NativeString,
        worktree_key: String,
        repository: RepositoryState,
        index_path: NativeString,
        expected: IndexCapture,
        desired: IndexCapture,
    ) -> Result<Self, RestorePlanError> {
        let record = Self {
            tag: PLAN_TAG,
            schema: PLAN_SCHEMA,
            session_id,
            worktree_root,
            worktree_key,
            repository,
            operation: PlanOperation::Index {
                index_path,
                expected,
                desired,
            },
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, RestorePlanError> {
        self.validate()?;
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(self, &mut bytes)
            .map_err(|error| RestorePlanError::Encode(error.to_string()))?;
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(RestorePlanError::TooLarge);
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, RestorePlanError> {
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(RestorePlanError::TooLarge);
        }
        let mut cursor = Cursor::new(bytes);
        let record: Self = ciborium::de::from_reader(&mut cursor)
            .map_err(|error| RestorePlanError::Decode(error.to_string()))?;
        if usize::try_from(cursor.position()).ok() != Some(bytes.len()) {
            return Err(RestorePlanError::TrailingBytes);
        }
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn id(&self) -> Result<RestorePlanId, RestorePlanError> {
        let bytes = self.encode()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(match self.schema {
            1 => b"anchor:restore-plan:v1\0",
            PLAN_SCHEMA => b"anchor:restore-plan:v2\0",
            schema => return Err(RestorePlanError::UnsupportedSchema(schema)),
        });
        hasher.update(&bytes);
        Ok(RestorePlanId::from_bytes(*hasher.finalize().as_bytes()))
    }

    fn validate(&self) -> Result<(), RestorePlanError> {
        if self.tag != PLAN_TAG {
            return Err(RestorePlanError::WrongTag);
        }
        if !matches!(self.schema, 1 | PLAN_SCHEMA) {
            return Err(RestorePlanError::UnsupportedSchema(self.schema));
        }
        if self.worktree_key.is_empty() {
            return Err(RestorePlanError::EmptyWorktreeKey);
        }
        if let PlanOperation::Worktree { items, .. } = &self.operation {
            if items.is_empty() || items.len() > MAX_PLAN_ITEMS {
                return Err(RestorePlanError::InvalidItemCount);
            }
            let mut paths = std::collections::BTreeSet::new();
            for item in items {
                if !paths.insert(item.path.clone()) {
                    return Err(RestorePlanError::DuplicatePath);
                }
                if item.expected.path_encoding() != item.path.encoding()
                    || item.desired.path_encoding() != item.path.encoding()
                {
                    return Err(RestorePlanError::MixedPathEncoding);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn referenced_manifests(&self) -> impl Iterator<Item = ManifestId> {
        let manifests = match &self.operation {
            PlanOperation::Worktree {
                base_manifest,
                session_manifest,
                current_manifest,
                ..
            } => vec![*base_manifest, *session_manifest, *current_manifest],
            PlanOperation::Index { .. } => Vec::new(),
        };
        manifests.into_iter()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum PlanOperation {
    Worktree {
        base_manifest: ManifestId,
        session_manifest: ManifestId,
        current_manifest: ManifestId,
        items: Vec<PlanItem>,
    },
    Index {
        index_path: NativeString,
        expected: IndexCapture,
        desired: IndexCapture,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlanItem {
    pub path: NativeRelativePath,
    pub expected: PlanPresence,
    pub desired: PlanPresence,
    pub proof: PlanProof,
}

impl PlanItem {
    pub(crate) fn exact(
        path: NativeRelativePath,
        expected: Option<&ManifestEntry>,
        desired: Option<&ManifestEntry>,
    ) -> Self {
        Self {
            path,
            expected: PlanPresence::from_entry(expected),
            desired: PlanPresence::from_entry(desired),
            proof: PlanProof::Exact,
        }
    }

    pub(crate) fn merged(
        path: NativeRelativePath,
        expected: Option<&ManifestEntry>,
        desired: &ManifestEntry,
        base: ObjectId,
        session: ObjectId,
        current: ObjectId,
        merged: ObjectId,
    ) -> Self {
        Self {
            path,
            expected: PlanPresence::from_entry(expected),
            desired: PlanPresence::from_entry(Some(desired)),
            proof: PlanProof::CleanTextMerge {
                base,
                session,
                current,
                merged,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum PlanProof {
    Exact,
    CleanTextMerge {
        base: ObjectId,
        session: ObjectId,
        current: ObjectId,
        merged: ObjectId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum PlanPresence {
    Absent { encoding: u8 },
    Present(PlanNode, PlanSafety),
}

impl PlanPresence {
    pub(crate) fn from_entry(entry: Option<&ManifestEntry>) -> Self {
        entry.map_or(
            Self::Absent {
                encoding: host_encoding_tag(),
            },
            |entry| {
                Self::Present(
                    PlanNode::from_entry(entry),
                    PlanSafety::from_observations(&entry.safety),
                )
            },
        )
    }

    pub(crate) fn to_entry(&self, path: &NativeRelativePath) -> Option<ManifestEntry> {
        match self {
            Self::Absent { .. } => None,
            Self::Present(node, safety) => Some(ManifestEntry {
                path: path.clone(),
                node: node.to_node(),
                safety: safety.to_observations(),
            }),
        }
    }

    fn path_encoding(&self) -> fence_core::PathEncoding {
        match self {
            Self::Absent { encoding } => decode_encoding(*encoding),
            Self::Present(node, _) => node.path_encoding(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum PlanNode {
    Regular {
        object: ObjectId,
        raw_size: u64,
        unix_exec_bits: Option<u8>,
        windows_readonly: Option<bool>,
        encoding: u8,
    },
    Symlink {
        target: NativeString,
        windows_link_kind: Option<WindowsSymlinkKind>,
        windows_substitute_name: Option<NativeString>,
        windows_reparse_flags: Option<u32>,
    },
    EmptyDirectory {
        encoding: u8,
    },
}

impl PlanNode {
    fn from_entry(entry: &ManifestEntry) -> Self {
        let encoding = encoding_tag(entry.path.encoding());
        match &entry.node {
            ManifestNode::Regular {
                object,
                raw_size,
                unix_exec_bits,
                windows_readonly,
            } => Self::Regular {
                object: *object,
                raw_size: *raw_size,
                unix_exec_bits: *unix_exec_bits,
                windows_readonly: *windows_readonly,
                encoding,
            },
            ManifestNode::Symlink {
                target,
                windows_link_kind,
                windows_substitute_name,
                windows_reparse_flags,
            } => Self::Symlink {
                target: target.clone(),
                windows_link_kind: *windows_link_kind,
                windows_substitute_name: windows_substitute_name.clone(),
                windows_reparse_flags: *windows_reparse_flags,
            },
            ManifestNode::EmptyDirectory => Self::EmptyDirectory { encoding },
        }
    }

    fn to_node(&self) -> ManifestNode {
        match self {
            Self::Regular {
                object,
                raw_size,
                unix_exec_bits,
                windows_readonly,
                ..
            } => ManifestNode::Regular {
                object: *object,
                raw_size: *raw_size,
                unix_exec_bits: *unix_exec_bits,
                windows_readonly: *windows_readonly,
            },
            Self::Symlink {
                target,
                windows_link_kind,
                windows_substitute_name,
                windows_reparse_flags,
            } => ManifestNode::Symlink {
                target: target.clone(),
                windows_link_kind: *windows_link_kind,
                windows_substitute_name: windows_substitute_name.clone(),
                windows_reparse_flags: *windows_reparse_flags,
            },
            Self::EmptyDirectory { .. } => ManifestNode::EmptyDirectory,
        }
    }

    fn path_encoding(&self) -> fence_core::PathEncoding {
        match self {
            Self::Regular { encoding, .. } | Self::EmptyDirectory { encoding } => {
                decode_encoding(*encoding)
            }
            Self::Symlink { target, .. } => target.encoding(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlanSafety(Option<u64>, u64, bool, #[serde(default)] Option<u8>);

impl PlanSafety {
    pub(crate) fn from_observations(value: &SafetyObservations) -> Self {
        Self(
            value.hardlink_group,
            value.link_count,
            value.extended_metadata == MetadataObservation::Present,
            Some(metadata_tag(value.extended_metadata)),
        )
    }

    pub(crate) fn to_observations(&self) -> SafetyObservations {
        SafetyObservations {
            hardlink_group: self.0,
            link_count: self.1,
            extended_metadata: self.3.map_or_else(
                || {
                    if self.2 {
                        MetadataObservation::Present
                    } else {
                        MetadataObservation::Unknown
                    }
                },
                decode_metadata_tag,
            ),
        }
    }
}

const fn metadata_tag(value: MetadataObservation) -> u8 {
    match value {
        MetadataObservation::Unknown => 0,
        MetadataObservation::Absent => 1,
        MetadataObservation::Present => 2,
        MetadataObservation::Unavailable => 3,
        MetadataObservation::PlatformManaged => 4,
    }
}

const fn decode_metadata_tag(value: u8) -> MetadataObservation {
    match value {
        1 => MetadataObservation::Absent,
        2 => MetadataObservation::Present,
        3 => MetadataObservation::Unavailable,
        4 => MetadataObservation::PlatformManaged,
        _ => MetadataObservation::Unknown,
    }
}

fn host_encoding_tag() -> u8 {
    encoding_tag(fence_core::PathEncoding::host())
}

fn encoding_tag(encoding: fence_core::PathEncoding) -> u8 {
    match encoding {
        fence_core::PathEncoding::UnixBytes => 1,
        fence_core::PathEncoding::WindowsWtf16Le => 2,
    }
}

fn decode_encoding(tag: u8) -> fence_core::PathEncoding {
    match tag {
        2 => fence_core::PathEncoding::WindowsWtf16Le,
        _ => fence_core::PathEncoding::UnixBytes,
    }
}

impl SessionStore {
    pub(crate) fn put_restore_plan(
        &self,
        plan: &RestorePlanRecord,
    ) -> Result<RestorePlanId, SessionError> {
        let id = plan.id()?;
        let bytes = plan.encode()?;
        let path = self.restore_plan_path(id);
        let parent = path.parent().ok_or(SessionError::InvalidLayout)?;
        private_directory(parent)?;
        if path.exists() {
            let existing = self.load_restore_plan(id)?;
            if existing == *plan {
                return Ok(id);
            }
            return Err(SessionError::RestorePlanCollision(id));
        }
        let mut file = NamedTempFile::new_in(parent)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        match file.persist_noclobber(&path) {
            Ok(file) => {
                file.sync_all()?;
                sync_parent(&path)?;
                Ok(id)
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                let existing = self.load_restore_plan(id)?;
                if existing == *plan {
                    Ok(id)
                } else {
                    Err(SessionError::RestorePlanCollision(id))
                }
            }
            Err(error) => Err(error.error.into()),
        }
    }

    pub(crate) fn load_restore_plan(
        &self,
        id: RestorePlanId,
    ) -> Result<RestorePlanRecord, SessionError> {
        let plan =
            RestorePlanRecord::decode(&bounded_read(&self.restore_plan_path(id), MAX_PLAN_BYTES)?)?;
        if plan.id()? != id {
            return Err(SessionError::RestorePlanIdentityMismatch(id));
        }
        Ok(plan)
    }

    pub(crate) fn list_restore_plans(
        &self,
    ) -> Result<Vec<(RestorePlanId, RestorePlanRecord)>, SessionError> {
        let root = self.root.join("plans").join("b3");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut plans = Vec::new();
        for prefix in fs::read_dir(root)? {
            let prefix = prefix?;
            if !prefix.file_type()?.is_dir() {
                return Err(SessionError::InvalidLayout);
            }
            let prefix_name = prefix
                .file_name()
                .into_string()
                .map_err(|_| SessionError::InvalidLayout)?;
            for entry in fs::read_dir(prefix.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file()
                    || entry.path().extension().is_none_or(|value| value != "cbor")
                {
                    return Err(SessionError::InvalidLayout);
                }
                let suffix = entry
                    .path()
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .ok_or(SessionError::InvalidLayout)?
                    .to_owned();
                let hex = format!("{prefix_name}{suffix}");
                let id = parse_plan_id(&hex)?;
                plans.push((id, self.load_restore_plan(id)?));
            }
        }
        Ok(plans)
    }

    fn restore_plan_path(&self, id: RestorePlanId) -> PathBuf {
        let hex = id.to_hex();
        self.root
            .join("plans")
            .join("b3")
            .join(&hex[..2])
            .join(format!("{}.cbor", &hex[2..]))
    }
}

fn parse_plan_id(value: &str) -> Result<RestorePlanId, SessionError> {
    if value.len() != 64 {
        return Err(SessionError::InvalidLayout);
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(RestorePlanId::from_bytes(output))
}

fn hex_nibble(value: u8) -> Result<u8, SessionError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(SessionError::InvalidLayout),
    }
}

#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn sync_parent(path: &std::path::Path) -> Result<(), SessionError> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Debug, Error)]
pub enum RestorePlanError {
    #[error("restore-plan record has the wrong type tag")]
    WrongTag,
    #[error("restore-plan schema {0} is unsupported")]
    UnsupportedSchema(u16),
    #[error("restore-plan encoding failed: {0}")]
    Encode(String),
    #[error("restore-plan decoding failed: {0}")]
    Decode(String),
    #[error("restore-plan record contains trailing bytes")]
    TrailingBytes,
    #[error("restore-plan record exceeds its size limit")]
    TooLarge,
    #[error("restore-plan item count is invalid")]
    InvalidItemCount,
    #[error("restore-plan contains duplicate paths")]
    DuplicatePath,
    #[error("restore-plan mixes path encodings")]
    MixedPathEncoding,
    #[error("restore-plan worktree key is empty")]
    EmptyWorktreeKey,
}

#[cfg(test)]
mod tests {
    use super::*;
    use fence_core::PathEncoding;
    use fence_git::RepositoryState;

    fn path(name: &[u8]) -> NativeRelativePath {
        NativeRelativePath::new(PathEncoding::host(), vec![name.to_vec()]).unwrap()
    }

    fn repository() -> RepositoryState {
        RepositoryState {
            head: fence_git::HeadState::Unborn {
                referent: b"refs/heads/main".to_vec(),
            },
            operation: fence_git::OperationState::None,
            object_hash: "Sha1".to_owned(),
            ignore_case: false,
            sparse_checkout: false,
            sparse_index: false,
            split_index: false,
        }
    }

    #[test]
    fn round_trip_and_identity_are_deterministic() {
        let record = RestorePlanRecord::worktree(
            SessionId::new(),
            NativeString::from_host(std::ffi::OsStr::new("/tmp/worktree")),
            "worktree".to_owned(),
            repository(),
            ManifestId::from_bytes([1; 32]),
            ManifestId::from_bytes([2; 32]),
            ManifestId::from_bytes([3; 32]),
            vec![PlanItem::exact(path(b"file"), None, None)],
        )
        .unwrap();
        let bytes = record.encode().unwrap();
        assert_eq!(RestorePlanRecord::decode(&bytes).unwrap(), record);
        assert_eq!(record.id().unwrap(), record.id().unwrap());
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let record = RestorePlanRecord::index(
            SessionId::new(),
            NativeString::from_host(std::ffi::OsStr::new("/tmp/worktree")),
            "worktree".to_owned(),
            repository(),
            NativeString::from_host(std::ffi::OsStr::new("/tmp/worktree/.git/index")),
            IndexCapture::Absent,
            IndexCapture::Absent,
        )
        .unwrap();
        let mut bytes = record.encode().unwrap();
        bytes.push(0);
        assert!(matches!(
            RestorePlanRecord::decode(&bytes),
            Err(RestorePlanError::TrailingBytes)
        ));
    }

    #[test]
    fn legacy_three_field_safety_decodes_as_unknown() {
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&(None::<u64>, 1_u64, false), &mut bytes).unwrap();
        let safety: PlanSafety = ciborium::de::from_reader(bytes.as_slice()).unwrap();
        assert_eq!(
            safety.to_observations(),
            SafetyObservations {
                hardlink_group: None,
                link_count: 1,
                extended_metadata: MetadataObservation::Unknown,
            }
        );
    }
}
