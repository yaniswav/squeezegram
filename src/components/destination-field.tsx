import {
  Check,
  CircleAlert,
  Pencil,
  RefreshCw,
  Star,
  TriangleAlert,
} from "lucide-react"
import { useId, useState } from "react"
import { useTranslation } from "react-i18next"
import { DestinationPicker } from "@/components/destination-picker"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import type { CheckState } from "@/hooks/use-destination-check"
import { errorMessage } from "@/i18n/errors"
import { formatDuration } from "@/lib/format"
import { type Destination, type DestinationPatch } from "@/lib/ipc"
import {
  destinationName,
  telegramName,
  useDestinations,
} from "@/state/destinations"

/** `store::MAX_MIN_INTERVAL_S` (one week), in the minutes the form uses. */
const MAX_INTERVAL_MINUTES = 7 * 24 * 60

const WHOLE = /^\d+$/

/** Minutes typed for the interval, or null when it is not a whole number in range. */
function intervalSeconds(minutes: string): number | null {
  const text = minutes.trim()

  if (text === "") return 0

  if (!WHOLE.test(text) || Number(text) > MAX_INTERVAL_MINUTES) return null

  return Number(text) * 60
}

interface DestinationFieldProps {
  id: string
  value: string
  onChange: (value: string) => void
}

/**
 * Destination field: the picker (Saved Messages, favorites, recently used, folders, all chats,
 * archive, other; forum topics on demand; Telegram search), then favorite, local settings
 * (name, minimum interval between two sends) and refresh. Favorite and local settings go
 * through `destination_update`.
 */
export function DestinationField({
  id,
  value,
  onChange,
}: DestinationFieldProps) {
  const { t, i18n } = useTranslation()
  const { status, list, byId, refresh, update } = useDestinations()
  const [refreshing, setRefreshing] = useState(false)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState("")
  const [minutes, setMinutes] = useState("")
  const ids = useId()

  if (status === "unavailable") {
    return (
      <div
        role="note"
        className="rounded-lg border border-dashed p-3 text-sm"
        aria-labelledby={`${id}-title`}
      >
        <p id={`${id}-title`} className="font-medium">
          {t("add.destination.unavailableTitle")}
        </p>
        <p className="mt-1 text-muted-foreground">
          {t("add.destination.unavailableBody")}
        </p>
      </div>
    )
  }

  const selected = value === "" ? undefined : byId.get(Number(value))
  const selectedName = selected
    ? destinationName(selected, t) || t("destinations.unnamed")
    : ""

  async function doRefresh() {
    setRefreshing(true)

    try {
      await refresh()
    } finally {
      setRefreshing(false)
    }
  }

  const interval = intervalSeconds(minutes)

  function edit(destination: Destination) {
    setDraft(destination.alias ?? "")
    setMinutes(
      destination.min_interval_s > 0
        ? Math.ceil(destination.min_interval_s / 60).toString()
        : "",
    )
    setEditing(true)
  }

  async function save(destination: Destination) {
    if (interval === null) return
    // A blank name removes the alias, which brings back the Telegram name.
    const alias = draft.trim()
    const patch: DestinationPatch = { alias: alias === "" ? null : alias }
    const shown = Math.ceil(destination.min_interval_s / 60) * 60

    // Left as shown: a value saved in seconds is not rounded to minutes behind the user's back.
    if (interval !== shown) patch.min_interval_s = interval

    if (await update(destination.id, patch)) setEditing(false)
  }

  return (
    <div className="space-y-2">
      <Label id={`${id}-label`} htmlFor={id}>
        {t("add.destination.label")}
      </Label>
      {status === "loading" && (
        <p className="text-sm text-muted-foreground">{t("common.loading")}</p>
      )}
      {status === "error" && (
        <p className="text-sm text-destructive">
          {t("add.destination.loadError")}
        </p>
      )}
      {status === "ready" && (
        <div className="flex items-center gap-1.5">
          {list.length === 0 ? (
            <p className="min-w-0 flex-1 text-sm text-muted-foreground">
              {t("add.destination.empty")}
            </p>
          ) : (
            <DestinationPicker
              id={id}
              value={value}
              onChange={onChange}
              labelledBy={`${id}-label`}
            />
          )}
          {selected && (
            <>
              <Button
                variant="ghost"
                size="icon-sm"
                aria-pressed={selected.favorite}
                aria-label={t(
                  selected.favorite
                    ? "destinations.unfavorite"
                    : "destinations.favorite",
                  { name: selectedName },
                )}
                onClick={() =>
                  void update(selected.id, { favorite: !selected.favorite })
                }
              >
                <Star className={selected.favorite ? "fill-current" : ""} />
              </Button>
              <Button
                variant="ghost"
                size="icon-sm"
                aria-label={t("destinations.edit", {
                  name: selectedName,
                })}
                title={t("destinations.edit", { name: selectedName })}
                onClick={() => edit(selected)}
              >
                <Pencil />
              </Button>
            </>
          )}
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={t("destinations.refresh")}
            title={t("destinations.refresh")}
            disabled={refreshing}
            onClick={() => void doRefresh()}
          >
            <RefreshCw className={refreshing ? "animate-spin" : ""} />
          </Button>
        </div>
      )}

      {selected && !editing && selected.min_interval_s > 0 && (
        <p className="text-xs text-muted-foreground">
          {t("destinations.interval.summary", {
            duration: formatDuration(selected.min_interval_s, i18n.language),
          })}
        </p>
      )}

      {selected && editing && (
        <form
          noValidate
          className="space-y-3 rounded-lg border p-3"
          onSubmit={(event) => {
            event.preventDefault()
            void save(selected)
          }}
          onKeyDown={(event) => {
            if (event.key === "Escape") setEditing(false)
          }}
        >
          <div className="space-y-2">
            <Label htmlFor={`${ids}-alias`}>
              {t("destinations.renameLabel")}
            </Label>
            <Input
              id={`${ids}-alias`}
              dir="auto"
              autoFocus
              value={draft}
              placeholder={telegramName(selected, t)}
              aria-describedby={`${ids}-alias-hint`}
              onChange={(event) => setDraft(event.target.value)}
            />
            <p
              id={`${ids}-alias-hint`}
              className="text-xs text-muted-foreground"
            >
              {t("destinations.renameHint")}
            </p>
          </div>
          <div className="space-y-2">
            <Label htmlFor={`${ids}-interval`}>
              {t("destinations.interval.label")}
            </Label>
            <Input
              id={`${ids}-interval`}
              type="number"
              min={0}
              max={MAX_INTERVAL_MINUTES}
              step={1}
              inputMode="numeric"
              className="sm:w-40"
              value={minutes}
              placeholder="0"
              aria-invalid={interval === null ? true : undefined}
              aria-describedby={
                interval === null
                  ? `${ids}-interval-error ${ids}-interval-hint`
                  : `${ids}-interval-hint`
              }
              onChange={(event) => setMinutes(event.target.value)}
            />
            {interval === null && (
              <p
                id={`${ids}-interval-error`}
                className="text-xs text-destructive"
              >
                {t("destinations.interval.error")}
              </p>
            )}
            <p
              id={`${ids}-interval-hint`}
              className="text-xs text-muted-foreground"
            >
              {t("destinations.interval.hint")}
            </p>
          </div>
          <div className="flex justify-end gap-2">
            <Button
              type="button"
              size="sm"
              variant="ghost"
              onClick={() => setEditing(false)}
            >
              {t("common.cancel")}
            </Button>
            <Button type="submit" size="sm" disabled={interval === null}>
              {t("destinations.save")}
            </Button>
          </div>
        </form>
      )}
    </div>
  )
}

// --- pre-send check ---

export function DestinationCheckNotice({ state }: { state: CheckState }) {
  const { t } = useTranslation()

  if (state.phase === "idle") return null

  let tone = "text-muted-foreground"
  let Icon = RefreshCw
  let text = t("add.destination.check.checking")
  let detail: string | null = null

  if (state.phase === "failed") {
    tone = "text-warning"
    Icon = TriangleAlert
    text = t("add.destination.check.failed")
    detail = errorMessage(t, state.code, state.params)
  } else if (state.phase === "done") {
    const { check } = state
    detail = check.code ? errorMessage(t, check.code, check.params) : null

    if (!check.allowed) {
      tone = "text-destructive"
      Icon = CircleAlert
      text = t("add.destination.check.blocked")
    } else if (check.code) {
      tone = "text-warning"
      Icon = TriangleAlert
      text = t("add.destination.check.warning")
    } else {
      tone = "text-success"
      Icon = Check
      text = t("add.destination.check.ok")
    }
  }

  return (
    <div role="status" className={`flex items-start gap-2 text-sm ${tone}`}>
      <Icon className="mt-0.5 size-4 shrink-0" aria-hidden />
      <div className="min-w-0">
        <p>{text}</p>
        {detail && <p className="text-xs">{detail}</p>}
      </div>
    </div>
  )
}
