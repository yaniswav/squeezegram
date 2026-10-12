//! Integration tests against a real ffmpeg/ffprobe.
//!
//! Set `SQUEEZEGRAM_TEST_FFMPEG_DIR` to the folder holding `ffmpeg.exe` and `ffprobe.exe`
//! (the pinned LGPL build); without it every test here returns early. Sources are generated
//! by ffmpeg itself (`testsrc2`, `mandelbrot`, noise, sine): no personal file is ever read.
//! The NVENC tests also return early when no NVENC encoder passes the detection test.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::MediaError;
use super::detect::{self, Encoder, Outcome, Tools};
use super::encode::{self, Job};
use super::plan::{Kind, MIB, Plan};
use super::probe;
use super::stage::FfmpegStage;
use crate::entitlements::{EntitlementStatus, Entitlements, Feature};
use crate::limit::LimitResolver;
use crate::queue::events::recorder::Recorder;
use crate::queue::events::{Phase, ProgressReporter};
use crate::queue::orchestrator::{
    MediaOutput, MediaStage, ProbeOutcome, StageContext, StageError, Treatment,
};
use crate::queue::store::{Item, NewItem, SendMode, Store};
use crate::queue::test_util::{TestDir, memory_store};
use crate::settings::SettingsStore;
use crate::sidecars::SidecarService;

fn ffmpeg_dir() -> Option<PathBuf> {
    std::env::var_os("SQUEEZEGRAM_TEST_FFMPEG_DIR")
        .map(PathBuf::from)
        .filter(|d| d.join("ffmpeg.exe").is_file() && d.join("ffprobe.exe").is_file())
}

/// Runs the generator ffmpeg synchronously; panics with its stderr on failure.
fn generate(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new(dir.join("ffmpeg.exe"))
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg starts");
    assert!(
        out.status.success(),
        "generator failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Env {
    ffmpeg: PathBuf,
    dir: TestDir,
    stage: FfmpegStage,
    store: Store,
    sink: Arc<Recorder>,
}

/// A license that grants every feature, for the tests of the Premium choices.
struct Granted;

impl Entitlements for Granted {
    fn allows(&self, _feature: Feature) -> bool {
        true
    }

    fn status(&self) -> EntitlementStatus {
        crate::entitlements::FreeOnly.status()
    }
}

impl Env {
    fn new(limit_mb: u64) -> Option<Self> {
        Self::with(
            limit_mb,
            Arc::new(crate::entitlements::FreeOnly),
            &json!({}),
        )
    }

    /// `extra` is a settings patch applied on top (`advanced_encoding`…).
    fn with(
        limit_mb: u64,
        entitlements: Arc<dyn Entitlements>,
        extra: &serde_json::Value,
    ) -> Option<Self> {
        let ffmpeg = ffmpeg_dir()?;
        let dir = TestDir::new("media-it");
        let settings = Arc::new(SettingsStore::load(dir.path().join("settings.json")).unwrap());
        settings
            .set(&serde_json::json!({
                "ffmpeg": { "custom_dir": ffmpeg.to_string_lossy() },
                "limit": { "custom_mb": limit_mb },
                "temp": { "dir": dir.path().join("tmp").to_string_lossy(), "min_free_gb": 0 },
            }))
            .unwrap();
        settings.set(extra).unwrap();
        let sidecars = Arc::new(SidecarService::new(
            dir.path().join("sidecars"),
            settings.clone(),
        ));
        Some(Self {
            ffmpeg,
            stage: FfmpegStage::new(
                sidecars,
                settings.clone(),
                Arc::new(LimitResolver::new(settings, None)),
                entitlements,
            ),
            dir,
            store: memory_store(),
            sink: Arc::new(Recorder::default()),
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn item(&mut self, path: &Path, mode: SendMode) -> Item {
        let destination_id = self.store.list_destinations().unwrap()[0].id;
        let id = self
            .store
            .add_items(&[NewItem {
                source_path: path.to_string_lossy().into_owned(),
                source_size: std::fs::metadata(path).unwrap().len() as i64,
                source_mtime: 1,
                destination_id,
                send_mode: mode,
                caption: String::new(),
            }])
            .unwrap()[0];
        self.store.get_item(id).unwrap()
    }

    fn ctx(&self, item: &Item, cancel: &CancellationToken) -> StageContext {
        StageContext {
            cancel: cancel.clone(),
            progress: ProgressReporter::new(item.id, Phase::Probing, self.sink.clone()),
        }
    }

    /// Probe, store what the orchestrator would store, then run encode or remux.
    async fn run(
        &mut self,
        path: &Path,
        mode: SendMode,
        cancel: &CancellationToken,
    ) -> (
        Item,
        Result<ProbeOutcome, StageError>,
        Option<Result<MediaOutput, StageError>>,
    ) {
        let item = self.item(path, mode);
        let probe = match self.stage.probe(&item, &self.ctx(&item, cancel)).await {
            Ok(probe) => probe,
            Err(e) => return (item, Err(e), None),
        };
        self.store
            .conn()
            .execute(
                "UPDATE items SET plan_json = ?1, temp_path = ?2 WHERE id = ?3",
                rusqlite::params![probe.plan_json, probe.temp_path, item.id],
            )
            .unwrap();
        let item = self.store.get_item(item.id).unwrap();
        let ctx = self.ctx(&item, cancel);
        let result = match probe.treatment {
            Treatment::Encode => Some(self.stage.encode(&item, &ctx).await),
            Treatment::Remux => Some(self.stage.remux(&item, &ctx).await),
            Treatment::Skip => None,
        };
        (item, Ok(probe), result)
    }

    async fn ffprobe(&self, path: &Path) -> probe::MediaInfo {
        let ffprobe = self.ffmpeg.join("ffprobe.exe");
        probe::probe(&ffprobe, path, false, &CancellationToken::new())
            .await
            .expect("output is readable by ffprobe")
    }

    async fn detection(&self) -> detect::Detection {
        let tools = Tools {
            ffmpeg: &self.ffmpeg.join("ffmpeg.exe"),
            ffprobe: &self.ffmpeg.join("ffprobe.exe"),
        };
        detect::detect(&tools, &self.path("det"), &CancellationToken::new())
            .await
            .unwrap()
    }
}

fn plan_of(probe: &ProbeOutcome) -> Plan {
    serde_json::from_str(probe.plan_json.as_deref().unwrap()).unwrap()
}

/// A noisy 720p MPEG-4 clip (not sendable as is, so it must be encoded).
fn noisy_clip(env: &Env, name: &str, seconds: u32) -> PathBuf {
    let path = env.path(name);
    let t = seconds.to_string();
    generate(
        &env.ffmpeg,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            &t,
            "-vf",
            "noise=alls=20:allf=t",
            "-c:v",
            "mpeg4",
            "-q:v",
            "3",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            path.to_str().unwrap(),
        ],
    );
    path
}

fn failed_code(error: &StageError) -> &str {
    match error {
        StageError::Failed { code, .. } => code,
        StageError::Cancelled => "cancelled",
    }
}

#[tokio::test]
async fn real_encoder_detection_on_this_machine() {
    let Some(env) = Env::new(10) else { return };
    let started = Instant::now();
    let detection = env.detection().await;
    println!(
        "detection in {:.2?}: {:#?}",
        started.elapsed(),
        detection.tested
    );
    assert!(
        detection.choose(false).is_some(),
        "at least one encoder must work"
    );
    assert!(
        detection
            .tested
            .iter()
            .any(|(e, o)| *e == Encoder::X264 && *o == Outcome::NotInBuild),
        "the LGPL build has no libx264"
    );
}

#[tokio::test]
async fn nvenc_lands_between_95_and_100_percent_of_the_target() {
    let Some(mut env) = Env::new(4) else { return };
    if env.detection().await.gpu() != Some(Encoder::Nvenc) {
        return;
    }
    let clip = noisy_clip(&env, "src.mkv", 20);
    let started = Instant::now();
    let (_, probe, result) = env
        .run(&clip, SendMode::Video, &CancellationToken::new())
        .await;
    let plan = plan_of(&probe.unwrap());
    assert_eq!(plan.kind, Kind::Encode);
    assert_eq!(plan.encoder.as_deref(), Some("h264_nvenc"));
    let output = result.unwrap().unwrap();
    let elapsed = started.elapsed();

    let limit = 4 * MIB;
    let ratio = output.output_size as f64 / limit as f64;
    println!(
        "nvenc: {} bytes = {:.2} % of the target, {:.1}x real time",
        output.output_size,
        ratio * 100.0,
        20.0 / elapsed.as_secs_f64()
    );
    assert!((0.95..=1.0).contains(&ratio), "ratio {ratio}");
    assert!(output.output_path.ends_with("src_sq.mp4"));

    let info = env.ffprobe(Path::new(&output.output_path)).await;
    let video = info.video.unwrap();
    assert_eq!(
        (video.codec.as_str(), video.pix_fmt.as_deref()),
        ("h264", Some("yuv420p"))
    );
    assert_eq!(info.audio.unwrap().codec, "aac");
    assert!((info.duration_s.unwrap() - 20.0).abs() < 0.5);
    let mut file = std::fs::File::open(&output.output_path).unwrap();
    assert_eq!(probe::moov_first(&mut file), Some(true), "faststart");
    assert!(!Path::new(&format!("{}.part", output.output_path)).exists());
    // Progress ended at 100 %.
    let progress = env.sink.progress.lock().unwrap();
    assert_eq!(progress.last().unwrap().percent, 100.0);
}

/// HEVC on a real GPU encoder, with a custom target below the limit (Premium choices). Returns
/// early when no HEVC encoder passes its test encode. The ratio is printed, not asserted: the
/// HEVC rate control is not measured; what must hold is that the limit is never exceeded.
#[tokio::test]
async fn hevc_nvenc_encodes_hevc_tagged_hvc1_under_the_limit_and_near_the_target() {
    let Some(mut env) = Env::with(
        8,
        Arc::new(Granted),
        &json!({ "advanced_encoding": {
            "video_codec": "hevc", "preset": "quality", "target_mb": 3
        } }),
    ) else {
        return;
    };
    if !env.stage.codec_support().await.hevc {
        return;
    }
    let clip = noisy_clip(&env, "src.mkv", 20);
    let (_, probe, result) = env
        .run(&clip, SendMode::Video, &CancellationToken::new())
        .await;
    let plan = plan_of(&probe.unwrap());
    assert_eq!(plan.kind, Kind::Encode);
    assert_eq!(plan.target_bytes, Some(3 * MIB));
    assert!(plan.encoder.as_deref().unwrap().starts_with("hevc_"));
    let output = result.unwrap().unwrap();

    let ratio = output.output_size as f64 / (3 * MIB) as f64;
    println!(
        "{}: {} bytes = {:.2} % of the 3 MiB target",
        plan.encoder.as_deref().unwrap(),
        output.output_size,
        ratio * 100.0
    );
    assert!(
        output.output_size as u64 <= 8 * MIB,
        "never above the limit"
    );
    assert!(ratio <= 1.1, "ratio {ratio}");
    let info = env.ffprobe(Path::new(&output.output_path)).await;
    let video = info.video.unwrap();
    assert_eq!(
        (video.codec.as_str(), video.pix_fmt.as_deref()),
        ("hevc", Some("yuv420p"))
    );
    assert!((info.duration_s.unwrap() - 20.0).abs() < 0.5);
    let tag = std::process::Command::new(env.ffmpeg.join("ffprobe.exe"))
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .args(["stream=codec_tag_string", "-of", "csv=p=0"])
        .arg(&output.output_path)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&tag.stdout).trim(), "hvc1");
}

#[tokio::test]
async fn cancelling_mid_encode_kills_ffmpeg_and_removes_the_part() {
    let Some(mut env) = Env::new(8) else { return };
    let clip = noisy_clip(&env, "long.mkv", 40);
    let cancel = CancellationToken::new();
    let item = env.item(&clip, SendMode::Video);
    let probe = env
        .stage
        .probe(&item, &env.ctx(&item, &cancel))
        .await
        .unwrap();
    env.store
        .conn()
        .execute(
            "UPDATE items SET plan_json = ?1 WHERE id = ?2",
            rusqlite::params![probe.plan_json, item.id],
        )
        .unwrap();
    let item = env.store.get_item(item.id).unwrap();

    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        canceller.cancel();
    });
    let started = Instant::now();
    let result = env.stage.encode(&item, &env.ctx(&item, &cancel)).await;
    assert_eq!(result, Err(StageError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(10));
    let dir = env.path("tmp").join(item.id.to_string());
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

#[tokio::test]
async fn a_truncated_source_is_reported_not_sent_half_encoded() {
    let Some(mut env) = Env::new(8) else { return };
    let full = env.path("full.mp4");
    generate(
        &env.ffmpeg,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-t",
            "20",
            "-c:v",
            "mpeg4",
            "-q:v",
            "4",
            "-movflags",
            "+faststart",
            full.to_str().unwrap(),
        ],
    );
    let bytes = std::fs::read(&full).unwrap();
    let cut = env.path("cut.mp4");
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();

    let (_, probe, result) = env
        .run(&cut, SendMode::Video, &CancellationToken::new())
        .await;
    let code = match (probe, result) {
        (Err(e), _) => failed_code(&e).to_owned(),
        (Ok(_), Some(Err(e))) => failed_code(&e).to_owned(),
        (Ok(_), other) => panic!("a truncated source produced {other:?}"),
    };
    assert_eq!(code, "FILE_UNREADABLE");
}

#[tokio::test]
async fn a_corrupted_file_fails_at_probe() {
    let Some(mut env) = Env::new(8) else { return };
    let junk = env.path("junk.mp4");
    std::fs::write(
        &junk,
        (0..200_000u32)
            .map(|i| (i * 7 % 256) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let (_, probe, _) = env
        .run(&junk, SendMode::Video, &CancellationToken::new())
        .await;
    assert_eq!(failed_code(&probe.unwrap_err()), "FILE_UNREADABLE");

    let missing = env.path("gone.mp4");
    std::fs::write(&missing, b"x").unwrap();
    let item = env.item(&missing, SendMode::Video);
    std::fs::remove_file(&missing).unwrap();
    let error = env
        .stage
        .probe(&item, &env.ctx(&item, &CancellationToken::new()))
        .await
        .unwrap_err();
    assert_eq!(failed_code(&error), "FILE_UNREADABLE");
}

#[tokio::test]
async fn remux_turns_a_conforming_mkv_into_a_faststart_mp4_without_reencoding() {
    let Some(mut env) = Env::new(50) else { return };
    let src = env.path("clip.mkv");
    generate(
        &env.ffmpeg,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "6",
            "-c:v",
            "libopenh264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            src.to_str().unwrap(),
        ],
    );
    let (item, probe, result) = env
        .run(&src, SendMode::Video, &CancellationToken::new())
        .await;
    let probe = probe.unwrap();
    assert_eq!(probe.treatment, Treatment::Remux, "{:?}", plan_of(&probe));
    assert!(
        probe
            .content_id
            .starts_with(&format!("{}:", item.source_size))
    );
    let output = result.unwrap().unwrap();
    assert!(output.output_path.ends_with("clip_sq.mp4"));
    let info = env.ffprobe(Path::new(&output.output_path)).await;
    assert_eq!(info.video.unwrap().codec, "h264");
    assert_eq!(info.audio.unwrap().codec, "aac");
    assert!((info.duration_s.unwrap() - 6.0).abs() < 0.5);
    let mut file = std::fs::File::open(&output.output_path).unwrap();
    assert_eq!(probe::moov_first(&mut file), Some(true));
}

#[tokio::test]
async fn photos_become_small_jpegs_and_transparency_gets_a_white_background() {
    let Some(mut env) = Env::new(50) else { return };
    let png = env.path("big.png");
    generate(
        &env.ffmpeg,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=6000x4000",
            "-frames:v",
            "1",
            png.to_str().unwrap(),
        ],
    );
    let (_, probe, result) = env
        .run(&png, SendMode::Photo, &CancellationToken::new())
        .await;
    assert_eq!(plan_of(&probe.unwrap()).kind, Kind::Photo);
    let output = result.unwrap().unwrap();
    assert!(output.output_path.ends_with("big_sq.jpg"));
    assert!(output.output_size as u64 <= 9_961_472);
    let info = env.ffprobe(Path::new(&output.output_path)).await;
    let video = info.video.unwrap();
    assert_eq!(video.codec, "mjpeg");
    // 6000x4000 is capped to 4096 on the long side.
    assert_eq!(video.width.max(video.height), 4096);

    let alpha = env.path("alpha.png");
    generate(
        &env.ffmpeg,
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=red@0.5:s=320x240,format=rgba",
            "-frames:v",
            "1",
            alpha.to_str().unwrap(),
        ],
    );
    let (_, probe, result) = env
        .run(&alpha, SendMode::Photo, &CancellationToken::new())
        .await;
    assert!(plan_of(&probe.unwrap()).alpha);
    let output = result.unwrap().unwrap();
    assert_eq!(
        env.ffprobe(Path::new(&output.output_path))
            .await
            .video
            .unwrap()
            .codec,
        "mjpeg"
    );
}

#[tokio::test]
async fn document_mode_only_hashes() {
    let Some(mut env) = Env::new(50) else { return };
    let file = env.path("doc.bin");
    std::fs::write(&file, vec![9u8; 100_000]).unwrap();
    let (_, probe, result) = env
        .run(&file, SendMode::Document, &CancellationToken::new())
        .await;
    let probe = probe.unwrap();
    assert_eq!(probe.treatment, Treatment::Skip);
    assert!(result.is_none());
    assert_eq!(
        probe.content_id,
        format!("100000:{}", blake3::hash(&vec![9u8; 100_000]).to_hex())
    );
}

/// libopenh264 and h264_mf are NOT measured: this only checks that they produce
/// a readable H.264 file and prints the size they reach against the target.
#[tokio::test]
async fn cpu_fallback_encoders_smoke() {
    let Some(mut env) = Env::new(4) else { return };
    let detection = env.detection().await;
    let clip = noisy_clip(&env, "cpu.mkv", 10);
    let item = env.item(&clip, SendMode::Video);
    let cancel = CancellationToken::new();
    let probe = env
        .stage
        .probe(&item, &env.ctx(&item, &cancel))
        .await
        .unwrap();
    let plan = plan_of(&probe);
    let tools = Tools {
        ffmpeg: &env.ffmpeg.join("ffmpeg.exe"),
        ffprobe: &env.ffmpeg.join("ffprobe.exe"),
    };
    for encoder in [Encoder::OpenH264, Encoder::MediaFoundation] {
        if !detection.tested.contains(&(encoder, Outcome::Works)) {
            println!("{} does not work here: skipped", encoder.name());
            continue;
        }
        let out_dir = env.path(&format!("cpu_{}", encoder.name()));
        let ctx = env.ctx(&item, &cancel);
        let job = Job {
            tools: &tools,
            input: &clip,
            plan: &plan,
            out_dir: &out_dir,
            min_free_bytes: 0,
            cancel: &cancel,
            progress: &ctx.progress,
        };
        match encode::encode_video(&job, encoder).await {
            Ok(produced) => {
                println!(
                    "{}: {} bytes = {:.1} % of the target (unmeasured scheme)",
                    encoder.name(),
                    produced.size,
                    produced.size as f64 / (4 * MIB) as f64 * 100.0
                );
                assert_eq!(
                    env.ffprobe(&produced.path).await.video.unwrap().codec,
                    "h264"
                );
            }
            Err(MediaError::Failed {
                code: "OUTPUT_TOO_LARGE",
                detail,
            }) => {
                println!(
                    "{}: OUTPUT_TOO_LARGE after the corrective pass ({detail})",
                    encoder.name()
                );
            }
            Err(other) => panic!("{}: {other}", encoder.name()),
        }
    }
}

/// Average brightness (0-255) of a picture, read back with ffmpeg.
fn mean_luma(dir: &Path, picture: &Path) -> u8 {
    let out = std::process::Command::new(dir.join("ffmpeg.exe"))
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-i"])
        .arg(picture)
        .args([
            "-vf",
            "scale=1:1",
            "-pix_fmt",
            "gray",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .expect("ffmpeg starts");
    out.stdout[0]
}

/// 0.5 s of black, then 9.5 s of white at `sar`: a thumbnail of the first frame is black.
fn black_then_white(dir: &Path, out: &Path, sar: &str) {
    let graph = format!("[0][1]concat=n=2:v=1:a=0,setsar={sar}");
    generate(
        dir,
        &[
            "-f",
            "lavfi",
            "-i",
            "color=black:s=1920x1080:d=0.5:r=25",
            "-f",
            "lavfi",
            "-i",
            "color=white:s=1920x1080:d=9.5:r=25",
            "-filter_complex",
            &graph,
            "-c:v",
            "mpeg4",
            &out.to_string_lossy(),
        ],
    );
}

#[tokio::test]
async fn the_thumbnail_is_a_small_jpeg_of_a_frame_past_the_black_opening() {
    let Some(ffmpeg) = ffmpeg_dir() else { return };
    let dir = TestDir::new("thumb-it");
    let clip = dir.path().join("clip.mp4");
    black_then_white(&ffmpeg, &clip, "1");
    let thumb = super::thumb::generate(
        &ffmpeg.join("ffmpeg.exe"),
        &clip,
        Some(10.0),
        &dir.path().join("out"),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!((thumb.width, thumb.height), (320, 180));
    let size = std::fs::metadata(&thumb.path).unwrap().len();
    assert!(size > 0 && size <= super::thumb::MAX_BYTES, "{size} bytes");
    let bytes = std::fs::read(&thumb.path).unwrap();
    assert_eq!(bytes[..2], [0xFF, 0xD8], "a JPEG");
    assert!(
        mean_luma(&ffmpeg, &thumb.path) > 200,
        "the frame at 10 % (1 s) is white, not the black opening"
    );
}

#[tokio::test]
async fn the_thumbnail_has_the_shape_the_player_shows() {
    let Some(ffmpeg) = ffmpeg_dir() else { return };
    let dir = TestDir::new("thumb-sar-it");
    let clip = dir.path().join("wide.mp4");
    // 1920x1080 pixels shown twice as wide: 3840x1080 on screen.
    black_then_white(&ffmpeg, &clip, "2");
    let thumb = super::thumb::generate(
        &ffmpeg.join("ffmpeg.exe"),
        &clip,
        Some(10.0),
        dir.path(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!((thumb.width, thumb.height), (320, 90));
}

/// A duration that overstates the video puts the frame past its end: the first one is used.
#[tokio::test]
async fn the_thumbnail_falls_back_to_the_first_frame_and_reports_a_broken_file() {
    let Some(ffmpeg) = ffmpeg_dir() else { return };
    let dir = TestDir::new("thumb-short-it");
    let clip = dir.path().join("short.mp4");
    generate(
        &ffmpeg,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x120:d=1:r=25",
            "-c:v",
            "mpeg4",
            &clip.to_string_lossy(),
        ],
    );
    let exe = ffmpeg.join("ffmpeg.exe");
    let cancel = CancellationToken::new();
    let thumb = super::thumb::generate(&exe, &clip, Some(500.0), dir.path(), &cancel)
        .await
        .unwrap();
    assert_eq!((thumb.width, thumb.height), (160, 120), "not enlarged");

    let broken = dir.path().join("broken.mp4");
    std::fs::write(&broken, b"not a video").unwrap();
    assert!(
        super::thumb::generate(&exe, &broken, None, dir.path(), &cancel)
            .await
            .is_err()
    );
}

/// Run on demand: `cargo test --lib -- --ignored blake3_speed`.
#[test]
#[ignore = "writes and hashes a 1 GiB file"]
fn blake3_speed_on_a_generated_gigabyte() {
    let dir = TestDir::new("blake3-bench");
    let path = dir.path().join("1g.bin");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&path).unwrap();
        let block: Vec<u8> = (0..1_048_576u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for _ in 0..1024 {
            file.write_all(&block).unwrap();
        }
    }
    let started = Instant::now();
    let id = super::hash::content_id_blocking(&path, &CancellationToken::new(), |_| {}).unwrap();
    let elapsed = started.elapsed();
    println!("blake3 of 1 GiB (chunked 16 MiB, rayon, cache warm): {elapsed:.2?}, id {id}");
    assert!(id.starts_with("1073741824:"));
}

/// The one real download of the pinned build (171 MB), into `SQUEEZEGRAM_REAL_DOWNLOAD_DIR`.
#[test]
#[ignore = "downloads 171 MB from GitHub"]
fn real_download_of_the_pinned_build() {
    let Some(dir) = std::env::var_os("SQUEEZEGRAM_REAL_DOWNLOAD_DIR").map(PathBuf::from) else {
        return;
    };
    let sink = Recorder::default();
    let started = Instant::now();
    crate::sidecars::install_build(&crate::sidecars::pinned_build(), &dir, "ffmpeg", &sink)
        .unwrap();
    println!("downloaded and installed in {:.1?}", started.elapsed());
    assert!(dir.join("ffmpeg").join("bin").join("ffmpeg.exe").is_file());
}
