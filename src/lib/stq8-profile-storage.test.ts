import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { loadProfile, saveProfile, clearProfile } from './stq8-profile-storage';

describe('stq8-profile-storage', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('loadProfile returns null when nothing is stored', () => {
    expect(loadProfile()).toBeNull();
  });

  it('saveProfile then loadProfile round-trips the exact string', () => {
    const profile = '{"centroids":{"0":[1,2,3]},"created":1700000000}';
    expect(saveProfile(profile)).toBe(true);
    expect(loadProfile()).toBe(profile);
  });

  it('saveProfile overwrites a previous value', () => {
    saveProfile('first');
    saveProfile('second');
    expect(loadProfile()).toBe('second');
  });

  it('clearProfile removes the stored value', () => {
    saveProfile('something');
    clearProfile();
    expect(loadProfile()).toBeNull();
  });

  it('clearProfile on empty storage does not throw', () => {
    expect(() => clearProfile()).not.toThrow();
  });

  // Edge cases for restricted-storage scenarios (quota exceeded, private
  // browsing, SecurityError on getter access). We mock at the prototype so
  // the helpers exercise the same call sites the runtime would hit.

  it('loadProfile returns null when getItem throws', () => {
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('SecurityError');
    });
    expect(loadProfile()).toBeNull();
  });

  it('saveProfile returns false and logs a warning when setItem throws', () => {
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('QuotaExceededError');
    });
    expect(saveProfile('{"x":1}')).toBe(false);
    expect(warnSpy).toHaveBeenCalled();
  });

  it('saveProfile logs a warning when localStorage is unavailable', () => {
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    // Simulate a browser that throws SecurityError on the localStorage
    // getter (some private-browsing contexts, sandboxed iframes).
    const originalDescriptor = Object.getOwnPropertyDescriptor(window, 'localStorage');
    Object.defineProperty(window, 'localStorage', {
      configurable: true,
      get: () => { throw new Error('SecurityError'); },
    });
    try {
      expect(saveProfile('{"x":1}')).toBe(false);
      expect(warnSpy).toHaveBeenCalledWith(
        expect.stringContaining('localStorage unavailable'),
      );
    } finally {
      if (originalDescriptor) {
        Object.defineProperty(window, 'localStorage', originalDescriptor);
      }
    }
  });

  it('clearProfile swallows removeItem errors', () => {
    const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    vi.spyOn(Storage.prototype, 'removeItem').mockImplementation(() => {
      throw new Error('SecurityError');
    });
    expect(() => clearProfile()).not.toThrow();
    expect(warnSpy).toHaveBeenCalled();
  });
});
