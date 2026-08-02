use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::model::{ENGINE_ABI_VERSION, Rule};
use crate::source::SourcePack;
use crate::strict_json;
use crate::validation::{
    MAX_COMPILED_BYTES, MAX_RULES, MAX_SOURCE_FILE_BYTES, validate_manifest, validate_rule,
    validate_rules,
};

const MAGIC: &[u8; 8] = b"YARPRUL\0";
const FORMAT_VERSION: u16 = 1;
const HEADER_LEN: usize = 128;
const DIGEST_OFFSET: usize = 80;
const DIGEST_LEN: usize = 32;
const MAX_ID_BYTES: usize = 128;
const MAX_PROGRAMS: usize = MAX_RULES * 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleRecord {
    pub id: String,
    offset: u64,
    length: u32,
    digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramEntry {
    pub program: String,
    pub candidates: Vec<u32>,
}

#[derive(Debug)]
pub struct CompiledPack {
    pub path: PathBuf,
    pub id: String,
    pub source_digest: [u8; 32],
    pub compiled_digest: [u8; 32],
    pub rules: Vec<RuleRecord>,
    pub programs: Vec<ProgramEntry>,
    records_offset: u64,
    records_len: u64,
    file: File,
}

/// Compile a validated source pack into deterministic indexed bytes.
///
/// # Errors
///
/// Returns an error when the source model is invalid or a compiled size or index conversion fails.
pub fn compile(pack: &SourcePack) -> Result<Vec<u8>, String> {
    validate_manifest(&pack.manifest)?;
    validate_rules(&pack.rules)?;
    let mut sorted_rules: Vec<&Rule> = pack.rules.iter().collect();
    sorted_rules.sort_by(|left, right| left.id.cmp(&right.id));

    let mut records = Vec::new();
    let mut rule_records = Vec::with_capacity(sorted_rules.len());
    for rule in &sorted_rules {
        let body = serde_jcs::to_vec(rule)
            .map_err(|error| format!("could not encode rule {}: {error}", rule.id))?;
        if body.len() > MAX_SOURCE_FILE_BYTES {
            return Err(format!(
                "compiled rule {} exceeds {MAX_SOURCE_FILE_BYTES} bytes",
                rule.id
            ));
        }
        let offset = u64::try_from(records.len())
            .map_err(|_| "compiled record offset does not fit u64".to_owned())?;
        let length = u32::try_from(body.len())
            .map_err(|_| format!("compiled rule {} is too large", rule.id))?;
        let digest = Sha256::digest(&body).into();
        records.extend_from_slice(&body);
        rule_records.push(RuleRecord {
            id: rule.id.clone(),
            offset,
            length,
            digest,
        });
    }

    let rule_indices: BTreeMap<&str, u32> = sorted_rules
        .iter()
        .enumerate()
        .map(|(index, rule)| {
            Ok((
                rule.id.as_str(),
                u32::try_from(index).map_err(|_| "rule index does not fit u32".to_owned())?,
            ))
        })
        .collect::<Result<_, String>>()?;
    let mut program_map = BTreeMap::<String, Vec<u32>>::new();
    for rule in sorted_rules {
        let index = rule_indices[rule.id.as_str()];
        for program in &rule.matcher.program {
            program_map.entry(program.clone()).or_default().push(index);
        }
    }
    let programs: Vec<ProgramEntry> = program_map
        .into_iter()
        .map(|(program, mut candidates)| {
            candidates.sort_unstable();
            candidates.dedup();
            ProgramEntry {
                program,
                candidates,
            }
        })
        .collect();

    let index = encode_index(&rule_records, &programs)?;
    let pack_id = pack.manifest.id.as_bytes();
    let header = encode_header(
        pack_id,
        rule_records.len(),
        programs.len(),
        &index,
        &records,
        pack.source_digest,
    )?;

    let capacity = HEADER_LEN
        .checked_add(pack_id.len())
        .and_then(|value| value.checked_add(index.len()))
        .and_then(|value| value.checked_add(records.len()))
        .ok_or_else(|| "compiled pack size overflowed".to_owned())?;
    if capacity as u64 > MAX_COMPILED_BYTES {
        return Err(format!("compiled pack exceeds {MAX_COMPILED_BYTES} bytes"));
    }
    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(&header);
    output.extend_from_slice(pack_id);
    output.extend_from_slice(&index);
    output.extend_from_slice(&records);
    Ok(output)
}

fn encode_header(
    pack_id: &[u8],
    rule_count: usize,
    program_count: usize,
    index: &[u8],
    records: &[u8],
    source_digest: [u8; 32],
) -> Result<[u8; HEADER_LEN], String> {
    let mut header = [0_u8; HEADER_LEN];
    header[0..8].copy_from_slice(MAGIC);
    put_u16(&mut header, 8, FORMAT_VERSION);
    put_u16(&mut header, 10, ENGINE_ABI_VERSION);
    put_u16(&mut header, 12, ENGINE_ABI_VERSION);
    put_u32(
        &mut header,
        16,
        u32::try_from(HEADER_LEN).map_err(|_| "header length does not fit u32".to_owned())?,
    );
    put_u32(
        &mut header,
        20,
        u32::try_from(pack_id.len()).map_err(|_| "pack id is too long".to_owned())?,
    );
    put_u32(
        &mut header,
        24,
        u32::try_from(rule_count).map_err(|_| "too many rules".to_owned())?,
    );
    put_u32(
        &mut header,
        28,
        u32::try_from(program_count).map_err(|_| "too many programs".to_owned())?,
    );
    put_u64(
        &mut header,
        32,
        u64::try_from(index.len())
            .map_err(|_| "compiled index length does not fit u64".to_owned())?,
    );
    put_u64(
        &mut header,
        40,
        u64::try_from(records.len())
            .map_err(|_| "compiled records length does not fit u64".to_owned())?,
    );
    header[48..80].copy_from_slice(&source_digest);
    let digest = header_index_digest(&header, pack_id, index);
    header[DIGEST_OFFSET..DIGEST_OFFSET + DIGEST_LEN].copy_from_slice(&digest);
    Ok(header)
}

impl CompiledPack {
    /// Open and verify a compiled pack header and program index.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe path, incompatible format, corrupt index, or digest mismatch.
    pub fn open(
        path: &Path,
        expected_source_digest: Option<[u8; 32]>,
        expected_compiled_digest: Option<[u8; 32]>,
    ) -> Result<Self, String> {
        let metadata =
            fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("{}: symlinks are not allowed", path.display()));
        }
        if !metadata.is_file() {
            return Err(format!("{}: expected a regular file", path.display()));
        }
        if metadata.len() > MAX_COMPILED_BYTES {
            return Err(format!(
                "{}: compiled pack exceeds {MAX_COMPILED_BYTES} bytes",
                path.display()
            ));
        }
        let path = fs::canonicalize(path)
            .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
        let mut file = File::open(&path)
            .map_err(|error| format!("could not open {}: {error}", path.display()))?;
        let opened_metadata = file
            .metadata()
            .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
        if !opened_metadata.is_file() || opened_metadata.len() > MAX_COMPILED_BYTES {
            return Err(format!("{}: invalid compiled pack file", path.display()));
        }
        let compiled_digest = file_digest(&mut file, &path)?;
        if let Some(expected) = expected_compiled_digest
            && expected != compiled_digest
        {
            return Err(format!("{}: compiled pack digest changed", path.display()));
        }
        let mut header = [0_u8; HEADER_LEN];
        file.read_exact(&mut header)
            .map_err(|error| format!("{}: truncated header: {error}", path.display()))?;
        let parsed = parse_header(&header, opened_metadata.len())?;
        if let Some(expected) = expected_source_digest
            && expected != parsed.source_digest
        {
            return Err(format!("{}: source digest changed", path.display()));
        }
        let mut pack_id = vec![0_u8; parsed.pack_id_len];
        file.read_exact(&mut pack_id)
            .map_err(|error| format!("{}: truncated pack id: {error}", path.display()))?;
        let id = String::from_utf8(pack_id.clone())
            .map_err(|_| format!("{}: pack id is not UTF-8", path.display()))?;
        validate_compiled_pack_id(&id)?;
        let mut index = vec![0_u8; parsed.index_len];
        file.read_exact(&mut index)
            .map_err(|error| format!("{}: truncated index: {error}", path.display()))?;
        let digest = header_index_digest(&header, &pack_id, &index);
        if digest != parsed.header_index_digest {
            return Err(format!(
                "{}: header or index digest mismatch",
                path.display()
            ));
        }
        let (rules, programs) = decode_index(&index, parsed.rule_count, parsed.program_count)?;
        validate_index(&rules, &programs, parsed.records_len)?;
        let records_offset = u64::try_from(HEADER_LEN)
            .ok()
            .and_then(|value| value.checked_add(parsed.pack_id_len as u64))
            .and_then(|value| value.checked_add(parsed.index_len as u64))
            .ok_or_else(|| "compiled record offset overflowed".to_owned())?;
        let mut pack = Self {
            path,
            id,
            source_digest: parsed.source_digest,
            compiled_digest,
            rules,
            programs,
            records_offset,
            records_len: parsed.records_len,
            file,
        };
        pack.verify_compiled_digest()?;
        Ok(pack)
    }

    #[must_use]
    pub fn candidate_indices(&self, program: &str) -> &[u32] {
        self.programs
            .binary_search_by(|entry| entry.program.as_str().cmp(program))
            .ok()
            .map_or(&[], |index| self.programs[index].candidates.as_slice())
    }

    /// Read and validate one indexed rule record.
    ///
    /// # Errors
    ///
    /// Returns an error when the index is invalid or the selected record is corrupt or invalid.
    pub fn read_rule(&mut self, index: u32) -> Result<Rule, String> {
        let record = self
            .rules
            .get(index as usize)
            .ok_or_else(|| format!("{}: invalid rule index {index}", self.path.display()))?;
        let absolute = self
            .records_offset
            .checked_add(record.offset)
            .ok_or_else(|| "compiled record offset overflowed".to_owned())?;
        self.file
            .seek(SeekFrom::Start(absolute))
            .map_err(|error| format!("{}: could not seek to rule: {error}", self.path.display()))?;
        let mut body = vec![0_u8; record.length as usize];
        self.file
            .read_exact(&mut body)
            .map_err(|error| format!("{}: truncated rule record: {error}", self.path.display()))?;
        let digest: [u8; 32] = Sha256::digest(&body).into();
        if digest != record.digest {
            return Err(format!(
                "{}: rule record digest mismatch for {}",
                self.path.display(),
                record.id
            ));
        }
        let rule: Rule = strict_json::from_slice(&body)
            .map_err(|error| format!("{}: {}: {error}", self.path.display(), record.id))?;
        validate_rule(&rule).map_err(|error| format!("{}: {error}", self.path.display()))?;
        if rule.id != record.id {
            return Err(format!(
                "{}: rule record id does not match index",
                self.path.display()
            ));
        }
        Ok(rule)
    }

    /// Rehash the open file and require the bytes to match those observed before parsing.
    ///
    /// # Errors
    ///
    /// Returns an error when the file changed after it was opened.
    pub fn verify_compiled_digest(&mut self) -> Result<(), String> {
        let digest = file_digest(&mut self.file, &self.path)?;
        if digest != self.compiled_digest {
            return Err(format!(
                "{}: compiled pack changed while loading",
                self.path.display()
            ));
        }
        Ok(())
    }

    /// Verify every record and every program-to-rule reference in the pack.
    ///
    /// # Errors
    ///
    /// Returns an error when any record or index relationship is invalid.
    pub fn verify_all(&mut self) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for index in 0..self.rules.len() {
            let index =
                u32::try_from(index).map_err(|_| "rule index does not fit u32".to_owned())?;
            let rule = self.read_rule(index)?;
            for program in &rule.matcher.program {
                let candidates = self.candidate_indices(program);
                if !candidates.contains(&index) {
                    return Err(format!(
                        "{}: rule {} is missing from program index {program}",
                        self.path.display(),
                        rule.id
                    ));
                }
            }
            seen.insert(index);
        }
        for entry in self.programs.clone() {
            for candidate in entry.candidates {
                if !seen.contains(&candidate) {
                    return Err(format!(
                        "{}: program index references unknown rule",
                        self.path.display()
                    ));
                }
                let rule = self.read_rule(candidate)?;
                if !rule.matcher.program.contains(&entry.program) {
                    return Err(format!(
                        "{}: program index disagrees with rule {}",
                        self.path.display(),
                        rule.id
                    ));
                }
            }
        }
        self.verify_compiled_digest()
    }

    #[must_use]
    pub const fn records_len(&self) -> u64 {
        self.records_len
    }
}

#[derive(Clone, Copy)]
struct ParsedHeader {
    pack_id_len: usize,
    rule_count: usize,
    program_count: usize,
    index_len: usize,
    records_len: u64,
    source_digest: [u8; 32],
    header_index_digest: [u8; 32],
}

fn parse_header(header: &[u8; HEADER_LEN], file_len: u64) -> Result<ParsedHeader, String> {
    if &header[0..8] != MAGIC {
        return Err("invalid compiled pack magic".to_owned());
    }
    if get_u16(header, 8) != FORMAT_VERSION {
        return Err("unsupported compiled pack format".to_owned());
    }
    let abi_min = get_u16(header, 10);
    let abi_max = get_u16(header, 12);
    if ENGINE_ABI_VERSION < abi_min || ENGINE_ABI_VERSION > abi_max {
        return Err(format!(
            "engine ABI {ENGINE_ABI_VERSION} is outside supported range {abi_min}..={abi_max}"
        ));
    }
    if get_u16(header, 14) != 0 || get_u32(header, 16) as usize != HEADER_LEN {
        return Err("invalid compiled pack header".to_owned());
    }
    if header[112..].iter().any(|byte| *byte != 0) {
        return Err("compiled pack reserved header bytes are not zero".to_owned());
    }
    let pack_id_len = get_u32(header, 20) as usize;
    let rule_count = get_u32(header, 24) as usize;
    let program_count = get_u32(header, 28) as usize;
    let index_len = usize::try_from(get_u64(header, 32))
        .map_err(|_| "compiled index length does not fit usize".to_owned())?;
    let records_len = get_u64(header, 40);
    if pack_id_len == 0 || pack_id_len > MAX_ID_BYTES {
        return Err("invalid compiled pack id length".to_owned());
    }
    if rule_count == 0
        || rule_count > MAX_RULES
        || program_count == 0
        || program_count > MAX_PROGRAMS
    {
        return Err("invalid compiled pack counts".to_owned());
    }
    let expected_len = (HEADER_LEN as u64)
        .checked_add(pack_id_len as u64)
        .and_then(|value| value.checked_add(index_len as u64))
        .and_then(|value| value.checked_add(records_len))
        .ok_or_else(|| "compiled pack length overflowed".to_owned())?;
    if expected_len != file_len {
        return Err(format!(
            "compiled pack length mismatch: expected {expected_len}, got {file_len}"
        ));
    }
    let mut source_digest = [0_u8; 32];
    source_digest.copy_from_slice(&header[48..80]);
    let mut header_index_digest = [0_u8; 32];
    header_index_digest.copy_from_slice(&header[80..112]);
    Ok(ParsedHeader {
        pack_id_len,
        rule_count,
        program_count,
        index_len,
        records_len,
        source_digest,
        header_index_digest,
    })
}

fn encode_index(rules: &[RuleRecord], programs: &[ProgramEntry]) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    write_u32(&mut output, rules.len())?;
    for rule in rules {
        write_string(&mut output, &rule.id)?;
        output.extend_from_slice(&rule.offset.to_le_bytes());
        output.extend_from_slice(&rule.length.to_le_bytes());
        output.extend_from_slice(&rule.digest);
    }
    write_u32(&mut output, programs.len())?;
    for entry in programs {
        write_string(&mut output, &entry.program)?;
        write_u32(&mut output, entry.candidates.len())?;
        for candidate in &entry.candidates {
            output.extend_from_slice(&candidate.to_le_bytes());
        }
    }
    Ok(output)
}

fn decode_index(
    bytes: &[u8],
    rule_count: usize,
    program_count: usize,
) -> Result<(Vec<RuleRecord>, Vec<ProgramEntry>), String> {
    let mut cursor = SliceCursor::new(bytes);
    if cursor.read_u32()? as usize != rule_count {
        return Err("compiled rule count disagrees with index".to_owned());
    }
    let mut rules = Vec::with_capacity(rule_count);
    for _ in 0..rule_count {
        let id = cursor.read_string()?;
        let offset = cursor.read_u64()?;
        let length = cursor.read_u32()?;
        let digest = cursor.read_array()?;
        rules.push(RuleRecord {
            id,
            offset,
            length,
            digest,
        });
    }
    if cursor.read_u32()? as usize != program_count {
        return Err("compiled program count disagrees with index".to_owned());
    }
    let mut programs = Vec::with_capacity(program_count);
    for _ in 0..program_count {
        let program = cursor.read_string()?;
        let count = cursor.read_u32()? as usize;
        if count == 0 || count > rule_count {
            return Err("invalid program candidate count".to_owned());
        }
        let mut candidates = Vec::with_capacity(count);
        for _ in 0..count {
            candidates.push(cursor.read_u32()?);
        }
        programs.push(ProgramEntry {
            program,
            candidates,
        });
    }
    if !cursor.is_empty() {
        return Err("compiled index has trailing bytes".to_owned());
    }
    Ok((rules, programs))
}

fn validate_index(
    rules: &[RuleRecord],
    programs: &[ProgramEntry],
    records_len: u64,
) -> Result<(), String> {
    if rules.windows(2).any(|pair| pair[0].id >= pair[1].id) {
        return Err("compiled rule table is not strictly sorted".to_owned());
    }
    if programs
        .windows(2)
        .any(|pair| pair[0].program >= pair[1].program)
    {
        return Err("compiled program table is not strictly sorted".to_owned());
    }
    for rule in rules {
        validate_compiled_rule_id(&rule.id)?;
        let end = rule
            .offset
            .checked_add(u64::from(rule.length))
            .ok_or_else(|| "compiled rule bounds overflowed".to_owned())?;
        if rule.length == 0 || rule.length as usize > MAX_SOURCE_FILE_BYTES || end > records_len {
            return Err("compiled rule record is out of bounds".to_owned());
        }
    }
    let mut ranges: Vec<(u64, u64)> = rules
        .iter()
        .map(|rule| (rule.offset, rule.offset + u64::from(rule.length)))
        .collect();
    ranges.sort_unstable();
    if ranges.first().is_none_or(|range| range.0 != 0)
        || ranges.windows(2).any(|pair| pair[0].1 != pair[1].0)
        || ranges.last().is_none_or(|range| range.1 != records_len)
    {
        return Err("compiled rule records must cover the record section exactly".to_owned());
    }
    for entry in programs {
        validate_program(&entry.program)?;
        if entry.candidates.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("program candidates are not strictly sorted".to_owned());
        }
        if entry
            .candidates
            .iter()
            .any(|candidate| *candidate as usize >= rules.len())
        {
            return Err("program candidate is out of bounds".to_owned());
        }
    }
    Ok(())
}

fn file_digest(file: &mut File, path: &Path) -> Result<[u8; 32], String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("could not hash {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind {}: {error}", path.display()))?;
    Ok(hasher.finalize().into())
}

fn header_index_digest(header: &[u8; HEADER_LEN], pack_id: &[u8], index: &[u8]) -> [u8; 32] {
    let mut clean_header = *header;
    clean_header[DIGEST_OFFSET..DIGEST_OFFSET + DIGEST_LEN].fill(0);
    let mut hasher = Sha256::new();
    hasher.update(b"yarp-rule-index-v1\0");
    hasher.update(clean_header);
    hasher.update(pack_id);
    hasher.update(index);
    hasher.finalize().into()
}

fn validate_compiled_pack_id(id: &str) -> Result<(), String> {
    validate_compiled_id(id, false, "pack")
}

fn validate_compiled_rule_id(id: &str) -> Result<(), String> {
    validate_compiled_id(id, true, "rule")
}

fn validate_compiled_id(id: &str, slash: bool, label: &str) -> Result<(), String> {
    let valid = |byte: u8| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'.' | b'_' | b'-')
            || (slash && byte == b'/')
    };
    if id.is_empty()
        || id.len() > MAX_ID_BYTES
        || !id.is_ascii()
        || !id.bytes().all(valid)
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id.as_bytes()[id.len() - 1].is_ascii_alphanumeric()
        || id
            .as_bytes()
            .windows(2)
            .any(|pair| is_id_separator(pair[0], slash) && is_id_separator(pair[1], slash))
    {
        return Err(format!("invalid compiled {label} id"));
    }
    Ok(())
}

const fn is_id_separator(byte: u8, slash: bool) -> bool {
    matches!(byte, b'.' | b'_' | b'-') || (slash && byte == b'/')
}

fn validate_program(program: &str) -> Result<(), String> {
    if program.is_empty()
        || program.len() > MAX_ID_BYTES
        || !program.is_ascii()
        || program.bytes().any(|byte| {
            byte.is_ascii_whitespace()
                || matches!(
                    byte,
                    b'/' | b'\\'
                        | 0
                        | b'|'
                        | b'&'
                        | b';'
                        | b'<'
                        | b'>'
                        | b'('
                        | b')'
                        | b'`'
                        | b'$'
                        | b'#'
                )
        })
    {
        return Err("invalid compiled program".to_owned());
    }
    Ok(())
}

fn write_string(output: &mut Vec<u8>, value: &str) -> Result<(), String> {
    let length = u16::try_from(value.len()).map_err(|_| "index string is too long".to_owned())?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn write_u32(output: &mut Vec<u8>, value: usize) -> Result<(), String> {
    output.extend_from_slice(
        &u32::try_from(value)
            .map_err(|_| "index count does not fit u32".to_owned())?
            .to_le_bytes(),
    );
    Ok(())
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut [u8], offset: usize, value: u64) {
    output[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(input: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([input[offset], input[offset + 1]])
}

fn get_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap_or_default())
}

fn get_u64(input: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(input[offset..offset + 8].try_into().unwrap_or_default())
}

struct SliceCursor<'a> {
    body: &'a [u8],
    offset: usize,
}

impl<'a> SliceCursor<'a> {
    const fn new(body: &'a [u8]) -> Self {
        Self { body, offset: 0 }
    }

    fn read_exact(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| "compiled index offset overflowed".to_owned())?;
        let value = self
            .body
            .get(self.offset..end)
            .ok_or_else(|| "compiled index is truncated".to_owned())?;
        self.offset = end;
        Ok(value)
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let bytes: [u8; 4] = self
            .read_exact(4)?
            .try_into()
            .map_err(|_| "compiled u32 is truncated".to_owned())?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        let bytes: [u8; 8] = self
            .read_exact(8)?
            .try_into()
            .map_err(|_| "compiled u64 is truncated".to_owned())?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_array(&mut self) -> Result<[u8; 32], String> {
        self.read_exact(32)?
            .try_into()
            .map_err(|_| "compiled digest is truncated".to_owned())
    }

    fn read_string(&mut self) -> Result<String, String> {
        let length_bytes: [u8; 2] = self
            .read_exact(2)?
            .try_into()
            .map_err(|_| "compiled string length is truncated".to_owned())?;
        let length = u16::from_le_bytes(length_bytes) as usize;
        String::from_utf8(self.read_exact(length)?.to_vec())
            .map_err(|_| "compiled string is not UTF-8".to_owned())
    }

    fn is_empty(&self) -> bool {
        self.offset == self.body.len()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use tempfile::{NamedTempFile, TempDir};

    use super::*;
    use crate::source::SourcePack;

    #[test]
    fn compiles_and_verifies_a_deterministic_index() {
        let directory = source_pack();
        let source = SourcePack::load(directory.path()).expect("source");
        let first = compile(&source).expect("compile");
        let second = compile(&source).expect("compile");
        assert_eq!(first, second);
        let mut file = NamedTempFile::new().expect("pack file");
        file.write_all(&first).expect("write pack");
        file.flush().expect("flush pack");
        let mut pack =
            CompiledPack::open(file.path(), Some(source.source_digest), None).expect("open");
        assert_eq!(pack.id, "test-pack");
        let expected_compiled_digest: [u8; 32] = Sha256::digest(&first).into();
        assert_eq!(pack.compiled_digest, expected_compiled_digest);
        assert_eq!(pack.candidate_indices("tool"), &[0]);
        assert_eq!(pack.read_rule(0).expect("rule").id, "tests/run");
        pack.verify_all().expect("verify");
        assert!(
            CompiledPack::open(file.path(), None, Some([0_u8; 32]))
                .expect_err("compiled digest mismatch")
                .contains("compiled pack digest changed")
        );
    }

    #[test]
    fn detects_changes_after_the_pack_was_opened() {
        let directory = source_pack();
        let source = SourcePack::load(directory.path()).expect("source");
        let bytes = compile(&source).expect("compile");
        let mut file = NamedTempFile::new().expect("pack file");
        file.write_all(&bytes).expect("write pack");
        file.flush().expect("flush pack");
        let mut pack = CompiledPack::open(file.path(), None, None).expect("open");

        file.as_file_mut()
            .seek(SeekFrom::Start(
                u64::try_from(bytes.len() - 1).expect("offset"),
            ))
            .expect("seek");
        file.write_all(&[bytes[bytes.len() - 1] ^ 1])
            .expect("modify pack");
        file.flush().expect("flush modification");

        assert!(
            pack.verify_compiled_digest()
                .expect_err("changed pack")
                .contains("changed while loading")
        );
    }

    #[test]
    fn rejects_oversized_indexed_records_before_allocation() {
        let length = u32::try_from(MAX_SOURCE_FILE_BYTES + 1).expect("record length");
        let rules = [RuleRecord {
            id: "tests/run".to_owned(),
            offset: 0,
            length,
            digest: [0_u8; 32],
        }];
        let programs = [ProgramEntry {
            program: "tool".to_owned(),
            candidates: vec![0],
        }];
        assert!(
            validate_index(&rules, &programs, u64::from(length))
                .expect_err("oversized record")
                .contains("out of bounds")
        );
    }

    #[test]
    fn rejects_corrupt_headers_indexes_and_records() {
        let directory = source_pack();
        let source = SourcePack::load(directory.path()).expect("source");
        let bytes = compile(&source).expect("compile");
        for offset in [0, 80, bytes.len() - 1] {
            let mut corrupt = bytes.clone();
            corrupt[offset] ^= 1;
            let mut file = NamedTempFile::new().expect("pack file");
            file.write_all(&corrupt).expect("write pack");
            file.flush().expect("flush pack");
            let opened = CompiledPack::open(file.path(), None, None);
            if offset == bytes.len() - 1 {
                let mut pack = opened.expect("record corruption loads index");
                assert!(pack.verify_all().is_err());
            } else {
                assert!(opened.is_err());
            }
        }
    }

    fn source_pack() -> TempDir {
        let directory = TempDir::new().expect("temp directory");
        fs::create_dir(directory.path().join("rules")).expect("rules directory");
        fs::write(
            directory.path().join("pack.json"),
            r#"{"schema_version":1,"id":"test-pack","rules":["rules/test.json"]}"#,
        )
        .expect("manifest");
        fs::write(
            directory.path().join("rules/test.json"),
            r#"{"id":"tests/run","match":{"program":["tool"],"argv_prefix":["run"]},"action":"reduce","reducer":{"kind":"test_summary"},"success":{"max_line_bytes":16384,"max_output_bytes":32768,"min_savings_bytes":120,"min_savings_basis_points":1000},"failure":{"max_line_bytes":16384,"max_output_bytes":65536,"min_savings_bytes":120,"min_savings_basis_points":500}}"#,
        )
        .expect("rule");
        directory
    }
}
