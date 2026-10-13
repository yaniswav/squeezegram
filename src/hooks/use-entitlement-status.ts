import { useEffect, useState } from "react"
import {
  entitlementsStatus,
  logFrontend,
  onEntitlementsStatus,
  type EntitlementStatus,
} from "@/lib/ipc"
import { entitlementsSeenLog } from "@/lib/entitlements"
import { follow } from "@/lib/subscription"

/** `null` while loading, `"failed"` when the core could not be asked. */
export type EntitlementState = EntitlementStatus | "failed" | null

/**
 * `entitlements_status`, then every `entitlements://status` (license change, sign-in,
 * sign-out). At startup the status goes from `pending` to `license` as soon as TDLib has
 * signed in, and the core sends that change only once: the listener is registered before the
 * status is asked, and an event already received wins over the answer.
 */
export function useEntitlementStatus() {
  const [status, setStatus] = useState<EntitlementState>(null)

  useEffect(
    () =>
      follow({
        listen: onEntitlementsStatus,
        onEvent: (next) => {
          logFrontend(entitlementsSeenLog(next, "event"))
          setStatus(next)
        },
        fetch: entitlementsStatus,
        onFetched: (next, eventSeen) => {
          logFrontend(entitlementsSeenLog(next, "fetch"))

          if (!eventSeen) setStatus(next)
        },
        onFetchFailed: () => setStatus((current) => current ?? "failed"),
      }),
    [],
  )

  return [status, setStatus] as const
}
