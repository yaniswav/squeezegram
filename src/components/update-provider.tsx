import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react"
import {
  onUpdate,
  onUpdateProgress,
  updateCheck,
  updateInfo,
  updateInstall,
  updateOpenPage,
  type UpdateInfo,
  type UpdateNotice,
  type UpdateProgress,
} from "@/lib/ipc"
import { useAlerts } from "@/state/alerts"
import { useSettings } from "@/state/settings"
import { UpdateContext, type UpdateApi } from "@/state/update"

/**
 * Update state shared by the banner and Settings: what the core found (at startup or on
 * request), the download progress, and the three actions. The core does the work; the
 * settings are read again after each check because it records the time of the check.
 */
export function UpdateProvider({ children }: { children: ReactNode }) {
  const { pushError } = useAlerts()
  const { reload } = useSettings()
  const [info, setInfo] = useState<UpdateInfo | null>(null)
  const [notice, setNotice] = useState<UpdateNotice | null>(null)
  const [progress, setProgress] = useState<UpdateProgress | null>(null)
  const [checking, setChecking] = useState(false)
  const [installing, setInstalling] = useState(false)

  useEffect(() => {
    // The startup check can finish before the first answer: the event is the newer one.
    let eventSeen = false

    const stopNotice = onUpdate((payload) => {
      eventSeen = true
      setNotice(payload)
      void reload()
    })

    const stopProgress = onUpdateProgress(setProgress)

    updateInfo().then(
      (loaded) => {
        setInfo(loaded)

        if (!eventSeen) setNotice(loaded.notice)
      },
      (cause: unknown) => console.warn("Update information unavailable", cause),
    )

    return () => {
      stopNotice()
      stopProgress()
    }
  }, [reload])

  const check = useCallback(async () => {
    setChecking(true)

    try {
      const found = await updateCheck()
      setNotice(found)

      return found
    } catch (cause) {
      pushError(cause)

      return null
    } finally {
      setChecking(false)
      void reload()
    }
  }, [pushError, reload])

  const install = useCallback(async () => {
    setInstalling(true)
    setProgress(null)

    try {
      await updateInstall()
    } catch (cause) {
      pushError(cause)
    } finally {
      setInstalling(false)
      setProgress(null)
    }
  }, [pushError])

  const openPage = useCallback(
    () => updateOpenPage().catch(pushError),
    [pushError],
  )

  const value = useMemo<UpdateApi>(
    () => ({
      info,
      notice,
      progress,
      checking,
      installing,
      check,
      install,
      openPage,
    }),
    [info, notice, progress, checking, installing, check, install, openPage],
  )

  return <UpdateContext value={value}>{children}</UpdateContext>
}
