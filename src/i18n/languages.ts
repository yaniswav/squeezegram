export type Direction = "ltr" | "rtl"

export interface LanguageInfo {
  code: string
  nativeName: string
  dir: Direction
}

// Display metadata for the languages the project plans to ship. A language only becomes
// selectable once its `locales/<code>.json` file exists (see `availableLanguages` in index.ts),
// so adding ru, es or ar means adding one JSON file, nothing else.
export const PLANNED_LANGUAGES: readonly LanguageInfo[] = [
  { code: "en", nativeName: "English", dir: "ltr" },
  { code: "ru", nativeName: "Русский", dir: "ltr" },
  { code: "es", nativeName: "Español", dir: "ltr" },
  { code: "ar", nativeName: "العربية", dir: "rtl" },
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
