// No `@/` import here: `pnpm test` runs it under node.

/**
 * Codes with a param that picks a more precise sentence: `<CODE>_<value>` when the catalogue
 * has it (`TG_WRITE_FORBIDDEN_banned`, `FFMPEG_FAILED_fallback_failed`…), else `<CODE>`.
 */
const VARIANT_PARAM: Readonly<Record<string, string>> = {
  TG_WRITE_FORBIDDEN: "reason",
  FFMPEG_FAILED: "cause",
  ENCODER_FALLBACK: "cause",
}

/** Codes that inform (the work went on) rather than report an error. */
const INFO_CODES: ReadonlySet<string> = new Set(["ENCODER_FALLBACK"])

/** Catalogue key of `code` for these params; `has` tells whether a key exists. */
export function messageKey(
  code: string,
  params: Readonly<Record<string, unknown>>,
  has: (key: string) => boolean,
): string {
  const name = Object.hasOwn(VARIANT_PARAM, code)
    ? VARIANT_PARAM[code]
    : undefined

  const value = name === undefined ? undefined : params[name]

  if (typeof value === "string") {
    const key = `${code}_${value}`

    if (has(key)) return key
  }

  return code
}

export function isInfoCode(code: string): boolean {
  return INFO_CODES.has(code)
}
