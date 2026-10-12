//! The real `SendStage`: builds the `UploadRequest` of an `encoded` item, drives
//! `TelegramTransport::upload`, turns its progress into `queue://progress` and its errors into
//! stable error codes. Watchdog: no progress for `send.watchdog_s` raises a
//! `TG_NO_PROGRESS` alert and never cancels by itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::service::{item_kind, plan_of, target_of};
use super::{
    Caption, MediaKind, Payload, Phase, Progress, Sent, TelegramTransport, TransportError,
    UploadRequest,
};
use crate::caption;
use crate::limit;
use crate::media::plan::{Kind, Plan};
use crate::media::probe::MediaInfo;
use crate::queue::events::{self, Phase as QueuePhase};
use crate::queue::orchestrator::{SendError, SendOutcome, SendStage, StageContext};
use crate::queue::store::{Destination, Item};
use crate::settings::SettingsStore;

/// The `progress` channel is lossy (`try_send`): a few slots are enough.
const PROGRESS_BUFFER: usize = 32;
/// Resolution cap of an encode (`media::stage`, 1080p).
const CAP_LONG: f64 = 1920.0;
const CAP_SHORT: f64 = 1080.0;

pub struct TransportStage {
    transport: Arc<dyn TelegramTransport>,
    settings: Arc<SettingsStore>,
}

impl TransportStage {
    pub fn new(transport: Arc<dyn TelegramTransport>, settings: Arc<SettingsStore>) -> Self {
        Self {
            transport,
            settings,
        }
    }
}

/// Display size of the sent video: rotation and pixel aspect ratio applied, and the 1080p cap
/// when it was re-encoded. A hint for Telegram's player only.
fn video_size(info: Option<&MediaInfo>, plan: Option<&Plan>) -> (u32, u32) {
    let Some(video) = info.and_then(|i| i.video.as_ref()) else {
        return (0, 0);
    };
    let (w, h) = video.rotated_size();
    let (mut w, h) = (f64::from(w), f64::from(h));
    if let Some((num, den)) = video.sar {
        w = w * f64::from(num) / f64::from(den);
    }
    let scale = if plan.is_some_and(|p| p.kind == Kind::Encode) && w > 0.0 && h > 0.0 {
        (CAP_LONG / w.max(h)).min(CAP_SHORT / w.min(h)).min(1.0)
    } else {
        1.0
    };
    let even = |v: f64| ((v * scale) as u32) & !1;
    (even(w), even(h))
}

fn payload(item: &Item, plan: Option<&Plan>) -> Payload {
    let info: Option<MediaInfo> = item
        .media_json
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok());
    match item_kind(item, plan) {
        MediaKind::Document => Payload::Document { thumbnail: None },
        MediaKind::Photo => {
            let (width, height) = plan
                .and_then(|p| p.photo)
                .map(|p| (p.width, p.height))
                .or_else(|| {
                    info.as_ref()
                        .and_then(|i| i.video.as_ref())
                        .map(|v| v.rotated_size())
                })
                .unwrap_or((0, 0));
            Payload::Photo { width, height }
        }
        MediaKind::Video => {
            let duration = info
                .as_ref()
                .and_then(|i| i.duration_s)
                .or_else(|| plan.and_then(|p| p.duration_s))
                .unwrap_or(0.0);
            let (width, height) = video_size(info.as_ref(), plan);
            Payload::Video {
                duration_s: duration.max(0.0).round() as u32,
                width,
                height,
                supports_streaming: true,
                thumbnail: None,
            }
        }
    }
}

/// The request for an `encoded` item, or the code of what is missing.
pub fn upload_request(item: &Item, destination: &Destination) -> Result<UploadRequest, SendError> {
    let target = target_of(destination).ok_or_else(|| SendError::failed("DB_ERROR"))?;
    let path = item
        .output_path
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| SendError::failed("FILE_UNREADABLE"))?;
    let size = match item.output_size.and_then(|s| u64::try_from(s).ok()) {
        Some(size) => size,
        None => std::fs::metadata(&path)
            .map_err(|_| SendError::failed("FILE_UNREADABLE"))?
            .len(),
    };
    let plan = plan_of(item);
    Ok(UploadRequest {
        path,
        size,
        target,
        payload: payload(item, plan.as_ref()),
        caption: Caption {
            text: item.caption.clone(),
            entities: Vec::new(),
        },
        // The limit the plan was built for (resolved at `media_start`).
        limit_bytes: plan.as_ref().map(|p| p.limit_bytes),
    })
}

/// Telegram's lowest caption limit (accounts without Premium): used while no account is
/// known, so a caption is never accepted here and refused later.
const STANDARD_CAPTION_CHARS: u32 = 1024;

/// The caption to send: the template of the item rendered with what is known of the final
/// file, or `CAPTION_TOO_LONG` when the result does not fit. Never truncated: a cut caption
/// would send something the user did not write, and no caption would drop what they did.
pub fn rendered_caption(
    item: &Item,
    request: &UploadRequest,
    today: &str,
    max_chars: u32,
) -> Result<String, SendError> {
    let source_name = Path::new(&item.source_path)
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let (duration_s, resolution) = match &request.payload {
        Payload::Video {
            duration_s,
            width,
            height,
            ..
        } => (
            (*duration_s > 0).then_some(*duration_s),
            (*width > 0 && *height > 0).then_some((*width, *height)),
        ),
        Payload::Photo { width, height } => (
            None,
            (*width > 0 && *height > 0).then_some((*width, *height)),
        ),
        Payload::Document { .. } => (None, None),
    };
    let text = caption::render(
        &item.caption,
        &caption::Fields {
            file_name: &source_name,
            duration_s,
            original_size: u64::try_from(item.source_size).unwrap_or(0),
            final_size: request.size,
            resolution,
            date: today,
            index: u32::try_from(item.batch_index).unwrap_or(1),
            count: u32::try_from(item.batch_count).unwrap_or(1),
        },
    );
    // UTF-16 units, as Telegram counts text offsets: never below the server's count, so a
    // caption over the limit fails here with a clear code instead of at Telegram.
    let length = text.encode_utf16().count();
    if length > max_chars as usize {
        return Err(SendError::Failed {
            code: "CAPTION_TOO_LONG".to_owned(),
            params: json!({ "max": max_chars, "length": length }),
        });
    }
    Ok(text)
}

/// Transport result → orchestrator outcome. A 429 (FLOOD_WAIT or SLOWMODE_WAIT, which an
/// upload cannot tell apart) means nothing went out: `waiting_server`. A timeout or an
/// unexpected answer leaves the message in doubt: `unknown`, never retried automatically.
pub fn outcome(result: Result<Sent, TransportError>) -> Result<SendOutcome, SendError> {
    let error = match result {
        Ok(sent) => {
            return Ok(SendOutcome::Sent {
                message_id: sent.message_id,
            });
        }
        Err(error) => error,
    };
    match error {
        TransportError::FloodWait { retry_after } | TransportError::SlowMode { retry_after } => {
            Ok(SendOutcome::FloodWait { retry_after })
        }
        TransportError::Cancelled => Err(SendError::Cancelled),
        TransportError::Timeout(_) | TransportError::Protocol(_) => {
            let (code, params) = error.code_and_params();
            tracing::warn!(
                event = "upload_error",
                outcome = "unknown",
                error_code = code,
                error = %error,
                "upload in doubt: the message may have reached Telegram"
            );
            Err(SendError::Unknown {
                code: code.to_owned(),
                params,
            })
        }
        TransportError::Other { code, message } => {
            tracing::warn!(
                event = "upload_error",
                outcome = "failed",
                error_code = "TG_UPLOAD_FAILED",
                tg_code = code,
                tg_message = message.as_str(),
                "Telegram refused the upload"
            );
            Err(SendError::Failed {
                code: "TG_UPLOAD_FAILED".to_owned(),
                params: json!({ "tgCode": code, "message": message }),
            })
        }
        other => {
            let (code, _) = other.code_and_params();
            tracing::warn!(
                event = "upload_error",
                outcome = "failed",
                error_code = code,
                error = %other,
                "upload failed"
            );
            let (code, params) = other.code_and_params();
            Err(SendError::Failed {
                code: code.to_owned(),
                params,
            })
        }
    }
}

/// Transport progress → `queue://progress` (percent, average rate since the first byte, ETA).
struct Meter {
    start: Option<(Instant, u64)>,
}

impl Meter {
    fn convert(&mut self, progress: Progress) -> events::Progress {
        let percent = if progress.total == 0 {
            0.0
        } else {
            progress.sent as f64 * 100.0 / progress.total as f64
        };
        let (phase, detail) = match progress.phase {
            Phase::Preparing => (QueuePhase::Uploading, Some("preparing".to_owned())),
            Phase::Uploading => (QueuePhase::Uploading, None),
            Phase::AwaitingServer => (QueuePhase::AwaitingServer, None),
        };
        let mut rate = None;
        if progress.phase == Phase::Uploading {
            let now = Instant::now();
            let (at, from) = *self.start.get_or_insert((now, progress.sent));
            let elapsed = now.duration_since(at).as_secs_f64();
            if elapsed >= 0.5 && progress.sent > from {
                rate = Some((progress.sent - from) as f64 / elapsed);
            }
        }
        let eta = rate.map(|r| progress.total.saturating_sub(progress.sent) as f64 / r);
        events::Progress {
            percent,
            bytes_per_sec: rate,
            eta_sec: eta,
            phase: Some(phase),
            detail,
        }
    }
}

#[async_trait]
impl SendStage for TransportStage {
    async fn upload(
        &self,
        item: &Item,
        destination: &Destination,
        ctx: &StageContext,
    ) -> Result<SendOutcome, SendError> {
        let mut request = upload_request(item, destination).inspect_err(|error| {
            tracing::warn!(event = "upload_error", outcome = "failed", error = %error, "upload request cannot be built")
        })?;
        let max_chars = limit::account(&self.transport.auth_state().borrow())
            .map_or(STANDARD_CAPTION_CHARS, |me| me.caption_max_chars);
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        request.caption.text =
            rendered_caption(item, &request, &today, max_chars).inspect_err(|error| {
                tracing::warn!(event = "upload_error", outcome = "failed", error = %error, "caption does not fit");
            })?;
        // No plan (an item prepared outside the media stage): the limit that applies now.
        if request.limit_bytes.is_none() {
            let auth = self.transport.auth_state();
            let effective =
                limit::resolve(&self.settings.get().limit, limit::account(&auth.borrow()));
            request.limit_bytes = Some(effective.limit_bytes);
        }
        let started = std::time::Instant::now();
        let size = request.size;
        tracing::info!(
            event = "upload_start",
            size,
            limit_bytes = request.limit_bytes,
            media = request.payload.media_kind().as_str(),
            chat = crate::logging::redact::chat(request.target.peer.tdlib_chat_id()).as_str(),
            topic_id = request.target.topic_id,
            // Only whether there is a caption, and its length: never its text.
            caption_chars = request.caption.text.chars().count(),
            path = %request.path.display(),
            attempt = item.attempts,
            "upload started"
        );
        let watchdog = Duration::from_secs(self.settings.get().send.watchdog_s.max(1));
        let (tx, mut rx) = mpsc::channel(PROGRESS_BUFFER);
        let upload = self.transport.upload(request, tx, ctx.cancel.clone());
        tokio::pin!(upload);
        let mut meter = Meter { start: None };
        let mut deadline = Instant::now() + watchdog;
        let mut warned = false;
        let result = loop {
            tokio::select! {
                result = &mut upload => break result,
                Some(progress) = rx.recv() => {
                    deadline = Instant::now() + watchdog;
                    warned = false;
                    ctx.progress.report(meter.convert(progress));
                }
                _ = tokio::time::sleep_until(deadline), if !warned => {
                    warned = true;
                    tracing::warn!(
                        event = "upload_stalled",
                        error_code = "TG_NO_PROGRESS",
                        seconds = watchdog.as_secs(),
                        "no upload progress (watchdog, never cancels)"
                    );
                    ctx.progress.alert(
                        "TG_NO_PROGRESS",
                        json!({ "seconds": watchdog.as_secs() }),
                    );
                }
            }
        };
        let dur_ms = crate::logging::ms_since(started);
        match &result {
            Ok(sent) => tracing::info!(
                event = "upload_done",
                dur_ms,
                size,
                bytes_per_sec = (size as f64 / (dur_ms.max(1) as f64 / 1000.0)).round(),
                message_id = sent.message_id,
                "upload confirmed by the server"
            ),
            Err(TransportError::Cancelled) => {
                tracing::info!(event = "upload_cancelled", dur_ms, "upload cancelled")
            }
            Err(error) => {
                let (code, _) = error.code_and_params();
                tracing::info!(
                    event = "upload_end",
                    dur_ms,
                    error_code = code,
                    "upload ended without a message"
                );
            }
        }
        outcome(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;
    use crate::queue::State;
    use crate::queue::events::recorder::Recorder;
    use crate::queue::orchestrator::{
        MediaOutput, MediaStage, Orchestrator, PauseRange, ProbeOutcome, StageError, Treatment,
    };
    use crate::queue::store::{HistoryFilter, NewItem, SendMode, Store};
    use crate::queue::test_util::TestDir;
    use crate::transport::mock::{MOCK_READ_ONLY_CHANNEL, MOCK_SAVED, MockTransport};
    use crate::transport::service::refresh_destinations;
    use crate::transport::{Forbidden, Peer};

    /// Sends the source as is (no ffmpeg in these tests).
    struct AsIs;

    #[async_trait]
    impl MediaStage for AsIs {
        async fn probe(&self, item: &Item, _: &StageContext) -> Result<ProbeOutcome, StageError> {
            Ok(ProbeOutcome {
                content_id: format!("{}:{}", item.source_size, item.id),
                identity_weak: false,
                media_json: None,
                plan_json: None,
                treatment: Treatment::Skip,
                temp_path: None,
            })
        }

        async fn encode(&self, _: &Item, _: &StageContext) -> Result<MediaOutput, StageError> {
            Err(StageError::failed("FFMPEG_FAILED"))
        }

        async fn remux(&self, _: &Item, _: &StageContext) -> Result<MediaOutput, StageError> {
            Err(StageError::failed("FFMPEG_FAILED"))
        }
    }

    struct Harness {
        queue: Orchestrator,
        mock: Arc<MockTransport>,
        sink: Arc<Recorder>,
        dir: TestDir,
        settings: Arc<SettingsStore>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn harness(name: &str, mock: MockTransport) -> Harness {
        let dir = TestDir::new(name);
        let settings =
            Arc::new(SettingsStore::load(dir.path().join(crate::settings::FILE_NAME)).unwrap());
        let mock = Arc::new(mock);
        let sink = Arc::new(Recorder::default());
        let queue = Orchestrator::new(
            Store::open_in_memory().unwrap(),
            Arc::new(AsIs),
            Arc::new(TransportStage::new(mock.clone(), settings.clone())),
            sink.clone(),
            PauseRange {
                min: Duration::ZERO,
                max: Duration::ZERO,
            },
        );
        refresh_destinations(mock.as_ref(), &queue).await.unwrap();
        let runner = queue.clone();
        let task = tokio::spawn(async move { runner.run().await });
        Harness {
            queue,
            mock,
            sink,
            dir,
            settings,
            task,
        }
    }

    impl Harness {
        fn destination(&self, peer: Peer) -> i64 {
            self.queue
                .with_store(|s| s.list_destinations())
                .unwrap()
                .into_iter()
                .find(|d| d.peer_id == peer.raw_id && d.topic_id == 0)
                .unwrap()
                .id
        }

        fn add(&self, name: &str, peer: Peer) -> i64 {
            self.add_captioned(name, peer, "caption")
        }

        fn add_captioned(&self, name: &str, peer: Peer, caption: &str) -> i64 {
            static SIZE: AtomicI64 = AtomicI64::new(100);
            let size = SIZE.fetch_add(1, Ordering::SeqCst);
            let path = self.dir.file(name, &vec![1u8; size as usize]);
            self.queue
                .add(&[NewItem {
                    source_path: path,
                    source_size: size,
                    source_mtime: 1,
                    destination_id: self.destination(peer),
                    send_mode: SendMode::Video,
                    caption: caption.into(),
                }])
                .unwrap()[0]
        }

        fn item(&self, id: i64) -> Item {
            self.queue.with_store(|s| s.get_item(id)).unwrap()
        }

        async fn wait_state(&self, id: i64, state: State) {
            for _ in 0..36_000 {
                if self.item(id).state == state {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!(
                "item {id} stuck in {} instead of {state}",
                self.item(id).state
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sends_through_the_mock_with_progress() {
        let h = harness("stage-send", MockTransport::authorized()).await;
        let id = h.add("a.mp4", MOCK_SAVED);
        h.queue.start();
        h.wait_state(id, State::Sent).await;
        let item = h.item(id);
        let sent = h.mock.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(item.tg_message_id, Some(sent[0].0));
        let progress = h.sink.progress.lock().unwrap();
        let mine: Vec<_> = progress.iter().filter(|p| p.item_id == id).collect();
        assert!(mine.iter().any(|p| p.phase == QueuePhase::Uploading));
        assert!(mine.iter().any(|p| p.phase == QueuePhase::AwaitingServer));
        assert!(
            mine.iter()
                .any(|p| p.bytes_per_sec.is_some_and(|r| r > 0.0))
        );
        assert!(h.sink.alert_codes().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn flood_wait_then_resume_through_the_orchestrator() {
        let h = harness("stage-flood", MockTransport::authorized()).await;
        h.mock.push_flood_wait(Duration::from_secs(90));
        let id = h.add("a.mp4", MOCK_SAVED);
        h.queue.start();
        h.wait_state(id, State::WaitingServer).await;
        let waiting = h.item(id);
        assert_eq!(waiting.last_error.as_deref(), Some("TG_FLOOD_WAIT"));
        assert!(waiting.next_run_at.is_some());
        assert!(h.mock.sent().is_empty(), "nothing went out");
        let started = Instant::now();
        h.wait_state(id, State::Sent).await;
        assert!(started.elapsed() >= Duration::from_secs(85));
        assert_eq!(h.mock.uploads(), 2, "one retry, after the delay");
        assert_eq!(h.mock.sent().len(), 1);
        let alerts = h.sink.alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].code, "TG_FLOOD_WAIT");
        assert_eq!(alerts[0].params, json!({ "retry_after": 90 }));
    }

    #[tokio::test(start_paused = true)]
    async fn errors_become_failed_or_unknown_with_an_alert() {
        let h = harness("stage-errors", MockTransport::authorized()).await;
        let forbidden = h.add("a.mp4", MOCK_READ_ONLY_CHANNEL);
        h.queue.start();
        h.wait_state(forbidden, State::Failed).await;
        assert_eq!(
            h.item(forbidden).last_error.as_deref(),
            Some("TG_WRITE_FORBIDDEN")
        );

        h.mock
            .push_failure(TransportError::Timeout("updateMessageSendSucceeded"));
        let doubt = h.add("b.mp4", MOCK_SAVED);
        h.wait_state(doubt, State::Unknown).await;
        assert_eq!(h.item(doubt).last_error.as_deref(), Some("TG_TIMEOUT"));

        h.mock.push_failure(TransportError::Other {
            code: 400,
            message: "MEDIA_EMPTY".into(),
        });
        let refused = h.add("c.mp4", MOCK_SAVED);
        h.wait_state(refused, State::Failed).await;

        let alerts = h.sink.alerts.lock().unwrap();
        let codes: Vec<_> = alerts
            .iter()
            .map(|a| (a.item_id, a.code.as_str()))
            .collect();
        assert_eq!(
            codes,
            [
                (Some(forbidden), "TG_WRITE_FORBIDDEN"),
                (Some(doubt), "TG_TIMEOUT"),
                (Some(refused), "TG_UPLOAD_FAILED")
            ]
        );
        assert_eq!(alerts[0].params, json!({ "reason": "admins_only" }));
        assert_eq!(
            alerts[2].params,
            json!({ "tgCode": 400, "message": "MEDIA_EMPTY" })
        );
        let history = h.queue.history(&HistoryFilter::default()).unwrap();
        assert_eq!(history.len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_during_the_upload() {
        let h = harness(
            "stage-cancel",
            MockTransport::authorized().with_step_delay(Duration::from_secs(1)),
        )
        .await;
        let id = h.add("a.mp4", MOCK_SAVED);
        h.queue.start();
        h.wait_state(id, State::Uploading).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        h.queue.cancel(id).unwrap();
        h.wait_state(id, State::Cancelled).await;
        assert!(h.mock.sent().is_empty());
        assert!(
            std::path::Path::new(&h.item(id).source_path).exists(),
            "the source is never deleted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_alerts_without_cancelling() {
        let h = harness("stage-watchdog", MockTransport::authorized()).await;
        h.settings
            .set(&json!({ "send": { "watchdog_s": 5 } }))
            .unwrap();
        h.mock.push_stall(Duration::from_secs(12));
        let id = h.add("a.mp4", MOCK_SAVED);
        h.queue.start();
        h.wait_state(id, State::Sent).await;
        assert_eq!(
            h.sink.alert_codes(),
            [(Some(id), "TG_NO_PROGRESS".to_owned())],
            "one alert per stall"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn signed_out_fails_with_not_authorized() {
        let h = harness("stage-signed-out", MockTransport::authorized()).await;
        h.mock.log_out().await.unwrap();
        let id = h.add("a.mp4", MOCK_SAVED);
        h.queue.start();
        h.wait_state(id, State::Failed).await;
        assert_eq!(h.item(id).last_error.as_deref(), Some("TG_NOT_AUTHORIZED"));
    }

    /// Without a plan the upload enforces the limit that applies now: a custom 1 MiB here, so
    /// a file of 1 MiB + 1 byte is refused before anything is sent.
    #[tokio::test(start_paused = true)]
    async fn without_a_plan_the_current_effective_limit_applies() {
        let h = harness("stage-limit", MockTransport::authorized().with_premium()).await;
        h.settings
            .set(&json!({ "limit": { "custom_mb": 1 } }))
            .unwrap();
        let size = 1024 * 1024 + 1;
        let path = h.dir.file("big.mp4", &vec![1u8; size]);
        let id = h
            .queue
            .add(&[NewItem {
                source_path: path,
                source_size: size as i64,
                source_mtime: 1,
                destination_id: h.destination(MOCK_SAVED),
                send_mode: SendMode::Video,
                caption: String::new(),
            }])
            .unwrap()[0];
        h.queue.start();
        h.wait_state(id, State::Failed).await;
        assert_eq!(h.item(id).last_error.as_deref(), Some("TG_FILE_TOO_LARGE"));
        let alerts = h.sink.alerts.lock().unwrap();
        assert_eq!(alerts[0].params, json!({ "limit": 1024 * 1024 }));
        assert!(h.mock.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_caption_template_is_rendered_when_the_file_is_known() {
        let h = harness("stage-caption", MockTransport::authorized()).await;
        let id = h.add_captioned(
            "été.mp4",
            MOCK_SAVED,
            "{name} ({index}/{count}) {ext} {{kept}} {nope} [{resolution}]",
        );
        h.queue.start();
        h.wait_state(id, State::Sent).await;
        assert_eq!(h.mock.captions(), ["été (1/1) mp4 {kept} {nope} []"]);
    }

    /// Free accounts get 1024 characters, Premium 4096 (the mock mirrors `Me`): counted after
    /// rendering, so a variable can push a caption over.
    #[tokio::test(start_paused = true)]
    async fn a_caption_over_the_limit_fails_the_item_and_sends_nothing() {
        let h = harness("stage-caption-long", MockTransport::authorized()).await;
        let fits = h.add_captioned("a.mp4", MOCK_SAVED, &"x".repeat(1024));
        let too_long = h.add_captioned("b.mp4", MOCK_SAVED, &"é".repeat(1025));
        let by_variable = h.add_captioned(
            "c.mp4",
            MOCK_SAVED,
            &format!("{}{{file}}", "x".repeat(1020)),
        );
        // 1023 + one emoji outside the BMP = 1025 UTF-16 units, as Telegram counts them.
        let emoji = h.add_captioned("d.mp4", MOCK_SAVED, &format!("{}😀", "x".repeat(1023)));
        h.queue.start();
        h.wait_state(fits, State::Sent).await;
        h.wait_state(too_long, State::Failed).await;
        h.wait_state(by_variable, State::Failed).await;
        h.wait_state(emoji, State::Failed).await;
        assert_eq!(
            h.item(too_long).last_error.as_deref(),
            Some("CAPTION_TOO_LONG")
        );
        let alerts = h.sink.alerts.lock().unwrap();
        let alert = alerts.iter().find(|a| a.item_id == Some(too_long)).unwrap();
        assert_eq!(alert.code, "CAPTION_TOO_LONG");
        assert_eq!(alert.params, json!({ "max": 1024, "length": 1025 }));
        let alert = alerts
            .iter()
            .find(|a| a.item_id == Some(by_variable))
            .unwrap();
        assert_eq!(alert.params, json!({ "max": 1024, "length": 1025 }));
        let alert = alerts.iter().find(|a| a.item_id == Some(emoji)).unwrap();
        assert_eq!(alert.params, json!({ "max": 1024, "length": 1025 }));
        // Only the caption that fits went out; the others were not sent without a caption.
        assert_eq!(h.mock.captions(), ["x".repeat(1024)]);
    }

    #[tokio::test(start_paused = true)]
    async fn premium_accounts_may_send_longer_captions() {
        let h = harness(
            "stage-caption-premium",
            MockTransport::authorized().with_premium(),
        )
        .await;
        let ok = h.add_captioned("a.mp4", MOCK_SAVED, &"x".repeat(4096));
        let over = h.add_captioned("b.mp4", MOCK_SAVED, &"x".repeat(4097));
        h.queue.start();
        h.wait_state(ok, State::Sent).await;
        h.wait_state(over, State::Failed).await;
        assert_eq!(h.item(over).last_error.as_deref(), Some("CAPTION_TOO_LONG"));
        let alerts = h.sink.alerts.lock().unwrap();
        let alert = alerts.iter().find(|a| a.item_id == Some(over)).unwrap();
        assert_eq!(alert.params, json!({ "max": 4096, "length": 4097 }));
    }

    #[test]
    fn the_request_carries_the_limit_of_the_plan() {
        let item = |plan_json: Option<String>| Item {
            id: 1,
            position: 1,
            source_path: "C:\\a.mkv".into(),
            source_size: 10,
            source_mtime: 1,
            content_id: None,
            identity_weak: false,
            destination_id: 1,
            send_mode: SendMode::Video,
            caption: String::new(),
            batch_index: 1,
            batch_count: 1,
            state: State::Encoded,
            hold_reason: None,
            origin: "manual".into(),
            media_json: None,
            plan_json,
            temp_path: None,
            output_path: Some("C:\\a_sq.mp4".into()),
            output_size: Some(10),
            progress_phase: None,
            progress_pct: None,
            attempts: 0,
            last_error: None,
            next_run_at: None,
            tg_message_id: None,
            tg_sent_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let destination = Destination {
            id: 1,
            peer_kind: "user".into(),
            peer_id: 7,
            topic_id: 0,
            is_forum: false,
            is_broadcast: false,
            is_saved: true,
            title: "Saved Messages".into(),
            alias: None,
            favorite: false,
            min_interval_s: 0,
            default_mode: SendMode::Video,
            refreshed_at: None,
            kind: "saved".into(),
            archived: false,
            folders: Vec::new(),
            can_write: None,
            listed: true,
            topic_closed: false,
            last_used_at: None,
            use_count: 0,
        };
        let plan = json!({
            "kind": "remux", "limit_bytes": 4_194_304_000_u64, "duration_s": 3.0,
            "video_bps": null, "audio_kbps": null, "video_index": 0, "audio_index": null,
            "vf": null, "hdr": false, "encoder": null, "output_name": "a_sq.mp4",
            "photo": null, "source_dims": null, "alpha": false, "note": null
        })
        .to_string();
        let request = upload_request(&item(Some(plan)), &destination).unwrap();
        assert_eq!(request.limit_bytes, Some(4_194_304_000));
        let request = upload_request(&item(None), &destination).unwrap();
        assert_eq!(request.limit_bytes, None, "filled in by the stage");
    }

    #[test]
    fn outcome_mapping() {
        let sent = Sent {
            message_id: 9,
            date_unix: 0,
            file_name: "a".into(),
            size: 1,
        };
        assert_eq!(outcome(Ok(sent)), Ok(SendOutcome::Sent { message_id: 9 }));
        let wait = Duration::from_secs(70);
        for error in [
            TransportError::FloodWait { retry_after: wait },
            TransportError::SlowMode { retry_after: wait },
        ] {
            assert_eq!(
                outcome(Err(error)),
                Ok(SendOutcome::FloodWait { retry_after: wait })
            );
        }
        assert_eq!(
            outcome(Err(TransportError::Cancelled)),
            Err(SendError::Cancelled)
        );
        let code = |error| match outcome(Err(error)) {
            Err(SendError::Failed { code, .. }) => ("failed", code),
            Err(SendError::Unknown { code, .. }) => ("unknown", code),
            other => panic!("{other:?}"),
        };
        let cases = [
            (TransportError::NotAuthorized, "failed", "TG_NOT_AUTHORIZED"),
            (TransportError::Network("x".into()), "failed", "TG_NETWORK"),
            (
                TransportError::WriteForbidden(Forbidden::PaidMessages { stars: 1 }),
                "failed",
                "TG_PAID_MESSAGES",
            ),
            (
                TransportError::FileTooLarge { limit: 1 },
                "failed",
                "TG_FILE_TOO_LARGE",
            ),
            (
                TransportError::FileUnreadable("x".into()),
                "failed",
                "FILE_UNREADABLE",
            ),
            (TransportError::BadDatabaseKey, "failed", "TG_BAD_DB_KEY"),
            (TransportError::Timeout("x"), "unknown", "TG_TIMEOUT"),
            (
                TransportError::Protocol("x".into()),
                "unknown",
                "TG_PROTOCOL",
            ),
            (
                TransportError::Other {
                    code: 500,
                    message: "x".into(),
                },
                "failed",
                "TG_UPLOAD_FAILED",
            ),
        ];
        for (error, kind, expected) in cases {
            assert_eq!(code(error), (kind, expected.to_owned()));
        }
    }

    #[test]
    fn video_size_applies_sar_rotation_and_the_cap() {
        let info = |w, h, rotation, sar| -> MediaInfo {
            serde_json::from_value(json!({
                "format_name": null, "duration_s": 3.0, "bit_rate": null, "audio": null,
                "video": { "index": 0, "codec": "h264", "profile": null, "width": w,
                    "height": h, "sar": sar, "pix_fmt": null, "field_order": null,
                    "color_transfer": null, "rotation": rotation, "bit_rate": null,
                    "hdr": false }
            }))
            .unwrap()
        };
        let encode: Plan = serde_json::from_value(json!({
            "kind": "encode", "limit_bytes": 1, "duration_s": 3.0, "video_bps": 1,
            "audio_kbps": null, "video_index": 0, "audio_index": null, "vf": null,
            "hdr": false, "encoder": null, "output_name": "x", "photo": null,
            "source_dims": null, "alpha": false, "note": null
        }))
        .unwrap();
        assert_eq!(
            video_size(Some(&info(3840, 2160, 0, json!(null))), None),
            (3840, 2160)
        );
        assert_eq!(
            video_size(Some(&info(3840, 2160, 0, json!(null))), Some(&encode)),
            (1920, 1080)
        );
        assert_eq!(
            video_size(Some(&info(3840, 2160, 90, json!(null))), Some(&encode)),
            (1080, 1920)
        );
        assert_eq!(
            video_size(Some(&info(720, 480, 0, json!([32, 27]))), None),
            (852, 480)
        );
        assert_eq!(video_size(None, None), (0, 0));
    }
}
