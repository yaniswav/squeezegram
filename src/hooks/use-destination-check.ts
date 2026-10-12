import { useEffect, useState } from "react"
import { errorOf, type ErrorReport } from "@/i18n/errors"
import {
  destinationCheck,
  type DestinationCheck,
  type SendMode,
} from "@/lib/ipc"

type CheckOutcome = { check: DestinationCheck } | { failed: ErrorReport }

export type CheckState =
  | { phase: "idle" }
  | { phase: "checking" }
  | { phase: "done"; check: DestinationCheck }
  | ({ phase: "failed" } & ErrorReport)

/** A destination of the batch and the mode its files go out in. */
export interface CheckTarget {
  destinationId: number
  mode: SendMode
}

const MODES: readonly SendMode[] = ["video", "document", "photo"]

function keyOf(target: CheckTarget): string {
  return `${target.destinationId}:${target.mode}`
}

function targetOf(key: string): CheckTarget | null {
  const [id, name] = key.split(":")
  const mode = MODES.find((mode) => mode === name)

  return mode ? { destinationId: Number(id), mode } : null
}

/**
 * `destination_check` for every destination of a batch, each with its own result: one that
 * Telegram refuses is reported for that destination only. All are checked again when the
 * destinations or the mode change; a result stays shown until its new one arrives.
 * `checkOf(null)` is idle.
 */
export function useDestinationChecks(targets: readonly CheckTarget[]) {
  const [outcomes, setOutcomes] = useState<ReadonlyMap<string, CheckOutcome>>(
    () => new Map(),
  )

  // A string, so the effect runs when the set changes, not on every render.
  const wanted = [...new Set(targets.map(keyOf))].join(" ")

  useEffect(() => {
    if (wanted === "") return
    let stale = false

    for (const key of wanted.split(" ")) {
      const target = targetOf(key)

      if (!target) continue
      const record = (outcome: CheckOutcome) => {
        if (!stale) setOutcomes((current) => new Map(current).set(key, outcome))
      }

      destinationCheck(target.destinationId, target.mode).then(
        (check) => record({ check }),
        (cause: unknown) => record({ failed: errorOf(cause) }),
      )
    }

    return () => {
      stale = true
    }
  }, [wanted])

  return function checkOf(target: CheckTarget | null): CheckState {
    if (target === null) return { phase: "idle" }
    const outcome = outcomes.get(keyOf(target))

    if (outcome === undefined) return { phase: "checking" }

    if ("failed" in outcome) return { phase: "failed", ...outcome.failed }

    return { phase: "done", check: outcome.check }
  }
}
