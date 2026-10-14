import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"

const tauriRoot = new URL("../../src-tauri/", import.meta.url)

const config = JSON.parse(
  readFileSync(new URL("tauri.conf.json", tauriRoot), "utf8"),
)

test("installer branding keeps the existing app icon and installation hooks", () => {
  const nsis = config.bundle.windows.nsis

  assert.equal(nsis.installMode, "currentUser")
  assert.equal(nsis.installerHooks, "windows/hooks.nsh")
  assert.equal(nsis.installerIcon, "icons/icon.ico")
  assert.equal(nsis.uninstallerIcon, "icons/icon.ico")
  assert.ok(config.bundle.icon.includes(nsis.installerIcon))
  assert.equal(nsis.template, undefined)
})

test("installer artwork uses the native NSIS dimensions and opaque RGB bitmaps", () => {
  for (const [key, width, height] of [
    ["headerImage", 150, 57],
    ["sidebarImage", 164, 314],
  ] as const) {
    const bitmap = readFileSync(
      new URL(config.bundle.windows.nsis[key], tauriRoot),
    )

    assert.equal(bitmap.toString("ascii", 0, 2), "BM")
    assert.equal(bitmap.readUInt32LE(2), bitmap.length)
    assert.equal(bitmap.readUInt32LE(14), 40)
    assert.equal(bitmap.readInt32LE(18), width)
    assert.equal(bitmap.readInt32LE(22), height)
    assert.equal(bitmap.readUInt16LE(26), 1)
    assert.equal(bitmap.readUInt16LE(28), 24)
    assert.equal(bitmap.readUInt32LE(30), 0)
    const stride = Math.ceil((width * 3) / 4) * 4

    assert.equal(bitmap.readUInt32LE(10) + stride * height, bitmap.length)
  }
})
