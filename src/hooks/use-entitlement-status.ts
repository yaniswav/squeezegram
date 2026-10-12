import { useEffect, useState } from "react"
import {
  entitlementsStatus,
  onEntitlementsStatus,
  type EntitlementStatus,
} from "@/lib/ipc"

/** `null` while loading, `"failed"` when the core could not be asked. */
export type EntitlementState = EntitlementStatus | "failed" | null

/** `entitlements_status`, then every `entitlements://status` (license change, sign-in, sign-out). */
export function useEntitlementStatus() {
  const [status, setStatus] = useState<EntitlementState>(null)

  useEffect(() => {
    let stale = false
    const stop = onEntitlementsStatus(setStatus)

    entitlementsStatus().then(
      (next) => {
        if (!stale) setStatus(next)
      },
      () => {
        if (!stale) setStatus((current) => current ?? "failed")
      },
    )

    return () => {
      stale = true
      stop()
    }
  }, [])

  return [status, setStatus] as const
}
