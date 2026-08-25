#[cfg(unix)]
mod unix {
    use fs2::FileExt as _;
    use sha2::{Digest, Sha256};
    use std::env;
    use std::fmt::Write as _;
    use std::fs::{self, File, OpenOptions};
    use std::os::unix::fs::{
        FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
    };
    use std::path::{Path, PathBuf};

    const RUNTIME_DIRECTORY_MODE: u32 = 0o700;
    const RUNTIME_FILE_MODE: u32 = 0o600;

    #[derive(Clone, Debug)]
    pub(crate) struct RuntimePaths {
        pub(crate) socket: PathBuf,
        pub(crate) lock: PathBuf,
        lifetime: PathBuf,
        pub(crate) archive_id: String,
        owner_uid: u32,
    }

    #[derive(Debug)]
    pub(crate) struct StartupLock {
        file: File,
    }

    impl Drop for StartupLock {
        fn drop(&mut self) {
            let _ = fs2::FileExt::unlock(&self.file);
        }
    }

    impl RuntimePaths {
        pub(crate) fn resolve(archive_path: &Path) -> Result<Self, String> {
            let base = if let Some(value) = env::var_os("XDG_RUNTIME_DIR") {
                PathBuf::from(value)
            } else {
                let base = env::temp_dir().join(format!(
                    "yarp-runtime-{}",
                    rustix::process::getuid().as_raw()
                ));
                create_private_directory(&base, rustix::process::getuid().as_raw())?;
                base
            };
            Self::resolve_in(&base, archive_path)
        }

        pub(crate) fn resolve_in(base: &Path, archive_path: &Path) -> Result<Self, String> {
            let base_metadata = fs::symlink_metadata(base).map_err(|error| {
                format!(
                    "could not inspect runtime directory {}: {error}",
                    base.display()
                )
            })?;
            if base_metadata.file_type().is_symlink() || !base_metadata.is_dir() {
                return Err(format!(
                    "runtime path {} is not a real directory",
                    base.display()
                ));
            }
            if base_metadata.permissions().mode() & 0o077 != 0 {
                return Err(format!(
                    "runtime directory {} must not be accessible by other users",
                    base.display()
                ));
            }
            let owner_uid = rustix::process::getuid().as_raw();
            if base_metadata.uid() != owner_uid {
                return Err(format!(
                    "runtime directory {} has the wrong owner",
                    base.display()
                ));
            }
            let directory = base.join("yarp");
            create_private_directory(&directory, owner_uid)?;
            let canonical = canonical_archive_path(archive_path)?;
            let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
            let mut archive_id = String::with_capacity(32);
            for byte in &digest[..16] {
                write!(archive_id, "{byte:02x}")
                    .map_err(|error| format!("could not format archive identity: {error}"))?;
            }
            let socket = directory.join(format!("archive-{archive_id}.sock"));
            let lock = directory.join(format!("archive-{archive_id}.lock"));
            let lifetime = directory.join(format!("archive-{archive_id}.lifetime"));
            if socket.as_os_str().as_encoded_bytes().len() >= 104 {
                return Err(format!(
                    "archive broker socket path is too long: {}",
                    socket.display()
                ));
            }
            Ok(Self {
                socket,
                lock,
                lifetime,
                archive_id,
                owner_uid,
            })
        }

        pub(crate) fn lock_exclusive(&self) -> Result<StartupLock, String> {
            lock_file(&self.lock, self.owner_uid, "startup", false)
        }

        pub(crate) fn lock_lifetime(&self) -> Result<StartupLock, String> {
            lock_file(&self.lifetime, self.owner_uid, "lifetime", false)
        }

        pub(crate) fn no_live_broker(&self) -> Result<bool, String> {
            match lock_file(&self.lifetime, self.owner_uid, "lifetime", true) {
                Ok(lock) => {
                    drop(lock);
                    Ok(true)
                }
                Err(error) if error.contains("already locked") => Ok(false),
                Err(error) => Err(error),
            }
        }

        pub(crate) fn remove_stale_socket(&self) -> Result<(), String> {
            let metadata = match fs::symlink_metadata(&self.socket) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => {
                    return Err(format!(
                        "could not inspect archive broker socket {}: {error}",
                        self.socket.display()
                    ));
                }
            };
            require_private_file(&self.socket, &metadata, self.owner_uid, true)?;
            fs::remove_file(&self.socket).map_err(|error| {
                format!(
                    "could not remove stale archive broker socket {}: {error}",
                    self.socket.display()
                )
            })
        }

        pub(crate) fn secure_socket(&self) -> Result<(), String> {
            fs::set_permissions(&self.socket, fs::Permissions::from_mode(RUNTIME_FILE_MODE))
                .map_err(|error| {
                    format!(
                        "could not set archive broker socket permissions {}: {error}",
                        self.socket.display()
                    )
                })?;
            let metadata = fs::symlink_metadata(&self.socket).map_err(|error| {
                format!(
                    "could not inspect archive broker socket {}: {error}",
                    self.socket.display()
                )
            })?;
            require_private_file(&self.socket, &metadata, self.owner_uid, true)
        }

        pub(crate) fn cleanup(&self) -> Result<(), String> {
            self.remove_stale_socket()
        }
    }

    fn canonical_archive_path(path: &Path) -> Result<PathBuf, String> {
        if path.exists() {
            return path.canonicalize().map_err(|error| {
                format!("could not resolve archive path {}: {error}", path.display())
            });
        }
        let mut existing = path
            .parent()
            .ok_or_else(|| format!("archive path {} has no parent", path.display()))?;
        let mut missing = Vec::new();
        while !existing.exists() {
            let name = existing.file_name().ok_or_else(|| {
                format!("could not find an existing parent for {}", path.display())
            })?;
            missing.push(name.to_os_string());
            existing = existing.parent().ok_or_else(|| {
                format!("could not find an existing parent for {}", path.display())
            })?;
        }
        let mut canonical = existing.canonicalize().map_err(|error| {
            format!(
                "could not resolve archive directory {}: {error}",
                existing.display()
            )
        })?;
        for name in missing.iter().rev() {
            canonical.push(name);
        }
        let name = path
            .file_name()
            .ok_or_else(|| format!("archive path {} has no file name", path.display()))?;
        Ok(canonical.join(name))
    }

    fn create_private_directory(path: &Path, owner_uid: u32) -> Result<(), String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(format!(
                        "archive broker runtime path {} is not a real directory",
                        path.display()
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = fs::create_dir(path)
                    && error.kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(format!(
                        "could not create archive broker runtime directory {}: {error}",
                        path.display()
                    ));
                }
            }
            Err(error) => {
                return Err(format!(
                    "could not inspect archive broker runtime directory {}: {error}",
                    path.display()
                ));
            }
        }
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            format!(
                "could not inspect archive broker runtime directory {}: {error}",
                path.display()
            )
        })?;
        if metadata.uid() != owner_uid {
            return Err(format!(
                "archive broker runtime directory {} has the wrong owner",
                path.display()
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(RUNTIME_DIRECTORY_MODE)).map_err(
            |error| {
                format!(
                    "could not set archive broker runtime directory permissions {}: {error}",
                    path.display()
                )
            },
        )?;
        Ok(())
    }

    fn lock_file(
        path: &Path,
        owner_uid: u32,
        label: &str,
        nonblocking: bool,
    ) -> Result<StartupLock, String> {
        reject_symlink_if_present(path, &format!("{label} lock"))?;
        let mut options = OpenOptions::new();
        options
            .create(true)
            .read(true)
            .write(true)
            .mode(RUNTIME_FILE_MODE);
        let file = options.open(path).map_err(|error| {
            format!(
                "could not open archive broker {label} lock {}: {error}",
                path.display()
            )
        })?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("could not inspect archive broker {label} lock: {error}"))?;
        require_private_file(path, &metadata, owner_uid, false)?;
        let result = if nonblocking {
            file.try_lock_exclusive()
        } else {
            file.lock_exclusive()
        };
        match result {
            Ok(()) => Ok(StartupLock { file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(format!("archive broker {label} lock is already locked"))
            }
            Err(error) => Err(format!(
                "could not lock archive broker {label} lock: {error}"
            )),
        }
    }

    fn reject_symlink_if_present(path: &Path, label: &str) -> Result<(), String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
                "archive broker {label} {} is a symlink",
                path.display()
            )),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "could not inspect archive broker {label} {}: {error}",
                path.display()
            )),
        }
    }

    fn require_private_file(
        path: &Path,
        metadata: &fs::Metadata,
        owner_uid: u32,
        socket: bool,
    ) -> Result<(), String> {
        if metadata.file_type().is_symlink()
            || (socket && !metadata.file_type().is_socket())
            || (!socket && !metadata.is_file())
        {
            return Err(format!(
                "archive broker runtime entry {} has the wrong type",
                path.display()
            ));
        }
        if metadata.uid() != owner_uid {
            return Err(format!(
                "archive broker runtime entry {} has the wrong owner",
                path.display()
            ));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "archive broker runtime entry {} is accessible by other users",
                path.display()
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        fn private_base() -> TempDir {
            let directory = TempDir::new().expect("runtime directory");
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private runtime directory");
            directory
        }

        #[test]
        fn runtime_identity_is_private_and_archive_specific() {
            let base = private_base();
            let first = RuntimePaths::resolve_in(base.path(), &base.path().join("data/a.sqlite"))
                .expect("first runtime");
            let second = RuntimePaths::resolve_in(base.path(), &base.path().join("data/b.sqlite"))
                .expect("second runtime");
            assert_ne!(first.archive_id, second.archive_id);
            assert_ne!(first.socket, second.socket);
            let metadata = first
                .lock_exclusive()
                .expect("lock")
                .file
                .metadata()
                .expect("metadata");
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            assert!(first.no_live_broker().expect("no broker"));
            let lifetime = first.lock_lifetime().expect("lifetime lock");
            assert!(!first.no_live_broker().expect("live broker"));
            drop(lifetime);
            assert!(first.no_live_broker().expect("released broker"));
        }

        #[test]
        fn rejects_a_symlinked_startup_lock() {
            let base = private_base();
            let paths = RuntimePaths::resolve_in(base.path(), &base.path().join("data/a.sqlite"))
                .expect("runtime");
            let target = base.path().join("target");
            File::create(&target).expect("target");
            symlink(&target, &paths.lock).expect("symlink");
            assert!(paths.lock_exclusive().unwrap_err().contains("symlink"));
        }

        #[test]
        fn rejects_a_public_runtime_directory() {
            let base = TempDir::new().expect("runtime directory");
            fs::set_permissions(base.path(), fs::Permissions::from_mode(0o755))
                .expect("public mode");
            let error = RuntimePaths::resolve_in(base.path(), &base.path().join("a.sqlite"))
                .expect_err("public runtime must fail");
            assert!(error.contains("other users"));
        }
    }
}

#[cfg(unix)]
pub(crate) use unix::RuntimePaths;

#[cfg(not(unix))]
mod unsupported {
    use std::path::Path;

    pub(crate) struct RuntimePaths;

    impl RuntimePaths {
        pub(crate) fn resolve(_archive_path: &Path) -> Result<Self, String> {
            Err("the local archive broker requires Unix-domain sockets".to_owned())
        }
    }
}

#[cfg(not(unix))]
pub(crate) use unsupported::RuntimePaths;
