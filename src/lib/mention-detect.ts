/**
 * ZEB-662 — receiver-side self-mention predicate. Reuses the ChannelMessageDto
 * `mentions` field (ZEB-534: owner-ids the message addresses) — the same signal
 * as the in-feed row highlight (ChannelMessageFeed.svelte). Mention priority is
 * viewer-relative, so this is computed per-viewer, never from the wire priority.
 */
export function messageMentionsOwner(
  message: { mentions?: string[] },
  selfOwnerIdHex: string,
): boolean {
  if (!selfOwnerIdHex) return false;
  return message.mentions?.includes(selfOwnerIdHex) ?? false;
}
