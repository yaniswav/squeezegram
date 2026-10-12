//! Background mode (paid feature `background`): the app keeps working with its window hidden.
//!
//! - **Close to the notification area.** Setting `background.close_action`: `quit` (default,
//!   and the only behavior without the license) or `tray`. With the license and `tray`,
//!   closing the window hides it; the queue and the watched folders keep going.
//! - **Tray icon** (`tray`): exists only while closing the window hides it. Menu: open,
//!   pause or resume the queue, quit. The tooltip tells what the queue is doing.
//! - **Quit.** From the tray it goes through the normal exit (`RunEvent::ExitRequested`, see
//!   `close_core` in `lib.rs`: watcher, queue, TDLib), after a confirmation in the window when
//!   an item is being worked on.
//!
//! Every decision that does not need the window system lives here as a function of its
//! inputs, so the license and setting combinations are tested without an app.
//!
//! Journal: `tray_created`, `tray_removed`, `tray_error`, `tray_action`, `app_hidden`,
//! `app_shown`, `tray_quit`, `quit_confirmation`.

mod labels;
mod tray;

use std::time::Duration;

pub use tray::Background;

use crate::settings::CloseAction;

/// Event sent to the window when a quit from the tray needs the user's confirmation.
pub const QUIT_REQUESTED: &str = "app://quit-requested";

/// How long a license that waits for its account (reconnection, sign-out) keeps the tray: the
/// license gate closes briefly at each reconnection, and the window must not pop up each time.
pub const PENDING_GRACE: Duration = Duration::from_secs(60);

/// What closing the window does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseBehavior {
    /// The app exits.
    Quit,
    /// The window hides; the app keeps running.
    Hide,
}

/// `Hide` only with the license and the `tray` choice: without the license the setting is kept
/// but ignored.
pub fn close_behavior(licensed: bool, action: CloseAction) -> CloseBehavior {
    match (licensed, action) {
        (true, CloseAction::Tray) => CloseBehavior::Hide,
        _ => CloseBehavior::Quit,
    }
}

/// Whether the tray icon should exist: closing the window hides it, so it needs a way back.
pub fn tray_wanted(licensed: bool, action: CloseAction) -> bool {
    licensed && action == CloseAction::Tray
}

/// Whether the feature counts as licensed right now. The license applies only while its
/// account is signed in, so it is briefly refused at each reconnection or sign-out; a tray
/// that exists is kept for `PENDING_GRACE` in that case.
pub fn licensed_for_tray(
    allows: bool,
    pending: bool,
    tray_exists: bool,
    pending_for: Duration,
) -> bool {
    allows || (pending && tray_exists && pending_for < PENDING_GRACE)
}

/// Tooltip and menu wording of the queue, from the counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueLine {
    Idle,
    Working(usize),
    Paused(usize),
}

/// `total` counts the items being worked on and those that will run (`Activity::total`).
pub fn queue_line(total: usize, running: bool) -> QueueLine {
    match (total, running) {
        (0, _) => QueueLine::Idle,
        (n, true) => QueueLine::Working(n),
        (n, false) => QueueLine::Paused(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIONS: [CloseAction; 2] = [CloseAction::Quit, CloseAction::Tray];

    #[test]
    fn closing_hides_only_with_the_license_and_the_tray_choice() {
        for licensed in [false, true] {
            for action in ACTIONS {
                let expected = if licensed && action == CloseAction::Tray {
                    CloseBehavior::Hide
                } else {
                    CloseBehavior::Quit
                };
                assert_eq!(
                    close_behavior(licensed, action),
                    expected,
                    "licensed {licensed}, {action:?}"
                );
            }
        }
    }

    #[test]
    fn the_tray_exists_only_while_closing_hides_the_window() {
        for action in ACTIONS {
            assert_eq!(tray_wanted(true, action), action == CloseAction::Tray);
            assert!(!tray_wanted(false, action), "no tray without the license");
        }
    }

    #[test]
    fn a_license_waiting_for_its_account_keeps_the_tray_for_a_while_only() {
        let short = Duration::from_secs(5);
        let long = PENDING_GRACE;
        assert!(licensed_for_tray(true, false, false, long));
        assert!(licensed_for_tray(false, true, true, short));
        assert!(!licensed_for_tray(false, true, true, long), "grace is over");
        assert!(
            !licensed_for_tray(false, true, false, short),
            "a tray that does not exist is not created on a license that is not applied"
        );
        assert!(
            !licensed_for_tray(false, false, true, short),
            "no license, removed license or another account: no grace"
        );
    }

    #[test]
    fn the_queue_line_follows_the_counts_and_the_run_state() {
        assert_eq!(queue_line(0, true), QueueLine::Idle);
        assert_eq!(queue_line(0, false), QueueLine::Idle);
        assert_eq!(queue_line(3, true), QueueLine::Working(3));
        assert_eq!(queue_line(3, false), QueueLine::Paused(3));
    }
}
