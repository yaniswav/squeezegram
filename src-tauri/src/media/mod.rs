//! Media pipeline: ffprobe, plan, encoder detection, encode/remux/photo, BLAKE3 identity
//! `stage` plugs it into the orchestrator.

pub mod detect;
pub mod encode;
pub mod hash;
pub mod plan;
pub mod probe;
pub mod procs;
pub mod stage;

#[cfg(test)]
mod integration_tests;

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
    /// A failure whose `params` go to the front with the code (`FFMPEG_FAILED{encoder,
    /// exitCode, atSeconds, cause}`); `detail` stays in the journal.
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
            MediaError::EncoderStopped(_) => Some("FFMPEG_FAILED"),
        }
    }
}

impl From<std::io::Error> for MediaError {
    fn from(error: std::io::Error) -> Self {
        MediaError::failed("IO_ERROR", error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, MediaError>;
