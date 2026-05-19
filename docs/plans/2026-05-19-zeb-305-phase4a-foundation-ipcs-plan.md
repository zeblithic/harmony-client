# ZEB-305 Phase 4a-foundation IPCs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Each implementer subagent MUST receive the "time-budget discipline" block per the `feedback_implementer_gate_time_budget` memory rule: commit before any long gate; if cargo exceeds 10 min wall-clock kill it and report DONE_WITH_CONCERNS.

**Goal:** Ship the 5 D-FROST committee management IPCs + 3 Tauri events + frontend TS contracts that the ticket scope ZEB-305 enumerates. This completes the Phase 4a-foundation surface so Phase 4a-main (sortition + STAR) can drive committee ceremonies.

**Architecture:** Five IPC handlers in `src-tauri/src/lib.rs` follow the `voting_create_tier1_poll` pattern source (lib.rs:20098): briefly lock `NodeState`, extract `dm_outbox`/`dfrost_logs`/`hlc_tracker`/etc, drop the lock, call into `community_dfrost_log::apply_with_identity` (the local-node path that decrypts sealed round-2 packages), emit a Tauri event on success. One small data-layer addition: a `local_nonces: Option<Vec<u8>>` field on `PendingSignSession` with `#[serde(skip, default)]` to stash the local FROST signing nonces between `dfrost_request_vrf_beacon` and `dfrost_contribute_threshold_sign`. Frontend TS payload contracts live at `src/lib/types/dfrost-events.ts`. Zenoh broadcast is **out of scope** (ticket explicitly defers to Phase 4a-main).

**Tech Stack:** Rust + Tauri 2 + `frost-ristretto255 = "3.0.0"` + `ciborium` (CBOR) + Svelte 5 + TS. No new deps.

---

## Status

- **Branch:** `zeb-305-phase4a-foundation-ipcs` (already created from `origin/main` @ 82a5c4f — ZEB-304 dev workflow PR squash). Working tree clean.
- **Base merges of note since ZEB-303 (PR #140):** ZEB-156 root-pin-set (PR #141 = 9a5c4ee), ZEB-304 dev-workflow PR (PR #142 = 82a5c4f). Neither touches dfrost code.
- **Verified gates green at base:** Implicitly — ZEB-304 PR #142 was green at squash; nothing has changed in the dfrost surface since.

---

## File Structure

**Modified files (existing):**
- `src-tauri/src/community_dfrost_log.rs` — add `local_nonces` field to `PendingSignSession`.
- `src-tauri/src/lib.rs` — add 5 IPC handlers + 3 Tauri event payload structs + register IPCs in the `tauri::generate_handler!` macro invocation.
- `src/lib/types/` (new directory if absent) — type contract file.

**New files:**
- `src/lib/types/dfrost-events.ts` — TypeScript event payload type definitions.
- `src-tauri/tests/community_dfrost_ipc_integration.rs` — 3 IPC round-trip integration tests.

**Untouched (read for patterns only):**
- `src-tauri/src/community_dfrost_crypto.rs` — `dkg_part1_local`, `dkg_part2_local`, `dkg_part3_local`, `verifying_key_to_bytes`, `verifying_share_to_bytes`, `verify_schnorr_signature` (all already exist from ZEB-301).
- `src-tauri/src/community_dfrost_types.rs` — `derive_vrf_seed`, `derive_vrf_output`, `DkgRoundPayload`, `VrfBeaconPayload`, `RefreshRoundPayload`, `ThresholdSignPayload`.
- `src-tauri/src/dm_signing.rs` — `seal_to_owner`, `open_from_owner` (X25519 sealed-package primitives).
- `src-tauri/src/lib.rs:20098` — `voting_create_tier1_poll` pattern source.
- `src-tauri/tests/community_dfrost_integration.rs` — multi-engine integration patterns (ZEB-303).

---

## Tasks

### Task 0: Pre-flight verification (no commit)

**Files:**
- Read: `src-tauri/src/community_dfrost_log.rs` (lines 215-225 for `PendingSignSession`, lines 720-740 for `apply_with_identity` signature)
- Read: `src-tauri/src/lib.rs` (lines 20098-20290 for `voting_create_tier1_poll` pattern)
- Verify: branch matches `zeb-305-phase4a-foundation-ipcs`, no uncommitted work

- [ ] **Step 1: Verify branch state**

```bash
git rev-parse --abbrev-ref HEAD  # expect: zeb-305-phase4a-foundation-ipcs
git status -uno  # expect: clean
git log --oneline -1  # expect: 82a5c4f Dev workflow: ... (PR #142)
```

- [ ] **Step 2: Confirm baseline gates are green (focused, not full workspace)**

```bash
cd src-tauri
cargo nextest run --locked --features test-fixtures -E 'binary(community_dfrost_integration)'
```

Expected: all 4 tests pass (the ZEB-303 multi-engine fixtures).

**No commit for Task 0.**

---

### Task 1: Add `local_nonces` field to `PendingSignSession`

**Files:**
- Modify: `src-tauri/src/community_dfrost_log.rs` (around line 218)
- Test: `src-tauri/src/community_dfrost_log.rs` inline `#[cfg(test)] mod tests`

- [ ] **Step 1: Write failing unit test**

Add to `community_dfrost_log.rs` inside the existing `mod tests` block:

```rust
#[test]
fn pending_sign_session_local_nonces_serde_skipped() {
    // local_nonces holds the local node's secret FROST signing nonces
    // between dfrost_request_vrf_beacon and dfrost_contribute_threshold_sign.
    // It MUST be marked #[serde(skip)] — persisting decrypted secret nonce
    // material across restarts leaks signing inputs (same security
    // posture as PendingDkg::round2_packages).
    let mut session = PendingSignSession::default();
    session.local_nonces = Some(vec![0xAA; 64]);
    session.message_hash = [0xBB; 32];

    // CBOR-encode → decode; local_nonces MUST round-trip as None.
    let mut buf = Vec::new();
    ciborium::into_writer(&session, &mut buf).expect("encode");
    let decoded: PendingSignSession = ciborium::from_reader(&buf[..]).expect("decode");
    assert_eq!(
        decoded.local_nonces, None,
        "local_nonces must be skipped during serialization (security)"
    );
    assert_eq!(decoded.message_hash, [0xBB; 32], "public fields preserved");
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pending_sign_session_local_nonces)'
```

Expected: FAIL with `error: no field local_nonces on PendingSignSession`.

- [ ] **Step 3: Add the field**

Modify `community_dfrost_log.rs` around line 218. Find:

```rust
pub struct PendingSignSession {
    /// VRF seed bytes (`derive_vrf_seed(poll_hash, epoch)`).
    pub message_hash: [u8; 32],
    /// Per-actor (commitments_bytes, share_bytes) contributions.
    pub contributions: BTreeMap<OwnerAddr, (Vec<u8>, Vec<u8>)>,
}
```

Replace with:

```rust
pub struct PendingSignSession {
    /// VRF seed bytes (`derive_vrf_seed(poll_hash, epoch)`).
    pub message_hash: [u8; 32],
    /// Per-actor (commitments_bytes, share_bytes) contributions.
    pub contributions: BTreeMap<OwnerAddr, (Vec<u8>, Vec<u8>)>,
    /// Local node's secret FROST signing nonces (CBOR-encoded
    /// `frost::round1::SigningNonces`). Populated by
    /// `dfrost_request_vrf_beacon` (which calls `frost::round1::commit`
    /// to produce both the public commitments + the secret nonces);
    /// consumed by `dfrost_contribute_threshold_sign` (which feeds them
    /// into `frost::round2::sign`).
    ///
    /// ZEB-305 security: `#[serde(skip, default)]` — these are the
    /// local node's secret nonces. Persisting them to disk would leak
    /// signing inputs across restarts. Same security justification as
    /// `PendingDkg::round2_packages`. Restart recovery for in-flight
    /// threshold-sign ceremonies requires re-requesting (re-running
    /// `dfrost_request_vrf_beacon`); we MUST NOT silently snapshot
    /// secret nonces onto the disk substrate.
    #[serde(skip, default)]
    pub local_nonces: Option<Vec<u8>>,
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pending_sign_session_local_nonces)'
```

Expected: PASS.

- [ ] **Step 5: Run formatter + clippy**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: 0 errors, 0 warnings.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_dfrost_log.rs
git commit -m "$(cat <<'EOF'
feat(zeb-305): add local_nonces field to PendingSignSession

The threshold-sign IPC pair (dfrost_request_vrf_beacon →
dfrost_contribute_threshold_sign) needs to retain the local node's
secret FROST signing nonces between calls. Adds a serde-skipped
Option<Vec<u8>> field on PendingSignSession mirroring the security
posture of PendingDkg::round2_packages.

EOF
)"
```

---

### Task 2: `dfrost_initiate_dkg` IPC + `dfrost-dkg-progress` event emission

**Files:**
- Modify: `src-tauri/src/lib.rs` (add to the IPC region near `voting_create_tier1_poll`)
- Modify: `src-tauri/src/lib.rs` (register in `tauri::generate_handler!`)

- [ ] **Step 1: Add Tauri event payload struct**

Add near the other voting-event payload structs in `lib.rs` (search for `VotingPollCreatedPayload`):

```rust
/// Tauri event payload for `"dfrost-dkg-progress"`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DfrostDkgProgressPayload {
    pub ceremony_id: String,
    pub round_num: u8,
    pub participants_so_far: u8,
}
```

- [ ] **Step 2: Add the IPC handler**

Add a new section in `lib.rs` (after the voting IPCs, before any unrelated handlers; search for a good landing zone near `voting_create_tier1_poll`):

```rust
/// Tauri IPC: admin initiates a D-FROST committee DKG ceremony.
/// Generates ceremony_id, runs `dkg_part1_local` for self, builds + signs
/// a `dr` rn=1 event, applies locally via `apply_with_identity`. Returns
/// hex(ceremony_id).
///
/// Out of scope (ZEB-305): Zenoh broadcast to peer committee members —
/// Phase 4a-main will wire the dfrost topic.
#[tauri::command]
async fn dfrost_initiate_dkg<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    members: Vec<String>, // hex-encoded OwnerAddr per member
    threshold: u16,
) -> Result<String, String> {
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(cid_bytes);

    // Decode + sort members for deterministic Identifier assignment.
    let mut member_addrs: Vec<crate::owner_state_types::OwnerAddr> = members
        .iter()
        .map(|hex_str| {
            let bytes: [u8; 16] = hex::decode(hex_str)
                .map_err(|e| format!("invalid member hex: {e}"))?
                .as_slice()
                .try_into()
                .map_err(|_| "member must be 16 bytes".to_string())?;
            Ok(crate::owner_state_types::OwnerAddr(bytes))
        })
        .collect::<Result<Vec<_>, String>>()?;
    member_addrs.sort();

    let max_signers = u16::try_from(member_addrs.len())
        .map_err(|_| "committee too large (>u16::MAX)".to_string())?;
    if max_signers < 2 {
        return Err("DKG requires min 2 members".to_string());
    }
    if threshold < 2 || threshold > max_signers {
        return Err(format!(
            "threshold must be in [2, {max_signers}], got {threshold}"
        ));
    }

    let (
        hlc_tracker,
        device_id,
        self_owner,
        self_x25519_priv,
        dm_outbox,
        dfrost_logs,
    ) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            crate::dm_signing::derive_x25519_priv(&g.dm_device_id.clone().ok_or("device_id missing")?),
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            std::sync::Arc::clone(&g.dfrost_logs),
        )
    };

    // Find self in member list to determine local Identifier.
    let self_index = member_addrs
        .iter()
        .position(|a| *a == self_owner)
        .ok_or("initiator must be a committee member")?;
    let self_id = crate::community_dfrost_crypto::identifier_for_index(self_index);

    // Generate ceremony_id from H(sorted_member_bytes || threshold || initiator_hlc).
    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    let mut hasher_bytes: Vec<u8> = Vec::with_capacity(member_addrs.len() * 16 + 10);
    for a in &member_addrs {
        hasher_bytes.extend_from_slice(&a.0);
    }
    hasher_bytes.extend_from_slice(&threshold.to_le_bytes());
    hasher_bytes.extend_from_slice(&hlc.physical.to_le_bytes());
    let ceremony_id: [u8; 32] = blake3::hash(&hasher_bytes).into();

    let (_r1_secret, r1_bytes) = crate::community_dfrost_crypto::dkg_part1_local(
        self_id,
        max_signers,
        threshold,
    )?;

    let payload = crate::community_dfrost_types::DkgRoundPayload {
        ceremony_id,
        round_num: 1,
        epoch: 0,
        proposed_members: member_addrs.clone(),
        threshold,
        round1_package: Some(r1_bytes),
        recipient_ciphertexts: None,
    };

    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        crate::community_dfrost_log::build_signed_dfrost_event(
            signing_key,
            self_owner,
            crate::community_dfrost_types::DfrostEventKind::DkgRound,
            &payload,
            hlc,
        )
        .map_err(|e| format!("build_signed: {e:?}"))?
    };

    // Apply locally via apply_with_identity (decrypts any sealed packages
    // — none on round 1, but the path is unified for symmetry with rn=2).
    {
        let log_arc = {
            let mut map = dfrost_logs.lock().await;
            map.entry(space_id)
                .or_insert_with(|| {
                    std::sync::Arc::new(tokio::sync::Mutex::new(
                        crate::community_dfrost_log::DfrostLog::new(),
                    ))
                })
                .clone()
        };
        let mut log = log_arc.lock().await;
        log.apply_with_identity(event, &self_owner, &self_x25519_priv)
            .map_err(|e| format!("apply: {e:?}"))?;
    }

    let ceremony_id_hex = hex::encode(ceremony_id);
    let evt_payload = DfrostDkgProgressPayload {
        ceremony_id: ceremony_id_hex.clone(),
        round_num: 1,
        participants_so_far: 1,
    };
    if let Err(e) = app.emit("dfrost-dkg-progress", &evt_payload) {
        tracing::warn!(error = %e, "dfrost-dkg-progress emit failed");
    }

    Ok(ceremony_id_hex)
}
```

- [ ] **Step 3: Register the IPC**

Find the `tauri::generate_handler!` invocation in `lib.rs` (search for `voting_create_tier1_poll` to find it; the macro lists every IPC handler). Add `dfrost_initiate_dkg` to the list.

- [ ] **Step 4: Verify it compiles + clippy clean**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -15
```

Expected: 0 errors. If `build_signed_dfrost_event` or `derive_x25519_priv` doesn't exist with that exact name, search the source:

```bash
grep -rn "fn build_signed_dfrost\|fn derive_x25519_priv" src/
```

and substitute the actual public function name. If `dfrost_logs` field doesn't exist on `NodeState`, locate the field via:

```bash
grep -n "dfrost_logs" src/lib.rs | head -5
```

- [ ] **Step 5: Run formatter**

```bash
cd src-tauri && cargo fmt --all
```

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-305): add dfrost_initiate_dkg IPC + dfrost-dkg-progress event

Admin-only entry point for a fresh D-FROST DKG ceremony. Derives
ceremony_id deterministically from member list + threshold + HLC,
runs frost::dkg::part1 for self, builds + signs a dr rn=1 event,
applies via apply_with_identity, emits dfrost-dkg-progress.

Out of scope (ticket): Zenoh broadcast to peer committee members.

EOF
)"
```

---

### Task 3: `dfrost_contribute_dkg_round` IPC (round_num=2)

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add the IPC handler for the round-2 path**

Add after `dfrost_initiate_dkg`:

```rust
/// Tauri IPC: committee member contributes their round-2 or round-3
/// DKG message. round_num=2 collects the round-1 packages received from
/// peers, runs frost::dkg::part2, seals each round-2 package per-recipient
/// via X25519, builds + signs a dr rn=2 event with recipient_ciphertexts.
/// round_num=3 collects round-2 packages received from peers (decrypted
/// via the local x25519 priv), runs frost::dkg::part3, builds + signs a
/// dk event with joint_vk + per-member verifying_shares.
#[tauri::command]
async fn dfrost_contribute_dkg_round<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    ceremony_id: String,
    round_num: u8,
) -> Result<(), String> {
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(cid_bytes);

    let ceremony_bytes: [u8; 32] = hex::decode(&ceremony_id)
        .map_err(|e| format!("invalid ceremony_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "ceremony_id must be 32 bytes".to_string())?;

    if round_num != 2 && round_num != 3 {
        return Err(format!("round_num must be 2 or 3, got {round_num}"));
    }

    let (hlc_tracker, device_id, self_owner, self_x25519_priv, dm_outbox, dfrost_logs) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        let did = g.dm_device_id.clone().ok_or("dm_device_id missing")?;
        let priv_key = crate::dm_signing::derive_x25519_priv(&did);
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            did,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            priv_key,
            g.dm_outbox.clone().ok_or("dm_outbox missing")?,
            std::sync::Arc::clone(&g.dfrost_logs),
        )
    };

    // Get the DfrostLog + pending ceremony state needed to build the round.
    let log_arc = {
        let mut map = dfrost_logs.lock().await;
        map.entry(space_id)
            .or_insert_with(|| {
                std::sync::Arc::new(tokio::sync::Mutex::new(
                    crate::community_dfrost_log::DfrostLog::new(),
                ))
            })
            .clone()
    };

    // Inspect pending state to figure out our member list + round-1 packages.
    let (members, threshold, round1_packages, round2_packages, participants_count) = {
        let log = log_arc.lock().await;
        let pending = log
            .pending_dkg
            .get(&ceremony_bytes)
            .ok_or("ceremony not found in pending_dkg")?;
        // members is sorted by ceremony invariant
        let r1: std::collections::BTreeMap<crate::owner_state_types::OwnerAddr, Vec<u8>> = pending
            .round1_packages
            .iter()
            .map(|(addr, pkg)| (*addr, pkg.clone()))
            .collect();
        let r2: std::collections::BTreeMap<crate::owner_state_types::OwnerAddr, Vec<u8>> = pending
            .round2_packages
            .iter()
            .map(|(addr, pkg)| (*addr, pkg.clone()))
            .collect();
        let count = r1.len() as u8;
        (
            pending.members.clone(),
            pending.threshold,
            r1,
            r2,
            count,
        )
    };

    let self_index = members
        .iter()
        .position(|a| *a == self_owner)
        .ok_or("not a committee member for this ceremony")?;
    let self_id = crate::community_dfrost_crypto::identifier_for_index(self_index);

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    let event = match round_num {
        2 => {
            // Re-run dkg_part1 for our identifier (deterministic from
            // identifier — we don't persist round-1 secret across calls
            // because round-1 secret is short-lived). We need the round-1
            // secret to feed into part2.
            //
            // R-note: Production implementations should reuse the same
            // round-1 secret produced during initiate. For ZEB-305 v1,
            // we accept the simplification that each member's part2 is
            // driven by a fresh part1 call — frost::dkg::part1 is
            // deterministic given a fixed RNG; we use OsRng so each call
            // produces fresh material. This is a placeholder until
            // proper secret-state persistence ships post-Phase-4a.
            return Err(
                "dfrost_contribute_dkg_round rn=2: round-1 secret persistence not yet implemented \
                 — use multi-engine test fixtures via apply_with_identity directly. Tracking via \
                 follow-up if this becomes a blocker for Phase 4a-main."
                    .to_string(),
            );
        }
        3 => {
            // R-note: same caveat as round 2 — need round-2 secret to call part3.
            return Err(
                "dfrost_contribute_dkg_round rn=3: round-2 secret persistence not yet implemented"
                    .to_string(),
            );
        }
        _ => unreachable!(),
    };

    #[allow(unreachable_code)]
    {
        let _ = event;
        let _ = log_arc;
        let _ = round2_packages;
        let _ = participants_count;
        let _ = self_x25519_priv;
        let _ = dm_outbox;
        let _ = app;
        let _ = threshold;
        Ok(())
    }
}
```

**DESIGN-DECISION CALL-OUT (read carefully):** The straightforward implementation of `dfrost_contribute_dkg_round` requires persisting the `round1::SecretPackage` between `initiate_dkg` and the round-2 call (FROST's `part2` takes `SecretPackage` by value). We have two options:

(A) **Persist the round-1 and round-2 SecretPackages on `DfrostLog`** with `#[serde(skip)]` (same pattern as `local_nonces`, `round2_packages`). This adds two more secret-state fields.

(B) **Defer the round-2/3 IPCs to a follow-up ticket** and only ship `dfrost_initiate_dkg` + the threshold-sign IPCs + refresh IPC in ZEB-305. The two-engine DKG flow remains exercised via `apply_with_identity` directly in tests (ZEB-303 already proves it works at the data layer).

The simpler/safer choice is **(A)** — add the secret-state fields. This task adopts that:

- [ ] **Step 2: Replace placeholder with real implementation (option A)**

Replace the stub IPC body above with the real implementation that:
- Adds two new `#[serde(skip, default)]` fields to `PendingDkg` in `community_dfrost_log.rs`:
  - `pub local_r1_secret: Option<Vec<u8>>` (CBOR-encoded `round1::SecretPackage`)
  - `pub local_r2_secret: Option<Vec<u8>>` (CBOR-encoded `round2::SecretPackage`)
- In Task 2 (`dfrost_initiate_dkg`), set `local_r1_secret = Some(ciborium::into_writer(&r1_secret))` BEFORE building the event so the apply path can persist it via a small helper `DfrostLog::stash_r1_secret(ceremony_id, bytes)`.
- In this Task 3:
  - For `round_num=2`: load `local_r1_secret` from pending, decode → `round1::SecretPackage`, call `dkg_part2_local`, encode `r2_secret` → stash via `stash_r2_secret`, seal each output package per-recipient via `dm_signing::seal_to_owner`, build `dr` rn=2 event with `recipient_ciphertexts`.
  - For `round_num=3`: load `local_r2_secret`, run `dkg_part3_local`, build `dk` event with `verifying_key_to_bytes(joint_vk)` + `verifying_share_to_bytes(...)` map.

Because this is multi-step and benefits from incremental TDD, the actual implementation should be done in subtasks 3a (data-layer fields + stash helpers + unit test), 3b (round_num=2 IPC body + integration check), 3c (round_num=3 IPC body + dk event build).

**For simplicity within this plan: implementers should treat Task 3 as a 3-step task and commit after each subtask. Task title kept as one logical IPC.**

- [ ] **Step 3: Verify cargo clippy + nextest**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'binary(community_dfrost_integration) or test(dfrost)'
```

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(zeb-305): dfrost_contribute_dkg_round IPC (round 2 + round 3)"
```

---

### Task 4: `dfrost_request_vrf_beacon` IPC + local_nonces stash

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add the IPC handler**

```rust
/// Tauri IPC: committee member produces their FROST signing-nonce
/// commitment for a VRF beacon. Computes message_hash = derive_vrf_seed,
/// runs frost::round1::commit (which returns secret SigningNonces +
/// public SigningCommitments), stashes the nonces on
/// PendingSignSession.local_nonces, builds + signs a ts event with
/// the public commitments.
#[tauri::command]
async fn dfrost_request_vrf_beacon<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    seed_hex: String,    // 32-byte poll_event_hash (or any seed) in hex
    epoch: u64,
) -> Result<String, String> {
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(cid_bytes);

    let seed_bytes: [u8; 32] = hex::decode(&seed_hex)
        .map_err(|e| format!("invalid seed hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "seed must be 32 bytes".to_string())?;

    let (hlc_tracker, device_id, self_owner, self_x25519_priv, dm_outbox, dfrost_logs) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        let did = g.dm_device_id.clone().ok_or("dm_device_id missing")?;
        let priv_key = crate::dm_signing::derive_x25519_priv(&did);
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            did,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            priv_key,
            g.dm_outbox.clone().ok_or("dm_outbox missing")?,
            std::sync::Arc::clone(&g.dfrost_logs),
        )
    };

    // Look up the active committee state for self_owner's KeyPackage.
    let log_arc = {
        let mut map = dfrost_logs.lock().await;
        map.entry(space_id)
            .or_insert_with(|| {
                std::sync::Arc::new(tokio::sync::Mutex::new(
                    crate::community_dfrost_log::DfrostLog::new(),
                ))
            })
            .clone()
    };

    let (key_package_bytes, ceremony_id, message_hash) = {
        let log = log_arc.lock().await;
        let active = log
            .active_committee
            .as_ref()
            .ok_or("no active committee — DKG must complete first")?;
        let kp_bytes = active
            .local_key_package_bytes
            .as_ref()
            .ok_or("local KeyPackage missing — were we a member of the DKG?")?
            .clone();
        let cid = active.ceremony_id;
        let msg_hash = crate::community_dfrost_types::derive_vrf_seed(&seed_bytes, epoch);
        (kp_bytes, cid, msg_hash)
    };

    // Decode local KeyPackage, run round1::commit.
    use frost_ristretto255::keys::KeyPackage;
    let key_package: KeyPackage = ciborium::from_reader(&key_package_bytes[..])
        .map_err(|e| format!("decode KeyPackage: {e}"))?;
    let signing_share = key_package.signing_share();

    let mut rng = frost_ristretto255::rand_core::OsRng;
    let (nonces, commitments) = frost_ristretto255::round1::commit(signing_share, &mut rng);

    let mut nonces_cbor = Vec::new();
    ciborium::into_writer(&nonces, &mut nonces_cbor).map_err(|e| format!("encode nonces: {e}"))?;
    let mut commitments_cbor = Vec::new();
    ciborium::into_writer(&commitments, &mut commitments_cbor)
        .map_err(|e| format!("encode commitments: {e}"))?;

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let hlc =
        crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    // Build + sign ts event with empty share (round 1 — share not yet computed).
    let payload = crate::community_dfrost_types::ThresholdSignPayload {
        ceremony_id,
        message_hash,
        commitments: commitments_cbor.clone(),
        share: Vec::new(), // empty until round 2 (contribute_threshold_sign)
    };

    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        crate::community_dfrost_log::build_signed_dfrost_event(
            signing_key,
            self_owner,
            crate::community_dfrost_types::DfrostEventKind::ThresholdSign,
            &payload,
            hlc,
        )
        .map_err(|e| format!("build_signed: {e:?}"))?
    };

    // Stash nonces on PendingSignSession.local_nonces BEFORE apply
    // (apply mutates pending_sign; stash after apply requires re-locking).
    // Strategy: apply first (which creates/updates the PendingSignSession),
    // then directly set local_nonces.
    {
        let mut log = log_arc.lock().await;
        log.apply_with_identity(event, &self_owner, &self_x25519_priv)
            .map_err(|e| format!("apply: {e:?}"))?;

        // Now stash the local nonces.
        let pending = log
            .pending_sign
            .get_mut(&ceremony_id)
            .ok_or("apply succeeded but pending_sign empty")?;
        pending.local_nonces = Some(nonces_cbor);
    }

    let ceremony_id_hex = hex::encode(ceremony_id);
    Ok(ceremony_id_hex)
}
```

- [ ] **Step 2: Register the IPC** (add to `tauri::generate_handler!`).

- [ ] **Step 3: Verify gates**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-305): dfrost_request_vrf_beacon IPC (stashes local FROST nonces)"
```

---

### Task 5: `dfrost_contribute_threshold_sign` IPC + `dfrost-beacon-ready` event

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add the event payload**

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DfrostBeaconReadyPayload {
    pub ceremony_id: String,
    pub vrf_output: String, // hex-encoded 32-byte VRF output
}
```

- [ ] **Step 2: Add the IPC handler**

The handler:
- Reads stashed `local_nonces` from `PendingSignSession`
- Decodes it back into `round1::SigningNonces`
- Reads peer `SigningCommitments` from `pending_sign.contributions`
- Runs `frost::round2::sign(signing_package, &nonces, &key_package)` → produces `SignatureShare`
- Builds a `ts` event with this share, applies it
- If threshold reached after apply: runs `frost::aggregate(...)` → 64-byte sig, builds a `vb` event with `vrf_output = derive_vrf_output(&sig[..32])`, applies that too, then emits `dfrost-beacon-ready`.

```rust
#[tauri::command]
async fn dfrost_contribute_threshold_sign<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    ceremony_id: String,
) -> Result<(), String> {
    // (Decode args, extract NodeState as in Task 4 — skip for brevity in the
    // plan; implementer should follow the same extraction pattern.)
    //
    // Then:
    //   1. Load PendingSignSession, decode local_nonces → SigningNonces.
    //   2. Build BTreeMap<Identifier, SigningCommitments> from
    //      pending_sign.contributions (each contribution = (commitments_bytes, share_bytes);
    //      decode the commitments_bytes per peer).
    //   3. signing_package = frost::SigningPackage::new(commitments_map, &message_hash)
    //   4. share = frost::round2::sign(&signing_package, &nonces, &key_package)?
    //   5. Encode share → CBOR bytes.
    //   6. Build ts event with `share = share_bytes`, apply.
    //   7. Check log.pending_sign[ceremony_id].contributions.len() vs threshold.
    //   8. If >= threshold:
    //      - Build sig = frost::aggregate(&signing_package, &shares_map, &pubkey_package)?
    //      - Encode sig → 64-byte bytes (frost::Signature::serialize).
    //      - r_compressed = sig_bytes[..32].try_into()?
    //      - vrf_output = derive_vrf_output(&r_compressed)
    //      - Build vb event payload {ceremony_id, message_hash, vrf_output, signature: sig_bytes, joint_vk: active.joint_vk}
    //      - Apply vb event.
    //      - Emit "dfrost-beacon-ready" with payload {ceremony_id_hex, vrf_output_hex}.
    todo!("implementer: fill in body per plan steps 1-8")
}
```

(Implementer subagent: replace the `todo!()` with the full body per the comment outline; cross-reference the `threshold_sign_two_engine_vrf_beacon_verifies` integration test in `tests/community_dfrost_integration.rs` for the exact `SigningPackage::new` / `aggregate` shape.)

- [ ] **Step 3: Register the IPC** (add to `tauri::generate_handler!`).

- [ ] **Step 4: Verify gates + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-305): dfrost_contribute_threshold_sign IPC + dfrost-beacon-ready event"
```

---

### Task 6: `dfrost_propose_refresh` IPC + `dfrost-refresh-progress` event

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add event payload**

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DfrostRefreshProgressPayload {
    pub ceremony_id: String,
    pub round_num: u8,
}
```

- [ ] **Step 2: Add the IPC**

Mirrors `dfrost_initiate_dkg` (run `dkg_part1_local` for self with NEW share but same identifier, build an `rf` rn=1 event with `proposed_epoch = current + 1`), with the differences:
- Read `active_committee.epoch` first; `proposed_epoch = current + 1`
- Event kind = `DfrostEventKind::ProactiveRefresh`
- Payload type = `RefreshRoundPayload` (mirrors `DkgRoundPayload` but with `epoch` field already in the wire format)
- Emit `dfrost-refresh-progress` with `{ceremony_id_hex, round_num: 1}`

(Implementer: structurally identical to Task 2 with the kind/payload/event swap. Refer to ZEB-303 `refresh_two_engine_preserves_joint_vk` test for refresh round-1 wire shape.)

- [ ] **Step 3: Register the IPC** (add to `tauri::generate_handler!`).

- [ ] **Step 4: Verify gates + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-305): dfrost_propose_refresh IPC + dfrost-refresh-progress event"
```

---

### Task 7: Frontend TypeScript event contracts

**Files:**
- Create: `src/lib/types/dfrost-events.ts`

- [ ] **Step 1: Write the file**

```typescript
// ZEB-305: TypeScript contracts for D-FROST Tauri events.
// Payload field naming follows the Tauri snake_case→camelCase boundary
// auto-conversion: Rust `pub ceremony_id: String` becomes `ceremonyId`
// in the TS payload (per `feedback_tauri_error_extraction` memory rule
// — though that's about ERROR extraction, same naming applies to event
// payloads via serde rename_all="camelCase").

/** Fires on every `dr` (DkgRound) event apply. */
export interface DfrostDkgProgressPayload {
  ceremonyId: string;       // hex(ceremony_id), 64 chars
  roundNum: number;         // 1, 2, or 3
  participantsSoFar: number; // count of round_num-1 packages received
}

/** Fires when a `vb` (VrfBeacon) event is applied — sig aggregated. */
export interface DfrostBeaconReadyPayload {
  ceremonyId: string;       // hex(ceremony_id), 64 chars
  vrfOutput: string;        // hex(vrf_output), 64 chars
}

/** Fires on every `rf` (ProactiveRefresh) event apply. */
export interface DfrostRefreshProgressPayload {
  ceremonyId: string;       // hex(ceremony_id), 64 chars
  roundNum: number;         // 1, 2, or 3
}

/** Event names: register listeners via `listen<DfrostDkgProgressPayload>(DFROST_DKG_PROGRESS, ...)`. */
export const DFROST_DKG_PROGRESS = 'dfrost-dkg-progress' as const;
export const DFROST_BEACON_READY = 'dfrost-beacon-ready' as const;
export const DFROST_REFRESH_PROGRESS = 'dfrost-refresh-progress' as const;
```

- [ ] **Step 2: Run frontend gates**

```bash
npx tsc --noEmit 2>&1 | tail -10
npx vitest run 2>&1 | tail -10
```

Expected: 0 type errors, no test regressions.

- [ ] **Step 3: Commit**

```bash
git add src/lib/types/dfrost-events.ts
git commit -m "feat(zeb-305): TypeScript event contracts for dfrost-* Tauri events"
```

---

### Task 8: Integration test — full DKG round-trip via IPCs

**Files:**
- Create: `src-tauri/tests/community_dfrost_ipc_integration.rs`

- [ ] **Step 1: Write a test that drives DKG end-to-end through the IPC handlers**

The test:
- Spins up two in-process NodeState instances (Alice + Bob)
- Calls `dfrost_initiate_dkg` from Alice's IPC handler (synchronously)
- Cross-applies Alice's `dr` rn=1 event to Bob's log (via `apply_with_identity` directly, simulating Zenoh delivery)
- Calls `dfrost_contribute_dkg_round(round_num=2)` from both Alice and Bob
- Cross-applies both `dr` rn=2 events
- Calls `dfrost_contribute_dkg_round(round_num=3)` from both Alice and Bob
- Cross-applies both `dk` events
- Asserts: both engines have identical `active_committee.joint_vk`

Pattern source: `community_dfrost_integration.rs::dkg_two_engine_2of2_converges_on_joint_vk` — adapt the helper struct `ActivatedCommittee` and `dkg_2of2_setup` to use the new IPC entry points instead of building events manually.

- [ ] **Step 2: Verify it passes**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'binary(community_dfrost_ipc_integration) and test(dkg)'
```

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/community_dfrost_ipc_integration.rs
git commit -m "test(zeb-305): full DKG IPC round-trip — 2-engine convergence"
```

---

### Task 9: Integration test — threshold-sign IPC round-trip

**Files:**
- Modify: `src-tauri/tests/community_dfrost_ipc_integration.rs`

- [ ] **Step 1: Add test driving the threshold-sign flow**

Builds on Task 8's `ActivatedCommittee` helper (DKG already complete on both sides):
- Calls `dfrost_request_vrf_beacon` from Alice + Bob with seed + epoch
- Cross-applies both `ts` events
- Calls `dfrost_contribute_threshold_sign` from Alice (or Bob — whoever crosses threshold)
- Asserts: a `vb` event was applied, joint Schnorr signature verifies via `verify_schnorr_signature`, `vrf_output` matches `derive_vrf_output`.

Pattern source: `community_dfrost_integration.rs::threshold_sign_two_engine_vrf_beacon_verifies`.

- [ ] **Step 2: Verify + commit**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'binary(community_dfrost_ipc_integration) and test(threshold_sign)'
git add src-tauri/tests/community_dfrost_ipc_integration.rs
git commit -m "test(zeb-305): threshold-sign IPC round-trip — VRF beacon end-to-end"
```

---

### Task 10: Integration test — refresh IPC round-trip

**Files:**
- Modify: `src-tauri/tests/community_dfrost_ipc_integration.rs`

- [ ] **Step 1: Add test driving the refresh flow**

Builds on Task 8's `ActivatedCommittee`:
- Calls `dfrost_propose_refresh` from Alice + Bob with same member list (new shares, same identifiers)
- Cross-applies both `rf` rn=1 events
- Calls `dfrost_contribute_dkg_round(round_num=2)` from Alice + Bob (refresh reuses DKG rounds 2+3)
- Calls `dfrost_contribute_dkg_round(round_num=3)` from Alice + Bob
- Cross-applies all events
- Asserts: `active_committee.joint_vk` is UNCHANGED after refresh (per acceptance criterion #4 from ZEB-301)

Pattern source: `community_dfrost_integration.rs::refresh_two_engine_preserves_joint_vk`.

- [ ] **Step 2: Verify + commit**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'binary(community_dfrost_ipc_integration) and test(refresh)'
git add src-tauri/tests/community_dfrost_ipc_integration.rs
git commit -m "test(zeb-305): refresh IPC round-trip — joint_vk preserved across epoch"
```

---

### Task 11: Final 5-gate sweep + push + PR creation

- [ ] **Step 1: Run all 5 CI gates locally**

```bash
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -5
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
npx tsc --noEmit 2>&1 | tail -10
npx vitest run 2>&1 | tail -10
```

All five must be clean: 0 fmt diffs, 0 clippy warnings, 0 test failures, 0 type errors, 0 frontend test failures.

**If any pre-existing test failures surface that are NOT introduced by this PR (e.g., known ZEB-302 flake class):** file a follow-up Linear ticket (do NOT invent a ZEB-NNN; use `mcp__plugin_linear_linear__save_issue` and use the returned ID). Document in the PR body. Do NOT bundle unrelated fixes into the active PR (per `feedback_unrelated_test_failures` memory rule).

- [ ] **Step 2: Push the branch**

```bash
git push -u origin zeb-305-phase4a-foundation-ipcs
```

- [ ] **Step 3: Create the PR via `gh pr create`**

```bash
gh pr create --title "ZEB-305 Phase 4a-foundation IPCs: D-FROST 5 Tauri commands + 3 Tauri events" --body "$(cat <<'EOF'
## Summary

Direct follow-up to [ZEB-303](https://linear.app/zeblith/issue/ZEB-303) (Phase 4a-foundation-completion tests + fixtures, merged in PR #140). Closes out the Phase 4a-foundation surface — the D-FROST committee data layer ([ZEB-301](https://linear.app/zeblith/issue/ZEB-301)) is now driveable from the frontend + Phase 4a-main sortition layer.

Closes ZEB-305.

## What ships

1. **5 IPC commands** with real signing-key extraction from `dm_outbox`:
   - `dfrost_initiate_dkg` — admin starts a new committee DKG ceremony
   - `dfrost_contribute_dkg_round` — committee member runs round 2 or 3
   - `dfrost_request_vrf_beacon` — committee member commits FROST nonces
   - `dfrost_contribute_threshold_sign` — committee member produces partial sig; aggregator on threshold reached
   - `dfrost_propose_refresh` — admin starts proactive secret-share refresh
2. **3 Tauri events** emitted from inside the IPC handlers on apply success:
   - `dfrost-dkg-progress` (on `dr` apply)
   - `dfrost-beacon-ready` (on `vb` apply)
   - `dfrost-refresh-progress` (on `rf` apply)
3. **Frontend TS payload contracts** at `src/lib/types/dfrost-events.ts`
4. **3 IPC round-trip integration tests** — DKG, threshold-sign, refresh (each end-to-end through the IPC handlers, 2-engine cross-apply)
5. **One data-layer addition**: `local_nonces: Option<Vec<u8>>` + `local_r1_secret: Option<Vec<u8>>` + `local_r2_secret: Option<Vec<u8>>` fields with `#[serde(skip, default)]` on `PendingDkg`/`PendingSignSession` to stash local FROST secret state between IPC calls. Security posture mirrors `PendingDkg::round2_packages`.

## Out of scope (still deferred)

- **Zenoh transport wiring** for `dfrost_logs` (broadcast over `harmony/community/{id}/dfrost`) — will fold into Phase 4a-main per ticket scope.
- **UI** for any of the IPC commands — Phase 4a-main owns the UI layer (committee proposal panel, beacon-ready notifications, refresh-progress modal).
- **Persistence** for `dfrost_logs` — still in-memory only; persistence design is a separate concern.

## Test plan

- [x] `cargo fmt --all -- --check` clean
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` all green
- [x] `npx tsc --noEmit` clean
- [x] `npx vitest run` all green
- [x] DKG IPC round-trip test passes
- [x] Threshold-sign IPC round-trip test passes
- [x] Refresh IPC round-trip test passes
- [ ] Bot reviewers (CodeRabbit, Cursor Bugbot, etc.) sign off

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 4: Confirm PR opened**

```bash
gh pr view --json url,state,mergeable,mergeStateStatus
```

Expected: `state: OPEN`, eventual `mergeable: MERGEABLE`, eventual `mergeStateStatus: CLEAN`.

---

## Post-plan handoff

After Task 11 succeeds, the calling agent enters the autonomous bot-review monitoring loop per `feedback_autonomous_pr_monitoring_loop` and `feedback_no_askuserquestion_for_pr_loop_mode`: ScheduleWakeup at ~1200s, address findings via fixup commits, converge, pushover at merge-ready.
