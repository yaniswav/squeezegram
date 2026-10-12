//! Which Telegram transport the app runs, chosen once at startup.
//!
//! `SQUEEZEGRAM_TRANSPORT` = `tdlib` | `mock` | `none`; without it, `mock` in a debug build and
//! `tdlib` in a release. `tdlib` needs an API identity (the product's, injected at build time,
//! or the personal override of the settings with its DPAPI-protected `api_hash`) and
//! `tdjson.dll` (see `tdlib::library`); when one is missing the app runs `none`, whose start
//! fails with the reason (`TG_NOT_CONFIGURED`, `TG_LIB_MISSING`) so the UI can say why. A DLL
//! that does not match the pinned SHA-256 is a warning in a debug build and a refusal in a
//! release.
//!
//! Each identity has its own TDLib session (`session_dir`): TDLib does not check `api_id`
//! against an existing database, it reuses the authorization it holds. Sharing one folder
//! would leave a user who switched to their own `api_id` signed in under the product's.
//!
//! Choosing never touches the network: `TdlibJson` is only built here. Its `start` (at app
//! launch) goes as far as `WaitPhone` and signs in to no account until the user types
//! something.
//!
//! Journal: `transport_selected`, `tdjson_found` / `tdjson_missing`, `tg_identity` (presence
//! and lengths, never the values), `proxy_loaded` (kind only), `transport_start`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::json;

use super::proxy::Proxy;
use super::tdlib::identity::{self, ApiIdentity, IdentityProblem, IdentitySource};
use super::tdlib::library::{self, Checked, SearchPaths};
use super::tdlib::{TdlibConfig, TdlibJson};
use super::{NoTransport, TelegramTransport, TransportError};
use crate::settings::{ProxySettings, Settings};

/// `tdlib` | `mock` | `none`.
pub const TRANSPORT_ENV: &str = "SQUEEZEGRAM_TRANSPORT";
/// Session folder of a personal identity, under `tdlib`: `custom-<api_id>`.
const PERSONAL_SESSION_PREFIX: &str = "custom-";
const DEVICE_PREFIX: &str = "squeezegram";

/// Sent to the front in the `auth://state` payload (`transport`), so it can flag the mock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Mock,
    Tdlib,
    None,
}

impl TransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportKind::Mock => "mock",
            TransportKind::Tdlib => "tdlib",
            TransportKind::None => "none",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "mock" => Some(TransportKind::Mock),
            "tdlib" => Some(TransportKind::Tdlib),
            "none" => Some(TransportKind::None),
            _ => None,
        }
    }
}

/// Everything the choice depends on: `from_process` in the app, written by hand in tests.
/// No `Debug`: it holds the identity.
pub struct Inputs {
    /// `SQUEEZEGRAM_TRANSPORT`.
    pub requested: Option<String>,
    /// Where `tdjson.dll` is searched; `local_app_data` also gives the TDLib folder.
    pub search: SearchPaths,
    /// `option_env!("SQUEEZEGRAM_TG_API_ID")` / `..._API_HASH` (build time).
    pub embedded_api_id: Option<String>,
    pub embedded_api_hash: Option<String>,
    /// `telegram.api_override.api_id`; its `api_hash` is read from the TDLib folder (DPAPI).
    pub override_api_id: Option<i64>,
    /// `telegram.device_name`.
    pub device_name: Option<String>,
    /// `COMPUTERNAME`, for the default device name.
    pub machine_name: Option<String>,
    /// `network.proxy`; its secret is read from the TDLib folder (DPAPI).
    pub proxy: Option<ProxySettings>,
    /// Release build: default `tdlib`, a DLL off the pin is refused.
    pub release: bool,
}

impl Inputs {
    pub fn from_process(settings: &Settings) -> Self {
        Self {
            requested: std::env::var(TRANSPORT_ENV).ok(),
            search: SearchPaths::from_process(),
            embedded_api_id: option_env!("SQUEEZEGRAM_TG_API_ID").map(str::to_owned),
            embedded_api_hash: option_env!("SQUEEZEGRAM_TG_API_HASH").map(str::to_owned),
            override_api_id: settings.telegram.api_override.as_ref().map(|o| o.api_id),
            device_name: settings.telegram.device_name.clone(),
            machine_name: std::env::var("COMPUTERNAME").ok(),
            proxy: settings.network.proxy.clone(),
            release: !cfg!(debug_assertions),
        }
    }

    /// `%LOCALAPPDATA%\squeezegram\tdlib`: the TDLib database and the personal `api_hash.bin`.
    pub(crate) fn tdlib_dir(&self) -> Option<PathBuf> {
        self.search
            .local_app_data
            .as_ref()
            .map(|base| base.join("squeezegram").join("tdlib"))
    }

    /// `telegram.device_name`, else `default_device_model`.
    pub(crate) fn device_model(&self) -> String {
        match self.device_name.as_ref().filter(|n| !n.trim().is_empty()) {
            Some(name) => name.trim().to_owned(),
            None => self.default_device_model(),
        }
    }

    /// "squeezegram <machine>": the name without a setting, shown as the placeholder in
    /// Settings.
    pub(crate) fn default_device_model(&self) -> String {
        match self.machine_name.as_ref().filter(|n| !n.trim().is_empty()) {
            Some(machine) => format!("{DEVICE_PREFIX} {}", machine.trim()),
            None => DEVICE_PREFIX.to_owned(),
        }
    }
}

pub struct Selection {
    pub transport: Arc<dyn TelegramTransport>,
    pub kind: TransportKind,
    /// Journal code of the choice (`debug_default`, `env`, `dll_missing`…).
    pub reason: &'static str,
}

fn unavailable(error: TransportError, reason: &'static str) -> Selection {
    Selection {
        transport: Arc::new(NoTransport::unavailable(error)),
        kind: TransportKind::None,
        reason,
    }
}

#[cfg(any(test, debug_assertions))]
fn mock(reason: &'static str) -> Selection {
    Selection {
        transport: Arc::new(super::mock::MockTransport::new()),
        kind: TransportKind::Mock,
        reason,
    }
}

/// The mock is not compiled into a release: nothing is selected.
#[cfg(not(any(test, debug_assertions)))]
fn mock(_: &'static str) -> Selection {
    Selection {
        transport: Arc::new(NoTransport::new()),
        kind: TransportKind::None,
        reason: "mock_not_in_release",
    }
}

/// The transport for this run, with one `transport_selected` line.
pub fn select(inputs: &Inputs) -> Selection {
    let default = if inputs.release {
        TransportKind::Tdlib
    } else {
        TransportKind::Mock
    };
    let requested = inputs
        .requested
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let (wanted, requested, reason) = match requested {
        None => (
            default,
            None,
            if inputs.release {
                "release_default"
            } else {
                "debug_default"
            },
        ),
        Some(text) => match TransportKind::parse(text) {
            Some(kind) => (kind, Some(kind.as_str()), "env"),
            // The value itself is not written: only that it was not understood.
            None => (default, Some("invalid"), "env_invalid_default"),
        },
    };
    let selection = match wanted {
        TransportKind::None => Selection {
            transport: Arc::new(NoTransport::new()),
            kind: TransportKind::None,
            reason,
        },
        TransportKind::Mock => mock(reason),
        TransportKind::Tdlib => tdlib(inputs, reason),
    };
    let (kind, reason) = (selection.kind.as_str(), selection.reason);
    if selection.kind == wanted {
        tracing::info!(
            event = "transport_selected",
            kind,
            reason,
            requested,
            release = inputs.release,
            "Telegram transport selected"
        );
    } else {
        tracing::warn!(
            event = "transport_selected",
            kind,
            reason,
            requested,
            wanted = wanted.as_str(),
            release = inputs.release,
            "Telegram transport unavailable, running without Telegram"
        );
    }
    selection
}

fn tdlib(inputs: &Inputs, reason: &'static str) -> Selection {
    let Some(dir) = inputs.tdlib_dir() else {
        return unavailable(
            TransportError::Protocol("LOCALAPPDATA is not set".into()),
            "no_local_app_data",
        );
    };
    let identity = resolve_identity(inputs, &dir);
    let library = find_library(&inputs.search);
    let identity = match identity {
        Ok(identity) => identity,
        Err(error) => return unavailable(error, "no_identity"),
    };
    let Some(library) = library else {
        return unavailable(
            TransportError::LibMissing(format!("{} not found", library::DLL_NAME)),
            "dll_missing",
        );
    };
    if !library.pinned && inputs.release {
        return unavailable(
            TransportError::LibMissing(format!(
                "{} does not match the pinned TDLib {} build",
                library.path.display(),
                library::PINNED_VERSION
            )),
            "dll_hash_mismatch",
        );
    }
    let personal = inputs.override_api_id.map(|_| identity.api_id);
    // The proxy belongs to the computer, not to an API identity: its secret stays in the
    // TDLib folder, beside the sessions.
    let proxy = load_proxy(inputs, &dir);
    let mut config = TdlibConfig::new(
        session_dir(&dir, personal),
        identity.api_id,
        identity.api_hash,
        inputs.device_model(),
    );
    config.proxy = proxy;
    Selection {
        transport: Arc::new(TdlibJson::with_dll(library.path, config)),
        kind: TransportKind::Tdlib,
        reason,
    }
}

/// Where TDLib keeps the session (database, files, `key.bin`, log) of an identity. The
/// product's stays in `tdlib` itself, where every existing session already is; a personal
/// `api_id` gets `tdlib\custom-<api_id>`, with its own DPAPI key. Switching never shares a
/// session, and switching back finds the earlier one. `api_hash.bin` and `bin\tdjson.dll`
/// stay in `tdlib`: they are not part of a session.
pub(crate) fn session_dir(tdlib_dir: &std::path::Path, personal_api_id: Option<i32>) -> PathBuf {
    match personal_api_id {
        None => tdlib_dir.to_path_buf(),
        Some(api_id) => tdlib_dir.join(format!("{PERSONAL_SESSION_PREFIX}{api_id}")),
    }
}

/// The override when `telegram.api_override` is set (its `api_hash` must be stored), else
/// the embedded identity. One `tg_identity` line: source, presence, lengths, problem code.
pub(crate) fn resolve_identity(
    inputs: &Inputs,
    tdlib_dir: &std::path::Path,
) -> Result<ApiIdentity, TransportError> {
    let (source, api_id, api_hash) = match inputs.override_api_id {
        Some(id) => {
            let hash = match identity::load_override_hash(tdlib_dir) {
                Ok(Some(hash)) => Some(hash),
                Ok(None) => None,
                Err(error) => {
                    log_identity(IdentitySource::Override, None, None, Err(&error));
                    return Err(error);
                }
            };
            (IdentitySource::Override, Some(id.to_string()), hash)
        }
        None => (
            IdentitySource::Embedded,
            inputs.embedded_api_id.clone(),
            inputs.embedded_api_hash.clone(),
        ),
    };
    let result = identity::parse_identity(api_id.as_deref(), api_hash.as_deref()).map_err(
        |problem| match (source, problem) {
            // An override without its stored hash is an incomplete override.
            (IdentitySource::Override, IdentityProblem::MissingApiHash) => {
                IdentityProblem::OverrideHashUnavailable.into_error()
            }
            _ => problem.into_error(),
        },
    );
    log_identity(
        source,
        api_id.as_deref(),
        api_hash.as_deref(),
        result.as_ref().map(|_| ()),
    );
    result
}

/// The proxy of the settings with its secret. An unreadable secret is kept as the error: the
/// transport then refuses to start (`TdlibConfig::proxy`) instead of connecting directly.
pub(crate) fn load_proxy(
    inputs: &Inputs,
    tdlib_dir: &std::path::Path,
) -> Result<Option<Proxy>, TransportError> {
    let result = super::tdlib::proxy::load(tdlib_dir, inputs.proxy.as_ref());
    let kind = inputs.proxy.as_ref().map(|proxy| proxy.kind.as_str());
    match &result {
        Ok(_) => tracing::info!(event = "proxy_loaded", kind, "Telegram proxy loaded"),
        Err(error) => tracing::warn!(
            event = "proxy_loaded",
            kind,
            error_code = error.code_and_params().0,
            error = %error,
            "Telegram proxy unusable, Telegram will not connect"
        ),
    }
    result
}

/// Lengths only: never the `api_id` or the `api_hash` themselves.
fn log_identity(
    source: IdentitySource,
    api_id: Option<&str>,
    api_hash: Option<&str>,
    result: Result<(), &TransportError>,
) {
    let len = |text: Option<&str>| text.map_or(0, |t| t.trim().chars().count());
    let (api_id_len, api_hash_len) = (len(api_id), len(api_hash));
    let present = api_id_len > 0 && api_hash_len > 0;
    match result {
        Ok(()) => tracing::info!(
            event = "tg_identity",
            source = source.as_str(),
            present,
            valid = true,
            api_id_len,
            api_hash_len,
            "Telegram API identity available"
        ),
        Err(error) => {
            let (code, _) = error.code_and_params();
            let problem = match error {
                TransportError::NotConfigured(problem) => *problem,
                _ => "unreadable",
            };
            tracing::warn!(
                event = "tg_identity",
                source = source.as_str(),
                present,
                valid = false,
                problem,
                api_id_len,
                api_hash_len,
                error_code = code,
                "Telegram API identity unusable"
            );
        }
    }
}

fn find_library(search: &SearchPaths) -> Option<Checked> {
    match search.locate() {
        Ok((path, origin)) => {
            let checked = library::check(path, origin);
            let path = checked.path.display().to_string();
            let sha256 = checked.sha256.as_deref();
            if checked.pinned {
                tracing::info!(
                    event = "tdjson_found",
                    path = path.as_str(),
                    origin = origin.as_str(),
                    sha256,
                    pinned = true,
                    version = library::PINNED_VERSION,
                    "tdjson.dll found, pinned build"
                );
            } else {
                tracing::warn!(
                    event = "tdjson_found",
                    path = path.as_str(),
                    origin = origin.as_str(),
                    sha256,
                    pinned = false,
                    expected = library::PINNED_SHA256,
                    "tdjson.dll found, but it is not the pinned build"
                );
            }
            Some(checked)
        }
        Err(searched) => {
            let searched = json!({
                "searched": searched.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
            })
            .to_string();
            tracing::warn!(
                event = "tdjson_missing",
                error_code = "TG_LIB_MISSING",
                json = searched.as_str(),
                "tdjson.dll not found"
            );
            None
        }
    }
}

/// `transport.start()` with its `transport_start` line (the app alerts the UI on `Err`).
pub async fn start(transport: &dyn TelegramTransport) -> Result<(), TransportError> {
    let started = Instant::now();
    let result = transport.start().await;
    match &result {
        Ok(()) => tracing::info!(
            event = "transport_start",
            dur_ms = crate::logging::ms_since(started),
            ok = true,
            "Telegram transport started"
        ),
        Err(error) => tracing::error!(
            event = "transport_start",
            dur_ms = crate::logging::ms_since(started),
            ok = false,
            error_code = error.code_and_params().0,
            error = %error,
            "Telegram transport did not start"
        ),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::testing::TestJournal;
    use crate::transport::AuthState;
    use crate::transport::tdlib::tests::TempDir;
    use std::path::Path;

    // Fake identity: 8 digits and 32 hex, never the product's real one.
    const FAKE_ID: &str = "24681357";
    const FAKE_HASH: &str = "c0ffee00c0ffee00c0ffee00c0ffee00";
    const OVERRIDE_ID: i64 = 97_531_864;
    const OVERRIDE_HASH: &str = "abcdef0123456789abcdef0123456789";

    /// The selection's journal lines come from callsites that only these tests hit. They run
    /// one at a time, so no test registers such a callsite while another installs its test
    /// journal: tracing would then cache the callsite as disabled and the line would be lost.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn inputs(dir: &TempDir, release: bool) -> Inputs {
        Inputs {
            requested: None,
            search: SearchPaths {
                env: None,
                local_app_data: Some(dir.path().join("local")),
                exe_dir: Some(dir.path().join("exe")),
            },
            embedded_api_id: Some(FAKE_ID.into()),
            embedded_api_hash: Some(FAKE_HASH.into()),
            override_api_id: None,
            device_name: None,
            machine_name: Some("TEST-PC".into()),
            proxy: None,
            release,
        }
    }

    /// A file at the installed place that is not the pinned build (nor a DLL).
    fn fake_dll(inputs: &Inputs) -> PathBuf {
        let path = library::installed_path(inputs.search.local_app_data.as_ref().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not a real tdjson.dll").unwrap();
        path
    }

    async fn start_code(selection: &Selection) -> Option<&'static str> {
        start(selection.transport.as_ref())
            .await
            .err()
            .map(|error| error.code_and_params().0)
    }

    #[test]
    fn kinds_parse_case_insensitively() {
        for kind in [
            TransportKind::Mock,
            TransportKind::Tdlib,
            TransportKind::None,
        ] {
            assert_eq!(TransportKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(TransportKind::parse(" TDLib "), Some(TransportKind::Tdlib));
        assert_eq!(TransportKind::parse("real"), None);
        assert_eq!(serde_json::to_value(TransportKind::Mock).unwrap(), "mock");
    }

    #[tokio::test]
    async fn debug_defaults_to_the_mock() {
        let _serial = SERIAL.lock().await;
        let dir = TempDir::new("select-debug");
        let selection = select(&inputs(&dir, false));
        assert_eq!(
            (selection.kind, selection.reason),
            (TransportKind::Mock, "debug_default")
        );
        assert_eq!(start_code(&selection).await, None);
    }

    #[tokio::test]
    async fn the_variable_chooses_and_a_bad_value_keeps_the_default() {
        let _serial = SERIAL.lock().await;
        let dir = TempDir::new("select-env");
        let mut inputs = inputs(&dir, false);
        inputs.requested = Some("none".into());
        let none = select(&inputs);
        assert_eq!((none.kind, none.reason), (TransportKind::None, "env"));
        assert_eq!(start_code(&none).await, None, "plain none: no alert");
        assert!(matches!(
            *none.transport.auth_state().borrow(),
            AuthState::Closed
        ));

        inputs.requested = Some(" MOCK ".into());
        assert_eq!(select(&inputs).kind, TransportKind::Mock);

        inputs.requested = Some("telegram-please".into());
        let fallback = select(&inputs);
        assert_eq!(
            (fallback.kind, fallback.reason),
            (TransportKind::Mock, "env_invalid_default")
        );
    }

    #[tokio::test]
    async fn release_without_identity_runs_none_with_tg_not_configured() {
        let _serial = SERIAL.lock().await;
        let dir = TempDir::new("select-noid");
        let mut inputs = inputs(&dir, true);
        fake_dll(&inputs);
        for (id, hash) in [
            (None, None),
            (Some(""), Some(FAKE_HASH)),
            (Some("12ab"), Some(FAKE_HASH)),
            (Some(FAKE_ID), Some("short")),
        ] {
            inputs.embedded_api_id = id.map(str::to_owned);
            inputs.embedded_api_hash = hash.map(str::to_owned);
            let selection = select(&inputs);
            assert_eq!(
                (selection.kind, selection.reason),
                (TransportKind::None, "no_identity"),
                "{id:?}"
            );
            assert_eq!(start_code(&selection).await, Some("TG_NOT_CONFIGURED"));
        }
    }

    #[tokio::test]
    async fn a_missing_dll_is_tg_lib_missing_without_a_crash() {
        let _serial = SERIAL.lock().await;
        let dir = TempDir::new("select-nodll");
        for release in [true, false] {
            let mut inputs = inputs(&dir, release);
            inputs.requested = Some("tdlib".into());
            let selection = select(&inputs);
            assert_eq!(
                (selection.kind, selection.reason),
                (TransportKind::None, "dll_missing")
            );
            assert_eq!(start_code(&selection).await, Some("TG_LIB_MISSING"));
            selection.transport.shutdown().await;
        }
        // SQUEEZEGRAM_TDJSON pointing nowhere: only that path is tried.
        let mut inputs = inputs(&dir, true);
        fake_dll(&inputs);
        inputs.search.env = Some(dir.path().join("nowhere").join("tdjson.dll"));
        assert_eq!(select(&inputs).reason, "dll_missing");
    }

    #[tokio::test]
    async fn an_unpinned_dll_warns_in_debug_and_is_refused_in_release() {
        let _serial = SERIAL.lock().await;
        let dir = TempDir::new("select-hash");
        let journal = TestJournal::new(&dir.path().join("logs"), "trace");

        let release = inputs(&dir, true);
        fake_dll(&release);
        let refused = select(&release);
        assert_eq!(
            (refused.kind, refused.reason),
            (TransportKind::None, "dll_hash_mismatch")
        );
        assert_eq!(start_code(&refused).await, Some("TG_LIB_MISSING"));

        let mut debug = inputs(&dir, false);
        debug.requested = Some("tdlib".into());
        let allowed = select(&debug);
        assert_eq!(
            (allowed.kind, allowed.reason),
            (TransportKind::Tdlib, "env")
        );
        // The fake file is not a DLL: `TdlibJson` fails to load it, offline, no crash.
        assert_eq!(start_code(&allowed).await, Some("TG_LIB_MISSING"));
        assert!(matches!(
            *allowed.transport.auth_state().borrow(),
            AuthState::Closed
        ));
        allowed.transport.shutdown().await;
        // Nothing was created in the TDLib folder (no key, no database).
        let tdlib_dir = debug.tdlib_dir().unwrap();
        assert!(!tdlib_dir.join("key.bin").exists() && !tdlib_dir.join("db").exists());

        let lines = journal.lines();
        let found: Vec<_> = lines
            .iter()
            .filter(|l| l["event"] == "tdjson_found")
            .collect();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|l| {
            l["level"] == "warn"
                && l["fields"]["pinned"] == false
                && l["fields"]["origin"] == "local_app_data"
                && l["fields"]["sha256"]
                    .as_str()
                    .is_some_and(|s| s.len() == 64)
                && l["fields"]["expected"] == library::PINNED_SHA256
        }));
        let selected: Vec<_> = lines
            .iter()
            .filter(|l| l["event"] == "transport_selected")
            .map(|l| (l["fields"]["kind"].as_str(), l["fields"]["reason"].as_str()))
            .collect();
        assert_eq!(
            selected,
            [
                (Some("none"), Some("dll_hash_mismatch")),
                (Some("tdlib"), Some("env"))
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_override_uses_the_dpapi_hash_and_needs_it() {
        let _serial = SERIAL.blocking_lock();
        let dir = TempDir::new("select-override");
        let mut inputs = inputs(&dir, true);
        let tdlib_dir = inputs.tdlib_dir().unwrap();
        // No override: the embedded identity.
        let embedded = resolve_identity(&inputs, &tdlib_dir).unwrap();
        assert_eq!(embedded.api_id, FAKE_ID.parse::<i32>().unwrap());
        assert_eq!(embedded.api_hash, FAKE_HASH);

        // Override without its stored hash: not configured, no fallback to the product's.
        inputs.override_api_id = Some(OVERRIDE_ID);
        let error = resolve_identity(&inputs, &tdlib_dir).unwrap_err();
        assert!(
            matches!(
                error,
                TransportError::NotConfigured("override_api_hash_unavailable")
            ),
            "{error}"
        );

        identity::store_override_hash(&tdlib_dir, OVERRIDE_HASH).unwrap();
        let personal = resolve_identity(&inputs, &tdlib_dir).unwrap();
        assert_eq!(i64::from(personal.api_id), OVERRIDE_ID);
        assert_eq!(personal.api_hash, OVERRIDE_HASH);

        // An override id that does not fit TDLib's int32 is invalid.
        inputs.override_api_id = Some(-3);
        assert!(matches!(
            resolve_identity(&inputs, &tdlib_dir),
            Err(TransportError::NotConfigured("api_id_invalid"))
        ));
    }

    #[test]
    fn each_identity_has_its_own_session_folder() {
        let tdlib = Path::new(r"C:\local\squeezegram\tdlib");
        // The product's session stays where existing installs have it.
        assert_eq!(session_dir(tdlib, None), tdlib);
        let mine = session_dir(tdlib, Some(97_531_864));
        assert_eq!(mine, tdlib.join("custom-97531864"));
        let other = session_dir(tdlib, Some(12_345));
        assert_ne!(mine, other);
        // No folder holds another identity's database (`db`), files or key.
        for (a, b) in [(&mine, &other), (&mine, &tdlib.to_path_buf())] {
            for part in ["db", "files", "key.bin"] {
                assert_ne!(a.join(part), b.join(part));
            }
        }
        assert!(!["db", "files", "bin"].contains(&mine.file_name().unwrap().to_str().unwrap()));
    }

    /// The folder `select` gives TDLib follows the identity in effect at each start: a switch
    /// to a personal `api_id` and back never opens the other identity's database.
    #[cfg(windows)]
    #[test]
    fn a_switch_of_identity_never_reuses_the_other_session() {
        let _serial = SERIAL.blocking_lock();
        let dir = TempDir::new("select-session");
        let mut inputs = inputs(&dir, false);
        let tdlib_dir = inputs.tdlib_dir().unwrap();
        let folder = |inputs: &Inputs| {
            let identity = resolve_identity(inputs, &tdlib_dir).unwrap();
            session_dir(&tdlib_dir, inputs.override_api_id.map(|_| identity.api_id))
        };
        let product = folder(&inputs);
        assert_eq!(product, tdlib_dir);

        identity::store_override_hash(&tdlib_dir, OVERRIDE_HASH).unwrap();
        inputs.override_api_id = Some(OVERRIDE_ID);
        let personal = folder(&inputs);
        assert_eq!(personal, tdlib_dir.join(format!("custom-{OVERRIDE_ID}")));

        inputs.override_api_id = None;
        assert_eq!(folder(&inputs), product, "back to the product's session");
    }

    #[test]
    fn device_name_setting_then_machine_name() {
        let dir = TempDir::new("select-device");
        let mut inputs = inputs(&dir, false);
        assert_eq!(inputs.device_model(), "squeezegram TEST-PC");
        inputs.device_name = Some("  My laptop ".into());
        assert_eq!(inputs.device_model(), "My laptop");
        assert_eq!(inputs.default_device_model(), "squeezegram TEST-PC");
        inputs.device_name = Some("   ".into());
        inputs.machine_name = None;
        assert_eq!(inputs.device_model(), "squeezegram");
    }

    /// Fake identities given as the build would (embedded) and as the override (settings +
    /// DPAPI), through every selection path and the start: none of their values reaches the
    /// journal, only presence and lengths.
    #[tokio::test]
    async fn no_identity_value_reaches_the_journal() {
        let _serial = SERIAL.lock().await;
        let dir = TempDir::new("select-leak");
        let journal = TestJournal::new(&dir.path().join("logs"), "trace");

        let mut inputs = inputs(&dir, false);
        inputs.requested = Some("tdlib".into());
        let _ = start(select(&inputs).transport.as_ref()).await; // DLL missing
        fake_dll(&inputs);
        let _ = start(select(&inputs).transport.as_ref()).await; // fake DLL, debug
        inputs.release = true;
        let _ = start(select(&inputs).transport.as_ref()).await; // fake DLL, release
        inputs.embedded_api_id = Some(format!("{FAKE_ID}x")); // invalid id
        let _ = start(select(&inputs).transport.as_ref()).await;
        inputs.embedded_api_id = Some(FAKE_ID.into());

        inputs.override_api_id = Some(OVERRIDE_ID);
        let _ = start(select(&inputs).transport.as_ref()).await; // override without hash
        #[cfg(windows)]
        {
            identity::store_override_hash(&inputs.tdlib_dir().unwrap(), OVERRIDE_HASH).unwrap();
            inputs.release = false;
            let _ = start(select(&inputs).transport.as_ref()).await; // override, fake DLL
        }

        let text = journal.text();
        for secret in [
            FAKE_ID,
            FAKE_HASH,
            &OVERRIDE_ID.to_string(),
            OVERRIDE_HASH,
            &FAKE_HASH[..12],
            &OVERRIDE_HASH[..12],
        ] {
            assert!(!text.contains(secret), "{secret:?} leaked into the journal");
        }

        let lines = journal.lines();
        let identity: Vec<_> = lines
            .iter()
            .filter(|l| l["event"] == "tg_identity")
            .collect();
        assert!(identity.len() >= 5, "{}", identity.len());
        let ok = identity
            .iter()
            .find(|l| l["fields"]["valid"] == true && l["fields"]["source"] == "embedded")
            .unwrap();
        assert_eq!(ok["fields"]["api_id_len"], FAKE_ID.len());
        assert_eq!(ok["fields"]["api_hash_len"], FAKE_HASH.len());
        assert_eq!(ok["fields"]["present"], true);
        assert!(
            identity
                .iter()
                .any(|l| l["fields"]["problem"] == "api_id_invalid"
                    && l["error_code"] == "TG_NOT_CONFIGURED")
        );
        assert!(identity.iter().any(
            |l| l["fields"]["problem"] == "override_api_hash_unavailable"
                && l["fields"]["source"] == "override"
        ));
        assert!(
            lines
                .iter()
                .any(|l| l["event"] == "transport_start" && l["fields"]["ok"] == false)
        );
        assert!(lines.iter().any(|l| l["event"] == "tdjson_missing"));
    }

    /// Shape of the identity injected by `build.rs` in this build (lengths and character
    /// classes only, never the values). Run with `cargo test --lib embedded_identity_shape
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore = "depends on the local .env or the CI secrets"]
    fn embedded_identity_shape() {
        let id = option_env!("SQUEEZEGRAM_TG_API_ID");
        let hash = option_env!("SQUEEZEGRAM_TG_API_HASH");
        let digits = id.is_some_and(|t| t.bytes().all(|b| b.is_ascii_digit()));
        let hex = hash.is_some_and(|t| t.bytes().all(|b| b.is_ascii_hexdigit()));
        println!(
            "api_id: {} chars, all digits: {digits}; api_hash: {} chars, all hex: {hex}",
            id.map_or(0, str::len),
            hash.map_or(0, str::len)
        );
        assert!(identity::parse_identity(id, hash).is_ok());
    }

    /// The real `tdjson.dll` passes the pin. Hashes the file only: no TDLib call, no start.
    /// Run with `SQUEEZEGRAM_TEST_TDJSON=<path> cargo test --lib real_tdjson_matches -- --ignored`.
    #[test]
    #[ignore = "needs a real tdjson.dll (SQUEEZEGRAM_TEST_TDJSON)"]
    fn real_tdjson_matches_the_pin() {
        let _serial = SERIAL.blocking_lock();
        let Some(path) = std::env::var_os("SQUEEZEGRAM_TEST_TDJSON") else {
            panic!("set SQUEEZEGRAM_TEST_TDJSON to the path of tdjson.dll");
        };
        let dir = TempDir::new("select-real");
        let mut inputs = inputs(&dir, true);
        inputs.search.env = Some(PathBuf::from(path));
        let checked = library::check(inputs.search.env.clone().unwrap(), library::Origin::Env);
        println!("sha256 {:?}, pinned {}", checked.sha256, checked.pinned);
        assert!(checked.pinned);
        // Built, never started (a start with the network on is not allowed in tests).
        let selection = select(&inputs);
        assert_eq!(
            (selection.kind, selection.reason),
            (TransportKind::Tdlib, "release_default")
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_proxy_comes_with_its_stored_secret_or_blocks_the_start() {
        use crate::transport::proxy::ProxyKind;
        use crate::transport::tdlib::proxy::store_secret;

        let dir = TempDir::new("select-proxy");
        let mut inputs = inputs(&dir, false);
        let tdlib_dir = inputs.tdlib_dir().unwrap();
        assert_eq!(load_proxy(&inputs, &tdlib_dir).unwrap(), None);

        inputs.proxy = Some(ProxySettings {
            kind: ProxyKind::Mtproto,
            host: "mt.example".into(),
            port: 443,
            username: String::new(),
        });
        assert!(matches!(
            load_proxy(&inputs, &tdlib_dir),
            Err(TransportError::ProxySecretUnavailable)
        ));
        let secret = "0123456789abcdef0123456789abcdef";
        store_secret(&tdlib_dir, secret).unwrap();
        let proxy = load_proxy(&inputs, &tdlib_dir).unwrap().unwrap();
        assert_eq!(
            (proxy.host.as_str(), proxy.secret.as_str()),
            ("mt.example", secret)
        );
    }
}
