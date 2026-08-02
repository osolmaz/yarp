use std::fs;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::model::{PackManifest, Rule};
use crate::strict_json;
use crate::validation::{
    MAX_SOURCE_BYTES, MAX_SOURCE_FILE_BYTES, validate_manifest, validate_rules,
};

const MANIFEST_NAME: &str = "pack.json";
const SOURCE_DIGEST_DOMAIN: &[u8] = b"yarp-rule-source-v1\0";

#[derive(Clone, Debug)]
pub struct SourcePack {
    pub root: PathBuf,
    pub manifest: PackManifest,
    pub rules: Vec<Rule>,
    pub source_digest: [u8; 32],
}

impl SourcePack {
    /// Load and validate one explicit source-pack directory.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths, invalid JSON, schema errors, conflicts, or size limits.
    pub fn load(root: &Path) -> Result<Self, String> {
        reject_symlink(root, "source pack root")?;
        let root = fs::canonicalize(root)
            .map_err(|error| format!("could not resolve source pack root: {error}"))?;
        if !root.is_dir() {
            return Err("source pack root is not a directory".to_owned());
        }
        let manifest_path = root.join(MANIFEST_NAME);
        let manifest_bytes = read_source_file(&root, &manifest_path, MANIFEST_NAME)?;
        let manifest: PackManifest = strict_json::from_slice(&manifest_bytes)
            .map_err(|error| format!("{MANIFEST_NAME}: {error}"))?;
        validate_manifest(&manifest).map_err(|error| format!("{MANIFEST_NAME}: {error}"))?;

        let mut total_bytes = manifest_bytes.len();
        let mut source_parts = vec![(MANIFEST_NAME.to_owned(), manifest_bytes)];
        let mut rules = Vec::with_capacity(manifest.rules.len());
        for relative in &manifest.rules {
            validate_relative_path(relative)?;
            let path = root.join(relative);
            let body = read_source_file(&root, &path, relative)?;
            total_bytes = total_bytes
                .checked_add(body.len())
                .ok_or_else(|| "source pack byte count overflowed".to_owned())?;
            if total_bytes > MAX_SOURCE_BYTES {
                return Err(format!("source pack exceeds {MAX_SOURCE_BYTES} bytes"));
            }
            let rule: Rule =
                strict_json::from_slice(&body).map_err(|error| format!("{relative}: {error}"))?;
            rules.push(rule);
            source_parts.push((relative.clone(), body));
        }
        validate_rules(&rules)?;
        let source_digest = hash_source_parts(&source_parts)?;
        Ok(Self {
            root,
            manifest,
            rules,
            source_digest,
        })
    }
}

fn read_source_file(root: &Path, path: &Path, label: &str) -> Result<Vec<u8>, String> {
    reject_path_symlinks(root, path, label)?;
    let metadata = fs::metadata(path).map_err(|error| format!("{label}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{label}: expected a regular file"));
    }
    if metadata.len() > MAX_SOURCE_FILE_BYTES as u64 {
        return Err(format!(
            "{label}: source file exceeds {MAX_SOURCE_FILE_BYTES} bytes"
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|error| format!("{label}: {error}"))?;
    if !canonical.starts_with(root) {
        return Err(format!("{label}: path escapes source pack root"));
    }
    let body = fs::read(&canonical).map_err(|error| format!("{label}: {error}"))?;
    if body.len() > MAX_SOURCE_FILE_BYTES {
        return Err(format!(
            "{label}: source file exceeds {MAX_SOURCE_FILE_BYTES} bytes"
        ));
    }
    Ok(body)
}

fn reject_symlink(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| format!("{label}: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{label}: symlinks are not allowed"));
    }
    Ok(())
}

fn reject_path_symlinks(root: &Path, path: &Path, label: &str) -> Result<(), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("{label}: path escapes source pack root"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        reject_symlink(&current, label)?;
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    let parsed = Path::new(path);
    if parsed.is_absolute()
        || parsed
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("invalid rule path: {path}"));
    }
    Ok(())
}

fn hash_source_parts(parts: &[(String, Vec<u8>)]) -> Result<[u8; 32], String> {
    let mut hasher = Sha256::new();
    hasher.update(SOURCE_DIGEST_DOMAIN);
    for (path, body) in parts {
        hash_length_and_bytes(&mut hasher, path.as_bytes())?;
        hash_length_and_bytes(&mut hasher, body)?;
    }
    Ok(hasher.finalize().into())
}

fn hash_length_and_bytes(hasher: &mut Sha256, bytes: &[u8]) -> Result<(), String> {
    let length = u64::try_from(bytes.len())
        .map_err(|_| "source component length does not fit u64".to_owned())?;
    hasher.update(length.to_le_bytes());
    hasher.update(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn loads_a_strict_source_pack_deterministically() {
        let directory = TempDir::new().expect("temp directory");
        fs::create_dir(directory.path().join("rules")).expect("rules directory");
        fs::write(
            directory.path().join("pack.json"),
            r#"{"schema_version":1,"id":"test-pack","rules":["rules/test.json"]}"#,
        )
        .expect("manifest");
        fs::write(
            directory.path().join("rules/test.json"),
            r#"{"id":"tests/run","match":{"program":["test"],"argv_prefix":["run"]},"action":"reduce","reducer":{"kind":"test_summary"},"success":{"max_line_bytes":16384,"max_output_bytes":32768,"min_savings_bytes":120,"min_savings_basis_points":1000},"failure":{"max_line_bytes":16384,"max_output_bytes":65536,"min_savings_bytes":120,"min_savings_basis_points":500}}"#,
        )
        .expect("rule");
        let first = SourcePack::load(directory.path()).expect("source pack");
        let second = SourcePack::load(directory.path()).expect("source pack");
        assert_eq!(first.source_digest, second.source_digest);
        assert_eq!(first.rules.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_rule_directories() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new().expect("temp directory");
        let outside = TempDir::new().expect("outside directory");
        fs::write(
            directory.path().join("pack.json"),
            r#"{"schema_version":1,"id":"test-pack","rules":["rules/test.json"]}"#,
        )
        .expect("manifest");
        fs::write(outside.path().join("test.json"), b"{}").expect("outside rule");
        symlink(outside.path(), directory.path().join("rules")).expect("symlink");
        let error = SourcePack::load(directory.path()).expect_err("symlink rejection");
        assert!(error.contains("symlink"));
    }

    #[test]
    fn rejects_duplicate_json_keys() {
        let directory = TempDir::new().expect("temp directory");
        let mut file = fs::File::create(directory.path().join("pack.json")).expect("manifest");
        write!(
            file,
            r#"{{"schema_version":1,"id":"one","id":"two","rules":[]}}"#
        )
        .expect("write");
        let error = SourcePack::load(directory.path()).expect_err("duplicate key");
        assert!(error.contains("duplicate object key"));
    }
}
