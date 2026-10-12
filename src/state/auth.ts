import { createContext } from "react"
import type { AuthState } from "@/lib/ipc"
import { useRequiredContext } from "./context"

export interface AuthApi {
  status: "loading" | "ready" | "error"
  /** Last `auth://state` payload (or the `auth_state` answer at load). */
  auth: AuthState | null
}

export const AuthContext = createContext<AuthApi | null>(null)

export function useAuth(): AuthApi {
  return useRequiredContext(AuthContext, "useAuth")
}
