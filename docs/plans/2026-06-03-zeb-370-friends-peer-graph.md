# ZEB-370 Friends & Peer Introductions — Phase 1 Implementation Plan

> **⚠️ CORRECTION (identity model changed mid-build):** the inline Task-1–10 type snippets below predate a mid-build correction to the identity model and are **out of date**. Friends key on the master **`owner_id`** (16 bytes), and `FriendEntry` stores the friend's **`master_ed25519: [u8; 32]`** anchor — *not* a `friend_owner_pub`/`inviter_owner_pub: [u8; 64]` Reticulum combined-pub. Handshake/token auth is the **device-#2 signature + `EnrollmentCert`** model (no separate optional-enrollment field; the cert is required). See spec §3 and the refactor commits `e168e7e` / `72b12f3`. **The shipped code and the spec are authoritative** — where any snippet below disagrees (e.g. `friend_owner_pub`/`inviter_owner_pub`, optional enrollment), the snippet is stale and the shipped code/spec win.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a first-class **Friend Graph** (owner-state CRDT) plus **token-based peering** in harmony-client — "send a friend a `harmony://friend/...` link, they redeem it, you're mutually peered and they appear in your Friends list" — entirely within harmony-client, reusing the merged ZEB-367 invite machinery.

**Architecture:** A new `FriendGraph` owner-state sub-CRDT (mirrors `OwnerDeviceCache`, LWW-merged, synced across the user's own devices on the existing owner-state topic). A `FriendTokenPayload` + `harmony://friend/` URL codec reusing `mint_invite_token` + the Case-A `PkarrInvitePublisher`. A `harmony/friend/v1` iroh ALPN + acceptor (mirrors `iroh_invite_acceptor.rs`) where each party writes a `FriendEntry` to *its own* owner-state — no shared community CRDT. Minimal Friends UI via a `FriendService` mirroring `community-service.ts`.

**Tech Stack:** Rust (Tauri v2 backend), `ciborium` canonical CBOR, `ed25519-dalek`, `x25519-dalek`, iroh 0.98, Svelte 5 frontend, `cargo-nextest` + vitest.

**Spec:** `docs/specs/2026-06-03-friends-peer-introductions-design.md`

**Deferred to Phase 1b/2 (NOT in this PR):** owner-X25519 pairwise-secret helper, `PkarrCase::Friend`/Case-D rendezvous, cross-WAN steady-state reconnection, mutual-key path, referral catalog, introduction broker, `PeerIntroPolicy` *enforcement*. (The `PeerIntroPolicy` *type* and `referrable`/`established_via` *fields* ship now so the data model needs no later migration.)

**Conventions (CLAUDE.md):**
- Gate from `src-tauri/`: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo fmt --all -- --check`. `--locked` and `--all-targets` are load-bearing.
- IPC: Rust params `snake_case`, JS args `camelCase` (Tauri auto-converts).
- Canonical-CBOR map keys at one nesting level must share the same encoded byte length (single-char key = 2 bytes; two-char = 3 bytes). Do not mix within a struct.
- Commit after each task. End commit messages with `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

---

## File Structure

**Create:**
- `src-tauri/src/friend_graph.rs` — `FriendGraph`, `FriendEntry`, `FriendStatus`, `FriendOrigin`, `PeerIntroPolicy` types + serde + unit tests.
- `src-tauri/src/friend_token.rs` — `FriendTokenPayload`, `encode_friend_token_url`/`decode_friend_token_url`, `mint_friend_token`.
- `src-tauri/src/iroh_friend_acceptor.rs` — `harmony/friend/v1` acceptor + `FriendLinkRequest`/`FriendLinkAccepted` wire types + packet codec.
- `src-tauri/tests/friend_token_roundtrip_integration.rs` — two-node end-to-end peering test.
- `src-tauri/tests/wire_format_zeb370_fixtures.rs` — canonical-CBOR pinning for the new wire types.
- `src/lib/friend-service.ts` — frontend service (mirrors `community-service.ts`).
- `src/lib/friend-service.test.ts` — vitest unit tests.
- `src/lib/components/FriendsPanel.svelte` — minimal Friends UI.

**Modify:**
- `src-tauri/src/owner_state_crdt.rs` — add `friend_graph` field to `OwnerState` (`:22`); add `apply_friend_update` method.
- `src-tauri/src/owner_state_sync.rs` — add FriendGraph loop to `merge_remote_into_local` (`:538`); destructure at `:541`.
- `src-tauri/src/owner_state_persist.rs` — add `friend_graph` to `CrdtFileV2` (`:78`) + both `From` impls (`:116`, `:131`).
- `src-tauri/src/owner_state_types.rs` — add `impl_canonical!(FriendGraph, FriendEntry, FriendTokenPayload, ...)` (`:1148`); add a `[u8;64]` bstr serde helper if one is not already shared.
- `src-tauri/src/iroh_endpoint.rs` — declare + register `HARMONY_FRIEND_V1` ALPN (`:47`, `:89`).
- `src-tauri/src/pkarr_invite_publisher.rs` — add `register_friend_token(&self, token_sig: &[u8;64], routing_blob)` + `unregister_friend_token` (handle `friend:{hex}`).
- `src-tauri/src/lib.rs` — `NodeState` field for the friend acceptor; IPCs `generate_friend_token`, `redeem_friend_token`, `list_friends`, `unfriend`; register in `generate_handler!`; `connectivity_link_friend_iroh_inner` connect path; dispatch wiring.
- `src/lib/<module registry>` — register `FriendService` + route the panel into the existing nav (mirror how `community-service.ts` is wired).
- `src/App.svelte` (or the nav host) — surface the Friends panel entry.

---

## Task 1: FriendGraph data types + serde

**Files:**
- Create: `src-tauri/src/friend_graph.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod friend_graph;`)
- Template to mirror: `src-tauri/src/owner_state_types.rs:438-728` (`OwnerDeviceCache`/`OwnerDeviceEntry`), and `community_invite.rs` `admin_identity_pub: Option<[u8;64]>` for the `[u8;64]` bstr serde.

- [ ] **Step 1: Write the failing test** (in `friend_graph.rs` `#[cfg(test)]`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn hlc(w: u64) -> Hlc { Hlc { wall_ms: w, logical: 0, device_id: "d".into() } }

    fn sample_entry() -> FriendEntry {
        FriendEntry {
            master_ed25519: [0x11; 32],
            display: Some("alice".into()),
            status: FriendStatus::Active,
            established_via: FriendOrigin::Token,
            referrable: false,
            learned_at: hlc(7),
        }
    }

    #[test]
    fn friend_entry_round_trips() {
        let e = sample_entry();
        let bytes = canonical_cbor_encode(&e).expect("encode");
        let back: FriendEntry = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(e, back);
    }

    #[test]
    fn friend_graph_round_trips_with_entry() {
        let mut g = FriendGraph::default();
        g.friends.insert(OwnerAddr([0x22; 16]), sample_entry());
        let bytes = canonical_cbor_encode(&g).expect("encode");
        let back: FriendGraph = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(g, back);
    }

    #[test]
    fn default_policy_is_friends_of_friends() {
        assert_eq!(PeerIntroPolicy::default(), PeerIntroPolicy::FriendsOfFriends);
    }
}
```

- [ ] **Step 2: Run it, watch it fail** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(friend_graph)'` → FAIL (types undefined).

- [ ] **Step 3: Implement the types.** Use single-char wire keys inside `FriendEntry` (all 2-byte encoded). Represent the enums as unit variants (CBOR encodes them as their name strings — values, not map keys, so unaffected by the same-length-key rule; pinned in Task 13).

```rust
//! ZEB-370 Phase 1: Friend Graph owner-state sub-CRDT + token/policy types.
//! Mirrors OwnerDeviceCache (owner_state_types.rs): BTreeMap keyed by OwnerAddr,
//! LWW-merged on `learned_at`. Friend links live in EACH owner's own owner-state
//! (replicated across that owner's devices) — there is no shared friend CRDT.

use crate::owner_state_types::{Hlc, OwnerAddr};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Lifecycle of a friend link. `Revoked` is an LWW tombstone (kept, not deleted,
/// so an unfriend on one device cannot be silently resurrected by a stale Active).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FriendStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "revoked")]
    Revoked,
}

/// How the link was formed (provenance, for UX + audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FriendOrigin {
    #[serde(rename = "mutual_key")]
    MutualKey,
    #[serde(rename = "token")]
    Token,
    #[serde(rename = "introduction")]
    Introduction,
}

/// Per-user policy governing whether OTHERS may reach you via a friend's
/// introduction. Stored now; ENFORCED in Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerIntroPolicy {
    #[serde(rename = "open")]
    Open,
    #[serde(rename = "fof")]
    FriendsOfFriends,
    #[serde(rename = "ask")]
    AskMe,
    #[serde(rename = "closed")]
    Closed,
}
impl Default for PeerIntroPolicy {
    fn default() -> Self { Self::FriendsOfFriends }
}

fn serialize_pub64<S: serde::Serializer>(v: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_bytes(v)
}
fn deserialize_pub64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
    let v: serde_bytes::ByteBuf = serde::Deserialize::deserialize(d)?;
    v.as_ref().try_into().map_err(|_| serde::de::Error::invalid_length(v.len(), &"64 bytes"))
}

/// One friend, keyed in `FriendGraph.friends` by the friend's OwnerAddr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendEntry {
    /// Friend's 64-byte owner identity: X25519_pub(32) || Ed25519_pub(32).
    #[serde(rename = "p", serialize_with = "serialize_pub64", deserialize_with = "deserialize_pub64")]
    pub friend_owner_pub: [u8; 64],
    #[serde(rename = "n", skip_serializing_if = "Option::is_none", default)]
    pub display: Option<String>,
    #[serde(rename = "s")]
    pub status: FriendStatus,
    #[serde(rename = "v")]
    pub established_via: FriendOrigin,
    #[serde(rename = "r", default)]
    pub referrable: bool,
    #[serde(rename = "l")]
    pub learned_at: Hlc,
}

/// Owner-state sub-CRDT. Replicated across the user's own devices via the existing
/// owner-state Zenoh topic; LWW-merged per entry on `learned_at`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendGraph {
    #[serde(rename = "f")]
    pub friends: BTreeMap<OwnerAddr, FriendEntry>,
}
impl FriendGraph {
    pub fn is_empty(&self) -> bool { self.friends.is_empty() }
}
```

Add `pub mod friend_graph;` to `lib.rs` near the other `mod` decls. (Use `serde_bytes` if already a dep; check `Cargo.toml` — `ciborium`/`serde_bytes` patterns already exist for `[u8;64]` in `community_invite.rs`; reuse that exact helper instead of the inline one above if present.)

- [ ] **Step 4: Run tests, watch pass** — same command → PASS.
- [ ] **Step 5: clippy + fmt** — `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all`.
- [ ] **Step 6: Commit** — `git add -A && git commit -m "feat(zeb-370): FriendGraph/FriendEntry/PeerIntroPolicy types + serde\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"`

---

## Task 2: Embed `friend_graph` in `OwnerState` + LWW apply

**Files:**
- Modify: `src-tauri/src/owner_state_crdt.rs` (`OwnerState` at `:22`; add `apply_friend_update`)
- Template: the `apply_owner_device_update` method (`owner_state_crdt.rs:588-726`) and its `#[cfg(test)]` LWW tests.

- [ ] **Step 1: Write the failing test** (in `owner_state_crdt.rs` test module):

```rust
#[test]
fn friend_lww_newer_wins_and_tombstone_sticks() {
    use crate::friend_graph::{FriendEntry, FriendOrigin, FriendStatus};
    use crate::owner_state_types::{Hlc, OwnerAddr};
    let mut s = OwnerState::default();
    let addr = OwnerAddr([9u8; 16]);
    let mk = |w: u64, st: FriendStatus| FriendEntry {
        friend_owner_pub: [1u8; 64], display: None, status: st,
        established_via: FriendOrigin::Token, referrable: false,
        learned_at: Hlc { wall_ms: w, logical: 0, device_id: "d".into() },
    };
    // First active.
    assert!(matches!(s.apply_friend_update(addr, mk(10, FriendStatus::Active)), ApplyOutcome::Applied));
    // Newer revoke wins (tombstone).
    assert!(matches!(s.apply_friend_update(addr, mk(20, FriendStatus::Revoked)), ApplyOutcome::Applied));
    assert_eq!(s.friend_graph.friends[&addr].status, FriendStatus::Revoked);
    // Stale active (older HLC) must NOT resurrect.
    assert!(matches!(s.apply_friend_update(addr, mk(15, FriendStatus::Active)), ApplyOutcome::StaleHlc));
    assert_eq!(s.friend_graph.friends[&addr].status, FriendStatus::Revoked);
}
```

- [ ] **Step 2: Run, watch fail** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(friend_lww)'` → FAIL.

- [ ] **Step 3: Implement.** Add the field to `OwnerState` (after `owner_device_cache`, wire key `"fg"`, skip-if-empty + default):

```rust
    #[serde(rename = "fg", skip_serializing_if = "crate::friend_graph::FriendGraph::is_empty", default)]
    pub friend_graph: crate::friend_graph::FriendGraph,
```

Add the apply method (mirror `apply_owner_device_update`'s LWW guard — reuse `Hlc::is_strictly_newer_than`):

```rust
    /// LWW-apply a friend entry. Newer `learned_at` wins; equal-HLC identical is
    /// idempotent; older is rejected as stale.
    pub fn apply_friend_update(
        &mut self,
        addr: crate::owner_state_types::OwnerAddr,
        entry: crate::friend_graph::FriendEntry,
    ) -> ApplyOutcome {
        match self.friend_graph.friends.get(&addr) {
            Some(existing) if existing.learned_at.is_strictly_newer_than(&entry.learned_at) => {
                ApplyOutcome::StaleHlc
            }
            Some(existing) if existing.learned_at == entry.learned_at => {
                if existing == &entry { ApplyOutcome::Merged } else { ApplyOutcome::InvariantFail }
            }
            _ => {
                self.friend_graph.friends.insert(addr, entry);
                ApplyOutcome::Applied
            }
        }
    }
```

(Confirm the exact `ApplyOutcome` variant names against `owner_state_crdt.rs` — use whatever `apply_owner_device_update` returns; adjust the test accordingly.)

- [ ] **Step 4: Run, watch pass.**
- [ ] **Step 5: clippy + fmt.**
- [ ] **Step 6: Commit** — `feat(zeb-370): embed friend_graph in OwnerState + LWW apply_friend_update`

---

## Task 3: Wire FriendGraph into sync + persistence + canonical registration

**Files:**
- Modify: `src-tauri/src/owner_state_sync.rs` (`merge_remote_into_local` `:538`, destructure `:541`)
- Modify: `src-tauri/src/owner_state_persist.rs` (`CrdtFileV2` `:78`; `From<&OwnerState>` `:116`; `From<CrdtFileV2>` `:131`)
- Modify: `src-tauri/src/owner_state_types.rs` (`impl_canonical!` `:1148`)

- [ ] **Step 1: Write the failing tests.**

(3a) Persistence round-trip + backward-compat — in `owner_state_persist.rs` tests:

```rust
#[test]
fn crdt_file_v2_round_trips_friend_graph() {
    let mut s = OwnerState::default();
    s.apply_friend_update(OwnerAddr([3u8; 16]), /* sample FriendEntry, learned_at hlc */ );
    let file = CrdtFileV2::from(&s);
    let bytes = canonical_cbor_encode(&file).expect("encode");
    let back: CrdtFileV2 = canonical_cbor_decode(&bytes).expect("decode");
    let s2 = OwnerState::from(back);
    assert_eq!(s2.friend_graph, s.friend_graph);
}

#[test]
fn pre_friendgraph_snapshot_loads_empty() {
    // A V2 file serialized WITHOUT the friend_graph field must load to empty.
    let s = OwnerState::default(); // no friends → field skipped on the wire
    let bytes = canonical_cbor_encode(&CrdtFileV2::from(&s)).expect("encode");
    let back: CrdtFileV2 = canonical_cbor_decode(&bytes).expect("decode");
    assert!(OwnerState::from(back).friend_graph.is_empty());
}
```

(3b) Two-engine sync convergence — in `tests/owner_state_sync.rs` (mirror `subscriber_fetches_and_merges_remote_state`): publisher applies a friend update + publishes root; subscriber engine converges so `friend_graph.friends` contains the addr. Use the `wait_until(..., Duration::from_secs(2))` helper (ZEB-347 floor).

- [ ] **Step 2: Run, watch fail.**

- [ ] **Step 3: Implement the wiring.**
  - `CrdtFileV2`: add `#[serde(rename = "fg", skip_serializing_if = "...is_empty", default)] pub friend_graph: FriendGraph,`. Add `friend_graph: s.friend_graph.clone()` to `From<&OwnerState>` and `friend_graph: f.friend_graph` to `From<CrdtFileV2>`.
  - `merge_remote_into_local`: add `friend_graph` to the `OwnerState { .. }` destructure of `remote` (`:541`), then after the `owner_device_cache` loop:

```rust
    for (addr, entry) in friend_graph.friends {
        local.apply_friend_update(addr, entry);
    }
```

  - `impl_canonical!`: extend the macro invocation to include `FriendGraph`, `FriendEntry`, and (for Task 4) `FriendTokenPayload` so they gain the sealed canonical-encode trait.

  - [ ] **Step 4: Run, watch pass.** Full sync test must converge.
  - [ ] **Step 5: clippy + fmt.**
  - [ ] **Step 6: Commit** — `feat(zeb-370): sync + persist FriendGraph sub-CRDT (auto-replicated across own devices)`

---

## Task 4: Friend-token payload + URL codec

**Files:**
- Create: `src-tauri/src/friend_token.rs` (add `mod friend_token;` to `lib.rs`)
- Template: `community_invite.rs` `encode_invite_url`/`decode_invite_url` (`:896`, `:961`), `URL_PREFIX`, `MAX_INVITE_BODY_B64_CHARS`.

- [ ] **Step 1: Write the failing test:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_invite::InviteToken;
    use crate::owner_state_types::{Hlc, OwnerAddr};

    fn sample() -> FriendTokenPayload {
        FriendTokenPayload {
            inviter_addr: OwnerAddr([1u8; 16]),
            inviter_owner_pub: [2u8; 64],
            display_hint: Some("bob".into()),
            token: InviteToken {
                inviter: OwnerAddr([1u8; 16]), invitee_hint: None,
                minted_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
                expires_at: None, sig: [3u8; 64],
            },
            inviter_enrollment: None, // Some(cert) in real mint; codec must handle None
        }
    }

    #[test]
    fn friend_token_url_round_trips() {
        let p = sample();
        let url = encode_friend_token_url(&p).expect("encode");
        assert!(url.starts_with("harmony://friend/"));
        assert_eq!(decode_friend_token_url(&url).expect("decode"), p);
    }

    #[test]
    fn decode_rejects_wrong_prefix() {
        assert!(decode_friend_token_url("harmony://invite/AAAA").is_err());
    }

    #[test]
    fn decode_rejects_oversized_body() {
        let url = format!("harmony://friend/{}", "A".repeat(MAX_FRIEND_BODY_B64_CHARS + 1));
        assert!(decode_friend_token_url(&url).is_err());
    }
}
```

- [ ] **Step 2: Run, watch fail.**

- [ ] **Step 3: Implement** `FriendTokenPayload` (derive Serialize/Deserialize + the canonical trait via `impl_canonical!`; `inviter_owner_pub: [u8;64]` reuses the bstr helper; `inviter_enrollment: Option<EnrollmentCert>`), plus `encode_friend_token_url`/`decode_friend_token_url` mirroring the invite codec: `const URL_PREFIX = "harmony://friend/"`, `const MAX_FRIEND_BODY_B64_CHARS` (reuse the invite cap), canonical-CBOR → base64url-no-pad → prefix; decode strips prefix, size-caps, base64url-decodes, canonical-CBOR-decodes. Use a `FriendTokenError` enum (mirror `InviteUrlError`).

- [ ] **Step 4: Run, watch pass.**
- [ ] **Step 5: clippy + fmt.**
- [ ] **Step 6: Commit** — `feat(zeb-370): FriendTokenPayload + harmony://friend/ URL codec`

---

## Task 5: Mint a friend token

**Files:**
- Modify: `src-tauri/src/friend_token.rs` (add `mint_friend_token`)
- Reuse: `invite_mint::mint_invite_token` (signs the `InviteToken` with the device-#2 key).

- [ ] **Step 1: Write the failing test:**

```rust
#[test]
fn minted_friend_token_verifies_and_encodes() {
    use crate::community_invite::verify_invite_token_sig_device_key;
    let device2 = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let p = mint_friend_token(
        OwnerAddr([1u8; 16]),       // inviter_addr
        [2u8; 64],                  // inviter_owner_pub
        Some("bob".into()),         // display_hint
        Hlc { wall_ms: 100, logical: 0, device_id: "d".into() }, // minted_at
        Some(200),                  // expires_at
        None,                       // inviter_enrollment (Some in prod)
        &device2,
    ).expect("mint");
    verify_invite_token_sig_device_key(&p.token, &device2.verifying_key().to_bytes()).expect("sig ok");
    assert!(encode_friend_token_url(&p).is_ok());
}
```

- [ ] **Step 2: Run, watch fail.**

- [ ] **Step 3: Implement:**

```rust
#[allow(clippy::too_many_arguments)]
pub fn mint_friend_token(
    inviter_addr: OwnerAddr,
    inviter_owner_pub: [u8; 64],
    display_hint: Option<String>,
    minted_at: Hlc,
    expires_at: Option<u64>,
    inviter_enrollment: Option<EnrollmentCert>,
    device2_signing_key: &ed25519_dalek::SigningKey,
) -> Result<FriendTokenPayload, String> {
    // invitee_hint = None (untargeted, "controlled open" friend link).
    let token = crate::invite_mint::mint_invite_token(
        inviter_addr, None, minted_at, expires_at, device2_signing_key,
    )?;
    Ok(FriendTokenPayload { inviter_addr, inviter_owner_pub, display_hint, token, inviter_enrollment })
}
```

- [ ] **Step 4: Run, watch pass.**
- [ ] **Step 5: clippy + fmt.**
- [ ] **Step 6: Commit** — `feat(zeb-370): mint_friend_token (reuses device-#2 InviteToken signing)`

---

## Task 6: Case-A pkarr publication for friend tokens

**Files:**
- Modify: `src-tauri/src/pkarr_invite_publisher.rs` (add `register_friend_token` / `unregister_friend_token`)
- Template: existing `register_invite`/`unregister_invite` (handle `invite:{hex}`, key `derive_ephemeral_key(PkarrCase::Invite, token_sig, epoch)`).

- [ ] **Step 1: Write the failing test** (mirror `enable_then_disable_round_trip` with `MockPkarrRelay`):

```rust
#[tokio::test]
async fn friend_token_register_unregister_round_trip() {
    // ... build PkarrPublisher with MockPkarrRelay as in existing tests ...
    let token_sig = [0x44u8; 64];
    pubr.register_friend_token(&token_sig, Arc::new(|| b"routing".to_vec())).await;
    assert!(publisher.active_handles().await.contains(&format!("friend:{}", hex::encode(token_sig))));
    pubr.unregister_friend_token(&token_sig).await;
    assert!(!publisher.active_handles().await.contains(&format!("friend:{}", hex::encode(token_sig))));
}
```

- [ ] **Step 2: Run, watch fail.**

- [ ] **Step 3: Implement** by factoring the existing `register_invite` body to share a private helper keyed on a `handle` string + `ikm`. Friend tokens reuse `PkarrCase::Invite` keying (Phase 1; switches to `PkarrCase::Friend` in Phase 1b) but a distinct `friend:` handle namespace so they don't collide with community invites:

```rust
pub async fn register_friend_token(
    &self,
    token_sig: &[u8; 64],
    routing_blob_builder: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
) {
    let handle = format!("friend:{}", hex::encode(token_sig));
    let sig = *token_sig;
    let key_builder: EphemeralKeyBuilder = Arc::new(move |at_ms| {
        let epoch_id = current_epoch_id(at_ms);
        derive_ephemeral_key(PkarrCase::Invite, &sig, &epoch_id.to_be_bytes())
    });
    let id_sk = self.identity_signing_key.clone();
    let id_pub = self.identity_pub;
    let blob = routing_blob_builder;
    let builder: RecordBuilder = Arc::new(move |at_ms| {
        PkarrRoutingRecord::sign_new(blob(), id_pub, at_ms, &id_sk).expect("sign")
    });
    self.publisher.register(handle, key_builder, builder).await;
}

pub async fn unregister_friend_token(&self, token_sig: &[u8; 64]) {
    self.publisher.unregister(&format!("friend:{}", hex::encode(token_sig))).await;
}
```

- [ ] **Step 4: Run, watch pass.**
- [ ] **Step 5: clippy + fmt.**
- [ ] **Step 6: Commit** — `feat(zeb-370): Case-A pkarr publish/unpublish for friend tokens`

---

## Task 7: `harmony/friend/v1` wire types + packet codec

**Files:**
- Create: `src-tauri/src/iroh_friend_acceptor.rs` (add `mod iroh_friend_acceptor;`)
- Template: `iroh_invite_acceptor.rs` framing (LE u32 length prefix + CBOR body; `HANDSHAKE_MAX_PACKET_LEN` = 256 KiB).

- [ ] **Step 1: Write the failing test:**

```rust
#[test]
fn friend_packets_round_trip() {
    let req = FriendLinkRequest {
        from_owner_pub: [5u8; 64], from_addr: OwnerAddr([5u8; 16]),
        display: Some("carol".into()), token_sig: [6u8; 64],
        enrollment: /* sample EnrollmentCert */, sig: [7u8; 64],
    };
    let bytes = encode_friend_request(&req).expect("encode");
    assert_eq!(decode_friend_request(&bytes).expect("decode"), req);

    let acc = FriendLinkAccepted { from_owner_pub: [8u8; 64], display: None, enrollment: /* .. */, sig: [9u8; 64] };
    let b2 = encode_friend_accepted(&acc).expect("encode");
    assert_eq!(decode_friend_accepted(&b2).expect("decode"), acc);
}

#[test]
fn decode_rejects_oversized() {
    assert!(decode_friend_request(&vec![0u8; FRIEND_MAX_PACKET_LEN + 1]).is_err());
}
```

- [ ] **Step 2: Run, watch fail.**

- [ ] **Step 3: Implement** `FriendLinkRequest`/`FriendLinkAccepted` (derive Serialize/Deserialize; `[u8;64]` bstr; `EnrollmentCert` from `community_membership`) and the CBOR encode/decode helpers (`ciborium::into_writer`/`from_reader`) with a `FRIEND_MAX_PACKET_LEN` bound. The request `sig` is the sender's device-#2 Ed25519 signature over canonical bytes of `(from_owner_pub, from_addr, token_sig)`; provide `friend_request_sig_preimage(...)` + a verify helper. Likewise `FriendLinkAccepted.sig` over `(from_owner_pub, token_sig)`.

- [ ] **Step 4: Run, watch pass.**
- [ ] **Step 5: clippy + fmt.**
- [ ] **Step 6: Commit** — `feat(zeb-370): harmony/friend/v1 wire types + packet codec + sig preimages`

---

## Task 8: Friend handshake acceptor (inbound) + ALPN registration

**Files:**
- Modify: `src-tauri/src/iroh_endpoint.rs` (declare `HARMONY_FRIEND_V1` at `:47`; add to `.alpns(vec![...])` at `:89`)
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (add `IrohFriendHandshakeAcceptor`)
- Template: `IrohInviteHandshakeAcceptor::handle_invite_handshake_inbound` (`iroh_invite_acceptor.rs:266-434`).

- [ ] **Step 1: Write the failing test** — a unit test over the inbound handler logic given an in-memory `crdt_state` and a valid `FriendLinkRequest`: assert it (a) verifies the request sig + enrollment, (b) calls `apply_friend_update` writing the requester as an `Active`/`Token` friend, (c) produces a `FriendLinkAccepted` with a valid sig. Reject a request whose sig fails. (Factor the verify+apply into a pure `process_friend_request(crdt_state, hlc_tracker, self_owner, self_pub, device2_key, enrollment, req) -> Result<FriendLinkAccepted, FriendError>` so it's testable without a live QUIC stream — this mirrors how `community_invite::handle_unicast` is separable.)

- [ ] **Step 2: Run, watch fail.**

- [ ] **Step 3: Implement** `process_friend_request` (the testable core) and `IrohFriendHandshakeAcceptor` implementing `IrohHandshakeDispatcher` (the QUIC wrapper: `accept_bi` → read LE u32 + body → `decode_friend_request` → `process_friend_request` → `encode_friend_accepted` → write LE u32 + body → `finish()` → `conn.closed()`, all bounded by `config.io_deadline`). Add the ALPN const + registration. The acceptor holds `crdt_state`, `hlc_tracker`, `self_owner`, `self_owner_pub`, `device2_signing_key`, `self_enrollment`, and an `AppHandle` to emit `friend-list-changed`. On success it calls the SyncEngine `notify_dirty()` so the new friend republishes to the owner's other devices.

- [ ] **Step 4: Run, watch pass.**
- [ ] **Step 5: clippy + fmt.**
- [ ] **Step 6: Commit** — `feat(zeb-370): friend handshake acceptor + HARMONY_FRIEND_V1 ALPN`

---

## Task 9: Dispatch multiplexing + NodeState wiring

**Files:**
- Modify: `src-tauri/src/lib.rs` (NodeState field + construction in `start_node`; dispatch wiring)
- Modify: the accept-loop dispatcher so non-zenoh ALPNs route by `conn.alpn()`: `HARMONY_HANDSHAKE_V1` → invite acceptor, `HARMONY_FRIEND_V1` → friend acceptor.
- Template: how `IrohInviteHandshakeAcceptor` is constructed and installed in `start_node` (search `IrohInviteHandshakeAcceptor` / the dispatcher install site).

- [ ] **Step 1: Write the failing test** — a unit test of the dispatch router: given a mock connection reporting ALPN `harmony/friend/v1`, the router invokes the friend acceptor; given `harmony/handshake/v1`, the invite acceptor. (If the existing dispatcher takes a single `Arc<dyn IrohHandshakeDispatcher>`, introduce a small `MultiplexDispatcher { invite, friend }` that reads `conn.alpn()` and delegates; unit-test its routing with stub dispatchers recording which was called.)

- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement** the `MultiplexDispatcher`, install both acceptors in `start_node`, add `pkarr`/acceptor handles to `NodeState` as needed, clear them in `stop_inner`.
- [ ] **Step 4: Run, watch pass.**
- [ ] **Step 5: clippy + fmt.**
- [ ] **Step 6: Commit** — `feat(zeb-370): multiplex iroh dispatch for friend + invite ALPNs`

---

## Task 10: Outbound connect + redeem (`connectivity_link_friend_iroh_inner`)

**Files:**
- Modify: `src-tauri/src/lib.rs` (new `connectivity_link_friend_iroh_inner`)
- Create: `src-tauri/tests/friend_token_roundtrip_integration.rs`
- Template: `connectivity_redeem_invite_iroh_inner` (`lib.rs:31804`) for resolve→connect→exchange.

- [ ] **Step 1: Write the failing integration test** — two in-process nodes (mirror the invite redeem integration test harness): node A `mint_friend_token` + `register_friend_token` (publish Case-A); node B decodes the URL, resolves A's reachability via pkarr (handle `friend:{sig}`), connects `HARMONY_FRIEND_V1`, exchanges request/accept. Assert: B's owner-state has `FriendEntry{A, Active, Token}` and A's has `FriendEntry{B, Active, Token}`; A unregistered the Case-A handle on consume. Wrap with the ZEB-347 generous timeout (`tokio::time::timeout(Duration::from_secs(45), ...)`).

- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement** `connectivity_link_friend_iroh_inner`: `decode_friend_token_url` → resolve inviter routing via the pkarr resolver under `friend:{token.sig}` → synthesize `EndpointAddr` → `connect(addr, HARMONY_FRIEND_V1)` → `open_bi` → write LE-framed `FriendLinkRequest` (signed) → read LE-framed `FriendLinkAccepted` (verify sig + enrollment against `inviter_owner_pub`) → `apply_friend_update` writing the inviter as friend → `notify_dirty()`. On the inviter side the acceptor (Task 8) already wrote B and called `unregister_friend_token`.
- [ ] **Step 4: Run, watch pass** (serial if it binds UDP; heed the port-contention flake — run `-j 1` to confirm if it flakes under load).
- [ ] **Step 5: clippy + fmt.**
- [ ] **Step 6: Commit** — `feat(zeb-370): outbound friend-link connect + two-node redeem integration test`

---

## Task 11: IPC commands

**Files:**
- Modify: `src-tauri/src/lib.rs` (4 commands + `generate_handler!` registration)
- Template: `generate_invite` (`:14814`), `redeem_invite` (`:17727`), `connectivity_discover_identity` (`:32605`) for the lock-snapshot-drop pattern.

- [ ] **Step 1: Write the failing test** — where command bodies have testable cores, test those (e.g. a `list_friends_inner(crdt_state) -> Vec<FriendDto>` that maps non-`Revoked` entries to DTOs; an `unfriend_inner` that writes a `Revoked` tombstone with a fresh HLC). Assert `list_friends_inner` hides revoked and surfaces active; `unfriend_inner` tombstones.

- [ ] **Step 2: Run, watch fail.**
- [ ] **Step 3: Implement** the four `#[tauri::command(rename_all = "snake_case")]` fns (thin wrappers over inner cores using the lock-snapshot-drop pattern), a `FriendDto` (addr hex, display, status, established_via, referrable), and `FriendLinkResultDto`. Register all four in `tauri::generate_handler![...]`. `generate_friend_token` calls `mint_friend_token` + `register_friend_token`; `redeem_friend_token` calls `connectivity_link_friend_iroh_inner`; both emit `friend-list-changed`.
- [ ] **Step 4: Run, watch pass.**
- [ ] **Step 5: clippy + fmt.**
- [ ] **Step 6: Commit** — `feat(zeb-370): friend IPCs (generate/redeem token, list, unfriend)`

---

## Task 12: Frontend FriendService + minimal UI

**Files:**
- Create: `src/lib/friend-service.ts`, `src/lib/friend-service.test.ts`, `src/lib/components/FriendsPanel.svelte`
- Modify: the service-registry / nav host (mirror `community-service.ts` wiring) + add the panel entry.
- Template: `src/lib/community-service.ts`, `src/lib/connectivity-adapter.ts`, `src/lib/nav-service.ts`.

- [ ] **Step 1: Write the failing vitest** (`friend-service.test.ts`): a mock `TauriAdapter` records invokes; assert `generateFriendToken()` → `invoke('generate_friend_token', { expiresAt: null })` returns the URL; `redeemFriendToken(url)` → `invoke('redeem_friend_token', { url })`; `listFriends()` maps the DTO array; `unfriend(addr)` → `invoke('unfriend', { peerAddr: addr })`; `friend-list-changed` event triggers `onFriendsChanged`.
- [ ] **Step 2: Run, watch fail** — `npx vitest run friend-service` (repo root).
- [ ] **Step 3: Implement** `FriendService` (per the blueprint in the explorer report: `connectAdapter`/`invoke`/`destroy` + the four methods + the event listener) and a minimal `FriendsPanel.svelte` (list active friends, "Generate friend link" → show URL to copy, "Add friend" → paste URL → redeem, per-row "Unfriend"). Wire the service into the registry and surface the panel in nav.
- [ ] **Step 4: Run, watch pass** — `npx vitest run friend-service` + `npx tsc --noEmit` (repo root).
- [ ] **Step 5: Commit** — `feat(zeb-370): FriendService + FriendsPanel UI`

---

## Task 13: Wire-format pins + full gate

**Files:**
- Create: `src-tauri/tests/wire_format_zeb370_fixtures.rs`
- Template: `tests/wire_format_zeb250_fixtures.rs` (hex const + `FILL_AFTER` regen + `ciborium::Value` structural asserts).

- [ ] **Step 1: Write pinning tests** for `FriendEntry`, `FriendGraph`, `FriendTokenPayload`, `FriendLinkRequest`, `FriendLinkAccepted` — deterministic all-constant-byte values, `canonical_cbor_encode` → hex compared to a pinned const (seed with `FILL_AFTER`), plus structural `ciborium::Value` map-key assertions.
- [ ] **Step 2: Run once to regenerate** the hex consts (tests panic and print actual), paste them in, re-run → PASS.
- [ ] **Step 3: Run the FULL gate** from `src-tauri/`:
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  - Frontend (repo root): `npx tsc --noEmit` && `npx vitest run`
  - Distinguish any failures in the known content/folder/move/rename/vine UDP-port-4242 flake by re-running representatives with `-j 1`.
- [ ] **Step 4: Commit** — `test(zeb-370): wire-format pins for friend types; full gate green`

---

## Self-Review (run before execution)

1. **Spec coverage:** Friend Graph CRDT (Tasks 1-3) ✓; token codec + mint + Case-A publish (4-6) ✓; `harmony/friend/v1` handshake in+out (7-10) ✓; IPC + UI (11-12) ✓; pins + gate (13) ✓. Deferred items (pairwise secret, Case-D, introductions, policy enforcement) are explicitly out-of-scope per the spec's phasing — no Phase-1 task should implement them.
2. **Type consistency:** `FriendEntry`/`FriendGraph`/`FriendStatus`/`FriendOrigin`/`PeerIntroPolicy` (Task 1) are used unchanged in 2/3/8/10/11; `FriendTokenPayload` (4) consumed by 5/6/10/11; `FriendLinkRequest`/`FriendLinkAccepted` (7) by 8/10. `apply_friend_update` (2) used by 3/8/10/11. Confirm `ApplyOutcome` variant names against `owner_state_crdt.rs` in Task 2.
3. **Guardrails honored:** owner-state sub-CRDT mirrors `OwnerDeviceCache` exactly; canonical-key-length rule respected (single-char keys in `FriendEntry`); `--locked`/`--all-targets`/`--features test-fixtures` in every gate; ZEB-347 timeout floors on async/integration tests; device-#2 Ed25519 for all signing (no birational mixing — the pairwise secret that would risk the ZEB-326 trap is deferred entirely).
4. **Open confirmations for implementers:** exact `ApplyOutcome` variants; whether a shared `[u8;64]` bstr serde helper already exists (reuse it); the exact dispatcher install site for the multiplexer; whether `serde_bytes` is already a dependency.
