# ZEB-674 — Per-viewer encrypted-file sharing: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a user encrypt a personal file at ingest, share read-access with specific friends by sealing the file's key to each of their devices, deliver those grants confidentially over the butler deposit transport, let grantees decrypt, and render an honest owner-side "Shared with" list — with lazy revoke.

**Architecture:** Client-only (`harmony-client`). A fresh per-file symmetric key (a reused `EpochKey`, "DEK") encrypts the whole blob before chunking (`encrypt_blob`), producing an `EncryptedDurable` CID. The DEK persists KeyTree-sealed on `OwnerState`, replicating to the owner's own devices. Sharing seals the DEK per grantee-device via `dm_signing::seal_to_owner_with_info` and delivers it inside a `DepositPayload.grant_push` field, butler-opaque. Grantees unwrap, fetch the (allowlisted) CID, and decrypt. Revoke is lazy (drop the owner-local grant record).

**Tech Stack:** Rust (`src-tauri`, tokio, serde/CBOR), Svelte 5 + TypeScript (`src`), Tauri IPC, `cargo nextest` / `vitest`.

## Global Constraints

- **Gates (CI parity, run from `src-tauri/`):** `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend (repo root): `npx tsc --noEmit`; `npx vitest run`. Iterative dev may use `scripts/test-select --context task`; **final pre-PR sweep uses the full commands.**
- **Tauri IPC naming:** Rust params `snake_case`; JS callers `camelCase` (auto-converted at the boundary). Wrong case → `undefined`.
- **Tauri IPC error extraction (JS):** `const msg = e instanceof Error ? e.message : String(e)`.
- **Keychain isolation in tests:** never construct `KeychainStore::new()` in test-reachable code; inject `keychain: None` via the `*_inner` seams; set `HARMONY_PASSPHRASE` in tests touching identity persistence.
- **`--locked` and `--all-targets` are load-bearing** on every gate command.
- **Seal domain separation:** all grant seals use a fresh `FILE_GRANT_SEAL_INFO = b"harmony-file-grant-v1"` — never reuse a DM/epoch info string.
- **Honesty rule (ZEB-610 §0):** the Share surface renders only backend-proven data; it is shown only on encrypted files and self-hides until `list_grants` resolves.
- One commit per task (TDD: test → run-fail → implement → run-pass → commit). Do **not** auto-merge the PR.

## File Structure

**Backend (`src-tauri/src`)**
- `owner_state_crdt.rs` — `OwnerState` struct (`:23`); add three fields: `file_deks`, `file_grants`, `received_file_grants`.
- `owner_state_types.rs` — new record types `GrantEntry`, `ReceivedFileGrant`; reuse `EpochKey` (`:479`), `OwnerDeviceCache.devices` (`:541`), `OwnerDeviceEntry.device_identity_pubs` (`:662`).
- `file_sharing.rs` **(new)** — the sharing logic: `FILE_GRANT_SEAL_INFO`, `FileGrantInner`, DEK generate/seal-at-rest helpers, per-device seal fan-out, `grant_push` build/parse, share/revoke orchestration. Keeps `lib.rs` from growing.
- `butler_deposit.rs` — add `grant_push: Option<Vec<u8>>` to `DepositPayload` (`:201`); inbound demux routes it.
- `lib.rs` — encrypt-on-ingest command; IPC commands `list_grants` / `grant_read` / `revoke_read`; register in `generate_handler!` (`:63197`); grantee inbound grant handler + open path.

**Frontend (`src`)**
- `lib/types.ts` — `FileGrant` DTO + encrypted-ingest option type.
- `lib/file-manager-service.ts` — `listGrants` / `grantRead` / `revokeRead`; `encrypted` flag on ingest.
- `lib/components/ShareList.svelte` **(restore from `cd2ed6c8^`, re-typed)**.
- `lib/components/FileDetailPanel.svelte` — mount ShareList (between `:199` backup section and `:207` `FileActions`), gated on encrypted.
- `App.svelte` — wire `grants`/`onGrant`/`onRevoke` into `<FileDetailPanel>` (`:3996`).

**Reused primitives (do not reimplement):** `encrypt_blob(&EpochKey, &[u8])` / `decrypt_blob(&EpochKey, &[u8])` (`community_state_sync.rs:163/189`); `dm_signing::seal_to_owner_with_info(&[u8;32], &[u8], &[u8])` / `open_from_owner_with_info` (`dm_signing.rs:79/128`); `streaming_ingest_with_options` + `IngestOptions` (`lib.rs:539/530`, encrypted+serveable path already tested at `lib.rs:64973`); `CommunityServeAllowlist::allow` / `ContentStore::allow_serve_subtree` (`content_store.rs:42/92`); `owner_id_from_master_ed25519` (`friend_graph.rs:57`); butler deposit `build_deposit_frame` / `IrohButlerDepositClient` (`butler_deposit.rs:463/544`); KeyTree self-seal idiom `owner_state_crypto::encrypt_friend_secret` (`lib.rs:55317`) for at-rest DEK sealing.

---

### Task 1: Per-file DEK + encrypt-on-ingest + sealed DEK store (C1)

**Files:** Create `src-tauri/src/file_sharing.rs`; Modify `owner_state_crdt.rs` (add `file_deks`), `lib.rs` (encrypt-ingest command + module decl). Test: `src-tauri/tests/file_sharing_dek.rs` + inline `#[cfg(test)]`.

**Interfaces — Produces:**
- `OwnerState.file_deks: BTreeMap<ContentId, Vec<u8>>` — value = KeyTree-sealed EpochKey bytes (sealed-to-self via the `encrypt_friend_secret` idiom).
- `file_sharing::generate_file_dek() -> EpochKey` (fresh 32 random bytes; mirror how `EpochKey` is constructed at its def).
- `file_sharing::seal_dek_at_rest(tree, &EpochKey) -> Vec<u8>` / `open_dek_at_rest(tree, &[u8]) -> Result<EpochKey, _>`.
- IPC `ingest_content_encrypted(path or bytes, name, mime) -> ContentDetailDto` (mirrors `ingest_content` but sets encryption): fresh DEK → `encrypt_blob(&dek, plaintext)` → `streaming_ingest_with_options(Cursor::new(ciphertext), tx, ChunkerConfig::DEFAULT, IngestOptions{ flags: ContentFlags{ encrypted: true, .. }, serveable: true })` → store `file_deks[root_cid] = seal_dek_at_rest(dek)`.

- [ ] **Step 1 — failing test** `encrypted_ingest_dek_round_trip` (`tests/file_sharing_dek.rs`): mint an owner (home-override + `HARMONY_PASSPHRASE`, `keychain: None`); ingest a known plaintext via the encrypted path; assert the returned CID has `flags().encrypted == true`; retrieve+unseal the DEK from `file_deks`; fetch the stored ciphertext for the CID and `decrypt_blob(&dek, ct)`; assert it equals the original plaintext.
- [ ] **Step 2 — run, expect FAIL** (`cargo nextest run --locked --features test-fixtures -E 'test(encrypted_ingest_dek_round_trip)'`): command/field absent.
- [ ] **Step 3 — implement:** add the `file_deks` field (serde-default, camelCase rename consistent with sibling fields; include in the CRDT/persistence path exactly like `spaces`); add `file_sharing.rs` with `generate_file_dek` / `seal_dek_at_rest` / `open_dek_at_rest`; add the `ingest_content_encrypted` command; `mod file_sharing;` in `lib.rs`.
- [ ] **Step 4 — run, expect PASS.** Add `sealed_dek_at_rest_is_not_plaintext` (assert the stored `file_deks` value ≠ raw DEK bytes) and `file_deks_persist_reload` (save `OwnerState`, reload, DEK still unseals).
- [ ] **Step 5 — gate + commit** (`fmt` + `clippy` + the three tests): `ZEB-674: per-file DEK encrypt-on-ingest + sealed DEK store`.

### Task 2: Grant records + per-device seal fan-out (C2)

**Files:** Modify `owner_state_crdt.rs` (add `file_grants`), `owner_state_types.rs` (add `GrantEntry`), `file_sharing.rs` (fan-out). Test: inline + `tests/file_sharing_grants.rs`.

**Interfaces — Produces:**
- `GrantEntry { grantee_owner: OwnerAddr, granted_at: u64 }` (serde camelCase); `OwnerState.file_grants: BTreeMap<ContentId, Vec<GrantEntry>>`.
- `file_sharing::grantee_device_x25519s(state, grantee_owner) -> Vec<[u8;32]>` — `owner_device_cache.devices[grantee_owner].device_identity_pubs` → for each `Some(p)`, `p[0..32]`.
- `file_sharing::FileGrantInner { cid: [u8;32], file_name: String, file_size: u64, mime: String, dek: [u8;32] }` (CBOR via the repo's canonical encode).
- `file_sharing::seal_grant_for_devices(inner: &FileGrantInner, devices: &[[u8;32]]) -> Result<Vec<Vec<u8>>, _>` — one `seal_to_owner_with_info(dev, cbor(inner), FILE_GRANT_SEAL_INFO)` per device.

- [ ] **Step 1 — failing test** `seal_fanout_one_per_known_device` (`tests/file_sharing_grants.rs`): build a `FileGrantInner`; supply 2 device X25519 pubs (derive from 2 test X25519 keypairs); `seal_grant_for_devices` → assert `len == 2`; open blob[0] with device-0 priv via `open_from_owner_with_info(.., FILE_GRANT_SEAL_INFO)` → parse → equals inner; assert opening blob[0] with device-1 priv fails (`DecryptionFailed`).
- [ ] **Step 2 — run, expect FAIL.**
- [ ] **Step 3 — implement:** `GrantEntry`, `file_grants` field (persisted like `file_deks`), `grantee_device_x25519s`, `FileGrantInner`, `seal_grant_for_devices`, and `FILE_GRANT_SEAL_INFO`.
- [ ] **Step 4 — run, expect PASS.** Add `grant_record_append_remove` (append two `GrantEntry`, remove one, assert set) and `grant_records_persist_reload`.
- [ ] **Step 5 — gate + commit:** `ZEB-674: grant records + per-device seal fan-out`.

### Task 3: Deposit distribution — `grant_push` field (C3)

**Files:** Modify `butler_deposit.rs` (`DepositPayload` + inbound demux). Test: inline `#[cfg(test)]` in `butler_deposit.rs`.

**Interfaces — Produces:**
- `DepositPayload.grant_push: Option<Vec<u8>>` (rename `"gp"`, `default, skip_serializing_if = "Option::is_none", with = "serde_bytes"` — exactly mirror `invite_packet`/`revocation_push` at `:218-239`). Value = canonical CBOR of `Vec<serde_bytes Vec<u8>>` (the per-device sealed grant blobs from Task 2).

- [ ] **Step 1 — failing test** `deposit_payload_grant_push_roundtrip_and_back_compat`: (a) a payload with `grant_push = Some(bytes)` `encode_deposit_payload`→`decode_deposit_payload` round-trips; (b) an old-style payload encoded WITHOUT the field still decodes (`grant_push == None`) — build the legacy bytes by encoding a struct value with `grant_push: None` and assert the key `"gp"` is absent from the CBOR.
- [ ] **Step 2 — run, expect FAIL** (field absent).
- [ ] **Step 3 — implement:** add the field; extend the inbound deposit-payload demux so a present `grant_push` routes to the grant handler (Task 4) while `None` is a no-op (do not break `cidnotify`/`invite`/`revocation` handling).
- [ ] **Step 4 — run, expect PASS.** Add `butler_cannot_open_grant_push` (the per-device seals inside `grant_push` are opaque without the grantee device X25519 priv — decode the Vec, attempt `open_from_owner_with_info` with an unrelated key, assert failure).
- [ ] **Step 5 — gate + commit:** `ZEB-674: carry file grants in DepositPayload.grant_push`.

### Task 4: Grantee receive + read path (C4)

**Files:** Modify `owner_state_crdt.rs` (`received_file_grants`), `owner_state_types.rs` (`ReceivedFileGrant`), `lib.rs`/`file_sharing.rs` (inbound handler + open). Test: `tests/file_sharing_grantee.rs`.

**Interfaces — Produces:**
- `ReceivedFileGrant { granter_owner: OwnerAddr, cid: [u8;32], file_name: String, file_size: u64, mime: String, sealed_dek: Vec<u8>, received_at: u64 }`; `OwnerState.received_file_grants: BTreeMap<ContentId, ReceivedFileGrant>`.
- `file_sharing::ingest_grant_push(state, my_device_x25519_priv, grant_push_bytes) -> Result<Option<ContentId>, _>` — decode `Vec<Vec<u8>>`; for each blob try `open_from_owner_with_info(my_priv, blob, FILE_GRANT_SEAL_INFO)`; on first success parse `FileGrantInner` → insert `received_file_grants[cid]` (store the *matched sealed blob* as `sealed_dek`) → return `Some(cid)`; if none open, `Ok(None)`.
- `file_sharing::open_received_file(state, my_device_x25519_priv, cid) -> Result<EpochKey, _>` — unseal the grantee's DEK from `received_file_grants[cid].sealed_dek`.

- [ ] **Step 1 — failing test** `grantee_ingest_then_decrypt` (`tests/file_sharing_grantee.rs`): build a `FileGrantInner{cid, .., dek}`; seal to a grantee device key → wrap as `Vec<Vec<u8>>` grant_push; `ingest_grant_push` → returns `Some(cid)` and populates `received_file_grants`; `open_received_file` → DEK; `decrypt_blob(&dek, ciphertext_of_cid)` equals the original plaintext (encrypt the plaintext under the same DEK in the test to produce the ciphertext).
- [ ] **Step 2 — run, expect FAIL.**
- [ ] **Step 3 — implement** the two functions + fields; wire `ingest_grant_push` into the Task 3 inbound demux.
- [ ] **Step 4 — run, expect PASS.** Add `grantee_ingest_no_matching_device_is_none` (grant sealed only to a device we don't hold → `Ok(None)`, no state change).
- [ ] **Step 5 — gate + commit:** `ZEB-674: grantee receive + decrypt path`.

### Task 5: IPC — `grant_read` / `revoke_read` / `list_grants` + share orchestration + allowlist (C2/C4/revoke)

**Files:** Modify `lib.rs` (3 `#[tauri::command]` + register at `:63197`), `file_sharing.rs` (orchestration). Test: inline command-level tests.

**Interfaces — Produces (IPC; JS camelCase args):**
- `grant_read(cid: String, grantee_address: String) -> Result<(), String>`: reject if `file_deks` lacks `cid` (`"ineligible: only encrypted files can be shared"`) or grantee is not a friend (`"ineligible: can only share with friends"`); unseal owner DEK; resolve grantee devices; `seal_grant_for_devices`; `allow_serve_subtree(cid)`; append `GrantEntry`; build `grant_push` and deliver via the butler deposit client the DM outbox uses (`build_deposit_frame` + `IrohButlerDepositClient`; mirror the DM deposit enqueue — see `dm_outbox.rs:918` wiring); persist; `emit_ser(sink, "grants-updated", {cid})`.
- `revoke_read(cid: String, grantee_address: String) -> Result<(), String>`: **lazy** — remove the matching `GrantEntry` from `file_grants[cid]`; persist; emit. Do NOT touch the DEK or the allowlist (documented: cannot withdraw already-granted access).
- `list_grants(cid: String) -> Result<Vec<FileGrantDto>, String>`: map `file_grants[cid]` → `FileGrantDto{ granteeAddress, displayName, grantedAt }` (resolve `displayName` from contacts; omit if unknown).

- [ ] **Step 1 — failing tests** (inline): `grant_read_rejects_public_cid` (public CID → `ineligible:` prefix); `grant_read_rejects_non_friend`; `grant_then_list_reflects_grantee` (grant to a friend fixture → `list_grants` contains them); `revoke_read_drops_record_lazily` (after revoke, `list_grants` omits them AND `file_deks[cid]` unchanged AND allowlist still contains cid).
- [ ] **Step 2 — run, expect FAIL.**
- [ ] **Step 3 — implement** the three commands + orchestration; register in `generate_handler!`. For deposit delivery, reuse the existing butler-deposit client seam; if a live client is unavailable in a unit test, gate the network send behind the same seam the DM outbox uses so tests exercise seal+record+allowlist without a live transport (assert the built `grant_push` decodes + opens for the fixture grantee device).
- [ ] **Step 4 — run, expect PASS.** Add `grant_read_emits_grants_updated`.
- [ ] **Step 5 — gate + commit:** `ZEB-674: grant/revoke/list IPC + share orchestration`.

### Task 6: Frontend types + service (C5 backend surface)

**Files:** Modify `src/lib/types.ts`, `src/lib/file-manager-service.ts`. Test: `src/lib/file-manager-service.test.ts` (vitest, mock adapter).

**Interfaces — Produces:**
- `types.ts`: `export interface FileGrant { granteeAddress: string; displayName: string | null; grantedAt: number }`; add `encrypted?: boolean` to the ingest options type.
- `file-manager-service.ts`: `listGrants(cid): Promise<FileGrant[]>` → `adapter.invoke('list_grants', { cid })`; `grantRead(cid, granteeAddress): Promise<void>`; `revokeRead(cid, granteeAddress): Promise<void>` (both with the `ineligible:`-prefix-stripping error idiom); route the `encrypted` flag to `ingest_content_encrypted`.

- [ ] **Step 1 — failing test** (`file-manager-service.test.ts`): mock adapter asserts `grantRead('cidX','addrY')` invokes `'grant_read'` with `{ cid:'cidX', granteeAddress:'addrY' }` (camelCase); `listGrants` maps the DTO; a rejection `"ineligible: ..."` surfaces with the prefix stripped.
- [ ] **Step 2 — run, expect FAIL** (`npx vitest run file-manager-service`).
- [ ] **Step 3 — implement** the methods + types.
- [ ] **Step 4 — run, expect PASS** + `npx tsc --noEmit` clean.
- [ ] **Step 5 — commit:** `ZEB-674: frontend grant service + types`.

### Task 7: Frontend ShareList restore + panel wiring (C5 UI)

**Files:** Create `src/lib/components/ShareList.svelte` (recover base from `git show cd2ed6c8^:src/lib/components/ShareList.svelte`, re-typed); Modify `FileDetailPanel.svelte`, `App.svelte`. Test: `src/lib/components/ShareList.test.ts`.

**Interfaces — Consumes:** `FileGrant[]` + friend contacts (reuse the `friendContacts` map already threaded to `StorageBuddySheet`). **Props:** `{ grants: FileGrant[], availableFriends: {address,displayName}[], isEncrypted: boolean, onGrant(address), onRevoke(address) }`.

- [ ] **Step 1 — failing test** (`ShareList.test.ts`): renders "Not shared with anyone" only when `grants=[]` AND resolved (not a pre-query placeholder); renders one row per grant with a Revoke control that calls `onRevoke`; the "Share with…" picker excludes already-granted friends and calls `onGrant`.
- [ ] **Step 2 — run, expect FAIL.**
- [ ] **Step 3 — implement:** restore + re-type ShareList onto `FileGrant`; mount in `FileDetailPanel.svelte` as a new `panel-section` between `:199` and `:207`, gated `{#if isEncrypted}`; add props to the `$props()` block (`:12-41`); wire `App.svelte` (`:3996`) to fetch grants via `fileManagerService.listGrants(cid)` and pass `onGrant`/`onRevoke` → `grantRead`/`revokeRead`, refreshing on the `grants-updated` event. Public files render no ShareList (honest).
- [ ] **Step 4 — run, expect PASS** + `npx tsc --noEmit` + `npx vitest run`.
- [ ] **Step 5 — commit:** `ZEB-674: restore honest ShareList + panel wiring`.

### Task 8: e2e-harness two-node scenario (stretch, `--features e2e`)

**Files:** `e2e-harness/src/driver.rs` (+ share/read verbs), `e2e-harness/tests/`. Only if the deposit path is reachable headless; otherwise file a follow-up and skip (log the deferral, do not silently drop).

- [ ] Owner ingests an encrypted file, `grant_read` to the grantee node, grantee fetches + decrypts, assert plaintext matches; owner `revoke_read` removes it from `list_grants`. Guard on a short/injectable path (no wall-clock sleep).

### Task 9: Full gates + PR

- [ ] `cargo fmt --all -- --check` clean; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean; `cargo nextest run --locked --workspace --all-targets --features test-fixtures` green; `npx tsc --noEmit` + `npx vitest run` green.
- [ ] Push branch `zeb-674-encrypted-file-sharing`; open PR (base `main`); body links ZEB-674 with `Closes ZEB-674`; trigger CodeRabbit once; converge all three comment buckets; **do not auto-merge**.
