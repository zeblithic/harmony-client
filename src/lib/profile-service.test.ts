import { describe, it, expect, beforeEach } from 'vitest';
import { loadProfile, saveProfile } from './profile-service';

describe('profile-service', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it('generates a unique random address on first load', () => {
    const profile = loadProfile();
    expect(profile.displayName).toBe('Anonymous');
    expect(profile.address).not.toBe('local');
    expect(profile.address.length).toBe(32); // 16 bytes = 32 hex chars
    expect(profile.address).toMatch(/^[0-9a-f]{32}$/);
  });

  it('persists address on first load so subsequent loads return same address', () => {
    const first = loadProfile();
    const second = loadProfile();
    expect(first.address).toBe(second.address);
  });

  it('roundtrips through save and load', () => {
    saveProfile({
      address: 'deadbeef01020304aabbccdd11223344',
      displayName: 'Alice',
      statusText: 'Building the mesh',
    });
    const loaded = loadProfile();
    expect(loaded.displayName).toBe('Alice');
    expect(loaded.address).toBe('deadbeef01020304aabbccdd11223344');
    expect(loaded.statusText).toBe('Building the mesh');
  });

  it('generates address for stored profile missing address', () => {
    localStorage.setItem('harmony-profile', JSON.stringify({ displayName: 'Bob' }));
    const loaded = loadProfile();
    expect(loaded.displayName).toBe('Bob');
    expect(loaded.address.length).toBe(32);
    expect(loaded.address).not.toBe('local');
  });

  it('migrates legacy "local" address to a unique one', () => {
    saveProfile({ address: 'local', displayName: 'Legacy' });
    const loaded = loadProfile();
    expect(loaded.displayName).toBe('Legacy');
    expect(loaded.address).not.toBe('local');
    expect(loaded.address.length).toBe(32);
    // Verify migration is persisted
    const reloaded = loadProfile();
    expect(reloaded.address).toBe(loaded.address);
  });

  it('handles corrupt localStorage gracefully', () => {
    localStorage.setItem('harmony-profile', 'not-json!!!');
    const loaded = loadProfile();
    expect(loaded.displayName).toBe('Anonymous');
    expect(loaded.address.length).toBe(32);
  });

  it('filters null values from stored JSON', () => {
    localStorage.setItem('harmony-profile', JSON.stringify({
      address: null,
      displayName: null,
      statusText: 'valid',
    }));
    const loaded = loadProfile();
    // null address gets migrated to a random one
    expect(loaded.address.length).toBe(32);
    expect(loaded.displayName).toBe('Anonymous');
    expect(loaded.statusText).toBe('valid');
  });

  it('load + merge + save roundtrip', () => {
    const initial = loadProfile();
    saveProfile({ ...initial, displayName: 'Original' });
    const current = loadProfile();
    const updated = { ...current, statusText: 'New status' };
    saveProfile(updated);

    const reloaded = loadProfile();
    expect(reloaded.displayName).toBe('Original');
    expect(reloaded.statusText).toBe('New status');
    expect(reloaded.address).toBe(initial.address);
  });
});
