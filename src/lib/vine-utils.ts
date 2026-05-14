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
 * - If `vine` is itself a reshare, its `originalCreatorAddress` /
 *   `originalCreatorName` already trace to the *true* origin (set by
 *   whichever client first reshared it). Propagate them as-is — this is
 *   the transitive case (Alice → Bob reshares → Carol reshares Bob's =
 *   Carol's reshare credits Alice).
 *
 * - Otherwise `vine` IS the origin — use its `creatorAddress` /
 *   `creatorName` so the downstream reshare credits this creator.
 *
 * Returns `undefined` fields only when the source vine itself is missing
 * the corresponding field (shouldn't happen for valid VineVideos, but the
 * helper stays total to avoid surprising callers).
 */
export function resolveOriginalCreator(vine: VineVideo): {
  originalCreatorAddress: string | undefined;
  originalCreatorName: string | undefined;
} {
  return {
    originalCreatorAddress: vine.originalCreatorAddress ?? vine.creatorAddress,
    originalCreatorName: vine.originalCreatorName ?? vine.creatorName,
  };
}
