//! Tauri commands. The Telegram ones delegate to `transport::service`.
//! Errors reach the front as `{ code, params }`; the texts live in the front's i18n files.
//!
//! Every command writes one `command` line to the journal (name, duration, result or
//! `error_code`, safe arguments only: ids, counts, kinds, setting keys; never a caption, a
//! value typed at sign-in or a setting value). Read-only commands log at `debug`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value, json};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;

use crate::limit::{EffectiveLimit, LimitResolver};
use crate::logging::{self, export, frontend};
use crate::media::stage::{FfmpegStage, GpuFailure};
use crate::queue::events::{
    EventSink, QUEUE_ALERT, QUEUE_CHANGED, QUEUE_PROGRESS, QUEUE_STATUS, QueueAlert, QueueChanged,
    QueueProgress, QueueStatus, SIDECARS_PROGRESS, SIDECARS_STATUS, SidecarProgress,
};
use crate::queue::intake;
use crate::queue::orchestrator::{Orchestrator, ResolveAction};
use crate::queue::store::{
    ClearScope, Destination, DestinationPatch, HistoryEntry, HistoryFilter, Item, ItemPatch,
    NewItem, SendMode,
};
use crate::queue::{QueueError, State as ItemState};
use crate::settings::{Settings, SettingsError, SettingsStore, patch_keys};
use crate::sidecars::discover::{self, Discovery};
use crate::sidecars::{SidecarError, SidecarService, SidecarStatus};
use crate::transport::select::TransportKind;
use crate::transport::service::{self, AuthPayload, DestinationCheck, ServiceError};
use crate::transport::{AuthInput, SentCandidate, TelegramTransport, TransportError};

/// Event of the login state machine.
pub const AUTH_STATE: &str = "auth://state";
/// Queue items whose error codes go into a debug export.
const EXPORT_ERRORS: i64 = 50;

pub struct AppState {
    pub queue: Orchestrator,
    pub settings: Arc<SettingsStore>,
    pub sidecars: Arc<SidecarService>,
    pub transport: Arc<dyn TelegramTransport>,
    /// Which transport runs (`auth_state`/`auth://state` carry it: the UI flags the mock).
    pub transport_kind: TransportKind,
    /// Kept for the encoder detection of `debug_export`.
    pub media: Arc<FfmpegStage>,
    /// Effective upload limit (`limit_status`); the media stage holds the same resolver.
    pub limits: Arc<LimitResolver>,
    /// Items changed by the startup recovery, announced once the page has loaded.
    pub recovered: Mutex<Vec<i64>>,
}

#[derive(Debug, Serialize)]
pub struct CommandError {
    pub code: &'static str,
    pub params: Value,
}

impl CommandError {
    fn io() -> Self {
        Self {
            code: "IO_ERROR",
            params: json!({}),
        }
    }
}

impl From<QueueError> for CommandError {
    fn from(error: QueueError) -> Self {
        let params = match &error {
            QueueError::ItemNotFound(id) | QueueError::ItemStarted(id) => json!({ "itemId": id }),
            QueueError::DestinationNotFound(id) => json!({ "destinationId": id }),
            QueueError::IllegalTransition { item_id, from, to } => {
                json!({ "itemId": item_id, "from": from, "to": to })
            }
            QueueError::FileUnreadable(path) => json!({ "path": path }),
            other => {
                tracing::warn!(event = "command_error_detail", error_code = other.code(), error = %other, "queue error");
                json!({})
            }
        };
        Self {
            code: error.code(),
            params,
        }
    }
}

impl From<TransportError> for CommandError {
    fn from(error: TransportError) -> Self {
        let (code, params) = error.code_and_params();
        tracing::warn!(event = "transport_error", error_code = code, error = %error, "Telegram request failed");
        Self { code, params }
    }
}

impl From<ServiceError> for CommandError {
    fn from(error: ServiceError) -> Self {
        match error {
            ServiceError::Queue(error) => error.into(),
            ServiceError::Transport(error) => error.into(),
        }
    }
}

impl From<SettingsError> for CommandError {
    fn from(error: SettingsError) -> Self {
        Self {
            code: error.code(),
            params: json!({}),
        }
    }
}

impl From<SidecarError> for CommandError {
    fn from(error: SidecarError) -> Self {
        tracing::warn!(event = "command_error_detail", error_code = error.code(), error = %error, "sidecar error");
        Self {
            code: error.code(),
            params: json!({}),
        }
    }
}

type CommandResult<T> = Result<T, CommandError>;

/// One command call in the journal: started by the command, finished with its result.
struct Call {
    name: &'static str,
    started: Instant,
    quiet: bool,
    item_id: Option<i64>,
    destination_id: Option<i64>,
    args: Map<String, Value>,
}

impl Call {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            started: Instant::now(),
            quiet: false,
            item_id: None,
            destination_id: None,
            args: Map::new(),
        }
    }

    /// Read-only and frequent: `debug` when it succeeds.
    fn quiet(name: &'static str) -> Self {
        Self {
            quiet: true,
            ..Self::new(name)
        }
    }

    fn item(mut self, id: i64) -> Self {
        self.item_id = Some(id);
        self
    }

    fn destination(mut self, id: i64) -> Self {
        self.destination_id = Some(id);
        self
    }

    fn arg(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.args.insert(key.to_owned(), value.into());
        self
    }

    fn log(self, error: Option<&CommandError>) {
        let dur_ms = logging::ms_since(self.started);
        let args = Value::Object(self.args).to_string();
        let (name, item_id, destination_id) = (self.name, self.item_id, self.destination_id);
        match error {
            None if self.quiet => tracing::debug!(
                event = "command",
                command = name,
                ok = true,
                dur_ms,
                item_id,
                destination_id,
                json = args.as_str(),
                "command {name} ok"
            ),
            None => tracing::info!(
                event = "command",
                command = name,
                ok = true,
                dur_ms,
                item_id,
                destination_id,
                json = args.as_str(),
                "command {name} ok"
            ),
            Some(error) => tracing::warn!(
                event = "command",
                command = name,
                ok = false,
                dur_ms,
                item_id,
                destination_id,
                error_code = error.code,
                json = args.as_str(),
                "command {name} failed"
            ),
        }
    }

    fn finish<T>(self, result: CommandResult<T>) -> CommandResult<T> {
        self.log(result.as_ref().err());
        result
    }

    /// For a command that cannot fail.
    fn done<T>(self, value: T) -> T {
        self.log(None);
        value
    }
}

/// Emits queue events to the webview.
pub struct TauriEvents(pub AppHandle);

impl TauriEvents {
    fn emit<T: Serialize + Clone>(&self, name: &str, payload: &T) {
        if let Err(error) = self.0.emit(name, payload) {
            tracing::warn!(event = "emit_failed", name, error = %error, "could not emit an event to the window");
        }
    }
}

impl EventSink for TauriEvents {
    fn queue_changed(&self, event: &QueueChanged) {
        self.emit(QUEUE_CHANGED, event);
    }

    fn queue_progress(&self, event: &QueueProgress) {
        self.emit(QUEUE_PROGRESS, event);
    }

    fn sidecar_progress(&self, event: &SidecarProgress) {
        self.emit(SIDECARS_PROGRESS, event);
    }

    fn queue_alert(&self, event: &QueueAlert) {
        self.emit(QUEUE_ALERT, event);
    }

    fn queue_status(&self, event: &QueueStatus) {
        self.emit(QUEUE_STATUS, event);
    }
}

/// Emits `auth://state`.
pub fn emit_auth_state(app: &AppHandle, payload: &AuthPayload) {
    if let Err(error) = app.emit(AUTH_STATE, payload) {
        tracing::warn!(event = "emit_failed", name = AUTH_STATE, error = %error, "could not emit an event to the window");
    }
}

fn source_file(path: &str) -> Result<(i64, i64), QueueError> {
    let unreadable = || QueueError::FileUnreadable(path.to_owned());
    let metadata = std::fs::metadata(Path::new(path)).map_err(|_| unreadable())?;
    if !metadata.is_file() {
        return Err(unreadable());
    }
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map(|age| age.as_secs() as i64)
        .unwrap_or(0);
    Ok((metadata.len() as i64, mtime))
}

// --- queue ---

/// Files and folders (walked recursively, non-media files skipped with a `FILES_SKIPPED`
/// alert per folder). All or nothing: an unreadable path refuses the whole call.
#[tauri::command]
pub fn queue_add(
    state: State<'_, AppState>,
    paths: Vec<String>,
    destination_id: i64,
    send_mode: Option<SendMode>,
    caption: Option<String>,
) -> CommandResult<Vec<i64>> {
    let call = Call::new("queue_add")
        .destination(destination_id)
        .arg("paths", paths.len())
        .arg("send_mode", send_mode.map(SendMode::as_str))
        .arg(
            "has_caption",
            caption.as_deref().is_some_and(|c| !c.is_empty()),
        );
    let result = (|| -> CommandResult<Vec<i64>> {
        let mode = match send_mode {
            Some(mode) => mode,
            None => {
                state
                    .queue
                    .with_store(|store| store.get_destination(destination_id))?
                    .default_mode
            }
        };
        let expanded = intake::expand(&paths)?;
        let mut items = Vec::with_capacity(expanded.files.len());
        for path in expanded.files {
            let (size, mtime) = source_file(&path)?;
            items.push(NewItem {
                source_path: path,
                source_size: size,
                source_mtime: mtime,
                destination_id,
                send_mode: mode,
                caption: caption.clone().unwrap_or_default(),
            });
        }
        let ids = state.queue.add(&items)?;
        for skipped in expanded.skipped {
            state.queue.alert(
                None,
                "FILES_SKIPPED",
                json!({ "path": skipped.folder, "count": skipped.count }),
            );
        }
        Ok(ids)
    })();
    call.finish(result)
}

#[tauri::command]
pub fn queue_list(state: State<'_, AppState>) -> CommandResult<Vec<Item>> {
    Call::quiet("queue_list").finish(state.queue.list().map_err(Into::into))
}

#[tauri::command]
pub fn queue_promote(state: State<'_, AppState>, item_ids: Vec<i64>) -> CommandResult<()> {
    Call::new("queue_promote")
        .arg("item_ids", item_ids.clone())
        .finish(state.queue.promote(&item_ids).map_err(Into::into))
}

#[tauri::command]
pub fn queue_start(state: State<'_, AppState>) {
    let call = Call::new("queue_start");
    state.queue.start();
    call.done(())
}

#[tauri::command]
pub fn queue_pause(state: State<'_, AppState>) {
    let call = Call::new("queue_pause");
    state.queue.pause();
    call.done(())
}

/// Running or paused; every change is also emitted on `queue://status`.
#[tauri::command]
pub fn queue_status(state: State<'_, AppState>) -> QueueStatus {
    Call::quiet("queue_status").done(state.queue.status())
}

#[tauri::command]
pub fn queue_cancel(state: State<'_, AppState>, item_id: i64) -> CommandResult<()> {
    Call::new("queue_cancel")
        .item(item_id)
        .finish(state.queue.cancel(item_id).map_err(Into::into))
}

#[tauri::command]
pub fn queue_reorder(state: State<'_, AppState>, item_ids: Vec<i64>) -> CommandResult<()> {
    Call::new("queue_reorder")
        .arg("count", item_ids.len())
        .finish(state.queue.reorder(&item_ids).map_err(Into::into))
}

/// User retry of a `failed` item: `encoded` if the encoded file is still there, else `ready`.
#[tauri::command]
pub fn queue_retry(state: State<'_, AppState>, item_id: i64) -> CommandResult<ItemState> {
    Call::new("queue_retry")
        .item(item_id)
        .finish(state.queue.retry_failed(item_id).map_err(Into::into))
}

/// Result of `queue_clear`.
#[derive(Debug, Serialize)]
pub struct QueueCleared {
    pub removed: usize,
    /// Items left in the queue because they are being worked on.
    pub running: usize,
}

/// Deletes the items that are not being worked on: `finished` (sent, failed, cancelled) or
/// `all` (also waiting, prepared and unconfirmed items; the front asks first). Their
/// temporary and encoded files go too, never the original.
#[tauri::command]
pub fn queue_clear(state: State<'_, AppState>, scope: ClearScope) -> CommandResult<QueueCleared> {
    let call = Call::new("queue_clear").arg("scope", scope.as_str());
    let result = state.queue.clear(scope).map(|outcome| QueueCleared {
        removed: outcome.removed.len(),
        running: outcome.running,
    });
    let call = match &result {
        Ok(cleared) => call
            .arg("removed", cleared.removed)
            .arg("running", cleared.running),
        Err(_) => call,
    };
    call.finish(result.map_err(Into::into))
}

#[tauri::command]
pub fn item_update(
    state: State<'_, AppState>,
    item_id: i64,
    patch: ItemPatch,
) -> CommandResult<()> {
    Call::new("item_update")
        .item(item_id)
        // Which fields changed, never the caption itself.
        .arg("caption", patch.caption.is_some())
        .arg("send_mode", patch.send_mode.map(SendMode::as_str))
        .finish(state.queue.update_item(item_id, &patch).map_err(Into::into))
}

/// Read only: messages of the destination that may be this `unknown` item.
#[tauri::command]
pub async fn item_reconcile(
    state: State<'_, AppState>,
    item_id: i64,
) -> CommandResult<Vec<SentCandidate>> {
    let call = Call::new("item_reconcile").item(item_id);
    let result = service::reconcile(state.transport.as_ref(), &state.queue, item_id).await;
    call.finish(result.map_err(Into::into))
}

/// `mark_sent` (with the message id of a candidate, or without) or `retry` of an `unknown` item.
#[tauri::command]
pub fn item_resolve_unknown(
    state: State<'_, AppState>,
    item_id: i64,
    action: ResolveAction,
    message_id: Option<i64>,
) -> CommandResult<()> {
    let call = Call::new("item_resolve_unknown")
        .item(item_id)
        .arg(
            "action",
            match action {
                ResolveAction::MarkSent => "mark_sent",
                ResolveAction::Retry => "retry",
            },
        )
        .arg("with_message_id", message_id.is_some());
    let result = state.queue.resolve_unknown(item_id, action, message_id);
    call.finish(result.map(|_| ()).map_err(Into::into))
}

// --- destinations ---

#[tauri::command]
pub async fn destinations_refresh(state: State<'_, AppState>) -> CommandResult<Vec<Destination>> {
    let call = Call::new("destinations_refresh");
    let result = service::refresh_destinations(state.transport.as_ref(), &state.queue).await;
    call.finish(result.map_err(Into::into))
}

/// Read only on Telegram: chats matching `query` (title or username), including chats in no
/// list; added to the cache. The journal gets the query's length, never its text.
#[tauri::command]
pub async fn destinations_search(
    state: State<'_, AppState>,
    query: String,
) -> CommandResult<Vec<Destination>> {
    let call = Call::new("destinations_search").arg("query_chars", query.trim().chars().count());
    let result = service::search_destinations(state.transport.as_ref(), &state.queue, &query).await;
    let call = match &result {
        Ok(found) => call.arg("count", found.len()),
        Err(_) => call,
    };
    call.finish(result.map_err(Into::into))
}

/// Topics of a forum (its `topic_id = 0` row), loaded when the user opens it.
#[tauri::command]
pub async fn destination_topics(
    state: State<'_, AppState>,
    destination_id: i64,
) -> CommandResult<Vec<Destination>> {
    let call = Call::new("destination_topics").destination(destination_id);
    let result = service::load_topics(state.transport.as_ref(), &state.queue, destination_id).await;
    let call = match &result {
        Ok(topics) => call.arg("count", topics.len()),
        Err(_) => call,
    };
    call.finish(result.map_err(Into::into))
}

#[tauri::command]
pub fn destinations_list(state: State<'_, AppState>) -> CommandResult<Vec<Destination>> {
    let result = state.queue.with_store(|store| store.list_destinations());
    Call::quiet("destinations_list").finish(result.map_err(Into::into))
}

/// Local settings: alias (`null` or blank removes it), favorite, minimum interval, mode.
#[tauri::command]
pub fn destination_update(
    state: State<'_, AppState>,
    destination_id: i64,
    patch: DestinationPatch,
) -> CommandResult<Destination> {
    let call = Call::new("destination_update")
        .destination(destination_id)
        // Which fields changed; the alias text is a personal name, never written.
        .arg("alias", patch.alias.is_some())
        .arg("favorite", patch.favorite)
        .arg("min_interval_s", patch.min_interval_s)
        .arg("default_mode", patch.default_mode.map(SendMode::as_str));
    let result = state.queue.with_store(|store| {
        store.update_destination(destination_id, &patch)?;
        store.get_destination(destination_id)
    });
    call.finish(result.map_err(Into::into))
}

/// Before encoding: rights for this mode, slow mode, closed topic, paid messages.
#[tauri::command]
pub async fn destination_check(
    state: State<'_, AppState>,
    destination_id: i64,
    send_mode: SendMode,
) -> CommandResult<DestinationCheck> {
    let call = Call::new("destination_check")
        .destination(destination_id)
        .arg("send_mode", send_mode.as_str());
    let result: CommandResult<DestinationCheck> = async {
        let destination = state
            .queue
            .with_store(|store| store.get_destination(destination_id))?;
        Ok(service::check_destination(state.transport.as_ref(), &destination, send_mode).await?)
    }
    .await;
    call.finish(result)
}

// --- Telegram session ---

/// Current `auth://state` payload, for a page that subscribes after the last change.
#[tauri::command]
pub fn auth_state(state: State<'_, AppState>) -> AuthPayload {
    Call::quiet("auth_state").done(service::auth_payload(
        &state.transport.auth_state().borrow(),
        state.transport_kind,
    ))
}

/// The input's kind is the only thing written to the journal.
#[tauri::command]
pub async fn auth_submit(state: State<'_, AppState>, input: AuthInput) -> CommandResult<()> {
    submit_auth(state.transport.as_ref(), input).await
}

/// Body of `auth_submit`, testable without a window.
pub(crate) async fn submit_auth(
    transport: &dyn TelegramTransport,
    input: AuthInput,
) -> CommandResult<()> {
    let call = Call::new("auth_submit").arg("kind", input.kind());
    let result = transport.auth_submit(input).await;
    call.finish(result.map_err(Into::into))
}

#[tauri::command]
pub async fn auth_logout(state: State<'_, AppState>) -> CommandResult<()> {
    let call = Call::new("auth_logout");
    let result = state.transport.log_out().await;
    call.finish(result.map_err(Into::into))
}

// --- settings, history, sidecars ---

#[tauri::command]
pub fn settings_get(state: State<'_, AppState>) -> Settings {
    Call::quiet("settings_get").done(state.settings.get())
}

/// The journal gets the changed keys (`send.pause_min_s`), never the values.
#[tauri::command]
pub fn settings_set(state: State<'_, AppState>, patch: Value) -> CommandResult<Settings> {
    let call = Call::new("settings_set").arg("keys", patch_keys(&patch));
    let result = state.settings.set(&patch).inspect(|settings| {
        state.queue.set_pauses(settings.pause_range());
        logging::apply_level(settings.logging.level.as_deref());
    });
    call.finish(result.map_err(Into::into))
}

#[tauri::command]
pub fn history_query(
    state: State<'_, AppState>,
    filters: Option<HistoryFilter>,
) -> CommandResult<Vec<HistoryEntry>> {
    let result = state.queue.history(&filters.unwrap_or_default());
    Call::quiet("history_query").finish(result.map_err(Into::into))
}

/// Deletes the whole history; returns the number of lines. A file already sent is then no
/// longer held as a duplicate. The queue and the destinations are not touched.
#[tauri::command]
pub fn history_clear(state: State<'_, AppState>) -> CommandResult<usize> {
    let call = Call::new("history_clear");
    let result = state.queue.clear_history();
    let call = match &result {
        Ok(removed) => call.arg("removed", *removed),
        Err(_) => call,
    };
    call.finish(result.map_err(Into::into))
}

#[tauri::command]
pub fn sidecars_status(state: State<'_, AppState>) -> SidecarStatus {
    Call::quiet("sidecars_status").done(state.sidecars.status())
}

/// Result of `encoder_status`.
#[derive(Debug, Serialize)]
pub struct EncoderStatus {
    /// A GPU encoder that stopped mid-encode in this session: until the app restarts, new
    /// encodes use the CPU (Settings shows a warning).
    pub gpu_failed: Option<GpuFailure>,
}

/// The encoder state of this session.
#[tauri::command]
pub fn encoder_status(state: State<'_, AppState>) -> EncoderStatus {
    Call::quiet("encoder_status").done(EncoderStatus {
        gpu_failed: state.media.gpu_failure(),
    })
}

/// The upload limit that applies now (`{ limit_bytes, source,
/// premium }`, source `custom` | `forced` | `account` | `default`). Settings shows it.
#[tauri::command]
pub fn limit_status(state: State<'_, AppState>) -> EffectiveLimit {
    Call::quiet("limit_status").done(state.limits.current())
}

/// Downloads and installs the pinned ffmpeg build; progress goes out on `sidecars://progress`.
#[tauri::command]
pub async fn sidecars_install(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<SidecarStatus> {
    let call = Call::new("sidecars_install");
    let service = state.sidecars.clone();
    let sink = TauriEvents(app);
    let result = match tauri::async_runtime::spawn_blocking(move || service.install_blocking(&sink))
        .await
    {
        Ok(status) => status.map_err(Into::into),
        Err(error) => {
            tracing::error!(event = "command_error_detail", error = %error, "sidecar install task failed");
            Err(CommandError::io())
        }
    };
    call.finish(result)
}

/// Looks for ffmpeg/ffprobe pairs already installed (known places and `PATH` folders) and
/// returns the working ones. Only proposes: nothing is recorded until `sidecars_use`.
#[tauri::command]
pub async fn sidecars_discover(state: State<'_, AppState>) -> CommandResult<Discovery> {
    let call = Call::new("sidecars_discover");
    let discovery = discover::discover(state.sidecars.installed_pair()).await;
    call.arg("found", discovery.candidates.len())
        .finish(Ok(discovery))
}

/// Records the folder of a detected candidate as `ffmpeg.custom_dir` (after re-validating it)
/// and emits `sidecars://status`.
#[tauri::command]
pub async fn sidecars_use(
    app: AppHandle,
    state: State<'_, AppState>,
    dir: String,
) -> CommandResult<SidecarStatus> {
    let call = Call::new("sidecars_use").arg("dir", dir.as_str());
    let result = state.sidecars.use_dir(&dir).await;
    if let Ok(status) = &result
        && let Err(error) = app.emit(SIDECARS_STATUS, status)
    {
        tracing::warn!(event = "emit_failed", name = SIDECARS_STATUS, error = %error, "could not emit an event to the window");
    }
    call.finish(result.map_err(Into::into))
}

// --- journal ---

/// A line from the UI (errors, console, screen changes). Validated, bounded, scrubbed and
/// rate-limited in `logging::frontend`; not itself logged as a `command`.
#[tauri::command]
pub fn log_frontend(level: String, event: String, msg: String, fields: Option<Value>) {
    frontend::record(&frontend::FrontendEntry {
        level,
        event,
        msg,
        fields,
    });
}

/// Folder of the journal files.
#[tauri::command]
pub fn log_path() -> CommandResult<String> {
    let result = logging::dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .ok_or_else(CommandError::io);
    Call::quiet("log_path").finish(result)
}

/// Opens the journal folder in the file explorer.
#[tauri::command]
pub fn log_open_dir() -> CommandResult<()> {
    let call = Call::new("log_open_dir");
    let result = match logging::dir() {
        Some(dir) => std::process::Command::new("explorer.exe")
            .arg(dir)
            .spawn()
            .map(|_| ())
            .map_err(|error| {
                tracing::warn!(event = "command_error_detail", error = %error, "explorer did not start");
                CommandError::io()
            }),
        None => Err(CommandError::io()),
    };
    call.finish(result)
}

/// Settings for a report: no identity (`api_override`, device name), paths masked later.
fn settings_for_report(settings: &Settings) -> Value {
    let mut value = serde_json::to_value(settings).unwrap_or_default();
    for key in ["api_override", "device_name"] {
        if let Some(field) = value.pointer_mut(&format!("/telegram/{key}"))
            && !field.is_null()
        {
            *field = json!("<set>");
        }
    }
    value
}

/// Debug export: asks where to save, writes a zip (masked journal + report).
/// `null` when the user closes the dialog.
#[tauri::command]
pub async fn debug_export(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<Option<String>> {
    let call = Call::new("debug_export");
    let result: CommandResult<Option<String>> = async {
        let dir = logging::dir().ok_or_else(CommandError::io)?.to_path_buf();
        let stamp = crate::queue::now_iso();
        let name = format!(
            "squeezegram-debug-{}.zip",
            stamp.get(..19).unwrap_or("now").replace(['-', ':'], "").replace('T', "-")
        );
        let (sender, receiver) = tokio::sync::oneshot::channel();
        app.dialog()
            .file()
            .set_file_name(name)
            .add_filter("ZIP", &["zip"])
            .save_file(move |path| {
                let _ = sender.send(path);
            });
        let Some(chosen) = receiver.await.ok().flatten() else {
            return Ok(None);
        };
        let out = chosen.into_path().map_err(|_| CommandError::io())?;

        let sidecars = state.sidecars.clone();
        let versions = tauri::async_runtime::spawn_blocking(move || {
            logging::system::versions(Some(&sidecars))
        })
        .await
        .map_err(|_| CommandError::io())?;
        let report = json!({
            "generated_at": crate::queue::now_iso(),
            "session_id": logging::session_id(),
            "log_level": logging::current_level(),
            "versions": versions,
            "encoder_detection": state.media.detection_report().await,
            "queue_errors": state.queue.with_store(|store| store.recent_errors(EXPORT_ERRORS))?,
            "settings": settings_for_report(&state.settings.get()),
            "notes": "Paths are masked as <path>/name; chats are hashed (c:xxxxxx). No TDLib log, no database.",
        });
        let target = out.clone();
        let summary = tauri::async_runtime::spawn_blocking(move || {
            export::write_zip(&target, &report, &dir)
        })
        .await
        .map_err(|_| CommandError::io())?
        .map_err(|error| {
            tracing::warn!(event = "command_error_detail", error = %error, "debug export not written");
            CommandError::io()
        })?;
        tracing::info!(
            event = "debug_export",
            files = summary.files,
            lines = summary.lines,
            skipped_lines = summary.skipped_lines,
            path = %out.display(),
            "debug export written"
        );
        Ok(Some(out.to_string_lossy().into_owned()))
    }
    .await;
    call.finish(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{ApiOverride, Settings};

    #[test]
    fn report_settings_hide_identity() {
        let mut settings = Settings::default();
        settings.telegram.api_override = Some(ApiOverride { api_id: 4242 });
        settings.telegram.device_name = Some("DESKTOP-SECRET".into());
        let value = settings_for_report(&settings);
        assert_eq!(value["telegram"]["api_override"], "<set>");
        assert_eq!(value["telegram"]["device_name"], "<set>");
        assert!(!value.to_string().contains("4242") && !value.to_string().contains("SECRET"));
        let plain = settings_for_report(&Settings::default());
        assert!(plain["telegram"]["api_override"].is_null());
    }
}
