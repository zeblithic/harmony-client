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
 * - If `vine` is itself a reshare AND both `originalCreatorAddress` and
 *   `originalCreatorName` are set, they already trace to the *true*
 *   origin (set by whichever client first reshared it). Propagate them
 *   as a PAIR — this is the transitive case (Alice → Bob reshares →
 *   Carol reshares Bob's = Carol's reshare credits Alice).
 *
 * - Otherwise (no `originalCreator*` fields, OR only one of the two is
 *   set — a malformed/partial wire payload) fall back to the source
 *   vine's `creatorAddress` / `creatorName` as a PAIR.
 *
 * The atomic "both or neither" rule (vs. the earlier per-field
 * fallback) avoids mixing an `originalCreatorAddress` from the
 * resharer's attribution with a `creatorName` from the source vine,
 * which would credit one identity by address and a different one by
 * display name. See FIX 4 in PR #120 round 1.
 */
export function resolveOriginalCreator(vine: VineVideo): {
  originalCreatorAddress: string;
  originalCreatorName: string;
} {
  if (vine.originalCreatorAddress != null && vine.originalCreatorName != null) {
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
