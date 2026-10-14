export type DeliveryRoute = "upload" | "copy" | "remote"

interface DeliveryItem {
  state: string
  send_via: DeliveryRoute
  source_item_id: number | null
}

/** Progress names a fallback before the next queue snapshot arrives. Terminal rows use
 * the persisted route, so a refused copy followed by an upload is shown as an upload. */
export function deliveryRoute(
  item: DeliveryItem,
  progress?: { detail?: string | null; phase?: string },
): DeliveryRoute {
  if (item.state === "uploading") {
    if (progress?.detail === "copy" || progress?.detail === "remote") {
      return progress.detail
    }

    if (progress?.phase === "uploading") return "upload"

    return item.send_via
  }

  if (
    item.source_item_id !== null &&
    ["ready", "probing", "encoding", "remuxing", "encoded"].includes(item.state)
  ) {
    return "copy"
  }

  return item.send_via
}
