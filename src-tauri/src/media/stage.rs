//! The real `MediaStage`: probe + identity, then encode, remux or photo.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::detect::{self, Detection, Encoder, Tools};
use super::encode::{self, EncoderStop, Job};
use super::metadata;
use super::plan::{self, EncodingOverride, Kind, MIB, Plan, PlanInput, VideoCodec};
use super::probe::{self, MediaInfo};
use super::{MediaError, Result, hash};
use crate::entitlements::{Entitlements, Feature};
use crate::limit::LimitResolver;
use crate::queue::events::{Phase, Progress};
use crate::queue::intake;
use crate::queue::orchestrator::{
    Analysis, MediaOutput, MediaStage, ProbeOutcome, StageContext, StageError, Treatment,
};
use crate::queue::store::{Item, SendMode};
use crate::settings::{AdvancedEncoding, Settings, SettingsStore};
use crate::sidecars::{Resolved, SidecarService};

const PROGRESS_TICK: Duration = Duration::from_millis(250);

/// The encoding choices of a plan (cap, audio bitrate, codec, preset, target size), and whether
/// they come from the advanced settings (`true`) or are the automatic ones.
///
/// The license is asked here, when the plan is built: a value saved while the feature was
/// granted (or written into `settings.json` or the item's row by hand) has no effect without
/// it. The item's own choices (`file`) replace the settings' codec, cap and target.
pub(crate) fn encoding_choice(
    settings: &Settings,
    entitlements: &dyn Entitlements,
    file: Option<&EncodingOverride>,
) -> (AdvancedEncoding, bool) {
    if !entitlements.allows(Feature::AdvancedEncoding) {
        return (AdvancedEncoding::AUTOMATIC, false);
    }
    let mut choice = settings.advanced_encoding.clone();
    if let Some(file) = file.filter(|file| file.is_valid()) {
        choice.video_codec = file.video_codec;
        choice.max_resolution = file.max_resolution;
        choice.target_mb = file.target_mb;
    }
    (choice, true)
}

/// The plan input of `item` under `choice`.
fn plan_input<'a>(
    source: &'a Path,
    item: &Item,
    limit_bytes: u64,
    settings: &Settings,
    choice: &AdvancedEncoding,
    moov_first: Option<bool>,
) -> PlanInput<'a> {
    PlanInput {
        source,
        source_size: u64::try_from(item.source_size).unwrap_or(0),
        send_mode: item.send_mode,
        limit_bytes,
        margin: settings.limit.margin_percent / 100.0,
        audio_kbps: choice.audio_kbps,
        cap: choice.max_resolution,
        codec: choice.video_codec,
        preset: choice.preset,
        target_bytes: choice.target_mb.map(|mb| mb * MIB),
        moov_first,
    }
}

/// The probe of `item` stored by an earlier look at it (the analysis at drop, or a previous
/// attempt), if the file is still the one it described: same size and modification time as
/// when it was queued. `stamp` is the file's now.
fn reusable_info(item: &Item, stamp: Option<(i64, i64)>) -> Option<MediaInfo> {
    if stamp != Some((item.source_size, item.source_mtime)) {
        return None;
    }
    serde_json::from_str(item.media_json.as_deref()?).ok()
}

/// Whether the `moov` atom of an MP4 comes first; `None` for other files.
async fn moov_first_of(source: &Path) -> Option<bool> {
    if !source
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
    {
        return None;
    }
    let path = source.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::File::open(path)
            .ok()
            .and_then(|mut f| probe::moov_first(&mut f))
    })
    .await
    .ok()
    .flatten()
}

/// Result of `item_encoding_estimate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EncodeEstimate {
    /// `encode` (re-encoded to `bytes`), `as_is` (sent or remuxed: about the source size),
    /// `photo` (no estimate), `too_small` (the target cannot hold this duration) or `unknown`
    /// (no duration to size an encode).
    pub outcome: &'static str,
    pub bytes: Option<u64>,
    pub limit_bytes: u64,
    /// The codec the encode will really use.
    pub codec: VideoCodec,
    /// HEVC was asked for and no HEVC encoder works: the encode will be H.264.
    pub codec_fallback: bool,
}

/// Result of `codec_support`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodecSupport {
    /// A HEVC encoder passed its test encode.
    pub hevc: bool,
    pub hevc_encoder: Option<&'static str>,
    /// Why not: `no_hevc_encoder` (ffmpeg has none that works here) or `ffmpeg_unavailable`.
    pub reason: Option<&'static str>,
}

/// Largest image read whole to remove its metadata. Far above any photo; a bigger file is sent as
/// it is (and the plan says so in the journal).
const STRIP_MAX_BYTES: i64 = 256 * MIB as i64;

/// A JPEG, PNG or WebP by its first bytes, small enough for `metadata::strip`.
fn strippable_image(source: &Path, size: i64) -> bool {
    use std::io::Read;

    let mut head = [0u8; metadata::SNIFF_LEN];
    size <= STRIP_MAX_BYTES
        && std::fs::File::open(source)
            .and_then(|mut file| file.read_exact(&mut head))
            .is_ok()
        && metadata::sniff(&head).is_some()
}

/// `plan` under "Remove location and camera data" (`plan::without_private_data`); the badge of
/// the background analysis and the probing apply it alike.
fn with_privacy(
    settings: &Settings,
    plan: Plan,
    source: &Path,
    item: &Item,
    info: Option<&MediaInfo>,
) -> Plan {
    if !settings.privacy.strip_metadata {
        return plan;
    }
    let private = plan::PrivateData {
        image: plan.kind == Kind::Skip && strippable_image(source, item.source_size),
        video_tags: info.is_some_and(|info| info.private_tags),
        languages: info.map(plan::copied_languages).unwrap_or_default(),
    };
    plan::without_private_data(plan, source, private)
}

struct Cached {
    ffmpeg: PathBuf,
    detection: Detection,
}

/// Custom temp dir, else `%LOCALAPPDATA%\squeezegram\tmp`. Each item encodes in `<root>\<id>`.
/// Shared with the startup sweep of `queue::sweep`, which must look in the same place.
pub fn temp_root(temp: &crate::settings::Temp) -> PathBuf {
    if let Some(dir) = temp.dir.as_ref().filter(|d| !d.trim().is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(|local| PathBuf::from(local).join("squeezegram").join("tmp"))
        .unwrap_or_else(|| std::env::temp_dir().join("squeezegram"))
}

pub struct FfmpegStage {
    sidecars: Arc<SidecarService>,
    settings: Arc<SettingsStore>,
    /// Effective upload limit, resolved once per item when its media work starts.
    limits: Arc<LimitResolver>,
    /// Asked for `AdvancedEncoding` each time a plan is built.
    entitlements: Arc<dyn Entitlements>,
    detection: Mutex<Option<Cached>>,
    /// The last probe of the estimate (`estimate`): the options panel asks again at each
    /// change, the file does not change.
    estimate_probe: std::sync::Mutex<Option<(PathBuf, i64, i64, MediaInfo)>>,
    gpu: GpuSession,
}

/// A GPU encoder that stopped mid-encode in this session (`encoder_status`), and that the
/// session now avoids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuFailure {
    pub encoder: String,
    /// Seconds the failed run lasted.
    pub at_seconds: u64,
}

/// Items in a row whose GPU encode failed to start, even after its retries, that turn the GPU
/// off for the session. One is not enough: the failure is random (about 3 runs in 8 on one
/// large file, then the next one works), so the GPU must be given a chance on the next item;
/// a second one in a row says the GPU is not usable now, and paying 3 failed tries per file
/// would be worse than the CPU.
const START_FAILED_ITEMS_TO_DISABLE: u32 = 2;

#[derive(Default)]
struct GpuState {
    /// Set once the GPU is avoided for the rest of the session.
    failure: Option<GpuFailure>,
    /// Items in a row whose GPU encode failed to start for good; reset by a GPU success.
    start_failed_items: u32,
}

/// Memory of GPU failures, for this session only. A GPU encoder that stops mid-encode (late, or
/// with an error of its own) is avoided at once: new encodes then go to the CPU instead of
/// paying a failed GPU attempt on every file. One that only failed to start (see
/// `encode::is_start_failure`) is avoided after `START_FAILED_ITEMS_TO_DISABLE` items in a
/// row. The GPU is tried again after a restart.
#[derive(Default)]
pub struct GpuSession(std::sync::Mutex<GpuState>);

impl GpuSession {
    fn state(&self) -> std::sync::MutexGuard<'_, GpuState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The failure the session avoids, if the GPU is off.
    pub fn failure(&self) -> Option<GpuFailure> {
        self.state().failure.clone()
    }

    /// Notes a GPU encode that failed for good (its retries included); `true` if the GPU is now
    /// off for the session.
    fn record(&self, stop: &EncoderStop) -> bool {
        let mut state = self.state();
        if stop.start_failure {
            state.start_failed_items += 1;
            if state.start_failed_items < START_FAILED_ITEMS_TO_DISABLE {
                return false;
            }
        }
        state.failure = Some(GpuFailure {
            encoder: stop.encoder_name().to_owned(),
            at_seconds: stop.at_seconds(),
        });
        true
    }

    /// A GPU encode went through: the failures to start were not a pattern.
    fn recovered(&self) {
        self.state().start_failed_items = 0;
    }
}

/// The encoder of a new encode: the user's preference, except that a GPU that stopped
/// mid-encode this session is skipped while a CPU encoder works. HEVC has no CPU encoder, so
/// the GPU is tried again whatever happened before.
fn pick_encoder(
    detection: &Detection,
    codec: VideoCodec,
    prefer_cpu: bool,
    gpu_failed: bool,
) -> Option<Encoder> {
    if codec == VideoCodec::Hevc {
        detection.hevc_gpu()
    } else if gpu_failed {
        detection.cpu().or_else(|| detection.gpu())
    } else {
        detection.choose(prefer_cpu)
    }
}

/// `{ encoder, exitCode, atSeconds }` of a stopped GPU encoder (params of `FFMPEG_FAILED`).
fn stop_params(stop: &EncoderStop) -> serde_json::Value {
    json!({
        "encoder": stop.encoder_name(),
        "exitCode": stop.exit_code,
        "atSeconds": stop.at_seconds(),
    })
}

fn stage_error(error: MediaError) -> StageError {
    let (code, params, detail) = match error {
        MediaError::Cancelled => {
            tracing::info!(event = "media_cancelled", "media work cancelled");
            return StageError::Cancelled;
        }
        MediaError::Failed { code, detail } => (code, json!({}), detail),
        MediaError::Reported {
            code,
            params,
            detail,
        } => (code, params, detail),
        // Not retried on the CPU (no fallback was possible): reported as such.
        MediaError::EncoderStopped(stop) => {
            let mut params = stop_params(&stop);
            params["cause"] = json!("no_cpu_encoder");
            ("FFMPEG_FAILED", params, stop.to_string())
        }
    };
    // `detail` holds paths and ffmpeg's last lines: journal only, never the UI.
    tracing::warn!(
        event = "media_failed",
        error_code = code,
        detail = detail.as_str(),
        "media work failed"
    );
    StageError::Failed {
        code: code.into(),
        params,
    }
}

/// The `cause` of the `ENCODER_FALLBACK` alert: it picks the sentence, which must say whether
/// the GPU is given up for the session (`gpu_off`) or tried again on the next file.
fn fallback_cause(stop: &EncoderStop, gpu_off: bool) -> &'static str {
    match (stop.start_failure, gpu_off) {
        (true, false) => "start_failed",
        (true, true) => "start_failed_again",
        (false, _) if stop.stderr_empty => "silent",
        (false, _) => "encoder_error",
    }
}

/// Codes of a failed CPU retry that say more than "the retry failed too": kept as they are.
const KEPT_FALLBACK_CODES: [&str; 5] = [
    "DISK_SPACE",
    "OUTPUT_TOO_LARGE",
    "FILE_UNREADABLE",
    "IO_ERROR",
    "FFMPEG_MISSING",
];

/// Encodes with `encoder`; if it is a GPU encoder that stops mid-encode (no cancellation, empty
/// stderr or a hardware error), retries ONCE with the CPU encoder `cpu` (the detection's first
/// working one: libx264, else libopenh264, else h264_mf), on a fresh `.part` and a plan rebuilt
/// for it. A GPU that merely failed to start was already tried again, silently, by
/// `encode::encode_video`; only if all its tries fail does it get here. Whether the GPU is then
/// skipped for the rest of the session depends on the kind of failure (`GpuSession::record`).
///
/// No loop: the retry runs on the CPU, which never gives `EncoderStopped`, and any failure of
/// the retry is final.
pub(crate) async fn encode_with_cpu_fallback(
    job: &Job<'_>,
    encoder: Encoder,
    cpu: Option<Encoder>,
    margin: f64,
    session: &GpuSession,
) -> Result<encode::Produced> {
    let stop = match encode::encode_video(job, encoder).await {
        Err(MediaError::EncoderStopped(stop)) => stop,
        other => {
            if other.is_ok() && encoder.is_gpu() {
                session.recovered();
            }
            return other;
        }
    };
    let gpu_off = session.record(&stop);
    let detail = stop.to_string();
    tracing::warn!(
        event = "encoder_failed_midway",
        error_code = "FFMPEG_FAILED",
        encoder = stop.encoder_name(),
        exit_code = stop.exit_code,
        dur_ms = stop.elapsed_ms,
        out_time_s = (stop.out_time_s * 100.0).round() / 100.0,
        stderr_empty = stop.stderr_empty,
        start_failure = stop.start_failure,
        attempts = stop.attempts,
        gpu_off,
        fallback = cpu.map(Encoder::name),
        stderr_tail = stop.stderr_tail.as_str(),
        "GPU encoder stopped mid-encode"
    );
    let mut params = stop_params(&stop);
    // A CPU retry would turn a HEVC plan into an H.264 file: HEVC has no CPU encoder to retry on.
    let hevc = job.plan.video_codec == VideoCodec::Hevc;
    let Some(cpu) = cpu.filter(|e| !e.is_gpu() && !hevc) else {
        params["cause"] = json!(if hevc {
            "hevc_gpu_only"
        } else {
            "no_cpu_encoder"
        });
        return Err(MediaError::Reported {
            code: "FFMPEG_FAILED",
            params,
            detail,
        });
    };

    let plan = plan::for_fallback(job.plan, cpu.name(), margin);
    tracing::info!(
        event = "encoder_chosen",
        encoder = cpu.name(),
        gpu = false,
        fallback = true,
        reason = "gpu_failed_midway",
        "CPU fallback encoder chosen"
    );
    let plan_json = json!({ "plan": plan }).to_string();
    tracing::info!(
        event = "media_plan",
        margin_percent = margin * 100.0,
        fallback = true,
        json = plan_json.as_str(),
        "plan rebuilt for the CPU fallback"
    );
    job.progress.alert(
        "ENCODER_FALLBACK",
        json!({
            "from": stop.encoder_name(),
            "to": cpu.name(),
            "atSeconds": stop.at_seconds(),
            "cause": fallback_cause(&stop, gpu_off),
        }),
    );
    job.progress.report_now(Progress {
        percent: 0.0,
        phase: Some(Phase::Encoding),
        detail: Some("cpu_fallback".to_owned()),
        ..Default::default()
    });
    let retry = Job {
        tools: job.tools,
        input: job.input,
        plan: &plan,
        out_dir: job.out_dir,
        min_free_bytes: job.min_free_bytes,
        cancel: job.cancel,
        progress: job.progress,
    };
    // `encode_video` deleted the GPU `.part` on its failure and starts from a fresh one.
    match encode::encode_video(&retry, cpu).await {
        Err(MediaError::Failed { code, detail }) if KEPT_FALLBACK_CODES.contains(&code) => {
            Err(MediaError::Failed { code, detail })
        }
        Err(MediaError::Cancelled) => Err(MediaError::Cancelled),
        Err(other) => {
            params["fallback"] = json!(cpu.name());
            params["cause"] = json!("fallback_failed");
            Err(MediaError::Reported {
                code: "FFMPEG_FAILED",
                params,
                detail: format!("{detail}; CPU fallback {}: {other}", cpu.name()),
            })
        }
        ok => ok,
    }
}

/// What `media_probe` writes: the probe reduced to what explains a plan.
fn probe_summary(
    item: &Item,
    content_id: &str,
    info: Option<&MediaInfo>,
    moov_first: Option<bool>,
) -> String {
    serde_json::json!({
        "send_mode": item.send_mode.as_str(),
        "source_size": item.source_size,
        "content_id": content_id,
        "format": info.and_then(|i| i.format_name.as_deref()),
        "duration_s": info.and_then(|i| i.duration_s),
        "bit_rate": info.and_then(|i| i.bit_rate),
        "video": info.and_then(|i| i.video.as_ref()).map(|v| serde_json::json!({
            "codec": v.codec, "profile": v.profile, "width": v.width, "height": v.height,
            "sar": v.sar, "pix_fmt": v.pix_fmt, "field_order": v.field_order,
            "color_transfer": v.color_transfer, "rotation": v.rotation, "hdr": v.hdr,
            "bit_rate": v.bit_rate,
        })),
        "audio": info.and_then(|i| i.audio.as_ref()).map(|a| serde_json::json!({
            "codec": a.codec, "channels": a.channels, "bit_rate": a.bit_rate,
        })),
        "moov_first": moov_first,
    })
    .to_string()
}

fn missing(error: crate::sidecars::SidecarError) -> MediaError {
    MediaError::failed(error.code(), error.to_string())
}

/// `[{ encoder, outcome, reason? }]` of the test encodes of `codec`.
fn tested_json(tested: &[(Encoder, detect::Outcome)], codec: VideoCodec) -> serde_json::Value {
    tested
        .iter()
        .map(|(encoder, outcome)| {
            let (outcome, reason) = match outcome {
                detect::Outcome::Works => ("works", None),
                detect::Outcome::NotInBuild => ("not_in_build", None),
                detect::Outcome::Failed(reason) => ("failed", Some(reason.as_str())),
            };
            let name = encoder.codec_name(codec).unwrap_or_else(|| encoder.name());
            serde_json::json!({ "encoder": name, "outcome": outcome, "reason": reason })
        })
        .collect()
}

/// `{ ffmpeg, tested: [...], hevc: [...] }`; `hevc` only once something asked for it.
fn detection_json(ffmpeg: &std::path::Path, detection: &Detection) -> serde_json::Value {
    let mut report = serde_json::json!({
        "ffmpeg": ffmpeg.to_string_lossy(),
        "tested": tested_json(&detection.tested, VideoCodec::H264),
    });
    if let Some(hevc) = &detection.hevc {
        report["hevc"] = tested_json(hevc, VideoCodec::Hevc);
    }
    report
}

impl FfmpegStage {
    pub fn new(
        sidecars: Arc<SidecarService>,
        settings: Arc<SettingsStore>,
        limits: Arc<LimitResolver>,
        entitlements: Arc<dyn Entitlements>,
    ) -> Self {
        Self {
            sidecars,
            settings,
            limits,
            entitlements,
            detection: Mutex::new(None),
            estimate_probe: std::sync::Mutex::new(None),
            gpu: GpuSession::default(),
        }
    }

    /// The GPU encoder that stopped mid-encode in this session, if any (`encoder_status`).
    pub fn gpu_failure(&self) -> Option<GpuFailure> {
        self.gpu.failure()
    }

    fn temp_base(&self) -> PathBuf {
        temp_root(&self.settings.get().temp)
    }

    /// Where `item`'s own encode is written. Not the only folder its output can be in: an item
    /// that reuses another item's encoded file (`Store::reusable_output`) names a path under
    /// that item's folder, so a cleanup must look at every row naming a path, not at ids.
    fn item_dir(&self, item: &Item) -> PathBuf {
        self.temp_base().join(item.id.to_string())
    }

    /// H.264 detection result for this ffmpeg, computed once per path (about 1 s).
    async fn detection(
        &self,
        resolved: &Resolved,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Detection> {
        self.detection_for(resolved, cancel, VideoCodec::H264).await
    }

    /// The detection, with the HEVC encoders tested too when `codec` is HEVC (once per path).
    async fn detection_for(
        &self,
        resolved: &Resolved,
        cancel: &tokio_util::sync::CancellationToken,
        codec: VideoCodec,
    ) -> Result<Detection> {
        let mut cache = self.detection.lock().await;
        let tools = Tools {
            ffmpeg: &resolved.ffmpeg,
            ffprobe: &resolved.ffprobe,
        };
        let mut detection = match cache.as_ref() {
            Some(cached) if cached.ffmpeg == resolved.ffmpeg => cached.detection.clone(),
            _ => {
                let started = std::time::Instant::now();
                let detection =
                    detect::detect(&tools, &self.temp_base().join("detect"), cancel).await?;
                let report = detection_json(&resolved.ffmpeg, &detection).to_string();
                tracing::info!(
                    event = "encoder_detection",
                    dur_ms = crate::logging::ms_since(started),
                    gpu = detection.gpu().map(Encoder::name),
                    cpu = detection.cpu().map(Encoder::name),
                    json = report.as_str(),
                    "encoder detection finished"
                );
                detection
            }
        };
        if codec == VideoCodec::Hevc && detection.hevc.is_none() {
            let started = std::time::Instant::now();
            let tested =
                detect::detect_hevc(&tools, &self.temp_base().join("detect"), cancel).await?;
            let report = tested_json(&tested, VideoCodec::Hevc).to_string();
            detection.hevc = Some(tested);
            tracing::info!(
                event = "hevc_detection",
                dur_ms = crate::logging::ms_since(started),
                encoder = detection
                    .hevc_gpu()
                    .and_then(|e| e.codec_name(VideoCodec::Hevc)),
                json = report.as_str(),
                "HEVC encoder detection finished"
            );
        }
        *cache = Some(Cached {
            ffmpeg: resolved.ffmpeg.clone(),
            detection: detection.clone(),
        });
        Ok(detection)
    }

    async fn forget_detection(&self) {
        *self.detection.lock().await = None;
    }

    /// Last encoder detection of this session (`debug_export`), `None` if none ran yet.
    pub async fn detection_report(&self) -> Option<serde_json::Value> {
        let cache = self.detection.lock().await;
        cache
            .as_ref()
            .map(|cached| detection_json(&cached.ffmpeg, &cached.detection))
    }

    /// What the options panel of a queued item shows: the size its output would have under
    /// `file` (or the settings), from a real ffprobe of the source and the same plan the
    /// encode will build. Nothing is written and no encode starts.
    pub async fn estimate(
        &self,
        item: &Item,
        file: Option<&EncodingOverride>,
    ) -> Result<EncodeEstimate> {
        let settings = self.settings.get();
        let limit_bytes = self.limits.current().limit_bytes;
        let (choice, _) = encoding_choice(&settings, self.entitlements.as_ref(), file);
        let mut estimate = EncodeEstimate {
            outcome: "as_is",
            bytes: u64::try_from(item.source_size).ok(),
            limit_bytes,
            codec: choice.video_codec,
            codec_fallback: false,
        };
        if item.send_mode == SendMode::Document {
            return Ok(estimate);
        }
        let source = PathBuf::from(&item.source_path);
        let resolved = self.sidecars.resolve().map_err(missing)?;
        let info = self.estimate_probe(&resolved, item, &source).await?;
        let input = plan_input(&source, item, limit_bytes, &settings, &choice, None);
        let plan = match plan::build_plan(Some(&info), &input) {
            Ok(plan) => plan,
            Err(plan::PlanError::TooLarge) => {
                estimate.outcome = "too_small";
                estimate.bytes = None;
                return Ok(estimate);
            }
            Err(plan::PlanError::Unreadable) => {
                estimate.outcome = "unknown";
                estimate.bytes = None;
                return Ok(estimate);
            }
        };
        match plan.kind {
            Kind::Encode => {
                estimate.outcome = "encode";
                estimate.bytes = plan.estimated_bytes();
                if plan.video_codec == VideoCodec::Hevc {
                    let detection = self
                        .detection_for(&resolved, &CancellationToken::new(), VideoCodec::Hevc)
                        .await?;
                    if detection.hevc_gpu().is_none() {
                        estimate.codec = VideoCodec::H264;
                        estimate.codec_fallback = true;
                    }
                }
            }
            Kind::Photo => {
                estimate.outcome = "photo";
                estimate.bytes = None;
            }
            Kind::Remux | Kind::RemuxAudio | Kind::Skip | Kind::Strip => {}
        }
        Ok(estimate)
    }

    /// ffprobe of `source`, kept for the next estimate of the same file.
    async fn estimate_probe(
        &self,
        resolved: &Resolved,
        item: &Item,
        source: &Path,
    ) -> Result<MediaInfo> {
        let key = (source.to_path_buf(), item.source_size, item.source_mtime);
        let cached = {
            let guard = self
                .estimate_probe
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .as_ref()
                .filter(|(path, size, mtime, _)| (path, size, mtime) == (&key.0, &key.1, &key.2))
                .map(|(_, _, _, info)| info.clone())
        };
        if let Some(info) = cached {
            return Ok(info);
        }
        let info =
            probe::probe(&resolved.ffprobe, source, false, &CancellationToken::new()).await?;
        *self
            .estimate_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((key.0, key.1, key.2, info.clone()));
        Ok(info)
    }

    /// Whether HEVC can be encoded here, from the HEVC test encodes (run once per ffmpeg).
    pub async fn codec_support(&self) -> CodecSupport {
        let unavailable = |reason| CodecSupport {
            hevc: false,
            hevc_encoder: None,
            reason: Some(reason),
        };
        let Ok(resolved) = self.sidecars.resolve() else {
            return unavailable("ffmpeg_unavailable");
        };
        match self
            .detection_for(&resolved, &CancellationToken::new(), VideoCodec::Hevc)
            .await
        {
            Ok(detection) => match detection.hevc_gpu() {
                Some(encoder) => CodecSupport {
                    hevc: true,
                    hevc_encoder: encoder.codec_name(VideoCodec::Hevc),
                    reason: None,
                },
                None => unavailable("no_hevc_encoder"),
            },
            Err(_) => unavailable("ffmpeg_unavailable"),
        }
    }

    /// What the badge of a queued item needs: ffprobe (unless an earlier look already has it)
    /// and the plan under the current settings. No hash, no encoder detection, no file written.
    /// `None` when the file is not the one that was queued.
    async fn analyze_inner(
        &self,
        item: &Item,
        cancel: &CancellationToken,
    ) -> Result<Option<Analysis>> {
        let source = PathBuf::from(&item.source_path);
        let stamp = intake::file_stamp(&source);
        if stamp != Some((item.source_size, item.source_mtime)) {
            return Ok(None);
        }
        let info = if item.send_mode == SendMode::Document {
            None
        } else if let Some(info) = reusable_info(item, stamp) {
            Some(info)
        } else {
            let resolved = self.sidecars.resolve().map_err(missing)?;
            let scan = !plan::is_image(&source);
            Some(probe::probe_in_background(&resolved.ffprobe, &source, scan, cancel).await?)
        };
        let settings = self.settings.get();
        let (choice, _) = encoding_choice(
            &settings,
            self.entitlements.as_ref(),
            item.encoding_override.as_ref(),
        );
        let input = plan_input(
            &source,
            item,
            self.limits.current().limit_bytes,
            &settings,
            &choice,
            moov_first_of(&source).await,
        );
        // A file that cannot be planned (too large, unreadable) keeps what ffprobe found; the
        // probing fails it with the right code when its turn comes.
        let plan_json = plan::build_plan(info.as_ref(), &input)
            .ok()
            .map(|plan| with_privacy(&settings, plan, &source, item, info.as_ref()))
            .and_then(|plan| serde_json::to_string(&plan).ok());
        Ok(Some(Analysis {
            media_json: info.and_then(|i| serde_json::to_string(&i).ok()),
            plan_json,
        }))
    }

    async fn probe_inner(&self, item: &Item, ctx: &StageContext) -> Result<ProbeOutcome> {
        let started = std::time::Instant::now();
        let settings = self.settings.get();
        // `limit_resolved`: the limit this item is planned for, and sent under (the plan
        // carries it to the upload).
        let limit = self.limits.resolve_logged();
        let source = PathBuf::from(&item.source_path);
        std::fs::File::open(&source)
            .map_err(|e| MediaError::failed("FILE_UNREADABLE", e.to_string()))?;
        let document = item.send_mode == SendMode::Document;
        let resolved = if document {
            None
        } else {
            Some(self.sidecars.resolve().map_err(missing)?)
        };
        if let Some(resolved) = &resolved {
            tracing::debug!(
                event = "sidecar_resolve",
                source = resolved.source.as_str(),
                ffmpeg = %resolved.ffmpeg.display(),
                "ffmpeg resolved"
            );
        }

        // Identity and ffprobe run together; the hash is the slow one on a big file.
        let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let hash_fraction = done.clone();
        let hashing = hash::content_id(source.clone(), ctx.cancel.clone(), move |fraction| {
            hash_fraction.store(fraction.to_bits(), std::sync::atomic::Ordering::Relaxed);
        });
        // The analysis at drop already ran ffprobe on this file: not again if it is unchanged.
        let reused = reusable_info(item, intake::file_stamp(&source));
        let info_reused = reused.is_some();
        let probing = async {
            match (&resolved, reused) {
                (None, _) => Ok(None),
                (Some(_), Some(info)) => Ok(Some(info)),
                (Some(r), None) => {
                    let scan = !plan::is_image(&source);
                    probe::probe(&r.ffprobe, &source, scan, &ctx.cancel)
                        .await
                        .map(Some)
                }
            }
        };
        let work = async { tokio::join!(hashing, probing) };
        tokio::pin!(work);
        let (content_id, info): (String, Option<MediaInfo>) = loop {
            tokio::select! {
                (hash, info) = &mut work => break (hash?, info?),
                _ = tokio::time::sleep(PROGRESS_TICK) => {
                    let fraction = f64::from_bits(done.load(std::sync::atomic::Ordering::Relaxed));
                    ctx.progress.report(Progress { percent: fraction * 100.0, ..Default::default() });
                }
            }
        };

        let moov_first = moov_first_of(&source).await;

        let (choice, advanced) = encoding_choice(
            &settings,
            self.entitlements.as_ref(),
            item.encoding_override.as_ref(),
        );
        let input = plan_input(
            &source,
            item,
            limit.limit_bytes,
            &settings,
            &choice,
            moov_first,
        );
        let summary = probe_summary(item, &content_id, info.as_ref(), moov_first);
        tracing::info!(
            event = "media_probe",
            dur_ms = crate::logging::ms_since(started),
            info_reused,
            json = summary.as_str(),
            "probe and identity done"
        );
        let plan = plan::build_plan(info.as_ref(), &input)
            .map_err(|e| MediaError::failed(e.code(), e.to_string()))?;
        let mut plan = with_privacy(&settings, plan, &source, item, info.as_ref());

        if plan.kind == Kind::Encode
            && let Some(resolved) = &resolved
        {
            let detection = self
                .detection_for(resolved, &ctx.cancel, plan.video_codec)
                .await?;
            let prefer_cpu = settings.encoding.prefer == "cpu";
            let gpu_failed = self.gpu.failure().is_some();
            if plan.video_codec == VideoCodec::Hevc && detection.hevc_gpu().is_none() {
                // Chosen while a GPU encoder worked, or in a file written by hand: the item
                // still goes out, as H.264 (the bitrate formula does not depend on the codec).
                tracing::warn!(
                    event = "codec_fallback",
                    from = "hevc",
                    to = "h264",
                    reason = "no_hevc_encoder",
                    "no HEVC encoder works: encoding as H.264"
                );
                ctx.progress.alert(
                    "CODEC_FALLBACK",
                    json!({ "from": "hevc", "to": "h264", "cause": "no_hevc_encoder" }),
                );
                plan.video_codec = VideoCodec::H264;
            }
            let codec = plan.video_codec;
            let encoder =
                pick_encoder(&detection, codec, prefer_cpu, gpu_failed).ok_or_else(|| {
                    MediaError::failed("FFMPEG_ENCODER_UNAVAILABLE", "no encoder passed its test")
                })?;
            let name = encoder.codec_name(codec).unwrap_or_else(|| encoder.name());
            tracing::info!(
                event = "encoder_chosen",
                encoder = name,
                gpu = encoder.is_gpu(),
                codec = codec.as_str(),
                preset = plan.preset.as_str(),
                prefer = settings.encoding.prefer.as_str(),
                gpu_skipped =
                    codec == VideoCodec::H264 && gpu_failed && !prefer_cpu && !encoder.is_gpu(),
                "encoder chosen"
            );
            plan.encoder = Some(name.to_owned());
        }
        let plan_json = serde_json::json!({ "plan": plan }).to_string();
        tracing::info!(
            event = "media_plan",
            margin_percent = settings.limit.margin_percent,
            advanced,
            file_override = advanced && item.encoding_override.is_some(),
            codec = plan.video_codec.as_str(),
            preset = plan.preset.as_str(),
            target_bytes = plan.target_bytes,
            json = plan_json.as_str(),
            "plan built"
        );

        let treatment = match plan.kind {
            Kind::Encode | Kind::Photo => Treatment::Encode,
            // A quick copy of the file, as a remux is: `media.remux` runs it (`run_plan`).
            Kind::Remux | Kind::RemuxAudio | Kind::Strip => Treatment::Remux,
            Kind::Skip => Treatment::Skip,
        };
        let temp_path = (treatment != Treatment::Skip).then(|| {
            self.item_dir(item)
                .join(format!("{}.part", plan.output_name))
                .to_string_lossy()
                .into_owned()
        });
        Ok(ProbeOutcome {
            content_id,
            identity_weak: false,
            media_json: info.and_then(|i| serde_json::to_string(&i).ok()),
            plan_json: serde_json::to_string(&plan).ok(),
            treatment,
            temp_path,
        })
    }

    fn plan_of(item: &Item) -> Result<Plan> {
        let json = item
            .plan_json
            .as_deref()
            .ok_or_else(|| MediaError::failed("FFMPEG_FAILED", "item has no plan"))?;
        serde_json::from_str(json)
            .map_err(|e| MediaError::failed("FFMPEG_FAILED", format!("bad plan: {e}")))
    }

    async fn run_plan(&self, item: &Item, ctx: &StageContext) -> Result<MediaOutput> {
        let started = std::time::Instant::now();
        let plan = Self::plan_of(item)?;
        let resolved = self.sidecars.resolve().map_err(missing)?;
        let tools = Tools {
            ffmpeg: &resolved.ffmpeg,
            ffprobe: &resolved.ffprobe,
        };
        let out_dir = self.item_dir(item);
        let settings = self.settings.get();
        let input = PathBuf::from(&item.source_path);
        let job = Job {
            tools: &tools,
            input: &input,
            plan: &plan,
            out_dir: &out_dir,
            min_free_bytes: settings.temp.min_free_gb.saturating_mul(1 << 30),
            cancel: &ctx.cancel,
            progress: &ctx.progress,
        };
        let produced = match plan.kind {
            Kind::Encode => {
                let margin = settings.limit.margin_percent / 100.0;
                self.encode_with_fallback(&job, &resolved, &settings.encoding.prefer, margin)
                    .await?
            }
            Kind::Photo => encode::photo(&job).await?,
            Kind::Remux | Kind::RemuxAudio => encode::remux(&job).await?,
            Kind::Strip => encode::strip(&job).await?,
            Kind::Skip => {
                return Err(MediaError::failed(
                    "FFMPEG_FAILED",
                    "nothing to run for a skipped item",
                ));
            }
        };
        tracing::info!(
            event = "media_done",
            dur_ms = crate::logging::ms_since(started),
            kind = plan.kind.as_str(),
            output_size = produced.size,
            limit_bytes = plan.limit_bytes,
            codec = plan.video_codec.as_str(),
            source_size = item.source_size,
            output_path = %produced.path.display(),
            "media output ready"
        );
        Ok(MediaOutput {
            output_path: produced.path.to_string_lossy().into_owned(),
            output_size: i64::try_from(produced.size).unwrap_or(i64::MAX),
        })
    }

    /// An encoder that cannot open (driver gone, GPU busy) triggers a new detection and one
    /// retry with the best other encoder. A GPU encoder that stops
    /// mid-encode is retried once on the CPU (`encode_with_cpu_fallback`).
    async fn encode_with_fallback(
        &self,
        job: &Job<'_>,
        resolved: &Resolved,
        prefer: &str,
        margin: f64,
    ) -> Result<encode::Produced> {
        let codec = job.plan.video_codec;
        let planned = job.plan.encoder.as_deref().and_then(Encoder::parse);
        let gpu_failed = self.gpu.failure().is_some();
        let mut encoder = match planned {
            Some(encoder) => encoder,
            None => pick_encoder(
                &self.detection_for(resolved, job.cancel, codec).await?,
                codec,
                prefer == "cpu",
                gpu_failed,
            )
            .ok_or_else(|| MediaError::failed("FFMPEG_ENCODER_UNAVAILABLE", "no encoder"))?,
        };
        // The CPU encoder of a fallback (detection cached since the probe). None for HEVC:
        // there is no CPU encoder of that codec, and an H.264 retry would change the file.
        let mut cpu = None;
        if encoder.is_gpu() && codec == VideoCodec::H264 {
            cpu = self.detection(resolved, job.cancel).await?.cpu();
            // Planned before the GPU failed: skipped as a new pick would.
            if gpu_failed && let Some(cpu_encoder) = cpu.take() {
                tracing::info!(
                    event = "encoder_chosen",
                    encoder = cpu_encoder.name(),
                    gpu = false,
                    gpu_skipped = true,
                    "GPU failed earlier this session: CPU encoder used"
                );
                encoder = cpu_encoder;
            }
        }
        match encode_with_cpu_fallback(job, encoder, cpu, margin, &self.gpu).await {
            Err(MediaError::Failed {
                code: "FFMPEG_ENCODER_UNAVAILABLE",
                detail,
            }) => {
                tracing::warn!(
                    event = "encoder_fallback",
                    error_code = "FFMPEG_ENCODER_UNAVAILABLE",
                    failed = encoder.codec_name(codec).unwrap_or_else(|| encoder.name()),
                    detail = detail.as_str(),
                    "encoder cannot open; detecting again"
                );
                self.forget_detection().await;
                let detection = self.detection_for(resolved, job.cancel, codec).await?;
                let other = detection.alternative_for(encoder, codec).ok_or_else(|| {
                    MediaError::failed("FFMPEG_ENCODER_UNAVAILABLE", "no other encoder works")
                })?;
                tracing::info!(
                    event = "encoder_chosen",
                    encoder = other.codec_name(codec).unwrap_or_else(|| other.name()),
                    gpu = other.is_gpu(),
                    fallback = true,
                    "fallback encoder chosen"
                );
                // Already the one retry: no CPU fallback after it.
                encode_with_cpu_fallback(job, other, None, margin, &self.gpu).await
            }
            other => other,
        }
    }
}

#[async_trait]
impl MediaStage for FfmpegStage {
    async fn analyze(
        &self,
        item: &Item,
        cancel: &CancellationToken,
    ) -> std::result::Result<Option<Analysis>, StageError> {
        self.analyze_inner(item, cancel).await.map_err(|error| {
            tracing::debug!(event = "analysis_error", error = %error, "analysis at drop failed");
            match error {
                MediaError::Cancelled => StageError::Cancelled,
                other => StageError::failed(other.code().unwrap_or("FFMPEG_FAILED")),
            }
        })
    }

    async fn probe(
        &self,
        item: &Item,
        ctx: &StageContext,
    ) -> std::result::Result<ProbeOutcome, StageError> {
        self.probe_inner(item, ctx).await.map_err(stage_error)
    }

    async fn encode(
        &self,
        item: &Item,
        ctx: &StageContext,
    ) -> std::result::Result<MediaOutput, StageError> {
        self.run_plan(item, ctx).await.map_err(stage_error)
    }

    async fn remux(
        &self,
        item: &Item,
        ctx: &StageContext,
    ) -> std::result::Result<MediaOutput, StageError> {
        self.run_plan(item, ctx).await.map_err(stage_error)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::queue::events::ProgressReporter;
    use crate::queue::events::recorder::Recorder;
    use crate::queue::test_util::TestDir;

    /// How the fake ffmpeg behaves.
    #[derive(Clone, Copy)]
    struct Fake {
        /// `-progress` blocks printed by `h264_nvenc` before it exits 1.
        gpu_blocks: u32,
        /// Printed on stderr by `h264_nvenc` (`None`: silent, as on the real machine).
        gpu_stderr: Option<&'static str>,
        /// `h264_nvenc` sleeps this long before exiting (cancellation test).
        gpu_wait_s: u32,
        /// Every CPU encoder exits 1 too.
        cpu_fails: bool,
        /// The GPU runs that fail (one counted in `gpuruns.txt` per run); the next ones succeed.
        gpu_fails: u32,
    }

    /// Stops silently 3 s into the output, every time: a failure to start, as seen on the real
    /// machine (`encode::is_start_failure`).
    const SILENT: Fake = Fake {
        gpu_blocks: 3,
        gpu_stderr: None,
        gpu_wait_s: 0,
        cpu_fails: false,
        gpu_fails: 1_000_000,
    };

    /// Stops silently 20 s into the output, every time: past the start-failure window.
    const LATE: Fake = Fake {
        gpu_blocks: 20,
        ..SILENT
    };

    /// `.cmd` stand-ins for ffmpeg and ffprobe (as in `sidecars::discover`). The ffmpeg one
    /// appends `<encoder> fresh|part_present` to `calls.txt` (whether the `.part` it is about
    /// to write already exists), writes its output and prints `-progress` blocks.
    fn fake_tools(dir: &Path, fake: Fake) -> (PathBuf, PathBuf) {
        let gpu_stderr = fake
            .gpu_stderr
            .map(|line| format!("echo {line} 1>&2\r\n"))
            .unwrap_or_default();
        let gpu_wait = if fake.gpu_wait_s > 0 {
            format!("ping -n {} 127.0.0.1 >nul\r\n", fake.gpu_wait_s + 1)
        } else {
            String::new()
        };
        let cpu_fail = if fake.cpu_fails { "exit /b 1\r\n" } else { "" };
        let lines = [
            "@echo off",
            "setlocal",
            "set \"HERE=%~dp0\"",
            "set \"ENC=\"",
            "set \"OUT=\"",
            ":args",
            "if \"%~1\"==\"\" goto run",
            "if \"%~1\"==\"-c:v\" set \"ENC=%~2\"",
            "set \"OUT=%~1\"",
            "shift",
            "goto args",
            ":run",
            "set \"STATE=fresh\"",
            "if /i not \"%OUT%\"==\"NUL\" if exist \"%OUT%\" set \"STATE=part_present\"",
            "echo %ENC% %STATE%>>\"%HERE%calls.txt\"",
            "if \"%ENC%\"==\"h264_nvenc\" goto gpu",
            "if \"%ENC%\"==\"hevc_nvenc\" goto gpu",
            cpu_fail.trim_end(),
            ":ok",
            "if /i not \"%OUT%\"==\"NUL\" echo encoded>\"%OUT%\"",
            "echo out_time_us=60000000",
            "echo progress=end",
            "exit /b 0",
            ":gpu",
            "echo run>>\"%HERE%gpuruns.txt\"",
            "set \"RUNS=0\"",
            "for /f \"usebackq\" %%c in (\"%HERE%gpuruns.txt\") do set /a RUNS+=1",
            &format!("if %RUNS% GTR {} goto ok", fake.gpu_fails),
            "echo partial>\"%OUT%\"",
            &format!("for /l %%i in (1,1,{}) do (", fake.gpu_blocks),
            "echo out_time_us=%%i000000",
            "echo speed=1.0x",
            "echo progress=continue",
            ")",
            gpu_stderr.trim_end(),
            gpu_wait.trim_end(),
            "exit /b 1",
        ];
        let ffmpeg = dir.join("ffmpeg.cmd");
        std::fs::write(&ffmpeg, lines.join("\r\n") + "\r\n").unwrap();
        let ffprobe = dir.join("ffprobe.cmd");
        std::fs::write(
            &ffprobe,
            "@echo off\r\necho {\"streams\":[],\"format\":{\"format_name\":\"mov,mp4\",\"duration\":\"60.000000\"}}\r\n",
        )
        .unwrap();
        (ffmpeg, ffprobe)
    }

    fn calls(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("calls.txt"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.trim().to_owned())
            .collect()
    }

    fn sample_plan() -> Plan {
        Plan {
            kind: Kind::Encode,
            limit_bytes: 100 * plan::MIB,
            duration_s: Some(60.0),
            video_bps: Some(1_000_000),
            audio_kbps: None,
            video_index: Some(0),
            audio_index: None,
            // No `=` in the arguments: cmd.exe splits batch parameters on it.
            vf: None,
            hdr: false,
            encoder: Some("h264_nvenc".into()),
            output_name: "clip_sq.mp4".into(),
            photo: None,
            source_dims: None,
            alpha: false,
            note: None,
            video_codec: VideoCodec::H264,
            preset: plan::EncodePreset::Balanced,
            target_bytes: None,
            stream_languages: Vec::new(),
        }
    }

    struct Run {
        result: Result<encode::Produced>,
        calls: Vec<String>,
        sink: Arc<Recorder>,
        session: GpuSession,
        out_dir: PathBuf,
        _dir: TestDir,
    }

    async fn run(
        name: &str,
        fake: Fake,
        cpu: Option<Encoder>,
        cancel_after: Option<Duration>,
    ) -> Run {
        run_plan(name, fake, sample_plan(), cpu, cancel_after).await
    }

    async fn run_plan(
        name: &str,
        fake: Fake,
        plan: Plan,
        cpu: Option<Encoder>,
        cancel_after: Option<Duration>,
    ) -> Run {
        run_in_session(name, fake, plan, cpu, cancel_after, GpuSession::default()).await
    }

    /// The next item of a session: the GPU's memory comes from the previous runs.
    async fn run_in(
        name: &str,
        fake: Fake,
        plan: Plan,
        cpu: Option<Encoder>,
        session: GpuSession,
    ) -> Run {
        run_in_session(name, fake, plan, cpu, None, session).await
    }

    async fn run_in_session(
        name: &str,
        fake: Fake,
        plan: Plan,
        cpu: Option<Encoder>,
        cancel_after: Option<Duration>,
        session: GpuSession,
    ) -> Run {
        let dir = TestDir::new(name);
        let (ffmpeg, ffprobe) = fake_tools(dir.path(), fake);
        let input = dir.path().join("in.mkv");
        std::fs::write(&input, b"source").unwrap();
        let out_dir = dir.path().join("out");
        let tools = Tools {
            ffmpeg: &ffmpeg,
            ffprobe: &ffprobe,
        };
        let cancel = CancellationToken::new();
        let sink = Arc::new(Recorder::default());
        let progress = ProgressReporter::new(1, Phase::Encoding, sink.clone());
        let job = Job {
            tools: &tools,
            input: &input,
            plan: &plan,
            out_dir: &out_dir,
            min_free_bytes: 0,
            cancel: &cancel,
            progress: &progress,
        };
        if let Some(delay) = cancel_after {
            let canceller = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                canceller.cancel();
            });
        }
        let result = encode_with_cpu_fallback(&job, Encoder::Nvenc, cpu, 0.03, &session).await;
        Run {
            result,
            calls: calls(dir.path()),
            sink,
            session,
            out_dir,
            _dir: dir,
        }
    }

    fn no_part_left(out_dir: &Path) {
        assert!(!out_dir.join("clip_sq.mp4.part").exists(), ".part left");
    }

    #[tokio::test]
    async fn a_late_silent_gpu_stop_is_retried_once_on_the_cpu() {
        let run = run("fallback-ok", LATE, Some(Encoder::X264), None).await;
        let produced = run.result.expect("the CPU retry succeeds");
        assert_eq!(produced.path, run.out_dir.join("clip_sq.mp4"));
        assert!(produced.path.is_file());
        no_part_left(&run.out_dir);
        // GPU once, then x264's two passes on a fresh `.part` (the GPU one was deleted).
        assert_eq!(
            run.calls,
            ["h264_nvenc fresh", "libx264 fresh", "libx264 fresh"]
        );
        let failure = run.session.failure().expect("remembered for the session");
        assert_eq!(failure.encoder, "h264_nvenc");

        let alerts = run.sink.alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].code, "ENCODER_FALLBACK");
        assert_eq!(alerts[0].item_id, Some(1));
        assert_eq!(alerts[0].params["from"], "h264_nvenc");
        assert_eq!(alerts[0].params["to"], "libx264");
        assert_eq!(alerts[0].params["cause"], "silent");
        assert_eq!(alerts[0].params["atSeconds"], failure.at_seconds);

        let progress = run.sink.progress.lock().unwrap();
        let fallback = progress
            .iter()
            .find(|p| p.detail.as_deref() == Some("cpu_fallback"))
            .expect("a cpu_fallback progress event");
        assert_eq!((fallback.phase, fallback.percent), (Phase::Encoding, 0.0));
        assert_eq!(progress.last().unwrap().percent, 100.0);
    }

    #[tokio::test]
    async fn a_failing_fallback_is_final_and_names_both_encoders() {
        let fake = Fake {
            cpu_fails: true,
            ..LATE
        };
        let run = run("fallback-fails", fake, Some(Encoder::OpenH264), None).await;
        let Err(MediaError::Reported { code, params, .. }) = run.result else {
            panic!("expected a reported failure, got {:?}", run.result);
        };
        assert_eq!(code, "FFMPEG_FAILED");
        assert_eq!(params["encoder"], "h264_nvenc");
        assert_eq!(params["exitCode"], 1);
        assert_eq!(params["fallback"], "libopenh264");
        assert_eq!(params["cause"], "fallback_failed");
        // No loop: one GPU run, one CPU run, nothing after.
        assert_eq!(run.calls, ["h264_nvenc fresh", "libopenh264 fresh"]);
        no_part_left(&run.out_dir);
        assert_eq!(
            run.sink.alert_codes(),
            [(Some(1), "ENCODER_FALLBACK".to_owned())]
        );
    }

    #[tokio::test]
    async fn cancelling_the_gpu_encode_never_falls_back() {
        let fake = Fake {
            gpu_wait_s: 5,
            ..SILENT
        };
        let started = std::time::Instant::now();
        let run = run(
            "fallback-cancel",
            fake,
            Some(Encoder::X264),
            Some(Duration::from_millis(400)),
        )
        .await;
        assert!(
            matches!(run.result, Err(MediaError::Cancelled)),
            "{:?}",
            run.result
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(run.calls, ["h264_nvenc fresh"]);
        assert_eq!(run.session.failure(), None, "a cancel is not a GPU failure");
        assert!(run.sink.alerts.lock().unwrap().is_empty());
        no_part_left(&run.out_dir);
    }

    #[tokio::test]
    async fn a_gpu_that_fails_to_start_once_succeeds_on_the_retry_without_any_alert() {
        let fake = Fake {
            gpu_fails: 1,
            ..SILENT
        };
        let run = run("retry-ok", fake, Some(Encoder::X264), None).await;
        let produced = run.result.expect("the second GPU run succeeds");
        assert_eq!(produced.path, run.out_dir.join("clip_sq.mp4"));
        // Twice the GPU, each time on a fresh `.part` (the failed run's was deleted), no CPU.
        assert_eq!(run.calls, ["h264_nvenc fresh", "h264_nvenc fresh"]);
        assert!(
            run.sink.alerts.lock().unwrap().is_empty(),
            "nothing to tell"
        );
        assert_eq!(run.session.failure(), None);
        no_part_left(&run.out_dir);
        // The bar went back to the start of the run for the new one.
        let progress = run.sink.progress.lock().unwrap();
        assert!(progress.iter().any(|p| p.percent == 0.0));
        assert_eq!(progress.last().unwrap().percent, 100.0);
    }

    #[tokio::test]
    async fn the_second_retry_can_be_the_one_that_works() {
        let fake = Fake {
            gpu_fails: 2,
            ..SILENT
        };
        let run = run("retry-ok-2", fake, Some(Encoder::X264), None).await;
        assert!(run.result.is_ok(), "{:?}", run.result);
        assert_eq!(run.calls, ["h264_nvenc fresh"; 3]);
        assert!(run.sink.alerts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn three_gpu_failures_to_start_fall_back_to_the_cpu_with_the_alert() {
        let run = run("retry-exhausted", SILENT, Some(Encoder::X264), None).await;
        assert!(run.result.is_ok(), "{:?}", run.result);
        // The same GPU command three times, then libx264's two passes.
        assert_eq!(
            run.calls,
            [
                "h264_nvenc fresh",
                "h264_nvenc fresh",
                "h264_nvenc fresh",
                "libx264 fresh",
                "libx264 fresh"
            ]
        );
        assert_eq!(
            run.sink.alert_codes(),
            [(Some(1), "ENCODER_FALLBACK".to_owned())]
        );
        no_part_left(&run.out_dir);
    }

    #[tokio::test]
    async fn a_late_gpu_failure_is_never_retried_on_the_gpu() {
        let run = run("retry-late", LATE, Some(Encoder::X264), None).await;
        assert!(run.result.is_ok(), "{:?}", run.result);
        assert_eq!(
            run.calls,
            ["h264_nvenc fresh", "libx264 fresh", "libx264 fresh"],
            "one GPU run, then the CPU"
        );
        assert_eq!(run.sink.alert_codes().len(), 1);
    }

    #[tokio::test]
    async fn an_early_failure_with_an_error_message_is_not_retried() {
        let fake = Fake {
            gpu_stderr: Some("Failed locking bitstream buffer: NV_ENC_ERR_GENERIC (20)"),
            ..SILENT
        };
        let run = run("retry-stderr", fake, Some(Encoder::X264), None).await;
        assert!(run.result.is_ok(), "{:?}", run.result);
        assert_eq!(
            run.calls,
            ["h264_nvenc fresh", "libx264 fresh", "libx264 fresh"]
        );
    }

    #[tokio::test]
    async fn hevc_is_retried_on_the_gpu_too() {
        let mut hevc = sample_plan();
        hevc.video_codec = VideoCodec::Hevc;
        hevc.encoder = Some("hevc_nvenc".into());
        let fake = Fake {
            gpu_fails: 2,
            ..SILENT
        };
        let run = run_plan("retry-hevc", fake, hevc, None, None).await;
        assert!(run.result.is_ok(), "{:?}", run.result);
        assert_eq!(run.calls, ["hevc_nvenc fresh"; 3]);
        assert!(run.sink.alerts.lock().unwrap().is_empty());
    }

    /// Cancels the job when the retry puts the progress bar back to 0 (just before its wait).
    struct CancelOnRestart(CancellationToken);

    impl crate::queue::events::EventSink for CancelOnRestart {
        fn queue_changed(&self, _: &crate::queue::events::QueueChanged) {}

        fn queue_progress(&self, event: &crate::queue::events::QueueProgress) {
            if event.percent == 0.0 {
                self.0.cancel();
            }
        }
    }

    #[tokio::test]
    async fn cancelling_while_the_retry_waits_ends_the_item_without_a_new_run() {
        let dir = TestDir::new("retry-cancel");
        let (ffmpeg, ffprobe) = fake_tools(dir.path(), SILENT);
        let input = dir.path().join("in.mkv");
        std::fs::write(&input, b"source").unwrap();
        let out_dir = dir.path().join("out");
        let tools = Tools {
            ffmpeg: &ffmpeg,
            ffprobe: &ffprobe,
        };
        let cancel = CancellationToken::new();
        let progress = ProgressReporter::new(
            1,
            Phase::Encoding,
            Arc::new(CancelOnRestart(cancel.clone())),
        );
        let plan = sample_plan();
        let job = Job {
            tools: &tools,
            input: &input,
            plan: &plan,
            out_dir: &out_dir,
            min_free_bytes: 0,
            cancel: &cancel,
            progress: &progress,
        };
        let session = GpuSession::default();
        let result =
            encode_with_cpu_fallback(&job, Encoder::Nvenc, Some(Encoder::X264), 0.03, &session)
                .await;
        assert!(matches!(result, Err(MediaError::Cancelled)), "{result:?}");
        assert_eq!(calls(dir.path()), ["h264_nvenc fresh"], "no second run");
        assert_eq!(session.failure(), None, "a cancel is not a GPU failure");
        no_part_left(&out_dir);
    }

    #[tokio::test]
    async fn every_retry_is_in_the_journal() {
        let dir = TestDir::new("retry-journal");
        let journal = crate::logging::testing::TestJournal::new(&dir.path().join("logs"), "info");
        let fake = Fake {
            gpu_fails: 2,
            ..SILENT
        };
        let run = run("retry-journal-run", fake, Some(Encoder::X264), None).await;
        assert!(run.result.is_ok(), "{:?}", run.result);
        let lines = journal.lines();
        let retries: Vec<_> = lines.iter().filter(|l| l["event"] == "gpu_retry").collect();
        assert_eq!(retries.len(), 2);
        for (line, attempt) in retries.iter().zip([2, 3]) {
            assert_eq!(line["fields"]["attempt"], attempt);
            assert_eq!(line["fields"]["max_attempts"], 3);
            assert_eq!(line["fields"]["exit_code"], 1);
            assert_eq!(line["fields"]["encoder"], "h264_nvenc");
            assert_eq!(line["fields"]["out_time_s"], 3.0);
        }
        assert_eq!(
            lines
                .iter()
                .filter(|l| l["event"] == "gpu_retry_ok")
                .count(),
            1
        );
        assert!(lines.iter().all(|l| l["event"] != "encoder_failed_midway"));
    }

    /// The alert of the one run `run` made, by its `cause`.
    fn alert_cause(run: &Run) -> String {
        let alerts = run.sink.alerts.lock().unwrap();
        assert_eq!(alerts.len(), 1, "one alert expected");
        assert_eq!(alerts[0].code, "ENCODER_FALLBACK");
        alerts[0].params["cause"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn one_item_that_fails_to_start_does_not_turn_the_gpu_off_for_the_session() {
        let run = run("session-one", SILENT, Some(Encoder::X264), None).await;
        assert!(run.result.is_ok(), "{:?}", run.result);
        assert_eq!(alert_cause(&run), "start_failed");
        assert_eq!(run.session.failure(), None, "the next file tries the GPU");
    }

    #[tokio::test]
    async fn a_second_item_in_a_row_that_fails_to_start_turns_the_gpu_off() {
        let first = run("session-two-a", SILENT, Some(Encoder::X264), None).await;
        assert_eq!(alert_cause(&first), "start_failed");

        let second = run_in(
            "session-two-b",
            SILENT,
            sample_plan(),
            Some(Encoder::X264),
            first.session,
        )
        .await;
        assert!(second.result.is_ok(), "{:?}", second.result);
        assert_eq!(alert_cause(&second), "start_failed_again");
        let failure = second.session.failure().expect("the GPU is off");
        assert_eq!(failure.encoder, "h264_nvenc");
        // Both items paid their three GPU tries.
        assert_eq!(
            second
                .calls
                .iter()
                .filter(|c| c.starts_with("h264_nvenc"))
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn a_gpu_encode_that_works_between_two_start_failures_resets_the_count() {
        let first = run("session-reset-a", SILENT, Some(Encoder::X264), None).await;
        assert_eq!(alert_cause(&first), "start_failed");

        let works = Fake {
            gpu_fails: 0,
            ..SILENT
        };
        let second = run_in(
            "session-reset-b",
            works,
            sample_plan(),
            Some(Encoder::X264),
            first.session,
        )
        .await;
        assert!(second.result.is_ok(), "{:?}", second.result);
        assert_eq!(second.calls, ["h264_nvenc fresh"]);

        let third = run_in(
            "session-reset-c",
            SILENT,
            sample_plan(),
            Some(Encoder::X264),
            second.session,
        )
        .await;
        assert_eq!(
            alert_cause(&third),
            "start_failed",
            "the count started over"
        );
        assert_eq!(third.session.failure(), None);
    }

    #[tokio::test]
    async fn a_late_or_reported_gpu_failure_turns_the_gpu_off_at_once() {
        let late = run("session-late", LATE, Some(Encoder::X264), None).await;
        assert_eq!(alert_cause(&late), "silent");
        assert!(late.session.failure().is_some());

        let fake = Fake {
            gpu_stderr: Some("Failed locking bitstream buffer: NV_ENC_ERR_GENERIC (20)"),
            ..SILENT
        };
        let reported = run("session-reported", fake, Some(Encoder::X264), None).await;
        assert_eq!(alert_cause(&reported), "encoder_error");
        assert!(reported.session.failure().is_some());
    }

    #[tokio::test]
    async fn without_a_cpu_encoder_the_stop_is_reported_with_its_numbers() {
        let run = run("fallback-none", LATE, None, None).await;
        let Err(MediaError::Reported { code, params, .. }) = run.result else {
            panic!("expected a reported failure, got {:?}", run.result);
        };
        assert_eq!(code, "FFMPEG_FAILED");
        assert_eq!(params["cause"], "no_cpu_encoder");
        assert_eq!(params["encoder"], "h264_nvenc");
        assert_eq!(params["exitCode"], 1);
        assert!(params["atSeconds"].is_u64());
        assert_eq!(run.calls, ["h264_nvenc fresh"]);
        assert!(run.session.failure().is_some());
        no_part_left(&run.out_dir);
    }

    #[tokio::test]
    async fn a_hardware_encoder_error_falls_back_but_an_input_error_does_not() {
        let fake = Fake {
            gpu_stderr: Some("Failed locking bitstream buffer: NV_ENC_ERR_GENERIC (20)"),
            ..SILENT
        };
        let run_hw = run("fallback-hw", fake, Some(Encoder::X264), None).await;
        assert!(run_hw.result.is_ok(), "{:?}", run_hw.result);
        assert_eq!(
            run_hw.sink.alerts.lock().unwrap()[0].params["cause"],
            "encoder_error"
        );

        let fake = Fake {
            gpu_stderr: Some("in.mkv: Invalid data found when processing input"),
            ..SILENT
        };
        let run_input = run("fallback-input", fake, Some(Encoder::X264), None).await;
        assert!(
            matches!(
                run_input.result,
                Err(MediaError::Failed {
                    code: "FILE_UNREADABLE",
                    ..
                })
            ),
            "{:?}",
            run_input.result
        );
        assert_eq!(run_input.calls, ["h264_nvenc fresh"]);
        assert_eq!(run_input.session.failure(), None);
    }

    /// The CPU retry with a REAL libx264 (never the GPU): a wrapper fakes the silent NVENC stop
    /// and forwards every other run to the ffmpeg of `SQUEEZEGRAM_TEST_X264_FFMPEG_DIR` (a
    /// build with libx264). Run on demand:
    /// `cargo test --lib -- --ignored real_x264_takes_over`.
    #[tokio::test]
    #[ignore = "needs an ffmpeg with libx264 (SQUEEZEGRAM_TEST_X264_FFMPEG_DIR)"]
    async fn real_x264_takes_over_after_a_silent_gpu_stop() {
        let Some(bin) = std::env::var_os("SQUEEZEGRAM_TEST_X264_FFMPEG_DIR").map(PathBuf::from)
        else {
            return;
        };
        let (real_ffmpeg, real_ffprobe) = (bin.join("ffmpeg.exe"), bin.join("ffprobe.exe"));
        let dir = TestDir::new("fallback-real-x264");
        let input = dir.path().join("src.mkv");
        let generated = std::process::Command::new(&real_ffmpeg)
            .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=640x360:rate=30",
                "-t",
                "6",
            ])
            .args(["-vf", "noise=alls=20:allf=t", "-c:v", "mpeg4", "-q:v", "3"])
            .arg(&input)
            .status()
            .unwrap();
        assert!(generated.success());
        let wrapper = dir.path().join("ffmpeg.cmd");
        let lines = [
            "@echo off".to_owned(),
            "setlocal".to_owned(),
            ":args".to_owned(),
            "if \"%~1\"==\"\" goto run".to_owned(),
            "if \"%~1\"==\"h264_nvenc\" goto gpu".to_owned(),
            "shift".to_owned(),
            "goto args".to_owned(),
            ":run".to_owned(),
            format!("\"{}\" %*", real_ffmpeg.display()),
            "exit /b %errorlevel%".to_owned(),
            ":gpu".to_owned(),
            "echo out_time_us=2000000".to_owned(),
            "echo progress=continue".to_owned(),
            "exit /b 1".to_owned(),
        ];
        std::fs::write(&wrapper, lines.join("\r\n") + "\r\n").unwrap();

        let limit = 2 * plan::MIB;
        let mut plan = sample_plan();
        plan.limit_bytes = limit;
        plan.duration_s = Some(6.0);
        plan.video_bps = plan::plan_bitrate(limit, 0.03, 6.0, None);
        let out_dir = dir.path().join("out");
        let tools = Tools {
            ffmpeg: &wrapper,
            ffprobe: &real_ffprobe,
        };
        let cancel = CancellationToken::new();
        let sink = Arc::new(Recorder::default());
        let progress = ProgressReporter::new(1, Phase::Encoding, sink.clone());
        let job = Job {
            tools: &tools,
            input: &input,
            plan: &plan,
            out_dir: &out_dir,
            min_free_bytes: 0,
            cancel: &cancel,
            progress: &progress,
        };
        let session = GpuSession::default();
        let produced =
            encode_with_cpu_fallback(&job, Encoder::Nvenc, Some(Encoder::X264), 0.03, &session)
                .await
                .expect("libx264 takes over");
        let info = probe::probe(&real_ffprobe, &produced.path, false, &cancel)
            .await
            .unwrap();
        println!(
            "libx264 fallback: {} bytes = {:.2} % of the limit, {:?} s",
            produced.size,
            produced.size as f64 / limit as f64 * 100.0,
            info.duration_s
        );
        assert_eq!(info.video.unwrap().codec, "h264");
        assert!(produced.size <= limit);
        // The wrapper's NVENC stop is a failure to start: tried three times, GPU not given up.
        assert_eq!(session.failure(), None);
        assert_eq!(
            sink.alert_codes(),
            [(Some(1), "ENCODER_FALLBACK".to_owned())]
        );
        no_part_left(&out_dir);
    }

    #[test]
    fn a_gpu_that_failed_this_session_is_skipped_while_a_cpu_encoder_works() {
        let detection = Detection {
            hevc: None,
            tested: vec![
                (Encoder::Nvenc, detect::Outcome::Works),
                (Encoder::X264, detect::Outcome::NotInBuild),
                (Encoder::OpenH264, detect::Outcome::Works),
            ],
        };
        assert_eq!(
            pick_encoder(&detection, VideoCodec::H264, false, false),
            Some(Encoder::Nvenc)
        );
        assert_eq!(
            pick_encoder(&detection, VideoCodec::H264, true, false),
            Some(Encoder::OpenH264),
            "CPU first"
        );
        assert_eq!(
            pick_encoder(&detection, VideoCodec::H264, false, true),
            Some(Encoder::OpenH264)
        );
        let gpu_only = Detection {
            hevc: None,
            tested: vec![(Encoder::Nvenc, detect::Outcome::Works)],
        };
        assert_eq!(
            pick_encoder(&gpu_only, VideoCodec::H264, false, true),
            Some(Encoder::Nvenc)
        );
    }

    /// A 4K HEVC source with AAC audio, encoded under a 100 MiB limit, planned with what
    /// `encoding_choice` gives for `settings` and `entitlements`.
    fn plan_with(settings: &Settings, entitlements: &dyn Entitlements) -> (Plan, bool) {
        plan_with_file(settings, entitlements, None)
    }

    fn plan_with_file(
        settings: &Settings,
        entitlements: &dyn Entitlements,
        file: Option<&EncodingOverride>,
    ) -> (Plan, bool) {
        let info: MediaInfo = serde_json::from_value(json!({
            "format_name": "matroska,webm", "duration_s": 600.0, "bit_rate": null,
            "video": { "index": 0, "codec": "hevc", "profile": "Main", "width": 3840,
                "height": 2160, "sar": [1, 1], "pix_fmt": "yuv420p",
                "field_order": "progressive", "color_transfer": null, "rotation": 0,
                "bit_rate": null, "hdr": false },
            "audio": { "index": 1, "codec": "aac", "channels": 2, "bit_rate": null }
        }))
        .unwrap();
        let (choice, advanced) = encoding_choice(settings, entitlements, file);
        let source = Path::new("D:\\clips\\uhd.mkv");
        let input = PlanInput {
            source,
            source_size: 900 * plan::MIB,
            send_mode: SendMode::Video,
            limit_bytes: 100 * plan::MIB,
            margin: 0.03,
            audio_kbps: choice.audio_kbps,
            cap: choice.max_resolution,
            codec: choice.video_codec,
            preset: choice.preset,
            target_bytes: choice.target_mb.map(|mb| mb * plan::MIB),
            moov_first: None,
        };
        (plan::build_plan(Some(&info), &input).unwrap(), advanced)
    }

    #[test]
    fn advanced_settings_reach_the_plan_only_with_the_feature() {
        use crate::entitlements::license::test_vectors;
        use crate::entitlements::{FreeOnly, LicenseEntitlements};
        use crate::transport::TelegramTransport;
        use crate::transport::mock::MockTransport;

        let mut settings = Settings::default();
        settings.advanced_encoding = AdvancedEncoding {
            max_resolution: plan::ResolutionCap::P2160,
            audio_kbps: 192,
            ..AdvancedEncoding::AUTOMATIC
        };
        // Changed by hand in the file: never read.
        settings.encoding.audio_kbps = 320;

        let automatic = |(plan, advanced): (Plan, bool)| {
            assert!(!advanced);
            assert_eq!(plan.audio_kbps, Some(128));
            let vf = plan.vf.unwrap();
            assert!(vf.contains("min(1920,iw)") && !vf.contains("3840"), "{vf}");
        };
        automatic(plan_with(&settings, &FreeOnly));

        let dir = TestDir::new("stage-advanced");
        let vectors = test_vectors::load();
        let license_file = dir.path().join("license.txt");
        let account = MockTransport::authorized();
        let licensed = |token: &str, auth| {
            std::fs::write(&license_file, token).unwrap();
            LicenseEntitlements::load(license_file.clone(), vectors.key_ring(), auth)
        };
        let valid = &vectors.case("valid").token;

        // Granted: the saved cap and bitrate, and a video budget that pays for the audio.
        let granted = licensed(valid, Some(account.auth_state()));
        let (plan, advanced) = plan_with(&settings, &granted);
        assert!(advanced);
        assert_eq!(plan.audio_kbps, Some(192));
        assert!(plan.vf.as_deref().unwrap().contains("min(3840,iw)"));
        assert_eq!(
            plan.video_bps,
            plan::plan_bitrate(100 * plan::MIB, 0.03, 600.0, Some(192))
        );

        // A license for another account, or no account signed in: automatic values.
        let other = licensed(
            &vectors.case("other_account").token,
            Some(account.auth_state()),
        );
        automatic(plan_with(&settings, &other));
        automatic(plan_with(&settings, &licensed(valid, None)));
    }

    /// The free plan (`FreeOnly`) or a license that grants everything, without a signed-in
    /// account to check.
    struct Grants(bool);

    impl Entitlements for Grants {
        fn allows(&self, _feature: Feature) -> bool {
            self.0
        }

        fn status(&self) -> crate::entitlements::EntitlementStatus {
            crate::entitlements::FreeOnly.status()
        }
    }

    fn chosen_settings() -> Settings {
        Settings {
            advanced_encoding: AdvancedEncoding {
                max_resolution: plan::ResolutionCap::P2160,
                audio_kbps: 192,
                video_codec: VideoCodec::Hevc,
                preset: plan::EncodePreset::Quality,
                target_mb: Some(40),
            },
            ..Settings::default()
        }
    }

    #[test]
    fn codec_preset_and_target_reach_the_plan_only_with_the_feature() {
        let settings = chosen_settings();

        let (free, advanced) = plan_with(&settings, &Grants(false));
        assert!(!advanced);
        assert_eq!(
            (free.video_codec, free.preset, free.target_bytes),
            (VideoCodec::H264, plan::EncodePreset::Balanced, None),
            "free users keep the automatic behavior"
        );
        assert_eq!(
            free.video_bps,
            plan::plan_bitrate(100 * plan::MIB, 0.03, 600.0, Some(128))
        );

        let (paid, advanced) = plan_with(&settings, &Grants(true));
        assert!(advanced);
        assert_eq!(
            (paid.video_codec, paid.preset, paid.target_bytes),
            (
                VideoCodec::Hevc,
                plan::EncodePreset::Quality,
                Some(40 * plan::MIB)
            )
        );
        assert_eq!(
            paid.video_bps,
            plan::plan_bitrate(40 * plan::MIB, 0.03, 600.0, Some(192)),
            "the target sets the budget, the margin still applies"
        );
        assert_eq!(
            paid.limit_bytes,
            100 * plan::MIB,
            "the limit stays the ceiling"
        );
    }

    #[test]
    fn a_files_choices_replace_the_settings_and_need_the_feature_too() {
        let settings = chosen_settings();
        let file = EncodingOverride {
            video_codec: VideoCodec::H264,
            max_resolution: plan::ResolutionCap::P1440,
            target_mb: None,
        };

        // Without the feature the item's own choices are ignored too.
        let (free, _) = plan_with_file(&settings, &Grants(false), Some(&file));
        assert_eq!(
            (free.video_codec, free.target_bytes),
            (VideoCodec::H264, None)
        );
        assert!(free.vf.as_deref().unwrap().contains("min(1920,iw)"));

        // With it: the file's codec, cap and target; the global preset and audio stay.
        let (paid, advanced) = plan_with_file(&settings, &Grants(true), Some(&file));
        assert!(advanced);
        assert_eq!(paid.video_codec, VideoCodec::H264);
        assert_eq!(
            paid.target_bytes, None,
            "fill the limit, though the settings have a target"
        );
        assert!(paid.vf.as_deref().unwrap().contains("min(2560,iw)"));
        assert_eq!(
            (paid.preset, paid.audio_kbps),
            (plan::EncodePreset::Quality, Some(192))
        );

        // A row edited by hand to an impossible target is ignored.
        let broken = EncodingOverride {
            target_mb: Some(0),
            ..file.clone()
        };
        let (kept, _) = plan_with_file(&settings, &Grants(true), Some(&broken));
        assert_eq!(kept.video_codec, VideoCodec::Hevc);
        assert_eq!(kept.target_bytes, Some(40 * plan::MIB));

        // Two files with different choices never share an encode (the plans differ).
        let first = serde_json::to_value(&paid).unwrap();
        let second = serde_json::to_value(&kept).unwrap();
        assert_ne!(first["video_codec"], second["video_codec"]);
    }

    #[test]
    fn hevc_is_picked_among_the_gpu_encoders_and_never_skipped_for_a_cpu_one() {
        let detection = Detection {
            tested: vec![
                (Encoder::Nvenc, detect::Outcome::Works),
                (Encoder::X264, detect::Outcome::Works),
            ],
            hevc: Some(vec![(Encoder::Nvenc, detect::Outcome::Works)]),
        };
        for (prefer_cpu, gpu_failed) in [(false, false), (true, false), (false, true), (true, true)]
        {
            assert_eq!(
                pick_encoder(&detection, VideoCodec::Hevc, prefer_cpu, gpu_failed),
                Some(Encoder::Nvenc),
                "no CPU HEVC encoder exists: cpu {prefer_cpu}, gpu failed {gpu_failed}"
            );
        }
        assert_eq!(
            pick_encoder(&detection, VideoCodec::H264, false, true),
            Some(Encoder::X264),
            "H.264 still skips a GPU that stopped"
        );
        let none = Detection {
            hevc: Some(vec![(Encoder::Nvenc, detect::Outcome::NotInBuild)]),
            ..detection
        };
        assert_eq!(pick_encoder(&none, VideoCodec::Hevc, false, false), None);
    }

    #[tokio::test]
    async fn a_hevc_encode_that_stops_midway_is_never_retried_as_h264() {
        let mut hevc = sample_plan();
        hevc.video_codec = VideoCodec::Hevc;
        hevc.encoder = Some("hevc_nvenc".into());
        // Even given a CPU encoder, the retry would turn the plan's HEVC into H.264.
        let run = run_plan("fallback-hevc", SILENT, hevc, Some(Encoder::X264), None).await;
        let Err(MediaError::Reported { code, params, .. }) = run.result else {
            panic!("expected a reported failure, got {:?}", run.result);
        };
        assert_eq!(code, "FFMPEG_FAILED");
        assert_eq!(params["cause"], "hevc_gpu_only");
        assert_eq!(params["encoder"], "hevc_nvenc");
        assert_eq!(
            run.calls, ["hevc_nvenc fresh"; 3],
            "the same GPU command three times, no CPU run"
        );
        assert!(run.sink.alerts.lock().unwrap().is_empty());
        // Three failures to start on one item are not yet a GPU to give up on.
        assert_eq!(run.session.failure(), None);
        no_part_left(&run.out_dir);
    }

    #[test]
    fn reported_params_reach_the_stage_error() {
        let error = stage_error(MediaError::Reported {
            code: "FFMPEG_FAILED",
            params: json!({ "encoder": "h264_nvenc", "cause": "fallback_failed" }),
            detail: "C:\\some\\path".into(),
        });
        assert_eq!(
            error,
            StageError::Failed {
                code: "FFMPEG_FAILED".into(),
                params: json!({ "encoder": "h264_nvenc", "cause": "fallback_failed" }),
            }
        );
        let error = stage_error(MediaError::failed("DISK_SPACE", "x"));
        assert_eq!(error, StageError::failed("DISK_SPACE"));
    }

    fn analyzed_item(media_json: Option<&str>) -> Item {
        let mut store = crate::queue::test_util::memory_store();
        let id = crate::queue::test_util::add_item(&mut store, "a.mp4");
        store
            .conn()
            .execute(
                "UPDATE items SET media_json = ?1, source_size = 10, source_mtime = 1 WHERE id = ?2",
                rusqlite::params![media_json, id],
            )
            .unwrap();
        store.get_item(id).unwrap()
    }

    const INFO: &str =
        r#"{"format_name":"mov,mp4","duration_s":3.0,"bit_rate":null,"video":null,"audio":null}"#;

    /// The ffprobe of the analysis at drop is reused only for the file it was made on.
    #[test]
    fn an_earlier_probe_is_reused_only_while_the_file_is_unchanged() {
        let item = analyzed_item(Some(INFO));
        let reused = reusable_info(&item, Some((10, 1))).unwrap();
        assert_eq!(reused.duration_s, Some(3.0));
        assert!(
            reusable_info(&item, Some((11, 1))).is_none(),
            "size changed"
        );
        assert!(
            reusable_info(&item, Some((10, 2))).is_none(),
            "modified since"
        );
        assert!(reusable_info(&item, None).is_none(), "file gone");
        assert!(reusable_info(&analyzed_item(None), Some((10, 1))).is_none());
        assert!(reusable_info(&analyzed_item(Some("{")), Some((10, 1))).is_none());
    }

    /// The badge and the probing see the same plan: a JPEG sent as a file becomes a stripped
    /// copy, unless the setting is off, the file is no JPEG/PNG/WebP, or it is too big to read.
    #[test]
    fn privacy_applies_to_images_sent_as_they_are_only() {
        let dir = crate::queue::test_util::TestDir::new("stage-privacy");
        let jpeg = dir.file("IMG_1.JPG", b"\xff\xd8\xff\xe1\0\x08Exif\0\0\xff\xd9");
        let heic = dir.file("IMG_2.HEIC", b"\0\0\0\x18ftypheic\0\0\0\0");
        let plan_of = |settings: &Settings, path: &str, size: i64| {
            let mut item = analyzed_item(None);
            item.source_path = path.to_owned();
            item.source_size = size;
            item.send_mode = SendMode::Document;
            let source = PathBuf::from(path);
            let input = plan_input(
                &source,
                &item,
                plan::MIB * 1024,
                settings,
                &AdvancedEncoding::AUTOMATIC,
                None,
            );
            let plan = plan::build_plan(None, &input).unwrap();
            with_privacy(settings, plan, &source, &item, None)
        };
        let on = Settings::default();
        let stripped = plan_of(&on, &jpeg, 14);
        assert_eq!(
            (stripped.kind, stripped.output_name.as_str()),
            (Kind::Strip, "IMG_1.JPG")
        );
        assert_eq!(plan_of(&on, &heic, 16).kind, Kind::Skip, "HEIC unchanged");
        assert_eq!(
            plan_of(&on, &jpeg, STRIP_MAX_BYTES + 1).kind,
            Kind::Skip,
            "too big to read whole"
        );
        let mut off = Settings::default();
        off.privacy.strip_metadata = false;
        assert_eq!(plan_of(&off, &jpeg, 14).kind, Kind::Skip);
    }
}
