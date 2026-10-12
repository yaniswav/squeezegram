import { setWindowTheme } from "@/lib/ipc"

export type ThemePreference = "light" | "dark" | "system"

// Keep in sync with public/theme-init.js, which applies the theme before first paint.
const STORAGE_KEY = "squeezegram.theme"

const systemDark = window.matchMedia("(prefers-color-scheme: dark)")

const listeners = new Set<() => void>()

export function isThemePreference(value: unknown): value is ThemePreference {
  return value === "light" || value === "dark" || value === "system"
}

function readPreference(): ThemePreference {
  try {
    const stored = localStorage.getItem(STORAGE_KEY)

    if (isThemePreference(stored)) return stored
  } catch {
    // Storage can be unavailable; follow the system theme.
  }

  return "system"
}

let preference = readPreference()

function applyToDocument(): void {
  const dark =
    preference === "dark" || (preference === "system" && systemDark.matches)

  const root = document.documentElement
  root.classList.toggle("dark", dark)
  root.style.colorScheme = dark ? "dark" : "light"
}

// Keeps the native title bar and WebView color scheme in step with the chosen theme.
function applyToWindow(): void {
  setWindowTheme(preference === "system" ? null : preference)
}

export function initTheme(): void {
  applyToDocument()
  applyToWindow()
  systemDark.addEventListener("change", applyToDocument)
}

export function getThemePreference(): ThemePreference {
  return preference
}

export function subscribeThemePreference(listener: () => void): () => void {
  listeners.add(listener)

  return () => listeners.delete(listener)
}

export function setThemePreference(next: ThemePreference): void {
  if (next === preference) return
  preference = next

  try {
    localStorage.setItem(STORAGE_KEY, next)
  } catch {
    // Not persisted; the choice still applies to this session.
  }

  applyToDocument()
  applyToWindow()
  listeners.forEach((listener) => listener())
}
