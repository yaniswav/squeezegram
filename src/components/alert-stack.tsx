import { CircleAlert, Info, X } from "lucide-react"
import { AnimatePresence, motion } from "motion/react"
import { useTranslation } from "react-i18next"
import { Button } from "@/components/ui/button"
import { errorMessage, isInfoCode } from "@/i18n/errors"
import { fileName } from "@/lib/format"
import { isJsonString } from "@/lib/json"
import { cn } from "@/lib/utils"
import { useAlerts } from "@/state/alerts"
import { useQueue } from "@/state/queue"

/** Errors and warnings from commands and `queue://alert`, translated from their codes. */
export function AlertStack() {
  const { t } = useTranslation()
  const { alerts, dismiss } = useAlerts()
  const { items } = useQueue()

  return (
    <section aria-label={t("alerts.label")} className="empty:hidden">
      <AnimatePresence initial={false}>
        {alerts.map((alert) => {
          const item =
            alert.itemId === undefined
              ? undefined
              : items.find((candidate) => candidate.id === alert.itemId)

          const path = isJsonString(alert.params.path)
            ? alert.params.path
            : null

          // Information (the work went on, e.g. the CPU took over): not styled as an error.
          const info = isInfoCode(alert.code)
          const Icon = info ? Info : CircleAlert

          return (
            <motion.div
              key={alert.id}
              role={info ? "status" : "alert"}
              layout
              initial={{ opacity: 0, y: -6 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, height: 0, marginBottom: 0 }}
              transition={{ duration: 0.18 }}
              className={cn(
                "mb-2 flex items-start gap-3 rounded-lg border p-3 text-sm",
                info
                  ? "border-border bg-muted/40"
                  : "border-destructive/30 bg-destructive/5 dark:bg-destructive/10",
              )}
            >
              <Icon
                className={cn(
                  "mt-0.5 size-4 shrink-0",
                  info ? "text-muted-foreground" : "text-destructive",
                )}
                aria-hidden
              />
              <div className="min-w-0 flex-1 space-y-0.5">
                {item && (
                  <p className="truncate font-medium" dir="auto">
                    {t("alerts.item", { name: fileName(item.source_path) })}
                  </p>
                )}
                <p>{errorMessage(t, alert.code, alert.params)}</p>
                {path && (
                  <p className="truncate font-mono text-xs text-muted-foreground">
                    <bdi title={path}>{path}</bdi>
                  </p>
                )}
              </div>
              <Button
                variant="ghost"
                size="icon-xs"
                aria-label={t("common.dismiss")}
                onClick={() => dismiss(alert.id)}
              >
                <X />
              </Button>
            </motion.div>
          )
        })}
      </AnimatePresence>
    </section>
  )
}
