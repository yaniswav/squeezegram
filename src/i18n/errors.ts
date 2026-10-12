import i18n from "i18next"
import type { TFunction } from "i18next"
import { formatBytes, formatDuration, formatNumber } from "@/lib/format"
import { IpcError, FRONT_CODES } from "@/lib/ipc"
import { isJsonNumber, type Json, type JsonObject } from "@/lib/json"
import { isInfoCode, messageKey } from "./error-key"
import en from "./locales/en.json"

type Errors = typeof en.errors

type ErrorCode = {
  [Key in keyof Errors]: Key extends `${infer Base}_${"one" | "other"}`
    ? Base
    : Key
}[keyof Errors]

function isKnownCode(code: string): code is ErrorCode {
  return (
    Object.hasOwn(en.errors, code) || Object.hasOwn(en.errors, `${code}_other`)
  )
}

function numberOf(value: Json | undefined): number | null {
  return isJsonNumber(value) && Number.isFinite(value) ? value : null
}

/**
 * Params as the core sends them, plus display forms: `duration` from `retry_after`, `seconds`
 * or `atSeconds`, `size` from `limit` (bytes), formatted `stars` and a translated `media`.
 */
function withDisplay(
  t: TFunction,
  params: JsonObject,
  language: string,
): JsonObject {
  const display: JsonObject = { ...params }

  const wait =
    numberOf(params.retry_after) ??
    numberOf(params.seconds) ??
    numberOf(params.atSeconds)

  if (wait !== null) display.duration = formatDuration(wait, language)
  const limit = numberOf(params.limit)

  if (limit !== null) display.size = formatBytes(limit, language)
  const stars = numberOf(params.stars)

  if (stars !== null) display.stars = formatNumber(stars, language)

  if (params.media === "video") display.media = t("mode.video")
  else if (params.media === "photo") display.media = t("mode.photo")
  else if (params.media === "document") display.media = t("mode.document")

  return display
}

/** Translated text of an error code from the core (`{ code, params }`) or from ipc.ts. */
export function errorMessage(
  t: TFunction,
  code: string,
  params: JsonObject = {},
): string {
  if (!isKnownCode(code)) return t("errors.UNEXPECTED", { code })
  const display = withDisplay(t, params, i18n.language)
  // A `reason` or `cause` param can pick a more precise sentence (error-key.ts).
  const key = messageKey(code, params, isKnownCode)

  return t(`errors.${isKnownCode(key) ? key : code}`, display)
}

export { isInfoCode }

/** An error code and its params, ready for `errorMessage`. */
export interface ErrorReport {
  code: string
  params: JsonObject
}

/** Code and params of anything thrown by an ipc.ts call. */
export function errorOf(cause: unknown): ErrorReport {
  if (cause instanceof IpcError)
    return { code: cause.code, params: cause.params }
  console.error(cause)

  return { code: FRONT_CODES.ipcError, params: {} }
}
