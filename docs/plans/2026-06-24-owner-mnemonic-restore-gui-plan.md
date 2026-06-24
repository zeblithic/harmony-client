# Owner-mnemonic restore GUI affordance — implementation plan

**Ticket:** ZEB-454 (GUI follow-up to ZEB-439, which shipped the headless owner-restore path in PR #241).

**Goal:** A GUI affordance to re-adopt the owner identity from its 24-word master-seed mnemonic, in two places: the identity/devices settings flow (existing install) and the fresh-install onboarding (total-loss recovery on a new machine). Same guards as the headless `harmony-app restore owner-mnemonic`.

**Architecture:** Surface-only. The backend orchestration already exists (`recovery_cli::restore_owner_mnemonic_with_keychain`). We factor a words-array variant + an owner-id preview, expose them as two Tauri commands, and build a single reusable Svelte restore wizard used by both DevicesPanel (Settings) and WelcomeModal (fresh install). Mirrors the existing Reticulum-seed restore wizard in `IdentityPanel.svelte` and reuses `TypedConfirmationModal.svelte`.

## Confirmation tiers (settled with Jake)

| Case | Behavior |
|---|---|
| No existing owner (fresh install / WelcomeModal) | Preview owner-id → single non-destructive "Restore identity" confirm → `restore(force=false)`. |
| Existing owner, **same** owner-id (Settings re-adopt) | Typed-confirm (type current owner-id 8-char prefix, mirroring the Reticulum gate) → `restore(force=true)`. |
| Existing owner, **different** owner-id | Hard refuse in the UI with a clear message; backend also refuses even with force. No override. |

Secret hygiene: never echo words to logs; the Rust path already wraps the phrase/seed in `Zeroizing`.

## Global Constraints

- ZEB id stays out of branch/commit/PR titles (PR body only).
- Gates: `cargo fmt --all -- --check`; `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E '<filter>'`; `npx tsc --noEmit`; `npx vitest run`. Final clippy/nextest sweep with `--all-targets` left to CI (relink cost).
- Keychain-hermetic tests: inject `keychain: None` via the `*_with_keychain` seam; set `HARMONY_PASSPHRASE`. Never `KeychainStore::new()` in test-reachable code.

## Task 1 — Backend: factor words-array owner restore + owner-id preview (`recovery_cli.rs`)

**Interfaces produced:**
- `pub fn preview_owner_mnemonic_owner_id(words: &[String]) -> Result<String, String>` — derive owner-id hex from 24 words, no disk write.
- `pub fn restore_owner_mnemonic_from_words_with_keychain(identity_dir: &Path, words: &[String], force: bool, keychain: Option<KeychainStore>) -> Result<String, String>` — same guards as the file path; returns restored owner-id hex.
- Refactor `restore_owner_mnemonic_with_keychain` (file path) to read words then delegate to the words variant + `eprintln!` the returned owner-id (mirrors the Reticulum `restore_mnemonic_from_words_with_keychain` / `restore_mnemonic_with_keychain` split at lines 860/878).

**Tests (recovery_cli.rs `#[cfg(test)]`, hermetic):**
- `preview_owner_mnemonic_owner_id_matches_restored_owner_id` — preview hex == owner-id after a real restore onto a fresh dir (round-trip through `export_owner_mnemonic_words_with_keychain` words).
- `preview_owner_mnemonic_owner_id_rejects_wrong_word_count` — 23 words → Err.
- `restore_owner_mnemonic_from_words_roundtrips_owner_id_onto_fresh_device` — words variant, empty dir, force=false → Ok(owner-id), re-export round-trips.
- `restore_owner_mnemonic_from_words_refuses_existing_without_force` / `...with_force_refuses_different_owner` / `...with_force_readopts_same_owner` — words-variant analogs of the existing file-path guard tests.
- Existing file-path tests must still pass unchanged (the refactor preserves them).

## Task 2 — Backend: two Tauri commands + registration (`owner_commands.rs`, `lib.rs`)

**Interfaces produced (camelCase args at the JS boundary):**
- `#[tauri::command] pub async fn preview_owner_mnemonic_identity(words: Vec<String>) -> Result<String, String>` → `run_blocking(|| recovery_cli::preview_owner_mnemonic_owner_id(&words))`.
- `#[tauri::command] pub async fn restore_owner_mnemonic_from_words(words: Vec<String>, force: bool) -> Result<String, String>` → resolve `resolve_identity_dir()`, `run_blocking(|| recovery_cli::restore_owner_mnemonic_from_words_with_keychain(&dir, &words, force, KeychainStore::new().ok()))`.
- Register both in the `tauri::generate_handler!` list in `lib.rs` (next to `owner_commands::issue_owner_recovery_token`).

## Task 3 — Frontend: reusable owner-restore wizard (`src/lib/components/OwnerRestoreWizard.svelte` + test)

A self-contained component driving: paste-24-words → live word-count/validation → preview (`preview_owner_mnemonic_identity`) showing the restored owner-id → tier branch (per table) → commit (`restore_owner_mnemonic_from_words`) → success. Props: `{ currentOwnerId: string | null, onRestored: (ownerId: string) => void, onCancel: () => void }`. `currentOwnerId == null` ⇒ fresh-install/non-destructive path; non-null ⇒ compare for same/different. Reuses `TypedConfirmationModal` for the same-owner overwrite gate. Errors surfaced inline (invoke error extraction: `e instanceof Error ? e.message : String(e)`).

**Tests (vitest, mocked adapter):** word-count gating; preview→same-owner shows typed-confirm and only commits with force=true; preview→different-owner refuses without ever calling restore; fresh-install (currentOwnerId null) commits force=false; restore error surfaced.

## Task 4 — Frontend: wire into DevicesPanel (Settings → Account)

Add a "Restore from recovery phrase" entry point in the owner identity section of `DevicesPanel.svelte` (sibling to the existing owner backup), opening `OwnerRestoreWizard` with `currentOwnerId` = the panel's current owner-id. `onRestored` → reload (mirrors the pairing-join `handleJoinComplete` page reload so a fresh `start_node` loads the new owner_state).

## Task 5 — Frontend: wire into WelcomeModal (fresh install)

Add a `'restore-mnemonic'` stage + an "Already have an identity? Restore from your recovery phrase" entry on the `explain` stage (alongside mint / join-via-pairing). Render `OwnerRestoreWizard` with `currentOwnerId={null}`. `onRestored` → reload (same as the existing `handleJoinComplete`).

## Verification

- Rust: fmt + `clippy -p harmony-app --lib` + `nextest -p harmony-app --lib -E 'test(owner_mnemonic)'` green.
- Frontend: `tsc --noEmit` + full `vitest run` green.
- DoD: GUI restore re-adopts the owner (same owner-id) with owner-id shown before the irreversible write; refuses a different identity; `export owner-mnemonic` round-trips the same words after a GUI restore (covered by the Task 1 round-trip test through the shared primitive).
