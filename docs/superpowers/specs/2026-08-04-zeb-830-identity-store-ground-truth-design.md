# ZEB-830 — `identity_store_backend` ground-truth reporting — Design

**Ticket:** ZEB-830 (follow-up to ZEB-768 / PR #570)
**Status:** design for review
**Scope decision (Jake):** read-path ground-truth fix + post-mint re-query.
**Validation decision (Jake):** reuse `load_secret`'s precedence (minimal new
keychain code), CI-test the file/neutral branches, and prove the keychain
branch with a real-macOS-Keychain check on Koya (whose keychain identity is a
disposable dev identity — the production identity lives on KRILE).

---

## Problem

`identity_store_backend()` (src-tauri/src/identity_commands.rs:601) reports

```rust
Ok(identity_store_backend_label(KeychainStore::new().is_ok()).to_string())
```

`KeychainStore::new().is_ok()` answers *"can a keychain handle be constructed?"*
— **availability**, not **where the owner seed actually persisted**. Two facts
make availability an overclaim:

1. **`keyring` v3 constructs `Entry` handles lazily.** The Secret-Service /
   macOS backend lookup happens at read/write, not at `Entry::new`. So on a
   keychain-less box (the exact ZEB-768 target population) `KeychainStore::new()`
   can succeed while every actual read/write fails.
2. **`owner_state::save_secret` (owner_state.rs:1085) falls through to the
   encrypted file** when `vault_save_slot` returns `Ok(false)` (no vault item)
   or `Err` (locked/unreadable) — `fell_through_to_enc`. The seed lands in the
   file even though the handle constructed.

Net: the getter can report `keychain` while the seed is in the encrypted file —
the very overclaim ZEB-768 exists to kill, narrowed to the keychain-available-
but-write-fell-through case. Compounding it, `WelcomeModal.svelte` queries the
getter only in `onMount` (line 74), **before** `svc.mint()` (line 103), so it
never observes where the seed actually landed.

## Ground truth already exists

`owner_state::load_secret` (owner_state.rs:1026) already computes the honest
answer as a side effect of loading — its keychain→file precedence for
`VaultSlot::OwnerMasterSeed`:

- `vault_load_slot(OwnerMasterSeed, legacy)` → `Ok(Some)` ⇒ **keychain**
- else `EncryptedFileStore::from_env(dir/"master_seed.enc")` loads `Some` ⇒ **encrypted-file**
- else (no fallback configured, no keychain error) ⇒ **neither** (un-minted)
- keychain read `Err` **and** no file fallback ⇒ propagate `Err` (inconclusive;
  never misclassify a locked keychain as un-minted)

The fix reports *that* location instead of handle-constructability.

## Goals

- `identity_store_backend()` reports the backend the `OwnerMasterSeed` **actually
  loaded from**, via the same precedence `load_secret` uses (single source of
  truth — the two cannot diverge).
- `WelcomeModal` re-queries after `mint()` so the backup-step copy reflects the
  real post-mint backend.
- The neutral / inconclusive case yields backend-neutral copy (already modelled
  frontend-side as `'unknown'`).

## Non-goals

- Changing `save_secret`/`load_secret` **behavior** or the keychain→file
  precedence itself (only *reading out* the location it already decides).
- Convergence-latency work — out of scope, unrelated to this ticket.
- CI coverage of the keychain branch — structurally impossible under the
  ZEB-428 isolation gate; handled by the Koya real-keychain check (below).

---

## Design

### Backend (Rust)

**1. A location tag in `owner_state.rs`:**

```rust
/// ZEB-830: which backend a persisted owner secret actually loaded from — the
/// ground truth `identity_store_backend` reports, vs. mere keychain
/// availability. Absence (the locate fn returning `Ok(None)`) means the secret
/// is in neither store (un-minted / inconclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedBackend {
    Keychain,
    EncryptedFile,
}
```

**2. Extract `load_secret`'s body into a location-returning `locate_secret`;
`load_secret` becomes a thin wrapper (signature unchanged → zero caller churn):**

```rust
/// The single implementation of the keychain→file precedence. Returns the
/// backend the secret loaded from alongside the bytes; `load_secret` and the
/// ZEB-830 probe both derive from this so they can never diverge.
fn locate_secret(
    use_os_keychain: bool,
    slot: VaultSlot,
    keychain_name: &str,
    identity_dir: &Path,
    fallback_filename: &str,
) -> Result<Option<(SeedBackend, Zeroizing<[u8; 32]>)>, String> {
    // ── verbatim current load_secret body, tagging each success arm ──
    //   keychain vault hit  → Ok(Some((SeedBackend::Keychain,      key)))
    //   encrypted-file hit  → Ok(Some((SeedBackend::EncryptedFile, seed)))
    //   neither             → Ok(None)
    //   keychain Err + no file fallback → Err(e)   (semantics preserved)
}

fn load_secret(/* unchanged signature */) -> Result<Option<Zeroizing<[u8; 32]>>, String> {
    Ok(locate_secret(use_os_keychain, slot, keychain_name, identity_dir, fallback_filename)?
        .map(|(_, bytes)| bytes))
}
```

The error-preservation branch (`keychain_err` propagated when no fallback is
configured) is retained *inside* `locate_secret`, so both callers inherit the
exact semantics the current `load_secret` tests already pin.

**3. A public probe for `OwnerMasterSeed`:**

```rust
/// ZEB-830: report which backend the persisted OwnerMasterSeed actually loaded
/// from, mirroring `load_secret`'s precedence via the shared `locate_secret`.
/// `Ok(None)` = seed in neither store (un-minted / inconclusive). The bytes are
/// read and immediately dropped (Zeroizing) — only the location is returned.
pub fn owner_master_seed_backend(
    use_os_keychain: bool,
    identity_dir: &Path,
) -> Result<Option<SeedBackend>, String> {
    Ok(locate_secret(
        use_os_keychain,
        VaultSlot::OwnerMasterSeed,
        KEYCHAIN_MASTER_SEED,     // "master_seed" (owner_state.rs:458)
        identity_dir,
        "master_seed.enc",
    )?
    .map(|(backend, _bytes)| backend))
}
```

**4. Rewrite the getter + refactor its label helper to a pure `Option<SeedBackend>`
→ `&'static str` (CI-testable, all three values):**

```rust
fn identity_store_backend_label(backend: Option<owner_state::SeedBackend>) -> &'static str {
    match backend {
        Some(owner_state::SeedBackend::Keychain)      => "keychain",
        Some(owner_state::SeedBackend::EncryptedFile) => "encrypted-file",
        None                                          => "unknown",
    }
}

#[tauri::command]
pub async fn identity_store_backend() -> Result<String, String> {
    // The owner secrets live in the identity DIRECTORY (parent of identity.key);
    // `resolve_path(None)` returns the identity.key FILE path, so use the
    // directory resolver — the probe joins `master_seed.enc` onto it. A failed
    // dir resolution is as inconclusive as a probe failure → "unknown".
    let Ok(identity_dir) = crate::owner_commands::resolve_identity_dir() else {
        return Ok("unknown".to_string());
    };
    run_blocking(move || {
        // ZEB-189 availability gate — the same decision mint makes for
        // `use_os_keychain` (callers pass `KeychainStore::new().ok()`;
        // `use_os_keychain = keychain.is_some()`, owner_state.rs:532/624).
        let use_os_keychain = KeychainStore::new().is_ok();
        // Inconclusive (locked keychain / unreadable file) → neutral, never an
        // overclaim; log for debuggability rather than surfacing a scary IPC error.
        let backend = owner_state::owner_master_seed_backend(use_os_keychain, &identity_dir)
            .unwrap_or_else(|e| {
                tracing::debug!("identity_store_backend probe inconclusive: {e}");
                None
            });
        Ok(identity_store_backend_label(backend).to_string())
    })
    .await
}
```

The getter now **never returns `Err`** — both an inconclusive probe and a failed
identity-directory resolution collapse to `"unknown"`, which
`normalizeIdentityStoreBackend` already renders as backend-neutral copy. (It must
resolve the identity **directory**, not the identity.key file path, or the
encrypted-file probe searches the wrong location — Qodo, PR #606.)

### Frontend (Svelte)

`onMount`'s pre-mint query stays (drives the explain-pane copy). Add a re-query
immediately after a successful `mint()` in `handleCreateIdentity`:

```js
const result = await svc.mint();
mintResult = result;
// ZEB-830: re-query post-mint — onMount ran before mint, and mint can fall
// through to the encrypted file even when the keychain handle constructed, so
// only now do we know where the seed ACTUALLY landed.
try {
  identityBackend = normalizeIdentityStoreBackend(
    await invoke<string>('identity_store_backend'),
  );
} catch (e) {
  console.debug('[zeb-830] post-mint identity_store_backend failed:', extractError(e));
}
stage = 'backup';
```

No type or copy change — `IdentityStoreBackend` and `identityKeyBackupNote`
already cover `'keychain' | 'encrypted-file' | 'unknown'`.

---

## Validation strategy

### CI-exercisable (committed tests)

The ZEB-428 gate makes `KeychainStore::new()` refuse in every test build, so CI
reaches only the file / neutral branches — which is exactly what these cover:

- **`owner_master_seed_backend` file branch:** `use_os_keychain=false`,
  `HARMONY_PASSPHRASE` set, a seed written to `master_seed.enc` in a tempdir ⇒
  `Some(EncryptedFile)`.
- **neutral branch:** nothing written anywhere ⇒ `Ok(None)`.
- **`load_secret` regression:** the existing load tests continue to pass,
  proving the `locate_secret` extraction preserved bytes + error semantics.
- **`identity_store_backend_label` string contract:** `Some(Keychain)` →
  `"keychain"`, `Some(EncryptedFile)` → `"encrypted-file"`, `None` → `"unknown"`
  (replaces the old bool-based contract test).
- **Frontend:** a `WelcomeModal` test asserting the backend is re-queried after
  mint (mock `invoke` returns a different value post-mint; assert the note
  reflects it), plus the existing normalize/note unit coverage.

### Keychain branch — Koya real-Keychain check (not CI)

The keychain branch is validated two ways:

1. **By construction** — `owner_master_seed_backend` and `load_secret` share
   `locate_secret`, so the probe's keychain arm *is* the same
   `vault_load_slot(OwnerMasterSeed)` read every app launch already performs and
   that `load_secret`'s production path exercises. No new keychain code path is
   introduced.
2. **Empirically, on Koya** — a manual run against the real macOS Keychain,
   captured in the PR. Koya's `harmony/identity` vault holds a **disposable dev
   identity** (confirmed present; production is on KRILE), so the check may
   freely write and clear the real `OwnerMasterSeed` slot:
   - write a known seed to the keychain vault slot (`use_os_keychain=true`) →
     `owner_master_seed_backend(true, dir)` ⇒ `Some(Keychain)`;
   - clear the slot, write the seed to `master_seed.enc` only ⇒
     `Some(EncryptedFile)`;
   - clear both ⇒ `Ok(None)`.

   Run via a **throwaway** `#[ignore]`d test gated behind
   `HARMONY_ALLOW_REAL_KEYCHAIN=1` (ZEB-428's sanctioned escape), executed once
   on Koya with output pasted into the PR. It is **not committed**: a test that
   writes the process-global `OwnerMasterSeed` slot must never enter the suite,
   or a contributor who sets that env for an unrelated real-keychain test could
   lose their own identity (the exact ZEB-428 class). CI's committed coverage
   stays file/neutral/contract only.

---

## Risks & edge cases

- **Extraction fidelity.** `locate_secret` must reproduce `load_secret`'s
  `keychain_err` propagation branch exactly (locked keychain + no fallback ⇒
  `Err`, not `Ok(None)`), or a locked keychain would misreport as un-minted.
  Mitigated by keeping `load_secret` a thin wrapper over the shared body and
  relying on its existing tests as the regression net.
- **Secret handling.** The probe loads the seed to learn its location, then
  drops it; the bytes are `Zeroizing` (zeroized on drop) and never logged. No
  new exposure surface.
- **First post-mint keychain read may prompt (macOS).** `owner_master_seed_backend`
  performs a real keychain read, unlike the old `is_ok()` check. Pre-mint
  (onMount) there is usually no owner vault yet → `load_vault` → `None` → no
  prompt. Post-mint the vault exists, but mint wrote it in the same session, so
  access is already granted. Bounded to at most two onboarding calls.
- **`use_os_keychain` mismatch.** The getter derives it from
  `KeychainStore::new().is_ok()`, the same gate mint uses, so the probe looks
  where mint would have written. If the keychain is unavailable, the probe skips
  it and reports the file/neutral result — consistent with where the seed is.

---

## Files

- `src-tauri/src/owner_state.rs` — `SeedBackend`, `locate_secret`,
  `load_secret` wrapper, `owner_master_seed_backend`; file/neutral + regression tests.
- `src-tauri/src/identity_commands.rs` — getter rewrite, label-helper refactor,
  string-contract test.
- `src/lib/components/WelcomeModal.svelte` — post-mint re-query.
- `src/lib/components/__tests__/WelcomeModal.*` — re-query test.
- Koya keychain check — throwaway `#[ignore]`d test, run on Koya, **not committed**.
