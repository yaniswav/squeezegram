import { History } from "lucide-react"
import { useCallback, useEffect, useId, useState } from "react"
import { useTranslation } from "react-i18next"
import { ClearHistory } from "@/components/clear-history"
import { EmptyState } from "@/components/empty-state"
import { PageHeader } from "@/components/page-header"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Label } from "@/components/ui/label"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { useFlash } from "@/hooks/use-flash"
import { formatBytes, formatDateTime } from "@/lib/format"
import { historyQuery, type HistoryEntry, type HistoryStatus } from "@/lib/ipc"
import { cn } from "@/lib/utils"
import { useAlerts } from "@/state/alerts"
import { destinationName, useDestinations } from "@/state/destinations"

const PAGE_SIZE = 100

const FILTERS = ["all", "sent", "failed", "unknown"] as const

type Filter = (typeof FILTERS)[number]

const STATUS_CLASS: Record<HistoryStatus, string> = {
  sent: "bg-success/15 text-success",
  failed: "bg-destructive/10 text-destructive dark:bg-destructive/20",
  unknown: "bg-warning/15 text-warning",
}

function isHistoryStatus(value: string): value is HistoryStatus {
  return value === "sent" || value === "failed" || value === "unknown"
}

function isMode(value: string): value is "video" | "document" | "photo" {
  return value === "video" || value === "document" || value === "photo"
}

function queryPage(which: Filter, offset: number): Promise<HistoryEntry[]> {
  return historyQuery({
    status: which === "all" ? undefined : which,
    limit: PAGE_SIZE,
    offset,
  })
}

export function HistoryPage({ focusTitle }: { focusTitle: boolean }) {
  const { t, i18n } = useTranslation()
  const language = i18n.language
  const { pushError } = useAlerts()
  const { byId } = useDestinations()
  const ids = useId()
  const [filter, setFilter] = useState<Filter>("all")
  const [entries, setEntries] = useState<HistoryEntry[]>([])
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading")
  const [hasMore, setHasMore] = useState(false)
  const [cleared, setCleared] = useFlash()

  const onFailed = useCallback(
    (cause: unknown) => {
      setStatus("error")
      pushError(cause)
    },
    [pushError],
  )

  const load = useCallback(
    (which: Filter, offset: number) =>
      queryPage(which, offset).then((page) => {
        setEntries((current) => (offset === 0 ? page : [...current, ...page]))
        setHasMore(page.length === PAGE_SIZE)
        setStatus("ready")
      }, onFailed),
    [onFailed],
  )

  useEffect(() => {
    let current = true
    queryPage(filter, 0).then((page) => {
      if (!current) return
      setEntries(page)
      setHasMore(page.length === PAGE_SIZE)
      setStatus("ready")
    }, onFailed)

    return () => {
      current = false
    }
  }, [filter, onFailed])

  function changeFilter(next: Filter) {
    setStatus("loading")
    setFilter(next)
  }

  function onCleared(removed: number) {
    setCleared(
      removed === 0
        ? t("history.clear.nothing")
        : t("history.clear.removed", { count: removed }),
    )
    setEntries([])
    setHasMore(false)
    void load(filter, 0)
  }

  return (
    <div className="space-y-6">
      <PageHeader
        title={t("history.title")}
        description={t("history.description")}
        focusTitle={focusTitle}
        actions={
          <ClearHistory
            disabled={
              status !== "ready" || (entries.length === 0 && filter === "all")
            }
            onCleared={onCleared}
          />
        }
      />

      <p role="status" className={cleared ? "text-sm" : "sr-only"}>
        {cleared}
      </p>

      <div className="flex items-center gap-3">
        <Label htmlFor={`${ids}-filter`}>{t("history.filter.label")}</Label>
        <Select
          value={filter}
          onValueChange={(value) => {
            const next = FILTERS.find((candidate) => candidate === value)

            if (next) changeFilter(next)
          }}
        >
          <SelectTrigger id={`${ids}-filter`} className="w-44">
            <SelectValue />
          </SelectTrigger>
          <SelectContent position="popper">
            {FILTERS.map((value) => (
              <SelectItem key={value} value={value}>
                {t(`history.filter.${value}`)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      <Card className="gap-0 py-0">
        {status === "error" && (
          <div className="flex items-center justify-between gap-3 p-4 text-sm">
            <p className="text-destructive">{t("history.loadError")}</p>
            <Button
              variant="outline"
              size="sm"
              onClick={() => void load(filter, 0)}
            >
              {t("common.retry")}
            </Button>
          </div>
        )}
        {status === "loading" && entries.length === 0 && (
          <p className="p-4 text-sm text-muted-foreground">
            {t("common.loading")}
          </p>
        )}
        {status === "ready" && entries.length === 0 && (
          <EmptyState icon={History} title={t("history.empty")} />
        )}

        {entries.length > 0 && (
          // Scrollable region: focusable so keyboard users can scroll it.
          <div
            className="overflow-x-auto"
            tabIndex={0}
            role="region"
            aria-labelledby={`${ids}-caption`}
          >
            <table
              className={cn(
                "w-full text-sm",
                status === "loading" && "opacity-60",
              )}
            >
              <caption id={`${ids}-caption`} className="sr-only">
                {t("history.tableCaption")}
              </caption>
              <thead className="border-b text-xs text-muted-foreground">
                <tr>
                  <th scope="col" className="px-4 py-2 text-start font-medium">
                    {t("history.column.file")}
                  </th>
                  <th scope="col" className="px-3 py-2 text-start font-medium">
                    {t("history.column.destination")}
                  </th>
                  <th scope="col" className="px-3 py-2 text-end font-medium">
                    {t("history.column.size")}
                  </th>
                  <th scope="col" className="px-3 py-2 text-start font-medium">
                    {t("history.column.mode")}
                  </th>
                  <th scope="col" className="px-3 py-2 text-start font-medium">
                    {t("history.column.status")}
                  </th>
                  <th scope="col" className="px-4 py-2 text-start font-medium">
                    {t("history.column.date")}
                  </th>
                </tr>
              </thead>
              <tbody className="divide-y">
                {entries.map((entry) => {
                  const destination = byId.get(entry.destination_id)

                  return (
                    <tr key={entry.id}>
                      <th
                        scope="row"
                        className="max-w-56 truncate px-4 py-2.5 text-start font-medium"
                        title={entry.source_path}
                      >
                        <bdi>{entry.file_name}</bdi>
                      </th>
                      <td className="px-3 py-2.5">
                        <bdi>
                          {destination
                            ? destinationName(destination, t)
                            : t("common.destinationNumber", {
                                id: entry.destination_id,
                              })}
                        </bdi>
                      </td>
                      <td className="px-3 py-2.5 text-end whitespace-nowrap tabular-nums">
                        {formatBytes(entry.size_sent, language)}
                      </td>
                      <td className="px-3 py-2.5">
                        {isMode(entry.mode)
                          ? t(`mode.${entry.mode}`)
                          : entry.mode}
                      </td>
                      <td className="px-3 py-2.5">
                        <div className="flex flex-wrap gap-1">
                          {isHistoryStatus(entry.status) ? (
                            <Badge className={STATUS_CLASS[entry.status]}>
                              {t(`history.status.${entry.status}`)}
                            </Badge>
                          ) : (
                            <Badge variant="outline">{entry.status}</Badge>
                          )}
                          {entry.encoded && (
                            <Badge variant="outline">
                              {t("history.compressed")}
                            </Badge>
                          )}
                          {entry.imported_from && (
                            <Badge variant="outline">
                              {t("history.imported")}
                            </Badge>
                          )}
                        </div>
                      </td>
                      <td className="px-4 py-2.5 whitespace-nowrap text-muted-foreground">
                        <time dateTime={entry.sent_at}>
                          {formatDateTime(new Date(entry.sent_at), language)}
                        </time>
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
        )}

        {hasMore && status === "ready" && (
          <div className="border-t p-3 text-center">
            <Button
              variant="outline"
              size="sm"
              onClick={() => void load(filter, entries.length)}
            >
              {t("history.loadMore")}
            </Button>
          </div>
        )}
      </Card>
    </div>
  )
}
