//! Common test helpers for harmony-app integration tests.
//!
//! Each integration test that imports from this module must `mod common;`
//! at its top — Cargo links the file per binary.

#[cfg(feature = "test-fixtures")]
pub mod library_fixtures;

#[cfg(feature = "test-fixtures")]
pub mod profile_fixtures;

/// Drop-based env-var guard. Sets the variable on construction and
/// removes it on drop, so a test panic mid-way through cannot leak the
/// variable into the next test (round-1 bot finding M3 — CodeAnt+CodeRabbit).
///
/// `#[serial]` alone serializes tests but doesn't help if one panics
/// between `set_var` and `remove_var` — the env var leaks. Wrap every
/// `set_var` site with this guard:
///
/// ```ignore
/// let _guard = common::set_env("HARMONY_PASSPHRASE", "test-pass");
/// // ... test body; guard drops at end of scope and cleans up.
/// ```
///
/// On a panic, the Drop impl still runs during unwind, so the cleanup
/// fires regardless of test outcome.
///
/// `dead_code` is allowed because each integration test binary gets its
/// own copy of this module via `mod common;` — binaries that don't use
/// the env helper would otherwise fail dead-code lints.
#[allow(dead_code)]
pub struct EnvVarGuard {
    name: &'static str,
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.name);
    }
}

/// Set an env var and return a Drop-based guard that clears it on
/// scope exit. See [`EnvVarGuard`] for rationale (round-1 bot M3).
#[allow(dead_code)]
#[must_use = "EnvVarGuard must be bound to a `let _guard = ...` so its Drop runs at scope exit"]
pub fn set_env(name: &'static str, value: &str) -> EnvVarGuard {
    std::env::set_var(name, value);
    EnvVarGuard { name }
}
