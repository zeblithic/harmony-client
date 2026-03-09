import { describe, it, expect, vi, afterEach } from 'vitest';
import { categoryIcon, tierTarget, formatBytes, relativeTime } from './file-utils';

describe('categoryIcon', () => {
  it('returns music note for music', () => {
    expect(categoryIcon('music')).toBe('\u266A');
  });

  it('returns play triangle for video', () => {
    expect(categoryIcon('video')).toBe('\u25B6');
  });

  it('returns folder icon for bundle', () => {
    expect(categoryIcon('bundle')).toBe('\uD83D\uDCC1');
  });
});

describe('tierTarget', () => {
  it('returns 1 for expendable', () => {
    expect(tierTarget('expendable')).toBe(1);
  });

  it('returns 3 for default', () => {
    expect(tierTarget('default')).toBe(3);
  });

  it('returns 9 for ultra', () => {
    expect(tierTarget('ultra')).toBe(9);
  });
});

describe('formatBytes', () => {
  it('formats bytes', () => {
    expect(formatBytes(500)).toBe('500 B');
  });

  it('formats kilobytes', () => {
    expect(formatBytes(45_000)).toBe('45.0 KB');
  });

  it('formats megabytes', () => {
    expect(formatBytes(250_000_000)).toBe('250.0 MB');
  });

  it('formats gigabytes', () => {
    expect(formatBytes(1_500_000_000)).toBe('1.5 GB');
  });
});

describe('relativeTime', () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('returns minutes for recent timestamps', () => {
    vi.spyOn(Date, 'now').mockReturnValue(1_000_000_000);
    expect(relativeTime(1_000_000_000 - 5 * 60_000)).toBe('5m ago');
  });

  it('returns hours', () => {
    vi.spyOn(Date, 'now').mockReturnValue(1_000_000_000);
    expect(relativeTime(1_000_000_000 - 3 * 3_600_000)).toBe('3h ago');
  });

  it('returns days', () => {
    vi.spyOn(Date, 'now').mockReturnValue(1_000_000_000);
    expect(relativeTime(1_000_000_000 - 5 * 86_400_000)).toBe('5d ago');
  });

  it('returns months', () => {
    vi.spyOn(Date, 'now').mockReturnValue(1_000_000_000);
    expect(relativeTime(1_000_000_000 - 90 * 86_400_000)).toBe('3mo ago');
  });
});
