//! The `tracing` layer that turns events into journal lines.
//!
//! Field conventions of the events:
//! - `event` (snake_case code), the message (`msg`), `item_id`, `destination_id`, `dur_ms` and
//!   `error_code` become top-level keys; every other field goes into `fields`;
//! - a field named `json` holding a JSON object is merged into `fields` (lists, nested data);
//! - `item_id` and `destination_id` are inherited from the enclosing spans (the orchestrator
//!   runs each item inside an `item` span).

use std::cell::Cell;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::NaiveDate;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record as SpanValues};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use super::file::RollingFile;
use super::record::Record;

/// Longest text kept in one field or message (an ffmpeg stderr tail is a few KB).
pub const MAX_TEXT: usize = 16 * 1024;
/// Longest `json` field merged into `fields`.
pub const MAX_JSON: usize = 64 * 1024;
pub const JSON_FIELD: &str = "json";
/// `event` of a line whose code was forgotten (or of a dependency's event).
pub const FALLBACK_EVENT: &str = "log";

thread_local! {
    /// Set while this thread writes a line: an event (or panic) raised while writing is
    /// dropped instead of deadlocking on the file.
    static WRITING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated {} bytes]", &text[..end], text.len() - end)
}

/// Shared by the layer and the lines written outside `tracing` (`app_start`, `panic`).
#[derive(Clone)]
pub struct Output {
    file: Arc<Mutex<RollingFile>>,
    mirror_stderr: bool,
}

struct WritingGuard;

impl Drop for WritingGuard {
    fn drop(&mut self) {
        WRITING.with(|w| w.set(false));
    }
}

impl Output {
    pub fn new(file: RollingFile, mirror_stderr: bool) -> Self {
        Self {
            file: Arc::new(Mutex::new(file)),
            mirror_stderr,
        }
    }

    fn lock(&self) -> MutexGuard<'_, RollingFile> {
        self.file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes one line; `false` when this thread is already writing (re-entrant call).
    pub fn write(&self, date: NaiveDate, line: &str) -> bool {
        if WRITING.with(|w| w.replace(true)) {
            return false;
        }
        let _guard = WritingGuard;
        if self.mirror_stderr {
            eprintln!("{line}");
        }
        if let Err(error) = self.lock().write_line(date, line) {
            eprintln!("journal: cannot write: {error}");
        }
        true
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct SpanIds {
    item_id: Option<i64>,
    destination_id: Option<i64>,
}

impl Visit for SpanIds {
    fn record_i64(&mut self, field: &Field, value: i64) {
        match field.name() {
            "item_id" => self.item_id = Some(value),
            "destination_id" => self.destination_id = Some(value),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_i64(field, i64::try_from(value).unwrap_or(i64::MAX));
    }

    fn record_debug(&mut self, _: &Field, _: &dyn fmt::Debug) {}
}

#[derive(Default)]
struct Fields {
    event: Option<String>,
    msg: Option<String>,
    item_id: Option<i64>,
    destination_id: Option<i64>,
    dur_ms: Option<u64>,
    error_code: Option<String>,
    fields: Map<String, Value>,
}

impl Fields {
    fn put(&mut self, name: &str, value: Value) {
        self.fields.insert(name.to_owned(), value);
    }

    fn text(&mut self, name: &str, value: String) {
        match name {
            "message" => self.msg = Some(truncate(&value, MAX_TEXT)),
            "event" => self.event = Some(truncate(&value, 64)),
            "error_code" => self.error_code = Some(truncate(&value, 64)),
            JSON_FIELD if value.len() <= MAX_JSON => match serde_json::from_str(&value) {
                Ok(Value::Object(map)) => self.fields.extend(map),
                _ => self.put(name, Value::String(truncate(&value, MAX_TEXT))),
            },
            _ => self.put(name, Value::String(truncate(&value, MAX_TEXT))),
        }
    }

    fn integer(&mut self, name: &str, value: i64) {
        match name {
            "item_id" => self.item_id = Some(value),
            "destination_id" => self.destination_id = Some(value),
            "dur_ms" => self.dur_ms = Some(u64::try_from(value).unwrap_or(0)),
            _ => self.put(name, value.into()),
        }
    }
}

impl Visit for Fields {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.integer(field.name(), value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match i64::try_from(value) {
            Ok(value) => self.integer(field.name(), value),
            Err(_) => self.put(field.name(), value.into()),
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        let number = serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number);
        self.put(field.name(), number);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field.name(), value.into());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.text(field.name(), value.to_owned());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let mut text = value.to_string();
        let mut source = value.source();
        while let Some(cause) = source {
            text.push_str(": ");
            text.push_str(&cause.to_string());
            source = cause.source();
        }
        self.text(field.name(), text);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.text(field.name(), format!("{value:?}"));
    }
}

/// Writes every enabled event as one JSON line.
pub struct JsonLayer {
    session_id: String,
    output: Output,
}

impl JsonLayer {
    pub fn new(session_id: String, output: Output) -> Self {
        Self { session_id, output }
    }
}

impl<S> Layer<S> for JsonLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut ids = SpanIds::default();
        attrs.record(&mut ids);
        if (ids.item_id.is_some() || ids.destination_id.is_some())
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().replace(ids);
        }
    }

    fn on_record(&self, id: &Id, values: &SpanValues<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            let mut ids = extensions.get_mut::<SpanIds>().copied().unwrap_or_default();
            values.record(&mut ids);
            extensions.replace(ids);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if WRITING.with(Cell::get) {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if (fields.item_id.is_none() || fields.destination_id.is_none())
            && let Some(scope) = ctx.event_scope(event)
        {
            for span in scope {
                if let Some(ids) = span.extensions().get::<SpanIds>() {
                    fields.item_id = fields.item_id.or(ids.item_id);
                    fields.destination_id = fields.destination_id.or(ids.destination_id);
                }
            }
        }
        let now = chrono::Utc::now();
        let metadata = event.metadata();
        let record = Record {
            ts: crate::queue::iso(now),
            level: metadata.level().as_str().to_ascii_lowercase(),
            target: metadata.target().to_owned(),
            event: fields.event.unwrap_or_else(|| FALLBACK_EVENT.to_owned()),
            msg: fields.msg.unwrap_or_default(),
            session_id: self.session_id.clone(),
            item_id: fields.item_id,
            destination_id: fields.destination_id,
            dur_ms: fields.dur_ms,
            error_code: fields.error_code,
            fields: fields.fields,
        };
        self.output.write(now.date_naive(), &record.to_line());
    }
}
