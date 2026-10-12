//! Startup and clean-shutdown rules.

use super::state::{Changes, Column};
use super::store::{HistoryStatus, Store, remove_quietly};
use super::{Result, State};

/// At startup: interrupted media work goes back to `ready` (its temporary file is deleted),
/// an interrupted upload becomes `unknown`, a server wait goes back to `encoded` with its
/// `next_run_at`. Returns the ids that changed.
///
/// Only the temporary file of interrupted work is deleted: it belongs to that item alone. An
/// encoded file is never deleted here, since items of the same source can share it (one per
/// destination) and the `unknown`, `encoded` and `ready` items left by the recovery may still
/// send it.
pub fn on_startup(store: &mut Store) -> Result<Vec<i64>> {
    let mut changed = Vec::new();
    let media = store.items_in_states(&[State::Probing, State::Encoding, State::Remuxing])?;
    let media_count = media.len();
    for item in media {
        if let Some(temp) = &item.temp_path {
            remove_quietly(temp);
        }
        store.transition(
            item.id,
            State::Ready,
            &Changes::new()
                .clear(Column::TempPath)
                .clear(Column::ProgressPhase)
                .clear(Column::ProgressPct)
                .reason("startup_recovery"),
        )?;
        changed.push(item.id);
    }
    let unknown = uploads_to_unknown(store, "startup_recovery")?;
    let unknown_count = unknown.len();
    changed.extend(unknown);
    let waiting = store.items_in_states(&[State::WaitingServer])?;
    let waiting_count = waiting.len();
    for item in waiting {
        store.transition(
            item.id,
            State::Encoded,
            &Changes::new()
                .clear(Column::ProgressPhase)
                .reason("startup_recovery"),
        )?;
        changed.push(item.id);
    }
    tracing::info!(
        event = "recovery",
        media_to_ready = media_count,
        uploading_to_unknown = unknown_count,
        waiting_to_encoded = waiting_count,
        "startup recovery applied"
    );
    Ok(changed)
}

/// On a clean shutdown: an item still uploading may have reached Telegram just before
/// `close()`, so it becomes `unknown`, never `ready`.
pub fn on_clean_shutdown(store: &mut Store) -> Result<Vec<i64>> {
    uploads_to_unknown(store, "clean_shutdown")
}

fn uploads_to_unknown(store: &mut Store, reason: &'static str) -> Result<Vec<i64>> {
    let mut changed = Vec::new();
    for item in store.items_in_states(&[State::Uploading])? {
        store.transition(
            item.id,
            State::Unknown,
            &Changes::new().clear(Column::ProgressPhase).reason(reason),
        )?;
        let updated = store.get_item(item.id)?;
        store.record_history(&updated, HistoryStatus::Unknown)?;
        changed.push(item.id);
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::{TestDir, add_item, force_state, memory_store};
    use rusqlite::params;
    use std::path::Path;

    #[test]
    fn startup_applies_the_spec_rules() {
        let dir = TestDir::new("recovery");
        let mut store = memory_store();
        let mut ids = std::collections::HashMap::new();
        let mut temps = Vec::new();
        for state in State::ALL {
            let id = add_item(&mut store, &format!("{state}.mp4"));
            force_state(&store, id, state);
            let temp = dir.file(&format!("{state}.part"), b"x");
            let output = dir.file(&format!("{state}_sq.mp4"), b"x");
            store
                .conn()
                .execute(
                    "UPDATE items SET temp_path = ?1, output_path = ?2, content_id = 'c',
                                      next_run_at = '2030-01-01T00:00:00.000Z' WHERE id = ?3",
                    params![temp, output, id],
                )
                .unwrap();
            ids.insert(state, id);
            temps.push((state, temp, output));
        }

        let changed = on_startup(&mut store).unwrap();
        assert_eq!(changed.len(), 5);

        for state in State::ALL {
            let item = store.get_item(ids[&state]).unwrap();
            let expected = match state {
                State::Probing | State::Encoding | State::Remuxing => State::Ready,
                State::Uploading => State::Unknown,
                State::WaitingServer => State::Encoded,
                other => other,
            };
            assert_eq!(item.state, expected, "from {state}");
        }
        let waiting = store.get_item(ids[&State::WaitingServer]).unwrap();
        assert_eq!(
            waiting.next_run_at.as_deref(),
            Some("2030-01-01T00:00:00.000Z")
        );
        for (state, temp, output) in temps {
            let media = matches!(state, State::Probing | State::Encoding | State::Remuxing);
            assert_eq!(Path::new(&temp).exists(), !media, "temp of {state}");
            assert!(
                Path::new(&output).exists(),
                "encoded file of {state} is kept"
            );
        }
        let history = store.query_history(&Default::default()).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, "unknown");

        assert!(on_startup(&mut store).unwrap().is_empty(), "idempotent");
    }

    /// One file queued for several destinations shares one encoded file: the recovery keeps it
    /// for every item left able to send it, and applies the F18 rules to each.
    #[test]
    fn startup_keeps_an_encoded_file_shared_by_destinations() {
        let dir = TestDir::new("recovery-shared");
        let mut store = memory_store();
        let source = dir.file("clip.mp4", b"src");
        let output = dir.file("clip_sq.mp4", b"out");
        let temp = dir.file("clip_sq.mp4.part", b"part");
        let ids: Vec<i64> = (0..5).map(|_| add_item(&mut store, &source)).collect();
        let [sent, uploading, waiting, encoding, probing] = ids[..] else {
            unreachable!()
        };
        for (id, state) in [
            (sent, State::Sent),
            (uploading, State::Uploading),
            (waiting, State::WaitingServer),
        ] {
            force_state(&store, id, state);
            store
                .conn()
                .execute(
                    "UPDATE items SET output_path = ?1, output_size = 3, content_id = 'c',
                                      next_run_at = '2030-01-01T00:00:00.000Z' WHERE id = ?2",
                    params![output, id],
                )
                .unwrap();
        }
        // Another plan for the same file, encoding its own.
        force_state(&store, encoding, State::Encoding);
        store
            .conn()
            .execute(
                "UPDATE items SET temp_path = ?1 WHERE id = ?2",
                params![temp, encoding],
            )
            .unwrap();
        force_state(&store, probing, State::Probing);

        let changed = on_startup(&mut store).unwrap();
        assert_eq!(changed.len(), 4);
        let state = |id| store.get_item(id).unwrap().state;
        assert_eq!(state(sent), State::Sent);
        assert_eq!(state(uploading), State::Unknown);
        assert_eq!(state(waiting), State::Encoded);
        assert_eq!(
            store.get_item(waiting).unwrap().next_run_at.as_deref(),
            Some("2030-01-01T00:00:00.000Z")
        );
        assert_eq!(state(encoding), State::Ready);
        assert_eq!(state(probing), State::Ready);
        assert!(
            Path::new(&output).exists(),
            "the shared encoded file is kept"
        );
        assert!(
            !Path::new(&temp).exists(),
            "the interrupted encode's own file goes"
        );
        assert!(Path::new(&source).exists());
        assert_eq!(
            store
                .items_in_states(&[State::Uploading, State::Probing, State::Encoding])
                .unwrap(),
            [],
            "nothing restarted"
        );
    }

    #[test]
    fn clean_shutdown_turns_uploading_into_unknown_only() {
        let mut store = memory_store();
        let uploading = add_item(&mut store, "a.mp4");
        let encoding = add_item(&mut store, "b.mp4");
        force_state(&store, uploading, State::Uploading);
        force_state(&store, encoding, State::Encoding);
        assert_eq!(on_clean_shutdown(&mut store).unwrap(), [uploading]);
        assert_eq!(store.get_item(uploading).unwrap().state, State::Unknown);
        assert_eq!(store.get_item(encoding).unwrap().state, State::Encoding);
    }
}
