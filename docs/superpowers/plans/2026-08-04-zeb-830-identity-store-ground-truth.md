# ZEB-830 — `identity_store_backend` ground-truth reporting — Implementation Plan

> **For agentic workers:** Steps use checkbox (`- [ ]`) syntax. TDD: failing test → minimal impl → green → commit.

**Goal:** Report the backend the owner seed *actually loaded from* (not keychain availability), and re-query it after mint, so onboarding backup copy never overclaims the keychain.

**Architecture:** Extract `load_secret`'s keychain→file precedence into a shared `locate_secret` returning the backend + bytes; `load_secret` wraps it, a new `owner_master_seed_backend` probe reads it and drops the bytes. The getter reports the probe. One frontend re-query after `mint()`.

**Tech Stack:** Rust (Tauri IPC, `keyring` v3, `Zeroizing`), Svelte 5, vitest.

## Global Constraints

- Rust gates from `src-tauri/`: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo fmt --all -- --check`.
- Frontend gates from repo root: `npx tsc --noEmit`, `npx vitest run`.
- ZEB-428: never construct `KeychainStore::new()` in code reachable from a committed test; file/neutral test paths pass `use_os_keychain=false` and set `HARMONY_PASSPHRASE`.
- Neutral backend value is the string `"unknown"` (frontend `normalizeIdentityStoreBackend` already maps it to neutral copy).
- `KEYCHAIN_MASTER_SEED = "master_seed"`, fallback file `"master_seed.enc"`, `VaultSlot::OwnerMasterSeed`.

---

### Task 1: `SeedBackend` + `locate_secret` extraction + `owner_master_seed_backend` probe

**Files:**
- Modify: `src-tauri/src/owner_state.rs` (`load_secret` ~1026; add enum, `locate_secret`, probe)
- Test: `src-tauri/src/owner_state.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `pub enum SeedBackend { Keychain, EncryptedFile }`; `pub fn owner_master_seed_backend(use_os_keychain: bool, identity_dir: &Path) -> Result<Option<SeedBackend>, String>`.

- [ ] **Step 1 — failing tests.** Add to owner_state tests: (a) `owner_master_seed_backend_reports_encrypted_file_when_seed_in_file` — tempdir, `HARMONY_PASSPHRASE` set, write a seed to the file via `save_secret(false, OwnerMasterSeed, KEYCHAIN_MASTER_SEED, dir, "master_seed.enc", &seed)`, assert `owner_master_seed_backend(false, dir) == Ok(Some(SeedBackend::EncryptedFile))`; (b) `owner_master_seed_backend_reports_none_when_no_seed_anywhere` — fresh tempdir + `HARMONY_PASSPHRASE`, assert `Ok(None)`. Use the existing test env-guard pattern (`home_override`/serialized env) already used by owner_state tests.
- [ ] **Step 2 — run, expect fail** (`owner_master_seed_backend` undefined): `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(owner_master_seed_backend)'`.
- [ ] **Step 3 — implement.** Add `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum SeedBackend { Keychain, EncryptedFile }`. Extract current `load_secret` body verbatim into `fn locate_secret(...) -> Result<Option<(SeedBackend, Zeroizing<[u8;32]>)>, String>`: keychain hit → `Ok(Some((SeedBackend::Keychain, key)))`; file `store.load()` → `Ok(Some(seed)) => Ok(Some((EncryptedFile, seed)))`, `Ok(None) => Ok(None)`, `Err => Err`; preserve the `keychain_err`/no-fallback `Err` branch exactly. Make `load_secret` a wrapper: `Ok(locate_secret(...)?.map(|(_, b)| b))`. Add the `pub fn owner_master_seed_backend` mapping `.map(|(b, _)| b)`.
- [ ] **Step 4 — run, expect pass** (same filter). Then regression: `-E 'test(load_secret) or test(save_owner_state) or test(mint)'` still green.
- [ ] **Step 5 — commit** (`git add -A && git commit`): `feat(zeb-830): locate_secret + owner_master_seed_backend probe (ground truth)`.

### Task 2: getter rewrite + label helper refactor + string contract

**Files:**
- Modify: `src-tauri/src/identity_commands.rs` (`identity_store_backend_label` ~568, `identity_store_backend` ~601)
- Test: `identity_commands.rs` `#[cfg(test)] mod tests` (the existing `identity_store_backend_label_pins_the_frontend_string_contract` ~803)

**Interfaces:**
- Consumes: `owner_state::{SeedBackend, owner_master_seed_backend}`, `identity::resolve_path`, `KeychainStore`.

- [ ] **Step 1 — update the failing contract test.** Rewrite `identity_store_backend_label_pins_the_frontend_string_contract` to the new signature: `identity_store_backend_label(Some(SeedBackend::Keychain)) == "keychain"`, `Some(SeedBackend::EncryptedFile) == "encrypted-file"`, `None == "unknown"`.
- [ ] **Step 2 — run, expect fail** (signature mismatch): `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(identity_store_backend_label)'`.
- [ ] **Step 3 — implement.** Refactor `identity_store_backend_label(backend: Option<owner_state::SeedBackend>) -> &'static str` (match → the three strings). Rewrite `identity_store_backend`: resolve `identity::resolve_path(None)?` outside `run_blocking`; inside, `let use_os_keychain = KeychainStore::new().is_ok();` then `owner_state::owner_master_seed_backend(use_os_keychain, &identity_path).unwrap_or_else(|e| { tracing::debug!(...); None })`, pass to label. Add `use` for `owner_state::SeedBackend` if it reads cleaner.
- [ ] **Step 4 — run, expect pass**; then `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` (catches unused old helper/imports) and `cargo fmt --all`.
- [ ] **Step 5 — commit:** `feat(zeb-830): identity_store_backend reports ground truth, not availability`.

### Task 3: WelcomeModal post-mint re-query + test

**Files:**
- Modify: `src/lib/components/WelcomeModal.svelte` (`handleCreateIdentity` ~98)
- Test: `src/lib/components/__tests__/WelcomeModal.*` (create/extend)

**Interfaces:**
- Consumes: `invoke('identity_store_backend')`, `normalizeIdentityStoreBackend`, `identityKeyBackupNote`.

- [ ] **Step 1 — failing test.** In a WelcomeModal component test, mock `invoke` so `identity_store_backend` returns `'encrypted-file'` and `mint` succeeds; drive `handleCreateIdentity` (click Create); assert the backup-step note renders the encrypted-file copy (i.e. the backend was re-queried after mint, not left at the onMount value). If no WelcomeModal test file exists, create one mirroring existing component-test setup.
- [ ] **Step 2 — run, expect fail:** `npx vitest run src/lib/components/__tests__/WelcomeModal`.
- [ ] **Step 3 — implement.** After `mintResult = result;` and before `stage = 'backup';`, re-query inside a `try/catch` (debug-log on failure), assigning `identityBackend = normalizeIdentityStoreBackend(await invoke<string>('identity_store_backend'))`.
- [ ] **Step 4 — run, expect pass;** then `npx tsc --noEmit`.
- [ ] **Step 5 — commit:** `feat(zeb-830): re-query identity backend after mint in WelcomeModal`.

### Task 4: Full gate sweep

- [ ] **Step 1 — Rust full:** `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (green).
- [ ] **Step 2 — clippy + fmt:** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all -- --check`.
- [ ] **Step 3 — Frontend:** from repo root `npx tsc --noEmit && npx vitest run`.
- [ ] **Step 4 — commit** any fmt/fixes if not already committed.

### Task 5: Koya real-Keychain validation (keychain branch) — NOT committed

- [ ] **Step 1 — throwaway test.** Write a `#[ignore]`d test (scratch, gated `HARMONY_ALLOW_REAL_KEYCHAIN=1`) that, against the real macOS keychain: writes a seed to the keychain vault slot (`use_os_keychain=true`) and asserts `owner_master_seed_backend(true, dir) == Some(Keychain)`; clears the slot + writes file-only → `Some(EncryptedFile)`; clears both → `None`. Koya's `harmony/identity` is a disposable dev identity — free to write/clear.
- [ ] **Step 2 — run on Koya:** `HARMONY_ALLOW_REAL_KEYCHAIN=1 cargo nextest run ... --run-ignored ignored-only -E 'test(zeb830_real_keychain)'`; capture output.
- [ ] **Step 3 — revert the throwaway** (`git checkout`/delete) so nothing that writes the global slot is committed. Save the captured output for the PR body.

### Task 6: PR + converge

- [ ] **Step 1 — push branch, open PR** with `Closes ZEB-830`, design/plan links, and the Koya validation output. Fire `@coderabbitai review` ONCE.
- [ ] **Step 2 — wait CI green + bots; scan all 3 comment buckets; bundle findings→fix→verify→push once/round.** Never auto-merge.
- [ ] **Step 3 — mark merge-ready; pushover; await Jake's merge; post-merge housekeeping.**

---

## Self-Review

- **Spec coverage:** getter ground truth (T2), re-query after mint (T3), neutral value (T2 label `None→"unknown"` + frontend), CI file/neutral/contract (T1/T2), Koya keychain check (T5). ✓
- **Placeholders:** none — code shapes are in the spec; tests name concrete assertions.
- **Type consistency:** `SeedBackend` variants and `owner_master_seed_backend` signature identical across T1/T2. ✓
