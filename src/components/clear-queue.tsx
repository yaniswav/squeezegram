import { Trash2 } from "lucide-react"
import { useId, useState } from "react"
import { useTranslation } from "react-i18next"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { Button } from "@/components/ui/button"
import { queueClear, type ClearScope } from "@/lib/ipc"
import { countClearable } from "@/lib/queue-clear"
import { useAlerts } from "@/state/alerts"
import { useQueue } from "@/state/queue"

const SCOPES: readonly ClearScope[] = ["finished", "all"]

interface ClearQueueProps {
  /** Receives the outcome sentence ("12 removed."). */
  onDone: (message: string) => void
}

/** "Clear queue" button and its confirmation, with the choice of what to clear. */
export function ClearQueue({ onDone }: ClearQueueProps) {
  const { t } = useTranslation()
  const { pushError } = useAlerts()
  const { status, items, refresh } = useQueue()
  const [open, setOpen] = useState(false)
  const [scope, setScope] = useState<ClearScope>("finished")
  const [busy, setBusy] = useState(false)
  const group = useId()

  const counts: Record<ClearScope, number> = {
    finished: countClearable(items, "finished"),
    all: countClearable(items, "all"),
  }
  const unconfirmed = items.filter((item) => item.state === "unknown").length

  function ask() {
    setScope("finished")
    setOpen(true)
  }

  async function clear() {
    setBusy(true)

    try {
      const { removed, running } = await queueClear(scope)
      await refresh()
      const outcome =
        removed === 0
          ? t("queue.clear.nothing")
          : t("queue.clear.removed", { count: removed })
      onDone(
        running > 0
          ? `${outcome} ${t("queue.clear.running", { count: running })}`
          : outcome,
      )
    } catch (error) {
      pushError(error)
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <Button
        variant="outline"
        disabled={busy || status !== "ready" || counts.all === 0}
        onClick={ask}
      >
        <Trash2 data-icon="inline-start" />
        {t("queue.clear.button")}
      </Button>
      <ConfirmDialog
        open={open}
        onOpenChange={setOpen}
        title={t("queue.clear.title")}
        description={t("queue.clear.body")}
        confirmLabel={t(`queue.clear.${scope}.label`)}
        cancelLabel={t("common.cancel")}
        destructive
        confirmDisabled={counts[scope] === 0}
        onConfirm={() => void clear()}
      >
        <fieldset className="space-y-2">
          <legend className="mb-2 text-sm font-medium">
            {t("queue.clear.choices")}
          </legend>
          {SCOPES.map((choice) => (
            <label
              key={choice}
              className="flex cursor-pointer items-start gap-3 rounded-lg border p-3 text-sm has-checked:border-primary has-checked:bg-muted/50 has-focus-visible:ring-3 has-focus-visible:ring-ring/50"
            >
              <input
                type="radio"
                name={group}
                value={choice}
                checked={scope === choice}
                onChange={() => setScope(choice)}
                className="mt-0.5 accent-primary"
              />
              <span className="space-y-0.5">
                <span className="block font-medium">
                  {t(`queue.clear.${choice}.label`)}
                </span>
                <span className="block text-muted-foreground">
                  {t(`queue.clear.${choice}.hint`, { count: counts[choice] })}
                </span>
              </span>
            </label>
          ))}
        </fieldset>
        {scope === "all" && unconfirmed > 0 && (
          <p className="text-sm text-warning">
            {t("queue.clear.unconfirmed", { count: unconfirmed })}
          </p>
        )}
      </ConfirmDialog>
    </>
  )
}
