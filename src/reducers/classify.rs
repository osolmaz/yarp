pub fn body(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r\n")
        .or_else(|| line.strip_suffix(b"\n"))
        .unwrap_or(line)
}

pub fn trim_start(mut value: &[u8]) -> &[u8] {
    while value
        .first()
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        value = &value[1..];
    }
    value
}

pub fn is_tool_outcome(line: &[u8]) -> bool {
    let line = trim_start(body(line));
    [
        &b"Process exited with code "[..],
        &b"Process running with session ID "[..],
        &b"Command exited with code "[..],
        &b"Command timed out"[..],
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

pub fn contains_ascii_insensitive(body: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && body.windows(needle.len()).any(|window| {
            window
                .iter()
                .zip(needle)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
}

pub const REGISTERED_DIAGNOSTICS: [&[u8]; 5] =
    [b"failure", b"panic", b"error", b"warning", b"test result"];

pub fn has_registered_diagnostic(body: &[u8]) -> bool {
    body.iter().enumerate().any(|(index, byte)| {
        let term = match byte.to_ascii_lowercase() {
            b'f' => Some(REGISTERED_DIAGNOSTICS[0]),
            b'p' => Some(REGISTERED_DIAGNOSTICS[1]),
            b'e' => Some(REGISTERED_DIAGNOSTICS[2]),
            b'w' => Some(REGISTERED_DIAGNOSTICS[3]),
            b't' => Some(REGISTERED_DIAGNOSTICS[4]),
            _ => None,
        };
        term.is_some_and(|term| {
            body.get(index..index.saturating_add(term.len()))
                .is_some_and(|candidate| {
                    candidate
                        .iter()
                        .zip(term)
                        .all(|(left, right)| left.eq_ignore_ascii_case(right))
                })
        })
    })
}

pub fn has_diagnostic_word(body: &[u8]) -> bool {
    [
        &b"error"[..],
        &b"failed"[..],
        &b"failure"[..],
        &b"warning"[..],
        &b"panic"[..],
        &b"fatal"[..],
        &b"exception"[..],
        &b"traceback"[..],
        &b"segmentation fault"[..],
    ]
    .iter()
    .any(|needle| contains_ascii_insensitive(body, needle))
}
