import { createContext } from "react"
import type { Item, QueueProgress } from "@/lib/ipc"
import { useRequiredContext } from "./context"

export interface QueueApi {
  status: "loading" | "ready" | "error"
  /** By position, as `queue_list` returns them. */
  items: Item[]
  /**
   * Last `queue://progress` per item, with the time it arrived (ms) and whether the encode
   * runs on the CPU after the GPU stopped (`detail: "cpu_fallback"`, kept while encoding).
   */
  progress: ReadonlyMap<
    number,
    QueueProgress & { receivedAt: number; cpuFallback: boolean }
  >
  /** From `queue_status` and `queue://status`; null until the core has answered. */
  running: boolean | null
  refresh(): Promise<void>
}

export const QueueContext = createContext<QueueApi | null>(null)

export function useQueue(): QueueApi {
  return useRequiredContext(QueueContext, "useQueue")
}
