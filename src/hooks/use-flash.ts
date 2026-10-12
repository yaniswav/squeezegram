import { useEffect, useState } from "react"

/** A short status message that goes away by itself. */
export function useFlash(durationMs = 8000) {
  const [message, setMessage] = useState<string | null>(null)
  useEffect(() => {
    if (message === null) return
    const timer = window.setTimeout(() => setMessage(null), durationMs)

    return () => window.clearTimeout(timer)
  }, [message, durationMs])

  return [message, setMessage] as const
}
