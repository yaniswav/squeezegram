import { createContext } from "react"
import type { UpdateInfo, UpdateNotice, UpdateProgress } from "@/lib/ipc"
import { useRequiredContext } from "./context"

export interface UpdateApi {
  /** From `update_info`; null until the core has answered, or outside the desktop app. */
  info: UpdateInfo | null
  /** The last check: an update when `available`, else "up to date". Null before any. */
  notice: UpdateNotice | null
  /** Download in progress or about to hand over to the installer; null otherwise. */
  progress: UpdateProgress | null
  checking: boolean
  installing: boolean
  /** "Check now"; an error becomes an alert and resolves to null. */
  check(): Promise<UpdateNotice | null>
  /** Download, verify, install, restart (installer build only). */
  install(): Promise<void>
  /** Opens the release page in the browser. */
  openPage(): Promise<void>
}

export const UpdateContext = createContext<UpdateApi | null>(null)

export function useUpdate(): UpdateApi {
  return useRequiredContext(UpdateContext, "useUpdate")
}
