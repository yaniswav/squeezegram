/** A translation catalogue: nested keys down to strings. */
export interface Tree {
  [key: string]: string | Tree
}

const PLACEHOLDER = /\{\{[^}]*\}\}/g

// Wraps a string in brackets and pads it by about 40 % so hard-coded text (no brackets),
// truncation (missing closing bracket) and overflow are visible at a glance.
// i18next placeholders such as {{count}} are left untouched.
function pseudolocalizeString(text: string): string {
  const visibleLength = text.replace(PLACEHOLDER, "").length
  const padding = "~".repeat(Math.max(3, Math.ceil(visibleLength * 0.4)))

  return `[${text} ${padding}]`
}

function isLeaf(value: string | Tree): value is string {
  return typeof value === "string"
}

export function pseudolocalize(tree: Tree): Tree {
  return Object.fromEntries(
    Object.entries(tree).map(([key, value]) => [
      key,
      isLeaf(value) ? pseudolocalizeString(value) : pseudolocalize(value),
    ]),
  )
}
