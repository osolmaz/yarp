use super::classify::{body, contains_ascii_insensitive, trim_start};
use super::evidence::EvidenceClass;

#[must_use]
pub fn classify(line: &[u8]) -> EvidenceClass {
    let line = trim_start(body(line));
    if is_outcome(line) {
        EvidenceClass::Outcome
    } else if line.starts_with("ℹ".as_bytes()) || is_artifact(line) {
        EvidenceClass::Example
    } else if is_diagnostic(line) {
        EvidenceClass::Diagnostic
    } else if is_progress(line) || line.is_empty() {
        EvidenceClass::Noise
    } else if line.starts_with(b"> ")
        || line.starts_with(b"[build]")
        || line.starts_with(b"[package]")
    {
        EvidenceClass::Structure
    } else {
        EvidenceClass::Example
    }
}

fn is_outcome(line: &[u8]) -> bool {
    [
        &b"build successful"[..],
        &b"build failed"[..],
        &b"command failed with exit code"[..],
        &b" passed in "[..],
    ]
    .iter()
    .any(|needle| contains_ascii_insensitive(line, needle))
        || [
            &b"Finished "[..],
            &b"Test Files  "[..],
            &b"Tests  "[..],
            &b"Build complete"[..],
        ]
        .iter()
        .any(|prefix| starts_ascii_insensitive(line, prefix))
        || line.starts_with("✔ Build complete".as_bytes())
}

fn is_artifact(line: &[u8]) -> bool {
    line.windows("├─ ".len())
        .any(|window| window == "├─ ".as_bytes() || window == "└─ ".as_bytes())
}

fn is_diagnostic(line: &[u8]) -> bool {
    [
        &b"error:"[..],
        &b"error["[..],
        &b"warning:"[..],
        &b"warning["[..],
        &b"fatal:"[..],
        &b"panic:"[..],
        &b"exception:"[..],
        &b"traceback"[..],
        &b"failed "[..],
        &b"failure:"[..],
        &b"npm error"[..],
        &b"npm err!"[..],
    ]
    .iter()
    .any(|prefix| starts_ascii_insensitive(line, prefix))
        || line.starts_with("✖".as_bytes())
        || line.starts_with("×".as_bytes())
        || [
            &b"##[error]"[..],
            &b"##[warning]"[..],
            &b"[error]"[..],
            &b"[warning]"[..],
            &b": error "[..],
            &b": warning "[..],
            &b" - error ts"[..],
            &b" - warning ts"[..],
            &b" command failed with exit code "[..],
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

fn is_progress(line: &[u8]) -> bool {
    [
        &b"Compiling "[..],
        &b"Checking "[..],
        &b"Fresh "[..],
        &b"Downloaded "[..],
        &b"Downloading "[..],
        &b"Building "[..],
        &b"Generating "[..],
        &b"[ "[..],
        &b"[="[..],
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_names_do_not_become_build_diagnostics() {
        assert_eq!(
            classify("ℹ dist/errors-hash.js 1.25 kB | gzip: 0.52 kB\n".as_bytes()),
            EvidenceClass::Example
        );
        assert_eq!(
            classify("19:21:07   ├─ /graph/payment-failure/index.html (+3ms)\n".as_bytes()),
            EvidenceClass::Example
        );
        assert_eq!(
            classify(b"      \"code\": \"space.variable.ENRICH_MAX_ERROR_RATE\",\n"),
            EvidenceClass::Example
        );
        assert_eq!(
            classify(b"+        throw new Error(`compiled hook runtime is missing`);\n"),
            EvidenceClass::Example
        );
        assert_eq!(
            classify(b"src/main.ts:2:5 - error TS2322: incompatible type\n"),
            EvidenceClass::Diagnostic
        );
        assert_eq!(
            classify("\u{2009}ELIFECYCLE\u{2009} Command failed with exit code 1.\n".as_bytes()),
            EvidenceClass::Outcome
        );
        assert_eq!(classify(b"error: build failed\n"), EvidenceClass::Outcome);
    }
}
