import { describe, it, expect, beforeEach } from 'vitest';
import { loadProfile, saveProfile } from './profile-service';

describe('profile-service', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it('returns default profile when nothing stored', () => {
    const profile = loadProfile();
    expect(profile.displayName).toBe('Anonymous');
    expect(profile.address).toBe('local');
  });

  it('roundtrips through save and load', () => {
    saveProfile({
      address: 'deadbeef',
      displayName: 'Alice',
      statusText: 'Building the mesh',
    });
    const loaded = loadProfile();
    expect(loaded.displayName).toBe('Alice');
    expect(loaded.address).toBe('deadbeef');
    expect(loaded.statusText).toBe('Building the mesh');
  });

  it('merges stored profile with defaults', () => {
    // Store a partial profile (missing address)
    localStorage.setItem('harmony-profile', JSON.stringify({ displayName: 'Bob' }));
    const loaded = loadProfile();
    expect(loaded.displayName).toBe('Bob');
    expect(loaded.address).toBe('local'); // default
  });

  it('handles corrupt localStorage gracefully', () => {
    localStorage.setItem('harmony-profile', 'not-json!!!');
    const loaded = loadProfile();
    expect(loaded.displayName).toBe('Anonymous');
  });

  it('filters null values from stored JSON', () => {
    localStorage.setItem('harmony-profile', JSON.stringify({
      address: null,
      displayName: null,
      statusText: 'valid',
    }));
    const loaded = loadProfile();
    expect(loaded.address).toBe('local'); // default, not null
    expect(loaded.displayName).toBe('Anonymous'); // default, not null
    expect(loaded.statusText).toBe('valid');
  });

  it('load + merge + save roundtrip', () => {
    saveProfile({ address: 'local', displayName: 'Original' });
    const current = loadProfile();
    const updated = { ...current, statusText: 'New status' };
    saveProfile(updated);

    const reloaded = loadProfile();
    expect(reloaded.displayName).toBe('Original');
    expect(reloaded.statusText).toBe('New status');
  });
});
