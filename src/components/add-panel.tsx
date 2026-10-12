import { FolderOpen, Plus, X } from "lucide-react"
import { AnimatePresence, motion } from "motion/react"
import { useId, useMemo, useRef, useState } from "react"
import { useTranslation } from "react-i18next"
import {
  DestinationCheckNotice,
  DestinationField,
} from "@/components/destination-field"
import { CaptionHelp } from "@/components/caption-help"
import { DestinationPicker } from "@/components/destination-picker"
import { PurchaseButton } from "@/components/purchase-button"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import { Label } from "@/components/ui/label"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Textarea } from "@/components/ui/textarea"
import {
  captionLength,
  captionLimit,
  counterState,
} from "@/lib/caption-template"
import { mostRecent } from "@/lib/destination-groups"
import { grants } from "@/lib/entitlements"
import { fileName } from "@/lib/format"
import {
  isDesktop,
  pickFiles,
  pickFolder,
  queueAdd,
  type Destination,
  type SendMode,
} from "@/lib/ipc"
import {
  useDestinationChecks,
  type CheckTarget,
} from "@/hooks/use-destination-check"
import { useCaptionPreview } from "@/hooks/use-caption-preview"
import { useGroupRiskGate } from "@/hooks/use-group-risk-gate"
import { useEntitlementStatus } from "@/hooks/use-entitlement-status"
import { useAlerts } from "@/state/alerts"
import { useAuth } from "@/state/auth"
import { useDestinations } from "@/state/destinations"
import { useQueue } from "@/state/queue"
import { useSettings } from "@/state/settings"

const MAX_LISTED = 5

const MODES: readonly SendMode[] = ["video", "document", "photo"]

/** A destination added to the batch after the first one; `id` is "" until one is picked. */
interface ExtraDestination {
  key: number
  id: string
}

interface AddPanelProps {
  /** Paths dropped on the window or picked, not queued yet (kept in memory only). */
  paths: string[]
  onPathsChange: (paths: string[]) => void
}

export function AddPanel({ paths, onPathsChange }: AddPanelProps) {
  const { t } = useTranslation()
  const { pushError } = useAlerts()
  const destinations = useDestinations()
  const { refresh } = useQueue()
  const { guard, dialog } = useGroupRiskGate()
  const ids = useId()
  /** The user's choice; "" until they pick one. */
  const [destinationId, setDestinationId] = useState("")
  const [mode, setMode] = useState<SendMode | "default">("default")
  // The user's text; null until they type, so the default template from Settings shows
  // (also when it loads after the panel, and again after each add).
  const [captionDraft, setCaptionDraft] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [added, setAdded] = useState<{
    files: number
    destinations: number
  } | null>(null)
  const [extraSlots, setExtraSlots] = useState<ExtraDestination[]>([])
  const nextSlot = useRef(0)
  const [entitlements] = useEntitlementStatus()
  const { settings } = useSettings()
  const { auth } = useAuth()
  const caption = captionDraft ?? settings?.send.caption_template ?? ""
  const previewed = useCaptionPreview(caption, paths[0], paths.length)
  // What the first file's caption will look like; an unrendered text counts as itself.
  const shown = previewed ?? caption
  const limit = captionLimit(auth?.me?.captionMaxChars)
  const length = captionLength(shown)
  const counter = counterState(length, limit)

  // Without the license the batch keeps one destination, as the core requires.
  const multiGranted =
    entitlements !== "failed" && grants(entitlements, "multi_destination")

  const extras = multiGranted ? extraSlots : []

  async function browse() {
    const picked = await pickFiles()

    if (picked.length > 0) {
      setAdded(null)
      onPathsChange([...new Set([...paths, ...picked])])
    }
  }

  // The core walks the folder when the paths are queued (queue/intake.rs): the panel only
  // holds its path, like a dropped folder.
  async function browseFolder() {
    const picked = await pickFolder()

    if (picked) {
      setAdded(null)
      onPathsChange([...new Set([...paths, picked])])
    }
  }

  // Until the user picks one, the destination files were last queued for (never imposed:
  // any choice replaces it).
  const suggested = useMemo(
    () => mostRecent(destinations.list),
    [destinations.list],
  )
  const chosen =
    destinationId !== "" ? destinationId : (suggested?.id.toString() ?? "")

  const destination =
    chosen === "" ? undefined : destinations.byId.get(Number(chosen))

  const extraDestinations = extras.map((slot) =>
    slot.id === "" ? undefined : destinations.byId.get(Number(slot.id)),
  )

  // Every destination once, the first one first.
  const batch = [destination, ...extraDestinations].filter(
    (entry, index, all): entry is Destination =>
      entry !== undefined &&
      all.findIndex((other) => other?.id === entry.id) === index,
  )

  // Checked once there are files to add: a destination shown before that (the suggested one)
  // asks Telegram nothing.
  const targetOf = (entry: Destination | undefined): CheckTarget | null =>
    paths.length > 0 && entry
      ? {
          destinationId: entry.id,
          mode: mode === "default" ? entry.default_mode : mode,
        }
      : null

  const checkOf = useDestinationChecks(
    batch.flatMap((entry) => targetOf(entry) ?? []),
  )

  // A refusal from Telegram's rights blocks the add; a failed check only warns.
  const blocked = batch.some((entry) => {
    const check = checkOf(targetOf(entry))

    return check.phase === "done" && !check.check.allowed
  })

  // Before the first send to a group or channel the user reads the risk once (F33); Saved
  // Messages and private chats go straight through.
  function submit() {
    if (batch.length === 0) return
    guard(batch, () => void send())
  }

  async function send() {
    setBusy(true)

    try {
      const created = await queueAdd(
        paths,
        batch.map((entry) => entry.id),
        mode === "default" ? undefined : mode,
        caption.trim() === "" ? undefined : caption,
      )

      onPathsChange([])
      setCaptionDraft(null)
      // One item per file and destination.
      setAdded({
        files: created.length / batch.length,
        destinations: batch.length,
      })
      // `queue_add` recorded the use: the recently used list changes.
      void destinations.reload()
      await refresh()
    } catch (error) {
      pushError(error)
    } finally {
      setBusy(false)
    }
  }

  function addSlot() {
    const key = nextSlot.current

    nextSlot.current += 1
    setExtraSlots((slots) => [...slots, { key, id: "" }])
  }

  function setSlot(key: number, id: string) {
    setExtraSlots((slots) =>
      slots.map((slot) => (slot.key === key ? { ...slot, id } : slot)),
    )
  }

  function removeSlot(key: number) {
    setExtraSlots((slots) => slots.filter((slot) => slot.key !== key))
  }

  // An added slot with no destination is never dropped silently: the add waits for a choice
  // or for the slot to be removed.
  const emptySlot = extraDestinations.some((entry) => entry === undefined)

  const canSubmit =
    paths.length > 0 &&
    !emptySlot &&
    destinations.status === "ready" &&
    destination !== undefined &&
    !blocked &&
    !busy

  return (
    <Card>
      <CardContent className="space-y-4">
        <h2 className="font-heading text-base font-medium">{t("add.title")}</h2>
        <div className="flex flex-wrap items-center gap-x-1.5 gap-y-1 text-sm text-muted-foreground">
          <FolderOpen className="size-4" aria-hidden />
          <span>{t("drop.hint")}</span>
          <Button
            variant="link"
            className="h-auto p-0"
            disabled={!isDesktop()}
            onClick={() => void browse()}
          >
            {t("drop.browse")}
          </Button>
          <Button
            variant="link"
            className="h-auto p-0"
            disabled={!isDesktop()}
            onClick={() => void browseFolder()}
          >
            {t("drop.browseFolder")}
          </Button>
        </div>

        <p role="status" className="text-sm text-success empty:hidden">
          {added !== null && paths.length === 0
            ? added.destinations > 1
              ? t("add.addedEach", {
                  count: added.files,
                  destinations: added.destinations,
                })
              : t("add.added", { count: added.files })
            : ""}
        </p>

        <AnimatePresence initial={false}>
          {paths.length > 0 && (
            <motion.div
              initial={{ opacity: 0, height: 0 }}
              animate={{ opacity: 1, height: "auto" }}
              exit={{ opacity: 0, height: 0 }}
              transition={{ duration: 0.2 }}
              className="space-y-4 overflow-hidden"
            >
              <section
                aria-labelledby={`${ids}-selected`}
                className="space-y-2"
              >
                <div className="flex items-center justify-between gap-2">
                  <h2 id={`${ids}-selected`} className="text-sm font-medium">
                    {t("add.selected", { count: paths.length })}
                  </h2>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => onPathsChange([])}
                  >
                    {t("add.clear")}
                  </Button>
                </div>
                <ul className="space-y-1">
                  {paths.slice(0, MAX_LISTED).map((path) => (
                    <li
                      key={path}
                      className="flex items-center gap-2 rounded-md bg-muted/50 ps-2.5 pe-1 text-sm"
                    >
                      <bdi className="min-w-0 flex-1 truncate" title={path}>
                        {fileName(path)}
                      </bdi>
                      <Button
                        variant="ghost"
                        size="icon-xs"
                        aria-label={t("add.remove", { name: fileName(path) })}
                        onClick={() =>
                          onPathsChange(paths.filter((other) => other !== path))
                        }
                      >
                        <X />
                      </Button>
                    </li>
                  ))}
                  {paths.length > MAX_LISTED && (
                    <li className="ps-2.5 text-xs text-muted-foreground">
                      {t("add.more", { count: paths.length - MAX_LISTED })}
                    </li>
                  )}
                </ul>
              </section>

              <DestinationField
                id={`${ids}-destination`}
                value={chosen}
                onChange={setDestinationId}
              />
              <DestinationCheckNotice state={checkOf(targetOf(destination))} />

              {extras.map((slot, index) => {
                const labelId = `${ids}-extra-${slot.key}-label`
                const number = index + 2

                return (
                  <div key={slot.key} className="space-y-2">
                    <span id={labelId} className="sr-only">
                      {t("add.destination.extraLabel", { number })}
                    </span>
                    <div className="flex items-center gap-1.5">
                      <DestinationPicker
                        id={`${ids}-extra-${slot.key}`}
                        value={slot.id}
                        onChange={(id) => setSlot(slot.key, id)}
                        labelledBy={labelId}
                      />
                      <Button
                        variant="ghost"
                        size="icon-sm"
                        aria-label={t("add.destination.removeExtra", {
                          number,
                        })}
                        onClick={() => removeSlot(slot.key)}
                      >
                        <X />
                      </Button>
                    </div>
                    {extraDestinations[index] === undefined ? (
                      <p className="text-sm text-muted-foreground">
                        {t("add.destination.extraEmpty")}
                      </p>
                    ) : (
                      <DestinationCheckNotice
                        state={checkOf(targetOf(extraDestinations[index]))}
                      />
                    )}
                  </div>
                )
              })}

              {destinations.status === "ready" &&
                destinations.list.length > 0 && (
                  <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={!multiGranted}
                      onClick={addSlot}
                    >
                      <Plus data-icon="inline-start" />
                      {t("add.destination.addAnother")}
                    </Button>
                    {!multiGranted && (
                      <>
                        <Badge variant="secondary">
                          {t("add.destination.premium")}
                        </Badge>
                        <p className="min-w-0 flex-1 basis-64 text-sm text-muted-foreground">
                          {t("add.destination.multiLocked")}
                        </p>
                        <PurchaseButton />
                      </>
                    )}
                  </div>
                )}

              <div className="grid gap-4 sm:grid-cols-[12rem_1fr]">
                <div className="space-y-2">
                  <Label htmlFor={`${ids}-mode`}>{t("add.mode.label")}</Label>
                  <Select
                    value={mode}
                    onValueChange={(value) => {
                      const chosen =
                        value === "default"
                          ? value
                          : MODES.find((mode) => mode === value)

                      if (chosen) setMode(chosen)
                    }}
                  >
                    <SelectTrigger id={`${ids}-mode`} className="w-full">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent position="popper">
                      <SelectItem value="default">
                        {t("add.mode.default")}
                      </SelectItem>
                      {MODES.map((value) => (
                        <SelectItem key={value} value={value}>
                          {t(`mode.${value}`)}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
                <div className="space-y-2">
                  <Label htmlFor={`${ids}-caption`}>
                    {t("add.caption.label")}
                  </Label>
                  <Textarea
                    id={`${ids}-caption`}
                    dir="auto"
                    rows={2}
                    value={caption}
                    aria-describedby={`${ids}-caption-hint`}
                    onChange={(event) => setCaptionDraft(event.target.value)}
                  />
                  <p
                    id={`${ids}-caption-hint`}
                    className="text-xs text-muted-foreground"
                  >
                    {t("add.caption.hint")}
                  </p>
                  <CaptionHelp />
                  {previewed !== null && previewed !== caption && (
                    <div className="space-y-1 rounded-lg bg-muted/50 px-2.5 py-2 text-xs">
                      <p className="font-medium">
                        {t("caption.preview.label")}
                      </p>
                      <p dir="auto" className="break-words whitespace-pre-wrap">
                        {previewed}
                      </p>
                      <p className="text-muted-foreground">
                        {t("caption.preview.estimated")}
                      </p>
                    </div>
                  )}
                  {caption !== "" && (
                    <p
                      className={
                        counter === "over"
                          ? "text-xs font-medium text-destructive"
                          : counter === "near"
                            ? "text-xs text-warning"
                            : "text-xs text-muted-foreground"
                      }
                    >
                      {t("caption.counter", { length, max: limit })}
                      {counter === "over" && ` ${t("caption.tooLong")}`}
                    </p>
                  )}
                </div>
              </div>

              <div className="flex justify-end">
                <Button disabled={!canSubmit} onClick={submit}>
                  <Plus data-icon="inline-start" />
                  {t("add.submit", { count: paths.length })}
                </Button>
              </div>
            </motion.div>
          )}
        </AnimatePresence>
      </CardContent>
      {dialog}
    </Card>
  )
}
