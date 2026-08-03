use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom};

use regex::bytes::{Regex, RegexBuilder};

use crate::archive::{Archive, SourceCompleteness, SourceName, VerifiedSource};
use crate::reducers::filter::AnsiStripper;

pub const SEARCH_HELP: &str = "Search one archived YARP call.\n\nExamples:\n  yarp search REF 'error|FAILED'\n  yarp search REF 'literal text' -F -i\n  yarp search REF 'warning' -v -C 3 --max-results 20\n  yarp read REF stdout 118:130\n\nUsage:\n  yarp search REF PATTERN [options]\n  yarp search REF -e PATTERN [-e PATTERN ...] [options]\n\nOptions: -e/--regexp -F/--fixed-strings -i/--ignore-case\n         -w/--word-regexp (ASCII boundaries) -v/--invert-match\n         -A/--after-context -B/--before-context -C/--context\n         -m/--max-results --\n";

const MAX_PATTERN_BYTES: usize = 1_024;
const MAX_PATTERNS: usize = 8;
const MAX_QUERY_LINE_BYTES: usize = 1024 * 1024;
const MAX_DISPLAY_LINE_BYTES: usize = 4 * 1024;
const MAX_DISPLAY_BYTES_PER_SOURCE: usize = 7 * 1024;
const MAX_DISPLAY_RECORDS: usize = 2_048;
const MAX_RENDERED_SELECTED_LINES: usize = 32;
const MAX_QUERY_OUTPUT_BYTES: usize = 32 * 1024;
const REGEX_SIZE_LIMIT: usize = 512 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub enum SearchOutcome {
    Matches(Vec<u8>),
    NoMatches(Vec<u8>),
}

#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the supported search flags are independent command-line switches"
)]
struct SearchOptions {
    archive_ref: String,
    patterns: Vec<String>,
    fixed: bool,
    ignore_case: bool,
    word: bool,
    invert: bool,
    before: usize,
    after: usize,
    max_results: usize,
}

#[derive(Debug)]
struct DisplayLine {
    text: String,
    selected: bool,
}

#[derive(Debug)]
struct SourceSearch {
    name: SourceName,
    completeness: SourceCompleteness,
    total_lines: u64,
    total_selected: u64,
    displayed_selected: Vec<u64>,
    lines: BTreeMap<u64, DisplayLine>,
    before: usize,
    after: usize,
    max_results: usize,
}

/// Search verified archived sources with a deliberately small `rg`-style syntax.
///
/// # Errors
///
/// Returns an error before source output when arguments, references, payloads, text, or bounds are
/// invalid.
pub fn search(arguments: &[String]) -> Result<SearchOutcome, String> {
    if matches!(arguments, [argument] if argument == "--help" || argument == "-h") {
        return Ok(SearchOutcome::Matches(SEARCH_HELP.as_bytes().to_vec()));
    }
    let options = parse_search(arguments)?;
    let matcher = compile_matcher(&options)?;
    let archive = Archive::open_read_only()?;
    let sources = archive.searchable_sources(&options.archive_ref)?;
    let mut results = Vec::with_capacity(sources.len());
    let mut selected = 0_u64;
    for source in sources {
        let result = search_source(source, &matcher, &options)?;
        selected = selected.saturating_add(result.total_selected);
        results.push(result);
    }
    if selected == 0 {
        return Ok(SearchOutcome::NoMatches(b"No matches\n".to_vec()));
    }
    let output = render_search(&options.archive_ref, &results)?;
    Ok(SearchOutcome::Matches(output))
}

/// Read one exact bounded line or byte range from a verified archived source.
///
/// # Errors
///
/// Returns an error before stdout when arguments, references, payloads, sources, or ranges are
/// invalid.
pub fn read(arguments: &[String]) -> Result<Vec<u8>, String> {
    let request = parse_read(arguments)?;
    let archive = Archive::open_read_only()?;
    let sources = archive.searchable_sources(&request.archive_ref)?;
    let mut source = choose_source(sources, request.source.as_deref())?;
    match request.range {
        ReadRange::Lines { start, end } => read_line_range(&mut source, start, end),
        ReadRange::Bytes { start, end } => read_byte_range(&mut source, start, end),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "keeping the closed search option grammar in one parser makes conflicts explicit"
)]
fn parse_search(arguments: &[String]) -> Result<SearchOptions, String> {
    let mut positional = Vec::new();
    let mut patterns = Vec::new();
    let mut fixed = false;
    let mut ignore_case = false;
    let mut word = false;
    let mut invert = false;
    let mut before = None;
    let mut after = None;
    let mut max_results = None;
    let mut end_options = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if end_options {
            positional.push(argument.clone());
            index += 1;
            continue;
        }
        match argument.as_str() {
            "--" => {
                end_options = true;
                index += 1;
            }
            "-F" | "--fixed-strings" => {
                fixed = true;
                index += 1;
            }
            "-i" | "--ignore-case" => {
                ignore_case = true;
                index += 1;
            }
            "-w" | "--word-regexp" => {
                word = true;
                index += 1;
            }
            "-v" | "--invert-match" => {
                invert = true;
                index += 1;
            }
            "-e" | "--regexp" => {
                patterns.push(require_value(arguments, index, argument)?);
                index += 2;
            }
            "-A" | "--after-context" => {
                let value = parse_count(&require_value(arguments, index, argument)?, 0, 50)?;
                set_consistent(&mut after, value, "after-context")?;
                index += 2;
            }
            "-B" | "--before-context" => {
                let value = parse_count(&require_value(arguments, index, argument)?, 0, 50)?;
                set_consistent(&mut before, value, "before-context")?;
                index += 2;
            }
            "-C" | "--context" => {
                let value = parse_count(&require_value(arguments, index, argument)?, 0, 50)?;
                set_consistent(&mut before, value, "before-context")?;
                set_consistent(&mut after, value, "after-context")?;
                index += 2;
            }
            "-m" | "--max-results" => {
                let value = parse_count(&require_value(arguments, index, argument)?, 1, 100)?;
                set_consistent(&mut max_results, value, "max-results")?;
                index += 2;
            }
            value if value.starts_with('-') => {
                return Err(format!(
                    "unknown search option {value}; expected: yarp search REF PATTERN"
                ));
            }
            _ => {
                positional.push(argument.clone());
                index += 1;
            }
        }
    }
    let Some(archive_ref) = positional.first().cloned() else {
        return Err("missing archive reference; expected: yarp search REF PATTERN".to_owned());
    };
    let positional_patterns = &positional[1..];
    if !patterns.is_empty() && !positional_patterns.is_empty() {
        return Err(
            "positional PATTERN and -e are mutually exclusive; expected: yarp search REF PATTERN"
                .to_owned(),
        );
    }
    if positional_patterns.len() > 1 {
        return Err(
            "search accepts one positional pattern; expected: yarp search REF PATTERN".to_owned(),
        );
    }
    if let Some(pattern) = positional_patterns.first() {
        patterns.push(pattern.clone());
    }
    if patterns.is_empty() {
        return Err("missing search pattern; expected: yarp search REF PATTERN".to_owned());
    }
    if patterns.len() > MAX_PATTERNS {
        return Err(format!("search accepts at most {MAX_PATTERNS} patterns"));
    }
    for pattern in &patterns {
        if pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES {
            return Err(format!(
                "each search pattern must contain 1 through {MAX_PATTERN_BYTES} UTF-8 bytes"
            ));
        }
    }
    Ok(SearchOptions {
        archive_ref,
        patterns,
        fixed,
        ignore_case,
        word,
        invert,
        before: before.unwrap_or(2),
        after: after.unwrap_or(2),
        max_results: max_results.unwrap_or(20),
    })
}

fn require_value(arguments: &[String], index: usize, option: &str) -> Result<String, String> {
    arguments
        .get(index + 1)
        .cloned()
        .ok_or_else(|| format!("{option} requires a value; expected: yarp search REF PATTERN"))
}

fn parse_count(value: &str, minimum: usize, maximum: usize) -> Result<usize, String> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("invalid canonical unsigned count {value:?}"));
    }
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("count {value:?} is outside the supported range"))?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(format!("count must be between {minimum} and {maximum}"));
    }
    Ok(parsed)
}

fn set_consistent(target: &mut Option<usize>, value: usize, label: &str) -> Result<(), String> {
    if target.is_some_and(|existing| existing != value) {
        return Err(format!("conflicting {label} values"));
    }
    *target = Some(value);
    Ok(())
}

fn compile_matcher(options: &SearchOptions) -> Result<Regex, String> {
    let alternatives = options
        .patterns
        .iter()
        .map(|pattern| {
            let pattern = if options.fixed {
                regex::escape(pattern)
            } else {
                pattern.clone()
            };
            if options.word {
                format!(r"(?:\b(?:{pattern})\b)")
            } else {
                format!("(?:{pattern})")
            }
        })
        .collect::<Vec<_>>()
        .join("|");
    RegexBuilder::new(&alternatives)
        .case_insensitive(options.ignore_case)
        .unicode(false)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|error| format!("invalid bounded search pattern: {error}"))
}

fn search_source(
    mut source: VerifiedSource,
    matcher: &Regex,
    options: &SearchOptions,
) -> Result<SourceSearch, String> {
    if source.media_type != "text/plain; charset=utf-8" {
        return Err(format!(
            "{} is binary; use yarp read REF {} --bytes START:END",
            source.name.as_str(),
            source.name.as_str()
        ));
    }
    source
        .body
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind {}: {error}", source.name.as_str()))?;
    let mut previous = VecDeque::<(u64, String)>::with_capacity(options.before);
    let mut display = DisplayStore::default();
    let mut displayed_selected = Vec::new();
    let mut total_selected = 0_u64;
    let mut total_lines = 0_u64;
    let mut after_remaining = 0_usize;
    let mut stripper = AnsiStripper::new();
    scan_query_lines(&mut source.body, |line_number, raw| {
        total_lines = line_number;
        let normalized = normalize_line(raw, &mut stripper)?;
        if after_remaining > 0 {
            display.insert(line_number, normalized.clone(), false);
            after_remaining -= 1;
        }
        let line_matches = matcher.is_match(normalized.as_bytes());
        let selected = if options.invert {
            !line_matches
        } else {
            line_matches
        };
        if selected {
            total_selected = total_selected.saturating_add(1);
            if displayed_selected.len() < options.max_results.min(MAX_RENDERED_SELECTED_LINES)
                && display.insert(line_number, normalized.clone(), true)
            {
                for (context_line, context) in &previous {
                    display.insert(*context_line, context.clone(), false);
                }
                displayed_selected.push(line_number);
                after_remaining = options.after;
            }
        }
        if options.before > 0 {
            if previous.len() == options.before {
                previous.pop_front();
            }
            previous.push_back((line_number, normalized));
        }
        Ok(())
    })?;
    Ok(SourceSearch {
        name: source.name,
        completeness: source.completeness,
        total_lines,
        total_selected,
        displayed_selected,
        lines: display.lines,
        before: options.before,
        after: options.after,
        max_results: options.max_results,
    })
}

#[derive(Default)]
struct DisplayStore {
    lines: BTreeMap<u64, DisplayLine>,
    bytes: usize,
}

impl DisplayStore {
    fn insert(&mut self, number: u64, text: String, selected: bool) -> bool {
        if let Some(existing) = self.lines.get_mut(&number) {
            existing.selected |= selected;
            return true;
        }
        let text = truncate_display_line(text);
        if self.lines.len() >= MAX_DISPLAY_RECORDS
            || self.bytes.saturating_add(text.len()) > MAX_DISPLAY_BYTES_PER_SOURCE
        {
            return false;
        }
        self.bytes = self.bytes.saturating_add(text.len());
        self.lines.insert(number, DisplayLine { text, selected });
        true
    }
}

fn scan_query_lines(
    reader: &mut impl Read,
    mut observe: impl FnMut(u64, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut line = Vec::with_capacity(64 * 1024);
    let mut line_number = 0_u64;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("could not read archived source: {error}"))?;
        if count == 0 {
            break;
        }
        let mut start = 0;
        for (index, byte) in buffer[..count].iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            append_bounded_line(&mut line, &buffer[start..=index])?;
            line_number = line_number.saturating_add(1);
            observe(line_number, &line)?;
            line.clear();
            start = index + 1;
        }
        append_bounded_line(&mut line, &buffer[start..count])?;
    }
    if !line.is_empty() {
        line_number = line_number.saturating_add(1);
        observe(line_number, &line)?;
    }
    Ok(())
}

fn append_bounded_line(line: &mut Vec<u8>, part: &[u8]) -> Result<(), String> {
    if line.len().saturating_add(part.len()) > MAX_QUERY_LINE_BYTES {
        return Err(format!(
            "archived source line exceeds {MAX_QUERY_LINE_BYTES} bytes; use a bounded yarp read --bytes range"
        ));
    }
    line.extend_from_slice(part);
    Ok(())
}

fn normalize_line(raw: &[u8], ansi: &mut AnsiStripper) -> Result<String, String> {
    let mut clean_bytes = Vec::with_capacity(raw.len().min(MAX_QUERY_LINE_BYTES));
    for byte in raw {
        if let Some(value) = ansi.push_byte(*byte) {
            clean_bytes.push(value);
        }
    }
    let clean_bytes = clean_bytes
        .strip_suffix(b"\r\n")
        .or_else(|| clean_bytes.strip_suffix(b"\n"))
        .unwrap_or(&clean_bytes);
    let text = std::str::from_utf8(clean_bytes)
        .map_err(|_| "archived source is not valid UTF-8; use yarp read --bytes".to_owned())?;
    let mut normalized = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\t' || !character.is_control() {
            normalized.push(character);
        } else if character.is_ascii() {
            write!(normalized, "\\x{:02x}", u32::from(character))
                .map_err(|error| format!("could not render control byte: {error}"))?;
        } else {
            write!(normalized, "\\u{{{:x}}}", u32::from(character))
                .map_err(|error| format!("could not render control character: {error}"))?;
        }
        if normalized.len() > MAX_QUERY_LINE_BYTES {
            return Err("normalized archived source line exceeds 1 MiB".to_owned());
        }
    }
    Ok(normalized)
}

fn truncate_display_line(mut value: String) -> String {
    if value.len() <= MAX_DISPLAY_LINE_BYTES {
        return value;
    }
    let mut end = MAX_DISPLAY_LINE_BYTES.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.truncate(end);
    value.push_str("...");
    value
}

fn render_search(archive_ref: &str, sources: &[SourceSearch]) -> Result<Vec<u8>, String> {
    let mut output = Vec::with_capacity(MAX_QUERY_OUTPUT_BYTES);
    output.extend_from_slice(format!("[yarp search: ref={archive_ref}]\n").as_bytes());
    for (source_index, source) in sources.iter().enumerate() {
        if source_index > 0 {
            output.extend_from_slice(b"--\n");
        }
        output.extend_from_slice(
            format!(
                "[source={} complete={} matches={} showing={} before={} after={} max_results={}]\n",
                source.name.as_str(),
                completeness_label(source.completeness),
                source.total_selected,
                source.displayed_selected.len(),
                source.before,
                source.after,
                source.max_results
            )
            .as_bytes(),
        );
        let groups = displayed_groups(source);
        for (group_index, (start, end)) in groups.iter().enumerate() {
            if group_index > 0 {
                output.extend_from_slice(b"--\n");
            }
            for (number, line) in source.lines.range(*start..=*end) {
                let separator = if line.selected { ':' } else { '-' };
                output.extend_from_slice(
                    format!(
                        "{}{separator}{}{separator}{}\n",
                        source.name.as_str(),
                        number,
                        line.text
                    )
                    .as_bytes(),
                );
            }
        }
        let omitted = source
            .total_selected
            .saturating_sub(u64::try_from(source.displayed_selected.len()).unwrap_or(u64::MAX));
        if omitted > 0 {
            output.extend_from_slice(
                format!("[yarp search: {omitted} selected line(s) omitted]\n").as_bytes(),
            );
        }
        for (start, end) in groups {
            output.extend_from_slice(
                format!(
                    "Read exact context: yarp read {archive_ref} {} {start}:{end}\n",
                    source.name.as_str()
                )
                .as_bytes(),
            );
        }
    }
    if output.len() > MAX_QUERY_OUTPUT_BYTES {
        return Err("bounded search rendering exceeded 32 KiB".to_owned());
    }
    Ok(output)
}

fn displayed_groups(source: &SourceSearch) -> Vec<(u64, u64)> {
    let mut groups = Vec::<(u64, u64)>::new();
    for selected in &source.displayed_selected {
        let start = selected.saturating_sub(source.before as u64).max(1);
        let end = selected
            .saturating_add(source.after as u64)
            .min(source.total_lines);
        if let Some(last) = groups.last_mut()
            && start <= last.1.saturating_add(1)
        {
            last.1 = last.1.max(end);
        } else {
            groups.push((start, end));
        }
    }
    groups
}

const fn completeness_label(value: SourceCompleteness) -> &'static str {
    match value {
        SourceCompleteness::Complete => "true",
        SourceCompleteness::Incomplete => "false",
        SourceCompleteness::Unknown => "unknown",
    }
}

#[derive(Debug)]
struct ReadRequest {
    archive_ref: String,
    source: Option<String>,
    range: ReadRange,
}

#[derive(Debug)]
enum ReadRange {
    Lines { start: u64, end: u64 },
    Bytes { start: u64, end: u64 },
}

fn parse_read(arguments: &[String]) -> Result<ReadRequest, String> {
    match arguments {
        [archive_ref, range] => {
            let (start, end) = parse_range(range, 1)?;
            Ok(ReadRequest {
                archive_ref: archive_ref.clone(),
                source: None,
                range: ReadRange::Lines { start, end },
            })
        }
        [archive_ref, source, range] => {
            let (start, end) = parse_range(range, 1)?;
            Ok(ReadRequest {
                archive_ref: archive_ref.clone(),
                source: Some(source.clone()),
                range: ReadRange::Lines { start, end },
            })
        }
        [archive_ref, source, bytes, range] if bytes == "--bytes" => {
            let (start, end) = parse_range(range, 0)?;
            Ok(ReadRequest {
                archive_ref: archive_ref.clone(),
                source: Some(source.clone()),
                range: ReadRange::Bytes { start, end },
            })
        }
        _ => Err("invalid read arguments; expected: yarp read REF [SOURCE] START:END".to_owned()),
    }
}

fn parse_range(value: &str, minimum_start: u64) -> Result<(u64, u64), String> {
    let Some((start, end)) = value.split_once(':') else {
        return Err("range must have START:END form".to_owned());
    };
    let start = parse_u64(start)?;
    let end = parse_u64(end)?;
    if start < minimum_start || end < start {
        return Err("range start and end are invalid".to_owned());
    }
    Ok((start, end))
}

fn parse_u64(value: &str) -> Result<u64, String> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("invalid canonical unsigned integer {value:?}"));
    }
    value
        .parse()
        .map_err(|_| format!("integer {value:?} is outside the supported range"))
}

fn choose_source(
    mut sources: Vec<VerifiedSource>,
    requested: Option<&str>,
) -> Result<VerifiedSource, String> {
    if let Some(requested) = requested {
        let Some(index) = sources
            .iter()
            .position(|source| source.name.as_str() == requested)
        else {
            let names = sources
                .iter()
                .map(|source| source.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "source {requested:?} is unavailable; choose: {names}"
            ));
        };
        return Ok(sources.swap_remove(index));
    }
    if sources.len() != 1 {
        let names = sources
            .iter()
            .map(|source| source.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!("source is required; choose: {names}"));
    }
    sources
        .pop()
        .ok_or_else(|| "archive reference has no readable source".to_owned())
}

fn read_byte_range(source: &mut VerifiedSource, start: u64, end: u64) -> Result<Vec<u8>, String> {
    if end > source.byte_length {
        return Err(format!(
            "byte range ends at {end}, but {} has {} bytes",
            source.name.as_str(),
            source.byte_length
        ));
    }
    let length = end.saturating_sub(start);
    if length > MAX_QUERY_OUTPUT_BYTES as u64 {
        return Err(format!(
            "exact read is {length} bytes; use a range no larger than {MAX_QUERY_OUTPUT_BYTES} bytes"
        ));
    }
    source
        .body
        .seek(SeekFrom::Start(start))
        .map_err(|error| format!("could not seek archived source: {error}"))?;
    let length = usize::try_from(length).map_err(|_| "byte range is too large".to_owned())?;
    let mut output = vec![0_u8; length];
    source
        .body
        .read_exact(&mut output)
        .map_err(|error| format!("could not read archived byte range: {error}"))?;
    Ok(output)
}

fn read_line_range(source: &mut VerifiedSource, start: u64, end: u64) -> Result<Vec<u8>, String> {
    source
        .body
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("could not rewind archived source: {error}"))?;
    let mut output = Vec::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut line_number = 1_u64;
    let mut saw_byte = false;
    let mut last_was_newline = false;
    let mut reached_end = false;
    'read: loop {
        let count = source
            .body
            .read(&mut buffer)
            .map_err(|error| format!("could not read archived line range: {error}"))?;
        if count == 0 {
            break;
        }
        for byte in &buffer[..count] {
            saw_byte = true;
            last_was_newline = *byte == b'\n';
            if (start..=end).contains(&line_number) {
                if output.len() == MAX_QUERY_OUTPUT_BYTES {
                    return Err(format!(
                        "exact line read exceeds {MAX_QUERY_OUTPUT_BYTES} bytes; use yarp read REF {} --bytes START:END",
                        source.name.as_str()
                    ));
                }
                output.push(*byte);
            }
            if *byte == b'\n' {
                if line_number == end {
                    reached_end = true;
                    break 'read;
                }
                line_number = line_number.saturating_add(1);
            }
        }
    }
    let total_lines = if reached_end {
        end
    } else if !saw_byte {
        0
    } else if last_was_newline {
        line_number.saturating_sub(1)
    } else {
        line_number
    };
    if start > total_lines || (!reached_end && end > total_lines) {
        return Err(format!(
            "line range {start}:{end} exceeds {} with {total_lines} lines",
            source.name.as_str()
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_search_options_in_any_position() {
        let options = parse_search(&strings(&[
            "-i",
            "yr_0123456789abcdef0123456789abcdef",
            "error",
            "-C",
            "3",
            "--max-results",
            "20",
        ]))
        .expect("search options");
        assert!(options.ignore_case);
        assert_eq!(options.before, 3);
        assert_eq!(options.after, 3);
        assert_eq!(options.max_results, 20);
    }

    #[test]
    fn supports_short_max_results_and_rejects_old_long_spelling() {
        let options = parse_search(&strings(&[
            "yr_0123456789abcdef0123456789abcdef",
            "error",
            "-m",
            "7",
        ]))
        .expect("short max-results option");
        assert_eq!(options.max_results, 7);
        assert!(
            parse_search(&strings(&[
                "yr_0123456789abcdef0123456789abcdef",
                "error",
                "--max-count",
                "7",
            ]))
            .is_err()
        );
    }

    #[test]
    fn ignore_case_uses_ascii_matching_without_unicode_features() {
        let options = parse_search(&strings(&[
            "yr_0123456789abcdef0123456789abcdef",
            "error.*code",
            "-i",
        ]))
        .expect("case-insensitive search options");
        let matcher = compile_matcher(&options).expect("case-insensitive matcher");
        assert!(matcher.is_match(b"ERROR 42 CODE"));
        assert!(matcher.is_match("éERROR code".as_bytes()));
    }

    #[test]
    fn word_regexp_uses_ascii_boundaries_without_unicode_features() {
        let options = parse_search(&strings(&[
            "yr_0123456789abcdef0123456789abcdef",
            "error",
            "-w",
        ]))
        .expect("word search options");
        let matcher = compile_matcher(&options).expect("word matcher");
        assert!(matcher.is_match(b"error"));
        assert!(matcher.is_match(b"an error!"));
        assert!(matcher.is_match("éerroré".as_bytes()));
        assert!(!matcher.is_match(b"terror"));
        assert!(!matcher.is_match(b"error_code"));
    }

    #[test]
    fn rejects_ambiguous_patterns_and_noncanonical_counts() {
        assert!(
            parse_search(&strings(&[
                "yr_0123456789abcdef0123456789abcdef",
                "one",
                "-e",
                "two",
            ]))
            .is_err()
        );
        assert!(
            parse_search(&strings(&[
                "yr_0123456789abcdef0123456789abcdef",
                "one",
                "-C",
                "03",
            ]))
            .is_err()
        );
    }

    #[test]
    fn strips_ansi_and_renders_control_bytes_visibly() {
        let mut stripper = AnsiStripper::new();
        assert_eq!(
            normalize_line(b"\x1b[31merror\x1b[0m\x07\n", &mut stripper).expect("line"),
            "error\\x07"
        );
    }

    #[test]
    fn parses_exact_line_and_byte_ranges() {
        assert!(matches!(
            parse_read(&strings(&[
                "yr_0123456789abcdef0123456789abcdef",
                "stdout",
                "1:4"
            ]))
            .expect("line read")
            .range,
            ReadRange::Lines { start: 1, end: 4 }
        ));
        assert!(matches!(
            parse_read(&strings(&[
                "yr_0123456789abcdef0123456789abcdef",
                "stdout",
                "--bytes",
                "0:4"
            ]))
            .expect("byte read")
            .range,
            ReadRange::Bytes { start: 0, end: 4 }
        ));
    }
}
