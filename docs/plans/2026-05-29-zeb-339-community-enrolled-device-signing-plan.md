# ZEB-339 — Community membership: enrolled-device signing + EnrollmentCert verification — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make community-membership events sign with the harmony-owner *enrolled device key* (#2) and verify the owner→device binding via the `EnrollmentCert`, so `actor = owner_id` stays correct end-to-end and `Create community` (plus redeem / leave / kick / unban / counter-sign / channel events / publisher-auth) stops failing with `ActorPubkeyMismatch`.

**Architecture:** Replace the flat `address_hash(signing_pubkey) == actor` check in three verify sites with cert-aware resolution: identity-introducing events (bootstrap Join / Join / PendingJoin) carry an `EnrollmentCert` proving `owner_id → device_ed25519`; steady-state events resolve the signer's device key from materialized membership (`MemberState.enrolled_device_keys`, learned from the Join cert). Signing switches from the Reticulum key (#3) to the enrolled `device_signing_key` (#2), carried at runtime on `DmOutbox` alongside the device's own cert. DM/owner-state/transport keep the Reticulum key (out of scope).

**Tech Stack:** Rust (`src-tauri`), `ed25519_dalek`, canonical CBOR (`harmony_owner::cbor` / `owner_state_crypto::canonical_cbor_encode`), `harmony_owner::certs::enrollment::{EnrollmentCert, EnrollmentIssuer}`, `harmony_owner::pubkey_bundle::PubKeyBundle`, `cargo nextest`, `cargo clippy`, `cargo fmt`.

**Spec:** `docs/specs/2026-05-29-zeb-339-community-membership-enrolled-device-signing-design.md` (commit `6bbb407`). The spec governs intent/behavior; this plan is authoritative over the spec where it adds the implementation-level decisions in "Plan clarifications" below.

**Branch:** `zeb-339-community-enrolled-device-signing` (already exists at HEAD `6bbb407`, off `origin/main` `509fd29` = merged ZEB-338). Per `feedback_no_worktrees`: use `git checkout` in the main repo; NEVER create a worktree.

---

## Plan clarifications / corrections to spec

These resolve implementation-surface details the spec's prose left open. They are faithful to the spec's model (esp. §6.5 community-only scope) and are called out here so spec-reviewer subagents do not flag them as drift.

1. **Production verify path is the sync engine, not `VerifyContext` callers.** The real verification runs through `community_state_sync::CommunitySyncEngine::insert_local_event` → `insert_event_with_resolved_pubs` → `community_membership::verify_event`. The 9 `VerifyContext { … }` sites in `lib.rs` are *test fixtures*. Removing the three identity-pub fields from `VerifyContext` (spec §6.2) therefore touches: the engine's two insert methods, `handle_incoming_publish`, and those 9 test sites.

2. **Three flat-check sites, not one.** Beyond `verify_signature` (community_membership.rs:963), the same `Identity::from_public_bytes(pub).address_hash == X` pattern lives in `verify_countersig` (community_membership.rs:1035) and *inline* in `handle_incoming_publish` (community_state_sync.rs:3168–3178). All three are replaced.

3. **`CommunityInviteSigned.joiner_identity_pub` STAYS; only `PendingJoin.joiner_identity_pub` is removed.** The spec §5.2 removes the redundant pub from `PendingJoin` (the joiner's cert rides on `event.enrollment`). But `CommunityInviteSigned` (community_invite.rs) is a *Reticulum unicast transport* envelope; its `joiner_identity_pub` + `signing_device_hash` bind the transport layer (X25519/DM), which keeps the Reticulum key per the community-only scope. What changes in the invite path: the inner `join_event`'s **membership** signature is verified via its carried `EnrollmentCert` (not `verify_signature(join_event, joiner_identity_pub)`), because the join_event is now device-#2-signed.

4. **`DmOutbox` is the runtime home for device #2's key + the device's own cert.** `DmOutbox` already holds `private_identity: Arc<PrivateIdentity>` *specifically* for the community counter-sign path (ZEB-262). We extend it with `community_signing_key: Arc<ed25519_dalek::SigningKey>` (device #2) and `enrollment_cert: harmony_owner::certs::enrollment::EnrollmentCert` (the device's own Master cert). `loaded.device_signing_key` — currently dropped after deriving the device-id string (lib.rs:2518) — is carried into this construction.

5. **The engine's publish signing key switches to device #2; DM/transport stays Reticulum.** `CommunitySyncEngineConfig.signing_key` becomes device #2 (it is used only for `publisher_sig`). `DmOutbox.signing_key` (used for DmAck/transport) stays the Reticulum key.

6. **`OwnerDeviceCacheResolver` is NOT deleted — only its use in the community membership + publish verify path is removed.** It is also constructed for the voting-log engine (lib.rs:~25019). Task 9 removes the resolver calls from `insert_local_event` / `insert_event_with_resolved_pubs` / `handle_incoming_publish`; whether the `identity_resolver` config field can then be deleted is determined by `grep` (see Task 9 Step 1). Default to leaving the field/type in place if any consumer remains; the spec's "remove from the community path" is satisfied by removing the *verify-time calls*.

7. **Test-helper migration is the risk gate (analogous to ZEB-338 T3).** Every existing community test pairs `actor` and `signing_key` from one `PrivateIdentity`, so `address_hash(key) == actor` holds trivially. Under the new model `actor = owner_id ≠ address_hash(device_key)`, so those tests must migrate to a shared `mint_test_owner()` helper (Task 2). Task 10 does the bulk migration. If the blast radius exceeds ~10-min wall-clock per file or the helper can't satisfy a test's intent, the implementer surfaces `DONE_WITH_CONCERNS` rather than stalling.

---

## File structure

| File | Responsibility | Change |
|---|---|---|
| `src-tauri/src/community_membership.rs` | event types, sign/verify primitives, materialize | `SignedMembershipEvent.enrollment`; `MemberState.enrolled_device_keys`; `verify_membership_signer` + `EnrolledDeviceKey`; new `VerifyError` variants; `verify_event`/`verify_countersig` rewrite; `VerifyContext` slimming; `PendingJoin` field removal; `materialize` cert ingestion; new device-key sign/countersign helpers; `mint_test_owner` test helper |
| `src-tauri/src/community_state_sync.rs` | production verify engine + publisher-auth | stop resolving actor/countersigner pubs; publisher-auth via materialized `enrolled_device_keys`; engine `signing_key` = device #2 |
| `src-tauri/src/community_invite.rs` | invite link payload + unicast redeem/counter-sign | invite payload carries inviter's cert; join_event verified via cert; counter-sign signs with device #2 |
| `src-tauri/src/dm_outbox.rs` | runtime signing context | add `community_signing_key` + `enrollment_cert` fields + constructor params |
| `src-tauri/src/lib.rs` | start_node wiring + all community mint sites | carry `device_signing_key` + own cert into `DmOutbox`; engine `signing_key` = device #2; switch every community mint site to device #2 + attach own cert on Join-bearing events; update 9 test `VerifyContext` sites |
| `src-tauri/tests/wire_format_*` | wire-format pinning | Join-with-cert + steady-state-without-cert fixtures; old fixtures still decode |

---

## Conventions every task MUST follow (HARD RULES)

- **Backend gates from `src-tauri/`** (run after the task's commit; foreground; `timeout 600`):
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  3. `cargo nextest run --locked --features test-fixtures -E 'test(<scoped>)'` during a task (scoped to the area); full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` in Task 13.
  4. `HARMONY_LARGE_TESTS=1 cargo nextest run --locked --features test-fixtures -E 'test(folder_ingest_walker_integration)'` in Task 13.
  5. MSRV `cargo check --locked --all-targets --features test-fixtures` in Task 13.
- **COMMIT BEFORE running the long gate** (`feedback_implementer_gate_time_budget`). Every cargo/nextest invocation FOREGROUND with `timeout 600`. If a gate exceeds 10-min wall-clock, surface `DONE_WITH_CONCERNS` — do NOT silently stall (`feedback_long_running_background_supervision`).
- **Pipe exit codes lie** (`feedback_pipe_exit_codes_lie`): use `set -o pipefail` or `${PIPESTATUS[0]}` when piping cargo through `tail`/`grep`.
- **Canonical CBOR determinism**: any map-typed field in a `CanonicalPayload` must be `BTreeMap`/`BTreeSet` (never `HashMap`/`HashSet`). `enrolled_device_keys` is a `BTreeSet<[u8;32]>` for exactly this reason.
- **Same-length-keys CBOR invariant**: new serde field codes are 2 chars at the `SignedMembershipEvent`/`MemberState`/`CommunityInvitePayload` nesting levels (matches existing codes there). New codes introduced: `en` (enrollment), `ek` (enrolled_device_keys), `ec` (inviter cert in invite payload).
- **macOS XprotectService** mitigation already applied on this machine (per CLAUDE.md). If cold `cargo nextest` hangs >10 min reappear, document in `DONE_WITH_CONCERNS`.
- Pre-existing orphan test failures captured at Task 0 baseline are NOT blocking; any NEW failure introduced by this work IS blocking (`feedback_test_drift_is_our_fault`, `feedback_unrelated_test_failures`).

---

## Task 0: Pre-flight baseline (no commit)

**Files:** none (read-only)

- [ ] **Step 1: Confirm branch + clean tree**

Run:
```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status --porcelain && git rev-parse --abbrev-ref HEAD
```
Expected: empty porcelain output; branch `zeb-339-community-enrolled-device-signing`.

- [ ] **Step 2: Capture the orphan baseline**

Run (foreground, `timeout 600`):
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures \
  -E 'package(harmony-app)' 2>&1 | tail -60
```
Record the set of FAILED tests. Known orphans (NOT blocking): `folder_ingest::tests`, `mint::tests`, `mint_sync::tests`, `rename_content_integration` (port-4242 flake), occasional `zenoh_iroh_*` timeouts. Any NEW failure introduced by Tasks 1–12 is blocking.

- [ ] **Step 3: Confirm the harmony-owner cert API is reachable**

Run:
```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && \
  grep -n 'harmony-owner' Cargo.toml && \
  grep -rn 'EnrollmentCert' src/pairing/ | head
```
Expected: `harmony-owner` is a dependency; `harmony_owner::certs::EnrollmentCert` is already imported in `src/pairing/`. Confirms the crate path `harmony_owner::certs::enrollment::EnrollmentCert` and `harmony_owner::pubkey_bundle::PubKeyBundle` are usable.

No commit.

---

## Task 1: `SignedMembershipEvent.enrollment` wire field

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (struct at 349–383; `EventPayload::from` at 463–473; tests near 3685)

The cert sits OUTSIDE the signed `EventPayload` (it is not added to `EventPayload`), so the signature domain is unchanged. Safe because `cert.owner_id` must equal the signed `actor`, the cert is master-signed (unforgeable), and the event sig must verify under `cert.device_pubkeys`.

- [ ] **Step 1: Write the failing round-trip test**

Add to the tests module of `community_membership.rs`:
```rust
#[test]
fn signed_event_enrollment_roundtrips_and_defaults_absent() {
    // A steady-state event encodes with NO `en` key (back-compat).
    let ev = SignedMembershipEvent {
        id: [1u8; 16],
        community_id: SpaceId([2u8; 16]),
        kind: MembershipEventKind::Leave,
        actor: OwnerAddr([3u8; 16]),
        at: Hlc::default(),
        sig: [0u8; 64],
        countersig: None,
        enrollment: None,
    };
    let bytes = canonical_cbor_encode(&ev).unwrap();
    let back: SignedMembershipEvent =
        crate::owner_state_crypto::canonical_cbor_decode(&bytes).unwrap();
    assert_eq!(back, ev);
    assert!(back.enrollment.is_none());
    // Old wire bytes (without `en`) still decode (serde default).
    // Re-encode WITHOUT the field by decoding a map missing `en`:
    assert!(back.enrollment.is_none());
}
```
(If `canonical_cbor_decode` is named differently in this crate, use the existing decode helper — grep for `canonical_cbor_decode` / `from_bytes` near the encode helper at line 972.)

- [ ] **Step 2: Run it — expect FAIL (no `enrollment` field)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(signed_event_enrollment_roundtrips)' 2>&1 | tail -20`
Expected: compile error — `SignedMembershipEvent` has no field `enrollment`.

- [ ] **Step 3: Add the field + import**

At the top of `community_membership.rs`, add (near other imports):
```rust
use harmony_owner::certs::enrollment::EnrollmentCert;
```
Append the field to `SignedMembershipEvent` (after `countersig`):
```rust
    /// ZEB-339: enrolment proof for the signer. REQUIRED on identity-
    /// introducing events (bootstrap Join, Join, PendingJoin); absent
    /// otherwise (the verifier resolves the signer's device key from
    /// materialized membership). Sits OUTSIDE the signed EventPayload —
    /// safe because cert.owner_id must equal the signed `actor`, the cert
    /// is master-signed (unforgeable), and the event sig must verify under
    /// cert.device_pubkeys.
    #[serde(rename = "en", skip_serializing_if = "Option::is_none", default)]
    pub enrollment: Option<EnrollmentCert>,
```
`EventPayload::from(&SignedMembershipEvent)` is unchanged (cert is not part of the payload). **Update every other `SignedMembershipEvent { … }` literal in this file** (in `sign_event`, `sign_event_with_identity`, `attach_countersig`, `attach_countersig_with_identity`, and all tests) to add `enrollment: None`. Find them: `grep -n 'SignedMembershipEvent {' src/community_membership.rs`.

- [ ] **Step 4: Run the test — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(signed_event_enrollment_roundtrips)' 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cd src-tauri && cargo fmt --all && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
git add -A && git commit -m "feat(zeb-339): SignedMembershipEvent carries optional EnrollmentCert"
```
Note: clippy/test compile may fail at other call sites in `lib.rs`/`community_invite.rs`/`community_state_sync.rs` that build `SignedMembershipEvent` literals. Add `enrollment: None` to each (grep across `src/`). If that balloons past 10 min, commit the membership.rs change and surface `DONE_WITH_CONCERNS` listing remaining literal sites for the next task.

---

## Task 2: `mint_test_owner` helper + `MemberState.enrolled_device_keys` + materialize ingestion (RISK GATE — shared test foundation)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`MemberState` 1149–1156; `materialize_with_now` Join handling 1453–1483; tests module)

This task lands the shared test helper that every later test depends on, and the materialized-membership field that steady-state verification reads.

- [ ] **Step 1: Write the failing test for the helper + materialize ingestion**

```rust
#[cfg(test)]
pub(crate) struct TestOwner {
    pub owner: OwnerAddr,
    pub device_key: ed25519_dalek::SigningKey,
    pub cert: EnrollmentCert,
}

#[cfg(test)]
pub(crate) fn mint_test_owner(seed: u8) -> TestOwner {
    // Deterministic master + device keys from `seed` so tests are reproducible.
    use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
    let master_sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let master_bundle = PubKeyBundle {
        classical: ClassicalKeys {
            ed25519_verify: master_sk.verifying_key().to_bytes(),
            x25519_pub: [0u8; 32],
        },
        post_quantum: None,
    };
    let owner_id = master_bundle.identity_hash();
    let device_sk = ed25519_dalek::SigningKey::from_bytes(&[seed ^ 0xFF; 32]);
    let device_bundle = PubKeyBundle {
        classical: ClassicalKeys {
            ed25519_verify: device_sk.verifying_key().to_bytes(),
            x25519_pub: [0u8; 32],
        },
        post_quantum: None,
    };
    let device_id = device_bundle.identity_hash();
    let cert = EnrollmentCert::sign_master(
        &master_sk, master_bundle, device_id, device_bundle, 1_700_000_000, None,
    )
    .expect("sign_master");
    cert.verify().expect("self-minted cert verifies");
    TestOwner { owner: OwnerAddr(owner_id), device_key: device_sk, cert }
}

#[test]
fn materialize_records_enrolled_device_key_from_join_cert() {
    let admin = mint_test_owner(0x11);
    // Build a bootstrap Join carrying admin's cert, signed by admin's device key.
    let join = sign_event(
        &EventPayload {
            id: [1u8; 16],
            community_id: SpaceId([9u8; 16]),
            kind: MembershipEventKind::Join,
            actor: admin.owner,
            at: Hlc::default(),
        },
        &admin.device_key,
    )
    .unwrap();
    let join = SignedMembershipEvent { enrollment: Some(admin.cert.clone()), ..join };
    let m = materialize(&[join], admin.owner);
    let ek = &m.members.get(&admin.owner).unwrap().enrolled_device_keys;
    assert!(ek.contains(&admin.device_key.verifying_key().to_bytes()));
}
```

- [ ] **Step 2: Run — expect FAIL** (`enrolled_device_keys` field missing; `mint_test_owner` may compile but `MemberState` has no such field).

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(materialize_records_enrolled)' 2>&1 | tail -20`
Expected: compile error on `enrolled_device_keys`.

- [ ] **Step 3: Add the field**

In `MemberState` (after `left_at`):
```rust
    /// ZEB-339: ed25519 verify keys vouched under this member's owner_id,
    /// learned from the EnrollmentCert carried on their Join. A SET so an
    /// owner with multiple devices in a community is representable (eventual
    /// state); populated with exactly one today.
    #[serde(rename = "ek", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub enrolled_device_keys: BTreeSet<[u8; 32]>,
```
Ensure `use std::collections::BTreeSet;` is present (it is — `MaterializedMembership` uses it). **Update every `MemberState { … }` literal** in the file to add `enrolled_device_keys: BTreeSet::new()` (grep `MemberState {`), EXCEPT where Task Step 4 sets it.

- [ ] **Step 4: Ingest the cert key in materialize**

In `materialize_with_now`, inside the `MembershipEventKind::Join` arm (1453–1483), when `should_refresh` inserts the `MemberState`, populate the set from the event's cert. Replace the `MemberState { … }` insert with:
```rust
        let mut enrolled = m
            .members
            .get(&event.actor)
            .map(|s| s.enrolled_device_keys.clone())
            .unwrap_or_default();
        if let Some(cert) = event.enrollment.as_ref() {
            enrolled.insert(cert.device_pubkeys.classical.ed25519_verify);
        }
        m.members.insert(
            event.actor,
            MemberState {
                status: MemberStatus::Joined,
                joined_at: event.at.clone(),
                left_at: None,
                enrolled_device_keys: enrolled,
            },
        );
```
(Carrying forward any prior set keeps a rejoin from dropping a previously-learned key — eventual multi-device.) Do the equivalent for any other arm that constructs a `MemberState` with `status: Joined` for the actor (e.g. PendingJoin→Joined transition via JoinCountersign, if it builds a fresh `MemberState`; preserve/merge `enrolled_device_keys` there too — grep the materialize body).

- [ ] **Step 5: Run — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(materialize_records_enrolled)' 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Back-compat decode test + commit**

Add a test that a `MaterializedMembership`/`MemberState` CBOR blob without `ek` decodes to an empty set (mirror the existing `channels` `#[serde(default)]` back-compat test). Then:
```bash
cd src-tauri && cargo fmt --all && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
git add -A && git commit -m "feat(zeb-339): MemberState.enrolled_device_keys + materialize ingests Join cert; add mint_test_owner helper"
```

---

## Task 3: `verify_membership_signer` primitive + `EnrolledDeviceKey` + error taxonomy

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`VerifyError` 538–772; new fns near `verify_signature` 963)

- [ ] **Step 1: Write failing unit tests for the primitive**

```rust
#[test]
fn verify_signer_accepts_cert_signed_event() {
    let o = mint_test_owner(0x21);
    let ev = sign_event(&EventPayload {
        id: [1u8;16], community_id: SpaceId([7u8;16]),
        kind: MembershipEventKind::Join, actor: o.owner, at: Hlc::default(),
    }, &o.device_key).unwrap();
    let ev = SignedMembershipEvent { enrollment: Some(o.cert.clone()), ..ev };
    let signer = EnrolledDeviceKey {
        owner: o.owner,
        device_ed25519: o.cert.device_pubkeys.classical.ed25519_verify,
    };
    assert!(verify_membership_signer(&ev, &signer).is_ok());
}

#[test]
fn verify_signer_rejects_tampered_event() {
    let o = mint_test_owner(0x22);
    let mut ev = sign_event(&EventPayload {
        id: [1u8;16], community_id: SpaceId([7u8;16]),
        kind: MembershipEventKind::Join, actor: o.owner, at: Hlc::default(),
    }, &o.device_key).unwrap();
    ev.id = [2u8; 16]; // tamper after signing
    let signer = EnrolledDeviceKey {
        owner: o.owner,
        device_ed25519: o.device_key.verifying_key().to_bytes(),
    };
    assert_eq!(verify_membership_signer(&ev, &signer), Err(VerifyError::SignatureInvalid));
}
```

- [ ] **Step 2: Run — expect FAIL** (no `EnrolledDeviceKey` / `verify_membership_signer`).

- [ ] **Step 3: Add the new error variants**

In `VerifyError`, add (keep `SignatureInvalid`; do NOT delete `ActorPubkeyMismatch`/`CounterSignerPubkeyMismatch`/`InvalidIdentityPub` yet — Task 4/5/8 remove their last uses, then a cleanup step deletes them):
```rust
    /// ZEB-339: Join/PendingJoin/bootstrap arrived with no `enrollment` cert.
    MissingEnrollmentCert,
    /// ZEB-339: `cert.verify()` failed (bad master sig / hash(master)!=owner_id
    /// / device-id mismatch / unknown version).
    EnrollmentCertInvalid,
    /// ZEB-339: `cert.owner_id != event.actor.0`.
    EnrollmentOwnerMismatch,
    /// ZEB-339: materialized lookup found no enrolled device key matching the
    /// signing key for `actor`.
    SignerNotEnrolledForActor,
    /// ZEB-339: counter-signer's signing key is not in the counter-signer's
    /// materialized `enrolled_device_keys`.
    CounterSignerNotEnrolled,
```

- [ ] **Step 4: Add `EnrolledDeviceKey` + `verify_membership_signer`**

Near `verify_signature` (963):
```rust
/// The minimal proven fact: this ed25519 key is a device enrolled under `owner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnrolledDeviceKey {
    pub owner: OwnerAddr,
    pub device_ed25519: [u8; 32],
}

/// Verify the event was authored by `signer`'s enrolled device key, over the
/// canonical EventPayload. `signer.owner` must equal `event.actor`.
pub fn verify_membership_signer(
    event: &SignedMembershipEvent,
    signer: &EnrolledDeviceKey,
) -> Result<(), VerifyError> {
    if signer.owner != event.actor {
        return Err(VerifyError::SignerNotEnrolledForActor);
    }
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signer.device_ed25519)
        .map_err(|_| VerifyError::SignatureInvalid)?;
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = Signature::from_bytes(&event.sig);
    vk.verify_strict(&bytes, &sig)
        .map_err(|_| VerifyError::SignatureInvalid)
}

/// Resolve an `EnrolledDeviceKey` from an identity-introducing event's carried
/// EnrollmentCert: verify the cert, bind owner==actor, return the device key.
pub fn enrolled_key_from_cert(
    event: &SignedMembershipEvent,
) -> Result<EnrolledDeviceKey, VerifyError> {
    let cert = event.enrollment.as_ref().ok_or(VerifyError::MissingEnrollmentCert)?;
    cert.verify().map_err(|_| VerifyError::EnrollmentCertInvalid)?;
    if cert.owner_id != event.actor.0 {
        return Err(VerifyError::EnrollmentOwnerMismatch);
    }
    Ok(EnrolledDeviceKey {
        owner: event.actor,
        device_ed25519: cert.device_pubkeys.classical.ed25519_verify,
    })
}
```
(`Signature` and `canonical_cbor_encode` are already imported — they back `verify_signature`.)

- [ ] **Step 5: Run unit tests — expect PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_signer)' 2>&1 | tail -20`

- [ ] **Step 6: Negative tests (one per cert error) + commit**

Add tests: forged cert (flip a byte of `cert.signature`) → `EnrollmentCertInvalid`; `cert.owner_id` mutated → `EnrollmentOwnerMismatch`; `enrollment: None` on a Join via `enrolled_key_from_cert` → `MissingEnrollmentCert`. Then fmt + clippy + commit:
```bash
git commit -am "feat(zeb-339): verify_membership_signer + EnrolledDeviceKey + cert error taxonomy"
```

---

## Task 4: Rewire `verify_event` + `verify_countersig` to cert / materialized-key resolution; slim `VerifyContext`

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`VerifyContext` 2173–2185; `verify_event` 2222–2819; `verify_countersig` 1035–1054)

This is the core. `verify_event` derives signer keys itself; callers no longer pre-resolve pubs.

- [ ] **Step 1: Write the production-pairing test (MUST fail on pre-Task-4 code, pass after)**

This is spec §9.1 — the test that proves the bug is fixed.
```rust
#[test]
fn verify_event_accepts_owner_id_actor_signed_by_enrolled_device() {
    // The PRODUCTION pairing: actor = owner_id, signed by device #2 (not the
    // owner_id's own key — owner_id has no signing key). Bootstrap Join.
    let admin = mint_test_owner(0x31);
    let cid = SpaceId([5u8; 16]);
    let join = sign_event(&EventPayload {
        id: [1u8;16], community_id: cid, kind: MembershipEventKind::Join,
        actor: admin.owner, at: Hlc::default(),
    }, &admin.device_key).unwrap();
    let join = SignedMembershipEvent { enrollment: Some(admin.cert.clone()), ..join };
    let prior = MaterializedMembership::default(); // bootstrap: empty prior
    let ctx = VerifyContext {
        expected_community_id: cid,
        admin_addr: admin.owner,
        is_invite_only: false,
    };
    assert_eq!(verify_event(&join, &prior, &ctx), Ok(()));
}
```

- [ ] **Step 2: Run — expect FAIL** (currently `VerifyContext` requires `actor_identity_pub`, and `verify_signature` would reject the owner_id/device mismatch). Confirms the bug reproduces under test.

- [ ] **Step 3: Slim `VerifyContext`**

```rust
pub struct VerifyContext {
    pub expected_community_id: SpaceId,
    pub admin_addr: OwnerAddr,
    pub is_invite_only: bool,
}
```
(Drop the lifetime `'a` and the three identity-pub fields. Update the `VerifyContext<'a>` references throughout this file and others — the compiler will list them.)

- [ ] **Step 4: Rewrite the actor-sig check in `verify_event`**

Replace the `verify_signature(event, ctx.actor_identity_pub)?;` call (~2261) with cert/materialized resolution:
```rust
    // ZEB-339: resolve the signer's enrolled device key, then verify the sig.
    let signer = match &event.kind {
        // Identity-introducing events carry their own cert.
        MembershipEventKind::Join | MembershipEventKind::PendingJoin { .. } => {
            enrolled_key_from_cert(event)?
        }
        // Steady-state events: resolve from materialized membership.
        _ => resolve_enrolled_signer(prior_state, event)?,
    };
    verify_membership_signer(event, &signer)?;
```
Add the helper (steady-state resolution: any enrolled key of the actor that verifies the sig):
```rust
fn resolve_enrolled_signer(
    prior_state: &MaterializedMembership,
    event: &SignedMembershipEvent,
) -> Result<EnrolledDeviceKey, VerifyError> {
    let member = prior_state
        .members
        .get(&event.actor)
        .ok_or(VerifyError::SignerNotEnrolledForActor)?;
    // Find the enrolled key that actually verifies this event's signature.
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = Signature::from_bytes(&event.sig);
    for key in &member.enrolled_device_keys {
        if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(key) {
            if vk.verify_strict(&bytes, &sig).is_ok() {
                return Ok(EnrolledDeviceKey { owner: event.actor, device_ed25519: *key });
            }
        }
    }
    Err(VerifyError::SignerNotEnrolledForActor)
}
```
NOTE on ordering vs. the existing power check: this signer-resolution happens BEFORE the per-kind dispatch / power gate, same position as the old `verify_signature` call. The "actor must be a Joined member" gate (`ActorNotJoined`, etc.) still runs in the dispatch block and is unchanged — `resolve_enrolled_signer` returning `SignerNotEnrolledForActor` for a non-member is the same causal-ordering invariant the spec §5.1 cites (no Join materialized yet → no enrolled key).

- [ ] **Step 5: Rewrite `verify_countersig`**

`verify_countersig` currently takes `signer_identity_pub: &[u8;64]`. Change the signature to resolve from materialized membership and verify the countersig over the EventPayload:
```rust
pub fn verify_countersig(
    event: &SignedMembershipEvent,
    prior_state: &MaterializedMembership,
) -> Result<(), VerifyError> {
    let cs = event.countersig.as_ref().ok_or(VerifyError::CounterSigRequired)?;
    let member = prior_state
        .members
        .get(&cs.signer)
        .ok_or(VerifyError::CounterSignerNotEnrolled)?;
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = Signature::from_bytes(&cs.sig);
    for key in &member.enrolled_device_keys {
        if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(key) {
            if vk.verify_strict(&bytes, &sig).is_ok() {
                return Ok(());
            }
        }
    }
    Err(VerifyError::CounterSignerNotEnrolled)
}
```
Update the call site in `verify_event` (~2320): change `verify_countersig(event, cs_identity_pub)?` to `verify_countersig(event, prior_state)?` and delete the `let cs_identity_pub = ctx.countersigner_identity_pub.ok_or(...)?;` line.

- [ ] **Step 6: Run the production-pairing test — expect PASS**; run the whole membership test module scoped.

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_event_accepts_owner_id_actor) + test(verify_countersig)' 2>&1 | tail -30`
Expected: the new test PASSES. (Many OTHER tests in this file will now fail to compile or fail — they pass `actor_identity_pub` into `VerifyContext`. Those are migrated in Task 10; this task only needs the new test green + the file to compile its non-test code. If test-module compile errors block the run, mark them with a `// MIGRATE ZEB-339 Task 10` comment and, if necessary, temporarily `#[ignore]` the broken tests so the new test runs — Task 10 un-ignores + migrates them. Prefer fixing in place if quick.)

- [ ] **Step 7: fmt + clippy (lib targets) + commit**

```bash
cd src-tauri && cargo fmt --all && \
  cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
git commit -am "feat(zeb-339): verify_event/verify_countersig resolve signer via cert or materialized enrolled keys; slim VerifyContext"
```
(Scope clippy to `--lib` here because `--all-targets` will surface the not-yet-migrated test sites; full `--all-targets` clippy returns in Task 13.)

---

## Task 5: Remove `PendingJoin.joiner_identity_pub`; resolve InviteToken signer via membership/cert

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`PendingJoin` 287–301; the PendingJoin verify gate in `verify_event`; `VerifyError` `PendingJoinJoinerPubMismatch`)

- [ ] **Step 1: Write/adjust the PendingJoin verify test**

Add a test that a PendingJoin (actor = joiner owner_id, signed by joiner device #2, carrying joiner cert, with an InviteToken signed by the admin's device #2) verifies when the admin is a Joined member in `prior_state` with the admin's enrolled key. Use two `mint_test_owner` identities. Assert `verify_event` returns `Ok(())`.

- [ ] **Step 2: Run — expect FAIL** (compile: PendingJoin still has `joiner_identity_pub`; token verification still keyed on `admin_identity_pub`).

- [ ] **Step 3: Remove the field**

In `MembershipEventKind::PendingJoin`, delete `joiner_identity_pub: [u8;64]` and its serde attrs; keep `invite_token`. Update all `PendingJoin { … }` literals (grep across `src/`). Delete the `PendingJoinJoinerPubMismatch` variant and its single use in the verify gate (the joiner-pub→actor hash check is now subsumed by `enrolled_key_from_cert` binding `cert.owner_id == actor`).

- [ ] **Step 4: Resolve the InviteToken inviter key from membership (§6.5)**

In the PendingJoin verify gate, the InviteToken signature was verified against `ctx.admin_identity_pub`. Replace with resolution of the **inviter's** enrolled device key from `prior_state` (the inviter — `invite_token.inviter` — is a Joined member; the community creator's bootstrap Join is genesis so its key is materialized first). Add a verify helper that mirrors `community_invite::verify_invite_token_signature` but takes the resolved ed25519 key:
```rust
fn verify_invite_token_sig_with_enrolled(
    token: &crate::community_invite::InviteToken,
    prior_state: &MaterializedMembership,
) -> Result<(), VerifyError> {
    let member = prior_state.members.get(&token.inviter)
        .ok_or(VerifyError::PendingJoinTokenInvalid)?;
    let token_bytes = crate::community_invite::canonical_invite_token_bytes(token)
        .map_err(|_| VerifyError::PendingJoinTokenInvalid)?;
    let sig = Signature::from_bytes(&token.sig);
    for key in &member.enrolled_device_keys {
        if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(key) {
            if vk.verify_strict(&token_bytes, &sig).is_ok() { return Ok(()); }
        }
    }
    Err(VerifyError::PendingJoinTokenInvalid)
}
```
(Ensure `canonical_invite_token_bytes` is `pub(crate)` in community_invite.rs — make it so if not.) Wire it into the PendingJoin gate in place of the `admin_identity_pub`-based check. The `invitee_hint`/expiry/`inviter == admin_addr` checks are unchanged.

- [ ] **Step 5: Run the PendingJoin test — expect PASS** (scope `-E 'test(pending_join)'`).

- [ ] **Step 6: fmt + clippy (lib) + commit**

```bash
git commit -am "feat(zeb-339): drop PendingJoin.joiner_identity_pub; verify InviteToken via inviter's enrolled key"
```

---

## Task 6: Carry device #2 key + own cert into runtime (`DmOutbox`)

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (`DmOutbox` struct 399–429; `DmOutbox::new`)
- Modify: `src-tauri/src/lib.rs` (start_node: device-id derivation ~2518; DmOutbox construction ~2594)

- [ ] **Step 1: Write the failing test**

In `dm_outbox.rs` tests, assert a constructed `DmOutbox` exposes `community_signing_key` (an `Arc<SigningKey>`) and `enrollment_cert` (an `EnrollmentCert`) distinct from `signing_key`, and that `enrollment_cert.verify().is_ok()` and `enrollment_cert.device_pubkeys.classical.ed25519_verify == community_signing_key.verifying_key().to_bytes()`.

- [ ] **Step 2: Run — expect FAIL** (no such fields).

- [ ] **Step 3: Add fields + constructor params**

In `DmOutbox`:
```rust
    /// ZEB-339: the harmony-owner ENROLLED device signing key (#2). Distinct
    /// from `signing_key` (the Reticulum/transport key, #3). Community
    /// membership events sign with this; DM/transport keep `signing_key`.
    pub(crate) community_signing_key: Arc<ed25519_dalek::SigningKey>,
    /// ZEB-339: this device's own Master EnrollmentCert (owner_id -> #2),
    /// attached to outbound identity-introducing events (bootstrap/redeem Join).
    pub(crate) enrollment_cert: harmony_owner::certs::enrollment::EnrollmentCert,
```
Add the two params to `DmOutbox::new` (after `private_identity`) and assign them. Update all `DmOutbox::new(` call sites (grep — production at lib.rs ~2594, plus any test constructors in dm_outbox.rs tests; tests can use `mint_test_owner`-style material or a small local helper).

- [ ] **Step 4: Wire production construction in lib.rs**

At the device-id derivation block (~2518), the loaded state and `device_signing_key` are in scope. Before `loaded` is consumed, extract:
```rust
    let community_signing_key_arc =
        std::sync::Arc::new(loaded.device_signing_key.insecure_clone());
    let this_device_id = {
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        PubKeyBundle::classical_only(
            loaded.device_signing_key.verifying_key().to_bytes(),
        ).identity_hash()
    };
    let own_enrollment_cert = loaded
        .state
        .enrollments
        .get(&this_device_id)
        .cloned()
        .ok_or_else(|| "owner state missing this device's enrollment cert".to_string())?;
```
(`SigningKey` is not `Clone` by default in ed25519-dalek v2; use `insecure_clone()` if available, else reconstruct via `SigningKey::from_bytes(&loaded.device_signing_key.to_bytes())`. Verify which exists — grep the crate; the codebase already wraps signing keys in `Arc`, so prefer building the `Arc` once and cloning the `Arc`.) Pass `community_signing_key_arc.clone()` and `own_enrollment_cert.clone()` into `DmOutbox::new`. Keep the existing `device_id` String derivation (still used for HLC device IDs) — note `this_device_id` here is the 16-byte hash for the enrollments lookup, a different value from the hex `device_id` String; name them distinctly.

- [ ] **Step 5: Run dm_outbox test — expect PASS; fmt + clippy (lib) + commit**

```bash
git commit -am "feat(zeb-339): DmOutbox carries enrolled device #2 key + own EnrollmentCert"
```

---

## Task 7: Switch all community mint sites to device #2 + attach own cert on Join-bearing events

**Files:**
- Modify: `src-tauri/src/lib.rs` (`mint_community_creation` 13586–13648; `create_community_inner` 13687+; `mint_redemption` ~14786; channel mints 12324/12521/12552; `mint_leave_event` 17661; `mint_kick_event` 19057; `mint_unban_event` 19091; admin-proposal mints; `generate_invite` token signing)

- [ ] **Step 1: Write the create-community integration test (drives the real path)**

Add a test that calls `create_community_inner` with `self_owner = owner_id` from a `mint_test_owner` and a `DmOutbox` whose `community_signing_key`/`enrollment_cert` are that owner's device #2/cert, and asserts the bootstrap Join inserts successfully (no `ActorPubkeyMismatch` / accepted by the engine). If `create_community_inner`'s signature makes a focused unit test impractical, instead assert at the `mint_community_creation` level: the returned `bootstrap_join.enrollment.is_some()` and `verify_event(&bootstrap_join, &MaterializedMembership::default(), &ctx).is_ok()`.

- [ ] **Step 2: Run — expect FAIL** (bootstrap Join has no cert; signed by Reticulum key).

- [ ] **Step 3: Thread device #2 + cert into `mint_community_creation`**

Change `mint_community_creation` to accept the cert and attach it to the bootstrap Join:
```rust
pub fn mint_community_creation(
    name: &str,
    is_invite_only: bool,
    self_owner: OwnerAddr,
    signing_key: &ed25519_dalek::SigningKey, // now device #2
    enrollment_cert: &EnrollmentCert,
    creation_hlc: Hlc,
) -> Result<MintedCommunity, String> {
    // ... build join_payload ...
    let mut bootstrap_join = sign_event(&join_payload, signing_key)
        .map_err(|e| format!("sign bootstrap join: {e}"))?;
    bootstrap_join.enrollment = Some(enrollment_cert.clone());
    // ... rest unchanged ...
}
```
In `create_community_inner`, the signing key + cert must come from device #2. Update its params to take `community_signing_key: Arc<SigningKey>` + `enrollment_cert: EnrollmentCert` (or fetch from the `dm_outbox` it already has access to). Pass them through to `mint_community_creation`.

- [ ] **Step 4: Switch every other mint site**

For each site that currently does `let signing_key = outbox_g.signing_key.as_ref();` for a **community membership** event, change to `let signing_key = outbox_g.community_signing_key.as_ref();`:
- `mint_redemption` self-join / PendingJoin (attach `outbox_g.enrollment_cert.clone()` to the Join/PendingJoin's `enrollment`).
- `create_channel`/`modify_channel`/`delete_channel` (channel events — no cert, steady-state).
- `leave_community` (`mint_leave_event`).
- `kick_from_community` (`mint_kick_event` + admin-proposal-kick).
- `set_power_level` (admin-proposal-set-power).
- `unban_from_community` (`mint_unban_event`).
- `generate_invite`: the `InviteToken` is signed here (grep for where `InviteToken.sig` is produced — likely a `build_*invite*` or `sign` call). Switch that to device #2 so the token verifies under the admin's enrolled key (Task 5's resolution). Also ensure the invite payload's inviter cert is attached (Task 8 owns the payload field; here just confirm the token is device-#2-signed).
- DO NOT change `outbox_g.signing_key` uses that sign **DmAck/transport** packets — those stay Reticulum.

Attach `enrollment: Some(outbox_g.enrollment_cert.clone())` ONLY on Join / PendingJoin / bootstrap events; leave it `None` on Leave/Kick/Unban/SetPower/Channel/Countersign events.

- [ ] **Step 5: Run create test — expect PASS**; scope `-E 'test(create_community) + test(mint_community)'`.

- [ ] **Step 6: fmt + clippy (lib) + commit**

```bash
git commit -am "feat(zeb-339): community mint sites sign with device #2; attach own cert on Join events"
```

---

## Task 8: Counter-sign with device #2 + invite path cert wiring

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`attach_countersig*` 988–1015)
- Modify: `src-tauri/src/community_invite.rs` (`CommunityInvitePayload` 89–165; `verify_packet_pure` 1373–1445; `handle_unicast` 1592–1849; encode/decode 806–895)

- [ ] **Step 1: Write failing tests**

(a) A counter-sign attached with device #2 verifies under the counter-signer's materialized enrolled key (`verify_countersig(event, prior_state).is_ok()`).
(b) `CommunityInvitePayload` round-trips with a new `inviter_enrollment: Option<EnrollmentCert>` field, and decode of a payload WITHOUT it yields `None` (back-compat).

- [ ] **Step 2: Run — expect FAIL.**

- [ ] **Step 3: Add a device-key countersign helper**

```rust
pub fn attach_countersig_with_device_key(
    event: &SignedMembershipEvent,
    signer_owner: OwnerAddr,
    signer_key: &ed25519_dalek::SigningKey,
) -> Result<SignedMembershipEvent, CryptoError> {
    let bytes = canonical_cbor_encode(&EventPayload::from(event))?;
    let sig = signer_key.sign(&bytes).to_bytes();
    let mut out = event.clone();
    out.countersig = Some(CounterSignature { signer: signer_owner, sig });
    Ok(out)
}
```
(Keep `attach_countersig_with_identity` if other callers remain; the community counter-sign path switches to the new helper.)

- [ ] **Step 4: Invite payload carries inviter cert**

Add to `CommunityInvitePayload`:
```rust
    /// ZEB-339: the inviter's Master EnrollmentCert, so a joiner who has not
    /// yet synced the community log can verify the inviter's owner->device
    /// binding (and thus the InviteToken signature) at first contact.
    #[serde(rename = "ec", skip_serializing_if = "Option::is_none", default)]
    pub inviter_enrollment: Option<EnrollmentCert>,
```
Add `use harmony_owner::certs::enrollment::EnrollmentCert;`. Populate it in `generate_invite` (lib.rs) when building the payload. Relax the invite-only field-presence validation in `encode_invite_url`/`decode_invite_url` to require `inviter_enrollment` for invite-only payloads.

- [ ] **Step 5: Switch the invite-path verification**

In `verify_packet_pure` (and/or `handle_unicast`), the inner `join_event` membership sig was checked via `verify_signature(&signed.join_event, &signed.joiner_identity_pub)`. Replace with cert-based verification using the join_event's own carried `enrollment`:
```rust
    let signer = community_membership::enrolled_key_from_cert(&signed.join_event)
        .map_err(|_| CommunityInviteVerifyError::JoinEventSigInvalid)?;
    community_membership::verify_membership_signer(&signed.join_event, &signer)
        .map_err(|_| CommunityInviteVerifyError::JoinEventSigInvalid)?;
```
(Add `JoinEventSigInvalid` to `CommunityInviteVerifyError` if no equivalent exists; reuse the existing variant otherwise.) The redeem flow (lib.rs) must attach the joiner's cert to `signed.join_event.enrollment` before sending (Task 7 attaches it at mint; confirm it survives into `CommunityInviteSigned`). `joiner_identity_pub`/`signing_device_hash` checks (transport binding) stay as-is. The counter-sign attach in `handle_unicast` switches to `attach_countersig_with_device_key` using `outbox_g.community_signing_key` + the local owner addr.

- [ ] **Step 6: Run scoped tests — expect PASS**; `-E 'test(invite) + test(countersig)'`.

- [ ] **Step 7: fmt + clippy (lib) + commit**

```bash
git commit -am "feat(zeb-339): counter-sign with device #2; invite payload carries inviter cert; verify join_event via cert"
```

---

## Task 9: Publisher-auth via materialized membership; remove resolver from community verify path

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (engine config/struct `signing_key` ~797/960; `insert_local_event` 1341–1376; `insert_event_with_resolved_pubs` 1411–1545; `handle_incoming_publish` 3120–3194)
- Modify: `src-tauri/src/lib.rs` (engine config construction — pass device #2 as `signing_key`)

- [ ] **Step 1: Resolve the resolver-field fate (grep)**

Run:
```bash
cd src-tauri && grep -rn 'identity_resolver' src/ | grep -v test
grep -rn 'OwnerDeviceCacheResolver' src/ | grep -v test
```
Determine whether `identity_resolver` is consumed by anything other than the community membership/publish verify we are about to change (e.g. the voting-log engine at lib.rs:~25019). Record the finding in the commit message. If the voting engine is a SEPARATE engine type, the `CommunitySyncEngineConfig.identity_resolver` field can be removed; if voting reuses `CommunitySyncEngine`, KEEP the field (set to `None` for community) and just stop calling it in the membership/publish paths.

- [ ] **Step 2: Write failing publisher-auth test**

A member's publish (`CommunityRootPublishPayload` signed with device #2) verifies against that member's materialized `enrolled_device_keys`; a publish whose `publisher_sig` is signed by a non-enrolled key, or whose `publisher_addr` is not a `Joined` member, is rejected (`PublisherSigInvalid`/`UnknownPublisher`). Drive `handle_incoming_publish` or a focused helper.

- [ ] **Step 3: Run — expect FAIL.**

- [ ] **Step 4: Engine signing key → device #2**

In `lib.rs` where `CommunitySyncEngineConfig { signing_key: …, .. }` is built, pass `community_signing_key_arc.clone()` (device #2) instead of the Reticulum `signing_key_arc`. (Publisher sig is the only use of `ctx.signing_key`.)

- [ ] **Step 5: Publisher-auth verify via membership**

In `handle_incoming_publish`, the membership-at-HLC gate (lines 3030–3118) already materializes membership and confirms `publisher_addr` is `Joined` at `payload.at`. Replace the resolver lookup + `Identity::from_public_bytes(publisher_pub).address_hash == publisher_addr` check (3120–3194) with: fetch the publisher's `MemberState.enrolled_device_keys` from that materialized state and verify `payload.publisher_sig` over `canonical_cbor_encode(&CommunityRootSignedPayload::from(&payload))` against any key in the set; reject as `PublisherSigInvalid` if none match. Remove the `ctx.identity_resolver` call here.

- [ ] **Step 6: Stop resolving actor/countersigner pubs in the engine insert path**

In `insert_local_event` (1341–1376): delete the `resolver.resolve(&event.actor)` and `resolver.resolve(&cs.signer)` calls; call `insert_event_with_resolved_pubs(event)` (now without pub args) — or inline into a single `insert_event` that builds the slimmed `VerifyContext { expected_community_id, admin_addr, is_invite_only }` and calls `state_g.insert_event(event, &ctx)`. The `admin_first_seen` / `bind_admin_identity_pub` machinery (1432–1459) keyed on `actor_pub` is no longer needed for verification — remove the admin-pub binding if `admin_identity_pub` has no remaining consumer (it fed `VerifyContext.admin_identity_pub`, now gone; grep `admin_identity_pub` to confirm and delete the `OnceLock` field + `bind_admin_identity_pub` if dead). Keep the `community_id` guard and the post-insert hooks.

- [ ] **Step 7: Run publisher-auth + insert tests — expect PASS**; scope `-E 'test(publish) + test(insert_local)'`.

- [ ] **Step 8: fmt + clippy (lib) + commit**

```bash
git commit -am "feat(zeb-339): publisher-auth via materialized enrolled keys; engine signs with device #2; drop resolver from community verify path"
```

---

## Task 10: Migrate all existing tests + 9 `VerifyContext` sites (RISK GATE — blast radius)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (test module), `src-tauri/src/lib.rs` (9 `VerifyContext` sites + community tests), `src-tauri/src/community_invite.rs` (tests), `src-tauri/src/community_state_sync.rs` (tests), `src-tauri/tests/*` integration tests touching community membership.

- [ ] **Step 1: Enumerate the blast radius**

Run:
```bash
cd src-tauri && grep -rn 'actor_identity_pub\|countersigner_identity_pub\|admin_identity_pub' src/ tests/ | wc -l
grep -rln 'VerifyContext' src/ tests/
grep -rn 'signing_key_from_identity\|OwnerAddr(.*\.address_hash)' src/ tests/ | wc -l
```
This is the migration surface. If it exceeds what fits in a 10-min wall-clock per file, split the migration across this task's commits per-file and surface progress; do NOT stall.

- [ ] **Step 2: Migrate the 9 lib.rs `VerifyContext` literals**

Each currently sets `actor_identity_pub`/`countersigner_identity_pub`/`admin_identity_pub`. Remove those three fields from each literal so it matches the slimmed struct:
```rust
VerifyContext {
    expected_community_id: …,
    admin_addr: …,
    is_invite_only: …,
}
```
For tests that fed a hand-built `identity_pub`, switch the test to `mint_test_owner` so the event's `actor = owner`, the event is signed by `device_key`, and (for Join) carries `cert`; for steady-state events, ensure the actor's prior Join (with cert) is materialized into `prior_state` first so `enrolled_device_keys` is populated.

- [ ] **Step 3: Migrate membership/invite/sync unit tests**

Replace the `let id = PrivateIdentity::from_seed(..); let actor = OwnerAddr(id.identity.address_hash); let sk = signing_key_from_identity(&id);` pattern with `mint_test_owner(seed)` → `{ owner, device_key, cert }`. Un-ignore any tests `#[ignore]`'d in Task 4. Counter-sign tests switch to `attach_countersig_with_device_key` + ensuring the counter-signer is a materialized member with their enrolled key.

- [ ] **Step 4: Run the full membership + invite + sync test modules — expect PASS** (foreground, `timeout 600`)

```bash
cd src-tauri && set -o pipefail && \
  cargo nextest run --locked --features test-fixtures \
  -E 'test(community)' 2>&1 | tail -60
```
Expected: 0 failures beyond the Task 0 orphan baseline. Any remaining failure is in-scope and must be fixed here.

- [ ] **Step 5: Delete now-dead variants/helpers**

Once no test references them, delete `VerifyError::{ActorPubkeyMismatch, CounterSignerPubkeyMismatch, InvalidIdentityPub, PendingJoinJoinerPubMismatch}` and the old `verify_signature`/`verify_countersig(_, &[u8;64])` signatures (if fully replaced). Run clippy to confirm no dead-code warnings.

- [ ] **Step 6: fmt + clippy (`--all-targets`) + commit**

```bash
cd src-tauri && cargo fmt --all && set -o pipefail && \
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -30
git commit -am "test(zeb-339): migrate community tests + VerifyContext sites to enrolled-device model; drop dead variants"
```

---

## Task 11: Cross-owner e2e + negative tests + regression guard

**Files:**
- Create/Modify: `src-tauri/src/community_membership.rs` tests (or a new `src-tauri/tests/zeb_339_enrolled_signing_integration.rs`)

- [ ] **Step 1: Cross-owner end-to-end test (spec §9.2)**

Two distinct `mint_test_owner` identities (creator + joiner). Exercise create → generate invite (carries inviter cert) → joiner PendingJoin (carries joiner cert, token verified via creator's enrolled key) → counter-sign (device #2) → join. Each side accepts the other purely from carried certs / materialized keys — NO shared cache / resolver. Assert final materialized membership has both as `Joined` with their enrolled keys.

- [ ] **Step 2: Negative tests, one per error variant (spec §9.3)**

`EnrollmentCertInvalid` (forged master sig), `EnrollmentOwnerMismatch` (`cert.owner_id != actor`), `SignerNotEnrolledForActor` (steady-state event whose signer key isn't in actor's set), `MissingEnrollmentCert` (Join with `enrollment: None`), `SignatureInvalid` (tampered event bytes), `CounterSignerNotEnrolled` (counter-sig from a non-member). One `#[test]` each, asserting the exact variant.

- [ ] **Step 3: Regression guard (spec §9.6)**

A structural test (mirroring the ZEB-338 phrasing-regression guard) asserting community signing consumes the device key, not the Reticulum key. Practical form: assert that an event produced by the create/mint path verifies under the device #2 cert's key but NOT under a separately-constructed Reticulum-style key — i.e. encode the invariant `actor != address_hash(community signing key)` and that verification still succeeds via the cert. Document the intent in a comment so a future refactor that reverts to Reticulum signing breaks this test.

- [ ] **Step 4: Run — expect PASS**; `-E 'test(zeb_339) + test(cross_owner) + test(enrolled)'`.

- [ ] **Step 5: commit**

```bash
git commit -am "test(zeb-339): cross-owner e2e + per-variant negative tests + signing-key regression guard"
```

---

## Task 12: Wire-format fixtures

**Files:**
- Modify/Create: `src-tauri/tests/wire_format_*` (find the membership-event fixture test: `grep -rln 'SignedMembershipEvent' src-tauri/tests/`)

- [ ] **Step 1: Pin a Join-with-cert fixture + a steady-state-without-cert fixture**

Using the deterministic `--features test-fixtures` helpers, encode (a) a bootstrap Join carrying an `EnrollmentCert` and (b) a Leave with `enrollment: None`. Commit the CBOR bytes as fixtures and assert byte-stability + decode round-trip.

- [ ] **Step 2: Confirm old fixtures still decode**

Assert the pre-ZEB-339 membership-event fixture (without `en`) and a pre-ZEB-339 `MemberState`/`MaterializedMembership` snapshot (without `ek`) still decode (serde `default`). If an existing pinned fixture's byte layout changed because a struct gained a field that serializes by default, regenerate that fixture and note the wire bump in the commit message (this is a pre-1.0 alpha; no migration needed — no communities exist yet).

- [ ] **Step 3: Run wire-format tests — expect PASS**; `-E 'test(wire_format)'` (`--features test-fixtures`).

- [ ] **Step 4: commit**

```bash
git commit -am "test(zeb-339): pin Join-with-cert + steady-state wire fixtures; confirm back-compat decode"
```

---

## Task 13: Final gate sweep + push + PR

**Files:** none (verification + ship)

- [ ] **Step 1: Backend full sweep (foreground, `timeout 600` each, COMMIT already done)**

```bash
cd src-tauri && set -o pipefail
cargo fmt --all -- --check 2>&1 | tail -5
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -60
HARMONY_LARGE_TESTS=1 cargo nextest run --locked --features test-fixtures -E 'test(folder_ingest_walker_integration)' 2>&1 | tail -20
```
Expected: fmt clean; clippy 0 warnings; nextest 0 NEW failures beyond Task 0 orphan baseline; large-tests pass. If any gate exceeds 10 min, surface `DONE_WITH_CONCERNS`.

- [ ] **Step 2: MSRV check**

```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -20
```
(Use the declared MSRV toolchain if CI pins one; otherwise stable.)

- [ ] **Step 3: Frontend gates (no frontend change expected, but CI runs them)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && npx tsc --noEmit 2>&1 | tail -10 && npx vitest run 2>&1 | tail -20
```

- [ ] **Step 4: Push**

```bash
git push -u origin zeb-339-community-enrolled-device-signing
```

- [ ] **Step 5: Create the PR**

PR title: `ZEB-339: community membership signs with enrolled device key + EnrollmentCert verification`

PR body must reference (markdown-linked, per `feedback_linear_pr_auto_close`):
- Spec commit `6bbb407` + path `docs/specs/2026-05-29-zeb-339-community-membership-enrolled-device-signing-design.md`
- Plan commit + path `docs/plans/2026-05-29-zeb-339-community-enrolled-device-signing-plan.md`
- `[ZEB-339](https://linear.app/zeblith/issue/ZEB-339)` (child of `[ZEB-217](https://linear.app/zeblith/issue/ZEB-217)`)
- Summary: the three-identity root cause; sign-with-device-#2 + cert-verify fix; the "Plan clarifications" decisions (esp. invite-path scope, resolver retained for voting); what's deferred (DM/owner-state unification follow-up).
- Test plan checklist: the 6 spec §9 test classes + the 5 CI jobs (fmt+clippy / nextest / large-tests / MSRV / frontend).

End the PR body with the Claude Code footer.

- [ ] **Step 6: Enter the autonomous bot-review loop** (handled by the controller per `feedback_autonomous_pr_monitoring_loop` — not a subagent step).

---

## Self-review (against the spec)

- **§1 root cause / §1.3 CI gap** → Task 4 Step 1 (production-pairing test, fail-then-pass). ✓
- **§2.1 sign with device #2** → Tasks 6 (carry key), 7 (mint sites), 9 (publish). ✓
- **§3 trust chain** → Task 3 (`enrolled_key_from_cert` does cert.verify + owner==actor; `verify_membership_signer` does the sig). ✓
- **§4 signing switch wiring** → Tasks 6, 7, 9. ✓
- **§5.1 `enrollment` field** → Task 1. ✓ **§5.2 PendingJoin pub removal** → Task 5. ✓ **§5.3 invite carries inviter cert** → Task 8 Step 4. ✓
- **§6.1 primitive / §6.2 VerifyContext slim / §6.3 error taxonomy** → Tasks 3, 4. ✓ **§6.4 countersign** → Tasks 4 Step 5, 8 Step 3. ✓ **§6.5 InviteToken resolution** → Task 5 Step 4. ✓
- **§7 enrolled_device_keys + materialize** → Task 2. ✓
- **§8 publisher-auth** → Task 9. ✓
- **§9 testing (all 6 classes)** → Tasks 4/11 (1,2), 11 (3), 12 (4), 9/11 (5), 11 (6). ✓
- **§10 out-of-scope** → DM/owner-state untouched (DmOutbox.signing_key + private_identity kept Reticulum); follow-up ticket filed post-PR by controller. ✓ Quorum certs: `enrolled_key_from_cert` calls `cert.verify()` which structurally checks Quorum but defers full verify — single-device alpha only mints Master certs, so Quorum is untested-but-not-panicking. ✓
- **Type consistency:** `EnrolledDeviceKey { owner, device_ed25519 }`, `verify_membership_signer`, `enrolled_key_from_cert`, `resolve_enrolled_signer`, `verify_countersig(event, prior_state)`, `attach_countersig_with_device_key`, `MemberState.enrolled_device_keys: BTreeSet<[u8;32]>`, `SignedMembershipEvent.enrollment: Option<EnrollmentCert>`, `CommunityInvitePayload.inviter_enrollment` — names used consistently across tasks. ✓
- **Placeholder scan:** no TBD/TODO; every code step has concrete code or an exact transformation + cited source location. The one judgment point (resolver-field fate) is resolved by a deterministic grep in Task 9 Step 1. ✓

**Risk gates flagged:** Task 2 (shared test helper must land first) and Task 10 (test-migration blast radius) — both carry `DONE_WITH_CONCERNS` escape hatches rather than open-ended stalls.
