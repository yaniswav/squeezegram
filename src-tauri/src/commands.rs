//! Tauri commands. The Telegram ones delegate to `transport::service`.
//! Errors reach the front as `{ code, params }`; the texts live in the front's i18n files.
//!
//! Every command writes one `command` line to the journal (name, duration, result or
//! `error_code`, safe arguments only: ids, counts, kinds, setting keys; never a caption, a
//! value typed at sign-in or a setting value). Read-only commands log at `debug`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use serde_json::{Map, Value, json};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::background::Background;
use crate::background::autostart::AutostartError;
use crate::entitlements::store::StoreError;
use crate::entitlements::{self, EntitlementStatus, Entitlements, Feature, LicenseEntitlements};
use crate::limit::{EffectiveLimit, LimitResolver};
use crate::logging::{self, export, frontend};
use crate::media::plan::EncodingOverride;
use crate::media::stage::{CodecSupport, EncodeEstimate, FfmpegStage, GpuFailure};
use crate::queue::events::{
    EventSink, QUEUE_ALERT, QUEUE_CHANGED, QUEUE_PROGRESS, QUEUE_STATUS, QueueAlert, QueueChanged,
    QueueProgress, QueueStatus, SIDECARS_PROGRESS, SIDECARS_STATUS, SidecarProgress,
};
use crate::queue::intake;
use crate::queue::orchestrator::{Orchestrator, ResolveAction};
use crate::queue::store::{
    ClearScope, Destination, DestinationPatch, HistoryEntry, HistoryFilter, Item, ItemPatch,
    NewItem, SendMode, WatchFolder, WatchFolderPatch,
};
use crate::queue::{QueueError, State as ItemState};
use crate::settings::{Settings, SettingsError, SettingsStore, patch_keys};
use crate::sidecars::discover::{self, Discovery};
use crate::sidecars::{SidecarError, SidecarService, SidecarStatus};
use crate::transport::identity_settings::{
    self, IdentityChangeError, IdentityRuntime, TelegramIdentity,
};
use crate::transport::select::TransportKind;
use crate::transport::service::{self, AuthPayload, DestinationCheck, ServiceError};
use crate::transport::{AuthInput, SentCandidate, TelegramTransport, TransportError};
use crate::update::{self, Trigger, UpdateError, UpdateInfo, UpdateNotice, UpdateService};
use crate::watch::WatchService;

/// Event of the login state machine.
pub const AUTH_STATE: &str = "auth://state";
/// Event of a new `EntitlementStatus` (license set or removed, sign-in, sign-out).
pub const ENTITLEMENTS_STATUS: &str = "entitlements://status";
/// Queue items whose error codes go into a debug export.
const EXPORT_ERRORS: i64 = 50;

pub struct AppState {
    pub queue: Orchestrator,
    pub settings: Arc<SettingsStore>,
    pub sidecars: Arc<SidecarService>,
    pub transport: Arc<dyn TelegramTransport>,
    /// Which transport runs (`auth_state`/`auth://state` carry it: the UI flags the mock).
    pub transport_kind: TransportKind,
    /// Startup facts of the Telegram identity (`telegram_identity`).
    pub identity: IdentityRuntime,
    /// Kept for the encoder detection of `debug_export`.
    pub media: Arc<FfmpegStage>,
    /// Effective upload limit (`limit_status`); the media stage holds the same resolver.
    pub limits: Arc<LimitResolver>,
    /// Paid features and the stored license (`entitlements_status`, `license_*`); the media
    /// stage holds the same instance.
    pub entitlements: Arc<LicenseEntitlements>,
    /// Last status sent on `entitlements://status`.
    pub entitlement_feed: Arc<StatusFeed>,
    /// Watched folders (`watch_*`); woken after a change of the folders or of the license.
    pub watcher: Arc<WatchService>,
    /// Tray icon and window visibility (`app_quit`).
    pub background: Arc<Background>,
    /// Update checks and installs (`update_*`).
    pub updates: Arc<UpdateService>,
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

    /// A paid feature the license in effect does not grant (`{ feature }`, its license name).
    fn not_licensed(feature: Feature) -> Self {
        Self {
            code: "FEATURE_NOT_LICENSED",
            params: json!({ "feature": feature.as_str() }),
        }
    }
}

impl From<QueueError> for CommandError {
    fn from(error: QueueError) -> Self {
        let params = match &error {
            QueueError::ItemNotFound(id) | QueueError::ItemStarted(id) => json!({ "itemId": id }),
            QueueError::DestinationNotFound(id) => json!({ "destinationId": id }),
            QueueError::WatchFolderNotFound(id) => json!({ "folderId": id }),
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

impl From<IdentityChangeError> for CommandError {
    fn from(error: IdentityChangeError) -> Self {
        match error {
            IdentityChangeError::Busy => Self {
                code: "TG_TRANSPORT_BUSY",
                params: json!({}),
            },
            // The field only: what was typed never leaves the core.
            IdentityChangeError::Invalid(field) => Self {
                code: "SETTINGS_INVALID",
                params: json!({ "field": field }),
            },
            IdentityChangeError::Storage(error) => {
                tracing::warn!(event = "command_error_detail", error_code = "IO_ERROR", error = %error, "api_hash not stored");
                CommandError::io()
            }
            IdentityChangeError::Settings(error) => error.into(),
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

impl From<StoreError> for CommandError {
    /// `LIC_*` or `IO_ERROR`; the store has already journaled it (`license_set`, `license_cleared`).
    fn from(error: StoreError) -> Self {
        Self {
            code: error.code(),
            params: json!({}),
        }
    }
}

impl From<UpdateError> for CommandError {
    /// The service has already journaled the failure (`update_failed`).
    fn from(error: UpdateError) -> Self {
        Self {
            code: error.code,
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

    /// The tray shows what the queue is doing; it updates after a pause, never inline.
    fn refresh_tray(&self) {
        if let Some(state) = self.0.try_state::<AppState>() {
            state.background.refresh();
        }
    }
}

impl EventSink for TauriEvents {
    fn queue_changed(&self, event: &QueueChanged) {
        self.emit(QUEUE_CHANGED, event);
        self.refresh_tray();
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
        self.refresh_tray();
    }
}

/// Emits `auth://state`.
pub fn emit_auth_state(app: &AppHandle, payload: &AuthPayload) {
    if let Err(error) = app.emit(AUTH_STATE, payload) {
        tracing::warn!(event = "emit_failed", name = AUTH_STATE, error = %error, "could not emit an event to the window");
    }
}

/// Last status sent on `entitlements://status`, shared by the license commands and the login
/// watcher so that each change goes out once.
#[derive(Default)]
pub struct StatusFeed(Mutex<Option<EntitlementStatus>>);

impl StatusFeed {
    /// Emits `status` unless it is the one sent last.
    pub fn publish(&self, app: &AppHandle, status: EntitlementStatus) {
        let mut last = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.as_ref() == Some(&status) {
            return;
        }
        if let Err(error) = app.emit(ENTITLEMENTS_STATUS, &status) {
            tracing::warn!(event = "emit_failed", name = ENTITLEMENTS_STATUS, error = %error, "could not emit an event to the window");
        }
        *last = Some(status);
    }
}

fn source_file(path: &str) -> Result<(i64, i64), QueueError> {
    intake::file_stamp(Path::new(path)).ok_or_else(|| QueueError::FileUnreadable(path.to_owned()))
}

// --- queue ---

/// Files and folders (walked recursively, non-media files skipped with a `FILES_SKIPPED`
/// alert per folder), for each of `destination_ids`. All or nothing: an unreadable path or an
/// unknown destination refuses the whole call. More than one destination is a paid feature.
#[tauri::command]
pub fn queue_add(
    state: State<'_, AppState>,
    paths: Vec<String>,
    destination_ids: Vec<i64>,
    send_mode: Option<SendMode>,
    caption: Option<String>,
) -> CommandResult<Vec<i64>> {
    add_files(
        &state.queue,
        state.entitlements.as_ref(),
        &paths,
        &destination_ids,
        send_mode,
        caption.as_deref(),
    )
}

/// Body of `queue_add`, testable without a window. One item per file and destination, file
/// by file: the items of one file follow each other, so its encoded file serves every
/// destination in turn (`Store::reusable_output`) and is deleted early. Without a mode, each
/// destination uses its own default.
///
/// The license is asked here, in the core: a page that offers more than one destination
/// without `Feature::MultiDestination` gets `FEATURE_NOT_LICENSED` and nothing is queued.
pub(crate) fn add_files(
    queue: &Orchestrator,
    entitlements: &dyn Entitlements,
    paths: &[String],
    destination_ids: &[i64],
    send_mode: Option<SendMode>,
    caption: Option<&str>,
) -> CommandResult<Vec<i64>> {
    let mut destinations: Vec<i64> = Vec::with_capacity(destination_ids.len());
    for &id in destination_ids {
        if !destinations.contains(&id) {
            destinations.push(id);
        }
    }
    let mut call = Call::new("queue_add")
        .arg("paths", paths.len())
        .arg("destinations", destinations.len())
        .arg("send_mode", send_mode.map(SendMode::as_str))
        .arg("has_caption", caption.is_some_and(|c| !c.is_empty()));
    match destinations[..] {
        [id] => call = call.destination(id),
        _ => call = call.arg("destination_ids", destinations.clone()),
    }
    let result = (|| -> CommandResult<Vec<i64>> {
        if destinations.len() > 1 && !entitlements.allows(Feature::MultiDestination) {
            return Err(CommandError::not_licensed(Feature::MultiDestination));
        }
        if destinations.is_empty() {
            // Nothing to queue the files for: the page always sends one.
            return Err(QueueError::DestinationNotFound(0).into());
        }
        let mut targets = Vec::with_capacity(destinations.len());
        for &destination_id in &destinations {
            let mode = match send_mode {
                Some(mode) => mode,
                None => {
                    queue
                        .with_store(|store| store.get_destination(destination_id))?
                        .default_mode
                }
            };
            targets.push((destination_id, mode));
        }
        let expanded = intake::expand(paths)?;
        let mut items = Vec::with_capacity(expanded.files.len() * targets.len());
        for path in expanded.files {
            let (size, mtime) = source_file(&path)?;
            for &(destination_id, mode) in &targets {
                items.push(NewItem {
                    source_path: path.clone(),
                    source_size: size,
                    source_mtime: mtime,
                    destination_id,
                    send_mode: mode,
                    caption: caption.unwrap_or_default().to_owned(),
                });
            }
        }
        let ids = queue.add(&items)?;
        for skipped in expanded.skipped {
            queue.alert(
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

/// `item_encoding_set` without the Tauri state: a choice needs the license and a target in
/// range; going back to the settings (`None`) needs neither.
fn set_encoding(
    entitlements: &dyn Entitlements,
    queue: &Orchestrator,
    item_id: i64,
    options: Option<&EncodingOverride>,
) -> CommandResult<()> {
    if let Some(file) = options {
        if !entitlements.allows(Feature::AdvancedEncoding) {
            return Err(CommandError::not_licensed(Feature::AdvancedEncoding));
        }
        if !file.is_valid() {
            return Err(CommandError {
                code: "SETTINGS_INVALID",
                params: json!({}),
            });
        }
    }
    queue
        .set_encoding_override(item_id, options)
        .map_err(CommandError::from)
}

/// Per-file encoding choices (F14): codec, maximum resolution and target size of one item,
/// `null` to go back to the global settings. Premium (`advanced_encoding`), checked here and
/// again when the plan is built; going back to the settings never needs the license. Only while
/// the item has not started (`ITEM_STARTED` otherwise): an encoded file is never silently
/// replaced, to encode it differently cancel the item and queue the file again.
#[tauri::command]
pub fn item_encoding_set(
    state: State<'_, AppState>,
    item_id: i64,
    options: Option<EncodingOverride>,
) -> CommandResult<()> {
    let call = Call::new("item_encoding_set")
        .item(item_id)
        .arg("reset", options.is_none());
    let call = match &options {
        Some(file) => call
            .arg("codec", file.video_codec.as_str())
            .arg("target_mb", file.target_mb),
        None => call,
    };
    let result = set_encoding(
        state.entitlements.as_ref(),
        &state.queue,
        item_id,
        options.as_ref(),
    );
    call.finish(result)
}

/// Read only: the output size of a queued item under `options` (or the global settings when
/// `null`), for the options panel. Runs ffprobe on the source, never an encode.
#[tauri::command]
pub async fn item_encoding_estimate(
    state: State<'_, AppState>,
    item_id: i64,
    options: Option<EncodingOverride>,
) -> CommandResult<EncodeEstimate> {
    let call = Call::quiet("item_encoding_estimate").item(item_id);
    let result: CommandResult<EncodeEstimate> = async {
        let item = state.queue.with_store(|store| store.get_item(item_id))?;
        state
            .media
            .estimate(&item, options.as_ref())
            .await
            .map_err(|error| {
                tracing::warn!(
                    event = "command_error_detail",
                    error_code = error.code(),
                    error = %error,
                    "estimate failed"
                );
                CommandError {
                    code: error.code().unwrap_or("FFMPEG_FAILED"),
                    params: json!({}),
                }
            })
    }
    .await;
    call.finish(result)
}

/// Whether HEVC can be encoded on this computer (a GPU encoder passed its test encode, once per
/// ffmpeg). Settings and the item options disable the HEVC choice with the reason when not.
#[tauri::command]
pub async fn codec_support(state: State<'_, AppState>) -> CommandResult<CodecSupport> {
    let support = state.media.codec_support().await;
    Call::quiet("codec_support").finish(Ok(support))
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

// --- watched folders ---

/// A watched folder as Settings shows it.
#[derive(Debug, Serialize)]
pub struct WatchedFolder {
    #[serde(flatten)]
    pub folder: WatchFolder,
    /// The watcher's last listing of it failed (missing, renamed, offline share).
    pub unreadable: bool,
}

fn watched(state: &AppState, folder: WatchFolder) -> WatchedFolder {
    WatchedFolder {
        unreadable: state.watcher.is_unreadable(folder.id),
        folder,
    }
}

/// Not in SPEC section 4 (which has `watch_set`): one command per change, as for destinations.
/// The folders are kept whatever the plan; the watcher lists them only while the license
/// grants `watched_folder`.
#[tauri::command]
pub fn watch_list(state: State<'_, AppState>) -> CommandResult<Vec<WatchedFolder>> {
    let result = state.queue.with_store(|store| store.list_watch_folders());
    let result = result.map(|folders| {
        folders
            .into_iter()
            .map(|folder| watched(&state, folder))
            .collect()
    });
    Call::quiet("watch_list").finish(result.map_err(Into::into))
}

/// Watches an existing folder given by its full path (`FILE_UNREADABLE` otherwise: a relative
/// path would depend on the app's working directory). The files already in it are left alone;
/// the next ones are queued for approval. `send_mode` absent: the destination's default.
#[tauri::command]
pub fn watch_add(
    state: State<'_, AppState>,
    path: String,
    destination_id: i64,
    send_mode: Option<SendMode>,
) -> CommandResult<WatchedFolder> {
    let call = Call::new("watch_add")
        .destination(destination_id)
        .arg("dir", path.as_str())
        .arg("send_mode", send_mode.map(SendMode::as_str));
    let folder = Path::new(&path);
    let result = if folder.is_absolute() && folder.is_dir() {
        state
            .queue
            .with_store(|store| store.add_watch_folder(&path, destination_id, send_mode))
    } else {
        Err(QueueError::FileUnreadable(path.clone()))
    };
    if result.is_ok() {
        state.watcher.wake();
    }
    let result = result.map(|folder| watched(&state, folder));
    call.finish(result.map_err(Into::into))
}

/// Enables or disables a folder, or changes its destination or mode (`send_mode: null`: the
/// destination's default). Enabled again, it leaves alone the files that came meanwhile.
#[tauri::command]
pub fn watch_update(
    state: State<'_, AppState>,
    folder_id: i64,
    patch: WatchFolderPatch,
) -> CommandResult<WatchedFolder> {
    let call = Call::new("watch_update")
        .arg("folder_id", folder_id)
        .arg("enabled", patch.enabled)
        .arg("destination_id", patch.destination_id)
        .arg("send_mode", patch.send_mode.is_some());
    let result = state
        .queue
        .with_store(|store| store.update_watch_folder(folder_id, &patch));
    if result.is_ok() {
        state.watcher.wake();
    }
    let result = result.map(|folder| watched(&state, folder));
    call.finish(result.map_err(Into::into))
}

/// Stops watching a folder; the items it brought stay in the queue.
#[tauri::command]
pub fn watch_remove(state: State<'_, AppState>, folder_id: i64) -> CommandResult<()> {
    let call = Call::new("watch_remove").arg("folder_id", folder_id);
    let result = state
        .queue
        .with_store(|store| store.remove_watch_folder(folder_id));
    if result.is_ok() {
        state.watcher.wake();
    }
    call.finish(result.map_err(Into::into))
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

/// The default device name and the personal API identity as Settings shows them: never the
/// `api_hash`, only whether one is stored.
#[tauri::command]
pub fn telegram_identity(state: State<'_, AppState>) -> TelegramIdentity {
    Call::quiet("telegram_identity").done(state.identity.status(&state.settings.get()))
}

/// Personal API identity (F32), used from the next start with its own TDLib session. Refused
/// with `TG_TRANSPORT_BUSY` while the transport starts or signs out, `SETTINGS_INVALID
/// { field }` for a malformed value. The
/// result says only whether an `api_hash` is stored; neither value reaches the journal.
#[tauri::command]
pub fn api_identity_set(
    state: State<'_, AppState>,
    api_id: String,
    api_hash: String,
) -> CommandResult<TelegramIdentity> {
    let call = Call::new("api_identity_set");
    let auth = state.transport.auth_state().borrow().clone();
    let result = state
        .identity
        .set_override(&state.settings, &auth, &api_id, &api_hash);
    call.finish(result.map_err(Into::into))
}

/// Back to the product's identity (and its session), from the next start; same rule.
#[tauri::command]
pub fn api_identity_clear(state: State<'_, AppState>) -> CommandResult<TelegramIdentity> {
    let call = Call::new("api_identity_clear");
    let auth = state.transport.auth_state().borrow().clone();
    let result = state.identity.clear_override(&state.settings, &auth);
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
    if identity_settings::touches_api_override(&patch) {
        // `api_identity_set` / `_clear` own it: they store the hash and check the session.
        return call.finish(Err(CommandError {
            code: "SETTINGS_INVALID",
            params: json!({ "field": "telegram.api_override" }),
        }));
    }
    let result = state.settings.set(&patch).inspect(|settings| {
        state.queue.set_pauses(settings.pause_range());
        // The badges of the files still waiting were worked out under the old limit and
        // encoding choices.
        if (patch.get("limit").is_some() || patch.get("advanced_encoding").is_some())
            && let Err(error) = state.queue.reanalyze_waiting()
        {
            tracing::warn!(event = "queue_error", error_code = error.code(), error = %error, "could not refresh the plans");
        }
        logging::apply_level(settings.logging.level.as_deref());
        // The tray follows the close choice and the language.
        state.background.refresh();
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

/// The caption template rendered for the first file of the add panel (`path`, if any) in a
/// batch of `count` files. Rendering stays in Rust so the preview and the send share one
/// implementation; only the values known before encoding are real (`caption::preview`).
/// The template is never logged.
#[tauri::command]
pub fn caption_preview(template: String, path: Option<String>, count: u32) -> String {
    let file = path.as_deref().and_then(|path| {
        let path = Path::new(path);
        let metadata = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
        Some((
            path.file_name()?.to_string_lossy().into_owned(),
            metadata.len(),
        ))
    });
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    Call::quiet("caption_preview").done(crate::caption::preview(
        &template,
        file.as_ref().map(|(name, size)| (name.as_str(), *size)),
        count,
        &today,
    ))
}

/// The upload limit that applies now (`{ limit_bytes, source,
/// premium }`, source `custom` | `forced` | `account` | `default`). Settings shows it.
#[tauri::command]
pub fn limit_status(state: State<'_, AppState>) -> EffectiveLimit {
    Call::quiet("limit_status").done(state.limits.current())
}

/// Not in SPEC section 4: the plan in effect, the features granted, whether the stored license
/// belongs to the signed-in account and the `LIC_*` code that keeps it from applying.
#[tauri::command]
pub fn entitlements_status(state: State<'_, AppState>) -> EntitlementStatus {
    Call::quiet("entitlements_status").done(state.entitlements.status())
}

/// Verifies `token` offline, then stores it. Refused with its `LIC_*` code when it does not
/// verify or belongs to another signed-in account; kept (`pending`) when no account is signed
/// in. The token is never written to the journal.
#[tauri::command]
pub fn license_set(
    app: AppHandle,
    state: State<'_, AppState>,
    token: String,
) -> CommandResult<EntitlementStatus> {
    let call = Call::new("license_set");
    let result = state.entitlements.save(&token);
    if let Ok(status) = &result {
        state.entitlement_feed.publish(&app, status.clone());
        state.watcher.wake();
        state.background.refresh();
    }
    call.finish(result.map_err(Into::into))
}

/// Removes the stored license: back to the free plan.
#[tauri::command]
pub fn license_clear(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<EntitlementStatus> {
    let call = Call::new("license_clear");
    let result = state.entitlements.clear().map(|()| {
        let status = state.entitlements.status();
        state.entitlement_feed.publish(&app, status.clone());
        state.watcher.wake();
        state.background.refresh();
        status
    });
    call.finish(result.map_err(Into::into))
}

/// Quits the app after the user confirmed it in the window (`app://quit-requested`): the
/// normal exit, with the clean shutdown. Needs no license: it is any exit.
#[tauri::command]
pub fn app_quit(state: State<'_, AppState>) {
    let call = Call::new("app_quit");
    state.background.quit("confirmation");
    call.done(())
}

/// Whether the app starts with Windows (the entry exists). Stays true after the license is
/// lost: the entry is left as it is, but it starts the app with its window (see
/// `background::autostart`).
#[tauri::command]
pub fn autostart_status(state: State<'_, AppState>) -> bool {
    Call::quiet("autostart_status").done(state.background.autostart_enabled())
}

/// Turns "start with Windows" on, or off. On needs the license (`FEATURE_NOT_LICENSED`);
/// off is always allowed. Returns whether it is on.
#[tauri::command]
pub fn autostart_set(state: State<'_, AppState>, enabled: bool) -> CommandResult<bool> {
    let call = Call::new("autostart_set").arg("enabled", enabled);
    let result = state
        .background
        .set_autostart(enabled)
        .map_err(|error| match error {
            AutostartError::NotLicensed => CommandError::not_licensed(Feature::Background),
            AutostartError::Io(_) => CommandError::io(),
        });
    call.finish(result)
}

/// `explorer.exe` from the Windows folder. A bare name would be looked up in the working
/// directory and the PATH first, where a planted `explorer.exe` would run instead.
fn explorer_exe() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    std::path::PathBuf::from(root).join("explorer.exe")
}

/// Opens the purchase bot in the browser. The address is fixed in the core.
#[tauri::command]
pub fn license_open_purchase() -> CommandResult<()> {
    let call = Call::new("license_open_purchase");
    let result = std::process::Command::new(explorer_exe())
        .arg(entitlements::PURCHASE_URL)
        .spawn()
        .map(|_| ())
        .map_err(|error| {
            tracing::warn!(event = "command_error_detail", error = %error, "explorer did not start");
            CommandError::io()
        });
    call.finish(result)
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

// --- updates ---

/// How this copy was installed (`installer` | `portable`), its version, and the update found
/// earlier in this session (`notice`, `null` if none). No network.
#[tauri::command]
pub fn update_info(state: State<'_, AppState>) -> UpdateInfo {
    Call::quiet("update_info").done(state.updates.info())
}

/// Asks GitHub for the latest release, whatever the automatic-check setting says. The result
/// is also emitted on `app://update`.
#[tauri::command]
pub async fn update_check(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<UpdateNotice> {
    let call = Call::new("update_check");
    let result = state.updates.check(&app, Trigger::Manual).await;
    call.finish(result.map_err(Into::into))
}

/// Installer build only: downloads the update (progress on `app://update-progress`), checks
/// its signature, closes the app and runs the installer, which restarts it. Returns only on
/// failure (or when nothing newer exists any more).
#[tauri::command]
pub async fn update_install(app: AppHandle, state: State<'_, AppState>) -> CommandResult<()> {
    let call = Call::new("update_install");
    let result = state.updates.install(&app).await;
    call.finish(result.map_err(Into::into))
}

/// Opens the page of the latest release in the browser. The address is fixed in the core: the
/// page cannot choose what gets opened.
#[tauri::command]
pub fn update_open_page() -> CommandResult<()> {
    let call = Call::new("update_open_page");
    let result = std::process::Command::new(explorer_exe())
        .arg(update::RELEASES_PAGE)
        .spawn()
        .map(|_| ())
        .map_err(|error| {
            tracing::warn!(event = "command_error_detail", error = %error, "explorer did not start");
            CommandError::io()
        });
    call.finish(result)
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
        Some(dir) => std::process::Command::new(explorer_exe())
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
    #[test]
    fn explorer_is_taken_from_the_windows_folder() {
        let exe = super::explorer_exe();
        assert!(exe.is_absolute());
        assert!(exe.ends_with("explorer.exe"));
    }

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

    #[test]
    fn identity_refusals_carry_a_code_and_no_value() {
        let busy: CommandError = IdentityChangeError::Busy.into();
        assert_eq!(busy.code, "TG_TRANSPORT_BUSY");
        let invalid: CommandError = IdentityChangeError::Invalid("api_hash").into();
        assert_eq!(
            (invalid.code, invalid.params),
            ("SETTINGS_INVALID", json!({ "field": "api_hash" }))
        );
    }

    use crate::entitlements::{EntitlementSource, FreeOnly, Plan};
    use crate::queue::events::recorder::Recorder;
    use crate::queue::orchestrator::{PauseRange, Unavailable};
    use crate::queue::test_util::{TestDir, destination, first_destination, memory_store};
    use std::time::Duration;

    /// A license that grants every feature.
    struct Licensed;

    impl Entitlements for Licensed {
        fn allows(&self, _feature: Feature) -> bool {
            true
        }

        fn status(&self) -> EntitlementStatus {
            EntitlementStatus {
                plan: Plan::Lifetime,
                source: EntitlementSource::License,
                features: Feature::ALL.to_vec(),
                account_matches: Some(true),
                reason: None,
            }
        }
    }

    /// A queue whose loop never runs: added items stay `ready`.
    fn idle_queue() -> Orchestrator {
        Orchestrator::new(
            memory_store(),
            Arc::new(Unavailable),
            Arc::new(Unavailable),
            Arc::new(Recorder::default()),
            PauseRange {
                min: Duration::ZERO,
                max: Duration::ZERO,
            },
        )
    }

    #[test]
    fn several_destinations_need_the_license_and_are_queued_file_by_file() {
        let dir = TestDir::new("commands-multi");
        let paths = [dir.file("a.mp4", b"a"), dir.file("b.mp4", b"bb")];
        let queue = idle_queue();
        let (chat, topic) = queue
            .with_store(|store| {
                let topic = store.upsert_destination(&destination(77, 4))?;
                let patch = DestinationPatch {
                    default_mode: Some(SendMode::Document),
                    ..Default::default()
                };
                store.update_destination(topic, &patch)?;
                Ok((first_destination(store), topic))
            })
            .unwrap();

        let refused = add_files(&queue, &FreeOnly, &paths, &[chat, topic], None, None).unwrap_err();
        assert_eq!(refused.code, "FEATURE_NOT_LICENSED");
        assert_eq!(refused.params, json!({ "feature": "multi_destination" }));
        assert!(queue.list().unwrap().is_empty(), "nothing queued");
        // The same destination twice is one destination: no license needed.
        let single = add_files(&queue, &FreeOnly, &paths[..1], &[chat, chat], None, None).unwrap();
        assert_eq!(single.len(), 1);
        queue.clear(ClearScope::All).unwrap();

        let ids = add_files(&queue, &Licensed, &paths, &[chat, topic], None, Some("hi")).unwrap();
        let items = queue.list().unwrap();
        assert_eq!(items.iter().map(|item| item.id).collect::<Vec<_>>(), ids);
        let queued: Vec<(&str, i64, SendMode, &str)> = items
            .iter()
            .map(|item| {
                let name = item.source_path.rsplit(['\\', '/']).next().unwrap();
                (
                    name,
                    item.destination_id,
                    item.send_mode,
                    item.caption.as_str(),
                )
            })
            .collect();
        // File by file, each destination with its own default mode.
        assert_eq!(
            queued,
            [
                ("a.mp4", chat, SendMode::Video, "hi"),
                ("a.mp4", topic, SendMode::Document, "hi"),
                ("b.mp4", chat, SendMode::Video, "hi"),
                ("b.mp4", topic, SendMode::Document, "hi"),
            ]
        );
        assert!(items.iter().all(|item| item.state == ItemState::Ready));

        // A mode chosen for the batch applies to every destination.
        let ids = add_files(
            &queue,
            &Licensed,
            &paths[..1],
            &[chat, topic],
            Some(SendMode::Video),
            None,
        )
        .unwrap();
        let modes: Vec<SendMode> = ids
            .iter()
            .map(|&id| {
                queue
                    .with_store(|store| store.get_item(id))
                    .unwrap()
                    .send_mode
            })
            .collect();
        assert_eq!(modes, [SendMode::Video, SendMode::Video]);

        // An unknown destination refuses the whole call.
        let before = queue.list().unwrap().len();
        let unknown = add_files(&queue, &Licensed, &paths, &[chat, 404], None, None).unwrap_err();
        assert_eq!(unknown.code, "DESTINATION_NOT_FOUND");
        assert_eq!(queue.list().unwrap().len(), before);
        let none = add_files(&queue, &Licensed, &paths, &[], None, None).unwrap_err();
        assert_eq!(none.code, "DESTINATION_NOT_FOUND");
    }

    #[test]
    fn a_files_encoding_choices_need_the_license_and_a_started_item_refuses_them() {
        use crate::media::plan::{ResolutionCap, VideoCodec};

        let dir = TestDir::new("commands-encoding");
        let paths = [dir.file("a.mp4", b"a")];
        let queue = idle_queue();
        let chat = queue
            .with_store(|store| Ok(first_destination(store)))
            .unwrap();
        let id = add_files(&queue, &Licensed, &paths, &[chat], None, None).unwrap()[0];
        let stored = || queue.with_store(|store| store.get_item(id)).unwrap();
        let hevc = |target_mb| EncodingOverride {
            video_codec: VideoCodec::Hevc,
            max_resolution: ResolutionCap::P1440,
            target_mb,
        };

        let refused = set_encoding(&FreeOnly, &queue, id, Some(&hevc(None))).unwrap_err();
        assert_eq!(refused.code, "FEATURE_NOT_LICENSED");
        assert_eq!(refused.params, json!({ "feature": "advanced_encoding" }));
        assert_eq!(stored().encoding_override, None, "nothing stored");

        set_encoding(&Licensed, &queue, id, Some(&hevc(Some(25)))).unwrap();
        assert_eq!(stored().encoding_override, Some(hevc(Some(25))));
        for bad in [Some(0), Some(5000)] {
            let invalid = set_encoding(&Licensed, &queue, id, Some(&hevc(bad))).unwrap_err();
            assert_eq!(invalid.code, "SETTINGS_INVALID", "{bad:?}");
        }
        assert_eq!(stored().encoding_override, Some(hevc(Some(25))), "kept");

        // Going back to the settings never needs the license (it may have been lost).
        set_encoding(&FreeOnly, &queue, id, None).unwrap();
        assert_eq!(stored().encoding_override, None);

        // Once the item has started its plan exists: refused, whatever the license.
        set_encoding(&Licensed, &queue, id, Some(&hevc(None))).unwrap();
        queue
            .with_store(|store| {
                store
                    .conn()
                    .execute("UPDATE items SET state = 'encoded' WHERE id = ?1", [id])
                    .map(|_| ())
                    .map_err(QueueError::from)
            })
            .unwrap();
        for options in [None, Some(&hevc(None))] {
            let started = set_encoding(&Licensed, &queue, id, options).unwrap_err();
            assert_eq!(started.code, "ITEM_STARTED");
        }
        assert_eq!(stored().encoding_override, Some(hevc(None)), "unchanged");
    }
}
