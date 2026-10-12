// Number, size, duration and date formatting goes through Intl with the active UI language.

/** Locale passed to Intl: the RTL pseudo-locale borrows Arabic so digits and dates mirror a real RTL language. */
export function intlLocale(language: string): string {
  return language === "xx-rtl" ? "ar" : language
}

const BYTE_UNITS = [
  "byte",
  "kilobyte",
  "megabyte",
  "gigabyte",
  "terabyte",
] as const

/** Sizes use 1024 steps with KB/MB/GB labels, the way Windows Explorer shows them. */
export function formatBytes(bytes: number, language: string): string {
  const exponent = Math.min(
    BYTE_UNITS.length - 1,
    bytes > 0 ? Math.floor(Math.log(bytes) / Math.log(1024)) : 0,
  )

  return new Intl.NumberFormat(intlLocale(language), {
    style: "unit",
    unit: BYTE_UNITS[exponent],
    unitDisplay: "short",
    maximumFractionDigits: exponent === 0 ? 0 : 1,
  }).format(bytes / 1024 ** exponent)
}

export function formatPercent(percent: number, language: string): string {
  return new Intl.NumberFormat(intlLocale(language), {
    style: "percent",
    maximumFractionDigits: 0,
  }).format(percent / 100)
}

export function formatNumber(value: number, language: string): string {
  return new Intl.NumberFormat(intlLocale(language)).format(value)
}

/** "1 hr, 5 min", "42 sec": at most the two largest units. */
export function formatDuration(totalSeconds: number, language: string): string {
  const locale = intlLocale(language)
  const seconds = Math.max(0, Math.round(totalSeconds))

  const parts: [number, "hour" | "minute" | "second"][] = [
    [Math.floor(seconds / 3600), "hour"],
    [Math.floor((seconds % 3600) / 60), "minute"],
    [seconds % 60, "second"],
  ]

  const firstNonZero = parts.findIndex(([value]) => value > 0)

  const shown =
    firstNonZero === -1
      ? [parts[2]]
      : parts.slice(firstNonZero, firstNonZero + 2)

  const labels = shown
    .filter(([value], index) => index === 0 || value > 0)
    .map(([value, unit]) =>
      new Intl.NumberFormat(locale, {
        style: "unit",
        unit,
        unitDisplay: "short",
      }).format(value),
    )

  return new Intl.ListFormat(locale, { style: "narrow", type: "unit" }).format(
    labels,
  )
}

/** "a, b and c" in the UI language. */
export function formatList(items: readonly string[], language: string): string {
  return new Intl.ListFormat(intlLocale(language), {
    type: "conjunction",
  }).format(items)
}

export function formatTime(date: Date, language: string): string {
  return new Intl.DateTimeFormat(intlLocale(language), {
    timeStyle: "short",
  }).format(date)
}

export function formatDateTime(date: Date, language: string): string {
  return new Intl.DateTimeFormat(intlLocale(language), {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date)
}

/** Last path segment, for both Windows and POSIX separators. */
export function fileName(path: string): string {
  const parts = path.split(/[\\/]/)

  return parts[parts.length - 1] || path
}
