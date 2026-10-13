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
import { follow } from "@/lib/subscription"
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
    // The startup check runs once the page has loaded and announces its result only once:
    // the listener is registered before the stored result is asked, and an event that lands
    // before the answer is the newer one.
    const stopNotice = follow({
      listen: onUpdate,
      onEvent: (payload) => {
        setNotice(payload)
        void reload()
      },
      fetch: updateInfo,
      onFetched: (loaded, eventSeen) => {
        setInfo(loaded)

        if (!eventSeen) setNotice(loaded.notice)
      },
      onFetchFailed: (cause) =>
        console.warn("Update information unavailable", cause),
    })

    const stopProgress = onUpdateProgress(setProgress)

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
