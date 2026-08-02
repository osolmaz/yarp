use super::classify::{body, contains_ascii_insensitive, trim_start};
use super::evidence::EvidenceClass;

#[must_use]
pub fn classify(line: &[u8]) -> EvidenceClass {
    let line = trim_start(body(line));
    if is_outcome(line) {
        EvidenceClass::Outcome
    } else if is_diagnostic(line) {
        EvidenceClass::Diagnostic
    } else if line.ends_with(b":") {
        EvidenceClass::Structure
    } else if line.is_empty() {
        EvidenceClass::Noise
    } else {
        EvidenceClass::Example
    }
}

fn is_outcome(line: &[u8]) -> bool {
    line.starts_with(b"On branch ")
        || line.starts_with(b"Your branch ")
        || line.starts_with(b"HEAD detached")
        || line.starts_with(b"## ")
        || line.starts_with(b"Active:")
        || line.starts_with(b"Loaded:")
        || contains_ascii_insensitive(line, b"process completed with exit code")
        || contains_ascii_insensitive(line, b"command failed with exit code")
        || contains_ascii_insensitive(line, b"test files  ")
        || contains_ascii_insensitive(line, b"tests  ")
}

fn is_diagnostic(line: &[u8]) -> bool {
    [
        &b"##[error]"[..],
        &b"##[warning]"[..],
        &b"npm error"[..],
        &b"failure reason:"[..],
        &b"level=error"[..],
        &b"level=warning"[..],
        &b"\"level\":\"error\""[..],
        &b"\"level\":\"warning\""[..],
    ]
    .iter()
    .any(|needle| contains_ascii_insensitive(line, needle))
        || [
            &b" ERROR "[..],
            &b" WARNING "[..],
            &b" WARN "[..],
            &b" FATAL "[..],
            &b" PANIC "[..],
        ]
        .iter()
        .any(|needle| line.windows(needle.len()).any(|window| window == *needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_item_names_do_not_become_diagnostics() {
        assert_eq!(
            classify(b"M  src/runtime/errors.ts\n"),
            EvidenceClass::Example
        );
        assert_eq!(
            classify(
                b"check\tstep\t2026-08-01T00:00:00Z ##[error]Process completed with exit code 1.\n"
            ),
            EvidenceClass::Outcome
        );
        assert_eq!(
            classify(b"service[1]: ERROR request failed\n"),
            EvidenceClass::Diagnostic
        );
    }
}
