// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"
import { DecodeError, type Decoder } from "./decode.ts"
import { isJson, isJsonObject, isJsonString } from "./json.ts"
import {
  newKeyNeeded,
  updateInfoDecoder,
  updateNoticeDecoder,
  updateProgressDecoder,
} from "./update.ts"

const RELEASES = "https://github.com/yaniswav/squeezegram/releases/latest"

const TAG = "https://github.com/yaniswav/squeezegram/releases/tag/v"

/** Where the decoder says the payload breaks its contract; null if it decodes. */
function failurePath(run: () => void): string | null {
  try {
    run()
  } catch (error) {
    return error instanceof DecodeError ? error.path : null
  }

  return null
}

test("an update notice as the core emits it decodes", () => {
  const payload = {
    available: true,
    version: "0.2.0",
    kind: "portable",
    notes: "Fixes",
    url: `${TAG}0.2.0`,
    requires_new_key: false,
  }

  assert.deepEqual(updateNoticeDecoder(payload, "$"), payload)
})

test("a notice that needs a new key carries the covered major", () => {
  const payload = {
    available: true,
    version: "2.0.0",
    kind: "installer",
    url: `${TAG}2.0.0`,
    requires_new_key: true,
    covers_major: 1,
  }

  const notice = updateNoticeDecoder(payload, "$")

  assert.deepEqual(notice, payload)
  assert.deepEqual(newKeyNeeded(notice), { covers: 1, next: 2 })
})

test("the new-key text needs the flag, the covered major and a readable version", () => {
  const base = {
    available: true,
    version: "2.1.0-rc.1",
    kind: "portable" as const,
    url: RELEASES,
  }

  assert.deepEqual(
    newKeyNeeded({ ...base, requires_new_key: true, covers_major: 0 }),
    { covers: 0, next: 2 },
  )
  assert.equal(newKeyNeeded({ ...base, requires_new_key: false }), null)

  assert.equal(
    newKeyNeeded({ ...base, requires_new_key: false, covers_major: 1 }),
    null,
  )

  assert.equal(newKeyNeeded({ ...base, requires_new_key: true }), null)

  assert.equal(
    newKeyNeeded({
      ...base,
      version: "next",
      requires_new_key: true,
      covers_major: 1,
    }),
    null,
  )
})

test("notes are optional and the 'no update' notice decodes", () => {
  const payload = {
    available: false,
    version: "0.1.0",
    kind: "installer",
    url: RELEASES,
    requires_new_key: false,
  }

  const notice = updateNoticeDecoder(payload, "$")

  assert.deepEqual(notice, payload)
  assert.equal("notes" in notice, false)
})

test("the info of a session with and without a notice decodes", () => {
  assert.deepEqual(
    updateInfoDecoder(
      { kind: "installer", current_version: "0.1.0", notice: null },
      "$",
    ),
    { kind: "installer", current_version: "0.1.0", notice: null },
  )

  const notice = {
    available: true,
    version: "0.2.0",
    kind: "installer",
    url: `${TAG}0.2.0`,
    requires_new_key: false,
  }

  assert.deepEqual(
    updateInfoDecoder(
      { kind: "installer", current_version: "0.1.0", notice },
      "$",
    ).notice,
    notice,
  )
})

test("progress with and without a length decodes", () => {
  assert.deepEqual(
    updateProgressDecoder(
      { phase: "downloading", downloaded: 512, total: 2048, percent: 25 },
      "$",
    ),
    { phase: "downloading", downloaded: 512, total: 2048, percent: 25 },
  )

  assert.deepEqual(
    updateProgressDecoder(
      { phase: "installing", downloaded: 9, total: null, percent: null },
      "$",
    ),
    { phase: "installing", downloaded: 9, total: null, percent: null },
  )
})

test("extra keys are left out", () => {
  assert.deepEqual(
    updateProgressDecoder(
      {
        phase: "downloading",
        downloaded: 1,
        total: 2,
        percent: 50,
        token: "secret",
      },
      "$",
    ),
    { phase: "downloading", downloaded: 1, total: 2, percent: 50 },
  )
})

test("a payload that breaks a contract names the mismatch", () => {
  const notice = {
    available: true,
    version: "0.2.0",
    kind: "installer",
    requires_new_key: false,
  }

  const progress = {
    phase: "downloading",
    downloaded: 1,
    total: 2,
    percent: 50,
  }

  const cases: [string, unknown, string][] = [
    ["notice", null, "$"],
    ["notice", { ...notice, url: RELEASES, available: "yes" }, "$.available"],
    ["notice", { ...notice, url: RELEASES, kind: "msi" }, "$.kind"],
    ["notice", { ...notice, url: RELEASES, version: 2 }, "$.version"],
    ["notice", { ...notice, url: RELEASES, notes: 3 }, "$.notes"],
    ["notice", notice, "$.url"],
    [
      "notice",
      { ...notice, url: RELEASES, requires_new_key: "no" },
      "$.requires_new_key",
    ],
    [
      "notice",
      { ...notice, url: RELEASES, requires_new_key: true, covers_major: "1" },
      "$.covers_major",
    ],
    [
      "notice",
      { available: true, version: "0.2.0", kind: "installer", url: RELEASES },
      "$.requires_new_key",
    ],
    ["progress", { ...progress, phase: "verifying" }, "$.phase"],
    ["progress", { ...progress, downloaded: "1" }, "$.downloaded"],
    ["progress", { phase: "downloading", downloaded: 1 }, "$.total"],
    [
      "progress",
      { phase: "downloading", downloaded: 1, total: 2 },
      "$.percent",
    ],
    ["info", { kind: "installer", current_version: "0.1.0" }, "$.notice"],
    ["info", { kind: "x", current_version: "0.1.0", notice: null }, "$.kind"],
  ]

  for (const [which, value, path] of cases) {
    assert.ok(isJson(value))

    const decode: Decoder<unknown> =
      which === "notice"
        ? updateNoticeDecoder
        : which === "progress"
          ? updateProgressDecoder
          : updateInfoDecoder

    assert.equal(
      failurePath(() => decode(value, "$")),
      path,
      `${which}: ${JSON.stringify(value)}`,
    )
  }
})

test("every update error code the core can return has an English text", () => {
  const text: unknown = JSON.parse(
    readFileSync(new URL("../i18n/locales/en.json", import.meta.url), "utf8"),
  )

  assert.ok(isJson(text) && isJsonObject(text) && isJsonObject(text.errors))

  for (const code of [
    "UPDATE_CHECK_FAILED",
    "UPDATE_NOT_PUBLISHED",
    "UPDATE_DOWNLOAD_FAILED",
    "UPDATE_SIGNATURE_INVALID",
    "UPDATE_INSTALL_FAILED",
    "UPDATE_NEEDS_NEW_KEY",
  ]) {
    assert.ok(isJsonString(text.errors[code]), code)
  }
})
