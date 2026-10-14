//! Internal batch links; kept out of the page's item payload until queue coordination uses them.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, params};

use super::{ITEM_COLUMNS, Item, Store, invalid, item_from_row};
use crate::queue::{QueueError, Result, State, now_iso};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendVia {
    Upload,
    Copy,
    Remote,
}

impl SendVia {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Copy => "copy",
            Self::Remote => "remote",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "upload" => Ok(Self::Upload),
            "copy" => Ok(Self::Copy),
            "remote" => Ok(Self::Remote),
            _ => Err(QueueError::InvalidValue("unknown send route".into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendMetadata {
    pub batch_id: Option<String>,
    pub source_item_id: Option<i64>,
    pub via: SendVia,
}

fn incompatible() -> QueueError {
    QueueError::InvalidValue("incompatible multi-destination batch".into())
}

fn same_media(left: &Item, right: &Item) -> bool {
    left.source_path == right.source_path
        && left.source_size == right.source_size
        && left.source_mtime == right.source_mtime
        && left.send_mode == right.send_mode
        && left.encoding_override == right.encoding_override
}

impl Store {
    pub fn send_metadata(&self, id: i64) -> Result<SendMetadata> {
        self.conn
            .query_row(
                "SELECT batch_id, source_item_id, send_via FROM items WHERE id = ?1",
                [id],
                |row| {
                    let route: String = row.get(2)?;
                    Ok(SendMetadata {
                        batch_id: row.get(0)?,
                        source_item_id: row.get(1)?,
                        via: SendVia::parse(&route).map_err(|error| invalid(2, error))?,
                    })
                },
            )
            .optional()?
            .ok_or(QueueError::ItemNotFound(id))
    }

    /// Link ready items of one file to the first id, atomically. Captions may differ by
    /// destination, but media and encoding choices must agree before an upload can be reused.
    /// This stores intent only; `send_via` records the actual route when the send starts.
    pub fn link_send_batch(&mut self, ids: &[i64]) -> Result<String> {
        if ids.len() < 2 {
            return Err(incompatible());
        }
        let tx = self.conn.transaction()?;
        let mut items = Vec::with_capacity(ids.len());
        let mut destinations = HashSet::new();
        for &id in ids {
            let item = tx
                .query_row(
                    &format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?1"),
                    [id],
                    item_from_row,
                )
                .optional()?
                .ok_or(QueueError::ItemNotFound(id))?;
            if item.state != State::Ready {
                return Err(QueueError::ItemStarted(id));
            }
            if item.hold_reason.is_some() || !destinations.insert(item.destination_id) {
                return Err(incompatible());
            }
            let unlinked: bool = tx.query_row(
                "SELECT batch_id IS NULL AND source_item_id IS NULL AND send_via = 'upload'
                 FROM items WHERE id = ?1",
                [id],
                |row| row.get(0),
            )?;
            if !unlinked || items.first().is_some_and(|first| !same_media(first, &item)) {
                return Err(incompatible());
            }
            items.push(item);
        }
        let batch_id = uuid::Uuid::new_v4().to_string();
        let now = now_iso();
        for &id in ids {
            tx.execute(
                "UPDATE items SET batch_id = ?1, source_item_id = ?2, updated_at = ?3
                 WHERE id = ?4",
                params![batch_id, (id != ids[0]).then_some(ids[0]), now, id],
            )?;
        }
        tx.commit()?;
        Ok(batch_id)
    }

    /// A dependent's primary, or `None` for an independent upload / primary itself. Missing
    /// and reused ids are errors: neither may silently authorize another local upload.
    pub fn send_batch_primary(&self, id: i64) -> Result<Option<Item>> {
        let metadata = self.send_metadata(id)?;
        let Some(source_id) = metadata.source_item_id else {
            return Ok(None);
        };
        let batch_id = metadata.batch_id.as_deref().ok_or_else(incompatible)?;
        if source_id == id {
            return Err(incompatible());
        }
        let primary_metadata = self.send_metadata(source_id)?;
        if primary_metadata.batch_id.as_deref() != Some(batch_id)
            || primary_metadata.source_item_id.is_some()
        {
            return Err(incompatible());
        }
        let item = self.get_item(id)?;
        let primary = self.get_item(source_id)?;
        if !same_media(&item, &primary) {
            return Err(incompatible());
        }
        Ok(Some(primary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::store::{NewItem, SendMode};
    use crate::queue::test_util::{TestDir, destination, force_state};

    fn pair(store: &mut Store) -> Vec<i64> {
        let destinations = [
            store.upsert_destination(&destination(1, 0)).unwrap(),
            store.upsert_destination(&destination(2, 7)).unwrap(),
        ];
        let items: Vec<_> = destinations
            .into_iter()
            .map(|destination_id| NewItem {
                source_path: "generated.mp4".into(),
                source_size: 100,
                source_mtime: 5,
                destination_id,
                send_mode: SendMode::Video,
                caption: format!("caption {destination_id}"),
            })
            .collect();
        store.add_items(&items).unwrap()
    }

    #[test]
    fn links_survive_reopening_and_keep_destination_captions() {
        let dir = TestDir::new("send-batch");
        let path = dir.path().join("queue.db");
        let mut store = Store::open(&path).unwrap();
        let ids = pair(&mut store);
        let before = store.list_items().unwrap();
        let batch = store.link_send_batch(&ids).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.send_metadata(ids[0]).unwrap(),
            SendMetadata {
                batch_id: Some(batch.clone()),
                source_item_id: None,
                via: SendVia::Upload,
            }
        );
        assert_eq!(
            store.send_metadata(ids[1]).unwrap(),
            SendMetadata {
                batch_id: Some(batch),
                source_item_id: Some(ids[0]),
                via: SendVia::Upload,
            }
        );
        assert_eq!(store.send_batch_primary(ids[0]).unwrap(), None);
        assert_eq!(
            store.send_batch_primary(ids[1]).unwrap().unwrap().id,
            ids[0]
        );
        for (old, current) in before.iter().zip(store.list_items().unwrap()) {
            assert_eq!(old.caption, current.caption);
            assert_eq!(old.state, current.state);
            assert_eq!(old.batch_index, current.batch_index);
            assert_eq!(old.batch_count, current.batch_count);
        }
    }

    #[test]
    fn incompatible_or_started_items_leave_every_link_untouched() {
        for column in [
            "source_path",
            "source_size",
            "source_mtime",
            "send_mode",
            "encoding_override",
            "destination_id",
            "hold_reason",
        ] {
            let mut store = Store::open_in_memory().unwrap();
            let ids = pair(&mut store);
            let value = match column {
                "source_path" => "other.mp4",
                "source_size" => "101",
                "source_mtime" => "6",
                "send_mode" => "document",
                "encoding_override" => {
                    r#"{"video_codec":"hevc","max_resolution":"original","target_mb":30}"#
                }
                "destination_id" => "1",
                _ => "watch_pending",
            };
            store
                .conn()
                .execute(
                    &format!("UPDATE items SET {column} = ?1 WHERE id = ?2"),
                    params![value, ids[1]],
                )
                .unwrap();
            assert!(store.link_send_batch(&ids).is_err(), "{column}");
            assert!(
                ids.iter()
                    .all(|&id| store.send_metadata(id).unwrap().batch_id.is_none())
            );
        }
        let mut store = Store::open_in_memory().unwrap();
        let ids = pair(&mut store);
        for state in [
            State::PendingReview,
            State::Encoded,
            State::Uploading,
            State::Sent,
            State::Unknown,
        ] {
            force_state(&store, ids[1], state);
            assert!(store.link_send_batch(&ids).is_err());
        }
        assert!(store.send_metadata(ids[0]).unwrap().batch_id.is_none());
    }

    #[test]
    fn missing_duplicate_and_relinked_members_are_rejected() {
        let mut store = Store::open_in_memory().unwrap();
        let ids = pair(&mut store);
        for invalid in [
            vec![],
            vec![ids[0]],
            vec![ids[0], ids[0]],
            vec![ids[0], 999],
        ] {
            assert!(store.link_send_batch(&invalid).is_err());
            assert!(store.send_metadata(ids[0]).unwrap().batch_id.is_none());
        }
        let batch = store.link_send_batch(&ids).unwrap();
        assert!(store.link_send_batch(&ids).is_err());
        assert_eq!(store.send_metadata(ids[1]).unwrap().batch_id, Some(batch));
    }

    #[test]
    fn a_write_failure_rolls_back_the_whole_batch() {
        let mut store = Store::open_in_memory().unwrap();
        let ids = pair(&mut store);
        store
            .conn()
            .execute_batch(
                "CREATE TRIGGER refuse_second_link BEFORE UPDATE OF batch_id ON items
             WHEN OLD.id = 2 BEGIN SELECT RAISE(ABORT, 'test write failure'); END;",
            )
            .unwrap();
        assert!(store.link_send_batch(&ids).is_err());
        for id in ids {
            assert_eq!(
                store.send_metadata(id).unwrap(),
                SendMetadata {
                    batch_id: None,
                    source_item_id: None,
                    via: SendVia::Upload,
                }
            );
        }
    }

    #[test]
    fn a_deleted_or_reused_primary_never_resolves_to_an_unrelated_item() {
        let mut store = Store::open_in_memory().unwrap();
        let ids = pair(&mut store);
        store.link_send_batch(&ids).unwrap();
        store
            .conn()
            .execute("DELETE FROM items WHERE id = ?1", [ids[0]])
            .unwrap();
        assert!(matches!(
            store.send_batch_primary(ids[1]),
            Err(QueueError::ItemNotFound(_))
        ));
        store
            .conn()
            .execute(
                "INSERT INTO items (id, position, source_path, source_size, source_mtime,
             destination_id, send_mode, state, origin, created_at, updated_at)
             VALUES (?1, 3, 'generated.mp4', 100, 5, 1, 'video', 'sent', 'manual', 'x', 'x')",
                [ids[0]],
            )
            .unwrap();
        assert!(store.send_batch_primary(ids[1]).is_err());
    }
}
