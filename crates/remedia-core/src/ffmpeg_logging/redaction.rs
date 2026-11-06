use super::*;

pub(super) fn redact_sensitive_text(message: &str) -> String {
    let redacted = redact_open_paths(&redact_urls(message));
    let redacted = redact_key_values(&redacted);
    redacted.chars().take(LOG_MESSAGE_BYTES - 1).collect()
}

pub(super) fn redact_open_paths(message: &str) -> String {
    let mut redacted = String::with_capacity(message.len().min(LOG_MESSAGE_BYTES));
    let mut copied_through = 0;
    for marker in ["Opening '", "Opening \""] {
        let mut scan_from = 0;
        while let Some(relative_start) = message[scan_from..].find(marker) {
            let marker_start = scan_from + relative_start;
            let path_start = marker_start + marker.len();
            let quote = if marker.ends_with('\'') { '\'' } else { '"' };
            let Some(relative_end) = message[path_start..].find(quote) else {
                break;
            };
            let path_end = path_start + relative_end;
            if marker_start >= copied_through {
                redacted.push_str(&message[copied_through..path_start]);
                redacted.push_str("[REDACTED_PATH]");
                copied_through = path_end;
            }
            scan_from = path_end + quote.len_utf8();
        }
    }
    redacted.push_str(&message[copied_through..]);
    redacted
}

pub(super) fn redact_urls(message: &str) -> String {
    let mut redacted = String::with_capacity(message.len().min(LOG_MESSAGE_BYTES));
    let bytes = message.as_bytes();
    let mut copied_through = 0;
    let mut scan_from = 0;
    while let Some(relative_marker) = message[scan_from..].find("://") {
        let marker = scan_from + relative_marker;
        let mut scheme_start = marker;
        while scheme_start > 0 && is_scheme_byte(bytes[scheme_start - 1]) {
            scheme_start -= 1;
        }
        if scheme_start == marker || !bytes[scheme_start].is_ascii_alphabetic() {
            scan_from = marker + 3;
            continue;
        }

        let body_start = marker + 3;
        let end = message[body_start..]
            .char_indices()
            .find_map(|(offset, character)| {
                is_url_terminator(character).then_some(body_start + offset)
            })
            .unwrap_or(message.len());
        redacted.push_str(&message[copied_through..scheme_start]);
        redacted.push_str(&message[scheme_start..body_start]);
        redacted.push_str("[REDACTED]");
        copied_through = end;
        scan_from = end.max(body_start);
    }
    redacted.push_str(&message[copied_through..]);
    redacted
}

pub(super) fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-')
}

pub(super) fn is_url_terminator(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '\'' | '"' | '<' | '>' | ')' | ']' | '}' | ',' | ';'
        )
}

pub(super) fn redact_key_values(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(eq) = rest.find('=') {
        let key_start = rest[..eq]
            .rfind(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .map_or(0, |index| index + 1);
        let key = &rest[key_start..eq];
        if !is_sensitive_key(key) {
            output.push_str(&rest[..eq + 1]);
            rest = &rest[eq + 1..];
            continue;
        }
        let value_end = rest[eq + 1..]
            .find(|character: char| character.is_whitespace() || character == '&')
            .map_or(rest.len(), |offset| eq + 1 + offset);
        output.push_str(&rest[..key_start]);
        output.push_str(key);
        output.push_str("=[REDACTED]");
        rest = &rest[value_end..];
    }
    output.push_str(rest);
    output
}

pub(super) fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "token",
        "secret",
        "password",
        "authorization",
        "apikey",
        "credential",
        "sessionid",
    ]
    .iter()
    .any(|sensitive| normalized.contains(sensitive))
        || normalized == "key"
}
