//! Media pipeline: ffprobe, plan, encoder detection, encode/remux/photo, metadata removal of
//! files sent as they are, BLAKE3 identity
//! `stage` plugs it into the orchestrator.

pub mod detect;
pub mod encode;
pub mod hash;
pub mod metadata;
pub mod plan;
pub mod probe;
pub mod procs;
pub mod stage;
pub mod thumb;

#[cfg(test)]
mod integration_tests;

// From intake::is_media and plan::is_image, plus the raw video demuxers already supported.
// image2 is deliberately excluded: it can open neighboring files even under a video extension.
pub(super) const INPUT_FORMAT_WHITELIST: &str = "mov,matroska,webm,avi,asf,flv,mpeg,mpegts,ogg,h264,hevc,m4v,jpeg_pipe,png_pipe,apng,webp_pipe,bmp_pipe,tiff_pipe";

fn check_input_extension(input: &std::path::Path) -> Result<()> {
    if crate::queue::intake::is_media(input) {
        Ok(())
    } else {
        Err(MediaError::failed(
            "INPUT_FORMAT_UNSUPPORTED",
            "input extension is outside the supported video/photo formats",
        ))
    }
}

// Only ffmpeg's whitelist refusal: a damaged file of an allowed format stays FILE_UNREADABLE.
fn unsupported_format(stderr: &str) -> bool {
    stderr.contains("not on whitelist")
}

fn file_input(mut args: procs::Args, input: &std::path::Path) -> procs::Args {
    // Force single-file image readers: filename patterns otherwise select image2 before probing.
    let image_format = input.extension().and_then(|e| e.to_str()).and_then(|e| {
        match e.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some("jpeg_pipe"),
            "png" => Some("png_pipe"),
            "webp" => Some("webp_pipe"),
            "bmp" => Some("bmp_pipe"),
            "tif" | "tiff" => Some("tiff_pipe"),
            _ => None,
        }
    });
    if let Some(format) = image_format {
        args = args.all(&["-f", format]);
    }
    // Both are input AVFormat options. A protocol restriction alone still permits UNC via file.
    args.all(&[
        "-protocol_whitelist",
        "file",
        "-format_whitelist",
        INPUT_FORMAT_WHITELIST,
        "-i",
    ])
    .arg(input)
}

#[cfg(test)]
fn assert_file_input(args: procs::Args, input: &std::path::Path) {
    let args = args.into_vec();
    let at = args.iter().position(|arg| arg == "-i").unwrap();
    assert_eq!(args[at - 4], "-protocol_whitelist");
    assert_eq!(args[at - 3], "file");
    assert_eq!(args[at - 2], "-format_whitelist");
    assert_eq!(args[at - 1], INPUT_FORMAT_WHITELIST);
    assert_eq!(args[at + 1], input.as_os_str());
    for option in ["-protocol_whitelist", "-format_whitelist"] {
        assert_eq!(args.iter().filter(|arg| *arg == option).count(), 1);
    }
    assert!(!args.iter().any(|arg| arg == "-pattern_type"));
    assert!(
        !INPUT_FORMAT_WHITELIST
            .split(',')
            .any(|f| matches!(f, "image2" | "concat" | "hls" | "dash" | "sdp"))
    );
}

/// Why a media step stopped. `Failed::code` is a stable error code; `detail` is for the log
/// (it may hold paths and ffmpeg's last lines, so it never reaches the front).
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("cancelled")]
    Cancelled,
    #[error("{code}: {detail}")]
    Failed { code: &'static str, detail: String },
    /// A GPU encoder stopped before the end of an encode (`encode::EncoderStop`): the stage
    /// retries once on the CPU.
    #[error("{0}")]
    EncoderStopped(Box<encode::EncoderStop>),
    /// A failure whose `params` go to the front with the code (`FFMPEG_FAILED` or
    /// `FFMPEG_KILLED`, plus `{encoder, exitCode, atSeconds, cause}`); `detail` stays in the
    /// journal.
    #[error("{code}: {detail}")]
    Reported {
        code: &'static str,
        params: serde_json::Value,
        detail: String,
    },
}

impl MediaError {
    pub fn failed(code: &'static str, detail: impl Into<String>) -> Self {
        MediaError::Failed {
            code,
            detail: detail.into(),
        }
    }

    pub fn code(&self) -> Option<&'static str> {
        match self {
            MediaError::Cancelled => None,
            MediaError::Failed { code, .. } | MediaError::Reported { code, .. } => Some(code),
            MediaError::EncoderStopped(stop) => Some(stop.code()),
        }
    }
}

impl From<std::io::Error> for MediaError {
    fn from(error: std::io::Error) -> Self {
        MediaError::failed("IO_ERROR", error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, MediaError>;

#[cfg(test)]
mod input_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn extension_policy_matches_existing_intake_formats() {
        for ext in [
            "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "ts", "mts",
            "m2ts", "3gp", "ogv", "jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff",
        ] {
            check_input_extension(Path::new(&format!("generated.{}", ext.to_uppercase()))).unwrap();
        }
        for name in [
            "playlist.m3u8",
            "list.ffconcat",
            "manifest.mpd",
            "image.svg",
            "script.vpy",
            "animation.gif",
            "file.txt",
            "noext",
            "clip.mp4.part",
        ] {
            assert_eq!(
                check_input_extension(Path::new(name)).unwrap_err().code(),
                Some("INPUT_FORMAT_UNSUPPORTED")
            );
        }
    }

    #[tokio::test]
    async fn unsupported_extensions_are_rejected_before_starting_a_probe() {
        let missing_exe = Path::new("no-such-ffprobe.exe");
        let cancel = tokio_util::sync::CancellationToken::new();
        for background in [false, true] {
            let result = if background {
                probe::probe_in_background(missing_exe, Path::new("playlist.m3u8"), true, &cancel)
                    .await
            } else {
                probe::probe(missing_exe, Path::new("playlist.m3u8"), true, &cancel).await
            };
            assert_eq!(result.unwrap_err().code(), Some("INPUT_FORMAT_UNSUPPORTED"));
        }
    }
}
