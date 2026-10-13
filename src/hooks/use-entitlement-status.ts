import { useEffect, useState } from "react"
import {
  entitlementsStatus,
  onEntitlementsStatus,
  type EntitlementStatus,
} from "@/lib/ipc"
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
        onEvent: setStatus,
        fetch: entitlementsStatus,
        onFetched: (next, eventSeen) => {
          if (!eventSeen) setStatus(next)
        },
        onFetchFailed: () => setStatus((current) => current ?? "failed"),
      }),
    [],
  )

  return [status, setStatus] as const
}
