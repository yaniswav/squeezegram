import type { LucideIcon } from "lucide-react"
import type { ReactNode } from "react"

interface EmptyStateProps {
  icon: LucideIcon
  title: string
  children?: ReactNode
}

export function EmptyState({ icon: Icon, title, children }: EmptyStateProps) {
  return (
    <div className="flex flex-col items-center gap-2 px-6 py-10 text-center">
      <Icon className="size-8 text-muted-foreground" aria-hidden />
      <p className="text-sm font-medium">{title}</p>
      {children && (
        <div className="max-w-md text-sm text-muted-foreground">{children}</div>
      )}
    </div>
  )
}
