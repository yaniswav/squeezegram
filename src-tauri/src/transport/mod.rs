//! Contract between the orchestrator and Telegram. No TDLib type leaves this module.
//!
//! - `stage`: the `SendStage` that drives a transport for the orchestrator.
//! - `service`: destinations cache, pre-send check, reconciliation, `auth://state` payload.
//! - `mock`: deterministic `MockTransport`, compiled in tests and debug builds only.
//! - `tdlib`: `TdlibJson`, the TDLib transport over `tdjson` (login, chats, upload, shutdown).
//! - `proxy`: the proxy every connection goes through (SOCKS5, HTTP, MTProto), validated.
//! - `select`: which transport the app uses (`SQUEEZEGRAM_TRANSPORT`, identity, DLL).
//! - `identity_settings`: the device name and personal API identity Settings shows and edits.
//! - `NoTransport`: no Telegram at all; refuses everything, with the reason when there is one.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub mod identity_settings;
#[cfg(any(test, debug_assertions))]
pub mod mock;
pub mod proxy;
pub mod select;
pub mod service;
pub mod stage;
pub mod tdlib;

// ---------- Destination identity (transport-neutral) ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeerKind {
    User,
    BasicGroup,
    /// Supergroup, forum or channel: same MTProto id space.
    Channel,
}

impl PeerKind {
    /// Value of `destinations.peer_kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            PeerKind::User => "user",
            PeerKind::BasicGroup => "basic_group",
            PeerKind::Channel => "channel",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(PeerKind::User),
            "basic_group" => Some(PeerKind::BasicGroup),
            "channel" => Some(PeerKind::Channel),
            _ => None,
        }
    }
}

/// Raw MTProto id (positive), as stored in the database. A transport's own `chat_id` is
/// derived from it; only `Peer` is ever persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Peer {
    pub kind: PeerKind,
    pub raw_id: i64,
}

impl Peer {
    /// TDLib convention (`DialogId.h`, `ZERO_CHANNEL_ID = -1000000000000`).
    pub fn tdlib_chat_id(&self) -> i64 {
        match self.kind {
            PeerKind::User => self.raw_id,
            PeerKind::BasicGroup => -self.raw_id,
            PeerKind::Channel => -1_000_000_000_000 - self.raw_id,
        }
    }
}

/// Whole chat (`topic_id = None`) or forum topic (`forumTopicInfo.forum_topic_id`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub peer: Peer,
    pub topic_id: Option<i32>,
}

// ---------- Session and account ----------

const REDACTED: &str = "<redacted>";

#[derive(Clone)]
pub struct Me {
    pub user_id: i64,
    pub display_name: String,
    pub username: Option<String>,
    pub is_premium: bool,
    /// `getOption("message_caption_length_max")`.
    pub caption_max_chars: u32,
    /// Deduced: 4000 MiB with Premium, else 2000 MiB (no API gives it).
    pub upload_limit_bytes: u64,
}

/// Written by hand: identity fields never reach a log.
impl fmt::Debug for Me {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Me")
            .field("user_id", &REDACTED)
            .field("display_name", &REDACTED)
            .field("username", &self.username.as_ref().map(|_| REDACTED))
            .field("is_premium", &self.is_premium)
            .field("caption_max_chars", &self.caption_max_chars)
            .field("upload_limit_bytes", &self.upload_limit_bytes)
            .finish()
    }
}

#[derive(Clone)]
pub enum AuthState {
    Starting,
    WaitPhone,
    /// To draw as a QR code; TDLib renews the link.
    WaitQr {
        link: String,
    },
    WaitCode {
        resend_in: Option<Duration>,
    },
    WaitEmail,
    WaitEmailCode,
    /// Two-step verification.
    WaitPassword {
        hint: String,
    },
    Ready(Me),
    LoggingOut,
    Closed,
    /// The start failed and left no client (the key or the database cannot be opened, the
    /// proxy is unusable…). Unlike `Closed`, the page says why and, when it may help, offers
    /// `reset_session`.
    StartFailed(TransportError),
    /// Any step the app does not handle (sign-up, Premium purchase required…): generic screen.
    OtherStep {
        name: String,
    },
}

/// Written by hand: the QR link is a login token.
impl fmt::Debug for AuthState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthState::Starting => f.write_str("Starting"),
            AuthState::WaitPhone => f.write_str("WaitPhone"),
            AuthState::WaitQr { .. } => f.write_str("WaitQr { link: <redacted> }"),
            AuthState::WaitCode { resend_in } => f
                .debug_struct("WaitCode")
                .field("resend_in", resend_in)
                .finish(),
            AuthState::WaitEmail => f.write_str("WaitEmail"),
            AuthState::WaitEmailCode => f.write_str("WaitEmailCode"),
            AuthState::WaitPassword { hint } => {
                f.debug_struct("WaitPassword").field("hint", hint).finish()
            }
            AuthState::Ready(me) => f.debug_tuple("Ready").field(me).finish(),
            AuthState::LoggingOut => f.write_str("LoggingOut"),
            AuthState::Closed => f.write_str("Closed"),
            // The code only: a proxy error's text names the proxy server.
            AuthState::StartFailed(error) => {
                write!(f, "StartFailed({})", error.code_and_params().0)
            }
            AuthState::OtherStep { name } => {
                f.debug_struct("OtherStep").field("name", name).finish()
            }
        }
    }
}

/// `auth_submit(input)`: `{ "kind": "phone", "value": "+33…" }`, `{ "kind": "request_qr" }`…
#[derive(Clone, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum AuthInput {
    Phone(String),
    RequestQr,
    Code(String),
    ResendCode,
    Email(String),
    EmailCode(String),
    Password(String),
}

impl AuthInput {
    /// The kind of input, the only thing the journal ever records about it.
    pub fn kind(&self) -> &'static str {
        match self {
            AuthInput::Phone(_) => "phone",
            AuthInput::RequestQr => "request_qr",
            AuthInput::Code(_) => "code",
            AuthInput::ResendCode => "resend_code",
            AuthInput::Email(_) => "email",
            AuthInput::EmailCode(_) => "email_code",
            AuthInput::Password(_) => "password",
        }
    }
}

/// Written by hand: phone number, codes, password and e-mail are never logged.
impl fmt::Debug for AuthInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            AuthInput::Phone(_) => "Phone",
            AuthInput::RequestQr => return f.write_str("RequestQr"),
            AuthInput::Code(_) => "Code",
            AuthInput::ResendCode => return f.write_str("ResendCode"),
            AuthInput::Email(_) => "Email",
            AuthInput::EmailCode(_) => "EmailCode",
            AuthInput::Password(_) => "Password",
        };
        f.debug_tuple(name).field(&REDACTED).finish()
    }
}

// ---------- Destinations ----------

/// What a chat is, for display and grouping (`destinations.kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChatKind {
    /// "Saved Messages", the chat with oneself.
    Saved,
    Private,
    BasicGroup,
    /// Supergroup or forum (`is_forum` tells them apart).
    Supergroup,
    /// Broadcast channel.
    Channel,
}

impl ChatKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatKind::Saved => "saved",
            ChatKind::Private => "private",
            ChatKind::BasicGroup => "basic_group",
            ChatKind::Supergroup => "supergroup",
            ChatKind::Channel => "channel",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "saved" => Some(ChatKind::Saved),
            "private" => Some(ChatKind::Private),
            "basic_group" => Some(ChatKind::BasicGroup),
            "supergroup" => Some(ChatKind::Supergroup),
            "channel" => Some(ChatKind::Channel),
            _ => None,
        }
    }
}

/// A Telegram chat folder (`chatFolderInfo`) a chat belongs to. The name is personal data:
/// shown on screen, never written to the journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatFolder {
    pub id: i32,
    pub name: String,
    /// Rank of the folder in the user's folder list (0 first).
    pub position: u32,
}

#[derive(Clone, Debug)]
pub struct Dialog {
    pub peer: Peer,
    pub title: String,
    pub username: Option<String>,
    pub is_forum: bool,
    /// Broadcast channel (`chatTypeSupergroup.is_channel`), for display.
    pub is_broadcast: bool,
    pub is_saved_messages: bool,
    pub kind: ChatKind,
    /// In the main list, the archive or a folder at this load; false for a chat found by
    /// search only.
    pub listed: bool,
    /// In `chatListArchive`.
    pub archived: bool,
    /// Folders (`chatListFolder`) holding the chat, in the user's folder order.
    pub folders: Vec<ChatFolder>,
    /// Approximate right to post media, from the data the list already has (no network
    /// request); `None` when unknown (private chats). `can_send` stays the real check.
    pub can_write: Option<bool>,
    /// Private chat of a deleted account (`userTypeDeleted`, empty title): returned by
    /// `list_dialogs` only so the cache can drop it; never a destination.
    pub deleted: bool,
}

#[derive(Clone, Debug)]
pub struct Topic {
    pub id: i32,
    pub title: String,
    pub is_general: bool,
    pub is_closed: bool,
}

// ---------- Pre-send check ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Photo,
    Document,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Video => "video",
            MediaKind::Photo => "photo",
            MediaKind::Document => "document",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Forbidden {
    NotMember,
    JoinRequired,
    Banned { until_unix: Option<i64> },
    Restricted { until_unix: Option<i64> },
    MediaNotAllowed(MediaKind),
    AdminsOnly,
    TopicClosed,
    PaidMessages { stars: i64 },
}

impl Forbidden {
    /// `TG_PAID_MESSAGES{stars}` or `TG_WRITE_FORBIDDEN{reason}`.
    pub fn code_and_params(&self) -> (&'static str, Value) {
        let reason = |reason: &str| json!({ "reason": reason });
        match self {
            Forbidden::PaidMessages { stars } => ("TG_PAID_MESSAGES", json!({ "stars": stars })),
            Forbidden::NotMember => ("TG_WRITE_FORBIDDEN", reason("not_member")),
            Forbidden::JoinRequired => ("TG_WRITE_FORBIDDEN", reason("join_required")),
            Forbidden::Banned { until_unix } => (
                "TG_WRITE_FORBIDDEN",
                json!({ "reason": "banned", "untilUnix": until_unix }),
            ),
            Forbidden::Restricted { until_unix } => (
                "TG_WRITE_FORBIDDEN",
                json!({ "reason": "restricted", "untilUnix": until_unix }),
            ),
            Forbidden::MediaNotAllowed(kind) => (
                "TG_WRITE_FORBIDDEN",
                json!({ "reason": "media_not_allowed", "media": kind.as_str() }),
            ),
            Forbidden::AdminsOnly => ("TG_WRITE_FORBIDDEN", reason("admins_only")),
            Forbidden::TopicClosed => ("TG_WRITE_FORBIDDEN", reason("topic_closed")),
        }
    }
}

/// There is no `Verdict::Unknown`: a check either allows or denies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    Denied(Forbidden),
}

#[derive(Clone, Debug)]
pub struct SlowMode {
    pub delay: Duration,
    /// Wait left at `SendCheck::checked_at` (TDLib does not notify its evolution).
    pub wait_remaining: Duration,
}

#[derive(Clone, Debug)]
pub struct SendCheck {
    pub verdict: Verdict,
    pub slow_mode: Option<SlowMode>,
    pub checked_at: SystemTime,
}

// ---------- Sending ----------

#[derive(Clone, Debug)]
pub struct Thumbnail {
    /// JPEG under 200 KB, 320 px at most.
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub enum Payload {
    Video {
        duration_s: u32,
        width: u32,
        height: u32,
        supports_streaming: bool,
        thumbnail: Option<Thumbnail>,
    },
    Photo {
        width: u32,
        height: u32,
    },
    Document {
        thumbnail: Option<Thumbnail>,
    },
}

impl Payload {
    pub fn media_kind(&self) -> MediaKind {
        match self {
            Payload::Video { .. } => MediaKind::Video,
            Payload::Photo { .. } => MediaKind::Photo,
            Payload::Document { .. } => MediaKind::Document,
        }
    }
}

#[derive(Clone, Debug)]
pub enum EntityKind {
    Bold,
    Italic,
    Underline,
    Strike,
    Spoiler,
    Code,
    TextUrl(String),
}

#[derive(Clone, Debug)]
pub struct Entity {
    pub offset_utf16: u32,
    pub length_utf16: u32,
    pub kind: EntityKind,
}

/// Formatted caption (TDLib `formattedText`): no HTML, nothing to escape.
#[derive(Clone, Debug, Default)]
pub struct Caption {
    pub text: String,
    pub entities: Vec<Entity>,
}

#[derive(Clone, Debug)]
pub struct UploadRequest {
    pub path: PathBuf,
    pub size: u64,
    pub target: Target,
    pub payload: Payload,
    pub caption: Caption,
    /// Size limit to enforce: the effective limit the plan used (`crate::limit`), so the
    /// upload and the plan agree. `None`: the account's (`Me::upload_limit_bytes`).
    pub limit_bytes: Option<u64>,
}

/// Copy a message Telegram already holds. The source and destination may be forum topics;
/// TDLib only needs the source chat and message id, while `target.topic_id` selects the
/// destination topic.
#[derive(Clone)]
pub struct CopyRequest {
    pub from: Target,
    pub message_id: i64,
    pub target: Target,
    pub media: MediaKind,
    pub caption: Caption,
    pub file_name: String,
    pub size: u64,
}

impl fmt::Debug for CopyRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CopyRequest")
            .field("from", &REDACTED)
            .field("message_id", &self.message_id)
            .field("target", &REDACTED)
            .field("media", &self.media)
            .field("caption_chars", &self.caption.text.chars().count())
            .field("file_name", &self.file_name)
            .field("size", &self.size)
            .finish()
    }
}

/// Send media again from Telegram's remote file cache, without reading or uploading a local
/// file. This is the fallback when Telegram refuses a message copy.
#[derive(Clone)]
pub struct RemoteRequest {
    pub remote_id: String,
    pub target: Target,
    pub payload: Payload,
    pub caption: Caption,
    pub file_name: String,
    pub size: u64,
}

impl fmt::Debug for RemoteRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteRequest")
            .field("remote_id", &REDACTED)
            .field("target", &REDACTED)
            .field("payload", &self.payload)
            .field("caption_chars", &self.caption.text.chars().count())
            .field("file_name", &self.file_name)
            .field("size", &self.size)
            .finish()
    }
}

/// There is no `Waiting { retry_after }` phase: the orchestrator carries the wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Preparing,
    Uploading,
    /// Bytes sent (`updateFile` with `remote.is_uploading_completed`), no
    /// `updateMessageSendSucceeded` yet.
    AwaitingServer,
}

#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub phase: Phase,
    pub sent: u64,
    pub total: u64,
}

#[derive(Clone)]
pub struct Sent {
    pub message_id: i64,
    pub date_unix: i64,
    pub file_name: String,
    pub size: u64,
    /// TDLib `remoteFile.id`; opaque and never logged. `None` if Telegram omitted it, in
    /// which case a later destination must copy the message or upload the local file.
    pub remote_id: Option<String>,
}

impl fmt::Debug for Sent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sent")
            .field("message_id", &self.message_id)
            .field("date_unix", &self.date_unix)
            .field("file_name", &self.file_name)
            .field("size", &self.size)
            .field("remote_id", &self.remote_id.as_ref().map(|_| REDACTED))
            .finish()
    }
}

/// Fingerprint for the reconciliation of an `unknown` item.
#[derive(Clone, Debug)]
pub struct SentProbe {
    pub target: Target,
    pub media: MediaKind,
    /// `None` for a photo: the server re-encodes it and drops the name.
    pub file_name: Option<String>,
    pub size: u64,
    pub not_before_unix: i64,
}

/// A message found on the server that may be the item; the user decides (`item_reconcile`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SentCandidate {
    pub message_id: i64,
    pub date_unix: i64,
    pub kind: MediaKind,
    pub file_name: Option<String>,
    pub size: Option<u64>,
    pub caption_preview: String,
}

// ---------- Errors: never a panic, never a wait without a timeout ----------

#[derive(Clone, Debug, thiserror::Error)]
pub enum TransportError {
    #[error("not authorized")]
    NotAuthorized,
    #[error("network: {0}")]
    Network(String),
    /// TDLib: `429 Too Many Requests: retry after X` (waits over 60 s; below, TDLib waits).
    #[error("flood wait {retry_after:?}")]
    FloodWait { retry_after: Duration },
    #[error("slow mode {retry_after:?}")]
    SlowMode { retry_after: Duration },
    #[error("write forbidden: {0:?}")]
    WriteForbidden(Forbidden),
    #[error("file too large (limit {limit} bytes)")]
    FileTooLarge { limit: u64 },
    #[error("file unreadable: {0}")]
    FileUnreadable(String),
    #[error("cancelled")]
    Cancelled,
    #[error("timeout waiting for {0}")]
    Timeout(&'static str),
    /// TDLib: `401` in answer to `setTdlibParameters` → recreate the client.
    #[error("bad database key")]
    BadDatabaseKey,
    #[error("unexpected response: {0}")]
    Protocol(String),
    #[error("telegram error {code}: {message}")]
    Other { code: i32, message: String },
    /// `tdjson.dll` missing or not loadable (loaded lazily: the app still starts).
    #[error("TDLib library unavailable: {0}")]
    LibMissing(String),
    /// No usable Telegram API identity (`api_id`/`api_hash` missing or invalid). The text
    /// names the field (`api_id_missing`…), never a value.
    #[error("Telegram API identity not configured: {0}")]
    NotConfigured(&'static str),
    /// A transport method this transport does not implement; nothing was sent.
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),
    /// The proxy was refused or could not reach Telegram. TDLib's message goes to the page
    /// (`TG_PROXY_FAILED{message}`) only: it names the proxy server ("Failed to connect to
    /// 10.9.9.9:1080"), so the text every log line prints (`Display`) leaves it out.
    #[error("proxy refused or unreachable")]
    Proxy(String),
    /// A proxy is set but its DPAPI-protected secret cannot be read (other Windows account or
    /// machine, corrupt file). Nothing connects until the user enters it again.
    #[error("proxy secret unavailable")]
    ProxySecretUnavailable,
    /// `reset_session` while a client runs (or the start did not fail): the session in use is
    /// never deleted.
    #[error("the Telegram session is in use")]
    SessionInUse,
}

impl TransportError {
    /// Stable code and parameters for the front. `Other` (a request Telegram
    /// refused, such as a wrong login code) becomes `TG_REJECTED` with Telegram's message.
    pub fn code_and_params(&self) -> (&'static str, Value) {
        match self {
            TransportError::NotAuthorized => ("TG_NOT_AUTHORIZED", json!({})),
            TransportError::Network(_) => ("TG_NETWORK", json!({})),
            TransportError::FloodWait { retry_after } => (
                "TG_FLOOD_WAIT",
                json!({ "retry_after": seconds_up(*retry_after) }),
            ),
            TransportError::SlowMode { retry_after } => (
                "TG_SLOWMODE",
                json!({ "retry_after": seconds_up(*retry_after) }),
            ),
            TransportError::WriteForbidden(forbidden) => forbidden.code_and_params(),
            TransportError::FileTooLarge { limit } => {
                ("TG_FILE_TOO_LARGE", json!({ "limit": limit }))
            }
            TransportError::FileUnreadable(_) => ("FILE_UNREADABLE", json!({})),
            TransportError::Cancelled => ("CANCELLED", json!({})),
            TransportError::Timeout(_) => ("TG_TIMEOUT", json!({})),
            TransportError::BadDatabaseKey => ("TG_BAD_DB_KEY", json!({})),
            TransportError::Protocol(_) => ("TG_PROTOCOL", json!({})),
            TransportError::Other { code, message } => {
                ("TG_REJECTED", json!({ "tgCode": code, "message": message }))
            }
            TransportError::LibMissing(_) => ("TG_LIB_MISSING", json!({})),
            // The problem names a field or a state (`api_id_missing`,
            // `override_api_hash_unavailable`), never a value: the page picks its sentence.
            TransportError::NotConfigured(problem) => {
                ("TG_NOT_CONFIGURED", json!({ "problem": problem }))
            }
            TransportError::NotImplemented(what) => ("TG_NOT_IMPLEMENTED", json!({ "what": what })),
            TransportError::Proxy(message) => ("TG_PROXY_FAILED", json!({ "message": message })),
            TransportError::ProxySecretUnavailable => ("TG_PROXY_SECRET_UNAVAILABLE", json!({})),
            TransportError::SessionInUse => ("TG_SESSION_IN_USE", json!({})),
        }
    }

    /// A start failure that comes from the local session itself (key refused by DPAPI or by
    /// TDLib, database or folder unreadable, TDLib not answering on it): resetting the session
    /// may fix it. Proxy, library and identity problems have their own remedy.
    pub fn session_reset_may_help(&self) -> bool {
        matches!(
            self,
            TransportError::BadDatabaseKey
                | TransportError::Protocol(_)
                | TransportError::Timeout(_)
        )
    }
}

/// Whole seconds, rounded up (a wait is never shortened).
pub fn seconds_up(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

#[async_trait]
pub trait TelegramTransport: Send + Sync {
    /// Creates the client, applies the parameters (log verbosity first), opens the database.
    async fn start(&self) -> Result<(), TransportError>;

    /// Login state machine; the UI subscribes and shows QR / code / password.
    fn auth_state(&self) -> watch::Receiver<AuthState>;
    async fn auth_submit(&self, input: AuthInput) -> Result<(), TransportError>;
    async fn log_out(&self) -> Result<(), TransportError>;

    async fn me(&self) -> Result<Me, TransportError>;

    /// Every chat of the main list, the archive and each chat folder, deduplicated.
    async fn list_dialogs(&self, limit: usize) -> Result<Vec<Dialog>, TransportError>;
    async fn list_topics(&self, peer: &Peer) -> Result<Vec<Topic>, TransportError>;
    /// Read only: chats matching `query` by title or username, including those absent from
    /// the lists (old chats, public chats). Results have `listed = false`.
    async fn search_chats(&self, query: &str, limit: usize) -> Result<Vec<Dialog>, TransportError>;

    /// Rights for the media kind, slow mode, closed topic, paid messages.
    async fn can_send(
        &self,
        target: &Target,
        media: MediaKind,
    ) -> Result<SendCheck, TransportError>;

    /// `Ok` only after the server confirmed (TDLib: `updateMessageSendSucceeded`).
    /// `cancel` fired: cancels the upload (TDLib: `deleteMessages` on the temporary id) and
    /// returns `Cancelled`, unless the message went out first (then `Ok(Sent)`).
    /// `progress` is bounded and lossy (`try_send`); nothing is sent on it after the return.
    async fn upload(
        &self,
        req: UploadRequest,
        progress: mpsc::Sender<Progress>,
        cancel: CancellationToken,
    ) -> Result<Sent, TransportError>;

    /// Copy a confirmed message to another destination, replacing its caption. There is no
    /// byte progress for a server-side copy; success is still confirmed by
    /// `updateMessageSendSucceeded`.
    async fn send_copy(
        &self,
        req: CopyRequest,
        cancel: CancellationToken,
    ) -> Result<Sent, TransportError>;

    /// Reuse TDLib's opaque remote file id when a copy is unavailable. There is no local
    /// upload and therefore no byte progress.
    async fn send_remote(
        &self,
        req: RemoteRequest,
        cancel: CancellationToken,
    ) -> Result<Sent, TransportError>;

    /// Read only: messages of the destination that may be this upload; the user decides.
    async fn verify_sent(&self, probe: &SentProbe) -> Result<Vec<SentCandidate>, TransportError>;

    /// Read only: the `https://t.me/` link of a message sent to `target`. `None` where Telegram
    /// has none (private chats, basic groups, Saved Messages) and for a message deleted since.
    async fn message_link(
        &self,
        target: &Target,
        message_id: i64,
    ) -> Result<Option<String>, TransportError>;

    /// Every connection goes through `proxy` (`None`: direct). Kept for the next start when no
    /// client runs; applied before the first network request of a start, and at once otherwise.
    async fn set_proxy(&self, proxy: Option<proxy::Proxy>) -> Result<(), TransportError>;

    /// Round trip to Telegram through `proxy`, without changing the proxy in use.
    async fn test_proxy(&self, proxy: &proxy::Proxy) -> Result<Duration, TransportError>;

    /// After a failed start only (`AuthState::StartFailed`): deletes the current identity's
    /// local session (database, files, key) and starts again, so the user can sign in anew.
    /// The queue, history and settings are not part of it. `SessionInUse` otherwise.
    async fn reset_session(&self) -> Result<(), TransportError> {
        Err(TransportError::NotImplemented("reset_session"))
    }

    async fn shutdown(&self);
}

/// No Telegram at all (`SQUEEZEGRAM_TRANSPORT=none`, or TDLib unusable: identity or DLL
/// missing). It never sends anything. Without a reason every call is `TG_NOT_AUTHORIZED`;
/// with one (`unavailable`), `start` and every call return it, so the UI shows why
/// (`TG_LIB_MISSING`, `TG_NOT_CONFIGURED`).
pub struct NoTransport {
    auth: watch::Sender<AuthState>,
    reason: Option<TransportError>,
}

impl NoTransport {
    pub fn new() -> Self {
        Self {
            auth: watch::Sender::new(AuthState::Closed),
            reason: None,
        }
    }

    pub fn unavailable(reason: TransportError) -> Self {
        Self {
            reason: Some(reason),
            ..Self::new()
        }
    }

    fn refuse<T>(&self) -> Result<T, TransportError> {
        Err(self.reason.clone().unwrap_or(TransportError::NotAuthorized))
    }
}

impl Default for NoTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TelegramTransport for NoTransport {
    async fn start(&self) -> Result<(), TransportError> {
        match &self.reason {
            Some(reason) => Err(reason.clone()),
            None => Ok(()),
        }
    }

    fn auth_state(&self) -> watch::Receiver<AuthState> {
        self.auth.subscribe()
    }

    async fn auth_submit(&self, _: AuthInput) -> Result<(), TransportError> {
        self.refuse()
    }

    async fn log_out(&self) -> Result<(), TransportError> {
        self.refuse()
    }

    async fn me(&self) -> Result<Me, TransportError> {
        self.refuse()
    }

    async fn list_dialogs(&self, _: usize) -> Result<Vec<Dialog>, TransportError> {
        self.refuse()
    }

    async fn list_topics(&self, _: &Peer) -> Result<Vec<Topic>, TransportError> {
        self.refuse()
    }

    async fn search_chats(&self, _: &str, _: usize) -> Result<Vec<Dialog>, TransportError> {
        self.refuse()
    }

    async fn can_send(&self, _: &Target, _: MediaKind) -> Result<SendCheck, TransportError> {
        self.refuse()
    }

    async fn upload(
        &self,
        _: UploadRequest,
        _: mpsc::Sender<Progress>,
        _: CancellationToken,
    ) -> Result<Sent, TransportError> {
        self.refuse()
    }

    async fn send_copy(
        &self,
        _: CopyRequest,
        _: CancellationToken,
    ) -> Result<Sent, TransportError> {
        self.refuse()
    }

    async fn send_remote(
        &self,
        _: RemoteRequest,
        _: CancellationToken,
    ) -> Result<Sent, TransportError> {
        self.refuse()
    }

    async fn verify_sent(&self, _: &SentProbe) -> Result<Vec<SentCandidate>, TransportError> {
        self.refuse()
    }

    async fn message_link(&self, _: &Target, _: i64) -> Result<Option<String>, TransportError> {
        self.refuse()
    }

    /// Nothing connects: the proxy is only saved (by the caller) for the next run.
    async fn set_proxy(&self, _: Option<proxy::Proxy>) -> Result<(), TransportError> {
        Ok(())
    }

    async fn test_proxy(&self, _: &proxy::Proxy) -> Result<Duration, TransportError> {
        self.refuse()
    }

    async fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tdlib_chat_ids_follow_dialogid_h() {
        let channel = Peer {
            kind: PeerKind::Channel,
            raw_id: 1_234_567_890,
        };
        assert_eq!(channel.tdlib_chat_id(), -1_001_234_567_890);
        let group = Peer {
            kind: PeerKind::BasicGroup,
            raw_id: 42,
        };
        assert_eq!(group.tdlib_chat_id(), -42);
        let user = Peer {
            kind: PeerKind::User,
            raw_id: 7,
        };
        assert_eq!(user.tdlib_chat_id(), 7);
        for kind in [PeerKind::User, PeerKind::BasicGroup, PeerKind::Channel] {
            assert_eq!(PeerKind::parse(kind.as_str()), Some(kind));
        }
    }

    // The trait must stay usable as `Arc<dyn TelegramTransport>`.
    fn _object_safe(_: std::sync::Arc<dyn TelegramTransport>) {}

    #[test]
    fn debug_never_shows_secrets() {
        let inputs = [
            AuthInput::Phone("+33612345678".into()),
            AuthInput::Code("12345".into()),
            AuthInput::Email("me@example.com".into()),
            AuthInput::EmailCode("67890".into()),
            AuthInput::Password("hunter2".into()),
        ];
        for input in &inputs {
            let text = format!("{input:?} {input:#?}");
            for secret in [
                "+33612345678",
                "12345",
                "me@example.com",
                "67890",
                "hunter2",
            ] {
                assert!(!text.contains(secret), "{text}");
            }
        }
        let me = Me {
            user_id: 987_654_321,
            display_name: "Alice Secret".into(),
            username: Some("alice_s".into()),
            is_premium: true,
            caption_max_chars: 4096,
            upload_limit_bytes: 1,
        };
        let qr = AuthState::WaitQr {
            link: "tg://login?token=SECRET".into(),
        };
        let failed = AuthState::StartFailed(TransportError::Proxy("10.9.9.9:1080".into()));
        let text = format!(
            "{me:?} {:?} {qr:?} {failed:?}",
            AuthState::Ready(me.clone())
        );
        for secret in ["987654321", "Alice", "alice_s", "SECRET", "10.9.9.9"] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(text.contains("StartFailed(TG_PROXY_FAILED)"), "{text}");
        assert!(text.contains("is_premium: true"));

        let remote = "opaque-remote-secret";
        let sent = Sent {
            message_id: 1,
            date_unix: 2,
            file_name: "a.mp4".into(),
            size: 3,
            remote_id: Some(remote.into()),
        };
        let request = RemoteRequest {
            remote_id: remote.into(),
            target: Target {
                peer: Peer {
                    kind: PeerKind::User,
                    raw_id: 42,
                },
                topic_id: None,
            },
            payload: Payload::Document { thumbnail: None },
            caption: Caption {
                text: "private caption".into(),
                entities: Vec::new(),
            },
            file_name: "a.mp4".into(),
            size: 3,
        };
        let text = format!("{sent:?} {request:?}");
        for secret in [remote, "private caption", "raw_id: 42"] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[test]
    fn auth_input_is_tagged_by_kind() {
        let phone: AuthInput =
            serde_json::from_value(json!({ "kind": "phone", "value": "+1" })).unwrap();
        assert!(matches!(phone, AuthInput::Phone(p) if p == "+1"));
        let qr: AuthInput = serde_json::from_value(json!({ "kind": "request_qr" })).unwrap();
        assert!(matches!(qr, AuthInput::RequestQr));
        let code: AuthInput =
            serde_json::from_value(json!({ "kind": "email_code", "value": "1" })).unwrap();
        assert!(matches!(code, AuthInput::EmailCode(_)));
        assert!(serde_json::from_value::<AuthInput>(json!({ "kind": "bogus" })).is_err());
    }

    #[test]
    fn errors_map_to_spec_codes() {
        let cases: Vec<(TransportError, &str, Value)> = vec![
            (
                TransportError::NotAuthorized,
                "TG_NOT_AUTHORIZED",
                json!({}),
            ),
            (TransportError::Network("x".into()), "TG_NETWORK", json!({})),
            (
                TransportError::FloodWait {
                    retry_after: Duration::from_millis(30_500),
                },
                "TG_FLOOD_WAIT",
                json!({ "retry_after": 31 }),
            ),
            (
                TransportError::SlowMode {
                    retry_after: Duration::from_secs(12),
                },
                "TG_SLOWMODE",
                json!({ "retry_after": 12 }),
            ),
            (
                TransportError::WriteForbidden(Forbidden::AdminsOnly),
                "TG_WRITE_FORBIDDEN",
                json!({ "reason": "admins_only" }),
            ),
            (
                TransportError::WriteForbidden(Forbidden::MediaNotAllowed(MediaKind::Video)),
                "TG_WRITE_FORBIDDEN",
                json!({ "reason": "media_not_allowed", "media": "video" }),
            ),
            (
                TransportError::WriteForbidden(Forbidden::PaidMessages { stars: 10 }),
                "TG_PAID_MESSAGES",
                json!({ "stars": 10 }),
            ),
            (
                TransportError::FileTooLarge { limit: 2048 },
                "TG_FILE_TOO_LARGE",
                json!({ "limit": 2048 }),
            ),
            (
                TransportError::FileUnreadable("C:\\x".into()),
                "FILE_UNREADABLE",
                json!({}),
            ),
            (TransportError::Timeout("ack"), "TG_TIMEOUT", json!({})),
            (TransportError::BadDatabaseKey, "TG_BAD_DB_KEY", json!({})),
            (
                TransportError::Protocol("x".into()),
                "TG_PROTOCOL",
                json!({}),
            ),
            (
                TransportError::Other {
                    code: 400,
                    message: "PHONE_CODE_INVALID".into(),
                },
                "TG_REJECTED",
                json!({ "tgCode": 400, "message": "PHONE_CODE_INVALID" }),
            ),
            (
                TransportError::LibMissing("C:\\x\\tdjson.dll not found".into()),
                "TG_LIB_MISSING",
                json!({}),
            ),
            (
                TransportError::NotConfigured("api_id_missing"),
                "TG_NOT_CONFIGURED",
                json!({ "problem": "api_id_missing" }),
            ),
            (
                TransportError::NotImplemented("upload (M4)"),
                "TG_NOT_IMPLEMENTED",
                json!({ "what": "upload (M4)" }),
            ),
            (
                TransportError::Proxy("Connection refused".into()),
                "TG_PROXY_FAILED",
                json!({ "message": "Connection refused" }),
            ),
            (
                TransportError::ProxySecretUnavailable,
                "TG_PROXY_SECRET_UNAVAILABLE",
                json!({}),
            ),
            (TransportError::SessionInUse, "TG_SESSION_IN_USE", json!({})),
        ];
        for (error, code, params) in cases {
            assert_eq!(error.code_and_params(), (code, params), "{error}");
        }
    }

    #[test]
    fn a_session_reset_is_offered_for_local_session_failures_only() {
        for error in [
            TransportError::BadDatabaseKey,
            TransportError::Protocol("database key file: PermissionDenied".into()),
            TransportError::Timeout("setTdlibParameters"),
        ] {
            assert!(error.session_reset_may_help(), "{error}");
        }
        for error in [
            TransportError::Proxy("refused".into()),
            TransportError::ProxySecretUnavailable,
            TransportError::LibMissing("x".into()),
            TransportError::NotConfigured("api_id_missing"),
            TransportError::Other {
                code: 400,
                message: "Valid api_id must be provided".into(),
            },
            TransportError::SessionInUse,
        ] {
            assert!(!error.session_reset_may_help(), "{error}");
        }
    }

    #[tokio::test]
    async fn no_transport_refuses_everything() {
        let transport = NoTransport::new();
        transport.start().await.unwrap();
        assert!(matches!(
            *transport.auth_state().borrow(),
            AuthState::Closed
        ));
        let (tx, _rx) = mpsc::channel(1);
        let request = UploadRequest {
            path: PathBuf::from("a.mp4"),
            size: 1,
            target: Target {
                peer: Peer {
                    kind: PeerKind::User,
                    raw_id: 1,
                },
                topic_id: None,
            },
            payload: Payload::Document { thumbnail: None },
            caption: Caption::default(),
            limit_bytes: None,
        };
        assert!(matches!(
            transport
                .upload(request, tx, CancellationToken::new())
                .await,
            Err(TransportError::NotAuthorized)
        ));
        assert!(matches!(
            transport.list_dialogs(10).await,
            Err(TransportError::NotAuthorized)
        ));
    }

    #[tokio::test]
    async fn an_unavailable_transport_reports_its_reason() {
        let transport = NoTransport::unavailable(TransportError::NotConfigured("api_id_missing"));
        let error = transport.start().await.unwrap_err();
        assert_eq!(error.code_and_params().0, "TG_NOT_CONFIGURED");
        assert!(matches!(
            *transport.auth_state().borrow(),
            AuthState::Closed
        ));
        assert!(matches!(
            transport.auth_submit(AuthInput::RequestQr).await,
            Err(TransportError::NotConfigured(_))
        ));
        assert!(matches!(
            transport.reset_session().await,
            Err(TransportError::NotImplemented(_))
        ));
        let missing = NoTransport::unavailable(TransportError::LibMissing("x".into()));
        assert_eq!(
            missing.me().await.unwrap_err().code_and_params().0,
            "TG_LIB_MISSING"
        );
    }
}
