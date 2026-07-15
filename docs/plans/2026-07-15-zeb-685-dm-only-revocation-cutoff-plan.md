# ZEB-685 DM-only Device-Revocation Cutoff — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the DM-only device-revocation gap (ZEB-580 S3): when owner A revokes device D, push a master-signed `RevocationCert` (+ paired `EnrollmentCert`) to friends over the DM tunnel; the receiver verifies, stores it union-merged in the owner-state CRDT, and feeds the existing `RevokedDeviceProjection` so the three S2 cutoff sites reject D's DMs to DM-only contacts.

**Architecture:** A new `RevocationPush` DM control frame (no outer signature — certs are self-authenticating + tunnel-peer trust-bind). Receive-side verify/trust-bind → union-merged `revoked_dm_devices` map on `owner_state_crdt::OwnerState` → fed into the same `RevokedDeviceProjection.by_owner`. The three §5.2 cutoff sites are unchanged. Send is best-effort tunnel (deposit-durability is a follow-up).

**Tech Stack:** Rust, `harmony-owner` git dep (`RevocationCert`/`EnrollmentCert`), `ciborium` (canonical CBOR via `owner_state_crypto::canonical_cbor_encode`), `ed25519_dalek`, `cargo-nextest`.

## Global Constraints

- Cargo from `src-tauri/`. Gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Iterative: `scripts/test-select --context task`. Full `--all-targets` sweep only at the end.
- **Additive wire only** — no packet-version byte, no `FILE_VERSION` bump (matches `inviter_enrollment` / `revoked_device_keys` precedent). New `OwnerState` field uses `#[serde(rename=…, skip_serializing_if=…, default)]`.
- **Certs boxed** (`Box<RevocationCert>`, `Box<EnrollmentCert>`) to satisfy clippy `large_enum_variant` (mirrors `inviter_enrollment: Option<Box<EnrollmentCert>>`).
- **Trust-bind is load-bearing** — a friend may only revoke *their own* devices in the receiver's view; never relay third-party revocations.
- **Union-merge, never LWW** for `revoked_dm_devices` — a plain LWW field would drop concurrent revocations across the receiver's own devices.
- Scope: core N1 only. N3 residuals deferred.
- Two distinct `OwnerState` types: `harmony_owner::state::OwnerState` (trust doc: `enrollments`, `revocations`) vs `owner_state_crdt::OwnerState` (CRDT doc: `friend_graph`, `revoked_dm_devices`, `spaces`). Don't conflate.

---

### Task 1: `RevocationPush` DM control frame (wire type)

**Files:**
- Modify: `src-tauri/src/dm_envelope.rs` (add variant to `DmPacket` at ~:207; `encode_packet` ~:306; `decode_packet` ~:486; add a build helper near `build_signed_invite` :401)

**Interfaces:**
- Produces: `DmPacket::RevocationPush { revocation: Box<harmony_owner::certs::RevocationCert>, enrollment: Box<harmony_owner::certs::EnrollmentCert> }`; wire discriminant `0x05`; `pub(crate) fn build_revocation_push_packet(revocation: harmony_owner::certs::RevocationCert, enrollment: harmony_owner::certs::EnrollmentCert) -> DmPacket`.

- [ ] **Step 1: Write the failing round-trip test**

Add to the `#[cfg(test)] mod tests` in `dm_envelope.rs` (mint real certs via the existing test helpers — search the file for how other tests build an `EnrollmentCert`/`RevocationCert`; reuse that helper. If none exists, use `harmony_owner` test constructors — a master `SigningKey`, `sign_master` for the revocation, and the owner's own enrollment):

```rust
#[test]
fn revocation_push_round_trips() {
    let (revocation, enrollment) = super::tests_support::sample_revocation_and_enrollment();
    let pkt = build_revocation_push_packet(revocation.clone(), enrollment.clone());
    let wire = encode_packet(&pkt).expect("encode");
    assert_eq!(wire[0], 0x05, "discriminant");
    let back = decode_packet(&wire).expect("decode");
    match back {
        DmPacket::RevocationPush { revocation: r, enrollment: e } => {
            assert_eq!(*r, revocation);
            assert_eq!(*e, enrollment);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
```

(If no shared cert test-helper exists, inline the construction in the test; name it `sample_revocation_and_enrollment` and place it wherever the file's other tests build certs.)

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(revocation_push_round_trips)'`
Expected: FAIL to compile — `RevocationPush` / `build_revocation_push_packet` undefined.

- [ ] **Step 3: Add the variant**

In `DmPacket` (dm_envelope.rs:207-240), add after `CidNotifyWithBlob`:

```rust
    /// ZEB-685 (S3): a friend-scoped device-revocation push. Carries the
    /// revoking owner's master-signed `RevocationCert` + the paired
    /// `EnrollmentCert` (needed to bridge the cert's `target` device_id[16] to
    /// the revoked ed25519[32] the cutoff projection keys on). No outer frame
    /// signature — both certs are master-signed and the sender is authenticated
    /// by the tunnel-peer bind + `revocation.owner == peer owner` trust-bind
    /// (see dm ingest). Wire: `0x05 || cbor(RevocationPushBody)`.
    RevocationPush {
        revocation: Box<harmony_owner::certs::RevocationCert>,
        enrollment: Box<harmony_owner::certs::EnrollmentCert>,
    },
```

- [ ] **Step 4: Add the CBOR body + build helper**

Near `build_signed_invite` (dm_envelope.rs:401):

```rust
#[derive(serde::Serialize, serde::Deserialize)]
struct RevocationPushBody {
    #[serde(rename = "rv")]
    revocation: harmony_owner::certs::RevocationCert,
    #[serde(rename = "en")]
    enrollment: harmony_owner::certs::EnrollmentCert,
}

/// ZEB-685: construct a `RevocationPush` packet (no outer signature).
pub(crate) fn build_revocation_push_packet(
    revocation: harmony_owner::certs::RevocationCert,
    enrollment: harmony_owner::certs::EnrollmentCert,
) -> DmPacket {
    DmPacket::RevocationPush {
        revocation: Box::new(revocation),
        enrollment: Box::new(enrollment),
    }
}
```

- [ ] **Step 5: Encode/decode arms**

In `encode_packet` (dm_envelope.rs:306), add an early-return arm mirroring `CidNotifyWithBlob` (a length-delimited variant), BEFORE the `(disc, signed_bytes, signature)` match:

```rust
    if let DmPacket::RevocationPush { revocation, enrollment } = packet {
        let body = RevocationPushBody {
            revocation: (**revocation).clone(),
            enrollment: (**enrollment).clone(),
        };
        let cbor = crate::owner_state_crypto::canonical_cbor_encode(&body)
            .map_err(|e| EncodeError::ReSerialize(format!("revocation_push body: {e}")))?;
        let mut out = Vec::with_capacity(1 + cbor.len());
        out.push(0x05);
        out.extend_from_slice(&cbor);
        return Ok(out);
    }
```

In `decode_packet` (dm_envelope.rs:486), add a `0x05` discriminant arm (find where `0x01..0x04` are matched):

```rust
        0x05 => {
            let body: RevocationPushBody =
                crate::owner_state_crypto::canonical_cbor_decode(&bytes[1..])
                    .map_err(|e| DecodeError::Malformed(format!("revocation_push body: {e}")))?;
            Ok(DmPacket::RevocationPush {
                revocation: Box::new(body.revocation),
                enrollment: Box::new(body.enrollment),
            })
        }
```

(Confirm the exact `DecodeError` variant + the canonical-decode fn name by reading the existing `0x04` arm — use whatever `CidNotifyWithBlob` decode uses. If `canonical_cbor_decode` isn't the name, use the same decode call the other arms use.)

- [ ] **Step 6: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(revocation_push_round_trips)'`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/dm_envelope.rs
git commit -m "ZEB-685: RevocationPush DM control frame (wire type)"
```

---

### Task 2: Union-merged `revoked_dm_devices` store on the owner-state CRDT

**Files:**
- Modify: `src-tauri/src/owner_state_crdt.rs` (add field to `OwnerState` :22-77; add `apply_revoked_dm_device` mutator near `apply_friend_update` :905)
- Modify: `src-tauri/src/owner_state_sync.rs` (destructure + union-merge arm in `merge_remote_into_local` :224-360)

**Interfaces:**
- Produces: `OwnerState.revoked_dm_devices: BTreeMap<OwnerAddr, BTreeSet<[u8; 32]>>`; `pub fn apply_revoked_dm_device(&mut self, owner: OwnerAddr, ed25519: [u8; 32]) -> bool` (returns `true` iff newly inserted).

- [ ] **Step 1: Write the failing tests**

Add to `owner_state_crdt.rs` tests:

```rust
#[test]
fn apply_revoked_dm_device_unions() {
    let mut s = OwnerState::default();
    let owner = crate::owner_state_types::OwnerAddr([7u8; 16]);
    assert!(s.apply_revoked_dm_device(owner, [1u8; 32]));
    assert!(!s.apply_revoked_dm_device(owner, [1u8; 32]), "idempotent");
    assert!(s.apply_revoked_dm_device(owner, [2u8; 32]));
    assert_eq!(s.revoked_dm_devices.get(&owner).unwrap().len(), 2);
}
```

Add to `owner_state_sync.rs` tests (find how its tests build two `OwnerState`s and call `merge_remote_into_local`):

```rust
#[test]
fn revoked_dm_devices_merge_is_union_not_lww() {
    use crate::owner_state_types::OwnerAddr;
    let owner = OwnerAddr([7u8; 16]);
    // Two of the receiver's own devices each learned a DIFFERENT revocation.
    let mut local = crate::owner_state_crdt::OwnerState::default();
    local.apply_revoked_dm_device(owner, [1u8; 32]);
    let mut remote = crate::owner_state_crdt::OwnerState::default();
    remote.apply_revoked_dm_device(owner, [2u8; 32]);
    merge_remote_into_local(&mut local, remote);
    let set = local.revoked_dm_devices.get(&owner).unwrap();
    assert!(set.contains(&[1u8; 32]) && set.contains(&[2u8; 32]),
        "merge must UNION, not clobber: {set:?}");
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(apply_revoked_dm_device_unions) + test(revoked_dm_devices_merge_is_union_not_lww)'`
Expected: FAIL to compile.

- [ ] **Step 3: Add the field**

In `OwnerState` (owner_state_crdt.rs:22-77), add after `friend_graph`:

```rust
    /// ZEB-685 (S3): friend-scoped device revocations — owner → set of that
    /// owner's revoked #2 ed25519 keys, learned from `RevocationPush` frames
    /// pushed by that owner (a DM-only contact). Feeds `RevokedDeviceProjection`
    /// for the DM cutoff. GROW-ONLY / union-merged (NOT LWW — see
    /// owner_state_sync::merge_remote_into_local). Additive on the wire.
    #[serde(rename = "rd", skip_serializing_if = "BTreeMap::is_empty", default)]
    pub revoked_dm_devices:
        std::collections::BTreeMap<crate::owner_state_types::OwnerAddr, std::collections::BTreeSet<[u8; 32]>>,
```

- [ ] **Step 4: Add the mutator**

Near `apply_friend_update` (owner_state_crdt.rs:905):

```rust
/// ZEB-685: union a revoked #2 ed25519 key into the friend-scoped store.
/// Returns true iff newly inserted (grow-only; idempotent).
pub fn apply_revoked_dm_device(
    &mut self,
    owner: crate::owner_state_types::OwnerAddr,
    ed25519: [u8; 32],
) -> bool {
    self.revoked_dm_devices.entry(owner).or_default().insert(ed25519)
}
```

- [ ] **Step 5: Union-merge arm**

In `merge_remote_into_local` (owner_state_sync.rs:224-360): add `revoked_dm_devices` to the exhaustive destructure of `remote`, and add the per-key union loop (mirror `apply_outbox`'s `delivered_to.extend`):

```rust
    // in the `let OwnerState { … } = remote;` destructure, add:
        revoked_dm_devices,

    // after the friend_graph loop, add:
    for (owner, set) in revoked_dm_devices {
        local
            .revoked_dm_devices
            .entry(owner)
            .or_default()
            .extend(set.into_iter());
    }
```

- [ ] **Step 6: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(apply_revoked_dm_device_unions) + test(revoked_dm_devices_merge_is_union_not_lww)'`
Expected: PASS. (If `merge_remote_into_local` is private to the module, put the merge test in the same module or use a `pub(crate)`-visible path — check its visibility.)

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/owner_state_crdt.rs src-tauri/src/owner_state_sync.rs
git commit -m "ZEB-685: union-merged revoked_dm_devices store on owner-state CRDT"
```

---

### Task 3: Receive — `handle_revocation_push` (verify + trust-bind + store + feed) + dispatch

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (add `handle_revocation_push` near `apply_invite` :2383)
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (add a `RevocationPush` arm to the `decode_packet` match ~:457)

**Interfaces:**
- Consumes: `DmPacket::RevocationPush`, `apply_revoked_dm_device` (Task 2), `RevokedDeviceProjection` (`revoked.union_from_members` / a single-key union).
- Produces: `pub(crate) fn handle_revocation_push(state: &mut OwnerState, expected_owner: OwnerAddr, revocation: &RevocationCert, enrollment: &EnrollmentCert, revoked: &RevokedDeviceProjection) -> Result<(), DmReceiveError>`.

- [ ] **Step 1: Write the failing tests**

Add to `dm_outbox.rs` tests (reuse the file's cert/state test helpers). Cases: accept (master-issued, owner matches, target matches) → stored + `revoked.is_revoked` true; reject `revocation.owner != expected_owner`; reject `revocation.target != enrollment.device_id`; reject non-Master issuer; idempotent re-apply. Example core case:

```rust
#[test]
fn handle_revocation_push_accepts_and_feeds_projection() {
    let (mut state, proj, expected_owner, revocation, enrollment, revoked_ed) =
        sample_revocation_push_case(); // helper: builds a master-signed revocation
                                       // + matching enrollment for `expected_owner`
    assert!(!proj.is_revoked(&expected_owner, &revoked_ed));
    handle_revocation_push(&mut state, expected_owner, &revocation, &enrollment, &proj)
        .expect("accept");
    assert!(proj.is_revoked(&expected_owner, &revoked_ed), "projection fed");
    assert!(state.revoked_dm_devices.get(&expected_owner).unwrap().contains(&revoked_ed),
        "CRDT stored");
}

#[test]
fn handle_revocation_push_rejects_third_party_owner() {
    let (mut state, proj, expected_owner, revocation, enrollment, _) =
        sample_revocation_push_case();
    let wrong = crate::owner_state_types::OwnerAddr([0xEE; 16]); // not the cert's owner
    let err = handle_revocation_push(&mut state, wrong, &revocation, &enrollment, &proj);
    assert!(matches!(err, Err(DmReceiveError::SignerDeviceRevoked) | Err(_)),
        "must reject a revocation whose owner != the pushing friend");
}
```

(Name the reject error appropriately — reuse an existing `DmReceiveError` variant such as a verify/binding failure; add one only if none fits.)

- [ ] **Step 2: Run to verify fail**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(handle_revocation_push)'`
Expected: FAIL to compile.

- [ ] **Step 3: Implement the applier**

In `dm_outbox.rs` near `apply_invite`. Read the exact `RevocationCert::verify`, `EnrollmentCert::verify`, and `EnrollmentCert` field accessors (`device_pubkeys.classical.ed25519_verify`, `device_id`, `owner_id`) before writing; the shape:

```rust
/// ZEB-685: apply a friend-pushed device revocation. `expected_owner` is the
/// tunnel-peer's resolved owner (a friend). Verifies the master-signed
/// revocation + paired enrollment, trust-binds them to `expected_owner` (a
/// friend may only revoke THEIR OWN devices), bridges target device_id → the
/// revoked ed25519, stores it union-merged, and feeds the live projection.
pub(crate) fn handle_revocation_push(
    state: &mut OwnerState,
    expected_owner: OwnerAddr,
    revocation: &harmony_owner::certs::RevocationCert,
    enrollment: &harmony_owner::certs::EnrollmentCert,
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> Result<(), DmReceiveError> {
    // 1. Verify the master-signed revocation (self-verifies the embedded master pub).
    revocation
        .verify(None)
        .map_err(|_| DmReceiveError::/* pick a verify-failure variant */)?;
    // 2. Trust-bind: the revocation + enrollment must belong to the pushing friend.
    let cert_owner = OwnerAddr(revocation.owner_id);
    if cert_owner != expected_owner || OwnerAddr(enrollment.owner_id) != expected_owner {
        return Err(DmReceiveError::/* binding-failure variant */);
    }
    // 3. Verify the enrollment (master → #2 chain) and bind it to the cert target.
    enrollment
        .verify(0)
        .map_err(|_| DmReceiveError::/* variant */)?;
    if enrollment.device_id != revocation.target {
        return Err(DmReceiveError::/* variant */);
    }
    // 4. Bridge to the revoked ed25519 and store + feed.
    let ed25519 = enrollment.device_pubkeys.classical.ed25519_verify;
    state.apply_revoked_dm_device(expected_owner, ed25519);
    let mut one = std::collections::BTreeSet::new();
    one.insert(ed25519);
    revoked.union_from_members(std::iter::once((expected_owner, &one)));
    Ok(())
}
```

(Confirm `revocation.verify` takes `Option<&VerifyingKey>` and that `None` works for the Master variant — the seam map says it self-verifies via the embedded `master_pubkey`. Confirm `enrollment.verify(0)` is the right master-issued verify call — reuse whatever `apply_invite` uses to verify `inviter_enrollment`. Confirm `OwnerAddr` wraps `[u8;16]` — adjust the constructor if it's a named field.)

- [ ] **Step 4: Wire the dispatch arm**

In `dm_inbox_ingest.rs` `ingest_dm_packet`'s `decode_packet` match (~:457), add before the fall-through:

```rust
    crate::dm_envelope::DmPacket::RevocationPush { revocation, enrollment } => {
        let mut state = crdt_state.lock().await;
        let expected_owner = resolve_owner_for_peer(&state, peer_node_id).ok_or_else(|| {
            format!("revocation_push: unbindable tunnel peer {}", hex::encode(peer_node_id))
        })?;
        match crate::dm_outbox::handle_revocation_push(
            &mut state, expected_owner, &revocation, &enrollment, revoked,
        ) {
            Ok(()) => tracing::info!(owner = ?expected_owner, "ZEB-685: applied friend RevocationPush"),
            Err(e) => tracing::warn!(error = ?e, "ZEB-685: rejected RevocationPush"),
        }
        return Ok(false); // control frame — never delivered as a chat message
    }
```

(Confirm `resolve_owner_for_peer`'s exact name/signature from the `Invite` arm.)

- [ ] **Step 5: Run to verify pass + covering tests**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(handle_revocation_push)'`
Expected: PASS (accept + all reject cases).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/dm_outbox.rs src-tauri/src/dm_inbox_ingest.rs
git commit -m "ZEB-685: receive-side handle_revocation_push + ingest dispatch"
```

---

### Task 4: Send — friend-push hook in `revoke_device_inner`

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (snapshot CRDT-doc + tunnel-manager handles ~:886-901; friend-push hook after `cert_for_feed` :1009, alongside :1059/:1138)
- Possibly modify: `src-tauri/src/iroh_tunnel_dm_transport.rs` (a reusable `push_revocation_to_owner` helper) or add the send inline.

**Interfaces:**
- Consumes: `build_revocation_push_packet` + `encode_packet` (Task 1); `resolve_owner_tunnel_targets` (iroh_tunnel_dm_transport.rs:66) + `mgr.send_dm(node_id, contact, wire)`; `trust_snapshot.enrollments.get(&target)`; CRDT `friend_graph.friends`.

- [ ] **Step 1: Write the failing test**

Read how `owner_commands.rs` tests drive `revoke_device_inner` (or its testable inner). The cleanest testable seam is a pure helper `fn revocation_push_targets(crdt_state: &OwnerState) -> Vec<OwnerAddr>` returning the active-friend owners to push to, plus a helper that builds the wire. Test the enumeration + wire build without the live transport:

```rust
#[test]
fn revocation_push_targets_are_active_friends_only() {
    let mut s = crate::owner_state_crdt::OwnerState::default();
    // insert an Active friend and a Pending friend (use the file's friend test helper)
    // ...
    let targets = revocation_push_targets(&s);
    assert_eq!(targets.len(), 1, "only Active friends are pushed to");
}
```

(If the file already exercises the transport with a mock, prefer an end-to-end capture test; otherwise the pure enumeration + a Task-6 integration test cover it.)

- [ ] **Step 2: Run to verify fail**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(revocation_push_targets)'`
Expected: FAIL to compile.

- [ ] **Step 3: Add the enumeration helper + send hook**

Add the pure helper (in `owner_commands.rs` or `dm_outbox.rs`):

```rust
/// ZEB-685: the active-friend owners to push a device revocation to.
pub(crate) fn revocation_push_targets(
    crdt: &crate::owner_state_crdt::OwnerState,
) -> Vec<crate::owner_state_types::OwnerAddr> {
    crdt.friend_graph
        .friends
        .iter()
        .filter(|(_, e)| matches!(e.status, crate::friend_graph::FriendStatus::Active))
        .map(|(addr, _)| *addr)
        .collect()
}
```

In `revoke_device_inner`: (a) snapshot the CRDT-doc handle + tunnel-manager handle in the `:886-901` tuple (mirror `retire_nudge`); (b) after `cert_for_feed` (:1009), add the hook:

```rust
    // ZEB-685 (S3): push the revocation to DM-only friends (best-effort tunnel).
    if let Some(enrollment) = trust_snapshot.enrollments.get(&cert_for_feed.target).cloned() {
        let crdt = crdt_snapshot; // the owner_state_crdt::OwnerState snapshotted above
        let targets = revocation_push_targets(&crdt);
        if !targets.is_empty() {
            if let Ok(wire) = crate::dm_envelope::encode_packet(
                &crate::dm_envelope::build_revocation_push_packet(
                    cert_for_feed.clone(), enrollment,
                ),
            ) {
                for owner in targets {
                    let tunnel_targets = crate::iroh_tunnel_dm_transport::resolve_owner_tunnel_targets(&crdt, owner);
                    for (node_id, contact) in tunnel_targets {
                        mgr_handle.send_dm(node_id, &contact, wire.clone());
                    }
                }
            }
        }
    }
```

(Confirm `resolve_owner_tunnel_targets`'s visibility/signature and `mgr.send_dm`'s exact signature from `iroh_tunnel_dm_transport.rs`. If the tunnel manager isn't cleanly reachable from `revoke_device_inner`, add a `push_revocation_to_owner(mgr, crdt, owner, wire)` helper in `iroh_tunnel_dm_transport.rs` and call it. Best-effort: ignore send errors.)

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(revocation_push_targets)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/owner_commands.rs src-tauri/src/iroh_tunnel_dm_transport.rs
git commit -m "ZEB-685: send-side friend RevocationPush hook on device revoke"
```

---

### Task 5: Boot-replay — feed `revoked_dm_devices` into the projection

**Files:**
- Modify: `src-tauri/src/lib.rs` (boot-replay seed, alongside the community feed at ~:7930)

**Interfaces:**
- Consumes: `revoked_device_projection` (the projection being seeded at boot), `state.revoked_dm_devices`.

- [ ] **Step 1: Write the failing test**

Find the boot-replay test pattern for the community feed (search for `feed_revoked_from_materialized` / the :7930 wiring's test). Add a test that a persisted `revoked_dm_devices` re-seeds the projection at boot. If boot is hard to unit-test, add a focused helper `fn feed_revoked_from_dm_store(proj: &RevokedDeviceProjection, crdt: &OwnerState)` and test THAT directly:

```rust
#[test]
fn feed_revoked_from_dm_store_seeds_projection() {
    let mut s = crate::owner_state_crdt::OwnerState::default();
    let owner = crate::owner_state_types::OwnerAddr([7u8; 16]);
    s.apply_revoked_dm_device(owner, [9u8; 32]);
    let proj = crate::revoked_device_projection::RevokedDeviceProjection::default();
    feed_revoked_from_dm_store(&proj, &s);
    assert!(proj.is_revoked(&owner, &[9u8; 32]));
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(feed_revoked_from_dm_store)'`
Expected: FAIL to compile.

- [ ] **Step 3: Add the feed helper + boot wiring**

Add (near `feed_revoked_from_materialized`, lib.rs:3207):

```rust
/// ZEB-685: seed the revocation projection from the persisted friend-scoped
/// DM-revocation store (boot-replay + any full re-seed).
fn feed_revoked_from_dm_store(
    proj: &crate::revoked_device_projection::RevokedDeviceProjection,
    crdt: &crate::owner_state_crdt::OwnerState,
) {
    proj.union_from_members(
        crdt.revoked_dm_devices.iter().map(|(o, set)| (*o, set)),
    );
}
```

In `start_node_inner`, at the boot-replay seed alongside the community feed (~lib.rs:7930), call `feed_revoked_from_dm_store(&revoked_device_projection, &state_snapshot_at_boot)` (use the same boot-state handle the community feed reads).

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(feed_revoked_from_dm_store)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-685: boot-replay feed revoked_dm_devices into the cutoff projection"
```

---

### Task 6: End-to-end cutoff integration test (co-located, solo)

**Files:**
- Create/Modify: an integration test under `src-tauri/tests/` (find the DM-cutoff test the S2 slice added — likely `tests/` referencing `RevokedDeviceProjection` / `SignerDeviceRevoked`; extend it or add a sibling)

**Interfaces:**
- Consumes: everything above. Proves the gap closes with no shared community.

- [ ] **Step 1: Write the test**

Model on the S2 cutoff test (search `tests/` for `SignerDeviceRevoked` or the S2 integration). The scenario, at the `verify_cidnotify` / `apply_invite` layer (no live network needed): build B's state; establish A as a DM-only friend (Active `FriendEntry`, A's #2 cached); confirm a DM CidNotify signed by A's device D is ACCEPTED before revocation; apply a `RevocationPush` for D via `handle_revocation_push`; confirm the same CidNotify is now REJECTED (`SignerDeviceRevoked`) at `verify_cidnotify_admission`. Assert the before/after transition.

```rust
#[test]
fn dm_only_contact_cutoff_after_revocation_push() {
    // ... build B state + friend A (Active) + A's #2 device D cached ...
    // BEFORE: a D-signed CidNotify passes admission.
    assert!(verify_cidnotify_admission(/* … */, &proj).is_ok());
    // Apply the friend push.
    handle_revocation_push(&mut b_state, a_owner, &revocation_d, &enrollment_d, &proj).unwrap();
    // AFTER: the same CidNotify is cut off.
    assert!(matches!(
        verify_cidnotify_admission(/* … */, &proj),
        Err(DmReceiveError::SignerDeviceRevoked)
    ));
}
```

(Use the exact `verify_cidnotify_admission` signature from dm_outbox.rs:3328; reuse S2's test scaffolding for building a signed CidNotify from a #2 device.)

- [ ] **Step 2: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --features test-fixtures -E 'test(dm_only_contact_cutoff_after_revocation_push)'`
Expected: PASS (before: ok; after: `SignerDeviceRevoked`).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/
git commit -m "ZEB-685: end-to-end DM-only cutoff integration test"
```

---

## Final gate (after all tasks)

- [ ] `cd src-tauri && cargo fmt --all -- --check`
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [ ] Whole-branch review, then open PR. Flag at handoff: **delivery is best-effort tunnel (not deposit-durable) this slice** — durability is the noted follow-up.

## Self-Review notes

- **Spec coverage:** §3.1 frame → T1; §3.4/§Q1 store+merge → T2; §3.3 receive/verify/trust-bind → T3; §3.2 send → T4; §3.4 boot-replay → T5; the closing-the-gap claim → T6. Three cutoff sites unchanged (verified: T3 feeds `by_owner`, cutoffs read it). ✅
- **Type consistency:** `apply_revoked_dm_device(OwnerAddr, [u8;32]) -> bool`, `handle_revocation_push(&mut OwnerState, OwnerAddr, &RevocationCert, &EnrollmentCert, &RevokedDeviceProjection) -> Result<(), DmReceiveError>`, `build_revocation_push_packet(RevocationCert, EnrollmentCert) -> DmPacket`, `revocation_push_targets(&OwnerState) -> Vec<OwnerAddr>`, `feed_revoked_from_dm_store(&proj, &crdt)` — consistent across tasks. ✅
- **Implementer note:** several exact names (`DmReceiveError` verify variants, `revocation.verify(None)` acceptance for Master, `enrollment.verify(0)`, `resolve_owner_for_peer`, `resolve_owner_tunnel_targets` visibility, `canonical_cbor_decode`) must be confirmed against the referenced code before finalizing each step — the plan gives the shape + the anchor; the implementer reads the anchor. These are flagged inline, not placeholders.
