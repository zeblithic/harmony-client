import { describe, it, expect } from 'vitest';
import { relayStatusLabel } from '../relay-status-label';
import type { RelayHealth } from '../types/network-health';

function healthy(): RelayHealth {
  return { url: 'https://r.example', state: { kind: 'healthy' }, lastOutcome: null, lastSuccessMs: null };
}
function cooling(untilMs: number): RelayHealth {
  return {
    url: 'https://r.example',
    state: { kind: 'coolingDown', untilMs },
    lastOutcome: null,
    lastSuccessMs: null,
  };
}

describe('relayStatusLabel', () => {
  it('labels a healthy relay', () => {
    expect(relayStatusLabel(healthy(), 1_000)).toBe('Healthy');
  });

  it('labels a cooling relay with ceil seconds remaining', () => {
    // ceil((10000 - 5500) / 1000) === 5
    expect(relayStatusLabel(cooling(10_000), 5_500)).toBe('Cooling down (5s)');
  });

  it('clamps an already-elapsed cooldown to 0s', () => {
    expect(relayStatusLabel(cooling(1_000), 9_000)).toBe('Cooling down (0s)');
  });
});
