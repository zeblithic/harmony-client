# ZEB-668 S3 — Community retire-announce Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Communities learn that an owner's device was revoked and stop accepting its signatures — a new `DeviceRetire` membership event carries the `RevocationCert` proof, any surviving enrolled device relays it through the existing device-intro fleet dataset, and receivers remove the retired key from `enrolled_device_keys` with remove-wins tombstone semantics.

**Architecture:** Three layers, mirroring the ZEB-495 device-intro machinery it extends. (1) `community_membership.rs` gains a `MembershipEventKind::DeviceRetire { revocation, enrollment }` variant whose verify gate proves the cert pair against the actor's owner binding and whose materialize arm removes + tombstones the retired key. (2) The existing `community-device-intro` fleet dataset carries retire entries under a distinct key suffix; the relay sweeper's kind-assertion admits the new kind. (3) A new level-triggered deposit sweeper (`community_device_retire_deposit.rs`) diffs `OwnerState.revocations` against the dataset and deposits signed retire events for every depositable community — nudged by trust-sync applies, local revokes, and one startup pass.

**Tech Stack:** Rust (tokio, serde/CBOR canonical, ed25519-dalek), harmony-owner cert types. No frontend changes.

## Global Constraints

- Branch `zeb-668-s3-retire-announce` off main `81573123`. One PR; Jake merges.
- Additive wire fields ONLY — `#[serde(default)]` / `skip_serializing_if`, **no persisted-file version bumps** (spec §4; content-index lesson). Intro dataset stays `SCHEMA_V1`.
- Same-length-keys invariant per CBOR map nesting level: new variant code is 1-char (`"t"`), inner field keys 2-char (`rc`, `ec`).
- 30-day TTL + grow-only `relayed_by` + coverage GC: reuse `community_device_intro_ingest.rs` machinery untouched except the kind-assertion.
- Gates per task: `scripts/test-select --context task` (paste `round=…/bucket=…`), `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`. Final: full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` + `npx tsc --noEmit` + `npx vitest run` (frontend untouched but gate anyway).
- Keychain: NOT touched by this slice (no identity persistence changes).
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

## Design decisions locked (spec §4 conformance notes)

1. **Entry struct reuse, not a new `CommunityDeviceRetireEntry`.** Spec §4 names a new entry kind `{owner_id, device_id, revocation_cert, relayed_by, ttl}`. The as-built dataset (`CommunityDeviceIntroEntry`) wraps a `SignedMembershipEvent` — owner_id/device_id/revocation_cert all live INSIDE the signed event, and TTL is derived from `deposited_at` + `INTRO_TTL_MS`. Reusing the entry struct under a distinct map key (`retire_key` = intro key + `":r"` suffix) is the mechanism-true reading: it inherits relay/coverage/GC verbatim (which the spec explicitly demands) and adds zero wire surface. A second struct would duplicate the whole sweeper.
2. **The retire event carries the retired device's `EnrollmentCert` too.** `RevocationCert.target` is a 16-byte identity hash; communities store 32-byte ed25519 keys with no hash→key map. The Master-signed `EnrollmentCert` (retained in `OwnerState.enrollments` after revocation — rows are never deleted) binds `device_id → device_pubkeys.classical.ed25519_verify`, identifying exactly which key to remove, and supplies the verifying key for SelfDevice-issued revocation certs.
3. **Remove-wins tombstones.** `materialize` is a deterministic full replay in `event_sort_key` order. Clock skew can sort a retire BEFORE the announce it retires — every replica would then converge on the retired key being re-added. New `MemberState.revoked_device_keys` tombstone set (additive, `#[serde(default)]`): the retire arm removes + tombstones; every key-ADDING arm refuses tombstoned keys. Order-proof.
4. **EnrollmentCert verified at its own `issued_at`, not event time.** `EnrollmentCert::verify(now_secs)` enforces `expires_at`. Retiring a long-lost device whose cert has since expired must work — expiry gates a key's authority to ACT, which is irrelevant to removing it. The signature binding is what's load-bearing; verifying at `issued_at` (always within validity) checks exactly that.
5. **Actor must be a member — ANY status.** DeviceAnnounce requires Joined; retire is subtractive. A Left/Banned owner's stale compromised key must still be retirable (member state and enrolled keys persist across Leave, and a rejoin would otherwise resurrect the key's authority). No power level required: the authority is the carried cert, not the community ladder.
6. **Deposit for every depositable community, no retired-key-enrolled precondition.** Depositable = engine spawned + owner Joined + THIS device's key enrolled (so the event verifies at receivers). We do NOT require the retired key to be enrolled there — checking would race the in-flight announce (skip → announce lands later → revoked key accepted); the tombstone makes deposit-always safe, and receivers no-op removal + tombstone for never-announced keys.
7. **Boot-time enrolled-set filter.** The intro sweeper's coverage GC waits for `relayed_by ⊇ enrolled_device_ids`; a revoked device never relays, so every entry would stall to TTL. Filter revoked devices out of the boot-time `enrolled` snapshot. (Live-set refresh is a follow-up, noted in §9 filing.)

## File structure

- Modify: `src-tauri/src/community_membership.rs` — variant, tombstone field + cap, verify gate + helper, materialize arm, VerifyError variants, tests.
- Modify: `src-tauri/src/community_device_intro_crdt.rs` — `retire_key` helper + test.
- Modify: `src-tauri/src/community_device_intro_ingest.rs` — kind-assertion relax + test.
- Create: `src-tauri/src/community_device_retire_deposit.rs` — deposit sweeper (ctx trait, core sweep, task loop, prod ctx) + tests.
- Modify: `src-tauri/src/community_state_sync.rs` — `CommunitySyncRegistry::community_ids()`.
- Modify: `src-tauri/src/lib.rs` — enrolled-set filter, retire nudge channel + dual on_applied, sweeper spawn, `NodeState.community_device_retire_nudge`, module decl.
- Modify: `src-tauri/src/owner_commands.rs` — local-revoke nudge.

---

### Task 1: Membership core — DeviceRetire variant, tombstones, verify, materialize

**Files:**
- Modify: `src-tauri/src/community_membership.rs`

**Interfaces:**
- Produces: `MembershipEventKind::DeviceRetire { revocation: RevocationCert, enrollment: EnrollmentCert }` (variant code `"t"`, inner keys `rc`/`ec`); `MemberState.revoked_device_keys: BTreeSet<[u8;32]>` (`rk`); `VerifyError::{DeviceRetireForNonMember, DeviceRetireCertInvalid}`; `MAX_REVOKED_DEVICE_KEY_TOMBSTONES`.
- Consumes: `harmony_owner::certs::{RevocationCert, RevocationReason, RevocationIssuer, EnrollmentCert, EnrollmentIssuer}`.

- [ ] **Step 1: Extend the import** at `community_membership.rs:10`:

```rust
use harmony_owner::certs::{EnrollmentCert, RevocationCert};
```

- [ ] **Step 2: Add the variant** after `DeviceAnnounce` (`:354-355`), inside `MembershipEventKind`:

```rust
    /// ZEB-668 S3: retire-announce. A surviving enrolled device of `actor`
    /// broadcasts that one of the owner's devices has been REVOKED, carrying
    /// the proof (`RevocationCert`) plus the retired device's Master
    /// `EnrollmentCert` — the cert that binds the 16-byte revocation target
    /// to the 32-byte ed25519 key communities actually store (there is no
    /// hash→key map on the receiving side). On materialize the key is
    /// removed from `enrolled_device_keys` AND tombstoned in
    /// `revoked_device_keys` (remove-wins: no replay order can re-add it).
    ///
    /// Signer: any surviving enrolled device — steady-state
    /// `resolve_enrolled_signer`, NOT the cert side-channel path (the `en`
    /// side-channel stays None; the carried certs describe the RETIRED
    /// device, not the signer). Authorization: actor exists in `members`
    /// (ANY status — removal is subtractive; a Left owner's compromised key
    /// must still be retirable) and the cert pair proves itself
    /// (`verify_device_retire_certs`). No power level.
    ///
    /// Variant code "t" (1-char value, unused before this). Inner field
    /// keys are 2-char (rc, ec) per the same-length-keys invariant at this
    /// nesting level; the embedded harmony-owner certs are opaque CBOR maps
    /// below it (same as the `en` side-channel precedent).
    /// See `docs/specs/2026-07-11-zeb-668-device-management-design.md` §4.
    #[serde(rename = "t")]
    DeviceRetire {
        #[serde(rename = "rc")]
        revocation: RevocationCert,
        #[serde(rename = "ec")]
        enrollment: EnrollmentCert,
    },
```

- [ ] **Step 3: `cargo check` to enumerate every exhaustive match.** Run `cd src-tauri && cargo check --locked --features test-fixtures 2>&1 | grep -A2 "non-exhaustive"`. Expected sites (fix each as below; any additional site the compiler finds gets a no-op arm with a one-line ZEB-668 S3 comment):
  - `verify_event` step-4 kind gate (the match containing `DeviceAnnounceForNonMember` at `:3144`) → Step 6 arm.
  - `verify_event` step-5 power match (`:3178`) → add `MembershipEventKind::DeviceRetire { .. }` to the same no-power-required arm `DeviceAnnounce` occupies.
  - `materialize` kind match (`:2597`) → Step 8 arm.
  - Any Display/label helpers → literal `"DeviceRetire"`.

- [ ] **Step 4: Add the tombstone field to `MemberState`** (`:1514-1528`, after `enrolled_device_keys`):

```rust
    /// ZEB-668 S3: tombstones for retired (revoked) device keys —
    /// remove-wins. A key present here is NEVER re-added by any key-adding
    /// arm: `materialize` is a deterministic replay in `event_sort_key`
    /// order, and clock skew can sort a DeviceRetire BEFORE the
    /// DeviceAnnounce it retires — without the tombstone every replica
    /// would converge on the retired key resurrected. Additive field:
    /// `#[serde(default)]` + empty-skip keeps pre-S3 blobs and empty-set
    /// encodings byte-identical (no version bump).
    #[serde(rename = "rk", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub revoked_device_keys: BTreeSet<[u8; 32]>,
```

Fix every `MemberState { … }` struct literal the compiler flags with `revoked_device_keys: BTreeSet::new(),` (known: the `materialize` Join arm; test helper `joined_with_first_device` `:12109`; any invite/bootstrap-hint builders).

- [ ] **Step 5: Tombstone cap + sanctioned inserters** next to `insert_enrolled_key_capped` (`:4006`):

```rust
/// ZEB-668 S3: bound on `revoked_device_keys` tombstones per member — 2× the
/// enrolled cap. Tombstones accumulate across the owner's whole rotation
/// history and are never GC'd (that permanence is what makes them
/// replay-order-proof). Minting one requires a valid master- or self-signed
/// RevocationCert, so growth is owner-inflicted, not an attack surface. At
/// the cap new tombstones are dropped — removal still happens; only the
/// re-add guard for the dropped key degrades to order-dependent.
pub const MAX_REVOKED_DEVICE_KEY_TOMBSTONES: usize = 2 * MAX_ENROLLED_DEVICE_KEYS;

/// ZEB-668 S3: capped tombstone insert (mirrors `insert_enrolled_key_capped`).
fn insert_revoked_tombstone_capped(set: &mut BTreeSet<[u8; 32]>, key: [u8; 32]) {
    if set.contains(&key) || set.len() < MAX_REVOKED_DEVICE_KEY_TOMBSTONES {
        set.insert(key);
    }
}

/// ZEB-668 S3: the ONE sanctioned way to add an enrolled device key from a
/// materialize arm. Refuses tombstoned (retired) keys — remove-wins — then
/// applies the ZEB-401 cap. Every key-adding arm (Join, DeviceAnnounce)
/// MUST route through this; a raw `insert_enrolled_key_capped` call would
/// reopen the replay-order resurrection hole.
fn insert_enrolled_key_unless_retired(member: &mut MemberState, key: [u8; 32]) {
    if member.revoked_device_keys.contains(&key) {
        return;
    }
    insert_enrolled_key_capped(&mut member.enrolled_device_keys, key);
}
```

Then replace **every** materialize-arm call of `insert_enrolled_key_capped(&mut member.enrolled_device_keys, K)` with `insert_enrolled_key_unless_retired(member, K)` (grep `insert_enrolled_key_capped(` — known arms: Join, DeviceAnnounce `:2622`; leave the function itself and its direct unit tests untouched).

- [ ] **Step 6: Verify gate.** New `VerifyError` variants (append near `:891`, Display arms mirroring neighbors):

```rust
    /// ZEB-668 S3: DeviceRetire whose actor has no member entry at all.
    /// (ANY status is acceptable — this fires only for never-members.)
    DeviceRetireForNonMember,
    /// ZEB-668 S3: DeviceRetire whose carried cert pair fails verification
    /// or doesn't bind (owner_id ≠ actor, target ≠ enrollment.device_id,
    /// non-Master enrollment issuer, bad signature, Quorum issuer,
    /// oversize Other-reason).
    DeviceRetireCertInvalid,
```

Step-4 kind-gate arm (in the match containing the `DeviceAnnounceForNonMember` arm at `:3144`):

```rust
        MembershipEventKind::DeviceRetire {
            revocation,
            enrollment,
        } => {
            // ZEB-668 S3: subtractive retire. Actor must exist as a member —
            // ANY status; a Left/Banned owner's stale device key must still
            // be retirable (member state and enrolled keys persist across
            // Leave, and a rejoin would otherwise resurrect the key). No
            // power level: the authority is the carried RevocationCert
            // itself, not the community ladder. Signer resolution (step 1,
            // steady-state path) already proved the event is signed by one
            // of the actor's currently-enrolled devices.
            if !prior_state.members.contains_key(&event.actor) {
                return Err(VerifyError::DeviceRetireForNonMember);
            }
            verify_device_retire_certs(&event.actor, revocation, enrollment)?;
        }
```

And the helper (place near `enrolled_key_from_cert` `:1340`):

```rust
/// ZEB-668 S3: validate the cert pair carried by a `DeviceRetire`, proving —
/// with no communal state beyond the actor's OwnerAddr — that:
///
/// 1. `enrollment` is a genuine Master-issued cert for the actor's owner
///    (embedded master key hashes to `owner_id == actor.0`), binding the
///    16-byte `device_id` to the 32-byte ed25519 key communities store.
///    Verified at the cert's own `issued_at`, NOT event time: retire must
///    work for certs that have since EXPIRED (expiry gates a key's authority
///    to act — irrelevant to removing it; the signature binding is what's
///    load-bearing).
/// 2. `revocation` targets exactly that device (`target == device_id`),
///    names the same owner, and its signature verifies: Master-issued certs
///    are self-contained (`verify(None)` checks the embedded master key
///    hashes to `owner_id`); SelfDevice certs verify under the retired
///    device's own ed25519 key from the enrollment cert. Quorum issuers are
///    rejected (not implemented crate-side either).
/// 3. An `Other(reason)` string is capped — same DoS posture as moderation
///    reasons.
fn verify_device_retire_certs(
    actor: &OwnerAddr,
    revocation: &RevocationCert,
    enrollment: &EnrollmentCert,
) -> Result<(), VerifyError> {
    use harmony_owner::certs::{EnrollmentIssuer, RevocationIssuer, RevocationReason};
    if enrollment.owner_id != actor.0
        || revocation.owner_id != actor.0
        || revocation.target != enrollment.device_id
    {
        return Err(VerifyError::DeviceRetireCertInvalid);
    }
    if !matches!(enrollment.issuer, EnrollmentIssuer::Master { .. }) {
        return Err(VerifyError::DeviceRetireCertInvalid);
    }
    if enrollment.verify(enrollment.issued_at).is_err() {
        return Err(VerifyError::DeviceRetireCertInvalid);
    }
    if let RevocationReason::Other(s) = &revocation.reason {
        if s.chars().count() > MAX_MODERATION_REASON_CHARS {
            return Err(VerifyError::DeviceRetireCertInvalid);
        }
    }
    let ok = match &revocation.issuer {
        RevocationIssuer::Master { .. } => revocation.verify(None).is_ok(),
        RevocationIssuer::SelfDevice => {
            let retired_vk = enrollment.device_pubkeys.classical.ed25519_verify;
            match ed25519_dalek::VerifyingKey::from_bytes(&retired_vk) {
                Ok(vk) => revocation.verify(Some(&vk)).is_ok(),
                Err(_) => false,
            }
        }
        RevocationIssuer::Quorum { .. } => false,
    };
    if ok {
        Ok(())
    } else {
        Err(VerifyError::DeviceRetireCertInvalid)
    }
}
```

(Adapt the `ed25519_dalek::VerifyingKey` path to the file's existing import idiom; `MAX_MODERATION_REASON_CHARS` already exists per ZEB-649.)

- [ ] **Step 7: Step-1 signer routing — verify no change needed.** The step-1 match (`:2818-2830`) routes `Join | PendingJoin | DeviceAnnounce` through `enrolled_key_from_cert` and everything else through `resolve_enrolled_signer`. `DeviceRetire` correctly lands in the `_ =>` steady-state arm. Confirm by reading; do not edit.

- [ ] **Step 8: Materialize arm** (after DeviceAnnounce `:2629`):

```rust
            MembershipEventKind::DeviceRetire {
                revocation: _,
                enrollment: cert,
            } => {
                // ZEB-668 S3: remove-wins retire — remove the retired key
                // AND tombstone it so a DeviceAnnounce sorting after this
                // event in the deterministic replay can never re-add it.
                //
                // SECURITY INVARIANT (mirrors DeviceAnnounce): the cert pair
                // was verified by verify_event → verify_device_retire_certs;
                // this arm trusts the binding and must never panic on a
                // malformed replayed event, hence the defensive get_mut.
                // ANY member status qualifies (subtractive op).
                if let Some(member) = m.members.get_mut(&event.actor) {
                    let vk = cert.device_pubkeys.classical.ed25519_verify;
                    member.enrolled_device_keys.remove(&vk);
                    insert_revoked_tombstone_capped(&mut member.revoked_device_keys, vk);
                }
            }
```

- [ ] **Step 9: Tests** (in the existing test mod, reusing `mint_test_owner` / `mint_second_device` / `make_device_announce` / `joined_with_first_device` / `VerifyContext` idioms from the DeviceAnnounce tests at `:12134-12760`). Add fixture + tests:

```rust
    /// ZEB-668 S3: mint a RevocationCert for the SECOND device (the one
    /// `mint_second_device(master_seed, device_seed)` created). Master- or
    /// self-issued per `by_master`.
    fn mint_revocation_for_second_device(
        master_seed: u8,
        device2_sk: &ed25519_dalek::SigningKey,
        cert2: &EnrollmentCert,
        by_master: bool,
    ) -> RevocationCert {
        use harmony_owner::certs::RevocationReason;
        use harmony_owner::pubkey_bundle::{ClassicalKeys, PubKeyBundle};
        let master_sk = ed25519_dalek::SigningKey::from_bytes(&[master_seed; 32]);
        let master_bundle = PubKeyBundle {
            classical: ClassicalKeys {
                ed25519_verify: master_sk.verifying_key().to_bytes(),
                x25519_pub: [0u8; 32],
            },
            post_quantum: None,
        };
        if by_master {
            RevocationCert::sign_master(
                &master_sk,
                master_bundle,
                cert2.device_id,
                1_700_000_100,
                RevocationReason::Lost,
            )
            .expect("sign_master revocation")
        } else {
            RevocationCert::sign_self(
                device2_sk,
                cert2.owner_id,
                cert2.device_id,
                1_700_000_100,
                RevocationReason::Decommissioned,
            )
            .expect("sign_self revocation")
        }
    }

    /// ZEB-668 S3: a DeviceRetire for the second device, signed by the
    /// FIRST (surviving) device's key. Steady-state signer — no `en`
    /// side-channel.
    fn make_device_retire(
        owner: &TestOwner,
        community_id: SpaceId,
        revocation: RevocationCert,
        cert2: &EnrollmentCert,
        wall_ms: u64,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [0xDE; 16],
            community_id,
            kind: MembershipEventKind::DeviceRetire {
                revocation,
                enrollment: cert2.clone(),
            },
            actor: owner.owner,
            at: Hlc {
                wall_ms,
                logical: 0,
                device_id: "device1".into(),
            },
        };
        sign_event(&payload, &owner.device_key).expect("sign DeviceRetire")
    }
```

Test list (each is a distinct `#[test]`; assert the exact `VerifyError` variant where applicable):
1. `verify_event_accepts_master_signed_device_retire` — `joined_with_first_device` prior, second device announced first (or key seeded), master-issued cert → `Ok(())`.
2. `verify_event_accepts_self_signed_device_retire` — SelfDevice-issued cert → `Ok(())`.
3. `verify_event_rejects_device_retire_from_non_member` — empty prior → `DeviceRetireForNonMember`.
4. `verify_event_rejects_device_retire_with_wrong_owner_binding` — cert pair minted for a DIFFERENT master seed → `DeviceRetireCertInvalid`.
5. `verify_event_rejects_device_retire_with_mismatched_target` — revocation cert targets a third device id → `DeviceRetireCertInvalid`.
6. `verify_event_rejects_device_retire_with_tampered_revocation_sig` — flip a signature byte → `DeviceRetireCertInvalid`.
7. `verify_event_accepts_device_retire_for_left_member` — prior with `status: MemberStatus::Left` (any-status rule) → `Ok(())`.
8. `materialize_device_retire_removes_and_tombstones_key` — log `[join, announce, retire]` → key absent from `enrolled_device_keys`, present in `revoked_device_keys`.
9. `materialize_announce_after_retire_does_not_resurrect_key` — log where the announce's HLC sorts AFTER the retire's → key stays out (the remove-wins pin).
10. `insert_event_rejects_events_signed_by_retired_key` — build `CommunityState`, insert join+announce+retire, then a `Leave` signed by device2 → `InsertOutcome::Rejected(SignerNotEnrolledForActor)`.
11. `device_retire_wire_roundtrip` — mirror `device_announce_wire_roundtrip` (`:12602`): canonical-CBOR encode/decode round-trips; assert the map uses variant code `"t"` and keys `rc`/`ec`.
12. `revoked_tombstone_insert_is_capped` — fill to `MAX_REVOKED_DEVICE_KEY_TOMBSTONES`, next insert dropped, re-insert of present key idempotent.
13. `member_state_with_empty_tombstones_encodes_byte_identically` — serialize a pre-S3-shaped `MemberState` (empty `revoked_device_keys`) and assert no `rk` key in the CBOR map (additive-field honesty).

- [ ] **Step 10: Gate + commit.** `cd src-tauri && cargo fmt --all && scripts/test-select --context task` (from repo root; paste `round=…/bucket=…`), clippy `--all-targets`. Commit: `ZEB-668 S3 T1: DeviceRetire membership event — verify, materialize, remove-wins tombstones`.

---

### Task 2: Dataset retire key + relay kind relax + enrolled-set filter

**Files:**
- Modify: `src-tauri/src/community_device_intro_crdt.rs`
- Modify: `src-tauri/src/community_device_intro_ingest.rs`
- Modify: `src-tauri/src/lib.rs` (enrolled snapshot `:4965-4971`)

**Interfaces:**
- Produces: `CommunityDeviceIntroDoc::retire_key(community_id, device_id) -> String`.
- Consumes: `MembershipEventKind::DeviceRetire` (Task 1).

- [ ] **Step 1: `retire_key`** next to `key()` (`community_device_intro_crdt.rs:76-78`):

```rust
    /// ZEB-668 S3: map key for a RETIRE entry — the intro key plus a ":r"
    /// suffix. Distinct from `key()` so a retire never collides with the
    /// same device's still-pending intro entry (insert-once would otherwise
    /// silently drop whichever deposits second). Same `device_id` = 64-hex
    /// ed25519 verify key of the RETIRED device.
    pub fn retire_key(community_id: &SpaceId, device_id: &str) -> String {
        format!("{}:{}:r", hex::encode(community_id.0), device_id)
    }
```

(Match `key()`'s exact community-hex expression — read it first and reuse verbatim.) Test: `retire_key_is_intro_key_plus_r_suffix` asserting `retire_key(c, d) == format!("{}:r", CommunityDeviceIntroDoc::key(c, d))`.

- [ ] **Step 2: Relax the relay kind-assertion** (`community_device_intro_ingest.rs:270-278`):

```rust
        if !matches!(
            signed.kind,
            crate::community_membership::MembershipEventKind::DeviceAnnounce
                | crate::community_membership::MembershipEventKind::DeviceRetire { .. }
        ) {
            return Err(format!(
                "unexpected event kind in community-device-intro dataset: {:?}",
                signed.kind
            ));
        }
```

Update the preceding comment: the dataset's contract is now "device-lifecycle events: `DeviceAnnounce` + `DeviceRetire` (ZEB-668 S3)"; everything else still hard-rejects.

- [ ] **Step 3: Tests** in the ingest test mod (reuse `ProbeCtx`/`make_entry`): `relays_a_pending_device_retire_entry` (entry wrapping a signed `DeviceRetire` event relays; `relayed_by` gains self) and `still_rejects_non_lifecycle_kinds` (a signed `Leave` event stays pending with the assertion error). Build the retire event with Task 1's test fixtures (they are `#[cfg(test)]`-visible within the crate — if not visible across modules, mint inline with the same recipe).

- [ ] **Step 4: Enrolled-set filter** at `lib.rs:4965-4971`. Read the existing expression and add a revocation filter, preserving shape:

```rust
// ZEB-668 S3: exclude REVOKED devices from the coverage set — a revoked
// device never relays, so including it would stall every entry's
// coverage-GC to the 30-day TTL. Boot-time snapshot (live refresh is a
// filed follow-up).
.filter(|(device_id, _)| !loaded.state.is_revoked(**device_id))
```

- [ ] **Step 5: Gate + commit.** fmt, `scripts/test-select --context task`, clippy `--all-targets`. Commit: `ZEB-668 S3 T2: retire entries ride the device-intro dataset — key scheme, relay admit, coverage filter`.

---

### Task 3: Deposit sweeper + wiring

**Files:**
- Create: `src-tauri/src/community_device_retire_deposit.rs`
- Modify: `src-tauri/src/community_state_sync.rs` (registry enumeration)
- Modify: `src-tauri/src/lib.rs` (module decl; nudge channel; dual on_applied; sweeper spawn; `NodeState` field)
- Modify: `src-tauri/src/owner_commands.rs` (local-revoke nudge)

**Interfaces:**
- Produces: `deposit_pending_retires(doc, ctx) -> bool`; `run_community_device_retire_deposit_task(...)`; `CommunityDeviceRetireDepositCtx`; `CommunitySyncRegistry::community_ids() -> Vec<SpaceId>`; `NodeState.community_device_retire_nudge: Option<tokio::sync::mpsc::Sender<()>>`.
- Consumes: Task 1 variant + Task 2 `retire_key`; `OwnerState.{revocations, enrollments}` (harmony-owner); `CommunityDeviceIntroDoc`; intro sweeper donor patterns.

- [ ] **Step 1: Registry enumeration** (`community_state_sync.rs`, near `engine_arc` `:5093`):

```rust
    /// ZEB-668 S3: ids of every currently-spawned community engine. Used by
    /// the retire-deposit sweeper to enumerate candidate communities; each
    /// id is re-resolved via `engine_arc` (an engine may despawn between).
    pub async fn community_ids(&self) -> Vec<SpaceId> {
        self.engines.lock().await.keys().copied().collect()
    }
```

- [ ] **Step 2: The sweeper module** — create `src-tauri/src/community_device_retire_deposit.rs`:

```rust
//! ZEB-668 S3: community retire-announce — deposit side.
//!
//! Level-triggered sweeper that diffs the owner's trust state
//! (`OwnerState.revocations`) against the community-device-intro fleet
//! dataset and deposits ONE signed `DeviceRetire` membership event per
//! (depositable community × revoked device) under
//! `CommunityDeviceIntroDoc::retire_key`. The existing intro relay sweeper
//! (`community_device_intro_ingest`) then drives each entry into its
//! community engine exactly like an intro — grow-only `relayed_by`,
//! coverage/TTL GC unchanged.
//!
//! Level-triggered (state diff, not edge events) so it is restart-proof:
//! one startup pass plus one debounced pass per nudge. Nudge sources:
//! trust-sync applies (a sibling's or remote's revocation replicated in)
//! and local `revoke_device` completion.
//!
//! Depositable community = engine spawned + owner Joined + THIS device's
//! key enrolled there (the preconditions for the event to verify at
//! receivers). Deliberately NO "retired key enrolled there" precondition:
//! that check would race an in-flight DeviceAnnounce; deposit-always is
//! safe because the receiver side is remove-wins (tombstones).

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use harmony_owner::certs::{EnrollmentCert, RevocationCert};

use crate::community_device_intro_crdt::CommunityDeviceIntroDoc;
use crate::community_device_intro_crdt::CommunityDeviceIntroEntry;
use crate::community_membership::{EventPayload, MembershipEventKind, OwnerAddr};
use crate::owner_state_types::{Hlc, SpaceId};

/// Debounce for the deposit sweep (mirrors the intro relay sweeper).
pub const RETIRE_DEPOSIT_DEBOUNCE_MS: u64 = 250;

/// Everything the sweep needs from the outside world, injectable for tests.
#[async_trait]
pub trait CommunityDeviceRetireDepositCtx: Send + Sync {
    /// (revocation, retired-device enrollment) pairs from the owner's trust
    /// state. Pairs whose enrollment record is missing are skipped by the
    /// provider — without the cert there is nothing to bind the key.
    async fn revoked_pairs(&self) -> Vec<(RevocationCert, EnrollmentCert)>;
    /// Communities where THIS device can author a verifiable DeviceRetire:
    /// engine spawned, owner Joined, self key enrolled.
    async fn depositable_communities(&self) -> Vec<SpaceId>;
    /// Reserve the next HLC for this device (monotonic per device).
    async fn next_hlc(&self) -> Hlc;
    /// Sign `payload` with this device's membership signing key and return
    /// the canonical-CBOR bytes of the signed event.
    fn sign_and_encode(&self, payload: &EventPayload) -> Result<Vec<u8>, String>;
    /// This owner's address (event actor).
    fn self_owner(&self) -> OwnerAddr;
}

/// One deposit pass. Returns true when at least one entry was inserted
/// (caller then calls `notify_dirty` on the dataset engine).
pub async fn deposit_pending_retires(
    doc: &Arc<tokio::sync::Mutex<CommunityDeviceIntroDoc>>,
    ctx: &dyn CommunityDeviceRetireDepositCtx,
) -> bool {
    let pairs = ctx.revoked_pairs().await;
    if pairs.is_empty() {
        return false;
    }
    let communities = ctx.depositable_communities().await;
    if communities.is_empty() {
        return false;
    }
    let mut changed = false;
    for community_id in communities {
        for (rc, ec) in &pairs {
            let retired_vk_hex = hex::encode(ec.device_pubkeys.classical.ed25519_verify);
            let key = CommunityDeviceIntroDoc::retire_key(&community_id, &retired_vk_hex);
            {
                let g = doc.lock().await;
                if g.entries.contains_key(&key) {
                    continue;
                }
            }
            let hlc = ctx.next_hlc().await;
            let event_id: [u8; 16] = {
                use rand::RngCore;
                let mut buf = [0u8; 16];
                rand::thread_rng().fill_bytes(&mut buf);
                buf
            };
            let payload = EventPayload {
                id: event_id,
                community_id,
                kind: MembershipEventKind::DeviceRetire {
                    revocation: rc.clone(),
                    enrollment: ec.clone(),
                },
                actor: ctx.self_owner(),
                at: hlc.clone(),
            };
            match ctx.sign_and_encode(&payload) {
                Ok(bytes) => {
                    let mut g = doc.lock().await;
                    // Re-check under the write lock (a sibling merge may
                    // have landed the entry between our peek and now);
                    // insert-once either way.
                    g.entries.entry(key).or_insert_with(|| CommunityDeviceIntroEntry {
                        signed_event: bytes,
                        community_id,
                        deposited_at: hlc,
                        relayed_by: BTreeSet::new(),
                    });
                    changed = true;
                    tracing::info!(
                        community_id = ?community_id,
                        retired = %retired_vk_hex,
                        "ZEB-668 S3: DeviceRetire deposited for relay"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        community_id = ?community_id,
                        error = %e,
                        "ZEB-668 S3: sign/encode of DeviceRetire failed; will retry on next nudge"
                    );
                }
            }
        }
    }
    changed
}

/// Task loop: one startup pass, then one debounced pass per nudge burst.
/// Mirrors `run_community_device_intro_sweeper`. Exits when every nudge
/// sender is dropped.
pub async fn run_community_device_retire_deposit_task(
    doc: Arc<tokio::sync::Mutex<CommunityDeviceIntroDoc>>,
    ctx: Arc<dyn CommunityDeviceRetireDepositCtx>,
    mut nudge_rx: tokio::sync::mpsc::Receiver<()>,
    notify_dirty: Arc<dyn Fn() + Send + Sync>,
    debounce_ms: u64,
) {
    if deposit_pending_retires(&doc, ctx.as_ref()).await {
        notify_dirty();
    }
    while nudge_rx.recv().await.is_some() {
        tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)).await;
        // Drain the burst — one sweep covers every nudge received meanwhile.
        while nudge_rx.try_recv().is_ok() {}
        if deposit_pending_retires(&doc, ctx.as_ref()).await {
            notify_dirty();
        }
    }
}
```

(Read `run_community_device_intro_sweeper` (`community_device_intro_ingest.rs:188-208`) first and mirror its exact drain/debounce idiom if it differs from the above — the intro sweeper is the authority.)

- [ ] **Step 3: Prod ctx** (same file):

```rust
/// Production ctx: trust doc + registry + this device's signing identity.
pub struct ProdCommunityDeviceRetireDepositCtx {
    /// The owner trust doc (harmony-owner `OwnerState`), shared with the
    /// trust sync engine.
    pub trust_doc: Arc<tokio::sync::Mutex<harmony_owner::state::OwnerState>>,
    pub registry: Arc<crate::community_state_sync::CommunitySyncRegistry>,
    /// This device's membership signing key.
    pub signing_key: Arc<ed25519_dalek::SigningKey>,
    /// This device's ed25519 verify key (membership `enrolled_device_keys`
    /// representation).
    pub self_vk: [u8; 32],
    pub self_owner: OwnerAddr,
    /// 64-hex device id for HLC reservation.
    pub device_id: String,
    /// Shared HLC replay tracker (same one the intro deposit uses).
    pub hlc_tracker: Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, Hlc>>>,
}

#[async_trait]
impl CommunityDeviceRetireDepositCtx for ProdCommunityDeviceRetireDepositCtx {
    async fn revoked_pairs(&self) -> Vec<(RevocationCert, EnrollmentCert)> {
        let g = self.trust_doc.lock().await;
        g.revocations
            .iter()
            .filter_map(|rc| {
                // Enrollment rows are never deleted on revoke (S2), so the
                // cert is normally present. A revocation whose enrollment
                // hasn't replicated yet is skipped — nothing to bind the
                // key with; the level-triggered sweep retries on the next
                // nudge once the enrollment merges in.
                g.enrollments.get(&rc.target).map(|ec| (rc.clone(), ec.clone()))
            })
            .collect()
    }

    async fn depositable_communities(&self) -> Vec<SpaceId> {
        let ids = self.registry.community_ids().await;
        let mut out = Vec::new();
        for id in ids {
            let Some(engine) = self.registry.engine_arc(&id).await else {
                continue;
            };
            let state_arc = engine.state();
            let st = state_arc.lock().await;
            let mat = st.materialized(engine.admin_addr());
            if let Some(m) = mat.members.get(&self.self_owner) {
                if m.status == crate::community_membership::MemberStatus::Joined
                    && m.enrolled_device_keys.contains(&self.self_vk)
                {
                    out.push(id);
                }
            }
        }
        out
    }

    async fn next_hlc(&self) -> Hlc {
        let wall_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        crate::dm_outbox::reserve_next_hlc_for_device(&self.hlc_tracker, &self.device_id, wall_ms)
            .await
    }

    fn sign_and_encode(&self, payload: &EventPayload) -> Result<Vec<u8>, String> {
        let signed = crate::community_membership::sign_event(payload, self.signing_key.as_ref())
            .map_err(|e| format!("sign DeviceRetire: {e:?}"))?;
        crate::owner_state_crypto::canonical_cbor_encode(&signed)
            .map_err(|e| format!("encode DeviceRetire: {e:?}"))
    }

    fn self_owner(&self) -> OwnerAddr {
        self.self_owner
    }
}
```

(Adapt lock idioms to the actual `owner_trust_doc` type in `lib.rs` — it is the S1 trust doc `Arc<tokio::sync::Mutex<OwnerState>>` passed to `spawn_trust_applied_task` at `lib.rs:5471`. Adapt the `enrollments`/`revocations` iteration to harmony-owner's actual API: `revocations.iter()` yields `&RevocationCert`, `enrollments` is `BTreeMap<[u8;16], EnrollmentCert>`.)

- [ ] **Step 4: Wiring in `lib.rs`.**
  1. Module decl next to `community_device_intro_ingest`: `pub mod community_device_retire_deposit;`.
  2. `NodeState` field (near `community_device_intro_doc` `:1216-1240`):

```rust
    /// ZEB-668 S3: nudge sender for the community retire-deposit sweeper.
    /// `revoke_device` sends after a successful local revocation so the
    /// retire-announce deposits immediately (best-effort try_send — the
    /// level-triggered sweep is the guarantee, this is just latency).
    pub community_device_retire_nudge: Option<tokio::sync::mpsc::Sender<()>>,
```

  Add `community_device_retire_nudge: None,` to `NodeState::default()`/constructor sites the compiler flags.
  3. At the trust-engine construction (`:5399-5425`): create `let (retire_nudge_tx, retire_nudge_rx) = tokio::sync::mpsc::channel::<()>(1);` before the `FleetSyncConfig`, and replace `on_applied: Some(crate::dm_inbox_ingest::ingest_nudge_on_applied(trust_nudge_tx))` with a closure nudging BOTH:

```rust
                                on_applied: Some({
                                    // ZEB-668 S1 detector + S3 retire-deposit
                                    // sweeper both key off trust applies.
                                    let detector =
                                        crate::dm_inbox_ingest::ingest_nudge_on_applied(
                                            trust_nudge_tx,
                                        );
                                    let retire = crate::community_device_intro_ingest::relay_nudge_on_applied(
                                        retire_nudge_tx.clone(),
                                    );
                                    std::sync::Arc::new(move || {
                                        detector();
                                        retire();
                                    })
                                }),
```

  (Both helpers return `Arc<dyn Fn() + Send + Sync>` non-blocking level-trigger nudgers; if their types differ, read both and reuse whichever fits — worst case inline a `try_send` closure.)
  4. After the intro sweeper spawn (`:5614-5636` area, where `device_intro_*` handles are in scope): build the prod ctx and spawn:

```rust
                    // ── ZEB-668 S3: retire-deposit sweeper ─────────────────
                    let retire_ctx: std::sync::Arc<
                        dyn crate::community_device_retire_deposit::CommunityDeviceRetireDepositCtx,
                    > = std::sync::Arc::new(
                        crate::community_device_retire_deposit::ProdCommunityDeviceRetireDepositCtx {
                            trust_doc: std::sync::Arc::clone(&owner_trust_doc),
                            registry: std::sync::Arc::clone(&device_intro_registry),
                            signing_key: std::sync::Arc::clone(&device_intro_signing_key),
                            self_vk: device_intro_self_key,
                            self_owner: device_intro_self_owner,
                            device_id: device_intro_device_id.clone(),
                            hlc_tracker: std::sync::Arc::clone(&device_intro_hlc_tracker),
                        },
                    );
                    let retire_doc = std::sync::Arc::clone(&device_intro_doc);
                    let retire_engine = std::sync::Arc::clone(&device_intro_engine);
                    tokio::spawn(
                        crate::community_device_retire_deposit::run_community_device_retire_deposit_task(
                            retire_doc,
                            retire_ctx,
                            retire_nudge_rx,
                            std::sync::Arc::new(move || retire_engine.notify_dirty()),
                            crate::community_device_retire_deposit::RETIRE_DEPOSIT_DEBOUNCE_MS,
                        ),
                    );
```

  (The `device_intro_*` variable names above come from the self-introduce hook donor at `lib.rs:6469-6644` and the sweeper spawn at `:5614-5636`; read the spawn site and reuse the ACTUAL in-scope names — some exist only as clones captured for the delta-consumer closure, so clone fresh from the originals at the spawn site.)
  5. Stash the sender into `NodeState` alongside the other handle stashes: `guard.community_device_retire_nudge = Some(retire_nudge_tx);` (same locked-guard block that stores `owner_trust_doc`/`owner_trust_sync` handles).

- [ ] **Step 5: Local-revoke nudge** in `owner_commands.rs::revoke_device_inner` — snapshot the sender in the existing under-lock snapshot block (the one that grabs `owner_trust_doc`/`sync_engine` handles), then after the successful `mutate_trust_state` + `flush_now` (right after the `owner-devices-updated` emit at `:537`):

```rust
    // ZEB-668 S3: nudge the retire-deposit sweeper so the community
    // retire-announce goes out immediately (local revokes don't fire the
    // trust engine's on_applied — that's remote-merge only). Best-effort:
    // the sweeper's startup pass + trust-apply nudges are the guarantee.
    if let Some(tx) = retire_nudge {
        let _ = tx.try_send(());
    }
```

- [ ] **Step 6: Tests** (in `community_device_retire_deposit.rs`; mirror the ingest `ProbeCtx` pattern `community_device_intro_ingest.rs:347-397`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_membership::{
        mint_test_owner, MembershipEventKind, SignedMembershipEvent,
    };
    // ProbeCtx: canned pairs/communities, counting sign calls, deterministic
    // HLC (wall_ms bump per call), signing with mint_test_owner's device key.
    struct ProbeCtx { /* pairs, communities, owner: TestOwner, hlc: AtomicU64 */ }
    // … impl CommunityDeviceRetireDepositCtx for ProbeCtx …
}
```

Test list:
1. `deposits_one_entry_per_community_times_revocation` — 2 communities × 1 pair → 2 entries under `retire_key`, `changed == true`, each `signed_event` decodes to a `DeviceRetire` whose certs match the pair and whose actor is the owner.
2. `no_revocations_is_a_cheap_noop` — empty pairs → false, doc untouched.
3. `existing_entry_is_not_redeposited` — pre-seed one retire key → only the missing community gets an entry; second sweep returns false (idempotent).
4. `sign_failure_leaves_entry_missing_for_retry` — ctx whose `sign_and_encode` errors → false, no entry, no panic.
5. `startup_pass_deposits_then_nudge_drives_followup` — `#[tokio::test(start_paused = true)]` mirror of the intro sweeper's task-loop test: spawn the task with empty pairs, assert no notify; add a pair via shared state, nudge, assert notify fired and entry present (reuse the `wait_until` idiom).
6. In `community_state_sync.rs` tests: `community_ids_lists_spawned_engines` (spawn two engines via the existing test harness idiom, assert both ids returned) — if the existing test harness makes this heavy, fold the assertion into an existing registry test instead.

- [ ] **Step 7: Gate + commit.** fmt, `scripts/test-select --context task`, clippy `--all-targets`. Commit: `ZEB-668 S3 T3: retire-deposit sweeper — trust-state diff → signed DeviceRetire deposits, nudged by trust applies + local revokes`.

---

### Task 4: Full gates, spec ledger, PR

- [ ] **Step 1: Full sweeps.** `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures` then from repo root `npx tsc --noEmit && npx vitest run`. All green (nextest ~4,400 tests; budget ~20 min locally).
- [ ] **Step 2: Amend this plan** with any execution deltas (Execution amendments section).
- [ ] **Step 3: PR.** Push branch; open PR titled `ZEB-668 S3: community retire-announce — DeviceRetire event, remove-wins tombstones, fleet deposit+relay`; body: why (revoked device accepted forever — spec §4), what (three layers), design notes (§Design decisions 1-7 condensed), gates. Fire `@coderabbitai review` ONCE at open. Converge per standing loop.

## Self-review notes

- Spec §4 coverage: entry kind (as key-suffixed reuse — documented deviation), surviving-device relay (deposit sweeper + existing relay), receiver verify + `enrolled_device_keys` removal (T1), rejected-as-unknown (free via `SignerNotEnrolledForActor`, pinned by test 10). ✓
- No placeholders: every step carries code or an exact donor location + adaptation instruction where in-scope variable names can't be known statically (lib.rs wiring). The two "read donor first" notes (sweeper drain idiom, spawn-site variable names) are deliberate: those donors are authoritative and drift-prone.
- Type consistency: `retire_key` consumes the 64-hex ed25519 vk (same as intro `key`); `revoked_pairs` yields owned cert clones; `sign_and_encode` returns canonical CBOR bytes matching `CommunityDeviceIntroEntry.signed_event`. ✓
