/**
 * ZEB-665: shared HLC comparison. Extracted from the two identical private
 * copies in channel-message-service.ts and fork-timeline.ts (which now import
 * it). Matches the backend's canonical (wall_ms, logical, device_id) order —
 * wallMs alone is NOT a valid cursor (the ZEB-244 lesson).
 */
export interface HlcLike {
  wallMs: number;
  logical: number;
  deviceId: string;
}

/** wallMs → logical → deviceId lexical. Returns negative/0/positive. */
export function compareHlc(a: HlcLike, b: HlcLike): number {
  if (a.wallMs !== b.wallMs) return a.wallMs - b.wallMs;
  if (a.logical !== b.logical) return a.logical - b.logical;
  return a.deviceId < b.deviceId ? -1 : a.deviceId > b.deviceId ? 1 : 0;
}

/** Strictly newer: a > b. Mirrors the backend's `is_strictly_newer_than`. */
export function hlcNewer(a: HlcLike, b: HlcLike): boolean {
  return compareHlc(a, b) > 0;
}
