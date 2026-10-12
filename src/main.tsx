import { StrictMode } from "react"
import { createRoot } from "react-dom/client"
import App from "./App"
import { ErrorBoundary } from "./components/error-boundary"
import { initI18n } from "./i18n"
import { installFrontendLogging } from "./lib/ipc"
import { initTheme } from "./lib/theme"
import "./index.css"

const container = document.getElementById("root")

if (!container) throw new Error("Missing #root element")

// First, so errors of the start-up below reach the journal too.
installFrontendLogging()

initTheme()

void initI18n().then(() => {
  createRoot(container).render(
    <StrictMode>
      <ErrorBoundary>
        <App />
      </ErrorBoundary>
    </StrictMode>,
  )
})
