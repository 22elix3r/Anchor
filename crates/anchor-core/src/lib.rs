//! Safety-critical, platform-neutral primitives for Anchor.

pub mod path;
pub mod wire;

pub use path::{NativeRelativePath, NativeString, PathEncoding, PathError, PlatformMismatch};
