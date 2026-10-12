//! What Settings > Telegram shows of the identity the app presents to Telegram: the default
//! device name (F35) and the personal API identity (F32).
//!
//! Both are read once, when `select` builds the transport at startup: a change applies at the
//! next start. The personal `api_hash` never comes back out: the page only learns whether one
//! is stored.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use super::select::Inputs;
use super::tdlib::identity;
use crate::settings::Settings;

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
            release: false,
        }
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
