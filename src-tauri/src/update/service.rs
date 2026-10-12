//! The update flow around `tauri-plugin-updater`: check, download, install, and what goes
//! out of it (events to the page, lines in the journal, the time of the last check).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use super::{
    Kind, Phase, ProgressSampler, Trigger, UPDATE_EVENT, UPDATE_PROGRESS_EVENT, UpdateError,
    UpdateInfo, UpdateNotice, UpdateProgress, is_newer, should_check_at_startup,
};
use crate::commands::AppState;
use crate::logging;
use crate::settings::SettingsStore;

/// What the last check found, kept for the install and for a page opened later.
#[derive(Default)]
struct Cache {
    notice: Option<UpdateNotice>,
    update: Option<Update>,
}

pub struct UpdateService {
    settings: Arc<SettingsStore>,
    kind: Kind,
    current_version: String,
    cache: Mutex<Cache>,
    /// One check or install at a time.
    busy: tokio::sync::Mutex<()>,
    startup_done: AtomicBool,
}

impl UpdateService {
    pub fn new(settings: Arc<SettingsStore>, kind: Kind, current_version: String) -> Self {
        Self {
            settings,
            kind,
            current_version,
            cache: Mutex::new(Cache::default()),
            busy: tokio::sync::Mutex::new(()),
            startup_done: AtomicBool::new(false),
        }
    }

    pub fn info(&self) -> UpdateInfo {
        UpdateInfo {
            kind: self.kind,
            current_version: self.current_version.clone(),
            notice: self.lock_cache().notice.clone(),
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
                    ),
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
        let updater = app
            .updater_builder()
            .version_comparator(|current, release| is_newer(&current, &release.version))
            .on_before_exit(move || before_installer(&hook_app))
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
    /// The portable build refuses: it never installs anything.
    pub async fn install(&self, app: &AppHandle) -> Result<(), UpdateError> {
        if self.kind == Kind::Portable {
            return Err(self.fail(UpdateError::portable()));
        }
        let Ok(_busy) = self.busy.try_lock() else {
            return Err(self.fail(UpdateError::busy()));
        };
        let cached = self.lock_cache().update.clone();
        let update = match cached {
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
        let bytes = self.download(app, &update).await?;
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
        let result = update
            .download(
                |chunk, total| {
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
            )
            .await;
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
