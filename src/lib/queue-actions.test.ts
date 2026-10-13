// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { test } from "node:test"
import { canPause, canStart } from "./queue-actions.ts"

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
  "cancelled",
  "unknown",
]

const only = (state: string) => [{ state }]

test("start has an effect only with an item the loop can take", () => {
  const startable = STATES.filter((state) => canStart(only(state)))

  // `Store::next_runnable` reads `ready` and `encoded` items, nothing else.
  assert.deepEqual(startable, ["ready", "encoded"])
  assert.equal(canStart([]), false)
  assert.equal(canStart([{ state: "failed" }, { state: "ready" }]), true)
})

test("pause has an effect only while something runs or waits for its turn", () => {
  const pausable = STATES.filter((state) => canPause(only(state)))

  assert.deepEqual(pausable, [
    "ready",
    "probing",
    "encoding",
    "remuxing",
    "encoded",
    "uploading",
    "waiting_server",
  ])
  assert.equal(canPause([]), false)
  assert.equal(
    canPause([{ state: "sent" }, { state: "pending_review" }]),
    false,
  )
})
