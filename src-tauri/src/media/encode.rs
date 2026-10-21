//! ffmpeg runs: target-size encode, remux and photo.
//!
//! Every output is written to `<name>.part` with `-y`, then renamed atomically: without `-y`
//! ffmpeg exits 0 without writing when the file exists, and a killed encode leaves an unreadable
//! file that must never be reused.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::detect::{Encoder, Tools};
use super::metadata;
use super::plan::{self, EncodePreset, Kind, PHOTO_MAX_BYTES, PhotoSize, Plan, VideoCodec};
use super::probe;
use super::procs::{self, Args, Ending, Spec};
use super::{MediaError, Result};
use crate::queue::events::{Phase, Progress, ProgressReporter};

/// An output shorter than its source by more than this is a truncated source.
const DURATION_TOLERANCE_S: f64 = 2.0;
/// Free space required on the temp volume, as a multiple of the limit (assumed, not measured).
const SPACE_FACTOR: f64 = 1.2;
/// Room for the x264 pass log and the MP4 rewrite of `+faststart`.
const PASS_LOG_SLACK: u64 = 64 * plan::MIB;
const PHOTO_QUALITIES: [u32; 4] = [3, 5, 8, 12];
const PHOTO_SHRINKS: [f64; 3] = [1.0, 0.8, 0.64];

pub struct Job<'a> {
    pub tools: &'a Tools<'a>,
    pub input: &'a Path,
    pub plan: &'a Plan,
    /// `<temp>\<item id>`: holds the `.part` and the final file.
    pub out_dir: &'a Path,
    /// The temp volume must keep at least this much free (`temp.min_free_gb`).
    pub min_free_bytes: u64,
    pub cancel: &'a CancellationToken,
    pub progress: &'a ProgressReporter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Produced {
    pub path: PathBuf,
    pub size: u64,
}

impl Job<'_> {
    fn part(&self) -> PathBuf {
        self.out_dir.join(format!("{}.part", self.plan.output_name))
    }

    fn final_path(&self) -> PathBuf {
        self.out_dir.join(&self.plan.output_name)
    }
}

// --- progress ---

/// What one `-progress` block says.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// 0..=100, `None` if the duration is unknown.
    pub percent: Option<f64>,
    pub out_time_s: f64,
    pub speed: Option<f64>,
    pub total_size: Option<u64>,
    pub end: bool,
}

/// Parses `-progress pipe:1 -nostats` lines. `N/A` values keep the previous value; the last
/// block (`progress=end`) is forced to 100 %.
pub struct ProgressParser {
    duration_us: Option<f64>,
    out_time_us: f64,
    speed: Option<f64>,
    total_size: Option<u64>,
}

impl ProgressParser {
    pub fn new(duration_s: Option<f64>) -> Self {
        Self {
            duration_us: duration_s.filter(|d| *d > 0.0).map(|d| d * 1e6),
            out_time_us: 0.0,
            speed: None,
            total_size: None,
        }
    }

    /// Feeds one line; returns an update at the end of each block.
    pub fn feed(&mut self, line: &str) -> Option<Update> {
        let (key, value) = line.split_once('=')?;
        let value = value.trim();
        match key {
            "out_time_us" => {
                if let Ok(us) = value.parse::<f64>() {
                    self.out_time_us = us.max(0.0);
                }
            }
            "total_size" => self.total_size = value.parse().ok().or(self.total_size),
            "speed" => {
                self.speed = value
                    .trim_end_matches('x')
                    .trim()
                    .parse()
                    .ok()
                    .or(self.speed);
            }
            "progress" => {
                let end = value == "end";
                let percent = if end {
                    Some(100.0)
                } else {
                    self.duration_us
                        .map(|d| (self.out_time_us / d * 100.0).min(100.0))
                };
                return Some(Update {
                    percent,
                    out_time_s: self.out_time_us / 1e6,
                    speed: self.speed,
                    total_size: self.total_size,
                    end,
                });
            }
            _ => {}
        }
        None
    }
}

// --- arguments ---

/// Encoder options for a target video bitrate: the encoder of `codec`, its speed/quality
/// `preset`, the rate control, the profile and (HEVC) the MP4 tag.
///
/// NVENC and x264 at the balanced preset are measured. **QSV, AMF, libopenh264 and h264_mf
/// are NOT measured, nor is any HEVC encoder, nor the faster and quality presets**: they use
/// the same prudent CBR-like scheme (`-b:v V -maxrate V -bufsize 2V`); the corrective pass
/// catches a drift, but the size they reach has not been validated.
///
/// Presets per family (`faster` / `balanced` / `quality`): NVENC `p3` / `p5` / `p7`, x264 and
/// QSV `faster` / `medium` / `slower`, AMF `speed` / `balanced` / `quality`. libopenh264 and
/// h264_mf expose no speed knob in ffmpeg: the preset changes nothing there. HEVC exists on
/// the GPU families only (`Encoder::codec_name`; `encode_video` refuses the other pairs).
pub fn video_options(
    encoder: Encoder,
    codec: VideoCodec,
    preset: EncodePreset,
    video_bps: u64,
) -> Vec<String> {
    let v = video_bps.to_string();
    let buffer = (video_bps * 2).to_string();
    let rate = |extra: &[&str]| -> Vec<String> {
        let mut options: Vec<String> = extra.iter().map(|s| (*s).to_owned()).collect();
        options.extend(
            ["-b:v", &v, "-maxrate", &v, "-bufsize", &buffer]
                .iter()
                .map(|s| (*s).to_owned()),
        );
        options
    };
    let pick = |faster: &'static str, balanced: &'static str, quality: &'static str| match preset {
        EncodePreset::Faster => faster,
        EncodePreset::Balanced => balanced,
        EncodePreset::Quality => quality,
    };
    let name = encoder.codec_name(codec).unwrap_or_else(|| encoder.name());
    let mut options = match encoder {
        Encoder::Nvenc => rate(&[
            "-c:v",
            name,
            "-preset",
            pick("p3", "p5", "p7"),
            "-tune",
            "hq",
            "-rc",
            "cbr",
        ]),
        Encoder::X264 => rate(&["-c:v", name, "-preset", pick("faster", "medium", "slower")]),
        // Not measured:
        Encoder::Qsv => rate(&["-c:v", name, "-preset", pick("faster", "medium", "slower")]),
        Encoder::Amf => rate(&[
            "-c:v",
            name,
            "-quality",
            pick("speed", "balanced", "quality"),
            "-rc",
            "cbr",
        ]),
        Encoder::OpenH264 => rate(&["-c:v", name, "-rc_mode", "bitrate"]),
        Encoder::MediaFoundation => rate(&["-c:v", name, "-rate_control", "cbr"]),
    };
    if encoder != Encoder::MediaFoundation {
        let profile = match codec {
            VideoCodec::H264 => "high",
            VideoCodec::Hevc => "main",
        };
        options.extend(["-profile:v".to_owned(), profile.to_owned()]);
    }
    if codec == VideoCodec::Hevc {
        // Without `hvc1` an MP4 carries `hev1`, which Apple players and some Telegram clients
        // refuse.
        options.extend(["-tag:v".to_owned(), "hvc1".to_owned()]);
    }
    options
}

fn common_start(input: &Path) -> Args {
    super::file_input(
        Args::new().all(&[
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-y",
            "-progress",
            "pipe:1",
            "-nostats",
        ]),
        input,
    )
}

fn map_streams(args: Args, plan: &Plan) -> Args {
    let mut args = args;
    if let Some(index) = plan.video_index {
        args = args.arg("-map").arg(format!("0:{index}"));
    }
    if let Some(index) = plan.audio_index {
        args = args.arg("-map").arg(format!("0:{index}"));
    }
    args.all(&["-sn", "-dn", "-map_chapters", "-1", "-map_metadata", "-1"])
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Single,
    /// x264 only: statistics, no output file.
    First,
    Second,
}

fn passlog(job: &Job<'_>) -> PathBuf {
    job.out_dir.join("pass")
}

/// Full command line of an encode.
fn encode_args(job: &Job<'_>, encoder: Encoder, video_bps: u64, pass: Pass, part: &Path) -> Args {
    let plan = job.plan;
    let mut args = common_start(job.input);
    args = map_streams(args, plan);
    if let Some(vf) = &plan.vf {
        args = args.arg("-vf").arg(vf);
    }
    for option in video_options(encoder, plan.video_codec, plan.preset, video_bps) {
        args = args.arg(option);
    }
    match pass {
        Pass::Single => {}
        Pass::First => {
            args = args
                .all(&["-pass", "1"])
                .arg("-passlogfile")
                .arg(passlog(job))
        }
        Pass::Second => {
            args = args
                .all(&["-pass", "2"])
                .arg("-passlogfile")
                .arg(passlog(job))
        }
    }
    args = args.all(&["-pix_fmt", "yuv420p"]);
    if plan.hdr {
        args = args.all(&[
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-colorspace",
            "bt709",
        ]);
    }
    if pass == Pass::First {
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        return args.all(&["-an", "-f", "null"]).arg(null);
    }
    if let (Some(_), Some(kbps)) = (plan.audio_index, plan.audio_kbps) {
        args = args
            .all(&["-c:a", "aac", "-b:a"])
            .arg(format!("{kbps}k"))
            .all(&["-ac", "2"]);
    }
    args.all(&["-movflags", "+faststart", "-f", "mp4"])
        .arg(part)
}

fn remux_args(job: &Job<'_>, part: &Path) -> Args {
    let plan = job.plan;
    let mut args = map_streams(common_start(job.input), plan);
    args = match (plan.kind, plan.audio_kbps) {
        (Kind::RemuxAudio, Some(kbps)) => args
            .all(&["-c:v", "copy", "-c:a", "aac", "-b:a"])
            .arg(format!("{kbps}k"))
            .all(&["-ac", "2"]),
        _ => args.all(&["-c", "copy"]),
    };
    args.all(&["-movflags", "+faststart", "-f", "mp4"])
        .arg(part)
}

fn photo_args(job: &Job<'_>, size: PhotoSize, quality: u32, part: &Path) -> Args {
    let plan = job.plan;
    let scale = format!(
        "scale={}:{}:flags=lanczos,format=yuvj420p",
        size.width, size.height
    );
    let mut args = super::file_input(
        Args::new().all(&["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]),
        job.input,
    );
    match (plan.alpha, plan.source_dims) {
        // Transparent pixels would keep their hidden RGB values: lay the image over white.
        (true, Some((w, h))) => {
            args = args
                .all(&["-f", "lavfi", "-i"])
                .arg(format!("color=c=white:size={w}x{h}"))
                .arg("-filter_complex")
                .arg(format!("[1:v][0:v]overlay=format=auto:shortest=1,{scale}"));
        }
        _ => args = args.arg("-vf").arg(scale),
    }
    args.all(&["-map_metadata", "-1", "-frames:v", "1", "-q:v"])
        .arg(quality.to_string())
        .all(&["-f", "mjpeg"])
        .arg(part)
}

// --- running ---

/// Messages that mean the encoder could not be opened (no GPU, no driver, no session).
fn encoder_cannot_open(stderr: &str, code: Option<i32>, started: bool) -> bool {
    const MARKERS: [&str; 6] = [
        "OpenEncodeSessionEx failed",
        "No capable devices found",
        "Error creating a MFX session",
        "amfrt64.dll failed to open",
        "Cannot load nvcuda.dll",
        "Failed loading nvcuda.dll",
    ];
    MARKERS.iter().any(|m| stderr.contains(m)) || (code == Some(-542_398_533) && !started)
}

/// stderr lines of a hardware encoder that broke during an encode (driver reset, session lost,
/// device removed). Matched case-insensitively, and only for a GPU encoder.
fn hardware_encoder_error(stderr: &str) -> bool {
    const MARKERS: [&str; 10] = [
        "nv_enc_err",
        "encodepicture failed",
        "failed locking bitstream buffer",
        "error submitting video frame to the encoder",
        "error while encoding",
        "cuda_error",
        "mfx_err",
        "error during encoding",
        "amf_",
        "device removed",
    ];
    let lower = stderr.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

/// A GPU encoder run that ends this early, with nothing on stderr, never really started: the
/// session opened ("Nvenc initialized successfully") and ffmpeg then exited 1 before the first
/// frames went out. Measured on an RTX 4080 (two ffmpeg builds) with one 4 GB 4K60 HEVC file
/// whose `moov` is at the end: about 3 runs in 8 end like this, the next identical run
/// succeeds, and decoding alone or libx264 never fail.
///
/// 15 s of output is well past the 2 to 6 s seen on real failures and still costs a few
/// seconds to redo on a GPU; 60 s of wall time bounds what a retry can waste when the encode
/// is slow (a heavy source, a busy card). Beyond either, the run had really started and a
/// failure is a late one: never repeated blindly.
pub const START_FAILURE_MAX_OUT_S: f64 = 15.0;
pub const START_FAILURE_MAX_RUN_MS: u64 = 60_000;

/// Extra tries (after the first) of a GPU encode that failed to start.
pub const GPU_START_RETRIES: u32 = 2;

/// Extra tries after a silent GPU exit with useful progress. The signal can mean that another
/// program stopped ffmpeg, but it is not proof of that cause, so only one identical retry is made.
pub const GPU_SUSPECTED_KILL_RETRIES: u32 = 1;

/// Wait between two tries: lets a busy card or a driver reset settle. Short in tests.
const GPU_RETRY_DELAY: Duration = if cfg!(test) {
    Duration::from_millis(60)
} else {
    Duration::from_secs(2)
};

/// Whether a failed GPU run looks like a transient failure to start (see
/// `START_FAILURE_MAX_OUT_S`): silent, early, with little output. A stderr line is the
/// encoder saying what is wrong, so it is never retried blindly.
pub fn is_start_failure(stderr_empty: bool, out_time_s: f64, elapsed_ms: u64) -> bool {
    stderr_empty && out_time_s < START_FAILURE_MAX_OUT_S && elapsed_ms < START_FAILURE_MAX_RUN_MS
}

/// A silent code-1 exit after useful GPU progress is compatible with an external kill. On the
/// instrumented retry, a complete report ending in `Conversion failed` overrides that heuristic;
/// an incomplete report strengthens it. Neither signal proves which process stopped ffmpeg.
fn is_suspected_kill(
    encoder: Option<Encoder>,
    ending: Ending,
    code: Option<i32>,
    stderr: &str,
    started: bool,
    report_ends_with_conversion_failed: Option<bool>,
) -> bool {
    if !encoder.is_some_and(|encoder| encoder.is_gpu()) || ending != Ending::Exited || !started {
        return false;
    }
    match report_ends_with_conversion_failed {
        Some(ends_cleanly) => !ends_cleanly,
        None => code == Some(1) && stderr.trim().is_empty(),
    }
}

/// A GPU encoder that ended with a code other than 0, not cancelled, with an empty stderr or a
/// hardware encoder error (seen on an RTX 4080: `h264_nvenc` exits 1 after 2 to 6 s, nothing on
/// stderr, while a game and NVIDIA recording run, and at random on one large 4K file; cause not
/// established). One that is a `start_failure` was already tried again `attempts - 1` times.
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderStop {
    pub encoder: Encoder,
    pub codec: VideoCodec,
    pub exit_code: Option<i32>,
    /// Time spent in the failed run (the last one).
    pub elapsed_ms: u64,
    /// Last `out_time` reached, in seconds of output.
    pub out_time_s: f64,
    pub stderr_empty: bool,
    /// Last stderr lines (journal only).
    pub stderr_tail: String,
    /// The last run was inside the short start-failure window used by the session policy.
    pub start_failure: bool,
    /// Matches a silent exit after progress; an external kill is possible, not established.
    pub suspected_kill: bool,
    /// Diagnostic report of the one instrumented retry, when ffmpeg could create it.
    pub report_path: Option<PathBuf>,
    /// Whether the report's last non-empty line contains ffmpeg's normal failure marker.
    pub report_ends_with_conversion_failed: Option<bool>,
    /// Runs made of the same ffmpeg command, this one included.
    pub attempts: u32,
}

impl EncoderStop {
    /// The ffmpeg name of the encoder that stopped (`h264_nvenc`, `hevc_nvenc`…).
    pub fn encoder_name(&self) -> &'static str {
        self.encoder
            .codec_name(self.codec)
            .unwrap_or_else(|| self.encoder.name())
    }

    /// Whole seconds of the failed run, as shown to the user.
    pub fn at_seconds(&self) -> u64 {
        (self.elapsed_ms as f64 / 1000.0).round() as u64
    }

    /// Stable error code for this stop's own diagnostic signature.
    pub fn code(&self) -> &'static str {
        if self.suspected_kill {
            "FFMPEG_KILLED"
        } else {
            "FFMPEG_FAILED"
        }
    }
}

impl std::fmt::Display for EncoderStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} stopped mid-encode ({} attempts): exit {:?} after {} ms at {:.2} s of output, stderr: {}",
            self.encoder_name(),
            self.attempts,
            self.exit_code,
            self.elapsed_ms,
            self.out_time_s,
            if self.stderr_empty {
                "(empty)"
            } else {
                self.stderr_tail.as_str()
            }
        )
    }
}

fn classify_failure(stderr: &str, code: Option<i32>, started: bool) -> &'static str {
    if encoder_cannot_open(stderr, code, started) {
        "FFMPEG_ENCODER_UNAVAILABLE"
    } else if stderr.contains("No space left on device") {
        "DISK_SPACE"
    } else if super::unsupported_format(stderr) {
        "INPUT_FORMAT_UNSUPPORTED"
    } else if stderr.contains("moov atom not found")
        || stderr.contains("Invalid data found when processing input")
        || code == Some(-13)
    {
        "FILE_UNREADABLE"
    } else {
        "FFMPEG_FAILED"
    }
}

/// Lines of ffmpeg's stderr kept for the journal when a run fails.
const STDERR_LINES: usize = 40;

/// Only the end of an FFREPORT is needed to tell a normal ffmpeg failure from an abrupt stop.
const REPORT_TAIL_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone)]
struct DiagnosticReport {
    path: PathBuf,
    ends_with_conversion_failed: Option<bool>,
}

impl DiagnosticReport {
    fn external_termination_suspected(&self) -> bool {
        self.ends_with_conversion_failed == Some(false)
    }
}

/// One report per queue item. Production item folders are numeric; sanitising the fallback keeps
/// a test or future caller from copying an input name into the journal folder.
fn diagnostic_report_path(job: &Job<'_>) -> PathBuf {
    let item = job
        .out_dir
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("item");
    let item: String = item
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    #[cfg(test)]
    let base = job.out_dir;
    #[cfg(not(test))]
    let base = crate::logging::dir().unwrap_or(job.out_dir);
    base.join(format!("ffmpeg-{item}.log"))
}

/// FFREPORT token parsing needs escaped colons and single quotes; forward slashes avoid
/// backslash escaping on Windows. Its filename template then needs doubled literal `%`.
fn ffreport_value(path: &Path) -> OsString {
    let path = path.to_string_lossy().replace('\\', "/");
    let path = path
        .replace('%', "%%")
        .replace(':', "\\:")
        .replace('\'', "\\'");
    OsString::from(format!("file={path}:level=40"))
}

fn prepare_diagnostic_report(path: &Path) -> Option<OsString> {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(
            event = "ffmpeg_report_unavailable",
            report_path = %path.display(),
            error = %error,
            "ffmpeg diagnostic report folder could not be created"
        );
        return None;
    }
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            event = "ffmpeg_report_unavailable",
            report_path = %path.display(),
            error = %error,
            "stale ffmpeg diagnostic report could not be removed"
        );
        return None;
    }
    Some(ffreport_value(path))
}

fn report_ends_with_conversion_failed(path: &Path) -> Option<bool> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(REPORT_TAIL_BYTES);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut tail = Vec::with_capacity((len - from) as usize);
    file.read_to_end(&mut tail).ok()?;
    String::from_utf8_lossy(&tail)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.contains("Conversion failed"))
}

/// The ffmpeg spec of a job, with its `ffmpeg_start` journal line (command line with paths;
/// arguments never hold a caption or any other content).
fn ffmpeg_spec<'a>(job: &'a Job<'_>, args: Args, step: &str, ffreport: Option<&OsStr>) -> Spec<'a> {
    let mut spec = Spec::new(job.tools.ffmpeg, args)
        .cancel(job.cancel)
        .stderr_lines(STDERR_LINES)
        .env_remove("FFREPORT");
    if let Some(value) = ffreport {
        spec = spec.env("FFREPORT", value);
    }
    let command = serde_json::json!({ "command": spec.describe() }).to_string();
    tracing::info!(
        event = "ffmpeg_start",
        step,
        diagnostic_report = ffreport.is_some(),
        json = command.as_str(),
        "ffmpeg started"
    );
    spec
}

/// `ffmpeg_exit` (success) or `ffmpeg_failed` (with the stderr tail) journal line.
fn log_ffmpeg_end(
    output: &procs::Output,
    began: Instant,
    out_time_s: f64,
    code: Option<&str>,
    report: Option<&DiagnosticReport>,
) {
    let dur_ms = crate::logging::ms_since(began);
    let ending = match output.ending {
        Ending::Exited => "exited",
        Ending::TimedOut => "timed_out",
        Ending::Cancelled => "cancelled",
    };
    let report_path = report.map(|report| report.path.to_string_lossy().into_owned());
    let report_ends_with_conversion_failed =
        report.and_then(|report| report.ends_with_conversion_failed);
    let external_termination_suspected =
        report.is_some_and(DiagnosticReport::external_termination_suspected);
    match code {
        None => tracing::info!(
            event = "ffmpeg_exit",
            dur_ms,
            exit_code = output.code,
            ending,
            out_time_s = (out_time_s * 100.0).round() / 100.0,
            report_path = report_path.as_deref(),
            "ffmpeg finished"
        ),
        Some(code) => {
            let tail = serde_json::json!({ "stderr_tail": output.stderr_tail }).to_string();
            tracing::warn!(
                event = "ffmpeg_failed",
                dur_ms,
                exit_code = output.code,
                ending,
                error_code = code,
                out_time_s = (out_time_s * 100.0).round() / 100.0,
                report_path = report_path.as_deref(),
                report_ends_with_conversion_failed,
                external_termination_suspected,
                json = tail.as_str(),
                "ffmpeg failed"
            );
        }
    }
}

/// `run_ffmpeg`, and when a GPU encoder fails to start (`is_start_failure`), the same command
/// again up to `GPU_START_RETRIES` times: the failure is random and the next identical run
/// usually succeeds, whereas falling back to the CPU turns minutes into an hour. Every first
/// silent GPU failure gets one instrumented retry; legacy early start failures can still use their
/// third try when the report ends with ffmpeg's normal failure marker. A failure with an error on
/// stderr is returned at once, as is a cancellation (also during the wait between two tries). A
/// pause never interrupts an encode, so it does not end this wait either: the queue pauses after
/// the item.
async fn run_ffmpeg(
    job: &Job<'_>,
    step: &str,
    args: Args,
    duration_s: Option<f64>,
    span: (f64, f64),
    encoder: Option<Encoder>,
) -> Result<()> {
    let mut attempt = 1;
    let mut max_attempts = GPU_START_RETRIES + 1;
    let mut next_report = None;
    loop {
        let report = next_report.take();
        let result = run_ffmpeg_once(
            job,
            step,
            args.clone(),
            duration_s,
            span,
            encoder,
            report.as_deref(),
        )
        .await;
        let mut stop = match result {
            Err(MediaError::EncoderStopped(stop)) => stop,
            other => {
                if attempt > 1 && other.is_ok() {
                    tracing::info!(
                        event = "gpu_retry_ok",
                        step,
                        attempts = attempt,
                        "GPU encode succeeded on a retry"
                    );
                }
                return other;
            }
        };
        stop.attempts = attempt;
        let diagnostic_retry = attempt == 1 && stop.stderr_empty;
        if stop.suspected_kill {
            max_attempts = max_attempts.min(GPU_SUSPECTED_KILL_RETRIES + 1);
        }
        if diagnostic_retry && !stop.start_failure {
            max_attempts = max_attempts.min(2);
        }
        if (!stop.start_failure && !stop.suspected_kill && !diagnostic_retry)
            || attempt >= max_attempts
        {
            return Err(MediaError::EncoderStopped(stop));
        }
        if diagnostic_retry {
            next_report = Some(diagnostic_report_path(job));
        }
        let report_path = next_report
            .as_deref()
            .map(|path| path.to_string_lossy().into_owned());
        tracing::warn!(
            event = "gpu_retry",
            step,
            error_code = stop.code(),
            encoder = stop.encoder_name(),
            attempt = attempt + 1,
            max_attempts,
            exit_code = stop.exit_code,
            dur_ms = stop.elapsed_ms,
            out_time_s = (stop.out_time_s * 100.0).round() / 100.0,
            suspected_kill = stop.suspected_kill,
            report_path = report_path.as_deref(),
            "GPU encoder stopped without a final result; same command again"
        );
        // The failed run's output must not be seen by the next one (ffmpeg `-y` would overwrite
        // it, but a half-written file is no evidence of anything).
        remove_if_exists(&job.part());
        // The bar goes back to where this run began, as the new run does.
        job.progress.report_now(Progress {
            percent: span.0,
            ..Default::default()
        });
        wait_before_retry(job.cancel, GPU_RETRY_DELAY).await?;
        attempt += 1;
    }
}

/// Sleeps `delay`; `MediaError::Cancelled` at once if `cancel` fires meanwhile.
async fn wait_before_retry(cancel: &CancellationToken, delay: Duration) -> Result<()> {
    tokio::select! {
        () = tokio::time::sleep(delay) => Ok(()),
        () = cancel.cancelled() => Err(MediaError::Cancelled),
    }
}

/// Runs ffmpeg with progress mapped onto `start..start+width` percent. `encoder` is the video
/// encoder of an encode (`None` for a remux): a GPU one that stops mid-encode gives
/// `MediaError::EncoderStopped`.
async fn run_ffmpeg_once(
    job: &Job<'_>,
    step: &str,
    args: Args,
    duration_s: Option<f64>,
    (start, width): (f64, f64),
    encoder: Option<Encoder>,
    diagnostic_report: Option<&Path>,
) -> Result<()> {
    let mut parser = ProgressParser::new(duration_s);
    let began = Instant::now();
    let mut started = false;
    let mut reached_s = 0.0_f64;
    let report = diagnostic_report
        .and_then(|path| prepare_diagnostic_report(path).map(|value| (path.to_path_buf(), value)));
    let output = procs::run(
        ffmpeg_spec(
            job,
            args,
            step,
            report.as_ref().map(|(_, value)| value.as_os_str()),
        ),
        |line| {
            let Some(update) = parser.feed(line) else {
                return;
            };
            started |= update.out_time_s > 0.0;
            reached_s = update.out_time_s;
            let elapsed = began.elapsed().as_secs_f64().max(0.001);
            let percent = update.percent.map(|p| start + width * p / 100.0);
            let remaining = duration_s
                .zip(update.speed.filter(|s| *s > 0.0))
                .map(|(d, speed)| ((d - update.out_time_s) / speed).max(0.0));
            job.progress.report(Progress {
                percent: percent.unwrap_or(0.0),
                bytes_per_sec: update.total_size.map(|b| b as f64 / elapsed),
                eta_sec: remaining,
                phase: None,
                detail: update.speed.map(|s| format!("{s:.1}x")),
            });
        },
    )
    .await
    .map_err(|e| MediaError::failed("FFMPEG_MISSING", e.to_string()))?;
    match output.ending {
        Ending::Cancelled => {
            if let Some((path, _)) = &report {
                remove_if_exists(path);
            }
            log_ffmpeg_end(&output, began, reached_s, None, None);
            return Err(MediaError::Cancelled);
        }
        Ending::TimedOut => {
            log_ffmpeg_end(&output, began, reached_s, Some("FFMPEG_FAILED"), None);
            return Err(MediaError::failed("FFMPEG_FAILED", "timeout"));
        }
        Ending::Exited => {}
    }
    if !output.success() {
        let stderr = output.stderr_tail.join("\n");
        let report = report.map(|(path, _)| DiagnosticReport {
            ends_with_conversion_failed: report_ends_with_conversion_failed(&path),
            path,
        });
        let suspected_kill = is_suspected_kill(
            encoder,
            output.ending,
            output.code,
            &stderr,
            started,
            report
                .as_ref()
                .and_then(|report| report.ends_with_conversion_failed),
        );
        let code = if suspected_kill {
            "FFMPEG_KILLED"
        } else {
            classify_failure(&stderr, output.code, started)
        };
        log_ffmpeg_end(&output, began, reached_s, Some(code), report.as_ref());
        let stderr_empty = stderr.trim().is_empty();
        if let Some(encoder) = encoder.filter(|e| e.is_gpu())
            && (suspected_kill
                || (code == "FFMPEG_FAILED" && (stderr_empty || hardware_encoder_error(&stderr))))
        {
            let elapsed_ms = crate::logging::ms_since(began);
            return Err(MediaError::EncoderStopped(Box::new(EncoderStop {
                encoder,
                codec: job.plan.video_codec,
                exit_code: output.code,
                elapsed_ms,
                out_time_s: reached_s,
                stderr_empty,
                stderr_tail: stderr,
                start_failure: is_start_failure(stderr_empty, reached_s, elapsed_ms),
                suspected_kill,
                report_path: report.as_ref().map(|report| report.path.clone()),
                report_ends_with_conversion_failed: report
                    .as_ref()
                    .and_then(|report| report.ends_with_conversion_failed),
                attempts: 1,
            })));
        }
        return Err(MediaError::failed(
            code,
            format!("ffmpeg exit {:?}: {stderr}", output.code),
        ));
    }
    if let Some((path, _)) = report {
        remove_if_exists(&path);
    }
    log_ffmpeg_end(&output, began, reached_s, None, None);
    job.progress.report(Progress {
        percent: start + width,
        ..Default::default()
    });
    Ok(())
}

fn file_size(path: &Path) -> Result<u64> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > 0 => Ok(m.len()),
        Ok(_) => Err(MediaError::failed("FFMPEG_FAILED", "empty output")),
        Err(e) => Err(MediaError::failed(
            "FFMPEG_FAILED",
            format!("no output: {e}"),
        )),
    }
}

fn remove_if_exists(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            event = "file_delete_failed",
            path = %path.display(),
            error = %e,
            "could not delete a temporary file"
        );
    }
}

fn cleanup(job: &Job<'_>) {
    remove_if_exists(&job.part());
    cleanup_pass_logs(job);
}

/// Free bytes for the caller on the volume of `dir`; `None` if it cannot be read.
pub fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        use windows::core::PCWSTR;

        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut free = 0u64;
        // SAFETY: `wide` is NUL-terminated and outlives the call; `free` is a valid out pointer.
        unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None) }
            .ok()
            .map(|()| free)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        None
    }
}

/// `free` must cover `needed` and leave `min_free` on the volume.
pub fn enough_space(free: u64, needed: u64, min_free: u64) -> bool {
    free >= needed.max(min_free)
}

fn check_space(job: &Job<'_>, needed: u64) -> Result<()> {
    match free_space(job.out_dir) {
        Some(free) if !enough_space(free, needed, job.min_free_bytes) => Err(MediaError::failed(
            "DISK_SPACE",
            format!("{free} bytes free, {needed} needed"),
        )),
        _ => Ok(()),
    }
}

fn prepare(job: &Job<'_>, needed: u64) -> Result<()> {
    std::fs::create_dir_all(job.out_dir)?;
    check_space(job, needed)?;
    remove_if_exists(&job.part());
    Ok(())
}

/// The output must last about as long as the source: a truncated source makes ffmpeg exit 0
/// with a shorter file.
async fn verify_duration(job: &Job<'_>, part: &Path) -> Result<()> {
    let info = probe::probe_output(job.tools.ffprobe, part, job.cancel).await?;
    let produced = info
        .duration_s
        .ok_or_else(|| MediaError::failed("FFMPEG_FAILED", "output has no duration"))?;
    if let Some(source) = job.plan.duration_s
        && produced + DURATION_TOLERANCE_S < source
    {
        return Err(MediaError::failed(
            "FILE_UNREADABLE",
            format!("truncated source: {produced:.2} s produced of {source:.2} s"),
        ));
    }
    Ok(())
}

fn finish(job: &Job<'_>) -> Result<Produced> {
    let (part, target) = (job.part(), job.final_path());
    let size = file_size(&part)?;
    std::fs::rename(&part, &target)?;
    Ok(Produced { path: target, size })
}

/// Target-size encode with `encoder`, one corrective pass at most.
pub async fn encode_video(job: &Job<'_>, encoder: Encoder) -> Result<Produced> {
    super::check_input_extension(job.input)?;
    let result = encode_video_inner(job, encoder).await;
    if result.is_err() {
        cleanup(job);
    }
    result
}

async fn encode_video_inner(job: &Job<'_>, encoder: Encoder) -> Result<Produced> {
    let plan = job.plan;
    if encoder.codec_name(plan.video_codec).is_none() {
        return Err(MediaError::failed(
            "FFMPEG_ENCODER_UNAVAILABLE",
            format!(
                "{} has no {} encoder",
                encoder.name(),
                plan.video_codec.as_str()
            ),
        ));
    }
    // The hard ceiling, and what a custom target aims under (the limit when there is none).
    let (limit, aim) = (plan.limit_bytes, plan.aim_bytes());
    let mut video_bps = plan
        .video_bps
        .ok_or_else(|| MediaError::failed("FFMPEG_FAILED", "plan has no video bitrate"))?;
    let slack = if encoder == Encoder::X264 {
        PASS_LOG_SLACK
    } else {
        0
    };
    prepare(job, (aim as f64 * SPACE_FACTOR) as u64 + slack)?;
    let part = job.part();

    for attempt in 0..2 {
        remove_if_exists(&part);
        if encoder == Encoder::X264 {
            let first = encode_args(job, encoder, video_bps, Pass::First, &part);
            run_ffmpeg(
                job,
                "pass1",
                first,
                plan.duration_s,
                (0.0, 50.0),
                Some(encoder),
            )
            .await?;
            let second = encode_args(job, encoder, video_bps, Pass::Second, &part);
            run_ffmpeg(
                job,
                "pass2",
                second,
                plan.duration_s,
                (50.0, 50.0),
                Some(encoder),
            )
            .await?;
        } else {
            let args = encode_args(job, encoder, video_bps, Pass::Single, &part);
            run_ffmpeg(
                job,
                "encode",
                args,
                plan.duration_s,
                (0.0, 100.0),
                Some(encoder),
            )
            .await?;
        }
        let size = file_size(&part)?;
        // Over the aim: one corrective pass. A file still over a custom target but under the
        // limit (second pass, or a bitrate that cannot go lower) is kept: the target is a wish,
        // the limit is the rule.
        let corrected = if size <= aim || attempt > 0 {
            None
        } else {
            plan::corrective_video_bps(video_bps, aim, size)
        };
        if let Some(corrected) = corrected {
            tracing::info!(
                event = "encode_corrective_pass",
                size,
                limit,
                aim,
                video_bps,
                corrected_bps = corrected,
                codec = plan.video_codec.as_str(),
                "output over the target: one corrective pass"
            );
            video_bps = corrected;
        } else if size <= limit {
            verify_duration(job, &part).await?;
            cleanup_pass_logs(job);
            return finish(job);
        } else {
            return Err(MediaError::failed(
                "OUTPUT_TOO_LARGE",
                format!("{size} bytes against a limit of {limit}"),
            ));
        }
    }
    Err(MediaError::failed("OUTPUT_TOO_LARGE", "unreachable"))
}

fn cleanup_pass_logs(job: &Job<'_>) {
    for suffix in ["-0.log", "-0.log.mbtree"] {
        let mut name = passlog(job).into_os_string();
        name.push(suffix);
        remove_if_exists(Path::new(&name));
    }
}

/// `-c copy` into MP4 (or video copy with AAC audio).
pub async fn remux(job: &Job<'_>) -> Result<Produced> {
    super::check_input_extension(job.input)?;
    let result = remux_inner(job).await;
    if result.is_err() {
        cleanup(job);
    }
    result
}

async fn remux_inner(job: &Job<'_>) -> Result<Produced> {
    let source_size = std::fs::metadata(job.input)?.len();
    prepare(job, source_size + source_size / 10)?;
    let part = job.part();
    run_ffmpeg(
        job,
        "remux",
        remux_args(job, &part),
        job.plan.duration_s,
        (0.0, 100.0),
        None,
    )
    .await?;
    let size = file_size(&part)?;
    if size > job.plan.limit_bytes {
        return Err(MediaError::failed(
            "OUTPUT_TOO_LARGE",
            format!("{size} bytes against a limit of {}", job.plan.limit_bytes),
        ));
    }
    verify_duration(job, &part).await?;
    finish(job)
}

/// A copy of the image without its location and camera data (`metadata`), pixels untouched,
/// under the source's name in the item's folder. The source is only read.
pub async fn strip(job: &Job<'_>) -> Result<Produced> {
    super::check_input_extension(job.input)?;
    let result = strip_inner(job).await;
    if result.is_err() {
        cleanup(job);
    }
    result
}

/// A video sent as it is but for its metadata, unlike a remux for Telegram: every stream is
/// kept (all audio tracks, subtitles) and the chapters too (titles the author wrote, not
/// where or with what the video was shot). Gone: the file's metadata (location, make, model),
/// each stream's (creation time, a handler name a camera may fill with its model) and the
/// data tracks, where action cameras record their GPS telemetry. Each track's language is not
/// private and is written back from the probe (`Plan::stream_languages`, codes checked there);
/// titles are not, being free text.
fn strip_video_args(job: &Job<'_>, part: &Path) -> Args {
    let mut args = common_start(job.input)
        .all(&["-map", "0", "-map", "-0:d", "-ignore_unknown", "-c", "copy"])
        .all(&["-map_metadata", "-1", "-map_metadata:s", "-1"]);
    for (index, language) in job.plan.stream_languages.iter().enumerate() {
        if let Some(language) = language {
            args = args
                .arg(format!("-metadata:s:{index}"))
                .arg(format!("language={language}"));
        }
    }
    args.all(&["-movflags", "+faststart", "-f", "mp4"])
        .arg(part)
}

async fn strip_inner(job: &Job<'_>) -> Result<Produced> {
    let source_size = std::fs::metadata(job.input)?.len();
    prepare(job, source_size)?;
    if job.plan.note.as_deref() == Some(plan::PRIVATE_TAGS) {
        let part = job.part();
        run_ffmpeg(
            job,
            "strip",
            strip_video_args(job, &part),
            job.plan.duration_s,
            (0.0, 100.0),
            None,
        )
        .await?;
        let size = file_size(&part)?;
        if size > job.plan.limit_bytes {
            return Err(MediaError::failed(
                "OUTPUT_TOO_LARGE",
                format!("{size} bytes against a limit of {}", job.plan.limit_bytes),
            ));
        }
        verify_duration(job, &part).await?;
        return finish(job);
    }
    let (input, part) = (job.input.to_path_buf(), job.part());
    let failed = |detail: String| MediaError::failed("METADATA_STRIP_FAILED", detail);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let data = std::fs::read(&input)?;
        // A file that does not parse is refused, never sent with what it holds.
        let stripped = metadata::strip(&data)
            .map_err(|error| MediaError::failed("METADATA_STRIP_FAILED", error.to_string()))?
            .ok_or_else(|| {
                MediaError::failed("METADATA_STRIP_FAILED", "not a JPEG, PNG or WebP file")
            })?;
        std::fs::write(&part, stripped)?;
        Ok(())
    })
    .await
    .map_err(|error| failed(error.to_string()))??;
    if job.cancel.is_cancelled() {
        return Err(MediaError::Cancelled);
    }
    job.progress.report(Progress {
        percent: 100.0,
        ..Default::default()
    });
    finish(job)
}

/// Photo to JPEG: quality loop, then smaller dimensions, until under 9.5 MiB.
pub async fn photo(job: &Job<'_>) -> Result<Produced> {
    super::check_input_extension(job.input)?;
    let result = photo_inner(job).await;
    if result.is_err() {
        cleanup(job);
    }
    result
}

async fn photo_inner(job: &Job<'_>) -> Result<Produced> {
    let base = job
        .plan
        .photo
        .ok_or_else(|| MediaError::failed("FFMPEG_FAILED", "plan has no photo size"))?;
    let source_size = std::fs::metadata(job.input)?.len();
    prepare(job, source_size.max(PHOTO_MAX_BYTES))?;
    let part = job.part();
    job.progress.report(Progress {
        percent: 0.0,
        phase: Some(Phase::Encoding),
        ..Default::default()
    });
    for shrink in PHOTO_SHRINKS {
        let size = plan::shrink(base, shrink);
        for quality in PHOTO_QUALITIES {
            remove_if_exists(&part);
            run_photo(job, photo_args(job, size, quality, &part)).await?;
            if file_size(&part)? <= PHOTO_MAX_BYTES {
                job.progress.report(Progress {
                    percent: 100.0,
                    ..Default::default()
                });
                return finish(job);
            }
        }
    }
    Err(MediaError::failed(
        "OUTPUT_TOO_LARGE",
        "photo above 9.5 MiB at the lowest quality and size",
    ))
}

/// A photo run has no `-progress` output: only the exit status matters.
async fn run_photo(job: &Job<'_>, args: Args) -> Result<()> {
    let began = Instant::now();
    let output = procs::run(ffmpeg_spec(job, args, "photo", None), |_| {})
        .await
        .map_err(|e| MediaError::failed("FFMPEG_MISSING", e.to_string()))?;
    match output.ending {
        Ending::Cancelled => {
            log_ffmpeg_end(&output, began, 0.0, None, None);
            Err(MediaError::Cancelled)
        }
        Ending::TimedOut => {
            log_ffmpeg_end(&output, began, 0.0, Some("FFMPEG_FAILED"), None);
            Err(MediaError::failed("FFMPEG_FAILED", "timeout"))
        }
        Ending::Exited if output.success() => {
            log_ffmpeg_end(&output, began, 0.0, None, None);
            Ok(())
        }
        Ending::Exited => {
            let stderr = output.stderr_tail.join("\n");
            let code = classify_failure(&stderr, output.code, true);
            log_ffmpeg_end(&output, began, 0.0, Some(code), None);
            Err(MediaError::failed(
                code,
                format!("ffmpeg exit {:?}: {stderr}", output.code),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::queue::events::recorder::Recorder;

    #[test]
    fn progress_blocks_are_parsed_with_na_tolerated_and_100_forced_at_the_end() {
        let mut parser = ProgressParser::new(Some(20.0));
        let block = |time: &str, speed: &str, state: &str| {
            [
                "frame=436".to_owned(),
                "bitrate=3044.3kbits/s".to_owned(),
                "total_size=5505072".to_owned(),
                format!("out_time_us={time}"),
                format!("out_time_ms={time}"),
                format!("speed={speed}"),
                format!("progress={state}"),
            ]
        };
        let mut last = None;
        for line in block("N/A", "N/A", "continue") {
            last = parser.feed(&line).or(last);
        }
        let first = last.take().unwrap();
        assert_eq!(first.percent, Some(0.0), "N/A before the first frame");
        assert_eq!(first.speed, None);

        for line in block("10000000", " 6.5x", "continue") {
            last = parser.feed(&line).or(last);
        }
        let mid = last.take().unwrap();
        assert_eq!(mid.percent, Some(50.0));
        assert_eq!(mid.speed, Some(6.5));
        assert_eq!(mid.total_size, Some(5_505_072));

        // N/A in the middle of a run keeps the last known time.
        for line in block("N/A", "N/A", "continue") {
            last = parser.feed(&line).or(last);
        }
        assert_eq!(last.take().unwrap().percent, Some(50.0));

        // The real last block sits at 99.667 %: forced to 100.
        for line in block("19933333", "10.7x", "end") {
            last = parser.feed(&line).or(last);
        }
        let end = last.unwrap();
        assert!(end.end);
        assert_eq!(end.percent, Some(100.0));
    }

    #[test]
    fn progress_without_a_duration_has_no_percent_and_never_exceeds_100() {
        let mut parser = ProgressParser::new(None);
        parser.feed("out_time_us=5000000");
        assert_eq!(parser.feed("progress=continue").unwrap().percent, None);
        let mut parser = ProgressParser::new(Some(1.0));
        parser.feed("out_time_us=9000000");
        assert_eq!(
            parser.feed("progress=continue").unwrap().percent,
            Some(100.0)
        );
        assert_eq!(parser.feed("garbage line"), None);
    }

    #[test]
    fn every_encoder_gets_a_cbr_like_rate_control_with_a_double_buffer() {
        for encoder in [
            Encoder::Nvenc,
            Encoder::Qsv,
            Encoder::Amf,
            Encoder::OpenH264,
            Encoder::MediaFoundation,
            Encoder::X264,
        ] {
            let options =
                video_options(encoder, VideoCodec::H264, EncodePreset::Balanced, 1_000_000);
            let find = |flag: &str| {
                options
                    .iter()
                    .position(|o| o == flag)
                    .map(|i| options[i + 1].as_str())
            };
            assert_eq!(find("-c:v"), Some(encoder.name()));
            assert_eq!(find("-b:v"), Some("1000000"), "{encoder:?}");
            assert_eq!(find("-maxrate"), Some("1000000"));
            assert_eq!(find("-bufsize"), Some("2000000"));
        }
        let nvenc = video_options(Encoder::Nvenc, VideoCodec::H264, EncodePreset::Balanced, 5);
        assert!(nvenc.windows(2).any(|w| w == ["-rc", "cbr"]));
    }

    fn flag<'a>(options: &'a [String], name: &str) -> Option<&'a str> {
        options
            .iter()
            .position(|o| o == name)
            .map(|i| options[i + 1].as_str())
    }

    #[test]
    fn presets_map_to_each_encoder_family_and_balanced_is_the_old_recipe() {
        use EncodePreset::{Balanced, Faster, Quality};
        let options = |encoder, preset| video_options(encoder, VideoCodec::H264, preset, 1_000);
        for (encoder, name, values) in [
            (Encoder::Nvenc, "-preset", ["p3", "p5", "p7"]),
            (Encoder::X264, "-preset", ["faster", "medium", "slower"]),
            (Encoder::Qsv, "-preset", ["faster", "medium", "slower"]),
            (Encoder::Amf, "-quality", ["speed", "balanced", "quality"]),
        ] {
            for (preset, expected) in [Faster, Balanced, Quality].into_iter().zip(values) {
                let list = options(encoder, preset);
                assert_eq!(flag(&list, name), Some(expected), "{encoder:?} {preset:?}");
            }
        }
        // No speed knob in ffmpeg for these: the preset changes nothing.
        for encoder in [Encoder::OpenH264, Encoder::MediaFoundation] {
            assert_eq!(
                options(encoder, Faster),
                options(encoder, Quality),
                "{encoder:?}"
            );
        }
        // What shipped before the choice existed, byte for byte.
        assert_eq!(
            options(Encoder::Nvenc, Balanced).join(" "),
            "-c:v h264_nvenc -preset p5 -tune hq -rc cbr -b:v 1000 -maxrate 1000 -bufsize 2000 \
             -profile:v high"
        );
        assert_eq!(
            options(Encoder::X264, Balanced).join(" "),
            "-c:v libx264 -preset medium -b:v 1000 -maxrate 1000 -bufsize 2000 -profile:v high"
        );
    }

    #[test]
    fn hevc_uses_the_gpu_encoder_main_profile_and_the_hvc1_tag() {
        for (encoder, name) in [
            (Encoder::Nvenc, "hevc_nvenc"),
            (Encoder::Qsv, "hevc_qsv"),
            (Encoder::Amf, "hevc_amf"),
        ] {
            for preset in [
                EncodePreset::Faster,
                EncodePreset::Balanced,
                EncodePreset::Quality,
            ] {
                let list = video_options(encoder, VideoCodec::Hevc, preset, 2_000_000);
                assert_eq!(flag(&list, "-c:v"), Some(name));
                assert_eq!(flag(&list, "-profile:v"), Some("main"));
                assert_eq!(
                    flag(&list, "-tag:v"),
                    Some("hvc1"),
                    "{name}: MP4 needs hvc1"
                );
                assert_eq!(flag(&list, "-b:v"), Some("2000000"));
                assert_eq!(flag(&list, "-maxrate"), Some("2000000"));
                assert_eq!(flag(&list, "-bufsize"), Some("4000000"));
            }
        }
        // HEVC presets use the same knobs as the H.264 ones of the family.
        let quality = video_options(Encoder::Nvenc, VideoCodec::Hevc, EncodePreset::Quality, 1);
        assert_eq!(flag(&quality, "-preset"), Some("p7"));
        assert!(quality.windows(2).any(|w| w == ["-rc", "cbr"]));
        // H.264 never gets the tag.
        let h264 = video_options(Encoder::Nvenc, VideoCodec::H264, EncodePreset::Balanced, 1);
        assert_eq!(flag(&h264, "-tag:v"), None);
        // The CPU families have no HEVC encoder.
        for encoder in [Encoder::X264, Encoder::OpenH264, Encoder::MediaFoundation] {
            assert_eq!(encoder.codec_name(VideoCodec::Hevc), None, "{encoder:?}");
        }
    }

    #[test]
    fn a_hevc_encode_command_names_the_hevc_encoder_and_tags_the_mp4() {
        let fx = Fixture::new();
        let mut plan = sample_plan();
        plan.video_codec = VideoCodec::Hevc;
        plan.preset = EncodePreset::Quality;
        let (_, text) = fx.args(&plan, |job, part| {
            encode_args(job, Encoder::Nvenc, 900_000, Pass::Single, part)
        });
        assert!(
            text.contains("-c:v hevc_nvenc -preset p7 -tune hq -rc cbr -b:v 900000"),
            "{text}"
        );
        assert!(
            text.contains("-profile:v main -tag:v hvc1 -pix_fmt yuv420p"),
            "{text}"
        );
        assert!(
            !text.contains("h264_nvenc") && !text.contains("-profile:v high"),
            "{text}"
        );
        assert!(text.contains("-ac 2 -movflags +faststart -f mp4"), "{text}");
    }

    /// `.cmd` ffmpeg/ffprobe stand-ins for `encode_video`: every run logs its arguments in
    /// `calls.txt` and copies `first.bin` to the output the first time, `second.bin` after.
    struct Sizes {
        dir: crate::queue::test_util::TestDir,
    }

    impl Sizes {
        fn new(name: &str, first: usize, second: usize) -> Self {
            let dir = crate::queue::test_util::TestDir::new(name);
            std::fs::write(dir.path().join("first.bin"), vec![1u8; first]).unwrap();
            std::fs::write(dir.path().join("second.bin"), vec![2u8; second]).unwrap();
            let lines = [
                "@echo off",
                "setlocal",
                "set \"HERE=%~dp0\"",
                "set \"OUT=\"",
                ":args",
                "if \"%~1\"==\"\" goto run",
                "set \"OUT=%~1\"",
                "shift",
                "goto args",
                ":run",
                "echo %*>>\"%HERE%calls.txt\"",
                "if exist \"%HERE%first.done\" goto second",
                "echo done>\"%HERE%first.done\"",
                "copy /y \"%HERE%first.bin\" \"%OUT%\" >nul",
                "goto end",
                ":second",
                "copy /y \"%HERE%second.bin\" \"%OUT%\" >nul",
                ":end",
                "echo out_time_us=60000000",
                "echo progress=end",
                "exit /b 0",
            ];
            std::fs::write(dir.path().join("ffmpeg.cmd"), lines.join("\r\n") + "\r\n").unwrap();
            std::fs::write(
                dir.path().join("ffprobe.cmd"),
                "@echo off\r\necho {\"streams\":[],\"format\":{\"format_name\":\"mov,mp4\",\"duration\":\"60.000000\"}}\r\n",
            )
            .unwrap();
            Self { dir }
        }

        /// Runs `encode_video` with `plan` on `encoder`; returns the result and the codec
        /// (`-c:v`) and bitrate (`-b:v`) of each ffmpeg run.
        async fn run(
            &self,
            plan: &Plan,
            encoder: Encoder,
        ) -> (Result<Produced>, Vec<(String, String)>) {
            let ffmpeg = self.dir.path().join("ffmpeg.cmd");
            let ffprobe = self.dir.path().join("ffprobe.cmd");
            let tools = Tools {
                ffmpeg: &ffmpeg,
                ffprobe: &ffprobe,
            };
            let input = self.dir.path().join("in.mkv");
            std::fs::write(&input, b"source").unwrap();
            let out_dir = self.dir.path().join("out");
            let fx = Fixture::new();
            let job = Job {
                tools: &tools,
                input: &input,
                plan,
                out_dir: &out_dir,
                min_free_bytes: 0,
                cancel: &fx.cancel,
                progress: &fx.progress,
            };
            let result = encode_video(&job, encoder).await;
            let calls = std::fs::read_to_string(self.dir.path().join("calls.txt"))
                .unwrap_or_default()
                .lines()
                .map(|line| {
                    let words: Vec<&str> = line.split_whitespace().collect();
                    let after = |flag: &str| {
                        words
                            .iter()
                            .position(|w| *w == flag)
                            .map(|i| words[i + 1].to_owned())
                            .unwrap_or_default()
                    };
                    (after("-c:v"), after("-b:v"))
                })
                .collect();
            (result, calls)
        }

        fn out(&self, name: &str) -> PathBuf {
            self.dir.path().join("out").join(name)
        }
    }

    fn sized_plan(limit: u64, target: Option<u64>, codec: VideoCodec) -> Plan {
        Plan {
            limit_bytes: limit,
            target_bytes: target,
            stream_languages: Vec::new(),
            video_codec: codec,
            audio_kbps: None,
            audio_index: None,
            vf: None,
            output_name: "clip_sq.mp4".into(),
            ..sample_plan()
        }
    }

    #[tokio::test]
    async fn a_hevc_file_over_the_limit_gets_one_corrective_pass_with_hevc() {
        let sizes = Sizes::new("encode-hevc-corrective", 11_000, 9_000);
        let plan = sized_plan(10_000, None, VideoCodec::Hevc);
        let (result, calls) = sizes.run(&plan, Encoder::Nvenc).await;
        let produced = result.expect("the second pass fits");
        assert_eq!(produced.size, 9_000);
        let corrected = plan::corrective_video_bps(1_000_000, 10_000, 11_000).unwrap();
        assert_eq!(
            calls,
            [
                ("hevc_nvenc".into(), "1000000".into()),
                ("hevc_nvenc".into(), corrected.to_string()),
            ],
            "both passes are HEVC, the second at the corrected bitrate"
        );
        assert!(!sizes.out("clip_sq.mp4.part").exists());
    }

    #[tokio::test]
    async fn a_second_pass_still_over_the_limit_fails_and_leaves_no_file() {
        let sizes = Sizes::new("encode-hevc-too-large", 12_000, 11_000);
        let plan = sized_plan(10_000, None, VideoCodec::Hevc);
        let (result, calls) = sizes.run(&plan, Encoder::Nvenc).await;
        assert!(
            matches!(
                result,
                Err(MediaError::Failed {
                    code: "OUTPUT_TOO_LARGE",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(calls.len(), 2, "one correction, never a third run");
        assert!(!sizes.out("clip_sq.mp4").exists());
        assert!(!sizes.out("clip_sq.mp4.part").exists());
    }

    #[tokio::test]
    async fn a_file_within_the_limit_takes_a_single_pass() {
        let sizes = Sizes::new("encode-single", 9_990, 1);
        let plan = sized_plan(10_000, None, VideoCodec::H264);
        let (result, calls) = sizes.run(&plan, Encoder::Nvenc).await;
        assert_eq!(result.unwrap().size, 9_990);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "h264_nvenc");
    }

    #[tokio::test]
    async fn a_custom_target_is_corrected_against_the_target_but_the_limit_decides() {
        // Target 10 000 of a 100 000 limit. 11 000 is over the target: corrected. The second
        // 10 500 is still over the target but far under the limit: kept.
        let plan = sized_plan(100_000, Some(10_000), VideoCodec::Hevc);
        let sizes = Sizes::new("encode-target-kept", 11_000, 10_500);
        let (result, calls) = sizes.run(&plan, Encoder::Nvenc).await;
        assert_eq!(result.unwrap().size, 10_500);
        assert_eq!(calls.len(), 2);
        let corrected = plan::corrective_video_bps(1_000_000, 10_000, 11_000).unwrap();
        assert_eq!(
            calls[1].1,
            corrected.to_string(),
            "scaled from the target, not from the limit"
        );

        // Over the target by a little: one correction brings it under.
        let sizes = Sizes::new("encode-target-met", 10_300, 9_800);
        let (result, calls) = sizes.run(&plan, Encoder::Nvenc).await;
        assert_eq!(result.unwrap().size, 9_800);
        assert_eq!(calls.len(), 2);

        // A file under the target needs no correction.
        let sizes = Sizes::new("encode-target-under", 9_900, 1);
        let (result, calls) = sizes.run(&plan, Encoder::Nvenc).await;
        assert_eq!(result.unwrap().size, 9_900);
        assert_eq!(calls.len(), 1);
    }

    #[tokio::test]
    async fn the_limit_still_binds_with_a_custom_target() {
        let sizes = Sizes::new("encode-target-limit", 13_000, 12_500);
        let plan = sized_plan(12_000, Some(10_000), VideoCodec::H264);
        let (result, calls) = sizes.run(&plan, Encoder::Nvenc).await;
        assert!(
            matches!(
                result,
                Err(MediaError::Failed {
                    code: "OUTPUT_TOO_LARGE",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(calls.len(), 2);
    }

    #[tokio::test]
    async fn an_encoder_without_the_codec_is_refused_before_any_run() {
        let sizes = Sizes::new("encode-no-codec", 1, 1);
        let plan = sized_plan(10_000, None, VideoCodec::Hevc);
        let (result, calls) = sizes.run(&plan, Encoder::X264).await;
        assert!(
            matches!(
                result,
                Err(MediaError::Failed {
                    code: "FFMPEG_ENCODER_UNAVAILABLE",
                    ..
                })
            ),
            "{result:?}"
        );
        assert!(calls.is_empty(), "ffmpeg never ran");
    }

    #[test]
    fn only_a_silent_early_gpu_run_is_a_start_failure() {
        assert!(is_start_failure(true, 0.0, 800));
        assert!(is_start_failure(true, 4.57, 9_000), "the measured failure");
        assert!(is_start_failure(true, 14.9, 59_999));
        assert!(
            !is_start_failure(true, 15.0, 20_000),
            "15 s of output is late"
        );
        assert!(
            !is_start_failure(true, 3.0, 60_000),
            "a minute of work is late"
        );
        assert!(
            !is_start_failure(false, 1.0, 1_000),
            "an error on stderr is the encoder's own diagnosis"
        );
    }

    #[test]
    fn suspected_kill_requires_a_silent_code_one_gpu_exit_after_progress() {
        assert!(is_suspected_kill(
            Some(Encoder::Nvenc),
            Ending::Exited,
            Some(1),
            " \r\n\t",
            true,
            None
        ));
        for (encoder, ending, code, stderr, started) in [
            (Some(Encoder::Nvenc), Ending::Exited, Some(1), "", false),
            (Some(Encoder::Nvenc), Ending::Exited, Some(2), "", true),
            (Some(Encoder::Nvenc), Ending::Exited, None, "", true),
            (
                Some(Encoder::Nvenc),
                Ending::Exited,
                Some(1),
                "encoder error",
                true,
            ),
            (Some(Encoder::X264), Ending::Exited, Some(1), "", true),
            (None, Ending::Exited, Some(1), "", true),
            (Some(Encoder::Nvenc), Ending::Cancelled, Some(1), "", true),
            (Some(Encoder::Nvenc), Ending::TimedOut, Some(1), "", true),
        ] {
            assert!(!is_suspected_kill(
                encoder, ending, code, stderr, started, None
            ));
        }
        assert!(is_suspected_kill(
            Some(Encoder::Nvenc),
            Ending::Exited,
            Some(2),
            "warning",
            true,
            Some(false),
        ));
        assert!(!is_suspected_kill(
            Some(Encoder::Nvenc),
            Ending::Exited,
            Some(1),
            "",
            true,
            Some(true),
        ));
    }

    #[test]
    #[ignore = "requires SQUEEZEGRAM_TEST_FFMPEG_DIR with local ffmpeg"]
    fn ffreport_creates_the_literal_report_path() {
        let bin = std::env::var_os("SQUEEZEGRAM_TEST_FFMPEG_DIR").expect("local ffmpeg folder");
        let dir = crate::queue::test_util::TestDir::new("ffreport-literal");
        let folder = dir.path().join("O'Brien").join("100%");
        std::fs::create_dir_all(&folder).unwrap();
        let report = folder.join("ffmpeg-1.log");
        let output = std::process::Command::new(Path::new(&bin).join("ffmpeg.exe"))
            .env("FFREPORT", ffreport_value(&report))
            .args([
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "nullsrc",
                "-frames:v",
                "1",
                "-f",
                "null",
                "-",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(std::fs::metadata(&report).unwrap().len() > 0);
    }

    #[test]
    fn ffreport_path_is_escaped_and_only_the_last_report_line_decides() {
        let value = ffreport_value(Path::new(r"C:\Users\O'Brien\100%\ffmpeg-1.log"));
        assert_eq!(
            value.to_string_lossy(),
            r"file=C\:/Users/O\'Brien/100%%/ffmpeg-1.log:level=40"
        );

        let dir = crate::queue::test_util::TestDir::new("ffreport-tail");
        let path = dir.path().join("report.log");
        std::fs::write(&path, "Conversion failed!\r\n\r\n").unwrap();
        assert_eq!(report_ends_with_conversion_failed(&path), Some(true));
        std::fs::write(&path, "frame=42\r\nencoder stopped\r\n").unwrap();
        assert_eq!(report_ends_with_conversion_failed(&path), Some(false));
        std::fs::write(&path, "\r\n").unwrap();
        assert_eq!(report_ends_with_conversion_failed(&path), None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_between_tries_ends_at_once_when_cancelled() {
        let cancel = CancellationToken::new();
        let began = tokio::time::Instant::now();
        assert!(
            wait_before_retry(&cancel, Duration::from_secs(2))
                .await
                .is_ok()
        );
        assert_eq!(began.elapsed(), Duration::from_secs(2));

        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            canceller.cancel();
        });
        let began = tokio::time::Instant::now();
        let waited = wait_before_retry(&cancel, Duration::from_secs(2)).await;
        assert!(matches!(waited, Err(MediaError::Cancelled)));
        assert_eq!(began.elapsed(), Duration::from_millis(300));
    }

    #[test]
    fn failures_are_classified_into_spec_codes() {
        assert_eq!(
            classify_failure(
                "OpenEncodeSessionEx failed: out of memory (10)",
                Some(-1),
                true
            ),
            "FFMPEG_ENCODER_UNAVAILABLE"
        );
        assert_eq!(
            classify_failure("Nothing was written", Some(-542_398_533), false),
            "FFMPEG_ENCODER_UNAVAILABLE"
        );
        assert_eq!(
            classify_failure("Nothing was written", Some(-542_398_533), true),
            "FFMPEG_FAILED",
            "a failure after frames were produced is not an open failure"
        );
        assert_eq!(
            classify_failure("x: moov atom not found", Some(-1_094_995_529), false),
            "FILE_UNREADABLE"
        );
        assert_eq!(
            classify_failure("Permission denied", Some(-13), false),
            "FILE_UNREADABLE"
        );
        assert_eq!(
            classify_failure("No space left on device", Some(-28), true),
            "DISK_SPACE"
        );
        assert_eq!(classify_failure("whatever", Some(1), true), "FFMPEG_FAILED");
    }

    #[test]
    fn hardware_encoder_errors_are_told_apart_from_input_errors() {
        for line in [
            "[h264_nvenc @ 0x1] Failed locking bitstream buffer: NV_ENC_ERR_GENERIC (20)",
            "[h264_nvenc @ 0x1] EncodePicture failed!: device removed (8)",
            "Error submitting video frame to the encoder",
            "[h264_qsv @ 0x1] Error during encoding: device failed (-17)",
            "[h264_amf @ 0x1] SubmitInput() failed with error AMF_FAIL",
        ] {
            assert!(hardware_encoder_error(line), "{line}");
        }
        for line in [
            "in.mkv: Invalid data found when processing input",
            "No space left on device",
            "",
        ] {
            assert!(!hardware_encoder_error(line), "{line}");
        }
        let stop = EncoderStop {
            encoder: Encoder::Nvenc,
            codec: VideoCodec::H264,
            exit_code: Some(1),
            elapsed_ms: 4_499,
            out_time_s: 3.2,
            stderr_empty: true,
            stderr_tail: String::new(),
            start_failure: true,
            suspected_kill: true,
            report_path: Some(PathBuf::from(r"C:\logs\ffmpeg-1.log")),
            report_ends_with_conversion_failed: Some(false),
            attempts: 3,
        };
        assert_eq!(stop.code(), "FFMPEG_KILLED");
        assert_eq!(stop.at_seconds(), 4);
        assert!(stop.to_string().contains("h264_nvenc stopped mid-encode"));
        assert!(stop.to_string().contains("3 attempts"));
    }

    #[test]
    fn free_space_must_cover_the_need_and_the_configured_minimum() {
        assert!(enough_space(10, 5, 5));
        assert!(!enough_space(4, 5, 0));
        assert!(!enough_space(6, 5, 8));
    }

    #[test]
    fn free_space_of_the_temp_dir_is_readable() {
        assert!(free_space(&std::env::temp_dir()).is_some_and(|f| f > 0));
    }

    fn sample_plan() -> Plan {
        Plan {
            kind: Kind::Encode,
            limit_bytes: 100 * plan::MIB,
            duration_s: Some(60.0),
            video_bps: Some(1_000_000),
            audio_kbps: Some(128),
            video_index: Some(0),
            audio_index: Some(2),
            vf: Some("scale=w=2:h=2".into()),
            hdr: false,
            encoder: None,
            output_name: "clip_sq.mp4".into(),
            photo: None,
            source_dims: None,
            alpha: false,
            note: None,
            video_codec: VideoCodec::H264,
            preset: EncodePreset::Balanced,
            target_bytes: None,
            stream_languages: Vec::new(),
        }
    }

    fn to_strings(args: Args) -> Vec<String> {
        args.into_vec()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    struct Fixture {
        cancel: CancellationToken,
        progress: ProgressReporter,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                cancel: CancellationToken::new(),
                progress: ProgressReporter::new(1, Phase::Encoding, Arc::new(Recorder::default())),
            }
        }

        fn args(
            &self,
            plan: &Plan,
            make: impl FnOnce(&Job<'_>, &Path) -> Args,
        ) -> (Vec<String>, String) {
            let tools = Tools {
                ffmpeg: Path::new("ffmpeg.exe"),
                ffprobe: Path::new("ffprobe.exe"),
            };
            let job = Job {
                tools: &tools,
                input: Path::new("C:\\in put\\a.mkv"),
                plan,
                out_dir: Path::new("C:\\tmp\\1"),
                min_free_bytes: 0,
                cancel: &self.cancel,
                progress: &self.progress,
            };
            let list = to_strings(make(&job, &job.part()));
            let text = list.join(" ");
            (list, text)
        }
    }

    #[tokio::test]
    async fn unsupported_extensions_never_reach_conversion_or_strip() {
        let fx = Fixture::new();
        let plan = sample_plan();
        let tools = Tools {
            ffmpeg: Path::new("no-such-ffmpeg.exe"),
            ffprobe: Path::new("no-such-ffprobe.exe"),
        };
        let job = Job {
            tools: &tools,
            input: Path::new("playlist.m3u8"),
            plan: &plan,
            out_dir: Path::new("no-such-output"),
            min_free_bytes: 0,
            cancel: &fx.cancel,
            progress: &fx.progress,
        };
        for error in [
            encode_video(&job, Encoder::X264).await.unwrap_err(),
            remux(&job).await.unwrap_err(),
            strip(&job).await.unwrap_err(),
            photo(&job).await.unwrap_err(),
        ] {
            assert_eq!(error.code(), Some("INPUT_FORMAT_UNSUPPORTED"));
        }
    }

    #[test]
    fn every_file_input_is_restricted_without_restricting_generated_lavfi() {
        let fx = Fixture::new();
        let plan = sample_plan();
        fx.args(&plan, |job, part| {
            for pass in [Pass::Single, Pass::First, Pass::Second] {
                super::super::assert_file_input(
                    encode_args(job, Encoder::X264, 1_000_000, pass, part),
                    job.input,
                );
            }
            super::super::assert_file_input(remux_args(job, part), job.input);
            super::super::assert_file_input(strip_video_args(job, part), job.input);
            remux_args(job, part)
        });
        let mut plan = sample_plan();
        plan.kind = Kind::Photo;
        let size = PhotoSize {
            width: 100,
            height: 50,
        };
        for alpha in [false, true] {
            plan.alpha = alpha;
            plan.source_dims = Some((640, 480));
            fx.args(&plan, |job, part| {
                let job = Job {
                    input: Path::new("photo%d.png"),
                    ..*job
                };
                let args = photo_args(&job, size, 3, part);
                super::super::assert_file_input(args.clone(), job.input);
                let list = to_strings(args.clone());
                assert_eq!(
                    list.iter().filter(|arg| *arg == "-i").count(),
                    if alpha { 2 } else { 1 },
                );
                if alpha {
                    assert!(list.windows(4).any(|args| {
                        args == ["-f", "lavfi", "-i", "color=c=white:size=640x480"]
                    }));
                }
                args
            });
        }
    }

    #[test]
    fn normal_runs_log_only_errors_so_warnings_do_not_block_retries() {
        let args = to_strings(common_start(Path::new("clip.mp4")));
        assert!(args.windows(2).any(|pair| pair == ["-loglevel", "error"]));
    }

    #[test]
    fn encode_command_follows_the_recipe() {
        let fx = Fixture::new();
        let mut plan = sample_plan();
        let (list, text) = fx.args(&plan, |job, part| {
            encode_args(job, Encoder::Nvenc, 1_000_000, Pass::Single, part)
        });
        assert!(text.starts_with(
            &format!("-hide_banner -nostdin -loglevel error -y -progress pipe:1 -nostats -protocol_whitelist file -format_whitelist {} -i C:\\in put\\a.mkv", super::super::INPUT_FORMAT_WHITELIST)
        ));
        assert!(text.contains("-map 0:0 -map 0:2 -sn -dn -map_chapters -1 -map_metadata -1"));
        assert!(text.contains("-vf scale=w=2:h=2"));
        assert!(
            text.contains("-pix_fmt yuv420p -c:a aac -b:a 128k -ac 2 -movflags +faststart -f mp4")
        );
        assert!(list.last().unwrap().ends_with("clip_sq.mp4.part"));
        assert!(!text.contains("color_trc"));

        plan.hdr = true;
        plan.audio_index = None;
        plan.audio_kbps = None;
        let (_, text) = fx.args(&plan, |job, part| {
            encode_args(job, Encoder::OpenH264, 800_000, Pass::Single, part)
        });
        assert!(text.contains("-color_primaries bt709 -color_trc bt709 -colorspace bt709"));
        assert!(!text.contains("-c:a") && !text.contains("-map 0:2"));
    }

    #[test]
    fn x264_passes_share_a_log_and_the_first_pass_writes_nothing() {
        let fx = Fixture::new();
        let plan = sample_plan();
        let (_, first) = fx.args(&plan, |job, part| {
            encode_args(job, Encoder::X264, 1_000_000, Pass::First, part)
        });
        let (_, second) = fx.args(&plan, |job, part| {
            encode_args(job, Encoder::X264, 1_000_000, Pass::Second, part)
        });
        assert!(first.contains("-pass 1 -passlogfile C:\\tmp\\1\\pass"));
        assert!(first.ends_with("-an -f null NUL"));
        assert!(second.contains("-pass 2 -passlogfile C:\\tmp\\1\\pass"));
        assert!(second.contains("-c:a aac") && second.ends_with("clip_sq.mp4.part"));
    }

    #[test]
    fn remux_and_photo_commands() {
        let fx = Fixture::new();
        let mut plan = sample_plan();
        plan.kind = Kind::Remux;
        plan.audio_kbps = None;
        let (_, copy) = fx.args(&plan, remux_args);
        assert!(copy.contains("-c copy -movflags +faststart -f mp4"));

        plan.kind = Kind::RemuxAudio;
        plan.audio_kbps = Some(128);
        let (_, audio) = fx.args(&plan, remux_args);
        assert!(audio.contains("-c:v copy -c:a aac -b:a 128k -ac 2"));

        plan.kind = Kind::Photo;
        plan.output_name = "p_sq.jpg".into();
        let size = PhotoSize {
            width: 100,
            height: 50,
        };
        let (_, plain) = fx.args(&plan, |job, part| photo_args(job, size, 3, part));
        assert!(plain.contains(
            "-vf scale=100:50:flags=lanczos,format=yuvj420p -map_metadata -1 -frames:v 1 -q:v 3 -f mjpeg"
        ));

        plan.alpha = true;
        plan.source_dims = Some((640, 480));
        let (_, alpha) = fx.args(&plan, |job, part| photo_args(job, size, 5, part));
        assert!(alpha.contains("color=c=white:size=640x480"));
        assert!(alpha.contains("[1:v][0:v]overlay=format=auto:shortest=1,scale=100:50"));
    }
}
