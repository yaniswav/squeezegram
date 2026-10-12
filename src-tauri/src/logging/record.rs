//! One journal line: fixed top-level keys in a fixed order, then `fields`.
//!
//! Written by hand rather than through `serde_json::Map` (sorted keys): `ts` first keeps the
//! file readable and every line in the same order, also after the masking of `debug_export`.

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Record {
    /// UTC, ISO 8601 with milliseconds (`2026-10-09T07:12:01.123Z`).
    pub ts: String,
    /// `error`, `warn`, `info`, `debug` or `trace`.
    pub level: String,
    /// Rust module path (`squeezegram_lib::queue::state`), `squeezegram_lib::front` for the UI.
    pub target: String,
    /// Stable snake_case code of the event (`item_state`, `command`…).
    pub event: String,
    pub msg: String,
    pub session_id: String,
    pub item_id: Option<i64>,
    pub destination_id: Option<i64>,
    pub dur_ms: Option<u64>,
    /// Stable error code (`FFMPEG_FAILED`, `TG_FLOOD_WAIT`…).
    pub error_code: Option<String>,
    pub fields: Map<String, Value>,
}

fn push_key(out: &mut String, key: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
}

fn push_text(out: &mut String, key: &str, value: &str) {
    push_key(out, key);
    // Serializing a `str` cannot fail.
    out.push_str(&serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()));
}

impl Record {
    /// The JSON line, without the trailing newline.
    pub fn to_line(&self) -> String {
        let mut out = String::with_capacity(256);
        out.push('{');
        push_text(&mut out, "ts", &self.ts);
        out.push(',');
        push_text(&mut out, "level", &self.level);
        out.push(',');
        push_text(&mut out, "target", &self.target);
        out.push(',');
        push_text(&mut out, "event", &self.event);
        out.push(',');
        push_text(&mut out, "msg", &self.msg);
        out.push(',');
        push_text(&mut out, "session_id", &self.session_id);
        if let Some(id) = self.item_id {
            out.push(',');
            push_key(&mut out, "item_id");
            out.push_str(&id.to_string());
        }
        if let Some(id) = self.destination_id {
            out.push(',');
            push_key(&mut out, "destination_id");
            out.push_str(&id.to_string());
        }
        if let Some(ms) = self.dur_ms {
            out.push(',');
            push_key(&mut out, "dur_ms");
            out.push_str(&ms.to_string());
        }
        if let Some(code) = &self.error_code {
            out.push(',');
            push_text(&mut out, "error_code", code);
        }
        out.push(',');
        push_key(&mut out, "fields");
        out.push_str(&serde_json::to_string(&self.fields).unwrap_or_else(|_| "{}".into()));
        out.push('}');
        out
    }

    /// Reads a line back (`debug_export`). `None` when a mandatory key is missing.
    pub fn from_value(value: Value) -> Option<Self> {
        let Value::Object(mut map) = value else {
            return None;
        };
        let mut text = |key: &str| match map.remove(key) {
            Some(Value::String(text)) => Some(text),
            _ => None,
        };
        let ts = text("ts")?;
        let level = text("level")?;
        let event = text("event")?;
        let target = text("target").unwrap_or_default();
        let msg = text("msg").unwrap_or_default();
        let session_id = text("session_id").unwrap_or_default();
        let error_code = text("error_code");
        Some(Self {
            ts,
            level,
            target,
            event,
            msg,
            session_id,
            item_id: map.remove("item_id").and_then(|v| v.as_i64()),
            destination_id: map.remove("destination_id").and_then(|v| v.as_i64()),
            dur_ms: map.remove("dur_ms").and_then(|v| v.as_u64()),
            error_code,
            fields: match map.remove("fields") {
                Some(Value::Object(fields)) => fields,
                _ => Map::new(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn line_keeps_the_key_order_and_round_trips() {
        let mut fields = Map::new();
        fields.insert("from".into(), json!("ready"));
        fields.insert("path".into(), json!("C:\\a \"b\".mp4"));
        let record = Record {
            ts: "2026-10-09T07:12:01.123Z".into(),
            level: "info".into(),
            target: "squeezegram_lib::queue::state".into(),
            event: "item_state".into(),
            msg: "line\nbreak".into(),
            session_id: "s".into(),
            item_id: Some(12),
            destination_id: None,
            dur_ms: Some(5),
            error_code: Some("FFMPEG_FAILED".into()),
            fields,
        };
        let line = record.to_line();
        assert!(line.starts_with(
            r#"{"ts":"2026-10-09T07:12:01.123Z","level":"info","target":"squeezegram_lib::queue::state","event":"item_state","msg":"line\nbreak","session_id":"s","item_id":12,"dur_ms":5,"error_code":"FFMPEG_FAILED","fields":{"#
        ));
        assert!(!line.contains('\n'), "one physical line");
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(Record::from_value(parsed).unwrap(), record);
        assert_eq!(Record::from_value(json!({ "ts": "x" })), None);
    }
}
