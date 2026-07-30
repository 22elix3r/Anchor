//! Safety-critical, platform-neutral primitives for Anchor.

pub mod capture;
pub mod diff;
pub mod manifest;
pub mod object;
pub mod path;
pub mod restore;
pub mod wire;

pub use capture::{
    CaptureEngine, CaptureError, CaptureLimits, CaptureOptions, CaptureResult, CaptureStatistics,
    IncludeAll, ObservedKind, ScopeClassifier, ScopeDecision, ScopeError,
};
pub use diff::{ChangeKind, ManifestChange, ManifestDiff};
pub use manifest::{
    Completeness, Coverage, Manifest, ManifestEntry, ManifestError, ManifestId, ManifestNode,
    Omission, OmissionReason, SafetyObservations,
};
pub use object::{ObjectId, ObjectStore, StoreError};
pub use path::{NativeRelativePath, NativeString, PathEncoding, PathError, PlatformMismatch};
pub use restore::{
    ConflictReason, NoChangeReason, PathRestore, RestoreConflict, RestoreOutcome, RestorePlan,
    RestorePlanError,
};
