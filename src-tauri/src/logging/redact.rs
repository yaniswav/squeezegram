//! What never leaves as is: chat identifiers (hashed everywhere), secrets in texts that come
//! from outside the Rust code (the UI), and, in `debug_export` only, user paths.

use std::path::Path;
use std::sync::OnceLock;

use serde_json::Value;

/// Per-install key of the chat hash, in the journal folder (never exported).
const KEY_FILE: &str = "chat-hash.key";
/// Used until `init_key` ran (tests): hashes stay stable, they are just not secret.
const FALLBACK_KEY: [u8; 32] = *b"squeezegram journal chat hash k0";
/// 6 hex digits: about 1 collision in 3,000 for 100 chats, and no way back to the id.
const CHAT_HEX: usize = 6;

static KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// Loads or creates the per-install key. A key that cannot be stored stays in memory: hashes
/// then change from one session to the next, which only costs cross-session comparisons.
pub fn init_key(dir: &Path) {
    let path = dir.join(KEY_FILE);
    let key = match std::fs::read(&path) {
        Ok(bytes) if bytes.len() == 32 => {
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            key
        }
        _ => {
            let mut key = [0u8; 32];
            key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            let _ = std::fs::write(&path, key);
            key
        }
    };
    let _ = KEY.set(key);
}

/// Short keyed hash of a chat id (TDLib `chat_id` or raw peer id): `c:1a2b3c`.
pub fn chat(id: i64) -> String {
    let key = KEY.get().copied().unwrap_or(FALLBACK_KEY);
    let hash = blake3::keyed_hash(&key, &id.to_le_bytes());
    format!("c:{}", &hash.to_hex()[..CHAT_HEX])
}

// ---------- secrets in free text ----------

fn is_email_local(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._%+-".contains(c)
}

fn is_email_domain(c: char) -> bool {
    c.is_ascii_alphanumeric() || ".-".contains(c)
}

/// Replaces e-mail addresses, `+`-prefixed phone numbers and `tg://` links (QR login tokens)
/// with placeholders. Applied to every text the UI sends to the journal, and to the export.
pub fn scrub_secrets(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        // tg://login?token=…, tg://… : up to whitespace or a quote.
        if chars[i..].starts_with(&['t', 'g', ':', '/', '/']) {
            let mut end = i;
            while end < chars.len() && !chars[end].is_whitespace() && !"\"'<>".contains(chars[end])
            {
                end += 1;
            }
            out.push_str("<tg_link>");
            i = end;
            continue;
        }
        // +33 6 12 34 56 78, +1-555-0100: 7 digits or more after the plus sign.
        if chars[i] == '+' {
            let mut end = i + 1;
            let mut digits = 0;
            while end < chars.len() && (chars[end].is_ascii_digit() || " -().".contains(chars[end]))
            {
                if chars[end].is_ascii_digit() {
                    digits += 1;
                }
                end += 1;
            }
            if digits >= 7 {
                while end > i + 1 && !chars[end - 1].is_ascii_digit() {
                    end -= 1;
                }
                out.push_str("<phone>");
                i = end;
                continue;
            }
        }
        if chars[i] == '@' {
            let mut start = i;
            while start > 0 && is_email_local(chars[start - 1]) {
                start -= 1;
            }
            let mut end = i + 1;
            while end < chars.len() && is_email_domain(chars[end]) {
                end += 1;
            }
            let domain: String = chars[i + 1..end].iter().collect();
            let local_len = i - start;
            if local_len > 0 && domain.trim_matches('.').contains('.') {
                // The local part was already copied: take it back.
                for _ in 0..local_len {
                    out.pop();
                }
                out.push_str("<email>");
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---------- paths (debug_export) ----------

fn is_drive_at(chars: &[char], i: usize) -> bool {
    i + 2 < chars.len()
        && chars[i].is_ascii_alphabetic()
        && chars[i + 1] == ':'
        && (chars[i + 2] == '\\' || chars[i + 2] == '/')
}

/// Length of the prefix of an absolute Windows path starting at `i` (`C:\`, `\\?\C:\`, or
/// the `\\` of a UNC path), or `None`.
fn path_prefix_at(chars: &[char], i: usize) -> Option<usize> {
    if i > 0 && chars[i - 1].is_ascii_alphanumeric() {
        return None;
    }
    if is_drive_at(chars, i) {
        return Some(3);
    }
    let unc = i + 2 < chars.len() && chars[i] == '\\' && chars[i + 1] == '\\';
    if !unc {
        return None;
    }
    if chars[i + 2] == '?' && i + 3 < chars.len() && chars[i + 3] == '\\' {
        return is_drive_at(chars, i + 4).then_some(7);
    }
    (chars[i + 2].is_alphanumeric() || chars[i + 2] == '.').then_some(2)
}

/// Characters that cannot be in a Windows path, so they end one (`:` past the drive too).
fn ends_path(c: char) -> bool {
    "\"'<>|*?\n\r\t:;,".contains(c)
}

/// `<path>/name`: the last component only; a bare profile folder (`C:\Users\name`) is hidden
/// entirely since its name is the Windows account name.
fn masked(path: &str) -> String {
    let trimmed = path.trim_end_matches([' ', '\\', '/', '.']);
    let parts: Vec<&str> = trimmed
        .split(['\\', '/'])
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() <= 3
        && parts
            .get(1)
            .is_some_and(|p| p.eq_ignore_ascii_case("users"))
    {
        return "<path>".into();
    }
    format!(
        "<path>/{}",
        parts.last().copied().unwrap_or_default().trim()
    )
}

/// Replaces every absolute path in `text` by `<path>/<last component>`. Spaces are allowed
/// inside a path, so a path followed by words keeps those words in its "name": never a folder.
pub fn mask_paths(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(prefix) = path_prefix_at(&chars, i) {
            let mut end = i + prefix;
            // A space followed by another path ends this one.
            while end < chars.len()
                && !ends_path(chars[end])
                && !(chars[end].is_whitespace() && path_prefix_at(&chars, end + 1).is_some())
            {
                end += 1;
            }
            let path: String = chars[i..end].iter().collect();
            out.push_str(&masked(&path));
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Everything `debug_export` applies to a text: paths, then secrets.
pub fn mask_text(text: &str) -> String {
    scrub_secrets(&mask_paths(text))
}

/// Applies `f` to every string of a JSON value (keys untouched).
pub fn map_strings(value: &mut Value, f: &dyn Fn(&str) -> String) {
    match value {
        Value::String(text) => *text = f(text),
        Value::Array(items) => items.iter_mut().for_each(|item| map_strings(item, f)),
        Value::Object(map) => map.values_mut().for_each(|item| map_strings(item, f)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chats_are_short_stable_hashes() {
        let a = chat(-1_001_234_567_890);
        assert_eq!(a, chat(-1_001_234_567_890));
        assert_ne!(a, chat(-1_001_234_567_891));
        assert_eq!(a.len(), 2 + CHAT_HEX);
        assert!(!a.contains("1234567890"));
    }

    #[test]
    fn secrets_are_scrubbed_from_free_text() {
        let text = "call +33 6 12 34 56 78 or +1-555-010-0199, mail alice.b@example.co.uk, \
                    scan tg://login?token=AbC123_x then 1+1=2 and @mention and x@y";
        let scrubbed = scrub_secrets(text);
        assert_eq!(
            scrubbed,
            "call <phone> or <phone>, mail <email>, scan <tg_link> then 1+1=2 and @mention and x@y"
        );
    }

    #[test]
    fn paths_keep_only_their_last_component() {
        let cases = [
            (
                "C:\\Users\\Alice\\Videos\\clip one.mp4",
                "<path>/clip one.mp4",
            ),
            (
                "open C:/Users/Alice/a.mkv: No such file",
                "open <path>/a.mkv: No such file",
            ),
            (
                "\\\\nas\\share\\x.mp4 and D:\\tmp\\1\\",
                "<path>/x.mp4 and <path>/1",
            ),
            ("\\\\?\\C:\\long\\b.mp4", "<path>/b.mp4"),
            ("-i \"C:\\in put\\a.mkv\" -y", "-i \"<path>/a.mkv\" -y"),
            ("ratio 16:9, h264", "ratio 16:9, h264"),
            ("http://x/y", "http://x/y"),
        ];
        for (text, expected) in cases {
            assert_eq!(mask_paths(text), expected, "{text}");
        }
    }

    #[test]
    fn every_string_of_a_value_is_masked() {
        let mut value = json!({
            "args": ["-i", "C:\\Users\\Alice\\a.mp4"],
            "nested": { "dir": "C:\\Users\\Alice" },
            "count": 3
        });
        map_strings(&mut value, &mask_text);
        assert_eq!(
            value,
            json!({ "args": ["-i", "<path>/a.mp4"], "nested": { "dir": "<path>" }, "count": 3 })
        );
    }
}
