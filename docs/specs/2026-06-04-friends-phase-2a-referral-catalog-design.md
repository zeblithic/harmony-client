# Friends Phase 2a — Referral Catalog (Awareness) — Design

**Date:** 2026-06-04
**Parent arc:** `docs/specs/2026-06-03-friends-peer-introductions-design.md` (§6.1 "Awareness", §4.3 `ReferralCatalog`, §9 "Phase 2").
**Builds on:** ZEB-370 (Phase 1, Friend Graph + token peering, PR #186) and ZEB-371 (Phase 1b, Case-D rendezvous + Path A, PR #189, merged squash `20be1cb`).
**Status:** Approved design (decisions captured below); tracked as ZEB-375; pending implementation plan.
**Ticket:** ZEB-375 (Phase 2a). Sibling: ZEB-376 (Phase 2b broker + `PeerIntroPolicy`, blocked-by ZEB-375).

---

## 1. Problem & intent

Phase 1/1b gave Harmony a mutual, consenting **Friend Graph** and a way for two friends to find each other across WAN (Case-D). What it does **not** yet give you is any way to learn **whom your friends could introduce you to**. The full Phase-2 arc adds an active introduction broker (Path C) and a per-user `PeerIntroPolicy`, but those carry real trust surface and depend on Case-D rendezvous that has **not yet been validated on real hardware**.

**Phase 2a is the smallest, lowest-risk slice of that arc: passive awareness.** Over a live link to a friend `F`, you can fetch a **signed catalog** of the friends `F` has explicitly marked shareable, and *browse* them — names and owner ids only. Nothing connects to those people. No reachability is exposed. No policy is enforced. It is read-only, opt-in on the sharer's side, and authenticated on both ends.

This slice (a) delivers the `referrable`-flag UX and the catalog plumbing the broker will reuse, and (b) gives a concrete, user-visible reason to run the Phase-1b fleet smoke test before the security-sensitive broker (2b) lands.

### Goals
1. A sharer-side **`referrable`** opt-in: mark which of your Active friends you are willing to surface to your other friends.
2. A **`ReferralCatalog`** wire type served over a dedicated friend-PEX sub-protocol, signed by the sharer's device-#2 key, containing only their `referrable` Active friends.
3. A **browse** path: resolve a friend via Case-D, fetch + cryptographically verify their catalog, surface `{display, owner_id, alreadyFriend}` in the Friends UI.

### Non-goals (this slice — deferred to 2b)
- The active introduction broker (`IntroduceRequest` / `Introduction`, Path C).
- `PeerIntroPolicy` persistence, IPC, and enforcement (the type already ships, unenforced).
- Any "request introduction" action. The browse view is read-only; the button that *acts* on a catalog entry is 2b.
- Reachability exposure of referred peers. A catalog reveals identity (`{owner_id, display}`) only — never how to reach the referred peer.

---

## 2. Relationship to existing primitives

| Concern | Reuse (shipped) | New in 2a |
|---|---|---|
| Friend identity + `referrable` flag | `friend_graph.rs::FriendEntry { referrable, established_via, status }` (already present, default `referrable=false`) | IPC + UI to toggle `referrable`; LWW bump on toggle |
| Point-to-point owner auth | `iroh_friend_acceptor::authenticate_friend_request` / `enrolled_key_from_cert` core (device-#2 + `EnrollmentCert`) | Same core applied to `CatalogRequest` + `ReferralCatalog` |
| Friend transport | `harmony/friend/v1` ALPN + `MultiplexHandshakeDispatcher` + accept-loop allowlist | **New `harmony/friend-pex/v1` ALPN** + a third dispatch target (handshake stream untouched) |
| Resolve a friend across WAN | Case-D resolve (`pkarr_friend_publisher` / Phase-1b reconnection) | Browse dials the resolved address on the PEX ALPN |
| Strict CBOR wire types | `friend_graph.rs` serde conventions (single-char keys, capped fields, bounded decode) | `referral_catalog.rs` mirrors them |
| Owner-state sync of a flag flip | `hlc_tracker` bump + `owner_sync_engine` debounced publish/persist (the friend-accept path already does this) | `set_friend_referrable` reuses it |

---

## 3. Transport — a dedicated PEX ALPN (handshake untouched)

**Decision (2026-06-04):** the catalog rides its **own ALPN `harmony/friend-pex/v1`**, *not* an envelope on the existing `harmony/friend/v1` stream.

Rationale: the `harmony/friend/v1` inbound stream decodes its body **directly** as a `FriendLinkRequest` (`iroh_friend_acceptor.rs:1021`) with no message-type discriminator. Adding a second message kind to that stream would require wrapping the body in an envelope enum, which changes the Phase-1b handshake's on-wire bytes and its pinned ZEB-370 CBOR fixtures — perturbing the exact handshake the fleet smoke test is about to validate. A separate ALPN isolates 2a completely: the handshake codecs and fixtures are byte-for-byte unchanged. The arc spec's "one ALPN carries all friend control messages" (§5.1) is a soft preference; its hard requirement — *the introduction layer must not rework Phase-1* — is better served by leaving the handshake alone.

Four registration edits (the ALPN namespace is centralized in `iroh_endpoint.rs::alpn`):
1. **Const:** add `HARMONY_FRIEND_PEX_V1: &[u8] = b"harmony/friend-pex/v1"` to `iroh_endpoint::alpn`.
2. **Advertise:** add it to the production endpoint's accepted-ALPN sets (`iroh_endpoint.rs` `.alpns(vec![…])`, both builder sites).
3. **Forward:** extend the accept-loop allowlist branch (`zenoh_iroh_transport.rs:330`) `|| alpn_used == alpn::HARMONY_FRIEND_PEX_V1` so PEX connections are handed to the multiplexer.
4. **Route:** add a `Pex` target to `route_handshake_alpn` / `FriendDispatchTarget` and a `catalog` arm to `MultiplexHandshakeDispatcher`, delegating to the new catalog acceptor.

Stream framing mirrors the handshake exactly: `[u32 LE len][CBOR body]`, `len ∈ (0, FRIEND_MAX_PACKET_LEN]`, strict trailing-byte rejection, per-step IO timeouts.

---

## 4. Data model (new module `referral_catalog.rs`)

All types use the `friend_graph.rs` serde discipline: explicit single-char `#[serde(rename)]`, `serde_bytes` bstr for fixed byte arrays, capped `display`, bounded decode, strict trailing-byte rejection.

```rust
/// One shareable friend, as served to a requester. Identity only — never
/// reachability. Keyed on the master `owner_id` (16 bytes), consistent with the
/// Friend Graph. NOTE: this CORRECTS arc-spec §4.3's `peer_owner_pub:[u8;64]`,
/// which predates the owner_id-keying correction; to *name* a target to F in 2b
/// you need only its owner_id (in 2b, X dials you — you never add X directly).
pub struct ReferralEntry {
    pub peer_owner: OwnerAddr,        // "o" — the referred friend's owner_id
    pub display: Option<String>,      // "n" — capped, optional
}

/// What friend F serves YOU over `harmony/friend-pex/v1` in answer to a
/// CatalogRequest. Self-contained: embeds F's EnrollmentCert so the requester
/// verifies the device-#2 signature without any cached cert (Phase 1 caches
/// none in FriendEntry).
pub struct ReferralCatalog {
    pub author: OwnerAddr,            // "a" — F's owner_id (MUST == enrollment.owner_id)
    pub entries: Vec<ReferralEntry>, // "e" — only F's Active + referrable friends; bounded count
    pub at: Hlc,                     // "t" — F's clock at serve time (freshness)
    pub enrollment: EnrollmentCert,  // "c" — F's owner→device-#2 binding (verification key carrier)
    pub sig: [u8; 64],               // "s" — F's device-#2 sig over the catalog preimage
}

/// Authenticated request to browse F's referral catalog. Signed so F serves
/// only its Active friends, and BOUND to F's owner_id so a captured request
/// can't be replayed to a different friend to fish their catalog.
pub struct CatalogRequest {
    pub from_addr: OwnerAddr,        // "a" — requester R's owner_id (MUST == enrollment.owner_id)
    pub to_addr: OwnerAddr,          // "d" — F's owner_id; F rejects if != self_owner (anti-replay)
    pub enrollment: EnrollmentCert,  // "c" — R's owner→device-#2 binding
    pub sig: [u8; 64],               // "s" — R's device-#2 sig over the request preimage
}
```

### Signing preimages (domain-separated, mirroring `friend_request_sig_preimage`)
- **`CatalogRequest`** — R's device-#2 signs `("hcr1", from_addr=R, to_addr=F)`. Binding `to_addr` prevents replaying R's signed request to a different friend G.
- **`ReferralCatalog`** — F's device-#2 signs `("hrc1", author=F, subject=R, entries, at)`. `subject=R` is the requesting owner from the connection's CatalogRequest (a preimage input, **not** a wire field); binding it stops a catalog made for R being shown to a different requester S. `enrollment` and `sig` are never part of the signed bytes.

Bounds: `entries.len()` capped (e.g. `MAX_REFERRAL_ENTRIES`, generous for alpha); the whole body is additionally `FRIEND_MAX_PACKET_LEN`-bounded by the codec. If a sharer's referrable set exceeds the cap, the served catalog is truncated and the serve path **logs the drop count** (no silent truncation).

---

## 5. Flows

### 5.1 Serve (F answers a CatalogRequest)
A new `IrohFriendPexAcceptor` (structural sibling of `IrohFriendHandshakeAcceptor`, sharing its `crdt_state`, `self_owner`, `self_enrollment`, `device2_signing_key`, `hlc_tracker`):
1. `accept_bi` → read `[len][body]` → `decode_catalog_request`.
2. **Authenticate always:** run the `authenticate_friend_request`-equivalent core on `(from_addr, enrollment, sig)` over the `"hcr1"` preimage. Failure → close, serve nothing.
3. **Anti-replay:** require `req.to_addr == self_owner`. Mismatch → close.
4. **Friend gate:** under the CRDT lock, snapshot whether `from_addr` is an **Active** friend; drop the guard. Not an Active friend → return an **empty** signed catalog (benign; reveals nothing, doesn't distinguish "no referrables" from "you're not my friend").
5. **Build:** collect Active friends with `referrable == true` → `ReferralEntry { peer_owner, display }`, bounded; stamp `at` from a fresh `hlc_tracker` read; sign the `"hrc1"` preimage with device-#2.
6. Write `[len][encode_referral_catalog]`.

Read-only: the serve path **never mutates** owner-state.

### 5.2 Browse (you fetch F's catalog)
New IPC `browse_friend_referrals(friend_owner_hex) -> Vec<ReferralView>`:
1. Look up `F` in the local `FriendGraph`; require `status == Active` (else error — you can only browse an established friend).
2. **Case-D resolve** F's current reachability (same resolver Phase-1b reconnection uses; the per-friendship `sealed_secret` yields the resolve key). Unresolvable → typed "friend unreachable" error.
3. Dial the resolved address on `HARMONY_FRIEND_PEX_V1`; send a signed `CatalogRequest { from_addr=self, to_addr=F, … }`.
4. Read the `ReferralCatalog`; **verify**: `enrolled_key_from_cert(catalog.enrollment)` → device-#2 key; `catalog.author == F == enrollment.owner_id == the friend we asked`; `sig` valid over the `"hrc1"` preimage with `subject=self`. Any failure → typed verification error (no partial trust).
5. Project to a DTO: `ReferralView { ownerIdHex, display, alreadyFriend }` where `alreadyFriend` = the peer is already in our `FriendGraph` as Active/Pending (UX hint; the future 2b "request intro" affordance suppresses already-friends).

### 5.3 Set referrable
New IPC `set_friend_referrable(friend_owner_hex, referrable: bool)`:
- Under the CRDT lock, mutate the `FriendEntry.referrable`, bump `learned_at` via the shared `hlc_tracker` (so the flip LWW-wins and reaches your other devices), then arm the existing debounced owner-state publish + persist. Unknown/non-Active friend → typed error. Idempotent.

---

## 6. Trust, consent & abuse posture
- **Opt-in, default off.** A friend is surfaced only if its `referrable` flag is set — analogous to ZEB-281 `ProfileMembershipBroadcast`.
- **Identity only, never reachability.** A catalog entry is `{owner_id, display}`. Learning of a peer does not let you reach them; that requires the 2b broker + the target's own accept.
- **Both ends authenticated.** R proves owner via cert+sig to be served; the catalog is device-#2 signed and self-verifying via the embedded cert.
- **Replay-bound.** `CatalogRequest` binds `to_addr` (can't be re-aimed at another friend); `ReferralCatalog` binds `subject` (can't be re-shown to another requester).
- **Fail-closed serving.** Non-Active requester → empty catalog. Auth failure or `to_addr` mismatch → connection closed, nothing served.
- **No new global discoverability.** Browse rides Case-D (already-Active friendships only); it never toggles Case-B world-visibility.
- **Bounded + logged.** Catalog entry count is capped; truncation is logged, not silent.

---

## 7. Testing strategy (TDD)

**Unit (`referral_catalog.rs` + acceptor core)**
- Catalog build includes only Active + `referrable` friends; excludes Pending, Revoked, and `referrable=false`.
- `ReferralCatalog` round-trips through strict CBOR; oversized `display`/entry-count/trailing bytes → hard decode error.
- Catalog signature verifies against the embedded cert's device-#2 key; tampered `entries`/`author`/`at` → rejected; cert whose `owner_id != author` → rejected; non-`Master` issuer cert → rejected.
- `CatalogRequest` auth: valid Active-friend request served; non-friend → empty catalog; bad sig → closed; `to_addr != self_owner` → closed.
- Catalog preimage binds `subject`: a catalog signed for R fails verification when checked with `subject=S`.
- `set_friend_referrable`: flips the flag, bumps `learned_at`, LWW-merges, survives owner-state serde round-trip; unknown friend → error; idempotent.

**Integration (reuse the iroh harness; heed ZEB-347 / ZEB-374 load-flake guidance — generous timeouts, serial, avoid port contention)**
- Two-node browse: A marks one friend `referrable`, B (an Active friend of A) calls `browse_friend_referrals(A)`, receives and verifies a catalog containing exactly that one entry; a non-referrable friend of A is absent.
- A non-friend C dialing A's PEX ALPN receives an empty catalog (no leak).

---

## 8. Open questions (resolve in planning, not blockers)
1. **Catalog acceptor sharing.** Whether `IrohFriendPexAcceptor` is a distinct struct or a method surface on the existing friend acceptor. Leaning distinct struct (own file/responsibility) sharing the same handles via the builder — keeps the handshake acceptor focused.
2. **`alreadyFriend` projection.** Confirm the DTO computes it under the same CRDT snapshot used elsewhere; purely a UX hint, no trust weight.
3. **2b boundary marker.** The browse view ships read-only. Decide whether to show a disabled/"in 2b" affordance on entries or omit it entirely (leaning omit).

---

## 9. References
- Arc design: `docs/specs/2026-06-03-friends-peer-introductions-design.md` (§4.3, §6.1, §7, §9).
- Phase 1b: `docs/specs/2026-06-04-zeb-371-friends-phase-1b-design.md`.
- Auth core: `src-tauri/src/iroh_friend_acceptor.rs` (`authenticate_friend_request`, `friend_request_sig_preimage`, `FriendLinkRequest`); `src-tauri/src/community_membership.rs` (`enrolled_key_from_cert`).
- Transport: `src-tauri/src/iroh_endpoint.rs` (`alpn`), `src-tauri/src/zenoh_iroh_transport.rs` (accept-loop allowlist), `MultiplexHandshakeDispatcher` (`iroh_friend_acceptor.rs`).
- Data model: `src-tauri/src/friend_graph.rs` (`FriendEntry`, `PeerIntroPolicy`, serde conventions).
- Case-D resolve: `src-tauri/src/pkarr_friend_publisher.rs`.
- Opt-in precedent: ZEB-281 `ProfileMembershipBroadcast`.
- Flake guidance: ZEB-347; ZEB-374 (friend two-endpoint test stabilization).
</content>
</invoke>
