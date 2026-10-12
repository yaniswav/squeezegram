// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"
import { isInfoCode, messageKey } from "./error-key.ts"

const catalogue = JSON.parse(
  readFileSync(new URL("./locales/en.json", import.meta.url), "utf8"),
) as { errors: Record<string, string> }

const errors = catalogue.errors
const has = (key: string) => Object.hasOwn(errors, key)

/** `{{name}}` placeholders of a sentence. */
function placeholders(text: string): string[] {
  return [...text.matchAll(/\{\{(\w+)\}\}/g)].map((match) => match[1]).sort()
}

test("a cause picks the precise sentence of the GPU fallback codes", () => {
  const stopped = { encoder: "h264_nvenc", exitCode: 1, atSeconds: 4 }

  assert.equal(
    messageKey(
      "FFMPEG_FAILED",
      { ...stopped, cause: "fallback_failed", fallback: "libx264" },
      has,
    ),
    "FFMPEG_FAILED_fallback_failed",
  )

  assert.equal(
    messageKey("FFMPEG_FAILED", { ...stopped, cause: "no_cpu_encoder" }, has),
    "FFMPEG_FAILED_no_cpu_encoder",
  )

  assert.equal(
    messageKey("ENCODER_FALLBACK", { cause: "silent" }, has),
    "ENCODER_FALLBACK_silent",
  )

  assert.equal(
    messageKey("ENCODER_FALLBACK", { cause: "encoder_error" }, has),
    "ENCODER_FALLBACK",
  )

  // No params or an unknown cause: the general sentence.
  assert.equal(messageKey("FFMPEG_FAILED", {}, has), "FFMPEG_FAILED")
  assert.equal(
    messageKey("FFMPEG_FAILED", { cause: "nope" }, has),
    "FFMPEG_FAILED",
  )

  assert.equal(
    messageKey("TG_WRITE_FORBIDDEN", { reason: "banned" }, has),
    "TG_WRITE_FORBIDDEN_banned",
  )

  // `cause` only means something for the codes that declare it.
  assert.equal(messageKey("DISK_SPACE", { cause: "silent" }, has), "DISK_SPACE")
})

test("the GPU sentences only use params the core sends", () => {
  // `duration` is formatted from `atSeconds` by errors.ts.
  const sent: Record<string, string[]> = {
    FFMPEG_FAILED_fallback_failed: [
      "duration",
      "encoder",
      "exitCode",
      "fallback",
    ],
    FFMPEG_FAILED_no_cpu_encoder: ["duration", "encoder", "exitCode"],
    ENCODER_FALLBACK: ["duration", "from", "to"],
    ENCODER_FALLBACK_silent: ["duration", "from", "to"],
  }

  for (const [key, params] of Object.entries(sent)) {
    assert.ok(has(key), key)
    assert.deepEqual(placeholders(errors[key]), params.sort(), key)
  }

  assert.match(
    errors.ENCODER_FALLBACK_silent,
    /stopped without an error message \(after \{\{duration\}\}\)\. squeezegram switched to the CPU encoder/,
  )
})

test("the fallback notice is information, failures are errors", () => {
  assert.equal(isInfoCode("ENCODER_FALLBACK"), true)
  assert.equal(isInfoCode("FFMPEG_FAILED"), false)
})
