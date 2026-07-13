# ZEB-580 — DM packet signing migration #3 → #2 (cert-anchored, revocation-aware)

**Status:** design approved 2026-07-13 · **Owner:** Koya · **Epic:** ZEB-580 (split from ZEB-340 §1)
**Descends from:** `docs/specs/2026-07-11-zeb-668-device-management-design.md` (§8 honesty ledger, §9 follow-ups)
**Sibling (shipped):** ZEB-678 vine/follow signing migration #3→#2 (PRs #462/#463/#464)

---

## 1. Problem

DM packets sign with the **Reticulum identity key (#3)** — an unattested Ed25519 seed from
`harmony_identity::PrivateIdentity` — not the **enrolled device key (#2)** that the owner's master
key attests via an `EnrollmentCert`. Because #3 is not cert-attested, it is **not revocable**: a
device the owner has removed keeps signing DMs its peers accept forever. The ZEB-668 device-removal
UI says as much ("Its direct messages are a separate surface and aren't blocked yet — that cutoff
lands in follow-up work"; `RemoveDeviceDialog.svelte`). This ticket is that follow-up.

Community-membership events already moved to #2 + `EnrollmentCert` (ZEB-339); vine/follow records
moved in ZEB-678. DMs are the last #3 signing surface (owner-state is out of scope — it replicates
via KeyTree symmetric AEAD, not Ed25519 signing).

### 1.1 Current-state trace (main @ c06201e8)

The DM sign → transmit → receive → verify → cache path is **entirely #3-native**:

- **Sign.** DM bodies (`DmInviteSigned`, `DmCidNotifySigned`, `DmAckSigned`;
  `dm_envelope.rs:68-162`) are signed by `sign_dm_packet(body, signing_key)`
  (`dm_signing.rs:298`) where `signing_key` is the #3 Ed25519 key
  (`lib.rs:4704-4709`, sourced from `PrivateIdentity`).
- **`signing_device_hash`.** Every outbound body sets `our_signing_device_hash`
  (`dm_outbox.rs:600`, `1529-1533`; `lib.rs:13402-13412`), which is the **#3 identity's**
  `address_hash` = `SHA256(X25519_#3 ‖ Ed25519_#3)[:16]` (`lib.rs:4710-4711`, `3638`).
- **Verify.** `verify_dm_packet_signature(body, sig, identity_pub, expected_hash)`
  (`dm_signing.rs:322`) — Step 1 rejects unless `derive_device_hash_from_identity_pub(identity_pub)`
  equals the wire `signing_device_hash`; Step 2 verifies the Ed25519 signature against
  `identity_pub[32..64]`.
- **Receiver's `identity_pub` source.** For CidNotify/Ack, `lookup_pubkey_for_device`
  (`dm_outbox.rs:3096-3111`) resolves a 64-byte combined pub from `OwnerDeviceCache`; for the
  bootstrapping DmInvite, from the inline `inviter_identity_pub` (`dm_outbox.rs:2323-2328`). **Both
  are #3 pubs** (`ed25519.public_identity().to_public_bytes()`, `lib.rs:3667`).
- **Cache population.** `OwnerDeviceCache.device_identity_pubs` (`owner_state_types.rs:661`) is fed
  only #3 material — by the DmInvite inline pub, the friend handshake device bundle
  (`iroh_friend_acceptor.rs:1130-1142`), and CidNotify device-set growth (#371). No writer feeds a
  #2 pub.

### 1.2 Two facts that shape the design

**Fact A — the receiver has no path to a peer's #2 identity today.** OwnerDeviceCache and the
DmInvite inline pub are exclusively #3. So migrating is not a key swap: it requires teaching the DM
identity **bootstrap** about #2.

**Fact B — the revocation fact is siloed and there is no cross-community aggregate.** A revoked
device is recorded only in per-`(community, owner)` `MemberState.revoked_device_keys`
(`community_membership.rs:1679`), keyed by the raw 32-byte #2 Ed25519 key. `MembershipProjection`
(`network_health.rs:2198`) is owner-set-only (no device keys); `harmony_owner::state::is_revoked`
is self-owner-only. **Nothing answers "is owner X's device D revoked?"** across communities, and the
DM receive path (which sees only `owner_state_crdt::OwnerState`) cannot reach any of it.

**Fact C (mitigating) — the friend handshake already carries and verifies the peer's #2 cert.**
`FriendLinkRequest.enrollment` / `FriendLinkAccepted.enrollment`
(`iroh_friend_acceptor.rs:309-311`, `403-405`) carry the peer's #2 `EnrollmentCert`;
`verify_enrolled_device` (`:1026`) already verifies the master→#2 chain. Today the cert is verified
then **discarded** — only `master_ed25519` survives into `FriendEntry`. This device's *own* #2 cert
+ signing key are likewise already in-scope at handshake time (the same `own_enrollment_cert` that
feeds `DmOutbox::new`). So cert-anchored #2 identity via the **primary** bootstrap (the friend
handshake) costs **zero new wire**.

---

## 2. Goals / non-goals

### Goals

1. **G1 — Cert-anchored #2 DM identity.** DM packets sign with the enrolled device key (#2). The
   receiver resolves the sender's #2 identity from a **master-attested `EnrollmentCert`** (the friend
   handshake's, or a new inline field on `DmInvite`), verifying `master → #2` and
   `cert.owner_id == sender owner`.
2. **G2 — Shared-community revocation cutoff.** A DM signed by a #2 device the sender's owner has
   **revoked** is rejected, *when the receiver shares a community with that owner* (the only place the
   revocation fact exists).
3. **G3 — Safe rollout.** No hard regression of the cross-WAN-proven DM path (ZEB-504). Dual-path
   verification (legacy #3 + new #2) plus an opportunistic #2 identity-refresh for existing friends.
4. **G4 — Honesty copy.** Narrow the ZEB-668 removal-dialog caveat to reflect the shared-community
   cutoff; file the DM-only follow-up.

### Non-goals

- **N1 — DM-only (no shared community) cutoff.** No substrate exists (Fact B); a full cutoff needs
  cross-owner friend-scoped `RevocationCert` propagation. Deferred to the **S3 follow-up ticket**.
- **N2 — Owner-state signing.** Out of scope (KeyTree AEAD, not Ed25519; ticket §"Current state").
- **N3 — Quorum-issued device via DmInvite-only bootstrap.** The friend handshake already carries a
  `signer_certs` bundle (`iroh_friend_acceptor.rs:363`), so a **quorum-issued** #2 cert verifies fine
  on that path. The new `DmInvite` field (§4.4) carries only the single `EnrollmentCert`, so a
  quorum-issued device bootstrapping via DmInvite **without** a prior friend handshake is a documented
  residual — the same family as ZEB-682 (quorum self-publish). The fleet is master-issued, so S1 is
  correct for it; carrying `inviter_signer_certs` on `DmInvite` is a cheap future add if the residual
  ever bites. Flagged for the test matrix (master-issued must pass; quorum-via-DmInvite-only is a
  known no-op, not a silent failure — it falls back to the legacy #3 path until a handshake seeds #2).
- **N4 — Removing #3.** The #3 `PrivateIdentity` stays (it still anchors the iroh/tunnel transport
  and the friend handshake's contact digest). This ticket stops using #3 *for DM body signing*, not
  for transport identity.

---

## 3. Identity model

### 3.1 The three hashes of device #2 (why this needs a decision)

Device #2 has **three distinct 16-byte identifiers**, and conflating them is the classic failure mode:

| Name | Formula | Where used |
|---|---|---|
| **DM device hash** | `SHA256(X25519 ‖ Ed25519)[:16]` via `Identity::from_public_bytes` | `dm_signing.rs:286`, the wire `signing_device_hash`, OwnerDeviceCache keys |
| **Cert `device_id`** | `SHA256(cbor{ed25519_verify [+ ml_dsa]})[:16]` (ed25519-only) | `EnrollmentCert.device_id`, `harmony_owner` enrollment map keys |
| **Raw #2 Ed25519 key** | the 32-byte verifying key itself | community `revoked_device_keys`, `enrolled_device_keys` |

**Decision:** the DM layer keeps its own **DM device hash** scheme, synthesizing #2's 64-byte
combined pub from the cert (`x25519_pub ‖ ed25519_verify`). This keeps `verify_dm_packet_signature`
**unchanged** (Step 1 still derives the hash from the combined pub). The revocation bridge (§5)
crosses to the community namespace by extracting the raw Ed25519 = `combined_pub[32..64]` — no
hash↔key map is needed because we always hold the full combined pub once bootstrapped.

### 3.2 Trust chain

```
sender's owner master key  ──signs──▶  EnrollmentCert(owner_id, device_id, #2 pubkeys)  ──#2 signs──▶  DM packet body
        │                                                                                                    │
   receiver knows it                                                                        receiver verifies chain:
 (FriendEntry.master_ed25519,                                                          1. cert.verify() structurally ok
  or the cert self-carries                                                             2. cert.owner_id == claimed sender owner
  master_pubkey for a                                                                  3. derive #2 combined pub, its DM hash
  first-contact DmInvite)                                                                 == wire signing_device_hash
                                                                                       4. Ed25519 sig verifies under #2
```

The receiver need not have a pre-existing friendship: the cert self-carries its issuer
(`EnrollmentIssuer::Master { master_pubkey }`), and `owner_id = hash(master_pubkey)` binds it to the
sender-owner the DM already claims (`resolve_signed_origin_owner`). A friendship, when present,
gives the same anchor via `FriendEntry.master_ed25519` (a cross-check, not a prerequisite).

**Expiry-agnostic** (ZEB-378): cert verification for DM identity is structural
(`EnrollmentCert::verify(0)`), matching `DmOutbox::new`'s existing invariant
(`dm_outbox.rs:674-677`). Enrollment expiry is a separate axis and is **not** a DM-drop condition
here (see §8.3 for the flagged edge case).

---

## 4. S1 — cert-anchored #2 DM identity (dual-path)

### 4.1 New helper: synthesize #2's DM identity

Add to `dm_signing.rs` (or a small `dm_identity` helper module):

```rust
/// Build the 64-byte DM combined pub (X25519_pub ‖ Ed25519_pub) for an
/// enrolled device (#2) from its EnrollmentCert's classical pubkeys.
/// The DM device hash is derive_device_hash_from_identity_pub(&combined).
pub fn device2_combined_pub(cert: &EnrollmentCert) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&cert.device_pubkeys.classical.x25519_pub);
    out[32..].copy_from_slice(&cert.device_pubkeys.classical.ed25519_verify);
    out
}
```

`cert.device_pubkeys.classical.x25519_pub` is real birational material on the pinned harmony rev
(ZEB-372; the zeroed stub is test-only) — but **guard against an all-zero `x25519_pub`** at ingest
and refuse to cache such a #2 identity (a malformed/legacy cert must not silently produce a
degenerate combined pub). This is the only place the client reads `classical.x25519_pub` today.

### 4.2 Sender flip

At `DmOutbox` construction (`lib.rs:4712`) and in `DmOutbox`'s signing sites:

- Sign DM bodies with `community_signing_key` (#2) instead of `signing_key` (#3). The signing
  helpers (`build_signed_*`) are already key-agnostic — the change is *which* key the caller passes.
- Set `our_signing_device_hash` to #2's DM hash =
  `derive_device_hash_from_identity_pub(&device2_combined_pub(&enrollment_cert))`.
- `signing_key` (#3) and `private_identity` (#3) remain on `DmOutbox` for the transport/friend-digest
  and countersign paths (N4). Only DM *body* signing moves.

### 4.3 Bootstrap #1 — friend handshake (primary, zero new wire)

In `iroh_friend_acceptor.rs::process_friend_request` (and the symmetric accept-processing path):
after `verify_enrolled_device` succeeds, **stop discarding `req.enrollment`**. Compute
`device2_combined_pub(&req.enrollment)`, derive its DM hash, and feed it into `OwnerDeviceCache`
alongside (see §4.6) the existing #3 bundle via `apply_owner_device_update`. The peer's #2 identity
is now cached and revocation-checkable.

### 4.4 Bootstrap #2 — tunnel DmInvite (new additive field)

Add an **additive** field to `DmInviteSigned` (`dm_envelope.rs`):

```rust
/// ZEB-580: the inviter's enrolled-device (#2) EnrollmentCert, so the
/// receiver can verify master→#2 and cache the #2 DM identity without a
/// prior friend handshake. Additive; #[serde(default, skip_serializing_if)].
inviter_enrollment: Option<EnrollmentCert>,
```

Additive per the team rule — **no packet-version byte exists** (`dm_envelope.rs:164-222` are routing
discriminants, excluded from signed bytes) and **no `FILE_VERSION` bump** (matches the ZEB-678 /
`revoked_device_keys` precedent). On receive, `apply_invite`:

1. If `inviter_enrollment` present: verify `cert.verify(0)` (master-issued path; quorum-issued via
   DmInvite-only is the N3 residual), `cert.owner_id == resolved sender owner`, and that
   `device2_combined_pub(cert)`'s DM hash matches the wire `signing_device_hash`; cache the #2
   combined pub. The existing `inviter_identity_pub` (#3) stays for the legacy path.
2. If absent (legacy sender): current #3 behavior unchanged.

CidNotify/Ack need **no** cert field — they fail-closed unless #2 was already seeded by the handshake
or a DmInvite (bootstrap-ordering trace confirms first-contact CidNotify/Ack never bootstrap trust).

### 4.5 Dual-path verify

`verify_dm_packet_signature` is unchanged. Dual-path lives in the **resolution** step:
`lookup_pubkey_for_device` returns whatever combined pub is cached for the wire
`signing_device_hash` — a #3 pub for a legacy peer, a #2 pub for a bootstrapped-under-#2 peer. Both
verify identically. The cache can hold **both** a peer's #3 and #2 entries during transition
(different hashes, different vec slots); neither evicts the other.

### 4.6 Cache: additive #2 entries, first-write-wins per hash

`OwnerDeviceEntry` already stores parallel `devices: Vec<DeviceIdentityHash>` +
`device_identity_pubs: Vec<Option<[u8;64]>>` (`owner_state_types.rs:635-662`). A peer's #2 DM hash
is simply another entry in that owner's set. `apply_owner_device_update`'s existing
`rejects_pub_with_mismatched_hash` invariant (`owner_state_crdt.rs:3380`) already guards
pub↔hash consistency — the #2 combined pub satisfies it by construction. No struct change; the #2
identity rides the existing additive `device_identity_pubs` (`p`) field.

### 4.7 Rollout — hard flag-day (decided 2026-07-13)

All nodes are under single-operator control (the Koya/Ildwyn/AVALON fleet + a handful of alpha
testers), and no existing DM relationship needs preserving. So the rollout is a **hard flag-day**,
precedent ZEB-636 (iroh 1.0): the fleet updates and restarts together onto the #2-signing build, and
any existing DM contact is simply re-established via a fresh friend handshake / DM invite (which now
seeds the peer's #2 identity). **No opportunistic identity-refresh machinery is built** — buying out
of that complexity is exactly what the flag-day is for.

- **Dual-path receive is retained** — it is *free* (the receiver verifies a packet against whatever
  pub it holds for that hash, #3 or #2; the cache can hold both). This is not extra machinery, it is
  simply not ripping out the #3 verify path in this slice: it keeps the flag-day window graceful and
  lets a node still read a legacy #3 DM it had already bootstrapped. #3 body-verify is removed only
  if/when a later cleanup ticket retires the transport-identity concept (N4).
- **Gate:** a cross-WAN DM round-trip test (§7.3) proving the #2 path works end-to-end, run before
  merge and on the live fleet before the honesty copy retires in S2.

---

## 5. S2 — shared-community revocation cutoff

### 5.1 New primitive: the revoked-#2-keys aggregate

Nothing existing answers "is owner X's device D revoked?" (Fact B). Build a synchronous projection,
sibling to `MembershipProjection`:

```rust
/// ZEB-580 S2: owner_id → set of revoked #2 Ed25519 keys, aggregated across
/// the communities THIS node is joined in with that owner. Fed by the
/// DeviceRetire/community-membership materialize path; read (lock-free
/// snapshot) by the DM receive verify. Sticky within a session — a key
/// present in any joined community's revoked set stays revoked here.
struct RevokedDeviceProjection {
    by_owner: BTreeMap<OwnerAddr, BTreeSet<[u8; 32]>>,
}
```

- **Fed** from the same materialize arms that write `MemberState.revoked_device_keys`
  (`community_membership.rs:2843` and the rejoin/merge carries) — on DeviceRetire ingest, union the
  retired #2 Ed25519 key into `by_owner[owner]`. Also unioned on boot replay (mirror
  `MembershipProjection`'s `set_community_members` wiring at `lib.rs:7805`).
- **Removal semantics:** revocation is monotonic/sticky within the projection (a device is not
  "un-revoked"); a community the node *leaves* need not retract (leaving loses the fact, but a
  revoked device staying blocked is the safe direction — matches ZEB-678's sticky-revoked follower
  discipline). Documented, not a bug.
- **Reachability:** thread an `Arc<RevokedDeviceProjection>` (or a lock-free snapshot handle) into
  the DM receive context (`DmInboxIngestCtx` / the `handle_cidnotify*/handle_ack/apply_invite`
  call sites), the same way `MembershipProjection` is threaded into network-health.

### 5.2 The cutoff check

On successful signature verification of an **inbound DM** whose signer is a **#2** identity
(has a cached #2 combined pub): extract `ed25519 = combined_pub[32..64]`; if
`revoked_projection.by_owner[sender_owner].contains(&ed25519)` → **reject the DM** (drop, same
disposition as a signature failure; do not deliver, do not ack). Legacy #3-signed DMs are *not*
subject to the cutoff (no cert-attested device to revoke) — that gap is exactly why the migration is
the prerequisite, and it closes as peers move to #2.

### 5.3 Honesty copy + follow-up

- `RemoveDeviceDialog.svelte`: narrow the DM caveat to, in spirit: *"Its direct messages to people who
  share a community with you stop being accepted once the removal syncs; blocking DMs to contacts you
  only message directly (no shared community) lands in follow-up."* Update the two `RemoveDeviceDialog.test.ts`
  assertions (`stop accepting new posts` group + the DM caveat matcher) and the ZEB-668 §8 ledger row
  + §9 follow-up list.
- **File S3** (this epic's out-of-scope tail): "DM-only device-revocation cutoff — friend-scoped
  RevocationCert propagation." Owner A pushes a master-signed `RevocationCert` for device D to friend
  B over the friend/DM channel; B stores it friend-scoped and applies the same §5.2 check for
  DM-only contacts. Reference this spec §N1.

---

## 6. Components & data flow

```
SEND (new)                          RECEIVE (new, dual-path)
─────────                           ────────────────────────
DmOutbox.community_signing_key(#2)  ingest_dm_packet
  └ sign body                         ├ Invite: apply_invite
our_signing_device_hash = #2 DM hash │    ├ verify master→#2 (inline cert) OR legacy #3 inline pub
                                      │    └ cache #2 combined pub (device2_combined_pub)
DmInviteSigned.inviter_enrollment ───┤ CidNotify/Ack: lookup_pubkey_for_device (cache: #3 or #2)
  (additive, #2 cert)                │    └ verify_dm_packet_signature (unchanged)
                                      └ CUTOFF: ed25519 = combined_pub[32..64]
FriendLink{Request,Accepted}              if revoked_projection[sender_owner].contains(ed25519) → drop
  .enrollment (#2 cert, already      ▲
   on wire) → cache #2               │  RevokedDeviceProjection.by_owner  ◀── DeviceRetire materialize
                                      └───────  (community_membership.rs:2843 + boot replay)

Rollout: hard flag-day (§4.7) — fleet restarts together onto #2; existing contacts re-established.
```

## 7. Testing strategy

Existing pins that must be updated (not deleted): `dm_signing.rs`
(`sign_dm_packet_matches_private_identity_sign`, `derive_device_hash_equals_harmony_identity_address_hash`
— these pin the *#3* equivalence; add #2 analogues, keep #3 ones for the legacy path),
`dm_outbox.rs::dm_outbox_community_signing_key_and_enrollment_cert`, the `dm_envelope.rs` round-trips,
the `owner_state_crdt.rs` cache-ingest tests, and `tests/dm/*` integration.

### 7.1 S1 unit

- `device2_combined_pub` produces a pub whose DM hash is deterministic and differs from the same
  device's #3 hash; rejects all-zero `x25519_pub`.
- A #2-signed body verifies via `verify_dm_packet_signature` against the synthesized #2 combined pub.
- DmInvite with `inviter_enrollment`: master→#2 verify passes; `owner_id` mismatch rejects; hash
  mismatch (`signing_device_hash ≠ device2 hash`) rejects; absent field → legacy #3 path intact.
- Friend handshake caches the peer's #2 combined pub (was discarded).
- Dual-path: a cache holding both #3 and #2 entries for one owner verifies each packet against the
  right one.

### 7.2 S2 unit

- `RevokedDeviceProjection`: DeviceRetire ingest unions the #2 ed25519 into `by_owner`; sticky across
  a simulated community-leave; boot replay rebuilds it.
- Cutoff: inbound #2-signed DM from a revoked device (shared community) → dropped, not delivered/acked;
  from a non-revoked device → delivered; DM-only (owner not in any shared community's aggregate) →
  delivered (documented residual); legacy #3 DM → not subject to cutoff.

### 7.3 Integration / cross-WAN (the ZEB-504 gate)

- `tests/dm/*` round-trips re-greened under #2 signing.
- A two-process (and, before honesty-copy retires, a live-fleet) cross-WAN DM round-trip: friend
  handshake → #2 identity cached → DM send/receive/ack, proving no regression. A mixed old/new pair
  exercises dual-path + identity-refresh.

Gates: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures
--no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features
test-fixtures`, `npx tsc --noEmit`, `npx vitest run`. Iterative via `scripts/test-select`.

## 8. Security & edge cases

1. **Key-substitution** is still defended by `verify_dm_packet_signature` Step 1 (hash binds the
   pubkey); the added cert check binds #2 to the sender's owner master, strictly strengthening it.
2. **No downgrade hole.** A legacy #3 packet is accepted only against a cached #3 pub the receiver
   already trusted; presence of `inviter_enrollment` never *weakens* verification. A peer can't force
   a downgrade to dodge the cutoff, because the #3 path was never subject to the cutoff and the #2
   path adds it — the incentive runs the safe direction.
3. **Sticky revocation.** The projection never un-revokes within a session (§5.1).
4. **Malformed cert / zero x25519_pub** refused at ingest (§4.1).
5. **Enrollment-expiry edge (flagged, shared with ZEB-678).** DM identity verify is expiry-agnostic
   (§3.2), so an expired-but-not-revoked device still DMs — intended. But if a device's cert has
   *expired* by the time it's revoked, and some path filters expired enrollments before the retire
   materializes, the retire could be dropped and the projection never learn the revocation. This is a
   pre-existing ZEB-668/S1 concern, not introduced here; **note in the S2 plan and verify the
   materialize path does not expiry-filter DeviceRetire** (`insert_enrolled_key_unless_retired`
   ordering). File a follow-up only if the materialize path proves to drop expired retires.

## 9. Slicing

- **S1 — cert-anchored #2 DM identity + dual-path migration.** §4. Ships the sender flip, both
  bootstrap points, dual-path verify, cache #2, hard flag-day rollout, cross-WAN gate. Retires **no**
  copy yet (cutoff not present).
- **S2 — shared-community revocation cutoff + honesty copy.** §5. The aggregate projection, the DM
  verify cutoff, the narrowed dialog copy + ZEB-668 ledger update, and filing S3.
- **S3 (follow-up ticket, out of epic)** — DM-only friend-scoped RevocationCert propagation (N1).

Bundle discipline (per standing rules): one PR per slice; S1 merges and validates cross-WAN before
S2 builds the cutoff on top.

## 10. Open questions

- **Q1 — projection home (S2).** New module vs. extending `network_health`'s projection neighborhood.
  Settle in the S2 plan; it must be reachable from the DM receive ctx and fed by the community
  materialize path without a layering inversion.
