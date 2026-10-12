export type Direction = "ltr" | "rtl"

export interface LanguageInfo {
  code: string
  nativeName: string
  dir: Direction
  /** Machine-translated and not yet reviewed by a native speaker: shown with a "beta" mention. */
  beta?: boolean
}

// Display metadata for the languages the project ships. A language only becomes
// selectable once its `locales/<code>.json` file exists (see `availableLanguages` in index.ts),
// so adding a language means adding one JSON file and one entry here.
export const PLANNED_LANGUAGES: readonly LanguageInfo[] = [
  { code: "en", nativeName: "English", dir: "ltr" },
  { code: "fr", nativeName: "Français", dir: "ltr", beta: true },
  { code: "ru", nativeName: "Русский", dir: "ltr", beta: true },
  { code: "es", nativeName: "Español", dir: "ltr", beta: true },
  { code: "ar", nativeName: "العربية", dir: "rtl", beta: true },
  { code: "pt-BR", nativeName: "Português (Brasil)", dir: "ltr", beta: true },
  { code: "id", nativeName: "Bahasa Indonesia", dir: "ltr", beta: true },
]

// Development-only pseudo-locale: lengthened text in brackets, right-to-left layout.
export const PSEUDO_RTL: LanguageInfo = {
  code: "xx-rtl",
  nativeName: "Pseudo RTL (dev)",
  dir: "rtl",
}

export const DEFAULT_LANGUAGE = "en"

export function languageInfo(code: string): LanguageInfo | undefined {
  return code === PSEUDO_RTL.code
    ? PSEUDO_RTL
    : PLANNED_LANGUAGES.find((language) => language.code === code)
}

export function directionOf(code: string): Direction {
  return languageInfo(code)?.dir ?? "ltr"
}
