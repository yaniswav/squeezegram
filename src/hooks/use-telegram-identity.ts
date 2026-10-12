import { useEffect, useState } from "react"
import { telegramIdentity, type TelegramIdentity } from "@/lib/ipc"

/**
 * Default device name and personal API identity, read once (the core fixes them at startup);
 * the setter takes what a change returns. `null` while it is being read or if the core cannot
 * say.
 */
export function useTelegramIdentity(): [
  TelegramIdentity | null,
  (identity: TelegramIdentity) => void,
] {
  const [identity, setIdentity] = useState<TelegramIdentity | null>(null)

  useEffect(() => {
    let stale = false

    telegramIdentity().then(
      (next) => {
        if (!stale) setIdentity(next)
      },
      () => {
        if (!stale) setIdentity(null)
      },
    )

    return () => {
      stale = true
    }
  }, [])

  return [identity, setIdentity]
}
