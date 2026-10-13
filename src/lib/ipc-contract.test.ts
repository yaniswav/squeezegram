// `pnpm test`: static contract checks for the page/core IPC boundary.
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"

const page = readFileSync(new URL("./ipc.ts", import.meta.url), "utf8")

const core = readFileSync(
  new URL("../../src-tauri/src/commands.rs", import.meta.url),
  "utf8",
)

function between(source: string, start: string, end: string): string {
  const from = source.indexOf(start)
  assert.notEqual(from, -1, `missing ${start}`)
  const to = source.indexOf(end, from + start.length)
  assert.notEqual(to, -1, `missing ${end}`)

  return source.slice(from, to)
}

function camelCase(name: string): string {
  return name.replace(/_([a-z])/g, (_, letter: string) => letter.toUpperCase())
}

test("sidecars_use sends the argument name Tauri derives from Rust", () => {
  const pageCommand = between(
    page,
    "export function sidecarsUse(",
    "export function sidecarsChoose(",
  )

  const coreCommand = between(
    core,
    "pub async fn sidecars_use(",
    "pub async fn sidecars_choose(",
  )

  const rustArgument = coreCommand.match(/\n\s+([a-z_]+): u64,/)?.[1]

  const pageArgument = pageCommand.match(
    /call\("sidecars_use",[\s\S]*?\{\s*([a-zA-Z][a-zA-Z0-9]*)\s*,\s*\}/,
  )?.[1]

  assert.equal(rustArgument, "candidate_id")

  assert.equal(pageArgument, camelCase(rustArgument))
})
