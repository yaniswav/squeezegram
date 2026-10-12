import { CircleCheck, Download, Settings2, X } from "lucide-react"
import { AnimatePresence, motion } from "motion/react"
import { useCallback, useEffect, useState } from "react"
import { useTranslation } from "react-i18next"
import { FfmpegFinder } from "@/components/ffmpeg-finder"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Progress } from "@/components/ui/progress"
import { errorMessage, errorOf } from "@/i18n/errors"
import { formatBytes, formatPercent } from "@/lib/format"
import {
  onSidecarProgress,
  onSidecarStatus,
  sidecarsInstall,
  sidecarsStatus,
  type FfmpegRecommendation,
  type SidecarProgress,
  type SidecarStatus,
} from "@/lib/ipc"
import { useSettings } from "@/state/settings"

/** While another window or an earlier page load is installing, the status is polled. */
const POLL_MS = 2000

/**
 * First launch: shown while ffmpeg/ffprobe can't be resolved. Downloads the pinned
 * build through `sidecars_install`; the core resumes an interrupted download from its `.part`.
 */
export function SidecarBanner({
  onOpenSettings,
}: {
  onOpenSettings: () => void
}) {
  const { t, i18n } = useTranslation()
  const language = i18n.language
  const { settings } = useSettings()
  const customDir = settings?.ffmpeg.custom_dir ?? null
  const [status, setStatus] = useState<SidecarStatus | null>(null)
  const [installing, setInstalling] = useState(false)
  const [progress, setProgress] = useState<SidecarProgress | null>(null)
  const [error, setError] = useState<ReturnType<typeof errorOf> | null>(null)
  const [justInstalled, setJustInstalled] = useState(false)

  const [recommendation, setRecommendation] =
    useState<FfmpegRecommendation | null>(null)

  const onFailed = useCallback((cause: unknown) => setError(errorOf(cause)), [])

  const refresh = useCallback(
    () => sidecarsStatus().then(setStatus, onFailed),
    [onFailed],
  )

  // Re-resolve whenever the custom ffmpeg folder changes in the settings.
  useEffect(() => {
    sidecarsStatus().then(setStatus, onFailed)
  }, [onFailed, customDir])

  useEffect(() => onSidecarProgress(setProgress), [])
  useEffect(() => onSidecarStatus(setStatus), [])

  const busy = installing || status?.installing === true
  useEffect(() => {
    if (!status?.installing || installing) return
    const timer = window.setInterval(() => void refresh(), POLL_MS)

    return () => window.clearInterval(timer)
  }, [status?.installing, installing, refresh])

  async function install() {
    setInstalling(true)
    setError(null)
    setProgress(null)

    try {
      const next = await sidecarsInstall()
      setStatus(next)
      setJustInstalled(next.installed)
    } catch (caught) {
      setError(errorOf(caught))
    } finally {
      setInstalling(false)
    }
  }

  const missing = status !== null && !status.installed
  const customInvalid = missing && customDir !== null && customDir.trim() !== ""

  const visible =
    missing || justInstalled || (status === null && error !== null)

  const percent = progress?.percent ?? 0
  const verifying = busy && percent >= 100

  return (
    <AnimatePresence initial={false}>
      {visible && (
        <motion.section
          aria-labelledby="sidecar-title"
          initial={{ opacity: 0, y: -6 }}
          animate={{ opacity: 1, y: 0 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.2 }}
          className="rounded-xl border bg-card p-4 text-card-foreground"
        >
          {justInstalled && !missing ? (
            <div role="status" className="flex items-center gap-3 text-sm">
              <CircleCheck className="size-4 text-success" aria-hidden />
              <p id="sidecar-title" className="flex-1">
                {t("sidecars.installed")}
              </p>
              <Button
                variant="ghost"
                size="icon-xs"
                aria-label={t("common.dismiss")}
                onClick={() => setJustInstalled(false)}
              >
                <X />
              </Button>
            </div>
          ) : (
            <div className="space-y-3">
              <div className="flex items-start gap-3">
                <Download
                  className="mt-0.5 size-5 shrink-0 text-muted-foreground"
                  aria-hidden
                />
                <div className="min-w-0 flex-1 space-y-1">
                  <h2 id="sidecar-title" className="text-sm font-medium">
                    {t("sidecars.missingTitle")}
                  </h2>
                  <p className="text-sm text-muted-foreground">
                    {customInvalid
                      ? t("sidecars.customInvalid")
                      : status &&
                        t("sidecars.missingBody", {
                          size: formatBytes(status.pinned_size, language),
                          tag: status.pinned_tag,
                        })}
                  </p>
                </div>
              </div>

              {busy && (
                <div className="space-y-1.5" aria-live="polite">
                  <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
                    <span>
                      {verifying
                        ? t("sidecars.extracting")
                        : t("sidecars.downloading")}
                    </span>
                    <span className="tabular-nums">
                      {progress?.bytesPerSec && !verifying
                        ? `${t("common.rate", {
                            rate: formatBytes(progress.bytesPerSec, language),
                          })} · `
                        : ""}
                      {formatPercent(percent, language)}
                    </span>
                  </div>
                  <Progress
                    value={percent}
                    aria-label={t("sidecars.progressLabel")}
                  />
                </div>
              )}

              {error && (
                <p role="alert" className="text-sm text-destructive">
                  {errorMessage(t, error.code, error.params)}
                </p>
              )}

              <div className="flex flex-wrap gap-2">
                {!customInvalid && status && (
                  <Button
                    variant={
                      recommendation?.kind === "candidate"
                        ? "outline"
                        : "default"
                    }
                    disabled={busy}
                    onClick={() => void install()}
                  >
                    <Download data-icon="inline-start" />
                    {error ? t("sidecars.resume") : t("sidecars.install")}
                  </Button>
                )}
                {!customInvalid && recommendation?.kind === "download" && (
                  <Badge className="self-center">
                    {t("sidecars.finder.recommended")}
                  </Badge>
                )}
                <Button variant="outline" onClick={onOpenSettings}>
                  <Settings2 data-icon="inline-start" />
                  {t("sidecars.useOwn")}
                </Button>
              </div>

              {!busy && <FfmpegFinder onRecommendation={setRecommendation} />}
            </div>
          )}
        </motion.section>
      )}
    </AnimatePresence>
  )
}
