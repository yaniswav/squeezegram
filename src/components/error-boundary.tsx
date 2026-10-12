import { Component, type ErrorInfo, type ReactNode } from "react"
import { useTranslation } from "react-i18next"
import { Button } from "@/components/ui/button"
import { logReactError } from "@/lib/ipc"

interface Props {
  children: ReactNode
}

interface State {
  failed: boolean
}

/** Catches render errors, writes them to the journal, then shows a recovery screen. */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { failed: false }

  static getDerivedStateFromError(): State {
    return { failed: true }
  }

  componentDidCatch(cause: unknown, info: ErrorInfo) {
    logReactError(cause, info.componentStack)
  }

  render() {
    if (this.state.failed)
      return <RecoveryScreen onRetry={() => this.setState({ failed: false })} />

    return this.props.children
  }
}

function RecoveryScreen({ onRetry }: { onRetry: () => void }) {
  const { t } = useTranslation()

  return (
    <main className="flex h-svh items-center justify-center p-6">
      <div role="alert" className="max-w-md space-y-4 text-center">
        <h1 className="text-lg font-semibold">{t("recovery.title")}</h1>
        <p className="text-sm text-muted-foreground">{t("recovery.body")}</p>
        <div className="flex flex-wrap justify-center gap-2">
          <Button variant="outline" onClick={onRetry}>
            {t("recovery.retry")}
          </Button>
          <Button onClick={() => window.location.reload()}>
            {t("recovery.reload")}
          </Button>
        </div>
      </div>
    </main>
  )
}
