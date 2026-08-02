use super::classify::{body, contains_ascii_insensitive, trim_start};
use super::evidence::EvidenceClass;

#[must_use]
pub fn classify(line: &[u8]) -> EvidenceClass {
    let line = body(line);
    let trimmed = trim_start(line);
    if is_outcome(trimmed) {
        EvidenceClass::Outcome
    } else if is_routine(trimmed) {
        EvidenceClass::Noise
    } else if is_diagnostic(trimmed) {
        EvidenceClass::Diagnostic
    } else if trimmed.starts_with(b"running ")
        || trimmed.starts_with(b"collected ")
        || trimmed.starts_with(b"test session starts")
    {
        EvidenceClass::Structure
    } else if trimmed.is_empty() {
        EvidenceClass::Noise
    } else {
        EvidenceClass::Example
    }
}

fn is_outcome(line: &[u8]) -> bool {
    [
        &b"test result:"[..],
        &b"failures:"[..],
        &b"Test Files  "[..],
        &b"Tests  "[..],
    ]
    .iter()
    .any(|prefix| starts_ascii_insensitive(line, prefix))
        || contains_ascii_insensitive(line, b" passed in ")
        || contains_ascii_insensitive(line, b" failed, ")
            && contains_ascii_insensitive(line, b" passed")
}

fn is_diagnostic(line: &[u8]) -> bool {
    [
        &b"FAIL "[..],
        &b"FAILED "[..],
        &b"--- FAIL:"[..],
        &b"not ok "[..],
        &b"AssertionError"[..],
        &b"ZodError"[..],
        &b"Caused by:"[..],
        &b"Traceback"[..],
        &b"error:"[..],
        &b"error["[..],
        &b"npm error"[..],
        &b"thread '"[..],
        &b"E "[..],
    ]
    .iter()
    .any(|prefix| starts_ascii_insensitive(line, prefix))
        || line.starts_with("❯".as_bytes()) && contains_ascii_insensitive(line, b" failed")
        || contains_ascii_insensitive(line, b"testCodeFailure")
        || contains_ascii_insensitive(line, b"stopping after ")
            && contains_ascii_insensitive(line, b" failure")
}

fn starts_ascii_insensitive(line: &[u8], prefix: &[u8]) -> bool {
    line.get(..prefix.len()).is_some_and(|candidate| {
        candidate
            .iter()
            .zip(prefix)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn is_routine(line: &[u8]) -> bool {
    line.starts_with(b"test ") && (line.ends_with(b" ... ok") || line.ends_with(b" ok"))
        || line.starts_with(b"   Compiling ")
        || line.starts_with(b"Compiling ")
        || line.starts_with(b"Checking ")
        || line.starts_with(b"Fresh ")
        || contains_ascii_insensitive(line, b" passed in ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_test_names_and_fixture_values_are_not_diagnostics() {
        assert_eq!(
            classify(b"test handles_error_without_failure ... ok\n"),
            EvidenceClass::Noise
        );
        assert_eq!(
            classify(b"      \"code\": \"space.variable.ENRICH_MAX_ERROR_RATE\",\n"),
            EvidenceClass::Example
        );
        assert_eq!(
            classify(b"E       AssertionError: assert 1 == 2\n"),
            EvidenceClass::Diagnostic
        );
        assert_eq!(
            classify(b"error[E0308]: type mismatch\n"),
            EvidenceClass::Diagnostic
        );
    }
}
