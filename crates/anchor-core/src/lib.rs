//! Safety-critical, platform-neutral primitives for Anchor.

pub mod capture;
pub mod diff;
pub mod manifest;
pub mod merge;
#[cfg(unix)]
pub mod metadata;
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
    MetadataObservation, Omission, OmissionReason, SafetyObservations, WindowsSymlinkKind,
};
pub use merge::{
    TextMergeConflict, TextMergeError, TextMergeLimits, TextMergeResult,
    inverse_three_way_text_merge,
};
#[cfg(unix)]
pub use metadata::{
    observe_directory_extended_metadata, observe_extended_metadata,
    platform_managed_directory_metadata_equal, platform_managed_metadata_equal,
};
pub use object::{ObjectId, ObjectStore, StoreError};
pub use path::{NativeRelativePath, NativeString, PathEncoding, PathError, PlatformMismatch};
pub use restore::{
    ConflictReason, NoChangeReason, PathRestore, RestoreConflict, RestoreOutcome, RestorePlan,
    RestorePlanError,
};
