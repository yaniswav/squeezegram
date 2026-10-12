// Contract of `entitlements_status`, `license_set`, `license_clear` and `entitlements://status`
// (src-tauri/src/entitlements/mod.rs), and what the Settings page shows of it. Kept out of
// ipc.ts so `node --test` can import it (see entitlements.test.ts); ipc.ts re-exports the types.

import * as d from "./decode.ts"

/** `entitlements::Plan`. */
export type EntitlementPlan = "free" | "lifetime"

/** `entitlements::EntitlementSource`: why the plan is what it is. */
export type EntitlementSource =
  /** `FreeOnly`: no license is ever read. */
  | "build"
  /** No license stored. */
  | "none"
  /** A stored license verifies and belongs to the signed-in account. */
  | "license"
  /** A stored license verifies; no Telegram account is signed in to check it against. */
  | "pending"
  /** A license is stored but does not apply: `reason` has the code. */
  | "rejected"

/** `entitlements::Feature`. */
export type Feature =
  | "advanced_encoding"
  | "multi_destination"
  | "watched_folder"
  | "background"
  | "caption_templates"

/** `license::LicenseError::code`. */
export type LicenseCode =
  | "LIC_MALFORMED"
  | "LIC_UNKNOWN_KID"
  | "LIC_BAD_SIGNATURE"
  | "LIC_WRONG_ACCOUNT"
  | "LIC_NO_ACCOUNT"

/** `EntitlementStatus`. No identifier: neither the license nor the account. */
export interface EntitlementStatus {
  plan: EntitlementPlan
  source: EntitlementSource
  /** Granted now; empty unless `source` is `license`. */
  features: Feature[]
  /** The stored license was issued to the signed-in account; null when nothing to compare. */
  account_matches: boolean | null
  reason: LicenseCode | null
}

export const entitlementStatusDecoder = d.object<EntitlementStatus>({
  plan: d.literals<EntitlementPlan>({ free: true, lifetime: true }),
  source: d.literals<EntitlementSource>({
    build: true,
    none: true,
    license: true,
    pending: true,
    rejected: true,
  }),
  features: d.array(
    d.literals<Feature>({
      advanced_encoding: true,
      multi_destination: true,
      watched_folder: true,
      background: true,
      caption_templates: true,
    }),
  ),
  account_matches: d.nullable(d.boolean),
  reason: d.nullable(
    d.literals<LicenseCode>({
      LIC_MALFORMED: true,
      LIC_UNKNOWN_KID: true,
      LIC_BAD_SIGNATURE: true,
      LIC_WRONG_ACCOUNT: true,
      LIC_NO_ACCOUNT: true,
    }),
  ),
})

/** What the License section says about the status. */
export type LicenseView =
  | { kind: "free" }
  | {
      kind: "premium"
      plan: Exclude<EntitlementPlan, "free">
      features: Feature[]
    }
  /** Sign in to Telegram to check the stored license. */
  | { kind: "pending" }
  | { kind: "other_account" }
  /** Stored, but not a license this version accepts. */
  | { kind: "invalid"; code: LicenseCode }

export function licenseView(status: EntitlementStatus): LicenseView {
  switch (status.source) {
    case "build":
    case "none":
      return { kind: "free" }
    case "license":
      return status.plan === "free"
        ? { kind: "free" }
        : { kind: "premium", plan: status.plan, features: status.features }
    case "pending":
      return { kind: "pending" }
    case "rejected":
      if (status.account_matches === false) return { kind: "other_account" }

      return { kind: "invalid", code: status.reason ?? "LIC_MALFORMED" }
  }
}

/** A license file exists, applied or not: "Remove license" is offered. */
export function hasStoredLicense(status: EntitlementStatus): boolean {
  return (
    status.source === "license" ||
    status.source === "pending" ||
    status.source === "rejected"
  )
}

export function grants(
  status: EntitlementStatus | null,
  feature: Feature,
): boolean {
  return status?.features.includes(feature) ?? false
}
