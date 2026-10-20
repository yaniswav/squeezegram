// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"
import { isInfoCode, messageKey } from "./error-key.ts"

// SAFETY: this committed catalogue is validated as JSON by locales.test.ts and package checks.
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
    messageKey("FFMPEG_FAILED", { ...stopped, cause: "hevc_gpu_only" }, has),
    "FFMPEG_FAILED_hevc_gpu_only",
  )

  assert.equal(
    messageKey("FFMPEG_KILLED", { ...stopped, cause: "no_cpu_encoder" }, has),
    "FFMPEG_KILLED_no_cpu_encoder",
  )

  assert.equal(
    messageKey("FFMPEG_KILLED", { ...stopped, cause: "hevc_gpu_only" }, has),
    "FFMPEG_KILLED_hevc_gpu_only",
  )

  assert.equal(
    messageKey(
      "FFMPEG_KILLED",
      { ...stopped, cause: "fallback_failed", fallback: "libx264" },
      has,
    ),
    "FFMPEG_KILLED_fallback_failed",
  )

  assert.equal(
    messageKey("ENCODER_FALLBACK", { cause: "silent" }, has),
    "ENCODER_FALLBACK_silent",
  )

  assert.equal(
    messageKey("ENCODER_FALLBACK", { cause: "encoder_error" }, has),
    "ENCODER_FALLBACK",
  )

  // A GPU that only failed to start is tried again on the next file, or given up on the
  // second time: two sentences, as the first one must not promise a session-long CPU.
  assert.equal(
    messageKey("ENCODER_FALLBACK", { cause: "start_failed" }, has),
    "ENCODER_FALLBACK_start_failed",
  )

  assert.equal(
    messageKey("ENCODER_FALLBACK", { cause: "start_failed_again" }, has),
    "ENCODER_FALLBACK_start_failed_again",
  )

  assert.equal(
    messageKey("ENCODER_FALLBACK", { cause: "suspected_kill" }, has),
    "ENCODER_FALLBACK_suspected_kill",
  )

  assert.equal(
    messageKey("ENCODER_FALLBACK", { cause: "suspected_kill_gpu_off" }, has),
    "ENCODER_FALLBACK_suspected_kill_gpu_off",
  )

  // No params or an unknown cause: the general sentence.
  assert.equal(messageKey("FFMPEG_FAILED", {}, has), "FFMPEG_FAILED")
  assert.equal(
    messageKey("FFMPEG_FAILED", { cause: "nope" }, has),
    "FFMPEG_FAILED",
  )
  assert.equal(
    messageKey("FFMPEG_KILLED", { cause: "nope" }, has),
    "FFMPEG_KILLED",
  )

  assert.equal(
    messageKey("TG_WRITE_FORBIDDEN", { reason: "banned" }, has),
    "TG_WRITE_FORBIDDEN_banned",
  )

  // `cause` only means something for the codes that declare it.
  assert.equal(messageKey("DISK_SPACE", { cause: "silent" }, has), "DISK_SPACE")
})

test("an unreadable personal api_hash is not told as a build without identity", () => {
  assert.equal(
    messageKey(
      "TG_NOT_CONFIGURED",
      { problem: "override_api_hash_unavailable" },
      has,
    ),
    "TG_NOT_CONFIGURED_override_api_hash_unavailable",
  )
  // Every other problem keeps the general sentence.
  assert.equal(
    messageKey("TG_NOT_CONFIGURED", { problem: "api_id_missing" }, has),
    "TG_NOT_CONFIGURED",
  )
})

test("the GPU sentences only use params the core sends", () => {
  // `duration` is formatted from `atSeconds` by errors.ts.
  const sent = {
    FFMPEG_FAILED_fallback_failed: [
      "duration",
      "encoder",
      "exitCode",
      "fallback",
    ],
    FFMPEG_FAILED_no_cpu_encoder: ["duration", "encoder", "exitCode"],
    FFMPEG_FAILED_hevc_gpu_only: ["duration", "encoder", "exitCode"],
    FFMPEG_KILLED: ["duration", "encoder", "exitCode"],
    FFMPEG_KILLED_fallback_failed: [
      "duration",
      "encoder",
      "exitCode",
      "fallback",
    ],
    FFMPEG_KILLED_no_cpu_encoder: ["duration", "encoder", "exitCode"],
    FFMPEG_KILLED_hevc_gpu_only: ["duration", "encoder", "exitCode"],
    ENCODER_FALLBACK: ["duration", "from", "to"],
    ENCODER_FALLBACK_silent: ["duration", "from", "to"],
    ENCODER_FALLBACK_start_failed: ["from", "to"],
    ENCODER_FALLBACK_start_failed_again: ["from", "to"],
    ENCODER_FALLBACK_suspected_kill: ["duration", "from", "to"],
    ENCODER_FALLBACK_suspected_kill_gpu_off: ["duration", "from", "to"],
  } satisfies Readonly<Record<string, string[]>>

  for (const [key, params] of Object.entries(sent)) {
    assert.ok(has(key), key)
    assert.deepEqual(placeholders(errors[key]), params.sort(), key)
  }

  assert.match(
    errors.ENCODER_FALLBACK_silent,
    /stopped without an error message \(after \{\{duration\}\}\)\. squeezegram switched to the CPU encoder/,
  )
})

test("suspected external stops stay explicit about the uncertainty", () => {
  for (const key of [
    "FFMPEG_KILLED",
    "FFMPEG_KILLED_fallback_failed",
    "FFMPEG_KILLED_no_cpu_encoder",
    "FFMPEG_KILLED_hevc_gpu_only",
    "ENCODER_FALLBACK_suspected_kill",
    "ENCODER_FALLBACK_suspected_kill_gpu_off",
  ]) {
    assert.match(errors[key], /Another program may have stopped ffmpeg/, key)
    assert.match(errors[key], /a GPU encoder failure is also possible/, key)
  }

  assert.match(
    errors.ENCODER_FALLBACK_suspected_kill,
    /for this file and will try the GPU again on the next one/,
  )
  assert.match(
    errors.ENCODER_FALLBACK_suspected_kill_gpu_off,
    /keeps using it until the app restarts/,
  )
})

test("the fallback notice is information, failures are errors", () => {
  assert.equal(isInfoCode("ENCODER_FALLBACK"), true)
  assert.equal(isInfoCode("CODEC_FALLBACK"), true)
  assert.equal(isInfoCode("FFMPEG_FAILED"), false)
  assert.equal(isInfoCode("FFMPEG_KILLED"), false)
})

test("a refused proxy names its field, every field the core can name has a sentence", () => {
  // `transport::proxy::ProxyField` and the `kind` of `proxy_test`.
  for (const field of [
    "kind",
    "host",
    "port",
    "username",
    "password",
    "secret",
  ])
    assert.equal(
      messageKey("PROXY_INVALID", { field }, has),
      `PROXY_INVALID_${field}`,
    )
  assert.equal(messageKey("PROXY_INVALID", {}, has), "PROXY_INVALID")
  assert.deepEqual(placeholders(errors.TG_PROXY_FAILED), ["message"])
})

test("an unsafe temp folder uses the specific settings alert sentence", () => {
  assert.equal(messageKey("TEMP_DIR_UNSAFE", {}, has), "TEMP_DIR_UNSAFE")
  assert.equal(isInfoCode("TEMP_DIR_UNSAFE"), false)
  assert.deepEqual(placeholders(errors.TEMP_DIR_UNSAFE), [])
  assert.match(errors.TEMP_DIR_UNSAFE, /squeezegram-temp folder/)
})
