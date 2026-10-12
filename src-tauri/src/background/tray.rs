//! The tray icon and the window's visibility: everything of the background mode that needs the
//! window system. The rules it applies are the functions of the parent module.
//!
//! One worker task owns the changes: anything that may change what the tray shows (queue
//! events, settings, license and login changes) calls `refresh`, which wakes it; it waits a
//! moment so a burst of events costs one update, then creates, updates or removes the tray on
//! the main thread. Callers never wait for the window system, and no queue lock is ever held
//! while the tray is touched.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};
use tokio::sync::Notify;

use super::labels::Labels;
use super::{
    CloseBehavior, PENDING_GRACE, QUIT_REQUESTED, close_behavior, licensed_for_tray, queue_line,
    tray_wanted,
};
use crate::MAIN_WINDOW;
use crate::commands::AppState;
use crate::entitlements::{EntitlementSource, Entitlements, Feature};
use crate::queue::orchestrator::Orchestrator;
use crate::settings::SettingsStore;

const TRAY_ID: &str = "main";
const MENU_OPEN: &str = "tray_open";
const MENU_TOGGLE: &str = "tray_toggle";
const MENU_QUIT: &str = "tray_quit";
/// A burst of queue events is one update.
const COALESCE: Duration = Duration::from_millis(250);

/// The icon and the menu entries whose text changes.
struct TrayParts {
    tray: TrayIcon<Wry>,
    open: MenuItem<Wry>,
    toggle: MenuItem<Wry>,
    quit: MenuItem<Wry>,
}

pub struct Background {
    app: AppHandle,
    queue: Orchestrator,
    settings: Arc<SettingsStore>,
    entitlements: Arc<dyn Entitlements>,
    wake: Notify,
    tray: Mutex<Option<TrayParts>>,
    /// Since when the license has been waiting for its account, if it is.
    pending_since: Mutex<Option<Instant>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The state of the app, once `setup` has stored it.
fn background(app: &AppHandle) -> Option<Arc<Background>> {
    app.try_state::<AppState>()
        .map(|state| state.background.clone())
}

impl Background {
    pub fn new(
        app: AppHandle,
        queue: Orchestrator,
        settings: Arc<SettingsStore>,
        entitlements: Arc<dyn Entitlements>,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            queue,
            settings,
            entitlements,
            wake: Notify::new(),
            tray: Mutex::new(None),
            pending_since: Mutex::new(None),
        })
    }

    /// Starts the worker; the first update is requested at once.
    pub fn spawn(self: &Arc<Self>) {
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                this.wake.notified().await;
                tokio::time::sleep(COALESCE).await;
                let on_main = this.clone();
                if let Err(error) = this.app.run_on_main_thread(move || on_main.apply()) {
                    tracing::warn!(event = "tray_error", error = %error, "could not reach the main thread");
                }
            }
        });
        self.refresh();
    }

    /// Something that the tray shows or depends on may have changed.
    pub fn refresh(&self) {
        self.wake.notify_one();
    }

    /// Brings the window up (tray click, menu, second launch of the app).
    pub fn reveal(&self, from: &'static str) {
        let Some(window) = self.app.get_webview_window(MAIN_WINDOW) else {
            return;
        };
        match crate::show_and_focus(&window) {
            Ok(()) => tracing::info!(event = "app_shown", from, "window shown"),
            Err(error) => {
                tracing::warn!(event = "window_error", error = %error, "could not show the main window")
            }
        }
    }

    /// Whether closing the window hides it now: the choice and the license say so, and the
    /// tray exists to bring it back. Without the tray the app quits rather than vanish.
    pub fn hide_on_close(&self) -> bool {
        let tray_exists = lock(&self.tray).is_some();
        let behavior = close_behavior(
            self.licensed(tray_exists),
            self.settings.get().background.close_action,
        );
        behavior == CloseBehavior::Hide && tray_exists
    }

    /// Called when the window has been hidden instead of closed.
    pub fn hidden(&self) {
        tracing::info!(
            event = "app_hidden",
            reason = "close",
            "window hidden, the app keeps running"
        );
        self.refresh();
    }

    fn licensed(&self, tray_exists: bool) -> bool {
        let allows = self.entitlements.allows(Feature::Background);
        let pending = !allows && self.is_pending();
        let since = *lock(&self.pending_since);
        let pending_for = since.map_or(Duration::ZERO, |since| since.elapsed());
        licensed_for_tray(allows, pending, tray_exists, pending_for)
    }

    fn is_pending(&self) -> bool {
        self.entitlements.status().source == EntitlementSource::Pending
    }

    /// Quit asked from the tray: at once when nothing is being worked on, else the window
    /// comes up and asks.
    fn quit_from_tray(&self) {
        let running = self
            .queue
            .with_store(|store| store.activity())
            .map_or(0, |activity| activity.running);
        if running > 0 {
            tracing::info!(
                event = "quit_confirmation",
                running,
                "quit asked while items run, waiting for the user"
            );
            self.reveal("quit_confirmation");
            if let Err(error) = self.app.emit(QUIT_REQUESTED, json!({ "running": running })) {
                tracing::warn!(event = "emit_failed", name = QUIT_REQUESTED, error = %error, "could not emit an event to the window");
            }
        } else {
            self.quit("tray");
        }
    }

    /// The exit itself: `RunEvent::ExitRequested` runs the clean shutdown.
    pub fn quit(&self, from: &'static str) {
        tracing::info!(event = "tray_quit", from, "quit requested");
        self.app.exit(0);
    }

    fn on_menu(&self, id: &str) {
        tracing::info!(event = "tray_action", action = id, "tray menu");
        match id {
            MENU_OPEN => self.reveal("tray_menu"),
            MENU_TOGGLE => {
                if self.queue.is_running() {
                    self.queue.pause();
                } else {
                    self.queue.start();
                }
                self.refresh();
            }
            MENU_QUIT => self.quit_from_tray(),
            _ => {}
        }
    }

    /// Main thread: brings the tray in line with the license, the settings and the queue.
    fn apply(self: &Arc<Self>) {
        let settings = self.settings.get();
        let allows = self.entitlements.allows(Feature::Background);
        let pending = !allows && self.is_pending();
        self.track_pending(pending);

        let mut tray = lock(&self.tray);
        let licensed = self.licensed(tray.is_some());
        let wanted = tray_wanted(licensed, settings.background.close_action);
        if wanted && tray.is_none() {
            match self.build(&Labels::for_language(&settings.ui.language)) {
                Ok(parts) => {
                    *tray = Some(parts);
                    tracing::info!(event = "tray_created", "tray icon created");
                }
                Err(error) => {
                    tracing::warn!(event = "tray_error", error = %error, "could not create the tray icon");
                }
            }
        } else if !wanted && tray.is_some() {
            self.app.remove_tray_by_id(TRAY_ID);
            *tray = None;
            tracing::info!(event = "tray_removed", "tray icon removed");
            // A hidden window with no tray would leave no way back.
            let hidden = self
                .app
                .get_webview_window(MAIN_WINDOW)
                .is_some_and(|window| !window.is_visible().unwrap_or(true));
            if hidden {
                self.reveal("tray_removed");
            }
        }
        if let Some(parts) = tray.as_ref() {
            self.update(parts, &Labels::for_language(&settings.ui.language));
        }
    }

    /// Remembers since when the license waits for its account, and wakes up when the grace
    /// that keeps the tray is over.
    fn track_pending(self: &Arc<Self>, pending: bool) {
        let mut since = lock(&self.pending_since);
        match (pending, since.is_some()) {
            (true, false) => {
                *since = Some(Instant::now());
                let this = self.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(PENDING_GRACE + Duration::from_secs(1)).await;
                    this.refresh();
                });
            }
            (false, true) => *since = None,
            _ => {}
        }
    }

    fn build(&self, labels: &Labels) -> tauri::Result<TrayParts> {
        let open = MenuItem::with_id(&self.app, MENU_OPEN, &labels.open, true, None::<&str>)?;
        let toggle = MenuItem::with_id(&self.app, MENU_TOGGLE, &labels.pause, true, None::<&str>)?;
        let quit = MenuItem::with_id(&self.app, MENU_QUIT, &labels.quit, true, None::<&str>)?;
        let separator = PredefinedMenuItem::separator(&self.app)?;
        let items: [&dyn IsMenuItem<Wry>; 4] = [&open, &toggle, &separator, &quit];
        let menu = Menu::with_items(&self.app, &items)?;
        let mut builder = TrayIconBuilder::with_id(TRAY_ID)
            .menu(&menu)
            // Left click opens the window; the menu is the right click.
            .show_menu_on_left_click(false)
            .on_menu_event(|app, event| {
                if let Some(background) = background(app) {
                    background.on_menu(event.id().as_ref());
                }
            })
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                    && let Some(background) = background(tray.app_handle())
                {
                    background.reveal("tray_click");
                }
            });
        if let Some(icon) = self.app.default_window_icon() {
            builder = builder.icon(icon.clone());
        }
        let tray = builder.build(&self.app)?;
        Ok(TrayParts {
            tray,
            open,
            toggle,
            quit,
        })
    }

    /// Texts and tooltip from the language and the queue.
    fn update(&self, parts: &TrayParts, labels: &Labels) {
        let total = self
            .queue
            .with_store(|store| store.activity())
            .map_or(0, |activity| activity.total());
        let running = self.queue.is_running();
        let toggle = if running {
            &labels.pause
        } else {
            &labels.resume
        };
        let results = [
            parts.open.set_text(&labels.open),
            parts.toggle.set_text(toggle),
            parts.quit.set_text(&labels.quit),
            parts
                .tray
                .set_tooltip(Some(labels.tooltip(queue_line(total, running)))),
        ];
        for error in results.into_iter().filter_map(Result::err) {
            tracing::warn!(event = "tray_error", error = %error, "could not update the tray");
        }
    }
}
