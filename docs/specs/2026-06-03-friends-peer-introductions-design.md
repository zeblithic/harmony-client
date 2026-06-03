# Friends & Peer Introductions — Design

**Date:** 2026-06-03
**Parent:** ZEB-321 (cross-WAN peer discovery, reconnection, NAT traversal — cohesive story)
**Status:** Approved design; pending Linear ticket(s) + implementation plan.
**Related shipped work:** ZEB-322 (`harmony_pkarr` crate), ZEB-323 (pkarr cases A/B/C policies), ZEB-325 (Phase 2c iroh invite handshake), ZEB-339 (enrolled device-#2 signing), ZEB-367 (invite-only `generate_invite` / mint).

---

## 1. Problem & intent

Harmony's transport substrate (iroh end-to-end, pkarr discovery cases A/B/C, the `harmony/handshake/v1` iroh join handshake) can already *resolve a reachable address* across WAN. What it has **no model for** is a **social graph**: a persistent, mutual, consenting relationship between two Harmony users that is independent of any community.

This design adds that layer. A user "builds their own internet" by **peering with people they care about**. Peering bootstraps baseline connectivity: once you peer with a few friends, and they with theirs, your reachable set grows organically into a **connectivity island**, and islands merge into the global ecosystem as people intermingle. Crucially, a peered friend can act as a **directory + introducer**, letting you discover and form your *own* direct links to *their* peers **without going out-of-band**. Out-of-band invites are then reserved for the one case they're actually needed: **bridging two disjoint islands**.

This works with **zero communities**. It is a sibling to communities, not built on them.

### Goals

1. A first-class, mutual, consensual **Friend Graph**, replicated across the user's own devices.
2. Three consensual ways to form a friend link: **mutual-key**, **friend-token**, and **friend-vouched introduction**.
3. **Friend-scoped rendezvous** ("Case-D") so peered friends find each other and reconnect across WAN **without enabling global discoverability**.
4. **Introduce-me / peer-exchange (PEX)**: awareness of what a friend can refer you to, plus an active introduction broker, gated by a **per-user introduction policy**.
5. Designed cohesively so the introduction layer (Phase 2) requires **no rework** of the Phase-1 data model.

### Non-goals (this arc)

- General-purpose liveness/rebinding/reconnection for *all* peers (ZEB-321 Phase 3 — this arc only does the friend-scoped slice).
- Relay governance / civic-infrastructure relays (ZEB-321 Phase 5).
- EigenTrust-weighted relay selection (ZEB-46), ZipPIR private lookups (ZEB-47), billion-user Reticulum routing (ZEB-210).
- Reputation/trust *scoring* of friends. A friend link is binary (active / revoked); transitive trust weighting is explicitly out of scope.
- Unifying the DM contact surface with the Friend Graph. DMs may later ride on top of friends, but that is a separate follow-up.

---

## 2. Relationship to existing primitives (what we reuse vs. add)

| Concern | Existing primitive (reuse) | New in this arc |
|---|---|---|
| Replicated personal state | Owner-state CRDT + Zenoh sync (`owner_state_types.rs`, `OwnerDeviceCache` at `:439`, `impl` at `:1064`) | `FriendGraph` CRDT (same pattern) |
| Resolve a person by key | Case-B identity pkarr (`pkarr_identity_publisher.rs`, `connectivity_discover_identity` in `lib.rs`) | Case-D friend-scoped pkarr publisher |
| pkarr case derivation | `harmony_pkarr::{PkarrCase, derive_ephemeral_key, PkarrPublisher, PkarrRoutingRecord, RecordBuilder}` (ZEB-322) | `PkarrCase::Friend` (cross-repo, harmony-core) |
| Cross-WAN connect | iroh endpoint + `harmony/handshake/v1` ALPN (`iroh_endpoint.rs`, `iroh_invite_acceptor.rs`) | `harmony/friend/v1` ALPN for friend-link control messages |
| One-shot sealed token | Invite mint (`invite_mint.rs`: `mint_invite_token`, `seal_epoch_key`/`SealRecipient`) + Case-A publish (`pkarr_invite_publisher.rs`) | Friend-token: peer-scoped variant of the same machinery |
| Owner authentication | `community_membership::enrolled_key_from_cert` + `EnrollmentCert::verify()` (device-#2 + Master cert) | Same logic applied point-to-point (no `SignedMembershipEvent` wrapper) |
| Republish cadence | `reachability_publisher.rs` (startup / network-change / 60-min idle / force) | Case-D records added to the same cadence |

**Cross-repo dependency:** `PkarrCase::Friend` is a small additive change to the `harmony_pkarr` crate in harmony-core (mirrors how `Identity`/`Community`/`Invite` are defined). It ships as Phase 0 before the client work.

---

## 3. Identity & auth model

> **Correction (2026-06-03):** the original draft of this section keyed friends on the Reticulum/DM combined-pub identity (`X25519 ‖ Ed25519`) and rooted the pairwise secret in "owner X25519 ECDH." That conflated two distinct harmony identities and was wrong. Corrected below; the Phase-1 data model and the pairwise-secret plan changed accordingly.

A Harmony **owner** is identified by its master **`owner_id`** (16 bytes) = `PubKeyBundle::identity_hash()` = `SHA256(canonical_CBOR{ed25519_master_verify, ml_dsa?})[:16]` (`harmony-owner/pubkey_bundle.rs`). This is the SAME principal used by `self_owner`, community membership, profile cards (ZEB-341), and enrollment certs. **The Friend Graph keys friends on this master `owner_id`** — *not* the Reticulum/DM combined-pub `Identity::address_hash` (`SHA256(X25519‖Ed25519)[:16]`), which is a different identity used only for DM transport routing.

**Authentication = device-#2 signature + `EnrollmentCert` (the ZEB-339 model).** A node proves control of owner `O` by presenting `O`'s `EnrollmentCert` — a `Master`-issued cert binding `O`'s `owner_id` → an enrolled device-#2 Ed25519 key — and signing the handshake with that device-#2 key. A verifier:
1. runs `cert.verify()` (checks `master_pubkey.identity_hash() == cert.owner_id` and the master→device signature chain),
2. requires `cert.issuer == Master` (Quorum certs are rejected — the friend path can't fully verify them, mirroring `enrolled_key_from_cert`),
3. checks `cert.owner_id == claimed owner_id`, and
4. verifies the handshake signature against `cert.device_pubkeys.classical.ed25519_verify`.

This is exactly `community_membership::enrolled_key_from_cert`'s logic, applied point-to-point (no `SignedMembershipEvent` wrapper — extract the 4-step core). Runtime handles: the device-#2 key + this node's own cert live in `DmOutbox` (`community_signing_key`, `enrollment_cert`); `self_owner = OwnerAddr(loaded.state.owner_id)`.

> **Implementer rule:** the friend address IS the master `owner_id`. Enforce `addr == PubKeyBundle::classical_only(master_ed25519).identity_hash()` (v1; no PQ) in `apply_friend_update`. Sign/verify handshakes with the enrolled device-#2 key via the cert. Never key friends on, or authenticate via, the Reticulum combined-pub identity.

**Pairwise rendezvous secret — DEFERRED to Phase 1b, and OPEN.** Case-D was to be rooted in an "owner X25519 ECDH," but that does not hold under the master model: the master *and* device `PubKeyBundle` X25519 fields are currently a **zeroed stub** (`mint.rs` TODO v1.1). The only live per-owner X25519 is the **Reticulum identity** (`harmony_identity::PrivateIdentity::ecdh`), which is keyed on a *different* identity than `owner_id`. Phase 1b must reconcile "friends keyed on master `owner_id`" with "the only usable ECDH key is the Reticulum identity" (e.g. bind/store the friend's Reticulum identity alongside `owner_id`, or wait for the planned master/device X25519 HKDF derivation). **Phase 1 stores nothing that presumes a particular answer.**

---

## 4. Data model (Phase 1 — introduction-ready from day one)

All new types live in a new module `friend_graph.rs`, mirroring `owner_state_types.rs` conventions (strict custom serde with validation, LWW merge on an `Hlc`).

### 4.1 `FriendGraph` (owner-state CRDT)

```rust
/// Replicated across the user's own devices via owner-state Zenoh sync.
/// LWW-merged per entry on `learned_at`. Mirrors OwnerDeviceCache.
pub struct FriendGraph {
    pub friends: BTreeMap<OwnerAddr, FriendEntry>,
}

pub struct FriendEntry {
    /// The friend's master Ed25519 verify key. The map key (their `owner_id`)
    /// MUST equal `PubKeyBundle::classical_only(master_ed25519).identity_hash()`
    /// (v1; no PQ) — enforced in `apply_friend_update`. This anchors the friend
    /// to the SAME principal as their community/profile identity.
    pub master_ed25519: [u8; 32],
    /// Human label (their advertised display name at link time; refreshable). Length-capped.
    pub display: Option<String>,
    /// Lifecycle. Pending = request sent/received, not yet mutual.
    pub status: FriendStatus,           // Pending | Active | Revoked
    /// How this link was formed (provenance, for UX + audit).
    pub established_via: FriendOrigin,   // MutualKey | Token | Introduction
    /// Whether THIS friend may be surfaced in our referral catalog to others.
    /// (Sharer-side opt-in for the §6 awareness layer. Default false.)
    pub referrable: bool,
    /// LWW key.
    pub learned_at: Hlc,
}
```

Notes:
- **No reachability or pairwise-secret material is stored in Phase 1.** `cached_reachability` and any Reticulum-identity binding needed for the pairwise secret belong to Phase 1b (see §3 — the key model is unresolved). Phase 1 stores only the master identity anchor + metadata.
- The friend's identity is verified at the handshake (via their `EnrollmentCert`) before an entry is written; `master_ed25519` is the cert's `issuer.master_pubkey.classical.ed25519_verify`, and the map key is `cert.owner_id`.
- `status: Revoked` acts as a tombstone (LWW), so an unfriend on one device propagates and cannot be silently resurrected by a stale `Active` from another device unless its `learned_at` is strictly newer.
- The `referrable` and `established_via` fields exist in Phase 1 even though §6 ships in Phase 2 — this is the "introduction-ready data model" requirement that avoids a later CRDT migration.

### 4.2 `PeerIntroPolicy` (per-user setting)

```rust
/// Governs whether OTHERS may reach you via a friend's introduction.
/// Persisted alongside PkarrSettings (single-user setting, not per-friend).
pub enum PeerIntroPolicy {
    Open,             // accept any vouched introduction
    FriendsOfFriends, // accept iff the voucher is an Active friend of mine
    AskMe,            // surface a prompt; require explicit per-intro accept
    Closed,           // reject all introductions
}
// Default: FriendsOfFriends.
```

### 4.3 `ReferralCatalog` (Phase 2 wire type, transient — not CRDT state)

```rust
/// What a friend F serves to YOU over the live friend-link when you browse
/// "who can F introduce me to". Signed by F's device-#2 key.
pub struct ReferralCatalog {
    pub author: OwnerAddr,                 // F
    pub entries: Vec<ReferralEntry>,       // only F's `referrable` friends
    pub at: Hlc,
    pub sig: [u8; 64],                     // device-#2 over canonical bytes
}
pub struct ReferralEntry {
    pub peer_owner_pub: [u8; 64],
    pub display: Option<String>,
}
```

---

## 5. Peering handshake — three bootstrap paths

All paths are **consensual (request → accept)** and converge on the same outcome: a mutual `FriendEntry{status: Active}` on both sides, each keyed on the other's master `owner_id` and authenticated by the other's device-#2 + `EnrollmentCert`. There is **no unilateral add**. (A pairwise rendezvous secret is a Phase-1b concern — see §3.)

### 5.1 Transport

A new iroh ALPN `harmony/friend/v1` carries friend-link control messages (peer-request, accept, introduction, catalog-request/response). It is dispatched by the existing accept loop the same way `harmony/handshake/v1` is (`iroh_invite_acceptor.rs` is the structural template).

### 5.2 Path A — Mutual-key

Precondition: you already hold the friend's owner pubkey (out-of-band, prior community co-membership, address book, etc.).

1. Resolve the friend's current reachability. For an **already-active** friendship, resolve via **Case-D** (the target is publishing a Case-D record for this pairwise secret). For genuine **first contact** — no link yet, so the target is not yet publishing Case-D for you — fall back to **Case-B** if they enabled global discoverability, or use the Path-B token. (The pairwise secret is derivable from their pubkey alone, but no Case-D *record* exists until they've added you.) See §8 cold-start.
2. Open `harmony/friend/v1`, send `PeerRequest{ from_owner_pub, display, device2_cert, sig }`.
3. Friend's node verifies the device-#2 signature + enrollment cert, surfaces an accept UI (or auto per future setting), and on accept replies `PeerAccept{ from_owner_pub, device2_cert, sig }`.
4. Both sides write `FriendEntry{status: Active, established_via: MutualKey}` and begin Case-D publication for each other.

### 5.3 Path B — Friend-token (island bridging)

The out-of-band path, reusing the ZEB-367 mint machinery almost verbatim:

1. Inviter calls a peer-scoped `generate_friend_token` → `mint_invite_token` (device-#2 signed) + `seal_epoch_key` (untargeted/ephemeral, key rides the URL) where the sealed payload's intent is **"establish a PeerLink with owner O"** rather than "join community C". One-shot, published Case-A-style via the invite publisher, **unregister-on-consume**.
2. Redeemer opens the `harmony://friend/<token>` URL, resolves the inviter via the Case-A pkarr record, connects over `harmony/friend/v1`, and completes the §5.2 accept exchange.
3. Outcome: mutual `FriendEntry{established_via: Token}`.

### 5.4 Path C — Friend-vouched introduction (the PEX path)

This is §6. Architecturally it is **just a third way to reach the handshake**: a successful introduction terminates in the same `PeerAccept` exchange, yielding `established_via: Introduction`. There is deliberately **no separate "introduced-peer" type** — it's an ordinary friend link with different provenance.

---

## 6. Introduce-me / PEX (Phase 2), gated by `PeerIntroPolicy`

### 6.1 Awareness (passive)

Over an existing live `harmony/friend/v1` link to friend F, you may send `CatalogRequest`. F returns a `ReferralCatalog` (§4.3) containing only the friends F has marked `referrable`. This lets you **browse** whom F could introduce you to. Nothing connects; no reachability is exposed yet — just `{pubkey, display}`.

### 6.2 Introduction (active broker)

1. You → F (over your live link): `IntroduceRequest{ subject: me, target: X_owner_pub }`.
2. F validates X is an Active, `referrable` friend of F, then F → X (over F's live link to X): `Introduction{ voucher: F, subject: you (your owner_pub + device2_cert), sig_F }`.
3. **X enforces its own `PeerIntroPolicy`:**
   - `Open` → accept.
   - `FriendsOfFriends` → accept iff F is an Active friend of X (F's vouch is the proof; X already holds F in its graph).
   - `AskMe` → surface a prompt to X; proceed only on explicit accept.
   - `Closed` → reject (F relays a benign "declined" back to you; no detail leaked).
4. On accept, X initiates the §5.2 `PeerRequest`/`PeerAccept` exchange **directly with you** (X now holds your owner_pub from the introduction), establishing a mutual `FriendEntry{established_via: Introduction}` and Case-D rendezvous between you and X. F drops out of the path.

**F never holds private material for you or X.** F only relays **signed** envelopes. A malicious F cannot forge a vouch it isn't entitled to make (X verifies F's signature *and* that F is X's friend), and cannot man-in-the-middle the resulting link (the §5.2 exchange is authenticated by your and X's own device-#2 keys, and the pairwise secret is owner-ECDH between you and X — F is not party to it).

---

## 7. Trust, consent & abuse posture

- **No unilateral adds.** Every link requires the target's accept (explicit, or by a policy the target set themselves).
- **Per-relationship compartmentalization.** Each pairwise secret is independent; compromise of one friendship's secret does not unmask any other, and does not reveal the owner private key.
- **Target is the authority** on being reached-through-friends (`PeerIntroPolicy` is enforced on the target's node, not the voucher's).
- **Signed referrals.** Catalogs and introductions are device-#2 signed; a relayer cannot fabricate a vouch.
- **No global-discoverability requirement.** Case-D means peering never forces the world-visible Case-B toggle on.
- **Revocation.** Unfriend sets `status: Revoked` (LWW tombstone), stops Case-D publication for that friend (mirrors invite unregister-on-consume), and drops the friend from any future `ReferralCatalog`.
- **Privacy of the catalog.** A friend is only ever surfaced to others if its `referrable` flag is set (sharer-side opt-in, default off) — analogous to the opt-in `ProfileMembershipBroadcast` (ZEB-281) pattern.

---

## 8. Reconnection & cold-start

- **Reconnection (Phase 1).** Each active friend's Case-D record is republished and re-resolved on the existing `reachability_publisher` triggers (startup, network-change via if-watch debounce, 60-min idle). When a friend's address changes, the next resolve picks up the new `ReachabilityAnnouncePayload` and `cached_reachability` is updated. This is the friend-scoped slice of ZEB-321 Phase 3; it does not attempt the general all-peers rebinding protocol.
- **Cold-start / both-offline.** Case-D is a published DHT record, so a friend can be resolved even while *you* were offline, as long as they have republished within the record TTL. Case-D resolution only works once the friendship is **Active** — the target publishes one Case-D record per active friend. For genuine first contact (a brand-new mutual-key attempt where the target is not yet publishing Case-D for you — the pairwise secret is derivable from their pubkey, but no record exists yet), Path A falls back to Case-B (requires the target's global discoverability) or to the Path-B token. This boundary is acceptable for the alpha; a gossip-mesh liveness layer (the rejected "Approach 3") can be added later purely as an optimization without changing this model.

---

## 9. Phasing

- **Phase 0 — harmony-core (prereq, small):** add `PkarrCase::Friend` to `harmony_pkarr`; unit-test its derivation is distinct from Identity/Community/Invite.
- **Phase 1 — client foundation (this PR = ZEB-370; harmony-client only; ships a complete feature):**
  - `friend_graph.rs`: `FriendGraph` / `FriendEntry` CRDT (keyed on master `owner_id`) + strict serde + LWW merge.
  - `harmony/friend/v1` ALPN + `FriendLinkRequest`/`FriendLinkAccepted` handshake authenticated by device-#2 + `EnrollmentCert` (point-to-point `enrolled_key_from_cert` core); **Path B (friend-token)** via the reused ZEB-367 mint + Case-A pkarr machinery.
  - Friends UX (list, generate/redeem friend-token, unfriend) + IPCs.
  - `PeerIntroPolicy` *type* stored (not enforced); `referrable`/`established_via` fields present — introduction-ready data model, no later migration.
- **Phase 1b — cross-WAN rendezvous (next PR; needs harmony-core `PkarrCase::Friend`):**
  - Resolve the master-`owner_id`↔Reticulum-identity pairwise-secret question (§3); `PkarrFriendPublisher` (Case-D) + resolver wired into the `reachability_publisher` cadence; friend-scoped reconnection.
  - **Path A (mutual-key)** first-contact.
- **Phase 2 — introductions:**
  - `ReferralCatalog` request/response over `harmony/friend/v1`; `referrable` flag UX.
  - `IntroduceRequest` / `Introduction` broker; `PeerIntroPolicy` setting + enforcement; Path C.
  - Introductions UX (browse a friend's referrables, request intro, policy settings).

Each phase is an independent PR (or small PR set) under the new ticket(s).

---

## 10. Testing strategy (TDD)

**Unit (Phase 1)**
- Friend identity: `owner_id == PubKeyBundle::classical_only(master_ed25519).identity_hash()`; `apply_friend_update` rejects an entry whose key doesn't match its `master_ed25519`.
- Handshake auth: a valid device-#2 sig + `Master` `EnrollmentCert` verifies; tampered sig, wrong `owner_id`, or non-`Master` issuer is rejected.
- `FriendGraph` LWW merge: newer `learned_at` wins; `Revoked` tombstone not resurrected by stale `Active`; strict serde round-trip + rejection of malformed/oversized entries (mirror the `OwnerDeviceCache` serde tests).
- (Phase 1b) Case-D key derivation distinct from cases A/B/C for the same inputs.
- Friend-token mint → redeem produces a mutual PeerLink; tampered token rejected (reuse ZEB-367 token-sig tests as template).
- `PeerIntroPolicy` truth table: Open/FoF/AskMe/Closed each accept/reject correctly; FoF accepts only when voucher is an Active friend of the target.
- Referral catalog only includes `referrable` friends; signature verifies; tampered catalog rejected.

**Integration** (reuse the iroh/zenoh harness; heed ZEB-347 load-flake guidance — generous timeouts, avoid port contention)
- Two-node mutual-key peer; then simulate a network change and confirm Case-D re-resolve restores reachability.
- Two-node friend-token redeem across the handshake.
- Three-node introduction: F introduces A→X under each `PeerIntroPolicy`, asserting the correct accept/reject and that A↔X end up mutually linked with `established_via: Introduction` and F not party to their pairwise secret.

---

## 11. Open questions (resolve during planning, not blockers)

1. **Case-D record direction.** Do both friends publish under one shared `HKDF(pairwise_secret, epoch)` handle (and disambiguate by signer), or under direction-specific handles `HKDF(pairwise_secret, epoch, target_owner_pub)`? Leaning direction-specific to avoid two writers on one key. (Decide in Phase-0/1 with the `harmony_pkarr` shape.)
2. **Auto-accept settings.** Phase 1 ships explicit accept only. A later per-user "auto-accept mutual-key requests from owners I already hold" toggle is plausible but deferred.
3. **DM ↔ friend convergence.** Whether the existing DM surface should consume the Friend Graph (a friend becomes "someone I can DM") is a deliberate follow-up, not in this arc.
4. **Token URL scheme.** `harmony://friend/<token>` vs. reusing `harmony://invite/...` with an intent discriminator. Leaning on a distinct scheme for UX clarity.

---

## 12. References

- ZEB-321 umbrella + `docs/specs/2026-05-22-zeb-321-cross-wan-connectivity-design.md`, `docs/specs/2026-05-23-zeb-321-phase2-discovery-bootstrap-design.md`.
- ZEB-367 mint: `docs/specs/2026-06-02-phase4-invite-only-generate-design.md`; code `src-tauri/src/invite_mint.rs`, `src-tauri/src/community_invite.rs`, `src-tauri/src/pkarr_invite_publisher.rs`.
- Case-B template: `src-tauri/src/pkarr_identity_publisher.rs`; `connectivity_discover_identity` in `src-tauri/src/lib.rs`.
- Owner-state CRDT pattern: `src-tauri/src/owner_state_types.rs` (`OwnerDeviceCache`), `owner_state_sync.rs`.
- Reachability: `src-tauri/src/reachability_publisher.rs`, `reachability_record.rs`, `reachability_resolver.rs`.
- iroh handshake template: `src-tauri/src/iroh_endpoint.rs`, `iroh_invite_acceptor.rs`.
- Owner-to-owner sealing: `src-tauri/src/dm_signing.rs`.
- Opt-in sharing precedent: ZEB-281 `ProfileMembershipBroadcast`.
- Test-flake guidance: ZEB-347.
