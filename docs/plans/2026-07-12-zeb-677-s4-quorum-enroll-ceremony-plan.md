# ZEB-677 S4: Quorum enrollment ceremony (pre-armed window) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An owner without their master seed can enroll a NEW device when K=2 active sibling devices (holding Master-issued certs) participate: one sibling pre-arms a 15-minute window, another runs normal SAS pairing as the inviter, and the armed sibling auto-co-signs the quorum enrollment cert — completing the lost-master enrollment story.

**Architecture:** The `owner-quorum-req-v1` FleetSync dataset (S3) already carries the `EnrollArm` cell (storage/merge/expiry done) and the `QuorumRequest` CRDT. S4 adds: (1) `arm`/`disarm` IPCs that write the arm cell; (2) a `QuorumRequestKind::Enrollment` variant; (3) a **B-side auto-co-sign pass** in the sweep (react to an enrollment request while holding a live arm → sign + mint a `VouchingCert{Vouch}` + consume the arm); (4) a narrow **`QuorumEnrollPort`** async trait the pairing SM depends on, whose real impl writes the request and awaits B's co-sign (via an `Arc<Notify>` fired on merge) then assembles the cert; (5) a new bounded pairing-SM state **`AwaitingQuorumCosign`** (120s) at the inviter signing decision point; (6) **joiner-side auto-vouches** minted after ENROLL receipt; (7) a **DevicesPanel arm surface** with countdown + honesty copy.

**Tech Stack:** Rust (Tauri IPC + tokio + `harmony-owner` crate rev `1ecb4160`), FleetSyncEngine CRDT sync over Zenoh/iroh, Svelte 5 + vitest frontend.

## Global Constraints

Copied verbatim from `docs/specs/2026-07-12-zeb-677-quorum-wiring-design.md` (§0/§2/§5/§8/§10) and `CLAUDE.md`. Every task's requirements implicitly include these.

- **Crate:** `harmony-owner` git dep, resolved rev **`1ecb4160`** (`src-tauri/Cargo.toml:104,109`, `Cargo.lock:3041`). Read-only reference checkout: `/Users/zeblith/.cargo/git/checkouts/harmony-6e325dd2bc445c08/1ecb416/crates/harmony-owner/`. Do NOT edit the crate (S1 already merged its APIs as harmony#285).
- **K=2 fixed, depth-1:** quorum signers MUST hold Master-issued certs. No quorum-signs-quorum chains. K is not configurable.
- **Quorum-enrolled certs mint `expires_at: None`** (the crate's active-window governs liveness, not cert expiry).
- **Single-use arm:** the arm window is **15 minutes**, **single-use** — auto-disarms on first co-sign or expiry. The UI honesty copy commits to "window auto-closes after one use" (§8), so single-use MUST survive CRDT merge races (see Task 2's fresh-Hlc-expired-cell rule).
- **SM timeout:** the `AwaitingQuorumCosign` state is bounded at **120 s**.
- **Trust threshold:** `N_VOUCH_THRESHOLD_V1 = 1` (`trust.rs:6`) — one sibling vouch lifts the joiner Provisional→Full.
- **Vouch directions:** B (co-signer) mints `VouchingCert{Vouch}` FOR the new device (lifts it to Full). The joiner mints its OWN auto-vouches FOR each active sibling (`enroll_via_quorum` direction — only the joiner holds its key).
- **Honesty copy (§8):** arm surface says "For the next 15 minutes this device will approve one new device enrollment started from your other devices"; the affordance is hidden when the fleet lacks 2 active Master-certed devices → "This fleet can no longer manage devices without the recovery phrase". No in-app claim of universal peer acceptance.
- **Tauri IPC naming:** Rust params `snake_case`, JS callers pass `camelCase`; Tauri auto-converts at the boundary. Register every IPC in BOTH the Tauri handler list (`lib.rs`) and the RPC mirror (`api/rpc.rs`).
- **IPC error extraction (TS):** `const msg = e instanceof Error ? e.message : String(e)`.
- **Gates (CLAUDE.md), all from `src-tauri/` for cargo, repo root for frontend:**
  - fmt: `cargo fmt --all -- --check`
  - clippy: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - nextest: `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (iterative dev may use `scripts/test-select --context task`; final sweep is the full command)
  - tsc: `npx tsc --noEmit`; vitest: `npx vitest run`
- **Commit trailers on this branch:**
  ```
  Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc
  ```

---

## File Structure

| File | Responsibility | S4 change |
| --- | --- | --- |
| `src-tauri/src/owner_quorum_sync.rs` | quorum-req dataset: types, merge, sweep | `QuorumRequestKind::Enrollment` variant; `enrollment_quorum_payload` helper; B-side `try_cosign_enrollment` pass in `run_quorum_sweep`; `Arc<Notify>` apply-signal plumbing; fix 3 irrefutable-let sites |
| `src-tauri/src/owner_quorum_commands.rs` | quorum IPCs + planners | `arm_quorum_enrollment` / `disarm_quorum_enrollment` IPCs (`_inner`/`_impl`/command); `write_arm_cell` helper; fix 1 irrefutable-let site if present |
| `src-tauri/src/owner_quorum_enroll.rs` (**new**) | A-side ceremony port | `QuorumEnrollPort` trait + `LiveQuorumEnrollPort` impl (`open_enrollment_request` + `await_cosign_and_assemble`) |
| `src-tauri/src/pairing/types.rs` | pairing state enum | `PairingState::AwaitingQuorumCosign` variant |
| `src-tauri/src/pairing/state_machine.rs` | pairing SM | `AwaitingQuorumCosign` branch at signing point; 120s spawn-timeout-channel-back; `StartInviter.quorum_ctx` + `SessionCtx` fields; joiner-side auto-vouch hook after ENROLL install |
| `src-tauri/src/pairing_commands.rs` | inviter/joiner IPC entry | seed-absent branch: build `LiveQuorumEnrollPort`, pass `master_seed: None` + `quorum_ctx` |
| `src-tauri/src/owner_commands.rs` | owner-state view | `can_arm_enrollment: bool` on `OwnerStateView` |
| `src-tauri/src/lib.rs` | wiring | register 2 new IPCs; construct + share the `Arc<Notify>`; pass quorum handles to `pairing_commands` |
| `src-tauri/src/api/rpc.rs` | RPC mirror | 2 new `rpc!` blocks + name-list assertion |
| `src/lib/owner-service.ts` | frontend owner service | `canArmEnrollment?` field; `armEnrollment()`/`disarmEnrollment()` methods |
| `src/lib/pairing-service.ts` | frontend pairing state | `AwaitingQuorumCosign` mirror |
| `src/lib/components/DevicesPanel.svelte` | device UI | arm affordance + countdown (`setInterval`) + honesty copy |
| `src/lib/components/PairingInviter.svelte` | inviter UI | `AwaitingQuorumCosign` status copy |

---

## Task ordering & execution note

Task 1 (the enum variant) must land first — it makes the doc compile with both request kinds and forces the irrefutable-let fixes everything else builds on. Tasks 2–3 (arm IPCs + B-side co-sign) are the B-side data plane. Task 4 (port) + Task 5 (SM) are the A-side, tightly coupled — implement inline, not via a fresh subagent, because they thread new types (`QuorumEnrollPort`, `QuorumEnrollCtx`) across module boundaries. Task 6 (joiner vouches), Task 7 (UI), Task 8 (integration tests) follow. Peripheral mechanical tasks (1, 7's service methods) are subagent-friendly; the SM/port core is not.

---

### Task 1: `QuorumRequestKind::Enrollment` variant + irrefutable-let fixes

**Files:**
- Modify: `src-tauri/src/owner_quorum_sync.rs:105-117` (enum), `:364`, `:600` (irrefutable-lets), `src-tauri/src/owner_commands.rs:449` (irrefutable-let)
- Test: inline `#[cfg(test)] mod tests` in `owner_quorum_sync.rs`

**Interfaces:**
- Produces: `QuorumRequestKind::Enrollment { joiner_device_id_hex: String, joiner_pubkeys_cbor_hex: String }` — consumed by Tasks 3, 4.

- [ ] **Step 1: Write the failing test** — round-trip serde of the Enrollment variant, in `owner_quorum_sync.rs` tests:

```rust
#[test]
fn enrollment_request_kind_serde_round_trips() {
    let kind = QuorumRequestKind::Enrollment {
        joiner_device_id_hex: "ab".repeat(8),
        joiner_pubkeys_cbor_hex: "cc".repeat(4),
    };
    let bytes = crate::owner_state_crypto::canonical_cbor_encode(&kind).expect("encode");
    let back: QuorumRequestKind =
        crate::owner_state_crypto::canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(kind, back);
}
```

- [ ] **Step 2: Run it — expect a COMPILE failure** (variant does not exist yet):

`cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(enrollment_request_kind_serde_round_trips)'` → Expected: does not compile ("no variant named `Enrollment`").

- [ ] **Step 3: Add the variant.** In `owner_quorum_sync.rs:106-117`, add after the `Revocation` arm (serde rename `"n"` — `r`/`e`/`t`/`u`/`x` etc. are taken; pick unused single letters):

```rust
    /// S4 enrollment ceremony: the initiator asks its armed sibling to
    /// co-sign a quorum enrollment cert for a newly-paired device. The
    /// joiner's device id + pubkey bundle are fixed in the payload the
    /// signers cover (`enrollment_quorum_payload`).
    #[serde(rename = "n")]
    Enrollment {
        /// Hex of the joiner's 16-byte device id.
        #[serde(rename = "j")]
        joiner_device_id_hex: String,
        /// CBOR-hex of the joiner's `PubKeyBundle` (the enrolled key set).
        #[serde(rename = "b")]
        joiner_pubkeys_cbor_hex: String,
    },
```

- [ ] **Step 4: Fix the 3 irrefutable-let sites** — each `let QuorumRequestKind::Revocation { .. } = &req.kind;` becomes a guard that skips non-Revocation requests (these paths are revocation-completion only):
  - `owner_quorum_sync.rs:364` (in `prune_settled_requests`): change to
    ```rust
    let QuorumRequestKind::Revocation { target_hex, .. } = &req.kind else {
        return true; // enrollment requests: keep unless TTL-expired (handled above)
    };
    ```
  - `owner_quorum_sync.rs:600` (in `try_assemble`, revocation-only): change to
    ```rust
    let QuorumRequestKind::Revocation { reason, target_hex } = &req.kind else {
        return None; // enrollment assembly is A-side (owner_quorum_enroll), not the sweep
    };
    ```
  - `owner_commands.rs:449`: read the site first; if it destructures for the view, convert to `match`/`if let` emitting the revocation display and a distinct enrollment display (or skip enrollment rows if the view doesn't surface them yet — Task 7 decides). Minimal: `if let QuorumRequestKind::Revocation { .. } = &req.kind { .. }`.

- [ ] **Step 5: Run the test + clippy** — `cargo nextest run --locked --features test-fixtures -E 'test(enrollment_request_kind_serde_round_trips)'` (PASS) and `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` (no `irrefutable_let_patterns` / unreachable warnings).

- [ ] **Step 6: Commit** — `feat(zeb-677-s4): add QuorumRequestKind::Enrollment variant`.

---

### Task 2: arm / disarm IPCs + `can_arm_enrollment` view field

**Files:**
- Modify: `src-tauri/src/owner_quorum_commands.rs` (add IPCs near `:519-551`), `src-tauri/src/lib.rs` (handler list `~:57186`, RPC handles `~:5727`), `src-tauri/src/api/rpc.rs` (`~:981-1012`, `~:1954`), `src-tauri/src/owner_commands.rs` (`build_owner_state_view` `~:491`)
- Test: `owner_quorum_commands.rs` tests + `owner_commands.rs` view tests

**Interfaces:**
- Consumes: `EnrollArm` (`owner_quorum_sync.rs:173`), `Hlc` (verify its constructor/`now` API when implementing).
- Produces: IPCs `arm_quorum_enrollment() -> Result<u64, String>` (returns `armed_until_ms`), `disarm_quorum_enrollment() -> Result<(), String>`; `OwnerStateView.can_arm_enrollment: bool`.

**Single-use / disarm rule (binding — see Global Constraints):** disarm and B-side consume MUST NOT delete the `enroll_arms[self]` cell — a delete can be resurrected when an older `set_at` re-merges (LWW resurrection). Instead **write a fresh-Hlc `EnrollArm { set_at: <new Hlc>, armed_until_ms: <= now }`**. Because `set_at` is a monotonic `Hlc`, the merge at `owner_quorum_sync.rs:328-333` (`is_strictly_newer_than`) always picks the disarm over the earlier arm; prune (`:380`) reaps it after the horizon.

- [ ] **Step 1: Write the failing test** — arm writes a future cell; disarm supersedes it with an expired one. Model on the S3 IPC `_inner` tests (two-engine fixture in `owner_quorum_commands.rs` tests):

```rust
#[tokio::test]
async fn arm_then_disarm_supersedes_with_expired_cell() {
    let f = QuorumFixture::new(); // reuse the S3 test fixture builder
    let armed_until = arm_quorum_enrollment_inner(&f.quorum_doc, &f.quorum_engine, f.a_id, f.now_ms())
        .await
        .expect("arm");
    assert!(armed_until > f.now_ms());
    {
        let doc = f.quorum_doc.lock().await;
        let arm = doc.enroll_arms.get(&hex::encode(f.a_id)).expect("armed");
        assert!(arm.armed_until_ms > f.now_ms());
    }
    disarm_quorum_enrollment_inner(&f.quorum_doc, &f.quorum_engine, f.a_id, f.now_ms())
        .await
        .expect("disarm");
    {
        let doc = f.quorum_doc.lock().await;
        let arm = doc.enroll_arms.get(&hex::encode(f.a_id)).expect("cell present (superseded, not deleted)");
        assert!(arm.armed_until_ms <= f.now_ms(), "disarm writes an already-expired cell");
    }
}
```

- [ ] **Step 2: Run it — expect COMPILE failure** (`arm_quorum_enrollment_inner` undefined). If the S3 fixture lacks helpers (`now_ms`, `a_id`), extend it minimally.

- [ ] **Step 3: Implement `write_arm_cell` + the two `_inner` fns** in `owner_quorum_commands.rs`. `ARM_WINDOW_MS = 15 * 60 * 1000`.

```rust
use crate::owner_quorum_sync::{EnrollArm, QuorumReqDoc};

pub(crate) const ARM_WINDOW_MS: u64 = 15 * 60 * 1000;

/// Write (or supersede) THIS device's arm cell with a fresh Hlc so the
/// merge's LWW always prefers it. `armed_until_ms <= now` ⇒ disarmed.
async fn write_arm_cell(
    quorum_doc: &Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    quorum_engine: &Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
    self_id: [u8; 16],
    armed_until_ms: u64,
) {
    {
        let mut doc = quorum_doc.lock().await;
        // `Hlc::now`/tick API: verify the exact constructor in owner_state_types
        // at implementation (the doc already stamps `created_at`/`set_at`).
        let set_at = crate::owner_state_types::Hlc::now();
        doc.enroll_arms
            .insert(hex::encode(self_id), EnrollArm { set_at, armed_until_ms });
    }
    quorum_engine.notify_dirty();
    let _ = quorum_engine.flush_now().await; // warn-only: replicated doc + dirty latch is the durability boundary (S3 convention)
}

pub async fn arm_quorum_enrollment_inner(
    quorum_doc: &Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    quorum_engine: &Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
    self_id: [u8; 16],
    now_ms: u64,
) -> Result<u64, String> {
    let armed_until = now_ms.saturating_add(ARM_WINDOW_MS);
    write_arm_cell(quorum_doc, quorum_engine, self_id, armed_until).await;
    Ok(armed_until)
}

pub async fn disarm_quorum_enrollment_inner(
    quorum_doc: &Arc<tokio::sync::Mutex<QuorumReqDoc>>,
    quorum_engine: &Arc<crate::fleet_sync::FleetSyncEngine<QuorumReqDoc>>,
    self_id: [u8; 16],
    now_ms: u64,
) -> Result<(), String> {
    // Supersede with an already-expired cell (never delete).
    write_arm_cell(quorum_doc, quorum_engine, self_id, now_ms.saturating_sub(1)).await;
    Ok(())
}
```

- [ ] **Step 4: Add `_impl` seams + `#[tauri::command]` wrappers** mirroring the S3 revocation IPCs (`owner_quorum_commands.rs:493-551`). The command wrappers resolve `self_id` + `now_ms` from `NodeState` exactly as the S3 commands do (read the S3 wrappers for the exact `state`/`sink` plumbing and the self-device-id source). Eligibility guard: reject with `hasMaster:` when `master_seed.is_some()` (a master-holding device uses normal pairing), and `notEligible:` when this device lacks a Master-issued cert. Reuse `is_master_issued` (`:46`).

- [ ] **Step 5: Register both IPCs** — add `arm_quorum_enrollment, disarm_quorum_enrollment` to the Tauri handler list (`lib.rs:~57186-57188`) and add two `rpc!` blocks + the name-list assertion entry in `api/rpc.rs:~981-1012` / `:1954` (nullary args — reuse the empty-args struct pattern the S3 IPCs use, or a fresh `struct ArmEnrollmentArgs {}`).

- [ ] **Step 6: Add `can_arm_enrollment` to the view.** In `build_owner_state_view` (`owner_commands.rs`), after the existing `self_is_master`/`self_master_certed`/`active_master_certed` computation (`:337-356`), add:

```rust
// S4: can this device arm an enrollment window? Only master-less fleets
// use quorum enrollment; needs THIS device Master-certed + ≥1 OTHER active
// Master-certed sibling to act as the inviter.
let can_arm_enrollment = !self_is_master
    && self_master_certed
    && active_master_certed.iter().any(|id| *id != self_device_id);
```

and emit it in the `OwnerStateView { .. }` literal (`:491-501`). Add the field to the `OwnerStateView` Rust struct (serde `camelCase` → `canArmEnrollment`).

- [ ] **Step 7: Write a view test** — `can_arm_enrollment` is true iff master-less + self master-certed + another active master-certed sibling; false when self holds the master, when self isn't master-certed, and when no eligible sibling. Extend the S3 `quorum_view_fixture` (`owner_commands.rs` tests).

- [ ] **Step 8: Run tests + clippy + fmt.** `scripts/test-select --context task` covering the two modules, then the named tests.

- [ ] **Step 9: Commit** — `feat(zeb-677-s4): arm/disarm enrollment IPCs + canArmEnrollment view (single-use via fresh-Hlc supersede)`.

---

### Task 3: B-side auto-co-sign pass (sign + vouch + consume arm)

**Files:**
- Modify: `src-tauri/src/owner_quorum_sync.rs` (`run_quorum_sweep` `:678`, add `try_cosign_enrollment` + `enrollment_quorum_payload`)
- Test: two-engine test in `owner_quorum_sync.rs` tests

**Interfaces:**
- Consumes: `QuorumRequestKind::Enrollment` (Task 1); crate `EnrollmentCert::{quorum_signing_payload_bytes, sign_quorum_part}`, `VouchingCert::sign`, `Stance::Vouch`, `PubKeyBundle`.
- Produces: side effects on the quorum doc (self signature unioned) + trust doc (VouchingCert) + arm cell (consumed).

**Semantics:** B (holding a live arm, NOT the initiator, not yet co-signed) reacts to an `Enrollment` request: verifies the initiator's authenticating signature (`initiator_sigs[self]`) over the enrollment payload against the initiator's enrolled Master key; signs the same payload (`signatures[self].primary_sig_hex`); mints `VouchingCert{Vouch}` for the joiner and applies it via `add_vouching`; then consumes its arm (fresh-Hlc expired cell). The arm is the consent — no manual step (spec §5.2). Lock discipline mirrors the revocation sweep: collect under the quorum lock, mutate the trust doc under the trust lock, never both at once.

- [ ] **Step 1: Write `enrollment_quorum_payload` helper** (deterministic, no I/O) — wraps the crate's payload fn so A and B build identical bytes:

```rust
/// Canonical bytes both quorum signers cover for an enrollment cert.
/// `signers` is the sorted 2-element `[initiator, cosigner]` set (K=2).
pub fn enrollment_quorum_payload(
    owner_id: [u8; 16],
    joiner_device_id: [u8; 16],
    joiner_pubkeys: &harmony_owner::pubkey_bundle::PubKeyBundle,
    issued_at: u64,
    signers: &[[u8; 16]],
) -> Result<Vec<u8>, String> {
    harmony_owner::certs::enrollment::EnrollmentCert::quorum_signing_payload_bytes(
        owner_id,
        joiner_device_id,
        joiner_pubkeys,
        issued_at,
        None, // quorum certs mint expires_at: None (Global Constraints)
        signers,
    )
    .map_err(|e| format!("enrollment quorum payload: {e}"))
    // NOTE: verify the exact crate path/signature of quorum_signing_payload_bytes
    // at implementation (certs/enrollment.rs:187-206, rev 1ecb416).
}
```

- [ ] **Step 2: Write the failing two-engine test** — A writes an Enrollment request addressed to B; B's sweep auto-co-signs, vouches, and consumes its arm:

```rust
#[tokio::test]
async fn b_auto_cosigns_enrollment_when_armed() {
    let f = QuorumFixture::two_master_devices(); // A + B both master-certed, enrolled
    // B arms:
    arm_quorum_enrollment_inner(&f.b_quorum_doc, &f.b_quorum_engine, f.b_id, f.now_ms()).await.unwrap();
    f.sync_quorum().await; // A sees B's arm

    // A writes an Enrollment request for a fresh joiner, authenticated to B:
    let (joiner_id, joiner_pk, _joiner_sk) = f.fresh_joiner();
    let request_id = f.write_enrollment_request(f.a_id, joiner_id, &joiner_pk, /*cosigner*/ f.b_id).await;
    f.sync_quorum().await; // B sees the request

    // B's sweep pass runs:
    run_quorum_sweep(/* B's handles */).await;

    // B unioned its signature:
    {
        let doc = f.b_quorum_doc.lock().await;
        let req = doc.requests.get(&request_id).expect("request");
        assert!(req.signatures.contains_key(&hex::encode(f.b_id)), "B co-signed");
    }
    // B minted a Vouch for the joiner:
    {
        let trust = f.b_trust_doc.lock().await;
        assert!(trust.vouches_for(joiner_id).iter().any(|v| /* signer == b_id && Vouch */ true));
    }
    // B consumed its arm (fresh-Hlc expired cell):
    {
        let doc = f.b_quorum_doc.lock().await;
        let arm = doc.enroll_arms.get(&hex::encode(f.b_id)).expect("cell");
        assert!(arm.armed_until_ms <= f.now_ms(), "arm consumed");
    }
}
```

(The fixture helpers `two_master_devices`, `sync_quorum`, `fresh_joiner`, `write_enrollment_request`, `vouches_for` extend the S3 test fixture — build them minimally against the real crate API. Verify `OwnerState`'s vouch accessor name at implementation.)

- [ ] **Step 3: Run it — expect COMPILE/assert failure** (co-sign pass not implemented).

- [ ] **Step 4: Implement `try_cosign_enrollment`** and call it inside `run_quorum_sweep`'s Phase A loop (a NEW branch, parallel to the initiator-only assembly). For each request where `req.initiator_hex != self_hex`, `kind == Enrollment`, `!req.signatures.contains_key(self_hex)`, and self holds a live arm (`enroll_arms[self_hex]` with `armed_until_ms > now_ms`):
  1. Decode joiner id + `PubKeyBundle` from the Enrollment kind.
  2. `signers = sort([initiator, self_id])`; `payload = enrollment_quorum_payload(owner, joiner_id, &joiner_pk, req.issued_at, &signers)`.
  3. Verify `initiator_sigs[self_hex]` against the initiator's enrolled Master key over `payload` (reuse `verify_with_tag` with the enrollment tag — confirm the crate's enrollment quorum tag name; revocation uses `tags::REVOCATION`/`"Revocation-Quorum-Part"`, enrollment likely `tags::ENROLLMENT`/an enrollment label). Skip on failure (unauthenticated request).
  4. `sig = EnrollmentCert::sign_quorum_part(device_signing_key, &payload)`; stage `signatures[self_hex] = QuorumRequestSigs { primary_sig_hex: hex(sig), epoch_doc_sig_hex: None }`.
  5. Stage a `VouchingCert::sign(device_signing_key, owner, joiner_id, Stance::Vouch, now_secs)`.
  6. Stage an arm-consume for `self_hex`.

  Collect these as a `Vec<EnrollmentCosign>` under the quorum lock (like `CompletionCandidate`). After releasing the quorum lock: apply each vouch via `mutate_trust_state`→`add_vouching` (trust lock), then re-take the quorum lock to union the signature + write the consumed arm cell, then `notify_dirty` + `flush_now`. Emit `owner-devices-updated` (vouch changed trust) + `owner-quorum-updated`.

- [ ] **Step 5: Extend `SweepOutcome`** if needed (e.g. `enrollment_cosigns: usize`) so the applied task's emit gate (`from_nudge || outcome.doc_changed`) accounts for co-signs. Keep `doc_changed` true when a co-sign happened.

- [ ] **Step 6: Run the test + full module tests + clippy + fmt.**

- [ ] **Step 7: Commit** — `feat(zeb-677-s4): B-side auto-co-sign + Vouch + arm consume in quorum sweep`.

---

### Task 4: `QuorumEnrollPort` trait + `LiveQuorumEnrollPort` (A-side completion)

**Files:**
- Create: `src-tauri/src/owner_quorum_enroll.rs`
- Modify: `src-tauri/src/lib.rs` (module decl + construct the `Arc<Notify>` apply-signal), `src-tauri/src/owner_quorum_sync.rs` (fire the `Notify` in the applied task)
- Test: unit tests in `owner_quorum_enroll.rs` (two-engine: open request → simulate B co-sign → assemble)

**Interfaces:**
- Produces:
  ```rust
  #[async_trait::async_trait]
  pub trait QuorumEnrollPort: Send + Sync {
      /// Build the enrollment quorum payload over the joiner, sign A's part,
      /// write an Enrollment request addressed to the chosen armed sibling,
      /// return the request id.
      async fn open_enrollment_request(
          &self, joiner_device_id: [u8; 16],
          joiner_pubkeys: harmony_owner::pubkey_bundle::PubKeyBundle,
          issued_at: u64,
      ) -> Result<String, String>;
      /// Await B's co-signature (bounded by `deadline`), then assemble the
      /// quorum `EnrollmentCert`. Prunes the request on success.
      async fn await_cosign_and_assemble(
          &self, request_id: String, timeout: std::time::Duration,
      ) -> Result<harmony_owner::certs::enrollment::EnrollmentCert, String>;
  }
  ```
  Consumed by Task 5 (the SM holds `Arc<dyn QuorumEnrollPort>`).
- Consumes: crate `EnrollmentCert::{sign_quorum_part, assemble_quorum}`; the `Arc<Notify>` fired on each quorum `on_applied`.

**Apply-signal:** add an `Arc<tokio::sync::Notify>` fired in the quorum `on_applied` hook (alongside the existing nudge). `LiveQuorumEnrollPort` holds a clone; `await_cosign_and_assemble` loops `tokio::select! { _ = notify.notified() => .., _ = sleep(remaining) => timeout }`, re-checking `signatures` count each wake. The sweep task and the port both observe the same signal — no polling.

- [ ] **Step 1: Add the `Notify` to the wiring.** In `lib.rs` where the quorum `on_applied` is set (`~:5701`) and the applied task is spawned (`~:5727`), construct `let quorum_applied_notify = Arc::new(tokio::sync::Notify::new());`. In the `on_applied` closure (or in the applied task after each sweep), call `quorum_applied_notify.notify_waiters();`. Store a clone in `NodeState` (or pass to `pairing_commands`) so the port can be built. Verify the exact `on_applied`/`ingest_nudge_on_applied` closure shape at implementation.

- [ ] **Step 2: Write the failing port test** — `open_enrollment_request` writes an authenticated request; after a simulated B co-sign is merged in + `notify_waiters`, `await_cosign_and_assemble` returns a cert that `verify_quorum_with_signers` accepts:

```rust
#[tokio::test]
async fn port_opens_request_and_assembles_after_cosign() {
    let f = QuorumFixture::two_master_devices();
    let port = LiveQuorumEnrollPort::new(/* A's handles + notify */);
    let (joiner_id, joiner_pk, _sk) = f.fresh_joiner();
    let rid = port.open_enrollment_request(joiner_id, joiner_pk.clone(), f.now_secs()).await.unwrap();

    // Simulate B co-signing (reuse Task 3's write path) + fire the notify:
    f.simulate_b_cosign(&rid).await;
    f.quorum_applied_notify.notify_waiters();

    let cert = port.await_cosign_and_assemble(rid, Duration::from_secs(5)).await.unwrap();
    let signers = f.signer_certs_for(&cert); // A + B master enrollment certs
    cert.verify_quorum_with_signers(&signers, f.now_secs()).expect("valid quorum cert");
}

#[tokio::test]
async fn port_times_out_without_cosign() {
    let f = QuorumFixture::two_master_devices();
    let port = LiveQuorumEnrollPort::new(/* .. */);
    let (jid, jpk, _) = f.fresh_joiner();
    let rid = port.open_enrollment_request(jid, jpk, f.now_secs()).await.unwrap();
    let err = port.await_cosign_and_assemble(rid, Duration::from_millis(50)).await.unwrap_err();
    assert!(err.contains("timeout") || err.contains("co-sign"));
}
```

- [ ] **Step 3: Implement `LiveQuorumEnrollPort`.** Holds `quorum_doc`, `quorum_engine`, `trust_doc` (for signer certs), `device_signing_key`, `self_id`, `quorum_applied_notify`. `open_enrollment_request`: pick the armed sibling (first live `enroll_arms` entry ≠ self that is active + Master-certed), build `signers = sort([self, sibling])`, `payload = enrollment_quorum_payload(..)`, `initiator_sig = sign_quorum_part(key, &payload)`, write a `QuorumRequest { kind: Enrollment{..}, initiator_hex: self, initiator_sigs: {sibling: hex(initiator_sig)}, signatures: {}, issued_at, expires_at_ms: now_ms + ARM/TTL, created_at: Hlc::now(), declined_by: {} }`; `notify_dirty` + `flush_now`; return id. `await_cosign_and_assemble`: loop on the notify with an overall deadline; on each wake re-read the request, find a `signatures[cosigner]` that verifies over `payload`, then `parts = sort([(self, initiator_sig), (cosigner, cosig)])`, `assemble_quorum(owner, joiner_id, joiner_pk, issued_at, None, parts)`; prune the request; return the cert.

- [ ] **Step 4: Run both tests + clippy + fmt.**

- [ ] **Step 5: Commit** — `feat(zeb-677-s4): QuorumEnrollPort — open request + await co-sign + assemble (A-side)`.

---

### Task 5: `AwaitingQuorumCosign` pairing-SM state + inviter branch

**Files:**
- Modify: `src-tauri/src/pairing/types.rs:36-60` (state enum), `src-tauri/src/pairing/state_machine.rs` (`PairingCommand::StartInviter` `:36`, `SessionCtx` `:310`, `start_inviter` `:406`, decision point `:746-773`, main `select!` `:184`), `src-tauri/src/pairing_commands.rs:50-70` (seed-absent branch)
- Test: `state_machine.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `QuorumEnrollPort` (Task 4); the existing `sign_enrollment_for_joiner` master path stays for the seed-present case.
- Produces: `PairingState::AwaitingQuorumCosign` (frontend mirror in Task 7); `StartInviter.master_seed: Option<Zeroizing<[u8;32]>>` + `StartInviter.quorum_ctx: Option<Arc<dyn QuorumEnrollPort>>`.

**Decision-point logic** (`state_machine.rs:746-773`): the two `.expect()` become fallible. If `ctx.master_seed.is_some()` → existing master path unchanged. Else if `ctx.quorum_enroll.is_some()` → emit `AwaitingQuorumCosign`, then (donor: the `PERSIST_ACK_TIMEOUT` spawn-timeout-channel-back at `:980`) spawn a task that calls `port.open_enrollment_request(joiner_id, joiner_pk, now)` then `port.await_cosign_and_assemble(rid, Duration::from_secs(120))`, forwarding `Result<EnrollmentCert, String>` to a new `mpsc` whose receiver has a select arm (model on the `persist_done_rx` arm at `:261-280`) that — only if still `AwaitingQuorumCosign` in the same session — runs `add_enrollment` into `prospective_state` and continues to the existing ENROLL publish (`:855-861`). On `Err`/timeout → `PairingState::Failed { reason: "Your other device didn't co-sign in time — re-arm and retry" }` and clean teardown (existing failure idiom). Else (no seed, no port) → `Failed` with the master-absent copy.

- [ ] **Step 1: Add the state variant.** `types.rs`, after `Enrolling`: `AwaitingQuorumCosign,` (unit variant; serde `camelCase` → `awaitingQuorumCosign`).

- [ ] **Step 2: Write failing SM unit tests** (mock port — no real quorum engine), model on `happy_path_two_devices_pair` (`:1475`) with `ScriptedTransport`:
  - `inviter_enters_awaiting_quorum_when_seedless_with_port`: `StartInviter { master_seed: None, quorum_ctx: Some(mock_port_that_returns_cert), .. }`, drive to both-confirmed, assert the SM passes through `AwaitingQuorumCosign` then reaches `Enrolling`/publishes ENROLL with a Quorum cert.
  - `inviter_fails_on_quorum_timeout`: mock port whose `await_cosign_and_assemble` sleeps past 120s (use a short injected timeout or `tokio::time` pause) → `Failed`.
  - `inviter_fails_seedless_without_port`: `master_seed: None, quorum_ctx: None` → `Failed` with master-absent copy.
  Mock: `struct MockPort { outcome: PortOutcome }` implementing `QuorumEnrollPort`.

- [ ] **Step 3: Run — expect COMPILE failures** (new fields/state).

- [ ] **Step 4: Thread the fields.** `StartInviter.master_seed` → `Option<Zeroizing<[u8;32]>>`; add `quorum_ctx: Option<Arc<dyn QuorumEnrollPort>>`. `SessionCtx` gains `master_seed: Option<..>` (already Option) and `quorum_enroll: Option<Arc<dyn QuorumEnrollPort>>`; `start_inviter` stores both. Implement the decision-point branch + the spawn-timeout-channel-back + the new select arm per the logic above. **`AwaitingQuorumCosign` must obey the ZEB-198 cancel-from-every-phase rule** (`:2373+`): a Cancel while awaiting tears the session down and drops the spawned task.

- [ ] **Step 5: Update `pairing_commands.rs:50-70`.** Replace the hard `ok_or_else` seed failure with a branch: seed present → `StartInviter { master_seed: Some(seed), quorum_ctx: None, .. }`. Seed absent → check `can_arm`-equivalent (self Master-certed, an armed sibling exists); if eligible, build `LiveQuorumEnrollPort` from `NodeState`'s quorum handles + the shared `Notify`, send `StartInviter { master_seed: None, quorum_ctx: Some(Arc::new(port)), .. }`; if not eligible, return the existing master-absent error string (so the UI is unchanged for genuinely stuck fleets).

- [ ] **Step 6: Run SM tests + clippy (`--all-targets`) + fmt.**

- [ ] **Step 7: Commit** — `feat(zeb-677-s4): AwaitingQuorumCosign SM state + seedless inviter branch`.

---

### Task 6: Joiner-side auto-vouches after ENROLL receipt

**Files:**
- Modify: `src-tauri/src/pairing/state_machine.rs:1228+` (joiner ENROLL handler)
- Test: `state_machine.rs` tests (extend the seedless happy-path to assert the joiner minted sibling vouches)

**Interfaces:**
- Consumes: `enroll_via_quorum`'s joiner-vouch direction (`enroll_quorum.rs:106-118`): the joiner's `new_device_sk` signs `VouchingCert{Vouch}` for each active sibling ≠ self.
- Produces: joiner's `auto_vouch_certs` written into the installed owner state so they ride trust-sync.

**Semantics (spec §5.4):** after the joiner installs the enrollment cert + owner_state (`:1229+`), IF the installed cert is `EnrollmentIssuer::Quorum`, the joiner mints a `VouchingCert{Vouch}` for each active sibling (excluding itself) signed with its own device key, and applies them via `add_vouching`, so trust-sync replicates them. (Master-issued enrollment skips this — normal pairing already ratifies.) The joiner already holds its device signing key from the pairing handshake; locate it in the joiner ctx.

- [ ] **Step 1: Write the failing assertion** into the seedless happy-path SM test: after Complete, the joiner's installed owner state carries a `VouchingCert{Vouch}` from the joiner FOR each active sibling.

- [ ] **Step 2: Implement.** In the joiner ENROLL arm, after `add_enrollment` of the received cert, branch on `matches!(cert.issuer, EnrollmentIssuer::Quorum { .. })`: for each `sibling in installed_state.active_devices(now, window)` where `sibling != joiner_id`, `let v = VouchingCert::sign(&joiner_sk, owner_id, sibling, Stance::Vouch, now)?; installed_state.add_vouching(v)?;`. Persist via the same path the ENROLL handler already uses to persist the installed state. (Alternatively call `enroll_via_quorum` to get `auto_vouch_certs` directly — but that re-runs enrollment; prefer minting the vouches directly against the already-installed state to avoid double-enroll.)

- [ ] **Step 3: Run tests + clippy + fmt. Commit** — `feat(zeb-677-s4): joiner mints sibling auto-vouches on quorum enroll`.

---

### Task 7: UI arm surface + pairing status + service methods

**Files:**
- Modify: `src/lib/owner-service.ts` (types + methods), `src/lib/pairing-service.ts` (state mirror), `src/lib/components/DevicesPanel.svelte` (arm affordance + countdown), `src/lib/components/PairingInviter.svelte` (status copy)
- Test: `src/lib/components/__tests__/DevicesPanel.test.ts`

**Interfaces:**
- Consumes: `OwnerStateView.canArmEnrollment` (Task 2), `OwnerStateView.quorumArmedUntilMs` (already emitted, S3), IPCs `arm_quorum_enrollment`/`disarm_quorum_enrollment`.

- [ ] **Step 1: `owner-service.ts`** — add `canArmEnrollment?: boolean` to `OwnerStateView` (`:3-33`); add methods (model on `requestQuorumRevocation` `:160`):
  ```ts
  async armEnrollment(): Promise<number> { return this.invoke<number>('arm_quorum_enrollment', {}); }
  async disarmEnrollment(): Promise<void> { await this.invoke<void>('disarm_quorum_enrollment', {}); }
  ```
- [ ] **Step 2: `pairing-service.ts`** — add the `awaitingQuorumCosign` case to the frontend `PairingState` union + any status-label map (mirror `WaitingPeerConfirm`). `PairingInviter.svelte` — render "Waiting for your other device to approve…" when in that state.
- [ ] **Step 3: Write failing vitest** in `DevicesPanel.test.ts` (model on the S3 quorum-banner tests):
  - arm affordance renders (`data-testid="quorum-arm-button"`) when `canArmEnrollment` true + not armed; hidden when false.
  - when `quorumArmedUntilMs` is a future ts, a countdown (`data-testid="quorum-arm-countdown"`) + Cancel render; clicking Cancel calls `disarm_quorum_enrollment`.
  - honesty copy string present when armed ("approve one new device enrollment").
- [ ] **Step 4: Implement the DevicesPanel block** alongside the S3 banners (`~:841-891`), reusing `quorumActionInFlight`/`quorumError`. Add a client `setInterval` (1s) to re-render the countdown from `state.quorumArmedUntilMs` (a static backend ts — the countdown DISPLAYS remaining time but the authority is the backend cell); clear it in `onDestroy`. The existing `owner-quorum-updated` listener (`:481-514`) already refreshes on arm/disarm/consume — no new listener. Click-confirm tier (reversible action).
- [ ] **Step 5: Run `npx tsc --noEmit` + `npx vitest run`. Commit** — `feat(zeb-677-s4): DevicesPanel arm surface + countdown + pairing status`.

---

### Task 8: Two-engine enrollment ceremony integration test

**Files:**
- Test: new `src-tauri/tests/quorum_enroll_ceremony.rs` OR extend an existing two-engine harness (survey: donor is the trust-sync two-engine tests + `owner_quorum_commands.rs` fixtures)

**Interfaces:**
- Consumes: the full A→B→assemble→enroll path across two real `FleetSyncEngine` instances.

- [ ] **Step 1: Write the happy-path ceremony test** — B arms; A (seedless, via the port) opens an Enrollment request for a fresh joiner; sync; B's sweep auto-co-signs + vouches + consumes; sync; A's port assembles the cert; `add_enrollment` accepts it into A's prospective state; `verify_quorum_with_signers` passes with the [A,B] signer bundle; the joiner (given its key) mints sibling vouches and `evaluate_trust` reports the joiner **Full** (N=1 vouch from B). Assert the arm is consumed and cannot drive a second enrollment in-window.
- [ ] **Step 2: Write the decline/timeout variant** — no arm (B never arms) → A's port times out; request expires; no cert minted; joiner not enrolled.
- [ ] **Step 3: Run the full sweep** — `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (final CI-parity run, not test-select). Then fmt + clippy `--all-targets`. Commit — `test(zeb-677-s4): two-engine quorum enrollment ceremony (happy + timeout)`.

---

## Testing (from spec §10)

- **Pairing SM unit tests** (Task 5): `AwaitingQuorumCosign` co-sign-arrives / timeout / no-port, mock `QuorumEnrollPort`.
- **Two-engine ceremony** (Tasks 3, 8): request → auto-co-sign → assemble → enroll happy path; timeout; single-use arm (no second enrollment in-window); joiner Provisional→Full via B's vouch.
- **View gates** (Task 2): `canArmEnrollment` truth table.
- **UI** (Task 7): arm affordance render/hide, countdown, Cancel→disarm, honesty copy.
- **Serde** (Task 1): Enrollment variant round-trip; old-decoder tolerance (additive fields).

## Honesty ledger (§8) — copy this task touches

- Arm surface: "For the next 15 minutes this device will approve one new device enrollment started from your other devices." (window auto-closes after one use)
- Affordance hidden when the fleet lacks 2 active Master-certed devices — no false promise of quorum capability.
- No in-app claim that all peers accept quorum certs (release-notes concern, not this UI).

## Open risks resolved (for the reviewer)

1. **SM↔quorum coupling** → narrow `QuorumEnrollPort` trait; SM stays transport+port testable; real impl in `owner_quorum_enroll.rs`.
2. **Awaiting B's co-sign** → `Arc<Notify>` fired on quorum `on_applied`; port awaits it, no polling.
3. **`start_inviter` seedless** → branch in `pairing_commands.rs`; genuinely-stuck fleets keep the existing error string.
4. **New enum variant breaks irrefutable-lets** → Task 1 converts all 3 (+1 view site); clippy `--all-targets` backstops.
5. **Single-use arm race** → disarm/consume write a fresh-Hlc already-expired cell (never delete); monotonic Hlc wins LWW, defeating resurrection; honors the §8 single-use commitment.
6. **Countdown honesty** → countdown DISPLAYS remaining from the backend `quorumArmedUntilMs`; backend cell is the authority; `owner-quorum-updated` is the refresh signal.
