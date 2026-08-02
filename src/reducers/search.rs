use super::classify::{body, has_diagnostic_word};
use super::evidence::EvidenceClass;

#[must_use]
pub fn classify(line: &[u8]) -> EvidenceClass {
    let line = body(line);
    if line.is_empty() {
        EvidenceClass::Noise
    } else if (line.starts_with(b"rg:")
        || line.starts_with(b"grep:")
        || line.starts_with(b"git grep:")
        || line.starts_with(b"error:"))
        && has_diagnostic_word(line)
    {
        EvidenceClass::Diagnostic
    } else {
        EvidenceClass::Example
    }
}
