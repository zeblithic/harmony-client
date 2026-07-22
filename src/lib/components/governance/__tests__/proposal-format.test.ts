import { describe, it, expect } from 'vitest';
import { tier2LifecyclePill, formatHalfLife, thresholdPercent } from '../proposal-format';

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

describe('thresholdPercent', () => {
  it('floors so a sub-threshold proposal never displays a premature "100"', () => {
    // toFixed(0) would round [99.5, 100) up to 100 — but threshold-reached
    // fires only at an exact 100, so "100% reached" must not appear next to
    // an "Open" pill.
    expect(thresholdPercent(99.9)).toBe(99);
    expect(thresholdPercent(99.5)).toBe(99);
    expect(thresholdPercent(100)).toBe(100);
    expect(thresholdPercent(25)).toBe(25);
    expect(thresholdPercent(0.9)).toBe(0);
  });

  it('clamps out-of-range / non-finite input defensively', () => {
    expect(thresholdPercent(150)).toBe(100);
    expect(thresholdPercent(-5)).toBe(0);
    expect(thresholdPercent(Number.NaN)).toBe(0);
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
