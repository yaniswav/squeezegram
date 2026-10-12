import {
  ArrowDown,
  ArrowRight,
  ArrowUp,
  ExternalLink,
  File,
  Film,
  Image,
  RotateCcw,
  X,
  type LucideIcon,
} from "lucide-react"
import { motion } from "motion/react"
import { useEffect, useId, useState, type ReactNode } from "react"
import { useTranslation } from "react-i18next"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { ItemEncoding } from "@/components/item-encoding"
import { PlanBadge, QueueStateBadge } from "@/components/queue-state-badge"
import { Button } from "@/components/ui/button"
import { Progress } from "@/components/ui/progress"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Textarea } from "@/components/ui/textarea"
import { useGroupRiskGate } from "@/hooks/use-group-risk-gate"
import { useNow } from "@/hooks/use-now"
import { errorMessage } from "@/i18n/errors"
import {
  fileName,
  formatBytes,
  formatDateTime,
  formatDuration,
  formatPercent,
  formatTime,
} from "@/lib/format"
import {
  itemDuplicate,
  itemMedia,
  itemEncodingSet,
  itemReconcile,
  itemPlan,
  itemResolveUnknown,
  itemUpdate,
  openTelegramLink,
  queueCancel,
  queuePromote,
  queueRetry,
  type DuplicateInfo,
  type Item,
  type Phase,
  type QueueProgress,
  type SendMode,
  type SentCandidate,
} from "@/lib/ipc"
import {
  canCancel,
  isNotStarted,
  isPrepared,
  isWorking,
} from "@/lib/queue-state"
import { useAlerts } from "@/state/alerts"
import { destinationName, useDestinations } from "@/state/destinations"
import { useQueue } from "@/state/queue"

const MODE_ICON: Record<SendMode, LucideIcon> = {
  video: Film,
  document: File,
  photo: Image,
}

const MODES: readonly SendMode[] = ["video", "document", "photo"]

type Confirm = "cancel" | "duplicate" | "resend" | null

type Progressed = QueueProgress & { receivedAt: number; cpuFallback: boolean }

interface QueueItemProps {
  item: Item
  progress: Progressed | undefined
  /** Absent for finished items, which can't be reordered. */
  onMove?: (direction: -1 | 1) => void
  isFirst?: boolean
  isLast?: boolean
}

export function QueueItem({
  item,
  progress,
  onMove,
  isFirst = false,
  isLast = false,
}: QueueItemProps) {
  const { t, i18n } = useTranslation()
  const language = i18n.language
  const { pushError } = useAlerts()
  const { refresh } = useQueue()
  const { byId } = useDestinations()
  const { guard, dialog: groupRiskDialog } = useGroupRiskGate()
  const [confirm, setConfirm] = useState<Confirm>(null)
  const [busy, setBusy] = useState(false)

  const name = fileName(item.source_path)
  const plan = itemPlan(item)
  const media = itemMedia(item)
  const Icon = MODE_ICON[item.send_mode]
  const destination = byId.get(item.destination_id)
  const editable = isNotStarted(item.state)

  /** Runs a command, refreshes the queue; errors become alerts. True on success. */
  async function run<T>(action: () => Promise<T>): Promise<boolean> {
    setBusy(true)

    try {
      await action()
      await refresh()

      return true
    } catch (error) {
      pushError(error, item.id)

      return false
    } finally {
      setBusy(false)
    }
  }

  function requestCancel() {
    // Nothing is lost before the item starts: no confirmation needed.
    if (editable) void run(() => queueCancel(item.id))
    else setConfirm("cancel")
  }

  const video = media?.video

  const [width, height] =
    video && Math.abs(video.rotation % 180) === 90
      ? [video.height, video.width]
      : [video?.width, video?.height]

  return (
    <motion.li
      layout="position"
      initial={{ opacity: 0, y: 6 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.2 }}
      className="space-y-3 px-4 py-3"
      aria-busy={isWorking(item.state)}
    >
      <div className="flex items-start gap-3">
        <span className="mt-0.5 flex size-9 shrink-0 items-center justify-center rounded-lg bg-muted text-muted-foreground">
          <Icon className="size-4" aria-hidden />
        </span>

        <div className="min-w-0 flex-1 space-y-1">
          <p className="truncate text-sm font-medium" title={item.source_path}>
            <bdi>{name}</bdi>
          </p>
          <p className="flex flex-wrap items-center gap-x-1.5 gap-y-0.5 text-xs text-muted-foreground">
            <span>{formatBytes(item.source_size, language)}</span>
            {item.output_size !== null &&
              item.output_size !== item.source_size && (
                <>
                  <ArrowRight className="size-3 rtl:rotate-180" aria-hidden />
                  <span>{formatBytes(item.output_size, language)}</span>
                </>
              )}
            {media?.duration_s != null && (
              <span>· {formatDuration(media.duration_s, language)}</span>
            )}
            {width !== undefined && height !== undefined && (
              <span>· {t("queue.item.resolution", { width, height })}</span>
            )}
            <span>
              ·{" "}
              {t("queue.item.to", {
                name: destination
                  ? destinationName(destination)
                  : t("common.destinationNumber", { id: item.destination_id }),
              })}
            </span>
          </p>
        </div>

        <div className="flex shrink-0 flex-col items-end gap-1">
          <QueueStateBadge state={item.state} />
          {item.state !== "cancelled" && (
            <PlanBadge kind={plan?.kind ?? null} />
          )}
        </div>

        <div
          role="group"
          aria-label={t("queue.item.actions", { name })}
          className="flex shrink-0 items-center gap-0.5"
        >
          {onMove && (
            <>
              <Button
                variant="ghost"
                size="icon-sm"
                aria-label={t("queue.item.moveUp", { name })}
                disabled={busy || isFirst}
                onClick={() => onMove(-1)}
              >
                <ArrowUp />
              </Button>
              <Button
                variant="ghost"
                size="icon-sm"
                aria-label={t("queue.item.moveDown", { name })}
                disabled={busy || isLast}
                onClick={() => onMove(1)}
              >
                <ArrowDown />
              </Button>
            </>
          )}
          {canCancel(item.state) && (
            <Button
              variant="ghost"
              size="icon-sm"
              aria-label={t("queue.item.cancelNamed", { name })}
              disabled={busy}
              onClick={requestCancel}
            >
              <X />
            </Button>
          )}
        </div>
      </div>

      <div className="space-y-3 ps-12">
        <StatusPanel
          item={item}
          progress={progress}
          busy={busy}
          // A watched file is promoted to a send: the group warning applies here too.
          onPromote={() =>
            guard(
              destination ? [destination] : [],
              () => void run(() => queuePromote([item.id])),
            )
          }
          onConfirm={setConfirm}
          onRetry={() => void run(() => queueRetry(item.id))}
          onMarkSent={(messageId) =>
            void run(() => itemResolveUnknown(item.id, "mark_sent", messageId))
          }
        />

        {editable && (
          <ItemEditor
            item={item}
            busy={busy}
            onSave={(patch) => run(() => itemUpdate(item.id, patch))}
          />
        )}
        {editable && item.send_mode !== "document" && (
          <ItemEncoding
            item={item}
            busy={busy}
            onApply={(choices) => run(() => itemEncodingSet(item.id, choices))}
          />
        )}
        {!editable && item.caption !== "" && (
          <p className="text-sm whitespace-pre-wrap" dir="auto">
            {item.caption}
          </p>
        )}
      </div>

      <ConfirmDialog
        open={confirm === "cancel"}
        onOpenChange={(open) => !open && setConfirm(null)}
        title={t("queue.item.cancelDialog.title")}
        description={t("queue.item.cancelDialog.body", { name })}
        confirmLabel={t("queue.item.cancelDialog.confirm")}
        cancelLabel={t("queue.item.cancelDialog.keep")}
        destructive
        onConfirm={() => void run(() => queueCancel(item.id))}
      />
      <ConfirmDialog
        open={confirm === "duplicate"}
        onOpenChange={(open) => !open && setConfirm(null)}
        title={t("queue.item.duplicate.dialogTitle")}
        description={t("queue.item.duplicate.dialogBody", { name })}
        confirmLabel={t("queue.item.duplicate.dialogConfirm")}
        cancelLabel={t("common.cancel")}
        onConfirm={() =>
          guard(
            destination ? [destination] : [],
            () => void run(() => queuePromote([item.id])),
          )
        }
      />
      <ConfirmDialog
        open={confirm === "resend"}
        onOpenChange={(open) => !open && setConfirm(null)}
        title={t("queue.item.unknown.dialogTitle")}
        description={t("queue.item.unknown.dialogBody", { name })}
        confirmLabel={t("queue.item.unknown.sendAgain")}
        cancelLabel={t("common.cancel")}
        onConfirm={() => void run(() => itemResolveUnknown(item.id, "retry"))}
      />
      {groupRiskDialog}
    </motion.li>
  )
}

interface StatusPanelProps {
  item: Item
  progress: Progressed | undefined
  busy: boolean
  onPromote: () => void
  onConfirm: (confirm: Confirm) => void
  onRetry: () => void
  onMarkSent: (messageId?: number) => void
}

function StatusPanel({
  item,
  progress,
  busy,
  onPromote,
  onConfirm,
  onRetry,
  onMarkSent,
}: StatusPanelProps) {
  const { t, i18n } = useTranslation()
  const { alerts } = useAlerts()
  const language = i18n.language

  switch (item.state) {
    case "pending_review":
      return item.hold_reason === "duplicate" ? (
        <Notice tone="attention" text={t("queue.item.duplicate.body")}>
          <EarlierSend item={item} />
          <Button
            size="sm"
            variant="outline"
            disabled={busy}
            onClick={() => onConfirm("duplicate")}
          >
            {t("queue.item.duplicate.confirm")}
          </Button>
        </Notice>
      ) : (
        <Notice tone="attention" text={t("queue.item.watch.body")}>
          <Button
            size="sm"
            variant="outline"
            disabled={busy}
            onClick={onPromote}
          >
            {t("queue.item.watch.approve")}
          </Button>
        </Notice>
      )

    case "waiting_server":
      return (
        <>
          <WaitingNotice item={item} progress={progress} />
          <Steps item={item} progress={progress} />
        </>
      )

    case "unknown":
      return (
        <UnknownNotice
          item={item}
          busy={busy}
          onMarkSent={onMarkSent}
          onResend={() => onConfirm("resend")}
        />
      )

    case "failed": {
      // The row keeps only the code; the alert of the failure carries its params (encoder,
      // exit code…) for the precise sentence, while it is still listed.
      const report = alerts
        .filter(
          (alert) => alert.itemId === item.id && alert.code === item.last_error,
        )
        .at(-1)

      return (
        <Notice
          tone="danger"
          text={
            item.last_error
              ? errorMessage(t, item.last_error, report?.params)
              : t("queue.state.failed")
          }
        >
          {item.output_path !== null && (
            <p className="w-full text-xs text-muted-foreground">
              {t("queue.item.failed.keptHint")}
            </p>
          )}
          <Button size="sm" variant="outline" disabled={busy} onClick={onRetry}>
            <RotateCcw data-icon="inline-start" />
            {t("queue.item.retry")}
          </Button>
        </Notice>
      )
    }

    case "encoded":
      // Held by a schedule: its destination's minimum interval, or a FLOOD_WAIT carried over
      // a restart. Other items run meanwhile.
      return (
        <>
          {item.next_run_at && <ScheduledNotice item={item} />}
          <Steps item={item} progress={progress} />
        </>
      )

    case "probing":
    case "encoding":
    case "remuxing":
    case "uploading":
      return <Steps item={item} progress={progress} />

    case "sent":
      return item.tg_sent_at ? (
        <p className="text-xs text-muted-foreground">
          {t("queue.item.sentAt", {
            time: formatTime(new Date(item.tg_sent_at), language),
          })}
        </p>
      ) : null

    case "ready":
    case "cancelled":
      return null
  }
}

/**
 * Where a held duplicate already went: the date and destination of the earlier send, and a
 * link to that message where Telegram has one (supergroups and channels).
 */
function EarlierSend({ item }: { item: Item }) {
  const { t, i18n } = useTranslation()
  const { pushError } = useAlerts()
  const { byId } = useDestinations()
  const [info, setInfo] = useState<DuplicateInfo | null>(null)

  useEffect(() => {
    let stale = false

    // Nothing to show is not an error: the notice above already says why the item waits.
    itemDuplicate(item.id).then(
      (next) => {
        if (!stale) setInfo(next)
      },
      () => {
        if (!stale) setInfo(null)
      },
    )

    return () => {
      stale = true
    }
  }, [item.id])

  if (!info) return null
  const destination = byId.get(info.destination_id)

  return (
    <div className="flex w-full flex-wrap items-center gap-x-3 gap-y-1">
      <p className="text-xs text-muted-foreground">
        {t("queue.item.duplicate.sentTo", {
          destination: destination
            ? destinationName(destination)
            : t("common.destinationNumber", { id: info.destination_id }),
          date: formatDateTime(new Date(info.sent_at), i18n.language),
        })}
      </p>
      {info.link && (
        <Button
          size="sm"
          variant="ghost"
          onClick={() => {
            if (info.link) void openTelegramLink(info.link).catch(pushError)
          }}
        >
          <ExternalLink data-icon="inline-start" />
          {t("queue.item.duplicate.open")}
        </Button>
      )}
    </div>
  )
}

/** An upload interrupted mid-way: search the chat for it, then let the user decide. */
function UnknownNotice({
  item,
  busy,
  onMarkSent,
  onResend,
}: {
  item: Item
  busy: boolean
  onMarkSent: (messageId?: number) => void
  onResend: () => void
}) {
  const { t, i18n } = useTranslation()
  const { pushError } = useAlerts()
  const [searching, setSearching] = useState(false)
  const [candidates, setCandidates] = useState<SentCandidate[] | null>(null)

  async function search() {
    setSearching(true)

    try {
      setCandidates(await itemReconcile(item.id))
    } catch (error) {
      pushError(error, item.id)
    } finally {
      setSearching(false)
    }
  }

  return (
    <Notice tone="attention" text={t("queue.item.unknown.body")}>
      <Button
        size="sm"
        variant="outline"
        disabled={busy || searching}
        onClick={() => void search()}
      >
        {searching
          ? t("queue.item.unknown.searching")
          : t("queue.item.unknown.search")}
      </Button>
      <Button
        size="sm"
        variant="outline"
        disabled={busy}
        onClick={() => onMarkSent()}
      >
        {t("queue.item.unknown.markSent")}
      </Button>
      <Button size="sm" variant="outline" disabled={busy} onClick={onResend}>
        {t("queue.item.unknown.sendAgain")}
      </Button>
      {candidates !== null && (
        <div className="w-full space-y-1.5">
          {candidates.length === 0 ? (
            <p className="text-xs text-muted-foreground">
              {t("queue.item.unknown.noCandidate")}
            </p>
          ) : (
            <>
              <p className="text-xs font-medium">
                {t("queue.item.unknown.candidatesTitle")}
              </p>
              <ul className="space-y-1">
                {candidates.map((candidate) => (
                  <li
                    key={candidate.message_id}
                    className="flex items-center gap-2 rounded-md bg-muted/50 py-1 ps-2.5 pe-1 text-xs"
                  >
                    <span className="min-w-0 flex-1">
                      <span className="block truncate" dir="auto">
                        {t("queue.item.unknown.candidate", {
                          kind: t(`mode.${candidate.kind}`),
                          when: formatDateTime(
                            new Date(candidate.date_unix * 1000),
                            i18n.language,
                          ),
                        })}
                        {candidate.size !== null &&
                          `, ${formatBytes(candidate.size, i18n.language)}`}
                      </span>
                      {(candidate.file_name || candidate.caption_preview) && (
                        <span
                          className="block truncate text-muted-foreground"
                          dir="auto"
                        >
                          <bdi>
                            {candidate.file_name ?? candidate.caption_preview}
                          </bdi>
                        </span>
                      )}
                    </span>
                    <Button
                      size="xs"
                      variant="outline"
                      disabled={busy}
                      onClick={() => onMarkSent(candidate.message_id)}
                    >
                      {t("queue.item.unknown.useCandidate")}
                    </Button>
                  </li>
                ))}
              </ul>
            </>
          )}
        </div>
      )}
    </Notice>
  )
}

function Notice({
  tone,
  text,
  children,
}: {
  tone: "attention" | "danger"
  text: string
  children?: ReactNode
}) {
  return (
    <div
      className={
        tone === "danger"
          ? "space-y-2 rounded-lg border border-destructive/30 bg-destructive/5 p-3 dark:bg-destructive/10"
          : "space-y-2 rounded-lg border border-warning/30 bg-warning/5 p-3"
      }
    >
      <p className="text-sm">{text}</p>
      {children && (
        <div className="flex flex-wrap items-center gap-2">{children}</div>
      )}
    </div>
  )
}

function WaitingNotice({
  item,
  progress,
}: {
  item: Item
  progress: Progressed | undefined
}) {
  const { t, i18n } = useTranslation()
  const now = useNow(true)

  const target = item.next_run_at
    ? Date.parse(item.next_run_at)
    : progress?.retryAfterS !== undefined
      ? progress.receivedAt + progress.retryAfterS * 1000
      : null

  if (target === null || Number.isNaN(target)) {
    return <Notice tone="attention" text={t("queue.phase.waiting_server")} />
  }

  const remaining = Math.max(0, (target - now) / 1000)

  return (
    <Notice
      tone="attention"
      text={t("queue.item.waiting.body", {
        time: formatTime(new Date(target), i18n.language),
      })}
    >
      <p className="text-xs text-muted-foreground tabular-nums" aria-live="off">
        {t("queue.item.waiting.countdown", {
          duration: formatDuration(remaining, i18n.language),
        })}
      </p>
    </Notice>
  )
}

/** An `encoded` item whose `next_run_at` is still ahead; nothing once it is due. */
function ScheduledNotice({ item }: { item: Item }) {
  const { t, i18n } = useTranslation()
  const now = useNow(true)
  const target = item.next_run_at ? Date.parse(item.next_run_at) : Number.NaN

  if (Number.isNaN(target) || target <= now) return null

  const remaining = (target - now) / 1000
  const flood = item.last_error === "TG_FLOOD_WAIT"

  return (
    <Notice
      tone="attention"
      text={t(flood ? "queue.item.waiting.body" : "queue.item.interval.body", {
        time: formatTime(new Date(target), i18n.language),
      })}
    >
      <p className="text-xs text-muted-foreground tabular-nums" aria-live="off">
        {t(
          flood
            ? "queue.item.waiting.countdown"
            : "queue.item.interval.countdown",
          { duration: formatDuration(remaining, i18n.language) },
        )}
      </p>
    </Notice>
  )
}

type StepStatus = "waiting" | "active" | "done"

/** Two progressions, preparation (analysis, compression or remux) then sending. */
function Steps({
  item,
  progress,
}: {
  item: Item
  progress: Progressed | undefined
}) {
  const preparingPhase: Phase | null =
    item.state === "probing" ||
    item.state === "encoding" ||
    item.state === "remuxing"
      ? item.state
      : null

  const preparing = preparingPhase !== null
  const sending = item.state === "uploading" || item.state === "waiting_server"

  const prepareProgress =
    progress && ["probing", "encoding", "remuxing"].includes(progress.phase)
      ? progress
      : undefined

  const sendProgress =
    progress && ["uploading", "awaiting_server"].includes(progress.phase)
      ? progress
      : undefined

  const prepare: StepStatus = isPrepared(item)
    ? "done"
    : preparing
      ? "active"
      : "waiting"

  const send: StepStatus =
    item.state === "sent" ? "done" : sending ? "active" : "waiting"

  return (
    <div className="grid gap-3 sm:grid-cols-2">
      <Step
        step="prepare"
        status={prepare}
        phaseLabel={prepareProgress?.phase ?? preparingPhase}
        percent={prepareProgress?.percent ?? item.progress_pct ?? 0}
        progress={prepareProgress}
      />
      <Step
        step="send"
        status={send}
        phaseLabel={
          sendProgress?.phase ??
          (item.state === "uploading" ? "uploading" : null)
        }
        percent={sendProgress?.percent ?? 0}
        progress={sendProgress}
      />
    </div>
  )
}

function Step({
  step,
  status,
  phaseLabel,
  percent,
  progress,
}: {
  step: "prepare" | "send"
  status: StepStatus
  phaseLabel: QueueProgress["phase"] | null
  percent: number
  progress: Progressed | undefined
}) {
  const { t, i18n } = useTranslation()
  const language = i18n.language
  const labelId = useId()
  const value = status === "done" ? 100 : status === "active" ? percent : 0

  const statusText =
    status === "done"
      ? t("queue.step.done")
      : status === "waiting"
        ? t("queue.step.waiting")
        : phaseLabel === "encoding" && progress?.cpuFallback
          ? t("queue.phase.encodingCpuFallback")
          : phaseLabel
            ? t(`queue.phase.${phaseLabel}`)
            : formatPercent(value, language)

  const details: string[] = []

  if (status === "active" && progress) {
    if (progress.bytesPerSec)
      details.push(
        t("common.rate", { rate: formatBytes(progress.bytesPerSec, language) }),
      )

    if (progress.etaSec != null)
      details.push(
        t("common.eta", {
          duration: formatDuration(progress.etaSec, language),
        }),
      )
  }

  return (
    <div className="space-y-1.5">
      <div className="flex items-baseline justify-between gap-2 text-xs">
        <span id={labelId} className="font-medium">
          {t(`queue.step.${step}`)}
          <span className="font-normal text-muted-foreground">
            {" · "}
            {statusText}
          </span>
        </span>
        {status === "active" && (
          <span className="text-muted-foreground tabular-nums">
            {formatPercent(value, language)}
          </span>
        )}
      </div>
      <Progress
        value={value}
        aria-labelledby={labelId}
        className={status === "waiting" ? "opacity-50" : undefined}
      />
      {details.length > 0 && (
        <p className="text-xs text-muted-foreground tabular-nums">
          {details.join(" · ")}
        </p>
      )}
    </div>
  )
}

function ItemEditor({
  item,
  busy,
  onSave,
}: {
  item: Item
  busy: boolean
  onSave: (patch: {
    caption?: string
    send_mode?: SendMode
  }) => Promise<boolean>
}) {
  const { t } = useTranslation()
  const ids = useId()
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState(item.caption)

  async function saveCaption() {
    if (await onSave({ caption: draft })) setEditing(false)
  }

  return (
    <div className="flex flex-wrap items-start gap-3">
      <div className="flex items-center gap-2">
        <span id={`${ids}-mode`} className="text-xs text-muted-foreground">
          {t("queue.item.mode")}
        </span>
        <Select
          value={item.send_mode}
          disabled={busy}
          onValueChange={(value) => {
            const mode = MODES.find((candidate) => candidate === value)

            if (mode) void onSave({ send_mode: mode })
          }}
        >
          <SelectTrigger size="sm" aria-labelledby={`${ids}-mode`}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent position="popper">
            {MODES.map((mode) => (
              <SelectItem key={mode} value={mode}>
                {t(`mode.${mode}`)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      <div className="min-w-0 flex-1 basis-60">
        {editing ? (
          <div className="space-y-2">
            <label htmlFor={`${ids}-caption`} className="sr-only">
              {t("queue.item.caption.label")}
            </label>
            <Textarea
              id={`${ids}-caption`}
              dir="auto"
              rows={2}
              value={draft}
              autoFocus
              onChange={(event) => setDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  setDraft(item.caption)
                  setEditing(false)
                }
              }}
            />
            <div className="flex justify-end gap-2">
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  setDraft(item.caption)
                  setEditing(false)
                }}
              >
                {t("common.cancel")}
              </Button>
              <Button
                size="sm"
                disabled={busy || draft === item.caption}
                onClick={() => void saveCaption()}
              >
                {t("queue.item.caption.save")}
              </Button>
            </div>
          </div>
        ) : (
          <div className="flex items-start gap-2">
            <p
              className={
                item.caption
                  ? "min-w-0 flex-1 text-sm whitespace-pre-wrap"
                  : "min-w-0 flex-1 text-sm text-muted-foreground"
              }
              dir="auto"
            >
              {item.caption || t("queue.item.caption.empty")}
            </p>
            <Button
              size="sm"
              variant="outline"
              disabled={busy}
              onClick={() => {
                setDraft(item.caption)
                setEditing(true)
              }}
            >
              {t("queue.item.caption.edit")}
            </Button>
          </div>
        )}
      </div>
    </div>
  )
}
