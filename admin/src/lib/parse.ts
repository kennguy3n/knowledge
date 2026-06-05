/** Small input-parsing helpers shared across pages. */

/**
 * Parse a `<input type="number">` value into a positive integer, or
 * `undefined` when the field is empty or not a valid positive integer.
 *
 * `Number('')` is `0`, so a naive `Number(e.target.value)` would send
 * `limit=0` to the gateway when the operator clears the field. Returning
 * `undefined` lets the API client drop the param and the server apply its
 * own default/cap instead.
 */
export function parsePositiveInt(value: string): number | undefined {
  if (value.trim() === '') return undefined;
  const n = Number(value);
  return Number.isInteger(n) && n >= 1 ? n : undefined;
}
