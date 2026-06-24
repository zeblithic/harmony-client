// Friendly mapping for channel-artifact content-fetch failures.
//
// On preview/download failure the backend surfaces raw transport strings — e.g.
// `fetch 'harmony/content/3/<64hex>' timed out after 30s` (event_loop.rs) — which
// leak zenoh key-expressions and internal phrasing to a tester who has no model
// for them. Map the known transport-failure shapes to friendly copy; the raw
// string stays available for diagnostics via `console.warn` at the call site
// (the frontend equivalent of "keep detail in tracing").
//
// Messages that are ALREADY user-facing (the preview path's own
// "Not a previewable image — download it to view." or the decode-bomb dimension
// guards) match no transport pattern and pass through unchanged, so this is safe
// to apply to every caught error on the fetch path.

const TRANSPORT_PATTERNS: Array<{ match: RegExp; friendly: string }> = [
  {
    match: /timed out|timeout/i,
    friendly:
      'This file isn’t available right now — whoever shared it may be offline. Try again later.',
  },
  {
    match: /not found|no such|unavailable|notfound/i,
    friendly:
      'This file couldn’t be found — it may have been removed or never finished uploading.',
  },
  {
    // Catch-all for a leaked zenoh key-expression / raw fetch error that didn't
    // match a more specific shape above.
    match: /harmony\/content|fetch '|zenoh|iroh/i,
    friendly: 'Couldn’t load this file right now. Try again later.',
  },
];

/**
 * Map a caught content-fetch error message to user-facing copy. Known
 * transport-failure shapes become friendly text; anything already user-facing
 * is returned unchanged.
 */
export function mapContentFetchError(raw: string): string {
  for (const p of TRANSPORT_PATTERNS) {
    if (p.match.test(raw)) return p.friendly;
  }
  return raw;
}
