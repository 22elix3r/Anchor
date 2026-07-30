use std::fmt;
use std::io::Cursor;

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use thiserror::Error;

use crate::{NativeRelativePath, NativeString, ObjectId, PathEncoding, PathError};

const MANIFEST_TAG: u64 = 0x414d;
const MANIFEST_SCHEMA: u16 = 3;
const MAX_ENCODED_MANIFEST: usize = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ManifestId([u8; 32]);

impl ManifestId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parse a lowercase or uppercase hexadecimal BLAKE3 manifest identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::InvalidManifestId`] unless `value` contains exactly 64
    /// hexadecimal ASCII characters.
    pub fn from_hex(value: &str) -> Result<Self, ManifestError> {
        if value.len() != 64 {
            return Err(ManifestError::InvalidManifestId);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_nibble(pair[0]).ok_or(ManifestError::InvalidManifestId)?;
            let low = hex_nibble(pair[1]).ok_or(ManifestError::InvalidManifestId)?;
            bytes[index] = high << 4 | low;
        }
        Ok(Self(bytes))
    }
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for ManifestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for ManifestId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for ManifestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = ByteBuf::deserialize(deserializer)?;
        let bytes: [u8; 32] = bytes
            .as_ref()
            .try_into()
            .map_err(|_| serde::de::Error::custom("manifest ID must contain exactly 32 bytes"))?;
        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Completeness {
    Complete = 1,
    CompleteWithWarnings = 2,
    Degraded = 3,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Manifest {
    schema: u16,
    path_encoding: PathEncoding,
    entries: Vec<ManifestEntry>,
    coverage: Coverage,
}

impl Manifest {
    /// Construct and validate a manifest, sorting entries into canonical path order.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] for mixed encodings, duplicate paths, or leaf-prefix collisions.
    pub fn new(
        path_encoding: PathEncoding,
        mut entries: Vec<ManifestEntry>,
        coverage: Coverage,
    ) -> Result<Self, ManifestError> {
        if entries.len() > MAX_ENTRIES {
            return Err(ManifestError::TooManyEntries);
        }
        if entries
            .iter()
            .any(|entry| entry.path.encoding() != path_encoding)
        {
            return Err(ManifestError::MixedPathEncoding);
        }
        if coverage.completeness != Completeness::Degraded && !coverage.omissions.is_empty() {
            return Err(ManifestError::OmissionsRequireDegraded);
        }
        if coverage
            .omissions
            .iter()
            .any(|omission| omission.path.encoding() != path_encoding)
        {
            return Err(ManifestError::MixedPathEncoding);
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        validate_entry_tree(&entries)?;
        let manifest = Self {
            schema: MANIFEST_SCHEMA,
            path_encoding,
            entries,
            coverage,
        };
        manifest.validate_platform_metadata()?;
        Ok(manifest)
    }

    #[must_use]
    pub const fn path_encoding(&self) -> PathEncoding {
        self.path_encoding
    }

    /// Return the persistent schema used to identify this manifest.
    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema
    }

    #[must_use]
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    #[must_use]
    pub const fn coverage(&self) -> &Coverage {
        &self.coverage
    }

    /// Encode the canonical versioned manifest record.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] if CBOR serialization fails or exceeds the size limit.
    pub fn encode(&self) -> Result<Vec<u8>, ManifestError> {
        let mut output = Vec::new();
        match self.schema {
            1 => {
                let wire = ManifestWireV1(
                    MANIFEST_TAG,
                    1,
                    encoding_tag(self.path_encoding),
                    self.entries.iter().map(EntryWireV1::from).collect(),
                    CoverageWire::from(&self.coverage),
                );
                ciborium::ser::into_writer(&wire, &mut output)
                    .map_err(|error| ManifestError::Encode(error.to_string()))?;
            }
            2 => {
                let wire = ManifestWireV2(
                    MANIFEST_TAG,
                    2,
                    encoding_tag(self.path_encoding),
                    self.entries.iter().map(EntryWireV2::from).collect(),
                    CoverageWire::from(&self.coverage),
                );
                ciborium::ser::into_writer(&wire, &mut output)
                    .map_err(|error| ManifestError::Encode(error.to_string()))?;
            }
            MANIFEST_SCHEMA => {
                let wire = ManifestWireV3(
                    MANIFEST_TAG,
                    MANIFEST_SCHEMA,
                    encoding_tag(self.path_encoding),
                    self.entries.iter().map(EntryWireV3::from).collect(),
                    CoverageWire::from(&self.coverage),
                );
                ciborium::ser::into_writer(&wire, &mut output)
                    .map_err(|error| ManifestError::Encode(error.to_string()))?;
            }
            schema => return Err(ManifestError::UnsupportedSchema(schema)),
        }
        if output.len() > MAX_ENCODED_MANIFEST {
            return Err(ManifestError::EncodedTooLarge);
        }
        Ok(output)
    }

    /// Decode and validate a supported manifest record.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] for malformed, oversized, unsupported, or unsafe records.
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_ENCODED_MANIFEST {
            return Err(ManifestError::EncodedTooLarge);
        }
        if let Ok(wire) = ciborium::de::from_reader::<ManifestWireV3, _>(Cursor::new(bytes)) {
            if wire.0 != MANIFEST_TAG {
                return Err(ManifestError::WrongTag);
            }
            if wire.1 != MANIFEST_SCHEMA {
                return Err(ManifestError::UnsupportedSchema(wire.1));
            }
            return Self::from_v3_wire(wire);
        }
        if let Ok(wire) = ciborium::de::from_reader::<ManifestWireV2, _>(Cursor::new(bytes)) {
            if wire.0 != MANIFEST_TAG {
                return Err(ManifestError::WrongTag);
            }
            if wire.1 != 2 {
                return Err(ManifestError::UnsupportedSchema(wire.1));
            }
            return Self::from_v2_wire(wire);
        }
        let wire: ManifestWireV1 = ciborium::de::from_reader(Cursor::new(bytes))
            .map_err(|error| ManifestError::Decode(error.to_string()))?;
        if wire.0 != MANIFEST_TAG {
            return Err(ManifestError::WrongTag);
        }
        if wire.1 != 1 {
            return Err(ManifestError::UnsupportedSchema(wire.1));
        }
        Self::from_v1_wire(wire)
    }

    /// Compute the domain-separated identity of this manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] if canonical encoding fails.
    pub fn id(&self) -> Result<ManifestId, ManifestError> {
        let encoded = self.encode()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(match self.schema {
            1 => b"anchor:manifest:v1\0",
            2 => b"anchor:manifest:v2\0",
            MANIFEST_SCHEMA => b"anchor:manifest:v3\0",
            _ => return Err(ManifestError::UnsupportedSchema(self.schema)),
        });
        hasher.update(&encoded);
        Ok(ManifestId(*hasher.finalize().as_bytes()))
    }

    fn from_v1_wire(wire: ManifestWireV1) -> Result<Self, ManifestError> {
        if wire.3.len() > MAX_ENTRIES {
            return Err(ManifestError::TooManyEntries);
        }
        let encoding = decode_encoding(wire.2)?;
        let entries = wire
            .3
            .into_iter()
            .map(|entry| entry.into_entry(encoding))
            .collect::<Result<Vec<_>, _>>()?;
        let coverage = wire.4.into_coverage(encoding)?;
        Self::from_decoded(1, encoding, entries, coverage)
    }

    fn from_v2_wire(wire: ManifestWireV2) -> Result<Self, ManifestError> {
        if wire.3.len() > MAX_ENTRIES {
            return Err(ManifestError::TooManyEntries);
        }
        let encoding = decode_encoding(wire.2)?;
        let entries = wire
            .3
            .into_iter()
            .map(|entry| entry.into_entry(encoding))
            .collect::<Result<Vec<_>, _>>()?;
        let coverage = wire.4.into_coverage(encoding)?;
        Self::from_decoded(2, encoding, entries, coverage)
    }

    fn from_v3_wire(wire: ManifestWireV3) -> Result<Self, ManifestError> {
        if wire.3.len() > MAX_ENTRIES {
            return Err(ManifestError::TooManyEntries);
        }
        let encoding = decode_encoding(wire.2)?;
        let entries = wire
            .3
            .into_iter()
            .map(|entry| entry.into_entry(encoding))
            .collect::<Result<Vec<_>, _>>()?;
        let coverage = wire.4.into_coverage(encoding)?;
        Self::from_decoded(MANIFEST_SCHEMA, encoding, entries, coverage)
    }

    fn from_decoded(
        schema: u16,
        path_encoding: PathEncoding,
        mut entries: Vec<ManifestEntry>,
        coverage: Coverage,
    ) -> Result<Self, ManifestError> {
        if coverage.completeness != Completeness::Degraded && !coverage.omissions.is_empty() {
            return Err(ManifestError::OmissionsRequireDegraded);
        }
        if entries
            .iter()
            .any(|entry| entry.path.encoding() != path_encoding)
            || coverage
                .omissions
                .iter()
                .any(|omission| omission.path.encoding() != path_encoding)
        {
            return Err(ManifestError::MixedPathEncoding);
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        validate_entry_tree(&entries)?;
        let manifest = Self {
            schema,
            path_encoding,
            entries,
            coverage,
        };
        if schema >= 2 {
            manifest.validate_platform_metadata()?;
        }
        Ok(manifest)
    }

    fn validate_platform_metadata(&self) -> Result<(), ManifestError> {
        for entry in &self.entries {
            match (&entry.node, self.path_encoding) {
                (
                    ManifestNode::Regular {
                        unix_exec_bits,
                        windows_readonly,
                        ..
                    },
                    PathEncoding::UnixBytes,
                ) if unix_exec_bits.is_none() || windows_readonly.is_some() => {
                    return Err(ManifestError::InvalidPlatformMetadata);
                }
                (
                    ManifestNode::Regular {
                        unix_exec_bits,
                        windows_readonly,
                        ..
                    },
                    PathEncoding::WindowsWtf16Le,
                ) if unix_exec_bits.is_some() || windows_readonly.is_none() => {
                    return Err(ManifestError::InvalidPlatformMetadata);
                }
                (
                    ManifestNode::Symlink {
                        windows_link_kind,
                        windows_substitute_name,
                        windows_reparse_flags,
                        ..
                    },
                    PathEncoding::UnixBytes,
                ) if windows_link_kind.is_some()
                    || windows_substitute_name.is_some()
                    || windows_reparse_flags.is_some() =>
                {
                    return Err(ManifestError::InvalidPlatformMetadata);
                }
                (
                    ManifestNode::Symlink {
                        target,
                        windows_link_kind,
                        windows_substitute_name,
                        windows_reparse_flags,
                    },
                    PathEncoding::WindowsWtf16Le,
                ) if windows_link_kind.is_none()
                    || windows_substitute_name.is_none()
                    || windows_reparse_flags.is_none()
                    || target.encoding() != PathEncoding::WindowsWtf16Le
                    || windows_substitute_name
                        .as_ref()
                        .is_some_and(|value| value.encoding() != PathEncoding::WindowsWtf16Le) =>
                {
                    return Err(ManifestError::InvalidPlatformMetadata);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestEntry {
    pub path: NativeRelativePath,
    pub node: ManifestNode,
    pub safety: SafetyObservations,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestNode {
    Regular {
        object: ObjectId,
        raw_size: u64,
        unix_exec_bits: Option<u8>,
        windows_readonly: Option<bool>,
    },
    Symlink {
        target: NativeString,
        windows_link_kind: Option<WindowsSymlinkKind>,
        windows_substitute_name: Option<NativeString>,
        windows_reparse_flags: Option<u32>,
    },
    EmptyDirectory,
}

/// The target object type required by `CreateSymbolicLinkW`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
pub enum WindowsSymlinkKind {
    File = 1,
    Directory = 2,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SafetyObservations {
    pub hardlink_group: Option<u64>,
    pub link_count: u64,
    pub extended_metadata: MetadataObservation,
}

/// Whether unmodeled ACL/xattr/resource-fork metadata was observable at capture time.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MetadataObservation {
    /// A legacy manifest did not make a trustworthy query.
    #[default]
    Unknown,
    /// The platform query completed and found no extended metadata.
    Absent,
    /// The platform query found at least one unmodeled metadata record.
    Present,
    /// The platform or filesystem could not make a trustworthy query.
    Unavailable,
    /// Only platform-managed labeling metadata was present and can be compared before replace.
    PlatformManaged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Coverage {
    pub completeness: Completeness,
    pub omissions: Vec<Omission>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Omission {
    pub path: NativeRelativePath,
    pub reason: OmissionReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum OmissionReason {
    PermissionDenied = 1,
    Unstable = 2,
    UnsupportedType = 3,
    MountBoundary = 4,
    NestedRepository = 5,
    ReparsePoint = 6,
    ExplicitDegradedExclusion = 7,
    AlternateDataStream = 8,
    HardlinkTopology = 9,
    UnsupportedReparsePoint = 10,
    CloudPlaceholder = 11,
    EncryptedFile = 12,
    CaseSemanticsUnknown = 13,
}

#[derive(Serialize, Deserialize)]
struct ManifestWireV1(u64, u16, u8, Vec<EntryWireV1>, CoverageWire);

#[derive(Serialize, Deserialize)]
struct ManifestWireV2(u64, u16, u8, Vec<EntryWireV2>, CoverageWire);

#[derive(Serialize, Deserialize)]
struct ManifestWireV3(u64, u16, u8, Vec<EntryWireV3>, CoverageWire);

#[derive(Serialize, Deserialize)]
struct EntryWireV1(PathWire, NodeWireV1, SafetyWire);

#[derive(Serialize, Deserialize)]
struct EntryWireV2(PathWire, NodeWireV2, SafetyWire);

#[derive(Serialize, Deserialize)]
struct EntryWireV3(PathWire, NodeWireV2, SafetyWireV3);

#[derive(Serialize, Deserialize)]
struct PathWire(u8, Vec<ByteBuf>);

#[derive(Serialize, Deserialize)]
struct NativeStringWire(u8, ByteBuf);

#[derive(Serialize, Deserialize)]
struct NodeWireV1(
    u8,
    Option<ObjectId>,
    Option<u64>,
    Option<u8>,
    Option<NativeStringWire>,
    Option<u8>,
);

#[derive(Serialize, Deserialize)]
struct NodeWireV2(
    u8,
    Option<ObjectId>,
    Option<u64>,
    Option<u8>,
    Option<bool>,
    Option<NativeStringWire>,
    Option<u8>,
    Option<NativeStringWire>,
    Option<u32>,
);

#[derive(Serialize, Deserialize)]
struct SafetyWire(Option<u64>, u64, bool);

#[derive(Serialize, Deserialize)]
struct SafetyWireV3(Option<u64>, u64, u8);

#[derive(Serialize, Deserialize)]
struct CoverageWire(u8, Vec<OmissionWire>);

#[derive(Serialize, Deserialize)]
struct OmissionWire(PathWire, u16);

impl From<&ManifestEntry> for EntryWireV1 {
    fn from(entry: &ManifestEntry) -> Self {
        let node = match &entry.node {
            ManifestNode::Regular {
                object,
                raw_size,
                unix_exec_bits,
                ..
            } => NodeWireV1(
                1,
                Some(*object),
                Some(*raw_size),
                *unix_exec_bits,
                None,
                None,
            ),
            ManifestNode::Symlink {
                target,
                windows_link_kind,
                ..
            } => NodeWireV1(
                2,
                None,
                None,
                None,
                Some(NativeStringWire(
                    encoding_tag(target.encoding()),
                    ByteBuf::from(target.bytes().to_vec()),
                )),
                windows_link_kind.map(|kind| kind as u8),
            ),
            ManifestNode::EmptyDirectory => NodeWireV1(3, None, None, None, None, None),
        };
        Self(
            PathWire::from(&entry.path),
            node,
            SafetyWire(
                entry.safety.hardlink_group,
                entry.safety.link_count,
                entry.safety.extended_metadata == MetadataObservation::Present,
            ),
        )
    }
}

impl From<&ManifestEntry> for EntryWireV2 {
    fn from(entry: &ManifestEntry) -> Self {
        let node = match &entry.node {
            ManifestNode::Regular {
                object,
                raw_size,
                unix_exec_bits,
                windows_readonly,
            } => NodeWireV2(
                1,
                Some(*object),
                Some(*raw_size),
                *unix_exec_bits,
                *windows_readonly,
                None,
                None,
                None,
                None,
            ),
            ManifestNode::Symlink {
                target,
                windows_link_kind,
                windows_substitute_name,
                windows_reparse_flags,
            } => NodeWireV2(
                2,
                None,
                None,
                None,
                None,
                Some(NativeStringWire(
                    encoding_tag(target.encoding()),
                    ByteBuf::from(target.bytes().to_vec()),
                )),
                windows_link_kind.map(|kind| kind as u8),
                windows_substitute_name.as_ref().map(|value| {
                    NativeStringWire(
                        encoding_tag(value.encoding()),
                        ByteBuf::from(value.bytes().to_vec()),
                    )
                }),
                *windows_reparse_flags,
            ),
            ManifestNode::EmptyDirectory => {
                NodeWireV2(3, None, None, None, None, None, None, None, None)
            }
        };
        Self(
            PathWire::from(&entry.path),
            node,
            SafetyWire(
                entry.safety.hardlink_group,
                entry.safety.link_count,
                entry.safety.extended_metadata == MetadataObservation::Present,
            ),
        )
    }
}

impl From<&ManifestEntry> for EntryWireV3 {
    fn from(entry: &ManifestEntry) -> Self {
        let EntryWireV2(path, node, _) = EntryWireV2::from(entry);
        Self(
            path,
            node,
            SafetyWireV3(
                entry.safety.hardlink_group,
                entry.safety.link_count,
                metadata_observation_tag(entry.safety.extended_metadata),
            ),
        )
    }
}

impl EntryWireV1 {
    fn into_entry(self, manifest_encoding: PathEncoding) -> Result<ManifestEntry, ManifestError> {
        let path = self.0.into_path()?;
        if path.encoding() != manifest_encoding {
            return Err(ManifestError::MixedPathEncoding);
        }
        let node = match self.1 {
            NodeWireV1(1, Some(object), Some(raw_size), unix_exec_bits, None, None) => {
                if unix_exec_bits.is_some_and(|bits| bits > 0b111) {
                    return Err(ManifestError::InvalidExecuteBits);
                }
                ManifestNode::Regular {
                    object,
                    raw_size,
                    unix_exec_bits,
                    windows_readonly: None,
                }
            }
            NodeWireV1(2, None, None, None, Some(target), windows_link_kind) => {
                let target = target.into_native_string()?;
                if target.encoding() != manifest_encoding {
                    return Err(ManifestError::MixedPathEncoding);
                }
                ManifestNode::Symlink {
                    target,
                    windows_link_kind: windows_link_kind
                        .map(decode_windows_link_kind)
                        .transpose()?,
                    windows_substitute_name: None,
                    windows_reparse_flags: None,
                }
            }
            NodeWireV1(3, None, None, None, None, None) => ManifestNode::EmptyDirectory,
            _ => return Err(ManifestError::InvalidNode),
        };
        Ok(ManifestEntry {
            path,
            node,
            safety: SafetyObservations {
                hardlink_group: self.2.0,
                link_count: self.2.1,
                extended_metadata: if self.2.2 {
                    MetadataObservation::Present
                } else {
                    MetadataObservation::Unknown
                },
            },
        })
    }
}

impl EntryWireV2 {
    fn into_entry(self, manifest_encoding: PathEncoding) -> Result<ManifestEntry, ManifestError> {
        let path = self.0.into_path()?;
        if path.encoding() != manifest_encoding {
            return Err(ManifestError::MixedPathEncoding);
        }
        let node = match self.1 {
            NodeWireV2(
                1,
                Some(object),
                Some(raw_size),
                unix_exec_bits,
                windows_readonly,
                None,
                None,
                None,
                None,
            ) => {
                if unix_exec_bits.is_some_and(|bits| bits > 0b111) {
                    return Err(ManifestError::InvalidExecuteBits);
                }
                ManifestNode::Regular {
                    object,
                    raw_size,
                    unix_exec_bits,
                    windows_readonly,
                }
            }
            NodeWireV2(
                2,
                None,
                None,
                None,
                None,
                Some(target),
                Some(windows_link_kind),
                Some(windows_substitute_name),
                Some(windows_reparse_flags),
            ) => {
                let target = target.into_native_string()?;
                let windows_substitute_name = windows_substitute_name.into_native_string()?;
                if target.encoding() != manifest_encoding
                    || windows_substitute_name.encoding() != manifest_encoding
                {
                    return Err(ManifestError::MixedPathEncoding);
                }
                ManifestNode::Symlink {
                    target,
                    windows_link_kind: Some(decode_windows_link_kind(windows_link_kind)?),
                    windows_substitute_name: Some(windows_substitute_name),
                    windows_reparse_flags: Some(windows_reparse_flags),
                }
            }
            NodeWireV2(2, None, None, None, None, Some(target), None, None, None) => {
                let target = target.into_native_string()?;
                if target.encoding() != manifest_encoding {
                    return Err(ManifestError::MixedPathEncoding);
                }
                ManifestNode::Symlink {
                    target,
                    windows_link_kind: None,
                    windows_substitute_name: None,
                    windows_reparse_flags: None,
                }
            }
            NodeWireV2(3, None, None, None, None, None, None, None, None) => {
                ManifestNode::EmptyDirectory
            }
            _ => return Err(ManifestError::InvalidNode),
        };
        Ok(ManifestEntry {
            path,
            node,
            safety: SafetyObservations {
                hardlink_group: self.2.0,
                link_count: self.2.1,
                extended_metadata: if self.2.2 {
                    MetadataObservation::Present
                } else {
                    MetadataObservation::Unknown
                },
            },
        })
    }
}

impl EntryWireV3 {
    fn into_entry(self, manifest_encoding: PathEncoding) -> Result<ManifestEntry, ManifestError> {
        let Self(path, node, safety) = self;
        let mut entry =
            EntryWireV2(path, node, SafetyWire(None, 0, false)).into_entry(manifest_encoding)?;
        entry.safety = SafetyObservations {
            hardlink_group: safety.0,
            link_count: safety.1,
            extended_metadata: decode_metadata_observation(safety.2)?,
        };
        Ok(entry)
    }
}

fn decode_windows_link_kind(value: u8) -> Result<WindowsSymlinkKind, ManifestError> {
    match value {
        1 => Ok(WindowsSymlinkKind::File),
        2 => Ok(WindowsSymlinkKind::Directory),
        _ => Err(ManifestError::InvalidWindowsLinkKind(value)),
    }
}

const fn metadata_observation_tag(value: MetadataObservation) -> u8 {
    match value {
        MetadataObservation::Unknown => 0,
        MetadataObservation::Absent => 1,
        MetadataObservation::Present => 2,
        MetadataObservation::Unavailable => 3,
        MetadataObservation::PlatformManaged => 4,
    }
}

fn decode_metadata_observation(value: u8) -> Result<MetadataObservation, ManifestError> {
    match value {
        0 => Ok(MetadataObservation::Unknown),
        1 => Ok(MetadataObservation::Absent),
        2 => Ok(MetadataObservation::Present),
        3 => Ok(MetadataObservation::Unavailable),
        4 => Ok(MetadataObservation::PlatformManaged),
        value => Err(ManifestError::InvalidMetadataObservation(value)),
    }
}

impl From<&NativeRelativePath> for PathWire {
    fn from(path: &NativeRelativePath) -> Self {
        Self(
            encoding_tag(path.encoding()),
            path.components()
                .iter()
                .cloned()
                .map(ByteBuf::from)
                .collect(),
        )
    }
}

impl PathWire {
    fn into_path(self) -> Result<NativeRelativePath, ManifestError> {
        NativeRelativePath::new(
            decode_encoding(self.0)?,
            self.1.into_iter().map(ByteBuf::into_vec).collect(),
        )
        .map_err(ManifestError::Path)
    }
}

impl NativeStringWire {
    fn into_native_string(self) -> Result<NativeString, ManifestError> {
        NativeString::new(decode_encoding(self.0)?, self.1.into_vec()).map_err(ManifestError::Path)
    }
}

impl From<&Coverage> for CoverageWire {
    fn from(coverage: &Coverage) -> Self {
        let completeness = match coverage.completeness {
            Completeness::Complete => 1,
            Completeness::CompleteWithWarnings => 2,
            Completeness::Degraded => 3,
        };
        Self(
            completeness,
            coverage
                .omissions
                .iter()
                .map(|omission| {
                    OmissionWire(PathWire::from(&omission.path), omission.reason as u16)
                })
                .collect(),
        )
    }
}

impl CoverageWire {
    fn into_coverage(self, encoding: PathEncoding) -> Result<Coverage, ManifestError> {
        let completeness = match self.0 {
            1 => Completeness::Complete,
            2 => Completeness::CompleteWithWarnings,
            3 => Completeness::Degraded,
            value => return Err(ManifestError::InvalidCompleteness(value)),
        };
        let omissions = self
            .1
            .into_iter()
            .map(|omission| {
                let path = omission.0.into_path()?;
                if path.encoding() != encoding {
                    return Err(ManifestError::MixedPathEncoding);
                }
                let reason = match omission.1 {
                    1 => OmissionReason::PermissionDenied,
                    2 => OmissionReason::Unstable,
                    3 => OmissionReason::UnsupportedType,
                    4 => OmissionReason::MountBoundary,
                    5 => OmissionReason::NestedRepository,
                    6 => OmissionReason::ReparsePoint,
                    7 => OmissionReason::ExplicitDegradedExclusion,
                    8 => OmissionReason::AlternateDataStream,
                    9 => OmissionReason::HardlinkTopology,
                    10 => OmissionReason::UnsupportedReparsePoint,
                    11 => OmissionReason::CloudPlaceholder,
                    12 => OmissionReason::EncryptedFile,
                    13 => OmissionReason::CaseSemanticsUnknown,
                    value => return Err(ManifestError::UnknownOmissionReason(value)),
                };
                Ok(Omission { path, reason })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if completeness != Completeness::Degraded && !omissions.is_empty() {
            return Err(ManifestError::OmissionsRequireDegraded);
        }
        Ok(Coverage {
            completeness,
            omissions,
        })
    }
}

fn encoding_tag(encoding: PathEncoding) -> u8 {
    match encoding {
        PathEncoding::UnixBytes => 1,
        PathEncoding::WindowsWtf16Le => 2,
    }
}

fn decode_encoding(value: u8) -> Result<PathEncoding, ManifestError> {
    match value {
        1 => Ok(PathEncoding::UnixBytes),
        2 => Ok(PathEncoding::WindowsWtf16Le),
        _ => Err(ManifestError::UnknownPathEncoding(value)),
    }
}

fn validate_entry_tree(entries: &[ManifestEntry]) -> Result<(), ManifestError> {
    for pair in entries.windows(2) {
        let left = &pair[0].path;
        let right = &pair[1].path;
        if left == right {
            return Err(ManifestError::DuplicatePath);
        }
        if right.components().starts_with(left.components()) {
            return Err(ManifestError::LeafPrefixCollision);
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("manifest ID must contain exactly 64 hexadecimal characters")]
    InvalidManifestId,
    #[error("failed to encode manifest: {0}")]
    Encode(String),
    #[error("failed to decode manifest: {0}")]
    Decode(String),
    #[error("manifest exceeds its encoded-size limit")]
    EncodedTooLarge,
    #[error("manifest has too many entries")]
    TooManyEntries,
    #[error("manifest has the wrong record tag")]
    WrongTag,
    #[error("unsupported manifest schema {0}")]
    UnsupportedSchema(u16),
    #[error("manifest uses unknown path encoding {0}")]
    UnknownPathEncoding(u8),
    #[error("manifest mixes path encoding families")]
    MixedPathEncoding,
    #[error("manifest contains a duplicate path")]
    DuplicatePath,
    #[error("manifest leaf is a prefix of another entry")]
    LeafPrefixCollision,
    #[error("manifest contains an invalid node representation")]
    InvalidNode,
    #[error("manifest contains invalid execute bits")]
    InvalidExecuteBits,
    #[error("manifest contains invalid platform-specific metadata")]
    InvalidPlatformMetadata,
    #[error("manifest contains invalid Windows symbolic-link kind {0}")]
    InvalidWindowsLinkKind(u8),
    #[error("manifest contains invalid metadata-observation value {0}")]
    InvalidMetadataObservation(u8),
    #[error("manifest contains invalid completeness value {0}")]
    InvalidCompleteness(u8),
    #[error("manifest contains unknown omission reason {0}")]
    UnknownOmissionReason(u16),
    #[error("omissions require degraded completeness")]
    OmissionsRequireDegraded,
    #[error(transparent)]
    Path(#[from] PathError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(parts: &[&[u8]]) -> NativeRelativePath {
        NativeRelativePath::new(
            PathEncoding::UnixBytes,
            parts.iter().map(|part| part.to_vec()).collect(),
        )
        .unwrap()
    }

    fn empty_entry(path: NativeRelativePath) -> ManifestEntry {
        ManifestEntry {
            path,
            node: ManifestNode::EmptyDirectory,
            safety: SafetyObservations::default(),
        }
    }

    #[test]
    fn manifest_id_hex_round_trips_and_rejects_invalid_input() {
        let id = ManifestId::from_bytes([0xab; 32]);
        let text = id.to_string();
        assert_eq!(ManifestId::from_hex(&text).unwrap(), id);
        assert_eq!(ManifestId::from_hex(&text.to_uppercase()).unwrap(), id);
        assert!(matches!(
            ManifestId::from_hex("abcd"),
            Err(ManifestError::InvalidManifestId)
        ));
        assert!(matches!(
            ManifestId::from_hex(
                "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg"
            ),
            Err(ManifestError::InvalidManifestId)
        ));
    }

    #[test]
    fn manifest_round_trip_is_deterministic() {
        let entries = vec![
            ManifestEntry {
                path: path(&[b"src", b"main.rs"]),
                node: ManifestNode::Regular {
                    object: ObjectId::from_raw(b"fn main() {}\n"),
                    raw_size: 13,
                    unix_exec_bits: Some(0),
                    windows_readonly: None,
                },
                safety: SafetyObservations {
                    link_count: 1,
                    ..SafetyObservations::default()
                },
            },
            empty_entry(path(&[b"empty"])),
        ];
        let manifest = Manifest::new(
            PathEncoding::UnixBytes,
            entries,
            Coverage {
                completeness: Completeness::Complete,
                omissions: Vec::new(),
            },
        )
        .unwrap();
        let encoded = manifest.encode().unwrap();
        assert_eq!(Manifest::decode(&encoded).unwrap(), manifest);
        assert_eq!(manifest.encode().unwrap(), encoded);
        assert_eq!(manifest.id().unwrap(), manifest.id().unwrap());
    }

    #[test]
    fn legacy_manifest_keeps_its_wire_identity() {
        let legacy = Manifest {
            schema: 1,
            path_encoding: PathEncoding::UnixBytes,
            entries: vec![ManifestEntry {
                path: path(&[b"legacy"]),
                node: ManifestNode::Regular {
                    object: ObjectId::from_raw(b"legacy"),
                    raw_size: 6,
                    unix_exec_bits: Some(0),
                    windows_readonly: None,
                },
                safety: SafetyObservations::default(),
            }],
            coverage: Coverage {
                completeness: Completeness::Complete,
                omissions: Vec::new(),
            },
        };
        let id = legacy.id().unwrap();
        let decoded = Manifest::decode(&legacy.encode().unwrap()).unwrap();
        assert_eq!(decoded.schema_version(), 1);
        assert_eq!(decoded.id().unwrap(), id);
        assert_eq!(decoded, legacy);
    }

    #[test]
    fn schema_v2_absence_bit_migrates_to_unknown_metadata() {
        let legacy = Manifest {
            schema: 2,
            path_encoding: PathEncoding::UnixBytes,
            entries: vec![ManifestEntry {
                path: path(&[b"legacy"]),
                node: ManifestNode::Regular {
                    object: ObjectId::from_raw(b"legacy"),
                    raw_size: 6,
                    unix_exec_bits: Some(0),
                    windows_readonly: None,
                },
                safety: SafetyObservations {
                    hardlink_group: None,
                    link_count: 1,
                    extended_metadata: MetadataObservation::Unknown,
                },
            }],
            coverage: Coverage {
                completeness: Completeness::Complete,
                omissions: Vec::new(),
            },
        };
        let id = legacy.id().unwrap();
        let decoded = Manifest::decode(&legacy.encode().unwrap()).unwrap();
        assert_eq!(decoded.schema_version(), 2);
        assert_eq!(decoded.id().unwrap(), id);
        assert_eq!(
            decoded.entries()[0].safety.extended_metadata,
            MetadataObservation::Unknown
        );
    }

    #[test]
    fn windows_manifest_preserves_readonly_and_symlink_payload() {
        fn windows(value: &str) -> Vec<u8> {
            value.encode_utf16().flat_map(u16::to_le_bytes).collect()
        }

        let manifest = Manifest::new(
            PathEncoding::WindowsWtf16Le,
            vec![
                ManifestEntry {
                    path: NativeRelativePath::new(
                        PathEncoding::WindowsWtf16Le,
                        vec![windows("readonly.txt")],
                    )
                    .unwrap(),
                    node: ManifestNode::Regular {
                        object: ObjectId::from_raw(b"content"),
                        raw_size: 7,
                        unix_exec_bits: None,
                        windows_readonly: Some(true),
                    },
                    safety: SafetyObservations::default(),
                },
                ManifestEntry {
                    path: NativeRelativePath::new(
                        PathEncoding::WindowsWtf16Le,
                        vec![windows("link")],
                    )
                    .unwrap(),
                    node: ManifestNode::Symlink {
                        target: NativeString::new(
                            PathEncoding::WindowsWtf16Le,
                            windows(r"..\target"),
                        )
                        .unwrap(),
                        windows_link_kind: Some(WindowsSymlinkKind::File),
                        windows_substitute_name: Some(
                            NativeString::new(PathEncoding::WindowsWtf16Le, windows(r"..\target"))
                                .unwrap(),
                        ),
                        windows_reparse_flags: Some(1),
                    },
                    safety: SafetyObservations::default(),
                },
            ],
            Coverage {
                completeness: Completeness::Complete,
                omissions: Vec::new(),
            },
        )
        .unwrap();

        let decoded = Manifest::decode(&manifest.encode().unwrap()).unwrap();
        assert_eq!(decoded.schema_version(), 3);
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn duplicate_and_prefix_paths_are_rejected() {
        let coverage = Coverage {
            completeness: Completeness::Complete,
            omissions: Vec::new(),
        };
        assert!(matches!(
            Manifest::new(
                PathEncoding::UnixBytes,
                vec![empty_entry(path(&[b"a"])), empty_entry(path(&[b"a"]))],
                coverage.clone()
            ),
            Err(ManifestError::DuplicatePath)
        ));
        assert!(matches!(
            Manifest::new(
                PathEncoding::UnixBytes,
                vec![empty_entry(path(&[b"a"])), empty_entry(path(&[b"a", b"b"]))],
                coverage
            ),
            Err(ManifestError::LeafPrefixCollision)
        ));
    }
}
