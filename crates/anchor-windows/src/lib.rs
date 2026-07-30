//! Safety-reviewed Win32 filesystem primitives used by Anchor.
//!
//! This crate is deliberately the only Anchor crate allowed to invoke Win32 APIs through
//! `unsafe`. Its public API owns every handle and never exposes raw pointers or handles.

#![cfg(windows)]

mod filesystem;
mod path;

pub use filesystem::{
    DirectoryEntry, DirectoryHandle, FileIdentity, NodeHandle, NodeKind, NodeMetadata, ReparseKind,
    RootHandle, StreamInfo, SymbolicLinkData,
};
pub use path::{VerbatimPath, VerbatimPathError};

use std::io;

use thiserror::Error;

/// Failure from a Windows namespace operation.
#[derive(Debug, Error)]
pub enum WindowsError {
    /// A Win32 call failed.
    #[error("{operation} failed: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    /// The supplied path cannot be represented safely.
    #[error(transparent)]
    Path(#[from] VerbatimPathError),
    /// The object reached through a name differs from the enumerated object.
    #[error("the directory entry changed while it was being opened")]
    IdentityChanged,
    /// A native structure returned by the filesystem is malformed.
    #[error("the filesystem returned malformed {0}")]
    Malformed(&'static str),
    /// A bounded native query exceeded Anchor's safety limit.
    #[error("{0} exceeds Anchor's bounded query limit")]
    TooLarge(&'static str),
    /// An operation requires an ordinary directory.
    #[error("the opened node is not an ordinary directory")]
    NotDirectory,
}

fn io_error(operation: &'static str) -> WindowsError {
    WindowsError::Io {
        operation,
        source: io::Error::last_os_error(),
    }
}
