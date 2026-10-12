import {
  Download,
  FolderOpen,
  KeyRound,
  RefreshCw,
  RotateCcw,
  Save,
  Trash2,
} from "lucide-react"
import { AnimatePresence, motion } from "motion/react"
import { useEffect, useId, useState, type ReactNode } from "react"
import { useTranslation } from "react-i18next"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { DestinationField } from "@/components/destination-field"
import { FfmpegFinder } from "@/components/ffmpeg-finder"
import { PageHeader } from "@/components/page-header"
import { PurchaseButton } from "@/components/purchase-button"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import {
  useEntitlementStatus,
  type EntitlementState,
} from "@/hooks/use-entitlement-status"
import { availableLanguages } from "@/i18n"
import { errorMessage, errorOf, type ErrorReport } from "@/i18n/errors"
import {
  AUDIO_KBPS_CHOICES,
  AUTOMATIC_ENCODING,
  MAX_RESOLUTIONS,
  effectiveEncoding,
} from "@/lib/advanced-encoding"
import { grants, hasStoredLicense, licenseView } from "@/lib/entitlements"
import { formatDateTime, formatList, formatNumber } from "@/lib/format"
import {
  debugExport,
  encoderStatus,
  isDesktop,
  licenseClear,
  licenseSet,
  limitStatus,
  logOpenDir,
  logPath,
  pickFolder,
  sidecarsStatus,
  watchAdd,
  watchList,
  watchRemove,
  watchUpdate,
  type EncoderStatus,
  type EntitlementStatus,
  type LimitStatus,
  type MaxResolution,
  type SendMode,
  type Settings,
  type SettingsPatch,
  type SidecarStatus,
  type ThemeSetting,
  type WatchFolder,
} from "@/lib/ipc"
import { isThemePreference } from "@/lib/theme"
import { useAlerts } from "@/state/alerts"
import { useAuth } from "@/state/auth"
import { destinationName, useDestinations } from "@/state/destinations"
import { useSettings } from "@/state/settings"
import { useUpdate } from "@/state/update"

type Premium = "auto" | "yes" | "no"

type Prefer = "gpu" | "cpu"

/** Text inputs stay strings until validation; folders are null for "default". */
interface Draft {
  premium: Premium
  customMb: string
  margin: string
  prefer: Prefer
  maxResolution: MaxResolution
  audioKbps: string
  pauseMin: string
  pauseMax: string
  tempDir: string | null
  minFreeGb: string
  ffmpegDir: string | null
  theme: ThemeSetting
  language: string
  checkAutomatically: boolean
}

type ErrorKey =
  | "settings.limit.custom.error"
  | "settings.limit.margin.error"
  | "settings.send.errorInteger"
  | "settings.send.errorOrder"
  | "settings.temp.errorInteger"

type DraftErrors = Partial<
  Record<
    "customMb" | "margin" | "pauseMin" | "pauseMax" | "minFreeGb",
    ErrorKey
  >
>

function premiumOf(value: boolean | null): Premium {
  return value === null ? "auto" : value ? "yes" : "no"
}

function preferOf(value: string): Prefer {
  return value === "cpu" ? "cpu" : "gpu"
}

function themeOf(value: string): ThemeSetting {
  return isThemePreference(value) ? value : "system"
}

function emptyToNull(value: string | null): string | null {
  return value === null || value.trim() === "" ? null : value
}

function toDraft(settings: Settings): Draft {
  return {
    premium: premiumOf(settings.limit.premium),
    customMb: settings.limit.custom_mb?.toString() ?? "",
    margin: settings.limit.margin_percent.toString(),
    prefer: preferOf(settings.encoding.prefer),
    maxResolution: settings.advanced_encoding.max_resolution,
    audioKbps: settings.advanced_encoding.audio_kbps.toString(),
    pauseMin: settings.send.pause_min_s.toString(),
    pauseMax: settings.send.pause_max_s.toString(),
    tempDir: emptyToNull(settings.temp.dir),
    minFreeGb: settings.temp.min_free_gb.toString(),
    ffmpegDir: emptyToNull(settings.ffmpeg.custom_dir),
    theme: themeOf(settings.ui.theme),
    language: settings.ui.language,
    checkAutomatically: settings.updates.check_automatically,
  }
}

const WHOLE = /^\d+$/

/** Same rules as `Settings::validate` (settings.rs) plus the types serde expects. */
function validate(draft: Draft): DraftErrors {
  const errors: DraftErrors = {}

  if (draft.customMb.trim() !== "") {
    const mb = Number(draft.customMb)

    if (!WHOLE.test(draft.customMb.trim()) || mb <= 0)
      errors.customMb = "settings.limit.custom.error"
  }

  const margin = Number(draft.margin)

  if (draft.margin.trim() === "" || !(margin >= 0 && margin < 50))
    errors.margin = "settings.limit.margin.error"

  if (!WHOLE.test(draft.pauseMin.trim()))
    errors.pauseMin = "settings.send.errorInteger"

  if (!WHOLE.test(draft.pauseMax.trim()))
    errors.pauseMax = "settings.send.errorInteger"

  if (
    !errors.pauseMin &&
    !errors.pauseMax &&
    Number(draft.pauseMin) > Number(draft.pauseMax)
  )
    errors.pauseMin = "settings.send.errorOrder"

  if (!WHOLE.test(draft.minFreeGb.trim()))
    errors.minFreeGb = "settings.temp.errorInteger"

  return errors
}

/** Merge patch holding only the fields that changed; `null` resets to the default. */
function toPatch(draft: Draft, settings: Settings): SettingsPatch {
  const saved = toDraft(settings)
  const patch: SettingsPatch = {}
  const limit: NonNullable<SettingsPatch["limit"]> = {}

  if (draft.premium !== saved.premium)
    limit.premium = draft.premium === "auto" ? null : draft.premium === "yes"

  if (draft.customMb.trim() !== saved.customMb)
    limit.custom_mb =
      draft.customMb.trim() === "" ? null : Number(draft.customMb)

  if (Number(draft.margin) !== settings.limit.margin_percent)
    limit.margin_percent = Number(draft.margin)

  if (Object.keys(limit).length > 0) patch.limit = limit

  if (draft.prefer !== saved.prefer) patch.encoding = { prefer: draft.prefer }

  const advanced: NonNullable<SettingsPatch["advanced_encoding"]> = {}

  if (draft.maxResolution !== saved.maxResolution)
    advanced.max_resolution = draft.maxResolution

  if (Number(draft.audioKbps) !== settings.advanced_encoding.audio_kbps)
    advanced.audio_kbps = Number(draft.audioKbps)

  if (Object.keys(advanced).length > 0) patch.advanced_encoding = advanced

  const send: NonNullable<SettingsPatch["send"]> = {}

  if (Number(draft.pauseMin) !== settings.send.pause_min_s)
    send.pause_min_s = Number(draft.pauseMin)

  if (Number(draft.pauseMax) !== settings.send.pause_max_s)
    send.pause_max_s = Number(draft.pauseMax)

  if (Object.keys(send).length > 0) patch.send = send

  const temp: NonNullable<SettingsPatch["temp"]> = {}

  if (draft.tempDir !== saved.tempDir) temp.dir = draft.tempDir

  if (Number(draft.minFreeGb) !== settings.temp.min_free_gb)
    temp.min_free_gb = Number(draft.minFreeGb)

  if (Object.keys(temp).length > 0) patch.temp = temp

  if (draft.ffmpegDir !== saved.ffmpegDir)
    patch.ffmpeg = { custom_dir: draft.ffmpegDir }

  const ui: NonNullable<SettingsPatch["ui"]> = {}

  if (draft.theme !== saved.theme) ui.theme = draft.theme

  if (draft.language !== saved.language) ui.language = draft.language

  if (Object.keys(ui).length > 0) patch.ui = ui

  if (draft.checkAutomatically !== saved.checkAutomatically)
    patch.updates = { check_automatically: draft.checkAutomatically }

  return patch
}

export function SettingsPage({ focusTitle }: { focusTitle: boolean }) {
  const { t } = useTranslation()
  const { status, settings, reload } = useSettings()
  const [entitlements, setEntitlements] = useEntitlementStatus()

  return (
    <div className="space-y-6">
      <PageHeader title={t("settings.title")} focusTitle={focusTitle} />
      {status === "loading" && (
        <p className="text-sm text-muted-foreground">{t("common.loading")}</p>
      )}
      {status === "error" && (
        <>
          <div className="flex items-center justify-between gap-3 text-sm">
            <p className="text-destructive">{t("settings.loadError")}</p>
            <Button variant="outline" size="sm" onClick={() => void reload()}>
              {t("common.retry")}
            </Button>
          </div>
          {/* Most useful precisely when something is broken. */}
          <Diagnostics />
        </>
      )}
      {settings && (
        <>
          <LicenseSection status={entitlements} onChange={setEntitlements} />
          <WatchedFolders entitlements={entitlements} />
          <SettingsForm settings={settings} entitlements={entitlements} />
        </>
      )}
    </div>
  )
}

/** The plan in words. Names no account and shows nothing of the license itself. */
function LicenseStatusText({ status }: { status: EntitlementState }) {
  const { t, i18n } = useTranslation()

  if (status === null) return null

  if (status === "failed")
    return (
      <p className="text-sm text-muted-foreground">
        {t("settings.license.unavailable")}
      </p>
    )

  const view = licenseView(status)

  switch (view.kind) {
    case "premium": {
      const features = view.features.map((feature) =>
        t(`settings.license.feature.${feature}`),
      )

      return (
        <div role="status" className="space-y-1">
          <p className="text-sm font-medium">
            {t("settings.license.status.premium")}
          </p>
          {features.length > 0 && (
            <p className="text-sm text-muted-foreground">
              {t("settings.license.status.features", {
                list: formatList(features, i18n.language),
              })}
            </p>
          )}
        </div>
      )
    }

    case "free":
      return (
        <p role="status" className="text-sm font-medium">
          {t("settings.license.status.free")}
        </p>
      )

    case "pending":
      return (
        <LicenseNotice>{t("settings.license.status.pending")}</LicenseNotice>
      )

    case "other_account":
      return (
        <LicenseNotice>
          {t("settings.license.status.otherAccount")}
        </LicenseNotice>
      )

    case "invalid":
      return (
        <LicenseNotice>
          {t("settings.license.status.invalid", {
            message: errorMessage(t, view.code),
          })}
        </LicenseNotice>
      )
  }
}

function LicenseNotice({ children }: { children: ReactNode }) {
  return (
    <p
      role="status"
      className="rounded-lg border border-warning/30 bg-warning/5 p-3 text-sm"
    >
      {children}
    </p>
  )
}

/** Paste and activate a license, see what it unlocks, remove it. */
function LicenseSection({
  status,
  onChange,
}: {
  status: EntitlementState
  onChange: (status: EntitlementStatus) => void
}) {
  const { t } = useTranslation()
  const { pushError } = useAlerts()
  const id = useId()
  const [token, setToken] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<ErrorReport | null>(null)
  const [saved, setSaved] = useState<EntitlementStatus | null>(null)
  const [confirming, setConfirming] = useState(false)

  const stored =
    status !== null && status !== "failed" && hasStoredLicense(status)

  async function activate() {
    if (busy || token.trim() === "") return
    setBusy(true)
    setError(null)
    setSaved(null)

    try {
      const next = await licenseSet(token)
      onChange(next)
      setSaved(next)
      setToken("")
    } catch (cause) {
      setError(errorOf(cause))
    } finally {
      setBusy(false)
    }
  }

  async function remove() {
    setSaved(null)

    try {
      onChange(await licenseClear())
    } catch (cause) {
      pushError(cause)
    }
  }

  const describedBy = error ? `${id}-error ${id}-hint` : `${id}-hint`

  return (
    <Section
      title={t("settings.license.title")}
      hint={t("settings.license.hint")}
    >
      <LicenseStatusText status={status} />
      <form
        noValidate
        className="space-y-2"
        onSubmit={(event) => {
          event.preventDefault()
          void activate()
        }}
      >
        <label htmlFor={`${id}-token`} className="block text-sm font-medium">
          {t("settings.license.input.label")}
        </label>
        <div className="flex flex-wrap gap-2">
          <Input
            id={`${id}-token`}
            className="min-w-0 flex-1 basis-64 font-mono"
            autoComplete="off"
            spellCheck={false}
            value={token}
            aria-invalid={error ? true : undefined}
            aria-describedby={describedBy}
            onChange={(event) => {
              setToken(event.target.value)
              setError(null)
            }}
          />
          <Button
            type="submit"
            disabled={!isDesktop() || busy || token.trim() === ""}
          >
            <KeyRound data-icon="inline-start" />
            {busy
              ? t("settings.license.activating")
              : t("settings.license.activate")}
          </Button>
        </div>
        {error && (
          <p
            id={`${id}-error`}
            role="alert"
            className="text-xs text-destructive"
          >
            {errorMessage(t, error.code, error.params)}
          </p>
        )}
        <p id={`${id}-hint`} className="text-xs text-muted-foreground">
          {t("settings.license.input.hint")}
        </p>
        {saved && (
          <p role="status" className="text-sm">
            {saved.source === "pending"
              ? t("settings.license.savedPending")
              : t("settings.license.activated")}
          </p>
        )}
      </form>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <PurchaseButton />
        {stored && (
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => setConfirming(true)}
          >
            <Trash2 data-icon="inline-start" />
            {t("settings.license.remove.button")}
          </Button>
        )}
      </div>
      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title={t("settings.license.remove.title")}
        description={t("settings.license.remove.body")}
        confirmLabel={t("settings.license.remove.confirm")}
        cancelLabel={t("common.cancel")}
        destructive
        onConfirm={() => void remove()}
      />
    </Section>
  )
}

const SEND_MODES: readonly SendMode[] = ["video", "document", "photo"]

/**
 * Watched folders, saved at once through `watch_*` (outside the settings form). Without the
 * feature they stay listed and can be removed, but none is watched and none can be added.
 */
function WatchedFolders({ entitlements }: { entitlements: EntitlementState }) {
  const { t } = useTranslation()
  const { pushError } = useAlerts()
  const destinations = useDestinations()
  const ids = useId()
  const [folders, setFolders] = useState<WatchFolder[] | null>(null)
  const [path, setPath] = useState<string | null>(null)
  const [destinationId, setDestinationId] = useState("")
  const [mode, setMode] = useState<SendMode | "default">("default")
  const [busy, setBusy] = useState(false)

  const granted =
    entitlements !== "failed" && grants(entitlements, "watched_folder")

  useEffect(() => {
    let stale = false

    watchList().then(
      (list) => {
        if (!stale) setFolders(list)
      },
      (cause: unknown) => {
        if (stale) return
        setFolders([])
        pushError(cause)
      },
    )

    return () => {
      stale = true
    }
  }, [pushError])

  function replace(folder: WatchFolder) {
    setFolders((current) => {
      const list = current ?? []

      return list.some((f) => f.id === folder.id)
        ? list.map((f) => (f.id === folder.id ? folder : f))
        : [...list, folder]
    })
  }

  async function run(action: () => Promise<void>) {
    setBusy(true)

    try {
      await action()
    } catch (cause) {
      pushError(cause)
    } finally {
      setBusy(false)
    }
  }

  async function choose() {
    const picked = await pickFolder()

    if (picked) setPath(picked)
  }

  function add() {
    if (path === null || destinationId === "") return
    void run(async () => {
      replace(
        await watchAdd(
          path,
          Number(destinationId),
          mode === "default" ? undefined : mode,
        ),
      )
      setPath(null)
    })
  }

  function describe(folder: WatchFolder) {
    const destination =
      folder.destination_id === null
        ? undefined
        : destinations.byId.get(folder.destination_id)

    return t("settings.watch.summary", {
      destination: destination
        ? destinationName(destination) || t("destinations.unnamed")
        : t("settings.watch.noDestination"),
      mode: folder.send_mode
        ? t(`mode.${folder.send_mode}`)
        : t("add.mode.default"),
    })
  }

  return (
    <Section title={t("settings.watch.title")} hint={t("settings.watch.hint")}>
      {!granted && (
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
          <Badge variant="secondary">{t("settings.advanced.premium")}</Badge>
          <p className="min-w-0 flex-1 basis-64 text-sm text-muted-foreground">
            {t("settings.watch.locked")}
          </p>
          <PurchaseButton />
        </div>
      )}

      {folders !== null && folders.length === 0 && (
        <p className="text-sm text-muted-foreground">
          {t("settings.watch.empty")}
        </p>
      )}
      {folders !== null && folders.length > 0 && (
        <ul className="space-y-2">
          {folders.map((folder) => (
            <li
              key={folder.id}
              className="flex flex-wrap items-center gap-3 rounded-lg border p-3"
            >
              <div className="min-w-0 flex-1 basis-56 space-y-1">
                <p className="truncate font-mono text-xs" title={folder.path}>
                  <bdi>{folder.path}</bdi>
                </p>
                <p className="text-xs text-muted-foreground">
                  {describe(folder)}
                </p>
                <p className="text-xs text-muted-foreground">
                  {!granted
                    ? t("settings.watch.status.locked")
                    : folder.enabled
                      ? t("settings.watch.status.active")
                      : t("settings.watch.status.paused")}
                </p>
              </div>
              <Switch
                aria-label={t("settings.watch.toggle", { path: folder.path })}
                checked={folder.enabled}
                disabled={!granted || busy}
                onCheckedChange={(enabled) =>
                  void run(async () => {
                    replace(await watchUpdate(folder.id, { enabled }))
                  })
                }
              />
              <Button
                type="button"
                variant="ghost"
                size="icon-sm"
                aria-label={t("settings.watch.remove", { path: folder.path })}
                title={t("settings.watch.remove", { path: folder.path })}
                disabled={busy}
                onClick={() =>
                  void run(async () => {
                    await watchRemove(folder.id)
                    setFolders(
                      (current) =>
                        current?.filter((f) => f.id !== folder.id) ?? null,
                    )
                  })
                }
              >
                <Trash2 />
              </Button>
            </li>
          ))}
        </ul>
      )}

      {granted && (
        <div className="space-y-4 rounded-lg border border-dashed p-3">
          <FolderField
            id={`${ids}-folder`}
            label={t("settings.watch.folder")}
            value={path}
            defaultText={t("settings.watch.noFolder")}
            chooseText={t("settings.watch.choose")}
            resetText={t("common.cancel")}
            onChoose={() => void choose()}
            onReset={() => setPath(null)}
          />
          <DestinationField
            id={`${ids}-destination`}
            value={destinationId}
            onChange={setDestinationId}
          />
          <Field id={`${ids}-mode`} label={t("add.mode.label")}>
            {(props) => (
              <Choice
                {...props}
                value={mode}
                onChange={setMode}
                options={[
                  ["default", t("add.mode.default")] as const,
                  ...SEND_MODES.map(
                    (sendMode) => [sendMode, t(`mode.${sendMode}`)] as const,
                  ),
                ]}
              />
            )}
          </Field>
          <div className="flex justify-end">
            <Button
              type="button"
              disabled={
                !isDesktop() || busy || path === null || destinationId === ""
              }
              onClick={add}
            >
              <FolderOpen data-icon="inline-start" />
              {t("settings.watch.add")}
            </Button>
          </div>
        </div>
      )}
    </Section>
  )
}

/** Journal folder and debug report. */
function Diagnostics() {
  const { t } = useTranslation()
  const { pushError } = useAlerts()
  const [folder, setFolder] = useState<string | null>(null)
  const [exporting, setExporting] = useState(false)
  const [exported, setExported] = useState<string | null>(null)

  useEffect(() => {
    logPath().then(setFolder, () => setFolder(null))
  }, [])

  async function exportReport() {
    setExporting(true)
    setExported(null)

    try {
      const path = await debugExport()

      if (path) setExported(path)
    } catch (error) {
      pushError(error)
    } finally {
      setExporting(false)
    }
  }

  return (
    <Section
      title={t("settings.diagnostics.title")}
      hint={t("settings.diagnostics.hint")}
    >
      <div className="space-y-2">
        <p className="text-sm font-medium">
          {t("settings.diagnostics.folder")}
        </p>
        <p
          className="truncate rounded-lg border bg-muted/40 px-2.5 py-1.5 font-mono text-xs"
          title={folder ?? undefined}
        >
          {folder ? (
            <bdi>{folder}</bdi>
          ) : (
            <span className="font-sans text-muted-foreground">
              {t("settings.diagnostics.folderUnavailable")}
            </span>
          )}
        </p>
      </div>
      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={!isDesktop() || !folder}
          onClick={() => void logOpenDir().catch(pushError)}
        >
          <FolderOpen data-icon="inline-start" />
          {t("settings.diagnostics.openFolder")}
        </Button>
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={!isDesktop() || exporting}
          onClick={() => void exportReport()}
        >
          <Download data-icon="inline-start" />
          {exporting
            ? t("settings.diagnostics.exporting")
            : t("settings.diagnostics.export")}
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">
        {t("settings.diagnostics.exportHint")}
      </p>
      {exported && (
        <p role="status" className="text-xs text-muted-foreground">
          {t("settings.diagnostics.exported", { path: exported })}
        </p>
      )}
    </Section>
  )
}

const MIB = 1024 * 1024

/**
 * The upload limit the core applies now (`limit_status`) and where it comes from. Read again
 * when the saved limit settings or the Telegram session change.
 */
function LimitNow({ settings }: { settings: Settings["limit"] }) {
  const { t, i18n } = useTranslation()
  const session = useAuth().auth
  const signedIn = session?.state === "ready"
  const premium = session?.me?.isPremium
  const [status, setStatus] = useState<LimitStatus | "failed" | null>(null)
  const { custom_mb, premium: forced } = settings

  useEffect(() => {
    let stale = false
    limitStatus().then(
      (next) => {
        if (!stale) setStatus(next)
      },
      () => {
        if (!stale) setStatus("failed")
      },
    )

    return () => {
      stale = true
    }
  }, [custom_mb, forced, signedIn, premium])

  if (status === null) return null

  if (status === "failed")
    return (
      <p className="text-xs text-muted-foreground">
        {t("settings.limit.premium.currentUnavailable")}
      </p>
    )

  const source = {
    custom: t("settings.limit.premium.source.custom"),
    forced: status.premium
      ? t("settings.limit.premium.source.forcedPremium")
      : t("settings.limit.premium.source.forcedStandard"),
    account: status.premium
      ? t("settings.limit.premium.source.accountPremium")
      : t("settings.limit.premium.source.accountStandard"),
    default: t("settings.limit.premium.source.default"),
  }[status.source]

  return (
    <p className="text-xs text-muted-foreground" aria-live="polite">
      {t("settings.limit.premium.current", {
        mb: formatNumber(Math.round(status.limit_bytes / MIB), i18n.language),
        source,
      })}
    </p>
  )
}

function SettingsForm({
  settings,
  entitlements,
}: {
  settings: Settings
  entitlements: EntitlementState
}) {
  const { t } = useTranslation()
  const { save } = useSettings()
  const { alerts, pushError } = useAlerts()
  const ids = useId()
  const [draft, setDraft] = useState<Draft>(() => toDraft(settings))
  const [saving, setSaving] = useState(false)
  const [saved, setSaved] = useState(false)
  const [sidecars, setSidecars] = useState<SidecarStatus | null>(null)
  const [encoders, setEncoders] = useState<EncoderStatus | null>(null)

  const fallbacks = alerts.filter(
    (alert) => alert.code === "ENCODER_FALLBACK",
  ).length

  const errors = validate(draft)
  const valid = Object.keys(errors).length === 0
  const patch = valid ? toPatch(draft, settings) : {}

  const dirty = valid
    ? Object.keys(patch).length > 0
    : JSON.stringify(draft) !== JSON.stringify(toDraft(settings))

  const advancedGranted =
    entitlements !== "failed" && grants(entitlements, "advanced_encoding")

  // Without the feature the controls show what an encode uses, not what is saved.
  const encoding = effectiveEncoding(
    {
      max_resolution: draft.maxResolution,
      audio_kbps: Number(draft.audioKbps),
    },
    advancedGranted,
  )

  // Read again when a GPU stop is announced while this page is open.
  useEffect(() => {
    encoderStatus().then(setEncoders, () => setEncoders(null))
  }, [fallbacks])

  // Re-resolve ffmpeg whenever the saved custom folder changes.
  useEffect(() => {
    sidecarsStatus().then(setSidecars, () => setSidecars(null))
  }, [settings.ffmpeg.custom_dir])

  function update<K extends keyof Draft>(key: K, value: Draft[K]) {
    setSaved(false)
    setDraft((current) => ({ ...current, [key]: value }))
  }

  async function submit() {
    if (!valid || !dirty) return
    setSaving(true)

    try {
      const next = await save(patch)
      setDraft(toDraft(next))
      setSaved(true)
    } catch (error) {
      pushError(error)
    } finally {
      setSaving(false)
    }
  }

  async function chooseFolder(key: "tempDir" | "ffmpegDir") {
    const folder = await pickFolder()

    if (folder) update(key, folder)
  }

  const fieldError = (key: keyof DraftErrors) => {
    const message = errors[key]

    return message ? t(message) : undefined
  }

  return (
    <form
      noValidate
      className="space-y-6"
      onSubmit={(event) => {
        event.preventDefault()
        void submit()
      }}
    >
      <Section title={t("settings.limit.title")}>
        <Field
          id={`${ids}-premium`}
          label={t("settings.limit.premium.label")}
          hint={t("settings.limit.premium.hint")}
        >
          {(props) => (
            <Choice
              {...props}
              value={draft.premium}
              onChange={(value) => update("premium", value)}
              options={[
                ["auto", t("settings.limit.premium.auto")],
                ["yes", t("settings.limit.premium.yes")],
                ["no", t("settings.limit.premium.no")],
              ]}
            />
          )}
        </Field>
        <LimitNow settings={settings.limit} />
        <div className="grid gap-4 sm:grid-cols-2">
          <Field
            id={`${ids}-custom`}
            label={t("settings.limit.custom.label")}
            hint={t("settings.limit.custom.hint")}
            error={fieldError("customMb")}
          >
            {(props) => (
              <Input
                {...props}
                inputMode="numeric"
                value={draft.customMb}
                onChange={(event) => update("customMb", event.target.value)}
              />
            )}
          </Field>
          <Field
            id={`${ids}-margin`}
            label={t("settings.limit.margin.label")}
            hint={t("settings.limit.margin.hint")}
            error={fieldError("margin")}
          >
            {(props) => (
              <Input
                {...props}
                type="number"
                min={0}
                max={49.9}
                step={0.1}
                inputMode="decimal"
                value={draft.margin}
                onChange={(event) => update("margin", event.target.value)}
              />
            )}
          </Field>
        </div>
      </Section>

      <Section title={t("settings.encoding.title")}>
        <Field
          id={`${ids}-prefer`}
          label={t("settings.encoding.prefer.label")}
          hint={t("settings.encoding.prefer.hint")}
        >
          {(props) => (
            <Choice
              {...props}
              value={draft.prefer}
              onChange={(value) => update("prefer", value)}
              options={[
                ["gpu", t("settings.encoding.prefer.gpu")],
                ["cpu", t("settings.encoding.prefer.cpu")],
              ]}
            />
          )}
        </Field>
        {encoders?.gpu_failed && (
          <p
            role="status"
            className="rounded-lg border border-warning/30 bg-warning/5 p-3 text-sm"
          >
            {t("settings.encoding.prefer.gpuFailed", {
              encoder: encoders.gpu_failed.encoder,
            })}
          </p>
        )}
      </Section>

      <Section title={t("settings.advanced.title")}>
        {!advancedGranted && (
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
            <Badge variant="secondary">{t("settings.advanced.premium")}</Badge>
            <p className="min-w-0 flex-1 basis-64 text-sm text-muted-foreground">
              {t("settings.advanced.locked")}
            </p>
            <PurchaseButton />
          </div>
        )}
        <div className="grid gap-4 sm:grid-cols-2">
          <Field
            id={`${ids}-resolution`}
            label={t("settings.advanced.resolution.label")}
            hint={t("settings.advanced.resolution.hint")}
          >
            {(props) => (
              <Choice
                {...props}
                disabled={!advancedGranted}
                value={encoding.max_resolution}
                onChange={(value) => update("maxResolution", value)}
                options={MAX_RESOLUTIONS.map(
                  (resolution) =>
                    [
                      resolution,
                      t(`settings.advanced.resolution.${resolution}`),
                    ] as const,
                )}
              />
            )}
          </Field>
          <Field
            id={`${ids}-audio`}
            label={t("settings.advanced.audio.label")}
            hint={t("settings.advanced.audio.hint")}
          >
            {(props) => (
              <Choice
                {...props}
                disabled={!advancedGranted}
                value={encoding.audio_kbps.toString()}
                onChange={(value) => update("audioKbps", value)}
                options={AUDIO_KBPS_CHOICES.map(
                  (kbps) =>
                    [
                      kbps.toString(),
                      kbps === AUTOMATIC_ENCODING.audio_kbps
                        ? t("settings.advanced.audio.automatic", { kbps })
                        : t("settings.advanced.audio.value", { kbps }),
                    ] as const,
                )}
              />
            )}
          </Field>
        </div>
      </Section>

      <Section title={t("settings.send.title")} hint={t("settings.send.hint")}>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field
            id={`${ids}-pause-min`}
            label={t("settings.send.min")}
            error={fieldError("pauseMin")}
          >
            {(props) => (
              <Input
                {...props}
                type="number"
                min={0}
                step={1}
                inputMode="numeric"
                value={draft.pauseMin}
                onChange={(event) => update("pauseMin", event.target.value)}
              />
            )}
          </Field>
          <Field
            id={`${ids}-pause-max`}
            label={t("settings.send.max")}
            error={fieldError("pauseMax")}
          >
            {(props) => (
              <Input
                {...props}
                type="number"
                min={0}
                step={1}
                inputMode="numeric"
                value={draft.pauseMax}
                onChange={(event) => update("pauseMax", event.target.value)}
              />
            )}
          </Field>
        </div>
      </Section>

      <Section title={t("settings.temp.title")}>
        <FolderField
          id={`${ids}-temp`}
          label={t("settings.temp.dir")}
          value={draft.tempDir}
          defaultText={t("settings.temp.default")}
          chooseText={t("settings.temp.choose")}
          resetText={t("settings.temp.useDefault")}
          onChoose={() => void chooseFolder("tempDir")}
          onReset={() => update("tempDir", null)}
        />
        <Field
          id={`${ids}-min-free`}
          label={t("settings.temp.minFree")}
          hint={t("settings.temp.minFreeHint")}
          error={fieldError("minFreeGb")}
        >
          {(props) => (
            <Input
              {...props}
              type="number"
              min={0}
              step={1}
              inputMode="numeric"
              className="sm:w-40"
              value={draft.minFreeGb}
              onChange={(event) => update("minFreeGb", event.target.value)}
            />
          )}
        </Field>
      </Section>

      <Section title={t("settings.ffmpeg.title")}>
        <FolderField
          id={`${ids}-ffmpeg`}
          label={t("settings.ffmpeg.dir")}
          hint={t("settings.ffmpeg.hint")}
          value={draft.ffmpegDir}
          defaultText={t("settings.ffmpeg.default")}
          chooseText={t("settings.ffmpeg.choose")}
          resetText={t("settings.ffmpeg.useDefault")}
          onChoose={() => void chooseFolder("ffmpegDir")}
          onReset={() => update("ffmpegDir", null)}
        />
        <FfmpegFinder
          onUsed={(dir, status) => {
            // Already recorded by the core: the draft follows, so the form isn't dirty.
            setDraft((current) => ({ ...current, ffmpegDir: dir }))
            setSidecars(status)
          }}
        />
        {sidecars && (
          <p className="text-xs text-muted-foreground">
            {sidecars.ffmpeg_path ? (
              t("settings.ffmpeg.inUse", { path: sidecars.ffmpeg_path })
            ) : (
              <span className="text-destructive">
                {t("settings.ffmpeg.notFound")}
              </span>
            )}
          </p>
        )}
      </Section>

      <Section title={t("settings.appearance.title")}>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field
            id={`${ids}-theme`}
            label={t("settings.appearance.theme.label")}
          >
            {(props) => (
              <Choice
                {...props}
                value={draft.theme}
                onChange={(value) => update("theme", value)}
                options={[
                  ["dark", t("settings.appearance.theme.dark")],
                  ["light", t("settings.appearance.theme.light")],
                  ["system", t("settings.appearance.theme.system")],
                ]}
              />
            )}
          </Field>
          <Field
            id={`${ids}-language`}
            label={t("settings.appearance.language")}
          >
            {(props) => (
              <Choice
                {...props}
                value={draft.language}
                onChange={(value) => update("language", value)}
                options={availableLanguages().map(
                  ({ code, nativeName, beta }) =>
                    [
                      code,
                      beta
                        ? t("settings.appearance.languageBeta", {
                            name: nativeName,
                          })
                        : nativeName,
                    ] as const,
                )}
              />
            )}
          </Field>
        </div>
      </Section>

      <Section title={t("settings.updates.title")}>
        <UpdateStatus lastCheckAt={settings.updates.last_check_at} />
        <div className="flex items-center justify-between gap-4">
          <div className="min-w-0 space-y-1">
            <label
              htmlFor={`${ids}-auto-update`}
              className="block text-sm font-medium"
            >
              {t("settings.updates.automatic.label")}
            </label>
            <p
              id={`${ids}-auto-update-hint`}
              className="text-xs text-muted-foreground"
            >
              {t("settings.updates.automatic.hint")}
            </p>
          </div>
          <Switch
            id={`${ids}-auto-update`}
            aria-describedby={`${ids}-auto-update-hint`}
            checked={draft.checkAutomatically}
            onCheckedChange={(checked) => update("checkAutomatically", checked)}
          />
        </div>
      </Section>

      <Diagnostics />

      <div className="sticky bottom-0 -mx-1 flex flex-wrap items-center justify-end gap-3 border-t bg-background/95 px-1 py-3 backdrop-blur">
        <p
          role="status"
          className="me-auto text-sm text-muted-foreground empty:hidden"
        >
          {!valid
            ? t("settings.invalid")
            : dirty
              ? t("settings.unsaved")
              : saved
                ? t("settings.saved")
                : ""}
        </p>
        <Button
          type="button"
          variant="outline"
          disabled={!dirty || saving}
          onClick={() => {
            setDraft(toDraft(settings))
            setSaved(false)
          }}
        >
          <RotateCcw data-icon="inline-start" />
          {t("settings.discard")}
        </Button>
        <Button type="submit" disabled={!dirty || !valid || saving}>
          <Save data-icon="inline-start" />
          {saving ? t("settings.saving") : t("settings.save")}
        </Button>
      </div>
    </form>
  )
}

/** Version in use, result of the last check, and "Check now" (which ignores the switch). */
function UpdateStatus({ lastCheckAt }: { lastCheckAt: string | null }) {
  const { t, i18n } = useTranslation()
  const { info, notice, checking, check } = useUpdate()

  const result = notice
    ? notice.available
      ? t("settings.updates.available", { version: notice.version })
      : t("settings.updates.upToDate")
    : null

  return (
    <div className="space-y-2">
      {info && (
        <>
          <p className="text-sm">
            {t("settings.updates.current", { version: info.current_version })}
          </p>
          <p className="text-xs text-muted-foreground">
            {t(`settings.updates.kind.${info.kind}`)}
          </p>
        </>
      )}
      <div className="flex flex-wrap items-center gap-3">
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={!isDesktop() || checking}
          onClick={() => void check()}
        >
          <RefreshCw data-icon="inline-start" />
          {checking
            ? t("settings.updates.checking")
            : t("settings.updates.checkNow")}
        </Button>
        {result && (
          <p role="status" className="text-sm">
            {result}
          </p>
        )}
      </div>
      <p className="text-xs text-muted-foreground">
        {lastCheckAt
          ? t("settings.updates.lastChecked", {
              date: formatDateTime(new Date(lastCheckAt), i18n.language),
            })
          : t("settings.updates.neverChecked")}
      </p>
    </div>
  )
}

function Section({
  title,
  hint,
  children,
}: {
  title: string
  hint?: string
  children: ReactNode
}) {
  const id = useId()

  return (
    <Card role="group" aria-labelledby={id}>
      <CardHeader>
        <CardTitle>
          <h2 id={id}>{title}</h2>
        </CardTitle>
        {hint && <p className="text-sm text-muted-foreground">{hint}</p>}
      </CardHeader>
      <CardContent className="space-y-4">{children}</CardContent>
    </Card>
  )
}

interface ControlProps {
  id: string
  "aria-describedby"?: string
  "aria-invalid"?: boolean
}

function Field({
  id,
  label,
  hint,
  error,
  children,
}: {
  id: string
  label: string
  hint?: string
  error?: string
  children: (props: ControlProps) => ReactNode
}) {
  const describedBy =
    [error && `${id}-error`, hint && `${id}-hint`].filter(Boolean).join(" ") ||
    undefined

  return (
    <div className="space-y-2">
      <label htmlFor={id} className="block text-sm font-medium">
        {label}
      </label>
      {children({
        id,
        "aria-describedby": describedBy,
        "aria-invalid": error ? true : undefined,
      })}
      <AnimatePresence initial={false}>
        {error && (
          <motion.p
            id={`${id}-error`}
            initial={{ opacity: 0, height: 0 }}
            animate={{ opacity: 1, height: "auto" }}
            exit={{ opacity: 0, height: 0 }}
            className="text-xs text-destructive"
          >
            {error}
          </motion.p>
        )}
      </AnimatePresence>
      {hint && (
        <p id={`${id}-hint`} className="text-xs text-muted-foreground">
          {hint}
        </p>
      )}
    </div>
  )
}

function Choice<T extends string>({
  value,
  onChange,
  options,
  disabled = false,
  ...props
}: ControlProps & {
  value: T
  onChange: (value: T) => void
  options: readonly (readonly [T, string])[]
  disabled?: boolean
}) {
  return (
    <Select
      value={value}
      disabled={disabled}
      onValueChange={(next) => {
        const chosen = options.find(([option]) => option === next)

        if (chosen) onChange(chosen[0])
      }}
    >
      <SelectTrigger className="w-full" {...props}>
        <SelectValue />
      </SelectTrigger>
      <SelectContent position="popper">
        {options.map(([option, label]) => (
          <SelectItem key={option} value={option}>
            {label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
}

function FolderField({
  id,
  label,
  hint,
  value,
  defaultText,
  chooseText,
  resetText,
  onChoose,
  onReset,
}: {
  id: string
  label: string
  hint?: string
  value: string | null
  defaultText: string
  chooseText: string
  resetText: string
  onChoose: () => void
  onReset: () => void
}) {
  return (
    <div className="space-y-2" role="group" aria-labelledby={`${id}-label`}>
      <p id={`${id}-label`} className="text-sm font-medium">
        {label}
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <p
          className="min-w-0 flex-1 basis-56 truncate rounded-lg border bg-muted/40 px-2.5 py-1.5 font-mono text-xs"
          title={value ?? undefined}
          aria-describedby={hint ? `${id}-hint` : undefined}
        >
          {value ? (
            <bdi>{value}</bdi>
          ) : (
            <span className="font-sans text-muted-foreground">
              {defaultText}
            </span>
          )}
        </p>
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={!isDesktop()}
          onClick={onChoose}
        >
          <FolderOpen data-icon="inline-start" />
          {chooseText}
        </Button>
        {value && (
          <Button type="button" variant="ghost" size="sm" onClick={onReset}>
            {resetText}
          </Button>
        )}
      </div>
      {hint && (
        <p id={`${id}-hint`} className="text-xs text-muted-foreground">
          {hint}
        </p>
      )}
    </div>
  )
}
