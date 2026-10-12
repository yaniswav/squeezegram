import { ExternalLink } from "lucide-react"
import { useTranslation } from "react-i18next"
import { Button } from "@/components/ui/button"
import { isDesktop, licenseOpenPurchase } from "@/lib/ipc"
import { useAlerts } from "@/state/alerts"

/** Opens @squeezegram_bot on Telegram; the core holds the address. */
export function PurchaseButton() {
  const { t } = useTranslation()
  const { pushError } = useAlerts()

  return (
    <Button
      type="button"
      variant="link"
      size="sm"
      className="px-0"
      disabled={!isDesktop()}
      onClick={() => void licenseOpenPurchase().catch(pushError)}
    >
      <ExternalLink data-icon="inline-start" />
      {t("settings.license.buy")}
    </Button>
  )
}
