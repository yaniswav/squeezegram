//! Automatic updates.
//!
//! The installer build checks GitHub for the latest release, downloads the signed installer,
//! lets `tauri-plugin-updater` verify its minisign signature against the public key in
//! `tauri.conf.json`, then hands over to the installer, which restarts the app.
//! The portable build only tells the user that a newer version exists and where to get it: it
//! never downloads or replaces anything.
//!
//! What a check sends: one HTTPS GET on the `latest.json` endpoint with the plugin's default
//! headers (`Accept` and `User-Agent: tauri-plugin-updater/<plugin version>`). The endpoint
//! URL has no `{{current_version}}`, `{{target}}` or `{{arch}}` placeholder, so the app
//! version, the Windows version and the architecture are not sent either. GitHub sees the IP
//! address, as for any download. No identifier, no Telegram data.
//!
//! A key covers the app up to a major version (`maj`). The updater never moves a covered user
//! to a major the key does not cover: the notice says so (`requires_new_key`) and the install
//! refuses, both decided in the core by `needs_new_key`.
//!
//! - this file: kinds, notices, version comparison, key rule, cadence, error codes, progress
//!   sampling
//! - `installer`: Inno marker and validated installer arguments
//! - `service`: the plugin calls, the cached result, the events and the journal lines

mod installer;
mod service;

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use semver::Version;
use serde::Serialize;

pub use service::UpdateService;

use crate::settings::Updates;

/// Page of the latest release: where the user is sent when no newer version was announced.
pub const RELEASES_PAGE: &str = "https://github.com/yaniswav/squeezegram/releases/latest";
/// Pages of the tagged releases; the tag is `v` plus the version.
const RELEASE_TAG_PAGE: &str = "https://github.com/yaniswav/squeezegram/releases/tag/v";
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
    /// Installed by a supported Windows installer: can update itself.
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
    /// Page of the announced release (the latest one when nothing newer exists), for "open the
    /// download page".
    pub url: String,
    /// The stored key does not cover the announced version's major: the page offers a new key
    /// instead of the install, which the core would refuse anyway.
    pub requires_new_key: bool,
    /// The highest major the key covers; only present with `requires_new_key`, for the text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers_major: Option<u32>,
}

impl UpdateNotice {
    pub fn available(version: String, kind: Kind, notes: Option<String>) -> Self {
        Self {
            available: true,
            url: release_page(&version),
            version,
            kind,
            notes,
            requires_new_key: false,
            covers_major: None,
        }
    }

    pub fn up_to_date(current_version: String, kind: Kind) -> Self {
        Self {
            available: false,
            version: current_version,
            kind,
            notes: None,
            url: RELEASES_PAGE.to_owned(),
            requires_new_key: false,
            covers_major: None,
        }
    }

    /// Records the key rule's answer: the major the key covers when the announced version
    /// needs a new key, else `None`.
    pub fn with_missing_coverage(mut self, covers_major: Option<u32>) -> Self {
        self.requires_new_key = covers_major.is_some();
        self.covers_major = covers_major;
        self
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

/// Page of the release `version` on GitHub, or the latest-release page when the text is not a
/// plain SemVer version: the text ends up in a URL that is handed to the shell, so only the
/// SemVer characters go through.
pub fn release_page(version: &str) -> String {
    let plain = !version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
        && Version::parse(version).is_ok();
    if plain {
        format!("{RELEASE_TAG_PAGE}{version}")
    } else {
        RELEASES_PAGE.to_owned()
    }
}

/// Whether installing a release of major `candidate_major` would leave a user the key covers
/// today without coverage. True when a stored key's signature verified (`covers_major`), this
/// build is still within it, and the candidate is beyond it. A build already beyond the key has
/// nothing left to lose, and a free user has no key: neither is ever blocked. A candidate whose
/// major cannot be read (`None`) counts as beyond, so the rule fails closed.
pub fn needs_new_key(
    covers_major: Option<u32>,
    build_major: u32,
    candidate_major: Option<u64>,
) -> bool {
    let Some(covers) = covers_major else {
        return false;
    };
    build_major <= covers && candidate_major.is_none_or(|major| major > u64::from(covers))
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
    pub const NEEDS_NEW_KEY: &'static str = "UPDATE_NEEDS_NEW_KEY";

    fn new(code: &'static str, cause: &'static str) -> Self {
        Self { code, cause }
    }

    /// The portable build never installs.
    pub fn portable() -> Self {
        Self::new(Self::INSTALL_FAILED, "portable")
    }

    /// The stored key does not cover the major of the update.
    pub fn needs_new_key() -> Self {
        Self::new(Self::NEEDS_NEW_KEY, "major_not_covered")
    }

    fn busy() -> Self {
        Self::new(Self::INSTALL_FAILED, "busy")
    }

    /// Where the failure happened, for the journal (`check`, `download`, `install`).
    pub fn phase(&self) -> &'static str {
        match self.code {
            Self::CHECK_FAILED | Self::NOT_PUBLISHED => "check",
            Self::INSTALL_FAILED | Self::NEEDS_NEW_KEY => "install",
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
                "url": "https://github.com/yaniswav/squeezegram/releases/tag/v0.2.0",
                "requires_new_key": false
            })
        );
        let blocked = UpdateNotice::available("2.0.0".into(), Kind::Installer, None)
            .with_missing_coverage(Some(1));
        assert_eq!(
            serde_json::to_value(&blocked).unwrap(),
            serde_json::json!({
                "available": true,
                "version": "2.0.0",
                "kind": "installer",
                "url": "https://github.com/yaniswav/squeezegram/releases/tag/v2.0.0",
                "requires_new_key": true,
                "covers_major": 1
            })
        );
        assert!(
            !blocked.with_missing_coverage(None).requires_new_key,
            "judged again, the same notice can be allowed"
        );
        let none = UpdateNotice::up_to_date("0.1.0".into(), Kind::Installer);
        let value = serde_json::to_value(&none).unwrap();
        assert_eq!(value["available"], false);
        assert_eq!(value["version"], "0.1.0");
        assert_eq!(value["url"], RELEASES_PAGE);
        assert_eq!(value["requires_new_key"], false);
        assert!(value.get("covers_major").is_none());
        assert!(
            value.get("notes").is_none(),
            "notes are left out when absent"
        );
    }

    #[test]
    fn a_new_key_is_needed_only_by_a_covered_user_moving_past_the_key() {
        // (covers_major, build_major, candidate_major, needs_new_key)
        let cases = [
            // Sold today: maj 1 covers 0.x and 1.x.
            (Some(1), 0, 0, false),
            (Some(1), 0, 1, false),
            (Some(1), 1, 1, false),
            (Some(1), 0, 2, true),
            (Some(1), 1, 2, true),
            (Some(1), 1, 3, true),
            // Already beyond the key: nothing left to lose, never blocked.
            (Some(1), 2, 2, false),
            (Some(1), 2, 3, false),
            (Some(1), 3, 3, false),
            // A key that covers more.
            (Some(2), 1, 2, false),
            (Some(2), 2, 3, true),
            (Some(0), 0, 1, true),
            (Some(0), 0, 0, false),
            // No verified key (free, no licence, bad signature): never blocked.
            (None, 0, 3, false),
            (None, 1, 2, false),
            (None, 5, 9, false),
        ];
        for (covers, build, candidate, expected) in cases {
            assert_eq!(
                needs_new_key(covers, build, Some(candidate)),
                expected,
                "key {covers:?}, build {build}, candidate {candidate}"
            );
        }
        // A candidate whose major cannot be read fails closed for a covered user only.
        assert!(needs_new_key(Some(1), 1, None));
        assert!(!needs_new_key(Some(1), 2, None));
        assert!(!needs_new_key(None, 1, None));
    }

    #[test]
    fn the_release_page_is_the_tag_of_a_plain_version_and_else_the_latest() {
        for (version, page) in [
            ("0.2.0", "releases/tag/v0.2.0"),
            ("2.0.0", "releases/tag/v2.0.0"),
            ("1.0.0-rc.1", "releases/tag/v1.0.0-rc.1"),
            ("1.0.0+build.5", "releases/tag/v1.0.0+build.5"),
        ] {
            assert_eq!(
                release_page(version),
                format!("https://github.com/yaniswav/squeezegram/{page}")
            );
        }
        for hostile in [
            "",
            "latest",
            "1.0",
            "../../evil",
            "1.0.0/../x",
            "1.0.0?x=1",
            "1.0.0#frag",
            "1.0.0 --flag",
            "1.0.0&calc",
            "1.0.0\\x",
            "1.0.0\n",
            "v1.0.0",
            "\u{661}.0.0",
        ] {
            assert_eq!(release_page(hostile), RELEASES_PAGE, "{hostile:?}");
        }
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

        let new_key = UpdateError::needs_new_key();
        assert_eq!(
            (new_key.code, new_key.cause, new_key.phase()),
            ("UPDATE_NEEDS_NEW_KEY", "major_not_covered", "install")
        );

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
