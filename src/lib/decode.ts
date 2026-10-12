// Small decoders for what the core sends: each one checks a JSON value against the contract
// ipc.ts declares for it and returns it typed, or throws a `DecodeError` naming the mismatch.

import {
  isJsonArray,
  isJsonBoolean,
  isJsonNumber,
  isJsonObject,
  isJsonString,
  type Json,
  type JsonObject,
} from "./json.ts"

/** A payload that breaks its contract; `path` locates the value (`$.items[2].state`). */
export class DecodeError extends Error {
  readonly path: string
  readonly expected: string

  constructor(path: string, expected: string) {
    super(`${path}: expected ${expected}`)
    this.name = "DecodeError"
    this.path = path
    this.expected = expected
  }
}

/** Reads one JSON value; `undefined` stands for a field absent from its object. */
export type Decoder<T> = (value: Json | undefined, path: string) => T

export const string: Decoder<string> = (value, path) => {
  if (isJsonString(value)) return value
  throw new DecodeError(path, "a string")
}

export const number: Decoder<number> = (value, path) => {
  if (isJsonNumber(value)) return value
  throw new DecodeError(path, "a number")
}

export const boolean: Decoder<boolean> = (value, path) => {
  if (isJsonBoolean(value)) return value
  throw new DecodeError(path, "a boolean")
}

/** The `()` of a Rust command, serialized as `null`. */
export const nothing: Decoder<void> = (value, path) => {
  if (value !== null && value !== undefined) throw new DecodeError(path, "null")
}

/** `params` (`serde_json::Value`): an object; anything else reads as no params. */
export const params: Decoder<JsonObject> = (value) =>
  isJsonObject(value) ? value : {}

/** `Option<T>` serialized as `null`. */
export function nullable<T>(decode: Decoder<T>): Decoder<T | null> {
  return (value, path) => (value === null ? null : decode(value, path))
}

/** `Option<T>` omitted when `None` (`skip_serializing_if`). */
export function optional<T>(decode: Decoder<T>): Decoder<T | undefined> {
  return (value, path) =>
    value === undefined ? undefined : decode(value, path)
}

/**
 * One of the members of the string union `T`. Taking them as the keys of a `Record<T, true>`
 * lets the compiler reject a missing or unknown member.
 */
export function literals<T extends string>(
  members: Record<T, true>,
): Decoder<T> {
  const isMember = (text: string): text is T => Object.hasOwn(members, text)
  const expected = `one of ${Object.keys(members).join(", ")}`

  return (value, path) => {
    if (isJsonString(value) && isMember(value)) return value
    throw new DecodeError(path, expected)
  }
}

export function array<T>(decode: Decoder<T>): Decoder<T[]> {
  return (value, path) => {
    if (!isJsonArray(value)) throw new DecodeError(path, "an array")

    return value.map((entry, index) => decode(entry, `${path}[${index}]`))
  }
}

/** A decoder for every field of `T`, optional fields included. */
export type Fields<T> = { [Key in keyof T]-?: Decoder<T[Key]> }

/** An object with the fields of `T`; other keys are left out, an absent optional field stays absent. */
export function object<T>(fields: Fields<T>): Decoder<T> {
  return (value, path) => {
    if (!isJsonObject(value)) throw new DecodeError(path, "an object")
    const decoded: Partial<T> = {}

    for (const key in fields) {
      const field = fields[key](value[key], `${path}.${key}`)

      if (field !== undefined) decoded[key] = field
    }

    // SAFETY: the loop ran the decoder of every key of `T` (`Fields<T>` requires one per key);
    // only an `optional` decoder returns undefined, and its key may be absent from `T`.
    return decoded as T
  }
}
