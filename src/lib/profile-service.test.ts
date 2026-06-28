import { describe, it, expect, beforeEach } from 'vitest';
import { loadProfile, saveProfile } from './profile-service';

const OWNER_A = 'aaaa0000aaaa0000aaaa0000aaaa0000';
const OWNER_B = 'bbbb1111bbbb1111bbbb1111bbbb1111';

describe('profile-service (owner-scoped)', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it('generates a unique random address on first owner-scoped load', () => {
    const profile = loadProfile(OWNER_A);
    expect(profile.displayName).toBe('Anonymous');
    expect(profile.address).not.toBe('local');
    expect(profile.address.length).toBe(32); // 16 bytes = 32 hex chars
    expect(profile.address).toMatch(/^[0-9a-f]{32}$/);
  });

  it('persists address on first owner-scoped load so subsequent loads return same address', () => {
    const first = loadProfile(OWNER_A);
    const second = loadProfile(OWNER_A);
    expect(first.address).toBe(second.address);
  });

  it('roundtrips through owner-scoped save and load', () => {
    saveProfile(
      {
        address: 'deadbeef01020304aabbccdd11223344',
        displayName: 'Alice',
        statusText: 'Building the mesh',
      },
      OWNER_A,
    );
    const loaded = loadProfile(OWNER_A);
    expect(loaded.displayName).toBe('Alice');
    expect(loaded.address).toBe('deadbeef01020304aabbccdd11223344');
    expect(loaded.statusText).toBe('Building the mesh');
  });

  // ── ZEB-586 regression: the display name must not leak across identities ──
  it('does NOT leak one owner profile into another owner (ZEB-586)', () => {
    saveProfile({ address: 'a'.repeat(32), displayName: 'Alice' }, OWNER_A);
    const b = loadProfile(OWNER_B);
    expect(b.displayName).toBe('Anonymous'); // a fresh identity is NOT named Alice
    expect(b.address).not.toBe('a'.repeat(32));
  });

  it('owner-less load returns an ephemeral Anonymous default and ignores any legacy global key (ZEB-586)', () => {
    // The pre-fix bug: a fixed, owner-agnostic key whose value leaks to every
    // identity. An owner-less load must NOT surface it.
    localStorage.setItem(
      'harmony-profile',
      JSON.stringify({ address: 'c'.repeat(32), displayName: 'Legacy Leak' }),
    );
    const p = loadProfile();
    expect(p.displayName).toBe('Anonymous');
    expect(p.address).not.toBe('c'.repeat(32));
  });

  it('owner-less load persists nothing (writes no shared key)', () => {
    loadProfile();
    expect(localStorage.length).toBe(0);
  });

  it('generates address for stored owner-scoped profile missing address', () => {
    localStorage.setItem(
      `harmony-profile:owner-${OWNER_A}`,
      JSON.stringify({ displayName: 'Bob' }),
    );
    const loaded = loadProfile(OWNER_A);
    expect(loaded.displayName).toBe('Bob');
    expect(loaded.address.length).toBe(32);
    expect(loaded.address).not.toBe('local');
  });

  it('migrates legacy "local" address to a unique one (owner-scoped)', () => {
    saveProfile({ address: 'local', displayName: 'Legacy' }, OWNER_A);
    const loaded = loadProfile(OWNER_A);
    expect(loaded.displayName).toBe('Legacy');
    expect(loaded.address).not.toBe('local');
    expect(loaded.address.length).toBe(32);
    // Verify migration is persisted under the owner-scoped key
    const reloaded = loadProfile(OWNER_A);
    expect(reloaded.address).toBe(loaded.address);
  });

  it('handles corrupt owner-scoped localStorage gracefully', () => {
    localStorage.setItem(`harmony-profile:owner-${OWNER_A}`, 'not-json!!!');
    const loaded = loadProfile(OWNER_A);
    expect(loaded.displayName).toBe('Anonymous');
    expect(loaded.address.length).toBe(32);
  });

  it('filters null values from stored JSON', () => {
    localStorage.setItem(
      `harmony-profile:owner-${OWNER_A}`,
      JSON.stringify({ address: null, displayName: null, statusText: 'valid' }),
    );
    const loaded = loadProfile(OWNER_A);
    // null address gets migrated to a random one
    expect(loaded.address.length).toBe(32);
    expect(loaded.displayName).toBe('Anonymous');
    expect(loaded.statusText).toBe('valid');
  });

  it('owner-scoped load + merge + save roundtrip', () => {
    const initial = loadProfile(OWNER_A);
    saveProfile({ ...initial, displayName: 'Original' }, OWNER_A);
    const current = loadProfile(OWNER_A);
    const updated = { ...current, statusText: 'New status' };
    saveProfile(updated, OWNER_A);

    const reloaded = loadProfile(OWNER_A);
    expect(reloaded.displayName).toBe('Original');
    expect(reloaded.statusText).toBe('New status');
    expect(reloaded.address).toBe(initial.address);
  });
});
