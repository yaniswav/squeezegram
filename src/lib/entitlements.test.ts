// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { test } from "node:test"
import { DecodeError } from "./decode.ts"
import { entitlementStatusDecoder } from "./entitlements.ts"

test("the public build's status decodes", () => {
  assert.deepEqual(
    entitlementStatusDecoder({ plan: "free", source: "build" }, "$"),
    { plan: "free", source: "build" },
  )
})

test("every plan and source the core serializes decodes", () => {
  for (const plan of ["free", "lifetime"]) {
    for (const source of ["build", "none", "license", "rejected"]) {
      assert.deepEqual(entitlementStatusDecoder({ plan, source }, "$"), {
        plan,
        source,
      })
    }
  }
})

test("extra keys are left out", () => {
  assert.deepEqual(
    entitlementStatusDecoder(
      { plan: "lifetime", source: "license", token: "SQZ1.x.y" },
      "$",
    ),
    { plan: "lifetime", source: "license" },
  )
})

test("a payload that breaks the contract names the mismatch", () => {
  const cases: [unknown, string][] = [
    [null, "$"],
    ["free", "$"],
    [{ source: "build" }, "$.plan"],
    [{ plan: "pro", source: "build" }, "$.plan"],
    [{ plan: "Free", source: "build" }, "$.plan"],
    [{ plan: "free" }, "$.source"],
    [{ plan: "free", source: "trial" }, "$.source"],
    [{ plan: "free", source: 1 }, "$.source"],
  ]
  for (const [value, path] of cases) {
    assert.throws(
      // SAFETY: the decoder takes any JSON value; these are JSON values.
      () => entitlementStatusDecoder(value as never, "$"),
      (error: unknown) => error instanceof DecodeError && error.path === path,
      JSON.stringify(value),
    )
  }
})
