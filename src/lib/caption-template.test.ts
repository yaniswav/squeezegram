import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"
import {
  CAPTION_VARIABLES,
  STANDARD_CAPTION_CHARS,
  captionLength,
  captionLimit,
  counterState,
} from "./caption-template.ts"

test("the limit is the account's, else the standard one", () => {
  assert.equal(captionLimit(4096), 4096)
  assert.equal(captionLimit(undefined), STANDARD_CAPTION_CHARS)
})

test("characters are counted as the core does", () => {
  assert.equal(captionLength(""), 0)
  assert.equal(captionLength("été"), 3)
  // One code point outside the BMP is two UTF-16 units but one character.
  assert.equal(captionLength("a😀b"), 3)
})

test("the counter warns before the limit and fails above it", () => {
  assert.equal(counterState(0, 1024), "ok")
  assert.equal(counterState(921, 1024), "ok")
  assert.equal(counterState(922, 1024), "near")
  assert.equal(counterState(1024, 1024), "near")
  assert.equal(counterState(1025, 1024), "over")
})

test("every variable is described in the help", () => {
  const en = JSON.parse(
    readFileSync(new URL("../i18n/locales/en.json", import.meta.url), "utf8"),
  ) as { caption: { var: Record<string, string> } }

  assert.deepEqual(
    Object.keys(en.caption.var).sort(),
    [...CAPTION_VARIABLES].sort(),
  )
})
