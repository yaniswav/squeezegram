// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { test } from "node:test"
import {
  buildGroups,
  countFilters,
  matches,
  moveActive,
  mostRecent,
  navigable,
  normalize,
  parseFilter,
  type BuildInput,
  type PickerDestination,
  type PickerGroup,
} from "./destination-groups.ts"

const WORK = { id: 2, name: "Work", position: 0 }
const FAMILY = { id: 5, name: "Family", position: 1 }

let nextId = 1

function row(fields: Partial<PickerDestination>): PickerDestination {
  const id = nextId++

  return {
    id,
    peer_kind: "channel",
    peer_id: 1000 + id,
    topic_id: 0,
    is_forum: false,
    is_saved: false,
    title: `Chat ${id}`,
    alias: null,
    favorite: false,
    archived: false,
    folders: [],
    listed: true,
    topic_closed: false,
    last_used_at: null,
    use_count: 0,
    ...fields,
  }
}

const saved = row({
  title: "Saved Messages",
  is_saved: true,
  peer_kind: "user",
})
const zoe = row({ title: "Zoé", peer_kind: "user", folders: [FAMILY] })
const slow = row({ title: "Slow group", folders: [WORK, FAMILY] })
const news = row({ title: "News", favorite: true })
const blank = row({ title: "", peer_kind: "user" })
const oldFriend = row({ title: "Old friend", archived: true })
const forum = row({
  title: "Archived forum",
  is_forum: true,
  archived: true,
  folders: [WORK],
})
const topic = (id: number, name: string, closed = false) =>
  row({
    peer_id: forum.peer_id,
    topic_id: id,
    is_forum: true,
    archived: true,
    title: `Archived forum › ${name}`,
    topic_closed: closed,
  })
const clips = topic(5, "Clips")
const closed = topic(6, "Old", true)
const general = topic(1, "General")
const found = row({ title: "Public group", listed: false })
const used = row({
  title: "Used",
  last_used_at: "2026-10-09T10:00:00.000Z",
  use_count: 3,
})

const all = [
  saved,
  zoe,
  slow,
  news,
  blank,
  oldFriend,
  forum,
  clips,
  closed,
  general,
  found,
  used,
]

function build(overrides: Partial<BuildInput> = {}): PickerGroup[] {
  return buildGroups({
    destinations: all,
    query: "",
    expanded: new Set(),
    topicsState: () => "idle",
    remote: null,
    unnamedLabel: "Unnamed chat",
    ...overrides,
  })
}

const summary = (groups: PickerGroup[]) =>
  groups.map((g) => [g.key, g.options.map((o) => o.label)])

test("groups: saved, favorites, recent, each folder, chats, archive, search results", () => {
  assert.deepEqual(summary(build()), [
    ["saved", ["Saved Messages"]],
    ["favorites", ["News"]],
    ["recent", ["Used"]],
    ["folder:2", ["Archived forum", "Slow group"]],
    ["folder:5", ["Slow group", "Zoé"]],
    ["chats", ["News", "Slow group", "Used", "Zoé"]],
    ["archived", ["Archived forum", "Old friend"]],
    // Found by search only: a group of its own, last before the unnamed ones.
    ["telegram", ["Public group"]],
    // Chats without a name: folded at the very bottom, their number as the toggle.
    ["unnamed", ["1"]],
  ])
})

test("no empty row and unique keys (one DOM id per option)", () => {
  const groups = build({ expanded: new Set([forum.id]) })
  const options = groups.flatMap((g) => g.options)

  for (const option of options) {
    if (option.role !== "status") assert.notEqual(option.label.trim(), "")
  }

  const keys = options.map((o) => o.key)
  assert.equal(new Set(keys).size, keys.length)
})

test("an opened forum lists its topics, General first, closed ones disabled", () => {
  const groups = build({ expanded: new Set([forum.id]) })
  const archived = groups.find((g) => g.key === "archived")
  assert.ok(archived)
  assert.deepEqual(
    archived.options.map((o) => [o.role, o.label, o.depth, o.disabled]),
    [
      ["forum", "Archived forum", 0, false],
      ["topic", "General", 1, false],
      ["topic", "Clips", 1, false],
      ["topic", "Old", 1, true],
      ["chat", "Old friend", 0, false],
    ],
  )
  // The same forum opened in its folder too.
  const work = groups.find((g) => g.key === "folder:2")
  assert.equal(work?.options.length, 5)
})

test("topics loading, failed or missing show one status line", () => {
  const empty = buildGroups({
    destinations: [forum],
    query: "",
    expanded: new Set([forum.id]),
    topicsState: () => "loading",
    remote: null,
    unnamedLabel: "x",
  })
  // In its folder and in the archive: the same two lines in each.
  assert.deepEqual(
    empty.map((g) => g.key),
    ["folder:2", "archived"],
  )
  const options = empty[1].options
  assert.deepEqual(
    options.map((o) => [o.role, o.status, o.disabled]),
    [
      ["forum", undefined, false],
      ["status", "loading", true],
    ],
  )
})

test("keyboard skips closed topics and status lines, and stops at both ends", () => {
  const options = navigable(build({ expanded: new Set([forum.id]) }))
  assert.ok(options.every((o) => !o.disabled))
  assert.ok(!options.some((o) => o.destination.id === closed.id))
  const first = options[0].key
  const last = options[options.length - 1].key
  assert.equal(moveActive(options, null, 1), first)
  assert.equal(moveActive(options, null, -1), last)
  assert.equal(moveActive(options, first, -1), first)
  assert.equal(moveActive(options, last, 1), last)
  assert.equal(moveActive(options, first, 1), options[1].key)
  assert.equal(moveActive([], first, 1), null)
})

test("filter ignores case and accents; topics match by their own name", () => {
  assert.ok(matches("Zoé", "zoe"))
  assert.ok(matches("Slow group", "GROUP slow"))
  assert.equal(normalize("  Ça  va "), "ca va")
  assert.deepEqual(summary(build({ query: "zoe" })), [["results", ["Zoé"]]])
  assert.deepEqual(summary(build({ query: "clips" })), [
    ["results", ["Archived forum › Clips"]],
  ])
  // A closed topic found by name stays disabled.
  const old = build({ query: "old" }).flatMap((g) => g.options)
  assert.deepEqual(
    old.map((o) => [o.label, o.disabled]),
    [
      ["Archived forum › Old", true],
      ["Old friend", false],
    ],
  )
})

test("chats found by a Telegram search show in their own group", () => {
  const groups = build({ query: "zzz", remote: [found.id, 9999] })
  assert.deepEqual(summary(groups), [["telegram", ["Public group"]]])
  // Matched by name too: still in the Telegram group, once, never among the user's chats.
  const both = build({ query: "public", remote: [found.id] })
  assert.deepEqual(summary(both), [["telegram", ["Public group"]]])
})

test("unnamed chats are folded apart, never mixed with the others", () => {
  const favoriteBlank = row({ title: " ", favorite: true, alias: null })
  const destinations = [...all, favoriteBlank]
  const folded = buildGroups({
    destinations,
    query: "",
    expanded: new Set(),
    topicsState: () => "idle",
    remote: [blank.id],
    unnamedLabel: "Unnamed chat",
  })
  const unnamed = folded[folded.length - 1]
  assert.equal(unnamed.kind, "unnamed")
  assert.deepEqual(
    unnamed.options.map((o) => [o.role, o.label, o.expanded]),
    [["toggle", "2", false]],
  )
  // Not in favorites, nor in any other group.
  for (const group of folded.slice(0, -1)) {
    for (const option of group.options) {
      assert.ok(![blank.id, favoriteBlank.id].includes(option.destination.id))
    }
  }

  const open = buildGroups({
    destinations,
    query: "",
    expanded: new Set(),
    topicsState: () => "idle",
    remote: null,
    unnamedLabel: "Unnamed chat",
    unnamedOpen: true,
    unnamedCheck: (id) => (id === blank.id ? "denied" : "checking"),
  })
  const members = open[open.length - 1].options
  assert.deepEqual(
    members.map((o) => [o.role, o.label, o.depth, o.disabled, o.check]),
    [
      ["toggle", "2", 0, false, undefined],
      ["chat", "Unnamed chat", 1, true, "denied"],
      ["chat", "Unnamed chat", 1, false, "checking"],
    ],
  )
  // The keyboard reaches the toggle, skips what Telegram refused.
  const keys = navigable(open).map((o) => o.key)
  assert.ok(keys.includes("unnamed/toggle"))
  assert.ok(!keys.includes(`unnamed/${blank.id}`))

  // A filter or a Telegram search never shows them.
  const searched = buildGroups({
    destinations,
    query: "unnamed",
    expanded: new Set(),
    topicsState: () => "idle",
    remote: [blank.id],
    unnamedLabel: "Unnamed chat",
  })
  assert.deepEqual(searched, [])
})

const TABS = ["all", "main", "archived", "folders", "forums"] as const

const keysOf = (groups: PickerGroup[]) => groups.map((g) => g.key)

test("counts per tab come from the loaded data (listed, named chats only)", () => {
  // Saved, Zoé, Slow, News, Used, Old friend and the forum: topics and the blank are left out.
  assert.deepEqual(countFilters(all), {
    all: 7,
    main: 5,
    archived: 2,
    folders: 3,
    forums: 1,
  })
  // Found by search only: not the user's chats.
  assert.equal(countFilters([found]).all, 0)
  assert.equal(countFilters([]).archived, 0)
})

test("tab All keeps the grouping, with Archived open and counted", () => {
  const groups = build({ expanded: new Set([forum.id]) })
  const archived = groups.find((g) => g.kind === "archived")
  assert.ok(archived)
  // Two chats; the forum's three topics are shown under it but not counted.
  assert.equal(archived.count, 2)
  assert.equal(archived.options.length, 5)
  assert.ok(archived.options.every((o) => o.role !== "toggle"))
  assert.deepEqual(summary(build({ filter: "all" })), summary(build()))
})

test("tab Main: not archived, no folder groups, no archive", () => {
  assert.deepEqual(summary(build({ filter: "main" })), [
    ["saved", ["Saved Messages"]],
    ["favorites", ["News"]],
    ["recent", ["Used"]],
    ["chats", ["News", "Slow group", "Used", "Zoé"]],
    ["unnamed", ["1"]],
  ])
})

test("tab Archived: every archived chat by title, forums with their topics", () => {
  const groups = build({ filter: "archived", expanded: new Set([forum.id]) })
  assert.deepEqual(keysOf(groups), ["archived"])
  assert.equal(groups[0].count, 2)
  assert.deepEqual(
    groups[0].options.map((o) => [o.role, o.label, o.depth, o.disabled]),
    [
      ["forum", "Archived forum", 0, false],
      ["topic", "General", 1, false],
      ["topic", "Clips", 1, false],
      ["topic", "Old", 1, true],
      ["chat", "Old friend", 0, false],
    ],
  )

  // Private chats, a group, a channel and a forum: all of them, in title order.
  const many = [
    ...Array.from({ length: 12 }, (_, i) =>
      row({ title: `Archived ${12 - i}`, archived: true, peer_kind: "user" }),
    ),
    row({ title: "Archived group", archived: true, peer_kind: "chat" }),
    row({ title: "Archived channel", archived: true }),
    row({ title: "Archived topics", archived: true, is_forum: true }),
    row({ title: "Active" }),
  ]
  const list = buildGroups({
    destinations: many,
    query: "",
    filter: "archived",
    expanded: new Set(),
    topicsState: () => "idle",
    remote: null,
    unnamedLabel: "Unnamed chat",
  })
  assert.equal(list[0].count, 15)
  assert.equal(countFilters(many).archived, 15)
  const titles = list[0].options.map((o) => o.label)
  assert.deepEqual(
    titles,
    [...titles].sort((a, b) =>
      a.localeCompare(b, undefined, { sensitivity: "base", numeric: true }),
    ),
  )
  assert.ok(!titles.includes("Active"))
})

test("tab Folders groups by folder; chats in no folder are left out", () => {
  const groups = build({ filter: "folders" })
  assert.deepEqual(summary(groups), [
    ["folder:2", ["Archived forum", "Slow group"]],
    ["folder:5", ["Slow group", "Zoé"]],
  ])
  assert.deepEqual(
    groups.map((g) => g.folder?.name),
    ["Work", "Family"],
  )
})

test("tab Forums: only forums, archived or not", () => {
  const live = row({ title: "Live forum", is_forum: true })
  const groups = buildGroups({
    destinations: [...all, live],
    query: "",
    filter: "forums",
    expanded: new Set(),
    topicsState: () => "idle",
    remote: null,
    unnamedLabel: "Unnamed chat",
  })
  assert.deepEqual(summary(groups), [
    ["forums", ["Archived forum", "Live forum"]],
  ])
  assert.equal(groups[0].count, 2)
  assert.equal(countFilters([...all, live]).forums, 2)
})

test("the search runs inside the active tab", () => {
  assert.deepEqual(summary(build({ filter: "archived", query: "zoe" })), [])
  assert.deepEqual(summary(build({ filter: "main", query: "zoe" })), [
    ["results", ["Zoé"]],
  ])
  assert.deepEqual(summary(build({ filter: "archived", query: "old" })), [
    ["results", ["Archived forum › Old", "Old friend"]],
  ])
  // A topic follows its forum: an archived forum, so not in Main.
  assert.deepEqual(summary(build({ filter: "main", query: "clips" })), [])
  assert.deepEqual(summary(build({ filter: "forums", query: "clips" })), [
    ["results", ["Archived forum › Clips"]],
  ])
  assert.deepEqual(summary(build({ filter: "folders", query: "slow" })), [
    ["results", ["Slow group"]],
  ])
})

test("Telegram search results never mix with the user's chats, in any tab", () => {
  const stranger = row({ title: "Zoe fan club", listed: false })
  const destinations = [...all, stranger]

  for (const filter of TABS) {
    const groups = buildGroups({
      destinations,
      query: "zoe",
      filter,
      expanded: new Set(),
      topicsState: () => "idle",
      remote: [stranger.id],
      unnamedLabel: "Unnamed chat",
    })
    const external = groups.find((g) => g.kind === "telegram")
    assert.deepEqual(
      external?.options.map((o) => o.label),
      ["Zoe fan club"],
    )

    for (const group of groups.filter((g) => g.kind !== "telegram")) {
      assert.ok(
        group.options.every((o) => o.destination.listed),
        `${filter}: ${group.key}`,
      )
    }

    // The last group: after every chat of the user.
    assert.equal(groups[groups.length - 1].kind, "telegram")
  }

  // Found by the words and by Telegram: still once.
  const twice = buildGroups({
    destinations,
    query: "zoe",
    expanded: new Set(),
    topicsState: () => "idle",
    remote: [stranger.id, stranger.id],
    unnamedLabel: "Unnamed chat",
  })
  const hits = twice
    .flatMap((g) => g.options)
    .filter((o) => o.destination === stranger)
  assert.equal(hits.length, 1)

  // A chat of the user returned by Telegram stays among the user's chats...
  assert.deepEqual(
    summary(build({ query: "zzz", remote: [zoe.id, found.id] })),
    [
      ["results", ["Zoé"]],
      ["telegram", ["Public group"]],
    ],
  )
  // ... and only when it is in the active tab.
  assert.deepEqual(
    summary(build({ query: "zzz", filter: "archived", remote: [zoe.id] })),
    [],
  )
})

test("without a query, only the All tab lists chats found by search", () => {
  for (const filter of TABS) {
    assert.equal(
      keysOf(build({ filter })).includes("telegram"),
      filter === "all",
      filter,
    )
  }
})

test("no empty group or row in any tab; unnamed chats stay folded at the end", () => {
  for (const filter of TABS) {
    const groups = build({ filter, expanded: new Set([forum.id]) })

    for (const group of groups) {
      assert.ok(group.options.length > 0, `${filter}: ${group.key}`)

      for (const option of group.options) {
        if (option.role !== "status") assert.notEqual(option.label.trim(), "")
      }
    }

    const unnamed = groups.findIndex((g) => g.kind === "unnamed")
    assert.ok(unnamed === -1 || unnamed === groups.length - 1)
  }
})

test("the stored tab is validated, defaulting to All", () => {
  assert.equal(parseFilter("archived"), "archived")
  assert.equal(parseFilter("forums"), "forums")
  assert.equal(parseFilter(null), "all")
  assert.equal(parseFilter("bogus"), "all")
  assert.equal(parseFilter(""), "all")
})

/** `minutes` after 2026-10-10 10:00 UTC (after the fixtures' "Used"), as stored. */
const at = (minutes: number) =>
  new Date(Date.UTC(2026, 9, 10, 10, minutes)).toISOString()

/** The fixture chats with these uses, by id: `[last_used_at, use_count]`. */
function withUses(
  uses: ReadonlyMap<number, [string, number]>,
  destinations: readonly PickerDestination[] = all,
): PickerDestination[] {
  return destinations.map((d) => {
    const use = uses.get(d.id)

    return use ? { ...d, last_used_at: use[0], use_count: use[1] } : d
  })
}

test("recently used: the 8 newest, after Saved Messages and Favorites, topics as Chat › Topic", () => {
  const extra = Array.from({ length: 6 }, (_, i) =>
    row({ title: `Extra ${i + 1}`, last_used_at: at(i + 1), use_count: 1 }),
  )
  const destinations = withUses(
    new Map<number, [string, number]>([
      [saved.id, [at(59), 9]],
      [news.id, [at(50), 2]],
      [clips.id, [at(40), 1]],
      [closed.id, [at(58), 1]],
      [forum.id, [at(57), 1]],
      [blank.id, [at(56), 1]],
      [zoe.id, [at(30), 1]],
    ]),
    [...all, ...extra],
  )
  const groups = buildGroups({
    destinations,
    query: "",
    expanded: new Set(),
    topicsState: () => "idle",
    remote: null,
    unnamedLabel: "Unnamed chat",
  })
  assert.deepEqual(keysOf(groups).slice(0, 3), ["saved", "favorites", "recent"])
  const recent = groups[2]
  assert.equal(recent.kind, "recent")
  // Newest first; Saved Messages (its own group), a forum row (sent to through a topic), a
  // closed topic and an unnamed chat are left out; a favorite stays.
  assert.deepEqual(
    recent.options.map((o) => [o.role, o.label]),
    [
      ["chat", "News"],
      ["topic", "Archived forum › Clips"],
      ["chat", "Zoé"],
      ["chat", "Extra 6"],
      ["chat", "Extra 5"],
      ["chat", "Extra 4"],
      ["chat", "Extra 3"],
      ["chat", "Extra 2"],
    ],
  )
  assert.equal(recent.count, 8)
  // A topic of an archived forum, listed on its own: chosen directly, never folded.
  assert.equal(recent.options[1].depth, 0)
  assert.equal(recent.options[1].destination.id, clips.id)
})

test("recently used follows the tab: archived ones in Archived, first; none when nothing fits", () => {
  const destinations = withUses(
    new Map<number, [string, number]>([
      [clips.id, [at(40), 1]],
      [oldFriend.id, [at(20), 1]],
      [slow.id, [at(10), 1]],
    ]),
  )
  const view = (filter: (typeof TABS)[number]) =>
    summary(
      buildGroups({
        destinations,
        query: "",
        filter,
        expanded: new Set(),
        topicsState: () => "idle",
        remote: null,
        unnamedLabel: "Unnamed chat",
      }),
    )
  const recentOf = (filter: (typeof TABS)[number]) =>
    view(filter).find(([key]) => key === "recent")?.[1]

  assert.deepEqual(recentOf("all"), [
    "Archived forum › Clips",
    "Old friend",
    "Slow group",
    "Used",
  ])
  assert.deepEqual(recentOf("main"), ["Slow group", "Used"])
  // First in the other tabs, holding only what the tab shows.
  assert.deepEqual(view("archived").slice(0, 2), [
    ["recent", ["Archived forum › Clips", "Old friend"]],
    ["archived", ["Archived forum", "Old friend"]],
  ])
  assert.deepEqual(view("folders")[0], [
    "recent",
    ["Archived forum › Clips", "Slow group"],
  ])
  assert.deepEqual(view("forums")[0], ["recent", ["Archived forum › Clips"]])
  // Nothing used in a tab: no group at all (the fixtures' "Used" is in no folder).
  assert.equal(keysOf(build({ filter: "folders" })).includes("recent"), false)
})

test("a recent topic shows its forum's local name", () => {
  const renamed = { ...forum, alias: "Team" }
  const destinations = withUses(
    new Map<number, [string, number]>([[clips.id, [at(1), 1]]]),
  ).map((d) => (d.id === forum.id ? renamed : d))
  const recent = buildGroups({
    destinations,
    query: "",
    expanded: new Set(),
    topicsState: () => "idle",
    remote: null,
    unnamedLabel: "Unnamed chat",
  }).find((g) => g.kind === "recent")
  assert.deepEqual(
    recent?.options.map((o) => o.label),
    ["Team › Clips", "Used"],
  )
})

test("the add panel starts on the most recent destination that can be chosen", () => {
  assert.equal(mostRecent(all)?.id, used.id)
  // Saved Messages counts; a forum row, a closed topic or an unnamed chat never does.
  const newer = withUses(
    new Map<number, [string, number]>([
      [forum.id, [at(59), 1]],
      [closed.id, [at(58), 1]],
      [blank.id, [at(57), 1]],
    ]),
  )
  assert.equal(mostRecent(newer)?.id, used.id)
  const savedLast = withUses(
    new Map<number, [string, number]>([
      [saved.id, ["2026-10-09T11:00:00.000Z", 1]],
    ]),
  )
  assert.equal(mostRecent(savedLast)?.id, saved.id)
  // Same instant: the one used most.
  const tie = withUses(
    new Map<number, [string, number]>([
      [zoe.id, ["2026-10-09T10:00:00.000Z", 7]],
    ]),
  )
  assert.equal(mostRecent(tie)?.id, zoe.id)
  // A topic is chosen as such.
  const topicLast = withUses(
    new Map<number, [string, number]>([
      [clips.id, ["2026-10-09T12:00:00.000Z", 1]],
    ]),
  )
  assert.equal(mostRecent(topicLast)?.id, clips.id)
  assert.equal(mostRecent([zoe, slow]), undefined)
})

test("Saved Messages is shown and searched under the page's language", () => {
  const savedLabel = "Messages enregistrés"
  const labels = (groups: PickerGroup[]) =>
    groups.flatMap((g) => g.options.map((o) => o.label))

  assert.deepEqual(summary(build({ savedLabel }))[0], ["saved", [savedLabel]])
  assert.ok(
    labels(build({ savedLabel, query: "enregistres" })).includes(savedLabel),
  )
  assert.ok(!labels(build({ savedLabel, query: "saved" })).includes(savedLabel))
  // What is stored does not change: the picker works on copies.
  assert.equal(saved.title, "Saved Messages")

  // A local name still wins.
  const named = { ...saved, alias: "Me" }
  const groups = build({ savedLabel, destinations: [named] })

  assert.deepEqual(summary(groups)[0], ["saved", ["Me"]])
})
