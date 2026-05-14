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
