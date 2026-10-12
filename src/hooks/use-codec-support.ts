import { useEffect, useState } from "react"
import { codecSupport, type CodecSupport } from "@/lib/ipc"

/**
 * Whether HEVC can be encoded here, asked once `enabled` (the core runs a test encode of the
 * GPU encoders the first time). `null` while it is being asked or if the core cannot say.
 */
export function useCodecSupport(enabled: boolean): CodecSupport | null {
  const [support, setSupport] = useState<CodecSupport | null>(null)

  useEffect(() => {
    if (!enabled) return

    let stale = false

    codecSupport().then(
      (next) => {
        if (!stale) setSupport(next)
      },
      () => {
        if (!stale) setSupport(null)
      },
    )

    return () => {
      stale = true
    }
  }, [enabled])

  return support
}
