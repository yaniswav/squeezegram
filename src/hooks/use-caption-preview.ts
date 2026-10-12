import { useEffect, useState } from "react"
import { captionPreview, isDesktop } from "@/lib/ipc"

const DEBOUNCE_MS = 150

/**
 * The template rendered for the first file by the core, or null while there is nothing to
 * render or the core can't. Rendering stays in Rust so the preview cannot drift from the send.
 */
export function useCaptionPreview(
  template: string,
  firstPath: string | undefined,
  count: number,
): string | null {
  const [rendered, setRendered] = useState<{
    key: string
    text: string
  } | null>(null)
  const key = JSON.stringify([template, firstPath, count])
  const active = template.trim() !== "" && isDesktop()

  useEffect(() => {
    if (!active) return

    let stale = false

    const timer = setTimeout(() => {
      captionPreview(template, firstPath, count).then(
        (text) => {
          if (!stale) setRendered({ key, text })
        },
        () => {
          if (!stale) setRendered(null)
        },
      )
    }, DEBOUNCE_MS)

    return () => {
      stale = true
      clearTimeout(timer)
    }
  }, [active, key, template, firstPath, count])

  // A result for an older template is not shown while the new one is being rendered.
  return active && rendered?.key === key ? rendered.text : null
}
