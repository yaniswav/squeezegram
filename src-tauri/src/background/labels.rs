//! The tray's wording, read from the page's own catalogues (`src/i18n/locales`, group `tray`)
//! so there is one source of text and the tray follows `ui.language` even while the window is
//! hidden and has not loaded.

use serde_json::Value;

use super::QueueLine;

const FALLBACK: &str = "en";

/// Catalogue files by language code (`ui.language`, see `src/i18n/languages.ts`).
const CATALOGUES: [(&str, &str); 7] = [
    ("en", include_str!("../../../src/i18n/locales/en.json")),
    ("fr", include_str!("../../../src/i18n/locales/fr.json")),
    ("ru", include_str!("../../../src/i18n/locales/ru.json")),
    ("es", include_str!("../../../src/i18n/locales/es.json")),
    ("ar", include_str!("../../../src/i18n/locales/ar.json")),
    (
        "pt-BR",
        include_str!("../../../src/i18n/locales/pt-BR.json"),
    ),
    ("id", include_str!("../../../src/i18n/locales/id.json")),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labels {
    pub open: String,
    pub pause: String,
    pub resume: String,
    pub quit: String,
    idle: String,
    working: String,
    paused: String,
}

fn catalogue(language: &str) -> Option<Value> {
    let (_, text) = CATALOGUES.iter().find(|(code, _)| *code == language)?;
    serde_json::from_str(text).ok()
}

impl Labels {
    /// The wording in `language`, each missing text taken from English.
    pub fn for_language(language: &str) -> Self {
        let chosen = catalogue(language);
        let fallback = catalogue(FALLBACK);
        let text = |key: &str| -> String {
            [&chosen, &fallback]
                .into_iter()
                .flatten()
                .find_map(|catalogue| catalogue.pointer(&format!("/tray/{key}")))
                .and_then(Value::as_str)
                .unwrap_or(key)
                .to_owned()
        };
        Self {
            open: text("open"),
            pause: text("pause"),
            resume: text("resume"),
            quit: text("quit"),
            idle: text("idle"),
            working: text("working"),
            paused: text("paused"),
        }
    }

    /// Tooltip of the icon.
    pub fn tooltip(&self, line: QueueLine) -> String {
        match line {
            QueueLine::Idle => self.idle.clone(),
            QueueLine::Working(count) => self.working.replace("{{count}}", &count.to_string()),
            QueueLine::Paused(count) => self.paused.replace("{{count}}", &count.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_language_has_every_tray_text() {
        for (code, _) in CATALOGUES {
            let catalogue = catalogue(code).unwrap_or_else(|| panic!("{code} does not parse"));
            for key in [
                "open", "pause", "resume", "quit", "idle", "working", "paused",
            ] {
                assert!(
                    catalogue
                        .pointer(&format!("/tray/{key}"))
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty()),
                    "{code} lacks tray.{key}"
                );
            }
        }
    }

    #[test]
    fn the_tooltip_carries_the_count_and_an_unknown_language_falls_back_to_english() {
        let english = Labels::for_language("en");
        assert_eq!(english, Labels::for_language("xx"));
        assert!(english.tooltip(QueueLine::Working(3)).contains('3'));
        assert!(english.tooltip(QueueLine::Paused(12)).contains("12"));
        assert!(!english.tooltip(QueueLine::Idle).contains("{{"));
        assert!(!english.tooltip(QueueLine::Working(1)).contains("{{"));
    }
}
