//! Safety-reviewed Win32 filesystem primitives used by Anchor.
//!
//! This crate is deliberately the only Anchor crate allowed to invoke Win32 APIs through
//! `unsafe`. Its public API owns every handle and never exposes raw pointers or handles.

#![cfg(windows)]

mod filesystem;
mod mutation;
mod path;
mod system;

pub use filesystem::{
    DirectoryEntry, DirectoryHandle, FileIdentity, NodeHandle, NodeKind, NodeMetadata, ReparseKind,
    RootHandle, StreamInfo, SymbolicLinkData,
};
pub use mutation::MutationRoot;
pub use path::{VerbatimPath, VerbatimPathError};
pub use system::{KillOnCloseJob, harden_private_directory, local_app_data};

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
    /// A Windows API returned a failing HRESULT/NT-style status.
    #[error("{operation} failed with status 0x{status:08x}")]
    NativeStatus {
        operation: &'static str,
        status: i32,
    },
    /// An ntdll handle-relative operation returned a failing NTSTATUS.
    #[error("{operation} failed with NTSTATUS 0x{status:08x}")]
    NtStatus {
        operation: &'static str,
        status: i32,
    },
    /// An operation requires an ordinary directory.
    #[error("the opened node is not an ordinary directory")]
    NotDirectory,
    /// A supposedly private storage directory is a reparse point.
    #[error("private storage directory is a reparse point or is not a directory")]
    PrivateDirectoryReparse,
}

fn io_error(operation: &'static str) -> WindowsError {
    WindowsError::Io {
        operation,
        source: io::Error::last_os_error(),
    }
}
