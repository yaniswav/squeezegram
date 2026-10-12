import { useTranslation } from "react-i18next"
import { AddPanel } from "@/components/add-panel"
import { PageHeader } from "@/components/page-header"
import { QueueList } from "@/components/queue-list"

interface QueuePageProps {
  focusTitle: boolean
  stagedPaths: string[]
  onStagedPathsChange: (paths: string[]) => void
}

export function QueuePage({
  focusTitle,
  stagedPaths,
  onStagedPathsChange,
}: QueuePageProps) {
  const { t } = useTranslation()

  return (
    <div className="space-y-6">
      <PageHeader title={t("app.nav.queue")} focusTitle={focusTitle} />
      <AddPanel paths={stagedPaths} onPathsChange={onStagedPathsChange} />
      <QueueList />
    </div>
  )
}
