import { useEffect, useState } from "react"

/** Current time, refreshed every `intervalMs` while `enabled`. */
export function useNow(enabled: boolean, intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!enabled) return
    const timer = window.setInterval(() => setNow(Date.now()), intervalMs)

    return () => window.clearInterval(timer)
  }, [enabled, intervalMs])

  return now
}
