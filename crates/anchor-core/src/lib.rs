//! Safety-critical, platform-neutral primitives for Anchor.

pub mod capture;
pub mod manifest;
pub mod object;
pub mod path;
pub mod wire;

pub use capture::{
    CaptureEngine, CaptureError, CaptureLimits, CaptureOptions, CaptureResult, CaptureStatistics,
    IncludeAll, ObservedKind, ScopeClassifier, ScopeDecision, ScopeError,
};
pub use manifest::{
    Completeness, Coverage, Manifest, ManifestEntry, ManifestError, ManifestId, ManifestNode,
    Omission, OmissionReason, SafetyObservations,
};
pub use object::{ObjectId, ObjectStore, StoreError};
pub use path::{NativeRelativePath, NativeString, PathEncoding, PathError, PlatformMismatch};
