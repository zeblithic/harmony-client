import { describe, it, expect, afterEach } from 'vitest';
import { get } from 'svelte/store';
import { groupCallBanners } from './group-call-banner-store';

const rosterA = [{ owner: 'aaaa', device: 'dev-a', muted: false }];
const rosterB = [
  { owner: 'bbbb', device: 'dev-b', muted: false },
  { owner: 'cccc', device: 'dev-c', muted: true },
];

afterEach(() => {
  // The store is a module singleton — clear everything we touched.
  groupCallBanners.clear('space-1');
  groupCallBanners.clear('space-2');
});

describe('groupCallBanners (public Record<spaceId,{callId,roster}> shape)', () => {
  it('surfaces an active call and clears it when its roster empties', () => {
    groupCallBanners.apply('space-1', 'call-1', rosterA);
    expect(get(groupCallBanners)['space-1']).toEqual({ callId: 'call-1', roster: rosterA });
    groupCallBanners.apply('space-1', 'call-1', []);
    expect(get(groupCallBanners)['space-1']).toBeUndefined();
  });

  it('surfaces the LOWEST callId when two calls coexist for one space', () => {
    // Higher callId arrives first…
    groupCallBanners.apply('space-1', 'call-9', rosterA);
    expect(get(groupCallBanners)['space-1']?.callId).toBe('call-9');
    // …then a lower one — it should win (spec reconciliation).
    groupCallBanners.apply('space-1', 'call-2', rosterB);
    const entry = get(groupCallBanners)['space-1'];
    expect(entry?.callId).toBe('call-2');
    expect(entry?.roster).toEqual(rosterB);
  });

  it('falls back IMMEDIATELY to the surviving higher call when the lower one ends', () => {
    // Both concurrent calls active; banner shows the lower (call-2).
    groupCallBanners.apply('space-1', 'call-2', rosterA);
    groupCallBanners.apply('space-1', 'call-9', rosterB);
    expect(get(groupCallBanners)['space-1']?.callId).toBe('call-2');
    // The lower call's roster empties — the banner must NOT go dark; it falls
    // back to the still-active higher call without waiting for its next event.
    groupCallBanners.apply('space-1', 'call-2', []);
    const entry = get(groupCallBanners)['space-1'];
    expect(entry?.callId).toBe('call-9');
    expect(entry?.roster).toEqual(rosterB);
  });

  it('removes the space entry only once every concurrent call has ended', () => {
    groupCallBanners.apply('space-1', 'call-2', rosterA);
    groupCallBanners.apply('space-1', 'call-9', rosterB);
    groupCallBanners.apply('space-1', 'call-9', []);
    expect(get(groupCallBanners)['space-1']?.callId).toBe('call-2');
    groupCallBanners.apply('space-1', 'call-2', []);
    expect(get(groupCallBanners)['space-1']).toBeUndefined();
  });

  it('clear() drops ALL tracked calls for a space (even non-surfaced ones)', () => {
    groupCallBanners.apply('space-1', 'call-2', rosterA);
    groupCallBanners.apply('space-1', 'call-9', rosterB);
    groupCallBanners.clear('space-1');
    expect(get(groupCallBanners)['space-1']).toBeUndefined();
    // A re-apply of the previously-hidden higher call starts fresh — no stale
    // inner state resurrects the lower call.
    groupCallBanners.apply('space-1', 'call-9', rosterB);
    expect(get(groupCallBanners)['space-1']?.callId).toBe('call-9');
  });

  it('keeps spaces independent', () => {
    groupCallBanners.apply('space-1', 'call-1', rosterA);
    groupCallBanners.apply('space-2', 'call-2', rosterB);
    expect(get(groupCallBanners)['space-1']?.callId).toBe('call-1');
    expect(get(groupCallBanners)['space-2']?.callId).toBe('call-2');
    groupCallBanners.clear('space-1');
    expect(get(groupCallBanners)['space-1']).toBeUndefined();
    expect(get(groupCallBanners)['space-2']?.callId).toBe('call-2');
  });
});
