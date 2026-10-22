//! ffprobe: one JSON call, reduced to what the plan needs.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::MediaError;
use super::procs::{self, Args, Ending, Spec};

const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoInfo {
    /// Absolute stream index, passed to `-map 0:<index>`.
    pub index: u32,
    pub codec: String,
    pub profile: Option<String>,
    /// Coded size, before rotation.
    pub width: u32,
    pub height: u32,
    pub sar: Option<(u32, u32)>,
    pub pix_fmt: Option<String>,
    pub field_order: Option<String>,
    pub color_transfer: Option<String>,
    /// Display Matrix rotation in degrees (0 if none).
    pub rotation: i32,
    pub bit_rate: Option<u64>,
    pub hdr: bool,
}

impl VideoInfo {
    /// Size as displayed (after rotation, before the pixel aspect ratio).
    pub fn rotated_size(&self) -> (u32, u32) {
        if self.rotation.rem_euclid(180) == 90 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    pub fn interlaced(&self) -> bool {
        matches!(self.field_order.as_deref(), Some("tt" | "bb" | "tb" | "bt"))
    }

    /// More than 8 bits per color, by the name of the pixel format (`yuv420p10le`, `rgb48be`,
    /// `gray16le`, `p010le`, `gbrpf32le`).
    pub fn deep_color(&self) -> bool {
        let Some(fmt) = self.pix_fmt.as_deref() else {
            return false;
        };
        let name = fmt.trim_end_matches("le").trim_end_matches("be");
        let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        name[name.len() - digits..]
            .parse::<u32>()
            .is_ok_and(|bits| matches!(bits, 9 | 10 | 12 | 14 | 16 | 32 | 48 | 64))
    }

    pub fn has_alpha(&self) -> bool {
        let Some(fmt) = self.pix_fmt.as_deref() else {
            return false;
        };
        fmt.starts_with("yuva")
            || fmt.starts_with("gbrap")
            || matches!(
                fmt,
                "rgba"
                    | "bgra"
                    | "argb"
                    | "abgr"
                    | "ya8"
                    | "ya16le"
                    | "ya16be"
                    | "rgba64le"
                    | "rgba64be"
                    | "bgra64le"
                    | "bgra64be"
            )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioInfo {
    pub index: u32,
    pub codec: String,
    pub channels: Option<u32>,
    pub bit_rate: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub format_name: Option<String>,
    /// `None` when ffprobe does not know it (raw streams, some MKV).
    pub duration_s: Option<f64>,
    pub bit_rate: Option<u64>,
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
    /// The file or one of its streams names a place or a camera (`private_tag`), or it has a
    /// data track; only this flag is kept, never the values. Absent from items probed before
    /// it existed.
    #[serde(default)]
    pub private_tags: bool,
    /// Every stream, in order: its kind and its language when it has a valid one. Language
    /// labels are not private; `plan` carries them through a copy that drops all metadata.
    #[serde(default)]
    pub streams: Vec<StreamInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    /// ffprobe's `codec_type`: `video`, `audio`, `subtitle`, `data`, `attachment`…
    pub kind: String,
    pub language: Option<String>,
    /// Required so cached probes made before cover detection are refreshed.
    pub attached_pic: bool,
}

/// An ISO 639 code as ffmpeg writes it (`eng`, `fra`, `pt`): letters only, at most 8. Anything
/// else is not passed back to ffmpeg.
fn language_code(value: &serde_json::Value) -> Option<String> {
    let code = value.as_str()?;
    (!code.is_empty() && code.len() <= 8 && code.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| code.to_owned())
}

// --- raw ffprobe JSON ---

#[derive(Deserialize, Default)]
struct RawOutput {
    #[serde(default)]
    format: RawFormat,
    #[serde(default)]
    streams: Vec<RawStream>,
}

#[derive(Deserialize, Default)]
struct RawFormat {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
    #[serde(default)]
    tags: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct RawDisposition {
    #[serde(default)]
    default: u8,
    #[serde(default)]
    attached_pic: u8,
}

#[derive(Deserialize)]
struct RawSideData {
    side_data_type: Option<String>,
    rotation: Option<f64>,
}

#[derive(Deserialize, Default)]
struct RawStream {
    index: u32,
    codec_type: Option<String>,
    #[serde(default)]
    tags: serde_json::Map<String, serde_json::Value>,
    codec_name: Option<String>,
    profile: Option<String>,
    pix_fmt: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    field_order: Option<String>,
    color_transfer: Option<String>,
    sample_aspect_ratio: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
    channels: Option<u32>,
    #[serde(default)]
    disposition: RawDisposition,
    #[serde(default)]
    side_data_list: Vec<RawSideData>,
}

fn number(text: &Option<String>) -> Option<f64> {
    text.as_deref()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

fn bits(text: &Option<String>) -> Option<u64> {
    text.as_deref()?.parse::<u64>().ok()
}

fn ratio(text: &Option<String>) -> Option<(u32, u32)> {
    let (a, b) = text.as_deref()?.split_once(':')?;
    let (a, b) = (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?);
    (a > 0 && b > 0).then_some((a, b))
}

fn video_of(raw: &RawStream) -> VideoInfo {
    let rotation = raw
        .side_data_list
        .iter()
        .find(|d| d.side_data_type.as_deref() == Some("Display Matrix"))
        .and_then(|d| d.rotation)
        .map_or(0, |r| r.round() as i32);
    let hdr = matches!(
        raw.color_transfer.as_deref(),
        Some("smpte2084" | "arib-std-b67")
    );
    VideoInfo {
        index: raw.index,
        codec: raw.codec_name.clone().unwrap_or_default(),
        profile: raw.profile.clone(),
        width: raw.width.unwrap_or(0),
        height: raw.height.unwrap_or(0),
        sar: ratio(&raw.sample_aspect_ratio),
        pix_fmt: raw.pix_fmt.clone(),
        field_order: raw.field_order.clone(),
        color_transfer: raw.color_transfer.clone(),
        rotation,
        bit_rate: bits(&raw.bit_rate),
        hdr,
    }
}

/// A container tag that says where or with what a video was shot: `location` (MP4 `©xyz`,
/// Android), `com.apple.quicktime.location.ISO6709`, `make`/`model` and their QuickTime and
/// Android forms, serial numbers.
fn private_tag(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["location", "gps", "serial", "manufacturer"]
        .iter()
        .any(|part| key.contains(part))
        || key.ends_with("make")
        || key.ends_with("model")
}

/// Reduces ffprobe's JSON (`-show_format -show_streams`). The video stream is the first one
/// that is not a cover picture; the audio stream is the default one, else the first.
pub fn parse(json: &str) -> Result<MediaInfo, serde_json::Error> {
    let raw: RawOutput = serde_json::from_str(json)?;
    let video_raw = raw
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video") && s.disposition.attached_pic == 0);
    let audio_raw = {
        let mut audios = raw
            .streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some("audio"));
        let first = audios.next();
        let default = raw
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("audio") && s.disposition.default == 1);
        default.or(first)
    };
    let duration_s = number(&raw.format.duration)
        .or_else(|| video_raw.and_then(|s| number(&s.duration)))
        .or_else(|| audio_raw.and_then(|s| number(&s.duration)))
        .filter(|d| *d > 0.0);
    Ok(MediaInfo {
        format_name: raw.format.format_name.clone(),
        duration_s,
        bit_rate: bits(&raw.format.bit_rate),
        video: video_raw.map(video_of),
        audio: audio_raw.map(|s| AudioInfo {
            index: s.index,
            codec: s.codec_name.clone().unwrap_or_default(),
            channels: s.channels,
            bit_rate: bits(&s.bit_rate),
        }),
        streams: raw
            .streams
            .iter()
            .map(|stream| StreamInfo {
                kind: stream.codec_type.clone().unwrap_or_default(),
                language: stream.tags.get("language").and_then(language_code),
                attached_pic: stream.disposition.attached_pic != 0,
            })
            .collect(),
        // Data tracks may record GPS; covers and attachments may hide private image metadata.
        private_tags: raw.format.tags.keys().any(|key| private_tag(key))
            || raw.streams.iter().any(|stream| {
                matches!(stream.codec_type.as_deref(), Some("data" | "attachment"))
                    || stream.disposition.attached_pic != 0
                    || stream.tags.keys().any(|key| private_tag(key))
            }),
    })
}

/// Probes a file. `scan_duration` enables the (slower) packet scan when the duration is unknown.
pub async fn probe(
    ffprobe: &Path,
    input: &Path,
    scan_duration: bool,
    cancel: &CancellationToken,
) -> Result<MediaInfo, MediaError> {
    super::check_input_extension(input)?;
    probe_as(ffprobe, input, scan_duration, cancel, false).await
}

/// `probe` for a file nobody is waiting for yet (analysis at drop): ffprobe runs below normal
/// priority.
pub async fn probe_in_background(
    ffprobe: &Path,
    input: &Path,
    scan_duration: bool,
    cancel: &CancellationToken,
) -> Result<MediaInfo, MediaError> {
    super::check_input_extension(input)?;
    probe_as(ffprobe, input, scan_duration, cancel, true).await
}

/// Internal partial outputs end in .part; they still use the same restricted demuxers.
pub(super) async fn probe_output(
    ffprobe: &Path,
    input: &Path,
    cancel: &CancellationToken,
) -> Result<MediaInfo, MediaError> {
    probe_as(ffprobe, input, false, cancel, false).await
}

fn probe_args(input: &Path, scan_packets: bool) -> Args {
    let options: &[&str] = if scan_packets {
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
        ]
    } else {
        &[
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ]
    };
    super::file_input(Args::new().all(options), input)
}

async fn probe_as(
    ffprobe: &Path,
    input: &Path,
    scan_duration: bool,
    cancel: &CancellationToken,
    low_priority: bool,
) -> Result<MediaInfo, MediaError> {
    let mut spec = Spec::new(ffprobe, probe_args(input, false))
        .timeout(PROBE_TIMEOUT)
        .cancel(cancel)
        .collect_stdout();
    if low_priority {
        spec = spec.low_priority();
    }
    let output = procs::run(spec, |_| {})
        .await
        .map_err(|e| MediaError::failed("FFMPEG_MISSING", e.to_string()))?;
    if output.ending == Ending::Cancelled {
        return Err(MediaError::Cancelled);
    }
    if !output.success() {
        return Err(MediaError::failed(
            if super::unsupported_format(&output.stderr_tail.join("\n")) {
                "INPUT_FORMAT_UNSUPPORTED"
            } else if super::out_of_memory(&output.stderr_tail.join("\n"), output.code) {
                "MEMORY_LIMIT"
            } else {
                "FILE_UNREADABLE"
            },
            format!(
                "ffprobe {:?}: {}",
                output.code,
                output.stderr_tail.join(" | ")
            ),
        ));
    }
    let mut info = parse(&output.stdout)
        .map_err(|e| MediaError::failed("FILE_UNREADABLE", format!("ffprobe json: {e}")))?;
    if super::plan::is_image(input)
        && info
            .video
            .as_ref()
            .is_none_or(|v| v.width == 0 || v.height == 0)
    {
        return Err(MediaError::failed(
            "INPUT_FORMAT_UNSUPPORTED",
            "input is not a decodable supported photo",
        ));
    }
    if info.duration_s.is_none() && scan_duration && info.video.is_some() {
        info.duration_s = scan_last_pts(ffprobe, input, cancel, low_priority).await?;
    }
    Ok(info)
}

/// Fallback when the container gives no duration: the last video packet's `pts_time`.
async fn scan_last_pts(
    ffprobe: &Path,
    input: &Path,
    cancel: &CancellationToken,
    low_priority: bool,
) -> Result<Option<f64>, MediaError> {
    let mut spec = Spec::new(ffprobe, probe_args(input, true))
        .timeout(PROBE_TIMEOUT)
        .cancel(cancel);
    if low_priority {
        spec = spec.low_priority();
    }
    let mut last = None;
    let output = procs::run(spec, |line| {
        if let Ok(value) = line.trim().parse::<f64>() {
            last = Some(value);
        }
    })
    .await
    .map_err(|e| MediaError::failed("FFMPEG_MISSING", e.to_string()))?;
    if output.ending == Ending::Cancelled {
        return Err(MediaError::Cancelled);
    }
    Ok(last.filter(|v| *v > 0.0))
}

/// For an MP4/MOV file: is the `moov` atom before `mdat` (playable before it is fully
/// downloaded)? `None` if the top-level atoms cannot be walked.
pub fn moov_first<R: Read + Seek>(reader: &mut R) -> Option<bool> {
    let end = reader.seek(SeekFrom::End(0)).ok()?;
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut position = 0u64;
    while position + 8 <= end {
        reader.seek(SeekFrom::Start(position)).ok()?;
        let mut header = [0u8; 8];
        reader.read_exact(&mut header).ok()?;
        let size32 = u32::from_be_bytes(header[..4].try_into().ok()?) as u64;
        match &header[4..] {
            b"moov" => return Some(true),
            b"mdat" => return Some(false),
            _ => {}
        }
        let size = match size32 {
            0 => return None,
            1 => {
                let mut large = [0u8; 8];
                reader.read_exact(&mut large).ok()?;
                u64::from_be_bytes(large)
            }
            n => n,
        };
        if size < 8 {
            return None;
        }
        position += size;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn deep_color_is_read_from_the_pixel_format_name() {
        let deep = |fmt: &str| {
            let video = VideoInfo {
                index: 0,
                codec: "png".into(),
                profile: None,
                width: 1,
                height: 1,
                sar: None,
                pix_fmt: Some(fmt.into()),
                field_order: None,
                color_transfer: None,
                rotation: 0,
                bit_rate: None,
                hdr: false,
            };
            video.deep_color()
        };
        for wide in [
            "rgb48be",
            "rgba64le",
            "gray16be",
            "ya16le",
            "yuv420p10le",
            "yuv444p12le",
            "gbrp16le",
            "p010le",
            "gbrpf32le",
        ] {
            assert!(deep(wide), "{wide}");
        }
        for plain in [
            "rgb24", "rgba", "gray", "ya8", "pal8", "yuvj420p", "yuv420p", "bgr24", "rgb565le",
            "rgb555le", "monob",
        ] {
            assert!(!deep(plain), "{plain}");
        }
    }

    #[test]
    fn both_probes_restrict_formats_and_protocols() {
        for name in [
            "clip.mp4",
            "photo%d.JPG",
            "photo.png",
            "photo.webp",
            "photo.tiff",
        ] {
            for scan_packets in [false, true] {
                let input = Path::new(name);
                super::super::assert_file_input(probe_args(input, scan_packets), input);
            }
        }
    }

    const SAMPLE: &str = r#"{
      "streams": [
        {"index":0,"codec_type":"video","codec_name":"h264","profile":"High","width":1920,"height":1080,
         "pix_fmt":"yuv420p","field_order":"progressive","sample_aspect_ratio":"1:1","bit_rate":"4000000",
         "disposition":{"default":1,"attached_pic":0}},
        {"index":1,"codec_type":"audio","codec_name":"mp3","channels":2,"disposition":{"default":0}},
        {"index":2,"codec_type":"audio","codec_name":"aac","channels":6,"disposition":{"default":1}},
        {"index":3,"codec_type":"video","codec_name":"mjpeg","width":300,"height":300,"disposition":{"default":0,"attached_pic":1}}
      ],
      "format": {"format_name":"matroska,webm","duration":"61.5","bit_rate":"4200000"}
    }"#;

    #[test]
    fn reduces_the_json_and_picks_the_default_audio() {
        let info = parse(SAMPLE).unwrap();
        assert_eq!(info.duration_s, Some(61.5));
        assert_eq!(info.bit_rate, Some(4_200_000));
        let video = info.video.unwrap();
        assert_eq!((video.index, video.width, video.height), (0, 1920, 1080));
        assert_eq!(video.sar, Some((1, 1)));
        assert!(!video.hdr && !video.interlaced());
        let audio = info.audio.unwrap();
        assert_eq!(
            (audio.index, audio.codec.as_str(), audio.channels),
            (2, "aac", Some(6))
        );
    }

    #[test]
    fn missing_duration_stays_missing_and_stream_duration_is_a_fallback() {
        let none =
            r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"}],"format":{}}"#;
        assert_eq!(parse(none).unwrap().duration_s, None);
        let stream = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","duration":"7.25"}],"format":{"duration":"N/A"}}"#;
        assert_eq!(parse(stream).unwrap().duration_s, Some(7.25));
    }

    #[test]
    fn rotation_hdr_interlace_and_anamorphic_are_read() {
        let json = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc","width":3840,"height":2160,
          "color_transfer":"smpte2084","field_order":"tt","sample_aspect_ratio":"32:27","pix_fmt":"yuv420p10le",
          "side_data_list":[{"side_data_type":"Display Matrix","rotation":-90}]}],"format":{"duration":"3"}}"#;
        let video = parse(json).unwrap().video.unwrap();
        assert!(video.hdr && video.interlaced());
        assert_eq!(video.sar, Some((32, 27)));
        assert_eq!(video.rotation, -90);
        assert_eq!(video.rotated_size(), (2160, 3840));
        assert!(!video.has_alpha());
    }

    #[test]
    fn stream_languages_are_kept_when_they_are_codes() {
        let json = r#"{"streams":[
            {"index":0,"codec_type":"video","codec_name":"h264","tags":{"language":"und"}},
            {"index":1,"codec_type":"audio","codec_name":"aac","tags":{"language":"eng"}},
            {"index":2,"codec_type":"audio","codec_name":"aac","tags":{"language":"x; rm"}},
            {"index":3,"codec_type":"subtitle","codec_name":"mov_text","tags":{"language":"toolongcode"}},
            {"index":4,"codec_type":"data"}],"format":{}}"#;
        let streams = parse(json).unwrap().streams;
        let seen: Vec<(&str, Option<&str>)> = streams
            .iter()
            .map(|s| (s.kind.as_str(), s.language.as_deref()))
            .collect();
        assert_eq!(
            seen,
            [
                ("video", Some("und")),
                ("audio", Some("eng")),
                ("audio", None),
                ("subtitle", None),
                ("data", None)
            ]
        );
    }

    #[test]
    fn stream_tags_and_data_tracks_are_flagged_too() {
        let gopro = r#"{"streams":[
            {"index":0,"codec_type":"video","codec_name":"h264"},
            {"index":1,"codec_type":"data","codec_tag_string":"gpmd"}],"format":{}}"#;
        assert!(parse(gopro).unwrap().private_tags);
        let camera = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264",
            "tags":{"com.android.model":"FakePhone"}}],"format":{}}"#;
        assert!(parse(camera).unwrap().private_tags);
        let plain = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264",
            "tags":{"language":"eng","handler_name":"VideoHandler"}}],"format":{}}"#;
        assert!(!parse(plain).unwrap().private_tags);
    }

    #[test]
    fn covers_and_attachments_trigger_stripping_without_container_tags() {
        for stream in [
            r#"{"index":1,"codec_type":"video","disposition":{"attached_pic":1}}"#,
            r#"{"index":1,"codec_type":"attachment","tags":{"mimetype":"image/jpeg"}}"#,
        ] {
            let info = parse(&format!(
                r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"h264"}},{stream}]}}"#
            ))
            .unwrap();
            assert!(info.private_tags);
            assert_eq!(info.video.unwrap().index, 0);
        }
        assert!(serde_json::from_str::<StreamInfo>(r#"{"kind":"video","language":null}"#).is_err());
    }

    #[test]
    fn location_and_camera_tags_are_flagged_never_kept() {
        let with = |tags: &str| {
            parse(&format!(
                r#"{{"streams":[],"format":{{"format_name":"mov,mp4","tags":{tags}}}}}"#
            ))
            .unwrap()
        };
        for tags in [
            r#"{"location":"+48.8584+002.2945/","location-eng":"+48.8584+002.2945/"}"#,
            r#"{"com.apple.quicktime.location.ISO6709":"+48.8584+002.2945+035.000/"}"#,
            r#"{"com.apple.quicktime.make":"FakeMaker","com.apple.quicktime.model":"FakePhone"}"#,
            r#"{"com.android.manufacturer":"FakeMaker"}"#,
        ] {
            let info = with(tags);
            assert!(info.private_tags, "{tags}");
            let stored = serde_json::to_string(&info).unwrap();
            assert!(
                !stored.contains("48.8584") && !stored.contains("Fake"),
                "{stored}"
            );
        }
        for tags in [
            r#"{"major_brand":"isom","encoder":"Lavf61","creation_time":"2026-01-01T00:00:00Z"}"#,
            "{}",
        ] {
            assert!(!with(tags).private_tags, "{tags}");
        }
        // Items probed before the flag existed read as "none".
        let old: MediaInfo = serde_json::from_str(
            r#"{"format_name":null,"duration_s":null,"bit_rate":null,"video":null,"audio":null}"#,
        )
        .unwrap();
        assert!(!old.private_tags);
    }

    #[test]
    fn unknown_sar_values_are_dropped() {
        let json = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","sample_aspect_ratio":"0:1"}],"format":{}}"#;
        assert_eq!(parse(json).unwrap().video.unwrap().sar, None);
    }

    fn atom(kind: &[u8; 4], payload: usize) -> Vec<u8> {
        let mut bytes = ((payload + 8) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend(std::iter::repeat_n(0u8, payload));
        bytes
    }

    #[test]
    fn moov_position_is_read_from_the_top_level_atoms() {
        let fast: Vec<u8> = [atom(b"ftyp", 16), atom(b"moov", 100), atom(b"mdat", 500)].concat();
        let slow: Vec<u8> = [
            atom(b"ftyp", 16),
            atom(b"free", 8),
            atom(b"mdat", 500),
            atom(b"moov", 100),
        ]
        .concat();
        assert_eq!(moov_first(&mut Cursor::new(fast)), Some(true));
        assert_eq!(moov_first(&mut Cursor::new(slow)), Some(false));
        assert_eq!(moov_first(&mut Cursor::new(vec![0u8; 3])), None);
        assert_eq!(moov_first(&mut Cursor::new(atom(b"ftyp", 16))), None);
    }
}
