/** Variables of a caption template; the core replaces them at send time (caption.rs). */
export const CAPTION_VARIABLES = [
  "name",
  "file",
  "ext",
  "duration",
  "original_size",
  "final_size",
  "saving",
  "resolution",
  "date",
  "index",
  "count",
] as const

/** Telegram's limit for an account without Premium: used while no account is known. */
export const STANDARD_CAPTION_CHARS = 1024

export function captionLimit(accountMax: number | undefined): number {
  return accountMax ?? STANDARD_CAPTION_CHARS
}

/**
 * Length the way the core counts it: UTF-16 units, which Telegram uses for text offsets. An
 * emoji outside the BMP counts 2, so the count is never below what the server may count.
 */
export function captionLength(text: string): number {
  return text.length
}

export type CounterState = "ok" | "near" | "over"

/** "near" from 90 % of the limit, so the warning comes before the failure. */
export function counterState(length: number, max: number): CounterState {
  if (length > max) return "over"

  return length >= max * 0.9 ? "near" : "ok"
}
