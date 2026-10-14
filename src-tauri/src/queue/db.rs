//! SQLite opening and migrations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use super::{QueueError, Result};

/// Migrations in order: entry `i` brings the schema to version `i + 1`.
const MIGRATIONS: &[&str] = &[
    SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8,
];

pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

const SCHEMA_V1: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE destinations (
  id            INTEGER PRIMARY KEY,
  peer_kind     TEXT NOT NULL,
  peer_id       INTEGER NOT NULL,
  topic_id      INTEGER NOT NULL DEFAULT 0,
  is_forum      INTEGER NOT NULL DEFAULT 0,
  is_broadcast  INTEGER NOT NULL DEFAULT 0,
  is_saved      INTEGER NOT NULL DEFAULT 0,
  title         TEXT NOT NULL,
  alias         TEXT,
  favorite      INTEGER NOT NULL DEFAULT 0,
  min_interval_s INTEGER NOT NULL DEFAULT 0,
  default_mode  TEXT NOT NULL DEFAULT 'video',
  refreshed_at  TEXT,
  UNIQUE (peer_kind, peer_id, topic_id)
);

CREATE TABLE items (
  id            INTEGER PRIMARY KEY,
  position      INTEGER NOT NULL,
  source_path   TEXT NOT NULL,
  source_size   INTEGER NOT NULL,
  source_mtime  INTEGER NOT NULL,
  content_id    TEXT,
  identity_weak INTEGER NOT NULL DEFAULT 0,
  destination_id INTEGER NOT NULL REFERENCES destinations(id),
  send_mode     TEXT NOT NULL,
  caption       TEXT NOT NULL DEFAULT '',
  state         TEXT NOT NULL,
  hold_reason   TEXT,
  origin        TEXT NOT NULL,
  media_json    TEXT,
  plan_json     TEXT,
  temp_path     TEXT,
  output_path   TEXT,
  output_size   INTEGER,
  progress_phase TEXT,
  progress_pct   REAL,
  attempts      INTEGER NOT NULL DEFAULT 0,
  last_error    TEXT,
  next_run_at   TEXT,
  tg_message_id INTEGER,
  tg_sent_at    TEXT,
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL
);
CREATE INDEX items_state_pos ON items(state, position);

CREATE TABLE history (
  id            INTEGER PRIMARY KEY,
  content_id    TEXT NOT NULL,
  source_path   TEXT NOT NULL,
  file_name     TEXT NOT NULL,
  destination_id INTEGER NOT NULL REFERENCES destinations(id),
  size_sent     INTEGER NOT NULL,
  mode          TEXT NOT NULL,
  status        TEXT NOT NULL,
  duration_s    REAL,
  encoded       INTEGER NOT NULL DEFAULT 0,
  message_id    INTEGER,
  sent_at       TEXT NOT NULL,
  imported_from TEXT
);
CREATE INDEX history_identity ON history(content_id, destination_id);
CREATE INDEX history_path ON history(source_path, destination_id);

CREATE TABLE watch_folders (
  id        INTEGER PRIMARY KEY,
  path      TEXT NOT NULL UNIQUE,
  destination_id INTEGER REFERENCES destinations(id),
  enabled   INTEGER NOT NULL DEFAULT 1,
  stable_seconds INTEGER NOT NULL DEFAULT 10
);
";

/// v2: every chat list (main, archive, folders), chat search and topics on demand.
/// - `kind`: `saved` | `private` | `basic_group` | `supergroup` | `channel` (backfilled).
/// - `archived`: in `chatListArchive` at the last refresh.
/// - `folders`: JSON `[{ "id", "name", "position" }]`, the chat folders holding the chat.
/// - `can_write`: approximate right to post media (NULL: unknown); `destination_check` decides.
/// - `listed`: seen in a chat list at the last refresh (0: found by search only, or gone).
/// - `topic_closed`: closed forum topic (only its creator or managers post there).
const SCHEMA_V2: &str = "
ALTER TABLE destinations ADD COLUMN kind TEXT NOT NULL DEFAULT '';
ALTER TABLE destinations ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
ALTER TABLE destinations ADD COLUMN folders TEXT NOT NULL DEFAULT '[]';
ALTER TABLE destinations ADD COLUMN can_write INTEGER;
ALTER TABLE destinations ADD COLUMN listed INTEGER NOT NULL DEFAULT 1;
ALTER TABLE destinations ADD COLUMN topic_closed INTEGER NOT NULL DEFAULT 0;
UPDATE destinations SET kind = CASE
  WHEN is_saved = 1 THEN 'saved'
  WHEN peer_kind = 'user' THEN 'private'
  WHEN peer_kind = 'basic_group' THEN 'basic_group'
  WHEN is_broadcast = 1 THEN 'channel'
  ELSE 'supergroup'
END;
CREATE INDEX items_destination ON items(destination_id);
";

/// v3: recently used destinations, written by `queue_add` (`Store::add_items`).
/// - `last_used_at`: last time files were queued for it (ISO 8601 UTC), NULL if never.
/// - `use_count`: number of `queue_add` calls that chose it.
///
/// Backfill: `last_used_at` takes what v2 derived on every read (newest item or history line
/// of the destination), so the "recent" list does not start empty; the past number of uses is
/// not known, so `use_count` is 1 for a destination used before, 0 otherwise.
const SCHEMA_V3: &str = "
ALTER TABLE destinations ADD COLUMN last_used_at TEXT;
ALTER TABLE destinations ADD COLUMN use_count INTEGER NOT NULL DEFAULT 0;
UPDATE destinations SET last_used_at = (SELECT MAX(at) FROM (
    SELECT MAX(i.created_at) AS at FROM items i WHERE i.destination_id = destinations.id
    UNION ALL
    SELECT MAX(h.sent_at) FROM history h WHERE h.destination_id = destinations.id
));
UPDATE destinations SET use_count = 1 WHERE last_used_at IS NOT NULL;
";

/// v4: send mode of a watched folder (`watch::`), chosen when the folder is added.
/// - `send_mode`: `video` | `document` | `photo`; NULL = the destination's default mode, read
///   when a file is found (as `queue_add` without a mode).
///
/// No backfill: no version before this one wrote `watch_folders`.
const SCHEMA_V4: &str = "
ALTER TABLE watch_folders ADD COLUMN send_mode TEXT;
";

/// v5: position of an item in the batch one `queue_add` created, for the `{index}` and
/// `{count}` caption variables (rendered at send time, long after the call).
/// - `batch_index`: 1-based, among the items of the same destination in that call.
/// - `batch_count`: number of those items.
///
/// Items that exist already, and files found by a watched folder, are a batch of one.
const SCHEMA_V5: &str = "
ALTER TABLE items ADD COLUMN batch_index INTEGER NOT NULL DEFAULT 1;
ALTER TABLE items ADD COLUMN batch_count INTEGER NOT NULL DEFAULT 1;
";

/// v6: per-file encoding choices (`advanced_encoding`, F14).
/// - `encoding_override`: JSON `{ "video_codec", "max_resolution", "target_mb" }` of
///   `media::plan::EncodingOverride`; NULL = the global settings apply.
///
/// No backfill: every existing item keeps NULL, which is what it was encoded or will be
/// encoded with.
const SCHEMA_V6: &str = "
ALTER TABLE items ADD COLUMN encoding_override TEXT;
";

/// v7: what the watcher has already dealt with in a watched folder, so a file that arrives
/// while the app is closed is found at the next start (`watch::`).
/// - `watch_folders.seeded`: the folder's files at that time were recorded in `watch_seen`.
///   0 until the watcher first lists the folder (and again when the folder is disabled).
/// - `watch_seen`: one row per file left alone or taken: name, size and modification time
///   (seconds) as the directory listing showed them.
///
/// No backfill: the folder's files are only known by listing it, so an existing folder starts
/// with `seeded = 0` and is seeded from its content at the first listing after the upgrade.
/// Every file already there is then left alone, as the per-start baseline did before.
const SCHEMA_V7: &str = "
ALTER TABLE watch_folders ADD COLUMN seeded INTEGER NOT NULL DEFAULT 0;
CREATE TABLE watch_seen (
  folder_id INTEGER NOT NULL REFERENCES watch_folders(id) ON DELETE CASCADE,
  name      TEXT NOT NULL,
  size      INTEGER NOT NULL,
  mtime     INTEGER NOT NULL,
  PRIMARY KEY (folder_id, name)
) WITHOUT ROWID;
";

/// v8: links and send routes for multi-destination batches. Existing rows remain independent
/// uploads: their original queue-add boundaries cannot be reconstructed reliably.
/// `source_item_id` is kept even if the primary row is cleared; queue coordination must
/// resolve a missing source explicitly instead of silently treating it as a new upload.
const SCHEMA_V8: &str = "
ALTER TABLE items ADD COLUMN batch_id TEXT;
ALTER TABLE items ADD COLUMN source_item_id INTEGER;
ALTER TABLE items ADD COLUMN send_via TEXT NOT NULL DEFAULT 'upload'
    CHECK (send_via IN ('copy', 'remote', 'upload'));
ALTER TABLE history ADD COLUMN via TEXT NOT NULL DEFAULT 'upload'
    CHECK (via IN ('copy', 'remote', 'upload'));
";

/// `%APPDATA%\squeezegram\squeezegram.db`.
pub fn default_path() -> Result<PathBuf> {
    let app_data = std::env::var_os("APPDATA").ok_or(QueueError::NoAppData)?;
    Ok(PathBuf::from(app_data)
        .join("squeezegram")
        .join("squeezegram.db"))
}

/// Opens (or creates) the database at `path` and brings its schema up to date.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    prepare(Connection::open(path)?)
}

pub fn open_in_memory() -> Result<Connection> {
    prepare(Connection::open_in_memory()?)
}

fn prepare(mut conn: Connection) -> Result<Connection> {
    // An in-memory database answers "memory": WAL only applies to files.
    let _mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    migrate(&mut conn)?;
    Ok(conn)
}

pub fn schema_version(conn: &Connection) -> Result<i64> {
    let has_meta: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
        [],
        |row| row.get(0),
    )?;
    if !has_meta {
        return Ok(0);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match value {
        None => Ok(0),
        Some(text) => text
            .parse()
            .map_err(|_| QueueError::InvalidValue(format!("schema_version = {text}"))),
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current = schema_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(QueueError::SchemaTooNew(current));
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = index as i64 + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![version.to_string()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn fresh_database_gets_the_latest_schema() {
        let conn = open_in_memory().unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            tables(&conn),
            [
                "destinations",
                "history",
                "history_identity",
                "history_path",
                "items",
                "items_destination",
                "items_state_pos",
                "meta",
                "watch_folders",
                "watch_seen"
            ]
        );
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn file_database_uses_wal_and_reopens_without_migrating_again() {
        let dir = TestDir::new("db-reopen");
        let path = dir.path().join("nested").join("squeezegram.db");
        {
            let conn = open(&path).unwrap();
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('marker', 'kept')",
                [],
            )
            .unwrap();
        }
        let conn = open(&path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        let marker: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'marker'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(marker, "kept");
    }

    /// A v1 database (destinations of the main list only, an item and its history) moves to
    /// the latest schema in place: rows, ids and references kept, `kind` backfilled, new columns defaulted.
    #[test]
    fn v1_database_migrates_to_v2() {
        let dir = TestDir::new("db-v1-v2");
        let path = dir.path().join("squeezegram.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(SCHEMA_V1).unwrap();
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '1')",
                [],
            )
            .unwrap();
            tx.execute_batch(
                "INSERT INTO destinations (id, peer_kind, peer_id, topic_id, is_forum,
                     is_broadcast, is_saved, title, alias, favorite)
                 VALUES (1, 'user', 7, 0, 0, 0, 1, 'Saved Messages', NULL, 1),
                        (2, 'user', 8, 0, 0, 0, 0, 'Bob', 'Bobby', 0),
                        (3, 'basic_group', 9, 0, 0, 0, 0, 'Family', NULL, 0),
                        (4, 'channel', 10, 0, 0, 1, 0, 'News', NULL, 0),
                        (5, 'channel', 11, 2, 1, 0, 0, 'Forum › Videos', NULL, 0);
                 INSERT INTO items (position, source_path, source_size, source_mtime,
                     destination_id, send_mode, state, origin, created_at, updated_at)
                 VALUES (1, 'a.mp4', 1, 1, 5, 'video', 'sent', 'manual', 'x', 'x');
                 INSERT INTO history (content_id, source_path, file_name, destination_id,
                     size_sent, mode, status, sent_at)
                 VALUES ('1:a', 'a.mp4', 'a.mp4', 5, 1, 'video', 'sent', 'x');",
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, archived, folders, can_write, listed, topic_closed, alias,
                        favorite
                 FROM destinations ORDER BY id",
            )
            .unwrap();
        type Row = (
            i64,
            String,
            bool,
            String,
            Option<bool>,
            bool,
            bool,
            Option<String>,
            bool,
        );
        let rows: Vec<Row> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let kinds: Vec<(i64, &str)> = rows.iter().map(|r| (r.0, r.1.as_str())).collect();
        assert_eq!(
            kinds,
            [
                (1, "saved"),
                (2, "private"),
                (3, "basic_group"),
                (4, "channel"),
                (5, "supergroup")
            ]
        );
        for row in &rows {
            // Main-list rows: listed, not archived, no folder, rights unknown.
            assert_eq!(
                (row.2, row.3.as_str(), row.4, row.5, row.6),
                (false, "[]", None, true, false)
            );
        }
        assert_eq!(rows[1].7.as_deref(), Some("Bobby"), "local settings kept");
        assert!(rows[0].8);
        // References still valid.
        let history: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM history h JOIN destinations d ON d.id = h.destination_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(history, 1);
    }

    /// A v2 database moves to v3 in place: rows, ids, local settings and references kept;
    /// `last_used_at` backfilled from the newest item or history line, `use_count` 1 for a
    /// destination used before and 0 for the others.
    #[test]
    fn v2_database_migrates_to_v3() {
        let dir = TestDir::new("db-v2-v3");
        let path = dir.path().join("squeezegram.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(SCHEMA_V1).unwrap();
            tx.execute_batch(SCHEMA_V2).unwrap();
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '2')",
                [],
            )
            .unwrap();
            tx.execute_batch(
                "INSERT INTO destinations (id, peer_kind, peer_id, topic_id, is_forum,
                     is_broadcast, is_saved, title, alias, favorite, kind, archived, folders)
                 VALUES (1, 'user', 7, 0, 0, 0, 1, 'Saved Messages', NULL, 1, 'saved', 0, '[]'),
                        (2, 'user', 8, 0, 0, 0, 0, 'Bob', 'Bobby', 0, 'private', 1, '[]'),
                        (3, 'channel', 11, 2, 1, 0, 0, 'Forum › Videos', NULL, 0,
                         'supergroup', 0, '[{\"id\":2,\"name\":\"Work\",\"position\":0}]'),
                        (4, 'channel', 12, 0, 0, 0, 0, 'Never', NULL, 0, 'supergroup', 0, '[]');
                 INSERT INTO items (position, source_path, source_size, source_mtime,
                     destination_id, send_mode, state, origin, created_at, updated_at)
                 VALUES (1, 'a.mp4', 1, 1, 3, 'video', 'ready', 'manual',
                         '2026-10-01T10:00:00.000Z', '2026-10-01T10:00:00.000Z'),
                        (2, 'b.mp4', 1, 1, 2, 'video', 'sent', 'manual',
                         '2026-09-01T10:00:00.000Z', '2026-09-01T10:00:00.000Z');
                 INSERT INTO history (content_id, source_path, file_name, destination_id,
                     size_sent, mode, status, sent_at)
                 VALUES ('1:b', 'b.mp4', 'b.mp4', 2, 1, 'video', 'sent',
                         '2026-09-02T10:00:00.000Z'),
                        ('1:c', 'c.mp4', 'c.mp4', 1, 1, 'video', 'sent',
                         '2026-08-01T10:00:00.000Z');",
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        let mut stmt = conn
            .prepare(
                "SELECT id, last_used_at, use_count, alias, favorite, archived, folders
                 FROM destinations ORDER BY id",
            )
            .unwrap();
        type Row = (i64, Option<String>, i64, Option<String>, bool, bool, String);
        let rows: Vec<Row> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let used: Vec<(i64, Option<&str>, i64)> =
            rows.iter().map(|r| (r.0, r.1.as_deref(), r.2)).collect();
        assert_eq!(
            used,
            [
                (1, Some("2026-08-01T10:00:00.000Z"), 1),
                // The newest of the item and the history line.
                (2, Some("2026-09-02T10:00:00.000Z"), 1),
                (3, Some("2026-10-01T10:00:00.000Z"), 1),
                (4, None, 0)
            ]
        );
        assert_eq!(rows[1].3.as_deref(), Some("Bobby"), "local settings kept");
        assert!(rows[0].4 && rows[1].5);
        assert!(rows[2].6.contains("Work"));
        let references: i64 = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM items i JOIN destinations d ON d.id = i.destination_id)
                      + (SELECT COUNT(*) FROM history h JOIN destinations d ON d.id = h.destination_id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(references, 4);
        // Reopening does not migrate again.
        drop(stmt);
        drop(conn);
        assert_eq!(
            schema_version(&open(&path).unwrap()).unwrap(),
            SCHEMA_VERSION
        );
    }

    /// A v3 database moves to v4 in place: a watched folder keeps its row, its destination and
    /// its flags, and gets `send_mode = NULL` (the destination's default mode).
    #[test]
    fn v3_database_migrates_to_v4() {
        let dir = TestDir::new("db-v3-v4");
        let path = dir.path().join("squeezegram.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for sql in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3] {
                tx.execute_batch(sql).unwrap();
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '3')",
                [],
            )
            .unwrap();
            tx.execute_batch(
                "INSERT INTO destinations (id, peer_kind, peer_id, title)
                 VALUES (1, 'user', 7, 'Saved Messages');
                 INSERT INTO watch_folders (id, path, destination_id, enabled)
                 VALUES (1, 'C:\\Videos\\OBS', 1, 0);",
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        let row: (String, Option<i64>, bool, Option<String>) = conn
            .query_row(
                "SELECT path, destination_id, enabled, send_mode FROM watch_folders WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, (r"C:\Videos\OBS".to_owned(), Some(1), false, None));
        drop(conn);
        assert_eq!(
            schema_version(&open(&path).unwrap()).unwrap(),
            SCHEMA_VERSION
        );
    }

    /// A v4 database moves to v5 in place: queued items keep their rows and become a batch
    /// of one.
    #[test]
    fn v4_database_migrates_to_v5() {
        let dir = TestDir::new("db-v4-v5");
        let path = dir.path().join("squeezegram.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for sql in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4] {
                tx.execute_batch(sql).unwrap();
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '4')",
                [],
            )
            .unwrap();
            tx.execute_batch(
                "INSERT INTO destinations (id, peer_kind, peer_id, title)
                 VALUES (1, 'user', 7, 'Saved Messages');
                 INSERT INTO items (id, position, source_path, source_size, source_mtime,
                     destination_id, send_mode, caption, state, origin, created_at, updated_at)
                 VALUES (3, 1, 'a.mp4', 1, 1, 1, 'video', '{name}', 'ready', 'manual', 'x', 'x');",
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        let row: (String, i64, i64) = conn
            .query_row(
                "SELECT caption, batch_index, batch_count FROM items WHERE id = 3",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("{name}".to_owned(), 1, 1));
    }

    /// A v5 database moves to v6 in place: queued items keep their rows and read as "no
    /// per-file choices", the global settings keep applying to them.
    #[test]
    fn v5_database_migrates_to_v6() {
        use crate::media::plan::{EncodingOverride, ResolutionCap, VideoCodec};
        use crate::queue::store::Store;

        let dir = TestDir::new("db-v5-v6");
        let path = dir.path().join("squeezegram.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for sql in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5] {
                tx.execute_batch(sql).unwrap();
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '5')",
                [],
            )
            .unwrap();
            tx.execute_batch(
                "INSERT INTO destinations (id, peer_kind, peer_id, title)
                 VALUES (1, 'user', 7, 'Saved Messages');
                 INSERT INTO items (id, position, source_path, source_size, source_mtime,
                     destination_id, send_mode, caption, state, origin, created_at, updated_at,
                     batch_index, batch_count, plan_json)
                 VALUES (3, 1, 'a.mp4', 1, 1, 1, 'video', 'hi', 'ready', 'manual', 'x', 'x', 2, 4, NULL),
                        (4, 2, 'b.mp4', 1, 1, 1, 'video', '', 'encoded', 'manual', 'x', 'x', 1, 1,
                         '{\"kind\":\"encode\"}');",
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        assert_eq!(schema_version(store.conn()).unwrap(), SCHEMA_VERSION);
        let items = store.list_items().unwrap();
        assert_eq!(items.len(), 2);
        for item in &items {
            assert_eq!(item.encoding_override, None, "item {}", item.id);
        }
        // What v5 had is untouched.
        let first = &items[0];
        assert_eq!(
            (first.caption.as_str(), first.batch_index, first.batch_count),
            ("hi", 2, 4)
        );
        assert_eq!(items[1].plan_json.as_deref(), Some("{\"kind\":\"encode\"}"));

        // The new column works: set, read back, reset.
        let choices = EncodingOverride {
            video_codec: VideoCodec::Hevc,
            max_resolution: ResolutionCap::Original,
            target_mb: Some(30),
        };
        store.set_encoding_override(3, Some(&choices)).unwrap();
        assert_eq!(store.get_item(3).unwrap().encoding_override, Some(choices));
        store.set_encoding_override(3, None).unwrap();
        assert_eq!(store.get_item(3).unwrap().encoding_override, None);
        // Reopening does not migrate again.
        drop(store);
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    /// A v6 database moves to v7 in place: a watched folder keeps its row and flags and is not
    /// seeded yet (the watcher seeds it from the folder's content at its first listing, which
    /// is what keeps an upgrade from queueing every file already there); the new table
    /// follows the folder when it is removed.
    #[test]
    fn v6_database_migrates_to_v7() {
        let dir = TestDir::new("db-v6-v7");
        let path = dir.path().join("squeezegram.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for sql in [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6,
            ] {
                tx.execute_batch(sql).unwrap();
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '6')",
                [],
            )
            .unwrap();
            tx.execute_batch(
                "INSERT INTO destinations (id, peer_kind, peer_id, title)
                 VALUES (1, 'user', 7, 'Saved Messages');
                 INSERT INTO watch_folders (id, path, destination_id, enabled, send_mode)
                 VALUES (1, 'C:\\Videos\\OBS', 1, 1, 'document'),
                        (2, 'C:\\Videos\\Off', 1, 0, NULL);",
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        let mut stmt = conn
            .prepare(
                "SELECT id, path, destination_id, enabled, send_mode, seeded
                 FROM watch_folders ORDER BY id",
            )
            .unwrap();
        type Row = (i64, String, Option<i64>, bool, Option<String>, bool);
        let rows: Vec<Row> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                (
                    1,
                    r"C:\Videos\OBS".to_owned(),
                    Some(1),
                    true,
                    Some("document".to_owned()),
                    false
                ),
                (2, r"C:\Videos\Off".to_owned(), Some(1), false, None, false)
            ]
        );
        drop(stmt);
        let seen = |conn: &Connection| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM watch_seen", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(seen(&conn), 0, "nothing is invented for an old folder");
        conn.execute(
            "INSERT INTO watch_seen (folder_id, name, size, mtime) VALUES (1, 'a.mp4', 1, 2)",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM watch_folders WHERE id = 1", [])
            .unwrap();
        assert_eq!(seen(&conn), 0, "removed with its folder");
    }

    #[test]
    fn v7_database_migrates_to_v8_without_grouping_existing_items() {
        let dir = TestDir::new("db-v7-v8");
        let path = dir.path().join("squeezegram.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for sql in MIGRATIONS.iter().take(7) {
                tx.execute_batch(sql).unwrap();
            }
            tx.execute_batch(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '7');
                 INSERT INTO destinations (id, peer_kind, peer_id, title)
                 VALUES (1, 'user', 7, 'Saved Messages');
                 INSERT INTO items (id, position, source_path, source_size, source_mtime,
                     destination_id, send_mode, caption, state, origin, created_at, updated_at,
                     batch_index, batch_count, output_path, tg_message_id)
                 VALUES (3, 1, 'a.mp4', 100, 2, 1, 'video', 'caption', 'encoded',
                         'manual', 'x', 'x', 2, 4, 'a_sq.mp4', NULL),
                        (4, 2, 'a.mp4', 100, 2, 1, 'video', '', 'sent',
                         'manual', 'x', 'x', 1, 1, NULL, 123);
                 INSERT INTO history (id, content_id, source_path, file_name, destination_id,
                     size_sent, mode, status, message_id, sent_at)
                 VALUES (5, '100:hash', 'a.mp4', 'a.mp4', 1, 90, 'video', 'sent', 123, 'x');",
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        type Row = (
            i64,
            String,
            String,
            Option<String>,
            Option<i64>,
            i64,
            i64,
            Option<String>,
            Option<i64>,
            String,
        );
        let rows: Vec<Row> = conn
            .prepare(
                "SELECT id, state, caption, output_path, tg_message_id, batch_index, batch_count,
                        batch_id, source_item_id, send_via FROM items ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                (
                    3,
                    "encoded".into(),
                    "caption".into(),
                    Some("a_sq.mp4".into()),
                    None,
                    2,
                    4,
                    None,
                    None,
                    "upload".into()
                ),
                (
                    4,
                    "sent".into(),
                    "".into(),
                    None,
                    Some(123),
                    1,
                    1,
                    None,
                    None,
                    "upload".into()
                ),
            ]
        );
        let history: (i64, String, i64, String) = conn
            .query_row(
                "SELECT id, content_id, message_id, via FROM history",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(history, (5, "100:hash".into(), 123, "upload".into()));
        conn.execute(
            "UPDATE items SET batch_id = 'batch-1', source_item_id = 4, send_via = 'copy'
             WHERE id = 3",
            [],
        )
        .unwrap();
        conn.execute("UPDATE history SET via = 'remote' WHERE id = 5", [])
            .unwrap();
        drop(conn);
        let conn = open(&path).unwrap();
        let links: (String, i64, String) = conn
            .query_row(
                "SELECT batch_id, source_item_id, send_via FROM items WHERE id = 3",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(links, ("batch-1".into(), 4, "copy".into()));
        let via: String = conn
            .query_row("SELECT via FROM history WHERE id = 5", [], |r| r.get(0))
            .unwrap();
        assert_eq!(via, "remote");
        for route in ["copy", "remote", "upload"] {
            conn.execute("UPDATE items SET send_via = ?1 WHERE id = 3", [route])
                .unwrap();
            conn.execute("UPDATE history SET via = ?1 WHERE id = 5", [route])
                .unwrap();
        }
        for invalid in [Some("forward"), Some(""), None] {
            assert!(
                conn.execute("UPDATE items SET send_via = ?1 WHERE id = 3", [invalid])
                    .is_err()
            );
            assert!(
                conn.execute("UPDATE history SET via = ?1 WHERE id = 5", [invalid])
                    .is_err()
            );
        }
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = TestDir::new("db-newer");
        let path = dir.path().join("squeezegram.db");
        {
            let conn = open(&path).unwrap();
            conn.execute(
                "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        }
        assert!(matches!(open(&path), Err(QueueError::SchemaTooNew(99))));
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let conn = open_in_memory().unwrap();
        let result = conn.execute(
            "INSERT INTO items (position, source_path, source_size, source_mtime, destination_id,
                                send_mode, state, origin, created_at, updated_at)
             VALUES (1, 'a', 1, 1, 42, 'video', 'ready', 'manual', 'x', 'x')",
            [],
        );
        assert!(result.is_err());
    }
}
