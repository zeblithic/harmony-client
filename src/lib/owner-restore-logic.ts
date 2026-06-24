/**
 * Pure decision logic for the owner-mnemonic restore wizard (ZEB-454),
 * extracted so the security-critical tier classification is unit-testable
 * without standing up Tauri or the Svelte component.
 *
 * The wizard re-adopts the owner identity from its 24-word master-seed
 * mnemonic. The backend (`restore_owner_mnemonic_from_words`) is the
 * authoritative guard; this module decides the UI tier + the `force` flag so
 * the destructive case is gated behind a typed confirm and a different-owner
 * phrase is refused before any write is attempted.
 */

/** How a pasted phrase relates to the device's current owner identity. */
export type RestoreTier =
  /** No owner on this device (fresh install / WelcomeModal) — non-destructive. */
  | { kind: 'fresh' }
  /** Re-adopts the SAME owner already on this device — destructive (replaces
   *  the device key / owner_state), so it needs a typed confirm + force. */
  | { kind: 'readopt-same' }
  /** Belongs to a DIFFERENT owner — refuse outright, never restore. */
  | { kind: 'different-owner' };

/**
 * Classify a restore against the device's current owner-id. owner-ids are hex
 * from the backend; compared case-insensitively (normalized defensively).
 * `currentOwnerId == null`/empty means no owner on this device.
 */
export function classifyRestore(
  currentOwnerId: string | null,
  restoredOwnerId: string,
): RestoreTier {
  if (currentOwnerId == null || currentOwnerId.length === 0) return { kind: 'fresh' };
  return currentOwnerId.toLowerCase() === restoredOwnerId.toLowerCase()
    ? { kind: 'readopt-same' }
    : { kind: 'different-owner' };
}

/**
 * The `force` flag for `restore_owner_mnemonic_from_words`. Only same-owner
 * re-adoption overwrites an existing owner (force=true); a fresh install has
 * nothing to overwrite (force=false). `different-owner` never reaches restore
 * (the UI refuses), so it maps to false defensively.
 */
export function restoreForceFlag(tier: RestoreTier): boolean {
  return tier.kind === 'readopt-same';
}

/** The 8-char owner-id prefix the typed-confirm gate requires (mirrors the
 *  Reticulum restore gate). */
export function ownerIdPrefix(ownerId: string): string {
  return ownerId.slice(0, 8);
}

/** Split a pasted phrase into BIP39 words (whitespace-separated, no empties). */
export function parseMnemonicWords(input: string): string[] {
  return input.split(/\s+/).filter((w) => w.length > 0);
}

/** Word count of a pasted phrase (for the 24-word gate + live counter). */
export function countMnemonicWords(input: string): number {
  return parseMnemonicWords(input).length;
}

/**
 * Map a raw `preview_owner_mnemonic_identity` rejection to friendly copy.
 * Mirrors the Reticulum restore wizard's error mapping.
 */
export function friendlyPreviewError(raw: string): string {
  if (/checksum/i.test(raw)) {
    return "These 24 words don't form a valid recovery phrase. Double-check your transcription.";
  }
  if (/wordlist|not.*word/i.test(raw)) {
    return "One or more words isn't a recognized recovery word.";
  }
  return `Could not parse recovery phrase: ${raw}`;
}
