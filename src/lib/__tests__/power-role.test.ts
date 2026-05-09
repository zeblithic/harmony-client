import { describe, it, expect } from 'vitest';
import { POWER_THRESHOLDS, powerToRole } from '../types';

describe('POWER_THRESHOLDS', () => {
  it('mirrors backend community_membership.rs:1108 values', () => {
    expect(POWER_THRESHOLDS.invite).toBe(0);
    expect(POWER_THRESHOLDS.kick).toBe(50);
    expect(POWER_THRESHOLDS.setPower).toBe(100);
    expect(POWER_THRESHOLDS.max).toBe(100);
  });
});

describe('powerToRole', () => {
  it('returns "member" for power 0', () => {
    expect(powerToRole(0)).toBe('member');
  });

  it('returns "member" for power 49 (just below kick threshold)', () => {
    expect(powerToRole(49)).toBe('member');
  });

  it('returns "mod" for power 50 (kick threshold)', () => {
    expect(powerToRole(50)).toBe('mod');
  });

  it('returns "mod" for power 99 (just below admin threshold)', () => {
    expect(powerToRole(99)).toBe('mod');
  });

  it('returns "admin" for power 100', () => {
    expect(powerToRole(100)).toBe('admin');
  });
});
