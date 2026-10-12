// `settings.advanced_encoding` (src-tauri/src/settings.rs): the choices offered, and what an
// encode uses. Kept out of ipc.ts so `node --test` can import it.

import * as d from "./decode.ts"

/** `plan::ResolutionCap`: the longest side is capped, a smaller source is never upscaled. */
export type MaxResolution = "1080p" | "1440p" | "2160p" | "original"

export const MAX_RESOLUTIONS: readonly MaxResolution[] = [
  "1080p",
  "1440p",
  "2160p",
  "original",
]

/** `plan::VideoCodec`. */
export type VideoCodec = "h264" | "hevc"

export const VIDEO_CODECS: readonly VideoCodec[] = ["h264", "hevc"]

/** `plan::EncodePreset`: the speed/quality trade-off, mapped per encoder by the core. */
export type EncodePreset = "faster" | "balanced" | "quality"

export const ENCODE_PRESETS: readonly EncodePreset[] = [
  "faster",
  "balanced",
  "quality",
]

/** `plan::TARGET_MB_RANGE`: custom output sizes, in MiB. */
export const TARGET_MB_MIN = 1
export const TARGET_MB_MAX = 4000

/** `settings::AUDIO_KBPS_CHOICES`. */
export const AUDIO_KBPS_CHOICES: readonly number[] = [96, 128, 160, 192]

export interface AdvancedEncoding {
  max_resolution: MaxResolution
  audio_kbps: number
  video_codec: VideoCodec
  preset: EncodePreset
  /** MiB under the upload limit; null fills the limit. */
  target_mb: number | null
}

/** `AdvancedEncoding::AUTOMATIC`: every encode without the feature. */
export const AUTOMATIC_ENCODING: Readonly<AdvancedEncoding> = {
  max_resolution: "1080p",
  audio_kbps: 128,
  video_codec: "h264",
  preset: "balanced",
  target_mb: null,
}

export const maxResolutionDecoder = d.literals<MaxResolution>({
  "1080p": true,
  "1440p": true,
  "2160p": true,
  original: true,
})

export const videoCodecDecoder = d.literals<VideoCodec>({
  h264: true,
  hevc: true,
})

export const encodePresetDecoder = d.literals<EncodePreset>({
  faster: true,
  balanced: true,
  quality: true,
})

/** `plan::EncodingOverride`: the choices of one queued file, a complete set. */
export interface EncodingOverride {
  video_codec: VideoCodec
  max_resolution: MaxResolution
  /** MiB; null fills the upload limit. */
  target_mb: number | null
}

export const encodingOverrideDecoder = d.object<EncodingOverride>({
  video_codec: videoCodecDecoder,
  max_resolution: maxResolutionDecoder,
  target_mb: d.nullable(d.number),
})

/** A whole number of MiB within the range the core accepts. */
export function isTargetMb(value: number): boolean {
  return (
    Number.isInteger(value) && value >= TARGET_MB_MIN && value <= TARGET_MB_MAX
  )
}

/** `stage::EncodeEstimate`: what the options panel shows for a queued file. */
export interface EncodeEstimate {
  /** encode, as_is, photo, too_small or unknown (see the core). */
  outcome: "encode" | "as_is" | "photo" | "too_small" | "unknown"
  bytes: number | null
  limit_bytes: number
  /** The codec the encode will really use. */
  codec: VideoCodec
  /** HEVC was asked for and no encoder works: the encode will be H.264. */
  codec_fallback: boolean
}

/** `stage::CodecSupport`. */
export interface CodecSupport {
  hevc: boolean
  hevc_encoder: string | null
  reason: "no_hevc_encoder" | "ffmpeg_unavailable" | null
}

/**
 * What the next encode uses. Display only: the core makes the same choice when it builds the
 * plan (`media::stage::encoding_choice`), whatever the page shows.
 */
export function effectiveEncoding(
  saved: AdvancedEncoding,
  granted: boolean,
): AdvancedEncoding {
  return granted ? saved : { ...AUTOMATIC_ENCODING }
}
