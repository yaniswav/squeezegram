//! squeezegram core.
//!
//! Modules:
//!
//! - `queue`: item state machine, sequential orchestrator, SQLite persistence
//! - `transport`: `TelegramTransport` trait, `MockTransport` (debug builds), `TdlibJson`, the
//!   startup choice between them (`transport::select`) and the send stage
//! - `media`: ffprobe, plan, encoder detection, ffmpeg encode/remux/photo, BLAKE3 hashing,
//!   child processes with a Job Object (`media::procs`)
//! - `sidecars`: ffmpeg/ffprobe resolution and download, path of the bundled `tdjson.dll`
//! - `watch`: watched folders (polling, file size stability, `pending_review` only)
//! - `session_end`: the clean shutdown when Windows logs off or shuts down
//! - `background`: close to the tray, the tray icon and its menu (paid feature `background`)
//! - `settings`: versioned settings
//! - `limit`: the effective upload limit (settings, else the signed-in account)
//! - `logging`: the JSON Lines journal
//! - `entitlements`: paid features, granted by a license verified offline (`LicenseEntitlements`)
//! - `update`: update checks (installer: download and install; portable: tell the user)

pub mod background;
pub mod caption;
mod commands;
pub mod entitlements;
pub mod limit;
pub mod logging;
pub mod media;
pub mod queue;
mod session_end;
pub mod settings;
pub mod sidecars;
pub mod transport;
pub mod update;
pub mod watch;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, RunEvent, WebviewWindow, WindowEvent};

use background::Background;
use background::autostart::RegistryAutostart;
use commands::{AppState, StatusFeed, TauriEvents};
use entitlements::{Entitlements, LicenseEntitlements};
use limit::LimitResolver;
use media::stage::FfmpegStage;
use queue::orchestrator::Orchestrator;
use queue::store::Store;
use settings::SettingsStore;
use sidecars::SidecarService;
use transport::TelegramTransport;
use transport::select::TransportKind;
use transport::stage::TransportStage;
use update::UpdateService;
use watch::WatchService;

/// Label of the main window, see `app.windows` in `tauri.conf.json`.
pub(crate) const MAIN_WINDOW: &str = "main";
/// `close` then `authorizationStateClosed` took 6 to 22 ms when measured; bounded anyway.
const TRANSPORT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) fn show_and_focus(window: &WebviewWindow) -> tauri::Result<()> {
    window.unminimize()?;
    window.show()?;
    window.set_focus()
}

/// Called in the running instance whenever the user starts the app a second time.
fn focus_main_window(app: &AppHandle, args: &[String]) {
    tracing::info!(
        event = "second_instance",
        hidden = background::started_in_background(args),
        "second launch forwarded to this instance"
    );
    // The login entry firing while the app already runs must not pull its window up.
    if background::started_in_background(args) {
        return;
    }
    if let Some(window) = app.get_webview_window(MAIN_WINDOW)
        && let Err(error) = show_and_focus(&window)
    {
        tracing::warn!(event = "window_error", error = %error, "could not focus the main window");
    }
}

/// What is loaded before the window: needed by `app_start` (ffmpeg in use, journal level).
struct Boot {
    db_path: PathBuf,
    settings: Arc<SettingsStore>,
    sidecars: Arc<SidecarService>,
}

/// Paths and settings; logs nothing (the first journal line must be `app_start`).
fn boot() -> Result<Boot, Box<dyn std::error::Error>> {
    let db_path = queue::db::default_path()?;
    let settings = Arc::new(SettingsStore::load(SettingsStore::path_next_to(&db_path))?);
    let sidecar_base = SidecarService::default_base().ok_or("LOCALAPPDATA is not set")?;
    let sidecars = Arc::new(SidecarService::new(sidecar_base, settings.clone()));
    Ok(Boot {
        db_path,
        settings,
        sidecars,
    })
}

/// Opens the database, applies the startup recovery rules and builds the queue.
fn open_state(app: &AppHandle, boot: Boot) -> Result<AppState, Box<dyn std::error::Error>> {
    let Boot {
        db_path,
        settings,
        sidecars,
    } = boot;
    let mut store = Store::open(&db_path)?;
    let recovered = queue::recovery::on_startup(&mut store)?;
    // After recovery, so the temp files of interrupted items are already gone and only what no
    // item points to is left to remove.
    queue::sweep::sweep_default_root(&settings.get().temp, &store);
    // `SQUEEZEGRAM_TRANSPORT`, identity and `tdjson.dll`; logs `transport_selected`. Nothing
    // touches the network here.
    let transport::select::Selection {
        transport,
        kind: transport_kind,
        ..
    } = transport::select::select(&transport::select::Inputs::from_process(&settings.get()));
    // The plan and the upload share one limit: settings, else the signed-in account.
    let limits = Arc::new(LimitResolver::new(
        settings.clone(),
        Some(transport.auth_state()),
    ));
    // `license.txt` beside `settings.json`, judged against the account the transport signs in.
    let entitlements = Arc::new(LicenseEntitlements::production(
        LicenseEntitlements::path_beside(&SettingsStore::path_next_to(&db_path)),
        Some(transport.auth_state()),
    ));
    let media = Arc::new(FfmpegStage::new(
        sidecars.clone(),
        settings.clone(),
        limits.clone(),
        entitlements.clone(),
    ));
    let send = Arc::new(TransportStage::new(
        transport.clone(),
        settings.clone(),
        Arc::new(media::thumb::FfmpegThumbnails::new(sidecars.clone())),
    ));
    let queue = Orchestrator::new(
        store,
        media.clone(),
        send,
        Arc::new(TauriEvents(app.clone())),
        settings.get().pause_range(),
    );
    // Its own thread; lists nothing until the license grants `watched_folder`.
    let watcher = Arc::new(WatchService::start(
        queue.clone(),
        entitlements.clone(),
        settings.clone(),
    )?);
    let background = Background::new(
        app.clone(),
        queue.clone(),
        settings.clone(),
        entitlements.clone(),
        Arc::new(RegistryAutostart),
        background::started_in_background(std::env::args()),
    );
    let updates = Arc::new(UpdateService::new(
        settings.clone(),
        update::current_kind(),
        app.package_info().version.to_string(),
    ));
    Ok(AppState {
        queue,
        settings,
        sidecars,
        transport,
        transport_kind,
        media,
        limits,
        entitlements,
        entitlement_feed: Arc::new(StatusFeed::default()),
        watcher,
        background,
        updates,
        recovered: Mutex::new(recovered),
    })
}

/// Starts the transport and forwards every login state on `auth://state`. With TDLib the
/// start stops at `WaitPhone`: no account is signed in until the user types something.
fn spawn_transport(
    app: &AppHandle,
    transport: Arc<dyn TelegramTransport>,
    kind: TransportKind,
    queue: Orchestrator,
) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut states = transport.auth_state();
        // `TG_LIB_MISSING`, `TG_NOT_CONFIGURED`… reach the UI as an alert.
        if let Err(error) = transport::select::start(transport.as_ref()).await {
            let (code, params) = error.code_and_params();
            queue.alert(None, code, params);
        }
        loop {
            let payload = {
                let state = states.borrow_and_update();
                transport::service::log_auth_state(&state);
                transport::service::auth_payload(&state, kind)
            };
            commands::emit_auth_state(&app, &payload);
            if states.changed().await.is_err() {
                return;
            }
        }
    });
}

/// Sends `entitlements://status` after every change of the login state (the commands send it
/// after a license change). The status is computed for the account signed in at that moment.
/// The folder watcher and the tray are woken too: their gates follow the same verdict.
fn spawn_entitlements_watch(
    app: &AppHandle,
    entitlements: Arc<LicenseEntitlements>,
    feed: Arc<StatusFeed>,
    watcher: Arc<WatchService>,
    background: Arc<Background>,
    mut states: tokio::sync::watch::Receiver<transport::AuthState>,
) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            states.borrow_and_update();
            feed.publish(&app, entitlements.status());
            watcher.wake();
            background.refresh();
            if states.changed().await.is_err() {
                return;
            }
        }
    });
}

/// Once the page can listen: the update check, which waits for the network off the UI thread.
fn start_update_check(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(state) = app.try_state::<AppState>() {
            state.updates.check_at_startup(&app).await;
        }
    });
}

/// Announces the items changed by the startup recovery once the page can listen.
fn announce_recovery(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        let ids = std::mem::take(
            &mut *state
                .recovered
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        state.queue.emit_changed(&ids);
    }
}

/// Journal, then `app_start` with the versions: the first line of every session.
fn start_journal(boot: &Result<Boot, Box<dyn std::error::Error>>) {
    let Some(dir) = logging::default_dir() else {
        eprintln!("journal: LOCALAPPDATA is not set, no journal");
        return;
    };
    let boot = boot.as_ref().ok();
    let level = boot.and_then(|b| b.settings.get().logging.level);
    if let Err(error) = logging::init(dir, level.as_deref()) {
        eprintln!("journal: {error}");
        return;
    }
    let versions = logging::system::versions(boot.map(|b| b.sidecars.as_ref()));
    logging::app_start(&versions);
}

/// Clean shutdown, shared by the exit and by the update install (which ends the process
/// without `RunEvent::Exit`): the folder watcher stops (bounded) so it queues nothing more,
/// an item still uploading becomes `unknown`, then the transport closes (`close`, wait for
/// `authorizationStateClosed`, bounded). Blocks: call it from a thread that is not a runtime
/// worker.
pub(crate) fn close_core(state: &AppState) {
    state.watcher.stop();
    let (queue, transport) = (&state.queue, state.transport.clone());
    if let Err(error) = queue.shutdown() {
        tracing::error!(event = "shutdown_error", error_code = error.code(), error = %error, "queue shutdown failed");
    }
    let started = Instant::now();
    let timed_out = tauri::async_runtime::block_on(async move {
        tokio::time::timeout(TRANSPORT_SHUTDOWN_TIMEOUT, transport.shutdown())
            .await
            .is_err()
    });
    tracing::info!(
        event = "transport_shutdown",
        dur_ms = logging::ms_since(started),
        timed_out,
        "Telegram transport closed"
    );
}

pub fn run() {
    let boot = boot();
    start_journal(&boot);
    let boot = match boot {
        Ok(boot) => Some(boot),
        Err(error) => {
            tracing::error!(event = "init_error", step = "boot", error = %error, "startup failed before the window");
            None
        }
    };

    let app = tauri::Builder::default()
        // Must stay the first plugin: a second launch is forwarded to this instance, then exits.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            focus_main_window(app, &args);
        }))
        .plugin(tauri_plugin_dialog::init())
        // Driven from `commands::update_*` only: the page gets no permission on the plugin.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(move |app| {
            let boot = boot.ok_or("startup failed (see the init_error line)")?;
            let state = open_state(app.handle(), boot).inspect_err(|error| {
                tracing::error!(event = "init_error", step = "open_state", error = %error, "the queue could not be opened");
            })?;
            let queue = state.queue.clone();
            spawn_transport(
                app.handle(),
                state.transport.clone(),
                state.transport_kind,
                queue.clone(),
            );
            spawn_entitlements_watch(
                app.handle(),
                state.entitlements.clone(),
                state.entitlement_feed.clone(),
                state.watcher.clone(),
                state.background.clone(),
                state.transport.auth_state(),
            );
            let analyzer = queue.clone();
            tauri::async_runtime::spawn(async move { analyzer.run_analysis().await });
            tauri::async_runtime::spawn(async move { queue.run().await });
            let background = state.background.clone();
            app.manage(state);
            background.spawn();
            #[cfg(windows)]
            session_end::install(app.handle());
            tracing::info!(event = "setup_done", "core ready");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::queue_add,
            commands::queue_list,
            commands::queue_promote,
            commands::queue_start,
            commands::queue_pause,
            commands::queue_status,
            commands::queue_cancel,
            commands::queue_reorder,
            commands::queue_retry,
            commands::queue_clear,
            commands::item_update,
            commands::item_encoding_set,
            commands::item_encoding_estimate,
            commands::codec_support,
            commands::item_reconcile,
            commands::item_resolve_unknown,
            commands::destinations_refresh,
            commands::destinations_search,
            commands::destination_topics,
            commands::destinations_list,
            commands::destination_update,
            commands::destination_check,
            commands::watch_list,
            commands::watch_add,
            commands::watch_update,
            commands::watch_remove,
            commands::auth_state,
            commands::auth_submit,
            commands::auth_logout,
            commands::settings_get,
            commands::settings_set,
            commands::history_query,
            commands::history_clear,
            commands::sidecars_status,
            commands::encoder_status,
            commands::caption_preview,
            commands::limit_status,
            commands::entitlements_status,
            commands::license_set,
            commands::license_clear,
            commands::license_open_purchase,
            commands::sidecars_install,
            commands::sidecars_discover,
            commands::sidecars_use,
            commands::log_frontend,
            commands::log_path,
            commands::log_open_dir,
            commands::debug_export,
            commands::update_info,
            commands::update_check,
            commands::update_install,
            commands::update_open_page,
            commands::app_quit,
            commands::autostart_status,
            commands::autostart_set,
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == MAIN_WINDOW
                && let Some(state) = window.app_handle().try_state::<AppState>()
                && state.background.hide_on_close()
            {
                api.prevent_close();
                match window.hide() {
                    Ok(()) => state.background.hidden(),
                    Err(error) => {
                        tracing::warn!(event = "window_error", error = %error, "could not hide the main window")
                    }
                }
            }
        })
        // The window is created hidden (tauri.conf.json) and shown once the page has loaded,
        // so a dark theme never flashes white while the WebView starts.
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Finished {
                tracing::info!(event = "page_loaded", "page loaded");
                // Shown now, unless the app started hidden in the tray.
                match webview.app_handle().try_state::<AppState>() {
                    Some(state) => state.background.page_loaded(),
                    None => {
                        if let Err(error) = webview.window().show() {
                            tracing::warn!(event = "window_error", error = %error, "could not show the main window");
                        }
                    }
                }
                announce_recovery(webview.app_handle());
                start_update_check(webview.app_handle());
            }
        })
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            tracing::error!(event = "init_error", step = "tauri_build", error = %error, "the application could not be built");
            panic!("error while building the Tauri application: {error}");
        }
    };
    app.run(|app, event| match event {
        // Clean shutdown: the watcher stops, an item still uploading becomes `unknown`, then
        // the transport closes (`close`, wait for `authorizationStateClosed`, bounded).
        RunEvent::ExitRequested { .. } => {
            tracing::info!(event = "shutdown_start", "exit requested");
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            close_core(&state);
        }
        RunEvent::Exit => logging::app_exit("exit"),
        _ => {}
    });
}
