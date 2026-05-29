const HARMONY_INVITE_PREFIX = "harmony://invite/";

/**
 * Pick the first harmony://invite/... URL from a list. The deep-link
 * plugin can deliver multiple URLs at once on first launch (queued OS
 * events). Returns null when none match.
 */
export function extractHarmonyInviteUrl(urls: string[]): string | null {
  return urls.find((u) => u.startsWith(HARMONY_INVITE_PREFIX)) ?? null;
}

/**
 * ZEB-338: single-slot queue for a harmony:// invite that arrives before an
 * owner identity exists (fresh install + deep-link). The boot sequence /
 * WelcomeModal's onMinted drains it once the owner identity is present, then
 * routes it to the redeem dialog. Plain module-level `let` (not Svelte
 * $state) — this is a .ts module accessed only through the two functions
 * below, so reactivity would add nothing.
 *
 * "Consume once" semantics: consumeQueuedInvite clears the slot. If the
 * downstream redeem fails, the queue is NOT repopulated; the user retries via
 * the Help menu's paste-invite affordance (spec §5.3).
 */
let pendingInviteUrl: string | null = null;

export function queueInviteForPostMint(url: string): void {
  pendingInviteUrl = url;
}

export function consumeQueuedInvite(): string | null {
  const url = pendingInviteUrl;
  pendingInviteUrl = null;
  return url;
}
