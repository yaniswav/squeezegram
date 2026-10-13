// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { test } from "node:test"
import {
  groupBatches,
  isGroupFinished,
  moveGroup,
  type Groupable,
} from "./queue-groups.ts"

const CALL_1 = "2026-10-12T09:00:00.123Z"
const CALL_2 = "2026-10-12T09:00:05.456Z"

function item(
  id: number,
  source_path: string,
  destination_id: number,
  extra: Partial<Groupable> = {},
): Groupable {
  return {
    id,
    source_path,
    destination_id,
    created_at: CALL_1,
    batch_index: 1,
    batch_count: 1,
    state: "ready",
    ...extra,
  }
}

const ids = (groups: { items: Groupable[] }[]) =>
  groups.map((group) => group.items.map((one) => one.id))

test("the items of a file sent to several destinations form one group", () => {
  // `add_files` with two files and two destinations: a.mp4 → 1, 2, then b.mp4 → 1, 2.
  const items = [
    item(1, "C:\\v\\a.mp4", 1, { batch_index: 1, batch_count: 2 }),
    item(2, "C:\\v\\a.mp4", 2, { batch_index: 1, batch_count: 2 }),
    item(3, "C:\\v\\b.mp4", 1, { batch_index: 2, batch_count: 2 }),
    item(4, "C:\\v\\b.mp4", 2, { batch_index: 2, batch_count: 2 }),
  ]

  assert.deepEqual(ids(groupBatches(items)), [
    [1, 2],
    [3, 4],
  ])
  assert.equal(groupBatches(items)[1].key, "item-3")
})

test("a single destination keeps one group per item", () => {
  const items = [
    item(1, "a.mp4", 1, { batch_index: 1, batch_count: 2 }),
    item(2, "b.mp4", 1, { batch_index: 2, batch_count: 2 }),
  ]

  assert.deepEqual(ids(groupBatches(items)), [[1], [2]])
})

test("the same file queued by two calls stays two cards", () => {
  const items = [
    item(1, "a.mp4", 1),
    item(2, "a.mp4", 2, { created_at: CALL_2 }),
  ]

  assert.deepEqual(ids(groupBatches(items)), [[1], [2]])
})

test("the same file twice in one call for one destination is not merged", () => {
  // Two entries of one call for the same destination: different batch positions.
  const items = [
    item(1, "a.mp4", 1, { batch_index: 1, batch_count: 2 }),
    item(2, "a.mp4", 1, { batch_index: 2, batch_count: 2 }),
  ]

  assert.deepEqual(ids(groupBatches(items)), [[1], [2]])

  // Same position too (a hand-written row): a destination appears once per card.
  assert.deepEqual(
    ids(groupBatches([item(1, "a.mp4", 1), item(2, "a.mp4", 1)])),
    [[1], [2]],
  )
})

test("items of a batch moved apart are not regrouped", () => {
  const items = [
    item(1, "a.mp4", 1),
    item(3, "b.mp4", 3, { created_at: CALL_2 }),
    item(2, "a.mp4", 2),
  ]

  assert.deepEqual(ids(groupBatches(items)), [[1], [3], [2]])
})

test("a group is finished once each of its items is sent or cancelled", () => {
  const [group] = groupBatches([
    item(1, "a.mp4", 1, { state: "sent" }),
    item(2, "a.mp4", 2, { state: "uploading" }),
  ])

  assert.equal(isGroupFinished(group), false)
  group.items[1] = { ...group.items[1], state: "cancelled" }
  assert.equal(isGroupFinished(group), true)
  group.items[1] = { ...group.items[1], state: "failed" }
  assert.equal(isGroupFinished(group), false)
})

test("moving a group moves all of its items", () => {
  const groups = groupBatches([
    item(1, "a.mp4", 1),
    item(2, "a.mp4", 2),
    item(3, "b.mp4", 1, { created_at: CALL_2 }),
    item(4, "c.mp4", 1, { created_at: CALL_2, batch_index: 2 }),
  ])

  assert.deepEqual(moveGroup(groups, 0, 1), [3, 1, 2, 4])
  assert.deepEqual(moveGroup(groups, 2, -1), [1, 2, 4, 3])
  assert.equal(moveGroup(groups, 0, -1), null)
  assert.equal(moveGroup(groups, 2, 1), null)
})
