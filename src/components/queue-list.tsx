import { ChevronDown, Inbox, Pause, Play } from "lucide-react"
import { AnimatePresence } from "motion/react"
import { useId, useState } from "react"
import { useTranslation } from "react-i18next"
import { ClearQueue } from "@/components/clear-queue"
import { EmptyState } from "@/components/empty-state"
import { QueueItem } from "@/components/queue-item"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardAction,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { useFlash } from "@/hooks/use-flash"
import { useGroupRiskGate } from "@/hooks/use-group-risk-gate"
import { queuePause, queueReorder, queueStart } from "@/lib/ipc"
import { isFinished, isProcessable } from "@/lib/queue-state"
import { cn } from "@/lib/utils"
import { useAlerts } from "@/state/alerts"
import { useDestinations } from "@/state/destinations"
import { useQueue } from "@/state/queue"

export function QueueList() {
  const { t } = useTranslation()
  const { pushError } = useAlerts()
  const { status, items, progress, running, refresh } = useQueue()
  const { byId } = useDestinations()
  const { guard, dialog: groupRiskDialog } = useGroupRiskGate()
  const [showFinished, setShowFinished] = useState(false)
  const [toggling, setToggling] = useState(false)
  const [cleared, setCleared] = useFlash()
  const ids = useId()

  const active = items.filter((item) => !isFinished(item.state))
  const finished = items.filter((item) => isFinished(item.state))
  // `running` only says the orchestrator is armed; nothing runs without a processable item.
  const busy = items.some((item) => isProcessable(item.state))

  // Files queued before the warning existed (or by a watched folder) start here: the
  // notice also comes before the queue is armed for a group.
  function toggle() {
    if (running) return void toggleRunning()
    const targets = items
      .filter((item) => isProcessable(item.state))
      .flatMap((item) => byId.get(item.destination_id) ?? [])

    guard(targets, () => void toggleRunning())
  }

  async function toggleRunning() {
    setToggling(true)

    try {
      if (running) await queuePause()
      else await queueStart()
    } catch (error) {
      pushError(error)
    } finally {
      setToggling(false)
    }
  }

  async function move(index: number, direction: -1 | 1) {
    const order = active.map((item) => item.id)
    const target = index + direction

    if (target < 0 || target >= order.length) return
    ;[order[index], order[target]] = [order[target], order[index]]

    try {
      await queueReorder(order)
      await refresh()
    } catch (error) {
      pushError(error)
    }
  }

  return (
    <Card className="gap-0 pb-0">
      <CardHeader className="border-b">
        <CardTitle>
          <h2 id={`${ids}-title`}>{t("queue.listLabel")}</h2>
        </CardTitle>
        <CardDescription aria-live="polite">
          {running === null
            ? t("common.loading")
            : !running
              ? t("queue.paused")
              : busy
                ? t("queue.running")
                : t("queue.idle")}
        </CardDescription>
        <CardAction className="flex flex-wrap justify-end gap-2">
          <ClearQueue onDone={setCleared} />
          <Button
            variant={!running ? "default" : busy ? "outline" : "secondary"}
            disabled={toggling || status !== "ready" || running === null}
            onClick={toggle}
          >
            {running ? (
              <Pause data-icon="inline-start" />
            ) : (
              <Play data-icon="inline-start" className="rtl:-scale-x-100" />
            )}
            {running ? t("queue.pause") : t("queue.start")}
          </Button>
        </CardAction>
      </CardHeader>

      <p
        role="status"
        className={cleared ? "border-b px-4 py-2 text-sm" : "sr-only"}
      >
        {cleared}
      </p>

      {status === "loading" && (
        <p className="px-4 py-6 text-sm text-muted-foreground">
          {t("common.loading")}
        </p>
      )}
      {status === "error" && (
        <div className="flex items-center justify-between gap-3 px-4 py-6 text-sm">
          <p className="text-destructive">{t("queue.loadError")}</p>
          <Button variant="outline" size="sm" onClick={() => void refresh()}>
            {t("common.retry")}
          </Button>
        </div>
      )}

      {status === "ready" && active.length === 0 && (
        <EmptyState icon={Inbox} title={t("queue.empty.title")}>
          {t("queue.empty.body")}
        </EmptyState>
      )}

      {active.length > 0 && (
        <ul className="divide-y" aria-labelledby={`${ids}-title`}>
          <AnimatePresence initial={false}>
            {active.map((item, index) => (
              <QueueItem
                key={item.id}
                item={item}
                progress={progress.get(item.id)}
                onMove={(direction) => void move(index, direction)}
                isFirst={index === 0}
                isLast={index === active.length - 1}
              />
            ))}
          </AnimatePresence>
        </ul>
      )}

      {finished.length > 0 && (
        <div className="border-t">
          <Button
            variant="ghost"
            className="h-10 w-full justify-start rounded-none px-4"
            aria-expanded={showFinished}
            aria-controls={`${ids}-finished`}
            onClick={() => setShowFinished((shown) => !shown)}
          >
            <ChevronDown
              data-icon="inline-start"
              className={cn(
                "transition-transform motion-reduce:transition-none",
                !showFinished && "-rotate-90 rtl:rotate-90",
              )}
            />
            {t("queue.finished", { count: finished.length })}
          </Button>
          {showFinished && (
            <ul
              id={`${ids}-finished`}
              className="divide-y border-t"
              aria-label={t("queue.finishedLabel")}
            >
              {finished
                .slice()
                .reverse()
                .map((item) => (
                  <QueueItem key={item.id} item={item} progress={undefined} />
                ))}
            </ul>
          )}
        </div>
      )}
      {groupRiskDialog}
    </Card>
  )
}
