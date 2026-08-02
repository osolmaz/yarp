use super::classify::{body, contains_ascii_insensitive, has_diagnostic_word};
use super::evidence::EvidenceClass;

#[must_use]
pub fn classify(line: &[u8]) -> EvidenceClass {
    let line = body(line);
    if line.is_empty() {
        EvidenceClass::Noise
    } else if is_search_diagnostic(line) {
        EvidenceClass::Diagnostic
    } else {
        EvidenceClass::Example
    }
}

fn is_search_diagnostic(line: &[u8]) -> bool {
    let command_prefix = [
        &b"rg:"[..],
        &b"grep:"[..],
        &b"egrep:"[..],
        &b"fgrep:"[..],
        &b"git grep:"[..],
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix));
    (command_prefix || line.starts_with(b"error:"))
        && (has_diagnostic_word(line)
            || [
                &b"cannot"[..],
                &b"could not"[..],
                &b"denied"[..],
                &b"invalid"[..],
                &b"no such"[..],
                &b"unknown"[..],
                &b"unrecognized"[..],
            ]
            .iter()
            .any(|term| contains_ascii_insensitive(line, term)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_search_failures_without_misclassifying_matches() {
        assert_eq!(
            classify(b"rg: private: Permission denied\n"),
            EvidenceClass::Diagnostic
        );
        assert_eq!(
            classify(b"grep: missing: No such file or directory\n"),
            EvidenceClass::Diagnostic
        );
        assert_eq!(
            classify(b"src/lib.rs:4:grep: useful match\n"),
            EvidenceClass::Example
        );
    }
}
