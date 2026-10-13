// Event subscriptions, and the "current value, then its changes" pattern built on them.
// Free of Tauri so that the ordering can be tested with node's runner.

/**
 * Stops a subscription; safe to call before the listener is registered (React effect
 * cleanup). `ready` resolves once the core will deliver the event to it, or once registering
 * it failed (already reported): it never rejects.
 */
export type Subscription = (() => void) & { readonly ready: Promise<void> }

export function subscription(
  stop: () => void,
  ready: Promise<void>,
): Subscription {
  return Object.assign(() => stop(), { ready })
}

export interface FollowOptions<E, F> {
  /** Registers the handler of the event that announces each change (an `on…` of ipc.ts). */
  listen: (handler: (event: E) => void) => Subscription
  onEvent: (event: E) => void
  /** Asks the core for the current value. */
  fetch: () => Promise<F>
  /**
   * The answer of `fetch`. `eventSeen`: an event arrived after the listener was registered
   * and before this answer. The core sends one on every change, so from then on the events
   * carry the latest value and the answer may be older than the last of them.
   */
  onFetched: (value: F, eventSeen: boolean) => void
  onFetchFailed?: (cause: unknown) => void
}

/**
 * Follows a value the core holds and announces by event: the listener first, and only once it
 * is registered the current value. Asked in parallel, a change that happens before the
 * listener exists would be lost while the answer still says the older value, until the next
 * change (the core does not send an unchanged value again). Nothing is delivered after the
 * returned stop.
 */
export function follow<E, F>(options: FollowOptions<E, F>): () => void {
  let stopped = false
  let eventSeen = false

  const stop = options.listen((event) => {
    if (stopped) return
    eventSeen = true
    options.onEvent(event)
  })

  void stop.ready.then(() => {
    if (stopped) return

    return options.fetch().then(
      (value) => {
        if (!stopped) options.onFetched(value, eventSeen)
      },
      (cause: unknown) => {
        if (!stopped) options.onFetchFailed?.(cause)
      },
    )
  })

  return () => {
    stopped = true
    stop()
  }
}
