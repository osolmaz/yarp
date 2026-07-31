use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub const MAX_DATA_BYTES: u64 = 10_000_000_000;

#[derive(Clone, Copy, Debug)]
pub struct FileIdentity {
    pub device_id: u64,
    pub inode: u64,
    pub size_bytes: u64,
    pub mtime_ns: i64,
}

pub fn default_database_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| Error::InvalidArguments("HOME is not set".to_owned()))?;
    Ok(PathBuf::from(home).join(".local/share/toolcall-extractor/toolcalls.duckdb"))
}

pub fn prepare_database_path(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::InvalidArguments("database path has no parent".to_owned()))?;
    if parent.exists() {
        let metadata = fs::metadata(parent).map_err(|error| Error::io(parent, error))?;
        let mode = metadata.permissions().mode() & 0o777;
        if !metadata.is_dir() || mode != 0o700 {
            return Err(Error::InvalidArguments(format!(
                "database directory {} must already be private (mode 700)",
                parent.display()
            )));
        }
    } else {
        fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
        set_mode(parent, 0o700)?;
    }
    if path.exists() {
        set_mode(path, 0o600)?;
    }
    Ok(())
}

pub fn acquire_database_lock(path: &Path) -> Result<File> {
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|error| Error::io(&lock_path, error))?;
    set_mode(&lock_path, 0o600)?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
        Error::InvalidSource(format!("database is already being written: {error}"))
    })?;
    Ok(file)
}

pub fn protect_database(path: &Path) -> Result<()> {
    set_mode(path, 0o600)?;
    for suffix in ["wal", "tmp"] {
        let sidecar = PathBuf::from(format!("{}.{}", path.display(), suffix));
        if sidecar.exists() {
            set_mode(&sidecar, 0o600)?;
        }
    }
    Ok(())
}

pub fn verify_private(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::InvalidArguments("database path has no parent".to_owned()))?;
    let parent_mode = fs::metadata(parent)
        .map_err(|error| Error::io(parent, error))?
        .permissions()
        .mode()
        & 0o777;
    if parent_mode != 0o700 {
        return Err(Error::InvalidSource(format!(
            "{} has mode {parent_mode:o}, expected 700",
            parent.display()
        )));
    }
    let mode = fs::metadata(path)
        .map_err(|error| Error::io(path, error))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(Error::InvalidSource(format!(
            "{} has mode {mode:o}, expected 600",
            path.display()
        )));
    }
    Ok(())
}

pub fn data_directory_bytes(path: &Path) -> Result<u64> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::InvalidArguments("database path has no parent".to_owned()))?;
    let mut total = 0_u64;
    for entry in fs::read_dir(parent).map_err(|error| Error::io(parent, error))? {
        let entry = entry.map_err(|error| Error::io(parent, error))?;
        let metadata = entry
            .metadata()
            .map_err(|error| Error::io(entry.path(), error))?;
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

pub fn enforce_size_limit(path: &Path, incoming_upper_bound: u64) -> Result<()> {
    let current = data_directory_bytes(path)?;
    if current.saturating_add(incoming_upper_bound) > MAX_DATA_BYTES {
        return Err(Error::SizeLimit);
    }
    Ok(())
}

pub fn identity(path: &Path) -> Result<FileIdentity> {
    let metadata = fs::metadata(path).map_err(|error| Error::io(path, error))?;
    if !metadata.is_file() {
        return Err(Error::InvalidSource(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mtime_ns = metadata
        .mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(metadata.mtime_nsec());
    Ok(FileIdentity {
        device_id: metadata.dev(),
        inode: metadata.ino(),
        size_bytes: metadata.len(),
        mtime_ns,
    })
}

pub fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let canonical_root = fs::canonicalize(root).map_err(|error| Error::io(root, error))?;
    let canonical_path = fs::canonicalize(path).map_err(|error| Error::io(path, error))?;
    let relative = canonical_path.strip_prefix(&canonical_root).map_err(|_| {
        Error::InvalidSource(format!("{} escapes {}", path.display(), root.display()))
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(".".to_owned());
    }
    Ok(relative.to_string_lossy().into_owned())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| Error::io(path, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_private_database_parent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("data/toolcalls.duckdb");
        prepare_database_path(&path).expect("prepare");
        assert_eq!(
            fs::metadata(path.parent().expect("parent"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let shared = temp.path().join("shared");
        fs::create_dir(&shared).expect("shared");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).expect("shared mode");
        assert!(prepare_database_path(&shared.join("toolcalls.duckdb")).is_err());
        assert_eq!(
            fs::metadata(&shared)
                .expect("shared metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn rejects_path_outside_root() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::NamedTempFile::new().expect("file");
        assert!(relative_path(root.path(), outside.path()).is_err());
    }

    #[test]
    fn protects_locks_measures_and_identifies_owned_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("data/toolcalls.duckdb");
        prepare_database_path(&path).expect("prepare");
        fs::write(&path, b"data").expect("database");
        fs::write(format!("{}.wal", path.display()), b"wal").expect("wal");
        protect_database(&path).expect("protect");
        verify_private(&path).expect("private");
        let file_identity = identity(&path).expect("identity");
        assert_eq!(file_identity.size_bytes, 4);
        assert_eq!(
            relative_path(temp.path(), &path).expect("relative"),
            "data/toolcalls.duckdb"
        );
        assert_eq!(relative_path(temp.path(), temp.path()).expect("root"), ".");
        assert!(identity(temp.path()).is_err());
        assert_eq!(data_directory_bytes(&path).expect("size"), 7);
        enforce_size_limit(&path, 1).expect("under limit");
        assert!(enforce_size_limit(&path, MAX_DATA_BYTES).is_err());

        let lock = acquire_database_lock(&path).expect("lock");
        assert!(acquire_database_lock(&path).is_err());
        drop(lock);
        acquire_database_lock(&path).expect("relock");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("broaden");
        assert!(verify_private(&path).is_err());
        protect_database(&path).expect("repair");
        verify_private(&path).expect("repaired");
    }
}
