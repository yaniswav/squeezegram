import type { TFunction } from "i18next"
import { createContext } from "react"
import type { Destination, DestinationPatch } from "@/lib/ipc"
import { useRequiredContext } from "./context"

export interface DestinationsApi {
  /** `unavailable`: the core doesn't know `destinations_list` (older build). */
  status: "loading" | "ready" | "unavailable" | "error"
  list: Destination[]
  byId: ReadonlyMap<number, Destination>
  reload(): Promise<void>
  /** Asks Telegram (`destinations_refresh`); errors become alerts. */
  refresh(): Promise<void>
  /** Local alias / favorite (`destination_update`); false when refused (an alert is shown). */
  update(id: number, patch: DestinationPatch): Promise<boolean>
  /**
   * Telegram search (`destinations_search`, read only): the chats found, merged into `list`.
   * Rejects with the `IpcError` (the caller shows it next to the search).
   */
  search(query: string): Promise<Destination[]>
  /**
   * Topics of a forum row (`destination_topics`), merged into `list`; false when Telegram
   * refused (an alert is shown).
   */
  loadTopics(forumId: number): Promise<boolean>
}

export const DestinationsContext = createContext<DestinationsApi | null>(null)

export function useDestinations(): DestinationsApi {
  return useRequiredContext(DestinationsContext, "useDestinations")
}

/**
 * The name Telegram gives the chat, as shown: Saved Messages in the page's language (the
 * stored title is the one TDLib returned, in English whatever the language).
 */
export function telegramName(destination: Destination, t: TFunction): string {
  return destination.is_saved
    ? t("destinations.savedMessages")
    : destination.title.trim()
}

/** Local name, else Telegram name; "" when both are blank (the UI shows a placeholder). */
export function destinationName(
  destination: Destination,
  t: TFunction,
): string {
  return destination.alias?.trim() || telegramName(destination, t)
}

/** `list` with `rows` replacing the entries of the same id, new ones appended. */
export function mergeDestinations(
  list: readonly Destination[],
  rows: readonly Destination[],
): Destination[] {
  const fresh = new Map(rows.map((row) => [row.id, row]))
  const merged = list.map((d) => fresh.get(d.id) ?? d)
  const known = new Set(list.map((d) => d.id))

  return [...merged, ...rows.filter((row) => !known.has(row.id))]
}
