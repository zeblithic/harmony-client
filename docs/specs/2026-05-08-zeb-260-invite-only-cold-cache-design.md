# ZEB-260 Phase 4 Invite-Only Cold-Cache Bootstrap — Design Spec

**Ticket:** [ZEB-260](https://linear.app/zeblith/issue/ZEB-260) (parent: [ZEB-256](https://linear.app/zeblith/issue/ZEB-256))
**Date:** 2026-05-08
**Branch:** `zeb-260-invite-only-cold-cache`
**Related ship:** PR #89 (commit `7d32256`) — ZEB-262 Phase 4 invite-only redemption + ZEB-258 atomic-rollback
**Status:** Design

---

## Summary

Phase 4 invite-only redemption (PR #89) ships with a documented integration-test caveat: the test pre-seeds the joiner's CRDT engine with the admin's bootstrap event because production has no equivalent path. Without that pre-seed, the receive-side membership-at-HLC gate (`community_state_sync.rs::handle_incoming_publish` step 2) rejects the admin's publish-back, the joiner's `pending_redemption` oneshot never fires, and `redeem_invite_inner` times out. End-to-end invite-only redemption does not work in production today.

This spec adds a side-channel to the invite URL itself: the URL carries the admin's signed self-Join event (the bootstrap), and the joiner inserts it into the engine before sending the redemption unicast. By the time the admin's publish-back arrives, the joiner's CRDT has the admin as `Joined`, the gate admits, the merge proceeds, the oneshot fires.

The fix is structurally local: two new fields on `CommunityInvitePayload`, a six-step verification chain in `redeem_invite_inner`, no changes to the gate or the publish-back wire format.

## Scope

**In scope (Case A):**
- Phase 4 invite-only new-joiner Bob with empty CRDT receiving admin Alice's first publish-back. The joiner's view starts cold; the admin's bootstrap is unreachable except through the encrypted publish blob, which is post-gate.

**Out of scope (Cases B and C — defer per existing ZEB-260 ticket recommendation):**
- **Case B:** Open-community brand-new joiner whose self-Join is only inside their own publish blob. Receivers reject because the publisher is unknown.
- **Case C:** Self-Re-Join after Leave. Same gate-vs-blob inversion.

Cases B and C share the same root cause but require a *gate redesign* (blob pre-decrypt or self-publisher-bootstrap), not a side-channel. Bundling them with Case A would couple a tactical fix to a strategic gate change. They remain deferred until a real production blocker emerges. The Linear ZEB-260 ticket description will be re-scoped to Case A; Cases B+C carry forward to a follow-up ticket the user files (per HARD RULE: never invent Linear IDs).

## Background

### The membership-at-HLC gate

`community_state_sync.rs::handle_incoming_publish` evaluates membership status of the publisher *before* decrypting the blob:

```text
let materialized = prior_state_at_hlc(&events, &payload.at, ctx.admin_addr);
let member_state = materialized.members.get(&payload.publisher_addr).cloned();
if member_state.map(|s| s.status) != Some(Joined) {
    return ErrPreMutation(PublisherNotJoined { addr: publisher_addr });
}
```

The gate is correct and load-bearing for the censorship-defense threat model: a kicked member's publish must be rejected even though they retain the `MembershipKey` until ZEB-249 ships. Pre-decrypt evaluation is also a DoS pre-filter — we reject before paying decrypt cost.

### Why invite-only Phase 4 hits this

In invite-only Phase 4 redemption:

1. Admin Alice mints an invite URL containing `community_id`, `membership_key`, `admin_addr`, and (for invite-only) a signed `InviteToken`.
2. Joiner Bob decodes the URL, spawns his engine (empty CRDT), mints his own `bootstrap_join` (uncountersigned), sends a `CommunityInvitePacket` unicast to Alice carrying the join event + the `InviteToken`.
3. Alice's `community_invite::handle_unicast` verifies, attaches her counter-sig, calls `engine.insert_local_event_with_pubs(counter_signed_join, bob_pub, Some(alice_pub))`.
4. Alice's engine triggers a state-root publish carrying the counter-signed Join.
5. Bob's engine receives the publish via Zenoh subscription.
6. Bob's gate runs `prior_state_at_hlc` over Bob's local CRDT — which is empty. Alice (the `publisher_addr`) is unknown. **Reject `PublisherNotJoined`.**
7. The merge never happens. Bob's `pending_redemption` oneshot never fires. `redeem_invite_inner` times out.

Bob has no other path to Alice's bootstrap event. The admin's bootstrap is inside Alice's CRDT, which only ships via state-root publishes that the gate just rejected.

### Why the test pre-seed papers over this

The integration test at `src-tauri/tests/community_invite_only_integration.rs:365-430` carries an inline ZEB-260 OOB pre-seed:

```rust
bob_engine
    .insert_local_event(alice_minted.bootstrap_join.clone())
    .await
    .expect("bob pre-seed Alice bootstrap (ZEB-260 OOB)");
```

The test author had access to `alice_minted.bootstrap_join` (an in-process artifact of the test's `mint_invite` call) and inserted it directly. Production has no such cross-process artifact: Bob receives only the encoded URL bytes.

## Architecture

The fix adds **two side-channel fields**: the invite URL carries Alice's signed self-Join event (`admin_bootstrap`) and her identity public bytes (`admin_identity_pub`) so Bob can verify and insert her bootstrap into his engine before sending the redemption unicast. By the time Alice's publish-back arrives, Bob's CRDT has Alice as `Joined`, so the gate admits.

### What does NOT change

- The membership-at-HLC gate (`community_state_sync.rs::handle_incoming_publish`) is unchanged.
- The state-root publish wire format is unchanged.
- The encrypted-blob decryption pipeline is unchanged.
- The `IdentityResolver` `OwnerDeviceCacheResolver` cold-cache behavior is unchanged.
- The unicast `CommunityInvitePacket` wire format is unchanged.

### What changes

- **`community_invite.rs`:** `CommunityInvitePayload` gains two fields (`admin_bootstrap`, `admin_identity_pub`); the URL CBOR encoding gains two new keys (`ab`, `ap`).
- **`lib.rs::redeem_invite_inner`:** after `spawn_engine` and before sending the unicast, the joiner verifies the admin bootstrap's six-step chain and inserts via `engine.insert_local_event_with_pubs(admin_bootstrap, admin_identity_pub, None)`.
- **Tests:** the pre-seed in `community_invite_only_integration.rs` is removed (production now flows naturally end-to-end). New unit tests cover the verification chain. New wire-format fixtures cover the extended CBOR shape.

## Wire format

### `CommunityInvitePayload` extension

Two new fields, both REQUIRED for invite-only payloads, IGNORED for open-community payloads:

```rust
pub struct CommunityInvitePayload {
    // ... existing fields (community_id, membership_key, admin_addr,
    // community_name, is_invite_only, expires_at, invite_token) ...

    /// Admin's signed self-Join event. Required when `is_invite_only` is true.
    /// Bob inserts this into his engine during `redeem_invite_inner` so the
    /// admin's eventual publish-back passes the receive-side membership-at-HLC
    /// gate. (Without this, Bob's empty CRDT rejects the publish; see ZEB-260.)
    #[serde(rename = "ab", skip_serializing_if = "Option::is_none", default)]
    pub admin_bootstrap: Option<SignedMembershipEvent>,

    /// Admin's Ed25519 identity_pub (32-byte signing pub + 32-byte agreement
    /// pub concatenation, matching the existing `identity_pub` shape used by
    /// `harmony_identity::Identity::from_public_bytes`). Required when
    /// `is_invite_only` is true. Used to verify `admin_bootstrap` and
    /// passed to `insert_local_event_with_pubs`.
    #[serde(rename = "ap", skip_serializing_if = "Option::is_none", default)]
    #[serde(with = "serde_bytes_array")]
    pub admin_identity_pub: Option<[u8; 64]>,
}
```

CBOR keys remain same-length-2 per project convention (`ci`, `mk`, `ad`, `nm`, `io`, `ex`, `tk`, `ab`, `ap`).

`Option<...>` wrapping with `skip_serializing_if = "Option::is_none"` keeps open-community URLs byte-identical to today (no new CBOR keys appear). Invite-only URLs gain ~250 bytes (180 signed bootstrap + 64 sig + 64 admin pub, base64-encoded). Total invite-only URL stays under 700 bytes — well within QR-code budgets and well under URL length limits.

### Bootstrap event shape

`SignedMembershipEvent` (existing, in `community_membership.rs`) — unchanged. The bootstrap is admin's self-Join:

- `actor` = admin's `OwnerAddr`
- `community_id` = the community
- `kind` = `MembershipEventKind::Join { ... }`
- `at` = HLC at community creation
- `sig` = Ed25519 over canonical CBOR of `EventPayload`
- `countersig` = `None` (admin's bootstrap is always self-only)

## Verification chain

In `redeem_invite_inner`, after URL decode and before sending the unicast, Bob runs six checks against `admin_bootstrap` and `admin_identity_pub`:

```rust
// 1. Required-fields check (invite-only mode).
if payload.is_invite_only {
    let admin_bootstrap = payload.admin_bootstrap.as_ref()
        .ok_or(RedeemInviteError::BootstrapMissing)?;
    let admin_identity_pub = payload.admin_identity_pub.as_ref()
        .ok_or(RedeemInviteError::BootstrapMissing)?;

    // 2. identity_pub ↔ admin_addr binding.
    let admin_identity = harmony_identity::Identity::from_public_bytes(admin_identity_pub)
        .map_err(|_| RedeemInviteError::BootstrapInvalidPubkey)?;
    if admin_identity.address_hash != payload.admin_addr.0 {
        return Err(RedeemInviteError::BootstrapAddressMismatch);
    }

    // 3. bootstrap.actor ↔ admin_addr binding.
    if admin_bootstrap.actor != payload.admin_addr {
        return Err(RedeemInviteError::BootstrapActorMismatch);
    }

    // 4. bootstrap.community_id ↔ payload.community_id binding.
    if admin_bootstrap.community_id != payload.community_id {
        return Err(RedeemInviteError::BootstrapCommunityMismatch);
    }

    // 5. Ed25519 signature verification.
    community_membership::verify_signature(admin_bootstrap, admin_identity_pub)
        .map_err(|_| RedeemInviteError::BootstrapSignatureInvalid)?;

    // 6. Sanity: bootstrap is a self-Join with no countersig.
    if !matches!(admin_bootstrap.kind, MembershipEventKind::Join { .. })
        || admin_bootstrap.countersig.is_some() {
        return Err(RedeemInviteError::BootstrapKindInvalid);
    }
}
```

If all six pass, Bob calls:

```rust
engine.insert_local_event_with_pubs(
    admin_bootstrap.clone(),
    *admin_identity_pub,
    None,  // bootstrap has no countersig
).await.map_err(RedeemInviteError::BootstrapInsertFailed)?;
```

`insert_local_event_with_pubs` is the API added in PR #89 specifically for cold-cache bypass of `IdentityResolver`. It runs the engine's standard verify_event + state-mutate + post-Inserted hook chain with explicitly-provided pubkeys. Idempotent: if Bob has already inserted the bootstrap (rare — only if a prior aborted redemption to the same admin reached this step), the engine deduplicates by event id and returns `InsertOutcome::AlreadyPresent`.

### Tampering resistance

The four binding checks (steps 2-5) form a closed chain. Tampering with any of `{admin_bootstrap.actor, admin_bootstrap.community_id, admin_bootstrap.sig, admin_identity_pub, admin_addr}` breaks at least one check:

- Replacing `admin_bootstrap` alone → step 5 fails (sig doesn't verify under admin's pubkey).
- Replacing `admin_identity_pub` alone → step 2 fails (address_hash mismatch).
- Replacing both `admin_bootstrap` + `admin_identity_pub` with a coherent attacker-signed set → step 2 fails (attacker's identity_pub.address_hash ≠ admin_addr in payload).
- Replacing all three of `admin_bootstrap` + `admin_identity_pub` + `admin_addr` → step 4 fails (bootstrap.community_id ≠ payload.community_id) OR `MembershipKey` (encryption key) mismatch downstream.

The `community_id` field appears in both the bootstrap event and the payload top-level; tampering breaks the binding. The `MembershipKey` is a separate encryption key not tied to the bootstrap, but the gate-admit path requires the bootstrap-as-Joined precondition, which an attacker cannot forge.

## Order in `redeem_invite_inner`

Insert sequence on Bob's side, after URL decode:

1. Decode + verify URL (existing).
2. `spawn_engine(community_id, membership_key, admin_addr, is_invite_only, pub_tx, sub_rx)` (existing).
3. **Verify `admin_bootstrap` chain (6 checks).** *(NEW)*
4. **`engine.insert_local_event_with_pubs(admin_bootstrap, admin_pub, None)`.** *(NEW)*
5. Mint Bob's `bootstrap_join` (existing).
6. Register `pending_redemption` oneshot (existing).
7. Send unicast `CommunityInvitePacket` (existing).
8. Wait on oneshot with timeout (existing).
9. On success: commit owner-state Space LAST (existing — ZEB-258 atomic-rollback ordering).

Steps 3+4 sit between `spawn_engine` and the unicast send. Reasons:

- **AFTER `spawn_engine`:** the engine must exist to accept the event.
- **BEFORE the unicast send:** the admin's publish-back is generated *strictly later* than the admin receives the unicast (admin counter-signs, inserts, then publishes). Therefore the publish-back cannot arrive at Bob before Bob has the bootstrap. **No race window.**

If steps 3 or 4 fail, Bob aborts redemption and tears down the engine via `shutdown_engine_and_cleanup_persistence` — same rollback path as the existing partial-unicast-send / pending-redemption-timeout cases in ZEB-262. **Re-redemption guard:** the teardown only runs when the engine was freshly spawned by *this* redemption (`engine_already_existed=false`). On a re-redeem retry where the engine was already running from a prior successful path, the rollback skips the teardown so the prior state survives. This mirrors how `spawn_engine` itself is idempotent on re-redemption: the freshly-built channels are dropped and the existing adapter pair stays live. The same guard is applied to every rollback site in `redeem_invite_inner`'s invite-only branch (verify-failure, engine-vanish, insert-failure, missing-invite-token, build-packet, encode-packet, no-destinations, all-sends-failed, oneshot-recv-err, timeout, fence-check, apply-rejected) and the OPEN-branch insert paths.

## Error taxonomy

Extend the redeem-side error enum with seven new variants:

```rust
pub enum RedeemInviteError {
    // ... existing variants ...

    /// Invite-only payload missing `admin_bootstrap` or `admin_identity_pub`.
    /// Fires for old PR #89 invite URLs, which lack these fields. Stable IPC
    /// error string: "redeem_invite: invite-only payload missing admin bootstrap".
    BootstrapMissing,

    /// `admin_identity_pub` bytes are not a valid Ed25519 + X25519 pair.
    BootstrapInvalidPubkey,

    /// `Identity::from_public_bytes(admin_identity_pub).address_hash`
    /// does not equal `payload.admin_addr.0`.
    BootstrapAddressMismatch,

    /// `admin_bootstrap.actor` does not equal `payload.admin_addr`.
    BootstrapActorMismatch,

    /// `admin_bootstrap.community_id` does not equal `payload.community_id`.
    BootstrapCommunityMismatch,

    /// Ed25519 signature verification failed under `admin_identity_pub`.
    BootstrapSignatureInvalid,

    /// `admin_bootstrap.kind` is not `Join`, or `countersig` is `Some`.
    BootstrapKindInvalid,

    /// `engine.insert_local_event_with_pubs` returned an error. Should be
    /// effectively unreachable if the chain checks pass; surfaced for
    /// telemetry. Wraps the underlying `LocalInsertError` for debugging.
    BootstrapInsertFailed(LocalInsertError),
}
```

Each variant has a `Display` impl producing a stable IPC error string (NOT `Debug` repr, per the project's IPC discipline established in ZEB-262). Frontend rejection strings are stable across builds.

A `reason_tag()` method on the enum returns a short telemetry tag (`"bootstrap_missing"`, `"bootstrap_address_mismatch"`, etc.) for the existing `record_redeem_outcome` telemetry helper.

## Backward compatibility

PR #89 ships invite-only with no `admin_bootstrap` / `admin_identity_pub` fields. Old invite URLs lack `ab` / `ap`. Decision:

**Reject old invite-only URLs as `BootstrapMissing`.** Phase 4 invite-only has never been used in production; there are no real users to break. Re-issued invite URLs from updated builds carry the new fields and work end-to-end.

Open-community URLs (`is_invite_only == false`) are unchanged. The new fields are ignored. CBOR encoding stays byte-identical for open-community payloads thanks to `skip_serializing_if = "Option::is_none"`.

The existing `wire_format_community_fixtures.rs` open-community fixtures stay unchanged. Invite-only fixtures are updated to include the new fields.

## Testing

### Unit tests (`community_invite_unit.rs`, additions)

Each test constructs a synthetic `CommunityInvitePayload` and exercises one verification-chain branch:

- `redeem_rejects_invite_only_without_admin_bootstrap` — payload with `is_invite_only=true` but `admin_bootstrap=None` → `BootstrapMissing`.
- `redeem_rejects_invite_only_without_admin_identity_pub` — payload with bootstrap but no pubkey → `BootstrapMissing`.
- `redeem_rejects_admin_address_mismatch` — `admin_identity_pub.address_hash != admin_addr` → `BootstrapAddressMismatch`.
- `redeem_rejects_admin_actor_mismatch` — `admin_bootstrap.actor != admin_addr` → `BootstrapActorMismatch`.
- `redeem_rejects_admin_community_mismatch` — `admin_bootstrap.community_id != payload.community_id` → `BootstrapCommunityMismatch`.
- `redeem_rejects_invalid_admin_signature` — flipped sig byte → `BootstrapSignatureInvalid`.
- `redeem_rejects_admin_bootstrap_with_countersig` — bootstrap with non-`None` countersig → `BootstrapKindInvalid`.
- `redeem_rejects_admin_bootstrap_non_join_kind` — bootstrap with `Leave` kind → `BootstrapKindInvalid`.
- `redeem_admits_well_formed_admin_bootstrap` — happy-path verify passes (asserts no error before engine call).

These tests do not spawn an engine; they exercise the verification chain in isolation.

### Integration tests (`community_invite_only_integration.rs`, modifications)

- **Remove** the ZEB-260 OOB pre-seed (lines 365-430). Bob's engine starts empty and the test flows naturally end-to-end.
- Existing assertions stay: Alice has 2 events (her bootstrap + counter-signed Bob Join), Bob materializes Alice + himself as `Joined`.
- **New assertion:** Bob's CRDT contains exactly 2 events after redemption (admin's bootstrap + his own counter-signed Join), confirming the side-channel insert AND the publish-back merge both succeeded.

### New integration test

`community_invite_only_tampered_admin_bootstrap_rejects` — verifies a forged invite URL fails at the chain check, not at the publish-back gate. Asserts the tampering is caught synchronously inside `redeem_invite_inner` (before `spawn_engine` cleanup), and that the resulting error variant is the expected `Bootstrap*` variant for each tampering target.

### Wire-format fixtures (`wire_format_community_fixtures.rs`)

- Update the existing invite-only `CommunityInvitePayload` CBOR fixture to include `ab` + `ap` keys and pin canonical bytes.
- Add a fixture for the bootstrap-only sub-encoding (re-using the existing `SignedMembershipEvent` fixture pattern).

### `cargo fmt` + `cargo clippy` gates

Both run at every task verification (per HARD RULE established in PR #89). No new lint suppressions introduced.

## Spec / doc deltas

- **This spec:** `docs/specs/2026-05-08-zeb-260-invite-only-cold-cache-design.md` (new, this document).
- **Linear ZEB-260:** description re-scoped to Case A; Cases B+C carry forward to a follow-up ticket the user files.
- **`community_invite_only_integration.rs`:** ZEB-260 OOB pre-seed comment removed; replaced with a one-line note that production now seeds the bootstrap via the invite URL.
- **`community_state_sync.rs`:** the existing "Bootstrap caveat (tracked as ZEB-260)" comment in `handle_incoming_publish` is updated to clarify that Case A is fixed and Cases B+C remain.

## Open questions / risks

### Risk: invite URL size

Adding ~250 bytes to invite-only URLs pushes the URL toward QR-code limits (V20 QR holds ~858 alphanumeric chars at error-correction level L). Mitigations:

- Base64url encoding of the CBOR payload is already efficient (~33% overhead vs. binary).
- An invite-only URL is intended for direct sharing (link, paste) more often than QR. QR scenarios (in-person onboarding) can still use V25 or higher.
- If a future use case needs smaller URLs, we can split the bootstrap fields into a separate fetch URL appended as a query parameter — deferred until needed.

### Risk: bootstrap key rotation

If Alice rotates her identity (e.g., device migration), her identity_pub changes, so existing invite URLs with the old admin_identity_pub become unverifiable. Mitigations:

- Rotation-on-Alice's-side is already a separate reconciliation problem (ZEB-173 / ZEB-197 multi-device binding). For ZEB-260 scope, rotation is out of scope.
- Practically, the invite URL has a TTL via `expires_at`; users re-mint after rotation.

### Risk: Cases B+C linger

Cases B (open-community first-Join) and C (self-Re-Join after Leave) remain a UX gap. The existing ticket recommendation is to defer until a real production blocker. This spec does not change that. The censorship-defense correctness of the gate is unchanged.

## References

- **Spec:** [docs/specs/2026-05-07-zeb-262-phase-4-invite-only-kick-set-power-design.md](2026-05-07-zeb-262-phase-4-invite-only-kick-set-power-design.md) — Phase 4 base.
- **Plan:** [docs/plans/2026-05-07-zeb-262-phase-4-invite-only-kick-set-power-plan.md](../plans/2026-05-07-zeb-262-phase-4-invite-only-kick-set-power-plan.md) — Phase 4 implementation plan.
- **Code:** `src-tauri/src/community_invite.rs` — `CommunityInvitePayload`, `handle_unicast`, `verify_packet_pure`.
- **Code:** `src-tauri/src/community_state_sync.rs` — `handle_incoming_publish` (the gate), `insert_local_event_with_pubs`.
- **Code:** `src-tauri/src/community_membership.rs` — `SignedMembershipEvent`, `verify_signature`.
- **Code:** `src-tauri/src/lib.rs` — `redeem_invite_inner`.
- **Test:** `src-tauri/tests/community_invite_only_integration.rs:365-430` — the ZEB-260 OOB pre-seed this spec removes.
- **Linear:** [ZEB-260](https://linear.app/zeblith/issue/ZEB-260), [ZEB-256](https://linear.app/zeblith/issue/ZEB-256), [ZEB-262](https://linear.app/zeblith/issue/ZEB-262), [ZEB-258](https://linear.app/zeblith/issue/ZEB-258).
