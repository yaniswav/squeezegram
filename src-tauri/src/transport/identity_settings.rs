//! What Settings > Telegram shows of the identity the app presents to Telegram: the default
//! device name (F35) and the personal API identity (F32).
//!
//! Both are read once, when `select` builds the transport at startup: a change applies at the
//! next start. The personal `api_hash` never comes back out: the page only learns whether one
//! is stored, and the journal only that the identity changed (`tg_identity_changed`), never
//! the `api_id` nor the `api_hash`.
//!
//! A TDLib session belongs to the `api_id` it signed in with, and TDLib does not check it: so
//! each identity has its own session folder (`select::session_dir`). A change never touches
//! the running session, which may stay signed in; after the restart the other identity's
//! session opens (a new sign-in the first time, the earlier session when switching back). The
//! transport is not rebuilt in place (every service holds it). Only while the transport is
//! starting or signing out is a change refused, so it never races those steps.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use serde_json::{Value, json};

use super::select::Inputs;
use super::tdlib::identity;
use super::{AuthState, TransportError};
use crate::settings::{Settings, SettingsError, SettingsStore};

/// Startup facts the commands need, kept beside the transport they describe.
pub struct IdentityRuntime {
    /// `%LOCALAPPDATA%\squeezegram\tdlib`, where `api_hash.bin` lives; `None` without
    /// `LOCALAPPDATA` (nothing can be stored then).
    tdlib_dir: Option<PathBuf>,
    /// "squeezegram <machine>".
    default_device_name: String,
    /// The personal identity was set or removed since the transport was built.
    changed: AtomicBool,
}

/// Result of `telegram_identity`. Never the `api_hash`: only whether one is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TelegramIdentity {
    /// Used when `telegram.device_name` is empty; the page shows it as the placeholder.
    pub default_device_name: String,
    /// `telegram.api_override.api_id`; `None`: the product's identity.
    pub api_id: Option<i64>,
    /// A personal `api_hash` is stored and this Windows user can decrypt it.
    pub api_hash_saved: bool,
    /// The personal identity changed in this session: it applies once the app restarts.
    pub restart_required: bool,
}

/// Why a change of the personal identity was refused. Never carries a typed value.
#[derive(Debug)]
pub enum IdentityChangeError {
    /// The transport is starting or signing out: try again once it is done.
    Busy,
    /// `api_id` or `api_hash` is not of the expected form.
    Invalid(&'static str),
    /// No TDLib folder to store the `api_hash` in (`LOCALAPPDATA` missing), or it failed.
    Storage(TransportError),
    Settings(SettingsError),
}

/// Whether a `settings_set` patch touches `telegram.api_override`: only the identity commands
/// may, since the override is useless without its `api_hash` and needs the account signed out.
pub fn touches_api_override(patch: &Value) -> bool {
    patch.pointer("/telegram/api_override").is_some()
}

/// A change while the transport opens its database or signs out would race that step.
fn require_idle(auth: &AuthState) -> Result<(), IdentityChangeError> {
    match auth {
        AuthState::Starting | AuthState::LoggingOut => Err(IdentityChangeError::Busy),
        _ => Ok(()),
    }
}

impl IdentityRuntime {
    pub fn new(inputs: &Inputs) -> Self {
        Self {
            tdlib_dir: inputs.tdlib_dir(),
            default_device_name: inputs.default_device_model(),
            changed: AtomicBool::new(false),
        }
    }

    pub fn status(&self, settings: &Settings) -> TelegramIdentity {
        let api_hash_saved = self.tdlib_dir.as_deref().is_some_and(|dir| {
            // An unreadable blob (other user, corrupt) counts as absent: it cannot be used.
            matches!(identity::load_override_hash(dir), Ok(Some(_)))
        });
        TelegramIdentity {
            default_device_name: self.default_device_name.clone(),
            api_id: settings.telegram.api_override.as_ref().map(|o| o.api_id),
            api_hash_saved,
            restart_required: self.changed.load(Ordering::Relaxed),
        }
    }

    /// Stores a personal identity: the `api_hash` under DPAPI in `api_hash.bin`, the `api_id`
    /// in `telegram.api_override`. Both are checked first; nothing is written if either is
    /// invalid. Used from the next start.
    pub fn set_override(
        &self,
        settings: &SettingsStore,
        auth: &AuthState,
        api_id: &str,
        api_hash: &str,
    ) -> Result<TelegramIdentity, IdentityChangeError> {
        require_idle(auth)?;
        let api_id = identity::parse_api_id(Some(api_id))
            .map_err(|_| IdentityChangeError::Invalid("api_id"))?;
        identity::check_api_hash(Some(api_hash))
            .map_err(|_| IdentityChangeError::Invalid("api_hash"))?;
        let dir = self.tdlib_dir.as_deref().ok_or_else(|| {
            IdentityChangeError::Storage(TransportError::Protocol("LOCALAPPDATA is not set".into()))
        })?;
        // The hash first: an `api_override` without its hash would make the next start fail
        // with `override_api_hash_unavailable`, while a hash without the override is unused.
        identity::store_override_hash(dir, api_hash).map_err(IdentityChangeError::Storage)?;
        let saved = settings
            .set(&json!({ "telegram": { "api_override": { "api_id": api_id } } }))
            .map_err(IdentityChangeError::Settings)?;
        self.changed.store(true, Ordering::Relaxed);
        log_changed(identity::IdentitySource::Override);
        Ok(self.status(&saved))
    }

    /// Back to the product's identity: `telegram.api_override` removed, then `api_hash.bin`.
    pub fn clear_override(
        &self,
        settings: &SettingsStore,
        auth: &AuthState,
    ) -> Result<TelegramIdentity, IdentityChangeError> {
        require_idle(auth)?;
        let saved = settings
            .set(&json!({ "telegram": { "api_override": null } }))
            .map_err(IdentityChangeError::Settings)?;
        if let Some(dir) = &self.tdlib_dir {
            identity::clear_override_hash(dir).map_err(IdentityChangeError::Storage)?;
        }
        self.changed.store(true, Ordering::Relaxed);
        log_changed(identity::IdentitySource::Embedded);
        Ok(self.status(&saved))
    }
}

/// Only which identity the next start will use: never the `api_id` nor the `api_hash`.
fn log_changed(source: identity::IdentitySource) {
    tracing::info!(
        event = "tg_identity_changed",
        source = source.as_str(),
        "Telegram API identity changed; used from the next start"
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::transport::tdlib::library::SearchPaths;
    use crate::transport::tdlib::tests::TempDir;

    pub(crate) fn inputs(dir: &TempDir) -> Inputs {
        Inputs {
            requested: None,
            search: SearchPaths {
                env: None,
                local_app_data: Some(dir.path().join("local")),
                exe_dir: None,
            },
            embedded_api_id: None,
            embedded_api_hash: None,
            override_api_id: None,
            device_name: Some("Custom".into()),
            machine_name: Some("TEST-PC".into()),
            proxy: None,
            release: false,
        }
    }

    // Fake identity: never a real one.
    const FAKE_ID: &str = "13572468";
    const FAKE_HASH: &str = "5eed5eed5eed5eed5eed5eed5eed5eed";

    fn signed_in() -> AuthState {
        AuthState::Ready(crate::transport::Me {
            user_id: 1,
            display_name: "Test".into(),
            username: None,
            is_premium: false,
            caption_max_chars: 1024,
            upload_limit_bytes: 0,
        })
    }

    fn store(dir: &TempDir) -> SettingsStore {
        SettingsStore::load(dir.path().join("settings.json")).unwrap()
    }

    #[test]
    fn only_the_identity_commands_may_touch_the_override() {
        assert!(touches_api_override(
            &json!({ "telegram": { "api_override": { "api_id": 1 } } })
        ));
        assert!(touches_api_override(
            &json!({ "telegram": { "api_override": null } })
        ));
        assert!(!touches_api_override(
            &json!({ "telegram": { "device_name": "PC" } })
        ));
        assert!(!touches_api_override(
            &json!({ "send": { "pause_min_s": 1 } })
        ));
    }

    #[test]
    fn a_malformed_or_busy_change_writes_nothing() {
        let dir = TempDir::new("identity-refused");
        let runtime = IdentityRuntime::new(&inputs(&dir));
        let settings = store(&dir);
        for (id, hash, field) in [
            ("", FAKE_HASH, "api_id"),
            ("0", FAKE_HASH, "api_id"),
            ("-4", FAKE_HASH, "api_id"),
            ("12ab", FAKE_HASH, "api_id"),
            ("99999999999", FAKE_HASH, "api_id"),
            (FAKE_ID, "", "api_hash"),
            (FAKE_ID, &FAKE_HASH[..31], "api_hash"),
            (FAKE_ID, "z".repeat(32).as_str(), "api_hash"),
        ] {
            let error = runtime
                .set_override(&settings, &AuthState::WaitPhone, id, hash)
                .unwrap_err();
            assert!(
                matches!(error, IdentityChangeError::Invalid(f) if f == field),
                "{id:?} {hash:?}: {error:?}"
            );
        }
        let refused = runtime
            .set_override(&settings, &AuthState::Starting, FAKE_ID, FAKE_HASH)
            .unwrap_err();
        assert!(matches!(refused, IdentityChangeError::Busy));
        assert!(matches!(
            runtime.clear_override(&settings, &AuthState::LoggingOut),
            Err(IdentityChangeError::Busy)
        ));

        assert_eq!(settings.get(), Settings::default());
        let tdlib_dir = inputs(&dir).tdlib_dir().unwrap();
        assert!(!tdlib_dir.join(identity::API_HASH_FILE).exists());
        assert!(!runtime.status(&settings.get()).restart_required);
    }

    /// Set, read back by the next start's selection, then cleared. The hash is stored under
    /// DPAPI and never comes back out: not in the status, not in the settings the page reads,
    /// not in the journal (nor the `api_id`).
    #[cfg(windows)]
    #[test]
    fn the_hash_is_protected_used_at_the_next_start_and_never_shown() {
        let dir = TempDir::new("identity-set");
        let journal = crate::logging::testing::TestJournal::new(&dir.path().join("logs"), "trace");
        let mut next_start = inputs(&dir);
        let runtime = IdentityRuntime::new(&next_start);
        let settings = store(&dir);

        // Signed in: allowed, the running session is not touched (it has its own folder).
        let status = runtime
            .set_override(&settings, &signed_in(), FAKE_ID, FAKE_HASH)
            .unwrap();
        assert_eq!(status.api_id, Some(13_572_468));
        assert!(status.api_hash_saved && status.restart_required);
        let tdlib_dir = next_start.tdlib_dir().unwrap();
        let blob = std::fs::read(tdlib_dir.join(identity::API_HASH_FILE)).unwrap();
        assert!(
            !blob
                .windows(FAKE_HASH.len())
                .any(|w| w == FAKE_HASH.as_bytes()),
            "a DPAPI blob, not the hash"
        );

        // What the page can read: the status and the settings.
        let page = format!(
            "{} {}",
            serde_json::to_string(&runtime.status(&settings.get())).unwrap(),
            serde_json::to_string(&settings.get()).unwrap()
        );
        assert!(!page.contains(FAKE_HASH), "{page}");
        let file = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        assert!(!file.contains(FAKE_HASH), "never in settings.json");

        // The next start builds its inputs from the saved settings and takes the override.
        next_start.override_api_id = settings.get().telegram.api_override.map(|o| o.api_id);
        let used = crate::transport::select::resolve_identity(&next_start, &tdlib_dir).unwrap();
        assert_eq!(
            (used.api_id.to_string().as_str(), used.api_hash.as_str()),
            (FAKE_ID, FAKE_HASH)
        );

        let cleared = runtime
            .clear_override(&settings, &AuthState::Closed)
            .unwrap();
        assert_eq!(cleared.api_id, None);
        assert!(!cleared.api_hash_saved && cleared.restart_required);
        assert!(!tdlib_dir.join(identity::API_HASH_FILE).exists());

        let text = journal.text();
        for secret in [FAKE_ID, FAKE_HASH, &FAKE_HASH[..12]] {
            assert!(!text.contains(secret), "{secret:?} reached the journal");
        }
        let changes: Vec<_> = journal
            .lines()
            .into_iter()
            .filter(|l| l["event"] == "tg_identity_changed")
            .map(|l| l["fields"]["source"].clone())
            .collect();
        assert_eq!(changes, [json!("override"), json!("embedded")]);
    }

    #[test]
    fn the_status_gives_the_default_device_name_whatever_the_setting() {
        let dir = TempDir::new("identity-status");
        let runtime = IdentityRuntime::new(&inputs(&dir));
        let status = runtime.status(&Settings::default());
        assert_eq!(
            status,
            TelegramIdentity {
                default_device_name: "squeezegram TEST-PC".into(),
                api_id: None,
                api_hash_saved: false,
                restart_required: false,
            }
        );
    }
}
