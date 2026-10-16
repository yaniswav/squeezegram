// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"
import {
  AUTOMATIC_ENCODING,
  effectiveEncoding,
  encodingOverrideDecoder,
  isTargetMb,
  maxResolutionDecoder,
} from "./advanced-encoding.ts"
import { DecodeError } from "./decode.ts"
import {
  entitlementsSeenLog,
  entitlementStatusDecoder,
  grants,
  hasStoredLicense,
  licenseView,
  type EntitlementStatus,
} from "./entitlements.ts"
import { isJson, isJsonObject, isJsonString } from "./json.ts"

/** Where the decoder says the payload breaks its contract; null if it decodes. */
function failurePath(run: () => void): string | null {
  try {
    run()
  } catch (error) {
    return error instanceof DecodeError ? error.path : null
  }

  return null
}

const FREE_BUILD = {
  plan: "free",
  source: "build",
  features: [],
  account_matches: null,
  covers_major: null,
  reason: null,
}

/** What `LicenseEntitlements` serializes for each verdict (store.rs, `Verdict::status`). */
const STATUSES = {
  none: {
    plan: "free",
    source: "none",
    features: [],
    account_matches: null,
    covers_major: null,
    reason: null,
  },
  /** A lifetime licence lists every shipped feature, whatever its `feat` (store.rs). */
  premium: {
    plan: "lifetime",
    source: "license",
    features: [
      "advanced_encoding",
      "multi_destination",
      "watched_folder",
      "background",
    ],
    account_matches: true,
    covers_major: 1,
    reason: null,
  },
  pending: {
    plan: "free",
    source: "pending",
    features: [],
    account_matches: null,
    covers_major: 1,
    reason: "LIC_NO_ACCOUNT",
  },
  otherAccount: {
    plan: "free",
    source: "rejected",
    features: [],
    account_matches: false,
    covers_major: 1,
    reason: "LIC_WRONG_ACCOUNT",
  },
  /** The key verifies but covers only majors below this build's (store.rs, `OlderMajor`). */
  olderMajor: {
    plan: "free",
    source: "rejected",
    features: [],
    account_matches: null,
    covers_major: 1,
    reason: "LIC_OLDER_MAJOR",
  },
  corrupt: {
    plan: "free",
    source: "rejected",
    features: [],
    account_matches: null,
    covers_major: null,
    reason: "LIC_MALFORMED",
  },
} satisfies Record<string, EntitlementStatus>

test("the entitlement diagnostic names its path and contains no personal data", () => {
  for (const via of ["fetch", "event"] as const) {
    assert.deepEqual(entitlementsSeenLog(STATUSES.premium, via), {
      level: "info",
      event: "entitlements_seen",
      msg: "Entitlements received",
      fields: {
        via,
        source: "license",
        plan: "lifetime",
        features: [
          "advanced_encoding",
          "multi_destination",
          "watched_folder",
          "background",
        ],
      },
    })
  }
})

test("every status the core serializes decodes unchanged", () => {
  assert.deepEqual(entitlementStatusDecoder(FREE_BUILD, "$"), FREE_BUILD)

  for (const status of Object.values(STATUSES)) {
    // SAFETY: the decoder takes any JSON value; a status is one.
    assert.deepEqual(entitlementStatusDecoder(status as never, "$"), status)
  }
})

test("extra keys are left out", () => {
  assert.deepEqual(
    entitlementStatusDecoder({ ...FREE_BUILD, token: "SQZ1.x.y" }, "$"),
    FREE_BUILD,
  )
})

test("a payload that breaks the contract names the mismatch", () => {
  const cases: [unknown, string][] = [
    [null, "$"],
    ["free", "$"],
    [{ ...FREE_BUILD, plan: "pro" }, "$.plan"],
    [{ ...FREE_BUILD, plan: "Free" }, "$.plan"],
    [{ ...FREE_BUILD, source: "trial" }, "$.source"],
    [{ ...FREE_BUILD, source: 1 }, "$.source"],
    [{ ...FREE_BUILD, features: null }, "$.features"],
    [{ ...FREE_BUILD, features: ["teleport"] }, "$.features[0]"],
    [{ ...FREE_BUILD, account_matches: "yes" }, "$.account_matches"],
    [{ ...FREE_BUILD, account_matches: undefined }, "$.account_matches"],
    [{ ...FREE_BUILD, covers_major: "1" }, "$.covers_major"],
    [{ ...FREE_BUILD, covers_major: undefined }, "$.covers_major"],
    [{ ...FREE_BUILD, reason: "LIC_EXPIRED" }, "$.reason"],
  ]

  for (const [value, path] of cases) {
    assert.equal(
      // SAFETY: the decoder takes any JSON value; these are JSON values.
      failurePath(() => entitlementStatusDecoder(value as never, "$")),
      path,
      JSON.stringify(value),
    )
  }
})

test("the License section shows each verdict, never anything personal", () => {
  assert.deepEqual(licenseView(STATUSES.none), { kind: "free" })
  // SAFETY: FREE_BUILD has the fields of a status.
  assert.deepEqual(licenseView(FREE_BUILD as EntitlementStatus), {
    kind: "free",
  })
  assert.deepEqual(licenseView(STATUSES.premium), {
    kind: "premium",
    plan: "lifetime",
    features: [
      "advanced_encoding",
      "multi_destination",
      "watched_folder",
      "background",
    ],
  })
  assert.deepEqual(licenseView(STATUSES.pending), { kind: "pending" })
  assert.deepEqual(licenseView(STATUSES.otherAccount), {
    kind: "other_account",
  })
  assert.deepEqual(licenseView(STATUSES.corrupt), {
    kind: "invalid",
    code: "LIC_MALFORMED",
  })
  assert.deepEqual(licenseView(STATUSES.olderMajor), {
    kind: "invalid",
    code: "LIC_OLDER_MAJOR",
  })
})

test("remove is offered for a stored license, applied or not", () => {
  assert.equal(hasStoredLicense(STATUSES.none), false)
  // SAFETY: FREE_BUILD has the fields of a status.
  assert.equal(hasStoredLicense(FREE_BUILD as EntitlementStatus), false)

  const { premium, pending, otherAccount, olderMajor, corrupt } = STATUSES

  for (const status of [premium, pending, otherAccount, olderMajor, corrupt])
    assert.equal(hasStoredLicense(status), true, status.source)
})

test("a feature is granted only by the features of an applied license", () => {
  for (const feature of STATUSES.premium.features)
    assert.equal(grants(STATUSES.premium, feature), true, feature)

  // The page reads the list the core sends and decides nothing itself: a status that lists
  // fewer features (a plan that is not lifetime) grants only those.
  const limited: EntitlementStatus = {
    ...STATUSES.premium,
    features: ["advanced_encoding"],
  }

  assert.equal(grants(limited, "advanced_encoding"), true)
  assert.equal(grants(limited, "background"), false)

  const { none, pending, otherAccount, olderMajor, corrupt } = STATUSES

  for (const status of [none, pending, otherAccount, olderMajor, corrupt])
    assert.equal(grants(status, "advanced_encoding"), false, status.source)
  assert.equal(grants(null, "advanced_encoding"), false)
})

test("without the feature the automatic values are shown, whatever is saved", () => {
  const saved = {
    max_resolution: "2160p",
    audio_kbps: 192,
    video_codec: "hevc",
    preset: "quality",
    target_mb: 50,
  } as const

  assert.deepEqual(effectiveEncoding(saved, true), saved)
  assert.deepEqual(effectiveEncoding(saved, false), {
    max_resolution: "1080p",
    audio_kbps: 128,
    video_codec: "h264",
    preset: "balanced",
    target_mb: null,
  })
  assert.deepEqual(effectiveEncoding(saved, false), AUTOMATIC_ENCODING)

  for (const value of ["1080p", "1440p", "2160p", "original"])
    assert.equal(maxResolutionDecoder(value, "$"), value)
  assert.throws(() => maxResolutionDecoder("p1080", "$"), DecodeError)
})

test("a file's encoding choices are a complete set the core can read back", () => {
  const stored = { video_codec: "hevc", max_resolution: "1440p", target_mb: 25 }

  assert.deepEqual(encodingOverrideDecoder(stored, "$"), stored)
  assert.deepEqual(
    encodingOverrideDecoder({ ...stored, target_mb: null }, "$").target_mb,
    null,
  )
  assert.throws(
    () => encodingOverrideDecoder({ ...stored, video_codec: "av1" }, "$"),
    DecodeError,
  )
  assert.throws(
    () => encodingOverrideDecoder({ video_codec: "h264" }, "$"),
    DecodeError,
  )
})

test("a custom target is a whole number of MiB within the core's range", () => {
  for (const ok of [1, 50, 2000, 4000])
    assert.equal(isTargetMb(ok), true, `${ok}`)

  for (const bad of [0, -1, 4001, 1.5, Number.NaN])
    assert.equal(isTargetMb(bad), false, `${bad}`)
})

test("every license code and feature has an English text", () => {
  const text: unknown = JSON.parse(
    readFileSync(new URL("../i18n/locales/en.json", import.meta.url), "utf8"),
  )

  assert.ok(isJson(text) && isJsonObject(text) && isJsonObject(text.errors))

  for (const code of [
    "LIC_MALFORMED",
    "LIC_UNKNOWN_KID",
    "LIC_BAD_SIGNATURE",
    "LIC_WRONG_ACCOUNT",
    "LIC_NO_ACCOUNT",
    "LIC_OLDER_MAJOR",
  ]) {
    assert.ok(isJsonString(text.errors[code]), code)
  }

  const settings = text.settings

  assert.ok(isJsonObject(settings) && isJsonObject(settings.license))
  const names = settings.license.feature

  assert.ok(isJsonObject(names))

  for (const feature of [
    "advanced_encoding",
    "multi_destination",
    "watched_folder",
    "background",
  ]) {
    assert.ok(isJsonString(names[feature]), feature)
  }
})
