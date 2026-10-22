//! What never leaves as is: chat identifiers (hashed everywhere), secrets in texts that come
//! from outside the Rust code (the UI), and, in `debug_export` only, user paths.

use std::cell::RefCell;
use std::collections::HashMap;
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

/// Length of a Telegram `api_hash`: 32 hexadecimal digits.
const API_HASH_HEX: usize = 32;

/// Whether `chars[i..]` starts a word of exactly `API_HASH_HEX` hexadecimal digits (not part of
/// a longer word: a SHA-256 or a BLAKE3 hash, 64 digits, stays).
fn is_hex32_at(chars: &[char], i: usize) -> bool {
    let alnum = |c: &char| c.is_ascii_alphanumeric();
    if i > 0 && alnum(&chars[i - 1]) {
        return false;
    }
    let word = chars[i..].iter().take_while(|c| alnum(c)).count();
    word == API_HASH_HEX
        && chars[i..i + API_HASH_HEX]
            .iter()
            .all(char::is_ascii_hexdigit)
}

/// Shared proxy links carry the proxy's secret or password in their query.
const PROXY_LINKS: [&str; 2] = ["t.me/proxy?", "t.me/socks?"];

fn starts_with_text(chars: &[char], text: &str) -> bool {
    chars.len() >= text.chars().count() && text.chars().zip(chars).all(|(a, b)| a == *b)
}

/// End of a link starting at `i`: whitespace or a quote.
fn link_end(chars: &[char], i: usize) -> usize {
    let mut end = i;
    while end < chars.len() && !chars[end].is_whitespace() && !"\"'<>".contains(chars[end]) {
        end += 1;
    }
    end
}

fn is_base64url(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_-".contains(c)
}

/// Phone numbers written without `+` in a national layout (`d` is a digit, the rest literal):
/// fixed layouts only, so ids, dates, versions and addresses are left alone.
const PHONE_SHAPES: [&str; 5] = [
    "0d dd dd dd dd",
    "0d.dd.dd.dd.dd",
    "0d-dd-dd-dd-dd",
    "ddd-ddd-dddd",
    "(ddd) ddd-dddd",
];

/// Length of the national-layout phone number starting at `i` (a whole word), or `None`.
fn phone_shape_at(chars: &[char], i: usize) -> Option<usize> {
    if i > 0 && chars[i - 1].is_alphanumeric() {
        return None;
    }
    PHONE_SHAPES.iter().find_map(|shape| {
        let n = shape.chars().count();
        let same = i + n <= chars.len()
            && shape.chars().zip(&chars[i..]).all(|(p, c)| {
                if p == 'd' {
                    c.is_ascii_digit()
                } else {
                    p == *c
                }
            });
        let whole = !matches!(chars.get(i + n), Some(c) if c.is_alphanumeric());
        (same && whole).then_some(n)
    })
}

/// Length of a bot token (`123456789:AA…`, 30 or more secret characters) starting at `i`.
fn bot_token_at(chars: &[char], i: usize) -> Option<usize> {
    if !chars[i].is_ascii_digit() || (i > 0 && chars[i - 1].is_ascii_digit()) {
        return None;
    }
    let digits = chars[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    if !(5..=12).contains(&digits) || chars.get(i + digits) != Some(&':') {
        return None;
    }
    let secret = chars[i + digits + 1..]
        .iter()
        .take_while(|c| is_base64url(**c))
        .count();
    (secret >= 30).then_some(digits + 1 + secret)
}

/// Replaces e-mail addresses, phone numbers, `SQZ1` license keys, bot tokens, `tg://` links
/// (QR login tokens, proxies), `t.me` proxy links and 32-hex-digit words (the shape of an
/// `api_hash`) with placeholders. Applied to every text the UI sends to the journal, and to the export.
pub fn scrub_secrets(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        // A personal `api_hash` pasted somewhere the UI logs: the page never logs it on
        // purpose, this is the net under that rule.
        if is_hex32_at(&chars, i) {
            out.push_str("<hex32>");
            i += API_HASH_HEX;
            continue;
        }
        // `SQZ1.<payload>.<signature>`: a license key, base64url parts joined by dots.
        if starts_with_text(&chars[i..], "SQZ1.") && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            let mut end = i + 5;
            while end < chars.len() && (is_base64url(chars[end]) || chars[end] == '.') {
                end += 1;
            }
            while chars[end - 1] == '.' {
                end -= 1;
            }
            if end > i + 5 {
                out.push_str("<license>");
                i = end;
                continue;
            }
        }
        if let Some(len) = bot_token_at(&chars, i) {
            out.push_str("<bot_token>");
            i += len;
            continue;
        }
        if let Some(len) = phone_shape_at(&chars, i) {
            out.push_str("<phone>");
            i += len;
            continue;
        }
        // tg://login?token=…, tg://… : up to whitespace or a quote.
        if chars[i..].starts_with(&['t', 'g', ':', '/', '/']) {
            out.push_str("<tg_link>");
            i = link_end(&chars, i);
            continue;
        }
        // https://t.me/proxy?server=…&secret=…: the scheme was already copied, taken back.
        if PROXY_LINKS
            .iter()
            .any(|link| starts_with_text(&chars[i..], link))
        {
            for scheme in ["https://", "http://"] {
                if let Some(kept) = out.strip_suffix(scheme) {
                    out.truncate(kept.len());
                    break;
                }
            }
            out.push_str("<proxy_link>");
            i = link_end(&chars, i);
            continue;
        }
        // +33 6 12 34 56 78, 0033 6 12 34 56 78: 7 digits or more after the `+` or the `00`.
        let prefix = if chars[i] == '+' {
            1
        } else if starts_with_text(&chars[i..], "00") && (i == 0 || !chars[i - 1].is_alphanumeric())
        {
            2
        } else {
            0
        };
        if prefix > 0 {
            let mut end = i + prefix;
            let mut digits = 0;
            while end < chars.len() && (chars[end].is_ascii_digit() || " -().".contains(chars[end]))
            {
                if chars[end].is_ascii_digit() {
                    digits += 1;
                }
                end += 1;
            }
            if digits >= 7 {
                while end > i + prefix && !chars[end - 1].is_ascii_digit() {
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
    "\"<>|*?\n\r\t:".contains(c)
}

/// Valid in a Windows name (`Clients,Secret\a.mp4`), so they end a path only when no folder
/// separator follows before the next character that cannot be in a path.
fn ends_soft(chars: &[char], end: usize) -> bool {
    ",;'".contains(chars[end])
        && !chars[end + 1..]
            .iter()
            .take_while(|c| !ends_path(**c))
            .any(|c| *c == '\\' || *c == '/')
}

/// Extensions kept on a masked file name: media types and `.part`, nothing that names anyone.
const KEPT_EXTENSIONS: [&str; 20] = [
    "mp4", "mkv", "mov", "webm", "avi", "m4v", "ts", "mp3", "m4a", "ogg", "wav", "flac", "jpg",
    "jpeg", "png", "gif", "webp", "heic", "part", "exe",
];

/// The file names of one export: a name always gets the same marker, so support can follow one
/// file across lines, and the next export starts again at `file-1`.
#[derive(Default)]
pub struct Masker {
    names: RefCell<HashMap<String, String>>,
}

impl Masker {
    fn marker(&self, name: &str) -> String {
        let mut names = self.names.borrow_mut();
        let next = names.len() + 1;
        let key = name.to_lowercase();
        names
            .entry(key.clone())
            .or_insert_with(|| {
                let extension = Path::new(&key)
                    .extension()
                    .and_then(|e| e.to_str())
                    .filter(|e| KEPT_EXTENSIONS.contains(e));
                match extension {
                    Some(e) => format!("file-{next}.{e}"),
                    None => format!("file-{next}"),
                }
            })
            .clone()
    }

    /// `<path>/file-N`: no folder, the last component as a marker; a bare profile folder
    /// (`C:\Users\name`) is hidden entirely since its name is the Windows account name.
    fn masked(&self, path: &str) -> String {
        let trimmed = path.trim_end_matches([' ', '\\', '/', '.']);
        let parts: Vec<&str> = trimmed
            .split(['\\', '/'])
            .filter(|p| !p.is_empty())
            .collect();
        let name = parts.last().copied().unwrap_or_default().trim();
        if name.is_empty()
            || (parts.len() <= 3
                && parts
                    .get(1)
                    .is_some_and(|p| p.eq_ignore_ascii_case("users")))
        {
            return "<path>".into();
        }
        format!("<path>/{}", self.marker(name))
    }

    /// Replaces every absolute path in `text` by `<path>/file-N`. Spaces are allowed inside a
    /// path, so a path followed by words keeps those words in its "name": never a folder.
    pub fn mask_paths(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < chars.len() {
            if let Some(prefix) = path_prefix_at(&chars, i) {
                let mut end = i + prefix;
                // A space followed by another path ends this one.
                while end < chars.len()
                    && !ends_path(chars[end])
                    && !ends_soft(&chars, end)
                    && !(chars[end].is_whitespace() && path_prefix_at(&chars, end + 1).is_some())
                {
                    end += 1;
                }
                let path: String = chars[i..end].iter().collect();
                out.push_str(&self.masked(&path));
                i = end;
                continue;
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    /// Everything `debug_export` applies to a text: paths, then secrets.
    pub fn mask_text(&self, text: &str) -> String {
        scrub_secrets(&self.mask_paths(text))
    }
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
    fn a_32_hex_word_is_scrubbed_but_longer_hashes_stay() {
        let hash = "0123456789abcdefABCDEF0123456789";
        let sha256 = "ab".repeat(32);
        let text = format!(
            "api_hash={hash}; \"{hash}\" x{hash} {sha256} {}",
            &hash[..31]
        );
        assert_eq!(
            scrub_secrets(&text),
            format!(
                "api_hash=<hex32>; \"<hex32>\" x{hash} {sha256} {}",
                &hash[..31]
            )
        );
    }

    #[test]
    fn proxy_links_are_scrubbed_with_their_secret() {
        let text = "use https://t.me/proxy?server=1.2.3.4&port=443&secret=ee00ff \
                    or t.me/socks?server=h&user=u&pass=p, tg://proxy?secret=dd11 \
                    but keep https://t.me/somechannel";
        assert_eq!(
            scrub_secrets(text),
            "use <proxy_link> or <proxy_link> <tg_link> but keep https://t.me/somechannel"
        );
    }

    #[test]
    fn paths_keep_only_a_marker_of_their_last_component() {
        let cases = [
            (
                "C:\\Users\\Alice\\Videos\\clip one.mp4",
                "<path>/file-1.mp4",
            ),
            (
                "open C:/Users/Alice/a.mkv: No such file",
                "open <path>/file-1.mkv: No such file",
            ),
            (
                "\\\\nas\\share\\x.mp4|D:\\tmp\\1\\",
                "<path>/file-1.mp4|<path>/file-2",
            ),
            ("\\\\?\\C:\\long\\b.mp4", "<path>/file-1.mp4"),
            ("-i \"C:\\in put\\a.mkv\" -y", "-i \"<path>/file-1.mkv\" -y"),
            ("C:\\Users\\Alice", "<path>"),
            ("ratio 16:9, h264", "ratio 16:9, h264"),
            ("http://x/y", "http://x/y"),
        ];
        for (text, expected) in cases {
            assert_eq!(Masker::default().mask_paths(text), expected, "{text}");
        }
    }

    #[test]
    fn a_name_has_one_marker_for_the_whole_archive_and_unknown_extensions_go() {
        let masker = Masker::default();
        let first = masker.mask_text("C:\\Alice\\Wedding Jones.MP4|D:\\x\\secret.acme");
        assert_eq!(first, "<path>/file-1.mp4|<path>/file-2");
        assert_eq!(
            masker.mask_text("E:\\other\\wedding jones.mp4|C:\\y\\Other.mov"),
            "<path>/file-1.mp4|<path>/file-3.mov"
        );
    }

    #[test]
    fn commas_semicolons_and_apostrophes_in_folder_names_do_not_end_the_path() {
        let masker = Masker::default();
        let cases = [
            "C:\\Users\\Alice\\Clients,Confidentiel\\dossier\\clip.mp4",
            "C:\\Work\\Clients;Secret\\dossier\\clip.mp4",
            "C:\\Work\\O'Brien Films\\dossier\\clip.mp4",
            "C:\\Work\\Clients, Confidentiel Inc\\dossier\\clip.mp4",
            "failed on C:\\Work\\Clients,Secret\\dossier\\clip.mp4",
        ];
        for text in cases {
            let masked = masker.mask_text(text);
            for word in [
                "Confidentiel",
                "Secret",
                "Brien",
                "dossier",
                "Alice",
                "Work",
            ] {
                assert!(!masked.contains(word), "{text} -> {masked}");
            }
            assert!(masked.ends_with("<path>/file-1.mp4"), "{masked}");
        }
        // The same characters still end a path when they are punctuation around it.
        assert_eq!(
            Masker::default().mask_text("'C:\\a\\b.mp4', C:\\c\\d.mp4; then 'E:\\f\\g.mp4'"),
            "'<path>/file-1.mp4', <path>/file-2.mp4; then '<path>/file-3.mp4'"
        );
    }

    #[test]
    fn json_separators_end_a_path_inside_a_string() {
        let masker = Masker::default();
        let mut value = json!({
            "list": ["C:\\Clients,Secret\\dossier\\a.mp4", "D:\\x;y\\b.mkv"],
            "text": "{\"in\":\"C:\\\\Clients,Secret\\\\dossier\\\\a.mp4\",\"n\":3}"
        });
        map_strings(&mut value, &|text| masker.mask_text(text));
        assert_eq!(
            value,
            json!({
                "list": ["<path>/file-1.mp4", "<path>/file-2.mkv"],
                "text": "{\"in\":\"<path>/file-1.mp4\",\"n\":3}"
            })
        );
    }

    #[test]
    fn licenses_bot_tokens_and_phones_without_a_plus_are_scrubbed() {
        let text = "key SQZ1.eyJ2IjoxfQ_-abc.c2ln-_xyz. then \
                    https://api.telegram.org/bot123456789:AAHdummyDummyDummyDummyDummyDummy12/getMe, \
                    id 987654321:ZZdummyDummyDummyDummyDummyDummyDum, \
                    call 06 12 34 56 78 or 06.12.34.56.78 or 555-010-0199 or (555) 010-0199 \
                    or 0033 6 12 34 56 78 or 0033612345678.";
        assert_eq!(
            scrub_secrets(text),
            "key <license>. then https://api.telegram.org/bot<bot_token>/getMe, \
             id <bot_token>, call <phone> or <phone> or <phone> or <phone> \
             or <phone> or <phone>."
        );
    }

    #[test]
    fn numbers_that_are_not_secrets_stay() {
        let text = "SQZ1 and SQZ1. alone, xSQZ1.abc, at 12:34567 size 1234567890123 \
                    ip 192.168.1.100 date 2026-10-09 07:12:01 v1.2.3 \
                    token 123456789:short 4 5 6 0612345678 100-200-3000x";
        assert_eq!(scrub_secrets(text), text);
    }

    #[test]
    fn every_string_of_a_value_is_masked() {
        let masker = Masker::default();
        let mut value = json!({
            "args": ["-i", "C:\\Users\\Alice\\a.mp4"],
            "nested": { "dir": "C:\\Users\\Alice" },
            "count": 3
        });
        map_strings(&mut value, &|text| masker.mask_text(text));
        assert_eq!(
            value,
            json!({ "args": ["-i", "<path>/file-1.mp4"], "nested": { "dir": "<path>" }, "count": 3 })
        );
    }
}
