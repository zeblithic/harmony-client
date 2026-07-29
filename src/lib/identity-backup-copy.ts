/**
 * ZEB-768: the onboarding backup step ("Your identity is ready") must not
 * claim the OS keychain holds the identity key when the encrypted-file store
 * is actually in use — which happens on any Linux desktop without a Secret
 * Service / keyring provider, under `HARMONY_DISABLE_KEYCHAIN`, or for a
 * named profile. That screen is the one place a user learns where their only
 * identity key lives, immediately before a "Skip for now" on the backup, so
 * overstating durability there has real consequences (there is no central
 * recovery).
 *
 * The backend is reported by the `identity_store_backend` IPC getter as
 * `"keychain" | "encrypted-file"`. Anything else — an unrecognized value or a
 * failed call — collapses to `"unknown"` and yields backend-neutral wording,
 * never an unearned keychain claim.
 */
export type IdentityStoreBackend = 'keychain' | 'encrypted-file' | 'unknown';

/** Map the raw IPC string (or a failure/`null`) onto the known backend set. */
export function normalizeIdentityStoreBackend(
  raw: string | null | undefined,
): IdentityStoreBackend {
  return raw === 'keychain' || raw === 'encrypted-file' ? raw : 'unknown';
}

/**
 * The backup-step note describing where the identity key lives. Only the
 * `keychain` case may mention the keychain; the other two must not, so a
 * file-store user is never told their key is in a keychain it isn't in.
 */
export function identityKeyBackupNote(backend: IdentityStoreBackend): string {
  switch (backend) {
    case 'keychain':
      return "Your identity key is already stored in this device's secure keychain — " +
        'this recovery file is your portable backup for a lost or replaced device.';
    case 'encrypted-file':
      // The file is protected only by the Harmony passphrase (Argon2id +
      // XChaCha20-Poly1305) — no machine-derived key participates, so it is
      // portable, NOT machine-bound: the file plus the passphrase open it on
      // any device. Claiming otherwise would give a false durability/security
      // expectation (Qodo, PR #570).
      return 'Your identity key is saved as an encrypted file on this device ' +
        '(in ~/.harmony), protected by your Harmony passphrase — this recovery file ' +
        'is your portable backup for a lost or replaced device.';
    case 'unknown':
      return 'Your identity key is stored on this device — this recovery file is your ' +
        'portable backup for a lost or replaced device.';
  }
}
