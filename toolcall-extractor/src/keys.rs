use sha2::{Digest, Sha256};

use crate::error::Result;

#[must_use]
pub fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

#[must_use]
pub fn key(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.len().to_be_bytes());
        hash.update(part);
    }
    hex(&hash.finalize())
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

#[must_use]
pub fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            u8::try_from((high << 4) | low).ok()
        })
        .collect()
}

pub fn canonical_json(value: &serde_json::Value) -> Result<String> {
    serde_json::to_string(value).map_err(Into::into)
}

#[must_use]
pub fn now_ms() -> i64 {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[must_use]
pub fn parse_timestamp_ms(value: Option<&str>) -> Option<i64> {
    value
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|timestamp| timestamp.timestamp_millis())
}

#[must_use]
pub fn bounded_message(message: &str) -> String {
    const LIMIT: usize = 512;
    if message.len() <= LIMIT {
        return message.to_owned();
    }
    let mut end = LIMIT;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unambiguous_and_stable() {
        assert_ne!(key(&[b"ab", b"c"]), key(&[b"a", b"bc"]));
        assert_eq!(sha256(b"abc").len(), 32);
    }

    #[test]
    fn bounds_messages_at_utf8_boundaries() {
        let message = "é".repeat(300);
        let bounded = bounded_message(&message);
        assert!(bounded.len() <= 515);
        assert!(bounded.ends_with('…'));
    }
}
