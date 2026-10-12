//! Send queue: item state machine, SQLite persistence, startup recovery and the sequential
//! orchestrator.

pub mod db;
pub mod events;
pub mod intake;
pub mod orchestrator;
pub mod recovery;
pub mod state;
pub mod store;
pub mod sweep;

#[cfg(test)]
pub(crate) mod test_util;

pub use state::State;

/// Errors of the queue core. `code()` gives the stable code sent to the front.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("item {0} not found")]
    ItemNotFound(i64),
    #[error("destination {0} not found")]
    DestinationNotFound(i64),
    #[error("watched folder {0} not found")]
    WatchFolderNotFound(i64),
    #[error("item {item_id}: illegal transition {from} -> {to}")]
    IllegalTransition {
        item_id: i64,
        from: State,
        to: State,
    },
    #[error("item {0} has already started")]
    ItemStarted(i64),
    #[error("unreadable file: {0}")]
    FileUnreadable(String),
    #[error("invalid value in database: {0}")]
    InvalidValue(String),
    #[error("database schema version {0} is newer than this app")]
    SchemaTooNew(i64),
    #[error("APPDATA is not set")]
    NoAppData,
}

impl QueueError {
    pub fn code(&self) -> &'static str {
        match self {
            QueueError::Db(_) | QueueError::InvalidValue(_) | QueueError::SchemaTooNew(_) => {
                "DB_ERROR"
            }
            QueueError::Io(_) | QueueError::NoAppData => "IO_ERROR",
            QueueError::ItemNotFound(_) => "ITEM_NOT_FOUND",
            QueueError::DestinationNotFound(_) => "DESTINATION_NOT_FOUND",
            QueueError::WatchFolderNotFound(_) => "WATCH_FOLDER_NOT_FOUND",
            QueueError::IllegalTransition { .. } => "ILLEGAL_TRANSITION",
            QueueError::ItemStarted(_) => "ITEM_STARTED",
            QueueError::FileUnreadable(_) => "FILE_UNREADABLE",
        }
    }
}

pub type Result<T> = std::result::Result<T, QueueError>;

/// Current time in ISO 8601 UTC with millisecond precision (fixed width, so it sorts as text).
pub fn now_iso() -> String {
    iso(chrono::Utc::now())
}

pub fn iso(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn parse_iso(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&chrono::Utc))
}
