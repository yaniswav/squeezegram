// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { test } from "node:test"
import {
  countClearable,
  isClearable,
  isRunning,
  type ClearScope,
} from "./queue-clear.ts"

const STATES: readonly string[] = [
  "pending_review",
  "ready",
  "probing",
  "encoding",
  "remuxing",
  "encoded",
  "uploading",
  "waiting_server",
  "sent",
  "failed",
  "unknown",
  "cancelled",
]

function clearable(scope: ClearScope): string[] {
  return STATES.filter((state) => isClearable(state, scope))
}

test("clearing finished items only removes sent, failed and cancelled", () => {
  assert.deepEqual(clearable("finished"), ["sent", "failed", "cancelled"])
})

test("clearing everything keeps exactly the running items", () => {
  assert.deepEqual(
    STATES.filter((state) => isRunning(state)),
    ["probing", "encoding", "remuxing", "uploading", "waiting_server"],
  )
  assert.deepEqual(
    clearable("all"),
    STATES.filter((state) => !isRunning(state)),
  )
  assert.deepEqual(clearable("all"), [
    "pending_review",
    "ready",
    "encoded",
    "sent",
    "failed",
    "unknown",
    "cancelled",
  ])
})

test("the finished scope is always inside the all scope", () => {
  for (const state of clearable("finished")) {
    assert.ok(isClearable(state, "all"), state)
  }
})

test("items are counted per scope", () => {
  const items = STATES.map((state) => ({ state }))

  assert.equal(countClearable(items, "finished"), 3)
  assert.equal(countClearable(items, "all"), 7)
  assert.equal(countClearable([], "all"), 0)
  assert.equal(countClearable([{ state: "uploading" }], "all"), 0)
})

test("a primary still needed by a kept copy is excluded from the clear count", () => {
  const items = [
    { id: 1, state: "sent", source_item_id: null },
    { id: 2, state: "ready", source_item_id: 1 },
  ]

  assert.equal(countClearable(items, "finished"), 0)
  assert.equal(countClearable(items, "all"), 2)
  assert.equal(
    countClearable([items[0], { ...items[1], state: "uploading" }], "all"),
    0,
  )
})
