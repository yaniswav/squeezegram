// When the one-time warning about sending to groups applies (F33). Pure (no React, no
// imports) so `node --test` runs it as is: see group-risk.test.ts.

/** The fields of `Destination` (ipc.ts) the warning reads. */
export interface RiskDestination {
  peer_kind: string
  is_saved: boolean
}

/**
 * A group, supergroup or channel (topics included). Saved Messages and private chats are
 * both `user` peers: sending there carries no such risk, so they never trigger the warning.
 */
export function isGroupOrChannel(destination: RiskDestination): boolean {
  return !destination.is_saved && destination.peer_kind !== "user"
}

/** The warning is shown before the first send to a group, until the user has accepted it. */
export function needsGroupRiskNotice(
  destinations: readonly RiskDestination[],
  acknowledged: boolean,
): boolean {
  return !acknowledged && destinations.some(isGroupOrChannel)
}
