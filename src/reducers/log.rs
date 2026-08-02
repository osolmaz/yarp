use super::classify::{body, contains_ascii_insensitive};
use super::evidence::EvidenceClass;

#[must_use]
pub fn classify(line: &[u8]) -> EvidenceClass {
    let line = body(line);
    if line.is_empty() {
        EvidenceClass::Noise
    } else if is_diagnostic(line) {
        EvidenceClass::Diagnostic
    } else {
        EvidenceClass::Example
    }
}

fn is_diagnostic(line: &[u8]) -> bool {
    [
        &b"##[error]"[..],
        &b"##[warning]"[..],
        &b"[error]"[..],
        &b"[warning]"[..],
        &b"error: "[..],
        &b"warning: "[..],
        &b"fatal: "[..],
        &b"panic: "[..],
    ]
    .iter()
    .any(|prefix| starts_ascii_insensitive(line, prefix))
        || looks_timestamped(line)
            && [
                &b" ERROR "[..],
                &b" WARNING "[..],
                &b" WARN "[..],
                &b" FATAL "[..],
                &b" PANIC "[..],
                &b" FAILED "[..],
                &b"level=error"[..],
                &b"level=warning"[..],
                &b"\"level\":\"error\""[..],
                &b"\"level\":\"warning\""[..],
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

fn looks_timestamped(line: &[u8]) -> bool {
    line.get(..5)
        .is_some_and(|prefix| prefix[..4].iter().all(u8::is_ascii_digit) && prefix[4] == b'-')
        || [
            &b"Jan "[..],
            &b"Feb "[..],
            &b"Mar "[..],
            &b"Apr "[..],
            &b"May "[..],
            &b"Jun "[..],
            &b"Jul "[..],
            &b"Aug "[..],
            &b"Sep "[..],
            &b"Oct "[..],
            &b"Nov "[..],
            &b"Dec "[..],
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_code_terms_are_not_log_diagnostics() {
        assert_eq!(
            classify(b"+void main().catch((error: unknown) => {\n"),
            EvidenceClass::Example
        );
        assert_eq!(
            classify(b"2026-08-01T10:00:00Z ERROR request failed\n"),
            EvidenceClass::Diagnostic
        );
        assert_eq!(
            classify(b"2026-08-01T10:00:00Z level=warning message=retrying\n"),
            EvidenceClass::Diagnostic
        );
    }
}
