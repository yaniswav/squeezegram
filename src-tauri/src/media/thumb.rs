//! The preview picture of a video sent in Video mode (F4c), passed to Telegram as
//! `inputThumbnail`. Without it Telegram builds its own from the first frame, which is often
//! black.
//!
//! Limits: JPEG, 320 px on the longest side, under 200 KB. TDLib's `inputThumbnail` only says
//! the size "usually shouldn't exceed 320", and the 200 KB / JPEG rule comes from Telegram's
//! documentation of thumbnails (Bot API `sendVideo`); the client enforces neither, so both are
//! kept as the safe side.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::MediaError;
use super::procs::{self, Args, Ending, Spec};
use crate::sidecars::SidecarService;
use crate::transport::Thumbnail;

/// Longest side of the picture.
pub const MAX_SIDE: u32 = 320;
/// Largest file Telegram takes as a thumbnail.
pub const MAX_BYTES: u64 = 200 * 1024;
/// Where in the video the frame is taken, as a fraction of its duration: past the black frames
/// and fades that open most videos, and still inside a very short one.
const AT_FRACTION: f64 = 0.10;
/// Position used when the duration is not known.
const AT_UNKNOWN_S: f64 = 1.0;
/// Quality of the JPEG (`-q:v`, 2 best to 31 worst): at 320 px it stays far under `MAX_BYTES`.
const QUALITY: &str = "5";
const TIMEOUT: Duration = Duration::from_secs(30);
const FILE_NAME: &str = "thumb.jpg";

/// Makes the thumbnail of a video about to be sent.
#[async_trait]
pub trait Thumbnails: Send + Sync {
    /// A JPEG of `video` written in `out_dir` (created if needed). `duration_s` is the
    /// video's, when known.
    async fn video(
        &self,
        video: &Path,
        duration_s: Option<f64>,
        out_dir: &Path,
        cancel: &CancellationToken,
    ) -> Result<Thumbnail, MediaError>;
}

/// `Thumbnails` with the app's ffmpeg.
pub struct FfmpegThumbnails {
    sidecars: Arc<SidecarService>,
}

impl FfmpegThumbnails {
    pub fn new(sidecars: Arc<SidecarService>) -> Self {
        Self { sidecars }
    }
}

#[async_trait]
impl Thumbnails for FfmpegThumbnails {
    async fn video(
        &self,
        video: &Path,
        duration_s: Option<f64>,
        out_dir: &Path,
        cancel: &CancellationToken,
    ) -> Result<Thumbnail, MediaError> {
        let resolved = self
            .sidecars
            .resolve()
            .map_err(|error| MediaError::failed(error.code(), error.to_string()))?;
        generate(&resolved.ffmpeg, video, duration_s, out_dir, cancel).await
    }
}

/// Second of the video the frame is taken at.
fn frame_at(duration_s: Option<f64>) -> f64 {
    match duration_s {
        Some(duration) if duration.is_finite() && duration > 0.0 => duration * AT_FRACTION,
        _ => AT_UNKNOWN_S,
    }
}

/// Filter chain: the pixel aspect ratio applied (the picture has the shape the player shows),
/// then fitted in a `MAX_SIDE` square with even sizes (a smaller video is not enlarged), then
/// full-range YUV, the only kind the JPEG encoder takes: video is limited-range nearly always.
fn filter() -> String {
    format!(
        "scale=trunc(iw*sar):ih,setsar=1,scale='min({MAX_SIDE},iw)':'min({MAX_SIDE},ih)':force_original_aspect_ratio=decrease:force_divisible_by=2,format=yuvj420p"
    )
}

fn thumbnail_args(video: &Path, at: f64, out: &Path) -> Args {
    super::file_input(
        Args::new()
            .all(&[
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-y",
                "-ss",
            ])
            .arg(format!("{at:.3}")),
        video,
    )
    .all(&["-frames:v", "1", "-an", "-sn", "-vf"])
    .arg(filter())
    .all(&["-q:v", QUALITY])
    .arg(out)
}

/// One frame of `video` as a JPEG in `out_dir`. A frame past the end (a duration that
/// overstates the video) makes ffmpeg write nothing: the first frame is taken instead.
pub async fn generate(
    ffmpeg: &Path,
    video: &Path,
    duration_s: Option<f64>,
    out_dir: &Path,
    cancel: &CancellationToken,
) -> Result<Thumbnail, MediaError> {
    std::fs::create_dir_all(out_dir)?;
    let out = out_dir.join(FILE_NAME);
    for at in [frame_at(duration_s), 0.0] {
        // A leftover of an earlier attempt must not pass for this one.
        let _ = std::fs::remove_file(&out);
        let args = thumbnail_args(video, at, &out);
        let spec = Spec::new(ffmpeg, args).timeout(TIMEOUT).cancel(cancel);
        let output = procs::run(spec, |_| {})
            .await
            .map_err(|e| MediaError::failed("FFMPEG_MISSING", e.to_string()))?;
        if output.ending == Ending::Cancelled {
            return Err(MediaError::Cancelled);
        }
        if !output.success() {
            return Err(MediaError::failed(
                "FFMPEG_FAILED",
                format!(
                    "thumbnail: ffmpeg {:?}: {}",
                    output.code,
                    output.stderr_tail.join(" | ")
                ),
            ));
        }
        if std::fs::metadata(&out).is_ok_and(|m| m.len() > 0) {
            return finish(out);
        }
    }
    Err(MediaError::failed("FFMPEG_FAILED", "thumbnail: no frame"))
}

/// Checks the file against Telegram's limits and reads its size.
fn finish(path: PathBuf) -> Result<Thumbnail, MediaError> {
    let bytes = std::fs::read(&path)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(MediaError::failed(
            "FFMPEG_FAILED",
            format!(
                "thumbnail: {} bytes, over the {MAX_BYTES} limit",
                bytes.len()
            ),
        ));
    }
    let (width, height) = jpeg_size(&bytes)
        .filter(|(w, h)| *w > 0 && *h > 0 && (*w).max(*h) <= MAX_SIDE)
        .ok_or_else(|| MediaError::failed("FFMPEG_FAILED", "thumbnail: not a JPEG of 320 px"))?;
    Ok(Thumbnail {
        path,
        width,
        height,
    })
}

/// Width and height from the first start-of-frame marker of a JPEG.
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut at = 2;
    loop {
        if *bytes.get(at)? != 0xFF {
            return None;
        }
        let marker = *bytes.get(at + 1)?;
        match marker {
            // Padding, or markers without a length.
            0xFF => at += 1,
            0x01 | 0xD0..=0xD7 => at += 2,
            // Start of frame (baseline, extended, progressive…), not DHT, JPG or DAC.
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let frame = bytes.get(at + 5..at + 9)?;
                let height = u32::from(u16::from_be_bytes([frame[0], frame[1]]));
                let width = u32::from(u16::from_be_bytes([frame[2], frame[3]]));
                return Some((width, height));
            }
            0xD9 | 0xDA => return None,
            _ => {
                let length = u16::from_be_bytes([*bytes.get(at + 2)?, *bytes.get(at + 3)?]);
                at += 2 + usize::from(length);
            }
        }
    }
}

/// No ffmpeg: every video goes without a thumbnail (tests that do not look at them).
#[cfg(test)]
pub struct Unavailable;

#[cfg(test)]
#[async_trait]
impl Thumbnails for Unavailable {
    async fn video(
        &self,
        _: &Path,
        _: Option<f64>,
        _: &Path,
        _: &CancellationToken,
    ) -> Result<Thumbnail, MediaError> {
        Err(MediaError::failed(
            "FFMPEG_MISSING",
            "no ffmpeg in this test",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_input_is_restricted_after_the_seek() {
        let input = Path::new("clip.mp4");
        let args = thumbnail_args(input, 1.5, Path::new("thumb.jpg"));
        super::super::assert_file_input(args.clone(), input);
        let args = args.into_vec();
        let seek = args.iter().position(|arg| arg == "-ss").unwrap();
        assert_eq!(args[seek + 1], "1.500");
        assert_eq!(args[seek + 2], "-format_whitelist");
    }

    #[test]
    fn the_frame_is_a_tenth_into_the_video() {
        assert_eq!(frame_at(Some(60.0)), 6.0);
        assert_eq!(frame_at(Some(0.5)), 0.05);
        assert_eq!(frame_at(None), AT_UNKNOWN_S);
        assert_eq!(frame_at(Some(0.0)), AT_UNKNOWN_S);
        assert_eq!(frame_at(Some(f64::NAN)), AT_UNKNOWN_S);
    }

    /// SOI, an APP0 segment, then SOF0 of 200 x 320.
    fn sample() -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x06, 1, 2, 3, 4];
        bytes.extend([0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x01, 0x40, 0x00, 0xC8, 0x01]);
        bytes
    }

    #[test]
    fn the_size_is_read_from_the_start_of_frame() {
        assert_eq!(jpeg_size(&sample()), Some((200, 320)));
        assert_eq!(jpeg_size(&[0x89, b'P', b'N', b'G']), None);
        assert_eq!(jpeg_size(&sample()[..12]), None, "cut before the frame");
        assert_eq!(jpeg_size(&[]), None);
    }
}
