import { describe, it, expect } from 'vitest';
import { identityKeyBackupNote, normalizeIdentityStoreBackend } from './identity-backup-copy';

describe('normalizeIdentityStoreBackend', () => {
  it('passes through the two known backends', () => {
    expect(normalizeIdentityStoreBackend('keychain')).toBe('keychain');
    expect(normalizeIdentityStoreBackend('encrypted-file')).toBe('encrypted-file');
  });

  it('maps anything else — unknown value, empty, null, undefined — to "unknown"', () => {
    expect(normalizeIdentityStoreBackend('KEYCHAIN')).toBe('unknown');
    expect(normalizeIdentityStoreBackend('secret-service')).toBe('unknown');
    expect(normalizeIdentityStoreBackend('')).toBe('unknown');
    expect(normalizeIdentityStoreBackend(null)).toBe('unknown');
    expect(normalizeIdentityStoreBackend(undefined)).toBe('unknown');
  });
});

describe('identityKeyBackupNote (ZEB-768)', () => {
  it('only the keychain backend mentions the keychain', () => {
    expect(identityKeyBackupNote('keychain').toLowerCase()).toContain('keychain');
  });

  it('the encrypted-file note names the encrypted file and never claims a keychain', () => {
    const note = identityKeyBackupNote('encrypted-file').toLowerCase();
    expect(note).toContain('encrypted file');
    // The whole point of the ticket: a file-store user must not be told
    // their key is in a keychain it isn't in.
    expect(note).not.toContain('keychain');
  });

  it('the unknown fallback is backend-neutral and never claims a keychain', () => {
    const note = identityKeyBackupNote('unknown').toLowerCase();
    expect(note).not.toContain('keychain');
    expect(note).toContain('stored on this device');
  });

  it('every backend still frames the recovery file as the portable backup', () => {
    for (const backend of ['keychain', 'encrypted-file', 'unknown'] as const) {
      expect(identityKeyBackupNote(backend).toLowerCase()).toContain('portable backup');
    }
  });
});
