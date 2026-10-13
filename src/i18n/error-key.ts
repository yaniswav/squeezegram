// No `@/` import here: `pnpm test` runs it under node.
import { isJsonString, type JsonObject } from "../lib/json.ts"

/**
 * Codes with a param that picks a more precise sentence: `<CODE>_<value>` when the catalogue
 * has it (`TG_WRITE_FORBIDDEN_banned`, `FFMPEG_FAILED_fallback_failed`…), else `<CODE>`.
 */
const VARIANT_PARAM = {
  TG_WRITE_FORBIDDEN: "reason",
  FFMPEG_FAILED: "cause",
  FFMPEG_KILLED: "cause",
  ENCODER_FALLBACK: "cause",
  TG_NOT_CONFIGURED: "problem",
  PROXY_INVALID: "field",
} as const satisfies Readonly<Record<string, string>>

function hasVariantParam(code: string): code is keyof typeof VARIANT_PARAM {
  return Object.hasOwn(VARIANT_PARAM, code)
}

/** Codes that inform (the work went on) rather than report an error. */
const INFO_CODES: ReadonlySet<string> = new Set([
  "ENCODER_FALLBACK",
  "CODEC_FALLBACK",
])

/** Catalogue key of `code` for these params; `has` tells whether a key exists. */
export function messageKey(
  code: string,
  params: Readonly<JsonObject>,
  has: (key: string) => boolean,
): string {
  const name = hasVariantParam(code) ? VARIANT_PARAM[code] : undefined

  const value = name === undefined ? undefined : params[name]

  if (isJsonString(value)) {
    const key = `${code}_${value}`

    if (has(key)) return key
  }

  return code
}

export function isInfoCode(code: string): boolean {
  return INFO_CODES.has(code)
}
