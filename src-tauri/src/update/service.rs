//! The update flow around `tauri-plugin-updater`: check, download, install, and what goes
//! out of it (events to the page, lines in the journal, the time of the last check).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use semver::Version;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use super::{
    Kind, Phase, ProgressSampler, Trigger, UPDATE_EVENT, UPDATE_PROGRESS_EVENT, UpdateError,
    UpdateInfo, UpdateNotice, UpdateProgress, is_newer, needs_new_key, release_page,
    should_check_at_startup,
};
use crate::commands::AppState;
use crate::entitlements::Entitlements;
use crate::logging;
use crate::settings::SettingsStore;

const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
// The plugin bounds the whole request, not idle time: leave slow links (about 300 kbit/s for
// a 60 MiB installer) room to finish.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;

fn download_too_large(downloaded: u64, total: Option<u64>) -> bool {
    downloaded > MAX_DOWNLOAD_BYTES || total.is_some_and(|total| total > MAX_DOWNLOAD_BYTES)
}

/// What the last check found, kept for the install and for a page opened later.
#[derive(Default)]
struct Cache {
    notice: Option<UpdateNotice>,
    update: Option<Update>,
}

pub struct UpdateService {
    settings: Arc<SettingsStore>,
    /// Read live at every decision: the key can change between the check and the install.
    entitlements: Arc<dyn Entitlements>,
    kind: Kind,
    current_version: String,
    /// Major of the running build (`entitlements::BUILD_MAJOR`; a parameter for tests).
    build_major: u32,
    cache: Mutex<Cache>,
    /// One check or install at a time.
    busy: tokio::sync::Mutex<()>,
    startup_done: AtomicBool,
}

impl UpdateService {
    pub fn new(
        settings: Arc<SettingsStore>,
        entitlements: Arc<dyn Entitlements>,
        kind: Kind,
        current_version: String,
        build_major: u32,
    ) -> Self {
        Self {
            settings,
            entitlements,
            kind,
            current_version,
            build_major,
            cache: Mutex::new(Cache::default()),
            busy: tokio::sync::Mutex::new(()),
            startup_done: AtomicBool::new(false),
        }
    }

    /// The notice is judged again against the key as it is now: a key saved or removed since
    /// the check changes what the page may offer.
    pub fn info(&self) -> UpdateInfo {
        let notice = self.lock_cache().notice.clone();
        UpdateInfo {
            kind: self.kind,
            current_version: self.current_version.clone(),
            notice: notice.map(|notice| {
                if notice.available {
                    let missing = self.missing_coverage(&notice.version);
                    notice.with_missing_coverage(missing)
                } else {
                    notice
                }
            }),
        }
    }

    /// The major the stored key covers, when installing `version` would need a new key (see
    /// `needs_new_key`); `None` when the install is allowed. Reads the live licence status.
    fn missing_coverage(&self, version: &str) -> Option<u32> {
        let covers_major = self.entitlements.status().covers_major;
        let candidate_major = Version::parse(version).ok().map(|parsed| parsed.major);
        covers_major.filter(|_| needs_new_key(covers_major, self.build_major, candidate_major))
    }

    /// Refuses an update the key does not cover. Called by `install` on the plugin's own
    /// `Update`, before any download: nothing the page sends takes part.
    fn check_coverage(&self, version: &str) -> Result<(), UpdateError> {
        match self.missing_coverage(version) {
            Some(_) => Err(self.fail(UpdateError::needs_new_key())),
            None => Ok(()),
        }
    }

    /// Page of the release announced in this session, else the latest-release page. The version
    /// comes from the plugin's `Update`, never from the page.
    pub fn page_url(&self) -> String {
        match &self.lock_cache().update {
            Some(update) => release_page(&update.version),
            None => super::RELEASES_PAGE.to_owned(),
        }
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The check made once per launch, when the page can listen: only if the user left
    /// automatic checks on and the last one is more than 24 hours old. A failure is journaled
    /// and otherwise silent.
    pub async fn check_at_startup(&self, app: &AppHandle) {
        if self.startup_done.swap(true, Ordering::SeqCst) {
            return;
        }
        if !should_check_at_startup(&self.settings.get().updates, chrono::Utc::now()) {
            tracing::debug!(
                event = "update_check_skipped",
                "automatic update check off or done within 24 hours"
            );
            return;
        }
        let _ = self.check(app, Trigger::Automatic).await;
    }

    /// Asks the endpoint for the latest release. Always emits `app://update` so every window
    /// agrees; the time of the check is recorded whatever the result.
    pub async fn check(
        &self,
        app: &AppHandle,
        trigger: Trigger,
    ) -> Result<UpdateNotice, UpdateError> {
        let _busy = self.busy.lock().await;
        self.check_locked(app, trigger).await
    }

    async fn check_locked(
        &self,
        app: &AppHandle,
        trigger: Trigger,
    ) -> Result<UpdateNotice, UpdateError> {
        let started = Instant::now();
        let result = self.query(app).await;
        self.record_check_time();
        let dur_ms = logging::ms_since(started);
        match result {
            Ok(found) => {
                let notice = match &found {
                    Some(update) => UpdateNotice::available(
                        update.version.clone(),
                        self.kind,
                        update.body.clone(),
                    )
                    .with_missing_coverage(self.missing_coverage(&update.version)),
                    None => UpdateNotice::up_to_date(self.current_version.clone(), self.kind),
                };
                tracing::info!(
                    event = "update_check",
                    trigger = trigger.as_str(),
                    kind = self.kind.as_str(),
                    ok = true,
                    available = notice.available,
                    dur_ms,
                    "update check done"
                );
                if let Some(update) = &found {
                    tracing::info!(
                        event = "update_available",
                        version = update.version.as_str(),
                        current_version = self.current_version.as_str(),
                        kind = self.kind.as_str(),
                        requires_new_key = notice.requires_new_key,
                        "a newer version exists"
                    );
                }
                {
                    let mut cache = self.lock_cache();
                    cache.notice = Some(notice.clone());
                    cache.update = found;
                }
                emit(app, UPDATE_EVENT, &notice);
                Ok(notice)
            }
            Err(error) => {
                tracing::info!(
                    event = "update_check",
                    trigger = trigger.as_str(),
                    kind = self.kind.as_str(),
                    ok = false,
                    dur_ms,
                    "update check failed"
                );
                log_failure(&error);
                Err(error)
            }
        }
    }

    /// The plugin's check, with our SemVer rule and the shutdown hook the installer needs.
    async fn query(&self, app: &AppHandle) -> Result<Option<Update>, UpdateError> {
        let hook_app = app.clone();
        let mut builder = app
            .updater_builder()
            .version_comparator(|current, release| is_newer(&current, &release.version))
            .on_before_exit(move || before_installer(&hook_app));
        if self.kind == Kind::Installer {
            let exe = std::env::current_exe().map_err(|_| {
                UpdateError::new(UpdateError::CHECK_FAILED, "executable_path_unreadable")
            })?;
            let dir = exe.parent().ok_or_else(|| {
                UpdateError::new(UpdateError::CHECK_FAILED, "executable_path_unreadable")
            })?;
            if super::installer::is_inno(dir)? {
                let background =
                    std::env::args_os().any(|arg| arg == crate::background::BACKGROUND_ARG);
                builder = builder
                    .restart_after_install(false)
                    .installer_args(super::installer::arguments(dir, background)?);
            }
        }
        let updater = builder
            .timeout(CHECK_TIMEOUT)
            .build()
            .map_err(|error| UpdateError::from_check(&error))?;
        updater
            .check()
            .await
            .map_err(|error| UpdateError::from_check(&error))
    }

    fn record_check_time(&self) {
        let patch = json!({ "updates": { "last_check_at": crate::queue::now_iso() } });
        if let Err(error) = self.settings.set(&patch) {
            tracing::warn!(event = "update_check_time_not_saved", error_code = error.code(), error = %error, "the time of the check was not saved");
        }
    }

    /// Downloads the update, verifies its signature, closes the app cleanly and starts the
    /// installer, which restarts the app. On success the process ends inside this call.
    /// The portable build refuses: it never installs anything. An update whose major the stored
    /// key does not cover is refused too, before the download.
    pub async fn install(&self, app: &AppHandle) -> Result<(), UpdateError> {
        if self.kind == Kind::Portable {
            return Err(self.fail(UpdateError::portable()));
        }
        let Ok(_busy) = self.busy.try_lock() else {
            return Err(self.fail(UpdateError::busy()));
        };
        let cached = self.lock_cache().update.clone();
        let mut update = match cached {
            Some(update) => update,
            None => {
                self.check_locked(app, Trigger::Manual).await?;
                let Some(update) = self.lock_cache().update.clone() else {
                    // Nothing newer exists any more: nothing to install, and not a failure.
                    return Ok(());
                };
                update
            }
        };
        self.check_coverage(&update.version)?;
        update.timeout = Some(DOWNLOAD_TIMEOUT);
        let bytes = self.download(app, &update).await?;
        // Again after the download: a key saved while it ran must not be moved to a major it
        // does not cover.
        self.check_coverage(&update.version)?;
        emit(
            app,
            UPDATE_PROGRESS_EVENT,
            &UpdateProgress {
                phase: Phase::Installing,
                downloaded: bytes.len() as u64,
                total: Some(bytes.len() as u64),
                percent: Some(100),
            },
        );
        // On Windows `install` runs the installer and exits the process; it only returns on
        // failure. It runs on a blocking thread because the shutdown hook waits for the
        // Telegram transport.
        let installed = tauri::async_runtime::spawn_blocking(move || update.install(bytes)).await;
        let error = match installed {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => UpdateError::from_install(&error),
            Err(_) => UpdateError {
                code: UpdateError::INSTALL_FAILED,
                cause: "task_failed",
            },
        };
        Err(self.fail(error))
    }

    async fn download(&self, app: &AppHandle, update: &Update) -> Result<Vec<u8>, UpdateError> {
        let started = Instant::now();
        let mut sampler = ProgressSampler::default();
        let limit = tokio::sync::Notify::new();
        let mut downloaded = 0_u64;
        let mut too_large = false;
        let result = tokio::select! {
            biased;
            _ = limit.notified() => {
                return Err(self.fail(UpdateError::new(UpdateError::DOWNLOAD_FAILED, "too_large")));
            }
            result = update
            .download(
                |chunk, total| {
                    downloaded = downloaded.saturating_add(chunk as u64);
                    if download_too_large(downloaded, total) {
                        too_large = true;
                        limit.notify_one();
                        return;
                    }
                    let Some(sample) = sampler.chunk(chunk, total) else {
                        return;
                    };
                    emit(app, UPDATE_PROGRESS_EVENT, &sample.progress);
                    if sample.log {
                        tracing::info!(
                            event = "update_download_progress",
                            percent = sample.progress.percent,
                            downloaded = sample.progress.downloaded,
                            total = sample.progress.total,
                            "update download"
                        );
                    }
                },
                || {},
            ) => result,
        };
        // The plugin may finish in the same poll that signals the limit.
        if too_large {
            return Err(self.fail(UpdateError::new(UpdateError::DOWNLOAD_FAILED, "too_large")));
        }
        match result {
            Ok(bytes) => {
                tracing::info!(
                    event = "update_downloaded",
                    version = update.version.as_str(),
                    size = bytes.len(),
                    dur_ms = logging::ms_since(started),
                    "update downloaded and its signature verified"
                );
                Ok(bytes)
            }
            Err(error) => Err(self.fail(UpdateError::from_download(&error))),
        }
    }

    fn fail(&self, error: UpdateError) -> UpdateError {
        log_failure(&error);
        error
    }

    /// Version announced by the last check, for the journal line written just before the
    /// installer starts.
    fn announced_version(&self) -> Option<String> {
        self.lock_cache()
            .notice
            .as_ref()
            .filter(|notice| notice.available)
            .map(|notice| notice.version.clone())
    }
}

fn log_failure(error: &UpdateError) {
    tracing::warn!(
        event = "update_failed",
        phase = error.phase(),
        error_code = error.code,
        cause = error.cause,
        "update step failed"
    );
}

fn emit<T: serde::Serialize + Clone>(app: &AppHandle, name: &str, payload: &T) {
    if let Err(error) = app.emit(name, payload) {
        tracing::warn!(event = "emit_failed", name, error = %error, "could not emit an event to the window");
    }
}

/// Called by the plugin once the installer is extracted, right before it starts it and
/// ends the process (no `RunEvent::Exit` follows): same clean shutdown as a normal exit.
fn before_installer(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        tracing::info!(
            event = "update_installed",
            version = state.updates.announced_version().as_deref(),
            "installer starting, the app closes"
        );
        crate::close_core(&state);
    }
    logging::app_exit("update");
    app.cleanup_before_exit();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entitlements::{EntitlementStatus, Feature};
    use crate::queue::test_util::TestDir;
    use crate::update::RELEASES_PAGE;

    #[test]
    fn announced_download_over_the_cap_is_refused() {
        assert!(download_too_large(0, Some(MAX_DOWNLOAD_BYTES + 1)));
    }

    #[test]
    fn cumulative_download_over_the_cap_is_refused() {
        let mut downloaded = MAX_DOWNLOAD_BYTES - 1;
        downloaded += 1;
        assert!(!download_too_large(downloaded, None));
        downloaded += 1;
        assert!(download_too_large(downloaded, None));
    }

    #[test]
    fn download_exactly_at_the_cap_is_allowed() {
        assert!(!download_too_large(
            MAX_DOWNLOAD_BYTES,
            Some(MAX_DOWNLOAD_BYTES)
        ));
        assert!(!download_too_large(MAX_DOWNLOAD_BYTES, None));
    }

    /// A licence status whose key can change while the service lives.
    struct Key(Mutex<Option<u32>>);

    impl Key {
        fn new(covers_major: Option<u32>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(covers_major)))
        }

        fn set(&self, covers_major: Option<u32>) {
            *self.0.lock().unwrap() = covers_major;
        }
    }

    impl Entitlements for Key {
        fn allows(&self, _feature: Feature) -> bool {
            false
        }

        fn status(&self) -> EntitlementStatus {
            EntitlementStatus {
                covers_major: *self.0.lock().unwrap(),
                ..EntitlementStatus::FREE_BUILD
            }
        }
    }

    fn service(dir: &TestDir, key: &Arc<Key>, build_major: u32) -> UpdateService {
        let settings = SettingsStore::load(dir.path().join("settings.json")).unwrap();
        UpdateService::new(
            Arc::new(settings),
            key.clone(),
            Kind::Installer,
            format!("{build_major}.0.0"),
            build_major,
        )
    }

    #[test]
    fn the_install_gate_refuses_a_major_the_key_does_not_cover() {
        let dir = TestDir::new("update-gate");
        let key = Key::new(Some(1));
        let service = service(&dir, &key, 1);

        let refused = service.check_coverage("2.0.0").unwrap_err();
        assert_eq!(refused.code, "UPDATE_NEEDS_NEW_KEY");
        assert_eq!(refused.phase(), "install");
        assert!(service.check_coverage("1.9.3").is_ok());
        assert!(service.check_coverage("1.0.0-rc.1").is_ok());
        assert!(service.check_coverage("2.0.0-rc.1").is_err());
        assert!(
            service.check_coverage("not a version").is_err(),
            "a version without a readable major fails closed for a covered user"
        );
    }

    #[test]
    fn a_revoked_key_does_not_block_updates_to_the_free_app() {
        use crate::entitlements::LicenseEntitlements;
        use crate::entitlements::license::test_vectors;
        use crate::transport::TelegramTransport;
        use crate::transport::mock::{MOCK_ME_ID, MockTransport};

        let dir = TestDir::new("update-revoked-key");
        let vectors = test_vectors::load();
        let account = MockTransport::authorized();
        let refunded = vectors.lifetime_token(MOCK_ME_ID, &[], "refunded-key");
        let path = dir.path().join(crate::entitlements::store::FILE_NAME);
        std::fs::write(&path, &refunded).unwrap();
        let key = LicenseEntitlements::load(path, vectors.key_ring(), Some(account.auth_state()))
            .with_revoked_jtis(&["refunded-key"]);
        assert_eq!(key.status().reason, Some("LIC_REVOKED"));
        assert_eq!(key.status().covers_major, None);
        let settings = SettingsStore::load(dir.path().join("settings.json")).unwrap();
        let service = UpdateService::new(
            Arc::new(settings),
            Arc::new(key),
            Kind::Installer,
            "1.0.0".into(),
            1,
        );
        assert_eq!(service.missing_coverage("2.0.0"), None);
        assert!(service.check_coverage("2.0.0").is_ok());
        assert!(service.check_coverage("999.0.0").is_ok());
    }

    #[test]
    fn the_install_gate_reads_the_key_as_it_is_at_install_time() {
        let dir = TestDir::new("update-gate-live");
        let key = Key::new(None);
        let service = service(&dir, &key, 1);
        assert!(service.check_coverage("2.0.0").is_ok(), "free user");

        // A key saved after the check blocks the install...
        key.set(Some(1));
        assert!(service.check_coverage("2.0.0").is_err());
        // ...and one that covers the new major, or none, lets it through.
        key.set(Some(2));
        assert!(service.check_coverage("2.0.0").is_ok());
        key.set(None);
        assert!(service.check_coverage("2.0.0").is_ok());
    }

    #[test]
    fn a_build_already_past_the_key_is_not_blocked() {
        let dir = TestDir::new("update-gate-past");
        let key = Key::new(Some(1));
        let service = service(&dir, &key, 2);
        assert!(service.check_coverage("2.1.0").is_ok());
        assert!(service.check_coverage("3.0.0").is_ok());
    }

    #[test]
    fn the_notice_is_judged_again_against_the_key_when_the_page_asks() {
        let dir = TestDir::new("update-info");
        let key = Key::new(None);
        let service = service(&dir, &key, 1);
        assert!(service.info().notice.is_none());

        service.lock_cache().notice = Some(UpdateNotice::available(
            "2.0.0".into(),
            Kind::Installer,
            None,
        ));
        let notice = service.info().notice.unwrap();
        assert!(!notice.requires_new_key);
        assert_eq!(notice.covers_major, None);

        key.set(Some(1));
        let notice = service.info().notice.unwrap();
        assert!(notice.requires_new_key);
        assert_eq!(notice.covers_major, Some(1));
        assert_eq!(
            notice.url,
            "https://github.com/yaniswav/squeezegram/releases/tag/v2.0.0"
        );

        // "Up to date" is left alone, whatever the key.
        service.lock_cache().notice =
            Some(UpdateNotice::up_to_date("1.0.0".into(), Kind::Installer));
        let notice = service.info().notice.unwrap();
        assert!(!notice.requires_new_key);
        assert_eq!(notice.url, RELEASES_PAGE);
    }

    #[test]
    fn without_an_announced_update_the_page_is_the_latest_release() {
        let dir = TestDir::new("update-page");
        let service = service(&dir, &Key::new(None), 1);
        assert_eq!(service.page_url(), RELEASES_PAGE);
    }
}
