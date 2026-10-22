//! Safety caps on what one file may ask of the machine (SG-SEC-016 and 018): pixels, memory,
//! duration, folder depth, files per add. Everyone gets `CapSettings::DEFAULT`; with the
//! `advanced_encoding` feature the saved values apply, each within `[MIN, 2 x default]`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

const GIB: u64 = 1 << 30;
/// Lowest value of every cap, in the unit of its field.
const MIN: f64 = 1.0;

/// What `settings.json` keeps (`advanced_encoding.caps`), in the units the page shows. Plain
/// numbers so that any file loads: `resolve` clamps, loading never refuses (a value out of range
/// must not stop the app from starting).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapSettings {
    /// Pixels of a photo, in millions.
    pub photo_mpx: f64,
    /// Pixels of a photo with transparency or more than 8 bits per color (about 18 bytes each).
    pub photo_wide_mpx: f64,
    /// Pixels of a video frame, in millions.
    pub video_mpx: f64,
    /// `-threads` of an encode: memory grows with threads times frame size.
    pub threads: f64,
    /// Memory of one ffmpeg or ffprobe process, in GiB (Job Object limit).
    pub memory_gb: f64,
    /// Longest video, in hours.
    pub duration_h: f64,
    /// An encode that makes no progress for this long, in minutes, is stopped.
    pub stall_min: f64,
    /// Folder levels under the folder added.
    pub folder_depth: f64,
    /// Files in one add.
    pub files_per_add: f64,
    /// Items the queue may hold, finished ones included.
    pub queue_items: f64,
}

impl CapSettings {
    /// Measured in `private/security-review/F016-proposal.md`. 37.75 Mpx holds 8192x4608.
    pub const DEFAULT: Self = Self {
        photo_mpx: 210.0,
        photo_wide_mpx: 64.0,
        video_mpx: 37.75,
        threads: 6.0,
        memory_gb: 4.0,
        duration_h: 12.0,
        stall_min: 10.0,
        folder_depth: 32.0,
        files_per_add: 10_000.0,
        queue_items: 20_000.0,
    };

    /// The values in effect: each clamped to `[MIN, 2 x default]` (a non-number is the default).
    pub fn resolve(&self) -> Caps {
        let d = Self::DEFAULT;
        let real = |value: f64, default: f64| {
            if value.is_finite() {
                value.clamp(MIN, default * 2.0)
            } else {
                default
            }
        };
        let whole = |value: f64, default: f64| real(value, default).round();
        let pixels = |mpx: f64| (mpx * 1e6).round() as u64;
        Caps {
            photo_pixels: pixels(real(self.photo_mpx, d.photo_mpx)),
            photo_wide_pixels: pixels(real(self.photo_wide_mpx, d.photo_wide_mpx)),
            video_pixels: pixels(real(self.video_mpx, d.video_mpx)),
            threads: whole(self.threads, d.threads) as u32,
            memory_bytes: whole(self.memory_gb, d.memory_gb) as u64 * GIB,
            max_duration_s: whole(self.duration_h, d.duration_h) as u64 * 3600,
            stall: Duration::from_secs(whole(self.stall_min, d.stall_min) as u64 * 60),
            folder_depth: whole(self.folder_depth, d.folder_depth) as usize,
            files_per_add: whole(self.files_per_add, d.files_per_add) as usize,
            queue_items: whole(self.queue_items, d.queue_items) as usize,
        }
    }
}

impl Default for CapSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `CapSettings` in the units the code compares with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub photo_pixels: u64,
    pub photo_wide_pixels: u64,
    pub video_pixels: u64,
    pub threads: u32,
    pub memory_bytes: u64,
    pub max_duration_s: u64,
    pub stall: Duration,
    pub folder_depth: usize,
    pub files_per_add: usize,
    pub queue_items: usize,
}

impl Default for Caps {
    fn default() -> Self {
        CapSettings::DEFAULT.resolve()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_measured_values() {
        let caps = Caps::default();
        assert_eq!(caps.photo_pixels, 210_000_000);
        assert_eq!(caps.photo_wide_pixels, 64_000_000);
        // 8K: 8192x4608 fits, one more row does not.
        assert_eq!(caps.video_pixels, 37_750_000);
        assert!(8192 * 4608 <= caps.video_pixels && 8192 * 4609 > caps.video_pixels);
        assert_eq!(caps.threads, 6);
        assert_eq!(caps.memory_bytes, 4 << 30);
        assert_eq!(caps.max_duration_s, 12 * 3600);
        assert_eq!(caps.stall, Duration::from_secs(600));
        assert_eq!(caps.folder_depth, 32);
        assert_eq!(caps.files_per_add, 10_000);
        assert_eq!(caps.queue_items, 20_000);
    }

    #[test]
    fn a_saved_value_is_clamped_between_the_minimum_and_twice_the_default() {
        let high = CapSettings {
            photo_mpx: 1e12,
            photo_wide_mpx: 1e12,
            video_mpx: 1e12,
            threads: 1e12,
            memory_gb: 1e12,
            duration_h: 1e12,
            stall_min: 1e12,
            folder_depth: 1e12,
            files_per_add: 1e12,
            queue_items: 1e12,
        }
        .resolve();
        assert_eq!(high.photo_pixels, 420_000_000);
        assert_eq!(high.photo_wide_pixels, 128_000_000);
        assert_eq!(high.video_pixels, 75_500_000);
        assert_eq!(high.threads, 12);
        assert_eq!(high.memory_bytes, 8 << 30);
        assert_eq!(high.max_duration_s, 24 * 3600);
        assert_eq!(high.stall, Duration::from_secs(1200));
        assert_eq!(high.folder_depth, 64);
        assert_eq!(high.files_per_add, 20_000);
        assert_eq!(high.queue_items, 40_000);

        let low = CapSettings {
            photo_mpx: -5.0,
            photo_wide_mpx: 0.0,
            video_mpx: -0.1,
            threads: 0.0,
            memory_gb: -1.0,
            duration_h: 0.2,
            stall_min: f64::MIN,
            folder_depth: 0.4,
            files_per_add: -1e12,
            queue_items: 0.0,
        }
        .resolve();
        assert_eq!(low.photo_pixels, 1_000_000);
        assert_eq!(low.video_pixels, 1_000_000);
        assert_eq!((low.threads, low.memory_bytes), (1, 1 << 30));
        assert_eq!(
            (low.max_duration_s, low.stall),
            (3600, Duration::from_secs(60))
        );
        assert_eq!(
            (low.folder_depth, low.files_per_add, low.queue_items),
            (1, 1, 1)
        );

        // Inside the range the value is kept, a fraction of a whole cap is rounded.
        let kept = CapSettings {
            threads: 8.4,
            video_mpx: 50.5,
            ..CapSettings::DEFAULT
        }
        .resolve();
        assert_eq!((kept.threads, kept.video_pixels), (8, 50_500_000));
        // Not a number: the default.
        let odd = CapSettings {
            threads: f64::NAN,
            memory_gb: f64::INFINITY,
            ..CapSettings::DEFAULT
        }
        .resolve();
        assert_eq!(odd, Caps::default());
    }
}
