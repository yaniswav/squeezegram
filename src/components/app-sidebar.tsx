import {
  FlaskConical,
  History,
  ListVideo,
  Plug,
  Settings,
  Shrink,
  type LucideIcon,
} from "lucide-react"
import { motion } from "motion/react"
import { useTranslation } from "react-i18next"
import { cn } from "@/lib/utils"
import { useAuth } from "@/state/auth"

export type Page = "queue" | "history" | "connection" | "settings"

const PAGES: readonly { id: Page; icon: LucideIcon }[] = [
  { id: "queue", icon: ListVideo },
  { id: "history", icon: History },
  { id: "connection", icon: Plug },
  { id: "settings", icon: Settings },
]

interface AppSidebarProps {
  page: Page
  onNavigate: (page: Page) => void
}

export function AppSidebar({ page, onNavigate }: AppSidebarProps) {
  const { t } = useTranslation()
  // The mock answers like Telegram would: say so wherever the user is.
  const testTransport = useAuth().auth?.transport === "mock"

  return (
    <aside className="flex w-48 shrink-0 flex-col border-e bg-muted/30">
      <div className="flex h-14 items-center gap-2.5 px-4">
        <span className="flex size-7 items-center justify-center rounded-lg bg-primary text-primary-foreground">
          <Shrink className="size-3.5" aria-hidden />
        </span>
        {/* Product name: never translated. */}
        <span className="text-sm font-semibold tracking-tight">
          squeezegram
        </span>
      </div>
      <nav aria-label={t("app.nav.label")} className="px-2 py-2">
        <ul className="space-y-0.5">
          {PAGES.map(({ id, icon: Icon }) => {
            const current = page === id

            return (
              <li key={id} className="relative">
                {current && (
                  <motion.span
                    layoutId="nav-current"
                    className="absolute inset-0 rounded-lg bg-secondary"
                    transition={{ type: "spring", stiffness: 500, damping: 40 }}
                    aria-hidden
                  />
                )}
                <button
                  type="button"
                  aria-current={current ? "page" : undefined}
                  onClick={() => onNavigate(id)}
                  className={cn(
                    "relative flex h-8 w-full items-center gap-2.5 rounded-lg px-2.5 text-start text-sm transition-colors outline-none focus-visible:ring-2 focus-visible:ring-ring",
                    current
                      ? "font-medium text-foreground"
                      : "text-muted-foreground hover:bg-muted hover:text-foreground",
                  )}
                >
                  <Icon className="size-4 shrink-0" aria-hidden />
                  {t(`app.nav.${id}`)}
                </button>
              </li>
            )
          })}
        </ul>
      </nav>
      {testTransport && (
        <p
          role="note"
          title={t("app.testTransportHint")}
          className="mx-3 mt-auto mb-3 flex items-center gap-1.5 rounded-md border border-dashed px-2 py-1 text-xs text-muted-foreground"
        >
          <FlaskConical className="size-3 shrink-0" aria-hidden />
          {t("app.testTransport")}
          <span className="sr-only">{t("app.testTransportHint")}</span>
        </p>
      )}
    </aside>
  )
}
