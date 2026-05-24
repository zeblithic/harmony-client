const HARMONY_INVITE_PREFIX = "harmony://invite/";

/**
 * Pick the first harmony://invite/... URL from a list. The deep-link
 * plugin can deliver multiple URLs at once on first launch (queued OS
 * events). Returns null when none match.
 */
export function extractHarmonyInviteUrl(urls: string[]): string | null {
  return urls.find((u) => u.startsWith(HARMONY_INVITE_PREFIX)) ?? null;
}
