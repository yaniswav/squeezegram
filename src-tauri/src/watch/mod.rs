//! Watched folders (paid feature `watched_folder`, BESOINS F8).
//!
//! A file that appears in a watched folder is queued in `pending_review` once complete, and
//! is never sent without the user's approval (`queue_promote`, N4).
//!
//! **Polling, not change notifications.** Each folder is listed every `POLL_INTERVAL`.
//! `ReadDirectoryChangesW` (the `notify` crate) would need the same timer anyway: a file is
//! complete when its size has stopped changing for a while, which no event reports. It also
//! misses events on network shares and loses them when its buffer overflows under many files,
//! and a OneDrive folder reports sync work that is no new file. A listing reads directory
//! entries only and opens nothing but the few new files, so a poll costs one directory read
//! per folder, and no dependency is added.
//!
//! **When a file is taken.** Its size and modification time have not changed for
//! `settings.watch.stable_seconds` (two listings at least), it is not empty, its content is on
//! the disk (a OneDrive placeholder would be downloaded by any read: it waits until the user
//! or the sync client brings it), and it opens for reading while refusing writers, which fails
//! as long as a program still holds it open for writing. Partial and temporary files
//! (`PARTIAL_SUFFIXES`, `~$…`), hidden and system files, folders, links and files that are not
//! media (`queue::intake::is_media`) are never taken. Subfolders are not listed.
//!
//! **Which files.** Those the watcher has not dealt with yet. Each folder keeps a list of the
//! files it left alone or took (`watch_seen`: name, size, modification time as listed), saved
//! in the database. The first listing of a folder, when it has no list yet, is a baseline: all
//! its files are recorded and none is taken, at the first poll the license allows after the
//! folder is added or enabled again. That is also what an upgrade does for a folder that
//! existed before the list did, so it never floods the queue. Later, the first listing after a
//! start compares the folder with the saved list: a file that is not in it (new name, or the
//! same name with another size or time) arrived while the app was closed and is taken like any
//! other, once complete. Taken means queued, whatever happens to the item afterwards (sent,
//! removed, ignored): a restart never adds it again. A folder that is disabled starts over,
//! so what came meanwhile is left alone. A name that leaves the folder and comes back is a new
//! file. A path still waiting or running in the queue is not queued twice
//! (`Store::add_watched_item`), and content already sent to the destination is held as a
//! duplicate when probed, as for a manual add.
//!
//! **Approval.** What the user approves is a private copy, made when the file is queued
//! (`Orchestrator::add_watched`) in `<temp root>\frozen\<item id>`: the analysis, the approval
//! and the send all read it, so a file replaced afterwards, even by one of the same size, never
//! reaches the destination. The replacement is a new version (new size or time) and a new item
//! to approve. At most `MAX_WAITING_COPIES` files wait for approval: past that a file is left
//! for the next poll (`WATCH_WAITING_FULL` in the journal) until the user decides some.
//!
//! **Gate.** Nothing is listed unless `Entitlements::allows(WatchedFolder)`, asked at every
//! poll: the verdict follows the license and the signed-in account. The license commands and
//! the login watcher wake the thread so a change applies at once. The gate also closes for a
//! while on every reconnection or sign-out (no account, so the license waits): what each folder
//! was watching is kept meanwhile, and files that came in the gap are taken when it opens
//! again. Only a folder removed or disabled is forgotten, and starts a new baseline.
//!
//! **Thread.** A thread of its own: listing a network share can block for seconds. The queue's
//! lock is taken only to read the folders and the seen lists, to insert an item and to save a
//! seen list.
//!
//! Journal: `watch_active`, `watch_inactive`, `watch_folder_baseline`, `watch_folder_resumed`,
//! `watch_folder_error`, `watch_folder_recovered`, `watch_file_added`, `watch_file_skipped`,
//! `watch_file_ignored` (debug), `watch_stopped`, `watch_stop_timeout`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::entitlements::{Entitlements, Feature};
use crate::logging;
use crate::queue::intake;
use crate::queue::orchestrator::Orchestrator;
use crate::queue::store::{NewItem, SeenFile, WatchFolder};
use crate::settings::SettingsStore;

/// Time between two listings of the watched folders.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// How long the exit waits for the thread, which may be stuck listing an unreachable share.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Name endings of files another program is still downloading or writing. Torrent clients and
/// download managers allocate the full size at once and close the file between bursts, so
/// neither a stable size nor the write probe tells such a file is done: only its name does
/// (qBittorrent `.!qB`, Firefox and others `.part`, Chrome `.crdownload`, Safari `.download`,
/// Opera `.opdownload`, curl and others `.partial`).
const PARTIAL_SUFFIXES: [&str; 7] = [
    ".part",
    ".crdownload",
    ".tmp",
    ".!qb",
    ".partial",
    ".download",
    ".opdownload",
];

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// Why a new name is never taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ignored {
    Folder,
    Link,
    Hidden,
    System,
    Partial,
    Unsupported,
}

impl Ignored {
    fn as_str(self) -> &'static str {
        match self {
            Ignored::Folder => "folder",
            Ignored::Link => "link",
            Ignored::Hidden => "hidden",
            Ignored::System => "system",
            Ignored::Partial => "partial",
            Ignored::Unsupported => "unsupported",
        }
    }
}

/// Decided from the directory entry alone: nothing is opened.
fn ignored(name: &str, is_dir: bool, is_link: bool, attributes: u32) -> Option<Ignored> {
    let lower = name.to_lowercase();
    if is_link {
        Some(Ignored::Link)
    } else if is_dir {
        Some(Ignored::Folder)
    } else if name.starts_with('.') || attributes & FILE_ATTRIBUTE_HIDDEN != 0 {
        Some(Ignored::Hidden)
    } else if attributes & FILE_ATTRIBUTE_SYSTEM != 0 {
        Some(Ignored::System)
    } else if name.starts_with("~$") || PARTIAL_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        Some(Ignored::Partial)
    } else if !intake::is_media(Path::new(name)) {
        Some(Ignored::Unsupported)
    } else {
        None
    }
}

/// A cloud file whose content is not on the disk yet: reading it would download it.
fn online_only(attributes: u32) -> bool {
    attributes
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
}

/// Windows attributes as listed in the directory (no file is opened); 0 elsewhere.
fn attributes(metadata: &fs::Metadata) -> u32 {
    #[cfg(windows)]
    {
        std::os::windows::fs::MetadataExt::file_attributes(metadata)
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        0
    }
}

/// Opens the file for reading while refusing to share it with a writer: Windows refuses the
/// open (sharing violation) as long as another program holds it open for writing.
///
/// The handle lives a few microseconds, but meanwhile it is a lock: a writer that reopens the
/// file for writing in that instant gets a sharing violation. The probe runs only on a file
/// whose size has been stable for `stable_seconds`, once per poll, which makes that rare.
fn readable_alone(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        // Other readers, a rename or a delete by the user stay possible meanwhile.
        const FILE_SHARE_DELETE: u32 = 0x4;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    options.open(path).map(drop)
}

/// A new file, not complete yet.
struct Waiting {
    size: u64,
    modified: Option<SystemTime>,
    /// Since when `size` and `modified` have not changed.
    since: Instant,
    /// Complete, but the queue refused it (journaled once); retried at every poll.
    refused: bool,
}

/// Size and modification time (seconds since the Unix epoch) of a file as the directory listing
/// shows them: what `watch_seen` compares.
type Stamp = (u64, i64);

fn stamp(metadata: &fs::Metadata) -> Stamp {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |age| age.as_secs() as i64);
    (metadata.len(), modified)
}

/// A complete file, to queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Name in the folder, handed back to `Tracker::taken` or `Tracker::refused`.
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    /// Seconds since the Unix epoch, as `items.source_mtime`.
    pub modified_s: i64,
    /// What the listing showed, recorded in the seen list once the queue has the file.
    stamp: Stamp,
    /// From the first listing that showed it.
    pub waited: Duration,
    /// The queue refused it at an earlier poll.
    pub retry: bool,
}

/// What the listings of one folder have shown.
#[derive(Default)]
struct Tracker {
    /// The baseline listing is done.
    started: bool,
    /// Names never to take: present at the baseline, ignored, or taken. Forgotten when the
    /// name leaves the folder.
    known: HashSet<String>,
    waiting: HashMap<String, Waiting>,
    /// The last listing failed: its error was journaled once.
    failing: bool,
    /// The files left alone or taken that are worth remembering across a restart (media the
    /// baseline found, files taken), kept in step with `watch_seen`.
    seen: HashMap<String, Stamp>,
    /// `seen` has changed since it was saved.
    unsaved: bool,
    /// Saving failed: journaled once, tried again at every poll.
    save_failing: bool,
    /// The saved list of an earlier run, compared with the first listing. `None` for a folder
    /// never seeded, whose first listing is a baseline.
    saved: Option<HashMap<String, Stamp>>,
}

impl Tracker {
    /// A folder whose seen list was saved by an earlier run.
    fn resuming(saved: Vec<SeenFile>) -> Self {
        Self {
            saved: Some(
                saved
                    .into_iter()
                    .map(|file| (file.name, (file.size as u64, file.mtime)))
                    .collect(),
            ),
            ..Self::default()
        }
    }

    /// The seen list as stored.
    fn seen_files(&self) -> Vec<SeenFile> {
        self.seen
            .iter()
            .map(|(name, &(size, mtime))| SeenFile {
                name: name.clone(),
                size: size as i64,
                mtime,
            })
            .collect()
    }

    /// One listing of `dir` at `now`; returns the files that became complete.
    fn scan(
        &mut self,
        folder_id: i64,
        dir: &Path,
        now: Instant,
        stable: Duration,
    ) -> io::Result<Vec<Found>> {
        let entries = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
        let names: HashSet<String> = entries
            .iter()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        if !self.started {
            self.started = true;
            let Some(saved) = self.saved.take() else {
                // Only files a poll could take are worth remembering: the others are ignored
                // again by name at the next start.
                self.seen = entries
                    .iter()
                    .filter_map(|entry| {
                        let (listed, kind) = (entry.metadata().ok()?, entry.file_type().ok()?);
                        let name = entry.file_name().to_string_lossy().into_owned();
                        ignored(&name, kind.is_dir(), kind.is_symlink(), attributes(&listed))
                            .is_none()
                            .then(|| (name, stamp(&listed)))
                    })
                    .collect();
                self.unsaved = true;
                tracing::info!(
                    event = "watch_folder_baseline",
                    folder_id,
                    path = %dir.display(),
                    files = names.len(),
                    "watching a folder; the files already in it are left alone"
                );
                self.known = names;
                return Ok(Vec::new());
            };
            // A file counts as seen only if it is the one that was seen: another size or time
            // is a file that arrived (or was replaced) while the app was closed.
            for entry in &entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                let listed = entry.metadata().ok().map(|listed| stamp(&listed));
                if let Some(listed) = listed
                    && saved.get(&name) == Some(&listed)
                {
                    self.known.insert(name.clone());
                    self.seen.insert(name, listed);
                }
            }
            // Dropped from the list: gone from the folder, or changed.
            self.unsaved = self.seen.len() != saved.len();
            tracing::info!(
                event = "watch_folder_resumed",
                folder_id,
                path = %dir.display(),
                known = self.known.len(),
                files = names.len(),
                "watching a folder; files that came while the app was closed are taken once complete"
            );
        }
        self.known.retain(|name| names.contains(name));
        self.waiting.retain(|name, _| names.contains(name));
        let before = self.seen.len();
        self.seen.retain(|name, _| names.contains(name));
        self.unsaved |= self.seen.len() != before;

        let mut found = Vec::new();
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if self.known.contains(&name) {
                continue;
            }
            let path = entry.path();
            // From the directory listing: on Windows nothing is opened. An entry that cannot
            // be read (gone since the listing) is seen again at the next poll, or never.
            let (Ok(listed), Ok(kind)) = (entry.metadata(), entry.file_type()) else {
                continue;
            };
            let attributes = attributes(&listed);
            let listing = stamp(&listed);
            if !self.waiting.contains_key(&name)
                && let Some(reason) = ignored(&name, kind.is_dir(), kind.is_symlink(), attributes)
            {
                tracing::debug!(
                    event = "watch_file_ignored",
                    folder_id,
                    path = %path.display(),
                    reason = reason.as_str(),
                    "file in a watched folder ignored"
                );
                self.known.insert(name);
                continue;
            }
            // The listing's size and time can lag behind a file being written: ask the file,
            // unless it is online only (even that open could start a download).
            let metadata = if online_only(attributes) {
                listed
            } else {
                match fs::metadata(&path) {
                    Ok(metadata) => metadata,
                    // Gone or locked between the listing and now: the next poll tells.
                    Err(_) => continue,
                }
            };
            let (size, modified) = (metadata.len(), metadata.modified().ok());
            let observed = Waiting {
                size,
                modified,
                since: now,
                refused: false,
            };
            // A new file is compared from the next listing on.
            let Some(waiting) = self.waiting.get_mut(&name) else {
                self.waiting.insert(name, observed);
                continue;
            };
            if (size, modified) != (waiting.size, waiting.modified) {
                *waiting = observed;
                continue;
            }
            let waited = now.saturating_duration_since(waiting.since);
            if size == 0 || waited < stable || online_only(attributes) {
                continue;
            }
            if readable_alone(&path).is_err() {
                continue;
            }
            // Stays waiting until the queue has it (`taken`): a refused insert is tried again.
            let retry = waiting.refused;
            found.push(Found {
                name,
                path,
                size,
                modified_s: modified
                    .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |age| age.as_secs() as i64),
                stamp: listing,
                waited,
                retry,
            });
        }
        Ok(found)
    }

    /// The queue has the file (added, or already there): never taken again under this name,
    /// not even after a restart.
    fn taken(&mut self, file: &Found) {
        self.waiting.remove(&file.name);
        self.known.insert(file.name.clone());
        self.seen.insert(file.name.clone(), file.stamp);
        self.unsaved = true;
    }

    /// The queue refused the file (database busy, destination gone): tried at the next poll.
    fn refused(&mut self, name: &str) {
        if let Some(waiting) = self.waiting.get_mut(name) {
            waiting.refused = true;
        }
    }
}

/// The watcher's work, one poll at a time. The thread polls with the real clock, tests with
/// their own instants.
pub struct Engine {
    queue: Orchestrator,
    entitlements: Arc<dyn Entitlements>,
    /// Where the private copies go: the temp root, read at each file.
    settings: Arc<SettingsStore>,
    /// Whether the last poll was allowed to watch; `None` before the first poll.
    active: Option<bool>,
    trackers: HashMap<i64, Tracker>,
    /// Folders whose last listing failed (missing, renamed, offline share), for `watch_list`.
    unreadable: Arc<Mutex<HashSet<i64>>>,
}

impl Engine {
    pub fn new(
        queue: Orchestrator,
        entitlements: Arc<dyn Entitlements>,
        settings: Arc<SettingsStore>,
    ) -> Self {
        Self {
            queue,
            entitlements,
            settings,
            active: None,
            trackers: HashMap::new(),
            unreadable: Arc::default(),
        }
    }

    /// Kept up to date by every poll.
    pub fn unreadable(&self) -> Arc<Mutex<HashSet<i64>>> {
        self.unreadable.clone()
    }

    fn publish_unreadable(&self) {
        *lock(&self.unreadable) = self
            .trackers
            .iter()
            .filter(|(_, tracker)| tracker.failing)
            .map(|(&folder_id, _)| folder_id)
            .collect();
    }

    pub fn poll(&mut self, now: Instant, stable: Duration) {
        let folders = match self.queue.with_store(|store| store.list_watch_folders()) {
            Ok(folders) => folders,
            Err(error) => {
                tracing::error!(event = "queue_error", error_code = error.code(), error = %error, "could not read the watched folders");
                return;
            }
        };
        let watched: Vec<WatchFolder> = folders
            .into_iter()
            .filter(|folder| folder.enabled && folder.destination_id.is_some())
            .collect();
        // A folder removed or disabled is forgotten, gate open or not: enabled again, it
        // starts a new baseline. Only the database is read here, never a folder.
        self.trackers.retain(|&folder_id, tracker| {
            let keep = watched.iter().any(|folder| folder.id == folder_id);
            if !keep && !tracker.waiting.is_empty() {
                tracing::info!(
                    event = "watch_folder_released",
                    folder_id,
                    waiting = tracker.waiting.len(),
                    "watched folder removed or disabled; files not complete yet are dropped"
                );
            }
            keep
        });
        self.publish_unreadable();
        // Closed for a while at each reconnection or sign-out: the trackers stay as they are.
        if !self.entitlements.allows(Feature::WatchedFolder) {
            if self.active != Some(false) {
                tracing::info!(
                    event = "watch_inactive",
                    reason = "not_licensed",
                    folders = watched.len(),
                    "watched folders inactive: the license does not grant them now"
                );
                self.active = Some(false);
            }
            return;
        }
        if self.active != Some(true) {
            tracing::info!(
                event = "watch_active",
                folders = watched.len(),
                "watched folders active"
            );
            self.active = Some(true);
        }
        for folder in &watched {
            if !self.trackers.contains_key(&folder.id) {
                // A folder seeded by an earlier run is compared with its saved list; a read
                // that fails leaves the folder to the next poll rather than baselining blind.
                let saved = self.queue.with_store(|store| store.watch_seen(folder.id));
                match saved {
                    Ok(saved) => {
                        let tracker = saved.map_or_else(Tracker::default, Tracker::resuming);
                        self.trackers.insert(folder.id, tracker);
                    }
                    Err(error) => {
                        tracing::error!(event = "queue_error", error_code = error.code(), error = %error, "could not read the files already seen in a watched folder");
                        continue;
                    }
                }
            }
            let Some(tracker) = self.trackers.get_mut(&folder.id) else {
                continue;
            };
            match tracker.scan(folder.id, Path::new(&folder.path), now, stable) {
                Ok(found) => {
                    if tracker.failing {
                        tracker.failing = false;
                        tracing::info!(
                            event = "watch_folder_recovered",
                            folder_id = folder.id,
                            path = folder.path.as_str(),
                            "watched folder readable again"
                        );
                    }
                    for file in found {
                        if queue_file(&self.queue, &self.settings, folder, &file) {
                            tracker.taken(&file);
                        } else {
                            tracker.refused(&file.name);
                        }
                    }
                    save_seen(&self.queue, folder, tracker);
                }
                Err(error) => {
                    if !tracker.failing {
                        tracker.failing = true;
                        tracing::warn!(
                            event = "watch_folder_error",
                            folder_id = folder.id,
                            path = folder.path.as_str(),
                            error_code = "IO_ERROR",
                            error = %error,
                            "cannot list a watched folder"
                        );
                    }
                }
            }
        }
        self.publish_unreadable();
    }
}

/// Saves the seen list when it changed. A failure is journaled once and retried at every poll.
fn save_seen(queue: &Orchestrator, folder: &WatchFolder, tracker: &mut Tracker) {
    if !tracker.unsaved {
        return;
    }
    let files = tracker.seen_files();
    match queue.with_store(|store| store.save_watch_seen(folder.id, &files)) {
        Ok(_) => {
            tracker.unsaved = false;
            tracker.save_failing = false;
        }
        Err(error) if !tracker.save_failing => {
            tracker.save_failing = true;
            tracing::error!(event = "queue_error", folder_id = folder.id, error_code = error.code(), error = %error, "could not save the files already seen in a watched folder, tried again at the next poll");
        }
        Err(_) => {}
    }
}

/// Whether the queue has the file now (added, or already there).
fn queue_file(
    queue: &Orchestrator,
    settings: &SettingsStore,
    folder: &WatchFolder,
    file: &Found,
) -> bool {
    let Some(destination_id) = folder.destination_id else {
        return false;
    };
    let path = file.path.to_string_lossy().into_owned();
    let added = queue
        .with_store(|store| match folder.send_mode {
            Some(mode) => Ok(mode),
            None => Ok(store.get_destination(destination_id)?.default_mode),
        })
        .and_then(|send_mode| {
            let frozen_root = crate::media::stage::temp_root(&settings.get().temp)?.join("frozen");
            queue.add_watched(
                &NewItem {
                    source_path: path.clone(),
                    source_size: file.size as i64,
                    source_mtime: file.modified_s,
                    destination_id,
                    send_mode,
                    caption: String::new(),
                },
                &frozen_root,
            )
        });
    match added {
        Ok(Some(item_id)) => tracing::info!(
            event = "watch_file_added",
            item_id,
            destination_id,
            folder_id = folder.id,
            path = path.as_str(),
            size = file.size,
            dur_ms = file.waited.as_millis() as u64,
            retried = file.retry,
            "file from a watched folder waits for approval"
        ),
        Ok(None) => tracing::info!(
            event = "watch_file_skipped",
            destination_id,
            folder_id = folder.id,
            path = path.as_str(),
            reason = "already_queued",
            "file from a watched folder already in the queue"
        ),
        // Once per file: it is tried again at every poll until the queue takes it.
        Err(ref error) if !file.retry => tracing::warn!(
            event = "watch_file_skipped",
            destination_id,
            folder_id = folder.id,
            path = path.as_str(),
            reason = "queue_error",
            error_code = error.code(),
            error = %error,
            "file from a watched folder not queued yet, tried again at the next poll"
        ),
        Err(_) => {}
    }
    added.is_ok()
}

enum Signal {
    Wake,
    Stop,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The watcher thread: `wake` after a change of the folders or of the license, `stop` once
/// on exit.
pub struct WatchService {
    signals: Mutex<Option<Sender<Signal>>>,
    done: Mutex<Option<Receiver<()>>>,
    unreadable: Arc<Mutex<HashSet<i64>>>,
}

impl WatchService {
    pub fn start(
        queue: Orchestrator,
        entitlements: Arc<dyn Entitlements>,
        settings: Arc<SettingsStore>,
    ) -> io::Result<Self> {
        let (signals, received) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let mut engine = Engine::new(queue, entitlements, settings.clone());
        let unreadable = engine.unreadable();
        thread::Builder::new().name("watch".into()).spawn(move || {
            let started = Instant::now();
            loop {
                let stable = Duration::from_secs(settings.get().watch.stable_seconds);
                engine.poll(Instant::now(), stable);
                match received.recv_timeout(POLL_INTERVAL) {
                    Ok(Signal::Wake) | Err(RecvTimeoutError::Timeout) => {}
                    Ok(Signal::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            tracing::info!(
                event = "watch_stopped",
                dur_ms = logging::ms_since(started),
                "watcher stopped"
            );
            let _ = finished.send(());
        })?;
        Ok(Self {
            signals: Mutex::new(Some(signals)),
            done: Mutex::new(Some(done)),
            unreadable,
        })
    }

    /// Whether the last listing of this folder failed (journaled as `watch_folder_error`).
    pub fn is_unreadable(&self, folder_id: i64) -> bool {
        lock(&self.unreadable).contains(&folder_id)
    }

    /// Polls now instead of at the next tick.
    pub fn wake(&self) {
        if let Some(signals) = lock(&self.signals).as_ref() {
            let _ = signals.send(Signal::Wake);
        }
    }

    /// Ends the thread after its current poll, waiting for it at most `STOP_TIMEOUT`: a thread
    /// stuck on an unreachable share is left to the end of the process.
    pub fn stop(&self) {
        let Some(signals) = lock(&self.signals).take() else {
            return;
        };
        let _ = signals.send(Signal::Stop);
        if let Some(done) = lock(&self.done).take()
            && done.recv_timeout(STOP_TIMEOUT).is_err()
        {
            tracing::warn!(
                event = "watch_stop_timeout",
                "watcher still busy at exit, left behind"
            );
        }
    }
}

#[cfg(test)]
mod tests;
