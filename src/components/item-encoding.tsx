import { useEffect, useId, useState } from "react"
import type { TFunction } from "i18next"
import { useTranslation } from "react-i18next"
import { CodecNote } from "@/components/codec-note"
import { PurchaseButton } from "@/components/purchase-button"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { useCodecSupport } from "@/hooks/use-codec-support"
import { useEntitlementStatus } from "@/hooks/use-entitlement-status"
import {
  MAX_RESOLUTIONS,
  TARGET_MB_MAX,
  TARGET_MB_MIN,
  VIDEO_CODECS,
  isTargetMb,
} from "@/lib/advanced-encoding"
import { grants } from "@/lib/entitlements"
import { formatBytes } from "@/lib/format"
import {
  itemEncodingEstimate,
  type EncodeEstimate,
  type EncodingOverride,
  type Item,
  type MaxResolution,
  type VideoCodec,
} from "@/lib/ipc"
import { useSettings } from "@/state/settings"

const MIB = 1024 * 1024

/** How long the choices rest before the estimate is asked again. */
const ESTIMATE_DELAY_MS = 300

interface Draft {
  codec: VideoCodec
  resolution: MaxResolution
  fill: boolean
  mb: string
}

/** The choices of the file, else what the settings give (the starting point of the panel). */
function startingDraft(item: Item, fallback: EncodingOverride): Draft {
  const source = item.encoding_override ?? fallback

  return {
    codec: source.video_codec,
    resolution: source.max_resolution,
    fill: source.target_mb === null,
    mb: source.target_mb?.toString() ?? "",
  }
}

/** The draft as the core takes it; null while the size field is not a valid number. */
function toOverride(draft: Draft): EncodingOverride | null {
  const mb = draft.fill ? null : Number(draft.mb)

  if (mb !== null && !(/^\d+$/.test(draft.mb.trim()) && isTargetMb(mb)))
    return null

  return {
    video_codec: draft.codec,
    max_resolution: draft.resolution,
    target_mb: mb,
  }
}

/** One line telling what a file is set to encode with. */
function summary(
  t: TFunction,
  language: string,
  choices: EncodingOverride,
): string {
  return t("queue.item.encoding.summary", {
    codec: t(`settings.advanced.codec.${choices.video_codec}`),
    resolution: t(`settings.advanced.resolution.${choices.max_resolution}`),
    size:
      choices.target_mb === null
        ? t("queue.item.encoding.fillLimit")
        : formatBytes(choices.target_mb * MIB, language),
  })
}

/**
 * Premium: this file's codec, maximum resolution and output size (F14), with the size the
 * output would have. Offered while the item has not started; the core refuses it afterwards
 * and without the license, whatever this panel shows.
 */
export function ItemEncoding({
  item,
  busy,
  onApply,
}: {
  item: Item
  busy: boolean
  /** Saves the choices (`null`: back to the settings). True when it worked. */
  onApply: (choices: EncodingOverride | null) => Promise<boolean>
}) {
  const { t, i18n } = useTranslation()
  const language = i18n.language
  const [open, setOpen] = useState(false)
  const saved = item.encoding_override

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <Button
          size="sm"
          variant="outline"
          aria-expanded={open}
          onClick={() => setOpen((current) => !current)}
        >
          {t("queue.item.encoding.open")}
        </Button>
        {saved && (
          <p className="min-w-0 flex-1 text-xs text-muted-foreground">
            {summary(t, language, saved)}
          </p>
        )}
      </div>
      {open && (
        <EncodingPanel
          item={item}
          busy={busy}
          onApply={async (choices) => {
            const done = await onApply(choices)

            if (done) setOpen(false)

            return done
          }}
        />
      )}
    </div>
  )
}

function EncodingPanel({
  item,
  busy,
  onApply,
}: {
  item: Item
  busy: boolean
  onApply: (choices: EncodingOverride | null) => Promise<boolean>
}) {
  const { t, i18n } = useTranslation()
  const language = i18n.language
  const ids = useId()
  const { settings } = useSettings()
  const [entitlements] = useEntitlementStatus()
  const granted =
    entitlements !== "failed" && grants(entitlements, "advanced_encoding")
  const support = useCodecSupport(granted)

  const fallback: EncodingOverride = {
    video_codec: settings?.advanced_encoding.video_codec ?? "h264",
    max_resolution: settings?.advanced_encoding.max_resolution ?? "1080p",
    target_mb: settings?.advanced_encoding.target_mb ?? null,
  }

  const [draft, setDraft] = useState<Draft>(() => startingDraft(item, fallback))

  const choices = toOverride(draft)

  // The size the output would have, asked once the choices have rested. The answer carries
  // the choices it was asked for: any other choices read as "loading".
  const requestKey =
    choices === null
      ? null
      : `${choices.video_codec}|${choices.max_resolution}|${choices.target_mb}`

  const [answer, setAnswer] = useState<{
    key: string
    value: EncodeEstimate | "failed"
  } | null>(null)

  const estimate = answer?.key === requestKey ? answer.value : "loading"

  useEffect(() => {
    if (!granted || choices === null || requestKey === null) return

    let stale = false

    const timer = setTimeout(() => {
      itemEncodingEstimate(item.id, choices).then(
        (value) => {
          if (!stale) setAnswer({ key: requestKey, value })
        },
        () => {
          if (!stale) setAnswer({ key: requestKey, value: "failed" })
        },
      )
    }, ESTIMATE_DELAY_MS)

    return () => {
      stale = true
      clearTimeout(timer)
    }
    // `choices` is rebuilt at every render; `requestKey` stands for its fields.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [granted, item.id, requestKey])

  function change<K extends keyof Draft>(key: K, value: Draft[K]) {
    setDraft((current) => ({ ...current, [key]: value }))
  }

  if (!granted && !item.encoding_override) {
    return (
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-lg border p-3">
        <Badge variant="secondary">{t("settings.advanced.premium")}</Badge>
        <p className="min-w-0 flex-1 basis-64 text-sm text-muted-foreground">
          {t("queue.item.encoding.locked")}
        </p>
        <PurchaseButton />
      </div>
    )
  }

  return (
    <div className="space-y-3 rounded-lg border p-3">
      <p className="text-xs text-muted-foreground">
        {granted
          ? t("queue.item.encoding.intro")
          : t("queue.item.encoding.inactive")}
      </p>
      {granted && (
        <div className="grid gap-3 sm:grid-cols-3">
          <div className="space-y-1.5">
            <label id={`${ids}-codec`} className="block text-xs font-medium">
              {t("settings.advanced.codec.label")}
            </label>
            <Select
              value={draft.codec}
              onValueChange={(value) => {
                const codec = VIDEO_CODECS.find((known) => known === value)

                if (codec) change("codec", codec)
              }}
            >
              <SelectTrigger
                size="sm"
                className="w-full"
                aria-labelledby={`${ids}-codec`}
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent position="popper">
                {VIDEO_CODECS.map((codec) => (
                  <SelectItem
                    key={codec}
                    value={codec}
                    disabled={codec === "hevc" && support?.hevc === false}
                  >
                    {t(`settings.advanced.codec.${codec}`)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-1.5">
            <label
              id={`${ids}-resolution`}
              className="block text-xs font-medium"
            >
              {t("settings.advanced.resolution.label")}
            </label>
            <Select
              value={draft.resolution}
              onValueChange={(value) => {
                const resolution = MAX_RESOLUTIONS.find(
                  (known) => known === value,
                )

                if (resolution) change("resolution", resolution)
              }}
            >
              <SelectTrigger
                size="sm"
                className="w-full"
                aria-labelledby={`${ids}-resolution`}
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent position="popper">
                {MAX_RESOLUTIONS.map((resolution) => (
                  <SelectItem key={resolution} value={resolution}>
                    {t(`settings.advanced.resolution.${resolution}`)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-1.5">
            <label id={`${ids}-target`} className="block text-xs font-medium">
              {t("settings.advanced.target.label")}
            </label>
            <Select
              value={draft.fill ? "limit" : "custom"}
              onValueChange={(value) => change("fill", value === "limit")}
            >
              <SelectTrigger
                size="sm"
                className="w-full"
                aria-labelledby={`${ids}-target`}
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent position="popper">
                <SelectItem value="limit">
                  {t("settings.advanced.target.limit")}
                </SelectItem>
                <SelectItem value="custom">
                  {t("settings.advanced.target.custom")}
                </SelectItem>
              </SelectContent>
            </Select>
            {!draft.fill && (
              <>
                <label htmlFor={`${ids}-mb`} className="sr-only">
                  {t("settings.advanced.target.mb")}
                </label>
                <Input
                  id={`${ids}-mb`}
                  type="number"
                  min={TARGET_MB_MIN}
                  max={TARGET_MB_MAX}
                  step={1}
                  inputMode="numeric"
                  aria-invalid={choices === null ? true : undefined}
                  aria-describedby={
                    choices === null ? `${ids}-mb-error` : undefined
                  }
                  value={draft.mb}
                  onChange={(event) => change("mb", event.target.value)}
                />
                {choices === null && (
                  <p
                    id={`${ids}-mb-error`}
                    className="text-xs text-destructive"
                  >
                    {t("settings.advanced.target.error")}
                  </p>
                )}
              </>
            )}
          </div>
        </div>
      )}
      {granted && <CodecNote codec={draft.codec} support={support} />}
      {granted && choices !== null && (
        <p
          role="status"
          aria-live="polite"
          className="text-xs text-muted-foreground tabular-nums"
        >
          {estimateText(t, language, estimate)}
        </p>
      )}
      <div className="flex flex-wrap items-center justify-end gap-2">
        {item.encoding_override && (
          <Button
            size="sm"
            variant="ghost"
            disabled={busy}
            onClick={() => void onApply(null)}
          >
            {t("queue.item.encoding.reset")}
          </Button>
        )}
        {granted && (
          <Button
            size="sm"
            disabled={busy || choices === null}
            onClick={() => choices && void onApply(choices)}
          >
            {t("queue.item.encoding.apply")}
          </Button>
        )}
      </div>
    </div>
  )
}

function estimateText(
  t: TFunction,
  language: string,
  estimate: EncodeEstimate | "loading" | "failed" | null,
): string {
  if (estimate === null || estimate === "loading")
    return t("queue.item.encoding.estimate.loading")

  if (estimate === "failed") return t("queue.item.encoding.estimate.failed")

  const size =
    estimate.bytes === null ? "" : formatBytes(estimate.bytes, language)
  const limit = formatBytes(estimate.limit_bytes, language)
  const base = t(`queue.item.encoding.estimate.${estimate.outcome}`, {
    size,
    limit,
  })

  return estimate.codec_fallback
    ? `${base} ${t("queue.item.encoding.estimate.fallback")}`
    : base
}
