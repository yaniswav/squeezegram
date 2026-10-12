import { AnimatePresence, MotionConfig, motion } from "motion/react"
import { Direction } from "radix-ui"
import { useCallback, useEffect, useRef, useState } from "react"
import { useTranslation } from "react-i18next"
import { AlertStack } from "@/components/alert-stack"
import { AppSidebar, type Page } from "@/components/app-sidebar"
import { DropOverlay } from "@/components/drop-overlay"
import { AppProviders } from "@/components/providers"
import { QuitConfirm } from "@/components/quit-confirm"
import { SidecarBanner } from "@/components/sidecar-banner"
import { TooltipProvider } from "@/components/ui/tooltip"
import { UpdateBanner } from "@/components/update-banner"
import { useFileDrop } from "@/hooks/use-file-drop"
import { directionOf } from "@/i18n/languages"
import { logScreen } from "@/lib/ipc"
import { ConnectionPage } from "@/pages/connection-page"
import { HistoryPage } from "@/pages/history-page"
import { QueuePage } from "@/pages/queue-page"
import { SettingsPage } from "@/pages/settings-page"

export default function App() {
  const { i18n } = useTranslation()

  return (
    // Radix primitives ignore the DOM `dir` attribute, so they get the direction explicitly.
    <Direction.Provider dir={directionOf(i18n.language)}>
      {/* Every Motion animation honours prefers-reduced-motion. */}
      <MotionConfig reducedMotion="user">
        <TooltipProvider>
          <AppProviders>
            <Shell />
          </AppProviders>
        </TooltipProvider>
      </MotionConfig>
    </Direction.Provider>
  )
}

function Shell() {
  const { t } = useTranslation()
  const [page, setPage] = useState<Page>("queue")
  const [navigated, setNavigated] = useState(false)
  const [stagedPaths, setStagedPaths] = useState<string[]>([])
  const shownPage = useRef<Page | null>(null)

  // Every screen change goes to the journal (`navigate` event).
  useEffect(() => {
    logScreen(shownPage.current, page)
    shownPage.current = page
  }, [page])

  const navigate = useCallback((next: Page) => {
    setPage(next)
    setNavigated(true)
  }, [])

  const stage = useCallback(
    (paths: string[]) => {
      setStagedPaths((current) => [...new Set([...current, ...paths])])
      navigate("queue")
    },
    [navigate],
  )

  const dragging = useFileDrop(stage)

  return (
    <div className="flex h-full overflow-clip">
      <a
        href="#main"
        className="sr-only z-50 rounded-lg bg-primary px-3 py-2 text-sm text-primary-foreground focus:not-sr-only focus:fixed focus:start-3 focus:top-3"
      >
        {t("app.skipToContent")}
      </a>
      <AppSidebar page={page} onNavigate={navigate} />
      <main
        id="main"
        tabIndex={-1}
        className="min-w-0 flex-1 overflow-x-hidden overflow-y-auto overscroll-contain outline-none"
      >
        <div className="mx-auto w-full max-w-3xl space-y-4 px-6 py-6">
          <SidecarBanner onOpenSettings={() => navigate("settings")} />
          <UpdateBanner />
          <AlertStack />
          <AnimatePresence mode="wait" initial={false}>
            <motion.div
              key={page}
              initial={{ opacity: 0, y: 4 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0 }}
              transition={{ duration: 0.15 }}
            >
              {page === "queue" && (
                <QueuePage
                  focusTitle={navigated}
                  stagedPaths={stagedPaths}
                  onStagedPathsChange={setStagedPaths}
                />
              )}
              {page === "history" && <HistoryPage focusTitle={navigated} />}
              {page === "connection" && (
                <ConnectionPage focusTitle={navigated} />
              )}
              {page === "settings" && <SettingsPage focusTitle={navigated} />}
            </motion.div>
          </AnimatePresence>
        </div>
      </main>
      <DropOverlay visible={dragging} />
      <QuitConfirm />
    </div>
  )
}
