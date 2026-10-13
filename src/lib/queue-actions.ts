// When the queue's Start and Pause would change anything (src-tauri/src/queue/orchestrator.rs).
// Plain state strings, like queue-clear.ts, so that node's runner can test it.

/** `Store::next_runnable` takes these, in position order. */
const RUNNABLE: ReadonlySet<string> = new Set(["ready", "encoded"])

/** Running, waiting for the server, or waiting for its turn (`isProcessable`). */
const PROCESSABLE: ReadonlySet<string> = new Set([
  "ready",
  "probing",
  "encoding",
  "remuxing",
  "encoded",
  "uploading",
  "waiting_server",
])

/**
 * `queue_start` gives the loop something to take. Items held for review, failed or
 * unconfirmed wait for the user, and a running item goes on whether the queue is started or
 * not.
 */
export function canStart(items: readonly { state: string }[]): boolean {
  return items.some((item) => RUNNABLE.has(item.state))
}

/** `queue_pause` holds something back: an item runs, or waits for the server or its turn. */
export function canPause(items: readonly { state: string }[]): boolean {
  return items.some((item) => PROCESSABLE.has(item.state))
}
