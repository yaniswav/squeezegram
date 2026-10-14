import assert from "node:assert/strict"
import { test } from "node:test"
import { deliveryRoute } from "./delivery.ts"

test("pending destinations show a copy while independent items show an upload", () => {
  assert.equal(
    deliveryRoute({ state: "ready", source_item_id: 7, send_via: "upload" }),
    "copy",
  )
  assert.equal(
    deliveryRoute({ state: "ready", source_item_id: null, send_via: "upload" }),
    "upload",
  )
})

test("active fallback progress wins until the queue snapshot arrives", () => {
  assert.equal(
    deliveryRoute(
      { state: "uploading", source_item_id: 7, send_via: "copy" },
      { detail: "remote" },
    ),
    "remote",
  )
  assert.equal(
    deliveryRoute(
      { state: "uploading", source_item_id: 7, send_via: "copy" },
      { phase: "uploading" },
    ),
    "upload",
  )
})

test("finished rows use the actual route even when stale copy progress remains", () => {
  assert.equal(
    deliveryRoute(
      { state: "sent", source_item_id: 7, send_via: "upload" },
      { detail: "copy" },
    ),
    "upload",
  )
  assert.equal(
    deliveryRoute({ state: "unknown", source_item_id: 7, send_via: "remote" }),
    "remote",
  )
})
