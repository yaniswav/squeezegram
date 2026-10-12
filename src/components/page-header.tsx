import { useEffect, useRef, type ReactNode } from "react"

interface PageHeaderProps {
  title: string
  description?: ReactNode
  /** Buttons at the end of the header. */
  actions?: ReactNode
  /** Moves focus to the title, so screen readers announce the page after navigation. */
  focusTitle: boolean
}

export function PageHeader({
  title,
  description,
  actions,
  focusTitle,
}: PageHeaderProps) {
  const ref = useRef<HTMLHeadingElement>(null)
  useEffect(() => {
    if (focusTitle) ref.current?.focus()
  }, [focusTitle])

  return (
    <header className="flex flex-wrap items-start justify-between gap-3">
      <div className="space-y-1">
        <h1
          ref={ref}
          tabIndex={-1}
          className="text-xl font-semibold tracking-tight outline-none"
        >
          {title}
        </h1>
        {description && (
          <p className="text-sm text-muted-foreground">{description}</p>
        )}
      </div>
      {actions}
    </header>
  )
}
