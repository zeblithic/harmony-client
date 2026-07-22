import { describe, it, expect } from 'vitest';
import { tier2LifecyclePill, formatHalfLife } from '../proposal-format';

describe('tier2LifecyclePill', () => {
  it('maps each Tier-2 lifecycle to a single {variant, label} used by both pills', () => {
    // ZEB-648 item 1: card + panel-breadcrumb consume the SAME mapping so a
    // proposal never renders two different labels on one screen. Canonical
    // labels = the card's descriptive copy (approved fork).
    expect(tier2LifecyclePill('Open')).toEqual({ variant: 'open', label: 'Open' });
    expect(tier2LifecyclePill('ThresholdReached')).toEqual({
      variant: 'passing',
      label: 'Threshold reached — 24h window',
    });
    expect(tier2LifecyclePill('Finalized')).toEqual({ variant: 'passed', label: 'Finalized' });
    expect(tier2LifecyclePill('Archived')).toEqual({ variant: 'archived', label: 'Archived' });
  });
});

describe('formatHalfLife', () => {
  it('keeps whole-day half-lives as "Nd" (7-day fixture unchanged)', () => {
    expect(formatHalfLife(7 * 86_400)).toBe('7d');
    expect(formatHalfLife(86_400)).toBe('1d');
  });

  it('shows hours for sub-day half-lives instead of the "0d" bug', () => {
    // The old Math.round(s/86400) rendered "half-life 0d" for anything under
    // ~12h. formatHalfLife must surface a real duration instead.
    expect(formatHalfLife(6 * 3_600)).toBe('6h');
    expect(formatHalfLife(12 * 3_600)).toBe('12h');
    expect(formatHalfLife(3_600)).toBe('1h');
  });

  it('shows minutes for sub-hour half-lives (never "0d" or "0h")', () => {
    expect(formatHalfLife(30 * 60)).toBe('30m');
    expect(formatHalfLife(90)).toBe('2m');
  });

  it('is defensive against non-finite / non-positive input', () => {
    expect(formatHalfLife(0)).toBe('0m');
    expect(formatHalfLife(-100)).toBe('0m');
    expect(formatHalfLife(Number.NaN)).toBe('0m');
  });
});
