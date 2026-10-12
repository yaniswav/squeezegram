import { FolderOpen, Plus, X } from "lucide-react"
import { AnimatePresence, motion } from "motion/react"
import { useId, useMemo, useState } from "react"
import { useTranslation } from "react-i18next"
import {
  DestinationCheckNotice,
  DestinationField,
} from "@/components/destination-field"
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
import { mostRecent } from "@/lib/destination-groups"
import { fileName } from "@/lib/format"
import {
  isDesktop,
  pickFiles,
  pickFolder,
  queueAdd,
  type SendMode,
} from "@/lib/ipc"
import { useDestinationCheck } from "@/hooks/use-destination-check"
import { useAlerts } from "@/state/alerts"
import { useDestinations } from "@/state/destinations"
import { useQueue } from "@/state/queue"

const MAX_LISTED = 5

const MODES: readonly SendMode[] = ["video", "document", "photo"]

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
  const ids = useId()
  /** The user's choice; "" until they pick one. */
  const [destinationId, setDestinationId] = useState("")
  const [mode, setMode] = useState<SendMode | "default">("default")
  const [caption, setCaption] = useState("")
  const [busy, setBusy] = useState(false)
  const [added, setAdded] = useState<number | null>(null)

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

  async function submit() {
    if (!chosen) return
    setBusy(true)

    try {
      const created = await queueAdd(
        paths,
        [Number(chosen)],
        mode === "default" ? undefined : mode,
        caption.trim() === "" ? undefined : caption,
      )

      onPathsChange([])
      setCaption("")
      setAdded(created.length)
      // `queue_add` recorded the use: the recently used list changes.
      void destinations.reload()
      await refresh()
    } catch (error) {
      pushError(error)
    } finally {
      setBusy(false)
    }
  }

  const destination =
    chosen === "" ? undefined : destinations.byId.get(Number(chosen))

  // Checked once there are files to add: a destination shown before that (the suggested one)
  // asks Telegram nothing.
  const check = useDestinationCheck(
    paths.length > 0 && destination ? destination.id : null,
    destination ? (mode === "default" ? destination.default_mode : mode) : null,
  )

  // A refusal from Telegram's rights blocks the add; a failed check only warns.
  const blocked = check.phase === "done" && !check.check.allowed

  const canSubmit =
    paths.length > 0 &&
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
            ? t("add.added", { count: added })
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
              <DestinationCheckNotice state={check} />

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
                    onChange={(event) => setCaption(event.target.value)}
                  />
                  <p
                    id={`${ids}-caption-hint`}
                    className="text-xs text-muted-foreground"
                  >
                    {t("add.caption.hint")}
                  </p>
                </div>
              </div>

              <div className="flex justify-end">
                <Button disabled={!canSubmit} onClick={() => void submit()}>
                  <Plus data-icon="inline-start" />
                  {t("add.submit", { count: paths.length })}
                </Button>
              </div>
            </motion.div>
          )}
        </AnimatePresence>
      </CardContent>
    </Card>
  )
}
