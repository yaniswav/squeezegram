import { CircleArrowUp, Download, ExternalLink, X } from "lucide-react"
import { AnimatePresence, motion } from "motion/react"
import { useState } from "react"
import { useTranslation } from "react-i18next"
import { Button } from "@/components/ui/button"
import { Progress } from "@/components/ui/progress"
import { formatPercent } from "@/lib/format"
import { useQueue } from "@/state/queue"
import { useUpdate } from "@/state/update"

/**
 * A newer version exists. The installer build offers "Download and install" (the core checks
 * the signature, closes the app and runs the installer, which restarts it); the portable
 * build only links to the release page. Shown on every page until dismissed for that version.
 */
export function UpdateBanner() {
  const { t, i18n } = useTranslation()
  const { notice, progress, installing, install, openPage } = useUpdate()
  const { running } = useQueue()
  const [dismissed, setDismissed] = useState<string | null>(null)

  const found = notice?.available ? notice : null

  const shown =
    found !== null && (installing || dismissed !== found.version) ? found : null

  const portable = shown?.kind === "portable"
  // Closing to install ends an upload in progress; the user pauses the queue first.
  const queueBusy = running === true
  const handingOver = progress?.phase === "installing"
  const percent = progress?.percent ?? null

  return (
    <AnimatePresence initial={false}>
      {shown && (
        <motion.section
          aria-labelledby="update-title"
          initial={{ opacity: 0, y: -6 }}
          animate={{ opacity: 1, y: 0 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.2 }}
          className="space-y-2 rounded-xl border bg-card px-4 py-3 text-card-foreground"
        >
          <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
            <CircleArrowUp
              className="size-4 shrink-0 text-muted-foreground"
              aria-hidden
            />
            <p id="update-title" className="min-w-0 flex-1 text-sm">
              {t("update.banner.title", { version: shown.version })}
            </p>
            {portable ? (
              <Button
                variant="outline"
                size="sm"
                onClick={() => void openPage()}
              >
                <ExternalLink data-icon="inline-start" />
                {t("update.banner.openPage")}
              </Button>
            ) : (
              <Button
                size="sm"
                disabled={installing || queueBusy}
                onClick={() => void install()}
              >
                <Download data-icon="inline-start" />
                {t("update.banner.install")}
              </Button>
            )}
            {!installing && (
              <Button
                variant="ghost"
                size="icon-xs"
                aria-label={t("common.dismiss")}
                onClick={() => setDismissed(shown.version)}
              >
                <X />
              </Button>
            )}
          </div>

          {portable && (
            <p className="text-xs text-muted-foreground">
              {t("update.banner.portableHint")}
            </p>
          )}

          {!portable && queueBusy && !installing && (
            <p className="text-xs text-muted-foreground">
              {t("update.banner.queueRunning")}
            </p>
          )}

          {installing && (
            <div className="space-y-1.5" aria-live="polite">
              <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
                <span>
                  {handingOver
                    ? t("update.banner.installing")
                    : t("update.banner.downloading")}
                </span>
                {percent !== null && !handingOver && (
                  <span className="tabular-nums">
                    {formatPercent(percent, i18n.language)}
                  </span>
                )}
              </div>
              <Progress
                value={handingOver ? 100 : (percent ?? 0)}
                aria-label={t("update.banner.progressLabel")}
              />
            </div>
          )}
        </motion.section>
      )}
    </AnimatePresence>
  )
}
