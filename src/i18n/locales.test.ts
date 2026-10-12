// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
// Every shipped language is checked against en.json. Run one language with:
//   node --test --test-name-pattern="\[fr\]" src/i18n/locales.test.ts
import assert from "node:assert/strict"
import { readdirSync, readFileSync } from "node:fs"
import { test } from "node:test"
import { PLANNED_LANGUAGES } from "./languages.ts"

type Catalogue = { [key: string]: string | Catalogue }

const localesUrl = new URL("./locales/", import.meta.url)

function load(code: string): Catalogue {
  return JSON.parse(
    readFileSync(new URL(`${code}.json`, localesUrl), "utf8"),
  ) as Catalogue
}

/** Leaf strings keyed by dotted path, e.g. `common.cancel`. */
function flatten(tree: Catalogue, prefix = ""): Map<string, string> {
  const leaves = new Map<string, string>()

  for (const [key, value] of Object.entries(tree)) {
    const path = prefix ? `${prefix}.${key}` : key

    if (typeof value === "string") leaves.set(path, value)
    else for (const [sub, text] of flatten(value, path)) leaves.set(sub, text)
  }

  return leaves
}

const PLURAL_FORM = /_(zero|one|two|few|many|other)$/
const SOURCE_PLURAL = /_(one|other)$/

/**
 * Paths a catalogue must have for `code`. In en.json, `X_one` and `X_other` are one plural
 * group: each language needs one key per CLDR category i18next uses for it.
 */
function expectedPaths(source: Map<string, string>, code: string): Set<string> {
  const { pluralCategories } = new Intl.PluralRules(code).resolvedOptions()
  const paths = new Set<string>()

  for (const path of source.keys()) {
    const plural = SOURCE_PLURAL.exec(path)

    if (plural) {
      const base = path.slice(0, -plural[0].length)

      for (const category of pluralCategories) paths.add(`${base}_${category}`)
    } else paths.add(path)
  }

  return paths
}

/** The en.json string a path is translated from (plural forms share the `_other` text). */
function sourceOf(
  source: Map<string, string>,
  path: string,
): string | undefined {
  const plural = PLURAL_FORM.exec(path)

  if (plural) {
    const base = path.slice(0, -plural[0].length)

    return source.get(`${base}_other`) ?? source.get(`${base}_one`)
  }

  return source.get(path)
}

const PLACEHOLDER = /\{\{[^}]*\}\}/g
const TAG = /<\/?[a-zA-Z][^>]*>/g

function sortedMatches(text: string, pattern: RegExp): string[] {
  return (text.match(pattern) ?? []).sort()
}

// Terms that stay as they are in every language. Checked only where the English text has them.
const KEPT_TERMS = [
  "squeezegram",
  "Telegram",
  "Premium",
  "Stars",
  "ffmpeg",
  "FFmpeg",
  "TDLib",
  "NVENC",
  "QSV",
  "AMF",
  "H.264",
  "HEVC",
  "LGPL",
  "GPL",
  "SmartScreen",
  "Windows",
  "GitHub",
  "MiB",
  "MB",
  "SHA-256",
  "QR",
  "2FA",
]

// Unit symbols a language writes its own way (French: octet, not byte).
const LOCAL_TERMS: Record<string, Record<string, string>> = {
  fr: { MB: "Mo" },
  ru: { MB: "МБ" },
}

const en = flatten(load("en"))
const languages = PLANNED_LANGUAGES.filter(({ code }) => code !== "en")

test("every locale file belongs to a planned language", () => {
  const planned = new Set(PLANNED_LANGUAGES.map(({ code }) => code))

  for (const file of readdirSync(localesUrl)) {
    const code = file.replace(/\.json$/, "")

    assert.ok(planned.has(code), `${file} has no entry in PLANNED_LANGUAGES`)
  }
})

for (const { code } of languages) {
  test(`[${code}] has exactly the keys of en.json (with plural forms)`, () => {
    const catalogue = flatten(load(code))
    const expected = expectedPaths(en, code)
    const missing = [...expected].filter((path) => !catalogue.has(path))
    const extra = [...catalogue.keys()].filter((path) => !expected.has(path))

    assert.deepEqual({ missing, extra }, { missing: [], extra: [] })
  })

  test(`[${code}] keeps the placeholders and tags of each string`, () => {
    const catalogue = flatten(load(code))

    for (const [path, text] of catalogue) {
      const source = sourceOf(en, path)

      assert.ok(source !== undefined, `${path} has no English source`)
      assert.deepEqual(
        sortedMatches(text, PLACEHOLDER),
        sortedMatches(source, PLACEHOLDER),
        `placeholders of ${path}`,
      )
      assert.deepEqual(
        sortedMatches(text, TAG),
        sortedMatches(source, TAG),
        `tags of ${path}`,
      )
    }
  })

  test(`[${code}] has no empty string`, () => {
    const empty = [...flatten(load(code))]
      .filter(([, text]) => text.trim() === "")
      .map(([path]) => path)

    assert.deepEqual(empty, [])
  })

  test(`[${code}] keeps the untranslated terms that appear in English`, () => {
    const catalogue = flatten(load(code))

    for (const [path, text] of catalogue) {
      const source = sourceOf(en, path) ?? ""

      for (const term of KEPT_TERMS) {
        const expected = LOCAL_TERMS[code]?.[term] ?? term
        if (source.includes(term))
          assert.ok(text.includes(expected), `${expected} missing from ${path}`)
      }
    }
  })
}
