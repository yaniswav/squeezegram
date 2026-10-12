//! `TdlibJson`: the TDLib transport as a thin JSON layer over `tdjson`.
//! Start, login state machine, `auth_submit`, `log_out`, `shutdown` and `me` live here; `chats`
//! holds dialogs, topics and `can_send`, `send` holds `upload` and `verify_sent`, `proxy` the
//! proxy and its DPAPI-protected secret. Requests and
//! fields are named after `td_api.tl` of the pinned TDLib 1.8.68.
//!
//! Rules learned from how TDLib behaves:
//! - the log verbosity is set by `td_execute` before the client exists (level 1, never above
//!   2: at 3 TDLib logs every request, phone number, code, password, `api_hash`, key);
//! - one thread runs `td_receive` and routes answers by `@extra`; every request waits under a
//!   timeout, because TDLib never answers a request queued before a failed
//!   `setTdlibParameters`;
//! - no `unwrap` on TDLib JSON: anything unexpected is `TransportError::Protocol`;
//! - `401` in answer to `setTdlibParameters` is `BadDatabaseKey`, any other `401` is
//!   `NotAuthorized`, `429 … retry after X` is `FloodWait`.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::proxy::Proxy;
use super::{
    AuthInput, AuthState, ChatFolder, Dialog, Me, MediaKind, Peer, Progress, SendCheck, Sent,
    SentCandidate, SentProbe, Target, TelegramTransport, Topic, TransportError, UploadRequest,
};
use crate::media::plan::MIB;

pub mod auth;
pub mod bindings;
pub mod chats;
/// Shared with `build.rs`; in the crate only for its tests.
#[cfg(test)]
mod dotenv;
pub mod identity;
pub mod json;
pub mod key;
pub mod library;
pub mod proxy;
#[cfg(any(test, debug_assertions))]
pub mod replay;
pub mod send;
pub mod session;
#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod tests_chats_send;
#[cfg(test)]
mod tests_proxy;

pub use bindings::{DynLib, TdBindings};
pub use key::DatabaseKey;

use auth::Mapped;

/// TDLib log file inside the TDLib folder; TDLib rotates it to `.old` past the limit.
pub const LOG_FILE: &str = "tdlib.log";
/// Highest verbosity allowed before `Ready`.
const MAX_VERBOSITY: i32 = 2;
/// How long one `td_receive` waits: also the delay for the thread to notice a stop.
const RECEIVE_POLL: Duration = Duration::from_millis(250);
const JOIN_TIMEOUT: Duration = Duration::from_secs(1);
/// How long the transport waits for one of its own states (`Starting`, `Closed`) to become
/// visible to `auth_state()` subscribers. Only a subscriber holding a `watch::Ref` delays it.
const PUBLISH_WAIT: Duration = Duration::from_millis(500);
/// Name of the `OtherStep` shown when `getMe` fails right after `authorizationStateReady`.
pub const ME_UNAVAILABLE: &str = "me_unavailable";

/// `%LOCALAPPDATA%\squeezegram\tdlib`.
pub fn default_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("squeezegram").join("tdlib"))
}

/// What `TdlibJson` needs; every path lives under `dir` (injectable for tests).
#[derive(Clone)]
pub struct TdlibConfig {
    /// Database (`db\`), files (`files\`), `key.bin` and the log.
    pub dir: PathBuf,
    pub api_id: i32,
    /// Injected at build time or typed by the user; redacted in `Debug`.
    pub api_hash: String,
    /// Shown in Telegram > Devices (`telegram.device_name`); never empty.
    pub device_model: String,
    pub application_version: String,
    pub system_language_code: String,
    /// Clamped to `MAX_VERBOSITY`.
    pub log_verbosity: i32,
    pub log_max_bytes: u64,
    /// Requests that may go to the network (login steps, `logOut`).
    pub request_timeout: Duration,
    /// Each local start step (TDLib needed 2 to 60 ms when measured).
    pub start_timeout: Duration,
    /// `close` then `authorizationStateClosed` (6 to 22 ms when measured).
    pub close_timeout: Duration,
    /// After the `deleteMessages` of a cancel: how long a success that crossed it is awaited.
    pub cancel_grace: Duration,
    /// `setNetworkType(networkTypeNone)` before the parameters: tests and offline runs only.
    pub network_disabled: bool,
    /// How long `list_dialogs` waits for the first `updateChatFolders` of the session (TDLib
    /// sends it right after sign-in); after that the folders are taken as unknown.
    pub folders_wait: Duration,
    /// Proxy of the first start (`None`: direct). `Err`: one is set but cannot be used (its
    /// secret is unreadable), and no start connects until `set_proxy` replaces it.
    pub proxy: Result<Option<Proxy>, TransportError>,
}

impl TdlibConfig {
    pub fn new(dir: PathBuf, api_id: i32, api_hash: String, device_model: String) -> Self {
        Self {
            dir,
            api_id,
            api_hash,
            device_model,
            application_version: env!("CARGO_PKG_VERSION").to_owned(),
            system_language_code: "en".to_owned(),
            log_verbosity: 1,
            log_max_bytes: 5 * MIB,
            request_timeout: Duration::from_secs(20),
            start_timeout: Duration::from_secs(10),
            close_timeout: Duration::from_secs(2),
            cancel_grace: Duration::from_secs(2),
            network_disabled: false,
            folders_wait: Duration::from_secs(3),
            proxy: Ok(None),
        }
    }

    fn device_model(&self) -> &str {
        let model = self.device_model.trim();
        if model.is_empty() {
            "squeezegram"
        } else {
            model
        }
    }
}

impl fmt::Debug for TdlibConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TdlibConfig")
            .field("dir", &self.dir)
            .field("api_id", &self.api_id)
            .field("api_hash", &"<redacted>")
            .field("device_model", &self.device_model)
            .field("application_version", &self.application_version)
            .field("log_verbosity", &self.log_verbosity)
            .field("network_disabled", &self.network_disabled)
            .field("proxy", &self.proxy)
            .finish_non_exhaustive()
    }
}

/// OS id of the calling thread, the one a crash dump shows (journal field `os_thread`).
pub(crate) fn os_thread_id() -> u32 {
    #[cfg(windows)]
    {
        // SAFETY: no argument, no failure.
        unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Receiver {
    handle: JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

/// One state for the publisher thread: applied unless `only_if` names an older generation;
/// `done` is answered once it is visible (or skipped).
struct Publish {
    state: AuthState,
    only_if: Option<u64>,
    done: Option<oneshot::Sender<()>>,
}

/// `auth_state()` behind its own thread. Writing a `tokio::sync::watch` waits for every
/// `watch::Ref` a subscriber holds; done on the TDLib receive thread (as before), one guard
/// kept across an `.await` froze the dispatch of every TDLib message, and `shutdown` with it
/// (real read-only test, 2026-10-09). Here the receive thread only queues; the transport's own
/// waits (`Starting`, `Closed`) are bounded by `PUBLISH_WAIT`.
struct AuthPublisher {
    watch: Arc<watch::Sender<AuthState>>,
    /// Bumped by every publication, so a late `Ready(me)` never overwrites a newer state.
    generation: Arc<AtomicU64>,
    /// `None` if the thread could not start: states are then written inline.
    queue: Option<std::sync::mpsc::Sender<Publish>>,
}

impl AuthPublisher {
    fn new(initial: AuthState) -> Self {
        let watch = Arc::new(watch::Sender::new(initial));
        let generation = Arc::new(AtomicU64::new(0));
        let (queue, states) = std::sync::mpsc::channel::<Publish>();
        let (target, current) = (Arc::clone(&watch), Arc::clone(&generation));
        // Ends when the transport is dropped (the queue closes).
        let spawned = std::thread::Builder::new()
            .name("tdlib-auth-state".into())
            .spawn(move || {
                for publish in states {
                    apply(&target, &current, publish);
                }
            });
        Self {
            watch,
            generation,
            queue: spawned.ok().map(|_| queue),
        }
    }

    fn subscribe(&self) -> watch::Receiver<AuthState> {
        self.watch.subscribe()
    }

    /// Queues the state and returns false if it was written inline (no thread).
    fn enqueue(&self, publish: Publish) -> bool {
        match &self.queue {
            Some(queue) => match queue.send(publish) {
                Ok(()) => true,
                Err(std::sync::mpsc::SendError(publish)) => {
                    apply(&self.watch, &self.generation, publish);
                    false
                }
            },
            None => {
                apply(&self.watch, &self.generation, publish);
                false
            }
        }
    }

    /// Never waits (the receive thread).
    fn publish(&self, state: AuthState) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.enqueue(Publish {
            state,
            only_if: None,
            done: None,
        });
    }

    fn bump(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Applied only if nothing was published since `bump` returned `expected`.
    fn publish_if(&self, expected: u64, state: AuthState) {
        self.enqueue(Publish {
            state,
            only_if: Some(expected),
            done: None,
        });
    }

    /// The transport's own states: visible to subscribers on return, or after `PUBLISH_WAIT`
    /// at most (a subscriber holding a `watch::Ref` then delays only the publisher thread).
    async fn publish_wait(&self, state: AuthState) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let (done, applied) = oneshot::channel();
        let queued = self.enqueue(Publish {
            state,
            only_if: None,
            done: Some(done),
        });
        if queued && tokio::time::timeout(PUBLISH_WAIT, applied).await.is_err() {
            tracing::warn!(
                event = "auth_state_publish_slow",
                wait_ms = u64::try_from(PUBLISH_WAIT.as_millis()).unwrap_or(u64::MAX),
                "sign-in state not published in time (a subscriber holds the state)"
            );
        }
    }
}

fn apply(watch: &watch::Sender<AuthState>, generation: &AtomicU64, publish: Publish) {
    if publish
        .only_if
        .is_none_or(|expected| generation.load(Ordering::Acquire) == expected)
    {
        watch.send_replace(publish.state);
    }
    if let Some(done) = publish.done {
        let _ = done.send(());
    }
}

/// Who receives the updates an upload follows (`send::UPLOAD_UPDATES`), fed by the receive
/// thread. Not generic, so a `Subscription` can unregister itself on drop.
#[derive(Default)]
struct Listeners {
    next: AtomicU64,
    list: Mutex<Vec<(u64, mpsc::UnboundedSender<Value>)>>,
}

impl Listeners {
    fn subscribe(self: &Arc<Self>) -> Subscription {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::unbounded_channel();
        lock(&self.list).push((id, sender));
        Subscription {
            id,
            receiver,
            listeners: Arc::clone(self),
        }
    }

    fn forward(&self, message: &Value) {
        let mut list = lock(&self.list);
        list.retain(|(_, sender)| sender.send(message.clone()).is_ok());
    }

    /// The receive thread stopped: every subscriber sees its channel close.
    fn close_all(&self) {
        lock(&self.list).clear();
    }
}

/// Updates for one upload, in TDLib order; `None` once the receive thread stopped.
pub(crate) struct Subscription {
    id: u64,
    receiver: mpsc::UnboundedReceiver<Value>,
    listeners: Arc<Listeners>,
}

impl Subscription {
    async fn recv(&mut self) -> Option<Value> {
        self.receiver.recv().await
    }

    fn try_recv(&mut self) -> Result<Value, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        lock(&self.listeners.list).retain(|(id, _)| *id != self.id);
    }
}

struct Inner<B: TdBindings> {
    bindings: B,
    config: TdlibConfig,
    next_extra: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    /// Current client, 0 when there is none.
    client_id: AtomicI32,
    /// Held by the receive thread from its "is this the current client?" check to the
    /// publication of the state, and by `forget_client` around the reset of `client_id`: a
    /// state of the old client is then queued before the transport's own final state, never
    /// after it (a late `Closed` would hide a `StartFailed`).
    client_gate: Mutex<()>,
    /// Last raw `authorizationState*` type per client, for `start` / `shutdown` waits.
    raw: watch::Sender<(i32, String)>,
    auth: AuthPublisher,
    receiver: Mutex<Option<Receiver>>,
    /// Serialises `start`, `log_out` and `shutdown`.
    lifecycle: tokio::sync::Mutex<()>,
    version: Mutex<Option<String>>,
    /// Numbers the `tdlib_ffi` journal lines (every call into `tdjson`).
    ffi_calls: AtomicU64,
    listeners: Arc<Listeners>,
    /// Last `me()` of this session (`my_id`, Premium, limits); cleared on close and log out.
    me_cache: Mutex<Option<Me>>,
    /// Chat folders from the last `updateChatFolders` (`None` until it arrives); cleared on
    /// close and log out. Names are personal data, never logged.
    folders: Mutex<Option<Vec<ChatFolder>>>,
    /// Applied by every start (`open`) and by `set_proxy`; see `TdlibConfig::proxy`.
    proxy: Mutex<Result<Option<Proxy>, TransportError>>,
}

/// The TDLib transport over any `TdBindings` (`DynLib` in the app, `Replay` in tests).
pub struct TdlibJson<B: TdBindings> {
    inner: Arc<Inner<B>>,
}

impl<B: TdBindings> fmt::Debug for TdlibJson<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TdlibJson")
            .field("config", &self.inner.config)
            .field("client_id", &self.inner.client_id.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl TdlibJson<DynLib> {
    /// The app's transport: `tdjson.dll` at `lib_path`, loaded on first use.
    pub fn with_dll(lib_path: impl Into<PathBuf>, config: TdlibConfig) -> Self {
        Self::new(DynLib::new(lib_path), config)
    }
}

impl<B: TdBindings> TdlibJson<B> {
    pub fn new(bindings: B, config: TdlibConfig) -> Self {
        let proxy = config.proxy.clone();
        Self {
            inner: Arc::new(Inner {
                bindings,
                config,
                next_extra: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
                client_id: AtomicI32::new(0),
                client_gate: Mutex::new(()),
                raw: watch::Sender::new((0, String::new())),
                auth: AuthPublisher::new(AuthState::Starting),
                receiver: Mutex::new(None),
                lifecycle: tokio::sync::Mutex::new(()),
                version: Mutex::new(None),
                ffi_calls: AtomicU64::new(0),
                listeners: Arc::new(Listeners::default()),
                me_cache: Mutex::new(None),
                folders: Mutex::new(None),
                proxy: Mutex::new(proxy),
            }),
        }
    }

    pub fn bindings(&self) -> &B {
        &self.inner.bindings
    }

    /// `getOption("version")` read at start (`None` before).
    pub fn tdlib_version(&self) -> Option<String> {
        lock(&self.inner.version).clone()
    }

    /// Makes the chat of `peer` known to TDLib before any operation on it:
    /// with `use_message_database = false` a chat not loaded in memory is `400 Chat not
    /// found` after a restart. `create*Chat`, then `loadChats` and a second try if TDLib does
    /// not know the peer. Returns the TDLib `chat_id`. Every operation on a `Target` goes
    /// through it (`list_topics`, `can_send`, `upload`, `verify_sent`).
    pub async fn ensure_chat_known(&self, peer: &Peer) -> Result<i64, TransportError> {
        let chat = self.inner.known_chat(peer).await?;
        chats::chat_id(&chat)
    }
}

impl<B: TdBindings> Drop for TdlibJson<B> {
    fn drop(&mut self) {
        // The thread holds the other `Arc<Inner>`; it exits within `RECEIVE_POLL`.
        let receiver = lock(&self.inner.receiver).take();
        tracing::debug!(
            event = "tdlib_drop",
            client = self.inner.current_client(),
            receiver = receiver.is_some(),
            os_thread = os_thread_id(),
            "TDLib transport dropped"
        );
        if let Some(receiver) = receiver {
            receiver.stop.store(true, Ordering::Release);
        }
    }
}

impl<B: TdBindings> Inner<B> {
    fn current_client(&self) -> i32 {
        self.client_id.load(Ordering::Acquire)
    }

    /// Journal line written *before* a call into `tdjson`: after a native crash the last
    /// `tdlib_ffi` names the call in progress. The request type only, never its content.
    fn trace_ffi(&self, call: &'static str, what: &str, client: i32) {
        let n = self.ffi_calls.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::debug!(
            event = "tdlib_ffi",
            call,
            what,
            client,
            n,
            os_thread = os_thread_id(),
            "TDLib call"
        );
    }

    /// Synchronous request; its answer must be `ok`.
    fn execute_ok(&self, what: &'static str, body: &Value) -> Result<(), TransportError> {
        self.trace_ffi("td_execute", what, 0);
        let answer = self
            .bindings
            .execute(&body.to_string())?
            .ok_or_else(|| TransportError::Protocol(format!("{what}: no answer")))?;
        let answer: Value = serde_json::from_str(&answer)
            .map_err(|_| TransportError::Protocol(format!("{what}: answer is not JSON")))?;
        match answer["@type"].as_str() {
            Some("ok") => Ok(()),
            Some("error") => Err(TransportError::Protocol(format!(
                "{what}: {}",
                answer["message"].as_str().unwrap_or_default()
            ))),
            _ => Err(TransportError::Protocol(format!(
                "{what}: unexpected answer"
            ))),
        }
    }

    /// Sends a request with a fresh `@extra`; the answer arrives on the receiver.
    fn submit(
        &self,
        what: &'static str,
        mut body: Value,
    ) -> Result<(u64, oneshot::Receiver<Value>), TransportError> {
        let client = self.current_client();
        if client == 0 {
            return Err(TransportError::NotAuthorized);
        }
        let extra = self.next_extra.fetch_add(1, Ordering::Relaxed);
        body.as_object_mut()
            .ok_or_else(|| TransportError::Protocol(format!("{what}: request is not an object")))?
            .insert("@extra".into(), json!(extra));
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(extra, sender);
        self.trace_ffi("td_send", what, client);
        if let Err(error) = self.bindings.send(client, &body.to_string()) {
            lock(&self.pending).remove(&extra);
            return Err(error);
        }
        Ok((extra, receiver))
    }

    /// Waits for the answer under `timeout`; a TDLib `error` becomes a `TransportError`.
    async fn wait(
        &self,
        what: &'static str,
        extra: u64,
        receiver: oneshot::Receiver<Value>,
        timeout: Duration,
    ) -> Result<Value, TransportError> {
        let answer = match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => {
                return Err(TransportError::Protocol(format!(
                    "{what}: TDLib receive loop stopped"
                )));
            }
            Err(_) => {
                lock(&self.pending).remove(&extra);
                return Err(TransportError::Timeout(what));
            }
        };
        match answer["@type"].as_str() {
            Some("error") => Err(auth::map_error(&answer)),
            Some(_) => Ok(answer),
            None => Err(TransportError::Protocol(format!(
                "{what}: answer without @type"
            ))),
        }
    }

    async fn request(
        &self,
        what: &'static str,
        body: Value,
        timeout: Duration,
    ) -> Result<Value, TransportError> {
        let (extra, receiver) = self.submit(what, body)?;
        self.wait(what, extra, receiver, timeout).await
    }

    async fn wait_raw(
        &self,
        client: i32,
        kind: &'static str,
        timeout: Duration,
    ) -> Result<(), TransportError> {
        let mut raw = self.raw.subscribe();
        match tokio::time::timeout(
            timeout,
            raw.wait_for(|(id, current)| *id == client && current == kind),
        )
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err(TransportError::Protocol("state channel closed".into())),
            Err(_) => Err(TransportError::Timeout(kind)),
        }
    }

    async fn option_integer(&self, name: &str) -> Result<i64, TransportError> {
        let option = self
            .request(
                "getOption",
                json!({ "@type": "getOption", "name": name }),
                self.config.request_timeout,
            )
            .await?;
        if option["@type"] != "optionValueInteger" {
            return Err(TransportError::Protocol(format!(
                "option {name} is not an integer"
            )));
        }
        // TDLib JSON writes int64 as a string.
        match &option["value"] {
            Value::String(text) => text.parse().ok(),
            value => value.as_i64(),
        }
        .ok_or_else(|| TransportError::Protocol(format!("option {name}: bad value")))
    }

    async fn me(&self) -> Result<Me, TransportError> {
        let user = self
            .request(
                "getMe",
                json!({ "@type": "getMe" }),
                self.config.request_timeout,
            )
            .await?;
        let bad = |field: &str| TransportError::Protocol(format!("getMe: missing {field}"));
        if user["@type"] != "user" {
            return Err(bad("user"));
        }
        let user_id = user["id"].as_i64().ok_or_else(|| bad("id"))?;
        let first = user["first_name"]
            .as_str()
            .ok_or_else(|| bad("first_name"))?;
        let last = user["last_name"].as_str().unwrap_or_default();
        let is_premium = user["is_premium"]
            .as_bool()
            .ok_or_else(|| bad("is_premium"))?;
        let username = user["usernames"]["active_usernames"][0]
            .as_str()
            .map(str::to_owned);
        let caption = self.option_integer("message_caption_length_max").await?;
        let me = Me {
            user_id,
            display_name: format!("{first} {last}").trim().to_owned(),
            username,
            is_premium,
            caption_max_chars: u32::try_from(caption)
                .map_err(|_| TransportError::Protocol("caption length out of range".into()))?,
            // No API gives it: 4000 MiB with Premium, else 2000 MiB.
            upload_limit_bytes: if is_premium { 4000 * MIB } else { 2000 * MIB },
        };
        *lock(&self.me_cache) = Some(me.clone());
        Ok(me)
    }

    /// `me()` of this session if already read, else read now.
    async fn cached_me(&self) -> Result<Me, TransportError> {
        let cached = lock(&self.me_cache).clone();
        match cached {
            Some(me) => Ok(me),
            None => self.me().await,
        }
    }

    fn ensure_receiver(self: &Arc<Self>) -> Result<(), TransportError> {
        let runtime = Handle::try_current()
            .map_err(|_| TransportError::Protocol("no async runtime".into()))?;
        let mut receiver = lock(&self.receiver);
        if receiver.as_ref().is_some_and(|r| !r.handle.is_finished()) {
            return Ok(());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let inner = Arc::clone(self);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("tdlib-receive".into())
            .spawn(move || receive_loop(&inner, &thread_stop, &runtime))
            .map_err(|_| TransportError::Protocol("cannot start the TDLib thread".into()))?;
        *receiver = Some(Receiver { handle, stop });
        Ok(())
    }

    async fn stop_receiver(&self) {
        let Some(receiver) = lock(&self.receiver).take() else {
            return;
        };
        receiver.stop.store(true, Ordering::Release);
        // Polled, never a blocking `join` on the runtime's pool: a thread stuck in `tdjson`
        // would keep that task, and the runtime's shutdown, waiting forever.
        let deadline = std::time::Instant::now() + JOIN_TIMEOUT;
        while !receiver.handle.is_finished() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if receiver.handle.is_finished() {
            let _ = receiver.handle.join();
        } else {
            // Dropping the handle detaches the thread.
            tracing::warn!(
                event = "tdlib_thread_stuck",
                "TDLib receive thread did not stop in time"
            );
        }
    }

    /// Log settings, key, client, `setTdlibParameters`. On error the caller closes the client.
    async fn open(self: &Arc<Self>) -> Result<(), TransportError> {
        let config = &self.config;
        // A proxy that cannot be used stops the start before any client exists: connecting
        // without it could expose the user it is meant to protect.
        let proxy = lock(&self.proxy).clone()?;
        // 1. Verbosity first: nothing is logged with the default level.
        self.execute_ok(
            "setLogVerbosityLevel",
            &json!({
                "@type": "setLogVerbosityLevel",
                "new_verbosity_level": config.log_verbosity.clamp(0, MAX_VERBOSITY),
            }),
        )?;
        let database = config.dir.join("db");
        let files = config.dir.join("files");
        for dir in [&config.dir, &database, &files] {
            std::fs::create_dir_all(dir).map_err(|error| {
                TransportError::Protocol(format!("TDLib folder: {}", error.kind()))
            })?;
        }
        self.execute_ok(
            "setLogStream",
            &json!({
                "@type": "setLogStream",
                "log_stream": {
                    "@type": "logStreamFile",
                    "path": path_text(&config.dir.join(LOG_FILE))?,
                    "max_file_size": config.log_max_bytes,
                    "redirect_stderr": false,
                },
            }),
        )?;
        let key = DatabaseKey::load_or_create(&config.dir)?;
        self.ensure_receiver()?;
        self.trace_ffi("td_create_client_id", "", 0);
        let client = self.bindings.create_client_id()?;
        self.client_id.store(client, Ordering::Release);
        self.auth.publish_wait(AuthState::Starting).await;
        // 2. The first request creates the instance; `version` is readable before parameters.
        let version = self
            .request(
                "getOption",
                json!({ "@type": "getOption", "name": "version" }),
                config.start_timeout,
            )
            .await?;
        *lock(&self.version) = version["value"].as_str().map(str::to_owned);
        self.wait_raw(client, auth::WAIT_PARAMETERS, config.start_timeout)
            .await?;
        // 3. Queued before the parameters, answered after them. TDLib runs the proxy choice
        // before its first connection (`proxy.rs`).
        let (proxy_what, proxy_body) = proxy::choice_request(proxy.as_ref());
        let (proxy_extra, proxy_answer) = self.submit(proxy_what, proxy_body)?;
        let network = if config.network_disabled {
            Some(self.submit(
                "setNetworkType",
                json!({ "@type": "setNetworkType", "type": { "@type": "networkTypeNone" } }),
            )?)
        } else {
            None
        };
        let parameters = json!({
            "@type": "setTdlibParameters",
            "use_test_dc": false,
            "database_directory": path_text(&database)?,
            "files_directory": path_text(&files)?,
            "database_encryption_key": key.base64(),
            "use_file_database": true,
            "use_chat_info_database": true,
            // Imposed: with a message database TDLib resends by itself after a restart.
            "use_message_database": false,
            "use_secret_chats": false,
            "api_id": config.api_id,
            "api_hash": config.api_hash,
            "system_language_code": config.system_language_code,
            "device_model": config.device_model(),
            "system_version": "",
            "application_version": config.application_version,
        });
        drop(key);
        match self
            .request("setTdlibParameters", parameters, config.start_timeout)
            .await
        {
            Err(TransportError::NotAuthorized) => return Err(TransportError::BadDatabaseKey),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        if let Some((extra, receiver)) = network {
            self.wait("setNetworkType", extra, receiver, config.start_timeout)
                .await?;
        }
        self.finish_proxy(proxy_what, proxy_extra, proxy_answer, config.start_timeout)
            .await
    }

    /// No current client any more; what this session read about the account goes with it.
    fn forget_client(&self) {
        {
            let _gate = lock(&self.client_gate);
            self.client_id.store(0, Ordering::Release);
        }
        *lock(&self.me_cache) = None;
        *lock(&self.folders) = None;
    }

    /// `close` then `authorizationStateClosed`, bounded by `close_timeout`; forgets the client
    /// and publishes `last`: `Closed` on shutdown, `StartFailed` after a start that failed
    /// (also before creating a client).
    async fn close_client(&self, last: AuthState) {
        let client = self.current_client();
        if client == 0 {
            self.auth.publish_wait(last).await;
            return;
        }
        let timeout = self.config.close_timeout;
        let closing = async {
            self.request("close", json!({ "@type": "close" }), timeout)
                .await?;
            self.wait_raw(client, auth::CLOSED, timeout).await
        };
        if let Err(error) = tokio::time::timeout(timeout, closing)
            .await
            .unwrap_or(Err(TransportError::Timeout("close")))
        {
            tracing::warn!(event = "tdlib_close", error_code = error.code_and_params().0, error = %error, "TDLib close did not finish cleanly");
        }
        self.forget_client();
        self.auth.publish_wait(last).await;
    }

    /// `open`, and on error the client closed with `StartFailed` (a 401 key error requires a
    /// new client anyway). Caller holds the lifecycle lock.
    async fn open_or_fail(self: &Arc<Self>) -> Result<(), TransportError> {
        let result = self.open().await;
        if let Err(error) = &result {
            self.close_client(AuthState::StartFailed(error.clone()))
                .await;
        }
        result
    }
}

fn path_text(path: &Path) -> Result<&str, TransportError> {
    path.to_str()
        .ok_or_else(|| TransportError::Protocol("TDLib folder path is not UTF-8".into()))
}

fn receive_loop<B: TdBindings>(inner: &Arc<Inner<B>>, stop: &AtomicBool, runtime: &Handle) {
    let os_thread = os_thread_id();
    tracing::info!(
        event = "tdlib_thread",
        phase = "start",
        os_thread,
        "TDLib receive thread started"
    );
    let (mut polls, mut messages) = (0u64, 0u64);
    while !stop.load(Ordering::Acquire) {
        polls += 1;
        // Four empty polls a second: `trace` only.
        tracing::trace!(
            event = "tdlib_ffi",
            call = "td_receive",
            n = polls,
            os_thread,
            "TDLib call"
        );
        match inner.bindings.receive(RECEIVE_POLL) {
            Ok(Some(text)) => {
                messages += 1;
                dispatch(inner, &text, runtime, messages);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(event = "tdlib_receive", error_code = error.code_and_params().0, error = %error, "td_receive failed");
                std::thread::sleep(RECEIVE_POLL);
            }
        }
    }
    // Callers still waiting get "receive loop stopped" instead of their timeout.
    lock(&inner.pending).clear();
    inner.listeners.close_all();
    tracing::info!(
        event = "tdlib_thread",
        phase = "stop",
        os_thread,
        polls,
        messages,
        "TDLib receive thread stopped"
    );
}

fn dispatch<B: TdBindings>(inner: &Arc<Inner<B>>, text: &str, runtime: &Handle, n: u64) {
    // Messages may carry secrets (QR token, phone): their content is never logged.
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        tracing::warn!(
            event = "tdlib_bad_message",
            "TDLib message is not JSON, ignored"
        );
        return;
    };
    // The type only (`updateOption`, `ok`, `error`…), never a field of the message.
    tracing::debug!(
        event = "tdlib_recv",
        kind = message["@type"].as_str().unwrap_or_default(),
        client = message["@client_id"].as_i64(),
        has_extra = message.get("@extra").is_some(),
        n,
        "TDLib message"
    );
    if let Some(extra) = message.get("@extra") {
        if let Some(sender) = extra
            .as_u64()
            .and_then(|id| lock(&inner.pending).remove(&id))
        {
            let _ = sender.send(message);
        }
        return;
    }
    let kind = message["@type"].as_str().unwrap_or_default();
    let current = message["@client_id"].as_i64() == Some(i64::from(inner.current_client()));
    if kind == "updateAuthorizationState" {
        on_authorization_state(inner, &message, runtime);
    } else if kind == chats::UPDATE_CHAT_FOLDERS && current {
        match chats::parse_folders(&message) {
            Ok(folders) => *lock(&inner.folders) = Some(folders),
            Err(error) => {
                tracing::warn!(event = "tdlib_bad_message", error = %error, "chat folders update ignored")
            }
        }
    } else if send::UPLOAD_UPDATES.contains(&kind)
        && message["@client_id"].as_i64() == Some(i64::from(inner.current_client()))
    {
        inner.listeners.forward(&message);
    }
    // Other updates (chat list, users…) are not needed: chats are read on demand (`getChats`).
}

fn on_authorization_state<B: TdBindings>(inner: &Arc<Inner<B>>, message: &Value, runtime: &Handle) {
    let Some(client) = message["@client_id"]
        .as_i64()
        .and_then(|id| i32::try_from(id).ok())
    else {
        tracing::warn!(
            event = "tdlib_bad_message",
            "authorization update without @client_id, ignored"
        );
        return;
    };
    let state = &message["authorization_state"];
    if let Some(kind) = state["@type"].as_str() {
        inner.raw.send_replace((client, kind.to_owned()));
    }
    // Until the state is queued: see `Inner::client_gate`.
    let _gate = lock(&inner.client_gate);
    if client != inner.current_client() {
        return;
    }
    match auth::map_state(state) {
        Ok(Mapped::State(state)) => inner.auth.publish(state),
        Ok(Mapped::Ready) => {
            let generation = inner.auth.bump();
            let inner = Arc::clone(inner);
            runtime.spawn(async move {
                let state = match inner.me().await {
                    Ok(me) => AuthState::Ready(me),
                    Err(error) => {
                        tracing::warn!(event = "tdlib_get_me", error_code = error.code_and_params().0, error = %error, "getMe after sign-in failed");
                        AuthState::OtherStep {
                            name: ME_UNAVAILABLE.to_owned(),
                        }
                    }
                };
                inner.auth.publish_if(generation, state);
            });
        }
        Err(error) => {
            tracing::warn!(event = "tdlib_auth_state", error_code = error.code_and_params().0, error = %error, "unexpected authorization state")
        }
    }
}

#[async_trait]
impl<B: TdBindings> TelegramTransport for TdlibJson<B> {
    async fn start(&self) -> Result<(), TransportError> {
        let inner = &self.inner;
        let _lifecycle = inner.lifecycle.lock().await;
        if inner.current_client() != 0 {
            return Ok(());
        }
        // A failed start leaves no half-open client, and says why (`StartFailed`).
        inner.open_or_fail().await
    }

    fn auth_state(&self) -> watch::Receiver<AuthState> {
        self.inner.auth.subscribe()
    }

    async fn auth_submit(&self, input: AuthInput) -> Result<(), TransportError> {
        let (what, body) = auth::request_for(input);
        let inner = &self.inner;
        inner
            .request(what, body, inner.config.request_timeout)
            .await
            .map(|_| ())
    }

    /// `logOut` (server session revoked, local database destroyed by TDLib), then a fresh
    /// client so the login screen comes back (`WaitPhone`).
    async fn log_out(&self) -> Result<(), TransportError> {
        let inner = &self.inner;
        let _lifecycle = inner.lifecycle.lock().await;
        let client = inner.current_client();
        if client == 0 {
            return Err(TransportError::NotAuthorized);
        }
        let timeout = inner.config.request_timeout;
        inner
            .request("logOut", json!({ "@type": "logOut" }), timeout)
            .await?;
        inner.wait_raw(client, auth::CLOSED, timeout).await?;
        inner.forget_client();
        inner.open_or_fail().await
    }

    async fn me(&self) -> Result<Me, TransportError> {
        self.inner.me().await
    }

    async fn list_dialogs(&self, limit: usize) -> Result<Vec<Dialog>, TransportError> {
        self.inner.list_dialogs(limit).await
    }

    async fn list_topics(&self, peer: &Peer) -> Result<Vec<Topic>, TransportError> {
        self.inner.list_topics(peer).await
    }

    async fn search_chats(&self, query: &str, limit: usize) -> Result<Vec<Dialog>, TransportError> {
        self.inner.search_chats(query, limit).await
    }

    async fn can_send(
        &self,
        target: &Target,
        media: MediaKind,
    ) -> Result<SendCheck, TransportError> {
        self.inner.can_send(target, media).await
    }

    async fn upload(
        &self,
        req: UploadRequest,
        progress: mpsc::Sender<Progress>,
        cancel: CancellationToken,
    ) -> Result<Sent, TransportError> {
        self.inner.upload(req, progress, cancel).await
    }

    async fn verify_sent(&self, probe: &SentProbe) -> Result<Vec<SentCandidate>, TransportError> {
        self.inner.verify_sent(probe).await
    }

    async fn message_link(
        &self,
        target: &Target,
        message_id: i64,
    ) -> Result<Option<String>, TransportError> {
        self.inner.message_link(target, message_id).await
    }

    /// Under the lifecycle lock, so a start in progress applies either the old choice or this
    /// one, never half of each. A choice TDLib refuses is not kept for the next start.
    async fn set_proxy(&self, proxy: Option<Proxy>) -> Result<(), TransportError> {
        let inner = &self.inner;
        let _lifecycle = inner.lifecycle.lock().await;
        let previous = std::mem::replace(&mut *lock(&inner.proxy), Ok(proxy.clone()));
        if inner.current_client() == 0 {
            return Ok(());
        }
        let result = inner.apply_proxy(proxy.as_ref()).await;
        if result.is_err() {
            *lock(&inner.proxy) = previous;
        }
        result
    }

    async fn test_proxy(&self, proxy: &Proxy) -> Result<Duration, TransportError> {
        self.inner.ping_proxy(proxy).await
    }

    /// Under the lifecycle lock, and only while no client runs after a failed start: TDLib
    /// never has the files open when they are deleted. Then the same start as `start`.
    async fn reset_session(&self) -> Result<(), TransportError> {
        let inner = &self.inner;
        let _lifecycle = inner.lifecycle.lock().await;
        let failed = matches!(*inner.auth.watch.borrow(), AuthState::StartFailed(_));
        if inner.current_client() != 0 || !failed {
            return Err(TransportError::SessionInUse);
        }
        match session::reset(&inner.config.dir) {
            Ok(removed) => tracing::info!(
                event = "session_reset",
                removed = removed.join(","),
                "local Telegram session deleted, starting again"
            ),
            Err(error) => {
                tracing::warn!(event = "session_reset", error_code = error.code_and_params().0, error = %error, "local Telegram session could not be deleted");
                return Err(error);
            }
        }
        inner.auth.publish_wait(AuthState::Starting).await;
        inner.open_or_fail().await
    }

    async fn shutdown(&self) {
        let inner = &self.inner;
        let _lifecycle = inner.lifecycle.lock().await;
        inner.close_client(AuthState::Closed).await;
        inner.stop_receiver().await;
    }
}
