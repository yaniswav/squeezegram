//! Watcher tests on temporary folders. The clock is the instants handed to `scan` and `poll`:
//! "ten seconds later" is `t0 + 10 s`, without waiting.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::entitlements::EntitlementStatus;
use crate::queue::State;
use crate::queue::events::recorder::Recorder;
use crate::queue::orchestrator::{PauseRange, Unavailable};
use crate::queue::store::{HOLD_WATCH, ORIGIN_WATCH, SendMode, Store};
use crate::queue::test_util::{TestDir, destination, first_destination};

const STABLE: Duration = Duration::from_secs(10);

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn names(found: &[Found]) -> Vec<String> {
    found
        .iter()
        .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// The license verdict, switched by the test.
#[derive(Default)]
struct Switch(AtomicBool);

impl Switch {
    fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }
}

impl Entitlements for Switch {
    fn allows(&self, feature: Feature) -> bool {
        feature == Feature::WatchedFolder && self.0.load(Ordering::SeqCst)
    }

    fn status(&self) -> EntitlementStatus {
        EntitlementStatus::FREE_BUILD
    }
}

fn orchestrator(store: Store) -> Orchestrator {
    Orchestrator::new(
        store,
        Arc::new(Unavailable),
        Arc::new(Unavailable),
        Arc::new(Recorder::default()),
        PauseRange {
            min: Duration::ZERO,
            max: Duration::ZERO,
        },
    )
}

/// A database file in `dir` with one destination, reopened as a restart would.
fn open_store(dir: &TestDir) -> Store {
    let mut store = Store::open(&dir.path().join("squeezegram.db")).unwrap();
    store.upsert_destination(&destination(1, 0)).unwrap();
    store
}

fn watched_items(queue: &Orchestrator) -> Vec<String> {
    queue
        .list()
        .unwrap()
        .into_iter()
        .filter(|item| item.origin == ORIGIN_WATCH)
        .map(|item| {
            Path::new(&item.source_path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// One listing; what it found is handed to the queue (as `Engine::poll` does when it succeeds).
fn take(tracker: &mut Tracker, dir: &Path, now: Instant, stable: Duration) -> Vec<Found> {
    let found = tracker.scan(1, dir, now, stable).unwrap();
    for file in &found {
        tracker.taken(&file.name);
    }
    found
}

#[test]
fn a_file_is_taken_once_its_size_has_been_stable_long_enough() {
    let dir = TestDir::new("watch-stable");
    let mut tracker = Tracker::default();
    let t0 = Instant::now();
    assert!(take(&mut tracker, dir.path(), t0, STABLE).is_empty());

    let path = dir.path().join("clip.mp4");
    std::fs::write(&path, b"first part").unwrap();
    assert!(take(&mut tracker, dir.path(), t0 + secs(1), STABLE).is_empty());
    assert!(
        take(&mut tracker, dir.path(), t0 + secs(8), STABLE).is_empty(),
        "7 s stable is not enough"
    );

    // Still growing: the wait starts over from the poll that saw the change.
    std::fs::write(&path, b"first part, then more").unwrap();
    assert!(take(&mut tracker, dir.path(), t0 + secs(12), STABLE).is_empty());
    assert!(take(&mut tracker, dir.path(), t0 + secs(21), STABLE).is_empty());
    let found = take(&mut tracker, dir.path(), t0 + secs(22), STABLE);
    assert_eq!(names(&found), ["clip.mp4"]);
    assert_eq!(found[0].size, 21);
    assert_eq!(found[0].waited, secs(10));

    // Taken once.
    assert!(take(&mut tracker, dir.path(), t0 + secs(40), STABLE).is_empty());
}

#[test]
fn even_with_no_wait_a_file_needs_a_second_listing_and_some_content() {
    let dir = TestDir::new("watch-zero");
    let mut tracker = Tracker::default();
    let t0 = Instant::now();
    take(&mut tracker, dir.path(), t0, Duration::ZERO);
    std::fs::write(dir.path().join("a.mp4"), b"x").unwrap();
    std::fs::write(dir.path().join("empty.mp4"), b"").unwrap();
    assert!(take(&mut tracker, dir.path(), t0, Duration::ZERO).is_empty());
    let found = take(&mut tracker, dir.path(), t0 + secs(3), Duration::ZERO);
    assert_eq!(names(&found), ["a.mp4"], "an empty file is never complete");
}

#[test]
fn partial_hidden_and_unsupported_files_are_never_taken() {
    let dir = TestDir::new("watch-ignored");
    let mut tracker = Tracker::default();
    let t0 = Instant::now();
    take(&mut tracker, dir.path(), t0, STABLE);
    for name in [
        "a.mp4.part",
        "b.mp4.crdownload",
        "c.TMP",
        "~$d.mp4",
        ".e.mp4",
        "notes.txt",
        "keep.MKV",
    ] {
        std::fs::write(dir.path().join(name), b"content").unwrap();
    }
    std::fs::create_dir(dir.path().join("folder.mp4")).unwrap();
    take(&mut tracker, dir.path(), t0 + secs(1), STABLE);
    let found = take(&mut tracker, dir.path(), t0 + secs(60), STABLE);
    assert_eq!(names(&found), ["keep.MKV"]);

    // A download that completes is renamed: a new name, taken once stable.
    std::fs::rename(dir.path().join("a.mp4.part"), dir.path().join("a.mp4")).unwrap();
    take(&mut tracker, dir.path(), t0 + secs(61), STABLE);
    let found = take(&mut tracker, dir.path(), t0 + secs(71), STABLE);
    assert_eq!(names(&found), ["a.mp4"]);
}

#[test]
fn ignore_rules_follow_names_and_windows_attributes() {
    let none = 0;
    assert_eq!(ignored("clip.mp4", false, false, none), None);
    assert_eq!(ignored("photo.JPG", false, false, none), None);
    assert_eq!(
        ignored("clip.mp4", true, false, none),
        Some(Ignored::Folder)
    );
    assert_eq!(ignored("clip.mp4", false, true, none), Some(Ignored::Link));
    assert_eq!(
        ignored("clip.mp4", false, false, FILE_ATTRIBUTE_HIDDEN),
        Some(Ignored::Hidden)
    );
    assert_eq!(
        ignored("clip.mp4", false, false, FILE_ATTRIBUTE_SYSTEM),
        Some(Ignored::System)
    );
    assert_eq!(
        ignored(".clip.mp4", false, false, none),
        Some(Ignored::Hidden)
    );
    for partial in [
        "clip.mp4.part",
        "clip.crdownload",
        "x.tmp",
        "~$clip.mp4",
        "clip.mkv.!qB",
        "clip.mp4.partial",
        "clip.mp4.download",
        "clip.mp4.opdownload",
    ] {
        assert_eq!(
            ignored(partial, false, false, none),
            Some(Ignored::Partial),
            "{partial}"
        );
    }
    assert_eq!(
        ignored("Thumbs.db", false, false, none),
        Some(Ignored::Unsupported)
    );
    // A OneDrive file whose content is online only is a media file not ready yet, not an
    // ignored one: it is taken once the sync brings it.
    assert_eq!(
        ignored(
            "clip.mp4",
            false,
            false,
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        ),
        None
    );
    assert!(online_only(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS));
    assert!(online_only(FILE_ATTRIBUTE_OFFLINE));
    assert!(!online_only(FILE_ATTRIBUTE_HIDDEN));
}

#[cfg(windows)]
#[test]
fn a_file_still_open_for_writing_waits_until_it_is_closed() {
    use std::io::Write;

    let dir = TestDir::new("watch-writer");
    let mut tracker = Tracker::default();
    let t0 = Instant::now();
    take(&mut tracker, dir.path(), t0, STABLE);
    // A recorder that has paused: the size does not move, the handle is still open.
    let mut writer = std::fs::File::create(dir.path().join("rec.mkv")).unwrap();
    writer.write_all(b"frames").unwrap();
    writer.flush().unwrap();
    take(&mut tracker, dir.path(), t0 + secs(1), STABLE);
    assert!(
        take(&mut tracker, dir.path(), t0 + secs(30), STABLE).is_empty(),
        "stable but still open for writing"
    );
    drop(writer);
    let found = take(&mut tracker, dir.path(), t0 + secs(33), STABLE);
    assert_eq!(names(&found), ["rec.mkv"]);
}

#[test]
fn files_present_at_the_start_are_left_alone() {
    let dir = TestDir::new("watch-baseline");
    std::fs::write(dir.path().join("old.mp4"), b"old").unwrap();
    let mut tracker = Tracker::default();
    let t0 = Instant::now();
    take(&mut tracker, dir.path(), t0, STABLE);
    std::fs::write(dir.path().join("new.mp4"), b"new").unwrap();
    take(&mut tracker, dir.path(), t0 + secs(1), STABLE);
    let found = take(&mut tracker, dir.path(), t0 + secs(20), STABLE);
    assert_eq!(names(&found), ["new.mp4"]);

    // Removed then back under the same name: a new file.
    std::fs::remove_file(dir.path().join("old.mp4")).unwrap();
    take(&mut tracker, dir.path(), t0 + secs(21), STABLE);
    std::fs::write(dir.path().join("old.mp4"), b"again").unwrap();
    take(&mut tracker, dir.path(), t0 + secs(22), STABLE);
    let found = take(&mut tracker, dir.path(), t0 + secs(40), STABLE);
    assert_eq!(names(&found), ["old.mp4"]);
}

#[test]
fn found_files_wait_in_pending_review_and_never_run_by_themselves() {
    let dir = TestDir::new("watch-pending");
    let watched = dir.path().join("in");
    std::fs::create_dir(&watched).unwrap();
    let mut store = open_store(&dir);
    let destination_id = first_destination(&store);
    // Mode from the folder for one, from the destination's default for the other.
    store
        .update_destination(
            destination_id,
            &crate::queue::store::DestinationPatch {
                default_mode: Some(SendMode::Document),
                ..Default::default()
            },
        )
        .unwrap();
    store
        .add_watch_folder(
            &watched.to_string_lossy(),
            destination_id,
            Some(SendMode::Photo),
        )
        .unwrap();
    let other = dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    store
        .add_watch_folder(&other.to_string_lossy(), destination_id, None)
        .unwrap();
    let queue = orchestrator(store);
    let switch = Arc::new(Switch::default());
    switch.set(true);
    let mut engine = Engine::new(queue.clone(), switch);
    let t0 = Instant::now();
    engine.poll(t0, STABLE);
    std::fs::write(watched.join("a.jpg"), b"jpeg").unwrap();
    std::fs::write(other.join("b.mp4"), b"video").unwrap();
    engine.poll(t0 + secs(3), STABLE);
    engine.poll(t0 + secs(15), STABLE);

    let items = queue.list().unwrap();
    assert_eq!(items.len(), 2);
    for item in &items {
        assert_eq!(item.state, State::PendingReview);
        assert_eq!(item.hold_reason.as_deref(), Some(HOLD_WATCH));
        assert_eq!(item.origin, ORIGIN_WATCH);
        assert_eq!(item.destination_id, destination_id);
        assert_eq!(item.caption, "");
    }
    assert_eq!(items[0].send_mode, SendMode::Photo);
    assert_eq!(items[1].send_mode, SendMode::Document);
    assert_eq!(items[1].source_size, 5);
    let runnable = queue.with_store(|store| store.next_runnable()).unwrap();
    assert_eq!(runnable, None, "nothing runs without approval");
    // The destination was not chosen by the user: "recently used" is unchanged.
    let used = queue
        .with_store(|store| store.get_destination(destination_id))
        .unwrap();
    assert_eq!(used.use_count, 0);
}

#[test]
fn a_restart_adds_nothing_again() {
    let dir = TestDir::new("watch-restart");
    let watched = dir.path().join("in");
    std::fs::create_dir(&watched).unwrap();
    let switch = Arc::new(Switch::default());
    switch.set(true);
    let t0 = Instant::now();
    {
        let mut store = open_store(&dir);
        let destination_id = first_destination(&store);
        store
            .add_watch_folder(&watched.to_string_lossy(), destination_id, None)
            .unwrap();
        let queue = orchestrator(store);
        let mut engine = Engine::new(queue.clone(), switch.clone());
        engine.poll(t0, STABLE);
        std::fs::write(watched.join("kept.mp4"), b"1").unwrap();
        std::fs::write(watched.join("removed.mp4"), b"2").unwrap();
        engine.poll(t0 + secs(3), STABLE);
        engine.poll(t0 + secs(15), STABLE);
        assert_eq!(watched_items(&queue), ["kept.mp4", "removed.mp4"]);
        // The user removes one from the queue, then clears the finished items.
        let removed = queue.list().unwrap()[1].id;
        queue.cancel(removed).unwrap();
        queue
            .clear(crate::queue::store::ClearScope::Finished)
            .unwrap();
        // Still waiting to be complete when the app closes.
        std::fs::write(watched.join("late.mp4"), b"3").unwrap();
        engine.poll(t0 + secs(18), STABLE);
    }

    let queue = orchestrator(Store::open(&dir.path().join("squeezegram.db")).unwrap());
    let mut engine = Engine::new(queue.clone(), switch);
    let t1 = t0 + secs(100);
    for step in 0..5 {
        engine.poll(t1 + secs(step * 10), STABLE);
    }
    assert_eq!(watched_items(&queue), ["kept.mp4"]);

    // Watching goes on after the restart.
    std::fs::write(watched.join("next.mp4"), b"4").unwrap();
    engine.poll(t1 + secs(60), STABLE);
    engine.poll(t1 + secs(75), STABLE);
    assert_eq!(watched_items(&queue), ["kept.mp4", "next.mp4"]);
}

#[test]
fn the_license_gate_turns_watching_off_and_on() {
    let dir = TestDir::new("watch-gate");
    let watched = dir.path().join("in");
    std::fs::create_dir(&watched).unwrap();
    let mut store = open_store(&dir);
    let destination_id = first_destination(&store);
    store
        .add_watch_folder(&watched.to_string_lossy(), destination_id, None)
        .unwrap();
    let queue = orchestrator(store);
    let switch = Arc::new(Switch::default());
    let mut engine = Engine::new(queue.clone(), switch.clone());
    let t0 = Instant::now();

    // Not licensed: nothing is listed, nothing is taken.
    engine.poll(t0, STABLE);
    std::fs::write(watched.join("while_off.mp4"), b"1").unwrap();
    engine.poll(t0 + secs(3), STABLE);
    engine.poll(t0 + secs(30), STABLE);
    assert!(watched_items(&queue).is_empty());
    assert!(engine.trackers.is_empty());

    // Licensed: what is there now is the baseline; only what comes next is taken.
    switch.set(true);
    engine.poll(t0 + secs(31), STABLE);
    std::fs::write(watched.join("while_on.mp4"), b"2").unwrap();
    engine.poll(t0 + secs(34), STABLE);
    engine.poll(t0 + secs(45), STABLE);
    assert_eq!(watched_items(&queue), ["while_on.mp4"]);

    // The gate closes for a while (TDLib reconnects, or the user signs out and in): a file
    // on its way and one that lands meanwhile are taken once it opens again.
    std::fs::write(watched.join("on_its_way.mp4"), b"3").unwrap();
    engine.poll(t0 + secs(46), STABLE);
    switch.set(false);
    std::fs::write(watched.join("in_the_gap.mp4"), b"4").unwrap();
    engine.poll(t0 + secs(60), STABLE);
    engine.poll(t0 + secs(90), STABLE);
    assert_eq!(
        watched_items(&queue),
        ["while_on.mp4"],
        "nothing while closed"
    );
    switch.set(true);
    engine.poll(t0 + secs(91), STABLE);
    engine.poll(t0 + secs(102), STABLE);
    assert_eq!(
        watched_items(&queue),
        ["while_on.mp4", "on_its_way.mp4", "in_the_gap.mp4"]
    );
    // The folder itself stays configured.
    let folders = queue
        .with_store(|store| store.list_watch_folders())
        .unwrap();
    assert_eq!(folders.len(), 1);
}

#[test]
fn a_disabled_folder_is_not_watched_and_starts_over_when_enabled() {
    let dir = TestDir::new("watch-disabled");
    let watched = dir.path().join("in");
    std::fs::create_dir(&watched).unwrap();
    let mut store = open_store(&dir);
    let destination_id = first_destination(&store);
    let folder = store
        .add_watch_folder(&watched.to_string_lossy(), destination_id, None)
        .unwrap();
    let queue = orchestrator(store);
    let switch = Arc::new(Switch::default());
    switch.set(true);
    let mut engine = Engine::new(queue.clone(), switch);
    let t0 = Instant::now();
    engine.poll(t0, STABLE);
    let set_enabled = |enabled| {
        queue
            .with_store(|store| {
                store.update_watch_folder(
                    folder.id,
                    &crate::queue::store::WatchFolderPatch {
                        enabled: Some(enabled),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
    };
    set_enabled(false);
    std::fs::write(watched.join("meanwhile.mp4"), b"1").unwrap();
    engine.poll(t0 + secs(3), STABLE);
    engine.poll(t0 + secs(30), STABLE);
    set_enabled(true);
    engine.poll(t0 + secs(33), STABLE);
    engine.poll(t0 + secs(60), STABLE);
    assert!(watched_items(&queue).is_empty());
}

#[test]
fn an_unreadable_folder_is_retried_and_its_baseline_taken_when_it_comes_back() {
    let dir = TestDir::new("watch-missing");
    let watched = dir.path().join("share");
    let mut store = open_store(&dir);
    let destination_id = first_destination(&store);
    store
        .add_watch_folder(&watched.to_string_lossy(), destination_id, None)
        .unwrap();
    let queue = orchestrator(store);
    let switch = Arc::new(Switch::default());
    switch.set(true);
    let mut engine = Engine::new(queue.clone(), switch);
    let t0 = Instant::now();
    engine.poll(t0, STABLE);
    let unreadable = engine.unreadable();
    assert_eq!(unreadable.lock().unwrap().len(), 1, "shown in Settings");

    std::fs::create_dir(&watched).unwrap();
    std::fs::write(watched.join("there.mp4"), b"1").unwrap();
    engine.poll(t0 + secs(3), STABLE);
    assert!(unreadable.lock().unwrap().is_empty());
    std::fs::write(watched.join("new.mp4"), b"2").unwrap();
    engine.poll(t0 + secs(6), STABLE);
    engine.poll(t0 + secs(20), STABLE);
    assert_eq!(watched_items(&queue), ["new.mp4"]);
}

#[test]
fn a_path_already_in_the_queue_is_not_queued_twice() {
    let dir = TestDir::new("watch-twice");
    let watched = dir.path().join("in");
    std::fs::create_dir(&watched).unwrap();
    let mut store = open_store(&dir);
    let destination_id = first_destination(&store);
    store
        .add_watch_folder(&watched.to_string_lossy(), destination_id, None)
        .unwrap();
    let queue = orchestrator(store);
    let switch = Arc::new(Switch::default());
    switch.set(true);
    let mut engine = Engine::new(queue.clone(), switch);
    let t0 = Instant::now();
    engine.poll(t0, STABLE);
    let path = watched.join("both.mp4");
    std::fs::write(&path, b"1").unwrap();
    // The user drops the same file by hand before the watcher takes it.
    queue
        .add(&[NewItem {
            source_path: path.to_string_lossy().to_uppercase(),
            source_size: 1,
            source_mtime: 0,
            destination_id,
            send_mode: SendMode::Video,
            caption: String::new(),
        }])
        .unwrap();
    engine.poll(t0 + secs(3), STABLE);
    engine.poll(t0 + secs(20), STABLE);
    assert_eq!(queue.list().unwrap().len(), 1);
    assert!(watched_items(&queue).is_empty());
}

#[test]
fn a_file_the_queue_refuses_is_tried_again() {
    let dir = TestDir::new("watch-refused");
    let watched = dir.path().join("in");
    std::fs::create_dir(&watched).unwrap();
    let mut store = open_store(&dir);
    let destination_id = first_destination(&store);
    let folder = store
        .add_watch_folder(&watched.to_string_lossy(), destination_id, None)
        .unwrap();
    // A folder whose destination is gone: every insert is refused.
    store
        .conn()
        .execute_batch("PRAGMA foreign_keys = OFF")
        .unwrap();
    store
        .conn()
        .execute(
            "UPDATE watch_folders SET destination_id = 999 WHERE id = ?1",
            [folder.id],
        )
        .unwrap();
    let queue = orchestrator(store);
    let switch = Arc::new(Switch::default());
    switch.set(true);
    let mut engine = Engine::new(queue.clone(), switch);
    let t0 = Instant::now();
    engine.poll(t0, STABLE);
    std::fs::write(watched.join("a.mp4"), b"1").unwrap();
    engine.poll(t0 + secs(3), STABLE);
    engine.poll(t0 + secs(15), STABLE);
    engine.poll(t0 + secs(18), STABLE);
    assert!(watched_items(&queue).is_empty());
    let tracker = &engine.trackers[&folder.id];
    assert!(tracker.waiting["a.mp4"].refused, "still waiting, not known");

    // The user points the folder at a destination again: the file goes in.
    queue
        .with_store(|store| {
            store.update_watch_folder(
                folder.id,
                &crate::queue::store::WatchFolderPatch {
                    destination_id: Some(destination_id),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    engine.poll(t0 + secs(21), STABLE);
    assert_eq!(watched_items(&queue), ["a.mp4"]);
    engine.poll(t0 + secs(24), STABLE);
    assert_eq!(watched_items(&queue), ["a.mp4"], "taken once");
}

#[test]
fn the_service_stops_at_once() {
    let dir = TestDir::new("watch-service");
    let queue = orchestrator(open_store(&dir));
    let settings = Arc::new(SettingsStore::load(dir.path().join("settings.json")).unwrap());
    let service = WatchService::start(queue, Arc::new(Switch::default()), settings).unwrap();
    service.wake();
    let started = Instant::now();
    service.stop();
    assert!(
        started.elapsed() < STOP_TIMEOUT,
        "stop does not wait a poll"
    );
    // Idempotent, and a late wake is harmless.
    service.stop();
    service.wake();
}
