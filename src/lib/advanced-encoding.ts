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

/** `settings::AUDIO_KBPS_CHOICES`. */
export const AUDIO_KBPS_CHOICES: readonly number[] = [96, 128, 160, 192]

export interface AdvancedEncoding {
  max_resolution: MaxResolution
  audio_kbps: number
}

/** `AdvancedEncoding::AUTOMATIC`: every encode without the feature. */
export const AUTOMATIC_ENCODING: Readonly<AdvancedEncoding> = {
  max_resolution: "1080p",
  audio_kbps: 128,
}

export const maxResolutionDecoder = d.literals<MaxResolution>({
  "1080p": true,
  "1440p": true,
  "2160p": true,
  original: true,
})

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
