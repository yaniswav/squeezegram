//! Test fixtures: temporary directories and in-memory stores.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::State;
use super::store::{NewDestination, NewItem, SendMode, Store};

/// Temporary directory removed on drop.
pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new(name: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "squeezegram-test-{}-{name}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Creates a file and returns its path as text.
    pub fn file(&self, name: &str, content: &[u8]) -> String {
        let path = self.0.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn destination(peer_id: i64, topic_id: i64) -> NewDestination {
    NewDestination {
        peer_kind: "channel".into(),
        peer_id,
        topic_id,
        is_forum: topic_id != 0,
        is_broadcast: false,
        is_saved: false,
        title: format!("chat {peer_id}"),
        kind: "supergroup".into(),
        archived: false,
        folders: Vec::new(),
        can_write: Some(true),
        listed: true,
        topic_closed: false,
    }
}

/// In-memory store with one destination (peer 1, whole chat).
pub fn memory_store() -> Store {
    let mut store = Store::open_in_memory().unwrap();
    store.upsert_destination(&destination(1, 0)).unwrap();
    store
}

pub fn first_destination(store: &Store) -> i64 {
    store.list_destinations().unwrap()[0].id
}

pub fn add_item(store: &mut Store, path: &str) -> i64 {
    let destination_id = first_destination(store);
    store
        .add_items(&[NewItem {
            source_path: path.into(),
            source_size: 10,
            source_mtime: 1,
            destination_id,
            send_mode: SendMode::Video,
            caption: String::new(),
        }])
        .unwrap()[0]
}

/// Sets a state without validation, to build test situations.
pub fn force_state(store: &Store, id: i64, state: State) {
    store
        .conn()
        .execute(
            "UPDATE items SET state = ?1 WHERE id = ?2",
            rusqlite::params![state.as_str(), id],
        )
        .unwrap();
}
