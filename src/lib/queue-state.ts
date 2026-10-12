// Groupings of the item state machine (src-tauri/src/queue/state.rs).
import type { Item, ItemState } from "@/lib/ipc"

export type { ItemState as QueueState } from "@/lib/ipc"

/** Caption and mode may still change (`State::is_not_started`). */
export function isNotStarted(state: ItemState): boolean {
  return state === "pending_review" || state === "ready"
}

/** Nothing more will happen to the item. */
export function isFinished(state: ItemState): boolean {
  return state === "sent" || state === "cancelled"
}

/** `can_transition(_, Cancelled)`: any state but `sent` and `cancelled`. */
export function canCancel(state: ItemState): boolean {
  return !isFinished(state)
}

/** The item is being worked on by the orchestrator right now. */
export function isWorking(state: ItemState): boolean {
  return (
    state === "probing" ||
    state === "encoding" ||
    state === "remuxing" ||
    state === "uploading"
  )
}

/**
 * The orchestrator has something to do with the item: it is running, waiting for the
 * server, or queued for the loop (`ready`, or `encoded` after a restart). Other states
 * (`pending_review`, `failed`, `unknown`, finished) are never picked up by themselves.
 */
export function isProcessable(state: ItemState): boolean {
  return (
    isWorking(state) ||
    state === "waiting_server" ||
    state === "ready" ||
    state === "encoded"
  )
}

/** First step (analysis, then compression or remux) is over. */
export function isPrepared(item: Item): boolean {
  return (
    item.state === "encoded" ||
    item.state === "uploading" ||
    item.state === "waiting_server" ||
    item.state === "sent" ||
    ((item.state === "failed" || item.state === "unknown") &&
      item.output_path !== null)
  )
}
