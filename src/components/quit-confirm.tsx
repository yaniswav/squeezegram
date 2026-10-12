import { useEffect, useState } from "react"
import { useTranslation } from "react-i18next"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { appQuit, onQuitRequested } from "@/lib/ipc"
import { useAlerts } from "@/state/alerts"

/**
 * Asks before the tray's Quit interrupts work. The core brings the window up and sends
 * `app://quit-requested` only when an item is being compressed or sent; otherwise it quits
 * without asking.
 */
export function QuitConfirm() {
  const { t } = useTranslation()
  const { pushError } = useAlerts()
  const [open, setOpen] = useState(false)

  useEffect(() => onQuitRequested(() => setOpen(true)), [])

  return (
    <ConfirmDialog
      open={open}
      onOpenChange={setOpen}
      title={t("quit.title")}
      description={t("quit.body")}
      confirmLabel={t("quit.confirm")}
      cancelLabel={t("common.cancel")}
      destructive
      onConfirm={() => appQuit().catch(pushError)}
    />
  )
}
