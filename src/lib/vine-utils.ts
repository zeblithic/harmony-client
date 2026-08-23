/**
 * Pure helpers for vine-handling logic shared between App.svelte and tests.
 *
 * Kept separate from `vine-service.ts` so they can be unit-tested without
 * standing up a VineService (no adapter, no reactive `$state`, no Tauri).
 */
import type { VineVideo } from './types';
import { nonEmpty, type ResolvedName } from './display-label';
import { resolveAuthorLabel } from './mention-render';

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
 * ZEB-978: THE vine author-name resolver — the display-name ladder
 * (petname ► verified card ► wire ► hex) applied to a vine's creator.
 *
 * `creatorAddress` is signature-bound at cache admission (ZEB-673), so it is
 * an authenticated identity in the same namespace every other ladder surface
 * keys on. `creatorName` is free text the publisher chose — any descriptor
 * can carry any name, so it must never outrank a name YOU assigned (petname)
 * or a name the peer verifiably published (card). It enters the shared
 * {@link resolveAuthorLabel} ladder as the wire rung, just above the hex
 * floor, and visual sites render the result through `PeerName.svelte` so a
 * wire name is visibly unverified.
 *
 * Two vine-specific floors on top of the shared ladder (both preserve
 * ZEB-561's never-blank contract):
 *
 * - The `self` sentinel short-circuits to `ownDisplayName`, which can be
 *   blank on an offline publish — degrade to the hex floor, never blank.
 * - `wireToVine` bakes `creatorAddress.slice(0, 8)` into a blank wire name
 *   at ingest; a wire rung that just echoes the address prefix IS the hex
 *   floor and is tagged `hex` (mono style), not `wire` (unverified style).
 */
export function resolveVineCreatorName(
  vine: Pick<VineVideo, 'creatorAddress' | 'creatorName'>,
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName: string } | undefined,
): ResolvedName {
  const r = resolveAuthorLabel(
    { address: vine.creatorAddress, displayName: vine.creatorName ?? '' },
    resolveNickname,
    resolveCard,
  );
  const hexFloor = vine.creatorAddress.slice(0, 8);
  if (r.source === 'self' && nonEmpty(r.label) === undefined) {
    return { label: hexFloor, source: 'hex' };
  }
  if (r.source === 'wire') {
    const trimmed = r.label.trim();
    return { label: trimmed, source: trimmed === hexFloor ? 'hex' : 'wire' };
  }
  return r;
}

/**
 * ZEB-978: ladder-resolved "view original by …" label for a reshare's origin
 * creator.
 *
 * The ladder (petname ► card) runs ONLY against `originalCreatorAddress` —
 * when the origin address is absent there is nothing to verify a local name
 * against, and resolving on the fallback `creatorAddress` would let a
 * petname for the RESHARER label the ORIGINAL creator (a mis-credit worse
 * than showing the wire snapshot). Wire floors: a present
 * `originalCreatorName` shows even when its paired address is absent (a
 * name-only display has no pair-mixing risk — Qodo #337); on a RESHARE the
 * source vine's `creatorName` names the resharer and must never stand in as
 * the origin's wire candidate (CodeAnt PR #725 — a malformed pair with the
 * name blank would credit the resharer as the original creator), so a
 * reshare degrades to the origin's petname/card/hex instead. Only a
 * non-reshare (creator IS the origin) falls back to `creatorName`.
 */
export function resolveVineOriginalCreatorName(
  vine: VineVideo,
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName: string } | undefined,
): ResolvedName {
  const wireName =
    vine.originalCreatorName?.trim()
    || (vine.reshareOf != null ? '' : vine.creatorName);
  const addr = nonEmpty(vine.originalCreatorAddress);
  if (addr !== undefined) {
    return resolveVineCreatorName(
      { creatorAddress: addr, creatorName: wireName },
      resolveNickname,
      resolveCard,
    );
  }
  const name = nonEmpty(wireName);
  if (name !== undefined) return { label: name.trim(), source: 'wire' };
  return { label: vine.creatorAddress.slice(0, 8), source: 'hex' };
}

/**
 * ZEB-978 (CodeRabbit PR #725): the unique non-self identity addresses a vine
 * feed renders names for — creators, reshare origin creators, and Discover
 * `via` hops — for the `vines` card-subscription bucket. `resolveCard` only
 * resolves owners some MemberCardService bucket has subscribed (same
 * contract `feedAuthorOwnerIds` serves for channel feeds, ZEB-962): without a
 * bucket, a vine author with a verified card but no community/DM/friend
 * overlap would silently stay on the unverified wire rung. The `'self'`
 * sentinel is not an owner_id and is excluded.
 */
export function vineIdentityOwnerIds(vines: VineVideo[]): string[] {
  const owners = new Set<string>();
  const add = (addr: string | undefined) => {
    if (addr && addr !== 'self') owners.add(addr);
  };
  for (const v of vines) {
    add(v.creatorAddress);
    add(v.originalCreatorAddress);
    for (const hop of v.via ?? []) add(hop);
  }
  return [...owners];
}

/**
 * Index of the card center nearest the viewport center (ZEB-612 S2 feed
 * autoplay). Ties break toward the earlier card, which also makes jsdom's
 * all-zero rects deterministically pick index 0 (first card plays on mount).
 * Returns -1 for an empty list.
 */
export function pickCenterIndex(centers: number[], viewportCenter: number): number {
  let best = -1;
  let bestDist = Number.POSITIVE_INFINITY;
  for (let i = 0; i < centers.length; i++) {
    const d = Math.abs(centers[i] - viewportCenter);
    if (d < bestDist) {
      bestDist = d;
      best = i;
    }
  }
  return best;
}

/** "m:ss" badge text for the honest duration pill ("↻ 0:06"). */
export function formatVineDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '0:00';
  const total = Math.round(seconds);
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}:${String(s).padStart(2, '0')}`;
}

/**
 * Whether `vine` is the local user's own ORIGINAL (not a reshare) — the
 * only case where the Reshare verb is suppressed. Extracted verbatim from
 * VinePlayer's `isOwnOriginal` (FIX 2, PR #120 round 1): hex-keyed
 * self-authored vines that arrived before `ownAddress` was set weren't
 * remapped to the magic 'self' value, so both signals are checked.
 */
export function isOwnOriginalVine(vine: VineVideo, ownAddress?: string): boolean {
  return (
    !vine.reshareOf
    && (vine.creatorAddress === 'self'
      || (ownAddress != null && vine.creatorAddress === ownAddress))
  );
}
