use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;
use std::path::{Path, PathBuf};

use yarp_rule_pack::{Action, CompiledPack, Rule};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/builtin_rules.rs"));
}

pub use generated::{BUILTIN_PACK_ID, BUILTIN_SOURCE_DIGEST};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackRequest {
    pub path: PathBuf,
    pub expected_digest: Option<[u8; 32]>,
    pub expected_compiled_digest: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackReference {
    pub path: PathBuf,
    pub source_digest: [u8; 32],
    pub compiled_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedRule {
    pub pack_id: Cow<'static, str>,
    pub source_digest: [u8; 32],
    pub rule: SelectedRuleData,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectedRuleData {
    Builtin(&'static Rule),
    External(Box<Rule>),
}

impl Deref for SelectedRuleData {
    type Target = Rule;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Builtin(rule) => rule,
            Self::External(rule) => rule,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Selection {
    Reduce(SelectedRule),
    Passthrough(Vec<String>),
    Ambiguous(Vec<String>),
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleSummary {
    pub pack_id: String,
    pub rule: Rule,
}

struct ExternalPack {
    pack: CompiledPack,
    disabled: bool,
    recheck_digest_after_selection: bool,
}

pub struct Registry {
    external: Vec<ExternalPack>,
    references: Vec<PackReference>,
    diagnostics: Vec<String>,
}

impl Registry {
    /// Load explicit compiled packs and disable deterministic ID conflicts.
    ///
    /// # Errors
    ///
    /// Returns an error when a requested pack cannot be opened or validated.
    pub fn load(requests: &[PackRequest]) -> Result<Self, String> {
        let mut external = Vec::<ExternalPack>::new();
        let mut seen_paths = BTreeSet::new();
        for request in requests {
            let pack = CompiledPack::open(
                &request.path,
                request.expected_digest,
                request.expected_compiled_digest,
            )?;
            if !seen_paths.insert(pack.path.clone()) {
                if request.expected_compiled_digest.is_some()
                    && let Some(existing) = external
                        .iter_mut()
                        .find(|existing| existing.pack.path == pack.path)
                {
                    existing.recheck_digest_after_selection = true;
                }
                continue;
            }
            external.push(ExternalPack {
                pack,
                disabled: false,
                recheck_digest_after_selection: request.expected_compiled_digest.is_some(),
            });
        }
        let references = external
            .iter()
            .map(|external| PackReference {
                path: external.pack.path.clone(),
                source_digest: external.pack.source_digest,
                compiled_digest: external.pack.compiled_digest,
            })
            .collect();
        let diagnostics = disable_conflicts(&mut external);
        Ok(Self {
            external,
            references,
            diagnostics,
        })
    }

    #[must_use]
    pub fn builtins_only() -> Self {
        Self {
            external: Vec::new(),
            references: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    /// Select one action for an already parsed child argument vector.
    ///
    /// # Errors
    ///
    /// Returns an error when an indexed external candidate record is corrupt or invalid.
    pub fn select(&mut self, arguments: &[String]) -> Result<Selection, String> {
        let Some(program) = arguments.first() else {
            return Ok(Selection::Unsupported);
        };
        let mut matches = MatchAccumulator::default();
        if let Ok(index) =
            generated::BUILTIN_PROGRAM_INDEX.binary_search_by(|entry| entry.0.cmp(program.as_str()))
        {
            for rule_index in generated::BUILTIN_PROGRAM_INDEX[index].1 {
                let rule = &generated::BUILTIN_RULES[*rule_index];
                if rule.matcher.matches(arguments) {
                    matches.push(SelectedRule {
                        pack_id: Cow::Borrowed(BUILTIN_PACK_ID),
                        source_digest: BUILTIN_SOURCE_DIGEST,
                        rule: SelectedRuleData::Builtin(rule),
                    });
                }
            }
        }
        for external in &mut self.external {
            if external.disabled {
                continue;
            }
            let candidates = external.pack.candidate_indices(program).to_vec();
            for candidate in candidates {
                let rule = external.pack.read_rule(candidate)?;
                if rule.matcher.matches(arguments) {
                    matches.push(SelectedRule {
                        pack_id: Cow::Owned(external.pack.id.clone()),
                        source_digest: external.pack.source_digest,
                        rule: SelectedRuleData::External(Box::new(rule)),
                    });
                }
            }
            if external.recheck_digest_after_selection {
                external.pack.verify_compiled_digest()?;
            }
        }
        Ok(resolve(matches))
    }

    #[must_use]
    pub fn references(&self) -> &[PackReference] {
        &self.references
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Read summaries for every enabled built-in and external rule.
    ///
    /// # Errors
    ///
    /// Returns an error when an enabled external record is corrupt or invalid.
    pub fn summaries(&mut self) -> Result<Vec<RuleSummary>, String> {
        let mut summaries = generated::BUILTIN_RULES
            .iter()
            .cloned()
            .map(|rule| RuleSummary {
                pack_id: BUILTIN_PACK_ID.to_owned(),
                rule,
            })
            .collect::<Vec<_>>();
        for external in &mut self.external {
            if external.disabled {
                continue;
            }
            for index in 0..external.pack.rules.len() {
                let rule = external.pack.read_rule(
                    u32::try_from(index)
                        .map_err(|_| "external rule index does not fit u32".to_owned())?,
                )?;
                summaries.push(RuleSummary {
                    pack_id: external.pack.id.clone(),
                    rule,
                });
            }
        }
        summaries.sort_by(|left, right| {
            (&left.pack_id, &left.rule.id).cmp(&(&right.pack_id, &right.rule.id))
        });
        Ok(summaries)
    }
}

#[must_use]
pub fn requests_from_paths(paths: &[PathBuf]) -> Vec<PackRequest> {
    paths
        .iter()
        .map(|path| PackRequest {
            path: path.clone(),
            expected_digest: None,
            expected_compiled_digest: None,
        })
        .collect()
}

/// Read explicit pack paths from `YARP_RULE_PACKS`.
///
/// # Errors
///
/// Returns an error when the operating-system path list contains an empty entry.
pub fn requests_from_environment() -> Result<Vec<PackRequest>, String> {
    let Some(value) = std::env::var_os("YARP_RULE_PACKS") else {
        return Ok(Vec::new());
    };
    let paths = std::env::split_paths(&value).collect::<Vec<_>>();
    if paths.is_empty() || paths.iter().any(|path| path.as_os_str().is_empty()) {
        return Err("YARP_RULE_PACKS contains an empty path".to_owned());
    }
    Ok(requests_from_paths(&paths))
}

#[must_use]
pub fn digest_hex(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

/// Parse one exact SHA-256 digest.
///
/// # Errors
///
/// Returns an error when the value is not 64 hexadecimal digits.
pub fn parse_digest(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err("rule pack digest must contain 32 bytes".to_owned());
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        output[index] = (high << 4) | low;
    }
    Ok(output)
}

fn hex_digit(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("rule pack digest must be hexadecimal".to_owned()),
    }
}

/// Resolve a regular compiled pack without crossing a symlink outside a trusted project.
///
/// # Errors
///
/// Returns an error when either path cannot be resolved or any component is unsafe.
pub fn canonical_project_pack(project_root: &Path, candidate: &Path) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(project_root)
        .map_err(|error| format!("could not resolve project root: {error}"))?;
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_root.join(candidate)
    };
    let relative = candidate
        .strip_prefix(project_root)
        .or_else(|_| candidate.strip_prefix(&root))
        .map_err(|_| "project rule pack is not beneath the trusted project root".to_owned())?;
    let mut current = root.clone();
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| format!("could not inspect project rule pack: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("project rule pack path must not contain symlinks".to_owned());
        }
    }
    let metadata = std::fs::metadata(&candidate)
        .map_err(|error| format!("could not inspect project rule pack: {error}"))?;
    if !metadata.is_file() {
        return Err("project rule pack must be a regular file".to_owned());
    }
    let resolved = std::fs::canonicalize(&candidate)
        .map_err(|error| format!("could not resolve project rule pack: {error}"))?;
    if !resolved.starts_with(root) {
        return Err("project rule pack escapes the trusted project root".to_owned());
    }
    Ok(resolved)
}

#[derive(Default)]
struct MatchAccumulator {
    passthrough: Vec<String>,
    reductions: ReductionMatches,
}

#[derive(Default)]
enum ReductionMatches {
    #[default]
    None,
    One(SelectedRule),
    Many(Vec<String>),
}

impl MatchAccumulator {
    fn push(&mut self, selected: SelectedRule) {
        match selected.rule.action {
            Action::Passthrough => self.passthrough.push(qualified_id(&selected)),
            Action::Reduce => {
                self.reductions = match std::mem::take(&mut self.reductions) {
                    ReductionMatches::None => ReductionMatches::One(selected),
                    ReductionMatches::One(first) => {
                        ReductionMatches::Many(vec![qualified_id(&first), qualified_id(&selected)])
                    }
                    ReductionMatches::Many(mut ids) => {
                        ids.push(qualified_id(&selected));
                        ReductionMatches::Many(ids)
                    }
                };
            }
        }
    }
}

fn resolve(mut matches: MatchAccumulator) -> Selection {
    if !matches.passthrough.is_empty() {
        matches.passthrough.sort();
        return Selection::Passthrough(matches.passthrough);
    }
    match matches.reductions {
        ReductionMatches::None => Selection::Unsupported,
        ReductionMatches::One(selected) => Selection::Reduce(selected),
        ReductionMatches::Many(mut ids) => {
            ids.sort();
            Selection::Ambiguous(ids)
        }
    }
}

fn qualified_id(rule: &SelectedRule) -> String {
    format!("{}/{}", rule.pack_id, rule.rule.id)
}

fn disable_conflicts(external: &mut [ExternalPack]) -> Vec<String> {
    let builtin_ids: BTreeSet<&str> = generated::BUILTIN_RULES
        .iter()
        .map(|rule| rule.id.as_str())
        .collect();
    let mut pack_owners = BTreeMap::<String, Vec<usize>>::new();
    let mut rule_owners = BTreeMap::<String, Vec<usize>>::new();
    for (index, candidate) in external.iter().enumerate() {
        pack_owners
            .entry(candidate.pack.id.clone())
            .or_default()
            .push(index);
        for rule in &candidate.pack.rules {
            rule_owners.entry(rule.id.clone()).or_default().push(index);
        }
    }
    let mut disabled = BTreeSet::new();
    let mut diagnostics = BTreeSet::new();
    if let Some(owners) = pack_owners.get(BUILTIN_PACK_ID) {
        disabled.extend(owners.iter().copied());
        diagnostics.insert(format!(
            "external pack id conflicts with the built-in pack: {BUILTIN_PACK_ID}"
        ));
    }
    for (pack_id, owners) in pack_owners.iter().filter(|(_, owners)| owners.len() > 1) {
        disabled.extend(owners.iter().copied());
        diagnostics.insert(format!(
            "duplicate external pack id disables all copies: {pack_id}"
        ));
    }
    for (rule_id, owners) in rule_owners {
        if builtin_ids.contains(rule_id.as_str()) {
            disabled.extend(owners.iter().copied());
            diagnostics.insert(format!(
                "external rule conflicts with a built-in rule: {rule_id}"
            ));
        } else if owners.len() > 1 {
            disabled.extend(owners.iter().copied());
            diagnostics.insert(format!(
                "duplicate external rule id disables its packs: {rule_id}"
            ));
        }
    }
    for index in disabled {
        external[index].disabled = true;
    }
    diagnostics.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::io::{Seek as _, SeekFrom, Write as _};

    use serde::Deserialize;
    use sha2::{Digest as _, Sha256};
    use tempfile::TempDir;
    use yarp_rule_pack::SourcePack;

    use super::*;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FixtureFile {
        schema_version: u32,
        cases: Vec<MatchFixture>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct MatchFixture {
        id: String,
        rule_id: String,
        argv: Vec<String>,
        expected_action: String,
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn builtins_use_indexed_matching_and_guards() {
        let mut registry = Registry::builtins_only();
        let selection = registry
            .select(&strings(&["cargo", "test", "--workspace"]))
            .expect("selection");
        assert!(matches!(selection, Selection::Reduce(_)));
        let selection = registry
            .select(&strings(&["cargo", "test", "--message-format=json"]))
            .expect("selection");
        assert!(matches!(selection, Selection::Passthrough(_)));
        assert_eq!(
            registry
                .select(&strings(&["git", "push"]))
                .expect("selection"),
            Selection::Unsupported
        );
    }

    #[test]
    fn every_builtin_rule_matches_its_synthetic_fixture() {
        let fixtures: FixtureFile =
            serde_json::from_str(include_str!("../../rules/fixtures/builtin.json"))
                .expect("fixtures");
        assert_eq!(fixtures.schema_version, 1);
        let mut covered = BTreeSet::new();
        for fixture in fixtures.cases {
            let mut registry = Registry::builtins_only();
            let selection = registry.select(&fixture.argv).expect("selection");
            match (fixture.expected_action.as_str(), selection) {
                ("reduce", Selection::Reduce(selected)) => {
                    assert_eq!(selected.rule.id, fixture.rule_id, "{}", fixture.id);
                }
                ("passthrough", Selection::Passthrough(ids)) => {
                    assert!(
                        ids.iter().any(|id| id.ends_with(&fixture.rule_id)),
                        "{}: {ids:?}",
                        fixture.id
                    );
                }
                (expected, actual) => panic!("{}: expected {expected}, got {actual:?}", fixture.id),
            }
            covered.insert(fixture.rule_id);
        }
        assert_eq!(covered.len(), generated::BUILTIN_RULES.len());
    }

    #[cfg(unix)]
    #[test]
    fn expected_pack_digest_is_rechecked_after_selection() {
        let directory = TempDir::new().expect("temp directory");
        std::fs::create_dir(directory.path().join("rules")).expect("rules directory");
        std::fs::write(
            directory.path().join("pack.json"),
            r#"{"schema_version":1,"id":"external-pack","rules":["rules/test.json"]}"#,
        )
        .expect("manifest");
        std::fs::write(
            directory.path().join("rules/test.json"),
            r#"{"id":"tests/external","match":{"program":["external-tool"],"argv_prefix":["run"]},"action":"reduce","reducer":{"kind":"head_tail"},"success":{"head_lines":10,"tail_lines":10,"max_line_bytes":16384,"max_output_bytes":32768,"min_savings_bytes":120},"failure":{"head_lines":20,"tail_lines":20,"max_line_bytes":16384,"max_output_bytes":65536,"min_savings_bytes":120}}"#,
        )
        .expect("rule");
        let source = SourcePack::load(directory.path()).expect("source pack");
        let compiled = yarp_rule_pack::compile(&source).expect("compiled pack");
        let compiled_digest = Sha256::digest(&compiled).into();
        let pack_path = directory.path().join("pack.yrp");
        std::fs::write(&pack_path, &compiled).expect("write pack");
        let mut registry = Registry::load(&[PackRequest {
            path: pack_path.clone(),
            expected_digest: Some(source.source_digest),
            expected_compiled_digest: Some(compiled_digest),
        }])
        .expect("registry");

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(pack_path)
            .expect("open pack for mutation");
        file.seek(SeekFrom::Start(0)).expect("seek");
        file.write_all(&[compiled[0] ^ 1]).expect("mutate header");
        file.flush().expect("flush mutation");

        assert!(
            registry
                .select(&strings(&["external-tool", "run"]))
                .expect_err("changed pack")
                .contains("changed while loading")
        );
    }

    #[test]
    fn project_pack_rejects_symlinked_path_components() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::TempDir::new().expect("temp directory");
        let real = directory.path().join("real");
        std::fs::create_dir(&real).expect("real directory");
        std::fs::write(real.join("rules.yrp"), b"pack").expect("pack");
        symlink(&real, directory.path().join(".yarp")).expect("symlink");
        let error =
            canonical_project_pack(directory.path(), &directory.path().join(".yarp/rules.yrp"))
                .expect_err("symlink rejection");
        assert!(error.contains("symlink"));
    }

    #[test]
    fn digest_encoding_is_strict() {
        let digest = [7_u8; 32];
        assert_eq!(parse_digest(&digest_hex(&digest)).expect("digest"), digest);
        assert!(parse_digest("07").is_err());
        assert!(parse_digest("not-hex").is_err());
    }
}
