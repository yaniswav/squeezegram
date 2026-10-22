//! Processing plan: pure functions from the probe result and the limits to what to run.
//! No I/O here.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::probe::{MediaInfo, VideoInfo};
use crate::caps::Caps;
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
        && let Some(video) = plan_bitrate(plan.aim_bytes(), margin, duration, plan.audio_kbps)
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

/// Longest and shortest side of the downscale cap; named as in `settings.advanced_encoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionCap {
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "2160p")]
    P2160,
    /// Keeps the source size, within what NVENC accepts (4096).
    #[serde(rename = "original")]
    Original,
}

impl ResolutionCap {
    fn long_short(self) -> (u32, u32) {
        match self {
            ResolutionCap::P1080 => (1920, 1080),
            ResolutionCap::P1440 => (2560, 1440),
            ResolutionCap::P2160 => (3840, 2160),
            ResolutionCap::Original => (4096, 4096),
        }
    }
}

/// Video codec of an encode, named as in `settings.advanced_encoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VideoCodec {
    #[default]
    #[serde(rename = "h264")]
    H264,
    /// GPU encoders only (`hevc_nvenc`, `hevc_qsv`, `hevc_amf`): the pinned LGPL ffmpeg has no
    /// libx265. Tagged `hvc1` in the MP4.
    #[serde(rename = "hevc")]
    Hevc,
}

impl VideoCodec {
    /// Serialized name, for the journal.
    pub fn as_str(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
        }
    }
}

/// Speed/quality trade-off of the encoder, mapped per encoder family (`encode::video_options`).
/// `Balanced` is what every encode used before the choice existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncodePreset {
    Faster,
    #[default]
    Balanced,
    Quality,
}

impl EncodePreset {
    /// Serialized name, for the journal.
    pub fn as_str(self) -> &'static str {
        match self {
            EncodePreset::Faster => "faster",
            EncodePreset::Balanced => "balanced",
            EncodePreset::Quality => "quality",
        }
    }
}

/// Custom target sizes offered, in MiB. The top is the largest limit Telegram has (Premium):
/// a target at or above the effective limit is the same as filling it.
pub const TARGET_MB_RANGE: std::ops::RangeInclusive<u64> = 1..=4000;

/// Encoding choices of one queue item (`items.encoding_override`, F14). A complete set, not a
/// diff: what the page shows when the user saves is what the encode uses, whatever the global
/// settings become afterwards. Read only while the license grants `advanced_encoding`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncodingOverride {
    pub video_codec: VideoCodec,
    pub max_resolution: ResolutionCap,
    /// `None`: fill the limit.
    pub target_mb: Option<u64>,
}

impl EncodingOverride {
    pub fn is_valid(&self) -> bool {
        self.target_mb
            .is_none_or(|mb| TARGET_MB_RANGE.contains(&mb))
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
    /// Sendable as it is, but carrying location or camera data: a copy without them, pixels
    /// untouched: an image rewritten without its metadata (`metadata`), a video copied stream
    /// for stream without any (`PRIVATE_TAGS`).
    Strip,
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
            Kind::Strip => "strip",
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
    /// Codec of an encode. Plans stored before the choice existed read as H.264.
    #[serde(default)]
    pub video_codec: VideoCodec,
    #[serde(default)]
    pub preset: EncodePreset,
    /// Size the user asked for, below `limit_bytes`; `None`: the encode fills the limit.
    #[serde(default)]
    pub target_bytes: Option<u64>,
    /// `PRIVATE_TAGS` copy: the language of each stream it writes, in output order (`None`:
    /// none to give back).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_languages: Vec<Option<String>>,
}

impl Plan {
    /// The size an encode aims under: the target, else the limit. The bitrate, the corrective
    /// pass and the first size check use it; the limit stays the hard ceiling.
    pub fn aim_bytes(&self) -> u64 {
        self.target_bytes
            .map_or(self.limit_bytes, |target| target.min(self.limit_bytes))
    }

    /// The size an encode of this plan is expected to come out at: the planned bitrates over
    /// the duration (what `plan_bitrate` solves for the aim, less the margin). `None` for a
    /// plan that does not encode a video.
    pub fn estimated_bytes(&self) -> Option<u64> {
        if self.kind != Kind::Encode {
            return None;
        }
        let audio = self
            .audio_kbps
            .map_or(0.0, |k| f64::from(k) * 1000.0 * AUDIO_REAL_FACTOR);
        let bps = self.video_bps? as f64 + audio + CONTAINER_OVERHEAD_BPS;
        Some((bps * self.duration_s? / 8.0) as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum PlanError {
    /// OUTPUT_TOO_LARGE: nothing can fit.
    #[error("the output cannot fit under the limit")]
    TooLarge,
    /// FILE_UNREADABLE: no usable video/image, or no duration to size an encode.
    #[error("the file has no usable media")]
    Unreadable,
    /// PHOTO_TOO_LARGE, VIDEO_TOO_LARGE or VIDEO_TOO_LONG: over a safety cap (`caps`), refused
    /// before any work. `found` and `max` are in the unit of the message: Mpx, or hours.
    #[error("{code}: {found} over the maximum of {max}")]
    Capped {
        code: &'static str,
        found: f64,
        max: f64,
    },
}

impl PlanError {
    pub fn code(self) -> &'static str {
        match self {
            PlanError::TooLarge => "OUTPUT_TOO_LARGE",
            PlanError::Unreadable => "FILE_UNREADABLE",
            PlanError::Capped { code, .. } => code,
        }
    }
}

/// `Capped` when a frame of `pixels` is over `max`; the value is rounded up so that it never
/// reads as within the cap.
fn check_pixels(code: &'static str, pixels: u64, max: u64) -> Result<(), PlanError> {
    if pixels > max {
        return Err(PlanError::Capped {
            code,
            found: (pixels as f64 / 1e5).ceil() / 10.0,
            max: max as f64 / 1e6,
        });
    }
    Ok(())
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
    pub codec: VideoCodec,
    pub preset: EncodePreset,
    /// Custom target size in bytes (`None`: fill the limit). At or above the limit it is the
    /// same as `None`.
    pub target_bytes: Option<u64>,
    /// Result of `probe::moov_first` for an MP4, if known.
    pub moov_first: Option<bool>,
    /// The safety caps in effect (`stage::caps_in_effect`).
    pub caps: Caps,
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
        video_codec: VideoCodec::default(),
        preset: EncodePreset::default(),
        target_bytes: None,
        stream_languages: Vec::new(),
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

    // A target below the limit is what the output must fit under: a file already under it is
    // sent as it is, a larger one is encoded even if Telegram would take it.
    let target = input
        .target_bytes
        .filter(|bytes| *bytes < input.limit_bytes);
    let aim = target.unwrap_or(input.limit_bytes);
    let fits = input.source_size <= aim;
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

    // Only a file that is decoded and encoded costs memory and time: a copy does not.
    let (width, height) = video.rotated_size();
    check_pixels(
        "VIDEO_TOO_LARGE",
        u64::from(width) * u64::from(height),
        input.caps.video_pixels,
    )?;
    if let Some(seconds) = info
        .duration_s
        .filter(|s| *s > input.caps.max_duration_s as f64)
    {
        return Err(PlanError::Capped {
            code: "VIDEO_TOO_LONG",
            found: (seconds / 360.0).ceil() / 10.0,
            max: input.caps.max_duration_s as f64 / 3600.0,
        });
    }
    let duration = info.duration_s.ok_or(PlanError::Unreadable)?;
    let audio_kbps = audio.map(|_| input.audio_kbps);
    plan.video_bps =
        Some(plan_bitrate(aim, input.margin, duration, audio_kbps).ok_or(PlanError::TooLarge)?);
    plan.video_codec = input.codec;
    plan.preset = input.preset;
    plan.target_bytes = target;
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
    // The decode of a photo with transparency or more than 8 bits per color costs about 18 bytes
    // per pixel, twice a plain one at most: its own, lower cap.
    let max = if video.has_alpha() || video.deep_color() {
        input.caps.photo_wide_pixels
    } else {
        input.caps.photo_pixels
    };
    check_pixels("PHOTO_TOO_LARGE", u64::from(width) * u64::from(height), max)?;
    plan.photo = Some(photo_size(width, height));
    Ok(plan)
}

/// What a file the plan sends as it is (`Skip`) holds that "Remove location and camera data"
/// takes out; the stage finds it (first bytes, ffprobe tags), the plan stays free of I/O.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrivateData {
    /// A JPEG, PNG or WebP (`metadata::sniff`), small enough to be read whole.
    pub image: bool,
    /// The file names a place or a camera, or carries a data track (`MediaInfo::private_tags`).
    pub video_tags: bool,
    /// The language of each stream the copy keeps (`copied_languages`).
    pub languages: Vec<Option<String>>,
}

/// The streams the `PRIVATE_TAGS` copy writes (`encode::strip_video_args`: data tracks and
/// covers, attachments and streams of an unknown kind are left out), each with its language,
/// in output order.
pub fn copied_languages(info: &MediaInfo) -> Vec<Option<String>> {
    info.streams
        .iter()
        .filter(|stream| {
            matches!(stream.kind.as_str(), "video" | "audio" | "subtitle") && !stream.attached_pic
        })
        .map(|stream| stream.language.clone())
        .collect()
}

/// `Plan::note` of a video sent as it is but for its metadata (`encode::strip`).
pub const PRIVATE_TAGS: &str = "private_tags";

/// A `Skip` plan when the user removes location and camera data: a stripped copy under the
/// source's own name (the recipient sees the same file). An image is rewritten without its
/// metadata segments; a video whose file carries such tags is copied stream for stream without
/// any metadata (`PRIVATE_TAGS`). Any other plan, and any other file (HEIC, a video in
/// document mode, which is not probed), is left as it is.
pub fn without_private_data(mut plan: Plan, source: &Path, private: PrivateData) -> Plan {
    if plan.kind != Kind::Skip {
        return plan;
    }
    let video = private.video_tags && plan.video_index.is_some() && !is_image(source);
    if !private.image && !video {
        return plan;
    }
    plan.kind = Kind::Strip;
    plan.output_name = source.file_name().map_or_else(
        || plan.output_name.clone(),
        |n| n.to_string_lossy().into_owned(),
    );
    if video {
        plan.note = Some(PRIVATE_TAGS.into());
        plan.stream_languages = private.languages;
    }
    plan
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
            private_tags: false,
            streams: Vec::new(),
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
            codec: VideoCodec::H264,
            preset: EncodePreset::Balanced,
            target_bytes: None,
            moov_first: Some(true),
            caps: Caps::default(),
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

    /// A conforming H.264 file of `size` and 600 s, planned with the given target/codec/preset.
    fn encode_plan(
        size: u64,
        target_mb: Option<u64>,
        codec: VideoCodec,
        preset: EncodePreset,
    ) -> Plan {
        let source = PathBuf::from("clip.mp4");
        let info = info(video("h264", "High", "yuv420p"), Some("aac"), Some(600.0));
        let mut wanted = input(&source, size);
        wanted.target_bytes = target_mb.map(|mb| mb * MIB);
        wanted.codec = codec;
        wanted.preset = preset;
        build_plan(Some(&info), &wanted).unwrap()
    }

    #[test]
    fn a_custom_target_sets_the_bitrate_and_keeps_the_limit_as_the_ceiling() {
        let plan = encode_plan(
            300 * MIB,
            Some(40),
            VideoCodec::H264,
            EncodePreset::Balanced,
        );
        assert_eq!(plan.kind, Kind::Encode);
        // The margin applies to the target as it does to the limit.
        assert_eq!(
            plan.video_bps,
            plan_bitrate(40 * MIB, 0.03, 600.0, Some(128))
        );
        assert_eq!(
            plan.limit_bytes,
            100 * MIB,
            "the upload check keeps the real limit"
        );
        assert_eq!(plan.target_bytes, Some(40 * MIB));
        assert_eq!(plan.aim_bytes(), 40 * MIB);
        // The estimate follows the aim less the margin.
        let estimate = plan.estimated_bytes().unwrap() as f64;
        let expected = 40.0 * MIB as f64 * 0.97;
        assert!((estimate - expected).abs() / expected < 0.001, "{estimate}");
    }

    #[test]
    fn a_target_at_or_above_the_limit_is_the_same_as_filling_the_limit() {
        let fill = encode_plan(300 * MIB, None, VideoCodec::H264, EncodePreset::Balanced);
        for mb in [100, 4000] {
            let plan = encode_plan(
                300 * MIB,
                Some(mb),
                VideoCodec::H264,
                EncodePreset::Balanced,
            );
            assert_eq!(plan, fill, "target {mb} MiB");
        }
        assert_eq!(fill.target_bytes, None);
        assert_eq!(fill.aim_bytes(), 100 * MIB);
    }

    #[test]
    fn a_file_under_the_target_is_sent_as_is_and_one_above_it_is_encoded() {
        let under = encode_plan(30 * MIB, Some(40), VideoCodec::H264, EncodePreset::Balanced);
        assert_eq!(under.kind, Kind::Skip);
        // Telegram would take 60 MiB, but the user asked for 40.
        let above = encode_plan(60 * MIB, Some(40), VideoCodec::H264, EncodePreset::Balanced);
        assert_eq!(above.kind, Kind::Encode);
        // A target the duration cannot hold is refused before encoding.
        let source = PathBuf::from("clip.mp4");
        let long = info(
            video("h264", "High", "yuv420p"),
            Some("aac"),
            Some(36_000.0),
        );
        let mut tiny = input(&source, 60 * MIB);
        tiny.target_bytes = Some(5 * MIB);
        assert_eq!(
            build_plan(Some(&long), &tiny).unwrap_err(),
            PlanError::TooLarge
        );
    }

    #[test]
    fn codec_and_preset_reach_an_encode_plan_and_only_that() {
        let hevc = encode_plan(300 * MIB, None, VideoCodec::Hevc, EncodePreset::Quality);
        assert_eq!(
            (hevc.video_codec, hevc.preset),
            (VideoCodec::Hevc, EncodePreset::Quality)
        );
        // The bitrate formula does not depend on the codec.
        let h264 = encode_plan(300 * MIB, None, VideoCodec::H264, EncodePreset::Balanced);
        assert_eq!(hevc.video_bps, h264.video_bps);
        // A copy does not encode: its plan carries no choice, so two copies always match.
        let copy = encode_plan(10 * MIB, Some(40), VideoCodec::Hevc, EncodePreset::Quality);
        assert_eq!(copy.kind, Kind::Skip);
        assert_eq!(
            (copy.video_codec, copy.preset, copy.target_bytes),
            (VideoCodec::H264, EncodePreset::Balanced, None)
        );
    }

    #[test]
    fn the_cpu_retry_plan_keeps_the_custom_target() {
        let mut plan = encode_plan(300 * MIB, Some(40), VideoCodec::H264, EncodePreset::Faster);
        plan.encoder = Some("h264_nvenc".into());
        let next = for_fallback(&plan, "libx264", 0.05);
        assert_eq!(
            next.video_bps,
            plan_bitrate(40 * MIB, 0.05, 600.0, Some(128))
        );
        assert_eq!(
            (next.target_bytes, next.preset, next.video_codec),
            (plan.target_bytes, plan.preset, plan.video_codec)
        );
    }

    #[test]
    fn a_plan_stored_before_these_choices_reads_as_h264_balanced_no_target() {
        let old = serde_json::json!({
            "kind": "encode", "limit_bytes": 1, "duration_s": null, "video_bps": 5,
            "audio_kbps": null, "video_index": 0, "audio_index": null, "vf": null,
            "hdr": false, "encoder": "h264_nvenc", "output_name": "x_sq.mp4", "photo": null,
            "source_dims": null, "alpha": false, "note": null
        });
        let plan: Plan = serde_json::from_value(old).unwrap();
        assert_eq!(
            (plan.video_codec, plan.preset, plan.target_bytes),
            (VideoCodec::H264, EncodePreset::Balanced, None)
        );
    }

    #[test]
    fn a_files_choices_are_valid_within_the_target_range() {
        let choices = |target_mb| EncodingOverride {
            video_codec: VideoCodec::Hevc,
            max_resolution: ResolutionCap::P1440,
            target_mb,
        };
        for ok in [None, Some(1), Some(4000)] {
            assert!(choices(ok).is_valid(), "{ok:?}");
        }
        for bad in [Some(0), Some(4001)] {
            assert!(!choices(bad).is_valid(), "{bad:?}");
        }
        // The page sends this shape and no other.
        let json = serde_json::json!({
            "video_codec": "hevc", "max_resolution": "1440p", "target_mb": null
        });
        assert_eq!(
            serde_json::from_value::<EncodingOverride>(json).unwrap(),
            choices(None)
        );
        let extra = serde_json::json!({
            "video_codec": "h264", "max_resolution": "1080p", "target_mb": null, "audio": 1
        });
        assert!(serde_json::from_value::<EncodingOverride>(extra).is_err());
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
        // Past the duration cap it is refused as too long; with the cap out of the way, as too
        // large for the limit.
        assert_eq!(
            build_plan(Some(&long), &input(&source, MIB))
                .unwrap_err()
                .code(),
            "VIDEO_TOO_LONG"
        );
        let mut unbounded = input(&source, MIB);
        unbounded.caps.max_duration_s = u64::MAX;
        assert_eq!(
            build_plan(Some(&long), &unbounded).unwrap_err(),
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
        let uhd = video_filter(&video("h264", "High", "yuv420p"), ResolutionCap::P2160);
        assert!(
            uhd.contains("min(3840,iw)") && uhd.contains("min(2160,ih)"),
            "{uhd}"
        );
    }

    #[test]
    fn copied_languages_follow_only_the_streams_left_after_cover_removal() {
        let info = super::super::probe::parse(
            r#"{"streams":[
                {"index":0,"codec_type":"video","disposition":{"attached_pic":1}},
                {"index":1,"codec_type":"video","tags":{"language":"eng"}},
                {"index":2,"codec_type":"attachment"},
                {"index":3,"codec_type":"audio","tags":{"language":"fra"}},
                {"index":4,"codec_type":"data"},
                {"index":5,"codec_type":"subtitle","tags":{"language":"deu"}},
                {"index":6,"codec_type":"video"}]}"#,
        )
        .unwrap();
        assert_eq!(
            copied_languages(&info),
            [
                Some("eng".into()),
                Some("fra".into()),
                Some("deu".into()),
                None
            ]
        );
    }

    #[test]
    fn private_data_turns_only_a_sent_as_is_plan_into_a_copy_or_a_remux() {
        let image = PrivateData {
            image: true,
            ..PrivateData::default()
        };
        let tags = PrivateData {
            video_tags: true,
            languages: vec![None, Some("eng".into())],
            ..PrivateData::default()
        };
        // A photo sent as a file keeps its name.
        let photo = PathBuf::from(r"C:\in\IMG_0042.JPG");
        let mut doc = input(&photo, MIB);
        doc.send_mode = SendMode::Document;
        let skip = build_plan(None, &doc).unwrap();
        let stripped = without_private_data(skip.clone(), &photo, image.clone());
        assert_eq!(
            (stripped.kind, stripped.output_name.as_str()),
            (Kind::Strip, "IMG_0042.JPG")
        );
        assert_eq!(
            without_private_data(skip.clone(), &photo, PrivateData::default()),
            skip
        );
        // Document mode does not probe: a video there has no tags to act on.
        let clip = PathBuf::from("clip.mp4");
        let mut doc_video = input(&clip, MIB);
        doc_video.send_mode = SendMode::Document;
        let skip = build_plan(None, &doc_video).unwrap();
        assert_eq!(
            without_private_data(skip.clone(), &clip, tags.clone()),
            skip
        );

        // A compliant MP4 with a location is copied without it, without one it is sent as is.
        let compliant = info(video("h264", "High", "yuv420p"), Some("aac"), Some(10.0));
        let skip = build_plan(Some(&compliant), &input(&clip, MIB)).unwrap();
        assert_eq!(skip.kind, Kind::Skip);
        let copy = without_private_data(skip.clone(), &clip, tags.clone());
        assert_eq!(
            (copy.kind, copy.note.as_deref(), copy.output_name.as_str()),
            (Kind::Strip, Some(PRIVATE_TAGS), "clip.mp4"),
            "same streams, same name"
        );
        assert_eq!(copy.stream_languages, [None, Some("eng".to_owned())]);
        assert_eq!(
            without_private_data(skip.clone(), &clip, PrivateData::default()),
            skip
        );

        // Plans that re-encode already drop everything.
        let encode = build_plan(Some(&compliant), &input(&clip, 200 * MIB)).unwrap();
        assert_eq!(encode.kind, Kind::Encode);
        assert_eq!(without_private_data(encode.clone(), &clip, tags), encode);
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
        assert!(
            pano.width <= 4096 && pano.width.is_multiple_of(2) && pano.height.is_multiple_of(2)
        );
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

    fn capped(plan: Result<Plan, PlanError>) -> (&'static str, f64, f64) {
        match plan {
            Err(PlanError::Capped { code, found, max }) => (code, found, max),
            other => panic!("expected a cap refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_photo_is_taken_at_its_cap_and_refused_just_above() {
        let source = PathBuf::from("pic.jpg");
        let caps = Caps {
            photo_pixels: 20_000_000,
            photo_wide_pixels: 8_000_000,
            ..Caps::default()
        };
        let plan_for = |pix_fmt: &str, width: u32, height: u32| {
            let mut v = video("mjpeg", "", pix_fmt);
            (v.width, v.height) = (width, height);
            let mut input = input(&source, MIB);
            input.caps = caps;
            build_plan(Some(&info(v, None, None)), &input)
        };
        assert_eq!(plan_for("yuvj420p", 5000, 4000).unwrap().kind, Kind::Photo);
        assert_eq!(
            capped(plan_for("yuvj420p", 5000, 4001)),
            ("PHOTO_TOO_LARGE", 20.1, 20.0)
        );
        // Transparency or more than 8 bits per color: the lower cap.
        for wide in ["rgba", "rgb48be", "yuv420p10le"] {
            assert_eq!(
                plan_for(wide, 4000, 2000).unwrap().kind,
                Kind::Photo,
                "{wide}"
            );
            assert_eq!(
                capped(plan_for(wide, 4000, 2001)),
                ("PHOTO_TOO_LARGE", 8.1, 8.0),
                "{wide}"
            );
        }
        assert_eq!(plan_for("rgb24", 4000, 2001).unwrap().kind, Kind::Photo);
    }

    #[test]
    fn a_video_to_encode_is_refused_above_its_frame_and_duration_caps() {
        let source = PathBuf::from("clip.mkv");
        let caps = Caps {
            video_pixels: 3840 * 2160,
            max_duration_s: 1800,
            ..Caps::default()
        };
        let plan_for = |width: u32, height: u32, rotation: i32, seconds: f64| {
            let mut v = video("hevc", "Main", "yuv420p");
            (v.width, v.height, v.rotation) = (width, height, rotation);
            let mut input = input(&source, 900 * MIB);
            input.caps = caps;
            build_plan(Some(&info(v, Some("aac"), Some(seconds))), &input)
        };
        assert_eq!(plan_for(3840, 2160, 0, 1800.0).unwrap().kind, Kind::Encode);
        assert_eq!(plan_for(2160, 3840, 90, 60.0).unwrap().kind, Kind::Encode);
        assert_eq!(
            capped(plan_for(3840, 2161, 0, 60.0)),
            ("VIDEO_TOO_LARGE", 8.3, 8.2944)
        );
        assert_eq!(
            capped(plan_for(2161, 3840, 90, 60.0)),
            ("VIDEO_TOO_LARGE", 8.3, 8.2944)
        );
        assert_eq!(
            capped(plan_for(1920, 1080, 0, 1800.5)),
            ("VIDEO_TOO_LONG", 0.6, 0.5)
        );

        // A copy needs no memory and no time: an oversized or long file sent as it is passes.
        let mut v = video("h264", "High", "yuv420p");
        v.width = 4096;
        let mut input = input(&source, MIB);
        input.caps = Caps {
            video_pixels: 1,
            max_duration_s: 1,
            ..caps
        };
        let plan = build_plan(Some(&info(v, Some("aac"), Some(90_000.0))), &input).unwrap();
        assert_eq!(plan.kind, Kind::Remux);
    }
}
