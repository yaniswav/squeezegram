//! Queue events sent to the front (`queue://changed`, `queue://progress`, `queue://alert`,
//! `queue://status`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::time::Instant;

use super::store::ItemDelta;

pub const QUEUE_CHANGED: &str = "queue://changed";
pub const QUEUE_PROGRESS: &str = "queue://progress";
pub const QUEUE_ALERT: &str = "queue://alert";
/// Running or paused, emitted on every change (`queue_status`).
pub const QUEUE_STATUS: &str = "queue://status";
pub const SIDECARS_PROGRESS: &str = "sidecars://progress";
pub const SIDECARS_STATUS: &str = "sidecars://status";

/// About 4 Hz per item.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// Progress sampled into the journal (`progress` event).
pub const LOG_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueueChanged {
    pub items: Vec<ItemDelta>,
    /// Items deleted from the queue (`queue_clear`); the front drops them from its list.
    pub removed: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Probing,
    Encoding,
    Remuxing,
    Uploading,
    AwaitingServer,
    WaitingServer,
}

impl Phase {
    /// Serialized name (`awaiting_server`…).
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Probing => "probing",
            Phase::Encoding => "encoding",
            Phase::Remuxing => "remuxing",
            Phase::Uploading => "uploading",
            Phase::AwaitingServer => "awaiting_server",
            Phase::WaitingServer => "waiting_server",
        }
    }
}

/// What a stage reports.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Progress {
    pub percent: f64,
    pub bytes_per_sec: Option<f64>,
    pub eta_sec: Option<f64>,
    /// Overrides the stage phase (an upload reports `AwaitingServer` once the bytes are sent).
    pub phase: Option<Phase>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueProgress {
    pub item_id: i64,
    pub phase: Phase,
    pub percent: f64,
    pub bytes_per_sec: Option<f64>,
    pub eta_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Payload of `queue://alert`: an error or warning the front translates from `code`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueAlert {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<i64>,
    pub code: String,
    pub params: Value,
}

/// Payload of `queue://status` and result of `queue_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct QueueStatus {
    pub running: bool,
}

/// Payload of `sidecars://progress`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarProgress {
    pub name: String,
    pub percent: f64,
    pub bytes_per_sec: Option<f64>,
}

/// `queue_alert` journal line: the code and params sent to the UI.
pub fn log_alert(item_id: Option<i64>, code: &str, params: &Value) {
    let params = serde_json::json!({ "params": journal_params(code, params) }).to_string();
    tracing::warn!(
        event = "queue_alert",
        item_id,
        error_code = code,
        json = params.as_str(),
        "alert {code}"
    );
}

/// The params of an alert as the journal gets them: TDLib's message about a proxy names the
/// proxy server, so it reaches the page only.
fn journal_params(code: &str, params: &Value) -> Value {
    let mut params = params.clone();
    if code == "TG_PROXY_FAILED"
        && let Some(message) = params.get_mut("message")
    {
        *message = Value::from("<redacted>");
    }
    params
}

/// Where queue events go: the Tauri window in the app, a recorder in tests.
pub trait EventSink: Send + Sync {
    fn queue_changed(&self, event: &QueueChanged);
    fn queue_progress(&self, event: &QueueProgress);
    /// Download progress of a sidecar; sinks that do not care keep the default.
    fn sidecar_progress(&self, _event: &SidecarProgress) {}
    fn queue_alert(&self, _event: &QueueAlert) {}
    fn queue_status(&self, _event: &QueueStatus) {}
}

/// Progress reporter handed to a stage for one item and one phase. Throttled to
/// `PROGRESS_INTERVAL`, except phase changes and completion which always go through.
pub struct ProgressReporter {
    item_id: i64,
    phase: Phase,
    sink: Arc<dyn EventSink>,
    last: Mutex<Option<(Phase, Instant)>>,
    logged: Mutex<Option<(Phase, Instant)>>,
}

impl ProgressReporter {
    pub fn new(item_id: i64, phase: Phase, sink: Arc<dyn EventSink>) -> Self {
        Self {
            item_id,
            phase,
            sink,
            last: Mutex::new(None),
            logged: Mutex::new(None),
        }
    }

    pub fn item_id(&self) -> i64 {
        self.item_id
    }

    /// `progress` journal line: every `LOG_INTERVAL`, on a phase change and at 100 %.
    fn log(&self, phase: Phase, progress: &Progress, now: Instant, force: bool) {
        let Ok(mut logged) = self.logged.lock() else {
            return;
        };
        let due = force
            || match *logged {
                None => true,
                Some((last_phase, at)) => {
                    last_phase != phase
                        || progress.percent >= 100.0
                        || now.duration_since(at) >= LOG_INTERVAL
                }
            };
        if !due {
            return;
        }
        *logged = Some((phase, now));
        tracing::info!(
            event = "progress",
            item_id = self.item_id,
            phase = phase.as_str(),
            percent = (progress.percent.clamp(0.0, 100.0) * 10.0).round() / 10.0,
            bytes_per_sec = progress.bytes_per_sec.map(f64::round),
            eta_s = progress.eta_sec.map(f64::round),
            detail = progress.detail.as_deref(),
            "progress"
        );
    }

    pub fn report(&self, progress: Progress) {
        self.send(progress, false);
    }

    /// Like `report`, never throttled: for a one-off step the front must see
    /// (`detail: "cpu_fallback"`).
    pub fn report_now(&self, progress: Progress) {
        self.send(progress, true);
    }

    fn send(&self, progress: Progress, force: bool) {
        let phase = progress.phase.unwrap_or(self.phase);
        let now = Instant::now();
        self.log(phase, &progress, now, force);
        {
            let Ok(mut last) = self.last.lock() else {
                return;
            };
            let due = force
                || match *last {
                    None => true,
                    Some((last_phase, at)) => {
                        last_phase != phase
                            || progress.percent >= 100.0
                            || now.duration_since(at) >= PROGRESS_INTERVAL
                    }
                };
            if !due {
                return;
            }
            *last = Some((phase, now));
        }
        self.sink.queue_progress(&QueueProgress {
            item_id: self.item_id,
            phase,
            percent: progress.percent.clamp(0.0, 100.0),
            bytes_per_sec: progress.bytes_per_sec,
            eta_sec: progress.eta_sec,
            retry_after_s: None,
            detail: progress.detail,
        });
    }

    /// A warning about this item that does not change its state (`TG_NO_PROGRESS`…).
    pub fn alert(&self, code: &str, params: Value) {
        log_alert(Some(self.item_id), code, &params);
        self.sink.queue_alert(&QueueAlert {
            item_id: Some(self.item_id),
            code: code.to_owned(),
            params,
        });
    }
}

#[cfg(test)]
pub mod recorder {
    use super::*;

    /// Records every event.
    #[derive(Default)]
    pub struct Recorder {
        pub changed: Mutex<Vec<ItemDelta>>,
        pub removed: Mutex<Vec<i64>>,
        /// Number of `queue://changed` events, deltas or removals.
        pub changed_events: Mutex<usize>,
        pub progress: Mutex<Vec<QueueProgress>>,
        pub sidecar: Mutex<Vec<SidecarProgress>>,
        pub alerts: Mutex<Vec<QueueAlert>>,
        pub status: Mutex<Vec<QueueStatus>>,
    }

    impl Recorder {
        pub fn states_of(&self, id: i64) -> Vec<crate::queue::State> {
            let changed = self.changed.lock().unwrap();
            let mut states: Vec<_> = changed
                .iter()
                .filter(|d| d.id == id)
                .map(|d| d.state)
                .collect();
            states.dedup();
            states
        }

        pub fn alert_codes(&self) -> Vec<(Option<i64>, String)> {
            self.alerts
                .lock()
                .unwrap()
                .iter()
                .map(|a| (a.item_id, a.code.clone()))
                .collect()
        }
    }

    impl EventSink for Recorder {
        fn queue_changed(&self, event: &QueueChanged) {
            self.changed.lock().unwrap().extend(event.items.clone());
            self.removed.lock().unwrap().extend(event.removed.clone());
            *self.changed_events.lock().unwrap() += 1;
        }

        fn queue_progress(&self, event: &QueueProgress) {
            self.progress.lock().unwrap().push(event.clone());
        }

        fn sidecar_progress(&self, event: &SidecarProgress) {
            self.sidecar.lock().unwrap().push(event.clone());
        }

        fn queue_alert(&self, event: &QueueAlert) {
            self.alerts.lock().unwrap().push(event.clone());
        }

        fn queue_status(&self, event: &QueueStatus) {
            self.status.lock().unwrap().push(*event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::recorder::Recorder;
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn progress_is_throttled_to_about_4_hz() {
        let sink = Arc::new(Recorder::default());
        let reporter = ProgressReporter::new(1, Phase::Encoding, sink.clone());
        // 100 reports over one second.
        for step in 0..100 {
            reporter.report(Progress {
                percent: step as f64 * 0.5,
                ..Default::default()
            });
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let count = sink.progress.lock().unwrap().len();
        assert!((4..=5).contains(&count), "{count} events in 1 s");

        // A phase change and completion are never dropped.
        reporter.report(Progress {
            phase: Some(Phase::AwaitingServer),
            ..Default::default()
        });
        reporter.report(Progress {
            percent: 100.0,
            phase: Some(Phase::AwaitingServer),
            ..Default::default()
        });
        let events = sink.progress.lock().unwrap();
        assert_eq!(events.len(), count + 2);
        assert_eq!(events[count].phase, Phase::AwaitingServer);
    }

    #[test]
    fn progress_payload_uses_the_spec_names() {
        let event = QueueProgress {
            item_id: 3,
            phase: Phase::WaitingServer,
            percent: 0.0,
            bytes_per_sec: None,
            eta_sec: None,
            retry_after_s: Some(30),
            detail: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "itemId": 3, "phase": "waiting_server", "percent": 0.0,
                "bytesPerSec": null, "etaSec": null, "retryAfterS": 30
            })
        );
        let alert = QueueAlert {
            item_id: None,
            code: "FILES_SKIPPED".into(),
            params: serde_json::json!({ "count": 2 }),
        };
        assert_eq!(
            serde_json::to_value(&alert).unwrap(),
            serde_json::json!({ "code": "FILES_SKIPPED", "params": { "count": 2 } })
        );
        let alert = QueueAlert {
            item_id: Some(4),
            ..alert
        };
        assert_eq!(serde_json::to_value(&alert).unwrap()["itemId"], 4);
    }
}
