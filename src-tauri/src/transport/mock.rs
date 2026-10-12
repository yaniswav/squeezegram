//! Deterministic `MockTransport`. Compiled only in
//! tests and debug builds (`cfg(any(test, debug_assertions))`), never in a release.
//!
//! Login: phone (any `+` number) then code `MOCK_CODE`, then `MOCK_PASSWORD` if two-step
//! verification is on; a number starting with `+0` asks for a sign-up (`OtherStep`). QR: the
//! link is shown, `confirm_qr()` plays the phone that scans it.
//!
//! Chats: the main list, the archive (a private chat and a forum) and two folders, "Work" and
//! "Family" (`MOCK_SLOW_GROUP` is in the main list and in both folders); two more chats are in
//! no list and only found by `search_chats` (`MOCK_PUBLIC_GROUP`, `MOCK_OLD_GROUP`).
//!
//! Proxy: kept as set; a test answers `MOCK_PROXY_PING`. A proxy at the host
//! `MOCK_PROXY_DOWN` is refused (`Proxy`), when set as when tested.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::proxy::Proxy;
use super::{
    AuthInput, AuthState, ChatFolder, ChatKind, Dialog, Forbidden, Me, MediaKind, Payload, Peer,
    PeerKind, Progress, SendCheck, Sent, SentCandidate, SentProbe, SlowMode, Target,
    TelegramTransport, Topic, TransportError, UploadRequest, Verdict,
};

pub const MOCK_CODE: &str = "12345";
/// Round trip of a proxy test.
pub const MOCK_PROXY_PING: Duration = Duration::from_millis(120);
/// A proxy at this host is refused and never answers its test.
pub const MOCK_PROXY_DOWN: &str = "down.invalid";
pub const MOCK_PASSWORD: &str = "squeezegram";
pub const MOCK_ME_ID: i64 = 777_000_001;
/// Forum with topics General (1), Videos (2) and a closed Archive (3).
pub const MOCK_FORUM: Peer = Peer {
    kind: PeerKind::Channel,
    raw_id: 1_234_567_890,
};
/// Broadcast channel where the account cannot post.
pub const MOCK_READ_ONLY_CHANNEL: Peer = Peer {
    kind: PeerKind::Channel,
    raw_id: 2_000_000_001,
};
/// Supergroup with a 30 s slow mode, 12 s left.
pub const MOCK_SLOW_GROUP: Peer = Peer {
    kind: PeerKind::Channel,
    raw_id: 3_000_000_002,
};
/// Supergroup that charges Stars per message.
pub const MOCK_PAID_GROUP: Peer = Peer {
    kind: PeerKind::Channel,
    raw_id: 4_000_000_003,
};
/// Basic group where videos are not allowed (photos and documents are).
pub const MOCK_NO_VIDEO_GROUP: Peer = Peer {
    kind: PeerKind::BasicGroup,
    raw_id: 42,
};
pub const MOCK_SAVED: Peer = Peer {
    kind: PeerKind::User,
    raw_id: MOCK_ME_ID,
};
/// Archived private chat, also in the "Family" folder.
pub const MOCK_ARCHIVED_CHAT: Peer = Peer {
    kind: PeerKind::User,
    raw_id: 5_550_001,
};
/// Archived forum, in the "Work" folder: General (1), Clips (5), a closed Old (6).
pub const MOCK_ARCHIVED_FORUM: Peer = Peer {
    kind: PeerKind::Channel,
    raw_id: 6_000_000_005,
};
/// Public supergroup in no list (found by search); non-members may post.
pub const MOCK_PUBLIC_GROUP: Peer = Peer {
    kind: PeerKind::Channel,
    raw_id: 7_000_000_006,
};
/// Basic group in no list any more (found by search).
pub const MOCK_OLD_GROUP: Peer = Peer {
    kind: PeerKind::BasicGroup,
    raw_id: 43,
};
/// Private chat of a deleted account (empty title) in the main list.
pub const MOCK_DELETED: Peer = Peer {
    kind: PeerKind::User,
    raw_id: 5_550_009,
};
/// Folders of the mock account, in the user's order.
pub const MOCK_FOLDER_WORK: i32 = 2;
pub const MOCK_FOLDER_FAMILY: i32 = 5;

const LIMIT_STANDARD: u64 = 2000 * 1024 * 1024;
const LIMIT_PREMIUM: u64 = 4000 * 1024 * 1024;
const CAPTION_PREVIEW_CHARS: usize = 64;

/// The mock account; `premium` as `TdlibJson::me` deduces it (4000 MiB, 4096 characters).
fn me(premium: bool) -> Me {
    Me {
        user_id: MOCK_ME_ID,
        display_name: "Mock User".into(),
        username: Some("mock_user".into()),
        is_premium: premium,
        caption_max_chars: if premium { 4096 } else { 1024 },
        upload_limit_bytes: if premium {
            LIMIT_PREMIUM
        } else {
            LIMIT_STANDARD
        },
    }
}

fn folder(id: i32) -> ChatFolder {
    match id {
        MOCK_FOLDER_WORK => ChatFolder {
            id,
            name: "Mock Work".into(),
            position: 0,
        },
        _ => ChatFolder {
            id,
            name: "Mock Family".into(),
            position: 1,
        },
    }
}

/// A listed chat; `folders` are folder ids.
fn dialog(peer: Peer, title: &str, kind: ChatKind, is_forum: bool, lists: Lists) -> Dialog {
    Dialog {
        peer,
        title: title.into(),
        username: None,
        is_forum,
        is_broadcast: kind == ChatKind::Channel,
        is_saved_messages: kind == ChatKind::Saved,
        kind,
        listed: lists.listed,
        archived: lists.archived,
        folders: lists.folders.iter().map(|id| folder(*id)).collect(),
        can_write: match peer {
            MOCK_READ_ONLY_CHANNEL | MOCK_PAID_GROUP => Some(false),
            _ if kind == ChatKind::Private => None,
            _ => Some(true),
        },
        deleted: false,
    }
}

/// Where a mock chat is listed.
#[derive(Clone, Copy)]
struct Lists {
    listed: bool,
    archived: bool,
    folders: &'static [i32],
}

const MAIN: Lists = Lists {
    listed: true,
    archived: false,
    folders: &[],
};
const NO_LIST: Lists = Lists {
    listed: false,
    archived: false,
    folders: &[],
};

fn rejected(message: &str) -> TransportError {
    TransportError::Other {
        code: 400,
        message: message.into(),
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Debug, Clone)]
struct Message {
    id: i64,
    target: Target,
    kind: MediaKind,
    file_name: String,
    size: u64,
    caption: String,
    date_unix: i64,
}

#[derive(Default)]
struct Script {
    flood_waits: VecDeque<Duration>,
    failures: VecDeque<TransportError>,
    stalls: VecDeque<Duration>,
}

struct MockState {
    password: bool,
    email: bool,
    /// The account is Premium (`with_premium`).
    premium: bool,
    script: Script,
    messages: Vec<Message>,
    next_message_id: i64,
    uploads: usize,
    /// Calls to `can_send`.
    checks: usize,
    /// The thumbnail of each video upload: its size and whether the file was there.
    thumbnails: Vec<Option<(u32, u32, bool)>>,
    /// Chats whose rights were taken away after the start (`revoke_write`).
    revoked: Vec<(Peer, Forbidden)>,
    proxy: Option<Proxy>,
}

pub struct MockTransport {
    auth: watch::Sender<AuthState>,
    state: Mutex<MockState>,
    /// Upload of one tenth of the file.
    step_delay: Duration,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTransport {
    /// Signed out: `start()` leads to `WaitPhone`.
    pub fn new() -> Self {
        Self {
            auth: watch::Sender::new(AuthState::Starting),
            state: Mutex::new(MockState {
                password: false,
                email: false,
                premium: false,
                script: Script::default(),
                messages: Vec::new(),
                next_message_id: 1,
                uploads: 0,
                checks: 0,
                thumbnails: Vec::new(),
                revoked: Vec::new(),
                proxy: None,
            }),
            step_delay: Duration::from_millis(200),
        }
    }

    /// Already signed in (a session from a previous run).
    pub fn authorized() -> Self {
        let mock = Self::new();
        mock.auth.send_replace(AuthState::Ready(me(false)));
        mock
    }

    /// A Premium account (4000 MiB upload limit), signed in already or later.
    pub fn with_premium(self) -> Self {
        self.lock().premium = true;
        if matches!(*self.auth.borrow(), AuthState::Ready(_)) {
            self.set(AuthState::Ready(self.account()));
        }
        self
    }

    fn account(&self) -> Me {
        me(self.lock().premium)
    }

    pub fn with_step_delay(mut self, delay: Duration) -> Self {
        self.step_delay = delay;
        self
    }

    /// Two-step verification after the code.
    pub fn with_password(self) -> Self {
        self.lock().password = true;
        self
    }

    /// E-mail address and e-mail code after the phone number.
    pub fn with_email(self) -> Self {
        self.lock().email = true;
        self
    }

    fn lock(&self) -> MutexGuard<'_, MockState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The next upload is refused with `429 retry after <delay>` (nothing goes out).
    pub fn push_flood_wait(&self, retry_after: Duration) {
        self.lock().script.flood_waits.push_back(retry_after);
    }

    /// The next upload fails with this error, after some progress.
    pub fn push_failure(&self, error: TransportError) {
        self.lock().script.failures.push_back(error);
    }

    /// The next upload stops reporting progress for this long halfway through.
    pub fn push_stall(&self, duration: Duration) {
        self.lock().script.stalls.push_back(duration);
    }

    /// The phone scanned the QR code.
    pub fn confirm_qr(&self) {
        if matches!(*self.auth.borrow(), AuthState::WaitQr { .. }) {
            self.after_code();
        }
    }

    /// A message already in the chat (sent before a crash, for instance).
    pub fn seed_message(
        &self,
        target: Target,
        kind: MediaKind,
        file_name: &str,
        size: u64,
        date_unix: i64,
    ) -> i64 {
        let mut state = self.lock();
        let id = state.next_message_id;
        state.next_message_id += 1;
        state.messages.push(Message {
            id,
            target,
            kind,
            file_name: file_name.into(),
            size,
            caption: String::new(),
            date_unix,
        });
        id
    }

    /// The proxy last set (`set_proxy`).
    pub fn proxy(&self) -> Option<Proxy> {
        self.lock().proxy.clone()
    }

    /// Calls to `upload`, whatever their outcome.
    pub fn uploads(&self) -> usize {
        self.lock().uploads
    }

    /// Calls to `can_send`: what a pre-send check costs in TDLib requests.
    pub fn checks(&self) -> usize {
        self.lock().checks
    }

    /// For each video upload, the thumbnail it carried (width, height, file present at the
    /// time of the upload), `None` for a video sent without one.
    pub fn thumbnails(&self) -> Vec<Option<(u32, u32, bool)>> {
        self.lock().thumbnails.clone()
    }

    /// From now on `peer` refuses messages (rights lost, topic closed, paid messages…), as a
    /// chat can while an item waits in the queue.
    pub fn revoke_write(&self, peer: Peer, forbidden: Forbidden) {
        let mut state = self.lock();
        state.revoked.retain(|(other, _)| *other != peer);
        state.revoked.push((peer, forbidden));
    }

    /// Undoes `revoke_write`.
    pub fn restore_write(&self, peer: Peer) {
        self.lock().revoked.retain(|(other, _)| *other != peer);
    }

    /// Messages that went out, as `(message_id, target, file name)`.
    pub fn sent(&self) -> Vec<(i64, Target, String)> {
        self.lock()
            .messages
            .iter()
            .map(|m| (m.id, m.target, m.file_name.clone()))
            .collect()
    }

    /// Captions of the messages that went out, in order.
    pub fn captions(&self) -> Vec<String> {
        self.lock()
            .messages
            .iter()
            .map(|m| m.caption.clone())
            .collect()
    }

    fn set(&self, state: AuthState) {
        self.auth.send_replace(state);
    }

    fn after_code(&self) {
        if self.lock().password {
            self.set(AuthState::WaitPassword {
                hint: "mock hint".into(),
            });
        } else {
            self.set(AuthState::Ready(self.account()));
        }
    }

    fn ensure_ready(&self) -> Result<(), TransportError> {
        match *self.auth.borrow() {
            AuthState::Ready(_) => Ok(()),
            _ => Err(TransportError::NotAuthorized),
        }
    }

    /// The main list, the archive and the folders, deduplicated (Saved Messages first).
    fn dialogs() -> Vec<Dialog> {
        use ChatKind::{BasicGroup, Channel, Private, Saved, Supergroup};
        vec![
            dialog(MOCK_SAVED, "Saved Messages", Saved, false, MAIN),
            dialog(
                MOCK_FORUM,
                "Mock forum",
                Supergroup,
                true,
                Lists {
                    folders: &[MOCK_FOLDER_WORK],
                    ..MAIN
                },
            ),
            dialog(
                MOCK_READ_ONLY_CHANNEL,
                "Mock news channel",
                Channel,
                false,
                MAIN,
            ),
            dialog(
                MOCK_SLOW_GROUP,
                "Mock slow group",
                Supergroup,
                false,
                Lists {
                    folders: &[MOCK_FOLDER_WORK, MOCK_FOLDER_FAMILY],
                    ..MAIN
                },
            ),
            dialog(MOCK_PAID_GROUP, "Mock paid group", Supergroup, false, MAIN),
            dialog(
                MOCK_NO_VIDEO_GROUP,
                "Mock family",
                BasicGroup,
                false,
                Lists {
                    folders: &[MOCK_FOLDER_FAMILY],
                    ..MAIN
                },
            ),
            dialog(
                MOCK_ARCHIVED_CHAT,
                "Mock old friend",
                Private,
                false,
                Lists {
                    archived: true,
                    folders: &[MOCK_FOLDER_FAMILY],
                    ..MAIN
                },
            ),
            dialog(
                MOCK_ARCHIVED_FORUM,
                "Mock archived forum",
                Supergroup,
                true,
                Lists {
                    archived: true,
                    folders: &[MOCK_FOLDER_WORK],
                    ..MAIN
                },
            ),
            Dialog {
                can_write: Some(false),
                deleted: true,
                ..dialog(MOCK_DELETED, "", Private, false, MAIN)
            },
        ]
    }

    /// Chats in no list, only reachable through `search_chats`.
    fn unlisted() -> Vec<Dialog> {
        vec![
            dialog(
                MOCK_PUBLIC_GROUP,
                "Mock public group",
                ChatKind::Supergroup,
                false,
                NO_LIST,
            ),
            dialog(
                MOCK_OLD_GROUP,
                "Mock old project",
                ChatKind::BasicGroup,
                false,
                NO_LIST,
            ),
        ]
    }

    fn topics(peer: &Peer) -> Option<Vec<Topic>> {
        let topic = |id, title: &str, is_closed| Topic {
            id,
            title: title.into(),
            is_general: id == 1,
            is_closed,
        };
        match *peer {
            MOCK_FORUM => Some(vec![
                topic(1, "General", false),
                topic(2, "Videos", false),
                topic(3, "Archive", true),
            ]),
            MOCK_ARCHIVED_FORUM => Some(vec![
                topic(1, "General", false),
                topic(5, "Clips", false),
                topic(6, "Old", true),
            ]),
            _ => None,
        }
    }

    /// `check` with the rights taken away by `revoke_write`.
    fn current_check(
        &self,
        target: &Target,
        media: MediaKind,
    ) -> Result<SendCheck, TransportError> {
        let mut check = Self::check(target, media)?;
        let revoked = self
            .lock()
            .revoked
            .iter()
            .find(|(peer, _)| *peer == target.peer)
            .map(|(_, forbidden)| forbidden.clone());
        if let Some(forbidden) = revoked {
            check.verdict = Verdict::Denied(forbidden);
        }
        Ok(check)
    }

    fn check(target: &Target, media: MediaKind) -> Result<SendCheck, TransportError> {
        let known = Self::dialogs()
            .into_iter()
            .chain(Self::unlisted())
            .any(|d| d.peer == target.peer);
        if !known {
            return Err(rejected("Chat not found"));
        }
        let mut slow_mode = None;
        let verdict = match target.peer {
            MOCK_READ_ONLY_CHANNEL => Verdict::Denied(Forbidden::AdminsOnly),
            MOCK_DELETED => Verdict::Denied(Forbidden::NotMember),
            MOCK_PAID_GROUP => Verdict::Denied(Forbidden::PaidMessages { stars: 10 }),
            MOCK_NO_VIDEO_GROUP if media == MediaKind::Video => {
                Verdict::Denied(Forbidden::MediaNotAllowed(MediaKind::Video))
            }
            MOCK_SLOW_GROUP => {
                slow_mode = Some(SlowMode {
                    delay: Duration::from_secs(30),
                    wait_remaining: Duration::from_secs(12),
                });
                Verdict::Allowed
            }
            MOCK_FORUM | MOCK_ARCHIVED_FORUM => {
                let topics = Self::topics(&target.peer).unwrap_or_default();
                match target.topic_id {
                    None => Verdict::Allowed,
                    Some(id) => match topics.iter().find(|t| t.id == id) {
                        None => return Err(rejected("TOPIC_ID_INVALID")),
                        Some(topic) if topic.is_closed => Verdict::Denied(Forbidden::TopicClosed),
                        Some(_) => Verdict::Allowed,
                    },
                }
            }
            _ if target.topic_id.is_some() => return Err(rejected("TOPIC_ID_INVALID")),
            _ => Verdict::Allowed,
        };
        Ok(SendCheck {
            verdict,
            slow_mode,
            checked_at: SystemTime::now(),
        })
    }

    /// Sleeps one step; false if cancelled meanwhile.
    async fn step(&self, delay: Duration, cancel: &CancellationToken) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => !cancel.is_cancelled(),
            _ = cancel.cancelled() => false,
        }
    }
}

#[async_trait]
impl TelegramTransport for MockTransport {
    async fn start(&self) -> Result<(), TransportError> {
        if matches!(*self.auth.borrow(), AuthState::Starting | AuthState::Closed) {
            self.set(AuthState::WaitPhone);
        }
        Ok(())
    }

    fn auth_state(&self) -> watch::Receiver<AuthState> {
        self.auth.subscribe()
    }

    async fn auth_submit(&self, input: AuthInput) -> Result<(), TransportError> {
        let current = self.auth.borrow().clone();
        match (current, input) {
            (AuthState::WaitPhone | AuthState::WaitQr { .. }, AuthInput::Phone(phone)) => {
                let digits = phone.trim().trim_start_matches('+');
                if !phone.trim().starts_with('+')
                    || digits.is_empty()
                    || !digits.chars().all(|c| c.is_ascii_digit())
                {
                    return Err(rejected("PHONE_NUMBER_INVALID"));
                }
                if digits.starts_with('0') {
                    self.set(AuthState::OtherStep {
                        name: "registration".into(),
                    });
                } else if self.lock().email {
                    self.set(AuthState::WaitEmail);
                } else {
                    self.set(AuthState::WaitCode {
                        resend_in: Some(Duration::from_secs(60)),
                    });
                }
                Ok(())
            }
            (AuthState::WaitPhone | AuthState::WaitQr { .. }, AuthInput::RequestQr) => {
                self.set(AuthState::WaitQr {
                    link: "tg://login?token=bW9jay1xcg".into(),
                });
                Ok(())
            }
            (AuthState::WaitEmail, AuthInput::Email(email)) => {
                if !email.contains('@') {
                    return Err(rejected("EMAIL_INVALID"));
                }
                self.set(AuthState::WaitEmailCode);
                Ok(())
            }
            (AuthState::WaitEmailCode, AuthInput::EmailCode(code)) => {
                if code != MOCK_CODE {
                    return Err(rejected("EMAIL_CODE_INVALID"));
                }
                self.set(AuthState::WaitCode {
                    resend_in: Some(Duration::from_secs(60)),
                });
                Ok(())
            }
            (AuthState::WaitCode { .. }, AuthInput::ResendCode) => {
                self.set(AuthState::WaitCode {
                    resend_in: Some(Duration::from_secs(120)),
                });
                Ok(())
            }
            (AuthState::WaitCode { .. }, AuthInput::Code(code)) => {
                if code.trim() != MOCK_CODE {
                    return Err(rejected("PHONE_CODE_INVALID"));
                }
                self.after_code();
                Ok(())
            }
            (AuthState::WaitPassword { .. }, AuthInput::Password(password)) => {
                if password != MOCK_PASSWORD {
                    return Err(rejected("PASSWORD_HASH_INVALID"));
                }
                self.set(AuthState::Ready(self.account()));
                Ok(())
            }
            _ => Err(rejected("AUTH_STEP_UNEXPECTED")),
        }
    }

    async fn log_out(&self) -> Result<(), TransportError> {
        self.set(AuthState::LoggingOut);
        // TDLib closes, the local database is deleted, a new client waits for a number.
        self.set(AuthState::WaitPhone);
        Ok(())
    }

    async fn me(&self) -> Result<Me, TransportError> {
        match &*self.auth.borrow() {
            AuthState::Ready(me) => Ok(me.clone()),
            _ => Err(TransportError::NotAuthorized),
        }
    }

    async fn list_dialogs(&self, limit: usize) -> Result<Vec<Dialog>, TransportError> {
        self.ensure_ready()?;
        Ok(Self::dialogs().into_iter().take(limit).collect())
    }

    async fn list_topics(&self, peer: &Peer) -> Result<Vec<Topic>, TransportError> {
        self.ensure_ready()?;
        Self::topics(peer).ok_or_else(|| rejected("CHANNEL_FORUM_MISSING"))
    }

    /// Case-insensitive title match over every chat, listed or not; results carry no list
    /// flags (as with TDLib).
    async fn search_chats(&self, query: &str, limit: usize) -> Result<Vec<Dialog>, TransportError> {
        self.ensure_ready()?;
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        Ok(Self::dialogs()
            .into_iter()
            .chain(Self::unlisted())
            .filter(|d| !d.deleted && d.title.to_lowercase().contains(&query))
            .map(|d| Dialog {
                listed: false,
                archived: false,
                folders: Vec::new(),
                ..d
            })
            .take(limit)
            .collect())
    }

    async fn can_send(
        &self,
        target: &Target,
        media: MediaKind,
    ) -> Result<SendCheck, TransportError> {
        self.ensure_ready()?;
        self.lock().checks += 1;
        self.current_check(target, media)
    }

    async fn upload(
        &self,
        req: UploadRequest,
        progress: mpsc::Sender<Progress>,
        cancel: CancellationToken,
    ) -> Result<Sent, TransportError> {
        self.lock().uploads += 1;
        self.ensure_ready()?;
        if let Payload::Video { thumbnail, .. } = &req.payload {
            self.lock().thumbnails.push(
                thumbnail
                    .as_ref()
                    .map(|t| (t.width, t.height, t.path.is_file())),
            );
        }
        let media = req.payload.media_kind();
        if let Verdict::Denied(forbidden) = self.current_check(&req.target, media)?.verdict {
            return Err(TransportError::WriteForbidden(forbidden));
        }
        if !req.path.is_file() {
            return Err(TransportError::FileUnreadable(
                "the file to send is missing".into(),
            ));
        }
        // The limit the plan used, else the account's (as `TdlibJson`).
        let limit = req
            .limit_bytes
            .unwrap_or_else(|| self.account().upload_limit_bytes);
        if req.size > limit {
            return Err(TransportError::FileTooLarge { limit });
        }
        let report = |phase, sent| {
            let _ = progress.try_send(Progress {
                phase,
                sent,
                total: req.size,
            });
        };
        report(super::Phase::Preparing, 0);
        let (failure, stall) = {
            let mut state = self.lock();
            let script = &mut state.script;
            if let Some(retry_after) = script.flood_waits.pop_front() {
                // The temporary message is deleted: nothing went out.
                return Err(TransportError::FloodWait { retry_after });
            }
            (script.failures.pop_front(), script.stalls.pop_front())
        };
        const STEPS: u64 = 10;
        for step in 1..=STEPS {
            if !self.step(self.step_delay, &cancel).await {
                return Err(TransportError::Cancelled);
            }
            if step == STEPS / 2 {
                if let Some(error) = failure {
                    return Err(error);
                }
                if let Some(stall) = stall
                    && !self.step(stall, &cancel).await
                {
                    return Err(TransportError::Cancelled);
                }
            }
            report(super::Phase::Uploading, req.size * step / STEPS);
        }
        report(super::Phase::AwaitingServer, req.size);
        if !self.step(self.step_delay, &cancel).await {
            return Err(TransportError::Cancelled);
        }
        // `updateMessageSendSucceeded`: from here a cancel is too late.
        let file_name = req
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let date_unix = now_unix();
        let mut state = self.lock();
        let id = state.next_message_id;
        state.next_message_id += 1;
        state.messages.push(Message {
            id,
            target: req.target,
            kind: media,
            file_name: file_name.clone(),
            size: req.size,
            caption: req.caption.text,
            date_unix,
        });
        Ok(Sent {
            message_id: id,
            date_unix,
            file_name,
            size: req.size,
        })
    }

    async fn verify_sent(&self, probe: &SentProbe) -> Result<Vec<SentCandidate>, TransportError> {
        self.ensure_ready()?;
        let state = self.lock();
        let candidates = state
            .messages
            .iter()
            .rev()
            .filter(|m| {
                m.target == probe.target
                    && m.kind == probe.media
                    && m.date_unix >= probe.not_before_unix
            })
            // Photos lose their name on the server: every photo in the window is a candidate.
            .filter(|m| match &probe.file_name {
                Some(name) if probe.media != MediaKind::Photo => {
                    *name == m.file_name && m.size == probe.size
                }
                _ => true,
            })
            .take(50)
            .map(|m| {
                let photo = m.kind == MediaKind::Photo;
                SentCandidate {
                    message_id: m.id,
                    date_unix: m.date_unix,
                    kind: m.kind,
                    file_name: (!photo).then(|| m.file_name.clone()),
                    size: (!photo).then_some(m.size),
                    caption_preview: m.caption.chars().take(CAPTION_PREVIEW_CHARS).collect(),
                }
            })
            .collect();
        Ok(candidates)
    }

    /// Supergroups and channels only, like Telegram: `https://t.me/c/<raw id>/<message id>`.
    async fn message_link(
        &self,
        target: &Target,
        message_id: i64,
    ) -> Result<Option<String>, TransportError> {
        self.ensure_ready()?;
        let sent = self
            .lock()
            .messages
            .iter()
            .any(|m| m.id == message_id && m.target.peer == target.peer);
        Ok((sent && target.peer.kind == PeerKind::Channel)
            .then(|| format!("https://t.me/c/{}/{message_id}", target.peer.raw_id)))
    }

    async fn set_proxy(&self, proxy: Option<Proxy>) -> Result<(), TransportError> {
        if proxy
            .as_ref()
            .is_some_and(|proxy| proxy.host == MOCK_PROXY_DOWN)
        {
            return Err(TransportError::Proxy("Connection refused".into()));
        }
        self.lock().proxy = proxy;
        Ok(())
    }

    async fn test_proxy(&self, proxy: &Proxy) -> Result<Duration, TransportError> {
        if proxy.host == MOCK_PROXY_DOWN {
            return Err(TransportError::Proxy("Connection refused".into()));
        }
        Ok(MOCK_PROXY_PING)
    }

    async fn shutdown(&self) {
        self.set(AuthState::Closed);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::queue::test_util::TestDir;
    use crate::transport::{Caption, Payload, Phase};

    fn whole(peer: Peer) -> Target {
        Target {
            peer,
            topic_id: None,
        }
    }

    fn state_name(mock: &MockTransport) -> String {
        format!("{:?}", *mock.auth_state().borrow())
    }

    fn request(path: PathBuf, target: Target, payload: Payload) -> UploadRequest {
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        UploadRequest {
            path,
            size,
            target,
            payload,
            caption: Caption {
                text: "hello".into(),
                entities: Vec::new(),
            },
            limit_bytes: None,
        }
    }

    fn video() -> Payload {
        Payload::Video {
            duration_s: 3,
            width: 640,
            height: 360,
            supports_streaming: true,
            thumbnail: None,
        }
    }

    #[tokio::test]
    async fn login_by_phone_with_email_and_password() {
        let mock = MockTransport::new().with_password().with_email();
        assert!(matches!(
            mock.list_dialogs(10).await,
            Err(TransportError::NotAuthorized)
        ));
        mock.start().await.unwrap();
        assert_eq!(state_name(&mock), "WaitPhone");
        assert!(
            mock.auth_submit(AuthInput::Phone("123".into()))
                .await
                .is_err()
        );
        mock.auth_submit(AuthInput::Phone("+33 6".replace(' ', "")))
            .await
            .unwrap();
        assert_eq!(state_name(&mock), "WaitEmail");
        mock.auth_submit(AuthInput::Email("a@b.c".into()))
            .await
            .unwrap();
        mock.auth_submit(AuthInput::EmailCode(MOCK_CODE.into()))
            .await
            .unwrap();
        assert!(state_name(&mock).starts_with("WaitCode"));
        mock.auth_submit(AuthInput::ResendCode).await.unwrap();
        let wrong = mock.auth_submit(AuthInput::Code("0".into())).await;
        assert!(
            matches!(wrong, Err(TransportError::Other { ref message, .. }) if message == "PHONE_CODE_INVALID")
        );
        mock.auth_submit(AuthInput::Code(MOCK_CODE.into()))
            .await
            .unwrap();
        assert!(state_name(&mock).starts_with("WaitPassword"));
        assert!(
            mock.auth_submit(AuthInput::Password("nope".into()))
                .await
                .is_err()
        );
        mock.auth_submit(AuthInput::Password(MOCK_PASSWORD.into()))
            .await
            .unwrap();
        assert_eq!(mock.me().await.unwrap().user_id, MOCK_ME_ID);
        // Out-of-order input.
        assert!(
            mock.auth_submit(AuthInput::Code(MOCK_CODE.into()))
                .await
                .is_err()
        );

        mock.log_out().await.unwrap();
        assert_eq!(state_name(&mock), "WaitPhone");
        assert!(mock.me().await.is_err());
    }

    #[tokio::test]
    async fn login_by_qr_and_other_step() {
        let mock = MockTransport::new();
        mock.start().await.unwrap();
        mock.auth_submit(AuthInput::RequestQr).await.unwrap();
        assert!(
            matches!(&*mock.auth_state().borrow(), AuthState::WaitQr { link } if link.starts_with("tg://login"))
        );
        mock.confirm_qr();
        assert!(matches!(*mock.auth_state().borrow(), AuthState::Ready(_)));

        let other = MockTransport::new();
        other.start().await.unwrap();
        other
            .auth_submit(AuthInput::Phone("+0123".into()))
            .await
            .unwrap();
        assert!(
            matches!(&*other.auth_state().borrow(), AuthState::OtherStep { name } if name == "registration")
        );
        other.shutdown().await;
        assert_eq!(state_name(&other), "Closed");
    }

    #[tokio::test]
    async fn destinations_and_can_send_vary() {
        let mock = MockTransport::authorized();
        let dialogs = mock.list_dialogs(100).await.unwrap();
        assert!(dialogs[0].is_saved_messages);
        assert!(dialogs.iter().any(|d| d.is_forum));
        assert!(dialogs.iter().any(|d| d.is_broadcast));
        assert!(dialogs.iter().all(|d| d.listed));
        let deleted: Vec<Peer> = dialogs
            .iter()
            .filter(|d| d.deleted)
            .map(|d| d.peer)
            .collect();
        assert_eq!(deleted, [MOCK_DELETED]);
        // Archive, a forum in the archive, a chat in two folders.
        let archived: Vec<Peer> = dialogs
            .iter()
            .filter(|d| d.archived)
            .map(|d| d.peer)
            .collect();
        assert_eq!(archived, [MOCK_ARCHIVED_CHAT, MOCK_ARCHIVED_FORUM]);
        let slow = dialogs.iter().find(|d| d.peer == MOCK_SLOW_GROUP).unwrap();
        let ids: Vec<i32> = slow.folders.iter().map(|f| f.id).collect();
        assert_eq!(ids, [MOCK_FOLDER_WORK, MOCK_FOLDER_FAMILY]);
        let mut peers: Vec<Peer> = dialogs.iter().map(|d| d.peer).collect();
        peers.dedup();
        assert_eq!(peers.len(), dialogs.len(), "deduplicated");
        assert!(!dialogs.iter().any(|d| d.peer == MOCK_PUBLIC_GROUP));
        assert_eq!(mock.list_dialogs(2).await.unwrap().len(), 2);
        let found = mock.search_chats("PUBLIC", 10).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].peer, MOCK_PUBLIC_GROUP);
        assert!(!found[0].listed);
        let old = mock.search_chats("old", 10).await.unwrap();
        let old: Vec<Peer> = old.iter().map(|d| d.peer).collect();
        assert_eq!(old, [MOCK_ARCHIVED_CHAT, MOCK_OLD_GROUP]);
        assert!(mock.search_chats("  ", 10).await.unwrap().is_empty());
        let archived_topics = mock.list_topics(&MOCK_ARCHIVED_FORUM).await.unwrap();
        assert!(archived_topics[0].is_general && archived_topics[0].id == 1);
        assert!(archived_topics.iter().any(|t| t.is_closed));
        let topics = mock.list_topics(&MOCK_FORUM).await.unwrap();
        assert_eq!(topics.len(), 3);
        assert!(topics[0].is_general);
        assert!(mock.list_topics(&MOCK_SAVED).await.is_err());

        let verdict = |target, media| {
            let check = MockTransport::check(&target, media).unwrap();
            (check.verdict, check.slow_mode.map(|s| s.wait_remaining))
        };
        assert_eq!(
            verdict(whole(MOCK_SAVED), MediaKind::Video),
            (Verdict::Allowed, None)
        );
        assert_eq!(
            verdict(whole(MOCK_READ_ONLY_CHANNEL), MediaKind::Video).0,
            Verdict::Denied(Forbidden::AdminsOnly)
        );
        assert_eq!(
            verdict(whole(MOCK_PAID_GROUP), MediaKind::Document).0,
            Verdict::Denied(Forbidden::PaidMessages { stars: 10 })
        );
        assert_eq!(
            verdict(whole(MOCK_NO_VIDEO_GROUP), MediaKind::Video).0,
            Verdict::Denied(Forbidden::MediaNotAllowed(MediaKind::Video))
        );
        assert_eq!(
            verdict(whole(MOCK_NO_VIDEO_GROUP), MediaKind::Photo).0,
            Verdict::Allowed
        );
        assert_eq!(
            verdict(whole(MOCK_SLOW_GROUP), MediaKind::Video),
            (Verdict::Allowed, Some(Duration::from_secs(12)))
        );
        let topic = |id| Target {
            peer: MOCK_FORUM,
            topic_id: Some(id),
        };
        assert_eq!(verdict(topic(2), MediaKind::Video).0, Verdict::Allowed);
        assert_eq!(
            verdict(topic(3), MediaKind::Video).0,
            Verdict::Denied(Forbidden::TopicClosed)
        );
        assert!(MockTransport::check(&topic(99), MediaKind::Video).is_err());
        // Archived forum topics, and a chat found by search only.
        let archived = |id| Target {
            peer: MOCK_ARCHIVED_FORUM,
            topic_id: Some(id),
        };
        assert_eq!(verdict(archived(5), MediaKind::Video).0, Verdict::Allowed);
        assert_eq!(
            verdict(archived(6), MediaKind::Video).0,
            Verdict::Denied(Forbidden::TopicClosed)
        );
        assert_eq!(
            verdict(whole(MOCK_PUBLIC_GROUP), MediaKind::Video).0,
            Verdict::Allowed
        );
        assert!(
            MockTransport::check(
                &Target {
                    peer: MOCK_SAVED,
                    topic_id: Some(1)
                },
                MediaKind::Video
            )
            .is_err(),
            "a topic in a chat that is not a forum"
        );
        let unknown = whole(Peer {
            kind: PeerKind::User,
            raw_id: 5,
        });
        assert!(mock.can_send(&unknown, MediaKind::Video).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn upload_reports_progress_then_sends() {
        let dir = TestDir::new("mock-upload");
        let path = PathBuf::from(dir.file("clip_sq.mp4", &[0u8; 1000]));
        let mock = MockTransport::authorized();
        let (tx, mut rx) = mpsc::channel(64);
        let sent = mock
            .upload(
                request(path, whole(MOCK_SAVED), video()),
                tx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(sent.file_name, "clip_sq.mp4");
        assert_eq!(sent.size, 1000);
        let mut phases = Vec::new();
        while let Ok(p) = rx.try_recv() {
            phases.push((p.phase, p.sent));
        }
        assert_eq!(phases.first(), Some(&(Phase::Preparing, 0)));
        assert_eq!(phases.last(), Some(&(Phase::AwaitingServer, 1000)));
        assert!(phases.windows(2).all(|w| w[0].1 <= w[1].1), "monotonic");
        assert_eq!(mock.sent().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn upload_errors_flood_wait_and_cancel() {
        let dir = TestDir::new("mock-errors");
        let path = PathBuf::from(dir.file("a.mp4", b"abc"));
        let mock = MockTransport::authorized();
        let go = |target, payload| {
            let (tx, _rx) = mpsc::channel(64);
            mock.upload(
                request(path.clone(), target, payload),
                tx,
                CancellationToken::new(),
            )
        };
        assert!(matches!(
            go(whole(MOCK_READ_ONLY_CHANNEL), video()).await,
            Err(TransportError::WriteForbidden(Forbidden::AdminsOnly))
        ));
        mock.push_flood_wait(Duration::from_secs(90));
        assert!(matches!(
            go(whole(MOCK_SAVED), video()).await,
            Err(TransportError::FloodWait { retry_after }) if retry_after == Duration::from_secs(90)
        ));
        mock.push_failure(TransportError::Network("down".into()));
        assert!(matches!(
            go(whole(MOCK_SAVED), video()).await,
            Err(TransportError::Network(_))
        ));
        assert!(mock.sent().is_empty(), "nothing went out");
        assert!(
            go(whole(MOCK_SAVED), video()).await.is_ok(),
            "script used up"
        );

        let missing = request(dir.path().join("gone.mp4"), whole(MOCK_SAVED), video());
        let (tx, _rx) = mpsc::channel(1);
        assert!(matches!(
            mock.upload(missing, tx, CancellationToken::new()).await,
            Err(TransportError::FileUnreadable(_))
        ));

        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        let upload = mock.upload(
            request(path.clone(), whole(MOCK_SAVED), video()),
            tx,
            cancel.clone(),
        );
        let canceller = async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(upload, canceller);
        assert!(matches!(result, Err(TransportError::Cancelled)));
        assert_eq!(mock.sent().len(), 1);
        assert_eq!(mock.uploads(), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn verify_sent_returns_candidates() {
        let dir = TestDir::new("mock-verify");
        let mock = MockTransport::authorized();
        let target = whole(MOCK_SAVED);
        let old = mock.seed_message(target, MediaKind::Video, "a_sq.mp4", 3, 1_000);
        let video_path = PathBuf::from(dir.file("a_sq.mp4", b"abc"));
        let photo_path = PathBuf::from(dir.file("p_sq.jpg", b"jpeg"));
        let (tx, _rx) = mpsc::channel(64);
        let sent = mock
            .upload(
                request(video_path, target, video()),
                tx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        mock.upload(
            request(
                photo_path,
                target,
                Payload::Photo {
                    width: 10,
                    height: 10,
                },
            ),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let probe = SentProbe {
            target,
            media: MediaKind::Video,
            file_name: Some("a_sq.mp4".into()),
            size: 3,
            not_before_unix: 0,
        };
        let found = mock.verify_sent(&probe).await.unwrap();
        let ids: Vec<i64> = found.iter().map(|c| c.message_id).collect();
        assert_eq!(ids, [sent.message_id, old], "newest first");
        assert_eq!(found[0].caption_preview, "hello");
        assert_eq!(found[0].size, Some(3));

        let recent = SentProbe {
            not_before_unix: 2_000,
            ..probe.clone()
        };
        assert_eq!(mock.verify_sent(&recent).await.unwrap().len(), 1);
        let other_size = SentProbe {
            size: 4,
            ..probe.clone()
        };
        assert!(mock.verify_sent(&other_size).await.unwrap().is_empty());

        let photo = SentProbe {
            media: MediaKind::Photo,
            file_name: None,
            size: 999,
            ..probe
        };
        let photos = mock.verify_sent(&photo).await.unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].file_name, None, "photos lose their name");
    }
}
