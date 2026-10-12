import { createContext } from "react"
import type { JsonObject } from "@/lib/json"
import { useRequiredContext } from "./context"

/** An error or warning shown to the user, translated from its code at render time. */
export interface AppAlert {
  id: number
  code: string
  params: JsonObject
  itemId?: number
}

export interface AlertsApi {
  alerts: AppAlert[]
  push(code: string, params?: JsonObject, itemId?: number): void
  /** Anything thrown by an ipc.ts call. */
  pushError(cause: unknown, itemId?: number): void
  dismiss(id: number): void
}

export const AlertsContext = createContext<AlertsApi | null>(null)

export function useAlerts(): AlertsApi {
  return useRequiredContext(AlertsContext, "useAlerts")
}
