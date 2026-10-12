//! Minimal `.env` reading shared by `build.rs` (through `#[path]`) and the unit tests: the
//! product's Telegram identity (`SQUEEZEGRAM_TG_API_ID`, `SQUEEZEGRAM_TG_API_HASH`) is injected
//! at build time and never lives in the repository.
//!
//! Format: one `KEY=VALUE` per line; blank lines and lines starting with `#` are ignored; an
//! optional `export ` prefix and one pair of matching quotes around the value are removed.
//! Nothing here prints anything: the values are secrets.

/// The value of `key` in a `.env` text: the last assignment wins, as in a shell.
pub fn lookup(text: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() == key {
            found = Some(unquote(value.trim()).to_owned());
        }
    }
    found
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// What `build.rs` injects for `key`: the environment variable wins over the `.env` file; a
/// blank value or one holding a control character (it would break `cargo:rustc-env`) is
/// ignored. Validation of the content (digits, hex) is done at run time.
pub fn resolve(key: &str, from_env: Option<&str>, dotenv_text: Option<&str>) -> Option<String> {
    let usable = |value: &str| {
        let value = value.trim();
        (!value.is_empty() && !value.chars().any(char::is_control)).then(|| value.to_owned())
    };
    from_env.and_then(usable).or_else(|| {
        dotenv_text
            .and_then(|text| lookup(text, key))
            .as_deref()
            .and_then(usable)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fake values only: the real identity is never in a test.
    const FAKE: &str = "# product identity (fake)\n\
        \n\
        SQUEEZEGRAM_TG_API_ID=11223344\n\
        export SQUEEZEGRAM_TG_API_HASH = \"00112233445566778899aabbccddeeff\"\n\
        #SQUEEZEGRAM_TG_API_ID=99999999\n\
        NOT_A_PAIR\n\
        OTHER='x=y'\n\
        EMPTY=\n";

    #[test]
    fn reads_pairs_and_ignores_comments() {
        assert_eq!(
            lookup(FAKE, "SQUEEZEGRAM_TG_API_ID").as_deref(),
            Some("11223344")
        );
        assert_eq!(
            lookup(FAKE, "SQUEEZEGRAM_TG_API_HASH").as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
        assert_eq!(lookup(FAKE, "OTHER").as_deref(), Some("x=y"));
        assert_eq!(lookup(FAKE, "EMPTY").as_deref(), Some(""));
        assert_eq!(lookup(FAKE, "NOT_A_PAIR"), None);
        assert_eq!(lookup(FAKE, "MISSING"), None);
        assert_eq!(
            lookup("A=1\r\nA=2\r\n", "A").as_deref(),
            Some("2"),
            "last wins"
        );
    }

    #[test]
    fn the_environment_wins_and_blanks_are_ignored() {
        let key = "SQUEEZEGRAM_TG_API_ID";
        assert_eq!(
            resolve(key, Some("55667788"), Some(FAKE)).as_deref(),
            Some("55667788")
        );
        assert_eq!(resolve(key, None, Some(FAKE)).as_deref(), Some("11223344"));
        assert_eq!(
            resolve(key, Some("  "), Some(FAKE)).as_deref(),
            Some("11223344")
        );
        assert_eq!(resolve(key, None, None), None);
        assert_eq!(resolve("EMPTY", None, Some(FAKE)), None);
        assert_eq!(
            resolve(key, Some("1\n2"), None),
            None,
            "would break cargo:rustc-env"
        );
    }
}
