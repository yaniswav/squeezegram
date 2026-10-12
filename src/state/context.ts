import { useContext, type Context } from "react"

/** Reads a context that must be provided by `AppProviders`. */
export function useRequiredContext<T>(
  context: Context<T | null>,
  name: string,
): T {
  const value = useContext(context)

  if (value === null) throw new Error(`${name} is used outside AppProviders`)

  return value
}
