//! What the tray says about the work in progress: the state of the current batch and its
//! overall percentage, as pure functions of the queue's items and of the progress events, plus
//! the rule that keeps the tray from being rewritten more than once a second.
//!
//! The batch is the items still to do plus those that finished since the queue was last idle,
//! so the percentage never goes back when a file is done. It is weighted by the size of the
//! sources. Each item counts for half while it is compressed and half while it is sent; an
//! item that skips the compression jumps to the second half. It stays below 100 as long as an
//! item is left.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::queue::State;
use crate::queue::events::Phase;

/// The tray changes at most this often; a change that has to wait is made when it is over.
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    Idle,
    Compressing,
    Sending,
    /// The queue is paused with items left.
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub line: Line,
    /// Items still to do.
    pub count: usize,
    /// 0 to 99 while an item is left, 0 when idle.
    pub percent: u8,
}

/// An item of the batch; `state` is `None` once it is finished.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
    pub state: Option<State>,
    /// Size of the source in bytes.
    pub weight: u64,
    /// 0 to 1.
    pub fraction: f64,
}

pub fn summarize(entries: &[Entry], running: bool) -> Status {
    let active = entries.iter().filter(|entry| entry.state.is_some()).count();
    if active == 0 {
        return Status {
            line: Line::Idle,
            count: 0,
            percent: 0,
        };
    }
    let (done, total) = entries.iter().fold((0.0, 0.0), |(done, total), entry| {
        let weight = entry.weight.max(1) as f64;
        (
            done + weight * entry.fraction.clamp(0.0, 1.0),
            total + weight,
        )
    });
    let compressing = entries.iter().any(|entry| {
        matches!(
            entry.state,
            Some(State::Probing | State::Encoding | State::Remuxing)
        )
    });
    Status {
        line: match (running, compressing) {
            (false, _) => Line::Paused,
            (true, true) => Line::Compressing,
            (true, false) => Line::Sending,
        },
        count: active,
        percent: (done / total * 100.0).floor().min(99.0) as u8,
    }
}

/// How far an item is, from its state and the last progress of its phase (0 to 100).
pub fn item_fraction(state: State, live: Option<(Phase, f64)>) -> f64 {
    let phase_fraction = |phases: &[Phase]| {
        live.filter(|(phase, percent)| phases.contains(phase) && percent.is_finite())
            .map_or(0.0, |(_, percent)| (percent / 100.0).clamp(0.0, 1.0))
    };
    match state {
        State::Encoding | State::Remuxing => {
            0.5 * phase_fraction(&[Phase::Encoding, Phase::Remuxing])
        }
        State::Encoded => 0.5,
        State::Uploading | State::WaitingServer => {
            0.5 + 0.5 * phase_fraction(&[Phase::Uploading, Phase::AwaitingServer])
        }
        _ => 0.0,
    }
}

/// Follows the batch between two looks at the queue.
#[derive(Default)]
pub struct Tracker {
    entries: HashMap<i64, Entry>,
    live: HashMap<i64, (Phase, f64)>,
}

impl Tracker {
    /// A progress event. True when the whole percent of the item moved, so the tray may need
    /// to change.
    pub fn record(&mut self, id: i64, phase: Phase, percent: f64) -> bool {
        let whole = |percent: f64| percent.floor() as i64;
        self.live
            .insert(id, (phase, percent))
            .is_none_or(|(before, old)| before != phase || whole(old) != whole(percent))
    }

    /// `items` are the active ones (`Store::active_items`).
    pub fn status(&mut self, items: &[(i64, State, u64)], running: bool) -> Status {
        for (id, entry) in &mut self.entries {
            if !items.iter().any(|(active, ..)| active == id) {
                entry.state = None;
                entry.fraction = 1.0;
            }
        }
        self.live
            .retain(|id, _| items.iter().any(|(active, ..)| active == id));
        for &(id, state, size) in items {
            let fraction = item_fraction(state, self.live.get(&id).copied());
            let entry = self.entries.entry(id).or_insert(Entry {
                state: None,
                weight: size,
                fraction: 0.0,
            });
            if entry.state.is_none() {
                // New, or retried after it was finished.
                entry.fraction = 0.0;
            }
            entry.state = Some(state);
            entry.weight = size;
            // Never back: a retried upload starts again from the top of its phase.
            entry.fraction = entry.fraction.max(fraction);
        }
        let entries: Vec<Entry> = self.entries.values().copied().collect();
        let status = summarize(&entries, running);
        if status.line == Line::Idle {
            self.entries.clear();
            self.live.clear();
        }
        status
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// What is shown is already this.
    Skip,
    Now,
    /// Changed, but shown too recently: do it after this long.
    After(Duration),
}

/// Whether to write `next` now, given what was written and when.
pub fn due<T: PartialEq>(shown: Option<(&T, Instant)>, next: &T, now: Instant) -> Due {
    let Some((current, at)) = shown else {
        return Due::Now;
    };
    if current == next {
        return Due::Skip;
    }
    let since = now.saturating_duration_since(at);
    if since >= MIN_INTERVAL {
        Due::Now
    } else {
        Due::After(MIN_INTERVAL - since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(state: Option<State>, weight: u64, fraction: f64) -> Entry {
        Entry {
            state,
            weight,
            fraction,
        }
    }

    #[test]
    fn nothing_active_is_idle_whatever_finished() {
        let done = [entry(None, 10, 1.0)];
        assert_eq!(summarize(&[], true).line, Line::Idle);
        assert_eq!(summarize(&done, true).line, Line::Idle);
        assert_eq!(summarize(&done, true).percent, 0);
    }

    #[test]
    fn the_percent_is_weighted_by_size_and_counts_what_is_finished() {
        let items = [
            entry(None, 300, 1.0),
            entry(Some(State::Uploading), 100, 0.5),
            entry(Some(State::Ready), 100, 0.0),
        ];
        // (300 + 50) / 500
        let status = summarize(&items, true);
        assert_eq!(status.percent, 70);
        assert_eq!(status.count, 2);
        assert_eq!(status.line, Line::Sending);
    }

    #[test]
    fn it_stays_below_100_while_an_item_is_left_and_tolerates_odd_input() {
        let items = [
            entry(None, 1_000_000, 1.0),
            entry(Some(State::Uploading), 1, 1.0),
        ];
        assert_eq!(summarize(&items, true).percent, 99);
        let odd = [entry(Some(State::Ready), 0, f64::NAN), entry(None, 0, 7.0)];
        assert!(summarize(&odd, true).percent <= 99);
    }

    #[test]
    fn the_line_is_paused_when_the_queue_is_not_running_and_compressing_while_a_file_is_encoded() {
        let encoding = [entry(Some(State::Encoding), 10, 0.1)];
        assert_eq!(summarize(&encoding, true).line, Line::Compressing);
        assert_eq!(summarize(&encoding, false).line, Line::Paused);
        let mixed = [
            entry(Some(State::Uploading), 10, 0.6),
            entry(Some(State::Probing), 10, 0.0),
        ];
        assert_eq!(summarize(&mixed, true).line, Line::Compressing);
    }

    #[test]
    fn an_item_counts_half_for_the_compression_and_half_for_the_send() {
        let live = |phase, percent| Some((phase, percent));
        assert_eq!(item_fraction(State::Ready, None), 0.0);
        assert_eq!(
            item_fraction(State::Encoding, live(Phase::Encoding, 50.0)),
            0.25
        );
        assert_eq!(item_fraction(State::Encoded, None), 0.5);
        assert_eq!(
            item_fraction(State::Uploading, live(Phase::Uploading, 50.0)),
            0.75
        );
        // The last progress belongs to the phase before: it does not count.
        assert_eq!(
            item_fraction(State::Uploading, live(Phase::Encoding, 100.0)),
            0.5
        );
        assert_eq!(
            item_fraction(State::Uploading, live(Phase::Uploading, f64::NAN)),
            0.5
        );
    }

    #[test]
    fn the_tracker_keeps_finished_items_until_idle_and_never_goes_back() {
        let mut tracker = Tracker::default();
        let both = [(1, State::Uploading, 100), (2, State::Ready, 100)];
        assert!(tracker.record(1, Phase::Uploading, 80.0));
        // (0.9 + 0) / 2
        assert_eq!(tracker.status(&both, true).percent, 45);
        // Item 1 is sent: it stays in the batch at 100 %.
        let left = [(2, State::Probing, 100)];
        let status = tracker.status(&left, true);
        assert_eq!((status.count, status.percent), (1, 50));
        // A retried item is work to do again: it counts from the start, the rest is kept.
        let retry = [(1, State::Encoded, 100), (2, State::Probing, 100)];
        assert_eq!(tracker.status(&retry, true).percent, 25);
        // But an item never goes back while it is active (a retried upload starts again).
        let again = [(1, State::Uploading, 100), (2, State::Probing, 100)];
        assert_eq!(tracker.status(&again, true).percent, 25);
        assert_eq!(tracker.status(&retry, true).percent, 25);
        // Everything done: idle, and the next batch starts from zero.
        assert_eq!(tracker.status(&[], true).line, Line::Idle);
        let next = [(3, State::Ready, 100)];
        assert_eq!(tracker.status(&next, true).percent, 0);
    }

    #[test]
    fn a_progress_event_matters_only_when_the_whole_percent_moves() {
        let mut tracker = Tracker::default();
        assert!(tracker.record(1, Phase::Encoding, 10.2));
        assert!(!tracker.record(1, Phase::Encoding, 10.9));
        assert!(tracker.record(1, Phase::Encoding, 11.0));
        assert!(tracker.record(1, Phase::Uploading, 11.0));
    }

    #[test]
    fn the_tray_is_written_on_change_at_most_once_a_second() {
        let start = Instant::now();
        let later = |ms| start + Duration::from_millis(ms);
        assert_eq!(due(None, &1, start), Due::Now);
        assert_eq!(due(Some((&1, start)), &1, later(5_000)), Due::Skip);
        assert_eq!(
            due(Some((&1, start)), &2, later(300)),
            Due::After(Duration::from_millis(700))
        );
        assert_eq!(due(Some((&1, start)), &2, later(1_000)), Due::Now);
        // A clock that went back is not a reason to write.
        assert_eq!(
            due(Some((&1, later(500))), &2, start),
            Due::After(MIN_INTERVAL)
        );
    }
}
