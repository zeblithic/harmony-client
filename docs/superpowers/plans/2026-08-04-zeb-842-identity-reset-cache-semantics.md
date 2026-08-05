# ZEB-842 — Identity-reset cache semantics — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans (inline). TDD: failing test → minimal impl → green → commit. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Add a typed-confirm `erase_all_local_data` clean-slate wipe (identity + app-data subtrees, minus `profiles/`/`logs/`), reachable from both the boot-failure modal and Settings; keep the recovery reset non-destructive and make its copy honest.

**Architecture:** A new Tauri command `erase_all_local_data` hard-deletes the active profile's `identity_dir` and `app_data_dir` children except `profiles/` and `logs/`, reusing the reset's locking/keychain machinery. Two frontend surfaces (`StartupRecoveryOptions`, `IdentityPanel`) call the one command behind an `ERASE` typed-confirm. Design: `docs/superpowers/specs/2026-08-04-zeb-842-identity-reset-cache-semantics-design.md`.

**Tech Stack:** Rust (Tauri IPC, `tempfile` in tests), Svelte 5, vitest.

## Global Constraints

- Rust gates from `src-tauri/`: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo fmt --all -- --check`.
- Frontend gates from repo root: `npx tsc --noEmit`, `npx vitest run`.
- ZEB-428: never construct `KeychainStore::new()` in test-reachable code; the `_inner` seam takes `keychain: Option<KeychainStore>` and tests pass `None`.
- Per-profile isolation (ZEB-586): the wipe excludes the `profiles/` subtree by name so sibling identities/profiles are never touched; also excludes `logs/` (live tracing sink).
- Typed-confirm word is the fixed literal `ERASE` (uppercase), identical on both surfaces.
- Frontend components use design tokens only (`var(--…)`) — no raw color literals (ZEB-605 style-token-guard).

---

### Task 1: Backend — `remove_dir_children_except` + `erase_all_local_data(_inner)` + registration

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (add helper + `_inner` + command near `reset_local_identity` ~1895; tests in the existing `#[cfg(test)] mod tests`)
- Modify: `src-tauri/src/lib.rs` (register the command in `tauri::generate_handler!`, next to `owner_commands::reset_local_identity`)

**Interfaces:**
- Produces: `pub(crate) fn erase_all_local_data_inner(identity_dir: &Path, app_data_dir: &Path, keychain: Option<KeychainStore>) -> Result<(), String>`; `#[tauri::command] pub async fn erase_all_local_data(state) -> Result<(), String>`.
- Consumes: `resolve_identity_dir`, `crate::resolve_app_data_dir`, `crate::stop_inner`, `run_blocking`, `prod_keychain`, `OWNER_STATE_WRITE_LOCK`, `crate::identity::with_identity_dir_write_guard` (all already used by `reset_local_identity`).

- [ ] **Step 1 — failing tests.** Add three tests to `owner_commands.rs` tests (mirror `reset_local_identity_inner_snapshots_then_wipes_and_is_idempotent` at ~2019 — `tempfile::tempdir()`, keychain `None`):
  - `erase_all_local_data_inner_wipes_identity_and_app_data`: seed `identity_dir` with `owner_state.cbor`, `master_seed.enc`, `identity.key`, and a `_reset-backup-1700/owner_state.cbor`; seed `app_data_dir` with `mail/blob`, `avatars/a.png`, `follows.json`, `content-index.json`, `profile_cards.deadbeef.cbor`, `mint/x`, `storage_records.json`, `connectivity-settings.json`. Call `erase_all_local_data_inner(identity_dir, app_data_dir, None)`. Assert **every** seeded entry above is gone (including `identity.key` and the `_reset-backup-*` dir — erase-all is wholesale, unlike reset).
  - `erase_all_local_data_inner_preserves_profiles_and_logs`: under both `identity_dir` and `app_data_dir`, seed `profiles/other/owner_state.cbor` and `logs/app.log`, plus a removable sibling `mail/blob`. Erase. Assert `profiles/other/owner_state.cbor` and `logs/app.log` **survive** and the sibling is gone. (This is the ZEB-586 + live-sink guard, exercising the default-profile "delete children except X" path.)
  - `erase_all_local_data_inner_is_a_clean_noop_on_empty_dirs`: two fresh empty tempdirs → `erase_all_local_data_inner(...) == Ok(())`; dirs still exist and empty.
- [ ] **Step 2 — run, expect fail** (`erase_all_local_data_inner` undefined): `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(erase_all_local_data)'`.
- [ ] **Step 3 — implement helper.** Add near the reset helpers:
  ```rust
  /// Remove every direct child of `dir` whose file name is not in `excluded`,
  /// recursively for subdirectories. Best-effort: a child that cannot be removed
  /// is logged and skipped so one locked entry can't abort the whole wipe
  /// (ZEB-842). A missing `dir` is a clean no-op.
  fn remove_dir_children_except(dir: &Path, excluded: &[&str]) -> Result<(), String> {
      let entries = match std::fs::read_dir(dir) {
          Ok(e) => e,
          Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
          Err(e) => return Err(format!("read dir {}: {e}", dir.display())),
      };
      for entry in entries {
          let entry = match entry {
              Ok(e) => e,
              Err(e) => {
                  tracing::warn!(dir = %dir.display(), error = %e, "erase: skipping unreadable dir entry");
                  continue;
              }
          };
          let name = entry.file_name();
          if excluded.iter().any(|x| std::ffi::OsStr::new(x) == name) {
              continue;
          }
          let path = entry.path();
          let removed = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
              std::fs::remove_dir_all(&path)
          } else {
              std::fs::remove_file(&path)
          };
          if let Err(e) = removed {
              tracing::warn!(path = %path.display(), error = %e, "erase: could not remove entry — skipping");
          }
      }
      Ok(())
  }
  ```
- [ ] **Step 4 — implement `_inner` + command.** Add:
  ```rust
  /// Names excluded from the [`erase_all_local_data`] wipe: `profiles/` keeps
  /// sibling identities/profiles isolated (ZEB-586); `logs/` is the live tracing
  /// sink the webview-only reload keeps open, so deleting it just races the
  /// appender (diagnostic, not user content).
  const ERASE_EXCLUDED: &[&str] = &["profiles", "logs"];

  /// ZEB-842 clean-slate: hard-delete the active profile's identity dir and
  /// app-data dir children (minus [`ERASE_EXCLUDED`]) and best-effort clear the
  /// keychain. No snapshot — the recovery phrase is the identity backup. Held
  /// under [`OWNER_STATE_WRITE_LOCK`] + the identity write-guard like the reset.
  pub(crate) fn erase_all_local_data_inner(
      identity_dir: &Path,
      app_data_dir: &Path,
      keychain: Option<KeychainStore>,
  ) -> Result<(), String> {
      let _guard = OWNER_STATE_WRITE_LOCK
          .lock()
          .unwrap_or_else(|p| p.into_inner());
      crate::identity::with_identity_dir_write_guard(identity_dir, || {
          remove_dir_children_except(identity_dir, ERASE_EXCLUDED)
      })?;
      remove_dir_children_except(app_data_dir, ERASE_EXCLUDED)?;
      if let Some(kc) = keychain {
          for (item, err) in kc.delete_all() {
              tracing::warn!(keychain_item = item, error = %err, "erase_all_local_data: could not clear keychain item");
          }
      }
      Ok(())
  }

  /// ZEB-842: user-confirmed clean-slate wipe (typed-confirm on the GUI). Stops
  /// the node first so no engine rewrites a cache into the gap, then wipes.
  #[tauri::command]
  pub async fn erase_all_local_data(
      state: tauri::State<'_, Mutex<crate::NodeState>>,
  ) -> Result<(), String> {
      let identity_dir = resolve_identity_dir()?;
      let app_data_dir = crate::resolve_app_data_dir()?;
      crate::stop_inner(state.inner(), None);
      run_blocking(move || erase_all_local_data_inner(&identity_dir, &app_data_dir, prod_keychain())).await
  }
  ```
  Then register in `lib.rs` `tauri::generate_handler!` — add `owner_commands::erase_all_local_data,` next to `owner_commands::reset_local_identity,`.
- [ ] **Step 5 — run, expect pass** (`-E 'test(erase_all_local_data)'`); then `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all`.
- [ ] **Step 6 — commit:** `feat(zeb-842): erase_all_local_data command — clean-slate wipe of identity + app-data`.

### Task 2: Frontend — StartupRecoveryOptions erase-all + honest recovery copy

**Files:**
- Modify: `src/lib/components/StartupRecoveryOptions.svelte`
- Test: `src/lib/components/__tests__/StartupRecoveryOptions.test.ts`

**Interfaces:**
- Consumes: injected `invoke`/`reload` (existing props); `invoke('erase_all_local_data')`.

- [ ] **Step 1 — failing tests.** Add to `StartupRecoveryOptions.test.ts` (reuse its `makeInvoke` helper):
  - erase-all: open options → click "Erase all local data" → the go button stays disabled until the text input equals `ERASE`; typing `ERASE` enables it; clicking calls `invoke('erase_all_local_data')` then `reload()`.
  - erase error: `invoke` rejects → an error renders and `reload` is NOT called.
  - copy: the recovery reset-confirm block's copy mentions that cached content (messages) stays on the device (assert on the new wording substring).
- [ ] **Step 2 — run, expect fail:** `npx vitest run src/lib/components/__tests__/StartupRecoveryOptions`.
- [ ] **Step 3 — implement.** Extend `Mode` with `'erase-confirm' | 'erasing'`; add `let eraseText = $state('')` and `let eraseError = $state<string | null>(null)`. Add an "Erase all local data" button in the `.recovery-options` panel (below the reset button, `.recovery-btn` + a danger class) → sets `mode = 'erase-confirm'`. Render an erase-confirm block: a text input `bind:value={eraseText}` (placeholder `Type ERASE to confirm`), an error `<p>` when `eraseError`, and a go button `disabled={eraseText !== 'ERASE' || mode === 'erasing'}` whose handler mirrors `doReset` but `await invoke('erase_all_local_data')` then `reload()` (on throw: set `eraseError`, `mode = 'erase-confirm'`). Update the recovery reset checkbox copy to add: cached content (messages, avatars) stays on this device after a reset — use "Erase all local data" to remove everything. Colors via `var(--…)` only.
- [ ] **Step 4 — run, expect pass;** then `npx tsc --noEmit`.
- [ ] **Step 5 — commit:** `feat(zeb-842): erase-all in startup recovery modal + honest reset copy`.

### Task 3: Frontend — IdentityPanel erase-all danger zone

**Files:**
- Modify: `src/lib/components/IdentityPanel.svelte`
- Test: `src/lib/components/__tests__/IdentityPanel.*` (extend existing)

**Interfaces:**
- Consumes: the same `invoke('erase_all_local_data')`; a page reload on success.

- [ ] **Step 1 — failing test.** In the IdentityPanel test suite (mirror how it mocks `@tauri-apps/api/core`), add: from the idle panel, clicking "Erase all local data" opens a confirm view; a text input gated on `ERASE` enables the confirm button; confirming calls `invoke('erase_all_local_data')`. Stub the reload seam and assert it fires on success; assert an `invoke` rejection surfaces an error without reload.
- [ ] **Step 2 — run, expect fail:** `npx vitest run src/lib/components/__tests__/IdentityPanel`.
- [ ] **Step 3 — implement.** Add an `erase` arm to `wizardState`: `{ kind: 'erase'; step: { phase: 'confirm' | 'erasing'; typedText: string; error?: string } }`. In the idle `.actions` block (~664), add a third danger button "Erase all local data" → `wizardState = { kind: 'erase', step: { phase: 'confirm', typedText: '' } }`. Render the `erase` view mirroring the restore `confirm` phase (~1036): a text input bound to `step.typedText` (placeholder `Type ERASE to confirm`), a Cancel (→ `resetToIdle`), and a confirm button `disabled={step.typedText !== 'ERASE' || step.phase === 'erasing'}` that calls `invoke('erase_all_local_data')`, then reloads on success (via the same reload seam the tests stub) or sets `step.error` + phase back to `confirm` on throw. Explainer copy: this removes this device's identity and all cached data for this profile; your recovery phrase still restores the identity. Tokens only.
- [ ] **Step 4 — run, expect pass;** then `npx tsc --noEmit`.
- [ ] **Step 5 — commit:** `feat(zeb-842): erase-all danger action in IdentityPanel`.

### Task 4: Full gate sweep

- [ ] **Step 1 — Rust full:** `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (green).
- [ ] **Step 2 — clippy + fmt:** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` && `cargo fmt --all -- --check`.
- [ ] **Step 3 — Frontend:** from repo root `npx tsc --noEmit && npx vitest run`.
- [ ] **Step 4 — commit** any fmt/fixes not already committed.

### Task 5: PR + converge

- [ ] **Step 1 — push branch, open PR** with `Closes ZEB-842`, design/plan links, and a summary of the two-tier model + exclusions. Fire `@coderabbitai review` ONCE.
- [ ] **Step 2 — wait CI green + bots; scan all 3 comment buckets; bundle findings → fix → verify → push once/round.** Never auto-merge.
- [ ] **Step 3 — mark merge-ready; pushover; await Jake's merge; post-merge housekeeping.**

---

## Self-Review

- **Spec coverage:** new `erase_all_local_data` command (T1), wholesale-minus-`profiles/`/`logs/` scrub + default-profile trap (T1 helper + preserve test), hard-delete no-snapshot (T1), both surfaces (T2/T3), honest recovery copy (T2), `ERASE` typed-confirm (T2/T3), locking/keychain reuse (T1), test matrix incl. per-profile isolation + no-op (T1), gates (T4). ✓
- **Placeholders:** none — backend code is concrete; frontend steps name exact state, gates, and command strings.
- **Type consistency:** `erase_all_local_data_inner(identity_dir, app_data_dir, keychain)` and the `ERASE` literal are identical across tasks; `ERASE_EXCLUDED = ["profiles","logs"]` used by both dir removals. ✓
