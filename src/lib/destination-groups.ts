// Grouping, filtering and keyboard navigation of the destination picker. Pure (no React, no
// imports) so `node --test` runs it as is: see destination-groups.test.ts.

/** A Telegram chat folder (`ChatFolder` in ipc.ts). */
export interface PickerFolder {
  id: number
  name: string
  position: number
}

/** The fields of `Destination` (ipc.ts) the picker reads. */
export interface PickerDestination {
  id: number
  peer_kind: string
  peer_id: number
  topic_id: number
  is_forum: boolean
  is_saved: boolean
  title: string
  alias: string | null
  favorite: boolean
  archived: boolean
  folders: PickerFolder[]
  listed: boolean
  topic_closed: boolean
  /** Last time files were queued for it (ISO 8601 UTC), null if never. */
  last_used_at: string | null
  /** Number of times it was chosen to queue files. */
  use_count: number
}

/**
 * The tabs above the list. `all`: every chat, grouped; `main`: not archived; `archived`: the
 * Telegram archive; `folders`: chats in at least one Telegram folder, grouped by folder;
 * `forums`: forums only, archived or not.
 */
export const FILTERS = ["all", "main", "archived", "folders", "forums"] as const

export type DestinationFilter = (typeof FILTERS)[number]

export const DEFAULT_FILTER: DestinationFilter = "all"

/** A stored value back to a filter; anything unknown (or missing) is `all`. */
export function parseFilter(value: string | null): DestinationFilter {
  return FILTERS.find((filter) => filter === value) ?? DEFAULT_FILTER
}

/**
 * `saved`, `favorites`, `recent` (recently used: the chats and topics files were last queued
 * for), `folder` (one per Telegram folder), `chats` (main list),
 * `archived` (all archived chats), `forums` (forums tab), `unnamed` (chats without any name,
 * folded at the very bottom), `results` (local filter), `telegram` (chats in none of the
 * user's lists: found by a Telegram search, or gone; never mixed with the others).
 */
export type GroupKind =
  | "saved"
  | "favorites"
  | "recent"
  | "folder"
  | "chats"
  | "archived"
  | "forums"
  | "unnamed"
  | "results"
  | "telegram"

/** `loading` / `error` / `empty`: the topics of an opened forum are not there (yet). */
export type TopicsStatus = "loading" | "error" | "empty"

/** Unnamed chat: chosen only once `destination_check` allowed it. */
export type UnnamedCheck = "idle" | "checking" | "denied"

export interface PickerOption {
  /** Unique in the whole list (a chat can be in several groups): used for DOM ids. */
  key: string
  /**
   * `forum`: opens its topics, never selected; `status`: a line about those topics;
   * `toggle`: shows or hides the unnamed chats (its label is their number).
   */
  role: "chat" | "forum" | "topic" | "status" | "toggle"
  destination: PickerDestination
  /** Never empty: the name, else `unnamedLabel`. */
  label: string
  /** 1 for a topic shown under its forum. */
  depth: 0 | 1
  /** Closed topics and status lines: shown, skipped by the keyboard, never selected. */
  disabled: boolean
  expanded: boolean
  status?: TopicsStatus
  /** Unnamed chats: state of their check. */
  check?: UnnamedCheck
}

export interface PickerGroup {
  key: string
  kind: GroupKind
  /** For `folder` groups. */
  folder?: PickerFolder
  /** Chats (and forums) in the group: topics shown under a forum are not counted. */
  count: number
  options: PickerOption[]
}

export interface BuildInput {
  destinations: readonly PickerDestination[]
  query: string
  /** The active tab; `all` when absent. */
  filter?: DestinationFilter
  /** Ids of the forum rows (`topic_id = 0`) whose topics are shown. */
  expanded: ReadonlySet<number>
  /** State of the topic loading of an opened forum. */
  topicsState: (forumId: number) => "idle" | "loading" | "error"
  /** Ids returned by the last Telegram search for this query, if any. */
  remote: readonly number[] | null
  /** Label of a chat without a name (a deleted account cached before the fix…). */
  unnamedLabel: string
  /** The unnamed chats are shown (folded by default). */
  unnamedOpen?: boolean
  unnamedCheck?: (id: number) => UnnamedCheck
  recentLimit?: number
}

/** Chats and topics in the "Recently used" group. */
export const RECENT_LIMIT = 8
/** Separator between a forum and a topic in a topic row's title (service.rs). */
const TOPIC_SEPARATOR = " › "
const GENERAL_TOPIC_ID = 1

/** Lower case, accents removed, spaces collapsed: what the filter compares. */
export function normalize(text: string): string {
  return text
    .normalize("NFD")
    .replace(/\p{M}/gu, "")
    .toLocaleLowerCase()
    .replace(/\s+/g, " ")
    .trim()
}

/** Local name, else Telegram name; "" if both are blank. */
export function displayName(destination: PickerDestination): string {
  return destination.alias?.trim() || destination.title.trim()
}

/** A topic's own name, without its forum's prefix. */
export function topicName(topic: PickerDestination): string {
  const name = displayName(topic)

  if (topic.alias?.trim()) return name
  const at = name.indexOf(TOPIC_SEPARATOR)

  return at >= 0 ? name.slice(at + TOPIC_SEPARATOR.length).trim() : name
}

/** A Telegram search is worth it from two characters. */
export function searchable(query: string): boolean {
  return normalize(query).length >= 2
}

/** Every word of the query is in the text (accents and case ignored). */
export function matches(text: string, query: string): boolean {
  const haystack = normalize(text)

  return normalize(query)
    .split(" ")
    .every((word) => haystack.includes(word))
}

const peerKey = (d: PickerDestination) => `${d.peer_kind}:${d.peer_id}`

/** By name, chats without a name last. */
function byName(a: PickerDestination, b: PickerDestination): number {
  const [x, y] = [displayName(a), displayName(b)]

  return (
    Number(x === "") - Number(y === "") ||
    x.localeCompare(y, undefined, { sensitivity: "base", numeric: true }) ||
    a.id - b.id
  )
}

/** General first, then by name. */
function byTopic(a: PickerDestination, b: PickerDestination): number {
  const general =
    Number(b.topic_id === GENERAL_TOPIC_ID) -
    Number(a.topic_id === GENERAL_TOPIC_ID)

  return (
    general ||
    topicName(a).localeCompare(topicName(b), undefined, {
      sensitivity: "base",
      numeric: true,
    }) ||
    a.topic_id - b.topic_id
  )
}

/** No local name, no Telegram name: masked by default (deleted accounts…). */
export function isUnnamed(destination: PickerDestination): boolean {
  return destination.topic_id === 0 && displayName(destination) === ""
}

/** Newest use first; equal dates by number of uses, then by name. */
function byUse(a: PickerDestination, b: PickerDestination): number {
  return (
    (b.last_used_at ?? "").localeCompare(a.last_used_at ?? "") ||
    b.use_count - a.use_count ||
    byName(a, b)
  )
}

/**
 * Can be chosen as it is: named, a chat that is not a forum (a forum is sent to through one
 * of its topics) or a topic that is not closed.
 */
function choosable(d: PickerDestination): boolean {
  return !isUnnamed(d) && (d.topic_id === 0 ? !d.is_forum : !d.topic_closed)
}

/**
 * The destination files were last queued for, when it can still be chosen: what the add panel
 * starts on while the user has not chosen one. Saved Messages included.
 */
export function mostRecent(
  destinations: readonly PickerDestination[],
): PickerDestination | undefined {
  return destinations
    .filter((d) => d.last_used_at !== null && choosable(d))
    .sort(byUse)[0]
}

/** Does the chat belong to the tab? Topics are judged by their forum's row. */
function inFilter(filter: DestinationFilter, d: PickerDestination): boolean {
  switch (filter) {
    case "all":
      return true
    case "main":
      return !d.archived
    case "archived":
      return d.archived
    case "folders":
      return d.folders.length > 0
    case "forums":
      return d.is_forum
  }
}

/**
 * Number of chats per tab, from the loaded data: the user's own chats only (in a Telegram
 * list), Saved Messages included, forums counted once whatever their topics, chats without
 * a name left out (they have their own folded group).
 */
export function countFilters(
  destinations: readonly PickerDestination[],
): Record<DestinationFilter, number> {
  const named = destinations.filter((d) => !isUnnamed(d))
  const forums = new Set(
    named.filter((d) => d.topic_id === 0 && d.is_forum).map(peerKey),
  )
  const mine = named.filter(
    (d) => d.listed && (d.topic_id === 0 || !forums.has(peerKey(d))),
  )
  const count = (filter: DestinationFilter) =>
    mine.filter((d) => inFilter(filter, d)).length

  return {
    all: count("all"),
    main: count("main"),
    archived: count("archived"),
    folders: count("folders"),
    forums: mine.filter((d) => d.topic_id === 0 && d.is_forum).length,
  }
}

export function buildGroups(input: BuildInput): PickerGroup[] {
  const { expanded, topicsState, unnamedLabel } = input
  const filter = input.filter ?? DEFAULT_FILTER
  // Unnamed chats only ever show in their own folded group, never among the others.
  const unnamed = input.destinations.filter(isUnnamed)
  const destinations = input.destinations.filter((d) => !isUnnamed(d))
  const chats = destinations.filter((d) => d.topic_id === 0)
  const forumByPeer = new Map(
    chats.filter((d) => d.is_forum).map((d) => [peerKey(d), d]),
  )
  const topicsByPeer = new Map<string, PickerDestination[]>()

  for (const d of destinations) {
    if (d.topic_id === 0) continue
    const key = peerKey(d)
    topicsByPeer.set(key, [...(topicsByPeer.get(key) ?? []), d])
  }

  for (const topics of topicsByPeer.values()) topics.sort(byTopic)

  // Topic rows whose forum row is missing (cached before schema v2) stand on their own.
  const orphans = destinations.filter(
    (d) => d.topic_id !== 0 && !forumByPeer.has(peerKey(d)),
  )

  /** The row that decides the tab and the list of a topic: its forum's. */
  const ownerOf = (d: PickerDestination) =>
    d.topic_id === 0 ? d : (forumByPeer.get(peerKey(d)) ?? d)
  /** In one of the user's lists, and in the active tab. */
  const mine = (d: PickerDestination) => {
    const owner = ownerOf(d)

    return owner.listed && inFilter(filter, owner)
  }

  const label = (text: string) => text || unnamedLabel

  /** A topic listed on its own: "Chat › Topic", with the forum's current name. */
  const topicLabel = (d: PickerDestination) => {
    const forum = forumByPeer.get(peerKey(d))
    const chat = forum ? displayName(forum) : ""

    return chat ? `${chat}${TOPIC_SEPARATOR}${topicName(d)}` : displayName(d)
  }

  function optionsOf(groupKey: string, d: PickerDestination): PickerOption[] {
    if (d.topic_id !== 0) {
      return [
        {
          key: `${groupKey}/${d.id}`,
          role: "topic",
          destination: d,
          label: label(topicLabel(d)),
          depth: 0,
          disabled: d.topic_closed,
          expanded: false,
        },
      ]
    }

    if (!d.is_forum) {
      return [
        {
          key: `${groupKey}/${d.id}`,
          role: "chat",
          destination: d,
          label: label(displayName(d)),
          depth: 0,
          disabled: false,
          expanded: false,
        },
      ]
    }

    const open = expanded.has(d.id)
    const forum: PickerOption = {
      key: `${groupKey}/${d.id}`,
      role: "forum",
      destination: d,
      label: label(displayName(d)),
      depth: 0,
      disabled: false,
      expanded: open,
    }

    if (!open) return [forum]
    const topics = topicsByPeer.get(peerKey(d)) ?? []
    const state = topicsState(d.id)
    const children: PickerOption[] = topics.map((topic) => ({
      key: `${groupKey}/${d.id}/${topic.id}`,
      role: "topic",
      destination: topic,
      label: label(topicName(topic)),
      depth: 1,
      disabled: topic.topic_closed,
      expanded: false,
    }))

    // Cached topics show at once; a line tells while they load or if loading failed.
    const status: TopicsStatus | null =
      state === "loading"
        ? "loading"
        : state === "error"
          ? "error"
          : topics.length === 0
            ? "empty"
            : null

    if (status) {
      children.push({
        key: `${groupKey}/${d.id}/status`,
        role: "status",
        destination: d,
        label: "",
        depth: 1,
        disabled: true,
        expanded: false,
        status,
      })
    }

    return [forum, ...children]
  }

  function group(
    key: string,
    kind: GroupKind,
    members: PickerDestination[],
    folder?: PickerFolder,
  ): PickerGroup | null {
    if (members.length === 0) return null

    return {
      key,
      kind,
      folder,
      count: members.length,
      options: members.flatMap((d) => optionsOf(key, d)),
    }
  }

  const sendable = [
    ...chats,
    ...destinations.filter((d) => d.topic_id !== 0),
  ].filter((d) => !d.is_saved)

  const groups: (PickerGroup | null)[] = []
  const query = normalize(input.query)

  if (query !== "") {
    const remote = new Set(input.remote ?? [])
    // Chats match by name; topics by their own name (they are listed on their own); a
    // Telegram hit stays a hit even when its name doesn't contain the words.
    const hit = (d: PickerDestination) =>
      remote.has(d.id) ||
      matches(d.topic_id === 0 ? displayName(d) : topicName(d), query)

    const found = destinations
      .filter((d) => mine(d) && hit(d))
      .sort(
        (a, b) =>
          Number(b.is_saved) - Number(a.is_saved) ||
          Number(b.favorite) - Number(a.favorite) ||
          byName(a, b),
      )

    groups.push(group("results", "results", found))
    // Chats in none of the user's lists, whatever the tab: a group of their own.
    groups.push(
      group(
        "telegram",
        "telegram",
        destinations.filter((d) => !ownerOf(d).listed && hit(d)).sort(byName),
      ),
    )
  } else {
    const top = filter === "all" || filter === "main"
    const loose = [...chats, ...orphans].filter((d) => !d.is_saved)

    if (top) {
      groups.push(
        group(
          "saved",
          "saved",
          chats.filter((d) => d.is_saved && inFilter(filter, d)),
        ),
      )
      groups.push(
        group(
          "favorites",
          "favorites",
          sendable
            .filter((d) => d.favorite && inFilter(filter, ownerOf(d)))
            .sort(byName),
        ),
      )
    }

    // Recently used, in every tab: right after Saved Messages and Favorites in All and Main,
    // first in the others, holding only what the tab shows (archived ones in Archived…).
    // Topics are listed on their own, as "Chat › Topic".
    groups.push(
      group(
        "recent",
        "recent",
        sendable
          .filter(
            (d) =>
              d.last_used_at !== null &&
              choosable(d) &&
              inFilter(filter, ownerOf(d)),
          )
          .sort(byUse)
          .slice(0, input.recentLimit ?? RECENT_LIMIT),
      ),
    )

    if (filter === "all" || filter === "folders") {
      const folders = new Map<number, PickerFolder>()

      for (const d of chats) {
        if (!d.listed) continue

        for (const folder of d.folders) {
          if (!folders.has(folder.id)) folders.set(folder.id, folder)
        }
      }

      const ordered = [...folders.values()].sort(
        (a, b) => a.position - b.position || a.id - b.id,
      )

      for (const folder of ordered) {
        groups.push(
          group(
            `folder:${folder.id}`,
            "folder",
            chats
              .filter(
                (d) =>
                  d.listed &&
                  !d.is_saved &&
                  d.folders.some((f) => f.id === folder.id),
              )
              .sort(byName),
            folder,
          ),
        )
      }
    }

    if (top) {
      groups.push(
        group(
          "chats",
          "chats",
          loose.filter((d) => d.listed && !d.archived).sort(byName),
        ),
      )
    }

    // Archived chats of every kind (forums too, with their topics), by title.
    if (filter === "all" || filter === "archived") {
      groups.push(
        group(
          "archived",
          "archived",
          loose.filter((d) => d.listed && d.archived).sort(byName),
        ),
      )
    }

    if (filter === "forums") {
      groups.push(
        group(
          "forums",
          "forums",
          chats.filter((d) => d.listed && d.is_forum).sort(byName),
        ),
      )
    }

    // Found by an earlier Telegram search, in none of the user's lists.
    if (filter === "all") {
      groups.push(
        group(
          "telegram",
          "telegram",
          loose.filter((d) => !d.listed).sort(byName),
        ),
      )
    }

    const shownUnnamed = unnamed.filter((d) => inFilter(filter, d))

    if (shownUnnamed.length > 0) {
      const check = input.unnamedCheck ?? (() => "idle")
      const toggle: PickerOption = {
        key: "unnamed/toggle",
        role: "toggle",
        destination: shownUnnamed[0],
        label: String(shownUnnamed.length),
        depth: 0,
        disabled: false,
        expanded: input.unnamedOpen === true,
      }
      const members: PickerOption[] = input.unnamedOpen
        ? [...shownUnnamed]
            .sort((a, b) => a.id - b.id)
            .map((d) => ({
              key: `unnamed/${d.id}`,
              role: "chat",
              destination: d,
              label: unnamedLabel,
              depth: 1,
              disabled: check(d.id) === "denied",
              expanded: false,
              check: check(d.id),
            }))
        : []

      groups.push({
        key: "unnamed",
        kind: "unnamed",
        count: shownUnnamed.length,
        options: [toggle, ...members],
      })
    }
  }

  return groups.filter((g): g is PickerGroup => g !== null)
}

/** Options the keyboard moves through, in order. */
export function navigable(groups: readonly PickerGroup[]): PickerOption[] {
  return groups.flatMap((g) => g.options.filter((o) => !o.disabled))
}

/**
 * The option `delta` steps away from `current` (clamped at both ends); the first one when
 * `current` is not in the list any more.
 */
export function moveActive(
  options: readonly PickerOption[],
  current: string | null,
  delta: number,
): string | null {
  if (options.length === 0) return null
  const index = options.findIndex((o) => o.key === current)

  if (index < 0) return options[delta < 0 ? options.length - 1 : 0].key
  const next = Math.min(options.length - 1, Math.max(0, index + delta))

  return options[next].key
}
