use std::ffi::OsStr;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use thiserror::Error;
use uuid::Uuid;

/// A retained directory capability for Fence's private store.
#[derive(Clone, Debug)]
pub struct StoreFs {
    root: PathBuf,
    directory: Arc<Dir>,
}

impl StoreFs {
    /// Open or create a store root below one trusted ambient directory.
    ///
    /// Every component below `trusted_parent` is created and opened separately. Unix opens use
    /// `O_NOFOLLOW`; every component must belong to the effective uid and have mode `0700`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] for traversal, symlinks, ownership, permissions, or I/O errors.
    pub fn open_beneath(
        trusted_parent: impl AsRef<Path>,
        relative_root: impl AsRef<Path>,
    ) -> Result<Self, StoreFsError> {
        let trusted_parent = trusted_parent.as_ref();
        let relative_root = validate_relative(relative_root.as_ref())?;
        let mut directory = Dir::open_ambient_dir(trusted_parent, ambient_authority())?;
        #[cfg(windows)]
        let mut current_path = trusted_parent.to_path_buf();
        for component in relative_root.components() {
            let Component::Normal(name) = component else {
                return Err(StoreFsError::UnsafeRelativePath(
                    relative_root.to_path_buf(),
                ));
            };
            directory = open_or_create_private_component(&directory, name, false)?;
            #[cfg(windows)]
            {
                current_path.push(name);
                fence_windows::harden_private_directory(&current_path)?;
            }
        }
        Ok(Self {
            root: trusted_parent.join(relative_root),
            directory: Arc::new(directory),
        })
    }

    /// Open or create a directly named store root.
    ///
    /// This constructor is intended for isolated test/application-data roots. Repository-backed
    /// production code should use [`Self::open_beneath`].
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] if the root has no parent/name or cannot be secured.
    pub fn open_ambient(root: impl AsRef<Path>) -> Result<Self, StoreFsError> {
        let root = root.as_ref();
        let parent = root.parent().ok_or(StoreFsError::InvalidRoot)?;
        let name = root.file_name().ok_or(StoreFsError::InvalidRoot)?;
        let parent = Dir::open_ambient_dir(parent, ambient_authority())?;
        let directory = open_or_create_private_component(&parent, name, true)?;
        #[cfg(windows)]
        fence_windows::harden_private_directory(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            directory: Arc::new(directory),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ensure and open a private store directory relative to the retained root.
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] for unsafe components or metadata.
    pub fn ensure_dir(&self, relative: impl AsRef<Path>) -> Result<Dir, StoreFsError> {
        let relative = validate_relative_or_empty(relative.as_ref())?;
        let mut directory = self.directory.try_clone()?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(StoreFsError::UnsafeRelativePath(relative.to_path_buf()));
            };
            directory = open_or_create_private_component(&directory, name, false)?;
        }
        Ok(directory)
    }

    /// Open an existing directory relative to the retained root.
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] for missing, redirected, or unsafe components.
    pub fn open_dir(&self, relative: impl AsRef<Path>) -> Result<Dir, StoreFsError> {
        let relative = validate_relative_or_empty(relative.as_ref())?;
        let mut directory = self.directory.try_clone()?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(StoreFsError::UnsafeRelativePath(relative.to_path_buf()));
            };
            directory = open_existing_private_component(&directory, name)?;
        }
        Ok(directory)
    }

    /// Create one private directory and fail if its final name already exists.
    ///
    /// # Errors
    ///
    /// Returns `AlreadyExists` through [`StoreFsError::Io`] on a collision.
    pub fn create_dir_exclusive(&self, relative: impl AsRef<Path>) -> Result<Dir, StoreFsError> {
        let relative = validate_relative(relative.as_ref())?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let name = relative.file_name().ok_or(StoreFsError::InvalidRoot)?;
        let parent = self.open_dir(parent)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsFd as _;
            rustix::fs::mkdirat(parent.as_fd(), name, rustix::fs::Mode::from_raw_mode(0o700))
                .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        }
        #[cfg(not(unix))]
        parent.create_dir(name)?;
        #[cfg(windows)]
        fence_windows::harden_private_directory(&self.root.join(relative))?;
        open_existing_private_component(&parent, name)
    }

    /// Create an exclusively named private temporary file in a store directory.
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] if the parent is unsafe or a file cannot be created.
    pub fn temporary_file(&self, parent: impl AsRef<Path>) -> Result<StoreTempFile, StoreFsError> {
        let directory = self.ensure_dir(parent)?;
        for _ in 0..16 {
            let name = format!(".fence-tmp-{}", Uuid::now_v7());
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match directory.open_with(&name, &options) {
                Ok(file) => {
                    return Ok(StoreTempFile {
                        directory,
                        name,
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(StoreFsError::TemporaryNameExhausted)
    }

    /// Open a regular, owner-controlled store record relative to the retained root.
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] for a symlink, non-file, ownership mismatch, or I/O error.
    pub fn open_file(&self, relative: impl AsRef<Path>) -> Result<cap_std::fs::File, StoreFsError> {
        let relative = validate_relative(relative.as_ref())?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let name = relative.file_name().ok_or(StoreFsError::InvalidRoot)?;
        let directory = self.open_dir(parent)?;
        let before = directory.symlink_metadata(name)?;
        if !before.is_file() {
            return Err(StoreFsError::UnsafeFile(relative.to_path_buf()));
        }
        validate_owner(&before, relative)?;
        let file = directory.open(name)?;
        let opened = file.metadata()?;
        validate_owner(&opened, relative)?;
        let after = directory.symlink_metadata(name)?;
        if !after.is_file() || !same_identity(&before, &opened) || !same_identity(&opened, &after) {
            return Err(StoreFsError::UnstableFile(relative.to_path_buf()));
        }
        Ok(file)
    }

    /// Open or create an owner-controlled lock file below the retained root.
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] for unsafe parents, file type, ownership, or I/O errors.
    pub fn open_lock(&self, relative: impl AsRef<Path>) -> Result<std::fs::File, StoreFsError> {
        let relative = validate_relative(relative.as_ref())?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let name = relative.file_name().ok_or(StoreFsError::InvalidRoot)?;
        let directory = self.open_dir(parent)?;
        let before = match directory.symlink_metadata(name) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return Err(StoreFsError::UnsafeFile(relative.to_path_buf()));
                }
                validate_owner(&metadata, relative)?;
                Some(metadata)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            let nofollow = i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits())
                .map_err(|_| io::Error::other("O_NOFOLLOW does not fit the platform flag type"))?;
            options.mode(0o600).custom_flags(nofollow);
        }
        let file = directory.open_with(name, &options)?;
        let opened = file.metadata()?;
        if !opened.is_file() {
            return Err(StoreFsError::UnsafeFile(relative.to_path_buf()));
        }
        validate_owner(&opened, relative)?;
        let after = directory.symlink_metadata(name)?;
        if !after.is_file()
            || !same_identity(&opened, &after)
            || before
                .as_ref()
                .is_some_and(|metadata| !same_identity(metadata, &opened))
        {
            return Err(StoreFsError::UnstableFile(relative.to_path_buf()));
        }
        Ok(file.into_std())
    }

    /// Read a bounded store record through the retained capability.
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] for unsafe metadata, excess size, instability, or I/O errors.
    pub fn read_bounded(
        &self,
        relative: impl AsRef<Path>,
        maximum: usize,
    ) -> Result<Vec<u8>, StoreFsError> {
        let relative = relative.as_ref();
        let mut file = self.open_file(relative)?;
        let before = file.metadata()?;
        let length = usize::try_from(before.len()).map_err(|_| StoreFsError::LimitExceeded)?;
        if length > maximum {
            return Err(StoreFsError::LimitExceeded);
        }
        let mut bytes = Vec::with_capacity(length);
        Read::by_ref(&mut file)
            .take(u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1))
            .read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        if bytes.len() != length
            || !same_identity(&before, &after)
            || before.len() != after.len()
            || !self.file_identity_matches(relative, &after)?
        {
            return Err(StoreFsError::UnstableFile(relative.to_path_buf()));
        }
        Ok(bytes)
    }

    /// Recheck that a relative file name still resolves to an opened identity.
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] if the current name cannot be opened safely.
    pub fn file_identity_matches(
        &self,
        relative: impl AsRef<Path>,
        expected: &cap_std::fs::Metadata,
    ) -> Result<bool, StoreFsError> {
        let current = self.open_file(relative)?;
        Ok(same_identity(expected, &current.metadata()?))
    }

    /// Sync the retained root directory.
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] if the directory cannot be cloned or synchronized.
    pub fn sync_root(&self) -> Result<(), StoreFsError> {
        self.directory.try_clone()?.into_std_file().sync_all()?;
        Ok(())
    }

    /// Recheck the retained root's ownership and private permissions.
    ///
    /// # Errors
    ///
    /// Returns [`StoreFsError`] if metadata cannot be queried.
    pub fn root_is_private(&self) -> Result<bool, StoreFsError> {
        #[cfg(windows)]
        return Ok(fence_windows::private_directory_is_hardened(&self.root)?);
        #[cfg(not(windows))]
        {
            let metadata = self.directory.dir_metadata()?;
            if validate_owner(&metadata, Path::new("")).is_err() {
                return Ok(false);
            }
            Ok(validate_private_permissions(&metadata, Path::new("")).is_ok())
        }
    }
}

/// A capability-rooted temporary store record.
#[derive(Debug)]
pub struct StoreTempFile {
    directory: Dir,
    name: String,
    file: Option<cap_std::fs::File>,
}

impl StoreTempFile {
    /// Return the private temporary entry name relative to its retained directory.
    #[must_use]
    pub fn name(&self) -> &OsStr {
        OsStr::new(&self.name)
    }

    /// Borrow the still-live temporary file.
    ///
    /// # Panics
    ///
    /// Panics only after the file has been consumed by a publication method; those methods consume
    /// `self`, so safe callers cannot observe that state.
    pub fn file_mut(&mut self) -> &mut cap_std::fs::File {
        self.file.as_mut().expect("temporary file is present")
    }

    /// Publish without replacing an existing destination.
    ///
    /// # Errors
    ///
    /// Returns an `AlreadyExists` I/O error when the destination is already present.
    pub fn persist_noclobber(self, destination: impl AsRef<OsStr>) -> io::Result<()> {
        let destination_directory = self.directory.try_clone()?;
        self.persist_noclobber_in(&destination_directory, destination)
    }

    /// Publish without replacing an existing destination in another retained store directory.
    ///
    /// # Errors
    ///
    /// Returns an `AlreadyExists` I/O error when the destination is already present.
    pub fn persist_noclobber_in(
        mut self,
        destination_directory: &Dir,
        destination: impl AsRef<OsStr>,
    ) -> io::Result<()> {
        self.file_mut().sync_all()?;
        self.directory.hard_link(
            &self.name,
            destination_directory,
            Path::new(destination.as_ref()),
        )?;
        self.directory.remove_file(&self.name)?;
        self.file.take();
        destination_directory
            .try_clone()?
            .into_std_file()
            .sync_all()
    }

    /// Atomically replace a mutable destination.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if file or directory durability fails.
    pub fn replace(mut self, destination: impl AsRef<OsStr>) -> io::Result<()> {
        self.file_mut().sync_all()?;
        self.directory
            .rename(&self.name, &self.directory, Path::new(destination.as_ref()))?;
        self.file.take();
        self.directory.try_clone()?.into_std_file().sync_all()
    }
}

impl Read for StoreTempFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file_mut().read(buffer)
    }
}

impl Write for StoreTempFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file_mut().write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file_mut().flush()
    }
}

impl Seek for StoreTempFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file_mut().seek(position)
    }
}

impl Drop for StoreTempFile {
    fn drop(&mut self) {
        let _ = self.directory.remove_file(&self.name);
    }
}

fn validate_relative(path: &Path) -> Result<&Path, StoreFsError> {
    if path.as_os_str().is_empty() {
        return Err(StoreFsError::UnsafeRelativePath(path.to_path_buf()));
    }
    validate_relative_or_empty(path)
}

fn validate_relative_or_empty(path: &Path) -> Result<&Path, StoreFsError> {
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(StoreFsError::UnsafeRelativePath(path.to_path_buf()));
    }
    Ok(path)
}

fn open_or_create_private_component(
    parent: &Dir,
    name: &OsStr,
    repair_permissions: bool,
) -> Result<Dir, StoreFsError> {
    let created = match parent.symlink_metadata(name) {
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::fd::AsFd as _;
                rustix::fs::mkdirat(parent.as_fd(), name, rustix::fs::Mode::from_raw_mode(0o700))
                    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
            }
            #[cfg(not(unix))]
            parent.create_dir(name)?;
            true
        }
        Err(error) => return Err(error.into()),
    };
    #[cfg(not(unix))]
    let _ = (repair_permissions, created);
    #[cfg(unix)]
    if repair_permissions && !created {
        use cap_std::fs::{Permissions, PermissionsExt as _};
        parent.set_permissions(name, Permissions::from_mode(0o700))?;
    }
    let directory = open_existing_private_component(parent, name)?;
    Ok(directory)
}

fn open_existing_private_component(parent: &Dir, name: &OsStr) -> Result<Dir, StoreFsError> {
    let metadata = parent.symlink_metadata(name)?;
    if !metadata.is_dir() {
        return Err(StoreFsError::UnsafeDirectory(PathBuf::from(name)));
    }
    validate_owner(&metadata, Path::new(name))?;
    validate_private_permissions(&metadata, Path::new(name))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsFd as _;
        use std::os::fd::OwnedFd;

        let descriptor: OwnedFd = rustix::fs::openat(
            parent.as_fd(),
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        let directory = Dir::from_std_file(std::fs::File::from(descriptor));
        let opened = directory.dir_metadata()?;
        if !same_identity(&metadata, &opened) {
            return Err(StoreFsError::UnstableDirectory(PathBuf::from(name)));
        }
        Ok(directory)
    }
    #[cfg(not(unix))]
    {
        let directory = parent.open_dir(name)?;
        let opened = directory.dir_metadata()?;
        if !same_identity(&metadata, &opened) {
            return Err(StoreFsError::UnstableDirectory(PathBuf::from(name)));
        }
        Ok(directory)
    }
}

#[cfg(unix)]
fn validate_owner(metadata: &cap_std::fs::Metadata, path: &Path) -> Result<(), StoreFsError> {
    use cap_std::fs::MetadataExt as _;

    let expected_uid = rustix::process::geteuid().as_raw();
    if metadata.uid() != expected_uid {
        return Err(StoreFsError::OwnershipMismatch {
            path: path.to_path_buf(),
            expected_uid,
            actual_uid: metadata.uid(),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn validate_owner(_metadata: &cap_std::fs::Metadata, _path: &Path) -> Result<(), StoreFsError> {
    Ok(())
}

#[cfg(unix)]
fn validate_private_permissions(
    metadata: &cap_std::fs::Metadata,
    path: &Path,
) -> Result<(), StoreFsError> {
    use cap_std::fs::MetadataExt as _;

    if metadata.mode() & 0o077 != 0 {
        return Err(StoreFsError::UnsafePermissions(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn validate_private_permissions(
    _metadata: &cap_std::fs::Metadata,
    _path: &Path,
) -> Result<(), StoreFsError> {
    Ok(())
}

#[cfg(unix)]
fn same_identity(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_identity(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    use cap_fs_ext::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(any(unix, windows)))]
fn same_identity(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

#[derive(Debug, Error)]
pub enum StoreFsError {
    #[error("store root has no safe parent and final component")]
    InvalidRoot,
    #[error("unsafe relative store path: {0}")]
    UnsafeRelativePath(PathBuf),
    #[error("Fence store directory is a symlink or non-directory: {0}")]
    UnsafeDirectory(PathBuf),
    #[error("Fence store directory changed while it was opened: {0}")]
    UnstableDirectory(PathBuf),
    #[error("Fence store record is a symlink or non-file: {0}")]
    UnsafeFile(PathBuf),
    #[error("Fence store record changed while it was read: {0}")]
    UnstableFile(PathBuf),
    #[cfg(unix)]
    #[error(
        "Fence store path {path} belongs to uid {actual_uid}, expected effective uid {expected_uid}"
    )]
    OwnershipMismatch {
        path: PathBuf,
        expected_uid: u32,
        actual_uid: u32,
    },
    #[error("Fence store directory has group/other permissions: {0}")]
    UnsafePermissions(PathBuf),
    #[error("store temporary-name retries were exhausted")]
    TemporaryNameExhausted,
    #[error("store record exceeds its configured read limit")]
    LimitExceeded,
    #[cfg(windows)]
    #[error(transparent)]
    Windows(#[from] fence_windows::WindowsError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn refuses_an_intermediate_symlink_below_the_trusted_parent() {
        let trusted = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), trusted.path().join("fence")).unwrap();

        assert!(StoreFs::open_beneath(trusted.path(), "fence/users/u1/v1").is_err());
        assert!(!outside.path().join("users").exists());
    }

    #[test]
    fn refuses_weak_existing_store_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let trusted = tempfile::tempdir().unwrap();
        let component = trusted.path().join("fence");
        std::fs::create_dir(&component).unwrap();
        std::fs::set_permissions(&component, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            StoreFs::open_beneath(trusted.path(), "fence/users/u1/v1"),
            Err(StoreFsError::UnsafePermissions(_))
        ));
    }

    #[test]
    fn refuses_a_symlinked_lock_record() {
        let trusted = tempfile::tempdir().unwrap();
        let store = StoreFs::open_beneath(trusted.path(), "fence").unwrap();
        store.ensure_dir("locks").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            trusted.path().join("fence/locks/session.lock"),
        )
        .unwrap();

        assert!(matches!(
            store.open_lock("locks/session.lock"),
            Err(StoreFsError::UnsafeFile(_))
        ));
    }
}
