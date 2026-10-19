import type { TFunction } from "i18next"
import { Search, TriangleAlert } from "lucide-react"
import { useState } from "react"
import { useTranslation } from "react-i18next"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { errorMessage, errorOf } from "@/i18n/errors"
import {
  sidecarsDiscover,
  sidecarsUse,
  type FfmpegCandidate,
  type FfmpegDiscovery,
  type FfmpegRecommendation,
  type RecommendReason,
  type SidecarStatus,
} from "@/lib/ipc"
import { cn } from "@/lib/utils"
import { useSettings } from "@/state/settings"

function reasonsText(t: TFunction, reasons: RecommendReason[]): string {
  return reasons
    .flatMap((reason) =>
      reason.code.startsWith("license_")
        ? []
        : [t(`sidecars.finder.reason.${reason.code}`, { ...reason.params })],
    )
    .join(" ")
}

function Warning({ children }: { children: string }) {
  return (
    <p role="note" className="flex items-center gap-1 text-xs text-warning">
      <TriangleAlert className="size-3 shrink-0" aria-hidden />
      {children}
    </p>
  )
}

/**
 * Detection of an ffmpeg already installed. The core only proposes candidates (known install
 * folders, plus the PATH folders as listed-only entries), scored by the encoders that really
 * work with each and compared with the download; nothing is kept until the click on "Use this
 * one", which records the folder in `ffmpeg.custom_dir` through `sidecars_use`. The page
 * gives the candidate's id, never a path.
 */
export function FfmpegFinder({
  onUsed,
  onRecommendation,
}: {
  onUsed?: (status: SidecarStatus) => void
  /** The global advice once a search is done (null while searching): lets the parent
   *  highlight its own download button. */
  onRecommendation?: (recommendation: FfmpegRecommendation | null) => void
}) {
  const { t } = useTranslation()
  const { reload } = useSettings()
  const [discovery, setDiscovery] = useState<FfmpegDiscovery | null>(null)
  const [searching, setSearching] = useState(false)
  const [using, setUsing] = useState<number | null>(null)
  const [error, setError] = useState<ReturnType<typeof errorOf> | null>(null)

  async function search() {
    setSearching(true)
    setError(null)
    onRecommendation?.(null)

    try {
      const found = await sidecarsDiscover()
      setDiscovery(found)
      onRecommendation?.(found.recommendation)
    } catch (caught) {
      setError(errorOf(caught))
    } finally {
      setSearching(false)
    }
  }

  async function use(id: number) {
    setUsing(id)
    setError(null)

    try {
      const status = await sidecarsUse(id)
      await reload()
      onUsed?.(status)
    } catch (caught) {
      setError(errorOf(caught))
    } finally {
      setUsing(null)
    }
  }

  const candidates: FfmpegCandidate[] | null = discovery?.candidates ?? null
  const recommendation = discovery?.recommendation ?? null

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <Button
          type="button"
          variant="outline"
          disabled={searching || using !== null}
          onClick={() => void search()}
        >
          <Search data-icon="inline-start" />
          {t("sidecars.finder.search")}
        </Button>
        {searching && (
          <span
            role="status"
            className="text-xs text-muted-foreground"
            aria-live="polite"
          >
            {t("sidecars.finder.searching")}
          </span>
        )}
      </div>

      {error && (
        <p role="alert" className="text-sm text-destructive">
          {errorMessage(t, error.code, error.params)}
        </p>
      )}

      {candidates !== null && !searching && (
        <div className="space-y-2" aria-live="polite">
          <p className="text-xs text-muted-foreground">
            {candidates.length === 0
              ? t("sidecars.finder.none")
              : t("sidecars.finder.found", { count: candidates.length })}
          </p>
          {candidates.length > 0 && (
            <p className="text-xs text-muted-foreground">
              {t("sidecars.finder.intro")}
            </p>
          )}
          {recommendation && (
            <div className="space-y-0.5 rounded-lg border border-primary/40 bg-primary/5 p-3">
              <p className="text-sm font-medium">
                {t(`sidecars.finder.recommendation.${recommendation.kind}`)}
              </p>
              <p className="text-xs text-muted-foreground">
                {reasonsText(t, recommendation.reasons)}
              </p>
            </div>
          )}
          <ul className="space-y-2">
            {candidates.map((candidate) => (
              <li
                key={candidate.id}
                className={cn(
                  "flex flex-col gap-2 rounded-lg border p-3 sm:flex-row sm:items-center",
                  candidate.recommended && "border-primary/60 bg-primary/5",
                )}
              >
                <div className="min-w-0 flex-1 space-y-1">
                  <div className="flex flex-wrap items-center gap-1.5 text-sm">
                    <span className="font-medium">
                      {candidate.verified
                        ? t("sidecars.finder.version", {
                            version: candidate.version,
                          })
                        : "ffmpeg"}
                    </span>
                    <Badge
                      variant={
                        candidate.license === "lgpl" ? "secondary" : "outline"
                      }
                    >
                      {t(`sidecars.finder.license.${candidate.license}`)}
                    </Badge>
                    <Badge variant="outline">
                      {t(`sidecars.finder.origin.${candidate.origin}`)}
                    </Badge>
                    {candidate.recommended && (
                      <Badge>{t("sidecars.finder.recommended")}</Badge>
                    )}
                  </div>
                  <p className="text-xs break-all text-muted-foreground">
                    {candidate.dir}
                  </p>
                  {candidate.score > 0 && (
                    <p
                      className={cn(
                        "text-xs",
                        !candidate.recommended && "text-muted-foreground",
                      )}
                    >
                      {reasonsText(t, candidate.reasons)}
                    </p>
                  )}
                  {candidate.verified ? (
                    <p className="text-xs text-muted-foreground">
                      {candidate.working_encoders.length > 0
                        ? t("sidecars.finder.workingEncoders", {
                            list: candidate.working_encoders.join(", "),
                          })
                        : candidate.encoders.length > 0
                          ? t("sidecars.finder.encoders", {
                              list: candidate.encoders.join(", "),
                            })
                          : t("sidecars.finder.noEncoders")}
                    </p>
                  ) : (
                    <Warning>{t("sidecars.finder.unverified")}</Warning>
                  )}
                  {candidate.verified &&
                    candidate.working_encoders.length === 0 && (
                      <Warning>{t("sidecars.finder.unusable")}</Warning>
                    )}
                  {candidate.reasons.some(
                    (reason) => reason.code === "weak_fallback_only",
                  ) && <Warning>{t("sidecars.finder.weakWarning")}</Warning>}
                  {candidate.license === "gpl" && (
                    <p className="text-xs text-muted-foreground">
                      {t("sidecars.finder.gplNote")}
                    </p>
                  )}
                  {candidate.origin === "path" && (
                    <p className="text-xs text-muted-foreground">
                      {t("sidecars.finder.pathNote")}
                    </p>
                  )}
                </div>
                <Button
                  type="button"
                  size="sm"
                  variant={candidate.recommended ? "default" : "outline"}
                  disabled={using !== null}
                  onClick={() => void use(candidate.id)}
                >
                  {using === candidate.id
                    ? t("sidecars.finder.using")
                    : t("sidecars.finder.use")}
                </Button>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  )
}
