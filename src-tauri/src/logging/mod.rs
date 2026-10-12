//! Structured journal of the app: JSON Lines in
//! `%LOCALAPPDATA%\squeezegram\logs\squeezegram-YYYY-MM-DD.jsonl`, one `session_id` per run,
//! read directly to diagnose a problem.
//!
//! - `record`: the line format; `layer`: the `tracing` layer that writes it;
//! - `file`: daily files, size cap, retention (7 days, 50 MB);
//! - `redact`: chat hashes, secret scrubbing, path masking (export);
//! - `system`: versions for `app_start` and the export; `export`: `debug_export`;
//! - `frontend`: lines sent by the UI (`log_frontend`).
//!
//! Privacy rules: no phone number, login code, 2FA password, e-mail,
//! `api_hash`, database key, QR link, caption text or token is ever passed to an event; chat
//! ids go through `redact::chat`. User paths are written (local diagnosis) and masked by
//! `debug_export`.

pub mod export;
pub mod file;
pub mod frontend;
pub mod layer;
pub mod record;
pub mod redact;
pub mod system;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde_json::{Map, Value, json};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Registry, reload};

use file::{Retention, RollingFile};
use layer::{JsonLayer, Output};
use record::Record;

pub const LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

/// `info` in a release, `debug` in a debug build.
pub fn default_level() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "info"
    }
}

pub fn is_level(text: &str) -> bool {
    LEVELS.contains(&text)
}

/// `%LOCALAPPDATA%\squeezegram\logs`.
pub fn default_dir() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("squeezegram").join("logs"))
}

/// Our crates at `level`, dependencies at `warn` at most.
pub fn targets_for(level: &str) -> Targets {
    let level = if is_level(level) { level } else { "info" };
    let dependencies = if level == "error" { "error" } else { "warn" };
    format!("{dependencies},squeezegram_lib={level},squeezegram={level}")
        .parse()
        .unwrap_or_default()
}

struct Global {
    dir: PathBuf,
    session_id: String,
    started: Instant,
    output: Output,
    filter: reload::Handle<Targets, Registry>,
    /// `RUST_LOG` as given: it wins over the `logging.level` setting.
    rust_log: Option<String>,
    level: Mutex<String>,
}

static GLOBAL: OnceLock<Global> = OnceLock::new();

pub fn session_id() -> Option<&'static str> {
    GLOBAL.get().map(|g| g.session_id.as_str())
}

/// Folder of the journal once `init` succeeded.
pub fn dir() -> Option<&'static Path> {
    GLOBAL.get().map(|g| g.dir.as_path())
}

/// The level in force (`RUST_LOG` when set).
pub fn current_level() -> Option<String> {
    let global = GLOBAL.get()?;
    Some(
        global
            .level
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
    )
}

/// Installs the global subscriber writing to `dir` and the panic hook, at the `logging.level`
/// setting (`None` = build default) unless `RUST_LOG` is set. Nothing is logged yet: the first
/// line of the session is `app_start`.
pub fn init(dir: PathBuf, setting: Option<&str>) -> Result<(), String> {
    let file = RollingFile::open(dir.clone(), Retention::default())
        .map_err(|error| format!("journal folder: {error}"))?;
    redact::init_key(&dir);
    let session_id = uuid::Uuid::new_v4().to_string();
    // An unreadable RUST_LOG is ignored (the setting applies).
    let rust_log = std::env::var("RUST_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| {
            value
                .parse::<Targets>()
                .ok()
                .map(|targets| (value, targets))
        });
    let (targets, level, rust_log) = match rust_log {
        Some((value, targets)) => (targets, format!("RUST_LOG={value}"), Some(value)),
        None => {
            let level = setting.filter(|l| is_level(l)).unwrap_or(default_level());
            (targets_for(level), level.to_owned(), None)
        }
    };
    let (filter, handle) = reload::Layer::new(targets);
    let output = Output::new(file, cfg!(debug_assertions));
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(JsonLayer::new(session_id.clone(), output.clone()));
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|error| format!("journal subscriber: {error}"))?;
    let _ = GLOBAL.set(Global {
        dir,
        session_id,
        started: Instant::now(),
        output,
        filter: handle,
        rust_log,
        level: Mutex::new(level),
    });
    install_panic_hook();
    Ok(())
}

/// A line written outside the level filter: the session frame (`app_start`, `app_exit`) and
/// `panic` are always in the file.
fn write_always(level: &str, event: &str, msg: &str, dur_ms: Option<u64>, fields: Value) {
    let Some(global) = GLOBAL.get() else {
        return;
    };
    let now = chrono::Utc::now();
    let record = Record {
        ts: crate::queue::iso(now),
        level: level.to_owned(),
        target: module_path!().to_owned(),
        event: event.to_owned(),
        msg: msg.to_owned(),
        session_id: global.session_id.clone(),
        dur_ms,
        fields: match fields {
            Value::Object(map) => map,
            _ => Map::new(),
        },
        ..Default::default()
    };
    let line = record.to_line();
    if !global.output.write(now.date_naive(), &line) {
        // Re-entrant (a panic while writing a line): stderr is all that is left.
        eprintln!("{line}");
    }
}

/// First line of the session: versions, build, journal settings.
pub fn app_start(versions: &Value) {
    let Some(global) = GLOBAL.get() else {
        return;
    };
    let mut fields = versions.as_object().cloned().unwrap_or_default();
    fields.insert("pid".into(), std::process::id().into());
    fields.insert(
        "log".into(),
        json!({
            "dir": global.dir.to_string_lossy(),
            "level": current_level(),
            "rust_log": global.rust_log.is_some(),
        }),
    );
    write_always(
        "info",
        "app_start",
        "squeezegram started",
        None,
        Value::Object(fields),
    );
}

/// Last line of a clean session.
pub fn app_exit(reason: &str) {
    let Some(global) = GLOBAL.get() else {
        return;
    };
    let uptime = global.started.elapsed().as_millis() as u64;
    write_always(
        "info",
        "app_exit",
        "squeezegram exited",
        Some(uptime),
        json!({ "reason": reason }),
    );
}

/// Applies the `logging.level` setting (`None` = build default), unless `RUST_LOG` is set.
pub fn apply_level(setting: Option<&str>) {
    let Some(global) = GLOBAL.get() else {
        return;
    };
    if global.rust_log.is_some() {
        tracing::debug!(
            event = "log_level",
            setting,
            "RUST_LOG is set: the logging.level setting is ignored"
        );
        return;
    }
    let level = setting.filter(|l| is_level(l)).unwrap_or(default_level());
    let mut current = global
        .level
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *current == level {
        return;
    }
    // Written before the change, so lowering the level still leaves a trace.
    tracing::info!(
        event = "log_level",
        from = current.as_str(),
        to = level,
        "journal level changed"
    );
    if let Err(error) = global.filter.reload(targets_for(level)) {
        tracing::warn!(event = "log_level", error = %error, "journal level not changed");
        return;
    }
    *current = level.to_owned();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-text panic payload".to_owned());
        let location = info
            .location()
            .map(|at| format!("{}:{}:{}", at.file(), at.line(), at.column()));
        let backtrace: Vec<String> = std::backtrace::Backtrace::force_capture()
            .to_string()
            .lines()
            .take(200)
            .map(str::to_owned)
            .collect();
        write_always(
            "error",
            "panic",
            &format!("panic: {}", layer::truncate(&payload, 4000)),
            None,
            json!({
                "location": location,
                "thread": std::thread::current().name().unwrap_or("unnamed"),
                "backtrace": backtrace,
            }),
        );
        previous(info);
    }));
}

/// Milliseconds since `started`, for `dur_ms`.
pub fn ms_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
pub(crate) mod testing {
    //! A journal in a temporary folder for the current thread (a `#[tokio::test]` runs on one
    //! thread, so its spawned tasks are covered too).

    use super::*;
    use tracing::subscriber::{DefaultGuard, Interest};
    use tracing::{Dispatch, Event, Metadata, Subscriber, span};

    pub struct TestJournal {
        pub dir: PathBuf,
        _guard: DefaultGuard,
    }

    /// Enables nothing, but answers "ask me per event" for every callsite. `tracing` caches
    /// each callsite's interest across all live dispatchers; a callsite first hit on another
    /// test's thread while this journal registers can be cached as "never" and its events
    /// lost here (seen in CI). Kept alive for the whole run, this dispatcher keeps every
    /// cached interest at "sometimes", so each event asks the thread's own subscriber.
    struct Undecided;

    impl Subscriber for Undecided {
        fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
            Interest::sometimes()
        }
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            false
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, _: &Event<'_>) {}
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    static UNDECIDED: OnceLock<Dispatch> = OnceLock::new();

    impl TestJournal {
        pub fn new(dir: &Path, level: &str) -> Self {
            // Creating the dispatch registers it and rebuilds every cached interest.
            UNDECIDED.get_or_init(|| Dispatch::new(Undecided));
            let file = RollingFile::open(dir.to_path_buf(), Retention::default()).unwrap();
            let output = Output::new(file, false);
            let subscriber = tracing_subscriber::registry()
                .with(targets_for(level))
                .with(JsonLayer::new("test-session".into(), output));
            Self {
                dir: dir.to_path_buf(),
                _guard: tracing::subscriber::set_default(subscriber),
            }
        }

        /// Every line of every journal file, parsed.
        pub fn lines(&self) -> Vec<Value> {
            self.text()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        /// Whole content of the folder's journal files.
        pub fn text(&self) -> String {
            file::list(&self.dir)
                .unwrap()
                .iter()
                .map(|f| std::fs::read_to_string(&f.path).unwrap())
                .collect()
        }
    }
}

#[cfg(test)]
mod journal_tests;

#[cfg(test)]
mod tests {
    use super::testing::TestJournal;
    use super::*;
    use crate::queue::test_util::TestDir;

    #[test]
    fn every_line_has_the_mandatory_keys_and_span_ids() {
        let dir = TestDir::new("log-format");
        let journal = TestJournal::new(dir.path(), "debug");
        let span = tracing::error_span!("item", item_id = 7_i64, destination_id = 3_i64);
        span.in_scope(|| {
            tracing::info!(
                event = "media_done",
                dur_ms = 1234_u64,
                output_size = 42_u64,
                ratio = 0.5,
                ok = true,
                json = r#"{"args":["-i","a.mp4"]}"#,
                "media done"
            );
        });
        tracing::warn!(event = "command", error_code = "FFMPEG_FAILED", "failed");
        tracing::trace!(event = "filtered_out", "below the level");
        tracing::info!("no event code");

        let lines = journal.lines();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            for key in [
                "ts",
                "level",
                "target",
                "event",
                "msg",
                "session_id",
                "fields",
            ] {
                assert!(line.get(key).is_some(), "{key} missing in {line}");
            }
            let ts = line["ts"].as_str().unwrap();
            assert!(
                chrono::DateTime::parse_from_rfc3339(ts).is_ok()
                    && ts.ends_with('Z')
                    && ts.len() == 24,
                "{ts}"
            );
            assert_eq!(line["session_id"], "test-session");
        }
        let done = &lines[0];
        assert_eq!(done["level"], "info");
        assert_eq!(done["event"], "media_done");
        assert_eq!(done["msg"], "media done");
        assert_eq!(
            (done["item_id"].as_i64(), done["destination_id"].as_i64()),
            (Some(7), Some(3))
        );
        assert_eq!(done["dur_ms"], 1234);
        assert_eq!(
            done["fields"],
            json!({ "output_size": 42, "ratio": 0.5, "ok": true, "args": ["-i", "a.mp4"] })
        );
        assert_eq!(lines[1]["error_code"], "FFMPEG_FAILED");
        assert!(lines[1].get("item_id").is_none(), "outside the span");
        assert_eq!(lines[2]["event"], layer::FALLBACK_EVENT);
    }

    /// The only test that installs the process-wide journal and panic hook. Other tests run in
    /// parallel and may write to it, so only presence is checked, not order.
    #[test]
    fn global_journal_frames_the_session_and_records_panics() {
        let dir = std::env::temp_dir().join(format!(
            "squeezegram-test-{}-log-global",
            std::process::id()
        ));
        // `error`: the lines of the other tests stay out; the frame and panics bypass the level.
        init(dir.clone(), Some("error")).unwrap();
        if std::env::var_os("RUST_LOG").is_none() {
            assert_eq!(current_level().as_deref(), Some("error"));
        }
        app_start(&json!({ "app_version": "test" }));
        let joined = std::thread::Builder::new()
            .name("doomed".into())
            .spawn(|| panic!("boom in a test"))
            .unwrap()
            .join();
        assert!(joined.is_err());
        app_exit("test");

        let text: String = file::list(&dir)
            .unwrap()
            .iter()
            .map(|f| std::fs::read_to_string(&f.path).unwrap())
            .collect();
        let lines: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let find = |event: &str| lines.iter().find(|l| l["event"] == event).cloned();
        let start = find("app_start").expect("app_start");
        assert_eq!(start["fields"]["app_version"], "test");
        assert_eq!(start["session_id"], session_id().unwrap());
        assert!(start["fields"]["log"]["dir"].is_string());
        let panic = find("panic").expect("panic line");
        assert_eq!(panic["level"], "error");
        assert_eq!(panic["msg"], "panic: boom in a test");
        assert_eq!(panic["fields"]["thread"], "doomed");
        assert!(
            panic["fields"]["location"]
                .as_str()
                .unwrap()
                .contains("mod.rs")
        );
        assert!(!panic["fields"]["backtrace"].as_array().unwrap().is_empty());
        assert!(find("app_exit").expect("app_exit")["dur_ms"].is_u64());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn levels_and_targets() {
        assert_eq!(
            default_level(),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "info"
            }
        );
        assert!(is_level("trace") && !is_level("verbose"));
        let warn = targets_for("warn");
        assert!(warn.would_enable("squeezegram_lib::queue", &tracing::Level::WARN));
        assert!(!warn.would_enable("squeezegram_lib::queue", &tracing::Level::INFO));
        let debug = targets_for("debug");
        assert!(debug.would_enable("squeezegram_lib::front", &tracing::Level::DEBUG));
        assert!(
            !debug.would_enable("tao::platform", &tracing::Level::INFO),
            "dependencies stay at warn"
        );
        assert!(targets_for("bogus").would_enable("squeezegram_lib", &tracing::Level::INFO));
    }
}
