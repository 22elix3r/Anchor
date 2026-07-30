use std::fs;
use std::path::{Path, PathBuf};

use anchor_core::{CaptureLimits, CaptureOptions};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const CONFIG_LIMIT: u64 = 1024 * 1024;
const CAPTURE_POLICY_VERSION: u16 = 2;

/// Whether native command arguments are retained in session metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CommandRecording {
    ProgramOnly,
    FullArguments,
}

/// Frozen capture and metadata policy persisted with every new session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapturePolicy {
    pub version: u16,
    #[serde(alias = "max_files")]
    pub max_entries: u64,
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
    pub allow_degraded: bool,
    pub cross_mounts: bool,
    pub command_recording: CommandRecording,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        let limits = CaptureLimits::default();
        Self {
            version: CAPTURE_POLICY_VERSION,
            max_entries: limits.max_entries,
            max_total_bytes: limits.max_total_bytes,
            max_file_bytes: limits.max_file_bytes,
            allow_degraded: false,
            cross_mounts: false,
            command_recording: CommandRecording::ProgramOnly,
        }
    }
}

impl CapturePolicy {
    /// Validate a decoded or configured policy.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] for unknown policy versions or zero-valued limits.
    pub fn validate(self) -> Result<Self, ConfigError> {
        if !matches!(self.version, 1 | CAPTURE_POLICY_VERSION) {
            return Err(ConfigError::UnsupportedPolicy(self.version));
        }
        if self.max_entries == 0 || self.max_total_bytes == 0 || self.max_file_bytes == 0 {
            return Err(ConfigError::ZeroLimit);
        }
        Ok(self)
    }

    #[must_use]
    pub const fn capture_options(self) -> CaptureOptions {
        CaptureOptions {
            limits: CaptureLimits {
                max_entries: self.max_entries,
                max_total_bytes: self.max_total_bytes,
                max_file_bytes: self.max_file_bytes,
            },
            allow_degraded: self.allow_degraded,
            cross_mounts: self.cross_mounts,
        }
    }
}

/// Explicit command-line overrides, applied after user and project configuration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PolicyOverrides {
    pub max_entries: Option<u64>,
    pub max_total_bytes: Option<u64>,
    pub max_file_bytes: Option<u64>,
    pub allow_degraded: bool,
    pub cross_mounts: bool,
    pub record_arguments: bool,
}

impl PolicyOverrides {
    /// Apply explicit user authorization to a resolved policy.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if an explicit limit is zero.
    pub fn apply(self, mut policy: CapturePolicy) -> Result<CapturePolicy, ConfigError> {
        if let Some(value) = self.max_entries {
            policy.max_entries = value;
        }
        if let Some(value) = self.max_total_bytes {
            policy.max_total_bytes = value;
        }
        if let Some(value) = self.max_file_bytes {
            policy.max_file_bytes = value;
        }
        if self.allow_degraded {
            policy.allow_degraded = true;
        }
        if self.cross_mounts {
            policy.cross_mounts = true;
        }
        if self.record_arguments {
            policy.command_recording = CommandRecording::FullArguments;
        }
        policy.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigResolution {
    pub policy: CapturePolicy,
    pub user_config: Option<PathBuf>,
    pub project_config: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct ConfigLoader;

impl ConfigLoader {
    /// Resolve defaults, optional user configuration, and a monotonic project policy.
    ///
    /// The project file is `.anchor/config.toml`. It can lower limits and disable risky
    /// behavior, but cannot enable degraded capture, mount traversal, or argument recording.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] for malformed, oversized, symlinked project, or invalid files.
    pub fn load(worktree_root: &Path) -> Result<ConfigResolution, ConfigError> {
        let mut policy = CapturePolicy::default();
        let user_path = user_config_path();
        let user_config = load_optional(user_path.as_deref(), false)?;
        let loaded_user_path = user_config.as_ref().and(user_path.clone());
        if let Some(config) = user_config.as_ref() {
            policy = config.apply_user(policy)?;
        }

        let project_path = worktree_root.join(".anchor").join("config.toml");
        let project_config = load_optional(Some(&project_path), true)?;
        if let Some(config) = project_config.as_ref() {
            policy = config.apply_project(policy)?;
        }
        Ok(ConfigResolution {
            policy: policy.validate()?,
            user_config: loaded_user_path,
            project_config: project_config.map(|_| project_path),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    capture: CaptureConfig,
    privacy: PrivacyConfig,
}

impl ConfigFile {
    fn apply_user(self, mut policy: CapturePolicy) -> Result<CapturePolicy, ConfigError> {
        self.capture.apply_limits(&mut policy, false);
        if let Some(value) = self.capture.allow_degraded {
            policy.allow_degraded = value;
        }
        if let Some(value) = self.capture.cross_mounts {
            policy.cross_mounts = value;
        }
        if let Some(value) = self.privacy.record_command_arguments {
            policy.command_recording = if value {
                CommandRecording::FullArguments
            } else {
                CommandRecording::ProgramOnly
            };
        }
        policy.validate()
    }

    fn apply_project(self, mut policy: CapturePolicy) -> Result<CapturePolicy, ConfigError> {
        self.capture.apply_limits(&mut policy, true);
        if self.capture.allow_degraded == Some(false) {
            policy.allow_degraded = false;
        }
        if self.capture.cross_mounts == Some(false) {
            policy.cross_mounts = false;
        }
        if self.privacy.record_command_arguments == Some(false) {
            policy.command_recording = CommandRecording::ProgramOnly;
        }
        policy.validate()
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CaptureConfig {
    #[serde(alias = "max_files")]
    max_entries: Option<u64>,
    max_total_bytes: Option<u64>,
    max_file_bytes: Option<u64>,
    allow_degraded: Option<bool>,
    cross_mounts: Option<bool>,
}

impl CaptureConfig {
    fn apply_limits(self, policy: &mut CapturePolicy, only_tighten: bool) {
        if let Some(value) = self.max_entries {
            policy.max_entries = if only_tighten {
                policy.max_entries.min(value)
            } else {
                value
            };
        }
        if let Some(value) = self.max_total_bytes {
            policy.max_total_bytes = if only_tighten {
                policy.max_total_bytes.min(value)
            } else {
                value
            };
        }
        if let Some(value) = self.max_file_bytes {
            policy.max_file_bytes = if only_tighten {
                policy.max_file_bytes.min(value)
            } else {
                value
            };
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PrivacyConfig {
    record_command_arguments: Option<bool>,
}

fn load_optional(
    path: Option<&Path>,
    reject_symlink: bool,
) -> Result<Option<ConfigFile>, ConfigError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if reject_symlink && metadata.file_type().is_symlink() {
        return Err(ConfigError::ProjectSymlink(path.to_path_buf()));
    }
    if !metadata.is_file() {
        return Err(ConfigError::NotRegular(path.to_path_buf()));
    }
    if metadata.len() > CONFIG_LIMIT {
        return Err(ConfigError::TooLarge(path.to_path_buf()));
    }
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text)
        .map(Some)
        .map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
}

fn user_config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ANCHOR_CONFIG_FILE") {
        return Some(PathBuf::from(path));
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|root| root.join("Anchor").join("config.toml"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .map(|root| root.join("anchor").join("config.toml"))
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|root| root.join(".config").join("anchor").join("config.toml"))
            })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("capture policy schema {0} is unsupported")]
    UnsupportedPolicy(u16),
    #[error("capture limits must be greater than zero")]
    ZeroLimit,
    #[error("configuration file is too large: {0}")]
    TooLarge(PathBuf),
    #[error("configuration path is not a regular file: {0}")]
    NotRegular(PathBuf),
    #[error("project configuration cannot be a symlink: {0}")]
    ProjectSymlink(PathBuf),
    #[error("cannot read configuration {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse configuration {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_policy_can_only_tighten_user_policy() {
        let user: ConfigFile = toml::from_str(
            "[capture]\nmax_files=500\nallow_degraded=true\ncross_mounts=true\n\
             [privacy]\nrecord_command_arguments=true\n",
        )
        .unwrap();
        let project: ConfigFile = toml::from_str(
            "[capture]\nmax_files=1000\nallow_degraded=true\ncross_mounts=true\n\
             [privacy]\nrecord_command_arguments=true\n",
        )
        .unwrap();
        let user_policy = user.apply_user(CapturePolicy::default()).unwrap();
        let resolved = project.apply_project(user_policy).unwrap();
        assert_eq!(resolved.max_entries, 500);
        assert!(resolved.allow_degraded);
        assert!(resolved.cross_mounts);
        assert_eq!(resolved.command_recording, CommandRecording::FullArguments);

        let restrictive: ConfigFile = toml::from_str(
            "[capture]\nmax_files=100\nallow_degraded=false\ncross_mounts=false\n\
             [privacy]\nrecord_command_arguments=false\n",
        )
        .unwrap();
        let resolved = restrictive.apply_project(resolved).unwrap();
        assert_eq!(resolved.max_entries, 100);
        assert!(!resolved.allow_degraded);
        assert!(!resolved.cross_mounts);
        assert_eq!(resolved.command_recording, CommandRecording::ProgramOnly);
    }

    #[test]
    fn explicit_overrides_are_validated() {
        let error = PolicyOverrides {
            max_entries: Some(0),
            ..PolicyOverrides::default()
        }
        .apply(CapturePolicy::default())
        .unwrap_err();
        assert!(matches!(error, ConfigError::ZeroLimit));
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(toml::from_str::<ConfigFile>("[capture]\nmagic=true\n").is_err());
    }

    #[test]
    fn legacy_max_files_key_maps_to_the_entry_ceiling() {
        let config: ConfigFile = toml::from_str("[capture]\nmax_files=17\n").unwrap();
        let policy = config.apply_user(CapturePolicy::default()).unwrap();
        assert_eq!(policy.max_entries, 17);
    }

    #[test]
    fn duplicate_legacy_and_current_entry_keys_are_rejected() {
        assert!(toml::from_str::<ConfigFile>("[capture]\nmax_entries=17\nmax_files=18\n").is_err());
    }

    #[test]
    fn persisted_policy_v1_decodes_with_conservative_entry_semantics() {
        #[derive(Serialize)]
        struct LegacyPolicy {
            version: u16,
            max_files: u64,
            max_total_bytes: u64,
            max_file_bytes: u64,
            allow_degraded: bool,
            cross_mounts: bool,
            command_recording: CommandRecording,
        }

        let legacy = LegacyPolicy {
            version: 1,
            max_files: 17,
            max_total_bytes: 100,
            max_file_bytes: 50,
            allow_degraded: false,
            cross_mounts: false,
            command_recording: CommandRecording::ProgramOnly,
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&legacy, &mut bytes).unwrap();
        let decoded: CapturePolicy = ciborium::de::from_reader(bytes.as_slice()).unwrap();

        assert_eq!(decoded.validate().unwrap().max_entries, 17);
    }
}
