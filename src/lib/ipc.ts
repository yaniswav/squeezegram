// The only module that talks to Tauri. Types mirror the serde structs of src-tauri/src
// (field names exactly as serialized); every command error becomes an `IpcError` carrying a
// stable code that the UI translates. Command results and event
// payloads are decoded against these types (decode.ts) before the UI sees them.

import { invoke, isTauri, type InvokeArgs } from "@tauri-apps/api/core"
import { listen } from "@tauri-apps/api/event"
import { getCurrentWebview } from "@tauri-apps/api/webview"
import { getCurrentWindow } from "@tauri-apps/api/window"
import { open } from "@tauri-apps/plugin-dialog"
import type { DeliveryRoute as SendVia } from "./delivery"
// `d.string`, `d.object`…: the decoders, namespaced so they read apart from the types.
import * as d from "./decode"
import {
  isJson,
  isJsonObject,
  isJsonString,
  type Json,
  type JsonObject,
} from "./json"
import {
  encodePresetDecoder,
  encodingOverrideDecoder,
  maxResolutionDecoder,
  videoCodecDecoder,
  type AdvancedEncoding,
  type CodecSupport,
  type EncodeEstimate,
  type EncodingOverride,
} from "./advanced-encoding"
import {
  entitlementStatusDecoder,
  type EntitlementStatus,
} from "./entitlements"
import type { ClearScope } from "./queue-clear"
import { subscription, type Subscription } from "./subscription"
import {
  updateInfoDecoder,
  updateNoticeDecoder,
  updateProgressDecoder,
  type UpdateInfo,
  type UpdateNotice,
  type UpdateProgress,
} from "./update"

export type {
  AdvancedEncoding,
  CodecSupport,
  EncodeEstimate,
  EncodePreset,
  EncodingOverride,
  MaxResolution,
  VideoCodec,
} from "./advanced-encoding"

export type {
  EntitlementPlan,
  EntitlementSource,
  EntitlementStatus,
  Feature,
  LicenseCode,
} from "./entitlements"

export type {
  UpdateInfo,
  UpdateKind,
  UpdateNotice,
  UpdatePhase,
  UpdateProgress,
} from "./update"

// --- queue (src-tauri/src/queue/state.rs, store.rs) ---

/** `State`, serialized snake_case. */
export type ItemState =
  | "pending_review"
  | "ready"
  | "probing"
  | "encoding"
  | "remuxing"
  | "encoded"
  | "uploading"
  | "waiting_server"
  | "sent"
  | "failed"
  | "unknown"
  | "cancelled"

export type SendMode = "video" | "document" | "photo"

export type { DeliveryRoute as SendVia } from "./delivery"

/** `items.hold_reason`: "duplicate" | "watch_pending"; free text in SQLite. */
export type HoldReason = string

/** `Item` (no serde rename: snake_case fields). */
export interface Item {
  id: number
  position: number
  source_path: string
  source_size: number
  source_mtime: number
  content_id: string | null
  identity_weak: boolean
  destination_id: number
  send_mode: SendMode
  caption: string
  /**
   * 1-based position of the file in the `queue_add` call that queued it, among the items of
   * the same destination, and the number of those items. 1 and 1 for a watched file.
   */
  batch_index: number
  batch_count: number
  state: ItemState
  hold_reason: HoldReason | null
  origin: string
  media_json: string | null
  plan_json: string | null
  temp_path: string | null
  output_path: string | null
  output_size: number | null
  progress_phase: string | null
  progress_pct: number | null
  attempts: number
  /** Error code of the last failure (`FFMPEG_FAILED`, `TG_FLOOD_WAIT`…). */
  last_error: string | null
  next_run_at: string | null
  tg_message_id: number | null
  tg_sent_at: string | null
  created_at: string
  updated_at: string
  /** Encoding choices of this file (Premium); null: the global settings apply. */
  encoding_override: EncodingOverride | null
  source_item_id: number | null
  send_via: SendVia
}

/** `ItemPatch`: only for items not started yet. */
export interface ItemPatch {
  caption?: string
  send_mode?: SendMode
}

/** `ItemDelta`, entry of `queue://changed`. */
export interface ItemDelta {
  id: number
  state: ItemState
  hold_reason: HoldReason | null
  position: number
}

/** `DestinationPatch`: `alias: null` (or blank) removes the local name; absent = unchanged. */
export interface DestinationPatch {
  alias?: string | null
  favorite?: boolean
  min_interval_s?: number
  default_mode?: SendMode
}

/** `ChatFolder` (transport/mod.rs): a Telegram chat folder; the name is for display only. */
export interface ChatFolder {
  id: number
  name: string
  /** Rank in the user's folder list (0 first). */
  position: number
}

/** `Destination` (store.rs). */
export interface Destination {
  id: number
  peer_kind: string
  peer_id: number
  /** 0 = whole chat; for a forum, the row its topics are listed under. */
  topic_id: number
  is_forum: boolean
  is_broadcast: boolean
  is_saved: boolean
  title: string
  alias: string | null
  favorite: boolean
  min_interval_s: number
  default_mode: SendMode
  refreshed_at: string | null
  /** `saved` | `private` | `basic_group` | `supergroup` | `channel` (schema v2). */
  kind: string
  /** In the Telegram archive at the last refresh. */
  archived: boolean
  folders: ChatFolder[]
  /** Approximate right to post media; `null` = unknown (`destination_check` decides). */
  can_write: boolean | null
  /** In a chat list at the last refresh; false = found by search only, or gone. */
  listed: boolean
  /** Closed forum topic: not selectable. */
  topic_closed: boolean
  /** Last time files were queued for it (`queue_add`), ISO 8601 UTC; null if never. */
  last_used_at: string | null
  /** Number of `queue_add` calls that chose it. */
  use_count: number
}

/** `WatchFolder` (store.rs): its new files are queued in `pending_review`, never sent alone. */
export interface WatchFolder {
  id: number
  path: string
  /** Set by `watch_add`; null only in a row written by hand. */
  destination_id: number | null
  enabled: boolean
  /** null = the destination's default mode when a file is found. */
  send_mode: SendMode | null
  /** The watcher's last listing of it failed (missing, renamed, offline share). */
  unreadable: boolean
}

/** `WatchFolderPatch`: absent = unchanged; `send_mode: null` = the destination's default. */
export interface WatchFolderPatch {
  enabled?: boolean
  destination_id?: number
  send_mode?: SendMode | null
}

export type HistoryStatus = "sent" | "failed" | "unknown"

/** `HistoryEntry`. */
export interface HistoryEntry {
  id: number
  content_id: string
  source_path: string
  file_name: string
  destination_id: number
  size_sent: number
  mode: string
  status: string
  duration_s: number | null
  encoded: boolean
  message_id: number | null
  sent_at: string
  imported_from: string | null
  via: SendVia
}

export type { ClearScope }

/** `QueueCleared`: result of `queue_clear`. */
export interface QueueCleared {
  removed: number
  /** Items left in the queue because they are being worked on. */
  running: number
}

/** `HistoryFilter` (all optional; limit 1..1000, default 100). */
export interface HistoryFilter {
  destination_id?: number
  status?: HistoryStatus
  content_id?: string
  /** Inclusive lower bound on `sent_at`, ISO 8601 UTC. */
  since?: string
  limit?: number
  offset?: number
}

// --- media (src-tauri/src/media/plan.rs, probe.rs), stored as JSON text in the item ---

/** `plan::Kind`. */
/** `strip`: sent as it is, from a copy without its location and camera data. */
export type PlanKind =
  "encode" | "remux" | "remux_audio" | "skip" | "photo" | "strip"

/** The fields of `plan::Plan` the UI reads. */
export interface Plan {
  kind: PlanKind
  limit_bytes: number
  duration_s: number | null
  encoder: string | null
  output_name: string
}

/** The fields of `probe::MediaInfo` the UI reads. */
export interface MediaInfo {
  duration_s: number | null
  video: {
    codec: string
    width: number
    height: number
    rotation: number
  } | null
}

const planDecoder = d.object<Plan>({
  kind: d.literals<PlanKind>({
    encode: true,
    remux: true,
    remux_audio: true,
    skip: true,
    photo: true,
    strip: true,
  }),
  limit_bytes: d.number,
  duration_s: d.nullable(d.number),
  encoder: d.nullable(d.string),
  output_name: d.string,
})

const mediaInfoDecoder = d.object<MediaInfo>({
  duration_s: d.nullable(d.number),
  video: d.nullable(
    d.object<NonNullable<MediaInfo["video"]>>({
      codec: d.string,
      width: d.number,
      height: d.number,
      rotation: d.number,
    }),
  ),
})

/** JSON text stored in the item; null when absent, unreadable or not of the expected form. */
function parseStored<T>(text: string | null, decode: d.Decoder<T>): T | null {
  if (!text) return null

  try {
    return decode(JSON.parse(text), "$")
  } catch {
    return null
  }
}

export function itemPlan(item: Item): Plan | null {
  return parseStored(item.plan_json, planDecoder)
}

export function itemMedia(item: Item): MediaInfo | null {
  return parseStored(item.media_json, mediaInfoDecoder)
}

// --- settings (src-tauri/src/settings.rs, settings.v1) ---

export type ThemeSetting = "light" | "dark" | "system"

export interface Settings {
  version: number
  limit: {
    /** null: deduced from the account after sign-in; true/false: forced. */
    premium: boolean | null
    custom_mb: number | null
    margin_percent: number
  }
  encoding: {
    mode: string
    /** "gpu" | "cpu" (stage.rs only tests for "cpu"). */
    prefer: string
    /** Not read by the core: see `advanced_encoding.audio_kbps`. */
    audio_kbps: number
    video_codec: string
  }
  send: {
    pause_min_s: number
    pause_max_s: number
    watchdog_s: number
    /** Prefills the caption of the add panel; empty: no caption. */
    caption_template: string
  }
  telegram: {
    api_override: { api_id: number } | null
    device_name: string | null
    /** The warning about sending to groups and channels was accepted (F33). */
    group_risk_acknowledged: boolean
  }
  /** `dir` only changes through `tempDirChoose` (or back to `null` with `settingsSet`). */
  temp: { dir: string | null; min_free_gb: number }
  /** `custom_dir` only changes through `sidecarsUse` / `sidecarsChoose` (or back to `null`). */
  ffmpeg: { custom_dir: string | null }
  ui: { theme: string; language: string }
  watch: { stable_seconds: number }
  /** Journal level: null = info in a release, debug in a dev build. */
  logging: { level: "error" | "warn" | "info" | "debug" | "trace" | null }
  updates: {
    /** Check at startup, at most once per 24 hours. */
    check_automatically: boolean
    /** ISO 8601 UTC time of the last check, successful or not; null if never. */
    last_check_at: string | null
  }
  /** Used by an encode only while the license grants `advanced_encoding`. */
  advanced_encoding: AdvancedEncoding
  /** `background`: takes effect only while the license grants the feature. */
  background: { close_action: CloseAction }
  /** Changed only through `proxySet`: `settings_set` refuses this section. */
  network: { proxy: ProxySettings | null }
  /** Remove location and camera data from files sent without re-encoding. */
  privacy: { strip_metadata: boolean }
}

/** `transport::proxy::ProxyKind`. */
export type ProxyKind = "socks5" | "http" | "mtproto"

/** A proxy as `settings.json` keeps it: everything but its secret. */
export interface ProxySettings {
  kind: ProxyKind
  host: string
  port: number
  /** Empty for MTProto and for a proxy without authentication. */
  username: string
}

/** `proxy_get` / `proxy_set`: the secret never comes back, only whether one is stored. */
export interface ProxyView {
  proxy: ProxySettings | null
  has_secret: boolean
}

/** What `proxySet` and `proxyTest` take. `kind: null` is a direct connection. */
export interface ProxyInput {
  kind: ProxyKind | null
  host: string
  port: number
  username: string
  /** SOCKS5/HTTP password or MTProto secret; null keeps the stored one (same kind only). */
  secret: string | null
}

/** Payload of `app://quit-requested`. */
export interface QuitRequest {
  /** Items being worked on (compressed or sent) that a quit would interrupt. */
  running: number
}

/** What closing the window does: `quit` the app, or `tray` (keep running hidden). */
export type CloseAction = "quit" | "tray"

/** JSON merge patch (RFC 7396): objects merge, `null` resets a field to its default. */
export type SettingsPatch = {
  [Section in keyof Settings]?: Settings[Section] extends object
    ? { [Key in keyof Settings[Section]]?: Settings[Section][Key] | null }
    : Settings[Section]
} & {
  // The core refuses a path for these two (`tempDirChoose`, `sidecarsUse`, `sidecarsChoose`
  // set them): only a reset to the default goes through `settings_set`.
  temp?: { dir?: null }
  ffmpeg?: { custom_dir?: null }
}

// --- sidecars (src-tauri/src/sidecars/mod.rs) ---

export interface SidecarStatus {
  installed: boolean
  source: "custom" | "installed" | null
  ffmpeg_path: string | null
  ffprobe_path: string | null
  pinned_tag: string
  pinned_size: number
  installing: boolean
}

/** `encoder_status` (src-tauri/src/commands.rs). */
export interface EncoderStatus {
  /** A GPU encoder that stopped mid-encode this session: the CPU is used until a restart. */
  gpu_failed: { encoder: string; at_seconds: number } | null
}

/** Where the effective upload limit comes from (`limit::LimitSource`). */
export type LimitSource = "custom" | "forced" | "account" | "default"

/** `limit_status`: the upload limit that applies now (before the safety margin). */
export interface LimitStatus {
  limit_bytes: number
  source: LimitSource
  /** Forced, else the signed-in account's; false when unknown. */
  premium: boolean
}

/** `discover::Candidate`: a working ffmpeg/ffprobe pair found on this machine (proposal only). */
export interface FfmpegCandidate {
  /** What `sidecarsUse` takes: the core keeps each proposed folder, the page sends no path. */
  id: number
  /** Where it is, to show. */
  dir: string
  ffmpeg_path: string
  ffprobe_path: string
  origin:
    | "winget"
    | "chocolatey"
    | "scoop"
    | "program_files"
    | "system_drive"
    | "path"
  version: string
  license: "gpl" | "lgpl" | "unknown"
  /** Those of h264_nvenc, libx264, libopenh264 and h264_mf the build offers. */
  encoders: string[]
  /** H.264 encoders that really encode with this binary (test encode). */
  working_encoders: string[]
  /** Deterministic score, higher is better. */
  score: number
  /** The best choice overall, compared with the download. */
  recommended: boolean
  reasons: RecommendReason[]
}

/** A translatable reason (`sidecars.finder.reason.<code>`). */
export interface RecommendReason {
  code:
    | "hw_nvenc"
    | "hw_qsv"
    | "hw_amf"
    | "cpu_x264"
    | "weak_fallback_only"
    | "no_working_encoder"
    | "license_lgpl"
    | "license_gpl"
    | "candidate_adds_x264"
    | "candidate_adds_hardware"
    | "download_no_candidate"
    | "download_no_gain"
    | "download_hardware_tested"
    | "download_hardware_assumed"
  params: JsonObject
}

/** `recommend::Recommendation`: download the pinned build, or use a found candidate. */
export interface FfmpegRecommendation {
  kind: "download" | "candidate"
  candidate_dir?: string
  reasons: RecommendReason[]
}

/** `discover::Discovery`: candidates sorted best first, and the global advice. */
export interface FfmpegDiscovery {
  candidates: FfmpegCandidate[]
  recommendation: FfmpegRecommendation
}

// --- events (src-tauri/src/queue/events.rs) ---

export interface QueueChanged {
  items: ItemDelta[]
  /** Ids deleted from the queue (`queue_clear`). */
  removed: number[]
}

/** `events::Phase`. */
export type Phase =
  | "probing"
  | "encoding"
  | "remuxing"
  | "uploading"
  | "awaiting_server"
  | "waiting_server"

/** `QueueProgress` (camelCase). */
export interface QueueProgress {
  itemId: number
  phase: Phase
  percent: number
  bytesPerSec: number | null
  etaSec: number | null
  retryAfterS?: number
  detail?: string
}

/** `queue://alert`. */
export interface QueueAlert {
  itemId?: number
  code: string
  params: JsonObject
}

/** `SidecarProgress` (camelCase). */
export interface SidecarProgress {
  name: string
  percent: number
  bytesPerSec: number | null
}

/** `QueueStatus`: result of `queue_status` and payload of `queue://status`. */
export interface QueueStatus {
  running: boolean
}

// --- Telegram session (src-tauri/src/transport/mod.rs, service.rs) ---

/** `AuthPayload.state` (snake_case names chosen in `service::auth_payload`). */
export type AuthStateName =
  | "starting"
  | "wait_phone"
  | "wait_qr"
  | "wait_code"
  | "wait_email"
  | "wait_email_code"
  | "wait_password"
  | "ready"
  | "logging_out"
  | "closed"
  | "start_failed"
  | "other_step"

/** `MePayload` (camelCase). */
export interface AuthMe {
  displayName: string
  username?: string
  isPremium: boolean
  captionMaxChars: number
  uploadLimitBytes: number
}

/**
 * `select::TransportKind`: `mock` is the deterministic test transport (nothing reaches
 * Telegram), `tdlib` the real one, `none` no Telegram at all (identity or tdjson.dll missing).
 */
export type TransportKind = "mock" | "tdlib" | "none"

/** `AuthPayload` (camelCase, absent fields omitted): result of `auth_state`, payload of `auth://state`. */
export interface AuthState {
  state: AuthStateName
  /** Transport chosen at startup; the same for the whole session. */
  transport: TransportKind
  qrLink?: string
  resendInS?: number
  passwordHint?: string
  me?: AuthMe
  otherStep?: string
  /** `start_failed`: why Telegram did not start, as a command error reports it. */
  error?: { code: string; params: JsonObject }
  /** `start_failed`: `auth_reset_session` may fix it (the local session can't be opened). */
  canResetSession?: boolean
}

/**
 * `identity_settings::TelegramIdentity`: what Settings shows of the identity the app presents
 * to Telegram. Never the personal `api_hash`, only whether one is stored.
 */
export interface TelegramIdentity {
  /** "squeezegram <machine>": the device name when `telegram.device_name` is empty. */
  default_device_name: string
  /** `telegram.api_override.api_id`; null: the product's identity. */
  api_id: number | null
  /** A personal `api_hash` is stored for this Windows user. */
  api_hash_saved: boolean
  /** The personal identity changed in this session: it applies after a restart. */
  restart_required: boolean
}

/** `AuthInput`: `{ kind, value }`, `value` absent for `request_qr` and `resend_code`. */
export type AuthInput =
  | {
      kind: "phone" | "code" | "email" | "email_code" | "password"
      value: string
    }
  | { kind: "request_qr" | "resend_code" }

/** `DestinationCheck` (service.rs). `code` is a `TG_*` code translated like an error. */
export interface DestinationCheck {
  allowed: boolean
  code: string | null
  params: JsonObject
  slow_mode_delay_s: number | null
  slow_mode_wait_s: number | null
  checked_at: string
}

/** `item_duplicate` (transport/service.rs): the earlier send of a held duplicate. */
export interface DuplicateInfo {
  sent_at: string
  destination_id: number
  message_id: number | null
  /** Always `https://t.me/…`; null in private chats, basic groups, Saved Messages. */
  link: string | null
}

/** `SentCandidate` (transport/mod.rs, snake_case fields). */
export interface SentCandidate {
  message_id: number
  date_unix: number
  kind: "video" | "photo" | "document"
  file_name: string | null
  size: number | null
  caption_preview: string
}

// --- errors ---

/** Codes produced by this module, never by Rust. */
export const FRONT_CODES = {
  /** The command is not registered in this build of the core. */
  commandUnavailable: "COMMAND_UNAVAILABLE",
  /** Running in a plain browser (vite dev), outside the desktop shell. */
  notInTauri: "NOT_IN_TAURI",
  /**
   * Anything that is not a `{ code, params }` error (bad arguments, IPC failure), or a result
   * that does not decode to its declared type.
   */
  ipcError: "IPC_ERROR",
} as const

export class IpcError extends Error {
  readonly code: string
  readonly params: JsonObject
  readonly command: string

  constructor(command: string, code: string, params: JsonObject = {}) {
    super(`${command}: ${code}`)
    this.name = "IpcError"
    this.command = command
    this.code = code
    this.params = params
  }
}

const MISSING_COMMAND = /^Command \S+ not found$/

/** `cause`: the rejection of `invoke`, or the `DecodeError` of its result. */
function toIpcError(command: string, cause: unknown): IpcError {
  if (isJson(cause)) {
    // `CommandError { code, params }` from commands.rs.
    if (isJsonObject(cause) && isJsonString(cause.code)) {
      return new IpcError(
        command,
        cause.code,
        d.params(cause.params, "$.params"),
      )
    }

    // Tauri's own rejection for a command that is not in `generate_handler!`.
    if (isJsonString(cause) && MISSING_COMMAND.test(cause)) {
      return new IpcError(command, FRONT_CODES.commandUnavailable, { command })
    }
  }

  console.error(`IPC ${command} failed`, cause)

  return new IpcError(command, FRONT_CODES.ipcError, { command })
}

export function isDesktop(): boolean {
  return isTauri()
}

/** Runs a command and decodes its result; any failure is thrown as an `IpcError`. */
async function call<T>(
  command: string,
  decode: d.Decoder<T>,
  args?: InvokeArgs,
): Promise<T> {
  if (!isTauri()) {
    throw new IpcError(command, FRONT_CODES.notInTauri, { command })
  }

  try {
    // The core answers with serde_json: the result is JSON until decoded.
    return decode(await invoke<Json>(command, args), "$")
  } catch (raw) {
    const error = toIpcError(command, raw)
    // Code and command only: arguments may hold what the user typed.
    logFrontend({
      level: "warn",
      event: "command_error",
      msg: `${command} failed: ${error.code}`,
      fields: { command, code: error.code },
    })
    throw error
  }
}

// --- journal (src-tauri/src/logging) ---

export type LogLevel = "error" | "warn" | "info" | "debug"

/** One UI line of the journal; `event` is a short snake_case code. */
export interface FrontendLog {
  level: LogLevel
  event: string
  msg: string
  fields?: JsonObject
}

/** Same budget as the core (logging/frontend.rs): the excess is counted, then reported. */
const LOG_WINDOW_MS = 10_000

const LOG_MAX_PER_WINDOW = 30

const LOG_TEXT_MAX = 2000

let logWindowStart = 0

let logCount = 0

let logDropped = 0

let logSending = false

function clip(text: string, max = LOG_TEXT_MAX): string {
  return text.length > max ? `${text.slice(0, max)}…` : text
}

function sendLog(entry: FrontendLog): void {
  // A console wrapper reached again while sending: drop it rather than loop.
  if (logSending) return
  logSending = true

  try {
    invoke("log_frontend", {
      level: entry.level,
      event: entry.event,
      msg: clip(entry.msg),
      fields: entry.fields ?? null,
    }).catch(() => {
      // The journal is best effort; reporting this failure would loop.
    })
  } finally {
    logSending = false
  }
}

/** Writes a line to the core's journal. Never throws, never writes to the console. */
export function logFrontend(entry: FrontendLog): void {
  if (!isTauri()) return
  const now = Date.now()

  if (now - logWindowStart >= LOG_WINDOW_MS) {
    const dropped = logDropped
    logWindowStart = now
    logCount = 0
    logDropped = 0

    if (dropped > 0)
      sendLog({
        level: "warn",
        event: "frontend_dropped",
        msg: "UI lines dropped by the rate limit",
        fields: { dropped },
      })
  }

  if (logCount >= LOG_MAX_PER_WINDOW) {
    logDropped += 1

    return
  }

  logCount += 1
  sendLog(entry)
}

interface ErrorDescription {
  msg: string
  fields: JsonObject
}

/** Message and fields of anything thrown. */
function describeError(cause: unknown): ErrorDescription {
  if (cause instanceof IpcError)
    return {
      msg: cause.message,
      fields: { code: cause.code, command: cause.command },
    }

  if (cause instanceof Error)
    return {
      msg: `${cause.name}: ${cause.message}`,
      fields: { stack: clip(cause.stack ?? "", 4000) },
    }

  return { msg: clip(String(cause)), fields: {} }
}

function isText(cause: unknown): cause is string {
  return typeof cause === "string"
}

/** One argument of `console.error`/`console.warn`, often the error that caused the call. */
function describeArg(cause: unknown): string {
  if (isText(cause)) return cause

  if (cause instanceof Error) return `${cause.name}: ${cause.message}`

  try {
    return clip(JSON.stringify(cause) ?? String(cause), 500)
  } catch {
    return String(cause)
  }
}

let loggingInstalled = false

/** Uncaught errors, rejected promises and `console.error`/`console.warn` go to the journal. */
export function installFrontendLogging(): void {
  if (loggingInstalled || !isTauri()) return
  loggingInstalled = true
  window.addEventListener("error", (event) => {
    logFrontend({
      level: "error",
      event: "js_error",
      msg: event.message || "Uncaught error",
      fields: {
        source: event.filename,
        line: event.lineno,
        column: event.colno,
        ...describeError(event.error).fields,
      },
    })
  })
  window.addEventListener("unhandledrejection", (event) => {
    const { msg, fields } = describeError(event.reason)
    logFrontend({ level: "error", event: "unhandled_rejection", msg, fields })
  })

  for (const level of ["error", "warn"] as const) {
    const original = console[level].bind(console)
    console[level] = (...args: unknown[]) => {
      original(...args)
      logFrontend({
        level,
        event: level === "error" ? "console_error" : "console_warn",
        msg: args.map(describeArg).join(" "),
      })
    }
  }
}

/** A React render error caught by the error boundary. */
export function logReactError(cause: unknown, componentStack?: string | null) {
  const { msg, fields } = describeError(cause)
  logFrontend({
    level: "error",
    event: "react_error",
    msg,
    fields: { ...fields, component_stack: clip(componentStack ?? "", 4000) },
  })
}

/** A change of screen (sidebar navigation). */
export function logScreen(from: string | null, to: string) {
  logFrontend({
    level: "info",
    event: "navigate",
    msg: `screen ${to}`,
    fields: { from, to },
  })
}

// --- decoders: one per result or payload type above, checked field by field ---

const sendModeDecoder = d.literals<SendMode>({
  video: true,
  document: true,
  photo: true,
})

const itemStateDecoder = d.literals<ItemState>({
  pending_review: true,
  ready: true,
  probing: true,
  encoding: true,
  remuxing: true,
  encoded: true,
  uploading: true,
  waiting_server: true,
  sent: true,
  failed: true,
  unknown: true,
  cancelled: true,
})

const itemDecoder = d.object<Item>({
  id: d.number,
  position: d.number,
  source_path: d.string,
  source_size: d.number,
  source_mtime: d.number,
  content_id: d.nullable(d.string),
  identity_weak: d.boolean,
  destination_id: d.number,
  send_mode: sendModeDecoder,
  caption: d.string,
  batch_index: d.number,
  batch_count: d.number,
  state: itemStateDecoder,
  hold_reason: d.nullable(d.string),
  origin: d.string,
  media_json: d.nullable(d.string),
  plan_json: d.nullable(d.string),
  temp_path: d.nullable(d.string),
  output_path: d.nullable(d.string),
  output_size: d.nullable(d.number),
  progress_phase: d.nullable(d.string),
  progress_pct: d.nullable(d.number),
  attempts: d.number,
  last_error: d.nullable(d.string),
  next_run_at: d.nullable(d.string),
  tg_message_id: d.nullable(d.number),
  tg_sent_at: d.nullable(d.string),
  created_at: d.string,
  updated_at: d.string,
  encoding_override: d.nullable(encodingOverrideDecoder),
  source_item_id: d.nullable(d.number),
  send_via: d.literals<SendVia>({ upload: true, copy: true, remote: true }),
})

const itemDeltaDecoder = d.object<ItemDelta>({
  id: d.number,
  state: itemStateDecoder,
  hold_reason: d.nullable(d.string),
  position: d.number,
})

const chatFolderDecoder = d.object<ChatFolder>({
  id: d.number,
  name: d.string,
  position: d.number,
})

const destinationDecoder = d.object<Destination>({
  id: d.number,
  peer_kind: d.string,
  peer_id: d.number,
  topic_id: d.number,
  is_forum: d.boolean,
  is_broadcast: d.boolean,
  is_saved: d.boolean,
  title: d.string,
  alias: d.nullable(d.string),
  favorite: d.boolean,
  min_interval_s: d.number,
  default_mode: sendModeDecoder,
  refreshed_at: d.nullable(d.string),
  kind: d.string,
  archived: d.boolean,
  folders: d.array(chatFolderDecoder),
  can_write: d.nullable(d.boolean),
  listed: d.boolean,
  topic_closed: d.boolean,
  last_used_at: d.nullable(d.string),
  use_count: d.number,
})

const watchFolderDecoder = d.object<WatchFolder>({
  id: d.number,
  path: d.string,
  destination_id: d.nullable(d.number),
  enabled: d.boolean,
  send_mode: d.nullable(sendModeDecoder),
  unreadable: d.boolean,
})

const historyEntryDecoder = d.object<HistoryEntry>({
  id: d.number,
  content_id: d.string,
  source_path: d.string,
  file_name: d.string,
  destination_id: d.number,
  size_sent: d.number,
  mode: d.string,
  status: d.string,
  duration_s: d.nullable(d.number),
  encoded: d.boolean,
  message_id: d.nullable(d.number),
  sent_at: d.string,
  imported_from: d.nullable(d.string),
  via: d.literals<SendVia>({ upload: true, copy: true, remote: true }),
})

const proxySettingsDecoder = d.object<ProxySettings>({
  kind: d.literals<ProxyKind>({ socks5: true, http: true, mtproto: true }),
  host: d.string,
  port: d.number,
  username: d.string,
})

type LogLevelSetting = NonNullable<Settings["logging"]["level"]>

const settingsDecoder = d.object<Settings>({
  version: d.number,
  limit: d.object<Settings["limit"]>({
    premium: d.nullable(d.boolean),
    custom_mb: d.nullable(d.number),
    margin_percent: d.number,
  }),
  encoding: d.object<Settings["encoding"]>({
    mode: d.string,
    prefer: d.string,
    audio_kbps: d.number,
    video_codec: d.string,
  }),
  send: d.object<Settings["send"]>({
    pause_min_s: d.number,
    pause_max_s: d.number,
    watchdog_s: d.number,
    caption_template: d.string,
  }),
  telegram: d.object<Settings["telegram"]>({
    api_override: d.nullable(
      d.object<NonNullable<Settings["telegram"]["api_override"]>>({
        api_id: d.number,
      }),
    ),
    device_name: d.nullable(d.string),
    group_risk_acknowledged: d.boolean,
  }),
  temp: d.object<Settings["temp"]>({
    dir: d.nullable(d.string),
    min_free_gb: d.number,
  }),
  ffmpeg: d.object<Settings["ffmpeg"]>({ custom_dir: d.nullable(d.string) }),
  ui: d.object<Settings["ui"]>({ theme: d.string, language: d.string }),
  watch: d.object<Settings["watch"]>({ stable_seconds: d.number }),
  logging: d.object<Settings["logging"]>({
    // `logging::LEVELS`; settings.rs refuses any other value.
    level: d.nullable(
      d.literals<LogLevelSetting>({
        error: true,
        warn: true,
        info: true,
        debug: true,
        trace: true,
      }),
    ),
  }),
  updates: d.object<Settings["updates"]>({
    check_automatically: d.boolean,
    last_check_at: d.nullable(d.string),
  }),
  advanced_encoding: d.object<Settings["advanced_encoding"]>({
    max_resolution: maxResolutionDecoder,
    audio_kbps: d.number,
    video_codec: videoCodecDecoder,
    preset: encodePresetDecoder,
    target_mb: d.nullable(d.number),
  }),
  background: d.object<Settings["background"]>({
    close_action: d.literals<CloseAction>({ quit: true, tray: true }),
  }),
  network: d.object<Settings["network"]>({
    proxy: d.nullable(proxySettingsDecoder),
  }),
  privacy: d.object<Settings["privacy"]>({ strip_metadata: d.boolean }),
})

const proxyViewDecoder = d.object<ProxyView>({
  proxy: d.nullable(proxySettingsDecoder),
  has_secret: d.boolean,
})

const sidecarStatusDecoder = d.object<SidecarStatus>({
  installed: d.boolean,
  source: d.nullable(
    d.literals<NonNullable<SidecarStatus["source"]>>({
      custom: true,
      installed: true,
    }),
  ),
  ffmpeg_path: d.nullable(d.string),
  ffprobe_path: d.nullable(d.string),
  pinned_tag: d.string,
  pinned_size: d.number,
  installing: d.boolean,
})

const encoderStatusDecoder = d.object<EncoderStatus>({
  gpu_failed: d.nullable(
    d.object<NonNullable<EncoderStatus["gpu_failed"]>>({
      encoder: d.string,
      at_seconds: d.number,
    }),
  ),
})

const encodeEstimateDecoder = d.object<EncodeEstimate>({
  outcome: d.literals<EncodeEstimate["outcome"]>({
    encode: true,
    as_is: true,
    photo: true,
    too_small: true,
    unknown: true,
  }),
  bytes: d.nullable(d.number),
  limit_bytes: d.number,
  codec: videoCodecDecoder,
  codec_fallback: d.boolean,
})

const codecSupportDecoder = d.object<CodecSupport>({
  hevc: d.boolean,
  hevc_encoder: d.nullable(d.string),
  reason: d.nullable(
    d.literals<NonNullable<CodecSupport["reason"]>>({
      no_hevc_encoder: true,
      ffmpeg_unavailable: true,
    }),
  ),
})

const limitStatusDecoder = d.object<LimitStatus>({
  limit_bytes: d.number,
  source: d.literals<LimitSource>({
    custom: true,
    forced: true,
    account: true,
    default: true,
  }),
  premium: d.boolean,
})

const recommendReasonDecoder = d.object<RecommendReason>({
  code: d.literals<RecommendReason["code"]>({
    hw_nvenc: true,
    hw_qsv: true,
    hw_amf: true,
    cpu_x264: true,
    weak_fallback_only: true,
    no_working_encoder: true,
    license_lgpl: true,
    license_gpl: true,
    candidate_adds_x264: true,
    candidate_adds_hardware: true,
    download_no_candidate: true,
    download_no_gain: true,
    download_hardware_tested: true,
    download_hardware_assumed: true,
  }),
  params: d.params,
})

const ffmpegDiscoveryDecoder = d.object<FfmpegDiscovery>({
  candidates: d.array(
    d.object<FfmpegCandidate>({
      id: d.number,
      dir: d.string,
      ffmpeg_path: d.string,
      ffprobe_path: d.string,
      origin: d.literals<FfmpegCandidate["origin"]>({
        winget: true,
        chocolatey: true,
        scoop: true,
        program_files: true,
        system_drive: true,
        path: true,
      }),
      version: d.string,
      license: d.literals<FfmpegCandidate["license"]>({
        gpl: true,
        lgpl: true,
        unknown: true,
      }),
      encoders: d.array(d.string),
      working_encoders: d.array(d.string),
      score: d.number,
      recommended: d.boolean,
      reasons: d.array(recommendReasonDecoder),
    }),
  ),
  recommendation: d.object<FfmpegRecommendation>({
    kind: d.literals<FfmpegRecommendation["kind"]>({
      download: true,
      candidate: true,
    }),
    candidate_dir: d.optional(d.string),
    reasons: d.array(recommendReasonDecoder),
  }),
})

const queueChangedDecoder = d.object<QueueChanged>({
  items: d.array(itemDeltaDecoder),
  removed: d.array(d.number),
})

const queueClearedDecoder = d.object<QueueCleared>({
  removed: d.number,
  running: d.number,
})

const queueProgressDecoder = d.object<QueueProgress>({
  itemId: d.number,
  phase: d.literals<Phase>({
    probing: true,
    encoding: true,
    remuxing: true,
    uploading: true,
    awaiting_server: true,
    waiting_server: true,
  }),
  percent: d.number,
  bytesPerSec: d.nullable(d.number),
  etaSec: d.nullable(d.number),
  retryAfterS: d.optional(d.number),
  detail: d.optional(d.string),
})

const queueAlertDecoder = d.object<QueueAlert>({
  itemId: d.optional(d.number),
  code: d.string,
  params: d.params,
})

const sidecarProgressDecoder = d.object<SidecarProgress>({
  name: d.string,
  percent: d.number,
  bytesPerSec: d.nullable(d.number),
})

const queueStatusDecoder = d.object<QueueStatus>({ running: d.boolean })

const authStateDecoder = d.object<AuthState>({
  state: d.literals<AuthStateName>({
    starting: true,
    wait_phone: true,
    wait_qr: true,
    wait_code: true,
    wait_email: true,
    wait_email_code: true,
    wait_password: true,
    ready: true,
    logging_out: true,
    closed: true,
    start_failed: true,
    other_step: true,
  }),
  transport: d.literals<TransportKind>({ mock: true, tdlib: true, none: true }),
  qrLink: d.optional(d.string),
  resendInS: d.optional(d.number),
  passwordHint: d.optional(d.string),
  me: d.optional(
    d.object<AuthMe>({
      displayName: d.string,
      username: d.optional(d.string),
      isPremium: d.boolean,
      captionMaxChars: d.number,
      uploadLimitBytes: d.number,
    }),
  ),
  otherStep: d.optional(d.string),
  error: d.optional(
    d.object<{ code: string; params: JsonObject }>({
      code: d.string,
      params: d.params,
    }),
  ),
  canResetSession: d.optional(d.boolean),
})

const destinationCheckDecoder = d.object<DestinationCheck>({
  allowed: d.boolean,
  code: d.nullable(d.string),
  params: d.params,
  slow_mode_delay_s: d.nullable(d.number),
  slow_mode_wait_s: d.nullable(d.number),
  checked_at: d.string,
})

const telegramIdentityDecoder = d.object<TelegramIdentity>({
  default_device_name: d.string,
  api_id: d.nullable(d.number),
  api_hash_saved: d.boolean,
  restart_required: d.boolean,
})

const sentCandidateDecoder = d.object<SentCandidate>({
  message_id: d.number,
  date_unix: d.number,
  kind: d.literals<SentCandidate["kind"]>({
    video: true,
    photo: true,
    document: true,
  }),
  file_name: d.nullable(d.string),
  size: d.nullable(d.number),
  caption_preview: d.string,
})

// --- commands (argument names: Tauri maps camelCase keys to the snake_case Rust parameters) ---

/**
 * One item per file and destination. More than one destination needs the `multi_destination`
 * feature: without it the core refuses with `FEATURE_NOT_LICENSED` and queues nothing.
 */
export function queueAdd(
  paths: string[],
  destinationIds: number[],
  sendMode?: SendMode,
  caption?: string,
  uploadOnce = false,
): Promise<number[]> {
  return call("queue_add", d.array(d.number), {
    paths,
    destinationIds,
    sendMode,
    caption,
    uploadOnce,
  })
}

/**
 * A caption template rendered for the first file of a batch of `count`, by the same function
 * as the send. Only what is known before encoding is real: see `src-tauri/src/caption.rs`.
 */
export function captionPreview(
  template: string,
  path: string | undefined,
  count: number,
): Promise<string> {
  return call("caption_preview", d.string, { template, path, count })
}

export function queueList(): Promise<Item[]> {
  return call("queue_list", d.array(itemDecoder))
}

/** `pending_review` → `ready`, all or nothing. */
export function queuePromote(itemIds: number[]): Promise<void> {
  return call("queue_promote", d.nothing, { itemIds })
}

export function queueStart(): Promise<void> {
  return call("queue_start", d.nothing)
}

/** Takes effect after the current item. */
export function queuePause(): Promise<void> {
  return call("queue_pause", d.nothing)
}

export function queueCancel(itemId: number): Promise<void> {
  return call("queue_cancel", d.nothing, { itemId })
}

/** Puts `itemIds` first, in that order; the other items keep their order after them. */
export function queueReorder(itemIds: number[]): Promise<void> {
  return call("queue_reorder", d.nothing, { itemIds })
}

export function itemUpdate(itemId: number, patch: ItemPatch): Promise<void> {
  return call("item_update", d.nothing, { itemId, patch })
}

/** User retry of a `failed` item: back to `encoded` if the prepared file is still there, else `ready`. */
/**
 * Premium: this file's encoding choices; `null` goes back to the global settings. Only while
 * the item has not started.
 */
export function itemEncodingSet(
  itemId: number,
  options: EncodingOverride | null,
): Promise<void> {
  return call("item_encoding_set", d.nothing, { itemId, options })
}

/** The output size of a queued file under `options` (`null`: the global settings). */
export function itemEncodingEstimate(
  itemId: number,
  options: EncodingOverride | null,
): Promise<EncodeEstimate> {
  return call("item_encoding_estimate", encodeEstimateDecoder, {
    itemId,
    options,
  })
}

/** Whether HEVC can be encoded here (a GPU encoder passed its test encode). */
export function codecSupport(): Promise<CodecSupport> {
  return call("codec_support", codecSupportDecoder)
}

export function queueRetry(itemId: number): Promise<ItemState> {
  return call("queue_retry", itemStateDecoder, { itemId })
}

/**
 * Deletes the items that are not being worked on, with their prepared and temporary files
 * (never the originals). `all` also removes waiting, prepared and unconfirmed items.
 */
export function queueClear(scope: ClearScope): Promise<QueueCleared> {
  return call("queue_clear", queueClearedDecoder, { scope })
}

/** Running or paused; every change is also emitted on `queue://status`. */
export function queueStatus(): Promise<QueueStatus> {
  return call("queue_status", queueStatusDecoder)
}

/** `mark_sent` (with the id of a candidate, or without) or `retry` of an `unknown` item. */
export function itemResolveUnknown(
  itemId: number,
  action: "mark_sent" | "retry",
  messageId?: number,
): Promise<void> {
  return call("item_resolve_unknown", d.nothing, { itemId, action, messageId })
}

/** Read only: messages of the destination that may be this `unknown` item. */
export function itemReconcile(itemId: number): Promise<SentCandidate[]> {
  return call("item_reconcile", d.array(sentCandidateDecoder), { itemId })
}

/** When and where a held duplicate was already sent; null when there is nothing to show. */
export function itemDuplicate(itemId: number): Promise<DuplicateInfo | null> {
  return call(
    "item_duplicate",
    d.nullable(
      d.object<DuplicateInfo>({
        sent_at: d.string,
        destination_id: d.number,
        message_id: d.nullable(d.number),
        link: d.nullable(d.string),
      }),
    ),
    { itemId },
  )
}

/** Opens a `https://t.me/` message link; the core refuses anything else (`LINK_REFUSED`). */
export function openTelegramLink(url: string): Promise<void> {
  return call("open_telegram_link", d.nothing, { url })
}

/** Destinations known locally (no network). */
export function destinationsList(): Promise<Destination[]> {
  return call("destinations_list", d.array(destinationDecoder))
}

/** Asks Telegram for the dialogs and topics, stores them and returns the whole list. */
export function destinationsRefresh(): Promise<Destination[]> {
  return call("destinations_refresh", d.array(destinationDecoder))
}

/**
 * Read only on Telegram: chats matching `query` by title or username, including chats in no
 * list (old chats, public chats). They are added to the cache and returned in Telegram's order.
 */
export function destinationsSearch(query: string): Promise<Destination[]> {
  return call("destinations_search", d.array(destinationDecoder), { query })
}

/** Topics of a forum (its `topic_id = 0` row), loaded from Telegram and cached. */
export function destinationTopics(
  destinationId: number,
): Promise<Destination[]> {
  return call("destination_topics", d.array(destinationDecoder), {
    destinationId,
  })
}

/** Local settings of a destination; returns it as stored. */
export function destinationUpdate(
  destinationId: number,
  patch: DestinationPatch,
): Promise<Destination> {
  return call("destination_update", destinationDecoder, {
    destinationId,
    patch,
  })
}

/** Before queueing: rights for this mode, slow mode, closed topic, paid messages. */
export function destinationCheck(
  destinationId: number,
  sendMode: SendMode,
): Promise<DestinationCheck> {
  return call("destination_check", destinationCheckDecoder, {
    destinationId,
    sendMode,
  })
}

/** Watched folders, kept whatever the plan (the core watches them only with the license). */
export function watchList(): Promise<WatchFolder[]> {
  return call("watch_list", d.array(watchFolderDecoder))
}

/** Watches an existing folder; the files already in it are left alone. */
export function watchAdd(
  path: string,
  destinationId: number,
  sendMode?: SendMode,
): Promise<WatchFolder> {
  return call("watch_add", watchFolderDecoder, {
    path,
    destinationId,
    sendMode,
  })
}

export function watchUpdate(
  folderId: number,
  patch: WatchFolderPatch,
): Promise<WatchFolder> {
  return call("watch_update", watchFolderDecoder, { folderId, patch })
}

/** Stops watching; the items already found stay in the queue. */
export function watchRemove(folderId: number): Promise<void> {
  return call("watch_remove", d.nothing, { folderId })
}

export function authState(): Promise<AuthState> {
  return call("auth_state", authStateDecoder)
}

export function authSubmit(input: AuthInput): Promise<void> {
  return call("auth_submit", d.nothing, { input })
}

export function authLogout(): Promise<void> {
  return call("auth_logout", d.nothing)
}

/**
 * After a failed start only: deletes the local Telegram session of the identity in use and
 * starts Telegram again (the user signs in anew). The queue, history and settings stay.
 * Rejects with `TG_SESSION_IN_USE` while a session runs.
 */
export function authResetSession(): Promise<void> {
  return call("auth_reset_session", d.nothing)
}

/** Default device name and personal API identity (never the stored `api_hash`). */
export function telegramIdentity(): Promise<TelegramIdentity> {
  return call("telegram_identity", telegramIdentityDecoder)
}

/**
 * Stores a personal API identity, used from the next start with its own Telegram session: the
 * `api_hash` is kept encrypted by the core and never returned. Rejects with
 * `TG_TRANSPORT_BUSY` while the transport starts or signs out, `SETTINGS_INVALID { field }`
 * for a malformed value.
 */
export function apiIdentitySet(
  apiId: string,
  apiHash: string,
): Promise<TelegramIdentity> {
  return call("api_identity_set", telegramIdentityDecoder, { apiId, apiHash })
}

/** Back to the product's identity (and its session) from the next start; same rule. */
export function apiIdentityClear(): Promise<TelegramIdentity> {
  return call("api_identity_clear", telegramIdentityDecoder)
}

export function settingsGet(): Promise<Settings> {
  return call("settings_get", settingsDecoder)
}

export function settingsSet(patch: SettingsPatch): Promise<Settings> {
  return call("settings_set", settingsDecoder, { patch })
}

export function proxyGet(): Promise<ProxyView> {
  return call("proxy_get", proxyViewDecoder)
}

/**
 * Saves the proxy (its secret protected by Windows, never sent back) and applies it to
 * Telegram at once. `PROXY_INVALID { field }` names the refused field.
 */
export function proxySet(input: ProxyInput): Promise<ProxyView> {
  return call("proxy_set", proxyViewDecoder, { input })
}

/** Round trip to Telegram through this proxy; the proxy in use is not changed. */
export function proxyTest(input: ProxyInput): Promise<{ latency_ms: number }> {
  return call(
    "proxy_test",
    d.object<{ latency_ms: number }>({ latency_ms: d.number }),
    { input },
  )
}

export function historyQuery(filters?: HistoryFilter): Promise<HistoryEntry[]> {
  return call("history_query", d.array(historyEntryDecoder), { filters })
}

/**
 * Deletes every history line and returns how many. A file already sent is no longer held
 * as a duplicate afterwards. The queue and the destinations are not touched.
 */
export function historyClear(): Promise<number> {
  return call("history_clear", d.number)
}

export function sidecarsStatus(): Promise<SidecarStatus> {
  return call("sidecars_status", sidecarStatusDecoder)
}

/** Whether a GPU encoder stopped mid-encode in this session. */
export function encoderStatus(): Promise<EncoderStatus> {
  return call("encoder_status", encoderStatusDecoder)
}

/**
 * The upload limit that applies now: custom, else forced Premium/standard, else the signed-in
 * account's, else 2000 MB.
 */
export function limitStatus(): Promise<LimitStatus> {
  return call("limit_status", limitStatusDecoder)
}

/** The plan in effect, the features granted, and why a stored license does not apply. */
export function entitlementsStatus(): Promise<EntitlementStatus> {
  return call("entitlements_status", entitlementStatusDecoder)
}

/**
 * Verifies the license offline and stores it. Rejects with a `LIC_*` code when it does not
 * verify or belongs to another signed-in Telegram account.
 */
export function licenseSet(token: string): Promise<EntitlementStatus> {
  return call("license_set", entitlementStatusDecoder, { token })
}

/** Removes the stored license: back to the free plan. */
export function licenseClear(): Promise<EntitlementStatus> {
  return call("license_clear", entitlementStatusDecoder)
}

/** Opens the purchase bot on Telegram in the browser (the core knows the address). */
export function licenseOpenPurchase(): Promise<void> {
  return call("license_open_purchase", d.nothing)
}

/** Long: downloads about 170 MB, progress on `sidecars://progress`. */
export function sidecarsInstall(): Promise<SidecarStatus> {
  return call("sidecars_install", sidecarStatusDecoder)
}

/** Looks for an ffmpeg already installed; only proposes, records nothing. */
export function sidecarsDiscover(): Promise<FfmpegDiscovery> {
  return call("sidecars_discover", ffmpegDiscoveryDecoder)
}

/**
 * Records the folder of a candidate of the last search as `ffmpeg.custom_dir` after
 * re-validating it. `SIDECAR_CHOICE_STALE` when a newer search replaced that candidate.
 */
export function sidecarsUse(candidateId: number): Promise<SidecarStatus> {
  return call("sidecars_use", sidecarStatusDecoder, {
    candidateId,
  })
}

/**
 * Opens the native folder dialog in the core and records the chosen ffmpeg folder like
 * `sidecarsUse`. Resolves to null when the dialog is closed.
 */
export function sidecarsChoose(): Promise<SidecarStatus | null> {
  return call("sidecars_choose", d.nullable(sidecarStatusDecoder))
}

/**
 * Opens the native folder dialog in the core and records the chosen temporary folder, after
 * the core checked it is a writable folder on a local drive (`TEMP_DIR_INVALID`). Resolves
 * to the new settings, or null when the dialog is closed.
 */
export function tempDirChoose(): Promise<Settings | null> {
  return call("temp_dir_choose", d.nullable(settingsDecoder))
}

/** Folder of the journal files (`%LOCALAPPDATA%\squeezegram\logs`). */
export function logPath(): Promise<string> {
  return call("log_path", d.string)
}

/** Quits the app (after the confirmation of `onQuitRequested`), through the clean shutdown. */
export function appQuit(): Promise<void> {
  return call("app_quit", d.nothing)
}

/** Whether the app starts with Windows (the startup entry exists). */
export function autostartStatus(): Promise<boolean> {
  return call("autostart_status", d.boolean)
}

/**
 * Turns "start with Windows" on (`FEATURE_NOT_LICENSED` without the license) or off (always
 * allowed). Resolves to whether it is on.
 */
export function autostartSet(enabled: boolean): Promise<boolean> {
  return call("autostart_set", d.boolean, { enabled })
}

/** Opens the journal folder in the file explorer. */
export function logOpenDir(): Promise<void> {
  return call("log_open_dir", d.nothing)
}

/**
 * Asks where to save, then writes a debug report (.zip: masked journal, versions, encoder
 * detection, error codes). Resolves to the saved path, or null if the dialog was closed.
 */
export function debugExport(): Promise<string | null> {
  return call("debug_export", d.nullable(d.string))
}

/** How this copy was installed, its version, and any update found earlier. No network. */
export function updateInfo(): Promise<UpdateInfo> {
  return call("update_info", updateInfoDecoder)
}

/** Asks GitHub for the latest release, whatever the automatic-check setting says. */
export function updateCheck(): Promise<UpdateNotice> {
  return call("update_check", updateNoticeDecoder)
}

/**
 * Installer build: downloads the update (progress on `app://update-progress`), checks its
 * signature, closes the app and runs the installer, which restarts it. It resolves only when
 * nothing newer exists any more; on success the app ends before that.
 */
export function updateInstall(): Promise<void> {
  return call("update_install", d.nothing)
}

/** Opens the page of the latest release in the browser (the core knows the address). */
export function updateOpenPage(): Promise<void> {
  return call("update_open_page", d.nothing)
}

// --- events ---

/**
 * Subscribes to a Tauri event. The result stops it synchronously, even before `listen` has
 * resolved (React effect cleanup), and its `ready` says when the core delivers the event: a
 * current value asked before that may miss a change (see `follow`). A payload that does not
 * decode is reported and dropped: the handler only sees well-formed events.
 */
function subscribe<T>(
  event: string,
  decode: d.Decoder<T>,
  handler: (payload: T) => void,
): Subscription {
  if (!isTauri()) return subscription(() => {}, Promise.resolve())

  let stopped = false
  let stop: (() => void) | undefined

  // The core emits serde_json: the payload is JSON until decoded.
  const ready = listen<Json>(event, ({ payload }) => {
    let decoded: T

    try {
      decoded = decode(payload, "$")
    } catch (cause) {
      console.error(`Dropped a malformed ${event} event`, cause)

      return
    }

    handler(decoded)
  }).then(
    (unlisten) => {
      if (stopped) unlisten()
      else stop = unlisten
    },
    (cause: unknown) => console.error(`Could not listen to ${event}`, cause),
  )

  return subscription(() => {
    stopped = true
    stop?.()
  }, ready)
}

const quitRequestedDecoder = d.object<QuitRequest>({ running: d.number })

/** The tray's Quit was used while items run: the window asks before quitting. */
export function onQuitRequested(handler: (event: QuitRequest) => void) {
  return subscribe("app://quit-requested", quitRequestedDecoder, handler)
}

export function onQueueChanged(handler: (event: QueueChanged) => void) {
  return subscribe("queue://changed", queueChangedDecoder, handler)
}

export function onQueueProgress(handler: (event: QueueProgress) => void) {
  return subscribe("queue://progress", queueProgressDecoder, handler)
}

export function onQueueAlert(handler: (event: QueueAlert) => void) {
  return subscribe("queue://alert", queueAlertDecoder, handler)
}

export function onQueueStatus(handler: (event: QueueStatus) => void) {
  return subscribe("queue://status", queueStatusDecoder, handler)
}

export function onAuthState(handler: (event: AuthState) => void) {
  return subscribe("auth://state", authStateDecoder, handler)
}

export function onSidecarProgress(handler: (event: SidecarProgress) => void) {
  return subscribe("sidecars://progress", sidecarProgressDecoder, handler)
}

export function onUpdate(handler: (event: UpdateNotice) => void) {
  return subscribe("app://update", updateNoticeDecoder, handler)
}

export function onUpdateProgress(handler: (event: UpdateProgress) => void) {
  return subscribe("app://update-progress", updateProgressDecoder, handler)
}

export function onSidecarStatus(handler: (event: SidecarStatus) => void) {
  return subscribe("sidecars://status", sidecarStatusDecoder, handler)
}

/** A new status after a license change, a sign-in or a sign-out. */
export function onEntitlementsStatus(
  handler: (event: EntitlementStatus) => void,
) {
  return subscribe("entitlements://status", entitlementStatusDecoder, handler)
}

// --- window, native drag-and-drop, dialogs ---

export type FileDropEvent =
  { type: "enter" | "over" | "leave" } | { type: "drop"; paths: string[] }

/** Native drag-and-drop of the webview: the only way to get real file paths. */
export function onFileDrop(
  handler: (event: FileDropEvent) => void,
): () => void {
  if (!isTauri()) return () => {}

  let stopped = false
  let stop: (() => void) | undefined
  getCurrentWebview()
    .onDragDropEvent(({ payload }) => {
      if (payload.type === "drop")
        handler({ type: "drop", paths: payload.paths })
      else handler({ type: payload.type })
    })
    .then((unlisten) => {
      if (stopped) unlisten()
      else stop = unlisten
    })
    .catch((cause: unknown) =>
      console.error("Drag-and-drop is unavailable", cause),
    )

  return () => {
    stopped = true
    stop?.()
  }
}

export async function pickFiles(): Promise<string[]> {
  if (!isTauri()) return []
  const selected = await open({ multiple: true, directory: false })

  return selected ?? []
}

export async function pickFolder(): Promise<string | null> {
  if (!isTauri()) return null

  return open({ multiple: false, directory: true })
}

/** Native title bar and WebView color scheme; `null` follows the system. */
export function setWindowTheme(theme: "light" | "dark" | null): void {
  if (!isTauri()) return
  getCurrentWindow()
    .setTheme(theme)
    .catch((cause: unknown) =>
      console.warn("Could not set the window theme", cause),
    )
}
