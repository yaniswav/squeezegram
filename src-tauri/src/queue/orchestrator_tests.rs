//! Orchestrator tests on deterministic doubles, with tokio's paused clock: pauses, FLOOD_WAIT
//! delays and minimum intervals elapse instantly. The schedule's wall time (`Clock`) follows
//! the same paused clock, so `next_run_at` comes due when the test lets time pass.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use tokio::task::JoinHandle;

use super::*;
use crate::queue::events::recorder::Recorder;
use crate::queue::store::{ClearScope, HOLD_DUPLICATE, SendMode};
use crate::queue::test_util::{TestDir, destination, first_destination, memory_store};

/// Counts stages running at the same time, across media and send.
#[derive(Default)]
struct Concurrency {
    active: AtomicUsize,
    max: AtomicUsize,
}

struct Active<'a>(&'a Concurrency);

impl Concurrency {
    fn enter(&self) -> Active<'_> {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
        Active(self)
    }
}

impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy)]
enum MediaStep {
    Ok,
    Fail,
    WaitCancel,
}

struct FakeMedia {
    treatment: Treatment,
    encode_steps: Mutex<VecDeque<MediaStep>>,
    encodes: AtomicUsize,
    entered: Notify,
    concurrency: Arc<Concurrency>,
    /// How long an analysis takes (it ends early if cancelled), and what it saw.
    analysis_time: Mutex<Duration>,
    analysis_concurrency: Concurrency,
    analyses: Mutex<Vec<(i64, SendMode)>>,
    analyses_cancelled: AtomicUsize,
}

fn sq_path(source: &str) -> String {
    let path = Path::new(source);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{stem}_sq.mp4"))
        .to_string_lossy()
        .into_owned()
}

#[async_trait]
impl MediaStage for FakeMedia {
    async fn analyze(
        &self,
        item: &Item,
        cancel: &CancellationToken,
    ) -> std::result::Result<Option<Analysis>, StageError> {
        let _active = self.analysis_concurrency.enter();
        self.analyses
            .lock()
            .unwrap()
            .push((item.id, item.send_mode));
        let time = *self.analysis_time.lock().unwrap();
        tokio::select! {
            _ = tokio::time::sleep(time) => {}
            _ = cancel.cancelled() => {
                self.analyses_cancelled.fetch_add(1, Ordering::SeqCst);
                return Err(StageError::Cancelled);
            }
        }
        Ok(Some(Analysis {
            media_json: Some(json!({ "analyzed": item.id }).to_string()),
            plan_json: Some(json!({ "analyzed_as": item.send_mode.as_str() }).to_string()),
        }))
    }

    async fn probe(
        &self,
        item: &Item,
        _: &StageContext,
    ) -> std::result::Result<ProbeOutcome, StageError> {
        let _active = self.concurrency.enter();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let name = Path::new(&item.source_path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        Ok(ProbeOutcome {
            content_id: format!("{}:{name}", item.source_size),
            identity_weak: false,
            media_json: Some("{}".into()),
            // The mode changes the plan, as in the real stage: no reuse across modes.
            plan_json: Some(json!({ "mode": item.send_mode.as_str() }).to_string()),
            treatment: self.treatment,
            temp_path: Some(format!("{}.part", sq_path(&item.source_path))),
        })
    }

    async fn encode(
        &self,
        item: &Item,
        ctx: &StageContext,
    ) -> std::result::Result<MediaOutput, StageError> {
        let _active = self.concurrency.enter();
        self.encodes.fetch_add(1, Ordering::SeqCst);
        let temp = item.temp_path.clone().unwrap_or_default();
        std::fs::write(&temp, b"encoded").map_err(|_| StageError::failed("DISK_SPACE"))?;
        self.entered.notify_one();
        ctx.progress.report(crate::queue::events::Progress {
            percent: 50.0,
            ..Default::default()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let step = self
            .encode_steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(MediaStep::Ok);
        match step {
            MediaStep::Ok => {
                let output = sq_path(&item.source_path);
                std::fs::rename(&temp, &output).unwrap();
                Ok(MediaOutput {
                    output_path: output,
                    output_size: 7,
                })
            }
            MediaStep::Fail => Err(StageError::Failed {
                code: "FFMPEG_FAILED".into(),
                params: json!({ "encoder": "h264_nvenc", "exitCode": 1, "atSeconds": 4 }),
            }),
            MediaStep::WaitCancel => {
                ctx.cancel.cancelled().await;
                Err(StageError::Cancelled)
            }
        }
    }

    async fn remux(
        &self,
        item: &Item,
        ctx: &StageContext,
    ) -> std::result::Result<MediaOutput, StageError> {
        self.encode(item, ctx).await
    }
}

#[derive(Debug, Clone, Copy)]
enum SendStep {
    Sent,
    FloodWait(u64),
    Fail,
    Unknown,
    WaitCancel,
    /// Blocks until the test opens the gate, then sends.
    Gate,
}

struct FakeSend {
    steps: Mutex<VecDeque<SendStep>>,
    calls: Mutex<Vec<(i64, Instant)>>,
    entered: Notify,
    gate: Notify,
    next_id: AtomicI64,
    concurrency: Arc<Concurrency>,
    reuse_steps: Mutex<VecDeque<SendStep>>,
    routes: Mutex<Vec<(i64, SendVia)>>,
    /// The bytes of the file each upload was handed.
    sent: Mutex<Vec<(i64, Vec<u8>)>>,
}

impl FakeSend {
    fn calls_for(&self, id: i64) -> Vec<Instant> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(item, _)| *item == id)
            .map(|(_, at)| *at)
            .collect()
    }

    fn order(&self) -> Vec<i64> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect()
    }
}

#[async_trait]
impl SendStage for FakeSend {
    async fn reuse(
        &self,
        item: &Item,
        _: &Destination,
        _: &Destination,
        _: i64,
        via: SendVia,
        _: &StageContext,
    ) -> std::result::Result<SendOutcome, SendError> {
        self.routes.lock().unwrap().push((item.id, via));
        match self
            .reuse_steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(SendStep::Sent)
        {
            SendStep::Sent => Ok(SendOutcome::Sent {
                message_id: self.next_id.fetch_add(1, Ordering::SeqCst),
            }),
            SendStep::Fail => Err(SendError::failed("TG_UPLOAD_FAILED")),
            SendStep::Unknown => Err(SendError::unknown("TG_TIMEOUT")),
            SendStep::FloodWait(seconds) => Ok(SendOutcome::FloodWait {
                retry_after: Duration::from_secs(seconds),
            }),
            _ => Err(SendError::Cancelled),
        }
    }

    async fn upload(
        &self,
        item: &Item,
        _: &Destination,
        ctx: &StageContext,
    ) -> std::result::Result<SendOutcome, SendError> {
        let _active = self.concurrency.enter();
        self.calls.lock().unwrap().push((item.id, Instant::now()));
        if let Some(bytes) = item
            .output_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
        {
            self.sent.lock().unwrap().push((item.id, bytes));
        }
        self.entered.notify_one();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(SendStep::Sent);
        let sent = Ok(SendOutcome::Sent {
            message_id: self.next_id.fetch_add(1, Ordering::SeqCst),
        });
        match step {
            SendStep::Sent => sent,
            SendStep::FloodWait(seconds) => Ok(SendOutcome::FloodWait {
                retry_after: Duration::from_secs(seconds),
            }),
            SendStep::Fail => Err(SendError::failed("TG_UPLOAD_FAILED")),
            SendStep::Unknown => Err(SendError::unknown("TG_TIMEOUT")),
            SendStep::WaitCancel => {
                ctx.cancel.cancelled().await;
                Err(SendError::Cancelled)
            }
            SendStep::Gate => {
                self.gate.notified().await;
                sent
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn batch_uploads_once_then_copies_and_preserves_routes() {
    let h = Harness::new("batch-copies", Treatment::Encode);
    let targets: Vec<_> = h
        .chats(3)
        .into_iter()
        .map(|id| (id, SendMode::Video))
        .collect();
    let ids = h.add_to("one.mp4", &targets);
    h.orch.with_store(|s| s.link_send_batch(&ids)).unwrap();
    h.orch.start();
    h.wait_state(ids[2], State::Sent).await;
    assert_eq!(h.send.order(), [ids[0]]);
    assert_eq!(
        *h.send.routes.lock().unwrap(),
        [(ids[1], SendVia::Copy), (ids[2], SendVia::Copy)]
    );
    assert_eq!(h.media.encodes.load(Ordering::SeqCst), 1);
    let routes: Vec<String> = h
        .orch
        .with_store(|s| {
            let mut stmt = s.conn().prepare("SELECT via FROM history ORDER BY id")?;
            Ok(stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(routes, ["upload", "copy", "copy"]);
}

#[tokio::test(start_paused = true)]
async fn batch_reuse_refusals_fall_back_but_unknown_never_does() {
    for unknown in [false, true] {
        let h = Harness::new("batch-fallback", Treatment::Skip);
        let targets: Vec<_> = h
            .chats(2)
            .into_iter()
            .map(|id| (id, SendMode::Video))
            .collect();
        let ids = h.add_to("one.mp4", &targets);
        h.orch.with_store(|s| s.link_send_batch(&ids)).unwrap();
        h.send.reuse_steps.lock().unwrap().extend(if unknown {
            vec![SendStep::Unknown]
        } else {
            vec![SendStep::Fail, SendStep::Fail]
        });
        h.orch.start();
        h.wait_state(ids[1], if unknown { State::Unknown } else { State::Sent })
            .await;
        if unknown {
            assert_eq!(h.send.order(), [ids[0]]);
            assert_eq!(*h.send.routes.lock().unwrap(), [(ids[1], SendVia::Copy)]);
        } else {
            assert_eq!(h.send.order(), [ids[0], ids[1]]);
            assert_eq!(
                *h.send.routes.lock().unwrap(),
                [(ids[1], SendVia::Copy), (ids[1], SendVia::Remote)]
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn batch_primary_unknown_holds_dependents_until_manual_confirmation() {
    let h = Harness::new("batch-unknown", Treatment::Skip);
    let targets: Vec<_> = h
        .chats(2)
        .into_iter()
        .map(|id| (id, SendMode::Video))
        .collect();
    let ids = h.add_to("one.mp4", &targets);
    h.orch.with_store(|s| s.link_send_batch(&ids)).unwrap();
    h.send_steps(&[SendStep::Unknown]);
    h.orch.start();
    h.wait_state(ids[0], State::Unknown).await;
    h.idle(2).await;
    assert_eq!(
        h.item(ids[1]).hold_reason.as_deref(),
        Some("awaiting_primary")
    );
    assert_eq!(h.send.order(), [ids[0]]);
    h.orch
        .with_store(|store| {
            store.remember_reconciliation_candidates(&store.get_item(ids[0])?, vec![1000])
        })
        .unwrap();
    h.orch
        .resolve_unknown(ids[0], ResolveAction::MarkSent, Some(1000))
        .unwrap();
    h.wait_state(ids[1], State::Sent).await;
    assert_eq!(h.send.order(), [ids[0]]);
}

#[tokio::test(start_paused = true)]
async fn batch_failed_primary_promotes_the_next_destination() {
    let h = Harness::new("batch-promotion", Treatment::Skip);
    let targets: Vec<_> = h
        .chats(3)
        .into_iter()
        .map(|id| (id, SendMode::Video))
        .collect();
    let ids = h.add_to("one.mp4", &targets);
    h.orch.with_store(|s| s.link_send_batch(&ids)).unwrap();
    h.send_steps(&[SendStep::Fail, SendStep::Sent]);
    h.orch.start();
    h.wait_state(ids[2], State::Sent).await;
    assert_eq!(h.send.order(), [ids[0], ids[1]]);
    assert_eq!(*h.send.routes.lock().unwrap(), [(ids[2], SendVia::Copy)]);
}

/// Wall time that advances with tokio's (paused) clock, from the real time at creation.
struct PausedClock {
    origin: DateTime<Utc>,
    started: Instant,
}

impl PausedClock {
    fn new() -> Self {
        Self {
            origin: Utc::now(),
            started: Instant::now(),
        }
    }
}

impl Clock for PausedClock {
    fn now(&self) -> DateTime<Utc> {
        self.origin + self.started.elapsed()
    }
}

struct Harness {
    orch: Orchestrator,
    clock: Arc<PausedClock>,
    media: Arc<FakeMedia>,
    send: Arc<FakeSend>,
    sink: Arc<Recorder>,
    concurrency: Arc<Concurrency>,
    dir: TestDir,
    task: JoinHandle<()>,
}

impl Harness {
    fn new(name: &str, treatment: Treatment) -> Self {
        let concurrency = Arc::new(Concurrency::default());
        let media = Arc::new(FakeMedia {
            treatment,
            encode_steps: Mutex::new(VecDeque::new()),
            encodes: AtomicUsize::new(0),
            entered: Notify::new(),
            concurrency: concurrency.clone(),
            analysis_time: Mutex::new(Duration::from_millis(5)),
            analysis_concurrency: Concurrency::default(),
            analyses: Mutex::new(Vec::new()),
            analyses_cancelled: AtomicUsize::new(0),
        });
        let send = Arc::new(FakeSend {
            reuse_steps: Mutex::new(VecDeque::new()),
            routes: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
            steps: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            entered: Notify::new(),
            gate: Notify::new(),
            next_id: AtomicI64::new(1000),
            concurrency: concurrency.clone(),
        });
        let sink = Arc::new(Recorder::default());
        let clock = Arc::new(PausedClock::new());
        let orch = Orchestrator::with_clock(
            memory_store(),
            media.clone(),
            send.clone(),
            sink.clone(),
            PauseRange {
                min: Duration::ZERO,
                max: Duration::ZERO,
            },
            clock.clone(),
        );
        let runner = orch.clone();
        let task = tokio::spawn(async move { runner.run().await });
        Self {
            orch,
            clock,
            media,
            send,
            sink,
            concurrency,
            dir: TestDir::new(name),
            task,
        }
    }

    fn send_steps(&self, steps: &[SendStep]) {
        self.send
            .steps
            .lock()
            .unwrap()
            .extend(steps.iter().copied());
    }

    fn encode_steps(&self, steps: &[MediaStep]) {
        self.media
            .encode_steps
            .lock()
            .unwrap()
            .extend(steps.iter().copied());
    }

    fn add(&self, name: &str) -> i64 {
        let path = self.dir.file(name, b"source");
        let destination_id = self.orch.with_store(|s| Ok(first_destination(s))).unwrap();
        self.orch
            .add(&[NewItem {
                source_path: path,
                source_size: 6,
                source_mtime: 1,
                destination_id,
                send_mode: SendMode::Video,
                caption: String::new(),
            }])
            .unwrap()[0]
    }

    /// Writes one file and queues it for each `(destination, mode)` in one call, as a
    /// multi-destination batch does.
    fn add_to(&self, name: &str, targets: &[(i64, SendMode)]) -> Vec<i64> {
        let path = self.dir.file(name, b"source");
        let items: Vec<NewItem> = targets
            .iter()
            .map(|&(destination_id, send_mode)| NewItem {
                source_path: path.clone(),
                source_size: 6,
                source_mtime: 1,
                destination_id,
                send_mode,
                caption: String::new(),
            })
            .collect();
        self.orch.add(&items).unwrap()
    }

    /// The fixture chat, then new ones (peer ids from 77).
    fn chats(&self, count: i64) -> Vec<i64> {
        self.orch
            .with_store(|s| {
                let mut ids = vec![first_destination(s)];
                for peer in 77..76 + count {
                    ids.push(s.upsert_destination(&destination(peer, 0))?);
                }
                Ok(ids)
            })
            .unwrap()
    }

    fn set_interval(&self, destination_id: i64, seconds: i64) {
        let patch = crate::queue::store::DestinationPatch {
            min_interval_s: Some(seconds),
            ..Default::default()
        };
        self.orch
            .with_store(|s| s.update_destination(destination_id, &patch))
            .unwrap();
    }

    /// When the fake send was called for `id`, as offsets from the first call of the test.
    fn sent_after_start(&self, id: i64) -> Vec<Duration> {
        let first = self.send.calls.lock().unwrap()[0].1;
        self.send
            .calls_for(id)
            .into_iter()
            .map(|at| at - first)
            .collect()
    }

    fn item(&self, id: i64) -> Item {
        self.orch.with_store(|s| s.get_item(id)).unwrap()
    }

    async fn wait_state(&self, id: i64, state: State) {
        // Up to one hour of (paused) time.
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

    /// Starts the background analysis, as the app does at startup.
    fn analyze_in_background(&self) -> JoinHandle<()> {
        let runner = self.orch.clone();
        tokio::spawn(async move { runner.run_analysis().await })
    }

    fn slow_analysis(&self, seconds: u64) {
        *self.media.analysis_time.lock().unwrap() = Duration::from_secs(seconds);
    }

    async fn wait_plan(&self, id: i64) -> String {
        for _ in 0..3600 {
            if let Some(plan) = self.item(id).plan_json {
                return plan;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("item {id} was never analyzed");
    }

    /// Lets the orchestrator run for a while of paused time.
    async fn idle(&self, seconds: u64) {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

use State::*;

#[tokio::test(start_paused = true)]
async fn encodes_then_sends_and_cleans_up() {
    let h = Harness::new("orch-success", Treatment::Encode);
    let id = h.add("a.mp4");
    h.orch.start();
    h.wait_state(id, Sent).await;

    assert_eq!(
        h.sink.states_of(id),
        [Ready, Probing, Encoding, Encoded, Uploading, Sent]
    );
    let item = h.item(id);
    assert_eq!(item.tg_message_id, Some(1000));
    assert!(item.tg_sent_at.is_some());
    assert_eq!(item.attempts, 1);
    assert_eq!(item.content_id.as_deref(), Some("6:a.mp4"));
    assert!(Path::new(&item.source_path).exists(), "source kept");
    assert!(
        !Path::new(item.output_path.as_deref().unwrap()).exists(),
        "encoded file deleted once sent"
    );
    let history = h.orch.history(&HistoryFilter::default()).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, "sent");
    assert_eq!(history[0].message_id, Some(1000));
    assert!(history[0].encoded);
    let progress = h.sink.progress.lock().unwrap();
    assert!(
        progress
            .iter()
            .any(|p| p.item_id == id && p.phase == Phase::Encoding)
    );
}

#[tokio::test(start_paused = true)]
async fn skip_sends_the_source_and_keeps_it() {
    let h = Harness::new("orch-skip", Treatment::Skip);
    let id = h.add("a.mp4");
    h.orch.start();
    h.wait_state(id, Sent).await;
    assert_eq!(
        h.sink.states_of(id),
        [Ready, Probing, Encoded, Uploading, Sent]
    );
    let item = h.item(id);
    assert_eq!(item.output_path.as_deref(), Some(item.source_path.as_str()));
    assert!(Path::new(&item.source_path).exists());
    assert_eq!(h.media.encodes.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn a_watched_file_replaced_after_its_copy_is_sent_as_approved() {
    let h = Harness::new("orch-frozen", Treatment::Skip);
    let path = h.dir.file("a.mp4", b"source");
    let destination_id = h.orch.with_store(|s| Ok(first_destination(s))).unwrap();
    let root = h.dir.path().join("frozen");
    let id = h
        .orch
        .add_watched(
            &NewItem {
                source_path: path.clone(),
                source_size: 6,
                source_mtime: 1,
                destination_id,
                send_mode: SendMode::Video,
                caption: String::new(),
            },
            &root,
        )
        .unwrap()
        .unwrap();
    let frozen = h.item(id).frozen_path.unwrap();
    assert_eq!(std::fs::read(&frozen).unwrap(), b"source");

    // Replaced by a file of the same size while it waited for approval.
    std::fs::write(&path, b"CHANGE").unwrap();
    h.orch.promote(&[id]).unwrap();
    h.orch.start();
    h.wait_state(id, Sent).await;

    assert_eq!(
        h.send.sent.lock().unwrap().as_slice(),
        [(id, b"source".to_vec())]
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"CHANGE",
        "original untouched"
    );
    assert!(!Path::new(&frozen).exists(), "copy deleted once sent");
}

#[tokio::test(start_paused = true)]
async fn a_watched_file_that_cannot_be_copied_is_not_queued() {
    let h = Harness::new("orch-frozen-missing", Treatment::Skip);
    let destination_id = h.orch.with_store(|s| Ok(first_destination(s))).unwrap();
    let missing = h.dir.path().join("gone.mp4").to_string_lossy().into_owned();
    let result = h.orch.add_watched(
        &NewItem {
            source_path: missing,
            source_size: 6,
            source_mtime: 1,
            destination_id,
            send_mode: SendMode::Video,
            caption: String::new(),
        },
        &h.dir.path().join("frozen"),
    );
    assert!(result.is_err());
    assert!(h.orch.list().unwrap().is_empty(), "nothing left to approve");
}

#[tokio::test(start_paused = true)]
async fn remux_goes_through_remuxing() {
    let h = Harness::new("orch-remux", Treatment::Remux);
    let id = h.add("a.mkv");
    h.orch.start();
    h.wait_state(id, Sent).await;
    assert_eq!(
        h.sink.states_of(id),
        [Ready, Probing, Remuxing, Encoded, Uploading, Sent]
    );
}

#[tokio::test(start_paused = true)]
async fn failed_and_unknown_are_never_sent_again_automatically() {
    let h = Harness::new("orch-failures", Treatment::Encode);
    h.send_steps(&[SendStep::Fail, SendStep::Unknown]);
    let failed = h.add("a.mp4");
    let unknown = h.add("b.mp4");
    let sent = h.add("c.mp4");
    h.orch.start();
    h.wait_state(sent, Sent).await;
    h.idle(600).await;

    let item = h.item(failed);
    assert_eq!(item.state, Failed);
    assert_eq!(item.last_error.as_deref(), Some("TG_UPLOAD_FAILED"));
    assert!(
        Path::new(item.output_path.as_deref().unwrap()).exists(),
        "encoded file kept for a manual retry (F15)"
    );
    assert_eq!(h.item(unknown).state, Unknown);
    assert_eq!(h.send.calls_for(failed).len(), 1);
    assert_eq!(h.send.calls_for(unknown).len(), 1);

    let statuses: Vec<String> = h
        .orch
        .history(&HistoryFilter::default())
        .unwrap()
        .into_iter()
        .map(|entry| entry.status)
        .collect();
    assert_eq!(statuses.len(), 3);
    for status in ["sent", "failed", "unknown"] {
        assert!(statuses.iter().any(|s| s == status), "{status}");
    }
}

#[tokio::test(start_paused = true)]
async fn manual_retry_reuses_the_encoded_file() {
    let h = Harness::new("orch-retry", Treatment::Encode);
    h.send_steps(&[SendStep::Fail]);
    let id = h.add("a.mp4");
    h.orch.start();
    h.wait_state(id, Failed).await;
    assert_eq!(h.orch.retry(id).unwrap(), Encoded);
    h.wait_state(id, Sent).await;
    assert_eq!(
        h.media.encodes.load(Ordering::SeqCst),
        1,
        "no second encode"
    );
    assert_eq!(h.send.calls_for(id).len(), 2);
    assert_eq!(h.item(id).attempts, 2);
}

#[tokio::test(start_paused = true)]
async fn media_failure_marks_failed_and_removes_the_temp_file() {
    let h = Harness::new("orch-media-fail", Treatment::Encode);
    h.encode_steps(&[MediaStep::Fail]);
    let id = h.add("a.mp4");
    h.orch.start();
    h.wait_state(id, Failed).await;
    let item = h.item(id);
    assert_eq!(item.last_error.as_deref(), Some("FFMPEG_FAILED"));
    assert_eq!(item.temp_path, None);
    assert!(!Path::new(&format!("{}.part", sq_path(&item.source_path))).exists());
    assert!(h.send.order().is_empty());
    // The stage's params reach the alert, for a message that names the encoder.
    let alerts = h.sink.alerts.lock().unwrap();
    let alert = alerts.iter().find(|a| a.code == "FFMPEG_FAILED").unwrap();
    assert_eq!(alert.item_id, Some(id));
    assert_eq!(
        alert.params,
        json!({ "encoder": "h264_nvenc", "exitCode": 1, "atSeconds": 4 })
    );
}

#[tokio::test(start_paused = true)]
async fn flood_wait_waits_then_resumes() {
    let h = Harness::new("orch-flood", Treatment::Encode);
    h.send_steps(&[SendStep::FloodWait(30), SendStep::Sent]);
    let id = h.add("a.mp4");
    h.orch.start();
    h.wait_state(id, WaitingServer).await;
    let waiting = h.item(id);
    assert!(waiting.next_run_at.is_some());
    assert_eq!(waiting.last_error.as_deref(), Some("TG_FLOOD_WAIT"));

    h.wait_state(id, Sent).await;
    assert_eq!(
        h.sink.states_of(id),
        [
            Ready,
            Probing,
            Encoding,
            Encoded,
            Uploading,
            WaitingServer,
            Uploading,
            Sent
        ]
    );
    let calls = h.send.calls_for(id);
    assert_eq!(calls.len(), 2);
    assert!(
        calls[1] - calls[0] >= Duration::from_secs(29),
        "{:?}",
        calls[1] - calls[0]
    );
    let item = h.item(id);
    assert_eq!(item.attempts, 2);
    assert_eq!(item.next_run_at, None);
    let progress = h.sink.progress.lock().unwrap();
    let wait = progress
        .iter()
        .find(|p| p.phase == Phase::WaitingServer)
        .unwrap();
    assert_eq!(wait.retry_after_s, Some(30));
}

#[tokio::test(start_paused = true)]
async fn pause_during_a_flood_wait_puts_the_item_back_and_resuming_honours_the_wait() {
    let h = Harness::new("orch-pause-flood", Treatment::Encode);
    h.send_steps(&[SendStep::FloodWait(7200), SendStep::Sent]);
    let id = h.add("a.mp4");
    h.orch.start();
    h.wait_state(id, WaitingServer).await;
    let waiting = h.item(id);
    let due = waiting.next_run_at.clone().unwrap();

    h.orch.pause();
    h.wait_state(id, Encoded).await;
    let parked = h.item(id);
    assert_eq!(parked.next_run_at.as_deref(), Some(due.as_str()));
    assert_eq!(parked.last_error.as_deref(), Some("TG_FLOOD_WAIT"));
    assert_eq!(parked.progress_phase, None);

    // Two hours pass while paused: nothing is sent.
    h.idle(7300).await;
    assert_eq!(h.item(id).state, Encoded);
    assert_eq!(h.send.calls_for(id).len(), 1);

    // Resumed after the wait is over: the upload goes at once.
    h.orch.start();
    h.wait_state(id, Sent).await;
    assert_eq!(h.send.calls_for(id).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn resuming_after_a_pause_in_a_flood_wait_waits_for_what_remains() {
    let h = Harness::new("orch-pause-flood-remaining", Treatment::Encode);
    h.send_steps(&[SendStep::FloodWait(600), SendStep::Sent]);
    let id = h.add("a.mp4");
    h.orch.start();
    h.wait_state(id, WaitingServer).await;
    h.orch.pause();
    h.wait_state(id, Encoded).await;

    h.idle(100).await;
    h.orch.start();
    h.idle(300).await;
    assert_eq!(h.item(id).state, Encoded, "400 s into a 600 s wait");
    assert_eq!(h.send.calls_for(id).len(), 1);

    h.wait_state(id, Sent).await;
    let calls = h.send.calls_for(id);
    assert!(
        calls[1] - calls[0] >= Duration::from_secs(599),
        "{:?}",
        calls[1] - calls[0]
    );
}

#[tokio::test(start_paused = true)]
async fn pause_requested_during_the_upload_that_ends_in_a_flood_wait() {
    let h = Harness::new("orch-pause-then-flood", Treatment::Encode);
    h.send_steps(&[SendStep::FloodWait(3600), SendStep::Sent]);
    let id = h.add("a.mp4");
    h.orch.start();
    h.send.entered.notified().await;
    // Paused while the upload runs; it then ends in a FLOOD_WAIT.
    h.orch.pause();
    h.wait_state(id, Encoded).await;
    assert_eq!(h.send.calls_for(id).len(), 1);
    assert!(h.item(id).next_run_at.is_some());
    h.idle(4000).await;
    assert_eq!(h.send.calls_for(id).len(), 1, "paused: nothing is sent");
}

#[tokio::test(start_paused = true)]
async fn cancel_during_encoding() {
    let h = Harness::new("orch-cancel-encode", Treatment::Encode);
    h.encode_steps(&[MediaStep::WaitCancel]);
    let id = h.add("a.mp4");
    let next = h.add("b.mp4");
    h.orch.start();
    h.media.entered.notified().await;
    let temp = h.item(id).temp_path.unwrap();
    assert!(Path::new(&temp).exists());

    h.orch.cancel(id).unwrap();
    h.wait_state(id, Cancelled).await;
    assert!(!Path::new(&temp).exists(), "temp file deleted");
    assert!(Path::new(&h.item(id).source_path).exists());
    h.wait_state(next, Sent).await;
    assert_eq!(h.send.order(), [next]);
}

#[tokio::test(start_paused = true)]
async fn cancel_during_uploading() {
    let h = Harness::new("orch-cancel-upload", Treatment::Encode);
    h.send_steps(&[SendStep::WaitCancel]);
    let id = h.add("a.mp4");
    h.orch.start();
    h.send.entered.notified().await;
    assert_eq!(h.item(id).state, Uploading);
    h.orch.cancel(id).unwrap();
    h.wait_state(id, Cancelled).await;
    let item = h.item(id);
    assert!(!Path::new(item.output_path.as_deref().unwrap()).exists());
    assert_eq!(item.tg_message_id, None);
}

#[tokio::test(start_paused = true)]
async fn cancel_during_waiting_server() {
    let h = Harness::new("orch-cancel-wait", Treatment::Encode);
    h.send_steps(&[SendStep::FloodWait(600)]);
    let id = h.add("a.mp4");
    h.orch.start();
    h.wait_state(id, WaitingServer).await;
    h.orch.cancel(id).unwrap();
    h.wait_state(id, Cancelled).await;
    assert_eq!(h.send.calls_for(id).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn cancel_of_a_waiting_item_is_immediate() {
    let h = Harness::new("orch-cancel-ready", Treatment::Encode);
    let id = h.add("a.mp4");
    h.orch.cancel(id).unwrap();
    assert_eq!(h.item(id).state, Cancelled);
    h.orch.start();
    h.idle(10).await;
    assert!(h.send.order().is_empty());
}

#[tokio::test(start_paused = true)]
async fn pause_takes_effect_after_the_current_item() {
    let h = Harness::new("orch-pause", Treatment::Encode);
    h.send_steps(&[SendStep::Gate]);
    let first = h.add("a.mp4");
    let second = h.add("b.mp4");
    let third = h.add("c.mp4");
    h.orch.start();
    h.send.entered.notified().await;
    h.orch.pause();
    assert!(!h.orch.is_running());
    h.send.gate.notify_one();
    h.wait_state(first, Sent).await;
    h.idle(600).await;
    assert_eq!(h.item(second).state, Ready);
    assert_eq!(h.item(third).state, Ready);

    h.orch.start();
    h.wait_state(third, Sent).await;
    assert_eq!(h.send.order(), [first, second, third]);
}

#[tokio::test(start_paused = true)]
async fn one_item_at_a_time_in_position_order() {
    let h = Harness::new("orch-sequential", Treatment::Encode);
    let ids: Vec<i64> = ["a", "b", "c", "d"]
        .iter()
        .map(|name| h.add(&format!("{name}.mp4")))
        .collect();
    h.orch.reorder(&[ids[3], ids[1]]).unwrap();
    h.orch.start();
    for &id in &ids {
        h.wait_state(id, Sent).await;
    }
    assert_eq!(h.concurrency.max.load(Ordering::SeqCst), 1);
    assert_eq!(h.send.order(), [ids[3], ids[1], ids[0], ids[2]]);
}

#[tokio::test(start_paused = true)]
async fn items_added_while_running_are_picked_up() {
    let h = Harness::new("orch-late-add", Treatment::Encode);
    h.orch.start();
    h.idle(5).await;
    let id = h.add("a.mp4");
    h.wait_state(id, Sent).await;
}

#[tokio::test(start_paused = true)]
async fn pause_between_sends_is_respected() {
    let h = Harness::new("orch-send-pause", Treatment::Encode);
    h.orch.set_pauses(PauseRange {
        min: Duration::from_secs(5),
        max: Duration::from_secs(8),
    });
    let first = h.add("a.mp4");
    let second = h.add("b.mp4");
    h.orch.start();
    h.wait_state(second, Sent).await;
    let gap = h.send.calls_for(second)[0] - h.send.calls_for(first)[0];
    assert!(gap >= Duration::from_secs(5), "{gap:?}");
    assert!(gap <= Duration::from_secs(9), "{gap:?}");
}

#[tokio::test(start_paused = true)]
async fn duplicate_is_held_until_promoted() {
    let h = Harness::new("orch-duplicate", Treatment::Encode);
    let id = h.add("a.mp4");
    // Same content already sent to the same destination.
    let mut previous = h.item(id);
    previous.content_id = Some("6:a.mp4".into());
    h.orch
        .with_store(|s| s.record_history(&previous, HistoryStatus::Sent))
        .unwrap();

    h.orch.start();
    h.wait_state(id, PendingReview).await;
    let held = h.item(id);
    assert_eq!(held.hold_reason.as_deref(), Some(HOLD_DUPLICATE));
    assert!(h.send.order().is_empty());

    h.orch.promote(&[id]).unwrap();
    h.wait_state(id, Sent).await;
    assert_eq!(h.send.order(), [id]);
}

#[tokio::test(start_paused = true)]
async fn clean_shutdown_during_upload_leaves_unknown() {
    let h = Harness::new("orch-shutdown", Treatment::Encode);
    h.send_steps(&[SendStep::Gate]);
    let id = h.add("a.mp4");
    h.orch.start();
    h.send.entered.notified().await;
    h.orch.shutdown().unwrap();
    assert_eq!(h.item(id).state, Unknown);
    h.idle(1).await;
    assert!(h.task.is_finished());
}

#[tokio::test(start_paused = true)]
async fn queue_retry_accepts_only_failed_items() {
    let h = Harness::new("orch-retry-failed", Treatment::Encode);
    h.send_steps(&[SendStep::Fail, SendStep::Unknown]);
    let failed = h.add("a.mp4");
    let unknown = h.add("b.mp4");
    h.orch.start();
    h.wait_state(unknown, Unknown).await;
    assert_eq!(h.item(failed).state, Failed);

    assert!(matches!(
        h.orch.retry_failed(unknown),
        Err(QueueError::IllegalTransition { from: Unknown, .. })
    ));
    assert!(matches!(
        h.orch.retry_failed(999),
        Err(QueueError::ItemNotFound(999))
    ));
    assert_eq!(h.orch.retry_failed(failed).unwrap(), Encoded);
    h.wait_state(failed, Sent).await;
    assert!(matches!(
        h.orch.retry_failed(failed),
        Err(QueueError::IllegalTransition { from: Sent, .. })
    ));
    assert_eq!(h.item(unknown).state, Unknown, "untouched");
}

#[tokio::test(start_paused = true)]
async fn resolve_unknown_marks_sent_or_retries() {
    let h = Harness::new("orch-resolve", Treatment::Encode);
    h.send_steps(&[SendStep::Unknown, SendStep::Unknown, SendStep::Fail]);
    let marked = h.add("a.mp4");
    let retried = h.add("b.mp4");
    let failed = h.add("c.mp4");
    h.orch.start();
    h.wait_state(failed, Failed).await;
    let encoded = h.item(marked).output_path.unwrap();
    assert!(Path::new(&encoded).exists());

    assert!(matches!(
        h.orch
            .resolve_unknown(marked, ResolveAction::MarkSent, Some(4242)),
        Err(QueueError::ReconcileNotProposed)
    ));
    assert_eq!(h.item(marked).state, Unknown);
    assert!(
        Path::new(&encoded).exists(),
        "refusal keeps the encoded file"
    );
    h.orch
        .with_store(|store| {
            store.remember_reconciliation_candidates(&store.get_item(marked)?, vec![4242])
        })
        .unwrap();
    assert_eq!(
        h.orch
            .resolve_unknown(marked, ResolveAction::MarkSent, Some(4242))
            .unwrap(),
        Sent
    );
    let item = h.item(marked);
    assert_eq!(item.tg_message_id, Some(4242));
    assert!(
        !Path::new(&encoded).exists(),
        "encoded file deleted once sent"
    );
    let sent = h
        .orch
        .history(&HistoryFilter {
            status: Some(HistoryStatus::Sent),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].message_id, Some(4242));

    // Only an unknown item can be resolved.
    for action in [ResolveAction::MarkSent, ResolveAction::Retry] {
        assert!(matches!(
            h.orch.resolve_unknown(failed, action, None),
            Err(QueueError::IllegalTransition { from: Failed, .. })
        ));
        assert!(h.orch.resolve_unknown(marked, action, None).is_err());
    }

    let calls_before = h.send.calls_for(retried).len();
    assert_eq!(
        h.orch
            .resolve_unknown(retried, ResolveAction::Retry, None)
            .unwrap(),
        Encoded
    );
    h.wait_state(retried, Sent).await;
    assert_eq!(h.send.calls_for(retried).len(), calls_before + 1);
    assert_eq!(h.media.encodes.load(Ordering::SeqCst), 3, "no new encode");
}

#[test]
fn resolve_action_uses_snake_case() {
    let action: ResolveAction = serde_json::from_str("\"mark_sent\"").unwrap();
    assert_eq!(action, ResolveAction::MarkSent);
    let action: ResolveAction = serde_json::from_str("\"retry\"").unwrap();
    assert_eq!(action, ResolveAction::Retry);
    assert!(serde_json::from_str::<ResolveAction>("\"MarkSent\"").is_err());
}

#[tokio::test(start_paused = true)]
async fn status_is_emitted_on_change_only() {
    let h = Harness::new("orch-status", Treatment::Encode);
    assert!(!h.orch.status().running);
    h.orch.pause();
    h.orch.start();
    h.orch.start();
    assert!(h.orch.status().running);
    h.orch.pause();
    let events: Vec<bool> = h
        .sink
        .status
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.running)
        .collect();
    assert_eq!(events, [true, false]);
    assert_eq!(
        serde_json::to_value(h.orch.status()).unwrap(),
        serde_json::json!({ "running": false })
    );
}

#[tokio::test(start_paused = true)]
async fn failures_raise_an_alert_with_the_item() {
    let h = Harness::new("orch-alerts", Treatment::Encode);
    h.encode_steps(&[MediaStep::Fail]);
    h.send_steps(&[SendStep::FloodWait(30), SendStep::Unknown]);
    let media = h.add("a.mp4");
    let send = h.add("b.mp4");
    h.orch.start();
    h.wait_state(send, Unknown).await;
    assert_eq!(
        h.sink.alert_codes(),
        [
            (Some(media), "FFMPEG_FAILED".to_owned()),
            (Some(send), "TG_FLOOD_WAIT".to_owned()),
            (Some(send), "TG_TIMEOUT".to_owned()),
        ]
    );
    let alerts = h.sink.alerts.lock().unwrap();
    assert_eq!(alerts[1].params, serde_json::json!({ "retry_after": 30 }));
}

#[tokio::test(start_paused = true)]
async fn clear_keeps_the_running_item_and_announces_the_removals() {
    let h = Harness::new("orch-clear", Treatment::Encode);
    h.send_steps(&[SendStep::Gate]);
    let running = h.add("a.mp4");
    let waiting = h.add("b.mp4");
    h.orch.start();
    h.send.entered.notified().await;
    assert_eq!(h.item(running).state, Uploading);
    let waiting_source = h.item(waiting).source_path;

    let outcome = h.orch.clear(ClearScope::All).unwrap();
    assert_eq!(outcome.removed, [waiting]);
    assert_eq!(outcome.running, 1);
    assert_eq!(h.item(running).state, Uploading);
    assert!(h.orch.with_store(|s| s.get_item(waiting)).is_err());
    assert!(Path::new(&waiting_source).exists(), "the source is kept");
    assert_eq!(*h.sink.removed.lock().unwrap(), [waiting]);

    // The upload that was running finishes normally, then it is a finished item.
    h.send.gate.notify_one();
    h.wait_state(running, Sent).await;
    let events = *h.sink.changed_events.lock().unwrap();
    let outcome = h.orch.clear(ClearScope::Finished).unwrap();
    assert_eq!(outcome.removed, [running]);
    assert_eq!(outcome.running, 0);
    assert_eq!(*h.sink.removed.lock().unwrap(), [waiting, running]);
    assert_eq!(*h.sink.changed_events.lock().unwrap(), events + 1);
    // Nothing to remove: no event.
    assert!(h.orch.clear(ClearScope::All).unwrap().removed.is_empty());
    assert_eq!(*h.sink.changed_events.lock().unwrap(), events + 1);
    assert!(h.orch.list().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn clear_removes_the_encoded_file_of_a_failed_item_but_not_its_source() {
    let h = Harness::new("orch-clear-files", Treatment::Encode);
    h.send_steps(&[SendStep::Fail]);
    let id = h.add("a.mp4");
    h.orch.start();
    h.wait_state(id, Failed).await;
    let item = h.item(id);
    let output = item.output_path.unwrap();
    assert!(Path::new(&output).exists(), "kept for a retry (F15)");

    let outcome = h.orch.clear(ClearScope::Finished).unwrap();
    assert_eq!(outcome.removed, [id]);
    assert!(!Path::new(&output).exists());
    assert!(Path::new(&item.source_path).exists());
}

#[tokio::test(start_paused = true)]
async fn clearing_the_history_lifts_the_duplicate_hold() {
    let h = Harness::new("orch-clear-history", Treatment::Encode);
    let first = h.add("a.mp4");
    h.orch.start();
    h.wait_state(first, Sent).await;

    let held = h.add("a.mp4");
    h.wait_state(held, PendingReview).await;
    assert_eq!(h.item(held).hold_reason.as_deref(), Some(HOLD_DUPLICATE));

    // Clearing the queue alone keeps the memory of the send.
    h.orch.clear(ClearScope::All).unwrap();
    assert_eq!(h.orch.history(&HistoryFilter::default()).unwrap().len(), 1);
    let again = h.add("a.mp4");
    h.wait_state(again, PendingReview).await;
    h.orch.clear(ClearScope::All).unwrap();

    assert_eq!(h.orch.clear_history().unwrap(), 1);
    assert!(
        h.orch
            .history(&HistoryFilter::default())
            .unwrap()
            .is_empty()
    );
    // Ids restart from 1 in an emptied table: count the sends, not the id.
    let sends = h.send.order().len();
    let free = h.add("a.mp4");
    h.wait_state(free, Sent).await;
    assert_eq!(h.send.order().len(), sends + 1);
}

#[tokio::test(start_paused = true)]
async fn one_encode_serves_every_destination_of_a_file() {
    let h = Harness::new("orch-multi", Treatment::Encode);
    h.send_steps(&[SendStep::Sent, SendStep::Gate]);
    let chats = h.chats(2);
    let ids = h.add_to(
        "a.mp4",
        &[(chats[0], SendMode::Video), (chats[1], SendMode::Video)],
    );
    h.orch.start();
    h.wait_state(ids[0], Sent).await;
    h.wait_state(ids[1], Uploading).await;
    let output = h.item(ids[0]).output_path.unwrap();
    assert_eq!(h.item(ids[1]).output_path.as_deref(), Some(output.as_str()));
    assert!(
        Path::new(&output).exists(),
        "kept for the second destination"
    );

    h.send.gate.notify_one();
    h.wait_state(ids[1], Sent).await;
    assert_eq!(h.media.encodes.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.sink.states_of(ids[1]),
        [Ready, Probing, Encoded, Uploading, Sent]
    );
    assert!(!Path::new(&output).exists(), "deleted after its last send");
    assert!(Path::new(&h.item(ids[0]).source_path).exists());
    let history = h.orch.history(&HistoryFilter::default()).unwrap();
    let mut sent: Vec<(i64, &str, bool)> = history
        .iter()
        .map(|entry| (entry.destination_id, entry.status.as_str(), entry.encoded))
        .collect();
    sent.sort();
    assert_eq!(sent, [(chats[0], "sent", true), (chats[1], "sent", true)]);
}

#[tokio::test(start_paused = true)]
async fn the_shared_file_stays_until_its_last_destination_is_done() {
    let h = Harness::new("orch-multi-lifetime", Treatment::Encode);
    h.send_steps(&[SendStep::Fail, SendStep::Sent, SendStep::Sent]);
    let chats = h.chats(3);
    let ids = h.add_to(
        "a.mp4",
        &chats
            .iter()
            .map(|&chat| (chat, SendMode::Video))
            .collect::<Vec<_>>(),
    );
    h.orch.start();
    h.wait_state(ids[2], Sent).await;
    assert_eq!(h.item(ids[0]).state, Failed);
    assert_eq!(h.item(ids[1]).state, Sent);
    assert_eq!(h.media.encodes.load(Ordering::SeqCst), 1);
    let output = h.item(ids[0]).output_path.unwrap();
    assert!(
        Path::new(&output).exists(),
        "kept for the failed item's retry"
    );

    h.orch.cancel(ids[0]).unwrap();
    assert!(!Path::new(&output).exists(), "no item uses it any more");
}

#[tokio::test(start_paused = true)]
async fn another_mode_is_encoded_on_its_own() {
    let h = Harness::new("orch-multi-modes", Treatment::Encode);
    let chats = h.chats(2);
    let ids = h.add_to(
        "a.mp4",
        &[(chats[0], SendMode::Video), (chats[1], SendMode::Document)],
    );
    h.orch.start();
    h.wait_state(ids[1], Sent).await;
    assert_eq!(h.item(ids[0]).state, Sent);
    assert_eq!(h.media.encodes.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.sink.states_of(ids[1]),
        [Ready, Probing, Encoding, Encoded, Uploading, Sent]
    );
    assert!(!Path::new(&h.item(ids[1]).output_path.unwrap()).exists());
}

#[tokio::test(start_paused = true)]
async fn a_file_already_sent_to_one_destination_is_new_to_another() {
    let h = Harness::new("orch-multi-duplicate", Treatment::Encode);
    let chats = h.chats(2);
    let ids = h.add_to(
        "a.mp4",
        &[(chats[0], SendMode::Video), (chats[1], SendMode::Video)],
    );
    // Same content already sent to the first destination only.
    let mut previous = h.item(ids[0]);
    previous.content_id = Some("6:a.mp4".into());
    h.orch
        .with_store(|s| s.record_history(&previous, HistoryStatus::Sent))
        .unwrap();

    h.orch.start();
    h.wait_state(ids[1], Sent).await;
    let held = h.item(ids[0]);
    assert_eq!(held.state, PendingReview);
    assert_eq!(held.hold_reason.as_deref(), Some(HOLD_DUPLICATE));
    assert_eq!(h.item(ids[1]).hold_reason, None);
    assert_eq!(h.send.order(), [ids[1]]);
    assert_eq!(h.media.encodes.load(Ordering::SeqCst), 1);
}

// --- analysis at drop ---

#[tokio::test(start_paused = true)]
async fn an_added_item_is_analyzed_in_the_background_without_starting_the_queue() {
    let h = Harness::new("analysis-basic", Treatment::Skip);
    let analysis = h.analyze_in_background();
    let id = h.add("a.mp4");
    let plan = h.wait_plan(id).await;
    assert_eq!(plan, json!({ "analyzed_as": "video" }).to_string());
    let item = h.item(id);
    assert_eq!(item.state, Ready, "analysis does not run the item");
    assert_eq!(item.media_json.as_deref(), Some("{\"analyzed\":1}"));
    assert!(!h.orch.is_running());
    assert_eq!(h.send.calls.lock().unwrap().len(), 0);
    // The page is told, so the badge shows without waiting for the queue.
    assert!(
        h.sink
            .changed
            .lock()
            .unwrap()
            .iter()
            .filter(|d| d.id == id)
            .count()
            >= 2
    );
    analysis.abort();
}

#[tokio::test(start_paused = true)]
async fn at_most_two_files_are_analyzed_at_once() {
    let h = Harness::new("analysis-two", Treatment::Skip);
    h.slow_analysis(3);
    let analysis = h.analyze_in_background();
    let ids: Vec<i64> = (0..6).map(|n| h.add(&format!("{n}.mp4"))).collect();
    for id in &ids {
        h.wait_plan(*id).await;
    }
    assert_eq!(h.media.analysis_concurrency.max.load(Ordering::SeqCst), 2);
    assert_eq!(h.media.analyses.lock().unwrap().len(), 6);
    analysis.abort();
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_or_cleared_item_drops_its_analysis() {
    let h = Harness::new("analysis-gone", Treatment::Skip);
    h.slow_analysis(60);
    let analysis = h.analyze_in_background();
    let cancelled = h.add("a.mp4");
    let cleared = h.add("b.mp4");
    let queued = h.add("c.mp4");
    h.idle(1).await;
    assert_eq!(
        h.media.analyses.lock().unwrap().len(),
        2,
        "two under way, one waiting"
    );

    h.orch.cancel(cancelled).unwrap();
    h.orch.clear(ClearScope::All).unwrap();
    h.idle(120).await;

    assert_eq!(h.media.analyses_cancelled.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.media.analyses.lock().unwrap().len(),
        2,
        "the waiting one was forgotten, not analyzed"
    );
    let rows = h.orch.list().unwrap();
    assert!(rows.iter().all(|item| item.plan_json.is_none()));
    let _ = (cleared, queued);
    analysis.abort();
}

/// The result of an analysis that began before the user changed the mode is never shown: the
/// item is analyzed again as what it is now.
#[tokio::test(start_paused = true)]
async fn a_change_during_the_analysis_makes_it_start_over() {
    let h = Harness::new("analysis-change", Treatment::Skip);
    h.slow_analysis(10);
    let analysis = h.analyze_in_background();
    let id = h.add("a.mp4");
    h.idle(2).await;
    h.orch
        .update_item(
            id,
            &ItemPatch {
                send_mode: Some(SendMode::Document),
                ..Default::default()
            },
        )
        .unwrap();
    let plan = h.wait_plan(id).await;
    assert_eq!(plan, json!({ "analyzed_as": "document" }).to_string());
    assert_eq!(
        h.media.analyses.lock().unwrap().as_slice(),
        [(id, SendMode::Video), (id, SendMode::Document)]
    );
    assert_eq!(h.media.analyses_cancelled.load(Ordering::SeqCst), 1);
    analysis.abort();
}

/// An item whose turn comes while it is still being analyzed is probed by the queue as usual
/// and keeps the plan of that probing.
#[tokio::test(start_paused = true)]
async fn an_item_that_starts_stops_its_analysis_and_keeps_its_probed_plan() {
    let h = Harness::new("analysis-starts", Treatment::Skip);
    h.slow_analysis(600);
    let analysis = h.analyze_in_background();
    let id = h.add("a.mp4");
    h.idle(1).await;
    h.orch.start();
    h.wait_state(id, Sent).await;
    assert_eq!(h.media.analyses_cancelled.load(Ordering::SeqCst), 1);
    let item = h.item(id);
    assert_eq!(item.plan_json.as_deref(), Some("{\"mode\":\"video\"}"));
    assert_eq!(item.media_json.as_deref(), Some("{}"));
    analysis.abort();
}

/// Items left without a badge by a previous run are analyzed when the app starts.
#[tokio::test(start_paused = true)]
async fn items_queued_before_the_start_are_analyzed_at_startup() {
    let h = Harness::new("analysis-startup", Treatment::Skip);
    let path = h.dir.file("old.mp4", b"source");
    let destination_id = h.orch.with_store(|s| Ok(first_destination(s))).unwrap();
    let ids = h
        .orch
        .with_store(|s| {
            s.add_items(&[NewItem {
                source_path: path,
                source_size: 6,
                source_mtime: 1,
                destination_id,
                send_mode: SendMode::Video,
                caption: String::new(),
            }])
        })
        .unwrap();
    let analysis = h.analyze_in_background();
    h.wait_plan(ids[0]).await;
    analysis.abort();
}

/// New limit or encoding settings: the badges of the waiting files are worked out again.
#[tokio::test(start_paused = true)]
async fn waiting_items_are_analyzed_again_when_the_settings_change() {
    let h = Harness::new("analysis-settings", Treatment::Skip);
    let analysis = h.analyze_in_background();
    let id = h.add("a.mp4");
    h.wait_plan(id).await;
    assert_eq!(h.media.analyses.lock().unwrap().len(), 1);
    h.orch.reanalyze_waiting().unwrap();
    h.idle(1).await;
    assert_eq!(h.media.analyses.lock().unwrap().len(), 2);
    assert!(h.item(id).plan_json.is_some());
    analysis.abort();
}

// --- minimum interval per destination (F2) ---

#[tokio::test(start_paused = true)]
async fn the_interval_reschedules_and_other_destinations_go_meanwhile() {
    let h = Harness::new("orch-interval", Treatment::Encode);
    let chats = h.chats(2);
    let (a, b) = (chats[0], chats[1]);
    h.set_interval(a, 60);
    let a1 = h.add_to("a1.mp4", &[(a, SendMode::Video)])[0];
    let a2 = h.add_to("a2.mp4", &[(a, SendMode::Video)])[0];
    let b1 = h.add_to("b1.mp4", &[(b, SendMode::Video)])[0];
    h.orch.start();

    h.wait_state(b1, Sent).await;
    // a2 was encoded, found the interval running and went back to the schedule: b1, behind
    // it in the queue, went first.
    assert_eq!(h.send.order(), [a1, b1]);
    let waiting = h.item(a2);
    assert_eq!(waiting.state, Encoded);
    assert!(Path::new(waiting.output_path.as_deref().unwrap()).exists());
    let due = parse_iso(waiting.next_run_at.as_deref().unwrap()).unwrap();
    let a1_sent = parse_iso(h.item(a1).tg_sent_at.as_deref().unwrap()).unwrap();
    assert_eq!(due - a1_sent, chrono::Duration::seconds(60));

    h.wait_state(a2, Sent).await;
    assert_eq!(h.send.order(), [a1, b1, a2]);
    let gap = h.sent_after_start(a2)[0] - h.sent_after_start(a1)[0];
    assert!(
        (Duration::from_secs(60)..Duration::from_secs(62)).contains(&gap),
        "{gap:?}"
    );
    assert_eq!(h.item(a2).next_run_at, None, "cleared by the upload");
    // Never uploaded twice, never two stages at once.
    assert_eq!(h.send.calls_for(a2).len(), 1);
    assert_eq!(h.concurrency.max.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn the_interval_counts_from_a_send_of_an_earlier_session() {
    let h = Harness::new("orch-interval-history", Treatment::Skip);
    let chat = h.chats(1)[0];
    h.set_interval(chat, 120);
    // Sent 30 s ago, before this session: only the history knows.
    let earlier = h.add("earlier.mp4");
    let mut sent = h.item(earlier);
    sent.content_id = Some("6:earlier".into());
    sent.tg_sent_at = Some(iso(h.clock.now() - chrono::Duration::seconds(30)));
    h.orch
        .with_store(|s| s.record_history(&sent, HistoryStatus::Sent))
        .unwrap();
    h.orch.cancel(earlier).unwrap();

    let started = Instant::now();
    let id = h.add("a.mp4");
    h.orch.start();
    h.wait_state(id, Sent).await;
    let waited = h.send.calls_for(id)[0] - started;
    assert!(
        (Duration::from_secs(90)..Duration::from_secs(92)).contains(&waited),
        "{waited:?}"
    );
    assert_eq!(
        h.sink.states_of(id),
        [Ready, Probing, Encoded, Uploading, Sent],
        "the wait is a schedule, not a state"
    );
}

#[tokio::test(start_paused = true)]
async fn the_pause_and_a_flood_wait_still_hold_the_queue_with_an_interval() {
    let h = Harness::new("orch-interval-flood", Treatment::Skip);
    h.orch.set_pauses(PauseRange {
        min: Duration::from_secs(10),
        max: Duration::from_secs(10),
    });
    let chats = h.chats(2);
    let (a, b) = (chats[0], chats[1]);
    h.set_interval(a, 30);
    // a1 meets a FLOOD_WAIT of 45 s: the account waits, the other destination with it.
    h.send_steps(&[SendStep::FloodWait(45), SendStep::Sent]);
    let a1 = h.add_to("a1.mp4", &[(a, SendMode::Video)])[0];
    let b1 = h.add_to("b1.mp4", &[(b, SendMode::Video)])[0];
    let a2 = h.add_to("a2.mp4", &[(a, SendMode::Video)])[0];
    h.orch.start();
    h.wait_state(a2, Sent).await;

    assert_eq!(h.send.order(), [a1, a1, b1, a2]);
    let a1_sent = h.sent_after_start(a1)[1];
    let b1_sent = h.sent_after_start(b1)[0];
    let a2_sent = h.sent_after_start(a2)[0];
    assert!(a1_sent >= Duration::from_secs(45), "{a1_sent:?}");
    // The pause follows a1's send; the interval counts from that send, not from its first try.
    assert!(b1_sent - a1_sent >= Duration::from_secs(10), "{b1_sent:?}");
    let gap = a2_sent - a1_sent;
    assert!(
        (Duration::from_secs(30)..Duration::from_secs(32)).contains(&gap),
        "{gap:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_rescheduled_item_can_be_cancelled_and_holds_nothing_back() {
    let h = Harness::new("orch-interval-cancel", Treatment::Skip);
    let chats = h.chats(2);
    h.set_interval(chats[0], 3600);
    let first = h.add("a.mp4");
    let second = h.add("b.mp4");
    h.orch.start();
    h.wait_state(first, Sent).await;
    h.idle(5).await;
    assert_eq!(h.item(second).state, Encoded);
    assert!(h.item(second).next_run_at.is_some());

    h.orch.cancel(second).unwrap();
    assert_eq!(h.item(second).state, Cancelled);
    let next = h.add_to("c.mp4", &[(chats[1], SendMode::Video)])[0];
    h.wait_state(next, Sent).await;
    assert_eq!(h.send.order(), [first, next]);
}

#[tokio::test(start_paused = true)]
async fn a_new_interval_applies_to_the_items_already_waiting() {
    let h = Harness::new("orch-interval-change", Treatment::Skip);
    let chat = h.chats(1)[0];
    // A week by mistake…
    h.set_interval(chat, 7 * 24 * 3600);
    let first = h.add("a.mp4");
    let second = h.add("b.mp4");
    h.orch.start();
    h.wait_state(first, Sent).await;
    h.idle(5).await;
    assert!(h.item(second).next_run_at.is_some());

    // …then 30 s: the waiting item follows the new value, not the week.
    let patch = crate::queue::store::DestinationPatch {
        min_interval_s: Some(30),
        ..Default::default()
    };
    let saved = h.orch.update_destination(chat, &patch).unwrap();
    assert_eq!(saved.min_interval_s, 30);
    h.wait_state(second, Sent).await;
    let gap = h.sent_after_start(second)[0] - h.sent_after_start(first)[0];
    assert!(
        (Duration::from_secs(30)..Duration::from_secs(32)).contains(&gap),
        "{gap:?}"
    );

    // Back to none: the next item goes at once.
    h.set_interval(chat, 3600);
    let third = h.add("c.mp4");
    h.idle(5).await;
    assert!(h.item(third).next_run_at.is_some());
    let none = crate::queue::store::DestinationPatch {
        min_interval_s: Some(0),
        ..Default::default()
    };
    h.orch.update_destination(chat, &none).unwrap();
    h.wait_state(third, Sent).await;
    let gap = h.sent_after_start(third)[0] - h.sent_after_start(second)[0];
    assert!(gap < Duration::from_secs(10), "{gap:?}");
}

#[test]
fn a_new_interval_leaves_a_flood_wait_schedule_alone() {
    let store = memory_store();
    let chat = first_destination(&store);
    let orch = Orchestrator::new(
        store,
        Arc::new(Unavailable),
        Arc::new(Unavailable),
        Arc::new(Recorder::default()),
        PauseRange {
            min: Duration::ZERO,
            max: Duration::ZERO,
        },
    );
    let path = |name: &str| format!("{name}.mp4");
    let ids = orch
        .add(&[
            NewItem {
                source_path: path("interval"),
                source_size: 1,
                source_mtime: 1,
                destination_id: chat,
                send_mode: SendMode::Video,
                caption: String::new(),
            },
            NewItem {
                source_path: path("flood"),
                source_size: 1,
                source_mtime: 1,
                destination_id: chat,
                send_mode: SendMode::Video,
                caption: String::new(),
            },
        ])
        .unwrap();
    let later = "2099-01-01T00:00:00.000Z";
    orch.with_store(|s| {
        s.conn().execute(
            "UPDATE items SET state = 'encoded', next_run_at = ?1 WHERE id IN (?2, ?3)",
            rusqlite::params![later, ids[0], ids[1]],
        )?;
        s.conn().execute(
            "UPDATE items SET last_error = 'TG_FLOOD_WAIT' WHERE id = ?1",
            [ids[1]],
        )?;
        Ok(())
    })
    .unwrap();
    let patch = crate::queue::store::DestinationPatch {
        min_interval_s: Some(0),
        ..Default::default()
    };
    orch.update_destination(chat, &patch).unwrap();
    let next_run = |id| orch.with_store(|s| s.get_item(id)).unwrap().next_run_at;
    assert_eq!(next_run(ids[0]), None);
    assert_eq!(next_run(ids[1]).as_deref(), Some(later));
    // Another setting does not touch the schedule.
    orch.with_store(|s| {
        s.conn().execute(
            "UPDATE items SET next_run_at = ?1 WHERE id = ?2",
            rusqlite::params![later, ids[0]],
        )?;
        Ok(())
    })
    .unwrap();
    let favorite = crate::queue::store::DestinationPatch {
        favorite: Some(true),
        ..Default::default()
    };
    orch.update_destination(chat, &favorite).unwrap();
    assert_eq!(next_run(ids[0]).as_deref(), Some(later));
}

#[tokio::test(start_paused = true)]
async fn a_flood_wait_recovered_at_startup_holds_the_queue_until_it_ends() {
    let h = Harness::new("orch-flood-recovered", Treatment::Skip);
    let chats = h.chats(2);
    // Another destination's item first in the queue, the recovered one behind it.
    let other = h.add_to("other.mp4", &[(chats[1], SendMode::Video)])[0];
    let flooded = h.add_to("a.mp4", &[(chats[0], SendMode::Video)])[0];
    let source = h.item(flooded).source_path;
    let until = iso(h.clock.now() + chrono::Duration::seconds(60));
    h.orch
        .with_store(|s| {
            s.conn().execute(
                "UPDATE items SET state = 'encoded', last_error = 'TG_FLOOD_WAIT',
                        next_run_at = ?1, output_path = ?2, output_size = 6,
                        content_id = '6:a.mp4'
                 WHERE id = ?3",
                rusqlite::params![until, source, flooded],
            )?;
            Ok(())
        })
        .unwrap();
    let started = Instant::now();
    h.orch.start();
    h.idle(30).await;
    assert!(
        h.send.order().is_empty(),
        "nothing goes during the server's wait"
    );
    assert_eq!(h.item(other).state, Ready, "not even prepared");

    h.wait_state(flooded, Sent).await;
    h.wait_state(other, Sent).await;
    assert_eq!(h.send.order(), [other, flooded], "then in queue order");
    let first = h.send.calls_for(other)[0] - started;
    assert!(first >= Duration::from_secs(60), "{first:?}");
}
