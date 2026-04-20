import { describe, it, expect, beforeEach } from 'vitest';
import { loadProfile, saveProfile, clearProfile } from './stq8-profile-storage';

describe('stq8-profile-storage', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it('loadProfile returns null when nothing is stored', () => {
    expect(loadProfile()).toBeNull();
  });

  it('saveProfile then loadProfile round-trips the exact string', () => {
    const profile = '{"centroids":{"0":[1,2,3]},"created":1700000000}';
    saveProfile(profile);
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
});
