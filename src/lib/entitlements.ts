// Contract of `entitlements_status` (src-tauri/src/entitlements/mod.rs). Kept out of ipc.ts so
// `node --test` can import it (see entitlements.test.ts); ipc.ts re-exports what the UI needs.

import * as d from "./decode.ts"

/** `entitlements::Plan`. */
export type EntitlementPlan = "free" | "lifetime"

/** `entitlements::EntitlementSource`: why the plan is what it is. */
export type EntitlementSource =
  /** Built from the public source: no paid feature. */
  | "build"
  /** No license stored. */
  | "none"
  /** A stored license verifies and belongs to the signed-in account. */
  | "license"
  /** A license is stored but does not apply (the journal has the code). */
  | "rejected"

/** `entitlements_status`: the plan in effect. */
export interface EntitlementStatus {
  plan: EntitlementPlan
  source: EntitlementSource
}

export const entitlementStatusDecoder = d.object<EntitlementStatus>({
  plan: d.literals<EntitlementPlan>({ free: true, lifetime: true }),
  source: d.literals<EntitlementSource>({
    build: true,
    none: true,
    license: true,
    rejected: true,
  }),
})
