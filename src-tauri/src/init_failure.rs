//! A start that fails before the window exists. The binary has no console in a release
//! (`windows_subsystem = "windows"`), so without this the app would just not appear: a native
//! message box says why, where the database and the journal are, then the process exits with a
//! non-zero code.
//!
//! The text is English: the page's translations are not loaded at this point.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::queue::QueueError;
use crate::queue::db::SCHEMA_VERSION;
use crate::settings::{SettingsError, SettingsStore};

/// Exit code of a start that failed before the window.
pub const EXIT_CODE: i32 = 1;
const TITLE: &str = "squeezegram can't start";

/// An environment variable Windows normally sets is missing (`LOCALAPPDATA`, `APPDATA`).
#[derive(Debug)]
pub struct MissingFolder(pub &'static str);

impl fmt::Display for MissingFolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not set", self.0)
    }
}

impl Error for MissingFolder {}

/// What the user is told; one per cause they can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// An older copy (a portable one, typically) opened a database a newer version migrated.
    NewerDatabase {
        found: i64,
        supported: i64,
    },
    /// SQLite cannot open or read `squeezegram.db` (damaged, locked, not a database).
    UnreadableDatabase,
    UnreadableSettings,
    MissingFolder(&'static str),
    /// Anything else (the WebView cannot start…): the error's own text.
    Other(String),
}

impl Cause {
    /// Journal field `cause`.
    pub fn code(&self) -> &'static str {
        match self {
            Cause::NewerDatabase { .. } => "newer_database",
            Cause::UnreadableDatabase => "unreadable_database",
            Cause::UnreadableSettings => "unreadable_settings",
            Cause::MissingFolder(_) => "missing_folder",
            Cause::Other(_) => "other",
        }
    }
}

/// The cause behind an error of `boot` or `open_state` (boxed as `dyn Error`).
pub fn cause_of(error: &(dyn Error + 'static)) -> Cause {
    if let Some(error) = error.downcast_ref::<QueueError>() {
        return match error {
            QueueError::SchemaTooNew(found) => Cause::NewerDatabase {
                found: *found,
                supported: SCHEMA_VERSION,
            },
            QueueError::NoAppData => Cause::MissingFolder("APPDATA"),
            QueueError::Db(_) | QueueError::Io(_) | QueueError::InvalidValue(_) => {
                Cause::UnreadableDatabase
            }
            other => Cause::Other(other.to_string()),
        };
    }
    if error.downcast_ref::<SettingsError>().is_some() {
        return Cause::UnreadableSettings;
    }
    if let Some(MissingFolder(name)) = error.downcast_ref::<MissingFolder>() {
        return Cause::MissingFolder(name);
    }
    Cause::Other(error.to_string())
}

/// Where the files the message names are; `None` when unknown.
#[derive(Debug, Default)]
pub struct Paths {
    pub database: Option<PathBuf>,
    pub settings: Option<PathBuf>,
    /// The journal folder, only once the journal is running.
    pub journal: Option<PathBuf>,
}

impl Paths {
    pub fn from_process() -> Self {
        let database = crate::queue::db::default_path().ok();
        Self {
            settings: database.as_deref().map(SettingsStore::path_next_to),
            database,
            journal: crate::logging::dir().map(Path::to_path_buf),
        }
    }
}

/// The message box text for `cause`.
pub fn message(cause: &Cause, paths: &Paths) -> String {
    let mut text = match cause {
        Cause::NewerDatabase { found, supported } => format!(
            "Its database was created by a newer version of squeezegram (schema {found}; this \
             version reads up to {supported}). Install the latest version, or use the newer \
             copy you ran before."
        ),
        Cause::UnreadableDatabase => "Its database can't be opened: it may be damaged, or \
             locked by another program. If it is damaged, moving the file elsewhere lets \
             squeezegram start with an empty queue and history."
            .to_owned(),
        Cause::UnreadableSettings => "Its settings file can't be read. Fix it or move it \
             elsewhere: squeezegram then starts with the default settings."
            .to_owned(),
        Cause::MissingFolder(name) => format!(
            "Windows did not give it a folder for its data ({name} is not set). Sign out of \
             Windows and back in, then try again."
        ),
        Cause::Other(error) => format!("Unexpected error: {error}"),
    };
    let named = match cause {
        Cause::NewerDatabase { .. } | Cause::UnreadableDatabase => {
            Some(("Database", &paths.database))
        }
        Cause::UnreadableSettings => Some(("Settings", &paths.settings)),
        Cause::MissingFolder(_) | Cause::Other(_) => None,
    };
    if let Some((label, Some(path))) = named {
        text.push_str(&format!("\n\n{label}: {}", path.display()));
    }
    if let Some(journal) = &paths.journal {
        text.push_str(&format!(
            "\n\nDetails are in the journal: {}",
            journal.display()
        ));
    }
    text
}

/// Says why on screen, then ends the process with `EXIT_CODE`.
pub fn report_and_exit(cause: Cause) -> ! {
    let text = message(&cause, &Paths::from_process());
    tracing::error!(
        event = "init_failed",
        cause = cause.code(),
        exit_code = EXIT_CODE,
        "startup failed, the user was told why"
    );
    // A debug build has a console; a release has none, only the box.
    eprintln!("{TITLE}: {text}");
    show(&text);
    crate::logging::app_exit("init_failed");
    std::process::exit(EXIT_CODE)
}

#[cfg(windows)]
fn show(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW,
    };
    use windows::core::HSTRING;
    // SAFETY: both strings live for the call; no owner window (none exists yet).
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(TITLE),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

#[cfg(not(windows))]
fn show(_: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths {
            database: Some(PathBuf::from(r"C:\data\squeezegram\squeezegram.db")),
            settings: Some(PathBuf::from(r"C:\data\squeezegram\settings.json")),
            journal: Some(PathBuf::from(r"C:\local\squeezegram\logs")),
        }
    }

    fn boxed(error: impl Error + 'static) -> Box<dyn Error> {
        Box::new(error)
    }

    #[test]
    fn each_startup_error_has_its_cause() {
        let cases = [
            (
                boxed(QueueError::SchemaTooNew(SCHEMA_VERSION + 2)),
                Cause::NewerDatabase {
                    found: SCHEMA_VERSION + 2,
                    supported: SCHEMA_VERSION,
                },
            ),
            (
                boxed(QueueError::Db(rusqlite::Error::InvalidQuery)),
                Cause::UnreadableDatabase,
            ),
            (
                boxed(QueueError::InvalidValue("schema_version = x".into())),
                Cause::UnreadableDatabase,
            ),
            (
                boxed(QueueError::NoAppData),
                Cause::MissingFolder("APPDATA"),
            ),
            (
                boxed(MissingFolder("LOCALAPPDATA")),
                Cause::MissingFolder("LOCALAPPDATA"),
            ),
            (
                boxed(SettingsError::Invalid("expected value".into())),
                Cause::UnreadableSettings,
            ),
        ];
        for (error, cause) in cases {
            assert_eq!(cause_of(error.as_ref()), cause, "{error}");
        }
        let other: Box<dyn Error> = "WebView2 is not installed".into();
        assert_eq!(
            cause_of(other.as_ref()),
            Cause::Other("WebView2 is not installed".into())
        );
    }

    /// A real corrupt file goes through the same path as at startup.
    #[test]
    fn a_file_that_is_not_a_database_is_unreadable() {
        let dir = crate::transport::tdlib::tests::TempDir::new("init-corrupt");
        let path = dir.path().join("squeezegram.db");
        std::fs::write(&path, vec![0x5Au8; 8192]).unwrap();
        let error = crate::queue::store::Store::open(&path).err().unwrap();
        assert_eq!(cause_of(&error), Cause::UnreadableDatabase);
    }

    #[test]
    fn the_message_explains_the_cause_and_names_the_files() {
        let newer = message(
            &Cause::NewerDatabase {
                found: 9,
                supported: 7,
            },
            &paths(),
        );
        assert!(newer.contains("newer version"), "{newer}");
        assert!(newer.contains("schema 9") && newer.contains("up to 7"));
        assert!(newer.contains(r"Database: C:\data\squeezegram\squeezegram.db"));
        assert!(newer.contains(r"journal: C:\local\squeezegram\logs"));

        let unreadable = message(&Cause::UnreadableDatabase, &paths());
        assert!(unreadable.contains("can't be opened"), "{unreadable}");
        assert!(unreadable.contains("squeezegram.db"));

        let settings = message(&Cause::UnreadableSettings, &paths());
        assert!(settings.contains(r"Settings: C:\data\squeezegram\settings.json"));
        assert!(!settings.contains("squeezegram.db"));

        let other = message(&Cause::Other("boom".into()), &paths());
        assert!(other.contains("boom") && other.contains("journal"));
        assert!(!other.contains("squeezegram.db"));
    }

    #[test]
    fn without_a_journal_the_message_does_not_point_to_one() {
        let folder = message(&Cause::MissingFolder("LOCALAPPDATA"), &Paths::default());
        assert!(folder.contains("LOCALAPPDATA is not set"), "{folder}");
        assert!(!folder.contains("journal"), "{folder}");
        // A known cause whose file path is unknown still explains itself.
        let newer = message(
            &Cause::NewerDatabase {
                found: 9,
                supported: 7,
            },
            &Paths::default(),
        );
        assert!(!newer.contains("Database:"), "{newer}");
    }

    #[test]
    fn every_cause_has_a_distinct_journal_code() {
        let codes = [
            Cause::NewerDatabase {
                found: 1,
                supported: 1,
            },
            Cause::UnreadableDatabase,
            Cause::UnreadableSettings,
            Cause::MissingFolder("APPDATA"),
            Cause::Other(String::new()),
        ]
        .map(|cause| cause.code());
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), codes.len());
    }
}
