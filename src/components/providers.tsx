import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react"
import { useTranslation } from "react-i18next"
import { errorOf } from "@/i18n/errors"
import { availableLanguages, setLanguage } from "@/i18n"
import {
  FRONT_CODES,
  IpcError,
  authState,
  destinationsList,
  destinationsRefresh,
  destinationsSearch,
  destinationTopics,
  destinationUpdate,
  onAuthState,
  onQueueAlert,
  onQueueChanged,
  onQueueProgress,
  onQueueStatus,
  queueList,
  queueStatus,
  settingsGet,
  settingsSet,
  type AuthState,
  type Destination,
  type DestinationPatch,
  type Item,
  type ItemDelta,
  type ItemState,
  type QueueProgress,
  type Settings,
  type SettingsPatch,
} from "@/lib/ipc"
import type { JsonObject } from "@/lib/json"
import { follow } from "@/lib/subscription"
import { isThemePreference, setThemePreference } from "@/lib/theme"
import { AlertsContext, useAlerts, type AppAlert } from "@/state/alerts"
import { AuthContext, useAuth, type AuthApi } from "@/state/auth"
import {
  DestinationsContext,
  mergeDestinations,
  type DestinationsApi,
} from "@/state/destinations"
import { QueueContext, type QueueApi } from "@/state/queue"
import { SettingsContext, type SettingsApi } from "@/state/settings"
import { UpdateProvider } from "./update-provider"

const MAX_ALERTS = 5

function AlertsProvider({ children }: { children: ReactNode }) {
  const [alerts, setAlerts] = useState<AppAlert[]>([])
  const nextId = useRef(1)

  const push = useCallback(
    (code: string, params: JsonObject = {}, itemId?: number) => {
      setAlerts((current) => {
        // The same problem reported twice stays a single alert.
        const rest = current.filter(
          (alert) => alert.code !== code || alert.itemId !== itemId,
        )

        const alert = { id: nextId.current++, code, params, itemId }

        return [...rest, alert].slice(-MAX_ALERTS)
      })
    },
    [],
  )

  const pushError = useCallback(
    (cause: unknown, itemId?: number) => {
      const { code, params } = errorOf(cause)
      push(code, params, itemId)
    },
    [push],
  )

  const dismiss = useCallback((id: number) => {
    setAlerts((current) => current.filter((alert) => alert.id !== id))
  }, [])

  const value = useMemo(
    () => ({ alerts, push, pushError, dismiss }),
    [alerts, push, pushError, dismiss],
  )

  return <AlertsContext value={value}>{children}</AlertsContext>
}

/** Applies `ui.theme` and `ui.language` from the core's settings. */
function applyUi(settings: Settings, currentLanguage: string): void {
  if (isThemePreference(settings.ui.theme))
    setThemePreference(settings.ui.theme)
  const language = settings.ui.language

  if (
    language !== currentLanguage &&
    availableLanguages().some(({ code }) => code === language)
  ) {
    void setLanguage(language)
  }
}

function SettingsProvider({ children }: { children: ReactNode }) {
  const { i18n } = useTranslation()
  const { pushError } = useAlerts()
  const [settings, setSettings] = useState<Settings | null>(null)
  const [status, setStatus] = useState<SettingsApi["status"]>("loading")

  const onLoaded = useCallback(
    (loaded: Settings) => {
      setSettings(loaded)
      setStatus("ready")
      applyUi(loaded, i18n.language)
    },
    [i18n],
  )

  const onFailed = useCallback(
    (cause: unknown) => {
      setStatus("error")
      pushError(cause)
    },
    [pushError],
  )

  const reload = useCallback(
    () => settingsGet().then(onLoaded, onFailed),
    [onLoaded, onFailed],
  )

  const save = useCallback(
    async (patch: SettingsPatch) => {
      const saved = await settingsSet(patch)
      setSettings(saved)
      applyUi(saved, i18n.language)

      return saved
    },
    [i18n],
  )

  useEffect(() => {
    settingsGet().then(onLoaded, onFailed)
  }, [onLoaded, onFailed])

  const value = useMemo(
    () => ({ status, settings, save, reload }),
    [status, settings, save, reload],
  )

  return <SettingsContext value={value}>{children}</SettingsContext>
}

function AuthProvider({ children }: { children: ReactNode }) {
  const { pushError } = useAlerts()
  const [auth, setAuth] = useState<AuthState | null>(null)
  const [status, setStatus] = useState<AuthApi["status"]>("loading")

  // The listener first, then the state: a change made while both were on their way (TDLib
  // signing in at startup) would otherwise be missed until the next one.
  useEffect(
    () =>
      follow({
        listen: onAuthState,
        onEvent: (payload) => {
          setAuth(payload)
          setStatus("ready")
        },
        fetch: authState,
        // An event that landed before the answer is at least as new as that answer.
        onFetched: (payload, eventSeen) => {
          if (eventSeen) return
          setAuth(payload)
          setStatus("ready")
        },
        onFetchFailed: (cause) => {
          setStatus((current) => (current === "ready" ? current : "error"))
          pushError(cause)
        },
      }),
    [pushError],
  )

  const value = useMemo(() => ({ status, auth }), [status, auth])

  return <AuthContext value={value}>{children}</AuthContext>
}

function DestinationsProvider({ children }: { children: ReactNode }) {
  const { pushError } = useAlerts()
  const signedIn = useAuth().auth?.state === "ready"
  const [list, setList] = useState<Destination[]>([])
  const [status, setStatus] = useState<DestinationsApi["status"]>("loading")

  const onLoaded = useCallback((loaded: Destination[]) => {
    setList(loaded)
    setStatus("ready")
  }, [])

  const onFailed = useCallback((cause: unknown) => {
    const missing =
      cause instanceof IpcError && cause.code === FRONT_CODES.commandUnavailable

    setStatus(missing ? "unavailable" : "error")

    if (!missing) console.error(cause)
  }, [])

  const reload = useCallback(
    () => destinationsList().then(onLoaded, onFailed),
    [onLoaded, onFailed],
  )

  // Loaded at start, and again once the account is signed in.
  useEffect(() => {
    destinationsList().then(onLoaded, onFailed)
  }, [onLoaded, onFailed, signedIn])

  const refresh = useCallback(async () => {
    try {
      onLoaded(await destinationsRefresh())
    } catch (error) {
      pushError(error)
    }
  }, [onLoaded, pushError])

  const update = useCallback(
    async (id: number, patch: DestinationPatch) => {
      try {
        const saved = await destinationUpdate(id, patch)
        setList((current) =>
          current.map((destination) =>
            destination.id === id ? saved : destination,
          ),
        )

        return true
      } catch (error) {
        pushError(error)

        return false
      }
    },
    [pushError],
  )

  const search = useCallback(async (query: string) => {
    const found = await destinationsSearch(query)
    setList((current) => mergeDestinations(current, found))

    return found
  }, [])

  const loadTopics = useCallback(
    async (forumId: number) => {
      try {
        const topics = await destinationTopics(forumId)
        setList((current) => mergeDestinations(current, topics))

        return true
      } catch (error) {
        pushError(error)

        return false
      }
    },
    [pushError],
  )

  const value = useMemo(
    () => ({
      status,
      list,
      byId: new Map(list.map((destination) => [destination.id, destination])),
      reload,
      refresh,
      update,
      search,
      loadTopics,
    }),
    [status, list, reload, refresh, update, search, loadTopics],
  )

  return <DestinationsContext value={value}>{children}</DestinationsContext>
}

/** States during which `queue://progress` events are meaningful. */
const PROGRESS_STATES: ReadonlySet<ItemState> = new Set([
  "probing",
  "encoding",
  "remuxing",
  "uploading",
  "waiting_server",
])

/** Deltas only carry state, hold reason and position; the debounced refetch fills the rest. */
function applyDeltas(
  items: Item[],
  deltas: ItemDelta[],
  removed: number[],
): Item[] {
  const byId = new Map(deltas.map((delta) => [delta.id, delta]))
  const gone = new Set(removed)

  return items
    .filter((item) => !gone.has(item.id))
    .map((item) => {
      const delta = byId.get(item.id)

      return delta
        ? {
            ...item,
            state: delta.state,
            hold_reason: delta.hold_reason,
            position: delta.position,
          }
        : item
    })
    .sort((a, b) => a.position - b.position || a.id - b.id)
}

const REFRESH_DELAY_MS = 150

/** `queue://progress` detail sent when the GPU stopped and the encode restarts on the CPU. */
const CPU_FALLBACK = "cpu_fallback"

type ProgressMap = QueueApi["progress"]

function QueueProvider({ children }: { children: ReactNode }) {
  const { push, pushError } = useAlerts()
  const [items, setItems] = useState<Item[]>([])
  const [status, setStatus] = useState<QueueApi["status"]>("loading")
  const [progress, setProgress] = useState<ProgressMap>(new Map())
  const [running, setRunning] = useState<boolean | null>(null)

  const onList = useCallback((list: Item[]) => {
    setItems(list)
    setStatus("ready")
  }, [])

  const onListFailed = useCallback(
    (cause: unknown) => {
      setStatus((current) => (current === "ready" ? current : "error"))
      pushError(cause)
    },
    [pushError],
  )

  // Only the latest request is applied: an answer overtaken by a later one is older than it.
  const latestList = useRef(0)

  const refresh = useCallback(() => {
    const request = ++latestList.current

    return queueList().then(
      (list) => {
        if (request === latestList.current) onList(list)
      },
      (cause: unknown) => {
        if (request === latestList.current) onListFailed(cause)
      },
    )
  }, [onList, onListFailed])

  useEffect(() => {
    let stopped = false
    let timer: number | undefined

    const scheduleRefresh = () => {
      window.clearTimeout(timer)
      timer = window.setTimeout(() => void refresh(), REFRESH_DELAY_MS)
    }

    const stopChanged = onQueueChanged(({ items: deltas, removed }) => {
      setItems((current) => applyDeltas(current, deltas, removed))
      setProgress((current) => {
        const next = new Map(current)

        for (const delta of deltas) {
          if (!PROGRESS_STATES.has(delta.state)) next.delete(delta.id)
        }

        for (const id of removed) next.delete(id)

        return next
      })
      scheduleRefresh()
    })

    const stopProgress = onQueueProgress((event: QueueProgress) => {
      setProgress((current) => {
        // Announced once by the core, then the encode reports its speed as usual.
        const cpuFallback =
          event.phase === "encoding" &&
          (event.detail === CPU_FALLBACK ||
            current.get(event.itemId)?.cpuFallback === true)

        return new Map(current).set(event.itemId, {
          ...event,
          receivedAt: Date.now(),
          cpuFallback,
        })
      })
    })

    const stopAlert = onQueueAlert(({ code, params, itemId }) =>
      push(code, params ?? {}, itemId),
    )

    // Listeners first, then the current state: a change made in between is either in the
    // answer or announced by an event (each `queue://changed` also asks for the list again).
    void stopChanged.ready.then(() => {
      if (!stopped) void refresh()
    })

    const stopStatus = follow({
      listen: onQueueStatus,
      onEvent: ({ running: now }) => setRunning(now),
      fetch: queueStatus,
      onFetched: ({ running: now }, eventSeen) => {
        if (!eventSeen) setRunning(now)
      },
      onFetchFailed: pushError,
    })

    return () => {
      stopped = true
      stopStatus()
      stopChanged()
      stopProgress()
      stopAlert()
      window.clearTimeout(timer)
    }
  }, [refresh, push, pushError])

  const value = useMemo(
    () => ({ status, items, progress, running, refresh }),
    [status, items, progress, running, refresh],
  )

  return <QueueContext value={value}>{children}</QueueContext>
}

export function AppProviders({ children }: { children: ReactNode }) {
  return (
    <AlertsProvider>
      <SettingsProvider>
        <AuthProvider>
          <DestinationsProvider>
            <QueueProvider>
              <UpdateProvider>{children}</UpdateProvider>
            </QueueProvider>
          </DestinationsProvider>
        </AuthProvider>
      </SettingsProvider>
    </AlertsProvider>
  )
}
