// ZEB-804 Task 5 — pure badge/label helpers for peer traffic staleness.
//
// `peerBadge('direct', 'dark')` === '⚠' is THE ZEB-804 lie, fixed: a peer the
// liveness machine pins as direct/14ms while nothing has flowed for 30+ min
// must not render the green check. `null` staleness (pre-field snapshots)
// degrades gracefully to the legacy ZEB-622 mapping.
//
// Import is relative — this repo registers no `$lib` alias in
// vitest.config.ts / tsconfig.json (existing tests all import relatively).
import { describe, expect, it } from 'vitest';
import { peerBadge, stalenessLabel } from '../networkHealthStaleness';

describe('peerBadge', () => {
  it('dark forces the warn badge over a direct connection', () => {
    expect(peerBadge('direct', 'dark')).toBe('⚠'); // the ZEB-804 lie, fixed
    expect(peerBadge('direct', 'fresh')).toBe('✓');
    expect(peerBadge('direct', null)).toBe('✓'); // pre-field snapshots degrade gracefully
    expect(peerBadge('noConnection', null)).toBe('✗');
    expect(peerBadge('relay', 'quiet')).toBe('⚠');
  });

  it('preserves the legacy ZEB-622 mapping when staleness is fresh or absent', () => {
    expect(peerBadge('relay', null)).toBe('⚠');
    expect(peerBadge('degraded', null)).toBe('⚠');
    expect(peerBadge('degraded', 'fresh')).toBe('⚠');
    expect(peerBadge('noConnection', 'dark')).toBe('✗'); // dark never upgrades ✗
  });

  it('quiet does not override the direct check — only dark does', () => {
    expect(peerBadge('direct', 'quiet')).toBe('✓');
  });
});

describe('stalenessLabel', () => {
  it('annotates non-fresh tiers with the age', () => {
    expect(stalenessLabel('dark', 8_100_000)).toContain('no traffic');
    expect(stalenessLabel('fresh', 30_000)).toBe('');
  });

  it('formats the age in whole minutes', () => {
    expect(stalenessLabel('dark', 8_100_000)).toBe('no traffic for 135m');
    expect(stalenessLabel('quiet', 720_000)).toBe('quiet for 12m');
  });

  it('handles dark with no traffic evidence ever (null age)', () => {
    // A connected-looking peer that has NEVER moved traffic is the ZEB-804
    // incident shape — there is no age to report, but the annotation must
    // still say "no traffic".
    expect(stalenessLabel('dark', null)).toContain('no traffic');
  });

  it('returns empty for null staleness (pre-field snapshots)', () => {
    expect(stalenessLabel(null, 8_100_000)).toBe('');
  });
});
