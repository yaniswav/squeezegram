// What `queue_clear` deletes (src-tauri/src/queue/store.rs, `ClearScope::removes`).

/** Sent, failed and cancelled items (`finished`), or everything that is not running (`all`). */
export type ClearScope = "finished" | "all"

const RUNNING: ReadonlySet<string> = new Set([
  "probing",
  "encoding",
  "remuxing",
  "uploading",
  "waiting_server",
])

const FINISHED: ReadonlySet<string> = new Set(["sent", "failed", "cancelled"])

/** The orchestrator is on the item, or holds it while Telegram makes it wait (`State::is_running`). */
export function isRunning(state: string): boolean {
  return RUNNING.has(state)
}

export function isClearable(state: string, scope: ClearScope): boolean {
  return scope === "finished" ? FINISHED.has(state) : !isRunning(state)
}

/** Items a clear would delete, as far as the list shown tells (the core has the last word). */
export function countClearable(
  items: readonly {
    id?: number
    state: string
    source_item_id?: number | null
  }[],
  scope: ClearScope,
): number {
  const needed = new Set<number>()

  for (const item of items) {
    if (!isClearable(item.state, scope) && item.source_item_id != null) {
      needed.add(item.source_item_id)
    }
  }

  return items.filter(
    (item) =>
      isClearable(item.state, scope) &&
      (item.id === undefined || !needed.has(item.id)),
  ).length
}
