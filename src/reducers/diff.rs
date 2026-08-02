use super::classify::body;
use super::evidence::EvidenceClass;

#[must_use]
pub fn classify(line: &[u8]) -> EvidenceClass {
    let line = body(line);
    if line.starts_with(b"diff --git ")
        || line.starts_with(b"index ")
        || line.starts_with(b"--- ")
        || line.starts_with(b"+++ ")
        || line.starts_with(b"@@ ")
        || line.starts_with(b"Binary files ")
        || line.starts_with(b"new file mode ")
        || line.starts_with(b"deleted file mode ")
        || line.starts_with(b"rename from ")
        || line.starts_with(b"rename to ")
    {
        EvidenceClass::Structure
    } else if (line.starts_with(b"+") && !line.starts_with(b"+++"))
        || (line.starts_with(b"-") && !line.starts_with(b"---"))
    {
        EvidenceClass::Example
    } else {
        EvidenceClass::Noise
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_source_terms_are_not_diff_diagnostics() {
        assert_eq!(
            classify(b"     const error = event.error;\n"),
            EvidenceClass::Noise
        );
        assert_eq!(
            classify(b"+    throw new Error(\"boom\");\n"),
            EvidenceClass::Example
        );
    }
}
