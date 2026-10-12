//! Synchronous queue API over the SQLite database.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};

use super::state::{self, Changes, Column};
use super::{QueueError, Result, State, db, now_iso};
use crate::transport::ChatFolder;

pub const HOLD_DUPLICATE: &str = "duplicate";
/// `hold_reason` of a file found in a watched folder: it waits for the user's approval (N4).
pub const HOLD_WATCH: &str = "watch_pending";
/// `origin` of an item found in a watched folder (`manual` for `queue_add`).
pub const ORIGIN_WATCH: &str = "watch";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    Video,
    Document,
    Photo,
}

impl SendMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SendMode::Video => "video",
            SendMode::Document => "document",
            SendMode::Photo => "photo",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "video" => Ok(SendMode::Video),
            "document" => Ok(SendMode::Document),
            "photo" => Ok(SendMode::Photo),
            other => Err(QueueError::InvalidValue(format!("send_mode = {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Item {
    pub id: i64,
    pub position: i64,
    pub source_path: String,
    pub source_size: i64,
    pub source_mtime: i64,
    pub content_id: Option<String>,
    pub identity_weak: bool,
    pub destination_id: i64,
    pub send_mode: SendMode,
    pub caption: String,
    pub state: State,
    pub hold_reason: Option<String>,
    pub origin: String,
    pub media_json: Option<String>,
    pub plan_json: Option<String>,
    pub temp_path: Option<String>,
    pub output_path: Option<String>,
    pub output_size: Option<i64>,
    pub progress_phase: Option<String>,
    pub progress_pct: Option<f64>,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub next_run_at: Option<String>,
    pub tg_message_id: Option<i64>,
    pub tg_sent_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

const ITEM_COLUMNS: &str = "id, position, source_path, source_size, source_mtime, content_id,
    identity_weak, destination_id, send_mode, caption, state, hold_reason, origin, media_json,
    plan_json, temp_path, output_path, output_size, progress_phase, progress_pct, attempts,
    last_error, next_run_at, tg_message_id, tg_sent_at, created_at, updated_at";

fn invalid(column: usize, error: QueueError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

fn item_from_row(row: &Row<'_>) -> rusqlite::Result<Item> {
    let send_mode: String = row.get(8)?;
    let state: String = row.get(10)?;
    Ok(Item {
        id: row.get(0)?,
        position: row.get(1)?,
        source_path: row.get(2)?,
        source_size: row.get(3)?,
        source_mtime: row.get(4)?,
        content_id: row.get(5)?,
        identity_weak: row.get(6)?,
        destination_id: row.get(7)?,
        send_mode: SendMode::parse(&send_mode).map_err(|e| invalid(8, e))?,
        caption: row.get(9)?,
        state: state.parse().map_err(|e| invalid(10, e))?,
        hold_reason: row.get(11)?,
        origin: row.get(12)?,
        media_json: row.get(13)?,
        plan_json: row.get(14)?,
        temp_path: row.get(15)?,
        output_path: row.get(16)?,
        output_size: row.get(17)?,
        progress_phase: row.get(18)?,
        progress_pct: row.get(19)?,
        attempts: row.get(20)?,
        last_error: row.get(21)?,
        next_run_at: row.get(22)?,
        tg_message_id: row.get(23)?,
        tg_sent_at: row.get(24)?,
        created_at: row.get(25)?,
        updated_at: row.get(26)?,
    })
}

#[derive(Debug, Clone)]
pub struct NewItem {
    pub source_path: String,
    pub source_size: i64,
    pub source_mtime: i64,
    pub destination_id: i64,
    pub send_mode: SendMode,
    pub caption: String,
}

/// `item_update` patch: only for items not started yet.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemPatch {
    pub caption: Option<String>,
    pub send_mode: Option<SendMode>,
}

/// Payload entry of `queue://changed`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemDelta {
    pub id: i64,
    pub state: State,
    pub hold_reason: Option<String>,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Destination {
    pub id: i64,
    pub peer_kind: String,
    pub peer_id: i64,
    pub topic_id: i64,
    pub is_forum: bool,
    pub is_broadcast: bool,
    pub is_saved: bool,
    pub title: String,
    pub alias: Option<String>,
    pub favorite: bool,
    pub min_interval_s: i64,
    pub default_mode: SendMode,
    pub refreshed_at: Option<String>,
    /// `saved` | `private` | `basic_group` | `supergroup` | `channel` (schema v2).
    pub kind: String,
    /// In the Telegram archive at the last refresh.
    pub archived: bool,
    /// Telegram chat folders holding the chat (names for display only).
    pub folders: Vec<ChatFolder>,
    /// Approximate right to post media; `None` = unknown (`destination_check` decides).
    pub can_write: Option<bool>,
    /// Seen in a chat list at the last refresh; false = found by search only, or gone.
    pub listed: bool,
    /// Closed forum topic.
    pub topic_closed: bool,
    /// Last time files were queued for it (`queue_add`), ISO 8601 UTC (schema v3).
    pub last_used_at: Option<String>,
    /// Number of `queue_add` calls that chose it (schema v3).
    pub use_count: i64,
}

/// What Telegram tells us about a destination (cache refresh, search, topics).
#[derive(Debug, Clone)]
pub struct NewDestination {
    pub peer_kind: String,
    pub peer_id: i64,
    /// 0 = whole chat.
    pub topic_id: i64,
    pub is_forum: bool,
    pub is_broadcast: bool,
    pub is_saved: bool,
    pub title: String,
    pub kind: String,
    pub archived: bool,
    pub folders: Vec<ChatFolder>,
    pub can_write: Option<bool>,
    pub listed: bool,
    pub topic_closed: bool,
}

/// Local settings of a destination (`destination_update`). `alias`: absent = unchanged,
/// `null` or blank = removed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DestinationPatch {
    #[serde(default, deserialize_with = "present_or_null")]
    pub alias: Option<Option<String>>,
    pub favorite: Option<bool>,
    pub min_interval_s: Option<i64>,
    pub default_mode: Option<SendMode>,
}

/// A watched folder (`watch_folders`). Its files are queued in `pending_review` only.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WatchFolder {
    pub id: i64,
    pub path: String,
    /// Required by `watch_add`; NULL only in a row written by hand.
    pub destination_id: Option<i64>,
    pub enabled: bool,
    /// `None`: the destination's default mode when a file is found (schema v4).
    pub send_mode: Option<SendMode>,
}

/// `watch_update` patch. `send_mode`: absent = unchanged, `null` = the destination's default.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WatchFolderPatch {
    pub enabled: Option<bool>,
    pub destination_id: Option<i64>,
    #[serde(default, deserialize_with = "present_or_null")]
    pub send_mode: Option<Option<SendMode>>,
}

const WATCH_FOLDER_COLUMNS: &str = "id, path, destination_id, enabled, send_mode";

fn watch_folder_from_row(row: &Row<'_>) -> rusqlite::Result<WatchFolder> {
    let mode: Option<String> = row.get(4)?;
    Ok(WatchFolder {
        id: row.get(0)?,
        path: row.get(1)?,
        destination_id: row.get(2)?,
        enabled: row.get(3)?,
        send_mode: mode
            .map(|mode| SendMode::parse(&mode))
            .transpose()
            .map_err(|e| invalid(4, e))?,
    })
}

fn destination_exists(conn: &Connection, id: i64) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM destinations WHERE id = ?1)",
        [id],
        |row| row.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(QueueError::DestinationNotFound(id))
    }
}

/// Tells a field set to `null` (`Some(None)`) from an absent one (`None`, via `default`).
fn present_or_null<'de, D, T>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Columns of `destinations d`.
const DESTINATION_COLUMNS: &str = "d.id, d.peer_kind, d.peer_id, d.topic_id, d.is_forum,
    d.is_broadcast, d.is_saved, d.title, d.alias, d.favorite, d.min_interval_s, d.default_mode,
    d.refreshed_at, d.kind, d.archived, d.folders, d.can_write, d.listed, d.topic_closed,
    d.last_used_at, d.use_count";

fn folders_json(folders: &[ChatFolder]) -> String {
    serde_json::to_string(folders).unwrap_or_else(|_| "[]".to_owned())
}

fn destination_from_row(row: &Row<'_>) -> rusqlite::Result<Destination> {
    let mode: String = row.get(11)?;
    let folders: String = row.get(15)?;
    Ok(Destination {
        id: row.get(0)?,
        peer_kind: row.get(1)?,
        peer_id: row.get(2)?,
        topic_id: row.get(3)?,
        is_forum: row.get(4)?,
        is_broadcast: row.get(5)?,
        is_saved: row.get(6)?,
        title: row.get(7)?,
        alias: row.get(8)?,
        favorite: row.get(9)?,
        min_interval_s: row.get(10)?,
        default_mode: SendMode::parse(&mode).map_err(|e| invalid(11, e))?,
        refreshed_at: row.get(12)?,
        kind: row.get(13)?,
        archived: row.get(14)?,
        // Written by this app only; unreadable JSON is shown as "no folder".
        folders: serde_json::from_str(&folders).unwrap_or_default(),
        can_write: row.get(16)?,
        listed: row.get(17)?,
        topic_closed: row.get(18)?,
        last_used_at: row.get(19)?,
        use_count: row.get(20)?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryStatus {
    Sent,
    Failed,
    Unknown,
}

impl HistoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HistoryStatus::Sent => "sent",
            HistoryStatus::Failed => "failed",
            HistoryStatus::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub content_id: String,
    pub source_path: String,
    pub file_name: String,
    pub destination_id: i64,
    pub size_sent: i64,
    pub mode: String,
    pub status: String,
    pub duration_s: Option<f64>,
    pub encoded: bool,
    pub message_id: Option<i64>,
    pub sent_at: String,
    pub imported_from: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HistoryFilter {
    pub destination_id: Option<i64>,
    pub status: Option<HistoryStatus>,
    pub content_id: Option<String>,
    /// Inclusive lower bound on `sent_at` (ISO 8601 UTC).
    pub since: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

const HISTORY_DEFAULT_LIMIT: i64 = 100;
const HISTORY_MAX_LIMIT: i64 = 1000;

fn upsert(conn: &Connection, d: &NewDestination) -> Result<i64> {
    let id = conn.query_row(
        "INSERT INTO destinations (peer_kind, peer_id, topic_id, is_forum, is_broadcast,
                                   is_saved, title, refreshed_at, kind, archived, folders,
                                   can_write, listed, topic_closed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT (peer_kind, peer_id, topic_id) DO UPDATE SET
           is_forum = excluded.is_forum, is_broadcast = excluded.is_broadcast,
           is_saved = excluded.is_saved, title = excluded.title,
           refreshed_at = excluded.refreshed_at, kind = excluded.kind,
           archived = excluded.archived, folders = excluded.folders,
           can_write = excluded.can_write, listed = excluded.listed,
           topic_closed = excluded.topic_closed
         RETURNING id",
        params![
            d.peer_kind,
            d.peer_id,
            d.topic_id,
            d.is_forum,
            d.is_broadcast,
            d.is_saved,
            d.title,
            now_iso(),
            d.kind,
            d.archived,
            folders_json(&d.folders),
            d.can_write,
            d.listed,
            d.topic_closed
        ],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// What `queue_clear` removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearScope {
    /// `sent`, `failed` and `cancelled` items.
    Finished,
    /// Every item that is not running: also the waiting, prepared and unconfirmed ones.
    All,
}

impl ClearScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ClearScope::Finished => "finished",
            ClearScope::All => "all",
        }
    }

    fn removes(self, state: State) -> bool {
        match self {
            ClearScope::Finished => matches!(state, State::Sent | State::Failed | State::Cancelled),
            ClearScope::All => !state.is_running(),
        }
    }
}

/// Result of `Store::clear_items`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearOutcome {
    /// Ids of the deleted items.
    pub removed: Vec<i64>,
    /// Items left alone because the orchestrator is working on them.
    pub running: usize,
}

/// A folder path without its trailing separators, so `C:\In\` and `C:\In` are one folder;
/// a root keeps its own (`C:\`, not the drive-relative `C:`).
fn folder_path(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        &path[..(trimmed.len() + 1).min(path.len())]
    } else {
        trimmed
    }
}

/// Paths compare case-insensitively (Windows) and with either separator.
fn path_key(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

/// An encoded file that `Store::reusable_output` found, and the item that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReusableOutput {
    pub item_id: i64,
    pub output_path: String,
    pub output_size: i64,
}

/// Whether two plans (`plan_json`) give the same file. The encoder is left out: it is picked
/// per run (a GPU that stopped earlier in the session is skipped) and every one makes H.264
/// under the same limit, bitrate, scaling and audio, which are what the settings decided.
fn same_plan(a: &str, b: &str) -> bool {
    let without_encoder = |text: &str| {
        let mut plan: serde_json::Value = serde_json::from_str(text).ok()?;
        if let Some(fields) = plan.as_object_mut() {
            fields.remove("encoder");
        }
        Some(plan)
    };
    match (without_encoder(a), without_encoder(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The same file as queued: path, size and modification time.
fn same_source(a: &Item, b: &Item) -> bool {
    a.source_size == b.source_size
        && a.source_mtime == b.source_mtime
        && path_key(&a.source_path) == path_key(&b.source_path)
}

/// The encoded files of `released` (and of the other items of their sources) that no item of
/// `items` needs any more. Several items of one source can share one encoded file (one per
/// destination of a batch), so a file stays while an item that is neither sent nor cancelled
/// holds it (a failed or unknown one keeps it for a retry, F15), or while an item of the same
/// source and mode is still `ready` or `probing`: its probing takes the file over when the plan
/// matches (`Store::reusable_output`), which finds it through the rows of `items` that name it.
/// A path that is the source of an item is never returned.
fn unneeded_outputs(items: &[&Item], released: &[&Item]) -> Vec<String> {
    let everyone = || items.iter().chain(released).copied();
    let sources: HashSet<String> = everyone().map(|item| path_key(&item.source_path)).collect();
    let output_key = |item: &Item| item.output_path.as_deref().map(path_key);
    let mut seen = HashSet::new();
    let mut unneeded = Vec::new();
    for item in released {
        let related = everyone().filter(|other| same_source(item, other));
        for path in item
            .output_path
            .iter()
            .chain(related.filter_map(|other| other.output_path.as_ref()))
        {
            let key = path_key(path);
            if sources.contains(&key) || !seen.insert(key.clone()) {
                continue;
            }
            // A removed row cannot be found by `reusable_output`: its file is reused by no one.
            let owners: Vec<&Item> = items
                .iter()
                .copied()
                .filter(|owner| output_key(owner).as_ref() == Some(&key))
                .collect();
            let needed = items.iter().any(|other| {
                let holds = output_key(other).as_ref() == Some(&key)
                    && !matches!(other.state, State::Sent | State::Cancelled);
                let may_reuse = matches!(other.state, State::Ready | State::Probing)
                    && owners.iter().any(|owner| {
                        owner.send_mode == other.send_mode && same_source(owner, other)
                    });
                holds || may_reuse
            });
            if needed {
                tracing::debug!(
                    event = "encoded_file_kept",
                    item_id = item.id,
                    path = path.as_str(),
                    "encoded file kept: another item still uses it"
                );
            } else {
                unneeded.push(path.clone());
            }
        }
    }
    unneeded
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: db::open(path)?,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            conn: db::open_in_memory()?,
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    // --- destinations ---

    /// Inserts or refreshes a destination; local settings (alias, favorite…) are kept.
    pub fn upsert_destination(&mut self, d: &NewDestination) -> Result<i64> {
        upsert(&self.conn, d)
    }

    /// `destinations_refresh`: the chats of every list, in one transaction. Rows not in
    /// `rows` stay (items refer to them) with `listed = 0`; topic rows follow their chat
    /// (archive, folders, listed, rights). Returns the ids, in the order of `rows`.
    pub fn replace_listed(&mut self, rows: &[NewDestination]) -> Result<Vec<i64>> {
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE destinations SET listed = 0 WHERE listed = 1", [])?;
        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            ids.push(upsert(&tx, row)?);
            tx.execute(
                "UPDATE destinations SET kind = ?1, archived = ?2, folders = ?3,
                   can_write = ?4, listed = ?5, is_forum = ?6
                 WHERE peer_kind = ?7 AND peer_id = ?8 AND topic_id <> 0",
                params![
                    row.kind,
                    row.archived,
                    folders_json(&row.folders),
                    row.can_write,
                    row.listed,
                    row.is_forum,
                    row.peer_kind,
                    row.peer_id
                ],
            )?;
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Private chats of deleted accounts (`Dialog::deleted`): their rows leave the cache.
    /// A row an item, a history line or a watched folder still refers to stays, unnamed and
    /// unlisted (the picker masks it). Returns `(removed, kept)`.
    pub fn purge_deleted(&mut self, peers: &[(String, i64)]) -> Result<(usize, usize)> {
        let tx = self.conn.transaction()?;
        let (mut removed, mut kept) = (0usize, 0usize);
        for (peer_kind, peer_id) in peers {
            removed += tx.execute(
                "DELETE FROM destinations AS d
                 WHERE d.peer_kind = ?1 AND d.peer_id = ?2
                   AND NOT EXISTS (SELECT 1 FROM items i WHERE i.destination_id = d.id)
                   AND NOT EXISTS (SELECT 1 FROM history h WHERE h.destination_id = d.id)
                   AND NOT EXISTS (SELECT 1 FROM watch_folders w WHERE w.destination_id = d.id)",
                params![peer_kind, peer_id],
            )?;
            kept += tx.execute(
                "UPDATE destinations SET title = '', listed = 0, can_write = 0
                 WHERE peer_kind = ?1 AND peer_id = ?2",
                params![peer_kind, peer_id],
            )?;
        }
        tx.commit()?;
        Ok((removed, kept))
    }

    /// A chat found by `destinations_search`: inserted with `listed = 0` if new; if already
    /// cached, only what the search knows is refreshed (title, kind, rights), never its list
    /// flags (archive, folders, listed) nor its local settings.
    pub fn upsert_found(&mut self, d: &NewDestination) -> Result<i64> {
        let id = self.conn.query_row(
            "INSERT INTO destinations (peer_kind, peer_id, topic_id, is_forum, is_broadcast,
                                       is_saved, title, refreshed_at, kind, archived, folders,
                                       can_write, listed, topic_closed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, '[]', ?10, 0, 0)
             ON CONFLICT (peer_kind, peer_id, topic_id) DO UPDATE SET
               is_forum = excluded.is_forum, is_broadcast = excluded.is_broadcast,
               is_saved = excluded.is_saved, title = excluded.title, kind = excluded.kind,
               can_write = excluded.can_write, refreshed_at = excluded.refreshed_at
             RETURNING id",
            params![
                d.peer_kind,
                d.peer_id,
                d.topic_id,
                d.is_forum,
                d.is_broadcast,
                d.is_saved,
                d.title,
                now_iso(),
                d.kind,
                d.can_write
            ],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn get_destination(&self, id: i64) -> Result<Destination> {
        self.conn
            .query_row(
                &format!("SELECT {DESTINATION_COLUMNS} FROM destinations d WHERE d.id = ?1"),
                [id],
                destination_from_row,
            )
            .optional()?
            .ok_or(QueueError::DestinationNotFound(id))
    }

    pub fn list_destinations(&self) -> Result<Vec<Destination>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DESTINATION_COLUMNS} FROM destinations d
             ORDER BY d.favorite DESC, d.title, d.topic_id"
        ))?;
        let rows = stmt.query_map([], destination_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn update_destination(&mut self, id: i64, patch: &DestinationPatch) -> Result<()> {
        let tx = self.conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM destinations WHERE id = ?1)",
            [id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(QueueError::DestinationNotFound(id));
        }
        if let Some(alias) = &patch.alias {
            let alias = alias.as_deref().map(str::trim).filter(|a| !a.is_empty());
            tx.execute(
                "UPDATE destinations SET alias = ?1 WHERE id = ?2",
                params![alias, id],
            )?;
        }
        if let Some(favorite) = patch.favorite {
            tx.execute(
                "UPDATE destinations SET favorite = ?1 WHERE id = ?2",
                params![favorite, id],
            )?;
        }
        if let Some(seconds) = patch.min_interval_s {
            tx.execute(
                "UPDATE destinations SET min_interval_s = ?1 WHERE id = ?2",
                params![seconds.max(0), id],
            )?;
        }
        if let Some(mode) = patch.default_mode {
            tx.execute(
                "UPDATE destinations SET default_mode = ?1 WHERE id = ?2",
                params![mode.as_str(), id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // --- items ---

    /// Appends items in `ready`, in the given order, and marks their destinations as used
    /// (`last_used_at` = now, `use_count` + 1 per call, whatever the number of files). Returns
    /// the item ids.
    pub fn add_items(&mut self, items: &[NewItem]) -> Result<Vec<i64>> {
        let tx = self.conn.transaction()?;
        let mut position: i64 =
            tx.query_row("SELECT COALESCE(MAX(position), 0) FROM items", [], |row| {
                row.get(0)
            })?;
        let now = now_iso();
        let mut ids = Vec::with_capacity(items.len());
        for item in items {
            let exists: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM destinations WHERE id = ?1)",
                [item.destination_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(QueueError::DestinationNotFound(item.destination_id));
            }
            position += 1;
            tx.execute(
                "INSERT INTO items (position, source_path, source_size, source_mtime,
                                    destination_id, send_mode, caption, state, origin,
                                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'manual', ?9, ?9)",
                params![
                    position,
                    item.source_path,
                    item.source_size,
                    item.source_mtime,
                    item.destination_id,
                    item.send_mode.as_str(),
                    item.caption,
                    State::Ready.as_str(),
                    now
                ],
            )?;
            ids.push(tx.last_insert_rowid());
        }
        let used: HashSet<i64> = items.iter().map(|item| item.destination_id).collect();
        for destination_id in used {
            tx.execute(
                "UPDATE destinations SET last_used_at = ?1, use_count = use_count + 1
                 WHERE id = ?2",
                params![now, destination_id],
            )?;
        }
        tx.commit()?;
        Ok(ids)
    }

    pub fn get_item(&self, id: i64) -> Result<Item> {
        self.conn
            .query_row(
                &format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?1"),
                [id],
                item_from_row,
            )
            .optional()?
            .ok_or(QueueError::ItemNotFound(id))
    }

    pub fn list_items(&self) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM items ORDER BY position, id"
        ))?;
        let rows = stmt.query_map([], item_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn items_in_states(&self, states: &[State]) -> Result<Vec<Item>> {
        let placeholders = vec!["?"; states.len()].join(", ");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM items WHERE state IN ({placeholders}) ORDER BY position, id"
        ))?;
        let rows = stmt.query_map(
            params_from_iter(states.iter().map(|state| state.as_str())),
            item_from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Next item the orchestrator may run: `ready`, or `encoded` (retry, or waiting_server
    /// recovered at startup), by position.
    pub fn next_runnable(&self) -> Result<Option<Item>> {
        Ok(self
            .items_in_states(&[State::Ready, State::Encoded])?
            .into_iter()
            .next())
    }

    pub fn transition(&mut self, id: i64, to: State, changes: &Changes) -> Result<State> {
        state::transition(&mut self.conn, id, to, changes)
    }

    /// `pending_review` → `ready`, all or nothing.
    pub fn promote(&mut self, ids: &[i64]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let changes = Changes::new().clear(Column::HoldReason).reason("promoted");
        let mut applied = Vec::with_capacity(ids.len());
        for &id in ids {
            applied.push((id, state::apply(&tx, id, State::Ready, &changes)?));
        }
        tx.commit()?;
        for (id, from) in applied {
            state::log_transition(id, from, State::Ready, &changes);
        }
        Ok(())
    }

    /// A file found in a watched folder: appended in `pending_review` (`hold_reason =
    /// 'watch_pending'`, `origin = 'watch'`), never in `ready`, so only `promote` lets it run
    /// (N4). The destination is not marked as used: the user did not choose it for this file.
    /// Returns `None`, adding nothing, when the same path already waits or runs in the queue
    /// (added by hand, or found before and not finished).
    pub fn add_watched_item(&mut self, item: &NewItem) -> Result<Option<i64>> {
        let tx = self.conn.transaction()?;
        destination_exists(&tx, item.destination_id)?;
        let key = path_key(&item.source_path);
        let queued: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT source_path FROM items WHERE state NOT IN (?1, ?2)")?;
            let rows = stmt.query_map(
                params![State::Sent.as_str(), State::Cancelled.as_str()],
                |row| row.get(0),
            )?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if queued.iter().any(|path| path_key(path) == key) {
            return Ok(None);
        }
        let now = now_iso();
        tx.execute(
            "INSERT INTO items (position, source_path, source_size, source_mtime,
                                destination_id, send_mode, caption, state, hold_reason, origin,
                                created_at, updated_at)
             VALUES ((SELECT COALESCE(MAX(position), 0) + 1 FROM items), ?1, ?2, ?3, ?4, ?5,
                     ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                item.source_path,
                item.source_size,
                item.source_mtime,
                item.destination_id,
                item.send_mode.as_str(),
                item.caption,
                State::PendingReview.as_str(),
                HOLD_WATCH,
                ORIGIN_WATCH,
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Some(id))
    }

    /// Puts `ids` first, in that order; the other items keep their relative order after them.
    /// Returns the ids whose position changed.
    pub fn reorder(&mut self, ids: &[i64]) -> Result<Vec<i64>> {
        let tx = self.conn.transaction()?;
        let current: Vec<(i64, i64)> = {
            let mut stmt = tx.prepare("SELECT id, position FROM items ORDER BY position, id")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            if !current.iter().any(|(existing, _)| existing == id) {
                return Err(QueueError::ItemNotFound(*id));
            }
        }
        let mut order: Vec<i64> = Vec::with_capacity(current.len());
        for &id in ids {
            if !order.contains(&id) {
                order.push(id);
            }
        }
        for &(id, _) in &current {
            if !order.contains(&id) {
                order.push(id);
            }
        }
        let mut changed = Vec::new();
        for (index, id) in order.into_iter().enumerate() {
            let position = index as i64 + 1;
            let old = current
                .iter()
                .find(|(existing, _)| *existing == id)
                .map(|(_, p)| *p);
            if old != Some(position) {
                tx.execute(
                    "UPDATE items SET position = ?1 WHERE id = ?2",
                    params![position, id],
                )?;
                changed.push(id);
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Cancels an item that is not being processed, deletes its temporary file and its encoded
    /// file unless another item still uses it.
    pub fn cancel(&mut self, id: i64) -> Result<()> {
        let item = self.get_item(id)?;
        self.transition(
            id,
            State::Cancelled,
            &Changes::new()
                .clear(Column::TempPath)
                .clear(Column::ProgressPhase)
                .clear(Column::NextRunAt)
                .reason("cancelled"),
        )?;
        self.discard_files(&item)
    }

    /// Deletes `item`'s temporary file (its own) and the encoded files of its source that no
    /// item needs any more (`unneeded_outputs`); never a source.
    pub fn discard_files(&self, item: &Item) -> Result<()> {
        if let Some(temp) = &item.temp_path {
            remove_quietly(temp);
        }
        self.release_outputs(item)
    }

    /// Deletes the encoded files of `item`'s source that no item needs any more, once `item`
    /// has left a state that kept them (sent, cancelled, or probed without taking them over).
    pub fn release_outputs(&self, item: &Item) -> Result<()> {
        let items = self.list_items()?;
        let items: Vec<&Item> = items.iter().collect();
        for path in unneeded_outputs(&items, &[item]) {
            remove_quietly(&path);
        }
        Ok(())
    }

    /// `queue_clear`: deletes the items `scope` selects, except those being worked on, in one
    /// transaction. `current` is the item the orchestrator holds (it may still be `encoded`
    /// while it waits for its turn to upload): never removed. Their temporary and encoded
    /// files go too, once the rows are gone; a path that is the source of any item, or the
    /// file of an item that stays, is never deleted, nor an encoded file that an item that
    /// stays may still take over (`unneeded_outputs`).
    pub fn clear_items(&mut self, scope: ClearScope, current: Option<i64>) -> Result<ClearOutcome> {
        let tx = self.conn.transaction()?;
        let items: Vec<Item> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM items ORDER BY position, id"
            ))?;
            let rows = stmt.query_map([], item_from_row)?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let (removed, kept): (Vec<&Item>, Vec<&Item>) = items
            .iter()
            .partition(|item| scope.removes(item.state) && Some(item.id) != current);
        let running = kept
            .iter()
            .filter(|item| item.state.is_running() || Some(item.id) == current)
            .count();
        for item in &removed {
            tx.execute("DELETE FROM items WHERE id = ?1", [item.id])?;
        }
        tx.commit()?;

        let protected: HashSet<String> = items
            .iter()
            .map(|item| path_key(&item.source_path))
            .chain(
                kept.iter()
                    .flat_map(|item| [item.temp_path.as_deref(), item.output_path.as_deref()])
                    .flatten()
                    .map(path_key),
            )
            .collect();
        for temp in removed.iter().filter_map(|item| item.temp_path.as_deref()) {
            if !protected.contains(&path_key(temp)) {
                remove_quietly(temp);
            }
        }
        for path in unneeded_outputs(&kept, &removed) {
            remove_quietly(&path);
        }
        Ok(ClearOutcome {
            removed: removed.iter().map(|item| item.id).collect(),
            running,
        })
    }

    /// `history_clear`: deletes every history line (and so the duplicate memory).
    /// Items and destinations are not touched. Returns the number of lines deleted.
    pub fn clear_history(&mut self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM history", [])?)
    }

    /// Caption and mode, only while the item has not started.
    pub fn update_item(&mut self, id: i64, patch: &ItemPatch) -> Result<()> {
        let tx = self.conn.transaction()?;
        if !state::current_state(&tx, id)?.is_not_started() {
            return Err(QueueError::ItemStarted(id));
        }
        if let Some(caption) = &patch.caption {
            tx.execute(
                "UPDATE items SET caption = ?1, updated_at = ?2 WHERE id = ?3",
                params![caption, now_iso(), id],
            )?;
        }
        if let Some(mode) = patch.send_mode {
            tx.execute(
                "UPDATE items SET send_mode = ?1, updated_at = ?2 WHERE id = ?3",
                params![mode.as_str(), now_iso(), id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// User retry of a `failed` or `unknown` item: `encoded` if the encoded file still exists,
    /// otherwise `ready`. Never called by the orchestrator.
    pub fn retry(&mut self, id: i64) -> Result<State> {
        let item = self.get_item(id)?;
        let encoded_exists = item
            .output_path
            .as_deref()
            .is_some_and(|path| Path::new(path).is_file());
        let to = if encoded_exists {
            State::Encoded
        } else {
            State::Ready
        };
        let mut changes = Changes::new()
            .clear(Column::LastError)
            .clear(Column::NextRunAt)
            .clear(Column::ProgressPhase)
            .reason(if encoded_exists {
                "user_retry_encoded_kept"
            } else {
                "user_retry_from_start"
            });
        if !encoded_exists {
            changes = changes.clear(Column::OutputPath).clear(Column::OutputSize);
        }
        self.transition(id, to, &changes)?;
        Ok(to)
    }

    /// `unknown` → `sent` confirmed by the user (`item_resolve_unknown`), with the message id
    /// of a reconciliation candidate or without; writes a `sent` history row. Returns the item.
    pub fn mark_sent(&mut self, id: i64, message_id: Option<i64>) -> Result<Item> {
        self.transition(
            id,
            State::Sent,
            &Changes::new()
                .set(Column::TgMessageId, message_id)
                .set(Column::TgSentAt, now_iso())
                .clear(Column::LastError)
                .clear(Column::ProgressPhase)
                .clear(Column::NextRunAt)
                .reason("user_mark_sent"),
        )?;
        let item = self.get_item(id)?;
        self.record_history(&item, HistoryStatus::Sent)?;
        Ok(item)
    }

    /// The encoded file of another item that item `id` can send as its own: same content, same
    /// mode, a plan that gives the same file (`same_plan`), still on disk. The newest wins.
    pub fn reusable_output(
        &self,
        id: i64,
        content_id: &str,
        send_mode: SendMode,
        plan_json: Option<&str>,
    ) -> Result<Option<ReusableOutput>> {
        let Some(plan_json) = plan_json else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT id, output_path, output_size, plan_json FROM items
             WHERE id <> ?1 AND content_id = ?2 AND send_mode = ?3 AND plan_json IS NOT NULL
               AND output_path IS NOT NULL AND output_size IS NOT NULL
               AND output_path <> source_path
             ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![id, content_id, send_mode.as_str()], |row| {
            Ok((
                ReusableOutput {
                    item_id: row.get(0)?,
                    output_path: row.get(1)?,
                    output_size: row.get(2)?,
                },
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (candidate, other_plan) = row?;
            if same_plan(plan_json, &other_plan) && Path::new(&candidate.output_path).is_file() {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    /// `probing` → `encoded` on the file `reusable_output` finds, with `identity` (the probe's
    /// columns). Found and taken in one call on the store: no `cancel` or `queue_clear` can
    /// delete the file in between, which would leave the item `encoded` on a missing file.
    pub fn take_over_output(
        &mut self,
        id: i64,
        content_id: &str,
        send_mode: SendMode,
        plan_json: Option<&str>,
        identity: Changes,
    ) -> Result<Option<ReusableOutput>> {
        let Some(reused) = self.reusable_output(id, content_id, send_mode, plan_json)? else {
            return Ok(None);
        };
        self.transition(
            id,
            State::Encoded,
            &identity
                .set(Column::OutputPath, reused.output_path.clone())
                .set(Column::OutputSize, reused.output_size)
                .clear(Column::ProgressPhase)
                .reason("encode_reused"),
        )?;
        Ok(Some(reused))
    }

    /// True when this content was already sent to this destination.
    pub fn is_duplicate(&self, content_id: &str, destination_id: i64) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM history
                            WHERE content_id = ?1 AND destination_id = ?2 AND status = 'sent')",
            params![content_id, destination_id],
            |row| row.get(0),
        )?)
    }

    /// `probing` → `pending_review` with `hold_reason = 'duplicate'`.
    pub fn hold_duplicate(&mut self, id: i64, content_id: &str, identity_weak: bool) -> Result<()> {
        self.transition(
            id,
            State::PendingReview,
            &Changes::new()
                .set(Column::ContentId, content_id.to_owned())
                .set(Column::IdentityWeak, identity_weak)
                .set(Column::HoldReason, HOLD_DUPLICATE.to_owned())
                .clear(Column::ProgressPhase)
                .reason("duplicate_held"),
        )?;
        Ok(())
    }

    /// Writes a history row for an item outcome. Items without identity (failed before
    /// probing finished) have no history.
    pub fn record_history(&mut self, item: &Item, status: HistoryStatus) -> Result<Option<i64>> {
        let Some(content_id) = &item.content_id else {
            return Ok(None);
        };
        let file_name = Path::new(&item.source_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| item.source_path.clone());
        let encoded = item
            .output_path
            .as_deref()
            .is_some_and(|out| out != item.source_path);
        self.conn.execute(
            "INSERT INTO history (content_id, source_path, file_name, destination_id, size_sent,
                                  mode, status, encoded, message_id, sent_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                content_id,
                item.source_path,
                file_name,
                item.destination_id,
                item.output_size.unwrap_or(item.source_size),
                item.send_mode.as_str(),
                status.as_str(),
                encoded,
                item.tg_message_id,
                item.tg_sent_at.clone().unwrap_or_else(now_iso)
            ],
        )?;
        Ok(Some(self.conn.last_insert_rowid()))
    }

    pub fn query_history(&self, filter: &HistoryFilter) -> Result<Vec<HistoryEntry>> {
        let mut sql = String::from(
            "SELECT id, content_id, source_path, file_name, destination_id, size_sent, mode,
                    status, duration_s, encoded, message_id, sent_at, imported_from
             FROM history WHERE 1 = 1",
        );
        let mut values: Vec<Value> = Vec::new();
        if let Some(destination_id) = filter.destination_id {
            sql.push_str(" AND destination_id = ?");
            values.push(destination_id.into());
        }
        if let Some(status) = filter.status {
            sql.push_str(" AND status = ?");
            values.push(status.as_str().to_owned().into());
        }
        if let Some(content_id) = &filter.content_id {
            sql.push_str(" AND content_id = ?");
            values.push(content_id.clone().into());
        }
        if let Some(since) = &filter.since {
            sql.push_str(" AND sent_at >= ?");
            values.push(since.clone().into());
        }
        sql.push_str(" ORDER BY sent_at DESC, id DESC LIMIT ? OFFSET ?");
        let limit = filter
            .limit
            .unwrap_or(HISTORY_DEFAULT_LIMIT)
            .clamp(1, HISTORY_MAX_LIMIT);
        values.push(limit.into());
        values.push(filter.offset.unwrap_or(0).max(0).into());
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), |row| {
            Ok(HistoryEntry {
                id: row.get(0)?,
                content_id: row.get(1)?,
                source_path: row.get(2)?,
                file_name: row.get(3)?,
                destination_id: row.get(4)?,
                size_sent: row.get(5)?,
                mode: row.get(6)?,
                status: row.get(7)?,
                duration_s: row.get(8)?,
                encoded: row.get(9)?,
                message_id: row.get(10)?,
                sent_at: row.get(11)?,
                imported_from: row.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Items whose last attempt left an error code, most recent first: codes, states and
    /// counters only (no path, no caption), for `debug_export`.
    pub fn recent_errors(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, state, last_error, attempts, send_mode, destination_id, updated_at
             FROM items WHERE last_error IS NOT NULL ORDER BY updated_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |row| {
            Ok(serde_json::json!({
                "item_id": row.get::<_, i64>(0)?,
                "state": row.get::<_, String>(1)?,
                "error_code": row.get::<_, String>(2)?,
                "attempts": row.get::<_, i64>(3)?,
                "send_mode": row.get::<_, String>(4)?,
                "destination_id": row.get::<_, i64>(5)?,
                "updated_at": row.get::<_, String>(6)?,
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // --- watched folders ---

    pub fn list_watch_folders(&self) -> Result<Vec<WatchFolder>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {WATCH_FOLDER_COLUMNS} FROM watch_folders ORDER BY id"
        ))?;
        let rows = stmt.query_map([], watch_folder_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_watch_folder(&self, id: i64) -> Result<WatchFolder> {
        self.conn
            .query_row(
                &format!("SELECT {WATCH_FOLDER_COLUMNS} FROM watch_folders WHERE id = ?1"),
                [id],
                watch_folder_from_row,
            )
            .optional()?
            .ok_or(QueueError::WatchFolderNotFound(id))
    }

    /// Watches `path`, enabled, stored without trailing separators. A folder already watched
    /// under the same path (compared as Windows does: case, separators) is returned as it is,
    /// not added twice.
    pub fn add_watch_folder(
        &mut self,
        path: &str,
        destination_id: i64,
        send_mode: Option<SendMode>,
    ) -> Result<WatchFolder> {
        destination_exists(&self.conn, destination_id)?;
        let path = folder_path(path);
        let key = path_key(path);
        if let Some(existing) = self
            .list_watch_folders()?
            .into_iter()
            .find(|folder| path_key(folder_path(&folder.path)) == key)
        {
            return Ok(existing);
        }
        self.conn.execute(
            "INSERT INTO watch_folders (path, destination_id, enabled, send_mode)
             VALUES (?1, ?2, 1, ?3)",
            params![path, destination_id, send_mode.map(SendMode::as_str)],
        )?;
        self.get_watch_folder(self.conn.last_insert_rowid())
    }

    pub fn update_watch_folder(
        &mut self,
        id: i64,
        patch: &WatchFolderPatch,
    ) -> Result<WatchFolder> {
        let tx = self.conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM watch_folders WHERE id = ?1)",
            [id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(QueueError::WatchFolderNotFound(id));
        }
        if let Some(enabled) = patch.enabled {
            tx.execute(
                "UPDATE watch_folders SET enabled = ?1 WHERE id = ?2",
                params![enabled, id],
            )?;
        }
        if let Some(destination_id) = patch.destination_id {
            destination_exists(&tx, destination_id)?;
            tx.execute(
                "UPDATE watch_folders SET destination_id = ?1 WHERE id = ?2",
                params![destination_id, id],
            )?;
        }
        if let Some(mode) = patch.send_mode {
            tx.execute(
                "UPDATE watch_folders SET send_mode = ?1 WHERE id = ?2",
                params![mode.map(SendMode::as_str), id],
            )?;
        }
        tx.commit()?;
        self.get_watch_folder(id)
    }

    /// Stops watching; the items already found stay in the queue.
    pub fn remove_watch_folder(&mut self, id: i64) -> Result<()> {
        match self
            .conn
            .execute("DELETE FROM watch_folders WHERE id = ?1", [id])?
        {
            0 => Err(QueueError::WatchFolderNotFound(id)),
            _ => Ok(()),
        }
    }

    pub fn deltas(&self, ids: &[i64]) -> Result<Vec<ItemDelta>> {
        let mut deltas = Vec::with_capacity(ids.len());
        for &id in ids {
            let item = self.get_item(id)?;
            deltas.push(ItemDelta {
                id: item.id,
                state: item.state,
                hold_reason: item.hold_reason,
                position: item.position,
            });
        }
        Ok(deltas)
    }
}

pub fn remove_quietly(path: &str) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            event = "file_delete_failed",
            path,
            error = %error,
            "could not delete a queue file"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::{
        TestDir, add_item, destination, first_destination, force_state, memory_store,
    };

    #[test]
    fn add_items_appends_ready_items_in_order() {
        let mut store = memory_store();
        let a = add_item(&mut store, "a.mp4");
        let b = add_item(&mut store, "b.mp4");
        let items = store.list_items().unwrap();
        assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), [a, b]);
        assert!(
            items
                .iter()
                .all(|i| i.state == State::Ready && i.origin == "manual")
        );
        assert_eq!(items[0].position + 1, items[1].position);
    }

    #[test]
    fn add_items_rejects_an_unknown_destination() {
        let mut store = memory_store();
        let item = NewItem {
            source_path: "a.mp4".into(),
            source_size: 1,
            source_mtime: 1,
            destination_id: 404,
            send_mode: SendMode::Video,
            caption: String::new(),
        };
        assert!(matches!(
            store.add_items(&[item]),
            Err(QueueError::DestinationNotFound(404))
        ));
        assert!(store.list_items().unwrap().is_empty());
    }

    #[test]
    fn reorder_puts_given_ids_first_and_keeps_the_rest() {
        let mut store = memory_store();
        let ids: Vec<i64> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| add_item(&mut store, name))
            .collect();
        let changed = store.reorder(&[ids[2], ids[0]]).unwrap();
        let order: Vec<i64> = store.list_items().unwrap().iter().map(|i| i.id).collect();
        assert_eq!(order, [ids[2], ids[0], ids[1], ids[3]]);
        assert_eq!(changed, [ids[2], ids[0], ids[1]]);
        assert!(matches!(
            store.reorder(&[999]),
            Err(QueueError::ItemNotFound(999))
        ));
    }

    #[test]
    fn promote_is_all_or_nothing() {
        let mut store = memory_store();
        let held = add_item(&mut store, "a.mp4");
        let ready = add_item(&mut store, "b.mp4");
        force_state(&store, held, State::PendingReview);
        store
            .conn()
            .execute(
                "UPDATE items SET hold_reason = 'duplicate' WHERE id = ?1",
                [held],
            )
            .unwrap();
        assert!(store.promote(&[held, ready]).is_err());
        assert_eq!(store.get_item(held).unwrap().state, State::PendingReview);
        store.promote(&[held]).unwrap();
        let item = store.get_item(held).unwrap();
        assert_eq!(item.state, State::Ready);
        assert_eq!(item.hold_reason, None);
    }

    #[test]
    fn duplicate_is_detected_per_destination_and_held() {
        let mut store = memory_store();
        let other = store.upsert_destination(&destination(77, 0)).unwrap();
        let id = add_item(&mut store, "a.mp4");
        let item = store.get_item(id).unwrap();
        // Already sent to the item's destination.
        let mut sent = item.clone();
        sent.content_id = Some("10:abc".into());
        store.record_history(&sent, HistoryStatus::Sent).unwrap();
        // Failed elsewhere does not count.
        let mut failed = sent.clone();
        failed.destination_id = other;
        store
            .record_history(&failed, HistoryStatus::Failed)
            .unwrap();

        assert!(store.is_duplicate("10:abc", item.destination_id).unwrap());
        assert!(!store.is_duplicate("10:abc", other).unwrap());
        assert!(!store.is_duplicate("10:zzz", item.destination_id).unwrap());

        force_state(&store, id, State::Probing);
        store.hold_duplicate(id, "10:abc", false).unwrap();
        let held = store.get_item(id).unwrap();
        assert_eq!(held.state, State::PendingReview);
        assert_eq!(held.hold_reason.as_deref(), Some(HOLD_DUPLICATE));
        assert_eq!(held.content_id.as_deref(), Some("10:abc"));
    }

    #[test]
    fn update_item_only_before_start() {
        let mut store = memory_store();
        let id = add_item(&mut store, "a.mp4");
        let patch = ItemPatch {
            caption: Some("hello".into()),
            send_mode: Some(SendMode::Document),
        };
        store.update_item(id, &patch).unwrap();
        let item = store.get_item(id).unwrap();
        assert_eq!(item.caption, "hello");
        assert_eq!(item.send_mode, SendMode::Document);
        force_state(&store, id, State::Encoding);
        assert!(matches!(
            store.update_item(id, &patch),
            Err(QueueError::ItemStarted(_))
        ));
    }

    #[test]
    fn cancel_deletes_temp_and_encoded_files_but_not_the_source() {
        let dir = TestDir::new("store-cancel");
        let source = dir.file("source.mp4", b"src");
        let output = dir.file("source_sq.mp4", b"out");
        let temp = dir.file("source_sq.mp4.part", b"part");
        let mut store = memory_store();
        let id = add_item(&mut store, &source);
        force_state(&store, id, State::Failed);
        store
            .conn()
            .execute(
                "UPDATE items SET output_path = ?1, temp_path = ?2 WHERE id = ?3",
                params![output, temp, id],
            )
            .unwrap();
        store.cancel(id).unwrap();
        assert_eq!(store.get_item(id).unwrap().state, State::Cancelled);
        assert!(Path::new(&source).exists());
        assert!(!Path::new(&output).exists());
        assert!(!Path::new(&temp).exists());
        assert!(matches!(
            store.cancel(id),
            Err(QueueError::IllegalTransition { .. })
        ));
    }

    /// An item in `state` with an encoded file and a temporary file next to its source.
    fn item_with_files(
        store: &mut Store,
        dir: &TestDir,
        name: &str,
        state: State,
    ) -> (i64, [String; 3]) {
        let source = dir.file(&format!("{name}.mp4"), b"src");
        let output = dir.file(&format!("{name}_sq.mp4"), b"out");
        let temp = dir.file(&format!("{name}_sq.mp4.part"), b"part");
        let id = add_item(store, &source);
        force_state(store, id, state);
        store
            .conn()
            .execute(
                "UPDATE items SET output_path = ?1, temp_path = ?2 WHERE id = ?3",
                params![output, temp, id],
            )
            .unwrap();
        (id, [source, output, temp])
    }

    fn remaining_ids(store: &Store) -> Vec<i64> {
        store.list_items().unwrap().iter().map(|i| i.id).collect()
    }

    #[test]
    fn clear_all_keeps_running_items_and_deletes_the_others_with_their_files() {
        let dir = TestDir::new("store-clear-all");
        let mut store = memory_store();
        let mut cleared = Vec::new();
        let mut kept = Vec::new();
        for state in State::ALL {
            let entry = item_with_files(&mut store, &dir, state.as_str(), state);
            if state.is_running() {
                kept.push(entry);
            } else {
                cleared.push(entry);
            }
        }
        assert_eq!((cleared.len(), kept.len()), (7, 5));

        let outcome = store.clear_items(ClearScope::All, None).unwrap();
        assert_eq!(outcome.running, 5);
        assert_eq!(
            outcome.removed,
            cleared.iter().map(|(id, _)| *id).collect::<Vec<_>>()
        );
        assert_eq!(
            remaining_ids(&store),
            kept.iter().map(|(id, _)| *id).collect::<Vec<_>>()
        );
        for (_, [source, output, temp]) in &cleared {
            assert!(Path::new(source).exists(), "the source is never deleted");
            assert!(!Path::new(output).exists(), "encoded file deleted");
            assert!(!Path::new(temp).exists(), "temporary file deleted");
        }
        for (_, files) in &kept {
            assert!(files.iter().all(|file| Path::new(file).exists()));
        }
        // Nothing left to remove.
        let again = store.clear_items(ClearScope::All, None).unwrap();
        assert!(again.removed.is_empty());
        assert_eq!(again.running, 5);
    }

    #[test]
    fn clear_finished_only_removes_sent_failed_and_cancelled() {
        let dir = TestDir::new("store-clear-finished");
        let mut store = memory_store();
        let mut by_state = Vec::new();
        for state in State::ALL {
            by_state.push((
                state,
                item_with_files(&mut store, &dir, state.as_str(), state),
            ));
        }
        let outcome = store.clear_items(ClearScope::Finished, None).unwrap();
        let removed_states: Vec<State> = by_state
            .iter()
            .filter(|(_, (id, _))| outcome.removed.contains(id))
            .map(|(state, _)| *state)
            .collect();
        assert_eq!(
            removed_states,
            [State::Sent, State::Failed, State::Cancelled]
        );
        assert_eq!(outcome.running, 5);
        assert_eq!(remaining_ids(&store).len(), 9);
        for (state, (_, [source, output, _])) in &by_state {
            assert!(Path::new(source).exists());
            let deleted = removed_states.contains(state);
            assert_eq!(!Path::new(output).exists(), deleted, "{state}");
        }
    }

    #[test]
    fn clear_never_removes_the_current_item() {
        let dir = TestDir::new("store-clear-current");
        let mut store = memory_store();
        let (waiting, files) = item_with_files(&mut store, &dir, "a", State::Encoded);
        let (other, _) = item_with_files(&mut store, &dir, "b", State::Encoded);
        let outcome = store.clear_items(ClearScope::All, Some(waiting)).unwrap();
        assert_eq!(outcome.removed, [other]);
        assert_eq!(outcome.running, 1);
        assert_eq!(remaining_ids(&store), [waiting]);
        assert!(files.iter().all(|file| Path::new(file).exists()));
    }

    #[test]
    fn clear_spares_files_that_a_kept_item_or_a_source_still_uses() {
        let dir = TestDir::new("store-clear-shared");
        let mut store = memory_store();
        let shared = dir.file("shared_sq.mp4", b"out");
        let other_source = dir.file("other.mp4", b"src");
        let skipped_source = dir.file("skipped.mp4", b"src");
        // Sent, but its output path now holds the encoded file of an item that stays.
        let sent = add_item(&mut store, &dir.file("a.mp4", b"src"));
        let encoded = add_item(&mut store, &dir.file("b.mp4", b"src"));
        // Failed, with an output path that is another item's source (a "skip" plan).
        let failed = add_item(&mut store, &skipped_source);
        let borrowed = add_item(&mut store, &other_source);
        force_state(&store, sent, State::Sent);
        force_state(&store, encoded, State::Uploading);
        force_state(&store, failed, State::Failed);
        for (id, output) in [
            (sent, &shared),
            (encoded, &shared),
            (failed, &other_source),
            (borrowed, &other_source),
        ] {
            store
                .conn()
                .execute(
                    "UPDATE items SET output_path = ?1 WHERE id = ?2",
                    params![output, id],
                )
                .unwrap();
        }
        let outcome = store.clear_items(ClearScope::All, None).unwrap();
        assert_eq!(outcome.removed, [sent, failed, borrowed]);
        assert!(Path::new(&shared).exists(), "used by an item that stays");
        assert!(
            Path::new(&other_source).exists(),
            "a source is never deleted"
        );
        assert!(Path::new(&skipped_source).exists());
    }

    /// One file queued for three destinations: `first` and `second` in video mode, `third` as
    /// a document. Returns the store, the ids, and the source and encoded paths.
    fn one_file_three_destinations(dir: &TestDir) -> (Store, [i64; 3], String, String) {
        let mut store = memory_store();
        let chats = [
            first_destination(&store),
            store.upsert_destination(&destination(77, 0)).unwrap(),
            store.upsert_destination(&destination(78, 3)).unwrap(),
        ];
        let source = dir.file("clip.mp4", b"src");
        let output = dir.file("clip_sq.mp4", b"out");
        let new = |destination_id, send_mode| NewItem {
            source_path: source.clone(),
            source_size: 3,
            source_mtime: 1,
            destination_id,
            send_mode,
            caption: String::new(),
        };
        let ids = store
            .add_items(&[
                new(chats[0], SendMode::Video),
                new(chats[1], SendMode::Video),
                new(chats[2], SendMode::Document),
            ])
            .unwrap();
        (store, [ids[0], ids[1], ids[2]], source, output)
    }

    fn set_output(store: &Store, id: i64, output: &str) {
        store
            .conn()
            .execute(
                "UPDATE items SET output_path = ?1, output_size = 3 WHERE id = ?2",
                params![output, id],
            )
            .unwrap();
    }

    fn release(store: &Store, id: i64) {
        store.release_outputs(&store.get_item(id).unwrap()).unwrap();
    }

    #[test]
    fn a_shared_encoded_file_stays_until_no_item_can_use_it() {
        let dir = TestDir::new("store-shared-output");
        let (store, [first, second, document], source, output) = one_file_three_destinations(&dir);
        set_output(&store, first, &output);
        force_state(&store, first, State::Sent);
        release(&store, first);
        assert!(Path::new(&output).exists(), "second may still reuse it");
        force_state(&store, second, State::Probing);
        release(&store, first);
        assert!(Path::new(&output).exists(), "second is probing");

        // Second took it over and its upload failed: kept for a retry.
        set_output(&store, second, &output);
        force_state(&store, second, State::Failed);
        release(&store, first);
        assert!(Path::new(&output).exists());

        // The document item, still waiting, never reuses a video encode.
        force_state(&store, second, State::Cancelled);
        assert_eq!(store.get_item(document).unwrap().state, State::Ready);
        release(&store, second);
        assert!(!Path::new(&output).exists(), "no item can use it any more");
        assert!(Path::new(&source).exists());
    }

    #[test]
    fn cancel_and_clear_spare_an_encoded_file_another_destination_still_uses() {
        let dir = TestDir::new("store-shared-cancel");
        let (mut store, [first, second, _], source, output) = one_file_three_destinations(&dir);
        for id in [first, second] {
            set_output(&store, id, &output);
            force_state(&store, id, State::Encoded);
        }
        store.cancel(first).unwrap();
        assert!(Path::new(&output).exists(), "second still sends it");
        force_state(&store, second, State::Unknown);
        let outcome = store.clear_items(ClearScope::Finished, None).unwrap();
        assert_eq!(outcome.removed, [first]);
        assert!(Path::new(&output).exists(), "second may be retried");
        force_state(&store, second, State::Sent);
        // The document item that stays never reuses a video encode.
        let outcome = store.clear_items(ClearScope::Finished, None).unwrap();
        assert_eq!(outcome.removed, [second]);
        assert!(!Path::new(&output).exists());
        assert!(Path::new(&source).exists());
    }

    #[test]
    fn clear_drops_an_encoded_file_whose_rows_are_gone() {
        let dir = TestDir::new("store-shared-clear");
        let (mut store, [first, second, _], _, output) = one_file_three_destinations(&dir);
        set_output(&store, first, &output);
        force_state(&store, first, State::Sent);
        store
            .release_outputs(&store.get_item(first).unwrap())
            .unwrap();
        assert!(Path::new(&output).exists());
        // Without the sent row, nothing can find the file to reuse it.
        store.clear_items(ClearScope::Finished, None).unwrap();
        assert!(!Path::new(&output).exists());
        assert_eq!(store.get_item(second).unwrap().state, State::Ready);
    }

    /// A `queue_clear` of the item that made the file, on either side of the take-over: the
    /// probing item either finds nothing (and encodes its own) or holds the file, never an
    /// `encoded` item on a deleted file.
    #[test]
    fn take_over_and_clear_never_leave_an_item_on_a_missing_file() {
        let plan = r#"{"kind":"encode","encoder":"h264_nvenc"}"#;
        let identity = || Changes::new().set(Column::ContentId, "3:abc".to_owned());
        for clear_first in [true, false] {
            let dir = TestDir::new("store-take-over");
            let (mut store, [first, second, _], _, output) = one_file_three_destinations(&dir);
            set_output(&store, first, &output);
            force_state(&store, first, State::Sent);
            force_state(&store, second, State::Probing);
            store
                .conn()
                .execute(
                    "UPDATE items SET content_id = '3:abc', plan_json = ?1 WHERE id = ?2",
                    params![plan, first],
                )
                .unwrap();
            let take = |store: &mut Store| {
                store
                    .take_over_output(second, "3:abc", SendMode::Video, Some(plan), identity())
                    .unwrap()
            };
            if clear_first {
                store.clear_items(ClearScope::Finished, None).unwrap();
                assert_eq!(take(&mut store), None);
                assert_eq!(store.get_item(second).unwrap().state, State::Probing);
            } else {
                assert_eq!(take(&mut store).unwrap().item_id, first);
                store.clear_items(ClearScope::Finished, None).unwrap();
                let item = store.get_item(second).unwrap();
                assert_eq!(item.state, State::Encoded);
                assert_eq!(item.output_path.as_deref(), Some(output.as_str()));
                assert!(Path::new(&output).exists(), "held by the item that took it");
            }
        }
    }

    #[test]
    fn reusable_output_needs_the_same_content_mode_and_plan_and_the_file() {
        let dir = TestDir::new("store-reusable");
        let (store, [first, second, document], source, output) = one_file_three_destinations(&dir);
        let plan = |encoder: &str, limit: u64| {
            serde_json::json!({ "kind": "encode", "encoder": encoder, "limit_bytes": limit })
                .to_string()
        };
        set_output(&store, first, &output);
        force_state(&store, first, State::Sent);
        store
            .conn()
            .execute(
                "UPDATE items SET content_id = '3:abc', plan_json = ?1 WHERE id = ?2",
                params![plan("h264_nvenc", 100), first],
            )
            .unwrap();
        let found = |id, content: &str, mode, plan: &str| {
            store
                .reusable_output(id, content, mode, Some(plan))
                .unwrap()
                .map(|reused| (reused.item_id, reused.output_path))
        };
        // Another encoder (a GPU that stopped this session) gives the same file.
        assert_eq!(
            found(second, "3:abc", SendMode::Video, &plan("libx264", 100)),
            Some((first, output.clone()))
        );
        assert_eq!(
            found(second, "3:abc", SendMode::Video, &plan("h264_nvenc", 200)),
            None,
            "another limit"
        );
        assert_eq!(
            found(
                document,
                "3:abc",
                SendMode::Document,
                &plan("h264_nvenc", 100)
            ),
            None,
            "another mode"
        );
        assert_eq!(
            found(second, "3:xyz", SendMode::Video, &plan("h264_nvenc", 100)),
            None,
            "another content"
        );
        assert_eq!(
            found(first, "3:abc", SendMode::Video, &plan("h264_nvenc", 100)),
            None,
            "not its own file"
        );
        // Sent as is: the output is the source, nothing to reuse.
        set_output(&store, first, &source);
        assert_eq!(
            found(second, "3:abc", SendMode::Video, &plan("h264_nvenc", 100)),
            None
        );
        set_output(&store, first, &output);
        std::fs::remove_file(&output).unwrap();
        assert_eq!(
            found(second, "3:abc", SendMode::Video, &plan("h264_nvenc", 100)),
            None,
            "file gone"
        );
    }

    #[test]
    fn clear_history_forgets_duplicates_and_leaves_items_and_destinations() {
        let mut store = memory_store();
        let id = add_item(&mut store, "dir/a.mp4");
        let mut item = store.get_item(id).unwrap();
        item.content_id = Some("1:a".into());
        store.record_history(&item, HistoryStatus::Sent).unwrap();
        store.record_history(&item, HistoryStatus::Failed).unwrap();
        let destinations = store.list_destinations().unwrap().len();
        assert!(store.is_duplicate("1:a", item.destination_id).unwrap());

        assert_eq!(store.clear_history().unwrap(), 2);
        assert!(
            store
                .query_history(&HistoryFilter::default())
                .unwrap()
                .is_empty()
        );
        assert!(!store.is_duplicate("1:a", item.destination_id).unwrap());
        assert_eq!(remaining_ids(&store), [id]);
        assert_eq!(store.list_destinations().unwrap().len(), destinations);
        assert_eq!(store.clear_history().unwrap(), 0);
    }

    #[test]
    fn retry_goes_to_encoded_only_if_the_encoded_file_exists() {
        let dir = TestDir::new("store-retry");
        let output = dir.file("a_sq.mp4", b"out");
        let mut store = memory_store();
        let kept = add_item(&mut store, "a.mp4");
        let lost = add_item(&mut store, "b.mp4");
        for (id, path) in [(kept, output.as_str()), (lost, "missing_sq.mp4")] {
            force_state(&store, id, State::Unknown);
            store
                .conn()
                .execute(
                    "UPDATE items SET output_path = ?1 WHERE id = ?2",
                    params![path, id],
                )
                .unwrap();
        }
        assert_eq!(store.retry(kept).unwrap(), State::Encoded);
        assert_eq!(store.retry(lost).unwrap(), State::Ready);
        assert_eq!(store.get_item(lost).unwrap().output_path, None);
        assert!(store.retry(kept).is_err(), "encoded cannot be retried");
    }

    #[test]
    fn destinations_are_unique_per_peer_and_topic() {
        let mut store = memory_store();
        let whole = store.upsert_destination(&destination(5, 0)).unwrap();
        let topic = store.upsert_destination(&destination(5, 3)).unwrap();
        assert_ne!(whole, topic);
        store
            .update_destination(
                whole,
                &DestinationPatch {
                    alias: Some(Some("mine".into())),
                    favorite: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut renamed = destination(5, 0);
        renamed.title = "New title".into();
        assert_eq!(store.upsert_destination(&renamed).unwrap(), whole);
        let refreshed = store.get_destination(whole).unwrap();
        assert_eq!(refreshed.title, "New title");
        assert_eq!(refreshed.alias.as_deref(), Some("mine"), "local alias kept");
        assert!(refreshed.favorite);
        assert_eq!(refreshed.topic_id, 0);

        let raw = store.conn().execute(
            "INSERT INTO destinations (peer_kind, peer_id, title) VALUES ('channel', 5, 'dup')",
            [],
        );
        assert!(
            raw.is_err(),
            "UNIQUE (peer_kind, peer_id, topic_id) with topic_id default 0"
        );
        // test_util's fixture destination + the two above.
        assert_eq!(store.list_destinations().unwrap().len(), 3);
    }

    /// A refresh flags what is in a list, keeps what is not (items refer to it) as unlisted,
    /// and topic rows follow their chat; a search never touches list flags.
    #[test]
    fn refresh_and_search_keep_list_flags_apart() {
        let mut store = memory_store();
        let folder = ChatFolder {
            id: 3,
            name: "Work".into(),
            position: 0,
        };
        let topic = store.upsert_destination(&destination(5, 2)).unwrap();
        let mut forum = destination(5, 0);
        forum.archived = true;
        forum.folders = vec![folder.clone()];
        forum.is_forum = true;
        let ids = store.replace_listed(&[forum.clone()]).unwrap();
        let forum_row = store.get_destination(ids[0]).unwrap();
        assert!(forum_row.archived && forum_row.listed);
        assert_eq!(forum_row.folders, std::slice::from_ref(&folder));
        let topic_row = store.get_destination(topic).unwrap();
        assert!(
            topic_row.archived && topic_row.listed,
            "the topic follows its chat"
        );
        assert_eq!(topic_row.folders, std::slice::from_ref(&folder));
        // memory_store's chat 1 was not in this refresh.
        let gone = store
            .list_destinations()
            .unwrap()
            .into_iter()
            .find(|d| d.peer_id == 1)
            .unwrap();
        assert!(!gone.listed);

        // Found by search: new rows are unlisted; a listed row keeps its flags.
        let mut found = destination(5, 0);
        found.title = "Renamed".into();
        found.listed = false;
        found.archived = false;
        found.folders = Vec::new();
        assert_eq!(store.upsert_found(&found).unwrap(), ids[0]);
        let kept = store.get_destination(ids[0]).unwrap();
        assert_eq!(kept.title, "Renamed");
        assert!(kept.archived && kept.listed);
        assert_eq!(kept.folders, [folder]);
        let mut other = destination(77, 0);
        other.listed = true;
        let new = store.upsert_found(&other).unwrap();
        assert!(!store.get_destination(new).unwrap().listed);
    }

    #[test]
    fn queueing_files_marks_the_destination_as_used() {
        let mut store = memory_store();
        let chat = first_destination(&store);
        let unused = store.upsert_destination(&destination(8, 0)).unwrap();
        let topic = store.upsert_destination(&destination(9, 4)).unwrap();
        let new = |path: &str, destination_id| NewItem {
            source_path: path.into(),
            source_size: 1,
            source_mtime: 1,
            destination_id,
            send_mode: SendMode::Video,
            caption: String::new(),
        };
        // One call with two files is one use; a call with nothing to add is none.
        store
            .add_items(&[new("C:\\a.mp4", chat), new("C:\\b.mp4", chat)])
            .unwrap();
        store.add_items(&[]).unwrap();
        let first = store.get_destination(chat).unwrap();
        assert_eq!(first.use_count, 1);
        let at = first.last_used_at.clone().expect("last_used_at");
        assert!(crate::queue::parse_iso(&at).is_some(), "ISO 8601: {at}");
        assert!(at.ends_with('Z'), "UTC: {at}");
        // A forum topic is a destination of its own.
        store.add_items(&[new("C:\\c.mp4", topic)]).unwrap();
        store.add_items(&[new("C:\\d.mp4", chat)]).unwrap();
        let again = store.get_destination(chat).unwrap();
        assert_eq!(again.use_count, 2);
        assert!(again.last_used_at.unwrap() >= at);
        let topic = store.get_destination(topic).unwrap();
        assert_eq!((topic.use_count, topic.last_used_at.is_some()), (1, true));
        let unused = store.get_destination(unused).unwrap();
        assert_eq!((unused.use_count, unused.last_used_at), (0, None));
        // A refused call (unknown destination) changes nothing.
        assert!(store.add_items(&[new("C:\\e.mp4", 404)]).is_err());
        // Clearing the queue and the history keeps the record.
        store.clear_items(ClearScope::All, None).unwrap();
        store.clear_history().unwrap();
        assert_eq!(store.get_destination(chat).unwrap().use_count, 2);
    }

    #[test]
    fn destination_patch_tells_null_from_absent() {
        let mut store = memory_store();
        let id = store.upsert_destination(&destination(9, 0)).unwrap();
        let patch = |json| serde_json::from_value::<DestinationPatch>(json).unwrap();
        store
            .update_destination(id, &patch(serde_json::json!({ "alias": " Mine " })))
            .unwrap();
        assert_eq!(
            store.get_destination(id).unwrap().alias.as_deref(),
            Some("Mine")
        );
        store
            .update_destination(id, &patch(serde_json::json!({ "favorite": true })))
            .unwrap();
        let kept = store.get_destination(id).unwrap();
        assert_eq!(
            kept.alias.as_deref(),
            Some("Mine"),
            "absent alias unchanged"
        );
        assert!(kept.favorite);
        store
            .update_destination(id, &patch(serde_json::json!({ "alias": null })))
            .unwrap();
        assert_eq!(store.get_destination(id).unwrap().alias, None);
        store
            .update_destination(id, &patch(serde_json::json!({ "alias": "x" })))
            .unwrap();
        store
            .update_destination(id, &patch(serde_json::json!({ "alias": "  " })))
            .unwrap();
        assert_eq!(
            store.get_destination(id).unwrap().alias,
            None,
            "blank removes"
        );
    }

    #[test]
    fn mark_sent_records_a_sent_history_row() {
        let mut store = memory_store();
        let id = add_item(&mut store, "a.mp4");
        force_state(&store, id, State::Unknown);
        store
            .conn()
            .execute(
                "UPDATE items SET content_id = '1:a', last_error = 'TG_TIMEOUT' WHERE id = ?1",
                [id],
            )
            .unwrap();
        let item = store.mark_sent(id, Some(55)).unwrap();
        assert_eq!(item.state, State::Sent);
        assert_eq!(item.tg_message_id, Some(55));
        assert!(item.tg_sent_at.is_some());
        assert_eq!(item.last_error, None);
        assert!(store.is_duplicate("1:a", item.destination_id).unwrap());
        assert!(store.mark_sent(id, None).is_err(), "sent is final");
    }

    #[test]
    fn history_query_filters_and_limits() {
        let mut store = memory_store();
        let id = add_item(&mut store, "dir/a.mp4");
        let mut item = store.get_item(id).unwrap();
        item.content_id = Some("1:a".into());
        store.record_history(&item, HistoryStatus::Sent).unwrap();
        store.record_history(&item, HistoryStatus::Failed).unwrap();
        let all = store.query_history(&HistoryFilter::default()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].file_name, "a.mp4");
        let sent = store
            .query_history(&HistoryFilter {
                status: Some(HistoryStatus::Sent),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(sent.len(), 1);
        let limited = store
            .query_history(&HistoryFilter {
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(limited.len(), 1);
    }

    #[test]
    fn watch_folders_are_added_once_patched_and_removed() {
        let mut store = memory_store();
        let destination_id = first_destination(&store);
        assert!(matches!(
            store.add_watch_folder(r"C:\In", 999, None),
            Err(QueueError::DestinationNotFound(999))
        ));
        let folder = store
            .add_watch_folder(r"C:\In", destination_id, Some(SendMode::Document))
            .unwrap();
        assert!(folder.enabled);
        assert_eq!(folder.send_mode, Some(SendMode::Document));
        // The same folder spelt differently is the same folder, unchanged.
        for spelling in ["c:/in", r"C:\In\", "C:/IN//"] {
            let again = store
                .add_watch_folder(spelling, destination_id, None)
                .unwrap();
            assert_eq!(again, folder, "{spelling}");
        }
        assert_eq!(store.list_watch_folders().unwrap().len(), 1);

        let patch: WatchFolderPatch = serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        let updated = store.update_watch_folder(folder.id, &patch).unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.send_mode, Some(SendMode::Document), "absent: kept");
        let patch: WatchFolderPatch = serde_json::from_str(r#"{"send_mode": null}"#).unwrap();
        let updated = store.update_watch_folder(folder.id, &patch).unwrap();
        assert_eq!(updated.send_mode, None, "null: the destination's default");

        store.remove_watch_folder(folder.id).unwrap();
        assert!(matches!(
            store.remove_watch_folder(folder.id),
            Err(QueueError::WatchFolderNotFound(_))
        ));
        assert!(matches!(
            store.update_watch_folder(folder.id, &WatchFolderPatch::default()),
            Err(QueueError::WatchFolderNotFound(_))
        ));
    }

    #[test]
    fn folder_paths_lose_trailing_separators_but_roots_keep_theirs() {
        assert_eq!(folder_path(r"C:\In\"), r"C:\In");
        assert_eq!(folder_path("C:/In//"), "C:/In");
        assert_eq!(folder_path(r"C:\In"), r"C:\In");
        assert_eq!(folder_path(r"C:\"), r"C:\");
        assert_eq!(folder_path(r"C:\\"), r"C:\");
        assert_eq!(folder_path(r"\\server\share\"), r"\\server\share");
    }

    #[test]
    fn a_watched_file_waits_for_approval_and_is_not_queued_twice() {
        let mut store = memory_store();
        let destination_id = first_destination(&store);
        let item = NewItem {
            source_path: r"C:\In.mp4".into(),
            source_size: 3,
            source_mtime: 1,
            destination_id,
            send_mode: SendMode::Video,
            caption: String::new(),
        };
        let id = store.add_watched_item(&item).unwrap().unwrap();
        let held = store.get_item(id).unwrap();
        assert_eq!(held.state, State::PendingReview);
        assert_eq!(held.hold_reason.as_deref(), Some(HOLD_WATCH));
        assert_eq!(held.origin, ORIGIN_WATCH);
        assert_eq!(store.add_watched_item(&item).unwrap(), None);
        // Once the first one is gone (cancelled), the path may come again.
        store.cancel(id).unwrap();
        assert!(store.add_watched_item(&item).unwrap().is_some());
    }
}
