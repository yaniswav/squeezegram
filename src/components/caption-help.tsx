import { useTranslation } from "react-i18next"
import { CAPTION_VARIABLES } from "@/lib/caption-template"

/** The variables a caption template accepts, collapsed under the field that takes them. */
export function CaptionHelp() {
  const { t } = useTranslation()

  return (
    <details className="text-xs text-muted-foreground">
      <summary className="cursor-pointer select-none">
        {t("caption.help.summary")}
      </summary>
      <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
        {CAPTION_VARIABLES.map((name) => (
          <div key={name} className="contents">
            <dt>
              <code dir="ltr">{`{${name}}`}</code>
            </dt>
            <dd>{t(`caption.var.${name}`)}</dd>
          </div>
        ))}
      </dl>
      <p className="mt-2">
        {t("caption.help.escape", { open: "{{", close: "}}" })}
      </p>
    </details>
  )
}
