import { Trash2 } from "lucide-react"
import { useState } from "react"
import { useTranslation } from "react-i18next"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { Button } from "@/components/ui/button"
import { historyClear } from "@/lib/ipc"
import { useAlerts } from "@/state/alerts"

interface ClearHistoryProps {
  disabled: boolean
  /** Called with the number of lines deleted. */
  onCleared: (removed: number) => void
}

/** "Clear history" button and its confirmation, which explains the duplicate warning. */
export function ClearHistory({ disabled, onCleared }: ClearHistoryProps) {
  const { t } = useTranslation()
  const { pushError } = useAlerts()
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)

  async function clear() {
    setBusy(true)

    try {
      onCleared(await historyClear())
    } catch (error) {
      pushError(error)
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <Button
        variant="outline"
        disabled={disabled || busy}
        onClick={() => setOpen(true)}
      >
        <Trash2 data-icon="inline-start" />
        {t("history.clear.button")}
      </Button>
      <ConfirmDialog
        open={open}
        onOpenChange={setOpen}
        title={t("history.clear.title")}
        description={t("history.clear.body")}
        confirmLabel={t("history.clear.confirm")}
        cancelLabel={t("common.cancel")}
        destructive
        onConfirm={() => void clear()}
      />
    </>
  )
}
