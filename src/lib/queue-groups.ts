// One card per file of a multi-destination batch. The core keeps one item per (file,
// destination): `add_files` (commands.rs) queues the items of a file one after the other, one
// per destination, in a single `Store::add_items` call that gives them the same `created_at`
// and the same `batch_index`/`batch_count`. Only the display groups them.

/** What the grouping reads of an `Item`. */
export interface Groupable {
  id: number
  source_path: string
  destination_id: number
  created_at: string
  batch_index: number
  batch_count: number
  state: string
}

export interface ItemGroup<T extends Groupable> {
  /** Stable while the group's first item stays: the React key. */
  key: string
  /** In queue order; one per destination. */
  items: T[]
}

const FINISHED: ReadonlySet<string> = new Set(["sent", "cancelled"])

/** Queued by the same `queue_add` call for the same file. */
function sameBatchFile(a: Groupable, b: Groupable): boolean {
  return (
    a.source_path === b.source_path &&
    a.created_at === b.created_at &&
    a.batch_index === b.batch_index &&
    a.batch_count === b.batch_count
  )
}

/**
 * Groups `items` (in queue order) by file of a batch: consecutive items of the same call for
 * the same file, each for another destination. Items moved apart (by an older build that
 * reordered them one by one) stay apart.
 */
export function groupBatches<T extends Groupable>(
  items: readonly T[],
): ItemGroup<T>[] {
  const groups: ItemGroup<T>[] = []

  for (const item of items) {
    const last = groups.at(-1)

    if (
      last &&
      sameBatchFile(last.items[0], item) &&
      !last.items.some((other) => other.destination_id === item.destination_id)
    ) {
      last.items.push(item)
    } else {
      groups.push({ key: `item-${item.id}`, items: [item] })
    }
  }

  return groups
}

/** Nothing more will happen to any of its items: it goes to the finished list. */
export function isGroupFinished(group: ItemGroup<Groupable>): boolean {
  return group.items.every((item) => FINISHED.has(item.state))
}

/**
 * The item order for `queue_reorder` once the group at `index` has moved one step: its items
 * stay together. Null when it is already at that end.
 */
export function moveGroup<T extends Groupable>(
  groups: readonly ItemGroup<T>[],
  index: number,
  direction: -1 | 1,
): number[] | null {
  const target = index + direction

  if (index < 0 || index >= groups.length) return null
  if (target < 0 || target >= groups.length) return null
  const order = [...groups]
  ;[order[index], order[target]] = [order[target], order[index]]

  return order.flatMap((group) => group.items.map((item) => item.id))
}
