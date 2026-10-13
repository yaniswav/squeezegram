// `pnpm test`: node's own runner, TypeScript types stripped by node (no dependency).
import assert from "node:assert/strict"
import { test } from "node:test"
import { follow, subscription, type Subscription } from "./subscription.ts"

/** A promise and its resolve/reject, to settle by hand. */
function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (cause: unknown) => void

  const promise = new Promise<T>((ok, ko) => {
    resolve = ok
    reject = ko
  })

  return { promise, resolve, reject }
}

/** Lets every settled promise run its callbacks. */
const flush = () => new Promise<void>((done) => setTimeout(done, 0))

/** A fake event channel whose registration completes when `register()` is called. */
function channel<E>() {
  const registered = deferred<void>()
  let handler: ((event: E) => void) | null = null
  let stopped = 0

  const listen = (next: (event: E) => void): Subscription => {
    handler = next

    return subscription(() => {
      stopped += 1
    }, registered.promise)
  }

  return {
    listen,
    register: () => registered.resolve(),
    emit: (event: E) => handler?.(event),
    stops: () => stopped,
  }
}

test("the current value is asked only once the listener is registered", async () => {
  const events = channel<string>()
  let asked = 0
  const answer = deferred<string>()

  follow({
    listen: events.listen,
    onEvent: () => {},
    fetch: () => {
      asked += 1

      return answer.promise
    },
    onFetched: () => {},
  })

  await flush()
  assert.equal(asked, 0, "asked before the core could deliver the change")
  events.register()
  await flush()
  assert.equal(asked, 1)
})

test("an answer is flagged when an event arrived before it", async () => {
  const events = channel<string>()
  const answer = deferred<string>()
  const seen: string[] = []

  follow({
    listen: events.listen,
    onEvent: (event) => seen.push(`event:${event}`),
    fetch: () => answer.promise,
    onFetched: (value, eventSeen) => seen.push(`fetched:${value}:${eventSeen}`),
  })

  events.register()
  await flush()
  // The startup sequence of the license: `license` lands while `pending` is on its way.
  events.emit("license")
  answer.resolve("pending")
  await flush()
  assert.deepEqual(seen, ["event:license", "fetched:pending:true"])
})

test("a failed listener registration still asks for the current value once", async () => {
  const registered = deferred<void>()
  const failure = new Error("listener unavailable")
  const failures: unknown[] = []
  const seen: string[] = []
  let asked = 0

  follow({
    // Like ipc.ts's subscribe: report registration failure and resolve `ready`.
    listen: () =>
      subscription(
        () => {},
        registered.promise.then(undefined, (cause: unknown) => {
          failures.push(cause)
        }),
      ),
    onEvent: () => assert.fail("no listener was registered"),
    fetch: () => {
      asked += 1

      return Promise.resolve("license")
    },
    onFetched: (value, eventSeen) => seen.push(`fetched:${value}:${eventSeen}`),
    onFetchFailed: () =>
      assert.fail("registration failure is not a fetch failure"),
  })

  await flush()
  assert.equal(asked, 0, "asked before registration settled")
  registered.reject(failure)
  await flush()
  assert.deepEqual(failures, [failure])
  assert.equal(asked, 1)
  assert.deepEqual(seen, ["fetched:license:false"])
})

test("an answer without any event before it is not flagged", async () => {
  const events = channel<string>()
  const seen: string[] = []

  follow({
    listen: events.listen,
    onEvent: (event) => seen.push(`event:${event}`),
    fetch: () => Promise.resolve("license"),
    onFetched: (value, eventSeen) => seen.push(`fetched:${value}:${eventSeen}`),
  })

  events.register()
  await flush()
  events.emit("free")
  assert.deepEqual(seen, ["fetched:license:false", "event:free"])
})

test("stopped before the listener is registered: nothing is asked", async () => {
  const events = channel<string>()
  let asked = 0

  const stop = follow({
    listen: events.listen,
    onEvent: () => assert.fail("event after stop"),
    fetch: () => {
      asked += 1

      return Promise.resolve("x")
    },
    onFetched: () => assert.fail("answer after stop"),
  })

  stop()
  events.register()
  events.emit("late")
  await flush()
  assert.equal(asked, 0)
  assert.equal(events.stops(), 1)
})

test("stopped while the answer is on its way: it is dropped", async () => {
  const events = channel<string>()
  const answer = deferred<string>()
  const failure = deferred<string>()
  let calls = 0

  const stopAnswered = follow({
    listen: events.listen,
    onEvent: () => {},
    fetch: () => answer.promise,
    onFetched: () => (calls += 1),
  })

  const other = channel<string>()

  const stopFailed = follow({
    listen: other.listen,
    onEvent: () => {},
    fetch: () => failure.promise,
    onFetched: () => (calls += 1),
    onFetchFailed: () => (calls += 1),
  })

  events.register()
  other.register()
  await flush()
  stopAnswered()
  stopFailed()
  answer.resolve("x")
  failure.reject(new Error("gone"))
  await flush()
  assert.equal(calls, 0)
})

test("a failed answer is reported, the events still arrive", async () => {
  const events = channel<string>()
  const seen: string[] = []

  follow({
    listen: events.listen,
    onEvent: (event) => seen.push(`event:${event}`),
    fetch: () => Promise.reject(new Error("core unavailable")),
    onFetched: () => seen.push("fetched"),
    onFetchFailed: (cause) =>
      seen.push(`failed:${cause instanceof Error ? cause.message : ""}`),
  })

  events.register()
  await flush()
  events.emit("license")
  assert.deepEqual(seen, ["failed:core unavailable", "event:license"])
})
