import {
  Check,
  CircleAlert,
  Pencil,
  RefreshCw,
  Star,
  TriangleAlert,
} from "lucide-react"
import { useState } from "react"
import { useTranslation } from "react-i18next"
import { DestinationPicker } from "@/components/destination-picker"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import type { CheckState } from "@/hooks/use-destination-check"
import { errorMessage } from "@/i18n/errors"
import { type Destination } from "@/lib/ipc"
import { destinationName, useDestinations } from "@/state/destinations"

interface DestinationFieldProps {
  id: string
  value: string
  onChange: (value: string) => void
}

/**
 * Destination field: the picker (Saved Messages, favorites, recently used, folders, all chats,
 * archive, other; forum topics on demand; Telegram search), then favorite, local name and
 * refresh. Favorite and local name go through `destination_update`.
 */
export function DestinationField({
  id,
  value,
  onChange,
}: DestinationFieldProps) {
  const { t } = useTranslation()
  const { status, list, byId, refresh, update } = useDestinations()
  const [refreshing, setRefreshing] = useState(false)
  const [renaming, setRenaming] = useState(false)
  const [draft, setDraft] = useState("")

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
    ? destinationName(selected) || t("destinations.unnamed")
    : ""

  async function doRefresh() {
    setRefreshing(true)

    try {
      await refresh()
    } finally {
      setRefreshing(false)
    }
  }

  async function saveName(destination: Destination) {
    // A blank name removes the alias, which brings back the Telegram name.
    const alias = draft.trim()

    if (await update(destination.id, { alias: alias === "" ? null : alias }))
      setRenaming(false)
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
                aria-label={t("destinations.rename", {
                  name: selectedName,
                })}
                onClick={() => {
                  setDraft(selected.alias ?? "")
                  setRenaming(true)
                }}
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

      {selected && renaming && (
        <form
          className="space-y-2 rounded-lg border p-3"
          onSubmit={(event) => {
            event.preventDefault()
            void saveName(selected)
          }}
        >
          <Label htmlFor={`${id}-alias`}>{t("destinations.renameLabel")}</Label>
          <Input
            id={`${id}-alias`}
            dir="auto"
            autoFocus
            value={draft}
            placeholder={selected.title}
            aria-describedby={`${id}-alias-hint`}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") setRenaming(false)
            }}
          />
          <p id={`${id}-alias-hint`} className="text-xs text-muted-foreground">
            {t("destinations.renameHint")}
          </p>
          <div className="flex justify-end gap-2">
            <Button
              type="button"
              size="sm"
              variant="ghost"
              onClick={() => setRenaming(false)}
            >
              {t("common.cancel")}
            </Button>
            <Button type="submit" size="sm">
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
