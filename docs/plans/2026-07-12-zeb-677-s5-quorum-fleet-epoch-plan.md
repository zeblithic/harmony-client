# ZEB-677 S5 — Quorum-signed fleet epoch bump (full crypto cutoff) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans (inline) — this session drives it. Steps use `- [ ]` checkboxes.

**Goal:** Let a master-less fleet rotate its fleet KeyTree via a K=2 quorum co-sign — both bundled into the quorum revocation ceremony (one co-sign → revoke + crypto cutoff) and as a standalone manual rotation — so a revoked device loses fleet-data read access without the master seed. Retires the last honesty-ledger caveat ("Quorum revoke = instant crypto cutoff").

**Architecture:** The `fleet-keys-v1` carrier doc (`FleetKeyEpochDoc`) is master-signed today. S5 adds an **additive, self-certifying quorum signature**: the doc gains `quorum_sig: Option<QuorumDocSig>` + an embedded `signer_certs: Vec<EnrollmentCert>` bundle (depth-1 Master-issued), so the **synchronous** carrier merger verifies a quorum doc self-containedly (no trust-doc lock — the merger `Fn` can't `.await` the async trust `Mutex`), mirroring how the master path checks only authenticity+identity. A master-less bump **generates a fresh random KeyTree** (`KeyTree::generate_at_epoch`) — fleet keys are symmetric AEAD material distributed via sealed blobs, never required to be master-derived (existing `from_fleet_material` path). The ceremony carries the pre-built unsigned epoch doc inside the quorum request; B's single co-sign produces a second detached signature over the epoch-doc hash; A assembles the quorum-signed carrier and installs it under the existing monotonic, no-rollback rule.

**Tech Stack:** Rust (tauri backend), `harmony-owner` crate rev `1ecb4160` (quorum cert API present), `ciborium`/canonical-CBOR wire, `FleetSyncEngine`, Svelte 5 + vitest frontend.

## Global Constraints

- **harmony-owner rev** `1ecb4160ee62f19da23158e246e856d449159f93` (`src-tauri/Cargo.toml:109`). Do not bump.
- **Additive wire only.** Every new carrier/request field is `#[serde(default, skip_serializing_if = ...)]`; a master doc must serialize **byte-identical** to today (the golden fixture `wire_encoding_is_pinned_and_default_omits_empty_fields` at `fleet_key_epoch.rs` must pass unchanged). Old builds ignore new fields and reject quorum docs (empty `master_sig` → `verify()` false) — the honest §8 interop behavior.
- **Depth-1.** Quorum signers must hold **Master-issued** enrollment certs (`EnrollmentIssuer::Master`). No quorum-signs-quorum.
- **K=2 fixed.** ≥2 distinct signers, initiator included.
- **No-rollback rule.** A failed epoch install/flush leaves the revoke standing; `fleetEpochStale` banner is the retry surface. Mirror `revoke_device_inner`'s master-path bump (`owner_commands.rs` ~870–932): monotonic under-lock re-check (`if new_doc.epoch <= doc.epoch { return Err }`), warn-only on flush failure, never roll back the revoke.
- **`signing_bytes()` unchanged.** Both master and quorum signatures cover `{epoch, bump_wall_ms, sealed, master_pubkey}` (master_pubkey = `None` for quorum docs). `quorum_sig` and `signer_certs` are the signature envelope — never part of what is signed.
- **Gates (CLAUDE.md):** per-task `scripts/test-select --context task` (when used, paste its emitted `round=… bucket=…` summary line into the task report for traceability); fmt `cargo fmt --all -- --check`; clippy `--locked --all-targets --features test-fixtures --no-deps -- -D warnings`; final full sweep `cargo nextest run --locked --workspace --all-targets --features test-fixtures` + `npx tsc --noEmit` + `npx vitest run`. Cargo from `src-tauri/`. (This slice's gates were run via targeted `nextest -E` filters + full clippy `--all-targets` + full vitest — no `test-select` rounds, so no summary lines to record.)
- **Branch:** `zeb-677-s5-quorum-fleet-epoch` off latest `origin/main`. Commit trailers:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
  `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`

## Key existing seams (surveyed 2026-07-12, main `ec6debe7`)

- `src-tauri/src/fleet_key_epoch.rs`: `FleetKeyEpochDoc` (struct 56–78: fields `epoch"e"`, `bump_wall_ms"b"`, `sealed"s"`, `master_pubkey"k"`, `master_sig"g"`), `signing_bytes` (86–105), `sign` (109–119), `verify(&self, owner_id) -> bool` (124–143), `merge_fleet_keys_remote(local, remote, owner_id) -> bool` (162–179), `load_doc_or_recover(path, owner_id)` (~213), `unseal_own_material` (~346), golden fixture (~611–644), `FLEET_KEYS_SIG_DOMAIN` (51), `FLEET_EPOCH_SEAL_INFO` (49).
- `src-tauri/src/owner_state_crypto.rs`: `KeyTree` (128–220): `derive_at_epoch(seed, epoch)` (150), `to_fleet_material` (190), `from_fleet_material(&FleetKeyMaterial)` (210); `FleetKeyMaterial` (308–317, private key fields, `pub epoch`); `FleetKeySet` (233–296): `newest`, `install`, `accept_set`.
- `src-tauri/src/owner_commands.rs`: `plan_fleet_epoch_bump(trust, carrier, current_data_epoch, master_seed, now_ms) -> (FleetKeyEpochDoc, KeyTree)` (191–261; sealing loop 227–250 via `dm_signing::seal_to_owner_with_info`, `FLEET_EPOCH_SEAL_INFO`); `revoke_device_inner` (726–950; master-path bump block ~870–932); `merge_fleet_keys_remote` callsite (2249); `build_owner_state_view` (`fleet_epoch_stale` 441–445, assigned 508).
- `src-tauri/src/owner_quorum_sync.rs`: `QuorumRequestKind` (104–132: `Revocation { reason"e", target_hex"t" }` / `Enrollment {..}`), `QuorumRequestSigs` (134–146: `epoch_doc_sig_hex"e": Option<String>`, `primary_sig_hex"p": String`), `QuorumRequest` (148–183: `initiator_sigs"p"`, `signatures"s": BTreeMap<hex, QuorumRequestSigs>`), `revocation_pair_payload` (466–478), `try_assemble(trust, dsk, self_id, req) -> Option<RevocationCert>` (710–786), `run_quorum_sweep` (1149–1335), `MAX_QUORUM_SIG_ENTRIES = 16` (90).
- `src-tauri/src/owner_quorum_commands.rs`: `plan_quorum_revocation_request` (72–166), `cosign_request_core(doc, trust, dsk, self_id, request_id, now_ms) -> Result<bool, String>` (172–284; inserts `QuorumRequestSigs { epoch_doc_sig_hex: None, primary_sig_hex }` at 275–282).
- `src-tauri/src/lib.rs`: carrier engine boot (5810–5869; sync `merger` closure 5836–5848 calls `merge_fleet_keys_remote(local, remote, &fleet_keys_owner_id)`; `load_doc_or_recover` 5815), adoption task (5886–5996; `unseal_own_material` → `from_fleet_material` → `keys.install`), `bump_fleet_epoch` command (56660–56668), `bump_fleet_epoch_inner(state, keychain, sink)` (56571–56658; `notMaster:` at 56615), RPC (`api/rpc.rs:1032`).
- `src-tauri/src/owner_state.rs`: `OwnerStateView.fleet_epoch_stale` (24–28), `self_is_master`/`selfIsMaster` (from S3).
- Frontend: `src/lib/fleet-epoch-service.ts:13` (`bumpFleetEpoch`), `src/lib/owner-service.ts` (view type), `src/lib/components/DevicesPanel.svelte` (fleet-epoch-banner 892–919; seed branch `rotateFleetKeys` 226–, non-seed copy 914–917).
- Crate `harmony-owner`: `EnrollmentCert { owner_id, device_id, device_pubkeys: PubKeyBundle, issued_at, expires_at, issuer: EnrollmentIssuer }`, `verify(now_secs) -> Result<(), OwnerError>`; `EnrollmentIssuer::Master { master_pubkey }`; enrolled ed25519 = `cert.device_pubkeys.classical.ed25519_verify` (`[u8;32]`); `RevocationCert::{quorum_signing_payload_bytes, sign_quorum_part, assemble_quorum, verify_quorum_with_signers}`.

---

## Task 1: `KeyTree::generate_at_epoch` — random material for a master-less bump

**Files:**
- Modify: `src-tauri/src/owner_state_crypto.rs` (add method in `impl KeyTree`, ~after line 181)
- Test: same file `#[cfg(test)]`

**Interfaces:**
- Produces: `pub fn generate_at_epoch(epoch: u32) -> Self` — a `KeyTree` whose five 32-byte sub-keys are cryptographically-random (OsRng), stamped at `epoch`. Round-trips through `to_fleet_material`/`from_fleet_material`.

- [ ] **Step 1: Failing test.**
```rust
#[test]
fn generate_at_epoch_is_random_and_material_round_trips() {
    let a = KeyTree::generate_at_epoch(3);
    let b = KeyTree::generate_at_epoch(3);
    assert_eq!(a.epoch, 3);
    // Two independent generations differ (not master-derived).
    assert_ne!(a.to_fleet_material().entry_aead, b.to_fleet_material().entry_aead);
    // Material round-trips byte-identical.
    let m = a.to_fleet_material();
    let back = KeyTree::from_fleet_material(&m).expect("round-trip");
    assert_eq!(back.to_fleet_material().entry_aead, m.entry_aead);
    assert_eq!(back.epoch, 3);
    // Distinct from a master-derived tree at the same epoch.
    let seed = [7u8; 32];
    let derived = KeyTree::derive_at_epoch(&seed, 3).unwrap();
    assert_ne!(a.to_fleet_material().root_aead, derived.to_fleet_material().root_aead);
}
```
Note: `to_fleet_material` fields are private outside the module — this test lives **inside** `owner_state_crypto.rs`'s `mod tests`, where `FleetKeyMaterial`'s private fields are visible. (If they are not visible even in-module, compare via `crate::owner_state_crypto::encode_fleet_material_set(&[m])` byte equality instead.)

- [ ] **Step 2: Run — fails** (`generate_at_epoch` undefined). `cargo nextest run --locked --features test-fixtures -E 'test(generate_at_epoch)'`.

- [ ] **Step 3: Implement** (after `derive_at_epoch`, ~line 181):
```rust
    /// Generate a KeyTree with fresh **random** key material at `epoch`, for a
    /// master-less (quorum) fleet epoch bump. Fleet keys are symmetric AEAD
    /// material distributed to survivors as sealed blobs — never required to be
    /// re-derivable from the master seed (cert-only devices already adopt via
    /// [`Self::from_fleet_material`]). The initiator of a quorum bump holds no
    /// seed, so it mints new material here and seals it to survivors.
    pub fn generate_at_epoch(epoch: u32) -> Self {
        use rand::RngCore;
        let mut fill = || {
            let mut k = Zeroizing::new([0u8; 32]);
            rand::rngs::OsRng.fill_bytes(k.as_mut());
            k
        };
        Self {
            epoch,
            entry_aead: fill(),
            root_aead: fill(),
            lookup: fill(),
            nonce: fill(),
            friend_aead: fill(),
        }
    }
```

- [ ] **Step 4: Run — passes.**

- [ ] **Step 5: Commit** `feat(zeb-677-s5): KeyTree::generate_at_epoch for master-less fleet bump`.

---

## Task 2: `FleetKeyEpochDoc` quorum signature (self-certifying, additive)

**Files:**
- Modify: `src-tauri/src/fleet_key_epoch.rs`
- Test: same file

**Interfaces:**
- Produces:
  - `pub struct QuorumDocSig { pub signers: Vec<[u8;16]>, pub signatures: Vec<Vec<u8>> }`
  - New `FleetKeyEpochDoc` fields: `quorum_sig: Option<QuorumDocSig>` (serde `"q"`), `signer_certs: Vec<EnrollmentCert>` (serde `"c"`).
  - `FleetKeyEpochDoc::quorum_signing_bytes(&self) -> Result<Vec<u8>, String>` — alias of `signing_bytes` (same domain; a quorum doc has `master_pubkey: None`).
  - `FleetKeyEpochDoc::sign_quorum_part(sk: &SigningKey, bytes: &[u8]) -> Vec<u8>` (raw ed25519 over the domain-separated bytes).
  - `FleetKeyEpochDoc::assemble_quorum(&mut self, signers: Vec<[u8;16]>, signatures: Vec<Vec<u8>>, signer_certs: Vec<EnrollmentCert>)`.
  - `FleetKeyEpochDoc::verify_quorum(&self, owner_id: &[u8;16], now_secs: u64) -> bool`.
  - `verify` dispatches: `quorum_sig.is_some()` → needs `now_secs` (see Task 3 signature change).

- [ ] **Step 1: Failing tests** (append to `mod tests`):
```rust
fn master_cert_for(owner_sk: &ed25519_dalek::SigningKey, dev_sk: &ed25519_dalek::SigningKey, owner_id: [u8;16], now: u64) -> harmony_owner::certs::EnrollmentCert {
    // Helper: mint a Master-issued EnrollmentCert for dev under owner. Reuse the
    // existing test mint helper in this crate if present (grep `sign_master` test
    // usages, e.g. owner_quorum_sync tests) rather than re-deriving.
    unimplemented!("use the shared test helper")
}

#[test]
fn quorum_doc_signs_verifies_and_rejects_bad_bundles() {
    // Two Master-issued signer devices under the same owner.
    // Build an unsigned doc (epoch 1, some sealed map), each signer signs
    // quorum_signing_bytes, assemble_quorum with both certs.
    // ASSERT: doc.verify_quorum(&owner_id, now) == true.
    // ASSERT reject: <2 signers; a non-Master signer cert; a wrong-owner signer
    // cert; a tampered `sealed` entry (sig no longer matches); a signature by a
    // key other than the claimed signer's.
}

#[test]
fn quorum_doc_master_golden_fixture_unchanged() {
    // The existing wire_encoding_is_pinned... fixture (quorum_sig None,
    // signer_certs empty) must still encode to the pinned hex.
}
```

- [ ] **Step 2: Run — fails.**

- [ ] **Step 3: Implement.**
  1. Add near the struct:
```rust
use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};

/// A quorum co-signature envelope over [`FleetKeyEpochDoc::signing_bytes`],
/// mirroring `EnrollmentIssuer::Quorum`. Parallel vecs: `signatures[i]` is by
/// `signers[i]`'s enrolled ed25519 key.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuorumDocSig {
    #[serde(rename = "n", with = "quorum_signers_cbor")]
    pub signers: Vec<[u8; 16]>,
    #[serde(rename = "g")]
    pub signatures: Vec<Vec<u8>>,
}
```
  (Use a small local serde module `quorum_signers_cbor` for `Vec<[u8;16]>`, or store signers as `Vec<String>` hex to avoid a custom codec — pick hex `Vec<String>` for simplicity and to match the request-doc hex idiom; adjust the struct + verify accordingly.)
  2. Add two fields to `FleetKeyEpochDoc` (after `master_sig`):
```rust
    /// Quorum co-signature (master-less bump). Mutually exclusive with
    /// `master_sig` in practice; `verify` dispatches on presence.
    #[serde(rename = "q", default, skip_serializing_if = "Option::is_none")]
    pub quorum_sig: Option<QuorumDocSig>,
    /// Depth-1 signer bundle: the Master-issued EnrollmentCert of each quorum
    /// signer. Embedded so the SYNCHRONOUS carrier merger verifies without
    /// locking the async trust doc (mirrors ZEB-677 §2 chain carriage). Empty
    /// for master-signed docs.
    #[serde(rename = "c", default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
```
  3. `sign_quorum_part` + `assemble_quorum`:
```rust
impl FleetKeyEpochDoc {
    pub fn sign_quorum_part(sk: &ed25519_dalek::SigningKey, bytes: &[u8]) -> Vec<u8> {
        sk.sign(bytes).to_bytes().to_vec()
    }
    pub fn assemble_quorum(
        &mut self,
        signers: Vec<[u8; 16]>,
        signatures: Vec<Vec<u8>>,
        signer_certs: Vec<EnrollmentCert>,
    ) {
        self.master_pubkey = None;
        self.master_sig = Vec::new();
        self.quorum_sig = Some(QuorumDocSig { signers, signatures });
        self.signer_certs = signer_certs;
    }
```
  4. `verify_quorum` (self-contained, no trust doc):
```rust
    /// Verify a quorum-signed doc against its EMBEDDED signer bundle. Checks,
    /// mirroring the crate's `verify_quorum_with_signers`: ≥2 distinct signers;
    /// parity signers/signatures; every signer id has a matching embedded cert;
    /// each cert is Master-issued, `owner_id`-bound, and valid at `now_secs`;
    /// each signature verifies against that cert's enrolled ed25519 key over
    /// `signing_bytes()`. Live-revocation is NOT checked here (the ceremony
    /// gates signer revocation initiator-side; masters/master-issued signers
    /// mirror the master path, which also does not check revocation).
    pub fn verify_quorum(&self, owner_id: &[u8; 16], now_secs: u64) -> bool {
        let Some(q) = self.quorum_sig.as_ref() else { return false };
        if q.signers.len() < 2 || q.signers.len() != q.signatures.len() { return false; }
        let mut seen = std::collections::BTreeSet::new();
        for s in &q.signers { if !seen.insert(*s) { return false; } } // distinct
        let Ok(bytes) = self.signing_bytes() else { return false };
        for (signer_id, sig_bytes) in q.signers.iter().zip(q.signatures.iter()) {
            let Some(cert) = self.signer_certs.iter().find(|c| c.device_id == *signer_id) else { return false; };
            if cert.owner_id != *owner_id { return false; }
            if !matches!(cert.issuer, EnrollmentIssuer::Master { .. }) { return false; }
            if cert.verify(now_secs).is_err() { return false; }
            let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&cert.device_pubkeys.classical.ed25519_verify) else { return false; };
            let Ok(sig_arr) = <[u8;64]>::try_from(sig_bytes.as_slice()) else { return false; };
            if vk.verify_strict(&bytes, &ed25519_dalek::Signature::from_bytes(&sig_arr)).is_err() { return false; }
        }
        true
    }
```
  5. Dispatch in `verify` (signature changes to add `now_secs` — Task 3 updates callers):
```rust
    pub fn verify(&self, owner_id: &[u8; 16], now_secs: u64) -> bool {
        if self.quorum_sig.is_some() {
            return self.verify_quorum(owner_id, now_secs);
        }
        // ... existing master-signature body ...
    }
```

- [ ] **Step 4: Run — passes.** Confirm the golden fixture still matches (quorum_sig `None` + signer_certs empty omitted).

- [ ] **Step 5: Add a quorum golden fixture** pinning a canonical quorum doc's hex (new, does not replace the master fixture). Commit `feat(zeb-677-s5): FleetKeyEpochDoc self-certifying quorum signature`.

---

## Task 3: Reader threading — verify quorum docs in merge + boot

**Files:**
- Modify: `src-tauri/src/fleet_key_epoch.rs` (`merge_fleet_keys_remote`, `load_doc_or_recover`, their tests)
- Modify: `src-tauri/src/lib.rs` (carrier merger 5836–5848, boot `load_doc_or_recover` 5815)
- Modify: `src-tauri/src/owner_commands.rs` (merge callsite 2249)

**Interfaces:**
- `merge_fleet_keys_remote(local, remote, owner_id, now_secs: u64) -> bool`
- `load_doc_or_recover(path, owner_id, now_secs: u64) -> FleetKeyEpochDoc`
- Both dispatch to `verify(owner_id, now_secs)`; quorum docs verify via the embedded bundle (no trust doc needed).

- [ ] **Step 1: Failing tests** — extend `fleet_key_epoch.rs` merge tests: a valid quorum doc at higher epoch **adopts**; a quorum doc with a tampered `sealed`/bad signer bundle **rejected**; master-doc behavior unchanged.

- [ ] **Step 2: Run — fails** (arity mismatch + new cases).

- [ ] **Step 3: Implement.**
  - Add `now_secs` param to both fns; pass through to `verify`. Compute `now_secs` at each callsite via `std::time::SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()` (sync — fine inside the merger closure).
  - lib.rs merger (5836–5848): capture nothing new; compute `now_secs` inside the closure and pass it.
  - lib.rs boot (5815) and owner_commands.rs (2249): add `now_secs`.

- [ ] **Step 4: Run — passes.** `cargo check --locked` to catch all callers.

- [ ] **Step 5: Commit** `feat(zeb-677-s5): carrier reader accepts quorum-signed epoch docs`.

---

## Task 4: `plan_fleet_epoch_bump_quorum` — build the unsigned master-less epoch doc

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (new fn near `plan_fleet_epoch_bump`; refactor the sealing loop into a shared helper)
- Test: same file

**Interfaces:**
- Refactor `plan_fleet_epoch_bump`'s sealing loop (227–250) into:
  `fn seal_material_to_survivors(trust: &OwnerState, kt: &KeyTree, exclude: Option<[u8;16]>) -> Result<BTreeMap<String, Vec<u8>>, String>`
  (excludes revoked devices already; `exclude` additionally drops the revocation target when bundling, since the target may not yet be revoked in the snapshot at request build time).
- Produces:
  `pub(crate) fn plan_fleet_epoch_bump_quorum(trust: &OwnerState, current_data_epoch: u32, now_ms: u64, exclude_target: Option<[u8;16]>) -> Result<(FleetKeyEpochDoc, KeyTree), String>`
  — generates `KeyTree::generate_at_epoch(current_data_epoch + 1)`, seals to survivors (minus `exclude_target`), returns an **unsigned** doc (`quorum_sig: None`, `signer_certs: []`, `master_*` empty) + the new tree. The doc's `signing_bytes()` is what signers co-sign.

- [ ] **Step 1: Failing test** — `plan_fleet_epoch_bump_quorum` produces a doc at `current+1`, seals to all active non-target devices, tree is random (differs across two calls), doc is unsigned (`verify_quorum` false until assembled).

- [ ] **Step 2: Run — fails.**

- [ ] **Step 3: Implement** — extract `seal_material_to_survivors` (reused by both master and quorum planners), then the quorum planner. Master `plan_fleet_epoch_bump` keeps behavior byte-identical (regression: existing epoch-bump tests still pass).

- [ ] **Step 4: Run — passes** (new + existing epoch-bump tests).

- [ ] **Step 5: Commit** `feat(zeb-677-s5): plan_fleet_epoch_bump_quorum (random KeyTree, unsigned doc)`.

---

## Task 5: Bundle the epoch doc into the quorum revocation request

**Files:**
- Modify: `src-tauri/src/owner_quorum_sync.rs` (`QuorumRequestKind::Revocation` gains `epoch_doc_cbor_hex`)
- Modify: `src-tauri/src/owner_quorum_commands.rs` (`plan_quorum_revocation_request` builds + attaches the unsigned doc)
- Test: both files

**Interfaces:**
- `QuorumRequestKind::Revocation { reason, target_hex, epoch_doc_cbor_hex: Option<String> }` (serde `"d"`, `default`, `skip_serializing_if = "Option::is_none"`). Hex of canonical-CBOR of the unsigned `FleetKeyEpochDoc` from Task 4 (target excluded).
- `plan_quorum_revocation_request` gains inputs to build it: the current carrier epoch + a trust snapshot (it already has trust). Signature grows by the current fleet epoch: `..., current_fleet_epoch: u32, now_ms: u64`.

- [ ] **Step 1: Failing test** — a planned revocation request round-trips through `QuorumReqDoc` CBOR carrying `epoch_doc_cbor_hex = Some(..)`; the embedded doc decodes to an unsigned doc at `current_fleet_epoch + 1`. A request built with `current_fleet_epoch` unavailable (node not carrying keys) → `epoch_doc_cbor_hex = None` (revoke-only; banner offers manual rotate — the honest degraded path).

- [ ] **Step 2: Run — fails.**

- [ ] **Step 3: Implement** — in `plan_quorum_revocation_request`, after validating the target, call `plan_fleet_epoch_bump_quorum(trust, current_fleet_epoch, now_ms, Some(target))`, canonical-CBOR-encode the unsigned doc, hex it into the new field. Thread `current_fleet_epoch` from the caller (the IPC `request_quorum_revocation_inner` reads `keys.newest().epoch` from the resident `FleetKeySet`; `None`/0 when the node isn't carrying keys → skip the bump).

- [ ] **Step 4: Run — passes.**

- [ ] **Step 5: Commit** `feat(zeb-677-s5): bundle unsigned epoch doc into quorum revocation request`.

---

## Task 6: B-side — produce the second (epoch-doc) signature on co-sign

**Files:**
- Modify: `src-tauri/src/owner_quorum_commands.rs` (`cosign_request_core`)
- Test: same file

**Interfaces:**
- On co-sign, if `req.kind` is `Revocation { epoch_doc_cbor_hex: Some(hex), .. }`, decode the unsigned `FleetKeyEpochDoc`, compute `signing_bytes()`, `FleetKeyEpochDoc::sign_quorum_part(dsk, &bytes)`, and set `QuorumRequestSigs.epoch_doc_sig_hex = Some(hex(sig))` alongside the existing `primary_sig_hex`.

- [ ] **Step 1: Failing test** — an armed/authorized B co-signing a revocation request **with** a bundled epoch doc yields `QuorumRequestSigs { primary_sig_hex: <over RevocationCert payload>, epoch_doc_sig_hex: Some(<over epoch-doc bytes>) }`; a request **without** a bundled doc yields `epoch_doc_sig_hex: None` (unchanged S3 behavior).

- [ ] **Step 2: Run — fails.**

- [ ] **Step 3: Implement** — in `cosign_request_core` (275–282), branch on the kind's `epoch_doc_cbor_hex`; decode via `crate::owner_state_crypto::canonical_cbor_decode::<FleetKeyEpochDoc>`; sign its `signing_bytes()`. Keep `primary_sig_hex` exactly as today.

- [ ] **Step 4: Run — passes.**

- [ ] **Step 5: Commit** `feat(zeb-677-s5): co-signer produces detached epoch-doc signature`.

---

## Task 7: A-side — assemble the quorum-signed epoch doc

**Files:**
- Modify: `src-tauri/src/owner_quorum_sync.rs` (`try_assemble` return shape + epoch-doc assembly; `own_signer_cert_bundle` helper)
- Test: same file

**Interfaces:**
- `try_assemble(...) -> Option<QuorumAssembly>` where
  `struct QuorumAssembly { cert: Option<RevocationCert>, epoch_doc: Option<FleetKeyEpochDoc> }`. **Introduce `cert` as `Option` from the start** — always `Some` for the `Revocation` kind; Task 9's `EpochBump` kind sets it `None`. This avoids a mid-plan signature churn (self-review).
- Epoch-doc assembly: collect `epoch_doc_sig_hex` from the same ≥2 signers whose `primary_sig_hex` validated; mint A's own epoch-doc part (`sign_quorum_part` over the bundled doc's `signing_bytes()`); build `signers`/`signatures` (sorted, initiator included); resolve each signer's Master-issued `EnrollmentCert` from `trust`; `doc.assemble_quorum(signers, signatures, signer_certs)`. Only produce `epoch_doc: Some` when the request carried `epoch_doc_cbor_hex` **and** every counted signer supplied a valid `epoch_doc_sig_hex`; otherwise `None` (revoke still lands; banner retry).

- [ ] **Step 1: Failing test** — with a bundled request + B's dual sig, `try_assemble` returns `Some(QuorumAssembly { epoch_doc: Some(doc), .. })` where `doc.verify_quorum(&owner_id, now)` is true and `doc.epoch == current+1`; with a bundled request but B missing the epoch sig, returns `Some` with `epoch_doc: None` (revoke-only).

- [ ] **Step 2: Run — fails** (return type change ripples to `run_quorum_sweep` — expected; Task 8 fixes the driver).

- [ ] **Step 3: Implement** — change return type; add a `own_signer_cert_bundle(trust, signer_ids) -> Option<Vec<EnrollmentCert>>` that pulls each signer's Master-issued cert from `trust.enrollments` (None if any missing/non-Master — depth-1 guard). Verify each B `epoch_doc_sig_hex` against B's enrolled key over the bundled doc's `signing_bytes()` before counting it.

- [ ] **Step 4: Run — passes** (unit-level; driver compile handled next task).

- [ ] **Step 5: Commit** `feat(zeb-677-s5): assemble quorum-signed epoch doc from co-signatures`.

---

## Task 8: Driver — install + flush the quorum epoch doc after revoke

**Files:**
- Modify: `src-tauri/src/owner_quorum_sync.rs` (`run_quorum_sweep` signature + Phase B install)
- Modify: call sites of `run_quorum_sweep` (thread carrier handles + `FleetKeySet`) — likely `event_loop.rs`/`lib.rs`
- Test: `owner_quorum_sync.rs` two-engine test

**Interfaces:**
- `run_quorum_sweep` gains optional carrier handles: `carrier_doc: Option<Arc<Mutex<FleetKeyEpochDoc>>>`, `carrier_engine: Option<Arc<FleetSyncEngine<FleetKeyEpochDoc>>>`, `fleet_keys: Option<FleetKeySet>`. After applying a revocation whose `QuorumAssembly.epoch_doc` is `Some`, install under the **no-rollback rule** (mirror `revoke_device_inner` 899–916): lock carrier doc, `if new.epoch <= doc.epoch { warn; skip }` else `*doc = new; keys.install(from_fleet_material(unseal_own_material(...)))`? — **No**: the initiator already holds the new tree from `try_assemble`? It does not (assembly rebuilt only the doc). So: A unseals its own sealed blob from the assembled doc via `unseal_own_material` + `from_fleet_material` (same as the adoption task), `keys.install`, then `carrier_engine.notify_dirty()` + best-effort `flush_now()`. Warn-only on failure; revoke already stood in Phase B.

- [ ] **Step 1: Failing test — two-engine bundled ceremony.** Engine A (master-less, holds keys@epoch N) requests a quorum revoke of target T with a bundled bump; engine B co-signs (dual sig); drive both sweeps. ASSERT: T is revoked in both trust docs; A's carrier doc advances to epoch N+1 and `verify_quorum(&owner_id, now)` is true; A's `FleetKeySet.newest().epoch == N+1`; the epoch doc's `sealed` excludes T.

- [ ] **Step 2: Run — fails.**

- [ ] **Step 3: Implement** — thread the handles; guard the Phase-B **revoke** apply on `assembly.cert.is_some()` (so a future `EpochBump` assembly with `cert: None` installs the epoch doc only — Task 9 relies on this); add the epoch-doc install block guarded by `assembly.epoch_doc.is_some()`. Existing callers that don't carry keys pass `None` (revoke-only). Keep Phase C pruning unchanged.

- [ ] **Step 4: Run — passes.**

- [ ] **Step 5: Commit** `feat(zeb-677-s5): quorum sweep installs quorum-signed epoch doc (no-rollback)`.

---

## Task 9: Manual quorum epoch bump — `EpochBump` request kind + IPC

**Files:**
- Modify: `src-tauri/src/owner_quorum_sync.rs` (`QuorumRequestKind::EpochBump`), assembly for the standalone kind
- Modify: `src-tauri/src/owner_quorum_commands.rs` (planner + co-sign branch for `EpochBump`)
- Modify: `src-tauri/src/lib.rs` (`bump_fleet_epoch_inner` routes master-less → quorum request; or new `request_quorum_epoch_bump` IPC), `api/rpc.rs`
- Modify: `src-tauri/src/api/rpc.rs` (RPC mirror), Tauri handler registration
- Test: `owner_quorum_commands.rs` / `owner_quorum_sync.rs`

**Interfaces:**
- `QuorumRequestKind::EpochBump { epoch_doc_cbor_hex: String }` (serde `"m"`). No revocation. For this kind, `QuorumRequestSigs.primary_sig_hex` carries the **epoch-doc** signature (there is no RevocationCert payload); `epoch_doc_sig_hex` unused (`None`).
- IPC: `request_quorum_epoch_bump() -> Result<String, String>` (returns request id) — builds `plan_fleet_epoch_bump_quorum(trust, current_epoch, now_ms, None)`, writes an `EpochBump` request (A's own part attached), `notify_dirty` + `flush_now`. Master-less only (Master devices use `bump_fleet_epoch`). `_inner` seam + RPC mirror + handler registration.
- `try_assemble` handles `EpochBump`: no `RevocationCert`; returns `QuorumAssembly { cert: None, epoch_doc: Some }` (`cert` is already `Option` from Task 7). In Task 8's driver, Phase-B revoke is guarded on `cert.is_some()`, so an `EpochBump` assembly installs the epoch doc only — confirm that guard is written in Task 8 (skip-revoke-when-`cert`-None) so no change is needed here.
- `cosign_request_core` `EpochBump` branch: sign the embedded doc's `signing_bytes()` into `primary_sig_hex`.

- [ ] **Step 1: Failing tests** — (a) `request_quorum_epoch_bump` writes an `EpochBump` request with a bundled unsigned doc; (b) B co-signs → `primary_sig_hex` over epoch-doc bytes; (c) `try_assemble` on an `EpochBump` request returns `cert: None, epoch_doc: Some(valid)`; (d) two-engine: A manual-bumps, B co-signs, A installs epoch N+1 fleet-wide, no revocation occurs.

- [ ] **Step 2: Run — fails.**

- [ ] **Step 3: Implement** — the kind, planner, co-sign branch, assembly branch, driver skip-revoke-when-cert-None, IPC (+`_inner`+RPC+handler). Guard the IPC on master-less (`loaded.master_seed.is_none()`) and on quorum feasibility (≥2 Master-certed active devices incl self) — else a clear error.

- [ ] **Step 4: Run — passes.**

- [ ] **Step 5: Commit** `feat(zeb-677-s5): manual quorum epoch bump (EpochBump kind + IPC)`.

---

## Task 10: UI — master-less rotate affordance + honesty copy

**Files:**
- Modify: `src/lib/fleet-epoch-service.ts` (add `requestQuorumEpochBump()`)
- Modify: `src/lib/components/DevicesPanel.svelte` (fleet-epoch-banner non-seed branch 914–917)
- Test: `src/lib/components/DevicesPanel.test.ts` (or the panel's vitest file)

**Interfaces:**
- `export async function requestQuorumEpochBump(): Promise<string> { return invoke<string>('request_quorum_epoch_bump'); }`
- Banner logic: when `state.fleetEpochStale`:
  - seed holder (`state.canBackUp`): unchanged "Rotate fleet keys" → `bumpFleetEpoch()`.
  - master-less **and** quorum possible (`!state.selfIsMaster && <≥2 Master-certed active siblings incl self>` — reuse the same gate S4 uses for the arm affordance; expose a `canQuorumBump` view bool if the sibling-count logic isn't already frontend-visible): show "Rotate fleet keys (needs a co-sign)" → `requestQuorumEpochBump()`, with copy: *"Your other device will be asked to co-sign. Until keys rotate, a removed device may still read fleet-synced data."*
  - master-less and quorum **not** possible: existing honest copy, minus the false "rotate from your master device" if no master exists → *"This fleet can no longer rotate keys without the recovery phrase."* (fresh-identity floor, §8).

- [ ] **Step 1: Failing vitest** — renders the co-sign rotate button when `fleetEpochStale && !selfIsMaster && canQuorumBump`; renders the fresh-identity-floor copy when quorum not possible; seed path unchanged.

- [ ] **Step 2: Run — fails** (`npx vitest run <panel test>`).

- [ ] **Step 3: Implement** — add the service fn; branch the banner. If a `canQuorumBump` bool is needed, add it to `OwnerStateView` (Rust) computed like S4's arm gate and to `owner-service.ts`.

- [ ] **Step 4: Run — passes** (`npx vitest run`, `npx tsc --noEmit`).

- [ ] **Step 5: Commit** `feat(zeb-677-s5): DevicesPanel master-less rotate affordance + honesty copy`.

---

## Task 11: Integration — full bundled ceremony + reader rejection

**Files:**
- Test: `src-tauri/src/owner_quorum_sync.rs` (or a `tests/` integration file if the two-engine harness lives there)

**Interfaces:** consumes everything above.

- [ ] **Step 1: Tests.**
  1. **Full bundled cutoff:** A (master-less) `request_quorum_revocation` of T with bundle → B co-signs → drive sweeps → T revoked fleet-wide, carrier at N+1, `verify_quorum` true, T excluded from `sealed`, A's `FleetKeySet` publishes on N+1.
  2. **Reader rejects bad quorum doc:** a carrier doc whose `signer_certs` contains a **non-Master** (quorum-issued) signer is rejected by `merge_fleet_keys_remote` (depth-1); a doc with only 1 signer rejected; a tampered-`sealed` doc rejected.
  3. **No-rollback:** simulate epoch-doc install failure (e.g., stale carrier at higher epoch) → revoke still stands, `fleetEpochStale` true.

- [ ] **Step 2: Run — fail then pass.**

- [ ] **Step 3: Commit** `test(zeb-677-s5): full quorum cutoff ceremony + reader rejection matrix`.

---

## Final gates (before PR)

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (full sweep)
- [ ] `npx tsc --noEmit` && `npx vitest run`
- [ ] Update the spec §8 honesty ledger note if any interim copy changed; confirm the "Quorum revoke = instant crypto cutoff" row is now satisfied.
- [ ] PR to `zeblithic/harmony-client`, fire `@coderabbitai review` once, converge per standing loop rules.

## Self-review notes (author, 2026-07-12)

- **Spec coverage:** §7 doc-sig enum → Task 2 (additive field, not a literal enum — documented deviation for backward-compat + sync-merger self-containment, consistent with §2's additive-wire philosophy). Bundled bump → Tasks 4–8. Manual IPC quorum path → Task 9. Banner copy → Task 10. §10 epoch tests (carrier round-trip, reader rejects revoked/inactive signer, atomically-enough) → Tasks 2/3/11.
- **Deviation from spec "no chain carriage needed":** Task 2 embeds `signer_certs`. Rationale: the carrier `Merger` is synchronous and cannot lock the async trust `Mutex` to walk signers; embedding the depth-1 bundle keeps verification self-contained (and byte-cost is trivial next to the sealed map). Live-revocation is intentionally not enforced at the carrier — it mirrors the master path (no revocation check) and is gated in the ceremony. **Flag for PR reviewers.**
- **Random KeyTree:** validated against `from_fleet_material`'s "cert-only device, no seed" contract; no code re-derives epoch material from the seed except epoch-0 boot, which is untouched.
- **Type consistency:** `QuorumAssembly { cert: Option<RevocationCert>, epoch_doc: Option<FleetKeyEpochDoc> }` (Task 7 introduces, Task 9 makes `cert` optional — introduce it optional from the start to avoid a mid-plan signature churn).
- **Ordering risk:** Task 7 changes `try_assemble`'s return type before Task 8 fixes the driver — Task 7 leaves the tree non-compiling at the driver callsite; keep Tasks 7+8 in one working session (or gate Task 7's commit on a `#[allow(unused)]` shim). Acceptable within inline execution.
