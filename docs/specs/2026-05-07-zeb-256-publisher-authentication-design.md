# ZEB-256 — Cryptographic Publisher Authentication for Community State-Root Publishes

> **Status:** Design approved 2026-05-07 — pending implementation plan.
> **Linear:** [ZEB-256](https://linear.app/zeblith/issue/ZEB-256/) (parent: [ZEB-217](https://linear.app/zeblith/issue/ZEB-217/))
> **Required before:** Phase 4 of ZEB-217 Sub-C (invite-only flows)
> **Out of scope:** ZEB-249 (TreeKEM key rotation on kick) — kicked members lose write capability via this work but retain read until ZEB-249 rotates the `MembershipKey`.

## 1. Problem

The shipped Phase 2 envelope (`CommunityRootPublishPayload` in `src-tauri/src/community_state_sync.rs`) carries a `publisher_device_id: String` (inside the embedded `Hlc.device_id`) whose only authentication is the AEAD outer envelope (ChaCha20-Poly1305 with the per-community `MembershipKey`).

The `CommunityRootHlcTracker` uses `device_id` as the per-publisher slot key. Any community member with the `MembershipKey` can:

1. Encrypt a wire payload claiming any other member's `device_id` in `at.device_id`
2. Set `at.wall_ms` arbitrarily high
3. Wrap and publish to the per-community Zenoh topic

The receiver's tracker accepts the publish (no signature gate), records `tracker[device_id] = spoofed_HLC`, and then **silently rejects the real publisher's future publishes** as "not strictly newer." This is a censorship attack.

### Why this matters before Phase 4

Phase 2 ships **open communities only**. Open communities have a homogeneous trust model: every member already has the `MembershipKey`, so member-on-member abuse is in-scope of the membership grant.

Phase 4 introduces **invite-only** communities and **kick** semantics. The threat model changes:

- A kicked member retains the `MembershipKey` until [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/) (TreeKEM-style rotation on kick) lands. That's an explicit Phase 4 non-goal.
- A kicked-but-not-yet-rotated member could spoof an admin's `device_id` and censor admin publishes (the "I was just kicked but I can still silently break the community" attack).
- Same gap exists for any insider abuse scenario: one member can censor another by squatting their HLC slot.

The ZEB-249 rotation gap means we cannot rely on "kicked members lose the key" as the mitigation. Cryptographic publisher authentication is the load-bearing fix.

## 2. Approach

Mirror the per-event authentication shape (`SignedMembershipEvent` already carries an Ed25519 signature over `EventPayload` bound to `actor`'s identity_pub via the `IdentityResolver` trait). Add a publisher signature to `CommunityRootPublishPayload`, plus a strict membership-at-publish-HLC gate at receive time.

Three load-bearing decisions made during brainstorm:

1. **Tracker key shape:** `BTreeMap<String, Hlc>` → `BTreeMap<(OwnerAddr, String), Hlc>`. Each member's address gets its own per-device namespace; cross-addr `device_id` collisions become impossible by construction.

2. **Identity_pub source:** Resolved via `IdentityResolver` (existing trait used for receive-side `verify_event` and Phase 3's `insert_local_event`). NOT carried inline in the wire envelope. Cold-cache rejection is a transient soft-fail (next publish after cache propagation succeeds), matching the existing `SignedMembershipEvent.actor` resolution model.

3. **Membership-at-publish-HLC gate:** Strict — publisher must have `MemberStatus::Joined` at `publish.at.wall_ms` per the materialized membership state. This is the actual defense against the kicked-member attack: even with a stale `MembershipKey`, post-kick publishes get rejected at the receive-side gate, and the tracker is NOT advanced.

## 3. Wire format

`CommunityRootPublishPayload` gains 2 fields. All field codes are 2 chars (preserves the same-length-keys invariant at this nesting level):

```rust
/// State-root publish wire envelope. After ZEB-256, every publish is
/// signed by the publisher's local Ed25519 device key. Receivers
/// verify the signature, the publisher's current membership status,
/// and the per-(addr, device) replay tracker before merging events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRootPublishPayload {
    /// Content-ID of the encrypted CommunityState blob in the
    /// shared ContentStore. Unchanged from Phase 2.
    #[serde(rename = "rc")]
    pub root_cid: ContentId,

    /// Owner address of the publishing member. Receivers use this
    /// to (a) resolve identity_pub via IdentityResolver, (b) check
    /// membership-at-publish-HLC, (c) namespace the replay tracker.
    /// 16 bytes.
    #[serde(rename = "pa")]
    pub publisher_addr: OwnerAddr,

    /// Publisher's HLC at publish time. Carries device_id; tracker
    /// slot key is (publisher_addr, at.device_id). Unchanged shape
    /// from Phase 2 — only the tracker's interpretation changed.
    #[serde(rename = "at")]
    pub at: Hlc,

    /// Ed25519 signature over canonical CBOR of
    /// `CommunityRootSignedPayload { root_cid, publisher_addr, at }`.
    /// 64 bytes.
    #[serde(
        rename = "ps",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub publisher_sig: [u8; 64],
}

/// The unsigned portion of a CommunityRootPublishPayload — the
/// canonical-CBOR bytes the publisher signs. Mirrors EventPayload vs
/// SignedMembershipEvent: keeping the signed sub-payload as a separate
/// type means the signed bytes are unambiguous (no place to put
/// "the actual sig went here" in the encoded form).
///
/// All 3 field keys are 2 chars to satisfy the same-length-keys
/// invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRootSignedPayload {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    #[serde(rename = "pa")]
    pub publisher_addr: OwnerAddr,
    #[serde(rename = "at")]
    pub at: Hlc,
}

impl CanonicalPayloadSealed for CommunityRootSignedPayload {}
impl CanonicalPayload for CommunityRootSignedPayload {}

/// Convert a signed payload into its full wire-form envelope.
impl CommunityRootSignedPayload {
    pub fn into_wire(self, publisher_sig: [u8; 64]) -> CommunityRootPublishPayload {
        CommunityRootPublishPayload {
            root_cid: self.root_cid,
            publisher_addr: self.publisher_addr,
            at: self.at,
            publisher_sig,
        }
    }
}

/// Convenience: extract the signed sub-payload from a full wire envelope.
/// Used by receive-side verify to reproduce the canonical CBOR bytes
/// the publisher signed.
impl From<&CommunityRootPublishPayload> for CommunityRootSignedPayload {
    fn from(w: &CommunityRootPublishPayload) -> Self {
        Self {
            root_cid: w.root_cid,
            publisher_addr: w.publisher_addr,
            at: w.at.clone(),
        }
    }
}
```

**Wire overhead:** +80 bytes per publish (16 publisher_addr + 64 sig). Negligible relative to the encrypted state-root blob already shipped in the AEAD envelope.

## 4. Tracker

`CommunityRootHlcTracker.per_device` changes shape:

```rust
// Before:
pub per_device: BTreeMap<String, Hlc>,
// After:
pub per_device: BTreeMap<(OwnerAddr, String), Hlc>,
```

`would_accept` and `record` accept the addr alongside the HLC:

```rust
impl CommunityRootHlcTracker {
    pub fn would_accept(&self, publisher_addr: &OwnerAddr, candidate: &Hlc) -> bool {
        let key = (*publisher_addr, candidate.device_id.clone());
        match self.per_device.get(&key) {
            None => true,
            Some(prev) => candidate.is_strictly_newer_than(prev),
        }
    }

    pub fn record(&mut self, publisher_addr: OwnerAddr, hlc: Hlc) {
        let key = (publisher_addr, hlc.device_id.clone());
        // Same advance-after-success idiom as before; caller verifies
        // would_accept passed before calling record.
        self.per_device.insert(key, hlc);
    }
}
```

Persisted format breaks. Phase 2 has no production deployments, so the loader can hard-reject the old shape and rely on persist self-heal (existing Phase 2 behavior) to repopulate the tracker from scratch on next sync. No migration path.

## 5. Verification flow (receive side)

`handle_incoming_publish` adds three new gates BEFORE advancing the tracker or merging events. Order matters — cheapest checks first, signature verification last:

```
1. Decrypt AEAD envelope                                    (existing, unchanged)
2. Decode CBOR payload                                      (existing, unchanged)
3. NEW: publisher_addr is in materialized membership state
        at publish.at.wall_ms with status == Joined
        → reject as PublisherNotJoined; tracker NOT advanced
4. NEW: Resolve publisher_addr → identity_pub via IdentityResolver
        → reject as UnknownPublisher; tracker NOT advanced
5. NEW: Verify publisher_sig over canonical_cbor(CommunityRootSignedPayload::from(&payload))
        → reject as PublisherSigInvalid; tracker NOT advanced
6. tracker.would_accept(&publisher_addr, &at)               (modified key shape)
        → reject as DuplicateOrReplay; tracker NOT advanced
7. Fetch & decrypt state blob                               (existing, unchanged)
8. Misrouted-blob check                                     (existing, unchanged)
9. Merge events into CRDT                                   (existing, unchanged)
10. tracker.record(publisher_addr, at)                      (modified signature)
```

**Subtle ordering point:** step 3 happens BEFORE signature verification. That's deliberate — a stale-membership rejection is informational ("this addr was kicked"), and we shouldn't pay sig-verify cost for a publish we'll reject anyway. The membership check is over our locally-trusted state, so there's no integrity risk in trusting it pre-sig.

**State materialization at HLC:** step 3 mirrors how `verify_event` calls `prior_state_at_event` to compute power levels at an event's HLC. Reuse the same `prior_state_at_event(publish.at)` helper, then look up `members[publisher_addr].status`.

**Self-publish loopback:** when the engine receives its own publish (round-trip through the encrypted state-root topic), the existing self-resolver short-circuit (PR #87 Round 2 fix in `OwnerDeviceCacheResolver::resolve`) returns `self.self_identity_pub` directly. No additional plumbing needed.

**Cold-cache transient rejection:** if a brand-new joiner publishes before the receiver has merged their bootstrap Join, step 3 rejects with `PublisherNotJoined`. Once the receiver merges the joiner's Join (via the per-community Zenoh topic or the bootstrap flow), subsequent publishes from that joiner succeed. Same eventual-consistency UX as the existing `OwnerDeviceCache` propagation for membership events.

## 6. Signing flow (publish side)

The current publish loop builds `CommunityRootPublishPayload { root_cid, at: now }` at `community_state_sync.rs:1011`. After the change:

```rust
// Build the signed sub-payload
let signed = CommunityRootSignedPayload {
    root_cid,
    publisher_addr: ctx.self_owner,
    at: now,
};
let sig_bytes = canonical_cbor_encode(&signed)
    .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;
let publisher_sig = ctx.signing_key.sign(&sig_bytes).to_bytes();

// Wrap into wire envelope
let payload = signed.into_wire(publisher_sig);

// Existing AEAD wrap + Zenoh publish unchanged below.
```

`CommunitySyncEngineConfig` gains two fields:

```rust
/// Owner address of the local member. Embedded in every publish so
/// receivers can verify the signature against the right identity_pub
/// (resolved via IdentityResolver, NOT carried inline).
pub self_owner: OwnerAddr,

/// Local Ed25519 signing key for state-root publish signing. Same
/// handle Phase 3's `insert_local_event` already uses for membership
/// event signing — sourced from the local PrivateIdentity at engine
/// spawn time.
pub signing_key: Arc<ed25519_dalek::SigningKey>,
```

`CommunityRegistryConfig` gains the same two fields so spawn-time engines inherit them. `start_node`'s engine-spawn site populates both from `NodeState.dm_self_owner` and `NodeState.dm_outbox.signing_key` (already snapshotted for Phase 3 IPC use).

`InternalCtx` (the per-engine task state) also gains `self_owner` and `signing_key` so the publish loop has access. Mirror Phase 3's pattern of cloning fields out of `cfg` into the engine struct AND into `InternalCtx`.

## 7. Error taxonomy

Three new variants on `CommunitySyncError`:

```rust
/// Publish was signed correctly but the publisher's membership state
/// at the publish HLC does NOT have status Joined. Either they were
/// kicked, banned, or never joined the community we're tracking.
/// Tracker NOT advanced — defends against the post-kick censorship
/// attack where a kicked-but-still-keyed member tries to squat HLC
/// slots until ZEB-249 (key rotation) lands.
#[error(
    "publisher {addr:?} not joined at publish HLC \
     (status: {status:?}, left_at: {left_at:?})"
)]
PublisherNotJoined {
    addr: OwnerAddr,
    status: MemberStatus,
    /// MemberState.left_at field — set on both Leave and Kick events
    /// (the underlying CRDT field is overloaded). For PublisherNotJoined
    /// triggered by a kick, this carries the kick HLC; for one triggered
    /// by a voluntary Leave-then-republish, this carries the Leave HLC.
    left_at: Option<Hlc>,
},

/// IdentityResolver returned None for `publisher_addr`. Cold cache
/// (the publisher's identity_pub hasn't propagated to our owner-state
/// cache yet) or the addr was never a member. Transient when caused
/// by cold cache; persistent when caused by a wholly-fabricated addr
/// — both surface the same way at this layer. Tracker NOT advanced;
/// next publish after cache propagation succeeds.
#[error(
    "publisher {addr:?} identity not in resolver — \
     cache cold or addr not yet propagated"
)]
UnknownPublisher { addr: OwnerAddr },

/// Ed25519 signature over canonical_cbor(CommunityRootSignedPayload)
/// did not validate against the resolved identity_pub. This is the
/// load-bearing defense against the spoofing attack: a malicious
/// member with the MembershipKey cannot forge a publish claiming
/// another member's publisher_addr because they don't have that
/// member's signing key. Tracker NOT advanced.
#[error("publisher signature invalid for addr {addr:?}")]
PublisherSigInvalid { addr: OwnerAddr },
```

Each variant maps to a distinct `reason_tag` string for the `community-state-sync-degraded` IPC event:

| Variant | reason_tag |
|---|---|
| `PublisherNotJoined` | `"publisher_not_joined"` |
| `UnknownPublisher` | `"publisher_unknown"` |
| `PublisherSigInvalid` | `"publisher_sig_invalid"` |

## 8. Test surface

### Unit tests (`community_sync_engine_unit.rs`)

1. **`publish_carries_valid_publisher_sig`** — Engine publishes; verify the wire envelope's `publisher_sig` validates against `self_owner`'s identity_pub. Positive case for the signing path.

2. **`spoofed_publisher_addr_rejected_with_PublisherSigInvalid`** — Construct a `CommunityRootPublishPayload` where `publisher_addr` is Alice but `publisher_sig` is signed by Bob's key. Call `handle_incoming_publish`. Expect `PublisherSigInvalid`. Tracker entry for `(alice, *)` NOT advanced.

3. **`kicked_member_publish_rejected_with_PublisherNotJoined`** — Materialize a community state where Alice is kicked at HLC 100. Construct a publish from Alice at HLC 150 with a valid sig from her real identity_pub. Call `handle_incoming_publish`. Expect `PublisherNotJoined { status: MemberStatus::Banned, left_at: Some(hlc 100) }`. Tracker entry for `(alice, *)` NOT advanced. Even though the sig validates, the membership gate keeps Alice locked out.

4. **`cold_cache_publish_rejected_with_UnknownPublisher_then_succeeds_after_propagation`** — Engine receives a publish from a publisher whose `identity_pub` isn't yet in the resolver. Expect `UnknownPublisher` and tracker not advanced. Insert the identity_pub into the resolver. Re-deliver the same publish. Expect success and tracker advance.

### Integration test (`community_sync_integration.rs` — extend existing)

Two-engine bridge already covers happy-path convergence. Extend with a "Bob impersonates Alice" turn:

5. **`spoofed_publish_does_not_block_real_publisher`** — Engine A publishes legitimately at HLC 100 (tracker[(alice, alice-dev)] = 100). Inject a synthetic spoofed publish from Bob claiming `publisher_addr: alice` with HLC 200. Expect `PublisherSigInvalid` on receiver. Engine A then publishes legitimately at HLC 150. Expect success — tracker[(alice, alice-dev)] = 150 — proving the spoof did NOT advance Alice's slot.

### Wire-format fixture

`src-tauri/tests/wire_format_community_sync_fixtures.rs` regenerated to pin the new 4-field envelope shape. Existing pinned bytes are wholly invalidated; PR commit is the deliberate regeneration.

## 9. Breaking changes summary

| Area | Change | Migration |
|---|---|---|
| `CommunityRootPublishPayload` | +`publisher_addr` +`publisher_sig` | None — Phase 2 has no production deployments |
| `CommunityRootHlcTracker.per_device` | Key type `String` → `(OwnerAddr, String)` | Persisted format breaks; loader hard-rejects old shape; persist self-heal repopulates |
| `CommunitySyncEngineConfig` | +`self_owner` +`signing_key` | Internal API; all call sites updated in the same PR |
| `CommunityRegistryConfig` | +`self_owner` +`signing_key` | Same |
| `CommunitySyncError` | +3 variants | Internal API; degraded-event consumer matches on new reason_tags |
| `wire_format_community_sync_fixtures.rs` | Regenerated | Test artifact; PR commit is the regen |
| `community_state_persist.rs` | Loader rejects old tracker shape | Self-heal handles it |
| Test surface | +4 unit tests + 1 integration test extension | New |

## 10. Out of scope (explicitly deferred)

| Concern | Defer to | Rationale |
|---|---|---|
| `MembershipKey` rotation on kick | [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/) | Kicked members lose write capability via ZEB-256 (membership-at-HLC gate) but retain read until ZEB-249 rotates the symmetric key. ZEB-249 is the read-side fix; ZEB-256 is the write-side fix. They're complementary but independent. |
| Two-mode AEAD nonce discipline | Phase 4 of ZEB-217 Sub-C | Random-nonce (root publish) + deterministic-nonce (CAS blob) are already separated; ZEB-256 doesn't change either. |
| `DuplicateOrReplay` distinct error variant | Future hardening | Currently surfaces as `Duplicate` in the existing `IncomingOutcome` enum; renaming for clarity is a separate housekeeping concern. |
| Per-community signature algorithm choice (X25519 ECDH instead of Ed25519) | Future cryptographic agility | Ed25519 is the existing per-event signature scheme. ZEB-256 inherits it for consistency. |
| Multi-signature publishes (M-of-N admin sign-off on state-root publishes) | Future moderation hardening | Out of scope; ZEB-256 ships single-publisher signing, which is the per-event template. |

## 11. Acceptance criteria

- [ ] `CommunityRootPublishPayload` carries `publisher_addr` + `publisher_sig` per the wire-format spec
- [ ] `CommunityRootHlcTracker.per_device` keys on `(OwnerAddr, String)` and `would_accept` / `record` take the addr
- [ ] `handle_incoming_publish` runs the three new gates (membership-at-HLC, identity resolve, sig verify) BEFORE the existing replay-tracker check; tracker NOT advanced on any rejection
- [ ] `CommunitySyncEngineConfig` + `CommunityRegistryConfig` expose `self_owner` + `signing_key`; `start_node` populates both
- [ ] All 3 new `CommunitySyncError` variants exist with distinct `reason_tag` strings
- [ ] All 5 new tests pass (4 unit + 1 integration extension)
- [ ] Wire-format fixture regenerated and pinned
- [ ] All gates green: `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`
- [ ] Spoofing test (`spoofed_publish_does_not_block_real_publisher`) demonstrates the censorship attack is no longer possible

## 12. Phase 4 enablement

After ZEB-256 ships, Phase 4 of ZEB-217 Sub-C (invite-only flow + kick semantics) can proceed:

- Kicked members' state-root publishes get rejected at the receive-side membership gate
- The `MembershipKey` they retain lets them decrypt incoming publishes (read), but ZEB-256 prevents them from publishing forged updates (write)
- ZEB-249 will close the read gap by rotating the `MembershipKey` on kick
- Together, ZEB-256 + ZEB-249 fully enforce the invite-only / kick threat model
