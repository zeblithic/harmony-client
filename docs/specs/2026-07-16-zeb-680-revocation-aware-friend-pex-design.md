# ZEB-680: Revocation-aware friend/PEX verification — design

**Ticket:** ZEB-680 (ZEB-668 §9 follow-up 4). **Scope approved 2026-07-16:** full — verifier
enforcement + friend-link handshake carry, one PR.

## Context

Every friend/PEX enrollment-cert verifier funnels through the pure chokepoint
`enrollment_verify::verify_enrollment_any_issuer` (via the friend-handshake wrapper
`iroh_friend_acceptor::verify_enrolled_device`, `iroh_friend_acceptor.rs:783`). The chokepoint
checks expiry + master/quorum signature + owner binding on a **lone cert** — and pairing mints
certs with `expires_at: None`, so a revoked device's cert verifies forever. None of the
friend/PEX verifiers consults revocation (ZEB-668 spec §8: "…and friends/PEX immediately → NO").

Meanwhile the client already has everything needed on the knowledge side:

- `RevokedDeviceProjection` (`revoked_device_projection.rs:17`) — sync, global, by-owner
  aggregate of revoked #2 ed25519 keys. `is_revoked(&OwnerAddr, &[u8;32]) -> bool`
  (`:46`, std `RwLock`, never held across await). Fed from every community's
  `MemberState.revoked_device_keys` tombstones (`lib.rs:3228`), the DM store boot-seed
  (`lib.rs:3246`), and live `RevocationPush` handling (`dm_outbox.rs:2455`). Stashed on
  `NodeState` (`lib.rs:880`, accessor `:1747`).
- Propagation is dual-channel and durable: community retire-announce (ZEB-668 S3, 30-day TTL,
  level-triggered deposit sweeper) + `push_revocation_to_friends` (`owner_commands.rs`,
  ZEB-685) fanning out to **every Active friend** (`OwnerState::active_friend_owners`,
  `owner_state_crdt.rs:943`) with butler-rung deposit durability for offline friends (ZEB-691).
- The DM path already threads `&RevokedDeviceProjection` through its verifiers
  (`dm_outbox.rs:3470`, `dm_inbox_ingest.rs:481/871`) — the precedent this design copies.

Two gaps remain, and this design closes both:

1. **Consumption:** friend/PEX verifiers never ask the projection (§1).
2. **The "later friend" gap:** `RevocationPush` fires at revoke time to then-current friends.
   A peer who befriends the owner *after* a revoke (and shares no community) never learns of
   it, so the revoked device keeps working against them. Fix: carry the owner's own
   revocations in the friend-link handshake (§2) — the ticket's "in-handshake proof exchange".

## §1 Enforcement: consult the projection at the friend/PEX wrapper

`verify_enrolled_device` gains a revocation parameter and performs the consult **after** the
chokepoint verification succeeds, against the verified device key:

```rust
pub fn verify_enrolled_device(
    cert: &EnrollmentCert,
    signer_certs: &[EnrollmentCert],
    claimed_owner: OwnerAddr,
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
    now_secs: u64,
) -> Result<crate::enrollment_verify::VerifiedEnrollment, FriendHandshakeError> {
    let v = crate::enrollment_verify::verify_enrollment_any_issuer(/* unchanged */)?;
    if revoked.is_revoked(&claimed_owner, &v.device_ed25519) {
        return Err(FriendHandshakeError::DeviceRevoked);
    }
    Ok(v)
}
```

Design points:

- The chokepoint `verify_enrollment_any_issuer` itself stays **pure and untouched** — it is
  shared by community/invite/relay/butler/DM/vine/feed surfaces that each have their own
  revocation handling (or their own tickets). Only the friend/PEX face changes.
- New error variant `FriendHandshakeError::DeviceRevoked` (and equivalent mapping in any
  caller-local error enums). Reject behavior at each call site matches that site's existing
  failure handling — no new shed/ack semantics.
- Every caller threads the projection from its context (acceptors and lib.rs drivers all have
  `NodeState` access; unit tests construct `RevokedDeviceProjection::new()`):
  - `iroh_friend_acceptor.rs` — `authenticate_friend_request` (`:980`), the
    `process_friend_request` re-check (`:1046`), `serve()` (`:1655`).
  - `friend_intro.rs` — `authenticate_introduce_request` (`:148`, consult on the requester),
    `verify_introduction` (`:288`, consult on **both** the voucher cert at `:313` — the
    security-relevant check, the voucher is a friend whose revocations we likely know — and
    the subject cert at `:336` — opportunistic, the subject's owner may be a stranger).
  - `referral_catalog.rs` — `authenticate_catalog_request` (`:273`), catalog author verify
    (`:329`).
  - `lib.rs` dialer-side response verifies — `:51638` (`connectivity_link_friend_iroh_inner`),
    `:51920`, `:56138`.
- The quorum-cert path (ZEB-677 S2) flows through the same chokepoint; the consult applies to
  the recovered `device_ed25519` regardless of issuer, so quorum-issued certs inherit
  enforcement for free.

## §2 Handshake carry: own-fleet revocations on the friend-link frames

### Wire

New self-authenticating attestation pair, defined in `iroh_friend_acceptor.rs` with that
module's single-char-key convention (`dm_envelope::RevocationPushBody` is module-private and
uses two-char keys — same cert pair, not reusable across the module boundary):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationAttestation {
    #[serde(rename = "r")]
    pub revocation: RevocationCert,
    #[serde(rename = "e")]
    pub enrollment: Box<EnrollmentCert>,
}
```

Both `FriendLinkRequest` and `FriendLinkAccepted` gain:

```rust
/// ZEB-680: the sender's own fleet's device revocations (Master-issued
/// RevocationCert + the retired device's EnrollmentCert), so a NEW friend
/// learns of past revocations at link time. Not signature-bound — each pair
/// is independently self-authenticating (ZEB-677 signer_certs precedent).
/// Absent/empty for pre-ZEB-680 peers.
#[serde(rename = "v", default, skip_serializing_if = "Vec::is_empty")]
pub revocations: Vec<RevocationAttestation>,
```

- Deserialization is capped at `MAX_CARRIED_REVOCATIONS = 32`, and exceeding the cap is a
  **decode error that rejects the frame** — the established hostile-peer convention
  (`vec_devhash_capped` / `MAX_DEVICES_PER_OWNER` visitors error, they never truncate).
  Serialization sends at most the same cap (smallest-N by byte order for determinism,
  matching the ZEB-692 store-cap convention).
- **Byte pins (execution amendment, T1 review):** `zeb370_fixtures` DOES byte-pin both
  friend-link frames — the pins stay valid because an empty `revocations` vec skips the
  `"v"` key entirely (`skip_serializing_if`), and fixture literals stay empty. Old-frame
  decoding is covered by `serde(default)`; the intro/PEX frames (`IntroduceRequest`,
  `Introduction`, catalog) are **untouched**, so the zeb375/zeb376 hex pins stay byte-identical.

### Send side

Build from the trust snapshot exactly like `push_revocation_to_friends`: for each
`RevocationCert` in `OwnerState::revocations` (harmony-owner trust state), pair it with
`enrollments.get(&rc.target)`; skip pairs with no enrollment on record (warn, matching the
push path); include Master-issued revocations only (the set `verify_revocation_push` accepts);
cap at 32. Attach on both the request (initiator) and the Accepted response (responder).

### Receive side

Two phases, deliberately split:

1. **Verify at auth time (pure, no writes):** alongside the existing handshake auth, run
   `dm_outbox::verify_revocation_push(peer_owner, &att.revocation, &att.enrollment)` for each
   carried attestation. This enforces the trust-bind (a peer may only attest **their own**
   devices — `revocation.owner_id == peer_owner`) and the target↔enrollment binding. Any
   present-but-invalid attestation **rejects the handshake** (fail-closed, new typed error);
   an empty/absent list is the back-compat no-op.
2. **Apply at establishment time (writes):** only once the friendship is actually established
   (acceptor: post-consent accept path; dialer: after verifying the Accepted response), apply
   each verified pair via the `handle_revocation_push` machinery — store into
   `OwnerState.revoked_dm_devices` (union-merged, 256/owner cap) and feed
   `RevokedDeviceProjection`. **Every local `OwnerState` mutation must be followed by the
   owner-state sync engine's `notify_dirty()`** — without it the learned revocations are
   neither persisted nor replicated to the owner's other devices (established ZEB-248/#473
   invariant; the existing `RevocationPush` receive path shows the pattern).

Strangers who never complete a handshake write nothing (phase 2 gating), so the carry adds no
pre-consent state-growth surface; phase-1 verification is bounded by the 32-pair cap.

## §3 Copy + ledger updates

- `RemoveDeviceDialog.svelte` honesty paragraph: rewrite to state that once the removal syncs
  (or a friend learns of it at link time), the device can no longer friend-handshake, be
  introduced, or vouch for introductions against peers who know. Also correct the now-stale
  clause "blocking messages to contacts you only DM directly lands in follow-up work" —
  ZEB-685 shipped that cutoff (stale copy, same paragraph, in scope as ticket item 3).
- ZEB-668 spec §8 ledger row "…and friends/PEX immediately" updated from
  `NO — lone-cert verifiers, certs never expire` to reflect: enforced against peers who have
  learned the revocation (community tombstones, RevocationPush, or link-time carry), with the
  stranger residual noted (ZEB-678 set the precedent of updating the row in place).

## §4 Threat model / accepted residuals

- **Stranger residual (accepted):** a revoked device initiating a friend link with a total
  stranger (no shared community, never a friend of anyone who knows) still succeeds — it
  simply omits its own revocation and no channel reaches the stranger first. Offline-first
  systems cannot close this without an online authority. Mitigation over time: community
  tombstones + the link-time carry from any of the owner's good devices. The §8 ledger row
  states this honestly.
- **Attestation forgery:** impossible beyond the peer's own fleet — `verify_revocation_push`
  binds `revocation.owner_id` to the link peer and `revocation.target` to the enrollment's
  `device_id`.
- **Stripping:** the field is not signature-bound (ZEB-677 precedent); the friend link runs
  over an end-to-end encrypted iroh connection, so in-flight stripping is not in the threat
  model, and the sender who would "strip" its own list is the stranger residual above.
- **DoS:** carried list capped at 32; per-owner store capped at 256 (ZEB-692); verification
  is bounded ed25519 checks; writes gated on established friendship.

## §5 Testing

- **Enforcement units (per site):** projection containing the peer's device key → typed
  rejection; empty projection → unchanged success. Sites: wrapper, friend request auth,
  intro request auth, introduction verify (voucher-revoked and subject-revoked separately),
  catalog auth, dialer-side response verify (via existing seams).
- **Wire:** round-trip with `revocations` present/absent; old-frame decode (field absent →
  empty vec); cap enforced on decode (33 pairs → decode error); structural key-order test for
  the new field. zeb375/zeb376 fixture suites must
  pass byte-identical (they don't touch friend-link frames — asserting the constraint held).
- **Carry integration:** send-side builds the pair list from a trust snapshot (skip missing
  enrollment, cap); receive-side valid attestation lands in `revoked_dm_devices` + projection
  and marks owner-state dirty; invalid attestation rejects the handshake; post-establishment
  gating (no writes on a consent-denied handshake).
- **End-to-end regression:** full friend handshake where the requester's device is revoked in
  the acceptor's projection → link refused (existing acceptor test harness).
- **Frontend:** vitest assertion on the rewritten honesty copy (no existing pin; add one).

## §6 Global constraints & gates

- zeb375/zeb376 wire fixtures byte-identical; intro/PEX/catalog frames carry **no** new fields.
- Friend-link frames stay back-compatible (`serde(default)`, skip-if-empty).
- `enrollment_verify` chokepoint stays pure; no async added to any verifier.
- Local `OwnerState` mutations pair with `notify_dirty()`.
- Gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features
  test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace
  --all-targets --features test-fixtures` (final sweep; `scripts/test-select` for iterative
  rounds); `npx tsc --noEmit` + `npx vitest run` from repo root.

## §7 Follow-up candidates (file at end if applicable)

- Quorum-issued **RevocationCert** acceptance in `verify_revocation_push` (today
  Master-issued only) — belongs to the ZEB-677 lost-master story, not here.
- Carrying revocations on re-handshakes/reconnects of existing friendships (today: push +
  backfill cover these; carry is link-time only).
