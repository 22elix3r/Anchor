use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The native encoding family used by a persisted path.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
pub enum PathEncoding {
    /// Raw Unix `OsStr` bytes.
    UnixBytes = 1,
    /// Little-endian Windows UTF-16/WTF-16 code units.
    WindowsWtf16Le = 2,
}

impl PathEncoding {
    #[must_use]
    pub const fn host() -> Self {
        #[cfg(unix)]
        {
            Self::UnixBytes
        }
        #[cfg(windows)]
        {
            Self::WindowsWtf16Le
        }
    }
}

/// A relative path represented as validated native components.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct NativeRelativePath {
    encoding: PathEncoding,
    components: Vec<Vec<u8>>,
}

impl fmt::Debug for NativeRelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeRelativePath")
            .field("encoding", &self.encoding)
            .field("components", &self.components)
            .finish()
    }
}

impl NativeRelativePath {
    pub const MAX_COMPONENTS: usize = 4_096;
    pub const MAX_COMPONENT_BYTES: usize = 65_535;
    pub const MAX_TOTAL_BYTES: usize = 1_048_576;

    /// Construct a validated path.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] when any component is unsafe or exceeds a wire-format limit.
    pub fn new(encoding: PathEncoding, components: Vec<Vec<u8>>) -> Result<Self, PathError> {
        validate_components(encoding, &components)?;
        Ok(Self {
            encoding,
            components,
        })
    }

    #[must_use]
    pub const fn encoding(&self) -> PathEncoding {
        self.encoding
    }

    #[must_use]
    pub fn components(&self) -> &[Vec<u8>] {
        &self.components
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.components.is_empty()
    }

    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let (_, parents) = self.components.split_last()?;
        Some(Self {
            encoding: self.encoding,
            components: parents.to_vec(),
        })
    }

    /// Return a new path with one validated component appended.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] when the component is unsafe or the resulting path is too large.
    pub fn join_component(&self, component: Vec<u8>) -> Result<Self, PathError> {
        validate_component(self.encoding, &component)?;
        let mut components = self.components.clone();
        components.push(component);
        Self::new(self.encoding, components)
    }

    /// Return a new path with one native host component appended.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] if the host component is unsafe or the path exceeds its limits.
    pub fn join_host_component(&self, component: &OsStr) -> Result<Self, PathError> {
        if self.encoding != PathEncoding::host() {
            return Err(PathError::WrongHostEncoding);
        }
        self.join_component(native_component_from_os_str(component))
    }

    /// Convert a relative host path to its lossless persistent representation.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] for absolute paths, traversal components, or oversized paths.
    pub fn from_host_path(path: &Path) -> Result<Self, PathError> {
        if path.is_absolute() {
            return Err(PathError::Absolute);
        }

        let mut components = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(value) => {
                    components.push(native_component_from_os_str(value));
                }
                Component::CurDir => return Err(PathError::DotComponent),
                Component::ParentDir => return Err(PathError::ParentComponent),
                Component::RootDir | Component::Prefix(_) => return Err(PathError::Absolute),
            }
        }
        Self::new(PathEncoding::host(), components)
    }

    /// Convert this path to a native host path.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformMismatch`] when the stored encoding belongs to another OS family.
    pub fn to_host_path(&self) -> Result<PathBuf, PlatformMismatch> {
        if self.encoding != PathEncoding::host() {
            return Err(PlatformMismatch {
                stored: self.encoding,
                host: PathEncoding::host(),
            });
        }

        let mut output = PathBuf::new();
        for component in &self.components {
            output.push(native_component_to_os_string(component));
        }
        Ok(output)
    }
}

/// An opaque native string used for symlink targets and optional command records.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct NativeString {
    encoding: PathEncoding,
    bytes: Vec<u8>,
}

impl NativeString {
    pub const MAX_BYTES: usize = 1_048_576;

    /// Construct a validated opaque native string.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] for NUL data, malformed Windows code units, or excessive length.
    pub fn new(encoding: PathEncoding, bytes: Vec<u8>) -> Result<Self, PathError> {
        if bytes.len() > Self::MAX_BYTES {
            return Err(PathError::TooLong);
        }
        match encoding {
            PathEncoding::UnixBytes => {
                if bytes.contains(&0) {
                    return Err(PathError::Nul);
                }
            }
            PathEncoding::WindowsWtf16Le => {
                if bytes.len() % 2 != 0 {
                    return Err(PathError::OddWindowsLength);
                }
                if windows_units(&bytes).any(|unit| unit == 0) {
                    return Err(PathError::Nul);
                }
            }
        }
        Ok(Self { encoding, bytes })
    }

    #[must_use]
    pub const fn encoding(&self) -> PathEncoding {
        self.encoding
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn from_host(value: &OsStr) -> Self {
        Self {
            encoding: PathEncoding::host(),
            bytes: native_component_from_os_str(value),
        }
    }

    /// Convert this string to the host's native string representation.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformMismatch`] when the stored encoding belongs to another OS family.
    pub fn to_host(&self) -> Result<OsString, PlatformMismatch> {
        if self.encoding != PathEncoding::host() {
            return Err(PlatformMismatch {
                stored: self.encoding,
                host: PathEncoding::host(),
            });
        }
        Ok(native_component_to_os_string(&self.bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("path encoding {stored:?} cannot be restored on host encoding {host:?}")]
pub struct PlatformMismatch {
    pub stored: PathEncoding,
    pub host: PathEncoding,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PathError {
    #[error("absolute and prefixed paths are not allowed")]
    Absolute,
    #[error("empty path components are not allowed")]
    EmptyComponent,
    #[error("'.' path components are not allowed")]
    DotComponent,
    #[error("'..' path components are not allowed")]
    ParentComponent,
    #[error("path component contains a separator")]
    Separator,
    #[error("native path data contains a NUL")]
    Nul,
    #[error("Windows native data has an odd byte length")]
    OddWindowsLength,
    #[error("path exceeds the persistent-format limit")]
    TooLong,
    #[error("path has too many components")]
    TooManyComponents,
    #[error("operation requires a path encoded for the current host")]
    WrongHostEncoding,
}

fn validate_components(encoding: PathEncoding, components: &[Vec<u8>]) -> Result<(), PathError> {
    if components.len() > NativeRelativePath::MAX_COMPONENTS {
        return Err(PathError::TooManyComponents);
    }

    let mut total = 0usize;
    for component in components {
        validate_component(encoding, component)?;
        total = total
            .checked_add(component.len())
            .ok_or(PathError::TooLong)?;
        if total > NativeRelativePath::MAX_TOTAL_BYTES {
            return Err(PathError::TooLong);
        }
    }
    Ok(())
}

fn validate_component(encoding: PathEncoding, component: &[u8]) -> Result<(), PathError> {
    if component.is_empty() {
        return Err(PathError::EmptyComponent);
    }
    if component.len() > NativeRelativePath::MAX_COMPONENT_BYTES {
        return Err(PathError::TooLong);
    }

    match encoding {
        PathEncoding::UnixBytes => {
            if component.contains(&0) {
                return Err(PathError::Nul);
            }
            if component.contains(&b'/') {
                return Err(PathError::Separator);
            }
            if component == b"." {
                return Err(PathError::DotComponent);
            }
            if component == b".." {
                return Err(PathError::ParentComponent);
            }
        }
        PathEncoding::WindowsWtf16Le => {
            if component.len() % 2 != 0 {
                return Err(PathError::OddWindowsLength);
            }
            let units: Vec<u16> = windows_units(component).collect();
            if units.contains(&0) {
                return Err(PathError::Nul);
            }
            if units.iter().any(|unit| matches!(*unit, 0x2f | 0x5c)) {
                return Err(PathError::Separator);
            }
            if units == [u16::from(b'.')] {
                return Err(PathError::DotComponent);
            }
            if units == [u16::from(b'.'), u16::from(b'.')] {
                return Err(PathError::ParentComponent);
            }
        }
    }
    Ok(())
}

fn windows_units(bytes: &[u8]) -> impl Iterator<Item = u16> + '_ {
    bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
}

#[cfg(unix)]
fn native_component_from_os_str(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn native_component_from_os_str(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>()
}

#[cfg(unix)]
fn native_component_to_os_string(value: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(value.to_vec())
}

#[cfg(windows)]
fn native_component_to_os_string(value: &[u8]) -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(&windows_units(value).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_root_relative_path() {
        let path = NativeRelativePath::new(PathEncoding::UnixBytes, Vec::new()).unwrap();
        assert!(path.is_root());
    }

    #[test]
    fn rejects_unix_traversal_and_separator() {
        assert_eq!(
            NativeRelativePath::new(PathEncoding::UnixBytes, vec![b"..".to_vec()]),
            Err(PathError::ParentComponent)
        );
        assert_eq!(
            NativeRelativePath::new(PathEncoding::UnixBytes, vec![b"a/b".to_vec()]),
            Err(PathError::Separator)
        );
    }

    #[test]
    fn accepts_unpaired_windows_surrogate() {
        let bytes = 0xd800_u16.to_le_bytes().to_vec();
        assert!(NativeRelativePath::new(PathEncoding::WindowsWtf16Le, vec![bytes]).is_ok());
    }
}
