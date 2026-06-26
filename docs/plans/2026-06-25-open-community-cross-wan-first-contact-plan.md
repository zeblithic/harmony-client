# Open-community cross-WAN first-contact — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a remote user holding an open/public community invite link make cross-WAN first contact and bootstrap-sync the community, without depending on any one specific person (admin or inviter) being online.

**Architecture:** Beacons (the existing capped relay-advertiser set) publish their iroh reachability under N enumerated DHT rendezvous slot keys derived from the community `epoch_key` alone. A link-only joiner derives the same slot keys, resolves a live beacon (escalating-batch), dials it on the existing `HARMONY_HANDSHAKE_V1` ALPN, and sends a new `OpenJoinRequest` carrying a capability proof (`epoch_auth`) plus its self-signed Join event. The beacon verifies capability + identity + freshness, ban-checks, rate-limits, then admits the open Join via the shipping `bootstrap_admit_open_publisher` gate and serves a membership snapshot. All discovery/derivation stays in `harmony-client` (no `harmony-pkarr` change): rendezvous slot keys reuse `PkarrCase::Community` (whose `ikm` contract *is* the epoch key) with a distinct, length-disjoint info-prefix.

**Tech Stack:** Rust (Tauri backend, `src-tauri`), Svelte 5 frontend; iroh (QUIC P2P), pkarr/Mainline DHT discovery, Zenoh sync, CRDT membership; `hkdf`/`hmac`/`sha2`/`ed25519-dalek`/`ciborium` (all already in the dependency graph).

## Global Constraints

- **ZEB IDs out of branch/commit/PR names.** Reference the ticket in the PR *body* as plain text only (no `Closes`/`Fixes` keyword — Linear auto-closes referenced issues on merge; mark Done manually and protect the parent epic).
- **Single repo: `harmony-client` only.** Do NOT modify `harmony-pkarr` / the `harmony` repo. Rendezvous slot keys reuse `PkarrCase::Community` via `derive_ephemeral_key` with a distinct info-prefix.
- **Branch:** `open-community-cross-wan-first-contact` (already created off `main`; spec at `docs/specs/2026-06-25-open-community-cross-wan-first-contact-design.md`, commits `8aab0c1a` + `71c0638b`).
- **Crypto reuse, no new primitives:** HKDF-SHA256 via `hkdf::Hkdf::<sha2::Sha256>`, HMAC-SHA256 via `hmac::Hmac<sha2::Sha256>` (precedent: `owner_state_crypto.rs:246`, `community_channel_log.rs:67`); ed25519 verification via `verify_strict` (precedent: `community_state_sync.rs:2964`).
- **N (slot count) = `COMMUNITY_RELAY_ADVERTISERS_MAX`** (`= 4`, `community_relay_announce.rs:24`). Never hard-code `4`; always reference the const.
- **Tunable knobs follow the `from_env()` + `HARMONY_*` convention** (precedent: `HandshakeDialConfig::from_env`, `lib.rs:42388`; `HandshakeAcceptorConfig::from_env`, `iroh_invite_acceptor.rs:70`). Clamp durations to `>= 1ms`.
- **Constant-time comparison** for any secret/MAC equality (`subtle::ConstantTimeEq` if already in graph, else a manual constant-time compare — never `==` on a MAC).
- **Gates (run from `src-tauri/`):**
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - Dev: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(<name>)'` (scope to the task's tests)
  - Final sweep: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  - Frontend (from repo root): `npx tsc --noEmit` and `npx vitest run`
- **Commit cadence:** one commit per task (TDD: failing test → impl → green → commit). Commit messages end with the standard Co-Authored-By/Claude-Session trailers; NO ZEB ID in the subject.

---

## File Structure

**New files:**
- `src-tauri/src/community_rendezvous.rs` — pure derivation + ranking + joiner resolve: `rendezvous_slot_key`, `RENDEZVOUS_SLOT_COUNT`, `slot_for_advertiser`, `RendezvousResolveConfig`, `resolve_rendezvous`.
- `src-tauri/src/open_join_auth.rs` — `mint_epoch_auth`, `verify_epoch_auth` (the `epoch_auth` capability proof).
- `src-tauri/src/open_join_admit.rs` — beacon-side `verify_and_admit_open_join` (testable helper: capability + identity + freshness + ban-check + rate-limit → `bootstrap_admit_open_publisher`), plus `OpenJoinRateLimiter`.
- `src-tauri/src/community_rendezvous_publisher.rs` — beacon slot-publish registration + power-aware self-promotion + observability counters.
- `src-tauri/src/open_join_dial.rs` — joiner-side `connectivity_open_join_iroh_inner` (resolve → dial → send `OpenJoinRequest` → apply snapshot).
- `src-tauri/tests/community_open_join_cross_wan_integration.rs` — two-node, no-LAN cross-WAN open-join round-trip (must FAIL on main).

**Modified files:**
- `src-tauri/src/community_invite.rs` — add `OpenJoinRequest` struct + `OpenJoinPacket` variant + discriminant `0x11` encode/decode + `build_signed_open_join_packet`.
- `src-tauri/src/iroh_invite_acceptor.rs` — branch the inbound dispatcher on discriminant (`0x10` → existing invite; `0x11` → open-join admit path) and write the open-join response.
- `src-tauri/src/lib.rs` — register the new modules (`mod ...;`); wire `connectivity_open_join_iroh_inner` into `redeem_invite_inner_with_overrides`'s open branch (`~26066`); thread the rendezvous publisher/self-promotion into boot wiring; extend `RedemptionOutcome`/`RedeemInviteResultDto` with a retryable cold-start status.
- `src-tauri/src/community_relay_publisher.rs` — invoke the rendezvous-slot publisher when this node is a relay advertiser (rank < N).
- `src/lib/...` (frontend) — map the retryable cold-start status to a "no one's reachable yet — we'll keep trying" banner in the join flow; vitest for the mapping.

---

## Reference signatures (verbatim, from code recon — consume these, do not re-derive)

```
// harmony_pkarr (git-rev dep; DO NOT MODIFY — call only):
harmony_pkarr::derive::PkarrCase                  // Invite | Identity | Community | Friend; Community.salt() = b"harmony.pkarr.v1.community"
harmony_pkarr::derive::derive_ephemeral_key(case: PkarrCase, ikm: &[u8], info: &[u8]) -> ed25519_dalek::SigningKey
harmony_pkarr::epoch::current_epoch_id(now_ms: u64) -> u64
harmony_pkarr::epoch::epoch_tolerance_window(now_ms: u64) -> [u64; 3]   // [e-1, e, e+1]
harmony_pkarr::PkarrResolver::resolve(&self, vk: &ed25519_dalek::VerifyingKey) -> Result<Option<PkarrRoutingRecord>, _>
PkarrRoutingRecord::sign_new(blob: Vec<u8>, id_pub: [u8;64], at_ms: u64, expires_ms: u64, id_sk: &SigningKey) -> Result<Self, _>
PkarrRoutingRecord::verify_freshness(&self, now_ms: u64) -> Result<(), _>
PkarrRoutingRecord.routing_blob: Vec<u8>

// harmony-client (modify/extend):
crate::owner_state_types::EpochKey::as_bytes(&self) -> &[u8; 32]
crate::owner_state_types::{SpaceId, OwnerAddr, Hlc, DeviceIdentityHash}   // SpaceId.0: [u8;16]; OwnerAddr.0: [u8;16]
crate::reachability_record::ReachabilityAnnouncePayload                    // {iroh_node_id:[u8;32], home_relay_url:String, direct_addresses:Vec<SocketAddr>, announced_at_ms:u64, identity_signature:[u8;64], butler_set, bs_at}
crate::reachability_record::REACHABILITY_RECORD_TTL_MS
crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX           // = 4
crate::community_relay_resolver::CommunityRelayResolver::relays_for_community(&self, community_id: &SpaceId, now_ms: u64) -> Vec<CommunityRelayEntry>
crate::community_membership::bootstrap_admit_open_publisher(incoming_events: &[SignedMembershipEvent], publisher_addr: OwnerAddr, admin_addr: OwnerAddr, expected_community_id: SpaceId, publisher_at: &Hlc) -> Option<MemberState>
crate::community_membership::prior_state_at_hlc(all_events: &[SignedMembershipEvent], target_hlc: &Hlc, admin_addr: OwnerAddr) -> MaterializedMembership
crate::community_membership::enrolled_key_from_cert(event: &SignedMembershipEvent) -> Result<EnrolledDeviceKey, VerifyError>   // EnrolledDeviceKey{owner:OwnerAddr, device_ed25519:[u8;32]}
crate::community_membership::{MemberStatus, MemberState, MaterializedMembership, SignedMembershipEvent}   // MemberStatus::Banned is the tombstone
crate::community_invite::{CommunityInviteSigned, CommunityInvitePacket, encode_packet, decode_packet, build_signed_invite_packet, device_hash_from_identity_pub}
crate::community_invite::CommunityInvitePayload  // {community_id, admin_addr, is_invite_only:bool, invite_token:Option<InviteToken>, admin_identity_pub:Option<[u8;64]>, epoch_snapshot:InviteEpochSnapshot, inviter_enrollment, admin_bootstrap}
crate::iroh_endpoint::alpn::HARMONY_HANDSHAKE_V1   // b"harmony/handshake/v1"
crate::iroh_invite_acceptor::{IrohInviteHandshakeAcceptor, HANDSHAKE_MAX_PACKET_LEN, HandshakeAcceptError}
// joiner dial template (mirror exactly): lib.rs connectivity_redeem_invite_iroh_inner (~43131), HandshakeDialConfig (~42388)
// open-redeem branch to extend: lib.rs redeem_invite_inner_with_overrides open arm (~26066, `else if !payload.is_invite_only { engine_arc.insert_local_event(minted.bootstrap_join) ... }`)
// integration test to mirror: tests/pkarr_iroh_redeem_full_integration.rs :: bob_joins_alice_via_iroh_handshake_option_a (719), setup_two_party_iroh_handshake (283)
```

> **Implementer note:** these signatures are pinned from a recon pass. Before using one, open the cited file and confirm the exact field/parameter names against the live code (large files drift). If a signature differs, adapt the call and keep the plan's intent.

---

### Task 1: Rendezvous slot-key derivation

**Files:**
- Create: `src-tauri/src/community_rendezvous.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod community_rendezvous;` near the other `mod` declarations)

**Interfaces:**
- Consumes: `harmony_pkarr::derive::{PkarrCase, derive_ephemeral_key}`, `harmony_pkarr::epoch::current_epoch_id`, `crate::owner_state_types::EpochKey`, `crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX`.
- Produces:
  - `pub const RENDEZVOUS_SLOT_COUNT: usize = COMMUNITY_RELAY_ADVERTISERS_MAX;`
  - `pub const RENDEZVOUS_INFO_PREFIX: &[u8] = b"harmony.rendezvous.v1";`
  - `pub fn rendezvous_slot_key(epoch_key: &EpochKey, slot_index: u16, epoch_id: u64) -> ed25519_dalek::SigningKey`
  - `pub fn rendezvous_slot_verifying_key(epoch_key: &EpochKey, slot_index: u16, epoch_id: u64) -> ed25519_dalek::VerifyingKey` (convenience: `.verifying_key()`)

- [ ] **Step 1: Write the failing test**

Add to `src-tauri/src/community_rendezvous.rs` (create the file with this test + an empty module body to start):

```rust
//! Open-community cross-WAN first-contact: enumerated DHT rendezvous slots.
//!
//! A community-keyed rendezvous record lets a link-only joiner (holding the
//! community `epoch_key`) resolve a live serving "beacon" from the DHT. Slot
//! keys reuse `PkarrCase::Community` (its `ikm` contract IS the epoch key) with
//! a distinct, length-disjoint info-prefix so they can never collide with the
//! member-keyed records (`info = identity_pub(64) || epoch_id(8)`, 72 bytes).

use crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX;
use crate::owner_state_types::EpochKey;
use harmony_pkarr::derive::{derive_ephemeral_key, PkarrCase};

/// Number of enumerated rendezvous slots == the relay-advertiser cap.
pub const RENDEZVOUS_SLOT_COUNT: usize = COMMUNITY_RELAY_ADVERTISERS_MAX;

/// Domain-separation prefix for rendezvous slot derivation. Length-disjoint
/// from the 72-byte member-keyed `info`, so rendezvous keys can never alias a
/// member's reachability key even though both reuse the `Community` salt.
pub const RENDEZVOUS_INFO_PREFIX: &[u8] = b"harmony.rendezvous.v1";

/// `info = RENDEZVOUS_INFO_PREFIX || slot_index_be(2) || epoch_id_be(8)`.
fn rendezvous_info(slot_index: u16, epoch_id: u64) -> Vec<u8> {
    let mut info = Vec::with_capacity(RENDEZVOUS_INFO_PREFIX.len() + 2 + 8);
    info.extend_from_slice(RENDEZVOUS_INFO_PREFIX);
    info.extend_from_slice(&slot_index.to_be_bytes());
    info.extend_from_slice(&epoch_id.to_be_bytes());
    info
}

pub fn rendezvous_slot_key(
    epoch_key: &EpochKey,
    slot_index: u16,
    epoch_id: u64,
) -> ed25519_dalek::SigningKey {
    let info = rendezvous_info(slot_index, epoch_id);
    derive_ephemeral_key(PkarrCase::Community, epoch_key.as_bytes(), &info)
}

pub fn rendezvous_slot_verifying_key(
    epoch_key: &EpochKey,
    slot_index: u16,
    epoch_id: u64,
) -> ed25519_dalek::VerifyingKey {
    rendezvous_slot_key(epoch_key, slot_index, epoch_id).verifying_key()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ek() -> EpochKey {
        EpochKey::new([7u8; 32])
    }

    #[test]
    fn slot_key_is_deterministic() {
        let a = rendezvous_slot_verifying_key(&ek(), 0, 42);
        let b = rendezvous_slot_verifying_key(&ek(), 0, 42);
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn distinct_slots_and_epochs_give_distinct_keys() {
        let s0 = rendezvous_slot_verifying_key(&ek(), 0, 42).to_bytes();
        let s1 = rendezvous_slot_verifying_key(&ek(), 1, 42).to_bytes();
        let s0_next = rendezvous_slot_verifying_key(&ek(), 0, 43).to_bytes();
        assert_ne!(s0, s1, "different slot index must yield a different key");
        assert_ne!(s0, s0_next, "different epoch must yield a different key");
    }

    #[test]
    fn rendezvous_key_is_disjoint_from_member_keyed_record() {
        // Member-keyed info is identity_pub(64) || epoch_id(8) = 72 bytes under
        // the SAME Community salt. Reconstruct it and confirm no rendezvous slot
        // collides with it.
        let epoch_id = 42u64;
        let identity_pub = [9u8; 64];
        let mut member_info = Vec::with_capacity(72);
        member_info.extend_from_slice(&identity_pub);
        member_info.extend_from_slice(&epoch_id.to_be_bytes());
        let member_key =
            derive_ephemeral_key(PkarrCase::Community, ek().as_bytes(), &member_info)
                .verifying_key()
                .to_bytes();
        for slot in 0..RENDEZVOUS_SLOT_COUNT as u16 {
            let rk = rendezvous_slot_verifying_key(&ek(), slot, epoch_id).to_bytes();
            assert_ne!(rk, member_key, "slot {slot} aliased a member key");
        }
    }

    #[test]
    fn slot_count_tracks_advertiser_cap() {
        assert_eq!(RENDEZVOUS_SLOT_COUNT, COMMUNITY_RELAY_ADVERTISERS_MAX);
    }
}
```

- [ ] **Step 2: Run test to verify it fails (compile failure first)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_rendezvous)'`
Expected: FAIL — `mod community_rendezvous;` not yet declared → unresolved module / tests not found.

- [ ] **Step 3: Register the module**

In `src-tauri/src/lib.rs`, add alongside the other `mod` declarations (search for `mod community_relay_resolver;` and add after it):

```rust
mod community_rendezvous;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_rendezvous)'`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_rendezvous.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(open-join): rendezvous slot-key derivation

Enumerated DHT slot keys derived from epoch_key alone via PkarrCase::Community
with a length-disjoint "harmony.rendezvous.v1" info-prefix; proven disjoint
from the 72-byte member-keyed records. N = COMMUNITY_RELAY_ADVERTISERS_MAX.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 2: `epoch_auth` capability proof

**Files:**
- Create: `src-tauri/src/open_join_auth.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod open_join_auth;`)

**Interfaces:**
- Consumes: `hkdf::Hkdf<sha2::Sha256>`, `hmac::{Hmac, Mac}`, `crate::owner_state_types::{EpochKey, SpaceId}`.
- Produces:
  - `pub const EPOCH_AUTH_INFO: &[u8] = b"open-join-auth";`
  - `pub fn mint_epoch_auth(epoch_key: &EpochKey, community_id: &SpaceId, joiner_identity_pub: &[u8; 64], nonce: &[u8; 16], timestamp_ms: u64) -> [u8; 32]`
  - `pub fn verify_epoch_auth(epoch_key: &EpochKey, community_id: &SpaceId, joiner_identity_pub: &[u8; 64], nonce: &[u8; 16], timestamp_ms: u64, presented: &[u8; 32]) -> bool` (constant-time compare)

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/open_join_auth.rs`:

```rust
//! Capability proof for tokenless open-community join.
//!
//! The open invite link's `epoch_key` is the capability. A joiner proves it
//! holds the link by binding its identity + a fresh nonce/timestamp under a key
//! derived from `epoch_key`. A beacon (which also holds `epoch_key`) recomputes
//! and rejects on mismatch — so a party that merely learned a beacon's iroh
//! address cannot join without the link.
//!
//! `epoch_auth = HMAC( HKDF(epoch_key, "open-join-auth"),
//!                     community_id || joiner_identity_pub || nonce || timestamp_be )`

use crate::owner_state_types::{EpochKey, SpaceId};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const EPOCH_AUTH_INFO: &[u8] = b"open-join-auth";

type HmacSha256 = Hmac<Sha256>;

fn auth_preimage(
    community_id: &SpaceId,
    joiner_identity_pub: &[u8; 64],
    nonce: &[u8; 16],
    timestamp_ms: u64,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(16 + 64 + 16 + 8);
    msg.extend_from_slice(&community_id.0);
    msg.extend_from_slice(joiner_identity_pub);
    msg.extend_from_slice(nonce);
    msg.extend_from_slice(&timestamp_ms.to_be_bytes());
    msg
}

pub fn mint_epoch_auth(
    epoch_key: &EpochKey,
    community_id: &SpaceId,
    joiner_identity_pub: &[u8; 64],
    nonce: &[u8; 16],
    timestamp_ms: u64,
) -> [u8; 32] {
    // HKDF-Extract+Expand a per-purpose MAC key from the epoch key.
    let mut mac_key = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(&community_id.0), epoch_key.as_bytes())
        .expand(EPOCH_AUTH_INFO, mac_key.as_mut())
        .expect("32 <= 8160");
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(mac_key.as_ref()).expect("HMAC accepts any key length");
    mac.update(&auth_preimage(community_id, joiner_identity_pub, nonce, timestamp_ms));
    mac.finalize().into_bytes().into()
}

pub fn verify_epoch_auth(
    epoch_key: &EpochKey,
    community_id: &SpaceId,
    joiner_identity_pub: &[u8; 64],
    nonce: &[u8; 16],
    timestamp_ms: u64,
    presented: &[u8; 32],
) -> bool {
    let expected = mint_epoch_auth(epoch_key, community_id, joiner_identity_pub, nonce, timestamp_ms);
    // Constant-time compare (no early-exit on first mismatched byte).
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(presented.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ek() -> EpochKey {
        EpochKey::new([3u8; 32])
    }
    fn cid() -> SpaceId {
        SpaceId([1u8; 16])
    }

    #[test]
    fn valid_auth_round_trips() {
        let id = [5u8; 64];
        let nonce = [9u8; 16];
        let tag = mint_epoch_auth(&ek(), &cid(), &id, &nonce, 1000);
        assert!(verify_epoch_auth(&ek(), &cid(), &id, &nonce, 1000, &tag));
    }

    #[test]
    fn wrong_epoch_key_is_rejected() {
        let id = [5u8; 64];
        let nonce = [9u8; 16];
        let tag = mint_epoch_auth(&ek(), &cid(), &id, &nonce, 1000);
        let wrong = EpochKey::new([4u8; 32]);
        assert!(!verify_epoch_auth(&wrong, &cid(), &id, &nonce, 1000, &tag));
    }

    #[test]
    fn tampered_fields_are_rejected() {
        let id = [5u8; 64];
        let nonce = [9u8; 16];
        let tag = mint_epoch_auth(&ek(), &cid(), &id, &nonce, 1000);
        // Different timestamp, nonce, identity, or community each break it.
        assert!(!verify_epoch_auth(&ek(), &cid(), &id, &nonce, 1001, &tag));
        assert!(!verify_epoch_auth(&ek(), &cid(), &id, &[8u8; 16], 1000, &tag));
        assert!(!verify_epoch_auth(&ek(), &cid(), &[6u8; 64], &nonce, 1000, &tag));
        assert!(!verify_epoch_auth(&ek(), &SpaceId([2u8; 16]), &id, &nonce, 1000, &tag));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join_auth)'`
Expected: FAIL — module not declared.

- [ ] **Step 3: Register the module**

In `src-tauri/src/lib.rs`, add near the other `mod` declarations:

```rust
mod open_join_auth;
```

> If `zeroize` or `subtle` is not already a direct dependency in `src-tauri/Cargo.toml`, prefer the manual constant-time loop above (no new dep). `zeroize` IS already used (`owner_state_types.rs`), so `zeroize::Zeroizing` is available.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join_auth)'`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/open_join_auth.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(open-join): epoch_auth capability proof (HMAC over HKDF(epoch_key))

mint/verify the link-capability MAC binding community_id + joiner identity +
nonce + timestamp; constant-time compare; rejects wrong epoch_key and any
tampered field.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 3: Slot-claim ranking

**Files:**
- Modify: `src-tauri/src/community_rendezvous.rs` (add ranking fn + tests)

**Interfaces:**
- Consumes: `crate::owner_state_types::OwnerAddr`, `RENDEZVOUS_SLOT_COUNT`.
- Produces: `pub fn slot_for_advertiser(advertisers: &[OwnerAddr], me: &OwnerAddr) -> Option<u16>` — deterministic: sort advertisers ascending by their 16-byte address, the position of `me` is its slot index; returns `None` if `me` is not in the set or its rank `>= RENDEZVOUS_SLOT_COUNT`.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `src-tauri/src/community_rendezvous.rs`:

```rust
    use crate::owner_state_types::OwnerAddr;

    fn addr(b: u8) -> OwnerAddr {
        OwnerAddr([b; 16])
    }

    #[test]
    fn slot_assignment_is_deterministic_across_members() {
        // Two members compute the SAME ordering from the same (unordered) set.
        let set_a = vec![addr(3), addr(1), addr(2)];
        let set_b = vec![addr(2), addr(3), addr(1)];
        for who in [addr(1), addr(2), addr(3)] {
            assert_eq!(
                slot_for_advertiser(&set_a, &who),
                slot_for_advertiser(&set_b, &who),
                "ordering disagreed for {who:?}"
            );
        }
        // Sorted ascending: addr(1)->0, addr(2)->1, addr(3)->2.
        assert_eq!(slot_for_advertiser(&set_a, &addr(1)), Some(0));
        assert_eq!(slot_for_advertiser(&set_a, &addr(2)), Some(1));
        assert_eq!(slot_for_advertiser(&set_a, &addr(3)), Some(2));
    }

    #[test]
    fn not_in_set_returns_none() {
        let set = vec![addr(1), addr(2)];
        assert_eq!(slot_for_advertiser(&set, &addr(9)), None);
    }

    #[test]
    fn rank_beyond_cap_returns_none() {
        // RENDEZVOUS_SLOT_COUNT (=4) advertisers fill slots 0..3; a 5th (highest
        // address) ranks 4 >= cap and claims no slot.
        let set: Vec<OwnerAddr> = (1..=(RENDEZVOUS_SLOT_COUNT as u8 + 1)).map(addr).collect();
        let highest = addr(RENDEZVOUS_SLOT_COUNT as u8 + 1);
        assert_eq!(slot_for_advertiser(&set, &highest), None);
        // The one just under the cap still gets the last valid slot.
        let last_valid = addr(RENDEZVOUS_SLOT_COUNT as u8);
        assert_eq!(
            slot_for_advertiser(&set, &last_valid),
            Some((RENDEZVOUS_SLOT_COUNT - 1) as u16)
        );
    }

    #[test]
    fn duplicate_addresses_do_not_shift_ranks() {
        // Defensive: a duplicated advertiser must not change anyone's slot.
        let set = vec![addr(1), addr(2), addr(2), addr(3)];
        assert_eq!(slot_for_advertiser(&set, &addr(1)), Some(0));
        assert_eq!(slot_for_advertiser(&set, &addr(2)), Some(1));
        assert_eq!(slot_for_advertiser(&set, &addr(3)), Some(2));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_rendezvous)'`
Expected: FAIL — `slot_for_advertiser` not defined.

- [ ] **Step 3: Implement**

Add to the top-level of `src-tauri/src/community_rendezvous.rs` (above the `tests` module):

```rust
use crate::owner_state_types::OwnerAddr;

/// Deterministic slot claim: sort the advertiser set ascending by address; the
/// position of `me` is its slot index. `None` if `me` is absent or ranks at/
/// beyond the slot cap. Because the advertiser set is CRDT-replicated, every
/// member computes the same ordering, so each slot has exactly one writer.
pub fn slot_for_advertiser(advertisers: &[OwnerAddr], me: &OwnerAddr) -> Option<u16> {
    let mut sorted: Vec<OwnerAddr> = advertisers.to_vec();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    sorted.dedup_by(|a, b| a.0 == b.0);
    let rank = sorted.iter().position(|a| a.0 == me.0)?;
    if rank >= RENDEZVOUS_SLOT_COUNT {
        return None;
    }
    Some(rank as u16)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_rendezvous)'`
Expected: PASS (8 tests total in the module now).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_rendezvous.rs
git commit -m "$(cat <<'EOF'
feat(open-join): deterministic rendezvous slot claim by advertiser rank

Sort the CRDT-replicated advertiser set ascending by address; position == slot
index, capped at RENDEZVOUS_SLOT_COUNT. Dedup-safe so every member computes the
same single-writer-per-slot assignment.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 4: `OpenJoinRequest` message + wire framing

**Files:**
- Modify: `src-tauri/src/community_invite.rs`

**Interfaces:**
- Consumes: existing framing helpers in `community_invite.rs` (`encode_packet`/`decode_packet` discriminant pattern at `~1465`/`~1495`, `CanonicalPayload` trait, `device_hash_from_identity_pub`), `SignedMembershipEvent`, `SpaceId`, `Hlc`, `DeviceIdentityHash`.
- Produces:
  - `pub struct OpenJoinRequest { community_id: SpaceId, join_event: SignedMembershipEvent, joiner_identity_pub: [u8;64], signing_device_hash: DeviceIdentityHash, epoch_auth: [u8;32], nonce: [u8;16], created_at: Hlc }` (serde, CBOR, `#[serde(rename)]` 2-char keys like its sibling).
  - A new packet discriminant `0x11` for the open-join variant.
  - `pub fn build_signed_open_join_packet(req: OpenJoinRequest, sign_key: &ed25519_dalek::SigningKey) -> Result<CommunityInvitePacket, ...>` (mirrors `build_signed_invite_packet`).
  - `decode_packet` returns the open-join variant for discriminant `0x11`.

> **Design note:** model `OpenJoinRequest` exactly like `CommunityInviteSigned` (a `join_event` carrying the joiner's self-signed Join + its enrollment cert inside `join_event.enrollment`), swapping `invite_token` → (`epoch_auth`, `nonce`). Reuse the existing length-prefix + discriminant + CBOR-body framing. Extend the `CommunityInvitePacket` enum with an `OpenJoin { req, signature, signed_bytes }` variant rather than inventing a second envelope.

- [ ] **Step 1: Write the failing test**

Add a test module section in `src-tauri/src/community_invite.rs` (near the existing packet round-trip tests — search for the `Invite` packet round-trip test and mirror it):

```rust
#[cfg(test)]
mod open_join_packet_tests {
    use super::*;

    // Build a minimal self-signed Join event for the joiner. Reuse whatever
    // test helper the existing invite tests use to mint a SignedMembershipEvent
    // (search this file's tests for `fn ...join...` / `mint_*` and reuse it).
    fn joiner_signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[11u8; 32])
    }

    #[test]
    fn open_join_packet_round_trips_and_verifies() {
        let sk = joiner_signing_key();
        let req = OpenJoinRequest {
            community_id: SpaceId([1u8; 16]),
            join_event: super::tests_support::sample_join_event(&sk), // see Step 3 note
            joiner_identity_pub: [4u8; 64],
            signing_device_hash: DeviceIdentityHash([7u8; 16]),
            epoch_auth: [9u8; 32],
            nonce: [2u8; 16],
            created_at: Hlc { wall_ms: 1000, logical: 0, device_id: "j".into() },
        };
        let packet = build_signed_open_join_packet(req.clone(), &sk).expect("build");
        let wire = encode_packet(&packet).expect("encode");
        // First byte after the discriminant peel must be the open-join tag.
        assert_eq!(wire[0], 0x11, "open-join discriminant");
        let decoded = decode_packet(&wire).expect("decode");
        match decoded {
            CommunityInvitePacket::OpenJoin { req: got, .. } => {
                assert_eq!(got.community_id, req.community_id);
                assert_eq!(got.joiner_identity_pub, req.joiner_identity_pub);
                assert_eq!(got.epoch_auth, req.epoch_auth);
                assert_eq!(got.nonce, req.nonce);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn invite_and_open_join_discriminants_are_distinct() {
        // A 0x10 (invite) packet must NOT decode as open-join and vice-versa.
        let sk = joiner_signing_key();
        let req = OpenJoinRequest {
            community_id: SpaceId([1u8; 16]),
            join_event: super::tests_support::sample_join_event(&sk),
            joiner_identity_pub: [4u8; 64],
            signing_device_hash: DeviceIdentityHash([7u8; 16]),
            epoch_auth: [9u8; 32],
            nonce: [2u8; 16],
            created_at: Hlc { wall_ms: 1000, logical: 0, device_id: "j".into() },
        };
        let wire = encode_packet(&build_signed_open_join_packet(req, &sk).unwrap()).unwrap();
        assert_eq!(wire[0], 0x11);
        assert_ne!(wire[0], 0x10);
    }
}
```

> **Step 1 note:** If `community_invite.rs` already has a test helper that mints a `SignedMembershipEvent`, use it directly instead of `tests_support::sample_join_event` and delete that reference. Do NOT add a new helper if one exists — DRY.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join_packet)'`
Expected: FAIL — `OpenJoinRequest` / `build_signed_open_join_packet` / `OpenJoin` variant undefined.

- [ ] **Step 3: Implement the struct, variant, encode/decode, builder**

In `src-tauri/src/community_invite.rs`:

1. Define the struct next to `CommunityInviteSigned` (`~270`):

```rust
/// Tokenless open-community join request, a sibling of `CommunityInviteSigned`
/// on the same `HARMONY_HANDSHAKE_V1` ALPN. Carries the joiner's self-signed
/// Join (with enrollment cert inside `join_event.enrollment`) plus the
/// link-capability proof (`epoch_auth` + `nonce`) instead of an invite token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenJoinRequest {
    #[serde(rename = "ci")]
    pub community_id: SpaceId,
    #[serde(rename = "je")]
    pub join_event: SignedMembershipEvent,
    #[serde(
        rename = "ip",
        serialize_with = "serialize_identity_pub_as_bstr",
        deserialize_with = "deserialize_identity_pub_from_bstr"
    )]
    pub joiner_identity_pub: [u8; 64],
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,
    #[serde(
        rename = "ea",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub epoch_auth: [u8; 32],
    #[serde(
        rename = "no",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub nonce: [u8; 16],
    #[serde(rename = "ca")]
    pub created_at: Hlc,
}

impl CanonicalPayloadSealed for OpenJoinRequest {}
impl CanonicalPayload for OpenJoinRequest {}
```

> Reuse the `serialize_bytes_as_bstr`/`deserialize_bytes_from_bstr` helpers already in this file (used by `CommunityInviteSigned.joiner_identity_pub`/reachability records). If the 16-byte `nonce` needs a length-checked deserializer, mirror whatever the file uses for other fixed byte arrays.

2. Extend the packet enum (`~593`):

```rust
pub enum CommunityInvitePacket {
    Invite {
        signed: CommunityInviteSigned,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
    },
    OpenJoin {
        req: OpenJoinRequest,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
    },
}
```

3. Add the builder (mirror `build_signed_invite_packet`, `~1450`):

```rust
pub fn build_signed_open_join_packet(
    req: OpenJoinRequest,
    sign_key: &ed25519_dalek::SigningKey,
) -> Result<CommunityInvitePacket, CommunityInviteEncodeError> {
    let signed_bytes = canonical_cbor_encode(&req)
        .map_err(|e| CommunityInviteEncodeError::Cbor(e.to_string()))?;
    let signature = sign_key.sign(&signed_bytes).to_bytes();
    Ok(CommunityInvitePacket::OpenJoin {
        req,
        signature,
        signed_bytes,
    })
}
```

> Use the exact CBOR encode + sign helpers `build_signed_invite_packet` uses (copy its body, swap the type). If the existing builder uses `CanonicalPayload::to_canonical_bytes()` or similar, use that.

4. Extend `encode_packet` (`~1464`) to handle the new variant:

```rust
match packet {
    CommunityInvitePacket::Invite { signed_bytes, signature, .. } => {
        let mut out = Vec::with_capacity(1 + signed_bytes.len() + 64);
        out.push(0x10);
        out.extend_from_slice(signed_bytes);
        out.extend_from_slice(signature);
        Ok(out)
    }
    CommunityInvitePacket::OpenJoin { signed_bytes, signature, .. } => {
        let mut out = Vec::with_capacity(1 + signed_bytes.len() + 64);
        out.push(0x11);
        out.extend_from_slice(signed_bytes);
        out.extend_from_slice(signature);
        Ok(out)
    }
}
```

5. Extend `decode_packet` (`~1495`) discriminant match:

```rust
match disc {
    0x10 => { /* existing Invite decode unchanged */ }
    0x11 => {
        // body = [CBOR OpenJoinRequest][64-byte signature]
        let (cbor, sig_bytes) = body
            .split_at_checked(body.len().checked_sub(64).ok_or(
                CommunityInviteDecodeError::Truncated,
            )?)
            .ok_or(CommunityInviteDecodeError::Truncated)?;
        let req: OpenJoinRequest = ciborium::from_reader(cbor)
            .map_err(|e| CommunityInviteDecodeError::Cbor(e.to_string()))?;
        let mut signature = [0u8; 64];
        signature.copy_from_slice(sig_bytes);
        Ok(CommunityInvitePacket::OpenJoin {
            req,
            signature,
            signed_bytes: cbor.to_vec(),
        })
    }
    other => Err(CommunityInviteDecodeError::UnknownDiscriminant(*other)),
}
```

> Match the existing decode's exact split/parse idiom (the recon shows discriminant peel then `signed_bytes` + `signature`). Reuse whatever error variants exist; add `Truncated` only if absent.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join_packet)'`
Expected: PASS (2 tests). Also re-run the existing invite packet tests to confirm no regression: `-E 'test(community_invite)'`.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_invite.rs
git commit -m "$(cat <<'EOF'
feat(open-join): OpenJoinRequest handshake message + 0x11 wire framing

Sibling of CommunityInviteSigned on HARMONY_HANDSHAKE_V1: self-signed Join +
joiner identity + epoch_auth/nonce capability (no invite token). New 0x11
packet discriminant; encode/decode round-trip + discriminant-distinct tests.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 5: Beacon-side verify + admit helper

**Files:**
- Create: `src-tauri/src/open_join_admit.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod open_join_admit;`)

**Interfaces:**
- Consumes: `crate::open_join_auth::verify_epoch_auth`, `crate::community_membership::{bootstrap_admit_open_publisher, prior_state_at_hlc, enrolled_key_from_cert, MemberStatus, SignedMembershipEvent}`, `crate::community_invite::OpenJoinRequest`, `crate::owner_state_types::{EpochKey, SpaceId, OwnerAddr, Hlc}`.
- Produces:
  - `pub enum OpenJoinReject { BadCapability, BadJoinerSig, BadEnrollment, Stale, Replay, Banned, RateLimited, NotAdmittable }`
  - `pub struct OpenJoinAdmitOk { pub joiner_addr: OwnerAddr, pub member_events_snapshot: Vec<SignedMembershipEvent> }`
  - `pub struct OpenJoinRateLimiter { ... }` with `pub fn check(&mut self, source: OwnerAddr, now_ms: u64) -> bool` (true = allowed) and a per-window cap const + nonce-replay cache.
  - `pub fn verify_and_admit_open_join(req: &OpenJoinRequest, packet_sig: &[u8;64], epoch_key: &EpochKey, community_id: SpaceId, admin_addr: OwnerAddr, current_events: &[SignedMembershipEvent], now_ms: u64, freshness_window_ms: u64, limiter: &mut OpenJoinRateLimiter) -> Result<OpenJoinAdmitOk, OpenJoinReject>`

  The helper is pure/synchronous (no I/O, no engine lock) so it is fully unit-testable; the caller (Task 6) supplies `current_events` (the beacon engine's event log) and applies the admitted Join to the engine.

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/open_join_admit.rs` with the helper signatures (stubbed to fail) and these tests:

```rust
//! Beacon-side verification + admission for tokenless open-community join.
//! Pure/synchronous so it unit-tests without an engine or network. The caller
//! applies `OpenJoinAdmitOk.member_events_snapshot`'s new Join to its engine.

#[cfg(test)]
mod tests {
    use super::*;
    // Reuse the membership test helpers (mint admin self-Join + a joiner Join
    // with a valid enrollment cert). Search community_membership.rs tests for
    // the existing `mint_join_event` / `admin_bootstrap` helpers and import them
    // via `#[cfg(test)]`-exposed fns or rebuild minimal equivalents here.

    #[test]
    fn admits_a_valid_open_join() {
        let f = Fixture::new();
        let req = f.valid_request();
        let mut lim = OpenJoinRateLimiter::default();
        let ok = verify_and_admit_open_join(
            &req, &f.packet_sig, &f.epoch_key, f.community_id, f.admin_addr,
            &f.current_events, f.now_ms, 60_000, &mut lim,
        )
        .expect("valid open join should be admitted");
        assert_eq!(ok.joiner_addr, f.joiner_addr);
    }

    #[test]
    fn rejects_wrong_capability() {
        let f = Fixture::new();
        let mut req = f.valid_request();
        req.epoch_auth = [0u8; 32]; // not a valid MAC
        let mut lim = OpenJoinRateLimiter::default();
        assert!(matches!(
            verify_and_admit_open_join(&req, &f.packet_sig, &f.epoch_key, f.community_id,
                f.admin_addr, &f.current_events, f.now_ms, 60_000, &mut lim),
            Err(OpenJoinReject::BadCapability)
        ));
    }

    #[test]
    fn rejects_banned_identity() {
        let f = Fixture::with_banned_joiner();
        let req = f.valid_request();
        let mut lim = OpenJoinRateLimiter::default();
        assert!(matches!(
            verify_and_admit_open_join(&req, &f.packet_sig, &f.epoch_key, f.community_id,
                f.admin_addr, &f.current_events, f.now_ms, 60_000, &mut lim),
            Err(OpenJoinReject::Banned)
        ));
    }

    #[test]
    fn rejects_stale_timestamp() {
        let f = Fixture::new();
        let req = f.valid_request();
        let mut lim = OpenJoinRateLimiter::default();
        // now far beyond created_at + window.
        assert!(matches!(
            verify_and_admit_open_join(&req, &f.packet_sig, &f.epoch_key, f.community_id,
                f.admin_addr, &f.current_events, f.now_ms + 10_000_000, 60_000, &mut lim),
            Err(OpenJoinReject::Stale)
        ));
    }

    #[test]
    fn rejects_replayed_nonce() {
        let f = Fixture::new();
        let req = f.valid_request();
        let mut lim = OpenJoinRateLimiter::default();
        let _ = verify_and_admit_open_join(&req, &f.packet_sig, &f.epoch_key, f.community_id,
            f.admin_addr, &f.current_events, f.now_ms, 60_000, &mut lim).expect("first ok");
        assert!(matches!(
            verify_and_admit_open_join(&req, &f.packet_sig, &f.epoch_key, f.community_id,
                f.admin_addr, &f.current_events, f.now_ms, 60_000, &mut lim),
            Err(OpenJoinReject::Replay)
        ));
    }

    #[test]
    fn rate_limit_sheds_excess() {
        let f = Fixture::new();
        let mut lim = OpenJoinRateLimiter::default();
        let mut last = Ok(());
        for _ in 0..(OPEN_JOIN_RATE_LIMIT_PER_WINDOW + 1) {
            let req = f.fresh_request(); // unique nonce each time
            last = verify_and_admit_open_join(&req, &f.packet_sig, &f.epoch_key, f.community_id,
                f.admin_addr, &f.current_events, f.now_ms, 60_000, &mut lim)
                .map(|_| ());
        }
        assert!(matches!(last, Err(OpenJoinReject::RateLimited)));
    }
}
```

> **Fixture note:** Build `Fixture` using the membership crate's existing test mints (admin self-Join → `admin_addr`/`current_events`; a joiner Join signed by `joiner_sk` with a valid `EnrollmentCert`). The `packet_sig` is `joiner_sk.sign(canonical_cbor(&req))`. `epoch_auth` must be a *real* `mint_epoch_auth(...)` so `verify` passes. For `with_banned_joiner`, append an admin-issued Ban event for the joiner to `current_events`. Keep the fixture in the `tests` module.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join_admit)'`
Expected: FAIL — types/fn undefined.

- [ ] **Step 3: Implement the helper + limiter**

Add to `src-tauri/src/open_join_admit.rs` (above `tests`):

```rust
use crate::community_invite::OpenJoinRequest;
use crate::community_membership::{
    bootstrap_admit_open_publisher, enrolled_key_from_cert, prior_state_at_hlc, MemberStatus,
    SignedMembershipEvent,
};
use crate::open_join_auth::verify_epoch_auth;
use crate::owner_state_types::{EpochKey, Hlc, OwnerAddr, SpaceId};
use std::collections::{HashMap, HashSet};

pub const OPEN_JOIN_RATE_LIMIT_PER_WINDOW: usize = 20;
pub const OPEN_JOIN_RATE_LIMIT_WINDOW_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenJoinReject {
    BadCapability,
    BadJoinerSig,
    BadEnrollment,
    Stale,
    Replay,
    Banned,
    RateLimited,
    NotAdmittable,
}

pub struct OpenJoinAdmitOk {
    pub joiner_addr: OwnerAddr,
    pub member_events_snapshot: Vec<SignedMembershipEvent>,
}

/// Per-window admission limiter + nonce-replay cache. Window is coarse (count
/// per source per window); nonce cache prevents exact-replay within retention.
#[derive(Default)]
pub struct OpenJoinRateLimiter {
    window_start_ms: u64,
    count_in_window: usize,
    seen_nonces: HashSet<[u8; 16]>,
    nonce_seen_at: HashMap<[u8; 16], u64>,
}

impl OpenJoinRateLimiter {
    /// Returns true if a request is allowed; rolls the window over.
    fn allow(&mut self, now_ms: u64) -> bool {
        if now_ms.saturating_sub(self.window_start_ms) >= OPEN_JOIN_RATE_LIMIT_WINDOW_MS {
            self.window_start_ms = now_ms;
            self.count_in_window = 0;
        }
        if self.count_in_window >= OPEN_JOIN_RATE_LIMIT_PER_WINDOW {
            return false;
        }
        self.count_in_window += 1;
        true
    }

    fn is_replay(&mut self, nonce: &[u8; 16], now_ms: u64) -> bool {
        // Evict nonces older than the freshness horizon to bound memory.
        let horizon = now_ms.saturating_sub(OPEN_JOIN_RATE_LIMIT_WINDOW_MS.saturating_mul(4));
        self.nonce_seen_at.retain(|n, &mut t| {
            let keep = t >= horizon;
            if !keep {
                self.seen_nonces.remove(n);
            }
            keep
        });
        if self.seen_nonces.contains(nonce) {
            return true;
        }
        self.seen_nonces.insert(*nonce);
        self.nonce_seen_at.insert(*nonce, now_ms);
        false
    }
}

#[allow(clippy::too_many_arguments)]
pub fn verify_and_admit_open_join(
    req: &OpenJoinRequest,
    packet_sig: &[u8; 64],
    epoch_key: &EpochKey,
    community_id: SpaceId,
    admin_addr: OwnerAddr,
    current_events: &[SignedMembershipEvent],
    now_ms: u64,
    freshness_window_ms: u64,
    limiter: &mut OpenJoinRateLimiter,
) -> Result<OpenJoinAdmitOk, OpenJoinReject> {
    // 1. Community scope.
    if req.community_id != community_id {
        return Err(OpenJoinReject::NotAdmittable);
    }
    // 2. Freshness (bounded window; reject future-dated beyond small skew).
    let created = req.created_at.wall_ms;
    if now_ms.saturating_sub(created) > freshness_window_ms
        || created > now_ms.saturating_add(60_000)
    {
        return Err(OpenJoinReject::Stale);
    }
    // 3. Capability proof.
    if !verify_epoch_auth(
        epoch_key,
        &community_id,
        &req.joiner_identity_pub,
        &req.nonce,
        created,
        &req.epoch_auth,
    ) {
        return Err(OpenJoinReject::BadCapability);
    }
    // 4. Joiner identity control: the enrollment cert binds the join_event's
    //    device key to the joiner owner; verify it the same way the merge path
    //    does. (enrolled_key_from_cert checks cert.verify + Master issuer +
    //    owner match.)
    let enrolled =
        enrolled_key_from_cert(&req.join_event).map_err(|_| OpenJoinReject::BadEnrollment)?;
    // 5. Packet envelope signature: signed by the joiner's enrolled device key
    //    over the canonical CBOR of the request (mirror verify_publisher_sig's
    //    verify_strict posture).
    {
        use crate::owner_state_crypto::canonical_cbor_encode;
        let signed_bytes =
            canonical_cbor_encode(req).map_err(|_| OpenJoinReject::BadJoinerSig)?;
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&enrolled.device_ed25519)
            .map_err(|_| OpenJoinReject::BadJoinerSig)?;
        let sig = ed25519_dalek::Signature::from_bytes(packet_sig);
        vk.verify_strict(&signed_bytes, &sig)
            .map_err(|_| OpenJoinReject::BadJoinerSig)?;
    }
    // 6. Rate-limit + replay (after cheap structural checks, before admission).
    if limiter.is_replay(&req.nonce, now_ms) {
        return Err(OpenJoinReject::Replay);
    }
    if !limiter.allow(now_ms) {
        return Err(OpenJoinReject::RateLimited);
    }
    // 7. Ban-check against the materialized state at the joiner's Join HLC.
    let mat = prior_state_at_hlc(current_events, &req.join_event.at, admin_addr);
    if let Some(ms) = mat.members.get(&enrolled.owner) {
        if ms.status == MemberStatus::Banned {
            return Err(OpenJoinReject::Banned);
        }
    }
    // 8. Admit via the shipping open-admission gate: feed the request's own
    //    Join event alongside the current log; bootstrap_admit_open_publisher
    //    materializes the publisher's prefix and confirms they are Joined.
    let mut events_with_join = current_events.to_vec();
    events_with_join.push(req.join_event.clone());
    let admitted = bootstrap_admit_open_publisher(
        &events_with_join,
        enrolled.owner,
        admin_addr,
        community_id,
        &req.join_event.at,
    )
    .ok_or(OpenJoinReject::NotAdmittable)?;
    let _ = admitted; // MemberState confirms Joined; caller applies the event.

    Ok(OpenJoinAdmitOk {
        joiner_addr: enrolled.owner,
        member_events_snapshot: events_with_join,
    })
}
```

> **Implementer checks:** confirm `req.created_at` is the field name (Task 4 used `created_at`); confirm `mat.members` keys by `OwnerAddr` (recon: `BTreeMap<OwnerAddr, MemberState>`); confirm `canonical_cbor_encode` is the same encoder Task 4's builder used so the verify preimage matches the mint preimage byte-for-byte. The packet signature must cover the SAME bytes the builder signed (`signed_bytes` from the packet); thread the packet's `signed_bytes` through rather than re-encoding if there's any encoder-drift risk — preferred: pass `signed_bytes: &[u8]` into this helper instead of re-encoding `req`. Adjust the signature accordingly and update the test.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join_admit)'`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/open_join_admit.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(open-join): beacon-side verify + admit helper

Pure verify_and_admit_open_join: capability (epoch_auth), joiner enrollment +
envelope sig (verify_strict), freshness window, nonce-replay + per-window rate
limit, ban-check via prior_state_at_hlc, then bootstrap_admit_open_publisher.
Unit-tested incl. banned/stale/replay/rate-limit rejections.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 6: Wire the beacon accept dispatcher + open-join response

**Files:**
- Modify: `src-tauri/src/iroh_invite_acceptor.rs`

**Interfaces:**
- Consumes: `crate::community_invite::{decode_packet, CommunityInvitePacket, encode_packet}`, `crate::open_join_admit::{verify_and_admit_open_join, OpenJoinRateLimiter, OpenJoinReject}`, the beacon's `community_registry`/engine handles already held by `IrohInviteHandshakeAcceptor`, the community `epoch_key` (via `engine.membership_key()`), and `admin_addr` (the acceptor already resolves `community_id` → state).
- Produces: the accept dispatcher now branches on the decoded packet: `Invite` → existing `handle_invite_handshake_inbound`; `OpenJoin` → new `handle_open_join_inbound` that verifies+admits, applies the joiner's Join to the engine, and writes a length-prefixed CBOR ack-snapshot response.

> **Reuse:** the inbound read path (`accept_bi` → `[u32 LE len][packet]` → `decode_packet`) is identical for both variants. Factor the read so both variants share it, then `match` on the decoded packet. The response framing mirrors the invite-only countersign write (`[u32 LE len][cbor]` then `finish()`).

- [ ] **Step 1: Write the failing test**

Open-join admission over a real loopback iroh connection is exercised end-to-end by the Task 12 integration test. For this task, add a focused unit test that the dispatcher routes a `0x11` packet to the open-join path and a `0x10` packet to the invite path. In `src-tauri/src/iroh_invite_acceptor.rs` tests module:

```rust
#[test]
fn dispatch_routes_open_join_discriminant() {
    // decode_packet on a 0x11 wire yields the OpenJoin variant; on 0x10 the
    // Invite variant. This guards the match arm the acceptor branches on.
    use crate::community_invite::{decode_packet, CommunityInvitePacket};
    let open_wire = crate::community_invite::tests_support::sample_open_join_wire();
    let inv_wire = crate::community_invite::tests_support::sample_invite_wire();
    assert!(matches!(
        decode_packet(&open_wire).unwrap(),
        CommunityInvitePacket::OpenJoin { .. }
    ));
    assert!(matches!(
        decode_packet(&inv_wire).unwrap(),
        CommunityInvitePacket::Invite { .. }
    ));
}
```

> If exposing `tests_support` wire-builders is undesirable, instead assert the match logic by constructing the two packets via the Task 4 builders directly in this test. Keep it minimal — the real proof is Task 12.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(dispatch_routes_open_join)'`
Expected: FAIL until the helper/wire-builders exist (they do after Task 4; this may pass at the decode level but the acceptor branch is added in Step 3).

- [ ] **Step 3: Implement the dispatch branch + response**

In `src-tauri/src/iroh_invite_acceptor.rs`:

1. In the inbound handler, after `decode_packet(&packet_bytes)` (`~330`), replace the `let CommunityInvitePacket::Invite { signed, .. } = packet;` irrefutable bind with a match:

```rust
match packet {
    CommunityInvitePacket::Invite { signed, .. } => {
        // ... existing invite-only flow (handle_unicast, poll countersign,
        //     write countersign response) unchanged ...
    }
    CommunityInvitePacket::OpenJoin { req, signature, signed_bytes } => {
        return self
            .handle_open_join_inbound(&conn, send, req, signature, signed_bytes)
            .await;
    }
}
```

> The existing code reads `send`/`recv` before decode; thread them into the new method (the open-join path needs `send` to write the response).

2. Add the method on `IrohInviteHandshakeAcceptor`:

```rust
async fn handle_open_join_inbound(
    &self,
    conn: &Connection,
    mut send: iroh::endpoint::SendStream,
    req: crate::community_invite::OpenJoinRequest,
    signature: [u8; 64],
    _signed_bytes: Vec<u8>,
) -> Result<EventId, HandshakeAcceptError> {
    let community_id = req.community_id;
    let state_arc = self
        .community_registry
        .state_for(&community_id)
        .await
        .ok_or(HandshakeAcceptError::CommunityNotFound { community_id })?;

    // Snapshot inputs under the engine lock, then verify+admit OUTSIDE the lock.
    let (epoch_key, admin_addr, current_events) = {
        let g = state_arc.lock().await;
        (
            g.membership_key(),
            g.admin_addr(),                       // confirm accessor name
            g.events.values().cloned().collect::<Vec<_>>(),
        )
    };
    let now_ms = crate::time_source::now_ms();   // use the crate's wall-clock helper
    let mut limiter = self.open_join_limiter.lock().await; // Arc<Mutex<OpenJoinRateLimiter>> on the acceptor
    let admit = crate::open_join_admit::verify_and_admit_open_join(
        &req,
        &signature,
        &epoch_key,
        community_id,
        admin_addr,
        &current_events,
        now_ms,
        crate::open_join_admit::OPEN_JOIN_RATE_LIMIT_WINDOW_MS,
        &mut limiter,
    );
    drop(limiter);

    let admit = match admit {
        Ok(ok) => ok,
        Err(reject) => {
            tracing::warn!(?reject, remote_id = ?conn.remote_id(), "open-join rejected");
            // Write a typed rejection response so the joiner can surface it.
            let resp = OpenJoinResponse::Rejected { reason: format!("{reject:?}") };
            write_len_prefixed_cbor(&mut send, &resp, self.config.io_deadline).await?;
            return Err(HandshakeAcceptError::OpenJoinRejected);
        }
    };

    // Apply the admitted Join to the engine so it propagates via Zenoh.
    {
        let engine = self.community_registry.engine_for(&community_id).await
            .ok_or(HandshakeAcceptError::CommunityNotFound { community_id })?;
        engine
            .insert_remote_event(req.join_event.clone())   // confirm the insert API
            .await
            .map_err(|e| HandshakeAcceptError::HandleUnicast(format!("{e:?}")))?;
    }

    // Respond with an admitted ack + the membership snapshot for fast converge.
    let resp = OpenJoinResponse::Admitted {
        member_events: admit.member_events_snapshot,
    };
    write_len_prefixed_cbor(&mut send, &resp, self.config.io_deadline).await?;
    Ok(req.join_event.id)
}
```

3. Define the response type (in `community_invite.rs` next to `OpenJoinRequest`, so joiner + beacon share it):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OpenJoinResponse {
    Admitted { member_events: Vec<SignedMembershipEvent> },
    Rejected { reason: String },
}
```

4. Add `open_join_limiter: tokio::sync::Mutex<crate::open_join_admit::OpenJoinRateLimiter>` to `IrohInviteHandshakeAcceptor` (default in its constructor), and a `HandshakeAcceptError::OpenJoinRejected` variant. Add a small `write_len_prefixed_cbor` helper (factor from the existing countersign write).

> **Implementer checks:** confirm the engine accessors (`admin_addr()`, `engine_for`, `insert_remote_event` vs `insert_local_event`) against `community_state_sync.rs`. The joiner's Join is a *remote* event from the beacon's perspective — use the remote-insert API that runs `verify_event` (open Join is self-authorizing). If only `insert_local_event` exists for self events, use the registry's normal inbound-apply path. Confirm `now_ms` helper name (recon shows `std::time::SystemTime` used inline in resolver — reuse the same idiom if no `time_source` module exists).

- [ ] **Step 4: Run tests + clippy**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join)'` and `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: PASS / no warnings.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/iroh_invite_acceptor.rs src-tauri/src/community_invite.rs
git commit -m "$(cat <<'EOF'
feat(open-join): beacon accept dispatch + admitted-snapshot response

Branch the HARMONY_HANDSHAKE_V1 inbound handler on packet discriminant; 0x11
open-join verifies+admits (out of the engine lock), applies the Join, and
writes an Admitted{member_events} / Rejected{reason} response. Per-acceptor
rate limiter.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 7: Beacon rendezvous-slot publish

**Files:**
- Create: `src-tauri/src/community_rendezvous_publisher.rs`
- Modify: `src-tauri/src/lib.rs` (`mod community_rendezvous_publisher;` + boot wiring), `src-tauri/src/community_relay_publisher.rs` (trigger slot publish when this node is an advertiser)

**Interfaces:**
- Consumes: `crate::community_rendezvous::{rendezvous_slot_key, slot_for_advertiser, RENDEZVOUS_SLOT_COUNT}`, the existing `PkarrPublisher::register(handle, key_builder, builder)` seam (recon: `pkarr_community_publisher.rs:46-61`), `crate::community_relay_resolver::CommunityRelayResolver::relays_for_community`, `harmony_pkarr::epoch::current_epoch_id`, `ReachabilityAnnouncePayload` routing-blob builder (reuse the member publisher's `routing_blob_builder`).
- Produces:
  - `pub struct CommunityRendezvousPublisher { ... }` with `pub async fn refresh_slot(&self, community_id: SpaceId, epoch_key: EpochKey, advertisers: Vec<OwnerAddr>, me: OwnerAddr)` — computes `slot_for_advertiser`; if `Some(slot)`, registers a pkarr publish handle keyed by `rendezvous_slot_key(epoch_key, slot, current_epoch_id(now))`; if `None`, unregisters any prior slot handle for this community.

- [ ] **Step 1: Write the failing test**

Create `src-tauri/src/community_rendezvous_publisher.rs` with a test that uses a mock publisher capturing `register`/`unregister` calls:

```rust
//! Beacon-side publish of the community rendezvous slot record. A member that
//! is a relay advertiser at rank i < N publishes its reachability under
//! rendezvous_slot_key(epoch_key, i, epoch). Reuses the same routing blob and
//! pkarr register/unregister seam as the member-keyed community publisher.

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rank0_advertiser_registers_slot0() {
        let pub_spy = MockPublisher::default();
        let p = CommunityRendezvousPublisher::new_for_test(pub_spy.handle());
        let cid = SpaceId([1u8; 16]);
        let me = OwnerAddr([1u8; 16]);
        let others = vec![OwnerAddr([2u8; 16]), me];
        p.refresh_slot(cid, EpochKey::new([5u8; 32]), others, me).await;
        let regs = pub_spy.registrations();
        assert_eq!(regs.len(), 1);
        assert!(regs[0].handle.contains("rendezvous"));
        assert!(regs[0].handle.contains(&hex::encode(cid.0)));
    }

    #[tokio::test]
    async fn non_advertiser_unregisters_slot() {
        let pub_spy = MockPublisher::default();
        let p = CommunityRendezvousPublisher::new_for_test(pub_spy.handle());
        let cid = SpaceId([1u8; 16]);
        let me = OwnerAddr([9u8; 16]); // not in the set
        let others = vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])];
        // Pretend we were a beacon before, then dropped out.
        p.refresh_slot(cid, EpochKey::new([5u8; 32]), others.clone(), OwnerAddr([1u8;16])).await;
        p.refresh_slot(cid, EpochKey::new([5u8; 32]), others, me).await;
        assert!(pub_spy.unregistrations().iter().any(|h| h.contains("rendezvous")));
    }
}
```

> **Mock note:** mirror however the member publisher is tested (recon shows `PkarrPublisher` is an `Arc` with `register`/`unregister`). If there's no existing publisher mock, define a thin trait `RendezvousSink { async fn register(handle, key_builder, builder); async fn unregister(handle); }` that the real `PkarrPublisher` implements and a spy implements for tests. Keep the production path calling the real publisher.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_rendezvous_publisher)'`
Expected: FAIL.

- [ ] **Step 3: Implement**

Implement `CommunityRendezvousPublisher` mirroring `PkarrCommunityPublisher::on_community_joined` (recon `pkarr_community_publisher.rs:37-61`): build a `key_builder` closure that derives `rendezvous_slot_key(epoch_key, slot, current_epoch_id(at_ms))`, reuse the member publisher's `routing_blob_builder` + `PkarrRoutingRecord::sign_new` for the `builder`, register under handle `format!("rendezvous:{}:{}", hex::encode(community_id.0), slot)`. On `None` slot, `unregister` the per-community rendezvous handle(s). Track the currently-registered slot per community so a rank change re-registers under the new slot and unregisters the old.

> Wire `refresh_slot` to be called from `community_relay_publisher.rs`'s loop (which already knows when this node advertises) using `relays_for_community(...)` to get the advertiser set and `self_owner` for `me`. Pass `epoch_key` via `engine.membership_key()`.

- [ ] **Step 4: Run tests + clippy**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_rendezvous_publisher)'`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_rendezvous_publisher.rs src-tauri/src/community_relay_publisher.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(open-join): beacon publishes its rendezvous slot record

A relay advertiser at rank i<N registers a pkarr publish handle under
rendezvous_slot_key(epoch_key, i, epoch), reusing the member publisher's
routing-blob + sign_new seam; drops the slot when it leaves the advertiser set.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 8: Power-aware self-promotion + observability

**Files:**
- Modify: `src-tauri/src/community_rendezvous_publisher.rs`

**Interfaces:**
- Consumes: the advertiser set + this node's online/power state (an `is_high_availability`/power flag already used by the butler/relay opt-in — confirm the existing field), `RENDEZVOUS_SLOT_COUNT`.
- Produces:
  - `pub fn should_self_promote(advertisers: &[OwnerAddr], live_slot_count: usize, candidates_ranked: &[(OwnerAddr, PowerTier)], me: &OwnerAddr) -> bool` — pure decision: if the live slot set is under-filled and `me` is the lowest-ranked *eligible online* candidate (eligibility power-aware: desktop/opted-in preferred; low-power defers), return true.
  - `pub struct RendezvousObservability { pub promotions: AtomicU64, pub slot_fill_latency_samples: Mutex<Vec<u64>>, pub demotions: AtomicU64 }` with increment helpers, logged via `tracing`.

- [ ] **Step 1: Write the failing test**

Append tests to `src-tauri/src/community_rendezvous_publisher.rs`:

```rust
    #[test]
    fn lowest_ranked_eligible_online_promotes() {
        let a = OwnerAddr([1u8; 16]);
        let b = OwnerAddr([2u8; 16]);
        // Slots under-filled (0 live of N). Both online + high-availability.
        let ranked = vec![(a, PowerTier::HighAvailability), (b, PowerTier::HighAvailability)];
        assert!(should_self_promote(&[a, b], 0, &ranked, &a), "lowest rank promotes");
        assert!(!should_self_promote(&[a, b], 0, &ranked, &b), "higher rank defers");
    }

    #[test]
    fn low_power_defers_to_high_availability() {
        let a = OwnerAddr([1u8; 16]); // lowest rank but low power
        let b = OwnerAddr([2u8; 16]); // higher rank but high availability
        let ranked = vec![(a, PowerTier::LowPower), (b, PowerTier::HighAvailability)];
        assert!(!should_self_promote(&[a, b], 0, &ranked, &a), "low-power defers");
        assert!(should_self_promote(&[a, b], 0, &ranked, &b), "HA promotes as last resort");
    }

    #[test]
    fn filled_slots_suppress_promotion() {
        let a = OwnerAddr([1u8; 16]);
        let ranked = vec![(a, PowerTier::HighAvailability)];
        assert!(!should_self_promote(&[a], RENDEZVOUS_SLOT_COUNT, &ranked, &a));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_rendezvous_publisher)'`
Expected: FAIL — `should_self_promote`/`PowerTier` undefined.

- [ ] **Step 3: Implement**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerTier {
    HighAvailability, // desktop / opted-in / on AC power
    Normal,
    LowPower,         // mobile / battery — defers
}

/// Promote `me` only if the live slot set is under-filled AND `me` is the
/// lowest-ranked eligible online candidate. Eligibility is power-aware: prefer
/// HighAvailability, then Normal; LowPower promotes only when no better
/// candidate exists. Ranking among a tier is by address (same as slot claim),
/// so the decision is deterministic and convergent without coordination.
pub fn should_self_promote(
    _advertisers: &[OwnerAddr],
    live_slot_count: usize,
    candidates_ranked: &[(OwnerAddr, PowerTier)],
    me: &OwnerAddr,
) -> bool {
    if live_slot_count >= RENDEZVOUS_SLOT_COUNT {
        return false;
    }
    fn tier_rank(t: PowerTier) -> u8 {
        match t {
            PowerTier::HighAvailability => 0,
            PowerTier::Normal => 1,
            PowerTier::LowPower => 2,
        }
    }
    // Best tier present among online candidates.
    let Some(best_tier) = candidates_ranked.iter().map(|(_, t)| tier_rank(*t)).min() else {
        return false;
    };
    // Lowest-address candidate within the best tier is the promoter.
    let promoter = candidates_ranked
        .iter()
        .filter(|(_, t)| tier_rank(*t) == best_tier)
        .map(|(a, _)| *a)
        .min_by(|x, y| x.0.cmp(&y.0));
    promoter.map(|p| p.0 == me.0).unwrap_or(false)
}
```

Add the `RendezvousObservability` counters and increment them in the publisher: bump `promotions` when `refresh_slot` newly registers a slot due to self-promotion; record a `slot_fill_latency_sample` (now − under-filled-observed-at); `tracing::info!` each promotion with the observed-online-set size. These metrics exist so the convergence/oscillation behavior can be tuned from data (per the spec's open-question).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_rendezvous_publisher)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_rendezvous_publisher.rs
git commit -m "$(cat <<'EOF'
feat(open-join): power-aware self-promotion + observability counters

should_self_promote: lowest-ranked eligible online member fills an under-filled
slot; LowPower defers to HighAvailability. Promotion/latency/demotion counters
+ tracing so convergence can be tuned from observed data.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 9: Joiner escalating-batch rendezvous resolve

**Files:**
- Modify: `src-tauri/src/community_rendezvous.rs`

**Interfaces:**
- Consumes: `harmony_pkarr::PkarrResolver`, `harmony_pkarr::epoch::epoch_tolerance_window`, `rendezvous_slot_verifying_key`, `RENDEZVOUS_SLOT_COUNT`, `ReachabilityAnnouncePayload`.
- Produces:
  - `pub struct RendezvousResolveConfig { pub batch_curve: Vec<usize>, pub per_batch_deadline: Duration }` with `Default` (curve `[1, 2, RENDEZVOUS_SLOT_COUNT]` — try slot 0, widen to 0–1, then all) + `from_env()` reading `HARMONY_OPEN_JOIN_RESOLVE_*`.
  - `pub struct RendezvousResolveOutcome { pub payload: Option<ReachabilityAnnouncePayload>, pub winning_slot: Option<u16>, pub elapsed_ms: u64, pub batches_tried: usize }` (the instrumentation the spec's open-question asks for).
  - `pub async fn resolve_rendezvous(resolver: &PkarrResolver, epoch_key: &EpochKey, now_ms: u64, cfg: &RendezvousResolveConfig) -> RendezvousResolveOutcome` — escalating widening: for each batch width `w` in `batch_curve`, resolve slots `0..w` (across the epoch-tolerance window) in parallel; return on the first live, freshness-valid record, recording which slot answered and total elapsed.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `community_rendezvous.rs`. Use a mock resolver (a trait the real `PkarrResolver` satisfies, or an injected `Fn`) so the test is deterministic:

```rust
    // A resolver stub mapping verifying-key -> optional record, counting probes.
    #[tokio::test]
    async fn returns_slot0_without_widening_when_slot0_is_live() {
        let stub = StubResolver::with_live_slot(0);
        let cfg = RendezvousResolveConfig::default();
        let out = resolve_rendezvous_with(&stub, &EpochKey::new([5u8; 32]), 1_000_000, &cfg).await;
        assert_eq!(out.winning_slot, Some(0));
        assert_eq!(out.batches_tried, 1, "should not widen past the first batch");
    }

    #[tokio::test]
    async fn widens_to_find_a_live_slot_when_slot0_is_dead() {
        let stub = StubResolver::with_live_slot(2); // only slot 2 answers
        let cfg = RendezvousResolveConfig::default(); // curve [1,2,N]
        let out = resolve_rendezvous_with(&stub, &EpochKey::new([5u8; 32]), 1_000_000, &cfg).await;
        assert_eq!(out.winning_slot, Some(2));
        assert!(out.batches_tried >= 3, "had to widen to the full set");
    }

    #[tokio::test]
    async fn cold_start_returns_none() {
        let stub = StubResolver::all_dead();
        let cfg = RendezvousResolveConfig::default();
        let out = resolve_rendezvous_with(&stub, &EpochKey::new([5u8; 32]), 1_000_000, &cfg).await;
        assert_eq!(out.payload, None);
        assert_eq!(out.winning_slot, None);
    }
```

> Factor the resolve over a small `trait SlotResolver { async fn resolve_slot(&self, vk: &VerifyingKey) -> Option<ReachabilityAnnouncePayload>; }`. `resolve_rendezvous` (production) wraps a real `PkarrResolver` (derive slot vk → `pkarr.resolve` → freshness-check → decode blob); `resolve_rendezvous_with` takes any `SlotResolver` for tests. This keeps the escalating-batch logic pure and testable.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_rendezvous)'`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
use std::time::Duration;

pub struct RendezvousResolveConfig {
    /// Widening curve of batch widths, e.g. [1, 2, N]: probe slot 0, then 0..1,
    /// then all. Tunable so the success-rate/latency trade can be set from data.
    pub batch_curve: Vec<usize>,
    pub per_batch_deadline: Duration,
}

impl Default for RendezvousResolveConfig {
    fn default() -> Self {
        Self {
            batch_curve: vec![1, 2, RENDEZVOUS_SLOT_COUNT],
            per_batch_deadline: Duration::from_millis(2_500),
        }
    }
}

impl RendezvousResolveConfig {
    pub fn from_env() -> Self {
        // HARMONY_OPEN_JOIN_RESOLVE_CURVE="1,2,4", HARMONY_OPEN_JOIN_RESOLVE_DEADLINE_MS
        let mut cfg = Self::default();
        if let Ok(curve) = std::env::var("HARMONY_OPEN_JOIN_RESOLVE_CURVE") {
            let parsed: Vec<usize> = curve
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .filter(|w| *w >= 1 && *w <= RENDEZVOUS_SLOT_COUNT)
                .collect();
            if !parsed.is_empty() {
                cfg.batch_curve = parsed;
            }
        }
        if let Ok(ms) = std::env::var("HARMONY_OPEN_JOIN_RESOLVE_DEADLINE_MS") {
            if let Ok(ms) = ms.parse::<u64>() {
                cfg.per_batch_deadline = Duration::from_millis(ms.max(1));
            }
        }
        cfg
    }
}

#[derive(Debug, Default)]
pub struct RendezvousResolveOutcome {
    pub payload: Option<ReachabilityAnnouncePayload>,
    pub winning_slot: Option<u16>,
    pub elapsed_ms: u64,
    pub batches_tried: usize,
}
```

Implement `resolve_rendezvous_with<R: SlotResolver>(...)`: iterate `batch_curve`; for each width `w`, derive slot vks `0..w` across `epoch_tolerance_window(now_ms)`, probe them concurrently (`futures::future::join_all`), take the first `Some`, record `winning_slot` + `batches_tried` + elapsed; return early on hit. Production `resolve_rendezvous(...)` constructs a `PkarrSlotResolver { pkarr, now_ms }` whose `resolve_slot` derives the vk, calls `pkarr.resolve(&vk)`, runs `rec.verify_freshness(now_ms)`, and decodes `rec.routing_blob` into `ReachabilityAnnouncePayload`. Emit a `tracing::info!`/metric with `winning_slot` + `elapsed_ms` (the data the open-question wants).

> **Elapsed in tests:** `Date.now()`-style wall clock is fine in production (`SystemTime`), but tests pass `now_ms` explicitly and assert on `winning_slot`/`batches_tried`, not on `elapsed_ms` (avoid timing flake).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_rendezvous)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_rendezvous.rs
git commit -m "$(cat <<'EOF'
feat(open-join): escalating-batch rendezvous resolve (instrumented)

resolve_rendezvous widens slot probes [1,2,N] until a live record answers;
records winning-slot + elapsed + batches-tried so the parallel-resolve cost can
be tuned from data. Config knobs via HARMONY_OPEN_JOIN_RESOLVE_* env.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 10: Joiner open-join dial + send

**Files:**
- Create: `src-tauri/src/open_join_dial.rs`
- Modify: `src-tauri/src/lib.rs` (`mod open_join_dial;`)

**Interfaces:**
- Consumes: `crate::community_rendezvous::{resolve_rendezvous, RendezvousResolveConfig}`, `crate::open_join_auth::mint_epoch_auth`, `crate::community_invite::{build_signed_open_join_packet, encode_packet, decode_packet?, OpenJoinRequest, OpenJoinResponse}`, the iroh dial idioms from `connectivity_redeem_invite_iroh_inner` (`lib.rs:43425-43755`), `HandshakeDialConfig`.
- Produces:
  - `pub struct OpenJoinOutcome { pub status: String, pub community_id: Option<String> }` (mirror `RedemptionOutcome`; statuses: `"joined"`, `"no_beacon_reachable"`, `"beacon_rejected"`).
  - `pub async fn connectivity_open_join_iroh_inner(...) -> Result<OpenJoinOutcome, String>` — resolve a live beacon via `resolve_rendezvous`; if none → `Ok(OpenJoinOutcome{status:"no_beacon_reachable",..})` (retryable, NON-error); else synthesize the beacon's `EndpointAddr` (mirror `lib.rs:43390-43423`), dial `HARMONY_HANDSHAKE_V1`, `open_bi`, build+sign+send the `OpenJoinRequest` (`mint_epoch_auth` with a fresh random nonce + now), read the `OpenJoinResponse`, on `Admitted` merge `member_events` into the joiner engine and return `"joined"`; on `Rejected` return `"beacon_rejected"`.

- [ ] **Step 1: Write the failing test**

The real proof is the Task 12 integration test (it dials a live loopback beacon). For this task, add a unit test that the cold-start path returns a retryable non-error when resolve yields nothing, by injecting a resolve result:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cold_start_is_retryable_non_error() {
        // Inject a resolver outcome of "no live beacon" and assert the function
        // returns Ok(status="no_beacon_reachable"), not Err.
        let outcome = open_join_after_resolve(None /* no beacon */, test_ctx()).await;
        assert!(outcome.is_ok());
        assert_eq!(outcome.unwrap().status, "no_beacon_reachable");
    }
}
```

> Factor `connectivity_open_join_iroh_inner` so the post-resolve dial logic is a separately-callable `open_join_after_resolve(beacon: Option<ReachabilityAnnouncePayload>, ctx)` — the cold-start branch is then unit-testable without iroh, and the dial branch is covered by Task 12. `test_ctx()` builds the minimal context (engine handle, signing key, epoch_key, community_id) used by the cold-start branch (which never dials).

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join_dial)'`
Expected: FAIL.

- [ ] **Step 3: Implement**

Implement `connectivity_open_join_iroh_inner` by closely mirroring `connectivity_redeem_invite_iroh_inner` (recon section 3/6): the resolve step calls `resolve_rendezvous`; the addr-synthesis + `connect` + `open_bi` + length-prefixed write are copied from the invite-only dial (swap the packet to `build_signed_open_join_packet`); the response read decodes `OpenJoinResponse` instead of a countersign. Return the retryable `"no_beacon_reachable"` on empty resolve and on dial failure (mirror the invite-only `"inviter_unreachable"` non-error return).

> Generate the `nonce` with `rand::rngs::OsRng` (per-attempt fresh); compute `created_at`/`timestamp` from the same wall clock the beacon uses; `joiner_identity_pub`/`signing_device_hash` are derived exactly as the invite-only sender derives them (recon section 3, `lib.rs:43603-43640`). The `join_event` is the minted self `bootstrap_join` (same as the open-redeem path mints today).

- [ ] **Step 4: Run tests + clippy**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join_dial)'` and clippy.
Expected: PASS / no warnings.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/open_join_dial.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(open-join): joiner dial + send OpenJoinRequest

connectivity_open_join_iroh_inner resolves a live beacon (escalating batch),
synthesizes its iroh addr, dials HARMONY_HANDSHAKE_V1, sends a capability-proven
OpenJoinRequest, and merges the Admitted snapshot. Empty resolve / dial failure
is a retryable non-error (no_beacon_reachable).

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 11: Wire into the open-redeem path + retryable cold-start UI

**Files:**
- Modify: `src-tauri/src/lib.rs` (`redeem_invite_inner_with_overrides` open branch `~26066`; `RedemptionOutcome`/`RedeemInviteResultDto`)
- Modify: `src/lib/...` (frontend join flow + a vitest)

**Interfaces:**
- Consumes: `crate::open_join_dial::connectivity_open_join_iroh_inner`.
- Produces: the open-redeem path now (a) does the existing local self-Join insert AND (b) attempts cross-WAN first contact via `connectivity_open_join_iroh_inner`; the result maps to a DTO status the frontend renders. Same-LAN Zenoh path is unaffected (it still converges locally).

- [ ] **Step 1: Write the failing test (Rust DTO mapping)**

Add a unit test near the redeem DTO mapping that a `"no_beacon_reachable"` open-join outcome maps to a retryable DTO status (e.g. `RedeemInviteResultDto { pending: true, status: "searching", .. }`) rather than an error:

```rust
#[test]
fn open_join_cold_start_maps_to_retryable_dto() {
    let dto = redeem_dto_from_open_join_outcome(OpenJoinOutcome {
        status: "no_beacon_reachable".into(),
        community_id: Some("aa".into()),
    });
    assert!(dto.pending, "cold-start is retryable, not failed");
    assert_eq!(dto.status.as_deref(), Some("searching"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join_cold_start_maps)'`
Expected: FAIL — mapping fn / DTO field undefined.

- [ ] **Step 3: Implement the wiring + DTO field + frontend**

1. In `redeem_invite_inner_with_overrides` open branch (`~26066`), after the existing `engine_arc.insert_local_event(minted.bootstrap_join)`, call `connectivity_open_join_iroh_inner(...)` (passing the resolver/endpoint/engine handles already in scope — the function already receives `pkarr_resolver`-style args for the invite-only iroh path; thread the same ones). Map the `OpenJoinOutcome` to the DTO via `redeem_dto_from_open_join_outcome`. Add a `status: Option<String>` field to `RedeemInviteResultDto` (default `None`; `Some("searching")` for cold-start, `Some("joined")` on success). Keep the local insert so same-LAN keeps working.

2. Frontend: in the join handler that calls the redeem IPC, branch on `result.status === 'searching'` to show a non-blocking banner: "No one's reachable right now — we'll keep trying." Rely on the existing transport-epoch re-arm to retry; no new polling loop.

- [ ] **Step 4: Write + run the frontend test**

Add `src/lib/.../__tests__/open-join-cold-start.test.ts` asserting the join handler renders the retry banner for `status: 'searching'` and the normal joined state otherwise. Run (repo root): `npx vitest run open-join-cold-start` and `npx tsc --noEmit`.
Expected: PASS / no type errors.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src/lib
git commit -m "$(cat <<'EOF'
feat(open-join): wire cross-WAN first contact into open redeem + cold-start UI

Open redeem now attempts beacon first-contact after the local self-Join insert;
maps no_beacon_reachable to a retryable "searching" DTO status with a
keep-trying banner. Same-LAN Zenoh path unchanged.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 12: Two-node cross-WAN open-join integration test (must FAIL on main)

**Files:**
- Create: `src-tauri/tests/community_open_join_cross_wan_integration.rs`

**Interfaces:**
- Consumes: the full stack (Tasks 1–11). Mirrors `tests/pkarr_iroh_redeem_full_integration.rs` (`setup_two_party_iroh_handshake`, `283`; `bob_joins_alice_via_iroh_handshake_option_a`, `719`).

- [ ] **Step 1: Write the failing test**

Mirror the two-party harness, OPEN variant:
- Alice creates an **open** community (`is_invite_only = false`), is a relay advertiser at rank 0, and publishes her reachability under `rendezvous_slot_key(epoch_key, 0, epoch)` into the mock pkarr.
- Bob holds ONLY the open URL (`community_id` + `epoch_key`); no LAN multicast (loopback iroh only, as the existing harness already configures).
- Bob runs `connectivity_open_join_iroh_inner` (or the open redeem path): resolves slot 0 from the mock pkarr, dials Alice on `HARMONY_HANDSHAKE_V1`, sends the `OpenJoinRequest`.
- Assert: `outcome.status == "joined"`; **Alice's engine materializes Bob as `MemberStatus::Joined`** (this is the assertion that fails on main — on main the open path never dials, so Alice never learns about Bob cross-WAN).

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bob_open_joins_alice_cross_wan_via_rendezvous() {
    let s = setup_two_party_open_join().await; // mirror setup_two_party_iroh_handshake

    // Alice publishes her rendezvous slot-0 record into the mock pkarr.
    s.alice_publish_rendezvous_slot(0).await;

    let outcome = connectivity_open_join_iroh_inner(
        /* bob's open URL, pkarr_resolver, reachability, iroh endpoint, engine
           handles, signing key, enrollment cert, dial config, ... — mirror the
           invite-only test's argument wiring */
    )
    .await
    .expect("open join inner");

    assert_eq!(outcome.status, "joined");

    // The load-bearing cross-WAN assertion: Alice's engine sees Bob Joined.
    let alice_state = s.registry_alice.state_for(&s.community_id).await.unwrap();
    let mat = {
        let g = alice_state.lock().await;
        crate::community_membership::materialize(&g.events.values().cloned().collect::<Vec<_>>())
    };
    let bob = mat.members.get(&s.bob_addr).expect("Alice must know Bob");
    assert_eq!(bob.status, crate::community_membership::MemberStatus::Joined);
}
```

- [ ] **Step 2: Verify it FAILS on main (pre-fix baseline)**

Before the Task 1–11 commits are present (or by stashing them), run the test against `main`'s open-redeem path:
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(bob_open_joins_alice_cross_wan)'`
Expected: **FAIL** — Alice never learns about Bob (no dial on the open path). Record this as the must-fail baseline in the PR description.

- [ ] **Step 3: Confirm it PASSES with the full stack**

With Tasks 1–11 present:
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(bob_open_joins_alice_cross_wan)'`
Expected: PASS.

- [ ] **Step 4: Add the failover + cold-start integration variants**

Add two more tests in the same file:
- `open_join_fails_over_to_slot1_when_slot0_beacon_offline`: publish only slot 1; assert Bob still joins (escalating resolve widens).
- `open_join_cold_start_is_retryable_then_succeeds`: publish NO slot → assert `status == "no_beacon_reachable"`; then publish slot 0 and re-run → assert `"joined"`.

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(open_join)'`
Expected: PASS (all open-join integration tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/tests/community_open_join_cross_wan_integration.rs
git commit -m "$(cat <<'EOF'
test(open-join): two-node cross-WAN open-join round-trip (fails pre-fix)

Mirrors the invite-only handshake test, open variant: Bob holds only the URL
(epoch_key), resolves Alice's rendezvous slot, dials, is admitted; asserts
Alice's engine materializes Bob as Joined. Plus slot-1 failover + retryable
cold-start variants.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
```

---

### Task 13: Final full-sweep gates

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cd src-tauri && cargo fmt --all`
Then verify: `cargo fmt --all -- --check` → 0 diffs.

- [ ] **Step 2: Clippy (all targets)**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: 0 warnings. Fix any in-scope warnings; commit fixes.

- [ ] **Step 3: Full Rust test sweep**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: all PASS (treat any orphan transport/iroh flake per the known-flake memory — re-run once; if it reproduces and is unrelated, file a follow-up rather than folding a fix in).

- [ ] **Step 4: Frontend gates**

Run (repo root): `npx tsc --noEmit` then `npx vitest run`
Expected: no type errors; all vitest pass.

- [ ] **Step 5: Commit any sweep fixes + open the PR**

```bash
# only if the sweep required fixes:
git add -A && git commit -m "$(cat <<'EOF'
chore(open-join): final gate fixes (fmt/clippy/test sweep)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
EOF
)"
git push -u origin open-community-cross-wan-first-contact
```

Then open the PR (via `superpowers:finishing-a-development-branch`) and run the **usual autonomous bot-review loop**: Qodo + CodeAnt first pass → address everything (incl. decline-with-rationale, ignoring the CodeAnt plan-doc false-positive) → ONE CodeRabbit final review (`@coderabbitai review`) → converge → pushover Jake at ready-to-merge. NEVER self-merge; NEVER trigger Greptile. PR body: plain ZEB-570 reference (no close-keyword), spec + plan paths, the must-fail-on-main baseline, summary of the 13 tasks, test plan checklist. End with the Claude Code attribution line.

---

## Self-Review

**1. Spec coverage:**
- Component 1 (rendezvous record / enumerated slots) → Tasks 1, 3, 7. ✓
- Component 2 (tokenless open-join handshake: message + capability + beacon verify→admit) → Tasks 2, 4, 5, 6. ✓
- Component 3 (beacon election + self-healing) → Tasks 3 (slot claim), 7 (publish), 8 (power-aware self-promotion + observability). ✓
- Component 4 (joiner UX + cold-start) → Tasks 9 (resolve), 10 (dial + retryable cold-start), 11 (redeem wiring + UI). ✓
- Spec testing plan: unit (Tasks 1–10), integration must-fail-on-main + failover + cold-start (Task 12), gates (Task 13). ✓ E2E on the fleet (ZEB-447 harness) is correctly left as a post-merge fleet step, not a code task (matches spec "E2E … to validate live on the fleet").
- Open-question instrumentation: escalating-batch + winning-slot/latency metric (Task 9); self-promotion observability counters (Task 8). ✓ (matches the spec's `71c0638b` tuning refinement)
- Privacy/abuse posture: capability gate at resolve (slot keys from epoch_key) + at admission (`epoch_auth`), ban-check, rate-limit → Tasks 2, 5. ✓

**2. Placeholder scan:** No "TBD"/"implement later". Big-integration tasks (6, 10, 11, 12) cite exact insertion `file:line` + the function to mirror + concrete call sequences; where the live code may have drifted, an explicit "confirm against …" implementer-check is given (not a vague placeholder). Acceptable for a feature integrating into a 46k-line file.

**3. Type consistency:** `EpochKey::as_bytes() -> &[u8;32]`, `SpaceId.0:[u8;16]`, `OwnerAddr.0:[u8;16]`, `Hlc.wall_ms`, `OpenJoinRequest` fields (`community_id`, `join_event`, `joiner_identity_pub`, `signing_device_hash`, `epoch_auth`, `nonce`, `created_at`) are used identically across Tasks 4, 5, 6, 10. `mint_epoch_auth`/`verify_epoch_auth` signatures match between Tasks 2 and 5. `rendezvous_slot_key`/`rendezvous_slot_verifying_key` consistent across Tasks 1, 7, 9. `OpenJoinResponse::{Admitted,Rejected}` defined in Task 6, consumed in Task 10. `RENDEZVOUS_SLOT_COUNT` referenced (never literal `4`) throughout. ✓

**Known adaptation risks flagged for the implementer (not gaps):** exact engine accessor names (`admin_addr()`, `engine_for`, `insert_remote_event`), the canonical-CBOR encoder used for the packet signature preimage (must match mint/verify byte-for-byte — prefer threading `signed_bytes` over re-encoding), and reuse of existing membership test mints. Each is called out at its task.
