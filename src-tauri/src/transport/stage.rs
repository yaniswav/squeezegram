//! The real `SendStage`: builds the `UploadRequest` of an `encoded` item, drives
//! `TelegramTransport::upload`, turns its progress into `queue://progress` and its errors into
//! stable error codes. Watchdog: no progress for `send.watchdog_s` raises a
//! `TG_NO_PROGRESS` alert and never cancels by itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::service::{check_destination, item_kind, mode_kind, plan_of, target_of};
use super::{
    Caption, CopyRequest, MediaKind, Payload, Phase, Progress, RemoteRequest, Sent, Target,
    TelegramTransport, TransportError, UploadRequest,
};
use crate::caption;
use crate::limit;
use crate::media::plan::{Kind, Plan};
use crate::media::probe::MediaInfo;
use crate::media::stage::temp_root;
use crate::media::thumb::Thumbnails;
use crate::queue::events::{self, Phase as QueuePhase};
use crate::queue::orchestrator::{SendError, SendOutcome, SendStage, StageContext, StageError};
use crate::queue::store::{Destination, Item, SendVia};
use crate::settings::SettingsStore;

/// The `progress` channel is lossy (`try_send`): a few slots are enough.
const PROGRESS_BUFFER: usize = 32;
/// Resolution cap of an encode (`media::stage`, 1080p).
const CAP_LONG: f64 = 1920.0;
const CAP_SHORT: f64 = 1080.0;

/// How long a destination check is trusted. A batch of items for one chat shares it, and an
/// item that waits behind a long encode is checked again: rights change slowly compared with
/// this, but a chat is never trusted across the minutes an encode takes.
const PREFLIGHT_TTL: Duration = Duration::from_secs(30);

fn watchdog_deadline(now: Instant, watchdog: Duration) -> Instant {
    // An unrepresentable timeout alerts immediately; it must never panic or cancel a send.
    now.checked_add(watchdog).unwrap_or(now)
}

/// The picture written for one upload attempt.
struct TempThumbnail {
    path: PathBuf,
    /// The item's folder was there before the picture: it is not ours to remove.
    dir_existed: bool,
}

impl TempThumbnail {
    fn remove(&self) {
        crate::queue::store::remove_quietly(&self.path.to_string_lossy());
        if let Some(dir) = self.path.parent().filter(|_| !self.dir_existed) {
            // Only when empty: nothing else in there is deleted.
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// What a check found out: the refusal, or `None` when the destination takes the item.
type Refusal = Option<(&'static str, Value)>;

pub struct TransportStage {
    transport: Arc<dyn TelegramTransport>,
    settings: Arc<SettingsStore>,
    thumbnails: Arc<dyn Thumbnails>,
    /// Recent checks, by chat (topic included) and media kind.
    checks: Mutex<HashMap<(Target, MediaKind), (Instant, Refusal)>>,
}

impl TransportStage {
    pub fn new(
        transport: Arc<dyn TelegramTransport>,
        settings: Arc<SettingsStore>,
        thumbnails: Arc<dyn Thumbnails>,
    ) -> Self {
        Self {
            transport,
            settings,
            thumbnails,
            checks: Mutex::new(HashMap::new()),
        }
    }

    /// Gives a video its preview picture, written in the item's temp folder. Never fails the
    /// send: Telegram makes its own preview of a video that arrives without one.
    async fn attach_thumbnail(
        &self,
        item: &Item,
        request: &mut UploadRequest,
        cancel: &CancellationToken,
    ) -> Option<TempThumbnail> {
        let Payload::Video {
            duration_s,
            thumbnail,
            ..
        } = &mut request.payload
        else {
            return None;
        };
        let root = match temp_root(&self.settings.get().temp) {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!(event = "thumbnail_failed", error = %error, "no thumbnail: the video is sent without one");
                return None;
            }
        };
        let dir = root.join(item.id.to_string());
        let duration = (*duration_s > 0).then_some(f64::from(*duration_s));
        let dir_existed = dir.exists();
        match self
            .thumbnails
            .video(&request.path, duration, &dir, cancel)
            .await
        {
            Ok(made) => {
                let file = TempThumbnail {
                    path: made.path.clone(),
                    dir_existed,
                };
                tracing::debug!(
                    event = "thumbnail_made",
                    width = made.width,
                    height = made.height,
                    "video thumbnail made"
                );
                *thumbnail = Some(made);
                Some(file)
            }
            Err(error) => {
                tracing::warn!(
                    event = "thumbnail_failed",
                    error = %error,
                    "no thumbnail: the video is sent without one"
                );
                None
            }
        }
    }

    fn recent_check(&self, key: &(Target, MediaKind)) -> Option<Refusal> {
        let checks = self.checks.lock().unwrap_or_else(|p| p.into_inner());
        checks
            .get(key)
            .filter(|(at, _)| at.elapsed() < PREFLIGHT_TTL)
            .map(|(_, refusal)| refusal.clone())
    }

    fn remember_check(&self, key: (Target, MediaKind), refusal: Refusal) {
        let mut checks = self.checks.lock().unwrap_or_else(|p| p.into_inner());
        checks.retain(|_, (at, _)| at.elapsed() < PREFLIGHT_TTL);
        checks.insert(key, (Instant::now(), refusal));
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
    async fn reuse(
        &self,
        item: &Item,
        destination: &Destination,
        source: &Destination,
        message_id: i64,
        via: SendVia,
        ctx: &StageContext,
    ) -> Result<SendOutcome, SendError> {
        let mut request = upload_request(item, destination)?;
        let from = target_of(source).ok_or_else(|| SendError::failed("TG_UPLOAD_FAILED"))?;
        let max_chars = limit::account(&self.transport.auth_state().borrow())
            .map_or(STANDARD_CAPTION_CHARS, |me| me.caption_max_chars);
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        request.caption.text = rendered_caption(item, &request, &today, max_chars)?;
        let file_name = request
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        ctx.progress.report(events::Progress {
            percent: 0.0,
            bytes_per_sec: None,
            eta_sec: None,
            phase: Some(QueuePhase::AwaitingServer),
            detail: Some(via.as_str().into()),
        });
        let result = match via {
            SendVia::Copy => {
                self.transport
                    .send_copy(
                        CopyRequest {
                            from,
                            message_id,
                            target: request.target,
                            media: request.payload.media_kind(),
                            caption: request.caption,
                            file_name,
                            size: request.size,
                        },
                        ctx.cancel.clone(),
                    )
                    .await
            }
            SendVia::Remote => {
                let remote_id = match self.transport.message_remote(&from, message_id).await {
                    Ok(Some(id)) => id,
                    Ok(None)
                    | Err(TransportError::Other {
                        code: 400 | 404, ..
                    }) => {
                        return Err(SendError::failed("TG_UPLOAD_FAILED"));
                    }
                    Err(error) => return outcome(Err(error)),
                };
                self.transport
                    .send_remote(
                        RemoteRequest {
                            remote_id,
                            target: request.target,
                            payload: request.payload,
                            caption: request.caption,
                            file_name,
                            size: request.size,
                        },
                        ctx.cancel.clone(),
                    )
                    .await
            }
            SendVia::Upload => return self.upload(item, destination, ctx).await,
        };
        outcome(result)
    }

    async fn preflight(&self, item: &Item, destination: &Destination) -> Result<(), StageError> {
        // A row this app did not write: the upload reports it.
        let Some(target) = target_of(destination) else {
            return Ok(());
        };
        let key = (target, mode_kind(item.send_mode));
        let refusal = match self.recent_check(&key) {
            Some(refusal) => {
                tracing::debug!(
                    event = "preflight_cached",
                    destination_id = destination.id,
                    "destination check reused"
                );
                refusal
            }
            None => match check_destination(self.transport.as_ref(), destination, item.send_mode)
                .await
            {
                Ok(check) => {
                    let refusal = (!check.allowed)
                        .then(|| check.code.map(|code| (code, check.params)))
                        .flatten();
                    self.remember_check(key, refusal.clone());
                    refusal
                }
                // Offline or signed out: not a verdict, and not cached. The upload fails
                // with its own code if the problem is still there.
                Err(error) => {
                    tracing::warn!(
                        event = "preflight_unavailable",
                        destination_id = destination.id,
                        error = %error,
                        "destination could not be checked before the media work"
                    );
                    None
                }
            },
        };
        match refusal {
            None => Ok(()),
            Some((code, params)) => Err(StageError::Failed {
                code: code.to_owned(),
                params,
            }),
        }
    }

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
        let thumbnail = self.attach_thumbnail(item, &mut request, &ctx.cancel).await;
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
        let mut deadline = watchdog_deadline(Instant::now(), watchdog);
        let mut warned = false;
        let result = loop {
            tokio::select! {
                result = &mut upload => break result,
                Some(progress) = rx.recv() => {
                    deadline = watchdog_deadline(Instant::now(), watchdog);
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
        // Sent or not, the picture has done its job; the item's folder goes with it when it is
        // empty (a skipped item has no other file there).
        if let Some(file) = thumbnail {
            file.remove();
        }
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
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

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
    use crate::transport::{Forbidden, Peer, Thumbnail};

    /// Writes a small file where the real one would write its JPEG, and remembers what it was
    /// asked: the video, its duration and the folder.
    #[derive(Default)]
    struct FakeThumbs {
        fail: std::sync::atomic::AtomicBool,
        asked: Mutex<Vec<(PathBuf, Option<f64>, PathBuf)>>,
    }

    #[async_trait]
    impl Thumbnails for FakeThumbs {
        async fn video(
            &self,
            video: &Path,
            duration_s: Option<f64>,
            out_dir: &Path,
            _: &CancellationToken,
        ) -> Result<Thumbnail, crate::media::MediaError> {
            self.asked
                .lock()
                .unwrap()
                .push((video.to_owned(), duration_s, out_dir.to_owned()));
            if self.fail.load(Ordering::SeqCst) {
                return Err(crate::media::MediaError::failed("FFMPEG_FAILED", "boom"));
            }
            std::fs::create_dir_all(out_dir).unwrap();
            let path = out_dir.join("thumb.jpg");
            std::fs::write(&path, b"jpeg").unwrap();
            Ok(Thumbnail {
                path,
                width: 320,
                height: 180,
            })
        }
    }

    /// Sends the source as is (no ffmpeg in these tests); counts the items it was asked to
    /// probe, which is where the media work of an item begins.
    #[derive(Default)]
    struct AsIs {
        probes: AtomicUsize,
        shared_identity: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl MediaStage for AsIs {
        async fn probe(&self, item: &Item, _: &StageContext) -> Result<ProbeOutcome, StageError> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            Ok(ProbeOutcome {
                content_id: if self.shared_identity.load(Ordering::SeqCst) {
                    format!("{}:{}", item.source_size, item.source_path)
                } else {
                    format!("{}:{}", item.source_size, item.id)
                },
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
        media: Arc<AsIs>,
        thumbs: Arc<FakeThumbs>,
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
        let media = Arc::new(AsIs::default());
        let thumbs = Arc::new(FakeThumbs::default());
        // Saved custom roots must exist before use, as when picked in the native dialog.
        std::fs::create_dir(dir.path().join("tmp")).unwrap();
        // Item folders under the test's own directory, not the user's temp folder.
        settings
            .set(&json!({ "temp": { "dir": dir.path().join("tmp").to_string_lossy() } }))
            .unwrap();
        let queue = Orchestrator::new(
            Store::open_in_memory().unwrap(),
            media.clone(),
            Arc::new(TransportStage::new(
                mock.clone(),
                settings.clone(),
                thumbs.clone(),
            )),
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
            media,
            thumbs,
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
            self.add_as(name, peer, caption, SendMode::Video)
        }

        fn add_as(&self, name: &str, peer: Peer, caption: &str, mode: SendMode) -> i64 {
            static SIZE: AtomicI64 = AtomicI64::new(100);
            let size = SIZE.fetch_add(1, Ordering::SeqCst);
            let path = self.dir.file(name, &vec![1u8; size as usize]);
            self.queue
                .add(&[NewItem {
                    source_path: path,
                    source_size: size,
                    source_mtime: 1,
                    destination_id: self.destination(peer),
                    send_mode: mode,
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
    async fn real_stage_copies_and_falls_back_through_remote_then_upload() {
        use crate::transport::mock::MOCK_ARCHIVED_CHAT;

        for fallback in 0..3 {
            let h = harness("stage-batch", MockTransport::authorized()).await;
            h.media.shared_identity.store(true, Ordering::SeqCst);
            let path = h.dir.file("a.mp4", b"generated");
            let items: Vec<_> = [MOCK_SAVED, MOCK_ARCHIVED_CHAT]
                .into_iter()
                .map(|peer| NewItem {
                    source_path: path.clone(),
                    source_size: 9,
                    source_mtime: 1,
                    destination_id: h.destination(peer),
                    send_mode: SendMode::Video,
                    caption: "{name}".into(),
                })
                .collect();
            let ids = h.queue.add_with_copies(&items, true).unwrap();
            if fallback > 0 {
                h.mock.push_copy_failure(TransportError::Other {
                    code: 400,
                    message: "MESSAGE_COPY_FORBIDDEN".into(),
                });
            }
            if fallback > 1 {
                h.mock.push_remote_failure(TransportError::Other {
                    code: 400,
                    message: "FILE_REFERENCE_EXPIRED".into(),
                });
            }
            h.queue.start();
            h.wait_state(ids[1], State::Sent).await;
            assert_eq!(h.mock.uploads(), if fallback == 2 { 2 } else { 1 });
            assert_eq!(h.mock.copies(), 1);
            assert_eq!(h.mock.remote_sends(), usize::from(fallback > 0));
            assert_eq!(h.mock.captions(), ["a", "a"]);
            assert!(
                !h.sink
                    .alert_codes()
                    .iter()
                    .any(|(_, code)| code == "TG_NO_PROGRESS")
            );
            let via = h.queue.with_store(|s| s.send_metadata(ids[1])).unwrap().via;
            assert_eq!(
                via,
                [SendVia::Copy, SendVia::Remote, SendVia::Upload][fallback]
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
        // The refused chat never reached the probe, so the item has no identity to record.
        let history = h.queue.history(&HistoryFilter::default()).unwrap();
        assert_eq!(history.len(), 2);
    }

    /// The chat was fine when the file was added and is not any more when the item's turn
    /// comes: it is refused before the probe, hence before any encode, with the code the
    /// upload would have given, and a retry sends it once the chat takes files again.
    #[tokio::test(start_paused = true)]
    async fn a_destination_that_lost_its_rights_is_refused_before_the_media_work() {
        use crate::transport::mock::MOCK_SLOW_GROUP;
        let h = harness("stage-preflight", MockTransport::authorized()).await;
        let cases = [
            (
                Forbidden::AdminsOnly,
                "TG_WRITE_FORBIDDEN",
                json!({ "reason": "admins_only" }),
            ),
            (
                Forbidden::TopicClosed,
                "TG_WRITE_FORBIDDEN",
                json!({ "reason": "topic_closed" }),
            ),
            (
                Forbidden::PaidMessages { stars: 5 },
                "TG_PAID_MESSAGES",
                json!({ "stars": 5 }),
            ),
        ];
        for (forbidden, code, params) in cases {
            h.mock.revoke_write(MOCK_SLOW_GROUP, forbidden);
            let id = h.add("a.mp4", MOCK_SLOW_GROUP);
            h.queue.start();
            h.wait_state(id, State::Failed).await;
            assert_eq!(h.item(id).last_error.as_deref(), Some(code));
            let alerts = h.sink.alerts.lock().unwrap();
            let alert = alerts.iter().rev().find(|a| a.item_id == Some(id)).unwrap();
            assert_eq!((alert.code.as_str(), &alert.params), (code, &params));
            drop(alerts);
            // Let the cached refusal expire so the next case asks again.
            tokio::time::sleep(PREFLIGHT_TTL + Duration::from_secs(1)).await;
        }
        assert_eq!(
            h.media.probes.load(Ordering::SeqCst),
            0,
            "no media work started"
        );
        assert_eq!(h.mock.uploads(), 0, "nothing reached the upload");

        // Failed, not lost: once the chat accepts files again the user retries and it goes.
        h.mock.restore_write(MOCK_SLOW_GROUP);
        tokio::time::sleep(PREFLIGHT_TTL + Duration::from_secs(1)).await;
        let retried = h.queue.with_store(|s| s.list_items()).unwrap()[0].id;
        h.queue.retry_failed(retried).unwrap();
        h.wait_state(retried, State::Sent).await;
        assert_eq!(h.media.probes.load(Ordering::SeqCst), 1);
    }

    /// 50 files for one chat cost one TDLib check while the answer is fresh, a second chat
    /// its own, and the chat is asked again once the answer is old. A slow mode is a wait,
    /// not a refusal.
    #[tokio::test(start_paused = true)]
    async fn the_check_is_shared_by_a_batch_and_expires() {
        use crate::transport::mock::MOCK_SLOW_GROUP;
        // Uploads take no time here: the 30 s of a check's life are not spent by the batch.
        let mock = MockTransport::authorized().with_step_delay(Duration::ZERO);
        let h = harness("stage-preflight-cache", mock).await;
        let ids: Vec<i64> = (0..50)
            .map(|n| h.add(&format!("{n}.mp4"), MOCK_SLOW_GROUP))
            .collect();
        let other = h.add("other.mp4", MOCK_SAVED);
        h.queue.start();
        h.wait_state(other, State::Sent).await;
        for id in &ids {
            h.wait_state(*id, State::Sent).await;
        }
        assert_eq!(
            h.mock.checks(),
            2,
            "one check per chat, not one per file ({} files)",
            ids.len() + 1
        );

        tokio::time::sleep(PREFLIGHT_TTL + Duration::from_secs(1)).await;
        let later = h.add("later.mp4", MOCK_SLOW_GROUP);
        h.wait_state(later, State::Sent).await;
        assert_eq!(h.mock.checks(), 3, "asked again after the answer got old");
        assert!(h.sink.alert_codes().is_empty());
    }

    /// A chat that cannot be checked (signed out, offline) is not refused by the check: the
    /// upload gives its own verdict.
    #[tokio::test(start_paused = true)]
    async fn a_check_that_cannot_run_does_not_refuse_the_item() {
        let h = harness("stage-preflight-offline", MockTransport::authorized()).await;
        h.mock.log_out().await.unwrap();
        let id = h.add("a.mp4", MOCK_SAVED);
        h.queue.start();
        h.wait_state(id, State::Failed).await;
        assert_eq!(h.item(id).last_error.as_deref(), Some("TG_NOT_AUTHORIZED"));
        assert_eq!(
            h.media.probes.load(Ordering::SeqCst),
            1,
            "the media work started"
        );
        assert_eq!(h.mock.checks(), 0);
    }

    /// The folder of an item under the test's temp root.
    fn item_dir(h: &Harness, id: i64) -> PathBuf {
        temp_root(&h.settings.get().temp)
            .unwrap()
            .join(id.to_string())
    }

    #[cfg(windows)]
    #[tokio::test(start_paused = true)]
    async fn unsafe_temp_root_skips_thumbnail_without_writing_there() {
        let h = harness("stage-thumb-unsafe", MockTransport::authorized()).await;
        let root = h.dir.path().join("tmp");
        let private = root.join("squeezegram-temp");
        std::fs::create_dir(&private).unwrap();
        let sentinel = private.join("keep.bin");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        let id = h.add("a.mp4", MOCK_SAVED);
        h.queue.start();
        h.wait_state(id, State::Sent).await;
        assert!(h.thumbs.asked.lock().unwrap().is_empty());
        assert_eq!(h.mock.thumbnails(), [None]);
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(private).unwrap().count(), 1);
        assert_eq!(std::fs::read(sentinel).unwrap(), b"unchanged");
    }

    #[tokio::test(start_paused = true)]
    async fn a_video_goes_out_with_its_thumbnail_which_is_deleted_afterwards() {
        let h = harness("stage-thumb", MockTransport::authorized()).await;
        let id = h.add("a.mp4", MOCK_SAVED);
        h.queue.start();
        h.wait_state(id, State::Sent).await;
        assert_eq!(h.mock.thumbnails(), [Some((320, 180, true))]);
        let asked = h.thumbs.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        let (video, _, folder) = &asked[0];
        assert_eq!(
            video,
            Path::new(&h.item(id).source_path),
            "the file that is sent"
        );
        assert_eq!(folder, &item_dir(&h, id), "in the item's temp folder");
        assert!(!folder.exists(), "picture and empty folder are gone");
    }

    #[tokio::test(start_paused = true)]
    async fn the_thumbnail_is_deleted_when_the_upload_fails_and_a_shared_folder_stays() {
        let h = harness("stage-thumb-failed", MockTransport::authorized()).await;
        h.mock.push_failure(TransportError::Other {
            code: 400,
            message: "MEDIA_EMPTY".into(),
        });
        let id = h.add("a.mp4", MOCK_SAVED);
        // The folder already holds the item's encoded file: it must stay.
        let folder = item_dir(&h, id);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a_sq.mp4"), b"x").unwrap();
        h.queue.start();
        h.wait_state(id, State::Failed).await;
        assert_eq!(h.mock.thumbnails().len(), 1);
        assert!(!folder.join("thumb.jpg").exists());
        assert!(folder.join("a_sq.mp4").exists());
    }

    #[tokio::test(start_paused = true)]
    async fn a_thumbnail_that_cannot_be_made_never_fails_the_send() {
        let h = harness("stage-thumb-none", MockTransport::authorized()).await;
        h.thumbs.fail.store(true, Ordering::SeqCst);
        let id = h.add("a.mp4", MOCK_SAVED);
        h.queue.start();
        h.wait_state(id, State::Sent).await;
        assert_eq!(h.mock.thumbnails(), [None], "sent without a thumbnail");
        assert!(
            h.sink.alert_codes().is_empty(),
            "nothing for the user to do"
        );
    }

    /// Only a video sent as a video has one: a file is shown by its name, a photo is a photo.
    #[tokio::test(start_paused = true)]
    async fn documents_and_photos_get_no_thumbnail() {
        let h = harness("stage-thumb-modes", MockTransport::authorized()).await;
        let file = h.add_as("a.mp4", MOCK_SAVED, "", SendMode::Document);
        let photo = h.add_as("b.jpg", MOCK_SAVED, "", SendMode::Photo);
        h.queue.start();
        h.wait_state(file, State::Sent).await;
        h.wait_state(photo, State::Sent).await;
        assert!(h.thumbs.asked.lock().unwrap().is_empty());
        assert!(h.mock.thumbnails().is_empty());
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
    async fn watchdog_deadline_handles_extreme_durations() {
        let now = Instant::now();
        let normal = Duration::from_secs(120);
        assert_eq!(watchdog_deadline(now, normal).duration_since(now), normal);
        assert_eq!(watchdog_deadline(now, Duration::ZERO), now);
        for extreme in [Duration::from_secs(u64::MAX), Duration::MAX] {
            let deadline = watchdog_deadline(now, extreme);
            assert_eq!(deadline, now);
            tokio::time::sleep_until(deadline).await;
        }
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
            encoding_override: None,
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
            remote_id: None,
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
