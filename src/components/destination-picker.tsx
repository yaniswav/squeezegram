import {
  Archive,
  Bookmark,
  Check,
  ChevronRight,
  ChevronsUpDown,
  Clock,
  Folder,
  Globe,
  LoaderCircle,
  Lock,
  Megaphone,
  MessagesSquare,
  Search,
  Star,
  User,
  Users,
} from "lucide-react"
import { Popover as PopoverPrimitive } from "radix-ui"
import {
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
} from "react"
import { useTranslation } from "react-i18next"
import { Button } from "@/components/ui/button"
import {
  DEFAULT_FILTER,
  FILTERS,
  buildGroups,
  countFilters,
  moveActive,
  navigable,
  isUnnamed,
  parseFilter,
  searchable,
  type DestinationFilter,
  type PickerGroup,
  type UnnamedCheck,
  type PickerOption,
} from "@/lib/destination-groups"
import { destinationCheck, type Destination } from "@/lib/ipc"
import { cn } from "@/lib/utils"
import { destinationName, useDestinations } from "@/state/destinations"

/** Debounce of the automatic Telegram search (no local match). */
const SEARCH_DELAY_MS = 450
/** Options skipped by PageUp / PageDown. */
const PAGE = 8
const FILTER_KEY = "squeezegram.destination-filter"

function readFilter(): DestinationFilter {
  try {
    return parseFilter(localStorage.getItem(FILTER_KEY))
  } catch {
    // Storage can be unavailable; start on the default tab.
    return DEFAULT_FILTER
  }
}

function saveFilter(filter: DestinationFilter): void {
  try {
    localStorage.setItem(FILTER_KEY, filter)
  } catch {
    // Not remembered; the choice still applies to this session.
  }
}

type SearchState =
  | { phase: "idle" }
  | { phase: "searching"; query: string }
  | { phase: "done"; query: string; ids: number[] }
  | { phase: "failed"; query: string }

interface DestinationPickerProps {
  id: string
  value: string
  onChange: (value: string) => void
  /** Ids of the elements that name and describe the field. */
  labelledBy?: string
}

/**
 * Destination picker: a button opening a panel with a filter field, tabs (All, Main, Archived,
 * Folders, Forums, each with its number of chats; the last one is remembered) and a scrollable
 * list of a bounded height. The All tab is grouped as Saved Messages, Favorites, Recently used
 * (the 8 chats and topics files were last queued for), each Telegram folder, main chats and
 * Archived chats; the other tabs start with the recently used ones they hold. Chats found by
 * Telegram search are always in a group of their own. Forums open on their topics (loaded on demand; closed topics are
 * shown but cannot be chosen). When the filter matches nothing here, Telegram is searched
 * (read only).
 *
 * Accessibility: WAI-ARIA combobox pattern, plus a tablist (roving tabindex, arrows, Home/End). Focus stays in the filter field
 * (`role="combobox"`, `aria-activedescendant`); ↑/↓, PageUp/PageDown and Ctrl+Home/End move
 * through the options (disabled ones are skipped), → opens a forum, ← closes it or goes back to
 * it, Enter chooses, Esc closes and gives focus back to the button. A polite live region
 * reports the number of matches and the Telegram search.
 */
export function DestinationPicker({
  id,
  value,
  onChange,
  labelledBy,
}: DestinationPickerProps) {
  const { t } = useTranslation()
  const { list, byId, search, loadTopics } = useDestinations()
  const ids = useId()
  const listId = `${ids}-list`
  const inputRef = useRef<HTMLInputElement>(null)
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState("")
  const [expanded, setExpanded] = useState<ReadonlySet<number>>(new Set())
  const [topicsState, setTopicsState] = useState<
    ReadonlyMap<number, "loading" | "error">
  >(new Map())
  const [searchState, setSearchState] = useState<SearchState>({
    phase: "idle",
  })
  const [active, setActive] = useState<string | null>(null)
  const [filter, setFilter] = useState<DestinationFilter>(readFilter)
  const tabs = useRef<(HTMLButtonElement | null)[]>([])
  /** The folded "Unnamed" group is open. */
  const [unnamedOpen, setUnnamedOpen] = useState(false)
  /** Unnamed chats: `destination_check` running or refused (then not selectable). */
  const [checks, setChecks] = useState<ReadonlyMap<number, UnnamedCheck>>(
    new Map(),
  )
  /** Forums whose topics were asked for since the panel opened. */
  const asked = useRef(new Set<number>())
  /** The query of the latest Telegram search: older answers are dropped. */
  const latest = useRef("")

  const selected = value === "" ? undefined : byId.get(Number(value))
  const unnamed = t("destinations.unnamed")
  const trimmed = query.trim()

  const remote =
    searchState.phase === "done" && searchState.query === trimmed
      ? searchState.ids
      : null

  const groups = useMemo(
    () =>
      buildGroups({
        destinations: list,
        query,
        filter,
        expanded,
        topicsState: (forumId) => topicsState.get(forumId) ?? "idle",
        remote,
        unnamedLabel: unnamed,
        unnamedOpen,
        unnamedCheck: (id) => checks.get(id) ?? "idle",
      }),
    [
      list,
      query,
      filter,
      expanded,
      topicsState,
      remote,
      unnamed,
      unnamedOpen,
      checks,
    ],
  )
  const counts = useMemo(() => countFilters(list), [list])

  const options = useMemo(() => navigable(groups), [groups])
  const localMatches = trimmed
    ? (groups.find((g) => g.kind === "results")?.options.length ?? 0)
    : null

  // The active option, or the first one when it left the list (filter, refresh…).
  const current = options.some((o) => o.key === active)
    ? active
    : (options[0]?.key ?? null)

  const optionId = (key: string) => `${ids}-option-${key}`

  useEffect(() => {
    if (!open || current === null) return
    document
      .getElementById(optionId(current))
      ?.scrollIntoView({ block: "nearest" })
    // `optionId` only depends on `ids`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, current])

  async function runSearch(text: string) {
    latest.current = text
    setSearchState({ phase: "searching", query: text })

    try {
      const found = await search(text)

      if (latest.current === text)
        setSearchState({
          phase: "done",
          query: text,
          ids: found.map((d) => d.id),
        })
    } catch {
      if (latest.current === text)
        setSearchState({ phase: "failed", query: text })
    }
  }

  // Nothing here matches: ask Telegram, once per query.
  useEffect(() => {
    if (!open || localMatches !== 0 || !searchable(trimmed)) return
    if (searchState.phase !== "idle" && searchState.query === trimmed) return
    const timer = window.setTimeout(
      () => void runSearch(trimmed),
      SEARCH_DELAY_MS,
    )

    return () => window.clearTimeout(timer)
    // `runSearch` reads state through refs; `searchState` changes only by it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, localMatches, trimmed])

  function onOpenChange(next: boolean) {
    setOpen(next)
    // Closed with a filter: the next opening starts unfiltered (`options` below too).
    setQuery("")

    if (next) {
      asked.current.clear()
      setUnnamedOpen(false)
      setSearchState({ phase: "idle" })
      latest.current = ""
      // Start on the current choice when it is visible without a filter.
      const start = selected
        ? options.find((o) => o.destination.id === selected.id)
        : undefined
      setActive(start?.key ?? null)
    }
  }

  function selectFilter(next: DestinationFilter) {
    setFilter(next)
    saveFilter(next)
    setActive(null)
  }

  function onTabKeyDown(event: KeyboardEvent<HTMLButtonElement>, at: number) {
    const rtl = getComputedStyle(event.currentTarget).direction === "rtl"
    const last = FILTERS.length - 1
    let target: number

    switch (event.key) {
      case "ArrowRight":
        target = at + (rtl ? -1 : 1)
        break
      case "ArrowLeft":
        target = at + (rtl ? 1 : -1)
        break
      case "Home":
        target = 0
        break
      case "End":
        target = last
        break
      default:
        return
    }

    // Wraps around at both ends.
    const next = target < 0 ? last : target > last ? 0 : target

    event.preventDefault()
    selectFilter(FILTERS[next])
    tabs.current[next]?.focus()
  }

  function toggleForum(forum: Destination) {
    const opening = !expanded.has(forum.id)
    setExpanded((previous) => {
      const next = new Set(previous)

      if (opening) next.add(forum.id)
      else next.delete(forum.id)

      return next
    })

    if (!opening || asked.current.has(forum.id)) return
    asked.current.add(forum.id)
    setTopicsState((previous) => new Map(previous).set(forum.id, "loading"))
    void loadTopics(forum.id).then((ok) =>
      setTopicsState((previous) => {
        const next = new Map(previous)

        if (ok) next.delete(forum.id)
        else next.set(forum.id, "error")

        return next
      }),
    )
  }

  function choose(option: PickerOption) {
    if (option.disabled) return

    if (option.role === "forum") {
      toggleForum(option.destination as Destination)

      return
    }

    if (option.role === "toggle") {
      setUnnamedOpen((previous) => !previous)

      return
    }

    if (option.role === "chat" && isUnnamed(option.destination)) {
      void chooseUnnamed(option.destination as Destination)

      return
    }

    if (option.role === "chat" || option.role === "topic") {
      onChange(String(option.destination.id))
      onOpenChange(false)
    }
  }

  /**
   * A chat without a name (often a deleted account) is chosen only if Telegram allows sending
   * there (`destination_check`, read only); otherwise it stays disabled.
   */
  async function chooseUnnamed(destination: Destination) {
    if (checks.get(destination.id) === "checking") return
    const mark = (state: UnnamedCheck | null) =>
      setChecks((previous) => {
        const next = new Map(previous)

        if (state) next.set(destination.id, state)
        else next.delete(destination.id)

        return next
      })

    mark("checking")

    try {
      const check = await destinationCheck(
        destination.id,
        destination.default_mode,
      )

      if (!check.allowed) {
        mark("denied")

        return
      }

      mark(null)
      onChange(String(destination.id))
      onOpenChange(false)
    } catch {
      mark("denied")
    }
  }

  /** The forum option a nested topic is shown under, in the same group. */
  function parentOf(option: PickerOption): string | null {
    if (option.depth !== 1) return null
    const at = option.key.lastIndexOf("/")

    return at > 0 ? option.key.slice(0, at) : null
  }

  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    const option = options.find((o) => o.key === current)
    const input = event.currentTarget
    const atEnd = input.selectionStart === input.value.length
    const atStart = input.selectionEnd === 0
    let handled = true

    switch (event.key) {
      case "ArrowDown":
        setActive(moveActive(options, current, 1))
        break
      case "ArrowUp":
        setActive(moveActive(options, current, -1))
        break
      case "PageDown":
        setActive(moveActive(options, current, PAGE))
        break
      case "PageUp":
        setActive(moveActive(options, current, -PAGE))
        break
      case "Home":
      case "End":
        if (!event.ctrlKey) {
          handled = false
          break
        }

        setActive(moveActive(options, null, event.key === "Home" ? 1 : -1))
        break
      case "Enter":
        if (option) choose(option)
        break
      case "ArrowRight":
        if (atEnd && option?.role === "forum" && !option.expanded) {
          toggleForum(option.destination as Destination)
        } else if (atEnd && option?.role === "toggle" && !option.expanded) {
          setUnnamedOpen(true)
        } else handled = false
        break
      case "ArrowLeft":
        if (atStart && option?.role === "forum" && option.expanded) {
          toggleForum(option.destination as Destination)
        } else if (atStart && option?.role === "toggle" && option.expanded) {
          setUnnamedOpen(false)
        } else if (atStart && option?.key.startsWith("unnamed/")) {
          setActive("unnamed/toggle")
        } else if (atStart && option && parentOf(option)) {
          setActive(parentOf(option))
        } else handled = false
        break
      default:
        handled = false
    }

    if (handled) event.preventDefault()
  }

  function groupLabel(group: PickerGroup): string {
    switch (group.kind) {
      case "saved":
        return t("destinations.groups.saved")
      case "favorites":
        return t("destinations.favorites")
      case "recent":
        return t("destinations.groups.recent")
      case "folder":
        return group.folder?.name.trim() || t("destinations.groups.folder")
      case "chats":
        return t("destinations.groups.chats")
      case "archived":
        return t("destinations.groups.archived", { count: group.count })
      case "forums":
        return t("destinations.groups.forums", { count: group.count })
      case "unnamed":
        return t("destinations.groups.unnamed", { count: group.count })
      case "results":
        return t("destinations.groups.results")
      case "telegram":
        return t("destinations.groups.telegram")
    }
  }

  const selectedLabel = selected ? destinationName(selected) || unnamed : null
  const searching =
    searchState.phase === "searching" && searchState.query === trimmed
  const searchFailed =
    searchState.phase === "failed" && searchState.query === trimmed
  const nothing = groups.length === 0

  let announcement = ""

  if (open && !trimmed) {
    announcement = t("destinations.filters.announce", {
      name: t(`destinations.filters.${filter}`),
      count: counts[filter],
    })
  } else if (open) {
    announcement = searching
      ? t("destinations.search.searching")
      : nothing
        ? remote
          ? t("destinations.search.nothing")
          : t("destinations.search.noLocal")
        : t("destinations.search.count", {
            count: groups.reduce(
              (sum, g) =>
                sum + g.options.filter((o) => o.role !== "status").length,
              0,
            ),
          })
  }

  return (
    <PopoverPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <PopoverPrimitive.Trigger asChild>
        <Button
          id={id}
          type="button"
          variant="outline"
          aria-labelledby={labelledBy ? `${labelledBy} ${id}` : undefined}
          className="w-full min-w-0 flex-1 justify-between gap-2 px-2.5 font-normal"
        >
          {selected ? (
            <span className="flex min-w-0 items-center gap-1.5">
              <KindIcon destination={selected} />
              <bdi className="truncate" title={selectedLabel ?? undefined}>
                {selectedLabel}
              </bdi>
              {selected.archived && (
                <Archive
                  className="size-3.5 text-muted-foreground"
                  aria-label={t("destinations.badges.archived")}
                />
              )}
            </span>
          ) : (
            <span className="truncate text-muted-foreground">
              {t("add.destination.placeholder")}
            </span>
          )}
          <ChevronsUpDown className="text-muted-foreground" aria-hidden />
        </Button>
      </PopoverPrimitive.Trigger>
      <PopoverPrimitive.Portal>
        <PopoverPrimitive.Content
          align="start"
          sideOffset={4}
          collisionPadding={8}
          aria-label={t("destinations.panelLabel")}
          onOpenAutoFocus={(event) => {
            event.preventDefault()
            inputRef.current?.focus()
          }}
          className="z-50 flex max-h-(--radix-popover-content-available-height) w-(--radix-popover-trigger-width) min-w-72 max-w-[min(32rem,calc(100vw-1rem))] flex-col overflow-hidden rounded-lg bg-popover text-popover-foreground shadow-md ring-1 ring-foreground/10 data-open:animate-in data-open:fade-in-0 data-closed:animate-out data-closed:fade-out-0"
        >
          <div className="flex items-center gap-2 border-b ps-2.5 pe-1.5">
            <Search
              className="size-4 shrink-0 text-muted-foreground"
              aria-hidden
            />
            <input
              ref={inputRef}
              role="combobox"
              aria-expanded="true"
              aria-controls={listId}
              aria-autocomplete="list"
              aria-activedescendant={
                current === null ? undefined : optionId(current)
              }
              aria-label={t("destinations.search.label")}
              dir="auto"
              autoComplete="off"
              spellCheck={false}
              value={query}
              placeholder={t("destinations.search.placeholder")}
              className="h-9 min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground"
              onChange={(event) => {
                setQuery(event.target.value)
                setActive(null)
              }}
              onKeyDown={onKeyDown}
            />
            {searching && (
              <LoaderCircle
                className="size-4 shrink-0 animate-spin text-muted-foreground"
                aria-hidden
              />
            )}
          </div>

          <div
            role="tablist"
            aria-label={t("destinations.filters.label")}
            className="flex flex-wrap gap-1 border-b px-1.5 py-1.5"
          >
            {FILTERS.map((name, at) => (
              <button
                key={name}
                ref={(node) => {
                  tabs.current[at] = node
                }}
                type="button"
                role="tab"
                aria-selected={name === filter}
                aria-controls={listId}
                tabIndex={name === filter ? 0 : -1}
                className={cn(
                  "flex h-7 items-center gap-1.5 rounded-full px-2.5 text-xs outline-none select-none focus-visible:ring-2 focus-visible:ring-ring/50",
                  name === filter
                    ? "bg-accent font-medium text-accent-foreground ring-1 ring-foreground/10"
                    : "text-muted-foreground hover:bg-accent/50",
                )}
                // Keeps the focus in the filter field when clicked.
                onMouseDown={(event) => event.preventDefault()}
                onClick={() => selectFilter(name)}
                onKeyDown={(event) => onTabKeyDown(event, at)}
              >
                {t(`destinations.filters.${name}`)}
                <span className="tabular-nums opacity-70">{counts[name]}</span>
              </button>
            ))}
          </div>

          <div
            id={listId}
            role="listbox"
            aria-label={t("destinations.listLabel")}
            className="max-h-80 min-h-0 overflow-y-auto overscroll-contain p-1"
          >
            {groups.map((group) => {
              const headingId = `${ids}-group-${group.key}`

              return (
                <div
                  key={group.key}
                  role="group"
                  aria-labelledby={headingId}
                  className="pb-1"
                >
                  <div
                    id={headingId}
                    role="presentation"
                    className="flex items-center gap-1.5 px-2 pt-1.5 pb-1 text-xs font-medium text-muted-foreground"
                  >
                    <GroupIcon group={group} />
                    <bdi className="truncate">{groupLabel(group)}</bdi>
                  </div>
                  {(group.kind === "unnamed" || group.kind === "telegram") && (
                    <p
                      id={`${headingId}-hint`}
                      role="presentation"
                      className="px-2 pb-1 text-xs text-muted-foreground"
                    >
                      {group.kind === "unnamed"
                        ? t("destinations.unnamedHint")
                        : t("destinations.telegramHint")}
                    </p>
                  )}
                  {group.options.map((option) => (
                    <OptionRow
                      key={option.key}
                      id={optionId(option.key)}
                      describedBy={
                        option.role === "toggle"
                          ? `${headingId}-hint`
                          : undefined
                      }
                      option={option}
                      group={group}
                      active={option.key === current}
                      selected={
                        (option.role === "chat" || option.role === "topic") &&
                        option.destination.id === selected?.id
                      }
                      onHover={() => {
                        if (!option.disabled) setActive(option.key)
                      }}
                      onChoose={() => choose(option)}
                    />
                  ))}
                </div>
              )
            })}
          </div>

          {nothing && (
            <p className="px-3 pb-2 text-sm text-muted-foreground">
              {trimmed
                ? searching
                  ? t("destinations.search.searching")
                  : remote
                    ? t("destinations.search.nothing")
                    : t("destinations.search.noLocal")
                : list.length > 0
                  ? t("destinations.filters.empty")
                  : t("add.destination.empty")}
            </p>
          )}

          {trimmed !== "" && (
            <div className="flex items-center gap-2 border-t px-1.5 py-1">
              {searchFailed && (
                <p className="min-w-0 flex-1 px-1 text-xs text-destructive">
                  {t("destinations.search.failed")}
                </p>
              )}
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className={cn(
                  "min-w-0 justify-start",
                  !searchFailed && "flex-1",
                )}
                disabled={searching || !searchable(trimmed)}
                onClick={() => void runSearch(trimmed)}
              >
                <Globe data-icon="inline-start" aria-hidden />
                <span className="truncate">
                  {searching
                    ? t("destinations.search.searching")
                    : t("destinations.search.telegram", { query: trimmed })}
                </span>
              </Button>
            </div>
          )}

          <p role="status" aria-live="polite" className="sr-only">
            {announcement}
          </p>
        </PopoverPrimitive.Content>
      </PopoverPrimitive.Portal>
    </PopoverPrimitive.Root>
  )
}

interface OptionRowProps {
  id: string
  describedBy?: string
  option: PickerOption
  group: PickerGroup
  active: boolean
  selected: boolean
  onHover: () => void
  onChoose: () => void
}

function OptionRow({
  id,
  describedBy,
  option,
  group,
  active,
  selected,
  onHover,
  onChoose,
}: OptionRowProps) {
  const { t } = useTranslation()
  const destination = option.destination as Destination

  if (option.role === "status") {
    return (
      <div
        id={id}
        role="option"
        aria-selected={false}
        aria-disabled="true"
        className="flex min-h-8 items-center gap-2 rounded-md py-1.5 ps-8 pe-2 text-xs text-muted-foreground"
      >
        {option.status === "loading" && (
          <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
        )}
        {t(`destinations.topics.${option.status ?? "empty"}`)}
      </div>
    )
  }

  if (option.role === "toggle") {
    return (
      <div
        id={id}
        role="option"
        aria-selected={false}
        aria-describedby={describedBy}
        data-active={active || undefined}
        className={cn(
          "flex min-h-8 cursor-default items-center gap-2 rounded-md py-1.5 ps-2 pe-2 text-sm text-muted-foreground select-none",
          active && "bg-accent text-accent-foreground",
        )}
        onMouseDown={(event) => event.preventDefault()}
        onMouseMove={onHover}
        onClick={onChoose}
      >
        <ChevronRight
          className={cn(
            "size-4 shrink-0 transition-transform rtl:rotate-180",
            option.expanded && "rotate-90 rtl:rotate-90",
          )}
          aria-hidden
        />
        {t(
          option.expanded
            ? "destinations.unnamedHide"
            : "destinations.unnamedShow",
        )}
      </div>
    )
  }

  // Badges that the group does not already say.
  const archived = destination.archived && group.kind !== "archived"
  const readOnly = destination.can_write === false && !option.check
  const unlisted = !destination.listed

  return (
    <div
      id={id}
      role="option"
      aria-selected={selected}
      aria-disabled={option.disabled || undefined}
      data-active={active || undefined}
      className={cn(
        "flex min-h-8 cursor-default items-center gap-2 rounded-md py-1.5 pe-2 text-sm select-none",
        option.depth === 1 ? "ps-8" : "ps-2",
        active && "bg-accent text-accent-foreground",
        (option.disabled || readOnly) && "text-muted-foreground",
      )}
      // Keeps the focus in the filter field.
      onMouseDown={(event) => event.preventDefault()}
      onMouseMove={onHover}
      onClick={onChoose}
    >
      {option.role === "forum" ? (
        <ChevronRight
          className={cn(
            "size-4 shrink-0 text-muted-foreground transition-transform rtl:rotate-180",
            option.expanded && "rotate-90 rtl:rotate-90",
          )}
          aria-hidden
        />
      ) : option.depth === 1 ? null : (
        <KindIcon destination={destination} />
      )}
      <bdi className="min-w-0 flex-1 truncate" title={option.label}>
        {option.label}
      </bdi>
      {option.role === "forum" && (
        <span className="sr-only">
          {t(
            option.expanded
              ? "destinations.topics.hide"
              : "destinations.topics.show",
          )}
        </span>
      )}
      {option.role === "topic" && destination.topic_closed && (
        <span className="flex shrink-0 items-center gap-1 text-xs">
          <Lock className="size-3" aria-hidden />
          {t("destinations.badges.closed")}
        </span>
      )}
      {archived && (
        <span className="flex shrink-0 items-center gap-1 text-xs text-muted-foreground">
          <Archive className="size-3" aria-hidden />
          {t("destinations.badges.archived")}
        </span>
      )}
      {unlisted && (
        <span className="shrink-0 text-xs text-muted-foreground">
          {t("destinations.badges.unlisted")}
        </span>
      )}
      {readOnly && (
        <span className="shrink-0 text-xs text-muted-foreground">
          {t("destinations.badges.readOnly")}
        </span>
      )}
      {option.check === "checking" && (
        <span className="flex shrink-0 items-center gap-1 text-xs text-muted-foreground">
          <LoaderCircle className="size-3 animate-spin" aria-hidden />
          {t("destinations.badges.checking")}
        </span>
      )}
      {option.check === "denied" && (
        <span className="shrink-0 text-xs">
          {t("destinations.badges.cannotSend")}
        </span>
      )}
      {selected && <Check className="size-4 shrink-0" aria-hidden />}
    </div>
  )
}

function KindIcon({ destination }: { destination: Destination }) {
  const className = "size-4 shrink-0 text-muted-foreground"

  if (destination.is_saved)
    return <Bookmark className={className} aria-hidden />
  if (destination.is_forum)
    return <MessagesSquare className={className} aria-hidden />
  if (destination.is_broadcast || destination.kind === "channel")
    return <Megaphone className={className} aria-hidden />
  if (destination.peer_kind === "user")
    return <User className={className} aria-hidden />

  return <Users className={className} aria-hidden />
}

function GroupIcon({ group }: { group: PickerGroup }) {
  const className = "size-3.5 shrink-0"

  switch (group.kind) {
    case "favorites":
      return <Star className={className} aria-hidden />
    case "recent":
      return <Clock className={className} aria-hidden />
    case "folder":
      return <Folder className={className} aria-hidden />
    case "archived":
      return <Archive className={className} aria-hidden />
    case "forums":
      return <MessagesSquare className={className} aria-hidden />
    case "telegram":
      return <Globe className={className} aria-hidden />
    default:
      return null
  }
}
