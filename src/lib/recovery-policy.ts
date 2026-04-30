// Recovery-file export policy. Mirrored from
// src-tauri/src/recovery_policy.rs. The values MUST match;
// `recovery-policy.test.ts` reads the Rust file and asserts
// equality so drift fails CI.

/** Minimum recovery passphrase length, in Unicode codepoints
 * (matches the Rust backend's `passphrase.chars().count()` check). */
export const MIN_RECOVERY_PASSPHRASE_LEN = 12;

/** Maximum recovery comment length, in bytes. Mirrors
 * harmony-owner's hard cap on the underlying field. */
export const MAX_RECOVERY_COMMENT_BYTES = 256;
