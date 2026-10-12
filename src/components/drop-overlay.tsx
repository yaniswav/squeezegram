import { FileUp } from "lucide-react"
import { AnimatePresence, motion } from "motion/react"
import { useTranslation } from "react-i18next"

/** Shown while files are dragged over the window (the drop target is the whole window). */
export function DropOverlay({ visible }: { visible: boolean }) {
  const { t } = useTranslation()

  return (
    <AnimatePresence>
      {visible && (
        <motion.div
          aria-hidden
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.12 }}
          className="pointer-events-none fixed inset-0 z-40 flex items-center justify-center bg-background/85 p-6"
        >
          <motion.div
            initial={{ scale: 0.98 }}
            animate={{ scale: 1 }}
            className="flex size-full flex-col items-center justify-center gap-3 rounded-2xl border-2 border-dashed border-primary text-foreground"
          >
            <FileUp className="size-10" aria-hidden />
            <p className="text-lg font-medium">{t("drop.overlay")}</p>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  )
}
