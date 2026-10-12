import { useTranslation } from "react-i18next"
import { Badge } from "@/components/ui/badge"
import type { ItemState, PlanKind } from "@/lib/ipc"
import { cn } from "@/lib/utils"

type Tone = "neutral" | "active" | "attention" | "success" | "danger" | "muted"

const STATE_TONE: Record<ItemState, Tone> = {
  pending_review: "attention",
  ready: "neutral",
  probing: "active",
  encoding: "active",
  remuxing: "active",
  encoded: "neutral",
  uploading: "active",
  waiting_server: "attention",
  sent: "success",
  failed: "danger",
  unknown: "attention",
  cancelled: "muted",
}

const TONE_CLASS: Record<Tone, string> = {
  neutral: "bg-secondary text-secondary-foreground",
  active: "bg-primary text-primary-foreground",
  attention: "bg-warning/15 text-warning",
  success: "bg-success/15 text-success",
  danger: "bg-destructive/10 text-destructive dark:bg-destructive/20",
  muted: "border-border bg-transparent text-muted-foreground",
}

export function QueueStateBadge({ state }: { state: ItemState }) {
  const { t } = useTranslation()

  return (
    <Badge className={cn(TONE_CLASS[STATE_TONE[state]])}>
      {t(`queue.state.${state}`)}
    </Badge>
  )
}

const PLAN_TONE: Record<PlanKind, Tone> = {
  skip: "success",
  encode: "attention",
  remux: "neutral",
  remux_audio: "neutral",
  photo: "neutral",
}

/** Compliant as is, compression planned, or remux only. Known after analysis. */
export function PlanBadge({ kind }: { kind: PlanKind | null }) {
  const { t } = useTranslation()

  return (
    <Badge
      variant="outline"
      className={cn(kind ? TONE_CLASS[PLAN_TONE[kind]] : TONE_CLASS.muted)}
    >
      {t(kind ? `queue.plan.${kind}` : "queue.plan.none")}
    </Badge>
  )
}
