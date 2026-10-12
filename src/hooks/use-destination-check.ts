import { useEffect, useState } from "react"
import { errorOf, type ErrorReport } from "@/i18n/errors"
import {
  destinationCheck,
  type DestinationCheck,
  type SendMode,
} from "@/lib/ipc"

type CheckOutcome =
  | { key: string; check: DestinationCheck }
  | { key: string; failed: ErrorReport }

export type CheckState =
  | { phase: "idle" }
  | { phase: "checking" }
  | { phase: "done"; check: DestinationCheck }
  | ({ phase: "failed" } & ErrorReport)

/** `destination_check` for the chosen destination and mode, redone when either changes. */
export function useDestinationCheck(
  destinationId: number | null,
  mode: SendMode | null,
): CheckState {
  const [outcome, setOutcome] = useState<CheckOutcome | null>(null)

  const key =
    destinationId === null || mode === null ? null : `${destinationId}:${mode}`

  useEffect(() => {
    if (destinationId === null || mode === null || key === null) return
    let stale = false
    destinationCheck(destinationId, mode).then(
      (check) => {
        if (!stale) setOutcome({ key, check })
      },
      (cause: unknown) => {
        if (!stale) setOutcome({ key, failed: errorOf(cause) })
      },
    )

    return () => {
      stale = true
    }
  }, [destinationId, mode, key])

  if (key === null) return { phase: "idle" }

  if (outcome === null || outcome.key !== key) return { phase: "checking" }

  if ("failed" in outcome) return { phase: "failed", ...outcome.failed }

  return { phase: "done", check: outcome.check }
}
