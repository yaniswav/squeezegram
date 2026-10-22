import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"
import {
  CAP_KEYS,
  CAP_MIN,
  DEFAULT_CAPS,
  capIsWhole,
  capMax,
  isCap,
  shownCap,
} from "./advanced-encoding.ts"

const caps = readFileSync(
  new URL("../../src-tauri/src/caps.rs", import.meta.url),
  "utf8",
)

test("the defaults and the minimum are those of the core", () => {
  const block = caps.slice(caps.indexOf("pub const DEFAULT"))

  for (const key of CAP_KEYS) {
    const rust = new RegExp(`\\b${key}: ([0-9_.]+),`).exec(block)?.[1]

    assert.ok(rust, key)
    assert.equal(Number(rust.replaceAll("_", "")), DEFAULT_CAPS[key], key)
  }

  assert.match(caps, new RegExp(`const MIN: f64 = ${CAP_MIN}\\.0;`))
})

test("a cap is accepted from the minimum to twice its default, and only whole where whole", () => {
  for (const key of CAP_KEYS) {
    assert.equal(capMax(key), DEFAULT_CAPS[key] * 2, key)
    assert.ok(isCap(key, CAP_MIN), key)
    assert.ok(isCap(key, capMax(key)), key)
    assert.ok(isCap(key, DEFAULT_CAPS[key]), key)
    assert.ok(!isCap(key, CAP_MIN - 0.5), key)
    assert.ok(!isCap(key, capMax(key) + 1), key)
    assert.ok(!isCap(key, Number.NaN), key)
  }

  assert.ok(isCap("photo_mpx", 100.5))
  assert.ok(!isCap("threads", 4.5))
  assert.equal(capIsWhole("video_mpx"), false)
  assert.equal(capIsWhole("queue_items"), true)
})

test("a saved value out of range is shown clamped, as the core uses it", () => {
  assert.equal(shownCap("threads", 1e9), 12)
  assert.equal(shownCap("threads", -4), CAP_MIN)
  assert.equal(shownCap("threads", 7.6), 8)
  assert.equal(shownCap("video_mpx", 1e9), 75.5)
  assert.equal(shownCap("video_mpx", 50.25), 50.25)
  assert.equal(shownCap("memory_gb", Number.NaN), DEFAULT_CAPS.memory_gb)
})
