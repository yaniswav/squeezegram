//! SQLite opening and migrations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use super::{QueueError, Result};

/// Migrations in order: entry `i` brings the schema to version `i + 1`.
const MIGRATIONS: &[&str] = &[SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4];

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
        assert_eq!(schema_version(&conn).unwrap(), 4);
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
                "watch_folders"
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
        assert_eq!(schema_version(&conn).unwrap(), 4);
        let row: (String, Option<i64>, bool, Option<String>) = conn
            .query_row(
                "SELECT path, destination_id, enabled, send_mode FROM watch_folders WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, (r"C:\Videos\OBS".to_owned(), Some(1), false, None));
        drop(conn);
        assert_eq!(schema_version(&open(&path).unwrap()).unwrap(), 4);
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
