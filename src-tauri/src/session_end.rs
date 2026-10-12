//! Windows logoff and shutdown.
//!
//! The event loop (tao) answers `WM_ENDSESSION` by ending the process on the spot: Tauri's
//! `RunEvent::ExitRequested` never fires, so `close_core` would not run, and a PC shut down
//! with the app in the tray would leave the watcher running, an upload `uploading` instead of
//! `unknown`, and TDLib not closed. A subclass on the main window sees the message first and
//! runs the same clean shutdown, bounded so that it never keeps the session from ending.
//!
//! It acts on `WM_ENDSESSION` with `wParam` true, not on `WM_QUERYENDSESSION`: the query only
//! asks whether the session may end and any program can answer no, in which case the app must
//! go on with its transport open (tao makes the same choice for its own handling).

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// What the shutdown may take once Windows has said the session ends. Windows stops waiting
/// for programs after a few seconds; `close_core` alone can take longer (watcher 2 s, TDLib
/// 3 s), so what is not done by then is left to the recovery rules.
pub const BUDGET: Duration = Duration::from_secs(3);

/// Runs `work` on a thread of its own and waits for it at most `budget`. Returns whether it
/// ended in time; a late `work` is left running and ends with the process.
pub fn run_bounded(budget: Duration, work: impl FnOnce() + Send + 'static) -> bool {
    let (done, finished) = mpsc::channel::<()>();
    let started = thread::Builder::new()
        .name("session-end".into())
        .spawn(move || {
            work();
            let _ = done.send(());
        });
    if started.is_err() {
        return false;
    }
    // A closure that panicked has ended too: the sender is gone.
    !matches!(
        finished.recv_timeout(budget),
        Err(RecvTimeoutError::Timeout)
    )
}

#[cfg(windows)]
pub use subclass::install;

#[cfg(windows)]
mod subclass {
    use tauri::{AppHandle, Manager};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{WM_ENDSESSION, WM_NCDESTROY};

    use super::{BUDGET, run_bounded};
    use crate::MAIN_WINDOW;
    use crate::commands::AppState;

    /// Identifies this subclass on the window ("SQZ1").
    const SUBCLASS_ID: usize = 0x5351_5a31;

    /// Subclasses the main window. Call it on the thread that created the window (`setup`).
    pub fn install(app: &AppHandle) {
        let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
            return;
        };
        let hwnd = match window.hwnd() {
            // Tauri's HWND comes from another version of the `windows` crate.
            Ok(hwnd) => HWND(hwnd.0),
            Err(error) => {
                tracing::warn!(event = "window_error", error = %error, "no window handle, a Windows shutdown will not close cleanly");
                return;
            }
        };
        // Handed to the window procedure, freed when the window is destroyed.
        let data = Box::into_raw(Box::new(app.clone()));
        // SAFETY: `hwnd` is a window of this thread; `data` stays valid until `WM_NCDESTROY`.
        let installed =
            unsafe { SetWindowSubclass(hwnd, Some(procedure), SUBCLASS_ID, data as usize) };
        if !installed.as_bool() {
            // SAFETY: `data` came from `Box::into_raw` above and was not handed to anyone.
            drop(unsafe { Box::from_raw(data) });
            tracing::warn!(
                event = "window_error",
                "could not watch for a Windows shutdown"
            );
        }
    }

    fn end_session(app: &AppHandle) {
        tracing::info!(event = "session_end", "Windows is ending the session");
        let app = app.clone();
        let finished = run_bounded(BUDGET, move || {
            if let Some(state) = app.try_state::<AppState>() {
                crate::close_core(&state);
            }
        });
        tracing::info!(
            event = "session_end_done",
            timed_out = !finished,
            "shutdown for the session end finished"
        );
    }

    unsafe extern "system" fn procedure(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        data: usize,
    ) -> LRESULT {
        match message {
            // `wParam` is false when the end of the session was cancelled.
            WM_ENDSESSION if wparam.0 != 0 => {
                // SAFETY: `data` is the `AppHandle` boxed by `install`, alive until the
                // window is destroyed.
                end_session(unsafe { &*(data as *const AppHandle) });
            }
            WM_NCDESTROY => {
                // SAFETY: last message of the window; nothing uses `data` afterwards.
                unsafe {
                    let _ = RemoveWindowSubclass(hwnd, Some(procedure), SUBCLASS_ID);
                    drop(Box::from_raw(data as *mut AppHandle));
                }
            }
            _ => {}
        }
        // SAFETY: passes the message on to the next procedure of the chain, tao's.
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use super::*;

    #[test]
    fn work_that_ends_in_time_is_reported_done() {
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        assert!(run_bounded(Duration::from_secs(5), move || {
            flag.store(true, Ordering::SeqCst)
        }));
        assert!(ran.load(Ordering::SeqCst));
    }

    #[test]
    fn work_that_hangs_is_given_up_on_at_the_budget() {
        let started = Instant::now();
        let finished = run_bounded(Duration::from_millis(100), || {
            thread::sleep(Duration::from_secs(3))
        });
        assert!(!finished);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "does not wait for the work"
        );
    }

    #[test]
    fn work_that_panics_does_not_hold_the_shutdown() {
        let started = Instant::now();
        let finished = run_bounded(Duration::from_secs(5), || panic!("closing failed"));
        assert!(finished, "it ended");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
