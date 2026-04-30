//! Recovery-file export policy constants shared between the
//! owner-recovery export IPC (`crate::owner_commands`) and the
//! identity-recovery export IPC (`crate::identity_commands`).
//!
//! Mirrored in `src/lib/recovery-policy.ts`. The drift detector
//! `src/lib/recovery-policy.test.ts` asserts both files agree on
//! the integer literals; failing that test in CI is the signal
//! to re-sync.

/// Minimum recovery passphrase length, in Unicode codepoints
/// (matches the JS frontend's `[...str].length` check).
pub const MIN_RECOVERY_PASSPHRASE_LEN: usize = 12;

/// Maximum recovery comment length, in bytes. Mirrors harmony-owner's
/// hard cap on the underlying `comment` field; both the GUI IPC and
/// the encryption layer enforce this. The GUI pre-validates against
/// this constant so the user sees a friendly error before any I/O
/// rather than a generic encryption-layer error. The TS mirror MUST
/// equal this value — the drift detector in
/// `src/lib/recovery-policy.test.ts` enforces parity.
pub const MAX_RECOVERY_COMMENT_BYTES: usize = 256;
