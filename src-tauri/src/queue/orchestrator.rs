//! Sequential orchestrator: one item at a time, by position.
//!
//! Media work (probe, encode, remux) and sending are injected through `MediaStage` and
//! `SendStage`, so the loop is tested on deterministic doubles; the wall time of the schedule
//! through `Clock`, so tests run it on tokio's paused time.
//!
//! Three waits come before an upload, each where it belongs:
//! - the destination's minimum interval (F2) is a schedule: the item stays `encoded` with
//!   `next_run_at`, and the loop runs the other items meanwhile;
//! - the random pause between two sends (F20) and a FLOOD_WAIT (F21) hold the loop: they are
//!   about the account, so no other upload may go in between.

use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::events::{
    EventSink, Phase, ProgressReporter, QueueAlert, QueueChanged, QueueProgress, QueueStatus,
    log_alert,
};
use super::state::{Changes, Column};
use super::store::{
    ClearOutcome, ClearScope, Destination, DestinationPatch, HistoryEntry, HistoryFilter,
    HistoryStatus, Item, ItemPatch, NewItem, Store, remove_quietly,
};
use super::{QueueError, Result, State, iso, parse_iso, recovery};

/// What probing decided for the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Treatment {
    Encode,
    Remux,
    /// Already sendable as is ("skipping_encode"): the source is the output.
    Skip,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeOutcome {
    /// `size:blake3`.
    pub content_id: String,
    pub identity_weak: bool,
    pub media_json: Option<String>,
    pub plan_json: Option<String>,
    pub treatment: Treatment,
    /// Temporary output of encode/remux, deleted if the work is interrupted.
    pub temp_path: Option<String>,
}

/// What the media stage learned about a queued file before its turn: the `media_json` and
/// `plan_json` the probing stores, shown as the badge (compliant, compression planned...).
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    pub media_json: Option<String>,
    pub plan_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaOutput {
    pub output_path: String,
    pub output_size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StageError {
    #[error("cancelled")]
    Cancelled,
    /// `code` is a stable error code (`FFMPEG_FAILED`, `FILE_UNREADABLE`…); `params` go out
    /// with the `queue://alert` of the failure (`FFMPEG_FAILED{encoder, exitCode, atSeconds}`).
    #[error("{code}")]
    Failed { code: String, params: Value },
}

impl StageError {
    pub fn failed(code: &str) -> Self {
        StageError::Failed {
            code: code.to_owned(),
            params: json!({}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Sent {
        message_id: i64,
    },
    /// Nothing left: retry after the delay, never immediately.
    FloodWait {
        retry_after: Duration,
    },
}

/// `params` go out with the `queue://alert` of the failure (`TG_FILE_TOO_LARGE{limit}`...).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("cancelled")]
    Cancelled,
    #[error("{code}")]
    Failed { code: String, params: Value },
    /// The message may or may not have reached Telegram.
    #[error("{code}")]
    Unknown { code: String, params: Value },
}

impl SendError {
    pub fn failed(code: &str) -> Self {
        SendError::Failed {
            code: code.to_owned(),
            params: json!({}),
        }
    }

    pub fn unknown(code: &str) -> Self {
        SendError::Unknown {
            code: code.to_owned(),
            params: json!({}),
        }
    }
}

/// `item_resolve_unknown` actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveAction {
    /// The user found the message (with its id from a candidate, or without).
    MarkSent,
    /// Sends again; the front warns about a possible duplicate first.
    Retry,
}

/// Handed to a stage: the cancellation token of the current item and its progress reporter.
pub struct StageContext {
    pub cancel: CancellationToken,
    pub progress: ProgressReporter,
}

#[async_trait]
pub trait MediaStage: Send + Sync {
    /// Looks at a queued item right after it was added, in the background, so its badge is
    /// known long before its turn. Writes nothing and starts no encode. `None`: nothing to
    /// say (the file changed since it was queued, or there is no media tool). The probing
    /// reuses the result while the file is unchanged. Says nothing by default.
    async fn analyze(
        &self,
        _item: &Item,
        _cancel: &CancellationToken,
    ) -> std::result::Result<Option<Analysis>, StageError> {
        Ok(None)
    }

    async fn probe(
        &self,
        item: &Item,
        ctx: &StageContext,
    ) -> std::result::Result<ProbeOutcome, StageError>;
    async fn encode(
        &self,
        item: &Item,
        ctx: &StageContext,
    ) -> std::result::Result<MediaOutput, StageError>;
    async fn remux(
        &self,
        item: &Item,
        ctx: &StageContext,
    ) -> std::result::Result<MediaOutput, StageError>;
}

#[async_trait]
pub trait SendStage: Send + Sync {
    /// Asked right before the media work of an item starts (probe, then the encode, which is
    /// the expensive part): a destination checked when the file was added can have lost the
    /// right to receive it while the item waited. `Err` stops the item before any of that
    /// work; the upload checks the rights again by itself, so nothing is asked after it.
    /// A destination that cannot be checked is not a refusal. Accepts by default.
    async fn preflight(
        &self,
        _item: &Item,
        _destination: &Destination,
    ) -> std::result::Result<(), StageError> {
        Ok(())
    }

    /// Must honour `ctx.cancel`; returns `Sent` if the message went out before the cancel
    /// took effect.
    async fn upload(
        &self,
        item: &Item,
        destination: &Destination,
        ctx: &StageContext,
    ) -> std::result::Result<SendOutcome, SendError>;
}

/// Stand-in when ffmpeg or Telegram is not wired: every item fails with a code.
pub struct Unavailable;

#[async_trait]
impl MediaStage for Unavailable {
    async fn probe(
        &self,
        _: &Item,
        _: &StageContext,
    ) -> std::result::Result<ProbeOutcome, StageError> {
        Err(StageError::failed("FFMPEG_MISSING"))
    }

    async fn encode(
        &self,
        _: &Item,
        _: &StageContext,
    ) -> std::result::Result<MediaOutput, StageError> {
        Err(StageError::failed("FFMPEG_MISSING"))
    }

    async fn remux(
        &self,
        _: &Item,
        _: &StageContext,
    ) -> std::result::Result<MediaOutput, StageError> {
        Err(StageError::failed("FFMPEG_MISSING"))
    }
}

#[async_trait]
impl SendStage for Unavailable {
    async fn upload(
        &self,
        _: &Item,
        _: &Destination,
        _: &StageContext,
    ) -> std::result::Result<SendOutcome, SendError> {
        Err(SendError::failed("TG_NOT_AUTHORIZED"))
    }
}

/// Wall time of the schedule (`next_run_at`, `tg_sent_at`).
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The system clock: the app's.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Random pause between two sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseRange {
    pub min: Duration,
    pub max: Duration,
}

impl PauseRange {
    fn pick(self) -> Duration {
        if self.max <= self.min {
            return self.min;
        }
        let span = (self.max - self.min).as_millis() as u64;
        let random = RandomState::new().hash_one(std::time::SystemTime::now());
        self.min + Duration::from_millis(random % (span + 1))
    }
}

struct Current {
    item_id: i64,
    cancel: CancellationToken,
}

/// Analyses at most this many files at once: ffprobe is quick, and the point is to stay out
/// of the way of the encode and of the user.
const ANALYSES_AT_ONCE: usize = 2;

/// Items waiting for an analysis, and the ones being analyzed (with their cancellation, and a
/// number telling a later analysis of the same item from an earlier one).
#[derive(Default)]
struct Analyses {
    waiting: VecDeque<i64>,
    running: HashMap<i64, (u64, CancellationToken)>,
    runs: u64,
}

struct Inner {
    store: Mutex<Store>,
    media: Arc<dyn MediaStage>,
    send: Arc<dyn SendStage>,
    sink: Arc<dyn EventSink>,
    /// Lock order: `current` before `store`.
    current: Mutex<Option<Current>>,
    running: watch::Sender<bool>,
    wake: Notify,
    pauses: Mutex<PauseRange>,
    shutdown: CancellationToken,
    analyses: Mutex<Analyses>,
    analysis_wake: Notify,
    clock: Arc<dyn Clock>,
}

#[derive(Clone)]
pub struct Orchestrator {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves SQLite consistent (transactions roll back).
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Orchestrator {
    pub fn new(
        store: Store,
        media: Arc<dyn MediaStage>,
        send: Arc<dyn SendStage>,
        sink: Arc<dyn EventSink>,
        pauses: PauseRange,
    ) -> Self {
        Self::with_clock(store, media, send, sink, pauses, Arc::new(SystemClock))
    }

    pub fn with_clock(
        store: Store,
        media: Arc<dyn MediaStage>,
        send: Arc<dyn SendStage>,
        sink: Arc<dyn EventSink>,
        pauses: PauseRange,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store: Mutex::new(store),
                media,
                send,
                sink,
                current: Mutex::new(None),
                running: watch::Sender::new(false),
                wake: Notify::new(),
                pauses: Mutex::new(pauses),
                shutdown: CancellationToken::new(),
                analyses: Mutex::new(Analyses::default()),
                analysis_wake: Notify::new(),
                clock,
            }),
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.inner.clock.now()
    }

    pub fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        f(&mut lock(&self.inner.store))
    }

    /// Emits `queue://changed` for these items (also used for the startup recovery).
    pub fn emit_changed(&self, ids: &[i64]) {
        if ids.is_empty() {
            return;
        }
        match self.with_store(|store| store.deltas(ids)) {
            Ok(items) => self.inner.sink.queue_changed(&QueueChanged {
                items,
                removed: Vec::new(),
            }),
            Err(error) => tracing::error!(
                event = "queue_error",
                error_code = error.code(),
                error = %error,
                "could not read changed items"
            ),
        }
    }

    // --- commands ---

    pub fn add(&self, items: &[NewItem]) -> Result<Vec<i64>> {
        let ids = self.with_store(|store| store.add_items(items))?;
        let mut destinations: Vec<i64> = Vec::new();
        for item in items {
            if !destinations.contains(&item.destination_id) {
                destinations.push(item.destination_id);
            }
        }
        let added = json!({ "item_ids": ids, "destination_ids": destinations }).to_string();
        tracing::info!(
            event = "items_added",
            count = ids.len(),
            // The top-level key names one destination; a batch for several lists them.
            destination_id = match destinations[..] {
                [single] => Some(single),
                _ => None,
            },
            json = added.as_str(),
            "items added to the queue"
        );
        self.emit_changed(&ids);
        self.analyze_soon(&ids);
        self.inner.wake.notify_one();
        Ok(ids)
    }

    /// A file found in a watched folder, held in `pending_review` until the user approves it
    /// (`Store::add_watched_item`). The loop is not woken: the item cannot run yet.
    pub fn add_watched(&self, item: &NewItem) -> Result<Option<i64>> {
        let id = self.with_store(|store| store.add_watched_item(item))?;
        if let Some(id) = id {
            self.emit_changed(&[id]);
            self.analyze_soon(&[id]);
        }
        Ok(id)
    }

    pub fn list(&self) -> Result<Vec<Item>> {
        self.with_store(|store| store.list_items())
    }

    pub fn promote(&self, ids: &[i64]) -> Result<()> {
        self.with_store(|store| store.promote(ids))?;
        self.emit_changed(ids);
        self.inner.wake.notify_one();
        Ok(())
    }

    pub fn reorder(&self, ids: &[i64]) -> Result<()> {
        let changed = self.with_store(|store| store.reorder(ids))?;
        self.emit_changed(&changed);
        Ok(())
    }

    pub fn update_item(&self, id: i64, patch: &ItemPatch) -> Result<()> {
        self.with_store(|store| store.update_item(id, patch))?;
        if patch.send_mode.is_some() {
            // The badge described the other mode.
            self.analyze_again(&[id]);
        }
        Ok(())
    }

    /// `Store::set_encoding_override`; the badge is worked out again with the new choices.
    pub fn set_encoding_override(
        &self,
        id: i64,
        file: Option<&crate::media::plan::EncodingOverride>,
    ) -> Result<()> {
        self.with_store(|store| store.set_encoding_override(id, file))?;
        self.analyze_again(&[id]);
        Ok(())
    }

    /// The limit or the encoding settings changed: the plans shown for the items still waiting
    /// were built under the old ones, so they are worked out again.
    pub fn reanalyze_waiting(&self) -> Result<()> {
        let waiting = self.with_store(|store| {
            let items = store.items_in_states(&[State::Ready, State::PendingReview])?;
            Ok(items.into_iter().map(|item| item.id).collect::<Vec<_>>())
        })?;
        self.analyze_again(&waiting);
        Ok(())
    }

    /// User retry of a `failed` or `unknown` item (never automatic).
    pub fn retry(&self, id: i64) -> Result<State> {
        let state = self.with_store(|store| store.retry(id))?;
        self.emit_changed(&[id]);
        self.inner.wake.notify_one();
        Ok(state)
    }

    /// `queue_retry`: only a `failed` item; an `unknown` one goes through `resolve_unknown`,
    /// where the front warns about a duplicate.
    pub fn retry_failed(&self, id: i64) -> Result<State> {
        let state = self.with_store(|store| {
            require_state(store, id, State::Failed, State::Ready)?;
            store.retry(id)
        })?;
        self.emit_changed(&[id]);
        self.inner.wake.notify_one();
        Ok(state)
    }

    /// `item_resolve_unknown`: `mark_sent` (history `sent`, encoded file deleted) or `retry`.
    pub fn resolve_unknown(
        &self,
        id: i64,
        action: ResolveAction,
        message_id: Option<i64>,
    ) -> Result<State> {
        let state = self.with_store(|store| match action {
            ResolveAction::MarkSent => {
                require_state(store, id, State::Unknown, State::Sent)?;
                let item = store.mark_sent(id, message_id)?;
                store.discard_files(&item)?;
                Ok(State::Sent)
            }
            ResolveAction::Retry => {
                require_state(store, id, State::Unknown, State::Ready)?;
                store.retry(id)
            }
        })?;
        self.emit_changed(&[id]);
        if state != State::Sent {
            self.inner.wake.notify_one();
        }
        Ok(state)
    }

    /// Emits a `queue://alert` (error or warning, translated by the front from `code`).
    pub fn alert(&self, item_id: Option<i64>, code: &str, params: Value) {
        log_alert(item_id, code, &params);
        self.inner.sink.queue_alert(&QueueAlert {
            item_id,
            code: code.to_owned(),
            params,
        });
    }

    pub fn history(&self, filter: &HistoryFilter) -> Result<Vec<HistoryEntry>> {
        self.with_store(|store| store.query_history(filter))
    }

    pub fn start(&self) {
        self.set_running(true);
        self.inner.wake.notify_one();
    }

    /// Takes effect after the current item; an item only waiting for the server (FLOOD_WAIT)
    /// is put back to `encoded` at once.
    pub fn pause(&self) {
        self.set_running(false);
    }

    /// Emits `queue://status` when the value changes.
    fn set_running(&self, running: bool) {
        if self.inner.running.send_replace(running) != running {
            if running {
                tracing::info!(event = "queue_started", "queue started");
            } else {
                tracing::info!(
                    event = "queue_paused",
                    "queue paused (after the current item)"
                );
            }
            self.inner.sink.queue_status(&QueueStatus { running });
        }
    }

    pub fn is_running(&self) -> bool {
        *self.inner.running.borrow()
    }

    pub fn status(&self) -> QueueStatus {
        QueueStatus {
            running: self.is_running(),
        }
    }

    pub fn set_pauses(&self, pauses: PauseRange) {
        *lock(&self.inner.pauses) = pauses;
    }

    /// The current item is cancelled through its token (the stage stops, then the loop
    /// writes `cancelled`); any other item is cancelled right away.
    pub fn cancel(&self, id: i64) -> Result<()> {
        let current = lock(&self.inner.current);
        if let Some(current) = current.as_ref()
            && current.item_id == id
        {
            tracing::info!(
                event = "item_cancel",
                item_id = id,
                current = true,
                "cancel requested on the running item"
            );
            current.cancel.cancel();
            return Ok(());
        }
        tracing::info!(
            event = "item_cancel",
            item_id = id,
            current = false,
            "cancel requested"
        );
        self.with_store(|store| store.cancel(id))?;
        drop(current);
        self.forget_analyses(&[id]);
        self.emit_changed(&[id]);
        Ok(())
    }

    /// `queue_clear`: deletes the items `scope` selects that are not being worked on.
    pub fn clear(&self, scope: ClearScope) -> Result<ClearOutcome> {
        // `current` stays locked: `pick_next` cannot start an item between the choice of the
        // rows to delete and their deletion, and the item it holds is never one of them.
        let current = lock(&self.inner.current);
        let keep = current.as_ref().map(|current| current.item_id);
        let outcome = self.with_store(|store| store.clear_items(scope, keep))?;
        drop(current);
        self.forget_analyses(&outcome.removed);
        tracing::info!(
            event = "queue_cleared",
            scope = scope.as_str(),
            removed = outcome.removed.len(),
            running = outcome.running,
            "queue cleared"
        );
        if !outcome.removed.is_empty() {
            self.inner.sink.queue_changed(&QueueChanged {
                items: Vec::new(),
                removed: outcome.removed.clone(),
            });
        }
        Ok(outcome)
    }

    /// `destination_update`: the local settings of a destination. A new minimum interval also
    /// applies to its items already waiting for the old one: their schedule is dropped and the
    /// loop woken, so a shorter interval (or none) lets them go now rather than at the old time.
    pub fn update_destination(&self, id: i64, patch: &DestinationPatch) -> Result<Destination> {
        let (destination, released) = self.with_store(|store| {
            store.update_destination(id, patch)?;
            let released = match patch.min_interval_s {
                Some(_) => store.release_interval_holds(id)?,
                None => Vec::new(),
            };
            Ok((store.get_destination(id)?, released))
        })?;
        if !released.is_empty() {
            self.emit_changed(&released);
            self.inner.wake.notify_one();
        }
        Ok(destination)
    }

    /// `history_clear`: forgets every send, so a file already sent is no longer held as a
    /// duplicate. The queue and the destinations are not touched.
    pub fn clear_history(&self) -> Result<usize> {
        let removed = self.with_store(|store| store.clear_history())?;
        tracing::info!(event = "history_cleared", removed, "history cleared");
        Ok(removed)
    }

    /// Clean shutdown: stops the loop, an item still uploading becomes `unknown`.
    pub fn shutdown(&self) -> Result<()> {
        self.inner.shutdown.cancel();
        let changed = self.with_store(recovery::on_clean_shutdown)?;
        self.emit_changed(&changed);
        Ok(())
    }

    // --- loop ---

    pub async fn run(&self) {
        let mut running = self.inner.running.subscribe();
        let shutdown = self.inner.shutdown.clone();
        let mut last_sent: Option<Instant> = None;
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            if !*running.borrow_and_update() {
                tokio::select! {
                    _ = running.changed() => {}
                    _ = shutdown.cancelled() => return,
                }
                continue;
            }
            match self.pick_next() {
                Ok(Some((item, cancel))) => {
                    let id = item.id;
                    // Every line logged while this item runs carries its ids.
                    let span = tracing::error_span!(
                        "item",
                        item_id = id,
                        destination_id = item.destination_id
                    );
                    tokio::select! {
                        result = self.process(item, &cancel, &mut last_sent).instrument(span) => {
                            if let Err(error) = result {
                                tracing::error!(
                                    event = "queue_error",
                                    item_id = id,
                                    error_code = error.code(),
                                    error = %error,
                                    "item processing stopped on an internal error"
                                );
                            }
                        }
                        _ = shutdown.cancelled() => return,
                    }
                    self.release_current(id, &cancel);
                }
                Ok(None) => {
                    // An item held by its destination's interval wakes the loop when it is due.
                    let scheduled = self.until_next_scheduled();
                    tokio::select! {
                        _ = self.inner.wake.notified() => {}
                        _ = running.changed() => {}
                        _ = sleep_for(scheduled) => {}
                        _ = shutdown.cancelled() => return,
                    }
                }
                Err(error) => {
                    tracing::error!(
                        event = "queue_error",
                        error_code = error.code(),
                        error = %error,
                        "cannot pick the next item"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        _ = shutdown.cancelled() => return,
                    }
                }
            }
        }
    }

    // --- analysis at drop ---

    /// Queues items for an analysis (those already waiting or running are not queued twice).
    fn analyze_soon(&self, ids: &[i64]) {
        let mut analyses = lock(&self.inner.analyses);
        for &id in ids {
            if !analyses.waiting.contains(&id) && !analyses.running.contains_key(&id) {
                analyses.waiting.push_back(id);
            }
        }
        drop(analyses);
        self.inner.analysis_wake.notify_one();
    }

    /// The badge of these items is out of date: forget it, drop an analysis under way (it
    /// describes the old state) and start another.
    fn analyze_again(&self, ids: &[i64]) {
        self.forget_analyses(ids);
        match self.with_store(|store| store.clear_plans(ids)) {
            Ok(cleared) => self.emit_changed(&cleared),
            Err(error) => tracing::warn!(
                event = "queue_error",
                error_code = error.code(),
                error = %error,
                "could not clear stale plans"
            ),
        }
        self.analyze_soon(ids);
    }

    /// These items are gone, cancelled or about to run: their analyses are dropped, and one
    /// under way is stopped (its result would be thrown away).
    fn forget_analyses(&self, ids: &[i64]) {
        let mut analyses = lock(&self.inner.analyses);
        analyses.waiting.retain(|id| !ids.contains(id));
        for id in ids {
            if let Some((_, cancel)) = analyses.running.remove(id) {
                cancel.cancel();
            }
        }
    }

    /// The next item to analyze, marked as running.
    fn next_analysis(&self) -> Option<(i64, u64, CancellationToken)> {
        let mut analyses = lock(&self.inner.analyses);
        let id = analyses.waiting.pop_front()?;
        analyses.runs += 1;
        let run = analyses.runs;
        let cancel = self.inner.shutdown.child_token();
        analyses.running.insert(id, (run, cancel.clone()));
        let more = !analyses.waiting.is_empty();
        drop(analyses);
        if more {
            // One wake-up is stored at most: pass it on so the other worker takes its share.
            self.inner.analysis_wake.notify_one();
        }
        Some((id, run, cancel))
    }

    /// Analyzes queued items in the background until shutdown, `ANALYSES_AT_ONCE` at a time.
    /// Not tied to the queue's pause: it only reads files and sends nothing. Items already
    /// waiting without a badge (queued by a previous run) are taken first.
    pub async fn run_analysis(&self) {
        match self.with_store(|store| store.unanalyzed_ids()) {
            Ok(ids) => self.analyze_soon(&ids),
            Err(error) => tracing::warn!(
                event = "queue_error",
                error_code = error.code(),
                error = %error,
                "could not list the items to analyze"
            ),
        }
        let worker = || async {
            let shutdown = self.inner.shutdown.clone();
            loop {
                if shutdown.is_cancelled() {
                    return;
                }
                match self.next_analysis() {
                    Some((id, run, cancel)) => self.analyze_one(id, run, &cancel).await,
                    None => tokio::select! {
                        _ = self.inner.analysis_wake.notified() => {}
                        _ = shutdown.cancelled() => return,
                    },
                }
            }
        };
        // Two workers, as `ANALYSES_AT_ONCE` says.
        const _: () = assert!(ANALYSES_AT_ONCE == 2);
        let (_, _) = tokio::join!(worker(), worker());
    }

    async fn analyze_one(&self, id: i64, run: u64, cancel: &CancellationToken) {
        let finished = || {
            let mut analyses = lock(&self.inner.analyses);
            if analyses.running.get(&id).is_some_and(|(n, _)| *n == run) {
                analyses.running.remove(&id);
            }
        };
        // Gone, or not waiting any more: nothing to describe.
        let Ok(item) = self.get(id) else {
            return finished();
        };
        if !item.state.is_not_started() || item.plan_json.is_some() {
            return finished();
        }
        let started = std::time::Instant::now();
        let result = self.inner.media.analyze(&item, cancel).await;
        finished();
        // Cancelled, removed or about to run meanwhile: whatever it found is dropped.
        if cancel.is_cancelled() {
            return;
        }
        match result {
            Ok(Some(analysis)) => {
                let written = self.with_store(|store| {
                    store.set_analysis(
                        &item,
                        analysis.media_json.as_deref(),
                        analysis.plan_json.as_deref(),
                    )
                });
                match written {
                    Ok(true) => {
                        tracing::info!(
                            event = "item_analyzed",
                            item_id = id,
                            dur_ms = crate::logging::ms_since(started),
                            has_plan = analysis.plan_json.is_some(),
                            "item analyzed before its turn"
                        );
                        self.emit_changed(&[id]);
                    }
                    Ok(false) => tracing::debug!(
                        event = "analysis_dropped",
                        item_id = id,
                        "the item changed during its analysis"
                    ),
                    Err(error) => tracing::warn!(
                        event = "queue_error",
                        item_id = id,
                        error_code = error.code(),
                        error = %error,
                        "could not store an analysis"
                    ),
                }
            }
            Ok(None) | Err(StageError::Cancelled) => {}
            // Not a failure of the item: its probing reports the problem when its turn comes.
            Err(StageError::Failed { code, .. }) => tracing::debug!(
                event = "analysis_failed",
                item_id = id,
                error_code = code.as_str(),
                "analysis before the turn found nothing"
            ),
        }
    }

    /// How long until the next item waiting for its `next_run_at`; `None` if there is none.
    fn until_next_scheduled(&self) -> Option<Duration> {
        let now = self.now();
        match self.with_store(|store| store.next_scheduled(now)) {
            Ok(at) => at.map(|at| (at - now).to_std().unwrap_or(Duration::ZERO)),
            Err(error) => {
                tracing::error!(
                    event = "queue_error",
                    error_code = error.code(),
                    error = %error,
                    "cannot read the schedule"
                );
                // Looked at again on the next wake.
                None
            }
        }
    }

    /// Takes the next due `ready` (→ `probing`) or `encoded` item and makes it current.
    fn pick_next(&self) -> Result<Option<(Item, CancellationToken)>> {
        let mut current = lock(&self.inner.current);
        let now = self.now();
        let picked = self.with_store(|store| {
            let Some(item) = store.next_runnable(now)? else {
                return Ok(None);
            };
            if item.state == State::Ready {
                store.transition(
                    item.id,
                    State::Probing,
                    &Changes::new()
                        .set(Column::ProgressPhase, "probing".to_owned())
                        .reason("picked"),
                )?;
            }
            store.get_item(item.id).map(Some)
        })?;
        let Some(item) = picked else {
            return Ok(None);
        };
        let cancel = CancellationToken::new();
        *current = Some(Current {
            item_id: item.id,
            cancel: cancel.clone(),
        });
        drop(current);
        // Its own probing starts now: an analysis still running would only compete with it.
        self.forget_analyses(&[item.id]);
        self.emit_changed(&[item.id]);
        Ok(Some((item, cancel)))
    }

    /// A cancel that arrived after the stage had returned (item `failed`, or held as a
    /// duplicate) is applied here; on a `sent` item it is refused, as it should be.
    fn release_current(&self, id: i64, cancel: &CancellationToken) {
        let mut current = lock(&self.inner.current);
        *current = None;
        if cancel.is_cancelled()
            && self
                .with_store(|store| store.get_item(id))
                .is_ok_and(|item| item.state.can_transition(State::Cancelled))
            && self.with_store(|store| store.cancel(id)).is_ok()
        {
            drop(current);
            self.emit_changed(&[id]);
        }
    }

    fn change(&self, id: i64, to: State, changes: Changes) -> Result<()> {
        self.with_store(|store| store.transition(id, to, &changes))?;
        self.emit_changed(&[id]);
        Ok(())
    }

    fn get(&self, id: i64) -> Result<Item> {
        self.with_store(|store| store.get_item(id))
    }

    fn context(&self, id: i64, phase: Phase, cancel: &CancellationToken) -> StageContext {
        StageContext {
            cancel: cancel.clone(),
            progress: ProgressReporter::new(id, phase, self.inner.sink.clone()),
        }
    }

    fn finish_cancelled(&self, id: i64) -> Result<()> {
        self.with_store(|store| store.cancel(id))?;
        self.emit_changed(&[id]);
        Ok(())
    }

    fn finish_with(
        &self,
        id: i64,
        to: State,
        code: &str,
        status: HistoryStatus,
        reason: &'static str,
    ) -> Result<()> {
        let item = self.get(id)?;
        if let Some(temp) = &item.temp_path {
            remove_quietly(temp);
        }
        self.change(
            id,
            to,
            Changes::new()
                .set(Column::LastError, code.to_owned())
                .clear(Column::TempPath)
                .clear(Column::ProgressPhase)
                .reason(reason),
        )?;
        let item = self.get(id)?;
        self.with_store(|store| store.record_history(&item, status))?;
        Ok(())
    }

    async fn process(
        &self,
        item: Item,
        cancel: &CancellationToken,
        last_sent: &mut Option<Instant>,
    ) -> Result<()> {
        let id = item.id;
        if item.state == State::Probing && !self.run_media(item, cancel).await? {
            return Ok(());
        }
        self.run_upload(id, cancel, last_sent).await
    }

    /// Probing then encode/remux. Returns true when the item reached `encoded`.
    async fn run_media(&self, item: Item, cancel: &CancellationToken) -> Result<bool> {
        let id = item.id;
        tracing::info!(
            event = "media_start",
            source_path = item.source_path.as_str(),
            source_size = item.source_size,
            send_mode = item.send_mode.as_str(),
            has_caption = !item.caption.is_empty(),
            attempts = item.attempts,
            "media work started"
        );
        let destination = self.with_store(|store| store.get_destination(item.destination_id))?;
        if let Err(error) = self.inner.send.preflight(&item, &destination).await {
            match error {
                StageError::Cancelled => self.finish_cancelled(id)?,
                StageError::Failed { code, params } => {
                    self.finish_with(
                        id,
                        State::Failed,
                        &code,
                        HistoryStatus::Failed,
                        "destination_refused",
                    )?;
                    self.alert(Some(id), &code, params);
                    self.release_outputs(id)?;
                }
            }
            return Ok(false);
        }
        let ctx = self.context(id, Phase::Probing, cancel);
        let probe = match self.inner.media.probe(&item, &ctx).await {
            Ok(probe) => probe,
            Err(error) => {
                self.media_failed(id, error)?;
                return Ok(false);
            }
        };
        if cancel.is_cancelled() {
            self.finish_cancelled(id)?;
            return Ok(false);
        }

        // The identity is checked once: if it is unchanged since a previous probing, the user
        // already confirmed the item (promote or retry) and it is not held again.
        let already_checked = item.content_id.as_deref() == Some(probe.content_id.as_str());
        if !already_checked
            && self
                .with_store(|store| store.is_duplicate(&probe.content_id, item.destination_id))?
        {
            self.with_store(|store| {
                store.hold_duplicate(id, &probe.content_id, probe.identity_weak)
            })?;
            self.emit_changed(&[id]);
            self.release_outputs(id)?;
            return Ok(false);
        }

        let identity = Changes::new()
            .set(Column::ContentId, probe.content_id.clone())
            .set(Column::IdentityWeak, probe.identity_weak)
            .set(Column::MediaJson, probe.media_json.clone())
            .set(Column::PlanJson, probe.plan_json.clone());
        // The same file queued for another destination with the same plan was already
        // encoded: its file is sent again rather than encoded a second time.
        if probe.treatment != Treatment::Skip
            && let Some(reused) = self.with_store(|store| {
                store.take_over_output(
                    id,
                    &probe.content_id,
                    item.send_mode,
                    probe.plan_json.as_deref(),
                    identity.clone(),
                )
            })?
        {
            tracing::info!(
                event = "encode_reused",
                from_item_id = reused.item_id,
                output_size = reused.output_size,
                "encoded file of another item reused"
            );
            self.emit_changed(&[id]);
            return Ok(true);
        }
        let (to, phase, name) = match probe.treatment {
            Treatment::Skip => {
                self.change(
                    id,
                    State::Encoded,
                    identity
                        .set(Column::OutputPath, item.source_path.clone())
                        .set(Column::OutputSize, item.source_size)
                        .clear(Column::ProgressPhase)
                        .reason("sendable_as_is"),
                )?;
                self.release_outputs(id)?;
                return Ok(true);
            }
            Treatment::Encode => (State::Encoding, Phase::Encoding, "encoding"),
            Treatment::Remux => (State::Remuxing, Phase::Remuxing, "remuxing"),
        };
        self.change(
            id,
            to,
            identity
                .set(Column::TempPath, probe.temp_path.clone())
                .set(Column::ProgressPhase, name.to_owned())
                .reason("probed"),
        )?;
        // Encoding its own file: an earlier encode of its source kept for it is not needed.
        self.release_outputs(id)?;

        let item = self.get(id)?;
        let ctx = self.context(id, phase, cancel);
        let result = match probe.treatment {
            Treatment::Remux => self.inner.media.remux(&item, &ctx).await,
            _ => self.inner.media.encode(&item, &ctx).await,
        };
        match result {
            Ok(output) => {
                self.change(
                    id,
                    State::Encoded,
                    Changes::new()
                        .set(Column::OutputPath, output.output_path)
                        .set(Column::OutputSize, output.output_size)
                        .clear(Column::TempPath)
                        .clear(Column::ProgressPhase)
                        .reason("media_done"),
                )?;
                Ok(true)
            }
            Err(error) => {
                self.media_failed(id, error)?;
                Ok(false)
            }
        }
    }

    fn media_failed(&self, id: i64, error: StageError) -> Result<()> {
        match error {
            StageError::Cancelled => self.finish_cancelled(id),
            StageError::Failed { code, params } => {
                self.finish_with(
                    id,
                    State::Failed,
                    &code,
                    HistoryStatus::Failed,
                    "media_failed",
                )?;
                self.alert(Some(id), &code, params);
                self.release_outputs(id)
            }
        }
    }

    /// `Store::release_outputs` for an item that left `probing` without taking over an
    /// encoded file of its source.
    fn release_outputs(&self, id: i64) -> Result<()> {
        self.with_store(|store| {
            let item = store.get_item(id)?;
            store.release_outputs(&item)
        })
    }

    /// Upload, with `waiting_server` rounds on FLOOD_WAIT, until a final state; or, when the
    /// destination's minimum interval has not elapsed, back to the schedule.
    async fn run_upload(
        &self,
        id: i64,
        cancel: &CancellationToken,
        last_sent: &mut Option<Instant>,
    ) -> Result<()> {
        if !cancel.is_cancelled() && self.defer_for_interval(id)? {
            return Ok(());
        }
        loop {
            let item = self.get(id)?;
            match self.wait_before_upload(&item, *last_sent, cancel).await {
                Wait::Done => {}
                Wait::Cancelled => return self.finish_cancelled(id),
                Wait::Paused => return self.park_paused(&item),
            }
            self.change(
                id,
                State::Uploading,
                Changes::new()
                    .clear(Column::NextRunAt)
                    .set(Column::ProgressPhase, "uploading".to_owned())
                    .bump_attempts()
                    .reason("upload_start"),
            )?;
            let item = self.get(id)?;
            let destination =
                self.with_store(|store| store.get_destination(item.destination_id))?;
            let ctx = self.context(id, Phase::Uploading, cancel);
            match self.inner.send.upload(&item, &destination, &ctx).await {
                Ok(SendOutcome::Sent { message_id }) => {
                    self.change(
                        id,
                        State::Sent,
                        Changes::new()
                            .set(Column::TgMessageId, message_id)
                            .set(Column::TgSentAt, iso(self.now()))
                            .clear(Column::LastError)
                            .clear(Column::ProgressPhase)
                            .reason("sent"),
                    )?;
                    *last_sent = Some(Instant::now());
                    let item = self.get(id)?;
                    self.with_store(|store| {
                        store.record_history(&item, HistoryStatus::Sent)?;
                        store.discard_files(&item)
                    })?;
                    return Ok(());
                }
                Ok(SendOutcome::FloodWait { retry_after }) => {
                    let delay =
                        chrono::Duration::from_std(retry_after).unwrap_or(chrono::Duration::MAX);
                    let at = self
                        .now()
                        .checked_add_signed(delay)
                        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC);
                    let next_run_at = iso(at);
                    tracing::warn!(
                        event = "flood_wait",
                        error_code = "TG_FLOOD_WAIT",
                        retry_after_s = crate::transport::seconds_up(retry_after),
                        next_run_at = next_run_at.as_str(),
                        "Telegram asked to wait; nothing was sent"
                    );
                    self.change(
                        id,
                        State::WaitingServer,
                        Changes::new()
                            .set(Column::NextRunAt, iso(at))
                            .set(Column::LastError, "TG_FLOOD_WAIT".to_owned())
                            .set(Column::ProgressPhase, "waiting_server".to_owned())
                            .reason("flood_wait"),
                    )?;
                    self.alert(
                        Some(id),
                        "TG_FLOOD_WAIT",
                        json!({ "retry_after": crate::transport::seconds_up(retry_after) }),
                    );
                }
                Err(SendError::Cancelled) => return self.finish_cancelled(id),
                Err(SendError::Failed { code, params }) => {
                    self.finish_with(
                        id,
                        State::Failed,
                        &code,
                        HistoryStatus::Failed,
                        "upload_failed",
                    )?;
                    self.alert(Some(id), &code, params);
                    return Ok(());
                }
                Err(SendError::Unknown { code, params }) => {
                    self.finish_with(
                        id,
                        State::Unknown,
                        &code,
                        HistoryStatus::Unknown,
                        "upload_in_doubt",
                    )?;
                    self.alert(Some(id), &code, params);
                    return Ok(());
                }
            }
        }
    }

    /// The destination's minimum interval (F2), counted from its last successful send: if it
    /// has not elapsed, the item stays `encoded` with `next_run_at` at its end and the loop
    /// moves on. Checked again when the item comes back, since another item for the same
    /// destination may have been sent first. True when the item was deferred.
    fn defer_for_interval(&self, id: i64) -> Result<bool> {
        let item = self.get(id)?;
        let destination = self.with_store(|store| store.get_destination(item.destination_id))?;
        if destination.min_interval_s <= 0 {
            return Ok(false);
        }
        let Some(last) = self.with_store(|store| store.last_sent_at(destination.id))? else {
            return Ok(false);
        };
        let interval = chrono::Duration::try_seconds(destination.min_interval_s)
            .unwrap_or(chrono::Duration::MAX);
        let due = last
            .checked_add_signed(interval)
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        let now = self.now();
        if due <= now {
            return Ok(false);
        }
        let wait_s = crate::transport::seconds_up((due - now).to_std().unwrap_or_default());
        let next_run_at = iso(due);
        tracing::info!(
            event = "upload_deferred",
            min_interval_s = destination.min_interval_s,
            wait_s,
            next_run_at = next_run_at.as_str(),
            "minimum interval of the destination not elapsed; item rescheduled"
        );
        self.with_store(|store| store.defer(id, due))?;
        self.emit_changed(&[id]);
        Ok(true)
    }

    /// The queue was paused while the item waited for the server: it goes back to `encoded`
    /// with its `next_run_at` (and the FLOOD_WAIT that explains it) untouched, so the loop
    /// does not sit on it for the rest of a wait that may last hours, and resuming honours
    /// whatever remains of the wait.
    fn park_paused(&self, item: &Item) -> Result<()> {
        tracing::info!(
            event = "upload_wait_paused",
            next_run_at = item.next_run_at.as_deref().unwrap_or(""),
            "queue paused during the wait; item put back, its schedule kept"
        );
        if item.state == State::WaitingServer {
            self.change(
                item.id,
                State::Encoded,
                Changes::new()
                    .clear(Column::ProgressPhase)
                    .reason("paused_during_wait"),
            )?;
            self.emit_changed(&[item.id]);
        }
        Ok(())
    }

    /// Waits for `next_run_at` and for the pause between sends. A wait for the server also
    /// ends when the queue is paused (the short pause between sends does not: pausing takes
    /// effect after the current item).
    async fn wait_before_upload(
        &self,
        item: &Item,
        last_sent: Option<Instant>,
        cancel: &CancellationToken,
    ) -> Wait {
        let server_wait = item
            .next_run_at
            .as_deref()
            .and_then(parse_iso)
            .and_then(|at| (at - self.now()).to_std().ok())
            .unwrap_or(Duration::ZERO);
        let pause = last_sent
            .map(|at| lock(&self.inner.pauses).pick().saturating_sub(at.elapsed()))
            .unwrap_or(Duration::ZERO);
        if !server_wait.is_zero() {
            self.inner.sink.queue_progress(&QueueProgress {
                item_id: item.id,
                phase: Phase::WaitingServer,
                percent: 0.0,
                bytes_per_sec: None,
                eta_sec: None,
                retry_after_s: Some(server_wait.as_secs_f64().ceil() as u64),
                detail: None,
            });
        }
        let wait = server_wait.max(pause);
        if !wait.is_zero() {
            tracing::debug!(
                event = "upload_wait",
                item_id = item.id,
                server_wait_ms = server_wait.as_millis() as u64,
                pause_ms = pause.as_millis() as u64,
                "waiting before the upload"
            );
        }
        if cancel.is_cancelled() {
            return Wait::Cancelled;
        }
        if wait.is_zero() {
            return Wait::Done;
        }
        let mut running = self.inner.running.subscribe();
        tokio::select! {
            _ = tokio::time::sleep(wait) => {
                if cancel.is_cancelled() { Wait::Cancelled } else { Wait::Done }
            }
            _ = cancel.cancelled() => Wait::Cancelled,
            _ = queue_paused(&mut running), if !server_wait.is_zero() => Wait::Paused,
        }
    }
}

/// How `wait_before_upload` ended.
enum Wait {
    Done,
    Cancelled,
    Paused,
}

/// Completes when the queue is (or becomes) paused.
async fn queue_paused(running: &mut watch::Receiver<bool>) {
    // An error means the sender is gone, which cannot happen while the orchestrator lives.
    let _ = running.wait_for(|running| !*running).await;
}

/// Sleeps for `duration`, or forever without one (a `select!` branch that never fires).
async fn sleep_for(duration: Option<Duration>) {
    match duration {
        Some(duration) => tokio::time::sleep(duration).await,
        None => std::future::pending().await,
    }
}

/// Refuses an action on an item that is not in `expected` (as an illegal `from -> to`).
fn require_state(store: &Store, id: i64, expected: State, to: State) -> Result<()> {
    let from = store.get_item(id)?.state;
    if from == expected {
        Ok(())
    } else {
        Err(QueueError::IllegalTransition {
            item_id: id,
            from,
            to,
        })
    }
}

#[cfg(test)]
#[path = "orchestrator_tests.rs"]
mod tests;
