use super::classify::{body, contains_ascii_insensitive, trim_start};
use super::evidence::EvidenceClass;

#[must_use]
pub fn classify(line: &[u8]) -> EvidenceClass {
    let line = body(line);
    if line.is_empty() {
        EvidenceClass::Noise
    } else if is_cli_diagnostic(line) {
        EvidenceClass::Diagnostic
    } else if is_list_outcome(line) {
        EvidenceClass::Outcome
    } else {
        EvidenceClass::Example
    }
}

fn is_cli_diagnostic(line: &[u8]) -> bool {
    let line = trim_start(line);
    [
        &b"error:"[..],
        &b"fatal:"[..],
        &b"warning:"[..],
        &b"gh: error"[..],
    ]
    .iter()
    .any(|prefix| starts_ascii_insensitive(line, prefix))
}

fn is_list_outcome(line: &[u8]) -> bool {
    [
        &b"\tsuccess\t"[..],
        &b"\tfailure\t"[..],
        &b"\tcancelled\t"[..],
        &b"\tin_progress\t"[..],
        &b"\tqueued\t"[..],
    ]
    .iter()
    .any(|needle| contains_ascii_insensitive(line, needle))
}

fn starts_ascii_insensitive(line: &[u8], prefix: &[u8]) -> bool {
    line.get(..prefix.len()).is_some_and(|candidate| {
        candidate
            .iter()
            .zip(prefix)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_item_words_do_not_become_diagnostics() {
        assert_eq!(
            classify(b"abc123 fix: preserve error details after failures\n"),
            EvidenceClass::Example
        );
        assert_eq!(
            classify(b"completed\tfailure\tCI\tmain\n"),
            EvidenceClass::Outcome
        );
        assert_eq!(
            classify(b"fatal: bad revision\n"),
            EvidenceClass::Diagnostic
        );
    }
}
