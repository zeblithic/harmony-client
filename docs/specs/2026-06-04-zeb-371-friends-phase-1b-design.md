# Friends Phase 1b — Case-D Cross-WAN Rendezvous (stored per-friendship secret) + Path A — Design

**Date:** 2026-06-04
**Ticket:** ZEB-371 (parent ZEB-321). Builds on ZEB-370 (Phase 1, PR #186, merged).
**Status:** Approved key model + consent model; pending spec review → implementation plan.
**Cross-repo prereq:** ZEB-372 (real birational X25519) is **out of scope** here; Phase 1b deliberately does not depend on it (that's the whole point of the stored-secret approach).

---

## 1. Problem & intent

Phase 1 (ZEB-370) shipped a durable, mutual Friend Graph and a one-shot **friend-token** first-contact (`harmony://friend/<token>` → `harmony/friend/v1` handshake → mutual `FriendEntry{Active,Token}`). What it deliberately left out is **durable cross-WAN rendezvous**: once the one-shot token is consumed, two friends have **no way to re-find each other** after either's address changes. Phase 1 friends are only reachable for the lifetime of a single token redemption.

Phase 1b adds the connectivity substrate that makes the friend graph useful across WAN and over time:

1. A **per-friendship rendezvous secret**, established once at link time and stored, so each friend can publish/resolve the other's live reachability privately.
2. **Case-D**: a friend-scoped pkarr DHT slot derived from that secret — discoverability scoped to *this one friend*, never global.
3. **Friend-scoped reconnection**: republish/re-resolve on the existing reachability cadence so address changes self-heal.
4. **Path A (mutual-key)**: form a friend link when you already hold someone's owner key (no token), with a hybrid consent model.

This is the prerequisite for Phase 2 (introductions): the introduction broker requires friends to hold **live, durable links**, which is exactly what Case-D provides. Path A's non-token accept path is also the path Phase 2's Path C (introductions) reuses.

### Goals

- Establish + store a 32-byte per-friendship secret via **ephemeral X25519 ECDH** inside the existing handshake, surviving Reticulum-identity rotation and working across all of an owner's devices.
- `PkarrCase::Friend` (harmony-core) + a `PkarrFriendPublisher`/resolver wired into the reachability cadence.
- Path A mutual-key first-contact with consent = **auto-accept owners I already hold, prompt for new owners, gated by a per-user toggle**.

### Non-goals (this phase)

- Phase 2 introductions (`ReferralCatalog`, broker, `PeerIntroPolicy` *enforcement*). The policy *type* already ships (Phase 1).
- General all-peers liveness/rebinding (ZEB-321 Phase 3 in full) — only the friend-scoped slice.
- Re-keying / forward secrecy of the stored secret (deferred; see §6.4).
- The harmony-core birational X25519 (ZEB-372) — independent; would only *optionally* let Case-D re-root on identity-derived secrets later.

---

## 2. Decisions carried in (already settled)

- **Key model = stored per-friendship secret (option C; Jake, 2026-06-04).** The original "owner X25519 ECDH" is not viable: master/device `PubKeyBundle.x25519_pub` are zeroed stubs (`harmony-owner/mint.rs:53`), and the only live per-owner X25519 (Reticulum) is per-device, unbacked, and keyed on a different identity than `owner_id`. There is no single stable per-owner X25519 all of an owner's devices can recompute. So we establish a shared secret **once** (ephemeral ECDH) and **replicate the result** through owner-state sync.
- **Consent for non-token requests = Both (Jake, 2026-06-04).** Auto-accept a request from an `owner_id` already in my graph (`Active`/`Pending`) or a community co-member; surface an interactive prompt for genuinely-new owners; a per-user toggle gates the auto behavior.

---

## 3. Identity & secret model

Friends remain keyed on the master **`owner_id`** and authenticated by **device-#2 signature + `EnrollmentCert`** (unchanged from Phase 1, spec §3). Phase 1b adds an orthogonal **rendezvous secret** layer; it does not touch the identity/auth model.

### 3.1 Establishment (ephemeral ECDH in the handshake)

Both `FriendLinkRequest` and `FriendLinkAccepted` carry a fresh, single-use **ephemeral X25519 public key**. Each side generates an ephemeral keypair per handshake, sends the public half, and computes:

```
shared = X25519(my_eph_sk, their_eph_pk)            // 32 bytes, identical both sides
friendship_secret = HKDF-SHA256(
    salt = b"harmony.friend.v1.rendezvous",
    ikm  = shared,
    info = canonical(min(owner_a,owner_b) ‖ max(owner_a,owner_b)),   // sorted owner_ids
) -> 32 bytes
```

- ECDH is symmetric, so requester and accepter derive **the same** `friendship_secret`.
- The `info` binds the secret to the two **authenticated owner identities** (sorted so both compute it identically), defeating identity-confusion/unknown-key-share.
- Ephemeral keys are zeroized after derivation; only the derived secret is retained.

### 3.2 Authenticating the ephemeral key (MITM defense — load-bearing)

The ephemeral X25519 public key is **signed into the device-#2 handshake preimage**, so a relayer/MITM (a malicious introducer in Phase 2, or a network attacker) cannot swap ephemeral keys to sit in the middle of the ECDH. The Phase-1 preimage grows to include the ephemeral key and an optional token:

```rust
// "hfr1" request / "hfa1" accept domain tags, as in Phase 1.
fn friend_request_sig_preimage(from_addr: OwnerAddr, token_sig: Option<&[u8;64]>, eph_x25519_pub: &[u8;32]) -> Vec<u8>;
fn friend_accept_sig_preimage (from_addr: OwnerAddr, token_sig: Option<&[u8;64]>, eph_x25519_pub: &[u8;32]) -> Vec<u8>;
```

`token_sig` is now `Option` (absent for Path A). The preimage encodes presence explicitly (e.g. a CBOR `Option`) so a `None` and a zero-filled `Some` are distinct. The existing four verification steps (`verify_enrolled_device`) are unchanged; only the signed message grows.

### 3.3 Storage (KeyTree-sealed, in `FriendEntry`)

On-disk owner-state is **plaintext CBOR** (`owner_state_persist.rs`), so the secret is stored as an encrypted blob, not raw:

```rust
// new FriendEntry field (wire key "k"):
//   sealed_secret: Option<Vec<u8>>
//   = AEAD-seal( key = KeyTree.friend_secret_subkey(),
//                aad = friend owner_id,
//                plaintext = friendship_secret )
```

- `KeyTree::derive(master_seed)` is deterministic per owner, so **every device** reconstructs the same sealing key and can open the blob (owner-state sync carries the sealed bytes; each device decrypts locally). Reuses the AEAD already in `owner_state_crypto.rs` (the `encrypt_root_publish` primitive family); a new sub-key selector keeps friend-secret sealing domain-separated from root-publish sealing.
- `None` for legacy Phase-1 entries (no secret yet) and `Pending` entries.
- **Cleared on `status → Revoked`** (set back to `None` with the tombstone) so an unfriend forgets the rendezvous secret.

### 3.4 Migration / backfill

Pre-release (no deployed friendships; manual smoke test is the validation gate), so **no on-the-wire migration is required**. The `sealed_secret` field is additive (`skip_serializing_if = Option::is_none`, `default`), so an old entry decodes as `None`. Any Phase-1 test friendships simply re-link to gain a secret. A Phase-1-era `Active` friend with `sealed_secret: None` is treated as "rendezvous not yet established" — it is not Case-D-published, and the next successful handshake (re-link, or a Path-A reconnect) establishes one.

---

## 4. harmony-core: `PkarrCase::Friend` (Phase 0)

A one-variant additive change to `harmony-pkarr` (`crates/harmony-pkarr/src/derive.rs`), mirroring the existing Invite/Identity/Community variants:

```rust
pub enum PkarrCase { Invite, Identity, Community, Friend }

impl PkarrCase {
    pub fn salt(self) -> &'static [u8] {
        match self {
            // ...existing three...
            Self::Friend => b"harmony.pkarr.v1.friend",
        }
    }
}
```

- Add a **pinned reference vector** test (`reference_vector_case_friend`) and extend `different_cases_produce_different_keys` to cover `Friend`. The pin is load-bearing: drift makes every published Case-D record irretrievable without a v2 migration (same contract as the other three vectors).
- Then **re-pin** the `harmony-pkarr` git `rev` in `harmony-client/src-tauri/Cargo.toml` (both the base dependency line and the `test-fixtures` line — they must match).

`ikm` for Case-D is the 32-byte `friendship_secret`; `info` is direction-specific (§6.1).

---

## 5. Handshake wire-format changes

Additive to the Phase-1 `harmony/friend/v1` protocol (still `[u32 LE len][CBOR body]`, `FRIEND_MAX_PACKET_LEN`, strict-decode/trailing-bytes rejection, capped display). Pre-release ⇒ these are **required v1b fields** (clean break; no compat shims):

```rust
struct FriendLinkRequest {
    from_addr: OwnerAddr,
    display: Option<String>,                 // capped at ingress (unchanged)
    token_sig: Option<[u8; 64]>,             // CHANGED: Option (None = Path A)
    eph_x25519_pub: [u8; 32],                // NEW: ephemeral ECDH public, bstr(32)
    enrollment: EnrollmentCert,
    sig: [u8; 64],                           // device-#2 over the new preimage (§3.2)
}

enum FriendLinkResponse {                    // NEW: replaces the bare Accepted reply
    Accepted(FriendLinkAccepted),            // link complete; both derive the secret
    Pending,                                 // request recorded; awaiting the user's accept (Path A new owner)
}

struct FriendLinkAccepted {
    from_addr: OwnerAddr,
    display: Option<String>,
    eph_x25519_pub: [u8; 32],                // NEW
    enrollment: EnrollmentCert,
    sig: [u8; 64],                           // device-#2 over the accept preimage
}
```

`FriendLinkResponse` is length-prefixed/strict-decoded like the existing types; `Pending` carries no fields (no secret, no entry written by the acceptor).

---

## 6. Case-D rendezvous

### 6.1 Keying — one writer per slot

```
publish (I make myself findable to friend F):
    key = derive_ephemeral_key(Friend, ikm=friendship_secret, info = epoch_be ‖ self_owner_id)
resolve (I look up friend F):
    key = derive_ephemeral_key(Friend, ikm=friendship_secret, info = epoch_be ‖ friend_owner_id)
```

Direction-specific `info` (the **publisher's** `owner_id`) gives each side its **own** ephemeral keypair → its own BEP44 mutable slot, one writer each. (A single shared slot would have both friends deriving the *same* keypair and clobbering each other's mutable record — rejected.) Both sides can compute both slots (they share the secret and both owner_ids), so publish-mine / resolve-theirs is unambiguous.

### 6.2 Payload (sealed, defense-in-depth)

The Case-D record value is the same opaque iroh routing blob Case-B publishes (`PkarrRoutingRecord` over the reachability announce), **additionally sealed** to a sub-key of `friendship_secret` (AEAD; epoch in the AAD). The slot pubkey is already secret (only the friend can derive it), but sealing the value means a DHT scraper who somehow learns the pubkey still can't read the routing data. The resolver opens it with the same sub-key.

### 6.3 Publisher + resolver

- `PkarrFriendPublisher` registers **one `PkarrPublisher` handle per `Active` friend** (handle `friend:{hex(friend_owner_id)}`), mirroring `PkarrIdentityPublisher::enable`. The key-builder re-derives per epoch from the stored secret; the record-builder seals the current routing blob. Friends with `sealed_secret: None` are skipped.
- Resolver derives the friend's slot key across the `epoch_tolerance_window` (prev/current/next) and queries in parallel (reuses `PkarrResolver`), then opens + parses the routing blob and hands the iroh `NodeAddr` to the connect path.
- Add a handle when a friend becomes `Active` with a secret; drop it on `Revoked`/secret-cleared.

### 6.4 Reconnection cadence

Hook Case-D publish/refresh into the existing `reachability_publisher` triggers — **startup, network-change (if-watch debounce), 60-min idle, force** — so an address change republishes all active friends' Case-D slots promptly. Reachability is resolved **on demand** at connect time (no new persisted `cached_reachability` field in 1b — resolve-on-connect keeps the CRDT lean). This is the friend-scoped slice of ZEB-321 Phase 3; it does not attempt the general all-peers rebinding protocol.

**Rotation posture (v1b):** the stored secret is stable for the life of the friendship; only the **epoch rotates the slot** (location-unlinkability over time). The secret guards rendezvous-slot *location*, not message *content* (iroh transport encryption + device-#2 auth already protect content). Re-keying / forward secrecy is a deferred follow-up.

---

## 7. Path A (mutual-key) + consent

Path A forms a link when you already hold a friend's owner key, with **no token**. Consent (Jake's "Both"):

### 7.1 Inbound decision tree (acceptor)

After authenticating the request (cert + sig + the now-eph-bound preimage — always, proves owner control):

1. **`token_sig = Some`** → existing Phase-1 token gate (`try_consume_friend_token`, atomic one-shot, fail-closed). On pass → accept inline as `Active`/`Token`, derive + seal + store the secret, reply `Accepted`.
2. **`token_sig = None`** (Path A):
   a. `known = (owner ∈ FriendGraph with status Active|Pending) || community-co-member(owner)`.
   b. If `known && friend_auto_accept_known` → accept inline as `Active`/`MutualKey`, derive + store secret, reply `Accepted`.
   c. Else (new owner, or auto-accept off):
      - If a prior **Accept decision** is recorded for this owner → accept inline (as 2b) and clear the decision.
      - Else record a **pending-inbound request** (owner, display, learned_at) **iff not already present**, emit `friend-request-received`, reply `Pending`. Write nothing `Active`. (A `Revoked` owner is treated as new here — re-friending the deliberately-removed requires an explicit accept.)

`known`/auto never bypass identity auth — they only decide whether to *prompt*. A `Pending` reply never carries a secret or a friend entry.

### 7.2 Requester side

On `Pending`, write a local `FriendEntry{status: Pending, established_via: MutualKey, sealed_secret: None}` and surface "request sent." The requester **re-attempts the dial** on a small bounded cadence (and on a manual "retry"); completion happens on the A→B dial where B's recorded decision is `Accept` (or known+auto) — **B never has to dial A back**, sidestepping the cold-start reach-back problem. Each re-attempt sends a *fresh* ephemeral key; the secret is derived from whichever pair completes.

### 7.3 Accept/decline + policy

- IPCs `accept_friend_request(owner_id)` / `decline_friend_request(owner_id)`: Accept records the decision (completed on the requester's next dial) and emits `friend-list-changed`; Decline drops the pending-inbound and the decision.
- `list_pending_friend_requests()` projects the inbound queue for the prompt UI.
- New per-user setting `friend_auto_accept_known: bool` (**default true**), persisted alongside the existing friend/pkarr settings; surfaced as a toggle.
- Path-A initiation IPC `add_friend_by_key(owner_pub | owner_id, reachability hint)` → resolve (Case-B if discoverable, else fail with a clear "not reachable; use a token") → dial `harmony/friend/v1` with `token_sig: None`.

---

## 8. Data-model changes (summary)

- `FriendEntry`: **+`sealed_secret: Option<Vec<u8>>`** (wire key `"k"`, `skip_serializing_if`/`default`). Cleared on `Revoke`. Everything else unchanged.
- New transient (non-CRDT) **pending-inbound store** for Path-A prompts (owner, display, learned_at, decision) — process-local, like the live-token map; not synced.
- New setting `friend_auto_accept_known: bool` (default true).
- No change to `owner_id` derivation, `apply_friend_update`'s key↔master invariant, or the LWW/tombstone semantics.

---

## 9. Reconnection & cold-start (limits, unchanged from Phase-1 spec §8)

- **Reconnection** of an `Active` friend with a secret: self-heals on the reachability cadence via Case-D. Works even if you were offline, as long as the friend republished within TTL.
- **Path A genuine first contact** (owner you've never linked, no token) still requires the target to be reachable *some other way* at first contact — Case-B global discoverability (opt-in) — because the target isn't publishing Case-D for you yet. Otherwise: use a token. This narrowness is acceptable for alpha and is the reason the token path exists.

---

## 10. Security & abuse posture

- **MITM defense:** the ephemeral key is device-#2-signed into the preimage (§3.2) → no key-swap on the ECDH; the resulting link is authenticated end-to-end even when reached via an introducer (Phase 2).
- **Per-relationship compartmentalization:** each `friendship_secret` is independent; compromise of one reveals no other and never the owner key.
- **Secret at rest:** sealed with a KeyTree sub-key; the plaintext-CBOR on-disk state and the synced form never expose it. Cleared on unfriend.
- **Scoped discoverability:** Case-D slots are derived from a shared secret — peering never forces global Case-B on, and a slot is findable only by the one friend.
- **Fail-closed consent:** Path B keeps the atomic one-shot token gate; Path A never auto-accepts an unknown owner and never bypasses identity auth.
- **Revocation:** `Revoke` clears the secret, drops the Case-D handle (stops republish), and tombstones the entry (LWW; not resurrectable by a stale `Active`).

---

## 11. Phasing (PR = ZEB-371)

- **Phase 0 — harmony-core:** `PkarrCase::Friend` + reference vector + distinctness test; re-pin in `Cargo.toml`.
- **Phase 1 — secret establishment:** eph-X25519 fields on the wire types; `token_sig: Option`; extended sig preimages; `friendship_secret` HKDF; thread the secret through `process_friend_request` + the redeem path; `FriendEntry.sealed_secret` (+ KeyTree seal/open helper) + clear-on-revoke.
- **Phase 2 — Case-D crypto + publisher/resolver:** direction-specific key derivation + payload seal/open (+ reference-vector tests); `PkarrFriendPublisher` (per-active-friend register) + resolver.
- **Phase 3 — cadence + reconnection:** wire publish/refresh into `reachability_publisher` triggers; add/drop handles on Active/Revoke; resolve-on-connect for live reconnection.
- **Phase 4 — Path A + consent:** inbound decision tree; pending-inbound store; `accept`/`decline`/`list_pending` IPCs; `friend_auto_accept_known` setting; requester `Pending`/retry; `add_friend_by_key`.
- **Phase 5 — frontend + pins:** friend-request prompt/inbox UI + policy toggle + "add by key" UI; wire-format pin fixtures; full gate (nextest + clippy + fmt + frontend).

Each phase is TDD, committed incrementally.

---

## 12. Testing strategy (TDD)

**Unit**
- `PkarrCase::Friend` reference vector pinned; distinct from A/B/C for identical inputs.
- ECDH→HKDF: both sides derive an identical `friendship_secret`; sorted-owner `info` is order-independent; a swapped/unsigned ephemeral key fails sig verification (MITM defense).
- KeyTree seal/open round-trip; wrong sub-key fails; sealed bytes differ from plaintext; revoke clears `sealed_secret`.
- Case-D keying: publish-mine ≠ resolve-mine direction slots; payload seal/open round-trip, tampered value rejected.
- Path A decision tree truth table: token / known+auto / known+auto-off / unknown-new / unknown-with-prior-accept / revoked — each writes (or doesn't) the right entry and returns Accepted/Pending correctly; unknown never auto-links; identity auth always enforced.
- `FriendEntry` serde: `sealed_secret` round-trips; absent decodes as `None` (back-compat); oversized rejected.

**Integration** (reuse the iroh/pkarr/zenoh harness; heed the known UDP-4242 port-contention flake — generous timeouts, avoid parallel port reuse)
- Two-node token redeem now also establishes + stores a matching secret on both sides.
- Two-node: link, then simulate an address change and confirm Case-D re-resolve restores reachability (reconnection).
- Two-node Path A: known-owner auto-accept completes inline; new-owner returns `Pending`, then `accept_friend_request` + requester retry completes the mutual link with `established_via: MutualKey`.

---

## 13. Open questions (resolve in planning; not blockers)

1. **Community-co-member as "known"** — include community co-membership in the auto-accept `known` set in 1b, or start with friend-graph-only (Active/Pending) and add community in a follow-up? (Leaning: friend-graph-only first; community co-member is a larger state query.)
2. **Pending-inbound durability** — keep the Path-A pending queue process-local (lost on restart; requester re-dials) for 1b, or persist it? (Leaning: process-local for 1b; requester retry closes the loop.)
3. **Retry cadence bounds** — exact backoff/cap for the requester's Path-A re-dial. (Decide in the plan.)

---

## 14. References

- Phase-1 design: `docs/specs/2026-06-03-friends-peer-introductions-design.md` (§3 identity/auth, §8 cold-start, §11 carried-over open questions).
- ZEB-371 / ZEB-372 tickets; key-model rationale in `project_friends_peer_graph` memory.
- harmony-pkarr: `crates/harmony-pkarr/src/{derive,epoch,publisher,resolver}.rs` (Case derivation, epoch window, register/republish).
- Case-B template: `src-tauri/src/pkarr_identity_publisher.rs`.
- Handshake to extend: `src-tauri/src/iroh_friend_acceptor.rs` (wire types, preimages, `process_friend_request`, acceptor); redeem path `connectivity_link_friend_iroh_inner` in `lib.rs`.
- Reachability cadence: `src-tauri/src/reachability_publisher.rs`.
- At-rest crypto: `src-tauri/src/owner_state_crypto.rs` (`KeyTree`, `encrypt_root_publish` family); persistence `owner_state_persist.rs`.
- Friend CRDT: `src-tauri/src/friend_graph.rs`; apply/merge `owner_state_crdt.rs`.
