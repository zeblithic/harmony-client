# ZEB-339 — Community membership: sign with the enrolled device key + owner→device verification via EnrollmentCert

**Status:** Design approved 2026-05-29 (brainstorm with Jake). Ready for implementation planning.
**Linear:** [ZEB-339](https://linear.app/zeblith/issue/ZEB-339) (child of [ZEB-217](https://linear.app/zeblith/issue/ZEB-217), the community-membership epic).
**Scope:** harmony-client `src-tauri` (community membership + publisher-auth only). DM and owner-state signing are explicitly **out of scope** (deferred — see §10).

---

## 1. Problem

A freshly-minted owner on v0.1.1 clicks **Create community** and the bootstrap Join is rejected:

> `bootstrap Join not inserted (got Rejected(ActorPubkeyMismatch))`

### 1.1 Root cause

`community_membership::verify_signature` enforces a **flat identity check**:

```rust
let identity = harmony_identity::Identity::from_public_bytes(actor_identity_pub)?;
if identity.address_hash != event.actor.0 {
    return Err(VerifyError::ActorPubkeyMismatch);
}
```

i.e. "the signer's address must *equal* the actor." But production (`mint_community_creation`, `lib.rs`) sets:

- `actor = self_owner = OwnerAddr(loaded.state.owner_id)` — the **harmony-owner master `owner_id`**, and
- signs with the **Reticulum device key** (`signing_key` sourced from `reticulum_identity_bytes`).

These are unrelated keypairs. Worse, `owner_id` is **not a `harmony_identity` address at all**: harmony-owner computes it as `SHA256(canonical_cbor{ed25519_verify, ml_dsa_verify})[:16]` (`pubkey_bundle.rs::identity_hash`), whereas `verify_signature` computes `SHA256(X25519 ‖ Ed25519)[:16]`. And the master signing key is **dropped + zeroized at mint** (`mint.rs`), so we can never sign *as* `owner_id`. The flat check therefore can never pass.

### 1.2 The breakage is systemic

The same `actor = owner_id` + sign-with-Reticulum-key pattern is in `redeem_invite`, `leave`, `kick`, `unban`, `admin-countersign`, and channel events, plus the ZEB-256 community **publisher-auth** path (whose `OwnerDeviceCacheResolver` reinterprets `OwnerAddr` as a `DeviceIdentityHash` and looks it up in a cache keyed by Reticulum hashes). All of them funnel through the same `OwnerAddr`-as-device-address assumption that the harmony-owner `owner_id` integration silently broke.

### 1.3 Why CI stayed green

Every existing test derives `actor` **and** the signing key from a single `PrivateIdentity` (e.g. `let self_owner = OwnerAddr(identity.identity.address_hash); let signing_key = signing_key_from_identity(&identity);`). So `address_hash(signing_key) == actor` is always true in tests. No test drives the **production pairing** — `actor = owner_id` from a real `load_owner_state`, signing via a *separate* key. Closing this gap is a first-class requirement (§9).

---

## 2. The three identities (why this is subtle)

| # | Identity | `id` scheme | Role | Enrolled under owner? | Signs community events today? |
|---|----------|-------------|------|----------------------|-------------------------------|
| 1 | Owner **master** | `SHA256(cbor{ed25519,ml_dsa})[:16]` = `owner_id` | root of trust | n/a (it is the root) | no — key dropped after mint |
| 2 | harmony-owner **device** (`device_signing_key`) | owner-crate hash | the device harmony-owner certifies | **yes** — `Master` `EnrollmentCert` written at mint, persisted in `OwnerState.enrollments` | **no — loaded but used only for a Devices-panel display string** |
| 3 | **Reticulum** identity (`identity.key`) | `harmony_identity::address_hash` | transport + all runtime signing (DM, owner-state, community) | **no** | **yes** |

harmony-owner already produces a sound owner→device proof — the `EnrollmentCert` — but it certifies device #2, while community events are signed by device #3. There is no cryptographic link between the Reticulum key and `owner_id`. **That missing link is the real gap.**

### 2.1 Decision: sign with the enrolled device key (#2)

We switch community signing from the Reticulum key (#3) to the enrolled `device_signing_key` (#2). The key harmony-owner certifies becomes the key that signs, so the `EnrollmentCert` is a *direct* owner→signer proof. `actor` stays `owner_id`.

Two facts make this clean and cheap:

- Device #2 and its `Master` `EnrollmentCert` are **already created at mint** and persisted in `OwnerState.enrollments` on every installed identity — **no re-mint required**.
- At mint the device/master `PubKeyBundle`s are `post_quantum: None` with a *stubbed* `x25519_pub = [0u8;32]`, so an `EnrollmentCert` serializes to **~200 bytes**. The stubbed X25519 also confirms device #2 cannot do DM encryption — which is exactly why this work is **community-only** (membership needs device #2 only to *sign*, ed25519; DM/owner-state keep the Reticulum key with its real X25519).

---

## 3. Trust chain (replaces the flat check)

A verifier with only the event + its enrollment cert can confirm, with no prior state and no central authority:

```text
event.actor = owner_id ─────────────────────────────┐
                                                     │ must match
EnrollmentCert (Master{master_pubkey})               ▼
  ├─ cert.owner_id            ════════════════════►  owner_id
  ├─ hash(master_pubkey)      ─── cert.verify() ───►  == cert.owner_id   ✓ owner identity
  ├─ master sig over device   ─── cert.verify() ───►  valid              ✓ owner vouches device
  └─ cert.device_pubkeys.ed25519 ──────────┐
                                            │ verify_strict
event.sig  ─────────────────────────────────►  over canonical EventPayload  ✓ device authored it
```

`harmony_owner::certs::enrollment::EnrollmentCert::verify()` already performs the cert-internal checks for `Master` certs: `master_pubkey.identity_hash() == owner_id`, ed25519 signature over the canonical signing payload, and `device_pubkeys.identity_hash() == device_id`. We layer two community-level checks around it: `cert.owner_id == event.actor.0`, and the event signature verifies under `cert.device_pubkeys.classical.ed25519_verify`.

This is self-sovereign and works **cross-owner** (Koya verifying KRILE's events) because the cert is self-contained — `Master{master_pubkey}` embeds the master public bundle.

---

## 4. Signing switch (wiring)

`start_node` currently plumbs the Reticulum `signing_key` into the community paths. We instead plumb `LoadedOwnerState.device_signing_key` (#2) into:

- `mint_community_creation` / `create_community_inner` (bootstrap Join),
- `redeem_invite` / `mint_redemption` (joiner Join, PendingJoin),
- `leave` / `kick` / `unban` / `set-power` / channel-event mints,
- `attach_countersig*` (JoinCountersign, AdminCountersign),
- `generate_invite` (InviteToken signing),
- community state-root publishing (ZEB-256 `CommunityRootPublishPayload`).

No event-field *meaning* changes — `actor` stays `owner_id` everywhere; only the pen changes. The Reticulum key remains the signer for DM and owner-state (untouched).

The client obtains its own cert at runtime via `OwnerState.enrollments[derive_this_device_id(device_signing_key)]` and attaches it to outbound identity-introducing events (§5).

---

## 5. Wire format

### 5.1 `SignedMembershipEvent` gains an optional cert

```rust
pub struct SignedMembershipEvent {
    // ... id, community_id (ci), kind (kn), actor (ac), at, sig (sg), countersig (cs) — unchanged ...

    /// ZEB-339: enrolment proof for the signer. REQUIRED on identity-
    /// introducing events (bootstrap Join, Join, PendingJoin); absent
    /// otherwise (the verifier resolves the signer's device key from
    /// materialized membership). Sits OUTSIDE the signed EventPayload —
    /// safe because cert.owner_id must equal the signed `actor`, the cert
    /// is master-signed (unforgeable), and the event sig must verify under
    /// cert.device_pubkeys.
    #[serde(rename = "en", skip_serializing_if = "Option::is_none", default)]
    pub enrollment: Option<harmony_owner::certs::enrollment::EnrollmentCert>,
}
```

- **Required** on: bootstrap Join, `Join`, `PendingJoin`, and carried in the **invite payload** (the inviter's cert — the invite is the first-contact artifact).
- **Absent** on: `Leave`, `Kick`, `SetPower`, `Unban`, `ChannelCreate/Modify/Delete`, `AdminProposal/Countersign`, `JoinCountersign`, `EpochRotation/Catchup`, `Fork`, `ReachabilityAnnounce`, and community publishes — resolved from materialized membership.
- The cert is **not** part of `EventPayload` (the signed bytes), so it does not change the signature domain.

This rides an invariant `verify_event` *already* enforces: a non-Join actor must be a member in `prior_state` (the power check). Identity resolution uses the same causal ordering — if the actor's Join isn't materialized yet, resolution fails in the same class as "not a member."

### 5.2 `PendingJoin` drops the redundant pub

`MembershipEventKind::PendingJoin { invite_token, joiner_identity_pub: [u8;64] }` → **remove `joiner_identity_pub`**. The joiner is the `actor` of the PendingJoin, so their cert rides on the event's `enrollment` field; the old 64-byte pub was a half-stubbed value under the new model. (`InviteToken` is unchanged in shape; it is now signed by the admin's device #2 — see §6.)

### 5.3 Invite payload carries the inviter's cert

The `harmony://invite/...` payload (community_invite.rs) carries the **inviter's `EnrollmentCert`** alongside the existing `InviteToken` + membership key, so a joiner who has not yet synced the community log can verify the inviter's owner→device binding (and thus the `InviteToken` signature) at first contact.

---

## 6. Verification path

### 6.1 New signer-verification primitive

Replace the flat `verify_signature` with a cert-aware primitive:

```rust
/// The minimal proven fact: this ed25519 key is a device enrolled under `owner`.
struct EnrolledDeviceKey { owner: OwnerAddr, device_ed25519: [u8; 32] }

/// Verify the event was authored by a device enrolled under `event.actor`'s owner.
fn verify_membership_signer(
    event: &SignedMembershipEvent,
    signer: &EnrolledDeviceKey,
) -> Result<(), VerifyError>;
```

`verify_event` obtains `signer` two ways:

- **Join / PendingJoin / bootstrap** → from the event's carried `enrollment` cert: `cert.verify()` → `EnrollmentCertInvalid` on failure; assert `cert.owner_id == event.actor.0` → `EnrollmentOwnerMismatch`; take `cert.device_pubkeys.classical.ed25519_verify`.
- **All other kinds** → from `prior_state.members[event.actor].enrolled_device_keys`; if the actor is absent or the key set does not contain the signing key → `SignerNotEnrolledForActor`.

Then ed25519 `verify_strict` of `event.sig` over the canonical `EventPayload` → `SignatureInvalid`.

### 6.2 `VerifyContext` simplification

The pre-resolved 64-byte pub fields exist only because callers resolved them via `OwnerDeviceCacheResolver`:

```rust
// REMOVED from VerifyContext:
//   actor_identity_pub: &'a [u8;64]
//   countersigner_identity_pub: Option<&'a [u8;64]>
//   admin_identity_pub: Option<&'a [u8;64]>
```

`verify_event` now derives all needed keys itself (from cert or `prior_state`), so the `OwnerDeviceCacheResolver` dependency is **removed from the community path entirely**. Callers get simpler. `VerifyContext` retains `expected_community_id`, `admin_addr`, `is_invite_only`.

### 6.3 Error taxonomy

`ActorPubkeyMismatch` is replaced by precise variants:

| Variant | Meaning |
|---|---|
| `MissingEnrollmentCert` | Join/PendingJoin arrived with no `enrollment` cert |
| `EnrollmentCertInvalid` | `cert.verify()` failed (bad master sig / `hash(master)!=owner_id` / device-id mismatch) |
| `EnrollmentOwnerMismatch` | `cert.owner_id != event.actor.0` |
| `SignerNotEnrolledForActor` | materialized lookup found no matching enrolled device key for `actor` |
| `SignatureInvalid` | (kept) ed25519 verify_strict failed |

### 6.4 Counter-signature verification

`verify_countersig` likewise resolves the counter-signer's device key from materialized membership (the counter-signer is a member with power ≥ invite threshold) instead of the flat address check. A dedicated `CounterSignerNotEnrolled` variant replaces `CounterSignerPubkeyMismatch`.

### 6.5 `InviteToken` signature (PendingJoin)

`PendingJoin` carries an `InviteToken` signed by the admin/inviter. Verifying it needs the **inviter's** enrolled device key — obtained from materialized membership (the inviter is a Joined member; the community creator's bootstrap Join is the genesis, so its enrolled key is always materialized first), or, at a joiner's first contact before the log is synced, from the **inviter `EnrollmentCert` carried in the invite payload** (§5.3). This is why the removed `VerifyContext.admin_identity_pub` is no longer needed: the admin key is resolved the same way as every other signer, not threaded in by the caller.

---

## 7. Materialized membership

### 7.1 `MemberState` records enrolled keys

```rust
pub struct MemberState {
    #[serde(rename = "st")] pub status: MemberStatus,
    #[serde(rename = "ja")] pub joined_at: Hlc,
    #[serde(rename = "la", skip_serializing_if = "Option::is_none", default)] pub left_at: Option<Hlc>,

    /// ZEB-339: ed25519 verify keys vouched under this member's owner_id,
    /// learned from the EnrollmentCert carried on their Join. A SET so an
    /// owner with multiple devices in a community is representable (eventual
    /// state); populated with exactly one today.
    #[serde(rename = "ek", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub enrolled_device_keys: BTreeSet<[u8; 32]>,
}
```

`BTreeSet` keeps canonical CBOR deterministic (same constraint the existing `BTreeMap` fields cite). `#[serde(default)]` decodes pre-ZEB-339 cached snapshots as an empty set.

### 7.2 `materialize`

When a Join carrying its cert is applied, insert `cert.device_pubkeys.classical.ed25519_verify` into the member's `enrolled_device_keys` (in addition to the existing status/joined_at update). `materialize` rebuilds this from the event log on each call, like the other derived fields — so the set is a function of the (cert-bearing) Join events.

> **Eventual multi-device:** a future device-introduction event (or a Join from a second device of the same owner) carrying its own cert would add to the set. This spec does not introduce such an event; it only ensures the data model and verify path accommodate `|enrolled_device_keys| > 1`.

---

## 8. Publisher-auth (ZEB-256) — same fix, no new wire

`CommunityRootPublishPayload` already carries `publisher_addr (= owner_id)` + `publisher_sig` (no new field needed). Changes:

1. Sign the publish with **device #2**.
2. The inbound path **replaces** `OwnerDeviceCacheResolver.resolve(publisher_addr)` with a lookup into **materialized membership**: the existing membership-at-HLC gate already guarantees the publisher is `Joined`, so `members[publisher_addr].enrolled_device_keys` is populated — verify `publisher_sig` against it (any key in the set).
3. Remove the `IdentityResolver`/`OwnerDeviceCacheResolver` wiring from the community publish path.

Fixing the resolution model fixes create, membership verify, and publisher-auth together, and deletes the resolver hack rather than patching it.

---

## 9. Testing (closes the gap that let this ship)

1. **Production-pairing test (must-have):** drive `create_community_inner` with `self_owner = owner_id` from a real `load_owner_state` and signing via `device_signing_key` — the production pairing where `actor ≠ address_hash(signing_key)`. Asserted to **fail on current code, pass on the fix**.
2. **Cross-owner end-to-end:** two distinct owners (creator + joiner, each its own mint) exercise create → invite → PendingJoin → counter-sign → join; each side accepts the other purely from carried certs (no shared cache).
3. **Negative tests, one per error variant:** forged cert (bad master sig) → `EnrollmentCertInvalid`; `cert.owner_id != actor` → `EnrollmentOwnerMismatch`; signer key not in actor's enrolled set → `SignerNotEnrolledForActor`; Join with no cert → `MissingEnrollmentCert`; tampered event bytes → `SignatureInvalid`.
4. **Wire fixtures (pinned CBOR):** a Join-with-cert and a steady-state event-without-cert; bump the membership-event wire fixture; confirm old fixtures still decode (serde-default on `enrollment` + `enrolled_device_keys`).
5. **Publisher-auth test:** a member's publish verifies against its materialized enrolled key; a non-member / wrong-key publish is rejected.
6. **Regression guard:** a structural test ensuring community signing paths consume `device_signing_key`, not the Reticulum key (mirrors the ZEB-338 phrasing-regression guard pattern).

---

## 10. Out of scope / follow-ups

- **DM + owner-state signing stay on the Reticulum key.** They currently work; unifying the entire runtime device identity onto device #2 (re-keying `OwnerDeviceCache` from Reticulum `address_hash` to device #2) is a larger, higher-regression change not needed to unblock community create / cross-WAN. **Follow-up ticket to be filed** to track that unification.
- **Multi-device-per-owner in a community** (populating `enrolled_device_keys` with >1, device-introduction events). The data model and verify path are designed to accommodate it; no new event ships here.
- **Quorum (`EnrollmentIssuer::Quorum`) certs.** Mint produces `Master` certs only; quorum-issued device certs verify via `OwnerState` walk-back and are not exercised by single-device alpha. Verification should reject or defer `Quorum` issuers gracefully (no panic) and is otherwise untested here.

---

## 11. Files touched (anticipated)

- `src-tauri/src/community_membership.rs` — `SignedMembershipEvent.enrollment`, `PendingJoin` field removal, `verify_membership_signer`, `verify_event`/`VerifyContext`, error enum, `MemberState.enrolled_device_keys`, `materialize`, counter-sign verify.
- `src-tauri/src/community_state_sync.rs` — publisher-auth resolution via materialized membership; remove `OwnerDeviceCacheResolver` from the community path.
- `src-tauri/src/community_invite.rs` — invite payload carries the inviter's cert; redemption/counter-sign signing + verify.
- `src-tauri/src/lib.rs` — plumb `device_signing_key` into all community mint/sign sites (`mint_community_creation`, `create_community_inner`, `redeem_invite`/`mint_redemption`, leave/kick/unban/set-power/channel mints, `generate_invite`); read own `EnrollmentCert` from owner state.
- Tests + `tests/wire_format_*` fixtures as per §9.
