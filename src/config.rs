use std::collections::BTreeSet;
use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::rule_pack::CompiledPack;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use toml_edit::{Array, DocumentMut, Item, Table, value};

pub const CONFIG_VERSION: u32 = 1;
pub const DEFAULT_OUTPUT_CAP_BYTES: usize = 5 * 1024;
pub const MIN_OUTPUT_CAP_BYTES: usize = 1024;
pub const MAX_OUTPUT_CAP_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_RECOVERY_CAP_BYTES: usize = 32 * 1024;
pub const MIN_RECOVERY_CAP_BYTES: usize = 1024;
pub const MAX_RECOVERY_CAP_BYTES: usize = 48 * 1024;
pub const DEFAULT_RECOVERY_CAP_LINES: usize = 1900;
pub const MIN_RECOVERY_CAP_LINES: usize = 1;
pub const MAX_RECOVERY_CAP_LINES: usize = 1900;

const DEFAULT_DOCUMENT: &str = r"# YARP configuration
version = 1

[pruning]
enabled = true

[output]
cap_bytes = 5120
recovery_cap_bytes = 32768
recovery_cap_lines = 1900

[archive]
enabled = true
# Omit path to use the XDG data directory.

[rules]
packs = []
";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    version: u32,
    #[serde(default)]
    pruning: FilePruning,
    #[serde(default)]
    output: FileOutput,
    #[serde(default)]
    archive: FileArchive,
    #[serde(default)]
    rules: FileRules,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePruning {
    enabled: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileOutput {
    cap_bytes: Option<usize>,
    recovery_cap_bytes: Option<usize>,
    recovery_cap_lines: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileArchive {
    enabled: Option<bool>,
    path: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRules {
    #[serde(default)]
    packs: Option<Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolvedConfig {
    pub version: u32,
    pub pruning: PruningConfig,
    pub output: OutputConfig,
    pub archive: ArchiveConfig,
    pub rules: RulesConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PruningConfig {
    pub enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OutputConfig {
    pub cap_bytes: usize,
    pub recovery_cap_bytes: usize,
    pub recovery_cap_lines: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArchiveConfig {
    pub enabled: bool,
    pub path: PathBuf,
    #[serde(skip)]
    pub is_default_path: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RulesConfig {
    pub packs: Vec<PathBuf>,
}

/// Resolve the active user configuration path.
///
/// # Errors
///
/// Returns an error when neither an absolute XDG config directory nor a home directory exists.
pub fn path() -> Result<PathBuf, String> {
    if let Some(home) = std::env::var_os("XDG_CONFIG_HOME") {
        return absolute_directory(&home, "XDG_CONFIG_HOME")
            .map(|directory| directory.join("yarp/config.toml"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "could not resolve YARP configuration: HOME is not set".to_owned())?;
    absolute_directory(&home, "HOME").map(|directory| directory.join(".config/yarp/config.toml"))
}

/// Load and validate the active configuration or built-in defaults.
///
/// # Errors
///
/// Returns an error for path resolution, file I/O, TOML, schema, bounds, or rule-pack failures.
pub fn load() -> Result<ResolvedConfig, String> {
    load_from(&path()?)
}

/// Load and validate one explicit configuration path.
///
/// # Errors
///
/// Returns an error for file I/O, TOML, schema, bounds, path, or rule-pack failures.
pub fn load_from(config_path: &Path) -> Result<ResolvedConfig, String> {
    let body = match fs::read_to_string(config_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return defaults();
        }
        Err(error) => {
            return Err(format!("could not read {}: {error}", config_path.display()));
        }
    };
    let document = body
        .parse::<DocumentMut>()
        .map_err(|error| format!("invalid {}: {error}", config_path.display()))?;
    resolve_document(&document, config_path)
}

/// Resolve the built-in configuration defaults.
///
/// # Errors
///
/// Returns an error when the default archive path cannot be resolved.
pub fn defaults() -> Result<ResolvedConfig, String> {
    Ok(ResolvedConfig {
        version: CONFIG_VERSION,
        pruning: PruningConfig { enabled: true },
        output: OutputConfig {
            cap_bytes: DEFAULT_OUTPUT_CAP_BYTES,
            recovery_cap_bytes: DEFAULT_RECOVERY_CAP_BYTES,
            recovery_cap_lines: DEFAULT_RECOVERY_CAP_LINES,
        },
        archive: ArchiveConfig {
            enabled: true,
            path: default_archive_path()?,
            is_default_path: true,
        },
        rules: RulesConfig { packs: Vec::new() },
    })
}

/// Render a resolved configuration as TOML.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn show_toml(config: &ResolvedConfig) -> Result<String, String> {
    toml_edit::ser::to_string_pretty(config)
        .map_err(|error| format!("could not render resolved configuration: {error}"))
}

/// Render a resolved configuration as machine-readable JSON.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn show_json(config: &ResolvedConfig) -> Result<String, String> {
    serde_json::to_string_pretty(config)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|error| format!("could not render resolved configuration: {error}"))
}

/// Render one resolved configuration field.
///
/// # Errors
///
/// Returns an error when the field is unknown or cannot be serialized.
pub fn get(config: &ResolvedConfig, key: &str) -> Result<String, String> {
    match key {
        "version" => Ok(config.version.to_string()),
        "pruning.enabled" => Ok(config.pruning.enabled.to_string()),
        "output.cap_bytes" => Ok(config.output.cap_bytes.to_string()),
        "output.recovery_cap_bytes" => Ok(config.output.recovery_cap_bytes.to_string()),
        "output.recovery_cap_lines" => Ok(config.output.recovery_cap_lines.to_string()),
        "archive.enabled" => Ok(config.archive.enabled.to_string()),
        "archive.path" => path_text(&config.archive.path),
        "rules.packs" => serde_json::to_string(&config.rules.packs)
            .map_err(|error| format!("could not render rules.packs: {error}")),
        _ => Err(format!("unknown configuration field {key}")),
    }
}

/// Create the default configuration without replacing an existing file.
///
/// # Errors
///
/// Returns an error for path, permission, write, synchronization, or existing-file failures.
pub fn init() -> Result<(), String> {
    let config_path = path()?;
    let parent = config_parent(&config_path)?;
    prepare_directory(parent)?;
    if fs::symlink_metadata(&config_path).is_ok() {
        return Err(format!("{} already exists", config_path.display()));
    }
    let mut temporary = private_temporary(parent)?;
    temporary
        .write_all(DEFAULT_DOCUMENT.as_bytes())
        .map_err(|error| format!("could not write temporary configuration: {error}"))?;
    sync_temporary(&temporary)?;
    temporary.persist_noclobber(&config_path).map_err(|error| {
        format!(
            "could not create {}: {}",
            config_path.display(),
            error.error
        )
    })?;
    sync_directory(parent)?;
    Ok(())
}

/// Set one known field and atomically write the validated document.
///
/// # Errors
///
/// Returns an error for unknown fields, invalid values, invalid existing content, or write failures.
pub fn set(key: &str, arguments: &[String]) -> Result<ResolvedConfig, String> {
    let config_path = path()?;
    let mut document = editable_document(&config_path)?;
    set_document_value(&mut document, key, arguments)?;
    let resolved = resolve_document(&document, &config_path)?;
    write_document(&config_path, &document)?;
    Ok(resolved)
}

/// Remove one known field and atomically write the validated document.
///
/// # Errors
///
/// Returns an error when the field is required, unknown, absent, or the file cannot be validated or written.
pub fn unset(key: &str) -> Result<ResolvedConfig, String> {
    if key == "version" {
        return Err("version cannot be unset".to_owned());
    }
    let config_path = path()?;
    let mut document = editable_document(&config_path)?;
    let (section, field) = split_key(key)?;
    let remove_section = {
        let item = document
            .get_mut(section)
            .ok_or_else(|| format!("configuration field {key} is not set"))?;
        if let Some(table) = item.as_table_mut() {
            if table.remove(field).is_none() {
                return Err(format!("configuration field {key} is not set"));
            }
            table.is_empty()
        } else if let Some(table) = item.as_inline_table_mut() {
            if table.remove(field).is_none() {
                return Err(format!("configuration field {key} is not set"));
            }
            table.is_empty()
        } else {
            return Err(format!("configuration field {key} is not set"));
        }
    };
    if remove_section {
        document.remove(section);
    }
    let resolved = resolve_document(&document, &config_path)?;
    write_document(&config_path, &document)?;
    Ok(resolved)
}

fn resolve_document(document: &DocumentMut, config_path: &Path) -> Result<ResolvedConfig, String> {
    let parsed: FileConfig = toml_edit::de::from_document(document.clone())
        .map_err(|error| format!("invalid {}: {error}", config_path.display()))?;
    if parsed.version != CONFIG_VERSION {
        return Err(format!(
            "unsupported configuration version {}; expected {CONFIG_VERSION}",
            parsed.version
        ));
    }
    let cap_bytes = parsed.output.cap_bytes.unwrap_or(DEFAULT_OUTPUT_CAP_BYTES);
    if cap_bytes != 0 && !(MIN_OUTPUT_CAP_BYTES..=MAX_OUTPUT_CAP_BYTES).contains(&cap_bytes) {
        return Err(format!(
            "output.cap_bytes must be 0 or {MIN_OUTPUT_CAP_BYTES} through {MAX_OUTPUT_CAP_BYTES}"
        ));
    }
    let recovery_cap_bytes = parsed
        .output
        .recovery_cap_bytes
        .unwrap_or(DEFAULT_RECOVERY_CAP_BYTES);
    if !(MIN_RECOVERY_CAP_BYTES..=MAX_RECOVERY_CAP_BYTES).contains(&recovery_cap_bytes) {
        return Err(format!(
            "output.recovery_cap_bytes must be {MIN_RECOVERY_CAP_BYTES} through {MAX_RECOVERY_CAP_BYTES}"
        ));
    }
    let recovery_cap_lines = parsed
        .output
        .recovery_cap_lines
        .unwrap_or(DEFAULT_RECOVERY_CAP_LINES);
    if !(MIN_RECOVERY_CAP_LINES..=MAX_RECOVERY_CAP_LINES).contains(&recovery_cap_lines) {
        return Err(format!(
            "output.recovery_cap_lines must be {MIN_RECOVERY_CAP_LINES} through {MAX_RECOVERY_CAP_LINES}"
        ));
    }
    let config_directory = config_parent(config_path)?;
    let (archive_path, is_default_path) = match parsed.archive.path {
        Some(path) => (
            resolve_path(config_directory, &path, "archive.path")?,
            false,
        ),
        None => (default_archive_path()?, true),
    };
    let mut packs = Vec::new();
    let mut pack_ids = BTreeSet::new();
    for path in parsed.rules.packs.unwrap_or_default() {
        let resolved = resolve_path(config_directory, &path, "rules.packs")?;
        let pack = CompiledPack::open(&resolved, None, None)?;
        if !pack_ids.insert(pack.id.clone()) {
            return Err(format!("configured rule pack id {} is duplicated", pack.id));
        }
        packs.push(pack.path);
    }
    Ok(ResolvedConfig {
        version: CONFIG_VERSION,
        pruning: PruningConfig {
            enabled: parsed.pruning.enabled.unwrap_or(true),
        },
        output: OutputConfig {
            cap_bytes,
            recovery_cap_bytes,
            recovery_cap_lines,
        },
        archive: ArchiveConfig {
            enabled: parsed.archive.enabled.unwrap_or(true),
            path: archive_path,
            is_default_path,
        },
        rules: RulesConfig { packs },
    })
}

fn default_archive_path() -> Result<PathBuf, String> {
    if let Some(home) = std::env::var_os("XDG_DATA_HOME") {
        return absolute_directory(&home, "XDG_DATA_HOME")
            .map(|directory| directory.join("yarp/tool-calls.sqlite3"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "could not resolve YARP archive: HOME is not set".to_owned())?;
    absolute_directory(&home, "HOME")
        .map(|directory| directory.join(".local/share/yarp/tool-calls.sqlite3"))
}

fn absolute_directory(value: &std::ffi::OsStr, name: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!("{name} must be an absolute path"));
    }
    Ok(path)
}

fn resolve_path(base: &Path, value: &str, field: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    let path = PathBuf::from(value);
    let joined = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    std::path::absolute(&joined)
        .map_err(|error| format!("could not resolve {field} {}: {error}", joined.display()))
}

fn path_text(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn editable_document(config_path: &Path) -> Result<DocumentMut, String> {
    match fs::read_to_string(config_path) {
        Ok(body) => body
            .parse::<DocumentMut>()
            .map_err(|error| format!("invalid {}: {error}", config_path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "version = 1\n"
            .parse::<DocumentMut>()
            .map_err(|error| format!("could not create default configuration: {error}")),
        Err(error) => Err(format!("could not read {}: {error}", config_path.display())),
    }
}

fn set_document_value(
    document: &mut DocumentMut,
    key: &str,
    arguments: &[String],
) -> Result<(), String> {
    if key == "version" {
        require_count(key, arguments, 1)?;
        let version = parse_integer(key, &arguments[0])?;
        document["version"] = value(version);
        return Ok(());
    }
    let (section, field) = split_key(key)?;
    ensure_table(document, section)?;
    let item = match key {
        "pruning.enabled" | "archive.enabled" => {
            require_count(key, arguments, 1)?;
            value(parse_boolean(key, &arguments[0])?)
        }
        "output.cap_bytes" | "output.recovery_cap_bytes" | "output.recovery_cap_lines" => {
            require_count(key, arguments, 1)?;
            value(parse_integer(key, &arguments[0])?)
        }
        "archive.path" => {
            require_count(key, arguments, 1)?;
            value(arguments[0].as_str())
        }
        "rules.packs" => {
            let mut array = Array::new();
            for argument in arguments {
                array.push(argument.as_str());
            }
            value(array)
        }
        _ => return Err(format!("unknown configuration field {key}")),
    };
    if let Some(table) = document[section].as_table_mut() {
        table.insert(field, item);
        return Ok(());
    }
    let item = item
        .into_value()
        .map_err(|_| format!("configuration value {key} is not a TOML value"))?;
    document[section]
        .as_inline_table_mut()
        .ok_or_else(|| format!("configuration section {section} is not a table"))?
        .insert(field, item);
    Ok(())
}

fn split_key(key: &str) -> Result<(&str, &str), String> {
    match key {
        "pruning.enabled" => Ok(("pruning", "enabled")),
        "output.cap_bytes" => Ok(("output", "cap_bytes")),
        "output.recovery_cap_bytes" => Ok(("output", "recovery_cap_bytes")),
        "output.recovery_cap_lines" => Ok(("output", "recovery_cap_lines")),
        "archive.enabled" => Ok(("archive", "enabled")),
        "archive.path" => Ok(("archive", "path")),
        "rules.packs" => Ok(("rules", "packs")),
        _ => Err(format!("unknown configuration field {key}")),
    }
}

fn ensure_table(document: &mut DocumentMut, section: &str) -> Result<(), String> {
    if !document.contains_key(section) {
        document[section] = Item::Table(Table::new());
    }
    if !document[section].is_table() && document[section].as_inline_table().is_none() {
        return Err(format!("configuration section {section} is not a table"));
    }
    Ok(())
}

fn require_count(key: &str, arguments: &[String], expected: usize) -> Result<(), String> {
    if arguments.len() != expected {
        return Err(format!("{key} requires {expected} value(s)"));
    }
    Ok(())
}

fn parse_boolean(key: &str, value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{key} must be true or false")),
    }
}

fn parse_integer(key: &str, value: &str) -> Result<i64, String> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("{key} must be a canonical unsigned integer"));
    }
    value
        .parse::<i64>()
        .map_err(|_| format!("{key} integer is too large"))
}

fn config_parent(config_path: &Path) -> Result<&Path, String> {
    config_path.parent().ok_or_else(|| {
        format!(
            "configuration path has no parent: {}",
            config_path.display()
        )
    })
}

fn prepare_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not protect {}: {error}", path.display()))?;
    }
    Ok(())
}

fn private_temporary(directory: &Path) -> Result<NamedTempFile, String> {
    let file = NamedTempFile::new_in(directory)
        .map_err(|error| format!("could not create temporary configuration: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("could not protect temporary configuration: {error}"))?;
    }
    Ok(file)
}

fn sync_temporary(file: &NamedTempFile) -> Result<(), String> {
    file.as_file()
        .sync_all()
        .map_err(|error| format!("could not flush temporary configuration: {error}"))
}

fn write_document(config_path: &Path, document: &DocumentMut) -> Result<(), String> {
    let parent = config_parent(config_path)?;
    prepare_directory(parent)?;
    if fs::symlink_metadata(config_path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!(
            "refusing to replace symlink {}",
            config_path.display()
        ));
    }
    let mut temporary = private_temporary(parent)?;
    temporary
        .write_all(document.to_string().as_bytes())
        .map_err(|error| format!("could not write temporary configuration: {error}"))?;
    sync_temporary(&temporary)?;
    temporary.persist(config_path).map_err(|error| {
        format!(
            "could not replace {}: {}",
            config_path.display(),
            error.error
        )
    })?;
    sync_directory(parent)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("could not flush {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_uses_defaults() {
        let directory = TempDir::new().expect("temp directory");
        let config = load_from(&directory.path().join("config.toml")).expect("defaults");
        assert!(config.pruning.enabled);
        assert!(config.archive.enabled);
        assert_eq!(config.output.cap_bytes, DEFAULT_OUTPUT_CAP_BYTES);
        assert_eq!(config.output.recovery_cap_bytes, DEFAULT_RECOVERY_CAP_BYTES);
        assert_eq!(config.output.recovery_cap_lines, DEFAULT_RECOVERY_CAP_LINES);
    }

    #[test]
    fn resolves_relative_paths_and_rejects_unknown_fields() {
        let directory = TempDir::new().expect("temp directory");
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "version = 1\n[archive]\npath = 'data/archive.sqlite3'\n[output]\nunknown = 1\n",
        )
        .expect("write config");
        assert!(load_from(&path).is_err());
        fs::write(
            &path,
            "version = 1\n[archive]\npath = 'data/archive.sqlite3'\n",
        )
        .expect("write config");
        let config = load_from(&path).expect("valid config");
        assert_eq!(
            config.archive.path,
            directory.path().join("data/archive.sqlite3")
        );
    }

    #[test]
    fn rejects_invalid_versions_and_ranges() {
        let directory = TempDir::new().expect("temp directory");
        let path = directory.path().join("config.toml");
        for body in [
            "version = 2\n",
            "version = 1\n[output]\ncap_bytes = 1\n",
            "version = 1\n[output]\nrecovery_cap_bytes = 50000\n",
            "version = 1\n[output]\nrecovery_cap_lines = 1901\n",
        ] {
            fs::write(&path, body).expect("write config");
            assert!(load_from(&path).is_err(), "accepted {body:?}");
        }
    }
}
