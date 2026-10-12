import i18n from "i18next"
import { initReactI18next } from "react-i18next"
import {
  DEFAULT_LANGUAGE,
  PLANNED_LANGUAGES,
  PSEUDO_RTL,
  directionOf,
  languageInfo,
  type LanguageInfo,
} from "./languages"
import en from "./locales/en.json"
import type { Tree } from "./pseudo"

// Keep in sync with public/theme-init.js.
const STORAGE_KEY = "squeezegram.language"

// Lazy loaders for every catalogue except `en` (bundled as the default and fallback).
const lazyCatalogues = import.meta.glob<Tree>(
  ["./locales/*.json", "!./locales/en.json"],
  { import: "default" },
)

const cataloguePath = (code: string) => `./locales/${code}.json`

/** Languages the user can pick: English, every planned language that has a file, and the pseudo-locale in dev. */
export function availableLanguages(): LanguageInfo[] {
  const shipped = PLANNED_LANGUAGES.filter(
    ({ code }) =>
      code === DEFAULT_LANGUAGE || cataloguePath(code) in lazyCatalogues,
  )

  return import.meta.env.DEV ? [...shipped, PSEUDO_RTL] : shipped
}

async function ensureLoaded(code: string): Promise<void> {
  if (i18n.hasResourceBundle(code, "translation")) return

  if (code === PSEUDO_RTL.code) {
    if (import.meta.env.DEV) {
      const { pseudolocalize } = await import("./pseudo")
      i18n.addResourceBundle(code, "translation", pseudolocalize(en))
    }

    return
  }

  const load = lazyCatalogues[cataloguePath(code)]

  if (load) i18n.addResourceBundle(code, "translation", await load())
}

function readStoredLanguage(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY)

    if (stored && availableLanguages().some(({ code }) => code === stored))
      return stored
  } catch {
    // Storage can be unavailable; fall back to the default language.
  }

  return DEFAULT_LANGUAGE
}

function applyDocumentLanguage(code: string): void {
  document.documentElement.lang = code
  document.documentElement.dir = directionOf(code)
}

export async function setLanguage(code: string): Promise<void> {
  if (!languageInfo(code)) return
  await ensureLoaded(code)
  await i18n.changeLanguage(code)

  try {
    localStorage.setItem(STORAGE_KEY, code)
  } catch {
    // Not persisted; the choice still applies to this session.
  }
}

export async function initI18n(): Promise<void> {
  const initial = readStoredLanguage()

  i18n.on("languageChanged", applyDocumentLanguage)
  await i18n.use(initReactI18next).init({
    lng: DEFAULT_LANGUAGE,
    fallbackLng: DEFAULT_LANGUAGE,
    resources: { [DEFAULT_LANGUAGE]: { translation: en } },
    interpolation: { escapeValue: false },
  })

  if (initial !== DEFAULT_LANGUAGE) await ensureLoaded(initial)
  await i18n.changeLanguage(initial)
  applyDocumentLanguage(initial)
}
