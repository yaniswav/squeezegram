import { useCallback, useState } from "react"
import { useTranslation } from "react-i18next"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { GroupRiskGuidance } from "@/components/group-risk"
import { needsGroupRiskNotice, type RiskDestination } from "@/lib/group-risk"
import { useAlerts } from "@/state/alerts"
import { useSettings } from "@/state/settings"

/**
 * The one-time warning before the first send to a group or channel (F33). `guard` runs
 * `proceed` at once when the warning does not apply (Saved Messages, private chats, already
 * acknowledged); otherwise it shows `dialog` and runs `proceed` only if the user accepts.
 * The acknowledgement is saved in the settings, so it is asked once.
 */
export function useGroupRiskGate() {
  const { t } = useTranslation()
  const { pushError } = useAlerts()
  const { settings, save } = useSettings()
  // Wrapped: a bare function in state would be called by the setter.
  const [pending, setPending] = useState<{ proceed: () => void } | null>(null)
  const acknowledged = settings?.telegram.group_risk_acknowledged ?? false

  const guard = useCallback(
    (destinations: readonly RiskDestination[], proceed: () => void) => {
      if (needsGroupRiskNotice(destinations, acknowledged))
        setPending({ proceed })
      else proceed()
    },
    [acknowledged],
  )

  async function accept() {
    const waiting = pending

    setPending(null)

    if (!waiting) return

    try {
      await save({ telegram: { group_risk_acknowledged: true } })
    } catch (error) {
      // The user did accept: the send goes on, and the notice may come back next time.
      pushError(error)
    }

    waiting.proceed()
  }

  const dialog = (
    <ConfirmDialog
      open={pending !== null}
      onOpenChange={(open) => {
        if (!open) setPending(null)
      }}
      title={t("groupRisk.title")}
      description={t("groupRisk.intro")}
      confirmLabel={t("groupRisk.confirm")}
      cancelLabel={t("common.cancel")}
      onConfirm={() => void accept()}
    >
      <GroupRiskGuidance />
    </ConfirmDialog>
  )

  return { guard, dialog }
}
