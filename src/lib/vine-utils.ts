/**
 * Pure helpers for vine-handling logic shared between App.svelte and tests.
 *
 * Kept separate from `vine-service.ts` so they can be unit-tested without
 * standing up a VineService (no adapter, no reactive `$state`, no Tauri).
 */
import type { VineVideo } from './types';

/**
 * Resolve the true-origin attribution to attach when resharing `vine`.
 *
 * Rule (matches §Edge Cases → Resharing a reshare):
 *
 * - If `vine` IS a reshare (`reshareOf` set) AND both
 *   `originalCreatorAddress` and `originalCreatorName` are present,
 *   they already trace to the *true* origin (set by whichever client
 *   first reshared it). Propagate them as a PAIR — this is the
 *   transitive case (Alice → Bob reshares → Carol reshares Bob's =
 *   Carol's reshare credits Alice).
 *
 * - Otherwise (non-reshare vine, OR a reshare with no `originalCreator*`
 *   fields, OR only one of the two is set — a malformed/partial wire
 *   payload) fall back to the source vine's `creatorAddress` /
 *   `creatorName` as a PAIR.
 *
 * Two layered guards:
 *
 * 1. Atomic "both-or-neither" on the originalCreator pair (vs. the
 *    earlier per-field fallback) avoids mixing an
 *    `originalCreatorAddress` from a reshare's attribution with a
 *    `creatorName` from the source vine — which would credit one
 *    identity by address and a different one by display name. See
 *    FIX 4 in PR #120 round 1.
 *
 * 2. Predicating on `reshareOf` ensures that even if a non-reshare vine
 *    somehow carries stray `originalCreator*` fields (legacy data,
 *    upstream bug), we ignore them — non-reshares are credited by their
 *    own `creatorAddress`/`creatorName`, period. CodeRabbit PR #120
 *    round 2 finding.
 */
export function resolveOriginalCreator(vine: VineVideo): {
  originalCreatorAddress: string;
  originalCreatorName: string;
} {
  if (
    vine.reshareOf != null
    && vine.originalCreatorAddress != null
    && vine.originalCreatorName != null
  ) {
    return {
      originalCreatorAddress: vine.originalCreatorAddress,
      originalCreatorName: vine.originalCreatorName,
    };
  }
  return {
    originalCreatorAddress: vine.creatorAddress,
    originalCreatorName: vine.creatorName,
  };
}

/**
 * Non-blank display label for a vine's creator/resharer (ZEB-561).
 *
 * The wire descriptor's `creatorName` can be empty: a reshare or publish issued
 * via the headless `reshare_vine` / `publish_vine` RPC with `creatorName`
 * omitted carries `creatorName=""` (the backend has no persisted social display
 * name to default from), so a viewer would otherwise render a blank resharer.
 * Never show blank — fall back to a truncated owner-hex, the same final rung the
 * member/message surfaces use (`name || address.slice(0, 8)`).
 */
export function vineCreatorLabel(
  name: string | null | undefined,
  address: string,
): string {
  const trimmed = name?.trim();
  return trimmed && trimmed.length > 0 ? trimmed : address.slice(0, 8);
}

/**
 * Non-blank "originally by …" label for a reshare's origin creator (Qodo PR
 * #337).
 *
 * Display-layered fallback: the origin's `originalCreatorName` when present,
 * else the source vine's `creatorName`, else the truncated owner-hex floor.
 *
 * NOTE this is deliberately NOT {@link resolveOriginalCreator}. That resolver
 * enforces a both-or-neither pairing because it governs reshare *propagation* —
 * propagating an `originalCreatorAddress` of one identity with an
 * `originalCreatorName` of another would mis-credit. A name-only *display*
 * ("originally by X") shows no address, so there is no mixing risk: a present
 * `originalCreatorName` should be shown even if its paired address is absent
 * (the established attribution contract, asserted by VineCard/VineFeed tests).
 * We only add the missing rung Qodo flagged — fall back to `creatorName` (never
 * a truncated hex) before the floor when `originalCreatorName` is empty.
 */
export function vineOriginalCreatorLabel(vine: VineVideo): string {
  const name = vine.originalCreatorName?.trim() || vine.creatorName;
  return vineCreatorLabel(name, vine.originalCreatorAddress ?? vine.creatorAddress);
}
