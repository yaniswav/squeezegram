// Contract of the update commands and events (src-tauri/src/update/mod.rs). Kept out of ipc.ts
// so `node --test` can import it (see update.test.ts); ipc.ts re-exports what the UI needs.

import * as d from "./decode.ts"

/** `update::Kind`: how this copy of the app was installed. */
export type UpdateKind = "installer" | "portable"

/**
 * `update::UpdateNotice`: result of `update_check` and payload of `app://update`. `version` is
 * the announced one, or the running one when nothing newer exists.
 */
export interface UpdateNotice {
  available: boolean
  version: string
  kind: UpdateKind
  /** Release notes of the manifest. */
  notes?: string
  /** Page of the latest release, for the portable build. */
  url: string
}

/** `update_info`: no network. `notice` is the update found earlier in this session. */
export interface UpdateInfo {
  kind: UpdateKind
  current_version: string
  notice: UpdateNotice | null
}

/** `update::Phase`: `installing` is the last one, just before the app closes. */
export type UpdatePhase = "downloading" | "installing"

/** `update::UpdateProgress`: payload of `app://update-progress`. */
export interface UpdateProgress {
  phase: UpdatePhase
  downloaded: number
  /** Null when the server did not announce a length. */
  total: number | null
  /** Whole percent; null without a total. */
  percent: number | null
}

const updateKindDecoder = d.literals<UpdateKind>({
  installer: true,
  portable: true,
})

export const updateNoticeDecoder = d.object<UpdateNotice>({
  available: d.boolean,
  version: d.string,
  kind: updateKindDecoder,
  notes: d.optional(d.string),
  url: d.string,
})

export const updateInfoDecoder = d.object<UpdateInfo>({
  kind: updateKindDecoder,
  current_version: d.string,
  notice: d.nullable(updateNoticeDecoder),
})

export const updateProgressDecoder = d.object<UpdateProgress>({
  phase: d.literals<UpdatePhase>({ downloading: true, installing: true }),
  downloaded: d.number,
  total: d.nullable(d.number),
  percent: d.nullable(d.number),
})
