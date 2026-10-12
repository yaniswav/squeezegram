//! ffmpeg runs: target-size encode, remux and photo.
//!
//! Every output is written to `<name>.part` with `-y`, then renamed atomically: without `-y`
//! ffmpeg exits 0 without writing when the file exists, and a killed encode leaves an unreadable
//! file that must never be reused.

use std::path::{Path, PathBuf};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::detect::{Encoder, Tools};
use super::plan::{self, Kind, PHOTO_MAX_BYTES, PhotoSize, Plan};
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

/// Rate-control options of the H.264 encoders for a target video bitrate.
///
/// NVENC and x264 are measured. **QSV, AMF, libopenh264 and h264_mf are NOT
/// measured**: they use the same prudent CBR-like scheme (`-b:v V -maxrate V -bufsize 2V`); the
/// corrective pass catches a drift, but the size they reach has not been validated.
pub fn video_options(encoder: Encoder, video_bps: u64) -> Vec<String> {
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
    let mut options = match encoder {
        Encoder::Nvenc => rate(&[
            "-c:v",
            "h264_nvenc",
            "-preset",
            "p5",
            "-tune",
            "hq",
            "-rc",
            "cbr",
        ]),
        Encoder::X264 => rate(&["-c:v", "libx264", "-preset", "medium"]),
        // Not measured:
        Encoder::Qsv => rate(&["-c:v", "h264_qsv", "-preset", "medium"]),
        Encoder::Amf => rate(&["-c:v", "h264_amf", "-quality", "balanced", "-rc", "cbr"]),
        Encoder::OpenH264 => rate(&["-c:v", "libopenh264", "-rc_mode", "bitrate"]),
        Encoder::MediaFoundation => rate(&["-c:v", "h264_mf", "-rate_control", "cbr"]),
    };
    if encoder != Encoder::MediaFoundation {
        options.extend(["-profile:v".to_owned(), "high".to_owned()]);
    }
    options
}

fn common_start() -> Args {
    Args::new().all(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-y",
        "-progress",
        "pipe:1",
        "-nostats",
    ])
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
    let mut args = common_start().arg("-i").arg(job.input);
    args = map_streams(args, plan);
    if let Some(vf) = &plan.vf {
        args = args.arg("-vf").arg(vf);
    }
    for option in video_options(encoder, video_bps) {
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
    let mut args = map_streams(common_start().arg("-i").arg(job.input), plan);
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
    let mut args = Args::new()
        .all(&["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .arg("-i")
        .arg(job.input);
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

/// A GPU encoder that ended with a code other than 0, not cancelled, with an empty stderr or a
/// hardware encoder error (seen on an RTX 4080: `h264_nvenc` exits 1 after 2 to 6 s, nothing on
/// stderr, while a game and NVIDIA recording run; cause not established).
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderStop {
    pub encoder: Encoder,
    pub exit_code: Option<i32>,
    /// Time spent in the failed run.
    pub elapsed_ms: u64,
    /// Last `out_time` reached, in seconds of output.
    pub out_time_s: f64,
    pub stderr_empty: bool,
    /// Last stderr lines (journal only).
    pub stderr_tail: String,
}

impl EncoderStop {
    /// Whole seconds of the failed run, as shown to the user.
    pub fn at_seconds(&self) -> u64 {
        (self.elapsed_ms as f64 / 1000.0).round() as u64
    }
}

impl std::fmt::Display for EncoderStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} stopped mid-encode: exit {:?} after {} ms at {:.2} s of output, stderr: {}",
            self.encoder.name(),
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

/// The ffmpeg spec of a job, with its `ffmpeg_start` journal line (command line with paths;
/// arguments never hold a caption or any other content).
fn ffmpeg_spec<'a>(job: &'a Job<'_>, args: Args, step: &str) -> Spec<'a> {
    let spec = Spec::new(job.tools.ffmpeg, args)
        .cancel(job.cancel)
        .stderr_lines(STDERR_LINES);
    let command = serde_json::json!({ "command": spec.describe() }).to_string();
    tracing::info!(
        event = "ffmpeg_start",
        step,
        json = command.as_str(),
        "ffmpeg started"
    );
    spec
}

/// `ffmpeg_exit` (success) or `ffmpeg_failed` (with the stderr tail) journal line.
fn log_ffmpeg_end(output: &procs::Output, began: Instant, code: Option<&str>) {
    let dur_ms = crate::logging::ms_since(began);
    let ending = match output.ending {
        Ending::Exited => "exited",
        Ending::TimedOut => "timed_out",
        Ending::Cancelled => "cancelled",
    };
    match code {
        None => tracing::info!(
            event = "ffmpeg_exit",
            dur_ms,
            exit_code = output.code,
            ending,
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
                json = tail.as_str(),
                "ffmpeg failed"
            );
        }
    }
}

/// Runs ffmpeg with progress mapped onto `start..start+width` percent. `encoder` is the video
/// encoder of an encode (`None` for a remux): a GPU one that stops mid-encode gives
/// `MediaError::EncoderStopped`.
async fn run_ffmpeg(
    job: &Job<'_>,
    step: &str,
    args: Args,
    duration_s: Option<f64>,
    (start, width): (f64, f64),
    encoder: Option<Encoder>,
) -> Result<()> {
    let mut parser = ProgressParser::new(duration_s);
    let began = Instant::now();
    let mut started = false;
    let mut reached_s = 0.0_f64;
    let output = procs::run(ffmpeg_spec(job, args, step), |line| {
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
    })
    .await
    .map_err(|e| MediaError::failed("FFMPEG_MISSING", e.to_string()))?;
    match output.ending {
        Ending::Cancelled => {
            log_ffmpeg_end(&output, began, None);
            return Err(MediaError::Cancelled);
        }
        Ending::TimedOut => {
            log_ffmpeg_end(&output, began, Some("FFMPEG_FAILED"));
            return Err(MediaError::failed("FFMPEG_FAILED", "timeout"));
        }
        Ending::Exited => {}
    }
    if !output.success() {
        let stderr = output.stderr_tail.join("\n");
        let code = classify_failure(&stderr, output.code, started);
        log_ffmpeg_end(&output, began, Some(code));
        let stderr_empty = stderr.trim().is_empty();
        if let Some(encoder) = encoder.filter(|e| e.is_gpu())
            && code == "FFMPEG_FAILED"
            && (stderr_empty || hardware_encoder_error(&stderr))
        {
            return Err(MediaError::EncoderStopped(Box::new(EncoderStop {
                encoder,
                exit_code: output.code,
                elapsed_ms: crate::logging::ms_since(began),
                out_time_s: reached_s,
                stderr_empty,
                stderr_tail: stderr,
            })));
        }
        return Err(MediaError::failed(
            code,
            format!("ffmpeg exit {:?}: {stderr}", output.code),
        ));
    }
    log_ffmpeg_end(&output, began, None);
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
    let info = probe::probe(job.tools.ffprobe, part, false, job.cancel).await?;
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
    let result = encode_video_inner(job, encoder).await;
    if result.is_err() {
        cleanup(job);
    }
    result
}

async fn encode_video_inner(job: &Job<'_>, encoder: Encoder) -> Result<Produced> {
    let plan = job.plan;
    let limit = plan.limit_bytes;
    let mut video_bps = plan
        .video_bps
        .ok_or_else(|| MediaError::failed("FFMPEG_FAILED", "plan has no video bitrate"))?;
    let slack = if encoder == Encoder::X264 {
        PASS_LOG_SLACK
    } else {
        0
    };
    prepare(job, (limit as f64 * SPACE_FACTOR) as u64 + slack)?;
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
        if size <= limit {
            verify_duration(job, &part).await?;
            cleanup_pass_logs(job);
            return finish(job);
        }
        match (attempt, plan::corrective_video_bps(video_bps, limit, size)) {
            (0, Some(corrected)) => {
                tracing::info!(
                    event = "encode_corrective_pass",
                    size,
                    limit,
                    video_bps,
                    corrected_bps = corrected,
                    "output over the limit: one corrective pass"
                );
                video_bps = corrected;
            }
            _ => {
                return Err(MediaError::failed(
                    "OUTPUT_TOO_LARGE",
                    format!("{size} bytes against a limit of {limit}"),
                ));
            }
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

/// Photo to JPEG: quality loop, then smaller dimensions, until under 9.5 MiB.
pub async fn photo(job: &Job<'_>) -> Result<Produced> {
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
    let output = procs::run(ffmpeg_spec(job, args, "photo"), |_| {})
        .await
        .map_err(|e| MediaError::failed("FFMPEG_MISSING", e.to_string()))?;
    match output.ending {
        Ending::Cancelled => {
            log_ffmpeg_end(&output, began, None);
            Err(MediaError::Cancelled)
        }
        Ending::TimedOut => {
            log_ffmpeg_end(&output, began, Some("FFMPEG_FAILED"));
            Err(MediaError::failed("FFMPEG_FAILED", "timeout"))
        }
        Ending::Exited if output.success() => {
            log_ffmpeg_end(&output, began, None);
            Ok(())
        }
        Ending::Exited => {
            let stderr = output.stderr_tail.join("\n");
            let code = classify_failure(&stderr, output.code, true);
            log_ffmpeg_end(&output, began, Some(code));
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
            let lines = [
                "frame=436".to_owned(),
                "bitrate=3044.3kbits/s".to_owned(),
                "total_size=5505072".to_owned(),
                format!("out_time_us={time}"),
                format!("out_time_ms={time}"),
                format!("speed={speed}"),
                format!("progress={state}"),
            ];
            lines
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
            let options = video_options(encoder, 1_000_000);
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
        let nvenc = video_options(Encoder::Nvenc, 5);
        assert!(nvenc.windows(2).any(|w| w == ["-rc", "cbr"]));
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
            exit_code: Some(1),
            elapsed_ms: 4_499,
            out_time_s: 3.2,
            stderr_empty: true,
            stderr_tail: String::new(),
        };
        assert_eq!(stop.at_seconds(), 4);
        assert!(stop.to_string().contains("h264_nvenc stopped mid-encode"));
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

    #[test]
    fn encode_command_follows_the_recipe() {
        let fx = Fixture::new();
        let mut plan = sample_plan();
        let (list, text) = fx.args(&plan, |job, part| {
            encode_args(job, Encoder::Nvenc, 1_000_000, Pass::Single, part)
        });
        assert!(text.starts_with(
            "-hide_banner -nostdin -loglevel error -y -progress pipe:1 -nostats -i C:\\in put\\a.mkv"
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
        let (_, copy) = fx.args(&plan, |job, part| remux_args(job, part));
        assert!(copy.contains("-c copy -movflags +faststart -f mp4"));

        plan.kind = Kind::RemuxAudio;
        plan.audio_kbps = Some(128);
        let (_, audio) = fx.args(&plan, |job, part| remux_args(job, part));
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
