//! Processing plan: pure functions from the probe result and the limits to what to run.
//! No I/O here.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::probe::{MediaInfo, VideoInfo};
use crate::queue::store::SendMode;

pub const MIB: u64 = 1024 * 1024;
/// AAC `-b:a 128k` measures 128.8 kbit/s.
pub const AUDIO_REAL_FACTOR: f64 = 1.006;
/// MP4 `+faststart` container overhead, measured 8.7 kbit/s.
pub const CONTAINER_OVERHEAD_BPS: f64 = 9_000.0;
/// Guard rail below which a video is not worth encoding (assumed, not measured).
pub const MIN_VIDEO_BPS: u64 = 150_000;
/// A photo above this goes through the quality loop (Telegram: 10 MB).
pub const PHOTO_MAX_BYTES: u64 = 9_961_472; // 9.5 MiB
const PHOTO_MAX_SUM: f64 = 9800.0;
const PHOTO_MAX_SIDE: f64 = 4096.0;
/// Beyond this ratio Telegram refuses a photo.
const PHOTO_MAX_RATIO: f64 = 19.0;
const MAX_WIDTH_FOR_COPY: u32 = 4096;
const IMAGE_EXTENSIONS: [&str; 7] = ["jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff"];

/// Video bitrate (bit/s) that lands the file under `limit_bytes` with the safety margin.
/// `None`: not even the minimum bitrate fits (OUTPUT_TOO_LARGE before encoding).
pub fn plan_bitrate(
    limit_bytes: u64,
    margin: f64,
    duration_s: f64,
    audio_kbps: Option<u32>,
) -> Option<u64> {
    if duration_s <= 0.0 {
        return None;
    }
    let target = limit_bytes as f64 * (1.0 - margin);
    let audio = audio_kbps.map_or(0.0, |k| f64::from(k) * 1000.0 * AUDIO_REAL_FACTOR);
    let video = target * 8.0 / duration_s - audio - CONTAINER_OVERHEAD_BPS;
    (video >= MIN_VIDEO_BPS as f64).then_some(video as u64)
}

/// Bitrate of the one corrective pass after a file came out over the limit.
pub fn corrective_video_bps(previous: u64, limit_bytes: u64, real_bytes: u64) -> Option<u64> {
    if real_bytes <= limit_bytes {
        return None;
    }
    let video = (previous as f64 * (limit_bytes as f64 / real_bytes as f64) * 0.97) as u64;
    (video >= MIN_VIDEO_BPS).then_some(video)
}

/// Plan of the CPU retry after a GPU encoder stopped mid-encode: same streams and filters, the
/// fallback `encoder`, and the video bitrate recomputed with the current `margin`.
///
/// The formula is measured on NVENC and on x264 (which runs 2 passes
/// with `maxrate = bitrate`, see `encode::video_options`). libopenh264 and h264_mf are NOT
/// measured: their rate control is the prudent CBR-like scheme and the corrective pass catches
/// a drift. Without a duration the planned bitrate is kept.
pub fn for_fallback(plan: &Plan, encoder: &str, margin: f64) -> Plan {
    let mut next = plan.clone();
    next.encoder = Some(encoder.to_owned());
    if let Some(duration) = plan.duration_s
        && let Some(video) = plan_bitrate(plan.limit_bytes, margin, duration, plan.audio_kbps)
    {
        next.video_bps = Some(video);
    }
    next
}

/// `<radical>_sq.<ext>` (never derived by trimming characters from the path).
pub fn output_name(source: &Path, ext: &str) -> String {
    let stem = source
        .file_stem()
        .map_or_else(|| "output".into(), |s| s.to_string_lossy());
    format!("{stem}_sq.{ext}")
}

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Longest and shortest side of the downscale cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionCap {
    P1080,
    P1440,
    /// Keeps the source size, within what NVENC accepts (4096).
    Original,
}

impl ResolutionCap {
    fn long_short(self) -> (u32, u32) {
        match self {
            ResolutionCap::P1080 => (1920, 1080),
            ResolutionCap::P1440 => (2560, 1440),
            ResolutionCap::Original => (4096, 4096),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Re-encode to the target size.
    Encode,
    /// `-c copy` into MP4.
    Remux,
    /// Video copied, audio re-encoded to AAC.
    RemuxAudio,
    /// Already sendable: the source is the output.
    Skip,
    Photo,
}

impl Kind {
    /// Serialized name, for the journal.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Encode => "encode",
            Kind::Remux => "remux",
            Kind::RemuxAudio => "remux_audio",
            Kind::Skip => "skip",
            Kind::Photo => "photo",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhotoSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub kind: Kind,
    pub limit_bytes: u64,
    pub duration_s: Option<f64>,
    pub video_bps: Option<u64>,
    /// Set when the audio is (re-)encoded to AAC.
    pub audio_kbps: Option<u32>,
    pub video_index: Option<u32>,
    pub audio_index: Option<u32>,
    /// `-vf` chain of an encode.
    pub vf: Option<String>,
    pub hdr: bool,
    /// Chosen encoder (`h264_nvenc`, `libopenh264`…), filled in by the stage.
    pub encoder: Option<String>,
    pub output_name: String,
    pub photo: Option<PhotoSize>,
    /// Coded size of a photo source (needed to lay it over a white background).
    pub source_dims: Option<(u32, u32)>,
    pub alpha: bool,
    /// Why a photo was left alone, for the log.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// OUTPUT_TOO_LARGE: nothing can fit.
    #[error("the output cannot fit under the limit")]
    TooLarge,
    /// FILE_UNREADABLE: no usable video/image, or no duration to size an encode.
    #[error("the file has no usable media")]
    Unreadable,
}

impl PlanError {
    pub fn code(self) -> &'static str {
        match self {
            PlanError::TooLarge => "OUTPUT_TOO_LARGE",
            PlanError::Unreadable => "FILE_UNREADABLE",
        }
    }
}

pub struct PlanInput<'a> {
    pub source: &'a Path,
    pub source_size: u64,
    pub send_mode: SendMode,
    /// The effective limit (`crate::limit`), before the margin.
    pub limit_bytes: u64,
    /// 0.03 for 3 %.
    pub margin: f64,
    pub audio_kbps: u32,
    pub cap: ResolutionCap,
    /// Result of `probe::moov_first` for an MP4, if known.
    pub moov_first: Option<bool>,
}

fn empty_plan(kind: Kind, input: &PlanInput<'_>, ext: &str) -> Plan {
    Plan {
        kind,
        limit_bytes: input.limit_bytes,
        duration_s: None,
        video_bps: None,
        audio_kbps: None,
        video_index: None,
        audio_index: None,
        vf: None,
        hdr: false,
        encoder: None,
        output_name: output_name(input.source, ext),
        photo: None,
        source_dims: None,
        alpha: false,
        note: None,
    }
}

/// Decides what to do with a file. `info` is `None` only for the document mode, which does
/// not probe.
pub fn build_plan(info: Option<&MediaInfo>, input: &PlanInput<'_>) -> Result<Plan, PlanError> {
    if input.send_mode == SendMode::Document {
        if input.source_size > input.limit_bytes {
            return Err(PlanError::TooLarge);
        }
        return Ok(empty_plan(Kind::Skip, input, "bin"));
    }
    let info = info.ok_or(PlanError::Unreadable)?;
    let video = info.video.as_ref().ok_or(PlanError::Unreadable)?;
    if is_image(input.source) {
        return plan_photo(video, input);
    }

    let audio = info.audio.as_ref();
    let mut plan = empty_plan(Kind::Encode, input, "mp4");
    plan.duration_s = info.duration_s;
    plan.video_index = Some(video.index);
    plan.audio_index = audio.map(|a| a.index);

    let fits = input.source_size <= input.limit_bytes;
    let audio_is_aac = audio.is_none_or(|a| a.codec == "aac");
    // The decision comes from ffprobe, never from ffmpeg succeeding: it also "succeeds" on
    // 4:4:4, 10-bit, HEVC and VP9.
    if copy_compatible(video) && fits {
        let is_mp4 = input
            .source
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
        if audio_is_aac && is_mp4 && input.moov_first == Some(true) {
            plan.kind = Kind::Skip;
        } else if audio_is_aac {
            plan.kind = Kind::Remux;
        } else {
            plan.kind = Kind::RemuxAudio;
            plan.audio_kbps = Some(input.audio_kbps);
        }
        return Ok(plan);
    }

    let duration = info.duration_s.ok_or(PlanError::Unreadable)?;
    let audio_kbps = audio.map(|_| input.audio_kbps);
    plan.video_bps = Some(
        plan_bitrate(input.limit_bytes, input.margin, duration, audio_kbps)
            .ok_or(PlanError::TooLarge)?,
    );
    plan.audio_kbps = audio_kbps;
    plan.hdr = video.hdr;
    plan.vf = Some(video_filter(video, input.cap));
    Ok(plan)
}

/// H.264 8-bit 4:2:0 progressive, at most 4096 wide: sendable without re-encoding.
fn copy_compatible(video: &VideoInfo) -> bool {
    video.codec == "h264"
        && matches!(
            video.profile.as_deref(),
            Some("Constrained Baseline" | "Baseline" | "Main" | "High")
        )
        && video.pix_fmt.as_deref() == Some("yuv420p")
        && !video.interlaced()
        && video.rotated_size().0 <= MAX_WIDTH_FOR_COPY
}

/// The `-vf` chain: deinterlace if needed, square pixels, then the resolution cap (never an
/// upscale, even dimensions), then HDR to SDR.
pub fn video_filter(video: &VideoInfo, cap: ResolutionCap) -> String {
    let (long, short) = cap.long_short();
    let mut chain = Vec::new();
    if video.interlaced() {
        chain.push("yadif=mode=0:deint=interlaced".to_owned());
    }
    chain.push("scale=w='trunc(iw*sar/2)*2':h=ih,setsar=1".to_owned());
    chain.push(format!(
        "scale=w='if(gt(iw,ih),min({long},iw),min({short},iw))':h='if(gt(iw,ih),min({short},ih),min({long},ih))':force_original_aspect_ratio=decrease:force_divisible_by=2"
    ));
    if video.hdr {
        chain.push(
            "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p"
                .to_owned(),
        );
    }
    chain.join(",")
}

fn plan_photo(video: &VideoInfo, input: &PlanInput<'_>) -> Result<Plan, PlanError> {
    let (width, height) = video.rotated_size();
    if width == 0 || height == 0 {
        return Err(PlanError::Unreadable);
    }
    let mut plan = empty_plan(Kind::Photo, input, "jpg");
    plan.video_index = Some(video.index);
    plan.alpha = video.has_alpha();
    plan.source_dims = Some((video.width, video.height));
    let (long, short) = (width.max(height), width.min(height));
    if f64::from(long) / f64::from(short) > PHOTO_MAX_RATIO {
        // Telegram would refuse it as a photo: left as is, for the transport to send as a file.
        plan.kind = Kind::Skip;
        plan.output_name = input
            .source
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        plan.note = Some("photo_ratio".into());
        return Ok(plan);
    }
    plan.photo = Some(photo_size(width, height));
    Ok(plan)
}

/// Dimensions after the Telegram limits: `f = min(1, 9800/(w+h), 4096/max(w,h))`, even.
pub fn photo_size(width: u32, height: u32) -> PhotoSize {
    let (w, h) = (f64::from(width), f64::from(height));
    let f = (PHOTO_MAX_SUM / (w + h))
        .min(PHOTO_MAX_SIDE / w.max(h))
        .min(1.0);
    PhotoSize {
        width: even(w * f),
        height: even(h * f),
    }
}

/// Scales a photo size down by `factor`, keeping even dimensions.
pub fn shrink(size: PhotoSize, factor: f64) -> PhotoSize {
    PhotoSize {
        width: even(f64::from(size.width) * factor),
        height: even(f64::from(size.height) * factor),
    }
}

fn even(value: f64) -> u32 {
    (((value + 1e-6) as u32) & !1).max(2)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::media::probe::AudioInfo;

    fn video(codec: &str, profile: &str, pix_fmt: &str) -> VideoInfo {
        VideoInfo {
            index: 0,
            codec: codec.into(),
            profile: Some(profile.into()),
            width: 1920,
            height: 1080,
            sar: Some((1, 1)),
            pix_fmt: Some(pix_fmt.into()),
            field_order: Some("progressive".into()),
            color_transfer: None,
            rotation: 0,
            bit_rate: None,
            hdr: false,
        }
    }

    fn info(video: VideoInfo, audio: Option<&str>, duration: Option<f64>) -> MediaInfo {
        MediaInfo {
            format_name: Some("mov,mp4".into()),
            duration_s: duration,
            bit_rate: None,
            video: Some(video),
            audio: audio.map(|codec| AudioInfo {
                index: 1,
                codec: codec.into(),
                channels: Some(2),
                bit_rate: None,
            }),
        }
    }

    fn input<'a>(source: &'a Path, size: u64) -> PlanInput<'a> {
        PlanInput {
            source,
            source_size: size,
            send_mode: SendMode::Video,
            limit_bytes: 100 * MIB,
            margin: 0.03,
            audio_kbps: 128,
            cap: ResolutionCap::P1080,
            moov_first: Some(true),
        }
    }

    #[test]
    fn bitrate_follows_the_spec_formula() {
        // 100 MiB, 3 %, 600 s, AAC 128 k: 101 711 872 * 8 / 600 - 128 768 - 9 000.
        let expected = (104_857_600.0_f64 * 0.97 * 8.0 / 600.0 - 128_768.0 - 9_000.0) as u64;
        assert_eq!(
            plan_bitrate(100 * MIB, 0.03, 600.0, Some(128)),
            Some(expected)
        );
        assert_eq!(expected, 1_218_390);
        // Without audio the 128 k are given back to the video.
        let silent = plan_bitrate(100 * MIB, 0.03, 600.0, None).unwrap();
        assert_eq!(silent - expected, 128_768);
    }

    #[test]
    fn a_budget_below_the_minimum_is_refused_before_encoding() {
        assert_eq!(plan_bitrate(10 * MIB, 0.03, 36_000.0, Some(128)), None);
        assert_eq!(plan_bitrate(10 * MIB, 0.03, 0.0, None), None);
    }

    #[test]
    fn corrective_pass_scales_the_bitrate_below_the_limit() {
        // 4 % over: 5 Mbit/s * (100/104) * 0.97.
        let corrected = corrective_video_bps(5_000_000, 100 * MIB, 104 * MIB).unwrap();
        assert_eq!(corrected, (5_000_000.0 * (100.0 / 104.0) * 0.97) as u64);
        assert_eq!(corrective_video_bps(5_000_000, 100 * MIB, 100 * MIB), None);
        assert_eq!(corrective_video_bps(200_000, 100 * MIB, 200 * MIB), None);
    }

    #[test]
    fn the_fallback_plan_keeps_the_streams_and_recomputes_the_bitrate() {
        let source = PathBuf::from("big.mkv");
        let info = info(video("hevc", "Main", "yuv420p"), Some("aac"), Some(600.0));
        let mut plan = build_plan(Some(&info), &input(&source, 300 * MIB)).unwrap();
        plan.encoder = Some("h264_nvenc".into());
        // A margin changed since the probe is taken into account.
        let next = for_fallback(&plan, "libx264", 0.05);
        assert_eq!(next.encoder.as_deref(), Some("libx264"));
        assert_eq!(
            next.video_bps,
            plan_bitrate(100 * MIB, 0.05, 600.0, Some(128))
        );
        assert_eq!(
            (&next.vf, next.audio_kbps, next.output_name.as_str()),
            (&plan.vf, plan.audio_kbps, "big_sq.mp4")
        );
        // Same margin: same bitrate (the formula does not depend on the encoder).
        assert_eq!(
            for_fallback(&plan, "libopenh264", 0.03).video_bps,
            plan.video_bps
        );
        plan.duration_s = None;
        assert_eq!(
            for_fallback(&plan, "h264_mf", 0.05).video_bps,
            plan.video_bps
        );
    }

    #[test]
    fn output_names_append_sq_to_the_stem() {
        let cases = [
            ("C:\\v\\clip.mkv", "mp4", "clip_sq.mp4"),
            ("C:\\v\\a.b.mkv", "mp4", "a.b_sq.mp4"),
            ("C:\\v\\été ★ 日本語.mov", "mp4", "été ★ 日本語_sq.mp4"),
            ("C:\\v\\photo.PNG", "jpg", "photo_sq.jpg"),
            ("C:\\v\\noext", "mp4", "noext_sq.mp4"),
            ("C:\\v\\a_sq.mp4", "mp4", "a_sq_sq.mp4"),
        ];
        for (source, ext, expected) in cases {
            assert_eq!(
                output_name(&PathBuf::from(source), ext),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn a_conforming_mp4_under_the_limit_is_sent_as_is() {
        let source = PathBuf::from("in.mp4");
        let info = info(video("h264", "High", "yuv420p"), Some("aac"), Some(60.0));
        let plan = build_plan(Some(&info), &input(&source, 10 * MIB)).unwrap();
        assert_eq!(plan.kind, Kind::Skip);
        // moov at the end: a remux (faststart) fixes it.
        let mut slow = input(&source, 10 * MIB);
        slow.moov_first = Some(false);
        assert_eq!(build_plan(Some(&info), &slow).unwrap().kind, Kind::Remux);
    }

    #[test]
    fn remux_is_decided_by_ffprobe_not_by_the_container() {
        let source = PathBuf::from("in.mkv");
        let ok = info(video("h264", "Main", "yuv420p"), Some("aac"), Some(60.0));
        assert_eq!(
            build_plan(Some(&ok), &input(&source, MIB)).unwrap().kind,
            Kind::Remux
        );

        let ac3 = info(video("h264", "Main", "yuv420p"), Some("ac3"), Some(60.0));
        let plan = build_plan(Some(&ac3), &input(&source, MIB)).unwrap();
        assert_eq!((plan.kind, plan.audio_kbps), (Kind::RemuxAudio, Some(128)));

        // ffmpeg would copy all of these without complaint; they must be re-encoded.
        for (codec, profile, pix_fmt) in [
            ("hevc", "Main", "yuv420p"),
            ("vp9", "Profile 0", "yuv420p"),
            ("h264", "High 4:4:4 Predictive", "yuv444p"),
            ("h264", "High 10", "yuv420p10le"),
        ] {
            let bad = info(video(codec, profile, pix_fmt), Some("aac"), Some(60.0));
            let plan = build_plan(Some(&bad), &input(&source, MIB)).unwrap();
            assert_eq!(plan.kind, Kind::Encode, "{codec} {profile} {pix_fmt}");
            assert!(plan.video_bps.is_some());
        }

        let mut interlaced = video("h264", "High", "yuv420p");
        interlaced.field_order = Some("tt".into());
        let plan = build_plan(
            Some(&info(interlaced, Some("aac"), Some(60.0))),
            &input(&source, MIB),
        )
        .unwrap();
        assert_eq!(plan.kind, Kind::Encode);
        assert!(plan.vf.unwrap().starts_with("yadif"));
    }

    #[test]
    fn a_conforming_file_over_the_limit_is_encoded() {
        let source = PathBuf::from("big.mp4");
        let info = info(video("h264", "High", "yuv420p"), Some("aac"), Some(600.0));
        let plan = build_plan(Some(&info), &input(&source, 300 * MIB)).unwrap();
        assert_eq!(plan.kind, Kind::Encode);
        assert_eq!(plan.video_bps, Some(1_218_390));
        assert_eq!(plan.audio_kbps, Some(128));
        assert_eq!(plan.output_name, "big_sq.mp4");
    }

    #[test]
    fn encoding_needs_a_duration_and_a_feasible_budget() {
        let source = PathBuf::from("a.mkv");
        let no_duration = info(video("hevc", "Main", "yuv420p"), Some("aac"), None);
        assert_eq!(
            build_plan(Some(&no_duration), &input(&source, MIB)).unwrap_err(),
            PlanError::Unreadable
        );
        let long = info(video("hevc", "Main", "yuv420p"), None, Some(1_000_000.0));
        assert_eq!(
            build_plan(Some(&long), &input(&source, MIB)).unwrap_err(),
            PlanError::TooLarge
        );
        // No audio: the audio bitrate is not reserved.
        let silent = info(video("hevc", "Main", "yuv420p"), None, Some(600.0));
        let plan = build_plan(Some(&silent), &input(&source, MIB)).unwrap();
        assert_eq!((plan.audio_kbps, plan.audio_index), (None, None));
    }

    #[test]
    fn filter_squares_pixels_before_capping_and_tonemaps_hdr() {
        let mut v = video("hevc", "Main 10", "yuv420p10le");
        v.hdr = true;
        let vf = video_filter(&v, ResolutionCap::P1440);
        let square = vf.find("setsar=1").unwrap();
        let cap = vf.find("min(2560,iw)").unwrap();
        let tonemap = vf.find("tonemap=").unwrap();
        assert!(square < cap && cap < tonemap, "{vf}");
        assert!(!vf.contains("yadif"));
        assert!(
            video_filter(&video("h264", "High", "yuv420p"), ResolutionCap::Original)
                .contains("min(4096,iw)")
        );
    }

    #[test]
    fn document_mode_never_encodes() {
        let source = PathBuf::from("a.mkv");
        let mut doc = input(&source, 50 * MIB);
        doc.send_mode = SendMode::Document;
        assert_eq!(build_plan(None, &doc).unwrap().kind, Kind::Skip);
        doc.source_size = 200 * MIB;
        assert_eq!(build_plan(None, &doc).unwrap_err(), PlanError::TooLarge);
    }

    #[test]
    fn photos_are_capped_by_the_telegram_limits() {
        assert_eq!(
            photo_size(4032, 3024),
            PhotoSize {
                width: 4032,
                height: 3024
            }
        );
        // 16042x3852: sum 19 894 -> f = 0.4926 (and 4096/16042 = 0.2553 binds).
        let pano = photo_size(16042, 3852);
        assert!(pano.width <= 4096 && pano.width % 2 == 0 && pano.height % 2 == 0);
        assert!(pano.width >= 4094);
        let big = photo_size(8000, 6000);
        assert_eq!((big.width, big.height), (4096, 3072));
        assert_eq!(
            shrink(big, 0.8),
            PhotoSize {
                width: 3276,
                height: 2456
            }
        );
    }

    #[test]
    fn photo_plans_follow_extension_ratio_and_alpha() {
        let source = PathBuf::from("pic.PNG");
        let mut v = video("png", "", "rgba");
        v.width = 4000;
        v.height = 3000;
        v.rotation = 90;
        let plan = build_plan(Some(&info(v.clone(), None, None)), &input(&source, MIB)).unwrap();
        assert_eq!(plan.kind, Kind::Photo);
        assert!(plan.alpha);
        // Rotated by 90: 3000 x 4000, within 4096 and 9800.
        assert_eq!(
            plan.photo,
            Some(PhotoSize {
                width: 3000,
                height: 4000
            })
        );
        assert_eq!(plan.output_name, "pic_sq.jpg");

        v.width = 20_000;
        v.height = 500;
        v.rotation = 0;
        let plan = build_plan(Some(&info(v, None, None)), &input(&source, MIB)).unwrap();
        assert_eq!(
            (plan.kind, plan.note.as_deref()),
            (Kind::Skip, Some("photo_ratio"))
        );
    }
}
