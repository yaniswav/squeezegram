import { useTranslation } from "react-i18next"

/** Good practice and the promise, under the intro line: the dialog and Settings share it. */
export function GroupRiskGuidance() {
  const { t } = useTranslation()

  return (
    <div className="space-y-2 text-sm">
      <p className="font-medium">{t("groupRisk.practice.title")}</p>
      <ul className="list-disc space-y-1 ps-5">
        <li>{t("groupRisk.practice.permissions")}</li>
        <li>{t("groupRisk.practice.slowMode")}</li>
        <li>{t("groupRisk.practice.spam")}</li>
      </ul>
      <p className="text-muted-foreground">{t("groupRisk.promise")}</p>
    </div>
  )
}
