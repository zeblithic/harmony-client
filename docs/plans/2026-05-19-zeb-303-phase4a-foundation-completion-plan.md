# ZEB-303 Phase 4a-foundation-completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the D-FROST foundation by adding wire-format fixtures, multi-engine integration tests, the 5 IPC commands, and the 3 Tauri events. Picks up the 6 deferred tasks from PR [#137](https://github.com/zeblithic/harmony-client/pull/137)'s ZEB-301 plan (Tasks 8-10, 12-14 of `docs/plans/2026-05-18-zeb-301-phase4a-foundation-dfrost-committee-plan.md`).

> **Scope narrowing — this plan was authored at full ZEB-303 scope but Tasks 5-7 (IPCs + Tauri events) were split out mid-implementation to keep the PR reviewable.** The PR shipping under this plan ([#140](https://github.com/zeblithic/harmony-client/pull/140)) ships ONLY Tasks 1-4 (wire-format fixtures + 3 multi-engine integration tests + 1 delta-#3 negative test). Tasks 5-7 (5 IPC handlers + 3 Tauri events + TS payload contracts + final 5-gate sweep) are tracked under [ZEB-305](https://linear.app/zeblith/issue/ZEB-305) and will ship in a follow-up PR. The Tasks 5-7 sections below remain as the design contract that ZEB-305 implements.

**Architecture:** Builds on the merged data layer (`community_dfrost_types.rs`, `community_dfrost_log.rs`, `community_dfrost_crypto.rs`). No new modules — fixtures + integration tests sit in `src-tauri/tests/`, IPC handlers extend `src-tauri/src/lib.rs`. Tauri events emitted from inside the IPC handlers after successful apply.

**Tech stack:** FROST-Ristretto255 v3.0.0 (already in `Cargo.toml`); ciborium for CBOR; existing `dm_outbox` for signing-key extraction; existing `community_admin_quorum_integration.rs` for engine-fixture idiom; tokio for async.

## Deltas from the ZEB-301 plan

The merged code diverged from the original ZEB-301 plan during R1-R6 bot review. Tests + IPCs must reflect:

1. **`PendingSlot` enum** (`PendingSlot::Dkg | PendingSlot::Refresh`) — `apply_dkg_complete` resolves the ceremony from either slot, so refresh-completion ceremonies must be looked up in `pending_refresh`, NOT `pending_dkg`.
2. **`consensus_verifying_shares`** field on `PendingCeremony` — first `dk` sets it; subsequent `dk`s must match exactly (cross-confirmation invariant). Tests building multi-engine DKG flows must produce matching `verifying_shares` from all engines.
3. **Post-activation `pending_dkg` rejection** — if `committee_state.active && PendingSlot::Dkg`, `apply_dkg_complete` returns `InvariantViolation`. Means: once a committee is active, the only way to rotate is `rf` → `dk` (refresh path), never a fresh `pending_dkg`.
4. **Split VRF domain separators** — `VRF_SEED_DS = b"dfrost-vrf-seed-v1"` (used inside `derive_vrf_seed`); `VRF_OUTPUT_DS = b"dfrost-vrf-output-v1"` (used inside `derive_vrf_output`). Test/IPC code must use the public helpers, not raw hashes.
5. **Schnorr signature verification in `vb` path** — `apply_threshold_sign` accumulates contributions; `apply_vrf_beacon` requires `community_dfrost_crypto::verify_schnorr_signature(joint_vk, msg, sig_bytes)` to succeed before binding the VRF output. Tests that produce `vb` events must supply a real aggregated Schnorr signature, not a synthetic 64-byte blob.
6. **`round2_packages` is `#[serde(skip, default)]`** — decrypted round-2 secrets are NEVER serialized to disk/wire. Integration tests must trigger round-2 via `apply_with_identity` (encrypted path), not by pre-populating `round2_packages` on a deserialized state.
7. **`apply_with_identity` requires `recipient_ciphertexts.is_some()` for `rn=2`** — broadcast `dr` rn=2 without sealed ciphertexts is rejected.
8. **`apply_proactive_refresh` decrypts BEFORE mutating** — staging the decrypted package as `Option<Vec<u8>>` first, then handing off to apply. IPC handlers for the refresh path must mirror this ordering.

These 8 deltas are LOAD-BEARING for the integration tests + IPC signing flows.

### Coverage notes for deltas #3 + #8

* **Delta `#3`** (post-activation `pending_dkg` rejection): integration test `dk_rejected_after_active_with_pending_dkg_slot` (added in Task 2) constructs an active committee and then attempts a fresh `dk` against a stale `pending_dkg`, asserting `InvariantViolation`. The unit test `dk_against_pending_dkg_after_activation_rejected` in `community_dfrost_log.rs` covers the same path single-engine.
* **Delta `#8`** (refresh decrypt-before-mutate): exhaustively unit-tested in `community_dfrost_log.rs::apply_proactive_refresh_decrypts_before_mutating` (PR #137). The integration test for refresh (Task 4) exercises the happy path; the failure-staging path is single-engine by construction (the local decrypt would fail before any peer state is touched).

### Security notes for IPC-layer fields (Tasks 5-6 / ZEB-305)

When the IPC layer (deferred to ZEB-305) introduces in-memory secret fields on `DfrostLog`:

- **`local_dkg_secret`** (`round1::SecretPackage`, `round2::SecretPackage`): MUST NOT be serialized (`#[serde(skip)]` mandatory — analogous to delta `#6` for `round2_packages`). Clear after `dk` succeeds and `apply_dkg_complete` activates the committee.
- **`local_signing_nonces`** (`round1::SigningNonces`): nonce reuse with a different message is catastrophic Schnorr-key recovery. MUST `#[serde(skip)]`. Clear IMMEDIATELY after `frost::round2::sign` consumes them — never persist past a single signing round.
- **`local_key_package`** (`KeyPackage`): the long-lived signing share. `#[serde(skip)]`; ZEB-305 must define a separate persistence path (the on-disk store should be encrypted-at-rest under the device's identity key, not the in-memory `DfrostLog`).
- **Where possible**, wrap these fields in `zeroize::Zeroizing<...>` or implement `ZeroizeOnDrop` so panics + drops don't leak share material via memory dump.

---

## File Structure

| File | Action | Purpose |
|---|---|---|
| `src-tauri/tests/wire_format_zeb303_dfrost_fixtures.rs` | Create | Regen-on-first-run CBOR fixtures for `dr`/`dk`/`ts`/`vb`/`rf` envelopes |
| `src-tauri/tests/community_dfrost_integration.rs` | Create | 2-engine convergence tests for DKG + threshold-sign + VRF + refresh |
| `src-tauri/src/lib.rs` | Modify | Add 5 IPC commands; emit 3 Tauri events; register IPCs in invoke_handler |
| `docs/plans/2026-05-19-zeb-303-phase4a-foundation-completion-plan.md` | Create | This plan |

---

## Task 0 — Pre-flight Verification (NO COMMIT)

**Files:** N/A (read-only check)

- [ ] **Step 1: Confirm branch + main lineage**

```bash
git log --oneline -5
git rev-parse --abbrev-ref HEAD  # expect: zeb-303-phase4a-foundation-completion
git merge-base HEAD origin/main  # expect: 0c79d59
```

- [ ] **Step 2: Confirm baseline 5 gates green on the merge base**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
# Skip nextest baseline — slow; the ZEB-301 PR already verified
```

Expected: fmt clean, clippy clean.

- [ ] **Step 3: Inventory ZEB-301 deltas in current `community_dfrost_log.rs`**

```bash
grep -n "PendingSlot\|consensus_verifying_shares\|VRF_OUTPUT_DS\|VRF_SEED_DS\|verify_schnorr_signature\|serde(skip" src-tauri/src/community_dfrost_log.rs src-tauri/src/community_dfrost_types.rs src-tauri/src/community_dfrost_crypto.rs
```

Expected: all 8 delta markers from "Deltas from ZEB-301 plan" section above resolve to live code references.

- [ ] **No commit for Task 0** — verification only.

---

## Task 1 — Wire-Format Fixture Pinning

**Files:**
- Create: `src-tauri/tests/wire_format_zeb303_dfrost_fixtures.rs`

Pattern source: `src-tauri/tests/wire_format_zeb250_fixtures.rs` (regen-on-first-run; structural CBOR key checks via `ciborium::Value`).

Per-variant coverage of all 5 `DfrostEventKind` kinds — 7 payload fixtures + 7 envelope fixtures total. `dr` and `rf` each get round-1 + round-2 variants (different on-wire shape: rn=1 carries `round1_package`, rn=2 carries `recipient_ciphertexts`), so the full variant set is: `dr_round1`, `dr_round2`, `dk`, `ts`, `vb`, `rf_round1`, `rf_round2`. Following the hex-pinned-constants idiom from `wire_format_zeb290_fixtures.rs` (not file-based fixtures).

- [ ] **Step 1: Write the regen-on-first-run helper**

```rust
fn fixture_path(name: &str) -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("dfrost");
    p.push(name);
    p
}

fn load_or_regen<F: FnOnce() -> Vec<u8>>(name: &str, gen: F) -> Vec<u8> {
    let path = fixture_path(name);
    if let Ok(existing) = std::fs::read(&path) {
        existing
    } else {
        let fresh = gen();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &fresh).unwrap();
        panic!(
            "REGENERATE: wrote fresh fixture {} ({} bytes). Re-run the test; \
             commit the new file if intentional.",
            path.display(),
            fresh.len()
        );
    }
}
```

- [ ] **Step 2: Add one fixture function per event kind**

Each test:
1. Constructs a `SignedCommitteeEvent` of the kind (use deterministic inputs — `community_id = [0u8; 16]`, `hlc = HybridLogicalClock { wall: 1_700_000_000_000, logical: 0, node: NodeId::from_bytes([0u8; 32]) }`, etc.)
2. CBOR-encodes via `ciborium::into_writer`
3. Calls `load_or_regen` with a fixed filename
4. Asserts byte-equality (re-encode should produce same bytes; deserializing the fixture should produce an equivalent struct)
5. Structurally inspects via `ciborium::Value::deserialize` to verify the envelope has exactly 8 keys, each 2 chars

```rust
#[test]
fn dr_round1_envelope_is_8_two_char_keys() {
    let evt = build_sample_dr_round1();
    let mut bytes = Vec::new();
    ciborium::into_writer(&evt, &mut bytes).unwrap();
    let _ = load_or_regen("dr_round1.cbor", || bytes.clone());

    let pinned = std::fs::read(fixture_path("dr_round1.cbor")).unwrap();
    assert_eq!(bytes, pinned, "dr_round1 wire format drifted");

    let value: ciborium::Value = ciborium::from_reader(&pinned[..]).unwrap();
    let map = value.as_map().expect("envelope must be a CBOR map");
    assert_eq!(map.len(), 8, "envelope must have exactly 8 fields");
    for (k, _) in map {
        let key = k.as_text().expect("key must be text");
        assert_eq!(key.len(), 2, "key {key} must be 2 chars (same-length-keys invariant)");
    }
}
```

- [ ] **Step 3: Verify same-length-keys invariant across ALL inner payloads**

For each kind, also assert all inner payload keys are exactly 2 chars (per spec §3 same-length-keys invariant).

- [ ] **Step 4: Run tests to verify fixtures generate cleanly on first run, pass on second run**

```bash
cd src-tauri
# First run: will panic with REGENERATE messages
cargo nextest run --locked --features test-fixtures -E 'test(wire_format_zeb303)' || true
# Second run: should pass cleanly
cargo nextest run --locked --features test-fixtures -E 'test(wire_format_zeb303)'
```

Expected: 5/5 fixture tests pass on second run.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/tests/wire_format_zeb303_dfrost_fixtures.rs src-tauri/tests/fixtures/dfrost/
git commit -m "test(zeb-303): pin canonical CBOR fixtures for dfrost events (dr/dk/ts/vb/rf)"
```

---

## Task 2 — Two-Engine DKG Integration Test

**Files:**
- Create: `src-tauri/tests/community_dfrost_integration.rs`

Pattern source: `src-tauri/tests/community_admin_quorum_integration.rs` (engine fixtures, `ev()` helpers, snapshot helpers).

Goal: build a 2-of-2 DKG ceremony where two engines both participate as committee members and converge on `committee_state.active == true` with identical `joint_verifying_key` AND identical per-member `verifying_shares` (delta `#2` cross-confirmation invariant).

NOTE: 2-of-2 (rather than 1-of-1) is used because FROST `dkg::part1` requires `min_signers ≥ 2` and because 2-of-2 exercises the round-2 sealed-package cross-engine exchange path that 1-of-1 would skip. Higher thresholds (5-of-7) are deferred to a follow-up.

- [ ] **Step 1: Build engine fixtures**

```rust
fn fresh_engine() -> (Ed25519Keypair, OwnerAddr, DfrostLog) {
    use rand::rngs::OsRng;
    use ed25519_dalek::SigningKey;
    let signing = SigningKey::generate(&mut OsRng);
    let owner = OwnerAddr::from_ed25519_verifying_key(&signing.verifying_key());
    (Ed25519Keypair::from_signing(signing), owner, DfrostLog::new())
}
```

- [ ] **Step 2: Drive a 2-of-2 ceremony through `apply` + `apply_with_identity` on both engines**

Use `community_dfrost_crypto::{dkg_part1_local, dkg_part2_local, dkg_part3_local}`. Both members run part1, broadcast `dr` rn=1; both run part2 against the other's r1 package, seal r2 packages per-recipient via `dm_signing::seal_to_owner`, broadcast `dr` rn=2; each engine decrypts the targeted ciphertext via `apply_with_identity`; both engines locally run `dkg_part3_local` to derive `KeyPackage` + `PublicKeyPackage`. Activation requires both members' `dk` confirmations (threshold=2) — both broadcast `dk` with the FROST-guaranteed-identical payloads.

- [ ] **Step 3: Assert both engines converge on identical `joint_verifying_key` AND `verifying_shares`**

```rust
assert!(engine_a.committee_state.active);
assert!(engine_b.committee_state.active);
assert_eq!(
    engine_a.committee_state.joint_verifying_key,
    engine_b.committee_state.joint_verifying_key,
);
assert_eq!(engine_a.committee_state.current_epoch, 1);
assert_eq!(engine_b.committee_state.current_epoch, 1);
// Delta #2: cross-confirmation invariant — per-member verifying_shares MUST match.
assert_eq!(
    engine_a.committee_state.verifying_shares,
    engine_b.committee_state.verifying_shares,
);
```

- [ ] **Step 4: Run test, verify pass**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(dfrost_integration::dkg_two_engine)'
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/tests/community_dfrost_integration.rs
git commit -m "test(zeb-303): two-engine 2-of-2 DKG convergence via real FROST crypto"
```

---

## Task 3 — Threshold Sign + VRF Beacon Integration Test

**Files:**
- Modify: `src-tauri/tests/community_dfrost_integration.rs`

Goal: extend Task 2's fixture with a threshold-sign + VRF beacon flow. After Task 2's `dk`, engine A produces a real FROST aggregated Schnorr signature on `message_hash = derive_vrf_seed(community_id, poll_id, epoch)`, both engines apply the `ts` + `vb` events, and engine B (non-committee for the signing operation) verifies the VRF output.

- [ ] **Step 1: Add `threshold_sign_two_engine` test**

Using engine A's `KeyPackage` from Task 2:
1. Call `frost_ristretto255::round1::commit` to produce signing nonces + commitments
2. Build `ts` event with `signing_commitments` payload
3. Apply on both engines via `apply()` (broadcast path; `apply_with_identity` only needed for round-2 DKG)
4. Aggregate via `frost_ristretto255::aggregate` → 64-byte Schnorr signature
5. Build `vb` event with `signature_bytes` + `vrf_output = derive_vrf_output(&signature_bytes[0..32])`
6. Apply on both engines
7. Assert `engine_b.committee_state` contains the materialized `vrf_output` for that beacon

- [ ] **Step 2: Verify Schnorr signature externally to confirm `verify_schnorr_signature` path was exercised**

```rust
community_dfrost_crypto::verify_schnorr_signature(
    &engine_b.committee_state.joint_verifying_key.unwrap(),
    &message_hash,
    &signature_bytes,
).expect("aggregated signature must verify under joint vk");
```

- [ ] **Step 3: Run test, verify pass**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(dfrost_integration::threshold_sign_two_engine)'
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/community_dfrost_integration.rs
git commit -m "test(zeb-303): two-engine threshold-sign + VRF beacon"
```

---

## Task 4 — Proactive Refresh Integration Test

**Files:**
- Modify: `src-tauri/tests/community_dfrost_integration.rs`

Goal: 2-of-2 epoch rotation preserves `joint_verifying_key` across the refresh (ZEB-301 acceptance criterion #4 surfaced empirically).

- [ ] **Step 1: Add `refresh_preserves_joint_vk_two_engine` test**

Continuing from Task 2's converged 2-of-2 state (both engines active at epoch 1):
1. Build `rf` rn=1 event with `proposed_epoch = 2` and per-recipient sealed packages (synthetic share bytes sealed to each member's X25519 pubkey via `dm_signing::seal_to_owner`)
2. Apply on both engines via `apply_with_identity` (decrypts the targeted ciphertext on each engine)
3. Both engines should have `pending_refresh[ceremony_id]` populated with `proposed_epoch = 2`
4. Build `dk` events (one per member, threshold=2 requires both) with the SAME joint_verifying_key as epoch 1 (per refresh invariant — `apply_dkg_complete` rejects any drift)
5. Apply both `dk` events on both engines via `apply()` (broadcast path)
6. Assert `engine_a.committee_state.current_epoch == 2 && joint_verifying_key == epoch_1_vk`
7. Assert same on engine B

- [ ] **Step 2: Run test, verify pass**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(dfrost_integration::refresh_preserves)'
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/community_dfrost_integration.rs
git commit -m "test(zeb-303): two-engine 2-of-2 refresh preserves joint vk across epoch rotation"
```

---

## Task 5 — IPC Handlers: `dfrost_initiate_dkg` + `dfrost_contribute_dkg_round`

> **Deferred to [ZEB-305](https://linear.app/zeblith/issue/ZEB-305).** Not in scope for this PR. The sections below remain as the design contract that ZEB-305 implements.

**Files:**
- Modify: `src-tauri/src/lib.rs`

IPC pattern source: `voting_create_tier1_poll` at `src-tauri/src/lib.rs:20098` (snake_case params, NodeState lock unpack, dm_outbox signing-key extraction, log apply, optional fanout).

`NodeState.dfrost_logs` is `Arc<tokio::Mutex<HashMap<SpaceId, Arc<tokio::Mutex<DfrostLog>>>>>` (added in PR #137).

- [ ] **Step 1: Add `dfrost_initiate_dkg`**

```rust
#[tauri::command]
async fn dfrost_initiate_dkg<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    members: Vec<String>,  // hex-encoded OwnerAddrs
    threshold: u16,
) -> Result<String, String> {
    // 1. Parse community_id + members from hex
    // 2. Lock NodeState; extract dm_outbox, dfrost_logs, dm_self_owner, hlc_tracker, dm_device_id
    // 3. Compute ceremony_id = derive_ceremony_id(community_id, &members, threshold, proposed_epoch=current+1)
    // 4. Run dkg_part1_local for self
    // 5. Reserve HLC; build signed `dr` rn=1 event
    // 6. Get-or-insert DfrostLog for community; apply locally
    // 7. Stash local_dkg_secret + identifier_map on DfrostLog
    // 8. Return hex(ceremony_id)
}
```

Emit `dfrost-dkg-progress` event via `app.emit("dfrost-dkg-progress", { ceremony_id, round_num: 1, participants_so_far: 1 })`.

- [ ] **Step 2: Add `dfrost_contribute_dkg_round`**

Two sub-cases: `round_num=2` (uses `dkg_part2_local`, requires per-recipient X25519 seal via `dm_signing::seal_to_owner`); `round_num=3` (uses `dkg_part3_local` to derive joint vk + verifying shares, builds `dk` event).

```rust
#[tauri::command]
async fn dfrost_contribute_dkg_round<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    ceremony_id: String,  // hex
    round_num: u8,
) -> Result<(), String> {
    // 1. Parse ids
    // 2. Lock NodeState; extract dm_outbox, dfrost_logs, dm_self_owner, hlc_tracker, dm_device_id
    // 3. Lock DfrostLog for community; lookup pending ceremony
    // 4. Branch on round_num:
    //    - 2: collect peers' round-1 packages from pending; run dkg_part2_local;
    //         seal each to recipient via dm_signing::seal_to_owner; build `dr` rn=2 event
    //         with recipient_ciphertexts; apply_with_identity locally
    //    - 3: collect peers' round-2 packages (from local decrypts); run dkg_part3_local;
    //         build `dk` event with joint_vk + verifying_shares; apply locally
    // 5. Emit dfrost-dkg-progress
    Ok(())
}
```

- [ ] **Step 3: Register both IPCs in `invoke_handler` macro and add to the dfrost section comment**

- [ ] **Step 4: Run gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --features test-fixtures -E 'test(dfrost)'  # smoke
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-303): IPCs dfrost_initiate_dkg + dfrost_contribute_dkg_round"
```

---

## Task 6 — IPC Handlers: `dfrost_request_vrf_beacon` + `dfrost_contribute_threshold_sign` + `dfrost_propose_refresh`

> **Deferred to [ZEB-305](https://linear.app/zeblith/issue/ZEB-305).** Not in scope for this PR.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add `dfrost_request_vrf_beacon`**

```rust
#[tauri::command]
async fn dfrost_request_vrf_beacon<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    seed_hex: String,  // 32-byte seed
    epoch: u64,
) -> Result<String, String> {
    // 1. Parse ids + seed
    // 2. Verify caller is committee member (lookup committee_state.members)
    // 3. Compute message_hash = derive_vrf_seed(community_id, &seed, epoch)
    // 4. Generate signing_nonces + signing_commitments via frost_ristretto255::round1::commit
    // 5. Build `ts` event with signing_commitments payload
    // 6. Apply locally; stash nonces on DfrostLog.local_signing_nonces
    // 7. Return hex(ceremony_id)
}
```

- [ ] **Step 2: Add `dfrost_contribute_threshold_sign`**

Once enough `ts` events are received (threshold met), this IPC drives the FROST sign round-2 + aggregation:

```rust
#[tauri::command]
async fn dfrost_contribute_threshold_sign<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    ceremony_id: String,
) -> Result<(), String> {
    // 1. Lookup pending_sign session
    // 2. Run frost_ristretto255::round2::sign with local nonces + commitments
    // 3. Once we have threshold shares: frost_ristretto255::aggregate → 64-byte sig
    // 4. Compute vrf_output = derive_vrf_output(&sig[0..32])
    // 5. Build `vb` event with sig + vrf_output
    // 6. Apply locally → emits dfrost-beacon-ready event
}
```

- [ ] **Step 3: Add `dfrost_propose_refresh`**

```rust
#[tauri::command]
async fn dfrost_propose_refresh<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    new_members: Vec<String>,
    threshold: u16,
) -> Result<String, String> {
    // 1. Verify caller is admin (delegate to existing admin-quorum check if needed)
    // 2. Compute ceremony_id = derive_ceremony_id(community_id, &new_members, threshold, current_epoch + 1)
    // 3. Run dkg_part1_local for self (NEW share, OLD identifier preserved)
    // 4. Build `rf` rn=1 event with proposed_epoch = current + 1
    // 5. Apply locally
    // 6. Emit dfrost-refresh-progress
}
```

- [ ] **Step 4: Register all three IPCs in `invoke_handler`**

- [ ] **Step 5: Run gates**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-303): IPCs dfrost_request_vrf_beacon + dfrost_contribute_threshold_sign + dfrost_propose_refresh"
```

---

## Task 7 — Tauri Event Emission Audit

> **Deferred to [ZEB-305](https://linear.app/zeblith/issue/ZEB-305).** Not in scope for this PR.

**Files:**
- Modify: `src-tauri/src/lib.rs`

Ensure all three Tauri events fire from the IPC handlers added in Tasks 5-6:

- `dfrost-dkg-progress` — `dfrost_initiate_dkg` (rn=1), `dfrost_contribute_dkg_round` (rn=2, rn=3)
- `dfrost-beacon-ready` — `dfrost_contribute_threshold_sign` after aggregation
- `dfrost-refresh-progress` — `dfrost_propose_refresh`

- [ ] **Step 1: Verify emission sites**

```bash
grep -n 'emit.*dfrost-' src-tauri/src/lib.rs
# Expect: at least 5 emit calls (rn=1, rn=2, rn=3, beacon-ready, refresh-progress)
```

- [ ] **Step 2: Add a TS-side type contract for each event payload**

```typescript
// src/lib/types/dfrost-events.ts
export type DfrostDkgProgress = {
    ceremony_id: string;  // hex
    round_num: 1 | 2 | 3;
    participants_so_far: number;
};

export type DfrostBeaconReady = {
    ceremony_id: string;
    vrf_output: string;  // hex(32)
};

export type DfrostRefreshProgress = {
    ceremony_id: string;
    round_num: 1 | 2 | 3;
};
```

- [ ] **Step 3: Run frontend gates**

```bash
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs src/lib/types/dfrost-events.ts
git commit -m "feat(zeb-303): Tauri events dfrost-dkg-progress + dfrost-beacon-ready + dfrost-refresh-progress"
```

---

## Task 8 — 5-Gate Sweep + Push + PR

**Files:** N/A

- [ ] **Step 1: Run all five gates from a clean state**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
npx tsc --noEmit
npx vitest run
```

If `cargo nextest` surfaces the known [ZEB-302](https://linear.app/zeblith/issue/ZEB-302) parallel-test flake (`rename_disambiguates_siblings_with_shared_cid`), note it in the PR body — it's not blocking.

- [ ] **Step 2: Push branch**

```bash
git push -u origin zeb-303-phase4a-foundation-completion
```

- [ ] **Step 3: Open PR with markdown-linked Linear refs**

```bash
gh pr create --title "ZEB-303 Phase 4a-foundation-completion: D-FROST IPCs + Tauri events + fixtures + multi-engine tests" --body "$(cat <<'EOF'
## Summary

Direct follow-up to [ZEB-301](https://linear.app/zeblith/issue/ZEB-301) (Phase 4a-foundation data layer, PR [#137](https://github.com/zeblithic/harmony-client/pull/137)). Ships the 6 deferred plan tasks: wire-format fixtures for all 5 dfrost event kinds, 2-engine integration tests for DKG/threshold-sign/VRF/refresh, the 5 IPC commands (`dfrost_initiate_dkg`, `dfrost_contribute_dkg_round`, `dfrost_request_vrf_beacon`, `dfrost_contribute_threshold_sign`, `dfrost_propose_refresh`), and 3 Tauri events.

After this lands, the D-FROST foundation is fully accessible from the frontend; Phase 4a-main (sortition + STAR + drafting + UI under [ZEB-293](https://linear.app/zeblith/issue/ZEB-293)) can begin.

## What ships

- `tests/wire_format_zeb303_dfrost_fixtures.rs` — 5 fixtures (`dr`/`dk`/`ts`/`vb`/`rf`), structural CBOR-key checks
- `tests/community_dfrost_integration.rs` — 4 integration tests: DKG convergence (`dkg_two_engine_2of2_converges_on_joint_vk`), threshold-sign + VRF beacon (`threshold_sign_two_engine_vrf_beacon_verifies`), refresh preserves joint vk (`refresh_two_engine_preserves_joint_vk`), delta `#3` post-activation pending_dkg rejection (`dk_rejected_after_active_with_pending_dkg_slot`)
- `src/lib.rs` — 5 new IPCs + 3 new Tauri events
- `src/lib/types/dfrost-events.ts` — TS payload contracts

## Gates

- ✅ `cargo fmt --all -- --check`
- ✅ `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- ✅ `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- ✅ `npx tsc --noEmit`
- ✅ `npx vitest run`

## Test plan

- [ ] Bot reviewers (CodeRabbit, Cursor) pass
- [ ] Manual review: confirm IPCs extract signing key from `dm_outbox` (no `todo!()` panics)
- [ ] Manual review: confirm Tauri events fire from all expected sites
- [ ] User reviews + merges

## Linked issues

- [ZEB-303](https://linear.app/zeblith/issue/ZEB-303) — this ticket
- [ZEB-301](https://linear.app/zeblith/issue/ZEB-301) — foundation data layer (merged)
- [ZEB-293](https://linear.app/zeblith/issue/ZEB-293) — Phase 4 Tier 3a parent
- [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) — umbrella voting/polling design

Closes ZEB-303

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 4: Capture PR number for the autonomous bot-review loop**

---

## Self-Review Checklist (run after writing this plan)

1. ✅ Each task ends with a commit (except Task 0)
2. ✅ All 8 deltas from "Deltas from the ZEB-301 plan" section are mentioned in the relevant tasks
3. ✅ IPC pattern follows `voting_create_tier1_poll` (NodeState lock + dm_outbox.signing_key extraction + log apply)
4. ✅ Final PR body uses markdown-linked Linear refs; only `Closes ZEB-303` is bare (Linear auto-close cascade rule)
5. ✅ 5 gates listed verbatim (`cargo fmt --all -- --check` not just `cargo fmt`, per `feedback_cargo_fmt_gate`)
6. ✅ `--features test-fixtures` included everywhere `--all-targets` is used (per CLAUDE.md "load-bearing" note)
