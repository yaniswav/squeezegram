//! Orchestrator tests on deterministic doubles, with tokio's paused clock: pauses and
//! FLOOD_WAIT delays elapse instantly.

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
    async fn upload(
        &self,
        item: &Item,
        _: &Destination,
        ctx: &StageContext,
    ) -> std::result::Result<SendOutcome, SendError> {
        let _active = self.concurrency.enter();
        self.calls.lock().unwrap().push((item.id, Instant::now()));
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

struct Harness {
    orch: Orchestrator,
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
        });
        let send = Arc::new(FakeSend {
            steps: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            entered: Notify::new(),
            gate: Notify::new(),
            next_id: AtomicI64::new(1000),
            concurrency: concurrency.clone(),
        });
        let sink = Arc::new(Recorder::default());
        let orch = Orchestrator::new(
            memory_store(),
            media.clone(),
            send.clone(),
            sink.clone(),
            PauseRange {
                min: Duration::ZERO,
                max: Duration::ZERO,
            },
        );
        let runner = orch.clone();
        let task = tokio::spawn(async move { runner.run().await });
        Self {
            orch,
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
