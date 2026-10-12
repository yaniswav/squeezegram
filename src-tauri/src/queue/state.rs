//! Item states and validated transitions.

use std::fmt;
use std::str::FromStr;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params_from_iter};
use serde::{Deserialize, Serialize};

use super::{QueueError, Result, now_iso};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    PendingReview,
    Ready,
    Probing,
    Encoding,
    Remuxing,
    Encoded,
    Uploading,
    WaitingServer,
    Sent,
    Failed,
    Unknown,
    Cancelled,
}

impl State {
    pub const ALL: [State; 12] = [
        State::PendingReview,
        State::Ready,
        State::Probing,
        State::Encoding,
        State::Remuxing,
        State::Encoded,
        State::Uploading,
        State::WaitingServer,
        State::Sent,
        State::Failed,
        State::Unknown,
        State::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            State::PendingReview => "pending_review",
            State::Ready => "ready",
            State::Probing => "probing",
            State::Encoding => "encoding",
            State::Remuxing => "remuxing",
            State::Encoded => "encoded",
            State::Uploading => "uploading",
            State::WaitingServer => "waiting_server",
            State::Sent => "sent",
            State::Failed => "failed",
            State::Unknown => "unknown",
            State::Cancelled => "cancelled",
        }
    }

    /// Not started yet: caption and mode may still change.
    pub fn is_not_started(self) -> bool {
        matches!(self, State::PendingReview | State::Ready)
    }

    /// The orchestrator is working on the item (or waiting for Telegram to let it send):
    /// it can only be stopped by a cancel, never removed from the queue.
    pub fn is_running(self) -> bool {
        matches!(
            self,
            State::Probing
                | State::Encoding
                | State::Remuxing
                | State::Uploading
                | State::WaitingServer
        )
    }

    /// Allowed transitions. `failed` is terminal for the orchestrator only: the user may still
    /// retry or cancel it (cancelling deletes the kept encoded file).
    pub fn can_transition(self, to: State) -> bool {
        use State::*;
        match (self, to) {
            (_, Cancelled) => !matches!(self, Sent | Cancelled),
            (PendingReview, Ready) => true,
            (Ready, Probing) => true,
            // Remux, encode or nothing to do ("skipping_encode" goes straight to encoded).
            (Probing, Encoding | Remuxing | Encoded) => true,
            // Duplicate identity found while probing.
            (Probing, PendingReview) => true,
            (Probing | Encoding | Remuxing, Failed) => true,
            // Startup recovery.
            (Probing | Encoding | Remuxing, Ready) => true,
            (Encoding | Remuxing, Encoded) => true,
            (Encoded, Uploading) => true,
            (Uploading, Sent | WaitingServer | Failed | Unknown) => true,
            (WaitingServer, Uploading) => true,
            // Startup recovery keeps next_run_at.
            (WaitingServer, Encoded) => true,
            // User retry: encoded file still there, or start over.
            (Failed | Unknown, Encoded | Ready) => true,
            // User mark_sent after reconciliation.
            (Unknown, Sent) => true,
            _ => false,
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for State {
    type Err = QueueError;

    fn from_str(value: &str) -> Result<Self> {
        State::ALL
            .into_iter()
            .find(|state| state.as_str() == value)
            .ok_or_else(|| QueueError::InvalidValue(format!("state = {value}")))
    }
}

/// Item columns a transition may write besides `state` and `updated_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    ContentId,
    IdentityWeak,
    HoldReason,
    MediaJson,
    PlanJson,
    TempPath,
    OutputPath,
    OutputSize,
    ProgressPhase,
    ProgressPct,
    LastError,
    NextRunAt,
    TgMessageId,
    TgSentAt,
}

impl Column {
    fn name(self) -> &'static str {
        match self {
            Column::ContentId => "content_id",
            Column::IdentityWeak => "identity_weak",
            Column::HoldReason => "hold_reason",
            Column::MediaJson => "media_json",
            Column::PlanJson => "plan_json",
            Column::TempPath => "temp_path",
            Column::OutputPath => "output_path",
            Column::OutputSize => "output_size",
            Column::ProgressPhase => "progress_phase",
            Column::ProgressPct => "progress_pct",
            Column::LastError => "last_error",
            Column::NextRunAt => "next_run_at",
            Column::TgMessageId => "tg_message_id",
            Column::TgSentAt => "tg_sent_at",
        }
    }
}

/// Fields written together with a state change.
#[derive(Debug, Clone, Default)]
pub struct Changes {
    values: Vec<(Column, Value)>,
    bump_attempts: bool,
    /// Why, for the journal only (`item_state` event); never written to the database.
    reason: Option<&'static str>,
}

impl Changes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, column: Column, value: impl Into<Value>) -> Self {
        self.values.push((column, value.into()));
        self
    }

    pub fn clear(self, column: Column) -> Self {
        self.set(column, Value::Null)
    }

    pub fn bump_attempts(mut self) -> Self {
        self.bump_attempts = true;
        self
    }

    pub fn reason(mut self, reason: &'static str) -> Self {
        self.reason = Some(reason);
        self
    }

    /// The error code written to `last_error` by this change, if any.
    fn error_code(&self) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find_map(|(column, value)| match (column, value) {
                (Column::LastError, Value::Text(code)) => Some(code.as_str()),
                _ => None,
            })
    }
}

/// `item_state` journal line of a committed transition.
pub fn log_transition(item_id: i64, from: State, to: State, changes: &Changes) {
    tracing::info!(
        event = "item_state",
        item_id,
        from = from.as_str(),
        to = to.as_str(),
        reason = changes.reason.unwrap_or("unspecified"),
        error_code = changes.error_code(),
        "item {item_id}: {from} -> {to}"
    );
}

pub fn current_state(conn: &Connection, item_id: i64) -> Result<State> {
    let state: Option<String> = conn
        .query_row("SELECT state FROM items WHERE id = ?1", [item_id], |row| {
            row.get(0)
        })
        .optional()?;
    state.ok_or(QueueError::ItemNotFound(item_id))?.parse()
}

/// Validates and applies one transition inside the caller's transaction. Returns the old state.
pub fn apply(conn: &Connection, item_id: i64, to: State, changes: &Changes) -> Result<State> {
    let from = current_state(conn, item_id)?;
    if !from.can_transition(to) {
        return Err(QueueError::IllegalTransition { item_id, from, to });
    }
    let mut sql = String::from("UPDATE items SET state = ?, updated_at = ?");
    let mut values = vec![Value::from(to.as_str().to_owned()), Value::from(now_iso())];
    for (column, value) in &changes.values {
        sql.push_str(", ");
        sql.push_str(column.name());
        sql.push_str(" = ?");
        values.push(value.clone());
    }
    if changes.bump_attempts {
        sql.push_str(", attempts = attempts + 1");
    }
    sql.push_str(" WHERE id = ?");
    values.push(Value::from(item_id));
    conn.execute(&sql, params_from_iter(values))?;
    Ok(from)
}

/// One transition = one transaction.
pub fn transition(
    conn: &mut Connection,
    item_id: i64,
    to: State,
    changes: &Changes,
) -> Result<State> {
    let tx = conn.transaction()?;
    let from = apply(&tx, item_id, to, changes)?;
    tx.commit()?;
    log_transition(item_id, from, to, changes);
    Ok(from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::{add_item, force_state, memory_store};
    use State::*;

    /// Written independently from `can_transition`, straight from the state diagram.
    const LEGAL: &[(State, State)] = &[
        (PendingReview, Ready),
        (Ready, Probing),
        (Probing, Encoding),
        (Probing, Remuxing),
        (Probing, Encoded),
        (Probing, PendingReview),
        (Probing, Failed),
        (Probing, Ready),
        (Encoding, Encoded),
        (Encoding, Failed),
        (Encoding, Ready),
        (Remuxing, Encoded),
        (Remuxing, Failed),
        (Remuxing, Ready),
        (Encoded, Uploading),
        (Uploading, Sent),
        (Uploading, WaitingServer),
        (Uploading, Failed),
        (Uploading, Unknown),
        (WaitingServer, Uploading),
        (WaitingServer, Encoded),
        (Failed, Encoded),
        (Failed, Ready),
        (Unknown, Encoded),
        (Unknown, Ready),
        (Unknown, Sent),
    ];

    fn expected(from: State, to: State) -> bool {
        let cancel = to == Cancelled && !matches!(from, Sent | Cancelled);
        cancel || LEGAL.contains(&(from, to))
    }

    #[test]
    fn transition_table_matches_the_spec() {
        for from in State::ALL {
            for to in State::ALL {
                assert_eq!(
                    from.can_transition(to),
                    expected(from, to),
                    "{from} -> {to}"
                );
            }
        }
    }

    #[test]
    fn states_round_trip_through_text() {
        for state in State::ALL {
            assert_eq!(state.as_str().parse::<State>().unwrap(), state);
        }
        assert!("bogus".parse::<State>().is_err());
    }

    #[test]
    fn every_pair_is_applied_or_refused_in_the_database() {
        let mut store = memory_store();
        let id = add_item(&mut store, "a.mp4");
        for from in State::ALL {
            for to in State::ALL {
                force_state(&store, id, from);
                let result = transition(store.conn_mut(), id, to, &Changes::new());
                let now = current_state(store.conn(), id).unwrap();
                if expected(from, to) {
                    assert_eq!(result.unwrap(), from);
                    assert_eq!(now, to);
                } else {
                    assert!(
                        matches!(result, Err(QueueError::IllegalTransition { from: f, to: t, .. }) if f == from && t == to),
                        "{from} -> {to}"
                    );
                    assert_eq!(now, from, "refused transition must not write");
                }
            }
        }
    }

    #[test]
    fn transition_writes_fields_updated_at_and_attempts() {
        let mut store = memory_store();
        let id = add_item(&mut store, "a.mp4");
        force_state(&store, id, Encoded);
        store
            .conn()
            .execute("UPDATE items SET updated_at = 'old' WHERE id = ?1", [id])
            .unwrap();
        let changes = Changes::new()
            .set(Column::OutputPath, "out.mp4".to_owned())
            .clear(Column::NextRunAt)
            .bump_attempts();
        transition(store.conn_mut(), id, Uploading, &changes).unwrap();
        let item = store.get_item(id).unwrap();
        assert_eq!(item.state, Uploading);
        assert_eq!(item.output_path.as_deref(), Some("out.mp4"));
        assert_eq!(item.attempts, 1);
        assert_ne!(item.updated_at, "old");
    }

    #[test]
    fn missing_item_is_a_typed_error() {
        let mut store = memory_store();
        assert!(matches!(
            transition(store.conn_mut(), 999, Ready, &Changes::new()),
            Err(QueueError::ItemNotFound(999))
        ));
    }
}
