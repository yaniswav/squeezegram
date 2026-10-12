import { createContext } from "react"
import type { Settings, SettingsPatch } from "@/lib/ipc"
import { useRequiredContext } from "./context"

export interface SettingsApi {
  status: "loading" | "ready" | "error"
  settings: Settings | null
  /** Sends a merge patch to `settings_set`; rejects with an IpcError. */
  save(patch: SettingsPatch): Promise<Settings>
  reload(): Promise<void>
}

export const SettingsContext = createContext<SettingsApi | null>(null)

export function useSettings(): SettingsApi {
  return useRequiredContext(SettingsContext, "useSettings")
}
