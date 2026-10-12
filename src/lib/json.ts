// What crosses the Tauri IPC is JSON (serde_json on the Rust side): these types name it, and
// these guards are the only place that inspects a value's runtime representation.

/** A JSON value (`serde_json::Value`). */
export type Json = string | number | boolean | null | Json[] | JsonObject

/** A JSON object (`serde_json::Map`), such as the `params` of an error code. */
export interface JsonObject {
  [key: string]: Json
}

/** Whether a value received from outside (a rejection, a thrown value) is plain JSON. */
export function isJson(value: unknown): value is Json {
  return isJsonWithin(value, new Set())
}

/** `ancestors` stops a cyclic object from recursing forever: a cycle is not JSON. */
function isJsonWithin(value: unknown, ancestors: Set<object>): value is Json {
  if (
    value === null ||
    typeof value === "string" ||
    typeof value === "number" ||
    typeof value === "boolean"
  )
    return true

  if (typeof value !== "object" || ancestors.has(value)) return false
  ancestors.add(value)
  const entries = Array.isArray(value) ? value : Object.values(value)
  const valid = entries.every((entry) => isJsonWithin(entry, ancestors))
  ancestors.delete(value)

  return valid
}

// `undefined` stands for a key absent from its object.

export function isJsonObject(value: Json | undefined): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

export function isJsonArray(value: Json | undefined): value is Json[] {
  return Array.isArray(value)
}

export function isJsonString(value: Json | undefined): value is string {
  return typeof value === "string"
}

export function isJsonNumber(value: Json | undefined): value is number {
  return typeof value === "number"
}

export function isJsonBoolean(value: Json | undefined): value is boolean {
  return typeof value === "boolean"
}
