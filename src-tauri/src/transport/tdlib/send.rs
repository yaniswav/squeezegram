//! `upload`, `verify_sent` and `message_link`, named after `td_api.tl` of TDLib 1.8.68
//! (commit c15d3f5a).
//!
//! 1.8.68 restructured the media contents: `inputMessageVideo{video: inputVideo{video,
//! thumbnail, cover, start_timestamp, added_sticker_file_ids, duration, width, height,
//! supports_streaming}, caption, show_caption_above_media, self_destruct_type, has_spoiler}`,
//! `inputMessageDocument{document: inputDocument{document, thumbnail,
//! disable_content_type_detection}, caption}`, `inputMessagePhoto{photo: inputPhoto{photo,
//! thumbnail, video, added_sticker_file_ids, width, height}, caption, …}` (1.8.61 had a
//! flat shape).
//!
//! Upload: `sendMessage` answers at once with a temporary message; progress is
//! `updateFile` on the main file (`remote.uploaded_size`); `remote.is_uploading_completed`
//! without `updateMessageSendSucceeded` is `AwaitingServer`; success is ONLY
//! `updateMessageSendSucceeded` (final `message.id`); `updateMessageSendFailed` is a typed
//! error and its failed message is deleted (it never reached Telegram); `429 … retry after X`
//! is `FloodWait`. Cancel: `deleteMessages` on the temporary id, never once the success is
//! known. Nothing is ever resent (`resendMessages` is never called).

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::chats::{ChatType, chat_id};
use super::json::{self as j, int, int_or_zero};
use super::{Inner, TdBindings};
use crate::transport::{
    AuthState, Caption, EntityKind, Forbidden, MediaKind, Payload, PeerKind, Phase, Progress, Sent,
    SentCandidate, SentProbe, Target, Thumbnail, TransportError, UploadRequest, Verdict,
};

/// `searchChatMessages.limit` (at most 100) and the pages read for one reconciliation.
const SEARCH_PAGE: i32 = 50;
const MAX_SEARCH_PAGES: usize = 4;
const CAPTION_PREVIEW_CHARS: usize = 64;
/// `upload_progress` journal line at most this often.
const PROGRESS_LOG_EVERY: Duration = Duration::from_secs(5);

/// Update types an upload listens to (the receive thread forwards only these).
pub const UPLOAD_UPDATES: [&str; 4] = [
    "updateFile",
    "updateMessageSendSucceeded",
    "updateMessageSendFailed",
    "updateDeleteMessages",
];

fn redact(chat_id: i64) -> String {
    crate::logging::redact::chat(chat_id)
}

// ---------- request bodies ----------

fn local_file(path: &Path) -> Result<Value, TransportError> {
    let absolute = std::path::absolute(path)
        .map_err(|error| TransportError::FileUnreadable(error.kind().to_string()))?;
    let text = absolute
        .to_str()
        .ok_or_else(|| TransportError::FileUnreadable("path is not valid Unicode".into()))?;
    Ok(json!({ "@type": "inputFileLocal", "path": text }))
}

/// `inputThumbnail`, or null ("pass null to skip thumbnail uploading").
fn thumbnail(thumbnail: Option<&Thumbnail>) -> Result<Value, TransportError> {
    match thumbnail {
        None => Ok(Value::Null),
        Some(t) => Ok(json!({
            "@type": "inputThumbnail",
            "thumbnail": local_file(&t.path)?,
            "width": t.width,
            "height": t.height,
        })),
    }
}

/// `formattedText{text, entities: [textEntity{offset, length, type}]}`, UTF-16 offsets.
pub fn formatted_text(caption: &Caption) -> Value {
    let entities: Vec<Value> = caption
        .entities
        .iter()
        .map(|entity| {
            let kind = match &entity.kind {
                EntityKind::Bold => json!({ "@type": "textEntityTypeBold" }),
                EntityKind::Italic => json!({ "@type": "textEntityTypeItalic" }),
                EntityKind::Underline => json!({ "@type": "textEntityTypeUnderline" }),
                EntityKind::Strike => json!({ "@type": "textEntityTypeStrikethrough" }),
                EntityKind::Spoiler => json!({ "@type": "textEntityTypeSpoiler" }),
                EntityKind::Code => json!({ "@type": "textEntityTypeCode" }),
                EntityKind::TextUrl(url) => json!({ "@type": "textEntityTypeTextUrl", "url": url }),
            };
            json!({
                "@type": "textEntity",
                "offset": entity.offset_utf16,
                "length": entity.length_utf16,
                "type": kind,
            })
        })
        .collect();
    json!({ "@type": "formattedText", "text": caption.text, "entities": entities })
}

/// `inputMessageVideo` / `inputMessageDocument` / `inputMessagePhoto` (1.8.68 shapes).
pub fn input_content(req: &UploadRequest) -> Result<Value, TransportError> {
    let file = local_file(&req.path)?;
    let caption = formatted_text(&req.caption);
    Ok(match &req.payload {
        Payload::Video {
            duration_s,
            width,
            height,
            supports_streaming,
            thumbnail: thumb,
        } => json!({
            "@type": "inputMessageVideo",
            "video": {
                "@type": "inputVideo",
                "video": file,
                "thumbnail": thumbnail(thumb.as_ref())?,
                "cover": null,
                "start_timestamp": 0,
                "added_sticker_file_ids": [],
                "duration": i32::try_from(*duration_s).unwrap_or(i32::MAX),
                "width": i32::try_from(*width).unwrap_or(0),
                "height": i32::try_from(*height).unwrap_or(0),
                "supports_streaming": supports_streaming,
            },
            "caption": caption,
            "show_caption_above_media": false,
            "self_destruct_type": null,
            "has_spoiler": false,
        }),
        Payload::Document { thumbnail: thumb } => json!({
            "@type": "inputMessageDocument",
            "document": {
                "@type": "inputDocument",
                "document": file,
                "thumbnail": thumbnail(thumb.as_ref())?,
                // Document mode is explicit: never turned back into a video by TDLib.
                "disable_content_type_detection": true,
            },
            "caption": caption,
        }),
        Payload::Photo { width, height } => json!({
            "@type": "inputMessagePhoto",
            "photo": {
                "@type": "inputPhoto",
                "photo": file,
                "thumbnail": null,
                "video": null,
                "added_sticker_file_ids": [],
                "width": i32::try_from(*width).unwrap_or(0),
                "height": i32::try_from(*height).unwrap_or(0),
            },
            "caption": caption,
            "show_caption_above_media": false,
            "self_destruct_type": null,
            "has_spoiler": false,
        }),
    })
}

/// `messageTopicForum{forum_topic_id}`, or null for the whole chat.
pub fn message_topic(target: &Target) -> Value {
    match target.topic_id {
        Some(id) => json!({ "@type": "messageTopicForum", "forum_topic_id": id }),
        None => Value::Null,
    }
}

/// `sendMessage` with explicit `messageSendOptions`: `paid_message_star_count` is always 0
/// (the app never pays Stars).
pub fn send_message_body(chat_id: i64, req: &UploadRequest) -> Result<Value, TransportError> {
    Ok(json!({
        "@type": "sendMessage",
        "chat_id": chat_id,
        "topic_id": message_topic(&req.target),
        "reply_to": null,
        "options": {
            "@type": "messageSendOptions",
            "suggested_post_info": null,
            "disable_notification": false,
            "from_background": false,
            "protect_content": false,
            "allow_paid_broadcast": false,
            "paid_message_star_count": 0,
            "update_order_of_installed_sticker_sets": false,
            "scheduling_state": null,
            "effect_id": "0",
            "sending_id": 0,
            "only_preview": false,
        },
        "reply_markup": null,
        "input_message_content": input_content(req)?,
    }))
}

// ---------- error mapping ----------

/// `ALLOW_PAYMENT_REQUIRED_<stars>` → stars.
fn suffix_number(message: &str, prefix: &str) -> Option<i64> {
    let tail = message.split(prefix).nth(1)?;
    let digits: String = tail
        .trim_start_matches('_')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// A send error (answer to `sendMessage`, or `updateMessageSendFailed.error` with the failed
/// message's `sending_state`) → typed `TransportError`. Matches the MTProto codes and the
/// TDLib local texts known for these cases; anything else stays `Other` (shown with
/// Telegram's code and text, `TG_UPLOAD_FAILED`).
pub fn map_send_error(
    error: &Value,
    sending_state: &Value,
    media: MediaKind,
    limit: u64,
) -> TransportError {
    let mapped = super::auth::map_error(error);
    let required = int_or_zero(sending_state, "required_paid_message_star_count");
    let TransportError::Other { code, message } = mapped else {
        return mapped;
    };
    // 429 without a readable "retry after X": the failed state carries `retry_after`.
    if code == 429
        && let Some(seconds) = sending_state["retry_after"].as_f64()
        && seconds.is_finite()
        && seconds > 0.0
    {
        return TransportError::FloodWait {
            retry_after: Duration::from_secs_f64(seconds.ceil().min(1e9)),
        };
    }
    let upper = message.to_ascii_uppercase();
    let forbidden = |f| TransportError::WriteForbidden(f);
    if required > 0 || upper.contains("ALLOW_PAYMENT_REQUIRED") {
        let stars = if required > 0 {
            required
        } else {
            suffix_number(&upper, "ALLOW_PAYMENT_REQUIRED").unwrap_or(0)
        };
        return forbidden(Forbidden::PaidMessages { stars });
    }
    let has = |needle: &str| upper.contains(needle);
    if has("TOPIC_CLOSED") {
        return forbidden(Forbidden::TopicClosed);
    }
    if has("USER_BANNED_IN_CHANNEL") {
        return forbidden(Forbidden::Banned { until_unix: None });
    }
    if has("CHAT_SEND_VIDEOS_FORBIDDEN") || has("RIGHTS TO SEND VIDEOS") {
        return forbidden(Forbidden::MediaNotAllowed(MediaKind::Video));
    }
    if has("CHAT_SEND_PHOTOS_FORBIDDEN") || has("RIGHTS TO SEND PHOTOS") {
        return forbidden(Forbidden::MediaNotAllowed(MediaKind::Photo));
    }
    if has("CHAT_SEND_DOCS_FORBIDDEN") || has("RIGHTS TO SEND DOCUMENTS") {
        return forbidden(Forbidden::MediaNotAllowed(MediaKind::Document));
    }
    if has("CHAT_SEND_MEDIA_FORBIDDEN") {
        return forbidden(Forbidden::MediaNotAllowed(media));
    }
    if has("CHAT_ADMIN_REQUIRED") {
        return forbidden(Forbidden::AdminsOnly);
    }
    if has("CHAT_RESTRICTED") {
        return forbidden(Forbidden::Restricted { until_unix: None });
    }
    if has("CHAT_WRITE_FORBIDDEN") || has("CHANNEL_PRIVATE") {
        return forbidden(Forbidden::NotMember);
    }
    if has("FILE_PARTS_INVALID")
        || has("FILE_TOO_BIG")
        || has("TOO BIG FILE")
        || has("FILE IS TOO BIG")
    {
        return TransportError::FileTooLarge { limit };
    }
    TransportError::Other { code, message }
}

// ---------- upload state ----------

/// File id of the main file of a message content (the one whose `updateFile` is progress).
pub fn main_file_id(content: &Value) -> Option<i64> {
    match j::kind(content) {
        "messageVideo" => int(&content["video"]["video"], "id").ok(),
        "messageDocument" => int(&content["document"]["document"], "id").ok(),
        "messagePhoto" => content["photo"]["sizes"]
            .as_array()?
            .iter()
            .filter_map(|size| {
                let file = &size["photo"];
                let bytes = j::size(file, "size").max(j::size(file, "expected_size"));
                int(file, "id").ok().map(|id| (bytes, id))
            })
            .max()
            .map(|(_, id)| id),
        _ => None,
    }
}

#[derive(Debug)]
enum Event {
    Nothing,
    Progress(Progress),
    Succeeded(Result<Sent, TransportError>),
    Failed {
        error: Value,
        sending_state: Value,
        failed_id: i64,
    },
    /// The temporary message was deleted (by our cancel, or irrecoverably by TDLib).
    Deleted,
}

struct Tracker {
    chat_id: i64,
    temp_id: i64,
    file_id: Option<i64>,
    total: u64,
    file_name: String,
    size: u64,
    awaiting: bool,
}

impl Tracker {
    fn on_update(&mut self, update: &Value) -> Event {
        match j::kind(update) {
            "updateFile" => {
                let file = &update["file"];
                if self.file_id.is_none() || int(file, "id").ok() != self.file_id {
                    return Event::Nothing;
                }
                let total = if self.total > 0 {
                    self.total
                } else {
                    j::size(file, "size").max(j::size(file, "expected_size"))
                };
                let remote = &file["remote"];
                if j::flag(remote, "is_uploading_completed") {
                    if self.awaiting {
                        return Event::Nothing;
                    }
                    self.awaiting = true;
                    return Event::Progress(Progress {
                        phase: Phase::AwaitingServer,
                        sent: total,
                        total,
                    });
                }
                Event::Progress(Progress {
                    phase: Phase::Uploading,
                    sent: j::size(remote, "uploaded_size").min(total.max(1)),
                    total,
                })
            }
            "updateMessageSendSucceeded" => {
                let message = &update["message"];
                if int_or_zero(update, "old_message_id") != self.temp_id
                    || int_or_zero(message, "chat_id") != self.chat_id
                {
                    return Event::Nothing;
                }
                // Malformed success: in doubt (`unknown`), never "not sent".
                let sent = int(message, "id").and_then(|message_id| {
                    Ok(Sent {
                        message_id,
                        date_unix: int(message, "date")?,
                        file_name: self.file_name.clone(),
                        size: self.size,
                    })
                });
                Event::Succeeded(sent)
            }
            "updateMessageSendFailed" => {
                if int_or_zero(update, "old_message_id") != self.temp_id
                    || int_or_zero(&update["message"], "chat_id") != self.chat_id
                {
                    return Event::Nothing;
                }
                let message = &update["message"];
                Event::Failed {
                    error: update["error"].clone(),
                    sending_state: message["sending_state"].clone(),
                    failed_id: int(message, "id").unwrap_or(self.temp_id),
                }
            }
            "updateDeleteMessages" => {
                let ours = int_or_zero(update, "chat_id") == self.chat_id
                    && !j::flag(update, "from_cache")
                    && update["message_ids"].as_array().is_some_and(|ids| {
                        ids.iter().any(|id| j::int_value(id) == Some(self.temp_id))
                    });
                if ours { Event::Deleted } else { Event::Nothing }
            }
            _ => Event::Nothing,
        }
    }
}

/// Journal lines of one upload: sizes, durations, codes; never a raw chat id, never the
/// caption.
struct UploadLog {
    chat: String,
    started: Instant,
    last: Option<Instant>,
    media: MediaKind,
    /// Size limit of the account, for `TG_FILE_TOO_LARGE`.
    limit: u64,
}

impl UploadLog {
    fn progress(&mut self, progress: &Progress) {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|at| now.duration_since(at) < PROGRESS_LOG_EVERY)
        {
            return;
        }
        self.last = Some(now);
        let secs = self.started.elapsed().as_secs_f64();
        tracing::info!(
            event = "upload_progress",
            chat = self.chat.as_str(),
            sent = progress.sent,
            total = progress.total,
            percent = if progress.total == 0 {
                0.0
            } else {
                (progress.sent as f64 * 1000.0 / progress.total as f64).round() / 10.0
            },
            bytes_per_sec = if secs > 0.0 {
                (progress.sent as f64 / secs).round()
            } else {
                0.0
            },
            "upload progress"
        );
    }
}

impl<B: TdBindings> Inner<B> {
    /// `deleteMessages(chat_id, [id], revoke = true)` under the request timeout.
    async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<(), TransportError> {
        self.request(
            "deleteMessages",
            json!({ "@type": "deleteMessages", "chat_id": chat_id, "message_ids": [message_id], "revoke": true }),
            self.config.request_timeout,
        )
        .await
        .map(|_| ())
    }

    /// `updateMessageSendFailed`: the failed message never reached Telegram; it is deleted
    /// locally (no `resendMessages`, ever) and the error is typed.
    async fn send_failed(
        &self,
        log: &UploadLog,
        chat_id: i64,
        failed_id: i64,
        error: &Value,
        sending_state: &Value,
    ) -> TransportError {
        let mapped = map_send_error(error, sending_state, log.media, log.limit);
        let deleted = self.delete_message(chat_id, failed_id).await;
        let (code, _) = mapped.code_and_params();
        tracing::warn!(
            event = "upload_send_failed",
            chat = log.chat.as_str(),
            dur_ms = crate::logging::ms_since(log.started),
            error_code = code,
            tg_code = int_or_zero(error, "code"),
            tg_message = error["message"].as_str().unwrap_or_default(),
            retry_after_s = match &mapped {
                TransportError::FloodWait { retry_after } => Some(retry_after.as_secs()),
                _ => None,
            },
            failed_message_deleted = deleted.is_ok(),
            "Telegram refused the message; nothing was sent"
        );
        mapped
    }

    pub(super) async fn upload(
        &self,
        req: UploadRequest,
        progress: mpsc::Sender<Progress>,
        cancel: CancellationToken,
    ) -> Result<Sent, TransportError> {
        if cancel.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        let media = req.payload.media_kind();
        let metadata = std::fs::metadata(&req.path)
            .map_err(|error| TransportError::FileUnreadable(error.kind().to_string()))?;
        if !metadata.is_file() {
            return Err(TransportError::FileUnreadable("not a file".into()));
        }
        let me = self.cached_me().await?;
        // The effective limit of the plan when given (custom or forced), else the account's.
        let limit = req.limit_bytes.unwrap_or(me.upload_limit_bytes);
        if req.size > limit {
            return Err(TransportError::FileTooLarge { limit });
        }
        let chat = self.known_chat(&req.target.peer).await?;
        let chat_id = chat_id(&chat)?;
        let check = self
            .check_rights(&chat, &req.target, media, me.user_id, false)
            .await?;
        if let Verdict::Denied(forbidden) = check.verdict {
            return Err(TransportError::WriteForbidden(forbidden));
        }
        if cancel.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        let body = send_message_body(chat_id, &req)?;
        let report = |p: Progress| {
            let _ = progress.try_send(p);
        };
        report(Progress {
            phase: Phase::Preparing,
            sent: 0,
            total: req.size,
        });
        let mut log = UploadLog {
            chat: redact(chat_id),
            started: Instant::now(),
            last: None,
            media,
            limit,
        };
        // Subscribed before `sendMessage`: no update about this message can be missed.
        let mut updates = self.listeners.subscribe();
        let mut auth = self.auth.subscribe();
        // Never abandoned half-way: a cancel during this short request is handled after it.
        let message = match self
            .request("sendMessage", body, self.config.request_timeout)
            .await
        {
            Ok(message) => message,
            Err(TransportError::Timeout(what)) => return Err(TransportError::Timeout(what)),
            Err(error) => {
                // Refused before any temporary message: nothing was sent.
                let error = match error {
                    TransportError::Other { code, message } => map_send_error(
                        &json!({ "@type": "error", "code": code, "message": message }),
                        &Value::Null,
                        media,
                        limit,
                    ),
                    other => other,
                };
                tracing::warn!(
                    event = "upload_send_failed",
                    chat = log.chat.as_str(),
                    dur_ms = crate::logging::ms_since(log.started),
                    error_code = error.code_and_params().0,
                    step = "sendMessage",
                    "sendMessage refused; nothing was sent"
                );
                return Err(error);
            }
        };
        j::expect(&message, "message")?;
        let temp_id = int(&message, "id")?;
        if int(&message, "chat_id")? != chat_id {
            return Err(TransportError::Protocol(
                "sendMessage: message of another chat".into(),
            ));
        }
        let file_id = main_file_id(&message["content"]);
        let file_name = req
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut tracker = Tracker {
            chat_id,
            temp_id,
            file_id,
            total: req.size,
            file_name,
            size: req.size,
            awaiting: false,
        };
        tracing::info!(
            event = "upload_accepted",
            chat = log.chat.as_str(),
            media = media.as_str(),
            size = req.size,
            topic = req.target.topic_id.is_some(),
            has_thumbnail = matches!(
                &req.payload,
                Payload::Video {
                    thumbnail: Some(_),
                    ..
                } | Payload::Document { thumbnail: Some(_) }
            ),
            caption_chars = req.caption.text.chars().count(),
            file_tracked = file_id.is_some(),
            dur_ms = crate::logging::ms_since(log.started),
            "temporary message created, upload running"
        );
        loop {
            tokio::select! {
                // Updates first: a success already received always wins over a cancel.
                biased;
                update = updates.recv() => {
                    let Some(update) = update else {
                        return Err(TransportError::Protocol("TDLib stopped during the upload".into()));
                    };
                    match tracker.on_update(&update) {
                        Event::Nothing => {}
                        Event::Progress(p) => {
                            if p.phase == Phase::AwaitingServer {
                                tracing::info!(
                                    event = "upload_uploaded",
                                    chat = log.chat.as_str(),
                                    size = p.total,
                                    dur_ms = crate::logging::ms_since(log.started),
                                    "all bytes uploaded, waiting for the server"
                                );
                            } else {
                                log.progress(&p);
                            }
                            report(p);
                        }
                        Event::Succeeded(sent) => return sent,
                        Event::Failed { error, sending_state, failed_id } => {
                            return Err(self
                                .send_failed(&log, chat_id, failed_id, &error, &sending_state)
                                .await);
                        }
                        Event::Deleted => {
                            return Err(TransportError::Protocol(
                                "the message was deleted while being sent".into(),
                            ));
                        }
                    }
                }
                () = cancel.cancelled() => {
                    return self.cancel_upload(&log, &mut tracker, &mut updates).await;
                }
                changed = auth.changed() => {
                    let closed = changed.is_err()
                        || matches!(*auth.borrow_and_update(), AuthState::Closed | AuthState::LoggingOut);
                    if closed {
                        // The message may have gone out just before: in doubt.
                        return Err(TransportError::Protocol("TDLib closed during the upload".into()));
                    }
                }
            }
        }
    }

    /// Cancel: a success already queued wins (`Ok`, never a delete); otherwise
    /// `deleteMessages` on the temporary id, then a short grace for a success that crossed it.
    async fn cancel_upload(
        &self,
        log: &UploadLog,
        tracker: &mut Tracker,
        updates: &mut super::Subscription,
    ) -> Result<Sent, TransportError> {
        let mut queued = VecDeque::new();
        while let Ok(update) = updates.try_recv() {
            queued.push_back(update);
        }
        for update in queued {
            match tracker.on_update(&update) {
                Event::Succeeded(sent) => {
                    tracing::info!(
                        event = "upload_cancel",
                        step = "too_late",
                        chat = log.chat.as_str(),
                        "cancel arrived after the message was sent"
                    );
                    return sent;
                }
                Event::Failed {
                    error,
                    sending_state,
                    failed_id,
                } => {
                    return Err(self
                        .send_failed(log, tracker.chat_id, failed_id, &error, &sending_state)
                        .await);
                }
                _ => {}
            }
        }
        let deleted = self.delete_message(tracker.chat_id, tracker.temp_id).await;
        let grace = tokio::time::sleep(self.config.cancel_grace);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                biased;
                update = updates.recv() => {
                    let Some(update) = update else { break };
                    match tracker.on_update(&update) {
                        Event::Succeeded(sent) => {
                            tracing::info!(event = "upload_cancel", step = "too_late", chat = log.chat.as_str(), "the message went out before the cancel took effect");
                            return sent;
                        }
                        Event::Failed { .. } | Event::Deleted => break,
                        _ => {}
                    }
                }
                () = &mut grace => break,
            }
        }
        match deleted {
            Ok(()) => {
                tracing::info!(
                    event = "upload_cancel",
                    step = "deleted",
                    chat = log.chat.as_str(),
                    dur_ms = crate::logging::ms_since(log.started),
                    "temporary message deleted, upload cancelled"
                );
                Err(TransportError::Cancelled)
            }
            Err(error) => {
                let code = error.code_and_params().0;
                tracing::warn!(
                    event = "upload_cancel",
                    step = "delete_failed",
                    chat = log.chat.as_str(),
                    error_code = code,
                    error = %error,
                    "deleteMessages failed: the message may have been sent"
                );
                Err(TransportError::Protocol(format!(
                    "cancel: deleteMessages failed ({code})"
                )))
            }
        }
    }

    /// `message_link`, read only. Only supergroups and channels have message links: anything
    /// else is `None` without a request. `getMessageLink` (`chat_id`, `message_id`,
    /// `media_timestamp`, `checklist_task_id`, `poll_option_id`, `for_album`,
    /// `in_message_thread`) only works on a message TDLib holds, and without a message
    /// database one sent in an earlier session is not held: `getMessage` loads it first. A
    /// message deleted since (404) or one TDLib refuses a link for (400)
    /// is `None`; anything else is an error. The link itself is never logged.
    pub(super) async fn message_link(
        &self,
        target: &Target,
        message_id: i64,
    ) -> Result<Option<String>, TransportError> {
        if target.peer.kind != PeerKind::Channel {
            return Ok(None);
        }
        let chat_id = chat_id(&self.known_chat(&target.peer).await?)?;
        let timeout = self.config.request_timeout;
        let unavailable = |error: &TransportError| {
            matches!(
                error,
                TransportError::Other {
                    code: 400 | 404,
                    ..
                }
            )
        };
        let found = match self
            .request(
                "getMessage",
                json!({ "@type": "getMessage", "chat_id": chat_id, "message_id": message_id }),
                timeout,
            )
            .await
        {
            Err(error) if unavailable(&error) => None,
            answer => Some(answer?),
        };
        let link = match found {
            None => None,
            Some(_) => match self
                .request(
                    "getMessageLink",
                    json!({
                        "@type": "getMessageLink",
                        "chat_id": chat_id,
                        "message_id": message_id,
                        "media_timestamp": 0,
                        "checklist_task_id": 0,
                        "poll_option_id": "",
                        "for_album": false,
                        // A forum topic's message is linked inside its topic.
                        "in_message_thread": target.topic_id.is_some(),
                    }),
                    timeout,
                )
                .await
            {
                Err(error) if unavailable(&error) => None,
                answer => answer?["link"]
                    .as_str()
                    .filter(|link| link.starts_with("https://t.me/"))
                    .map(str::to_owned),
            },
        };
        tracing::debug!(
            event = "message_link",
            chat = redact(chat_id).as_str(),
            found = found.is_some(),
            linked = link.is_some(),
            "message link read"
        );
        Ok(link)
    }

    /// `verify_sent`, read only: `searchChatMessages` (filter by media kind, the topic, the
    /// sender: the account, the identity chosen to post in the chat, or nobody in a channel),
    /// newest first, until a message older than `not_before`.
    pub(super) async fn verify_sent(
        &self,
        probe: &SentProbe,
    ) -> Result<Vec<SentCandidate>, TransportError> {
        let started = Instant::now();
        let me = self.cached_me().await?;
        let chat = self.known_chat(&probe.target.peer).await?;
        let chat_id = chat_id(&chat)?;
        let channel = matches!(
            ChatType::parse(&chat)?,
            ChatType::Supergroup {
                is_channel: true,
                ..
            }
        );
        let chosen = &chat["message_sender_id"];
        let (sender, sender_filter) = if channel {
            // Posts are signed by the channel: `sender_id = me` finds nothing.
            (Value::Null, "none")
        } else if j::kind(chosen) == "messageSenderChat" {
            (chosen.clone(), "chat")
        } else {
            (
                json!({ "@type": "messageSenderUser", "user_id": me.user_id }),
                "me",
            )
        };
        let filter = match probe.media {
            MediaKind::Video => "searchMessagesFilterVideo",
            MediaKind::Photo => "searchMessagesFilterPhoto",
            MediaKind::Document => "searchMessagesFilterDocument",
        };
        let mut from = 0i64;
        let mut candidates = Vec::new();
        let (mut pages, mut scanned) = (0usize, 0usize);
        'pages: while pages < MAX_SEARCH_PAGES {
            pages += 1;
            let found = self
                .request(
                    "searchChatMessages",
                    json!({
                        "@type": "searchChatMessages",
                        "chat_id": chat_id,
                        "topic_id": message_topic(&probe.target),
                        "query": "",
                        "sender_id": sender,
                        "from_message_id": from,
                        "offset": 0,
                        "limit": SEARCH_PAGE,
                        "filter": { "@type": filter },
                    }),
                    self.config.request_timeout,
                )
                .await?;
            j::expect(&found, "foundChatMessages")?;
            let messages = j::array(&found, "messages")?;
            for message in messages {
                scanned += 1;
                let date = int(message, "date")?;
                if date < probe.not_before_unix {
                    break 'pages;
                }
                if let Some(candidate) = candidate(message, probe, date)? {
                    candidates.push(candidate);
                }
            }
            from = int_or_zero(&found, "next_from_message_id");
            if messages.is_empty() || from == 0 {
                break;
            }
        }
        tracing::info!(
            event = "verify_search",
            chat = redact(chat_id).as_str(),
            media = probe.media.as_str(),
            sender_filter,
            topic = probe.target.topic_id.is_some(),
            pages,
            scanned,
            candidates = candidates.len(),
            dur_ms = crate::logging::ms_since(started),
            "reconciliation search (read only)"
        );
        Ok(candidates)
    }
}

/// A found message → candidate if it is of the probe's kind and matches: name and size for a
/// video or document (size 0 in the probe = unknown, name only), anything for a photo (the
/// server re-encodes it and drops the name).
fn candidate(
    message: &Value,
    probe: &SentProbe,
    date: i64,
) -> Result<Option<SentCandidate>, TransportError> {
    let content = &message["content"];
    let (kind, name, size) = match j::kind(content) {
        "messageVideo" => {
            let video = &content["video"];
            let file = &video["video"];
            (
                MediaKind::Video,
                video["file_name"].as_str().map(str::to_owned),
                Some(j::size(file, "size").max(j::size(file, "expected_size"))),
            )
        }
        "messageDocument" => {
            let document = &content["document"];
            let file = &document["document"];
            (
                MediaKind::Document,
                document["file_name"].as_str().map(str::to_owned),
                Some(j::size(file, "size").max(j::size(file, "expected_size"))),
            )
        }
        "messagePhoto" => (MediaKind::Photo, None, None),
        _ => return Ok(None),
    };
    if kind != probe.media {
        return Ok(None);
    }
    if kind != MediaKind::Photo
        && let Some(wanted) = &probe.file_name
    {
        let same_name = name.as_deref() == Some(wanted.as_str());
        let same_size = probe.size == 0 || size == Some(probe.size);
        if !(same_name && same_size) {
            return Ok(None);
        }
    }
    Ok(Some(SentCandidate {
        message_id: int(message, "id")?,
        date_unix: date,
        kind,
        file_name: name,
        size,
        caption_preview: content["caption"]["text"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .take(CAPTION_PREVIEW_CHARS)
            .collect(),
    }))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::transport::{Entity, Peer, PeerKind};

    fn error(code: i32, message: &str) -> Value {
        json!({ "@type": "error", "code": code, "message": message })
    }

    fn map(code: i32, message: &str) -> TransportError {
        map_send_error(&error(code, message), &Value::Null, MediaKind::Video, 42)
    }

    fn reason(e: &TransportError) -> (&'static str, Value) {
        e.code_and_params()
    }

    #[test]
    fn send_errors_are_typed() {
        assert!(matches!(
            map(429, "Too Many Requests: retry after 95"),
            TransportError::FloodWait { retry_after } if retry_after == Duration::from_secs(95)
        ));
        // 429 without a delay in the text: the failed state's `retry_after`.
        let state = json!({ "@type": "messageSendingStateFailed", "retry_after": 70.2,
            "required_paid_message_star_count": 0 });
        assert!(matches!(
            map_send_error(&error(429, "Too Many Requests"), &state, MediaKind::Video, 1),
            TransportError::FloodWait { retry_after } if retry_after == Duration::from_secs(71)
        ));
        assert!(matches!(
            map(429, "Too Many Requests"),
            TransportError::Other { code: 429, .. }
        ));
        assert!(matches!(
            map(401, "Unauthorized"),
            TransportError::NotAuthorized
        ));
        assert_eq!(
            reason(&map(400, "ALLOW_PAYMENT_REQUIRED_250")),
            ("TG_PAID_MESSAGES", json!({ "stars": 250 }))
        );
        let paid =
            json!({ "@type": "messageSendingStateFailed", "required_paid_message_star_count": 30 });
        assert_eq!(
            reason(&map_send_error(
                &error(400, "x"),
                &paid,
                MediaKind::Photo,
                1
            )),
            ("TG_PAID_MESSAGES", json!({ "stars": 30 }))
        );
        for (text, expected) in [
            ("TOPIC_CLOSED", json!({ "reason": "topic_closed" })),
            (
                "USER_BANNED_IN_CHANNEL",
                json!({ "reason": "banned", "untilUnix": null }),
            ),
            (
                "CHAT_SEND_PHOTOS_FORBIDDEN",
                json!({ "reason": "media_not_allowed", "media": "photo" }),
            ),
            (
                "Not enough rights to send videos to the chat",
                json!({ "reason": "media_not_allowed", "media": "video" }),
            ),
            (
                "CHAT_SEND_DOCS_FORBIDDEN",
                json!({ "reason": "media_not_allowed", "media": "document" }),
            ),
            (
                "CHAT_SEND_MEDIA_FORBIDDEN",
                json!({ "reason": "media_not_allowed", "media": "video" }),
            ),
            ("CHAT_ADMIN_REQUIRED", json!({ "reason": "admins_only" })),
            (
                "CHAT_RESTRICTED",
                json!({ "reason": "restricted", "untilUnix": null }),
            ),
            ("CHAT_WRITE_FORBIDDEN", json!({ "reason": "not_member" })),
        ] {
            assert_eq!(
                reason(&map(403, text)),
                ("TG_WRITE_FORBIDDEN", expected),
                "{text}"
            );
        }
        for text in ["FILE_PARTS_INVALID", "Too big file", "FILE_TOO_BIG"] {
            assert_eq!(
                reason(&map(400, text)),
                ("TG_FILE_TOO_LARGE", json!({ "limit": 42 })),
                "{text}"
            );
        }
        assert!(matches!(
            map(400, "MEDIA_EMPTY"),
            TransportError::Other { code: 400, message } if message == "MEDIA_EMPTY"
        ));
        assert!(matches!(
            map_send_error(
                &json!({ "@type": "error" }),
                &Value::Null,
                MediaKind::Video,
                1
            ),
            TransportError::Protocol(_)
        ));
    }

    fn request(payload: Payload, topic_id: Option<i32>) -> UploadRequest {
        UploadRequest {
            path: PathBuf::from(r"C:\clips\a_sq.mp4"),
            size: 10,
            target: Target {
                peer: Peer {
                    kind: PeerKind::Channel,
                    raw_id: 5,
                },
                topic_id,
            },
            payload,
            caption: Caption {
                text: "héllo 👋 bold".into(),
                entities: vec![
                    Entity {
                        offset_utf16: 9,
                        length_utf16: 4,
                        kind: EntityKind::Bold,
                    },
                    Entity {
                        offset_utf16: 0,
                        length_utf16: 5,
                        kind: EntityKind::TextUrl("https://example.org".into()),
                    },
                ],
            },
            limit_bytes: None,
        }
    }

    #[test]
    fn bodies_follow_the_1_8_68_schema() {
        let video = request(
            Payload::Video {
                duration_s: 61,
                width: 1920,
                height: 1080,
                supports_streaming: true,
                thumbnail: Some(Thumbnail {
                    path: PathBuf::from(r"C:\clips\a.jpg"),
                    width: 320,
                    height: 180,
                }),
            },
            Some(7),
        );
        let body = send_message_body(-1_000_000_000_005, &video).unwrap();
        assert_eq!(body["@type"], "sendMessage");
        assert_eq!(body["chat_id"], -1_000_000_000_005i64);
        assert_eq!(
            body["topic_id"],
            json!({ "@type": "messageTopicForum", "forum_topic_id": 7 })
        );
        assert_eq!(body["options"]["@type"], "messageSendOptions");
        assert_eq!(body["options"]["paid_message_star_count"], 0);
        assert_eq!(body["options"]["only_preview"], false);
        let content = &body["input_message_content"];
        assert_eq!(content["@type"], "inputMessageVideo");
        let inner = &content["video"];
        assert_eq!(inner["@type"], "inputVideo");
        assert_eq!(inner["video"]["@type"], "inputFileLocal");
        assert_eq!(inner["video"]["path"], r"C:\clips\a_sq.mp4");
        assert_eq!(inner["thumbnail"]["@type"], "inputThumbnail");
        assert_eq!(inner["thumbnail"]["thumbnail"]["path"], r"C:\clips\a.jpg");
        assert_eq!(
            (&inner["duration"], &inner["width"], &inner["height"]),
            (&json!(61), &json!(1920), &json!(1080))
        );
        assert_eq!(inner["supports_streaming"], true);
        assert_eq!(inner["cover"], Value::Null);
        assert_eq!(content["caption"]["@type"], "formattedText");
        assert_eq!(content["caption"]["text"], "héllo 👋 bold");
        let entities = content["caption"]["entities"].as_array().unwrap();
        assert_eq!(entities[0]["type"]["@type"], "textEntityTypeBold");
        assert_eq!(
            (&entities[0]["offset"], &entities[0]["length"]),
            (&json!(9), &json!(4))
        );
        assert_eq!(entities[1]["type"]["url"], "https://example.org");
        // No flat 1.8.61 fields left on the message content.
        assert!(content.get("duration").is_none() && content.get("thumbnail").is_none());

        let document =
            send_message_body(1, &request(Payload::Document { thumbnail: None }, None)).unwrap();
        let content = &document["input_message_content"];
        assert_eq!(document["topic_id"], Value::Null);
        assert_eq!(content["@type"], "inputMessageDocument");
        assert_eq!(content["document"]["@type"], "inputDocument");
        assert_eq!(content["document"]["document"]["@type"], "inputFileLocal");
        assert_eq!(content["document"]["thumbnail"], Value::Null);
        assert_eq!(content["document"]["disable_content_type_detection"], true);

        let photo = send_message_body(
            1,
            &request(
                Payload::Photo {
                    width: 800,
                    height: 600,
                },
                None,
            ),
        )
        .unwrap();
        let content = &photo["input_message_content"];
        assert_eq!(content["@type"], "inputMessagePhoto");
        assert_eq!(content["photo"]["@type"], "inputPhoto");
        assert_eq!(content["photo"]["photo"]["@type"], "inputFileLocal");
        assert_eq!(
            (&content["photo"]["width"], &content["photo"]["height"]),
            (&json!(800), &json!(600))
        );
        assert_eq!(content["photo"]["video"], Value::Null);
    }

    #[test]
    fn main_file_of_each_content() {
        let video = json!({ "@type": "messageVideo", "video": { "@type": "video", "video": { "@type": "file", "id": 11 } } });
        assert_eq!(main_file_id(&video), Some(11));
        let document =
            json!({ "@type": "messageDocument", "document": { "document": { "id": 12 } } });
        assert_eq!(main_file_id(&document), Some(12));
        let photo = json!({ "@type": "messagePhoto", "photo": { "sizes": [
            { "type": "m", "photo": { "id": 13, "size": 100, "expected_size": 100 } },
            { "type": "y", "photo": { "id": 14, "size": 0, "expected_size": 5000 } },
        ] } });
        assert_eq!(main_file_id(&photo), Some(14));
        assert_eq!(main_file_id(&json!({ "@type": "messageText" })), None);
    }

    #[test]
    fn tracker_reports_progress_awaiting_server_and_only_its_success() {
        let mut tracker = Tracker {
            chat_id: 5,
            temp_id: 100,
            file_id: Some(11),
            total: 1000,
            file_name: "a.mp4".into(),
            size: 1000,
            awaiting: false,
        };
        let file = |id: i64, uploaded: u64, done: bool| {
            json!({ "@type": "updateFile", "file": { "@type": "file", "id": id, "size": 1000,
                "expected_size": 1000, "remote": { "@type": "remoteFile", "is_uploading_active": !done,
                "is_uploading_completed": done, "uploaded_size": uploaded } } })
        };
        assert!(matches!(
            tracker.on_update(&file(99, 10, false)),
            Event::Nothing
        ));
        assert!(matches!(
            tracker.on_update(&file(11, 400, false)),
            Event::Progress(Progress {
                phase: Phase::Uploading,
                sent: 400,
                total: 1000
            })
        ));
        assert!(matches!(
            tracker.on_update(&file(11, 1000, true)),
            Event::Progress(Progress {
                phase: Phase::AwaitingServer,
                sent: 1000,
                ..
            })
        ));
        assert!(matches!(
            tracker.on_update(&file(11, 1000, true)),
            Event::Nothing
        ));
        let other = json!({ "@type": "updateMessageSendSucceeded", "old_message_id": 101,
            "message": { "@type": "message", "id": 7, "chat_id": 5, "date": 1 } });
        assert!(matches!(tracker.on_update(&other), Event::Nothing));
        let ours = json!({ "@type": "updateMessageSendSucceeded", "old_message_id": 100,
            "message": { "@type": "message", "id": 7, "chat_id": 5, "date": 1 } });
        assert!(matches!(
            tracker.on_update(&ours),
            Event::Succeeded(Ok(Sent {
                message_id: 7,
                date_unix: 1,
                ..
            }))
        ));
        let malformed = json!({ "@type": "updateMessageSendSucceeded", "old_message_id": 100,
            "message": { "@type": "message", "chat_id": 5 } });
        assert!(matches!(
            tracker.on_update(&malformed),
            Event::Succeeded(Err(TransportError::Protocol(_)))
        ));
        let cached = json!({ "@type": "updateDeleteMessages", "chat_id": 5, "message_ids": [100],
            "is_permanent": false, "from_cache": true });
        assert!(matches!(tracker.on_update(&cached), Event::Nothing));
        let deleted = json!({ "@type": "updateDeleteMessages", "chat_id": 5, "message_ids": [100],
            "is_permanent": true, "from_cache": false });
        assert!(matches!(tracker.on_update(&deleted), Event::Deleted));
    }
}
