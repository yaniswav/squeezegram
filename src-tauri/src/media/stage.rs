//! The real `MediaStage`: probe + identity, then encode, remux or photo.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::json;
use tokio::sync::Mutex;

use super::detect::{self, Detection, Encoder, Tools};
use super::encode::{self, EncoderStop, Job};
use super::plan::{self, Kind, Plan, PlanInput};
use super::probe::{self, MediaInfo};
use super::{MediaError, Result, hash};
use crate::entitlements::{Entitlements, Feature};
use crate::limit::LimitResolver;
use crate::queue::events::{Phase, Progress};
use crate::queue::orchestrator::{
    MediaOutput, MediaStage, ProbeOutcome, StageContext, StageError, Treatment,
};
use crate::queue::store::{Item, SendMode};
use crate::settings::{AdvancedEncoding, Settings, SettingsStore};
use crate::sidecars::{Resolved, SidecarService};

const PROGRESS_TICK: Duration = Duration::from_millis(250);

/// The resolution cap and audio bitrate of a plan, and whether they come from the advanced
/// settings (`true`) or are the automatic ones.
///
/// The license is asked here, when the plan is built: a value saved while the feature was
/// granted (or written into `settings.json` by hand) has no effect without it.
pub(crate) fn encoding_choice(
    settings: &Settings,
    entitlements: &dyn Entitlements,
) -> (AdvancedEncoding, bool) {
    if entitlements.allows(Feature::AdvancedEncoding) {
        (settings.advanced_encoding.clone(), true)
    } else {
        (AdvancedEncoding::AUTOMATIC, false)
    }
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
    gpu: GpuSession,
}

/// A GPU encoder that stopped mid-encode in this session (`encoder_status`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuFailure {
    pub encoder: String,
    /// Seconds the failed run lasted.
    pub at_seconds: u64,
}

/// Memory of a GPU failure, for this session only: new encodes then go to the CPU instead of
/// paying a failed GPU attempt on every file. The GPU is tried again after a restart.
#[derive(Default)]
pub struct GpuSession(std::sync::Mutex<Option<GpuFailure>>);

impl GpuSession {
    pub fn failure(&self) -> Option<GpuFailure> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record(&self, stop: &EncoderStop) {
        let failure = GpuFailure {
            encoder: stop.encoder.name().to_owned(),
            at_seconds: stop.at_seconds(),
        };
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(failure);
    }
}

/// The encoder of a new encode: the user's preference, except that a GPU that stopped
/// mid-encode this session is skipped while a CPU encoder works.
fn pick_encoder(detection: &Detection, prefer_cpu: bool, gpu_failed: bool) -> Option<Encoder> {
    if gpu_failed {
        detection.cpu().or_else(|| detection.gpu())
    } else {
        detection.choose(prefer_cpu)
    }
}

/// `{ encoder, exitCode, atSeconds }` of a stopped GPU encoder (params of `FFMPEG_FAILED`).
fn stop_params(stop: &EncoderStop) -> serde_json::Value {
    json!({
        "encoder": stop.encoder.name(),
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
/// for it. The GPU is then skipped for the rest of the session (`session`).
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
        other => return other,
    };
    session.record(&stop);
    let detail = stop.to_string();
    tracing::warn!(
        event = "encoder_failed_midway",
        error_code = "FFMPEG_FAILED",
        encoder = stop.encoder.name(),
        exit_code = stop.exit_code,
        dur_ms = stop.elapsed_ms,
        out_time_s = (stop.out_time_s * 100.0).round() / 100.0,
        stderr_empty = stop.stderr_empty,
        fallback = cpu.map(Encoder::name),
        stderr_tail = stop.stderr_tail.as_str(),
        "GPU encoder stopped mid-encode; GPU skipped until the app restarts"
    );
    let mut params = stop_params(&stop);
    let Some(cpu) = cpu.filter(|e| !e.is_gpu()) else {
        params["cause"] = json!("no_cpu_encoder");
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
            "from": stop.encoder.name(),
            "to": cpu.name(),
            "atSeconds": stop.at_seconds(),
            "cause": if stop.stderr_empty { "silent" } else { "encoder_error" },
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

/// `{ ffmpeg, tested: [{ encoder, outcome, reason? }] }`.
fn detection_json(ffmpeg: &std::path::Path, detection: &Detection) -> serde_json::Value {
    let tested: Vec<_> = detection
        .tested
        .iter()
        .map(|(encoder, outcome)| {
            let (outcome, reason) = match outcome {
                detect::Outcome::Works => ("works", None),
                detect::Outcome::NotInBuild => ("not_in_build", None),
                detect::Outcome::Failed(reason) => ("failed", Some(reason.as_str())),
            };
            serde_json::json!({ "encoder": encoder.name(), "outcome": outcome, "reason": reason })
        })
        .collect();
    serde_json::json!({ "ffmpeg": ffmpeg.to_string_lossy(), "tested": tested })
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

    fn item_dir(&self, item: &Item) -> PathBuf {
        self.temp_base().join(item.id.to_string())
    }

    /// Detection result for this ffmpeg, computed once per path (about 1 s).
    async fn detection(
        &self,
        resolved: &Resolved,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Detection> {
        let mut cache = self.detection.lock().await;
        if let Some(cached) = cache.as_ref()
            && cached.ffmpeg == resolved.ffmpeg
        {
            return Ok(cached.detection.clone());
        }
        let tools = Tools {
            ffmpeg: &resolved.ffmpeg,
            ffprobe: &resolved.ffprobe,
        };
        let started = std::time::Instant::now();
        let detection = detect::detect(&tools, &self.temp_base().join("detect"), cancel).await?;
        let report = detection_json(&resolved.ffmpeg, &detection).to_string();
        tracing::info!(
            event = "encoder_detection",
            dur_ms = crate::logging::ms_since(started),
            gpu = detection.gpu().map(Encoder::name),
            cpu = detection.cpu().map(Encoder::name),
            json = report.as_str(),
            "encoder detection finished"
        );
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
        let probing = async {
            match &resolved {
                None => Ok(None),
                Some(r) => {
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

        let moov_first = if source
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
        {
            let path = source.clone();
            tokio::task::spawn_blocking(move || {
                std::fs::File::open(path)
                    .ok()
                    .and_then(|mut f| probe::moov_first(&mut f))
            })
            .await
            .ok()
            .flatten()
        } else {
            None
        };

        let (choice, advanced) = encoding_choice(&settings, self.entitlements.as_ref());
        let input = PlanInput {
            source: &source,
            source_size: u64::try_from(item.source_size).unwrap_or(0),
            send_mode: item.send_mode,
            limit_bytes: limit.limit_bytes,
            margin: settings.limit.margin_percent / 100.0,
            audio_kbps: choice.audio_kbps,
            cap: choice.max_resolution,
            moov_first,
        };
        let summary = probe_summary(item, &content_id, info.as_ref(), moov_first);
        tracing::info!(
            event = "media_probe",
            dur_ms = crate::logging::ms_since(started),
            json = summary.as_str(),
            "probe and identity done"
        );
        let mut plan = plan::build_plan(info.as_ref(), &input)
            .map_err(|e| MediaError::failed(e.code(), e.to_string()))?;

        if plan.kind == Kind::Encode
            && let Some(resolved) = &resolved
        {
            let detection = self.detection(resolved, &ctx.cancel).await?;
            let prefer_cpu = settings.encoding.prefer == "cpu";
            let gpu_failed = self.gpu.failure().is_some();
            let encoder = pick_encoder(&detection, prefer_cpu, gpu_failed).ok_or_else(|| {
                MediaError::failed("FFMPEG_ENCODER_UNAVAILABLE", "no encoder passed its test")
            })?;
            tracing::info!(
                event = "encoder_chosen",
                encoder = encoder.name(),
                gpu = encoder.is_gpu(),
                prefer = settings.encoding.prefer.as_str(),
                gpu_skipped = gpu_failed && !prefer_cpu && !encoder.is_gpu(),
                "encoder chosen"
            );
            plan.encoder = Some(encoder.name().to_owned());
        }
        let plan_json = serde_json::json!({ "plan": plan }).to_string();
        tracing::info!(
            event = "media_plan",
            margin_percent = settings.limit.margin_percent,
            advanced,
            json = plan_json.as_str(),
            "plan built"
        );

        let treatment = match plan.kind {
            Kind::Encode | Kind::Photo => Treatment::Encode,
            Kind::Remux | Kind::RemuxAudio => Treatment::Remux,
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
        let planned = job.plan.encoder.as_deref().and_then(Encoder::parse);
        let gpu_failed = self.gpu.failure().is_some();
        let mut encoder = match planned {
            Some(encoder) => encoder,
            None => pick_encoder(
                &self.detection(resolved, job.cancel).await?,
                prefer == "cpu",
                gpu_failed,
            )
            .ok_or_else(|| MediaError::failed("FFMPEG_ENCODER_UNAVAILABLE", "no encoder"))?,
        };
        // The CPU encoder of a fallback (detection cached since the probe).
        let mut cpu = None;
        if encoder.is_gpu() {
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
                    failed = encoder.name(),
                    detail = detail.as_str(),
                    "encoder cannot open; detecting again"
                );
                self.forget_detection().await;
                let detection = self.detection(resolved, job.cancel).await?;
                let other = detection.alternative_to(encoder).ok_or_else(|| {
                    MediaError::failed("FFMPEG_ENCODER_UNAVAILABLE", "no other encoder works")
                })?;
                tracing::info!(
                    event = "encoder_chosen",
                    encoder = other.name(),
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
    }

    const SILENT: Fake = Fake {
        gpu_blocks: 3,
        gpu_stderr: None,
        gpu_wait_s: 0,
        cpu_fails: false,
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
            cpu_fail.trim_end(),
            "if /i not \"%OUT%\"==\"NUL\" echo encoded>\"%OUT%\"",
            "echo out_time_us=60000000",
            "echo progress=end",
            "exit /b 0",
            ":gpu",
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
        let dir = TestDir::new(name);
        let (ffmpeg, ffprobe) = fake_tools(dir.path(), fake);
        let input = dir.path().join("in.mkv");
        std::fs::write(&input, b"source").unwrap();
        let out_dir = dir.path().join("out");
        let tools = Tools {
            ffmpeg: &ffmpeg,
            ffprobe: &ffprobe,
        };
        let plan = sample_plan();
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
        let session = GpuSession::default();
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
    async fn a_silent_gpu_stop_is_retried_once_on_the_cpu() {
        let run = run("fallback-ok", SILENT, Some(Encoder::X264), None).await;
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
            ..SILENT
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
    async fn without_a_cpu_encoder_the_stop_is_reported_with_its_numbers() {
        let run = run("fallback-none", SILENT, None, None).await;
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
        assert!(session.failure().is_some());
        assert_eq!(
            sink.alert_codes(),
            [(Some(1), "ENCODER_FALLBACK".to_owned())]
        );
        no_part_left(&out_dir);
    }

    #[test]
    fn a_gpu_that_failed_this_session_is_skipped_while_a_cpu_encoder_works() {
        let detection = Detection {
            tested: vec![
                (Encoder::Nvenc, detect::Outcome::Works),
                (Encoder::X264, detect::Outcome::NotInBuild),
                (Encoder::OpenH264, detect::Outcome::Works),
            ],
        };
        assert_eq!(pick_encoder(&detection, false, false), Some(Encoder::Nvenc));
        assert_eq!(
            pick_encoder(&detection, true, false),
            Some(Encoder::OpenH264),
            "CPU first"
        );
        assert_eq!(
            pick_encoder(&detection, false, true),
            Some(Encoder::OpenH264)
        );
        let gpu_only = Detection {
            tested: vec![(Encoder::Nvenc, detect::Outcome::Works)],
        };
        assert_eq!(pick_encoder(&gpu_only, false, true), Some(Encoder::Nvenc));
    }

    /// A 4K HEVC source with AAC audio, encoded under a 100 MiB limit, planned with what
    /// `encoding_choice` gives for `settings` and `entitlements`.
    fn plan_with(settings: &Settings, entitlements: &dyn Entitlements) -> (Plan, bool) {
        let info: MediaInfo = serde_json::from_value(json!({
            "format_name": "matroska,webm", "duration_s": 600.0, "bit_rate": null,
            "video": { "index": 0, "codec": "hevc", "profile": "Main", "width": 3840,
                "height": 2160, "sar": [1, 1], "pix_fmt": "yuv420p",
                "field_order": "progressive", "color_transfer": null, "rotation": 0,
                "bit_rate": null, "hdr": false },
            "audio": { "index": 1, "codec": "aac", "channels": 2, "bit_rate": null }
        }))
        .unwrap();
        let (choice, advanced) = encoding_choice(settings, entitlements);
        let source = Path::new("D:\\clips\\uhd.mkv");
        let input = PlanInput {
            source,
            source_size: 900 * plan::MIB,
            send_mode: SendMode::Video,
            limit_bytes: 100 * plan::MIB,
            margin: 0.03,
            audio_kbps: choice.audio_kbps,
            cap: choice.max_resolution,
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
}
