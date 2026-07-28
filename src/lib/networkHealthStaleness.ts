// ZEB-804 (spec §6) — pure badge/label helpers for peer traffic staleness.
//
// The Network Health panel's per-peer ✓/⚠/✗ badge previously derived from
// `connectionMode` alone, so a peer the liveness machine pinned as
// "direct/14ms" kept a green check through 46 minutes of total silence (the
// ZEB-803 incident). These helpers fold the server-derived staleness tier
// (`PeerHealth.staleness`, Task 4) into the badge: `dark` forces ⚠ over any
// connected-looking mode.
//
// Pure functions — unit-tested directly in
// `__tests__/network-health-staleness.test.ts` with no component render.

import type { ConnectionMode, PeerStaleness } from './types/network-health';

/**
 * Per-peer status badge.
 *
 * Legacy ZEB-622 mapping (direct → ✓, relay/degraded → ⚠, else ✗) with one
 * ZEB-804 override: `staleness === 'dark'` forces ⚠ unless the mode is
 * already `noConnection` (✗ is already the honest worst case — dark must
 * never *upgrade* it to a warning).
 *
 * `null`/`undefined` staleness (a pre-field snapshot, or a `noConnection`
 * peer — the server emits no tier for those) degrades gracefully to the
 * legacy mapping.
 */
export function peerBadge(
  mode: ConnectionMode,
  staleness: PeerStaleness | null | undefined,
): string {
  if (mode === 'noConnection') return '✗';
  if (staleness === 'dark') return '⚠'; // the ZEB-804 lie, fixed
  if (mode === 'direct') return '✓';
  // ZEB-622: relay and degraded both warn (⚠) but carry distinct titles.
  if (mode === 'relay' || mode === 'degraded') return '⚠';
  return '✗';
}

/**
 * Human annotation for a non-fresh staleness tier, appended to the badge
 * title and rendered beside the mode/rtt columns.
 *
 * - `fresh` / `null` / `undefined` → `''` (nothing to say).
 * - `quiet` → `quiet for Xm`.
 * - `dark`  → `no traffic for Xm` — or `no traffic observed` when `ageMs` is
 *   `null` (a connected-looking peer with NO traffic evidence ever: there is
 *   no age to report, which is exactly the ZEB-804 incident shape).
 *
 * Ages are whole minutes (`Math.floor`), mirroring the panel's whole-second
 * "last seen Xs ago" formatting.
 */
export function stalenessLabel(
  staleness: PeerStaleness | null | undefined,
  ageMs: number | null,
): string {
  if (staleness !== 'quiet' && staleness !== 'dark') return '';
  if (ageMs === null) {
    // Only reachable for `dark`: the server emits `quiet` solely by
    // bucketing a present `lastTrafficMs`.
    return staleness === 'dark' ? 'no traffic observed' : '';
  }
  const mins = Math.floor(Math.max(0, ageMs) / 60_000);
  return staleness === 'dark' ? `no traffic for ${mins}m` : `quiet for ${mins}m`;
}
