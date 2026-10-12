//! `log_frontend`: lines sent by the UI (errors, rejected promises, console, React errors,
//! command errors, screen changes). Untrusted input: the event code is validated, texts are
//! bounded and scrubbed of secrets, and a rate limit protects the journal from a loop.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Map, Value};

use super::layer::truncate;
use super::redact;

/// Target of every UI line (inside the `squeezegram_lib` filter).
pub const TARGET: &str = "squeezegram_lib::front";
pub const MAX_MSG: usize = 2000;
pub const MAX_FIELDS: usize = 8 * 1024;
/// At most `MAX_PER_WINDOW` lines per `WINDOW`; the excess is counted, then reported once.
pub const WINDOW: Duration = Duration::from_secs(10);
pub const MAX_PER_WINDOW: u32 = 40;

#[derive(Debug, Clone, Deserialize)]
pub struct FrontendEntry {
    pub level: String,
    pub event: String,
    pub msg: String,
    #[serde(default)]
    pub fields: Option<Value>,
}

#[derive(Debug, Default)]
pub struct Limiter {
    window_start: Option<Instant>,
    count: u32,
    dropped: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Admit {
    /// Write it; `dropped` lines were refused in the previous window.
    Yes {
        dropped: u32,
    },
    No,
}

impl Limiter {
    pub fn admit(&mut self, now: Instant) -> Admit {
        let expired = self
            .window_start
            .is_none_or(|start| now.duration_since(start) >= WINDOW);
        let mut dropped = 0;
        if expired {
            self.window_start = Some(now);
            self.count = 0;
            dropped = std::mem::take(&mut self.dropped);
        }
        if self.count >= MAX_PER_WINDOW {
            self.dropped += 1;
            return Admit::No;
        }
        self.count += 1;
        Admit::Yes { dropped }
    }
}

static LIMITER: Mutex<Limiter> = Mutex::new(Limiter {
    window_start: None,
    count: 0,
    dropped: 0,
});

fn valid_event(name: &str) -> bool {
    (1..=48).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// What reaches the journal for one entry: event code, message, `fields` as JSON text.
pub fn sanitize(entry: &FrontendEntry) -> (String, String, String) {
    let event = if valid_event(&entry.event) {
        entry.event.clone()
    } else {
        "frontend_invalid_event".to_owned()
    };
    let msg = redact::scrub_secrets(&truncate(&entry.msg, MAX_MSG));
    let fields = match &entry.fields {
        Some(Value::Object(map)) => map.clone(),
        Some(Value::Null) | None => Map::new(),
        Some(other) => Map::from_iter([("value".to_owned(), other.clone())]),
    };
    let mut value = Value::Object(fields);
    redact::map_strings(&mut value, &|text| {
        redact::scrub_secrets(&truncate(text, MAX_MSG))
    });
    let mut json = value.to_string();
    if json.len() > MAX_FIELDS {
        json = serde_json::json!({ "fields_truncated": json.len() }).to_string();
    }
    (event, msg, json)
}

/// Writes one UI line, unless the rate limit refuses it.
pub fn record(entry: &FrontendEntry) {
    let admit = LIMITER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .admit(Instant::now());
    let Admit::Yes { dropped } = admit else {
        return;
    };
    if dropped > 0 {
        tracing::warn!(target: TARGET, event = "frontend_dropped", dropped, "UI lines dropped by the rate limit");
    }
    let (event, msg, json) = sanitize(entry);
    match entry.level.as_str() {
        "error" => {
            tracing::error!(target: TARGET, event = event.as_str(), json = json.as_str(), "{msg}")
        }
        "warn" => {
            tracing::warn!(target: TARGET, event = event.as_str(), json = json.as_str(), "{msg}")
        }
        "debug" => {
            tracing::debug!(target: TARGET, event = event.as_str(), json = json.as_str(), "{msg}")
        }
        _ => tracing::info!(target: TARGET, event = event.as_str(), json = json.as_str(), "{msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rate_limit_counts_and_reports_the_excess() {
        let mut limiter = Limiter::default();
        let start = Instant::now();
        for _ in 0..MAX_PER_WINDOW {
            assert_eq!(limiter.admit(start), Admit::Yes { dropped: 0 });
        }
        assert_eq!(limiter.admit(start), Admit::No);
        assert_eq!(limiter.admit(start + Duration::from_secs(1)), Admit::No);
        assert_eq!(
            limiter.admit(start + WINDOW),
            Admit::Yes { dropped: 2 },
            "next window reports what was dropped"
        );
        assert_eq!(limiter.admit(start + WINDOW), Admit::Yes { dropped: 0 });
    }

    #[test]
    fn entries_are_validated_bounded_and_scrubbed() {
        let entry = FrontendEntry {
            level: "error".into(),
            event: "Bad Event!".into(),
            msg: format!("phone +33612345678 {}", "x".repeat(5000)),
            fields: Some(json!({ "stack": "at tg://login?token=SECRET", "n": 1 })),
        };
        let (event, msg, json) = sanitize(&entry);
        assert_eq!(event, "frontend_invalid_event");
        assert!(msg.starts_with("phone <phone> "));
        assert!(msg.len() < 2100);
        let fields: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(fields, json!({ "stack": "at <tg_link>", "n": 1 }));

        let ok = FrontendEntry {
            level: "info".into(),
            event: "navigate".into(),
            msg: "screen".into(),
            fields: Some(json!(["not", "an", "object"])),
        };
        let (event, _, json) = sanitize(&ok);
        assert_eq!(event, "navigate");
        assert_eq!(
            serde_json::from_str::<Value>(&json).unwrap(),
            json!({ "value": ["not", "an", "object"] })
        );

        let huge = FrontendEntry {
            fields: Some(
                json!({ "a": "y".repeat(1500), "b": "y".repeat(1500), "c": "y".repeat(1500), "d": "y".repeat(1500), "e": "y".repeat(1500), "f": "y".repeat(1500) }),
            ),
            ..ok
        };
        let (_, _, json) = sanitize(&huge);
        assert!(json.contains("fields_truncated"));
    }
}
