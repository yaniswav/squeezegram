//! Automatic updates.
//!
//! The installer build checks GitHub for the latest release, downloads the signed installer,
//! lets `tauri-plugin-updater` verify its minisign signature against the public key in
//! `tauri.conf.json`, then hands over to the installer (passive mode), which restarts the app.
//! The portable build only tells the user that a newer version exists and where to get it: it
//! never downloads or replaces anything.
//!
//! What a check sends: one HTTPS GET on the `latest.json` endpoint with the plugin's default
//! headers (`Accept` and `User-Agent: tauri-plugin-updater/<plugin version>`). The endpoint
//! URL has no `{{current_version}}`, `{{target}}` or `{{arch}}` placeholder, so the app
//! version, the Windows version and the architecture are not sent either. GitHub sees the IP
//! address, as for any download. No identifier, no Telegram data.
//!
//! - this file: kinds, notices, version comparison, cadence, error codes, progress sampling
//! - `service`: the plugin calls, the cached result, the events and the journal lines

mod service;

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use semver::Version;
use serde::Serialize;

pub use service::UpdateService;

use crate::settings::Updates;

/// Page of the latest release: where a portable build sends the user.
pub const RELEASES_PAGE: &str = "https://github.com/yaniswav/squeezegram/releases/latest";
/// Empty file the portable zip puts next to the executable.
pub const PORTABLE_FLAG: &str = "portable.flag";
/// Minimum time between two automatic checks.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Result of a check, sent to the page.
pub const UPDATE_EVENT: &str = "app://update";
/// Download progress of an installer update.
pub const UPDATE_PROGRESS_EVENT: &str = "app://update-progress";

/// How this copy of the app was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Installed by the NSIS installer: can update itself.
    Installer,
    /// Unzipped from the portable zip: tells the user, never installs.
    Portable,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Installer => "installer",
            Kind::Portable => "portable",
        }
    }
}

/// `portable` when `portable.flag` sits in the folder of the executable.
pub fn detect_kind(exe_dir: &Path) -> Kind {
    if exe_dir.join(PORTABLE_FLAG).is_file() {
        Kind::Portable
    } else {
        Kind::Installer
    }
}

/// Kind of the running executable; an executable path that cannot be read counts as installed.
pub fn current_kind() -> Kind {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(detect_kind))
        .unwrap_or(Kind::Installer)
}

/// Payload of `app://update` and result of `update_check`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateNotice {
    pub available: bool,
    /// The announced version; the running one when nothing newer exists.
    pub version: String,
    pub kind: Kind,
    /// Release notes of the manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Page of the release, for the portable build's "open the download page".
    pub url: &'static str,
}

impl UpdateNotice {
    pub fn available(version: String, kind: Kind, notes: Option<String>) -> Self {
        Self {
            available: true,
            version,
            kind,
            notes,
            url: RELEASES_PAGE,
        }
    }

    pub fn up_to_date(current_version: String, kind: Kind) -> Self {
        Self {
            available: false,
            version: current_version,
            kind,
            notes: None,
            url: RELEASES_PAGE,
        }
    }
}

/// Result of `update_info`: no network. `notice` is the update found earlier in this session.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateInfo {
    pub kind: Kind,
    pub current_version: String,
    pub notice: Option<UpdateNotice>,
}

/// Whether `candidate` is a newer release than `current`: SemVer precedence, where a
/// pre-release sorts below its release and build metadata (`+abc`) counts for nothing.
pub fn is_newer(current: &Version, candidate: &Version) -> bool {
    fn core(version: &Version) -> Version {
        Version {
            build: semver::BuildMetadata::EMPTY,
            ..version.clone()
        }
    }
    core(candidate) > core(current)
}

/// Whether 24 hours have passed since `last_check_at`. A missing, unreadable or future time
/// (the clock was moved back) counts as due.
pub fn is_due(last_check_at: Option<&str>, now: DateTime<Utc>) -> bool {
    let Some(last) = last_check_at.and_then(crate::queue::parse_iso) else {
        return true;
    };
    match now.signed_duration_since(last).to_std() {
        Ok(elapsed) => elapsed >= CHECK_INTERVAL,
        Err(_) => true,
    }
}

/// The startup check runs only when the user left automatic checks on and one is due.
pub fn should_check_at_startup(updates: &Updates, now: DateTime<Utc>) -> bool {
    updates.check_automatically && is_due(updates.last_check_at.as_deref(), now)
}

/// What started a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// At startup, subject to the setting and the 24 hour cadence.
    Automatic,
    /// "Check now": always runs.
    Manual,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Automatic => "automatic",
            Trigger::Manual => "manual",
        }
    }
}

/// An update failure: the stable code the page translates, plus a short cause for the journal.
/// The cause is a fixed word, never the plugin's message, which can carry the full URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateError {
    pub code: &'static str,
    pub cause: &'static str,
}

impl UpdateError {
    pub const CHECK_FAILED: &'static str = "UPDATE_CHECK_FAILED";
    pub const NOT_PUBLISHED: &'static str = "UPDATE_NOT_PUBLISHED";
    pub const DOWNLOAD_FAILED: &'static str = "UPDATE_DOWNLOAD_FAILED";
    pub const SIGNATURE_INVALID: &'static str = "UPDATE_SIGNATURE_INVALID";
    pub const INSTALL_FAILED: &'static str = "UPDATE_INSTALL_FAILED";

    fn new(code: &'static str, cause: &'static str) -> Self {
        Self { code, cause }
    }

    /// The portable build never installs.
    pub fn portable() -> Self {
        Self::new(Self::INSTALL_FAILED, "portable")
    }

    fn busy() -> Self {
        Self::new(Self::INSTALL_FAILED, "busy")
    }

    /// Where the failure happened, for the journal (`check`, `download`, `install`).
    pub fn phase(&self) -> &'static str {
        match self.code {
            Self::CHECK_FAILED | Self::NOT_PUBLISHED => "check",
            Self::INSTALL_FAILED => "install",
            _ => "download",
        }
    }

    /// An error of the check request or of the manifest it returned. A 404 on `latest.json`
    /// means no release is published yet: the connection is fine, so it has its own code
    /// instead of the "check your connection" text.
    pub fn from_check(error: &tauri_plugin_updater::Error) -> Self {
        let code = match error {
            tauri_plugin_updater::Error::ReleaseNotFound => Self::NOT_PUBLISHED,
            _ => Self::CHECK_FAILED,
        };
        Self::new(code, cause_of(error))
    }

    /// An error of the download: a signature that does not verify has its own code.
    pub fn from_download(error: &tauri_plugin_updater::Error) -> Self {
        use tauri_plugin_updater::Error;
        match error {
            Error::Minisign(_)
            | Error::Base64(_)
            | Error::SignatureUtf8(_)
            | Error::SignedVersionMismatch { .. }
            | Error::MissingSignedVersion => Self::new(Self::SIGNATURE_INVALID, cause_of(error)),
            _ => Self::new(Self::DOWNLOAD_FAILED, cause_of(error)),
        }
    }

    pub fn from_install(error: &tauri_plugin_updater::Error) -> Self {
        Self::new(Self::INSTALL_FAILED, cause_of(error))
    }
}

/// A fixed word for the kind of failure.
fn cause_of(error: &tauri_plugin_updater::Error) -> &'static str {
    use tauri_plugin_updater::Error;
    match error {
        Error::Reqwest(source) if source.is_timeout() => "timeout",
        Error::Reqwest(source) if source.is_connect() => "connect",
        Error::Reqwest(source) if source.is_status() => "http_status",
        Error::Reqwest(_) | Error::Network(_) => "network",
        Error::ReleaseNotFound => "release_not_found",
        Error::Serialization(_) | Error::Semver(_) => "bad_manifest",
        Error::TargetNotFound(_) | Error::TargetsNotFound(_) => "no_build_for_this_target",
        Error::Minisign(_)
        | Error::Base64(_)
        | Error::SignatureUtf8(_)
        | Error::SignedVersionMismatch { .. }
        | Error::MissingSignedVersion => "signature",
        Error::Io(_) => "io",
        _ => "other",
    }
}

/// Phase of an installer update, for `app://update-progress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Downloading,
    /// The download is verified; the installer is about to start and the app to close.
    Installing,
}

/// Payload of `app://update-progress`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateProgress {
    pub phase: Phase,
    pub downloaded: u64,
    /// `None` when the server did not announce a length.
    pub total: Option<u64>,
    /// Whole percent, `None` without a total.
    pub percent: Option<u8>,
}

/// Without a total, one event per this many bytes.
const UNKNOWN_TOTAL_STEP: u64 = 512 * 1024;
/// The journal gets one line per this many percent (and one at the end).
const LOG_STEP_PERCENT: u8 = 10;

/// Keeps the page and the journal from receiving one line per network chunk: the page gets an
/// event when the whole percent changes, the journal one per 10 percent.
#[derive(Debug, Default)]
pub struct ProgressSampler {
    downloaded: u64,
    sent_percent: Option<u8>,
    sent_bytes: u64,
    logged_step: Option<u8>,
}

/// What to do with a chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub progress: UpdateProgress,
    /// Whether the journal gets a line for this one.
    pub log: bool,
}

impl ProgressSampler {
    /// Adds a chunk; `Some` when it deserves an event.
    pub fn chunk(&mut self, bytes: usize, total: Option<u64>) -> Option<Sample> {
        self.downloaded = self.downloaded.saturating_add(bytes as u64);
        let total = total.filter(|total| *total > 0);
        let percent = total.map(|total| (self.downloaded.min(total) * 100 / total) as u8);
        let emit = match percent {
            Some(percent) => self.sent_percent != Some(percent),
            None => self.downloaded - self.sent_bytes >= UNKNOWN_TOTAL_STEP,
        };
        if !emit {
            return None;
        }
        self.sent_percent = percent;
        self.sent_bytes = self.downloaded;
        let step = percent.map(|percent| percent / LOG_STEP_PERCENT);
        let log = step.is_some() && step != self.logged_step;
        if log {
            self.logged_step = step;
        }
        Some(Sample {
            progress: UpdateProgress {
                phase: Phase::Downloading,
                downloaded: self.downloaded,
                total,
                percent,
            },
            log,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::test_util::TestDir;
    use chrono::TimeZone;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn at(hour: u32, minute: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, hour, minute, 0)
            .unwrap()
    }

    #[test]
    fn versions_compare_by_semver_precedence() {
        let newer = [
            ("0.1.0", "0.1.1"),
            ("0.1.9", "0.1.10"),
            ("0.9.0", "0.10.0"),
            ("0.1.0", "1.0.0"),
            ("0.2.0-rc.1", "0.2.0"),
            ("0.2.0-rc.1", "0.2.0-rc.2"),
            ("0.2.0-rc.2", "0.2.0-rc.10"),
        ];
        for (current, candidate) in newer {
            assert!(
                is_newer(&version(current), &version(candidate)),
                "{candidate} is newer than {current}"
            );
            assert!(
                !is_newer(&version(candidate), &version(current)),
                "{current} is not newer than {candidate}"
            );
        }
        for same in ["0.1.0", "1.2.3-beta.1"] {
            assert!(!is_newer(&version(same), &version(same)));
        }
        assert!(
            !is_newer(&version("0.1.0+a"), &version("0.1.0+b")),
            "build metadata does not order releases"
        );
        assert!(
            !is_newer(&version("0.2.0"), &version("0.2.0-rc.1")),
            "never offer a pre-release over its own release"
        );
    }

    #[test]
    fn a_folder_with_the_flag_is_portable_and_any_other_is_an_installation() {
        let dir = TestDir::new("update-kind");
        assert_eq!(detect_kind(dir.path()), Kind::Installer);

        std::fs::write(dir.path().join(PORTABLE_FLAG), b"").unwrap();
        assert_eq!(detect_kind(dir.path()), Kind::Portable);

        // A folder named like the flag is not the flag.
        let other = TestDir::new("update-kind-dir");
        std::fs::create_dir(other.path().join(PORTABLE_FLAG)).unwrap();
        assert_eq!(detect_kind(other.path()), Kind::Installer);

        // A folder that does not exist (unreadable executable path) is an installation.
        assert_eq!(detect_kind(&dir.path().join("missing")), Kind::Installer);
    }

    #[test]
    fn the_kind_is_serialized_in_lowercase() {
        assert_eq!(serde_json::to_value(Kind::Installer).unwrap(), "installer");
        assert_eq!(serde_json::to_value(Kind::Portable).unwrap(), "portable");
        assert_eq!(Kind::Portable.as_str(), "portable");
    }

    #[test]
    fn automatic_checks_run_at_most_once_per_24_hours() {
        let last = crate::queue::iso(at(8, 0, 11));
        let last = Some(last.as_str());
        // Injected clock: 1 minute before and exactly at the 24 hour mark, then later.
        assert!(!is_due(last, at(8, 0, 11)));
        assert!(!is_due(last, at(23, 59, 11)));
        assert!(!is_due(last, at(7, 59, 12)));
        assert!(is_due(last, at(8, 0, 12)));
        assert!(is_due(last, at(9, 0, 20)));
    }

    #[test]
    fn a_missing_unreadable_or_future_time_is_due() {
        let now = at(8, 0, 12);
        assert!(is_due(None, now));
        assert!(is_due(Some(""), now));
        assert!(is_due(Some("yesterday"), now));
        let future = crate::queue::iso(at(8, 0, 20));
        assert!(is_due(Some(&future), now), "the clock was moved back");
    }

    #[test]
    fn the_startup_check_needs_the_setting_and_a_due_time() {
        let now = at(8, 0, 12);
        let recent = crate::queue::iso(at(7, 0, 12));
        let old = crate::queue::iso(at(7, 0, 10));
        let case = |check_automatically, last: Option<&str>| Updates {
            check_automatically,
            last_check_at: last.map(str::to_owned),
        };
        assert!(should_check_at_startup(&case(true, None), now));
        assert!(should_check_at_startup(&case(true, Some(&old)), now));
        assert!(!should_check_at_startup(&case(true, Some(&recent)), now));
        assert!(!should_check_at_startup(&case(false, None), now));
        assert!(!should_check_at_startup(&case(false, Some(&old)), now));
        assert!(
            should_check_at_startup(&Updates::default(), now),
            "on by default"
        );
    }

    #[test]
    fn the_shipped_configuration_is_what_the_updater_accepts() {
        use tauri_plugin_updater::Config;

        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        let updater: Config = serde_json::from_value(conf["plugins"]["updater"].clone()).unwrap();
        assert_eq!(
            updater
                .endpoints
                .iter()
                .map(|url| url.as_str())
                .collect::<Vec<_>>(),
            ["https://github.com/yaniswav/squeezegram/releases/latest/download/latest.json"]
        );
        assert_eq!(updater.windows.unwrap().install_mode.to_string(), "passive");
        // latest.json is not signed: without this, a crafted manifest could pair a new version
        // number with an older, genuinely signed release and force a downgrade.
        assert!(updater.require_signed_version);
        // A public minisign key: base64 of "untrusted comment: minisign public key: ...".
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &updater.pubkey)
                .unwrap();
        assert!(decoded.starts_with(b"untrusted comment: minisign public key: "));
        // Signing the installer needs the private key, so only the release script turns it on.
        assert!(conf["bundle"].get("createUpdaterArtifacts").is_none());
    }

    #[test]
    fn the_page_has_no_permission_on_the_updater() {
        // The core drives the updater; a permission would let the page download and install.
        let capabilities = include_str!("../../capabilities/default.json");
        assert!(!capabilities.contains("updater:"));
        assert!(!capabilities.contains("process:"));
    }

    #[test]
    fn notices_serialize_as_the_page_expects() {
        let found = UpdateNotice::available("0.2.0".into(), Kind::Portable, Some("Fixes".into()));
        assert_eq!(
            serde_json::to_value(&found).unwrap(),
            serde_json::json!({
                "available": true,
                "version": "0.2.0",
                "kind": "portable",
                "notes": "Fixes",
                "url": "https://github.com/yaniswav/squeezegram/releases/latest"
            })
        );
        let none = UpdateNotice::up_to_date("0.1.0".into(), Kind::Installer);
        let value = serde_json::to_value(&none).unwrap();
        assert_eq!(value["available"], false);
        assert_eq!(value["version"], "0.1.0");
        assert!(
            value.get("notes").is_none(),
            "notes are left out when absent"
        );
    }

    #[test]
    fn errors_carry_a_code_per_phase_and_a_fixed_cause() {
        use tauri_plugin_updater::Error;
        let check = UpdateError::from_check(&Error::Network("down".into()));
        assert_eq!(check.code, "UPDATE_CHECK_FAILED");
        assert_eq!(check.cause, "network");
        assert_eq!(check.phase(), "check");

        // No release published yet (404 on latest.json): not a connection problem.
        let unpublished = UpdateError::from_check(&Error::ReleaseNotFound);
        assert_eq!(unpublished.code, "UPDATE_NOT_PUBLISHED");
        assert_eq!(unpublished.cause, "release_not_found");
        assert_eq!(unpublished.phase(), "check");

        let network = UpdateError::from_download(&Error::Network("HTTP 404".into()));
        assert_eq!(network.code, "UPDATE_DOWNLOAD_FAILED");
        assert_eq!(network.phase(), "download");

        let signature = UpdateError::from_download(&Error::SignatureUtf8("x".into()));
        assert_eq!(signature.code, "UPDATE_SIGNATURE_INVALID");
        assert_eq!(signature.cause, "signature");
        assert_eq!(signature.phase(), "download");

        let install = UpdateError::from_install(&Error::InvalidUpdaterFormat);
        assert_eq!(install.code, "UPDATE_INSTALL_FAILED");
        assert_eq!(install.phase(), "install");

        let portable = UpdateError::portable();
        assert_eq!(
            (portable.code, portable.cause),
            ("UPDATE_INSTALL_FAILED", "portable")
        );
    }

    #[test]
    fn progress_is_sampled_by_whole_percent_and_logged_every_ten() {
        let mut sampler = ProgressSampler::default();
        let total = Some(1000);
        let mut events = Vec::new();
        for _ in 0..100 {
            if let Some(sample) = sampler.chunk(10, total) {
                events.push(sample);
            }
        }
        assert_eq!(events.len(), 100, "one event per whole percent");
        assert_eq!(events[0].progress.percent, Some(1));
        assert_eq!(events[99].progress.percent, Some(100));
        assert_eq!(events[99].progress.downloaded, 1000);
        let logged: Vec<_> = events
            .iter()
            .filter(|sample| sample.log)
            .map(|sample| sample.progress.percent.unwrap())
            .collect();
        assert_eq!(logged, [1, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100]);

        // Small chunks inside the same percent send nothing.
        let mut sampler = ProgressSampler::default();
        assert!(sampler.chunk(1, Some(1_000_000)).is_some());
        assert!(sampler.chunk(1, Some(1_000_000)).is_none());
    }

    #[test]
    fn progress_without_a_length_is_sampled_by_size() {
        let mut sampler = ProgressSampler::default();
        assert!(sampler.chunk(100_000, None).is_none());
        let sample = sampler.chunk(500_000, None).unwrap();
        assert_eq!(sample.progress.downloaded, 600_000);
        assert_eq!(
            (sample.progress.total, sample.progress.percent),
            (None, None)
        );
        assert!(!sample.log, "no percent, no journal line");
        assert!(sampler.chunk(10, None).is_none());

        // A zero length is as good as none; a download past its announced length stays at 100.
        let mut sampler = ProgressSampler::default();
        assert_eq!(
            sampler.chunk(600_000, Some(0)).unwrap().progress.total,
            None
        );
        let mut sampler = ProgressSampler::default();
        assert_eq!(
            sampler.chunk(150, Some(100)).unwrap().progress.percent,
            Some(100)
        );
    }
}
