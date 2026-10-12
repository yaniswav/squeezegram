// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { test } from "node:test"
import {
  isGroupOrChannel,
  needsGroupRiskNotice,
  type RiskDestination,
} from "./group-risk.ts"

const saved: RiskDestination = { peer_kind: "user", is_saved: true }
const privateChat: RiskDestination = { peer_kind: "user", is_saved: false }
const basicGroup: RiskDestination = {
  peer_kind: "basic_group",
  is_saved: false,
}
const channel: RiskDestination = { peer_kind: "channel", is_saved: false }

test("groups, supergroups, channels and their topics are risky; Saved Messages and private chats are not", () => {
  assert.equal(isGroupOrChannel(basicGroup), true)
  assert.equal(isGroupOrChannel(channel), true)
  assert.equal(isGroupOrChannel(saved), false)
  assert.equal(isGroupOrChannel(privateChat), false)
})

test("the notice is due for a group until it is acknowledged", () => {
  assert.equal(needsGroupRiskNotice([channel], false), true)
  assert.equal(needsGroupRiskNotice([channel], true), false)
})

test("one group in a batch is enough, and Saved Messages or private chats alone never block", () => {
  assert.equal(
    needsGroupRiskNotice([saved, privateChat, basicGroup], false),
    true,
  )
  assert.equal(needsGroupRiskNotice([saved, privateChat], false), false)
  assert.equal(needsGroupRiskNotice([], false), false)
})
