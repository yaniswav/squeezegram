//! Encoder detection by a real test encode: `-encoders` lists encoders that
//! cannot run on this machine (QSV and AMF without hardware), so each candidate encodes 60
//! frames and the result is checked with ffprobe.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::plan::VideoCodec;
use super::procs::{self, Args, Ending, Spec};
use super::{MediaError, Result, probe};

const LIST_TIMEOUT: Duration = Duration::from_secs(15);
const CANDIDATE_TIMEOUT: Duration = Duration::from_secs(8);
const MIN_TEST_DURATION_S: f64 = 1.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Encoder {
    Nvenc,
    Qsv,
    Amf,
    /// Cisco OpenH264 linked into the LGPL build (Constrained Baseline to High, no 2 passes).
    OpenH264,
    /// Media Foundation (Microsoft software encoder or a hardware MFT).
    MediaFoundation,
    /// Only in an ffmpeg the user provides: the LGPL build has no libx264.
    X264,
}

const GPU: [Encoder; 3] = [Encoder::Nvenc, Encoder::Qsv, Encoder::Amf];
const CPU: [Encoder; 3] = [Encoder::X264, Encoder::OpenH264, Encoder::MediaFoundation];

impl Encoder {
    pub fn name(self) -> &'static str {
        match self {
            Encoder::Nvenc => "h264_nvenc",
            Encoder::Qsv => "h264_qsv",
            Encoder::Amf => "h264_amf",
            Encoder::OpenH264 => "libopenh264",
            Encoder::MediaFoundation => "h264_mf",
            Encoder::X264 => "libx264",
        }
    }

    /// The ffmpeg encoder of `codec` in this family, `None` when it has none: HEVC only comes
    /// from the GPU families (the LGPL build has no libx265, and `hevc_mf` is not used).
    pub fn codec_name(self, codec: VideoCodec) -> Option<&'static str> {
        match (codec, self) {
            (VideoCodec::H264, _) => Some(self.name()),
            (VideoCodec::Hevc, Encoder::Nvenc) => Some("hevc_nvenc"),
            (VideoCodec::Hevc, Encoder::Qsv) => Some("hevc_qsv"),
            (VideoCodec::Hevc, Encoder::Amf) => Some("hevc_amf"),
            (VideoCodec::Hevc, _) => None,
        }
    }

    /// The family of an encoder name, H.264 or HEVC (`plan.encoder` holds either).
    pub fn parse(name: &str) -> Option<Self> {
        Self::all().find(|encoder| {
            encoder.name() == name
                || encoder
                    .codec_name(VideoCodec::Hevc)
                    .is_some_and(|hevc| hevc == name)
        })
    }

    /// The H.264 encoders the app can use, GPU ones first.
    pub fn all() -> impl Iterator<Item = Encoder> {
        GPU.into_iter().chain(CPU)
    }

    pub fn is_gpu(self) -> bool {
        GPU.contains(&self)
    }

    /// Encoder options of the test encode.
    fn test_options(self, codec: VideoCodec) -> &'static [&'static str] {
        if codec == VideoCodec::Hevc {
            return match self {
                Encoder::Nvenc => &["-c:v", "hevc_nvenc", "-preset", "p4", "-tag:v", "hvc1"],
                Encoder::Qsv => &["-c:v", "hevc_qsv", "-tag:v", "hvc1"],
                Encoder::Amf => &["-c:v", "hevc_amf", "-tag:v", "hvc1"],
                _ => &[],
            };
        }
        match self {
            Encoder::Nvenc => &["-c:v", "h264_nvenc", "-preset", "p4"],
            Encoder::Qsv => &["-c:v", "h264_qsv"],
            Encoder::Amf => &["-c:v", "h264_amf"],
            Encoder::OpenH264 => &["-c:v", "libopenh264"],
            Encoder::MediaFoundation => &["-c:v", "h264_mf"],
            Encoder::X264 => &["-c:v", "libx264", "-preset", "ultrafast"],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Works,
    NotInBuild,
    /// `timeout`, the first stderr line, `ffprobe_unreadable` or `bad_output`.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detection {
    /// The H.264 encoders.
    pub tested: Vec<(Encoder, Outcome)>,
    /// The HEVC encoders (GPU ones, up to the first that works). `None` until a HEVC encode or
    /// the settings page asks (`detect_hevc`): an H.264-only session never pays for it.
    #[serde(default)]
    pub hevc: Option<Vec<(Encoder, Outcome)>>,
}

impl Detection {
    /// Every encoder whose test encode succeeded.
    pub fn working(&self) -> Vec<Encoder> {
        self.tested
            .iter()
            .filter(|(_, outcome)| *outcome == Outcome::Works)
            .map(|(encoder, _)| *encoder)
            .collect()
    }

    fn first_working(&self, group: &[Encoder]) -> Option<Encoder> {
        self.tested
            .iter()
            .find(|(encoder, outcome)| *outcome == Outcome::Works && group.contains(encoder))
            .map(|(encoder, _)| *encoder)
    }

    pub fn gpu(&self) -> Option<Encoder> {
        self.first_working(&GPU)
    }

    pub fn cpu(&self) -> Option<Encoder> {
        self.first_working(&CPU)
    }

    /// GPU first unless the user prefers the CPU; the other kind is the fallback.
    pub fn choose(&self, prefer_cpu: bool) -> Option<Encoder> {
        if prefer_cpu {
            self.cpu().or_else(|| self.gpu())
        } else {
            self.gpu().or_else(|| self.cpu())
        }
    }

    /// The HEVC encoder to use: the first GPU one whose test encode worked. There is no CPU
    /// one, so the CPU preference does not apply.
    pub fn hevc_gpu(&self) -> Option<Encoder> {
        self.hevc
            .as_ref()?
            .iter()
            .find(|(_, outcome)| *outcome == Outcome::Works)
            .map(|(encoder, _)| *encoder)
    }

    /// The encoder of `codec` for a new encode (`prefer_cpu` only matters for H.264).
    pub fn choose_for(&self, codec: VideoCodec, prefer_cpu: bool) -> Option<Encoder> {
        match codec {
            VideoCodec::H264 => self.choose(prefer_cpu),
            VideoCodec::Hevc => self.hevc_gpu(),
        }
    }

    /// The best encoder of `codec` other than `failed`, for the retry after an encoder fails
    /// to open.
    pub fn alternative_for(&self, failed: Encoder, codec: VideoCodec) -> Option<Encoder> {
        match codec {
            VideoCodec::H264 => self.alternative_to(failed),
            VideoCodec::Hevc => self
                .hevc
                .as_ref()?
                .iter()
                .find(|(e, o)| *o == Outcome::Works && *e != failed)
                .map(|(e, _)| *e),
        }
    }

    /// The best H.264 encoder other than `failed`.
    pub fn alternative_to(&self, failed: Encoder) -> Option<Encoder> {
        let others = |group: &[Encoder]| {
            self.tested
                .iter()
                .find(|(e, o)| *o == Outcome::Works && group.contains(e) && *e != failed)
                .map(|(e, _)| *e)
        };
        if failed.is_gpu() {
            others(&CPU).or_else(|| others(&GPU))
        } else {
            others(&GPU).or_else(|| others(&CPU))
        }
    }
}

/// Encoder names in the output of `ffmpeg -encoders`:
/// ` V....D libx264  ...` (flags are 6 characters, the first is V, A or S).
pub fn parse_encoder_list(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix(' ')?;
            let (flags, rest) = rest.split_at_checked(6)?;
            let mut chars = flags.chars();
            let kind = chars.next()?;
            if !matches!(kind, 'V' | 'A' | 'S')
                || !chars.all(|c| c == '.' || c.is_ascii_uppercase())
            {
                return None;
            }
            rest.strip_prefix(' ')?
                .split_whitespace()
                .next()
                .filter(|name| *name != "=")
                .map(str::to_owned)
        })
        .collect()
}

pub struct Tools<'a> {
    pub ffmpeg: &'a Path,
    pub ffprobe: &'a Path,
}

/// Tests every candidate (GPU ones until the first that works, then CPU ones likewise).
/// `work_dir` receives the short test files, always deleted.
pub async fn detect(
    tools: &Tools<'_>,
    work_dir: &Path,
    cancel: &CancellationToken,
) -> Result<Detection> {
    detect_with(tools, work_dir, cancel, true).await
}

/// Tests every encoder of the build (no early stop): used to rank the ffmpeg builds found on
/// the machine by what really works with each.
pub async fn detect_all(
    tools: &Tools<'_>,
    work_dir: &Path,
    cancel: &CancellationToken,
) -> Result<Detection> {
    detect_with(tools, work_dir, cancel, false).await
}

async fn detect_with(
    tools: &Tools<'_>,
    work_dir: &Path,
    cancel: &CancellationToken,
    stop_at_first: bool,
) -> Result<Detection> {
    std::fs::create_dir_all(work_dir)?;
    let available = list_encoders(tools, cancel).await?;

    let mut tested = Vec::new();
    for group in [&GPU[..], &CPU[..]] {
        for &encoder in group {
            if !available.iter().any(|name| name == encoder.name()) {
                tested.push((encoder, Outcome::NotInBuild));
                continue;
            }
            let outcome = test_encoder(tools, encoder, VideoCodec::H264, work_dir, cancel).await?;
            let works = outcome == Outcome::Works;
            tested.push((encoder, outcome));
            if works && stop_at_first {
                break;
            }
        }
    }
    Ok(Detection { tested, hevc: None })
}

/// Tests the HEVC encoders, GPU ones until the first that works. Run on demand (a HEVC encode,
/// the settings page), never with the H.264 detection.
pub async fn detect_hevc(
    tools: &Tools<'_>,
    work_dir: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<(Encoder, Outcome)>> {
    std::fs::create_dir_all(work_dir)?;
    let available = list_encoders(tools, cancel).await?;
    let mut tested = Vec::new();
    for encoder in GPU {
        let Some(name) = encoder.codec_name(VideoCodec::Hevc) else {
            continue;
        };
        if !available.iter().any(|listed| listed == name) {
            tested.push((encoder, Outcome::NotInBuild));
            continue;
        }
        let outcome = test_encoder(tools, encoder, VideoCodec::Hevc, work_dir, cancel).await?;
        let works = outcome == Outcome::Works;
        tested.push((encoder, outcome));
        if works {
            break;
        }
    }
    Ok(tested)
}

/// Encoder names of this ffmpeg (`-encoders`).
async fn list_encoders(tools: &Tools<'_>, cancel: &CancellationToken) -> Result<Vec<String>> {
    let list = procs::run(
        Spec::new(
            tools.ffmpeg,
            Args::new().all(&["-hide_banner", "-encoders"]),
        )
        .timeout(LIST_TIMEOUT)
        .cancel(cancel)
        .collect_stdout(),
        |_| {},
    )
    .await
    .map_err(|e| MediaError::failed("FFMPEG_MISSING", e.to_string()))?;
    if list.ending == Ending::Cancelled {
        return Err(MediaError::Cancelled);
    }
    if !list.success() {
        return Err(MediaError::failed(
            "FFMPEG_MISSING",
            format!("ffmpeg -encoders: {:?}", list.code),
        ));
    }
    Ok(parse_encoder_list(&list.stdout))
}

async fn test_encoder(
    tools: &Tools<'_>,
    encoder: Encoder,
    codec: VideoCodec,
    work_dir: &Path,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let name = encoder.codec_name(codec).unwrap_or_else(|| encoder.name());
    let file: PathBuf = work_dir.join(format!("det_{name}_{}.mp4", std::process::id()));
    let result = run_test(tools, encoder, codec, &file, cancel).await;
    // A failed encode can leave an empty file behind.
    let _ = std::fs::remove_file(&file);
    result
}

async fn run_test(
    tools: &Tools<'_>,
    encoder: Encoder,
    codec: VideoCodec,
    file: &Path,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let mut args = Args::new().all(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1280x720:rate=30",
    ]);
    args = args.all(encoder.test_options(codec)).all(&[
        "-pix_fmt",
        "yuv420p",
        "-frames:v",
        "60",
        "-an",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
    ]);
    args = args.arg(file);
    let output = procs::run(
        Spec::new(tools.ffmpeg, args)
            .timeout(CANDIDATE_TIMEOUT)
            .cancel(cancel),
        |_| {},
    )
    .await
    .map_err(|e| MediaError::failed("FFMPEG_MISSING", e.to_string()))?;
    match output.ending {
        Ending::Cancelled => return Err(MediaError::Cancelled),
        Ending::TimedOut => return Ok(Outcome::Failed("timeout".into())),
        Ending::Exited => {}
    }
    if !output.success() {
        let reason = output
            .stderr_tail
            .first()
            .cloned()
            .unwrap_or_else(|| format!("exit {:?}", output.code));
        return Ok(Outcome::Failed(reason));
    }
    let Ok(info) = probe::probe(tools.ffprobe, file, false, cancel).await else {
        return Ok(Outcome::Failed("ffprobe_unreadable".into()));
    };
    let good = info
        .video
        .as_ref()
        .is_some_and(|v| v.codec == codec.as_str() && v.pix_fmt.as_deref() == Some("yuv420p"))
        && info.duration_s.is_some_and(|d| d > MIN_TEST_DURATION_S)
        && info
            .format_name
            .as_deref()
            .is_some_and(|f| f.contains("mp4"));
    Ok(if good {
        Outcome::Works
    } else {
        Outcome::Failed("bad_output".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = " Encoders:
 V..... = Video
 ------
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)
 V....D h264_qsv             H.264 / AVC / MPEG-4 AVC (Intel Quick Sync Video acceleration) (codec h264)
 V....D libopenh264          OpenH264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 (codec h264)
 A....D aac                  AAC (Advanced Audio Coding)
 S..... srt                  SubRip subtitle
";

    #[test]
    fn encoder_names_are_read_from_the_list() {
        let names = parse_encoder_list(LIST);
        assert_eq!(
            names,
            ["h264_nvenc", "h264_qsv", "libopenh264", "aac", "srt"]
        );
    }

    fn detection(works: &[Encoder]) -> Detection {
        Detection {
            hevc: None,
            tested: GPU
                .into_iter()
                .chain(CPU)
                .map(|e| {
                    let outcome = if works.contains(&e) {
                        Outcome::Works
                    } else {
                        Outcome::Failed("x".into())
                    };
                    (e, outcome)
                })
                .collect(),
        }
    }

    #[test]
    fn choice_follows_the_preference_with_the_other_kind_as_fallback() {
        let both = detection(&[Encoder::Nvenc, Encoder::OpenH264]);
        assert_eq!(both.choose(false), Some(Encoder::Nvenc));
        assert_eq!(both.choose(true), Some(Encoder::OpenH264));
        assert_eq!(both.alternative_to(Encoder::Nvenc), Some(Encoder::OpenH264));

        let cpu_only = detection(&[Encoder::MediaFoundation, Encoder::OpenH264]);
        assert_eq!(cpu_only.choose(false), Some(Encoder::OpenH264));
        assert_eq!(
            cpu_only.alternative_to(Encoder::OpenH264),
            Some(Encoder::MediaFoundation)
        );

        let none = detection(&[]);
        assert_eq!(none.choose(false), None);
    }

    fn detection_with_hevc(h264: &[Encoder], hevc: Option<&[Encoder]>) -> Detection {
        let mut detection = detection(h264);
        detection.hevc = hevc.map(|working| {
            GPU.into_iter()
                .map(|e| {
                    let outcome = if working.contains(&e) {
                        Outcome::Works
                    } else {
                        Outcome::Failed("No capable devices found".into())
                    };
                    (e, outcome)
                })
                .collect()
        });
        detection
    }

    #[test]
    fn hevc_comes_from_the_gpu_only_whatever_the_cpu_preference() {
        let both = detection_with_hevc(
            &[Encoder::Nvenc, Encoder::OpenH264],
            Some(&[Encoder::Nvenc]),
        );
        for prefer_cpu in [false, true] {
            assert_eq!(
                both.choose_for(VideoCodec::Hevc, prefer_cpu),
                Some(Encoder::Nvenc)
            );
        }
        assert_eq!(
            both.choose_for(VideoCodec::H264, true),
            Some(Encoder::OpenH264)
        );

        // Not tested yet, or tested and none works: no HEVC encoder.
        let untested = detection_with_hevc(&[Encoder::Nvenc], None);
        assert_eq!(untested.choose_for(VideoCodec::Hevc, false), None);
        let none = detection_with_hevc(&[Encoder::Nvenc, Encoder::X264], Some(&[]));
        assert_eq!(none.hevc_gpu(), None);
        assert_eq!(
            none.choose_for(VideoCodec::H264, false),
            Some(Encoder::Nvenc)
        );
    }

    #[test]
    fn the_hevc_retry_stays_on_a_hevc_encoder() {
        let two = detection_with_hevc(
            &[Encoder::Nvenc, Encoder::X264],
            Some(&[Encoder::Nvenc, Encoder::Qsv]),
        );
        assert_eq!(
            two.alternative_for(Encoder::Nvenc, VideoCodec::Hevc),
            Some(Encoder::Qsv)
        );
        // The H.264 alternative is a CPU encoder: never offered for HEVC.
        assert_eq!(
            two.alternative_for(Encoder::Nvenc, VideoCodec::H264),
            Some(Encoder::X264)
        );
        let one = detection_with_hevc(&[Encoder::X264], Some(&[Encoder::Nvenc]));
        assert_eq!(one.alternative_for(Encoder::Nvenc, VideoCodec::Hevc), None);
    }

    #[test]
    fn encoder_names_per_codec_round_trip_to_the_family() {
        for encoder in Encoder::all() {
            for codec in [VideoCodec::H264, VideoCodec::Hevc] {
                if let Some(name) = encoder.codec_name(codec) {
                    assert_eq!(Encoder::parse(name), Some(encoder), "{name}");
                }
            }
        }
        assert_eq!(
            Encoder::Nvenc.codec_name(VideoCodec::Hevc),
            Some("hevc_nvenc")
        );
        assert_eq!(Encoder::Amf.codec_name(VideoCodec::H264), Some("h264_amf"));
        assert_eq!(
            Encoder::parse("hevc_mf"),
            None,
            "not an encoder the app uses"
        );
        assert_eq!(Encoder::parse("libx265"), None);
    }

    #[test]
    fn hevc_test_encodes_are_hevc_in_an_mp4_with_the_hvc1_tag() {
        for encoder in GPU {
            let options = encoder.test_options(VideoCodec::Hevc);
            assert!(
                options
                    .windows(2)
                    .any(|w| w[0] == "-c:v" && w[1].starts_with("hevc_")),
                "{options:?}"
            );
            assert!(options.windows(2).any(|w| w == ["-tag:v", "hvc1"]));
        }
        // An encoder without a HEVC variant has nothing to test.
        assert!(Encoder::X264.test_options(VideoCodec::Hevc).is_empty());
    }

    #[test]
    fn names_round_trip() {
        for encoder in GPU.into_iter().chain(CPU) {
            assert_eq!(Encoder::parse(encoder.name()), Some(encoder));
        }
        assert_eq!(Encoder::parse("h264_foo"), None);
    }
}
