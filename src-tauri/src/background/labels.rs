//! The tray's wording, read from the page's own catalogues (`src/i18n/locales`, group `tray`)
//! so there is one source of text and the tray follows `ui.language` even while the window is
//! hidden and has not loaded.

use serde_json::Value;

use super::QueueLine;
use super::progress::{Line, Status};

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
    status_compressing: String,
    status_sending: String,
    status_paused: String,
    percent: String,
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
            status_compressing: text("statusCompressing"),
            status_sending: text("statusSending"),
            status_paused: text("statusPaused"),
            percent: text("percent"),
        }
    }

    /// A whole percent as the language writes it (French puts a space before the sign).
    fn percent(&self, value: u8) -> String {
        self.percent.replace("{{value}}", &value.to_string())
    }

    /// Tooltip of the icon, with the overall percent while work runs.
    pub fn tooltip(&self, line: QueueLine, percent: Option<u8>) -> String {
        let text = match line {
            QueueLine::Idle => self.idle.clone(),
            QueueLine::Working(count) => self.working.replace("{{count}}", &count.to_string()),
            QueueLine::Paused(count) => self.paused.replace("{{count}}", &count.to_string()),
        };
        match percent {
            Some(value) => format!("{text} · {}", self.percent(value)),
            None => text,
        }
    }

    /// The disabled line at the top of the menu; none when nothing is in progress.
    pub fn status_line(&self, status: &Status) -> Option<String> {
        let template = match status.line {
            Line::Idle => return None,
            Line::Compressing => &self.status_compressing,
            Line::Sending => &self.status_sending,
            Line::Paused => &self.status_paused,
        };
        Some(
            template
                .replace("{{count}}", &status.count.to_string())
                .replace("{{percent}}", &self.percent(status.percent)),
        )
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
                "open",
                "pause",
                "resume",
                "quit",
                "idle",
                "working",
                "paused",
                "statusCompressing",
                "statusSending",
                "statusPaused",
                "percent",
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
        assert!(english.tooltip(QueueLine::Working(3), None).contains('3'));
        assert!(english.tooltip(QueueLine::Paused(12), None).contains("12"));
        assert!(!english.tooltip(QueueLine::Idle, None).contains("{{"));
        assert!(!english.tooltip(QueueLine::Working(1), None).contains("{{"));
    }

    fn status(line: Line, count: usize, percent: u8) -> Status {
        Status {
            line,
            count,
            percent,
        }
    }

    #[test]
    fn the_status_line_names_the_work_the_count_and_the_percent() {
        let english = Labels::for_language("en");
        assert_eq!(
            english.status_line(&status(Line::Sending, 3, 47)).unwrap(),
            "Sending · 3 in queue · 47%"
        );
        assert_eq!(
            english
                .status_line(&status(Line::Compressing, 1, 0))
                .unwrap(),
            "Compressing · 1 in queue · 0%"
        );
        assert_eq!(
            english.status_line(&status(Line::Paused, 3, 47)).unwrap(),
            "Paused · 3 in queue"
        );
        assert_eq!(english.status_line(&status(Line::Idle, 0, 0)), None);
    }

    #[test]
    fn french_puts_a_non_breaking_space_before_the_percent_sign() {
        let french = Labels::for_language("fr");
        assert_eq!(
            french.status_line(&status(Line::Sending, 3, 47)).unwrap(),
            "Envoi · 3 dans la file · 47\u{a0}%"
        );
        assert_eq!(
            french.tooltip(QueueLine::Working(3), Some(47)),
            "squeezegram : 3 dans la file · 47\u{a0}%"
        );
    }

    #[test]
    fn no_language_leaves_a_placeholder_in_the_status_line() {
        for (code, _) in CATALOGUES {
            let labels = Labels::for_language(code);
            for line in [Line::Compressing, Line::Sending, Line::Paused] {
                let text = labels.status_line(&status(line, 12, 34)).unwrap();
                assert!(!text.contains("{{"), "{code}: {text}");
                assert!(text.contains("12"), "{code}: {text}");
            }
            let tooltip = labels.tooltip(QueueLine::Working(12), Some(34));
            assert!(tooltip.contains("34") && !tooltip.contains("{{"), "{code}");
        }
    }
}
