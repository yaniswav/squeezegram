import { useEffect, useRef, useState } from "react"
import { onFileDrop } from "@/lib/ipc"

/**
 * Native drag-and-drop over the whole window (the browser DnD API hides file paths).
 * `onDrop` receives the real paths.
 */
export function useFileDrop(onDrop: (paths: string[]) => void) {
  const [isDragging, setIsDragging] = useState(false)
  const onDropRef = useRef(onDrop)

  useEffect(() => {
    onDropRef.current = onDrop
  }, [onDrop])

  useEffect(
    () =>
      onFileDrop((event) => {
        switch (event.type) {
          case "enter":
          case "over":
            setIsDragging(true)
            break
          case "leave":
            setIsDragging(false)
            break
          case "drop":
            setIsDragging(false)

            if (event.paths.length > 0) onDropRef.current(event.paths)
            break
        }
      }),
    [],
  )

  return isDragging
}
