use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use thiserror::Error;

/// A NUL-terminated absolute Win32 verbatim path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerbatimPath {
    units: Vec<u16>,
}

impl VerbatimPath {
    /// Convert a host path to an absolute `\\?\` path without lossy Unicode conversion.
    ///
    /// # Errors
    ///
    /// Returns an error for NUL data, device namespaces, non-absolute results, or paths beyond
    /// the native 32,767-wide-character limit.
    pub fn new(path: &Path) -> Result<Self, VerbatimPathError> {
        let absolute = std::path::absolute(path).map_err(VerbatimPathError::Absolute)?;
        let units = encode(&absolute);
        if units.contains(&0) {
            return Err(VerbatimPathError::Nul);
        }

        let mut verbatim = if starts_with(&units, &encode_literal(r"\\?\")) {
            units
        } else if starts_with(&units, &encode_literal(r"\\.\")) {
            return Err(VerbatimPathError::DeviceNamespace);
        } else if starts_with(&units, &encode_literal(r"\\")) {
            let mut output = encode_literal(r"\\?\UNC\");
            output.extend_from_slice(&units[2..]);
            output
        } else if has_drive_root(&units) {
            let mut output = encode_literal(r"\\?\");
            output.extend_from_slice(&units);
            output
        } else {
            return Err(VerbatimPathError::NotAbsolute);
        };
        if verbatim.len() >= 32_767 {
            return Err(VerbatimPathError::TooLong);
        }
        verbatim.push(0);
        Ok(Self { units: verbatim })
    }

    /// Construct a verbatim child path from one raw native filename.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is empty, contains a separator/NUL, or would exceed the
    /// native path limit.
    pub fn join_name(&self, name: &OsStr) -> Result<Self, VerbatimPathError> {
        let name = encode(Path::new(name));
        validate_name(&name)?;
        let mut units = self.units[..self.units.len() - 1].to_vec();
        if units.last().copied() != Some(u16::from(b'\\')) {
            units.push(u16::from(b'\\'));
        }
        units.extend_from_slice(&name);
        if units.len() >= 32_767 {
            return Err(VerbatimPathError::TooLong);
        }
        units.push(0);
        Ok(Self { units })
    }

    /// Return the NUL-terminated UTF-16/WTF-16 units required by Win32.
    pub(crate) fn as_ptr(&self) -> *const u16 {
        self.units.as_ptr()
    }

    /// Return this path as a native `PathBuf`, excluding the terminating NUL.
    pub fn to_path_buf(&self) -> PathBuf {
        use std::os::windows::ffi::OsStringExt as _;
        PathBuf::from(OsString::from_wide(
            &self.units[..self.units.len().saturating_sub(1)],
        ))
    }

    pub(crate) fn from_final_units(mut units: Vec<u16>) -> Result<Self, VerbatimPathError> {
        if units.contains(&0) {
            return Err(VerbatimPathError::Nul);
        }
        if !starts_with(&units, &encode_literal(r"\\?\")) {
            return Err(VerbatimPathError::NotVerbatim);
        }
        if units.len() >= 32_767 {
            return Err(VerbatimPathError::TooLong);
        }
        units.push(0);
        Ok(Self { units })
    }
}

/// Invalid input for a verbatim Windows path.
#[derive(Debug, Error)]
pub enum VerbatimPathError {
    #[error("cannot make the path absolute: {0}")]
    Absolute(std::io::Error),
    #[error("native path data contains a NUL")]
    Nul,
    #[error("Win32 device namespaces are not valid repository paths")]
    DeviceNamespace,
    #[error("the path is not absolute")]
    NotAbsolute,
    #[error("the resolved path is not in the verbatim namespace")]
    NotVerbatim,
    #[error("the path exceeds the Win32 verbatim-path limit")]
    TooLong,
    #[error("a child name is empty")]
    EmptyName,
    #[error("a child name contains a path separator")]
    Separator,
    #[error("a child name selects an alternate data stream")]
    AlternateDataStream,
    #[error("'.' and '..' are not valid child names")]
    Traversal,
}

fn encode(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    path.as_os_str().encode_wide().collect()
}

fn encode_literal(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

fn starts_with(value: &[u16], prefix: &[u16]) -> bool {
    value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn has_drive_root(units: &[u16]) -> bool {
    units.len() >= 3
        && u8::try_from(units[0])
            .ok()
            .is_some_and(|unit| unit.is_ascii_alphabetic())
        && units[1] == u16::from(b':')
        && matches!(units[2], 0x2f | 0x5c)
}

fn validate_name(name: &[u16]) -> Result<(), VerbatimPathError> {
    if name.is_empty() {
        return Err(VerbatimPathError::EmptyName);
    }
    if name.contains(&0) {
        return Err(VerbatimPathError::Nul);
    }
    if name.iter().any(|unit| matches!(*unit, 0x2f | 0x5c)) {
        return Err(VerbatimPathError::Separator);
    }
    if name.contains(&u16::from(b':')) {
        return Err(VerbatimPathError::AlternateDataStream);
    }
    if name == [u16::from(b'.')] || name == [u16::from(b'.'), u16::from(b'.')] {
        return Err(VerbatimPathError::Traversal);
    }
    Ok(())
}

trait U16Ascii {
    fn eq_ignore_ascii_case(&self, other: &[u16]) -> bool;
}

impl U16Ascii for [u16] {
    fn eq_ignore_ascii_case(&self, other: &[u16]) -> bool {
        self.len() == other.len()
            && self.iter().zip(other).all(|(left, right)| {
                u8::try_from(*left)
                    .ok()
                    .zip(u8::try_from(*right).ok())
                    .is_some_and(|(left, right)| left.eq_ignore_ascii_case(&right))
            })
    }
}
