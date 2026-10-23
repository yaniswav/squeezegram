//! Background mode (paid feature `background`): the app keeps working with its window hidden.
//!
//! - **Close to the notification area.** Setting `background.close_action`: `quit` (default,
//!   and the only behavior without the license) or `tray`. With the license and `tray`,
//!   closing the window hides it; the queue and the watched folders keep going.
//! - **Tray icon** (`tray`): exists only while the window can be hidden, that is when closing
//!   hides it or when the app was started hidden (`--background`). Menu: open,
//!   pause or resume the queue, quit. While work is pending a disabled line above them tells
//!   what it is, how many items are left and the overall percentage (`progress`); the tooltip
//!   tells what the queue is doing, with the same percentage.
//! - **Quit.** From the tray it goes through the normal exit (`RunEvent::ExitRequested`, see
//!   `close_core` in `lib.rs`: watcher, queue, TDLib), after a confirmation in the window when
//!   an item is being worked on.
//! - **Start with Windows** (`autostart`): an `HKCU\...\Run` entry that starts the app with
//!   `--background`. The entry is the only record of the choice. It is created only with the
//!   license; removing it is always allowed. Without the license the entry is left as it is,
//!   and an app started by it shows its window (no tray without the feature). At a licensed
//!   start an entry that runs another path is pointed at this executable. The installer's
//!   uninstaller removes the entry (see `windows/hooks.nsh`).
//!
//! Every decision that does not need the window system lives here as a function of its
//! inputs, so the license and setting combinations are tested without an app.
//!
//! Journal: `tray_created`, `tray_removed`, `tray_error`, `tray_action`, `app_hidden`,
//! `app_shown`, `tray_quit`, `quit_confirmation`, `start_visible`, `autostart_set`,
//! `autostart_repaired`, `autostart_error`.

pub mod autostart;
mod labels;
mod progress;
mod tray;

use std::time::Duration;

pub use tray::Background;

use crate::settings::CloseAction;

/// Argument of the entry that starts the app at login: stay hidden in the tray.
pub const BACKGROUND_ARG: &str = "--background";

/// Event sent to the window when a quit from the tray needs the user's confirmation.
pub const QUIT_REQUESTED: &str = "app://quit-requested";

/// How long a license that waits for its account (reconnection, sign-out) keeps the tray: the
/// license gate closes briefly at each reconnection, and the window must not pop up each time.
pub const PENDING_GRACE: Duration = Duration::from_secs(60);

/// How long an app started hidden waits for the license verdict, which needs the signed-in
/// account (TDLib opens its database and asks for `getMe`), before it shows its window rather
/// than stay invisible. Generous because a login is when the disk and the network are
/// busiest: giving up early would show the window of a licensed user at every boot.
pub const START_VERDICT_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether the arguments of this launch ask for a hidden start.
pub fn started_in_background<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|arg| arg.as_ref() == BACKGROUND_ARG)
}

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

/// Whether the tray icon should exist: the feature is licensed and the window can be hidden,
/// because closing hides it or because the app started hidden (a hidden window needs a way
/// back). Having the startup entry is not a reason: an app started by hand with the window
/// showing and `quit` chosen has nothing to hide.
pub fn tray_wanted(licensed: bool, action: CloseAction, started_hidden: bool) -> bool {
    licensed && (action == CloseAction::Tray || started_hidden)
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

/// What an app started hidden does with its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartVisibility {
    /// The tray is there: the window stays hidden.
    Hide,
    /// The license cannot be judged before an account signs in: wait.
    Wait,
    /// No tray (no license, or nothing asks for one): show the window as a normal start.
    Show,
}

pub fn start_visibility(tray_exists: bool, pending: bool, expired: bool) -> StartVisibility {
    if tray_exists {
        StartVisibility::Hide
    } else if pending && !expired {
        StartVisibility::Wait
    } else {
        StartVisibility::Show
    }
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
    fn the_tray_exists_for_a_reason_to_hide_and_never_without_the_license() {
        for action in ACTIONS {
            for started_hidden in [false, true] {
                assert!(
                    !tray_wanted(false, action, started_hidden),
                    "no tray without the license: {action:?}, started hidden {started_hidden}"
                );
            }
        }
        assert!(tray_wanted(true, CloseAction::Tray, false));
        assert!(tray_wanted(true, CloseAction::Tray, true));
        assert!(
            tray_wanted(true, CloseAction::Quit, true),
            "started hidden: the window needs a way back"
        );
        assert!(
            !tray_wanted(true, CloseAction::Quit, false),
            "window showing and closing quits: nothing to hide"
        );
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
    fn a_hidden_start_stays_hidden_only_with_a_tray() {
        assert_eq!(start_visibility(true, false, false), StartVisibility::Hide);
        assert_eq!(start_visibility(true, true, true), StartVisibility::Hide);
        assert_eq!(
            start_visibility(false, true, false),
            StartVisibility::Wait,
            "license waits for the account"
        );
        assert_eq!(
            start_visibility(false, true, true),
            StartVisibility::Show,
            "gave up waiting"
        );
        assert_eq!(
            start_visibility(false, false, false),
            StartVisibility::Show,
            "no license, or no reason for a tray"
        );
    }

    #[test]
    fn the_hidden_start_argument_is_recognised_exactly() {
        assert!(started_in_background(["squeezegram.exe", "--background"]));
        assert!(!started_in_background(["squeezegram.exe"]));
        assert!(!started_in_background([
            "squeezegram.exe",
            "--background=1"
        ]));
        assert!(!started_in_background(["--Background"]));
        assert!(!started_in_background(Vec::<String>::new()));
    }

    #[test]
    fn the_queue_line_follows_the_counts_and_the_run_state() {
        assert_eq!(queue_line(0, true), QueueLine::Idle);
        assert_eq!(queue_line(0, false), QueueLine::Idle);
        assert_eq!(queue_line(3, true), QueueLine::Working(3));
        assert_eq!(queue_line(3, false), QueueLine::Paused(3));
    }
}
