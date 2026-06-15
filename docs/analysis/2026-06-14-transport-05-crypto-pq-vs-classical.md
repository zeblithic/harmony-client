# Crypto Inventory: Post-Quantum vs Classical (harmony core + harmony-client)

**Date:** 2026-06-14
**Scope:** `harmony` core crates (`/Users/zeblith/work/zeblithic/harmony/crates/`, excluding `.worktrees/`) pinned at rev `dddf192` and `harmony-client/src-tauri` (which builds against that exact rev).
**Status:** Read-only forensic analysis. No code changed.

---

## TL;DR

The codebase is **not** "all classical defaulting back from PQ." It is a **deliberate two-class split that has drifted into an accidental mix**:

- **Two parallel identity systems exist and are both fully implemented** (`Identity`/`PrivateIdentity` = X25519+Ed25519 classical; `PqIdentity`/`PqPrivateIdentity` = ML-KEM-768 + ML-DSA-65 PQ). The client mints **both** from the same 32-byte seed on every boot.
- **The canonical, on-the-wire node address is the CLASSICAL one** (`SHA256(X25519_pub‖Ed25519_pub)[:16]`). The PQ address hash is a secondary identifier.
- **PQ is real in exactly two places**: the **friend↔friend CRDT replication tunnel** (`harmony-tunnel`, fully ML-KEM + ML-DSA, no classical keys at all) and the **KEL key-event-log** (ML-DSA/ML-KEM). Discovery/profile **verification** is suite-aware and *can* check ML-DSA.
- **PQ is decorative ("carried, not used") almost everywhere else**: friend handshakes, DMs, voice, owner/device enrollment, and pkarr all run on X25519/Ed25519. PQ keys are minted, transmitted in records, and stored on `Contact`s — but the actual seal/sign/ECDH in those paths is classical.
- **The single clearest "defaulting back to Curve25519":** every direct message is sealed with **X25519 ECDH + ChaCha20-Poly1305** and signed **Ed25519** (`dm_signing.rs`, `dm_crypto.rs`), even though both peers minted ML-KEM/ML-DSA keys and advertised them in the handshake. The PQ keys ride along as routing hints for a tunnel that only carries CRDT replication, not the DM body.

---

## (a) Crypto-use inventory

Legend: **PQ** = post-quantum (ML-KEM-768 / ML-DSA-65); **C** = classical (X25519 / Ed25519 / Ristretto255). **Used** = an actual `encapsulate`/`sign`/`diffie_hellman`/`verify` is performed on that path. **Carried** = key material is minted/stored/transmitted but no crypto op consumes it on that path.

### Primitives (`harmony-crypto`)

| Purpose | Algo | PQ/C | file:line | Notes |
|---|---|---|---|---|
| KEM primitive | ML-KEM-768 (FIPS 203) | PQ | `harmony-crypto/src/ml_kem.rs` | `ml-kem` crate |
| Signature primitive | ML-DSA-65 (FIPS 204) | PQ | `harmony-crypto/src/ml_dsa.rs` | `ml-dsa` crate |
| Hybrid KEM (X25519 ‖ ML-KEM, HKDF-combined) | X25519+ML-KEM | PQ (hybrid) | `harmony-crypto/src/hybrid_kem.rs:23-87` | **Implemented but unused** outside tests/exports |
| ECDH primitive | X25519 | C | `x25519-dalek` dep | |
| AEAD | ChaCha20-Poly1305 | C (sym) | `harmony-crypto/src/aead.rs:48-84` | seeds from PQ or classical secret depending on path |
| Legacy AEAD | AES-256-CBC + HMAC (Fernet) | C (sym) | `harmony-crypto/src/fernet.rs` | classical `Identity::encrypt` |

### Identity (`harmony-identity`)

| Purpose | Algo | PQ/C | file:line | Used/Carried |
|---|---|---|---|---|
| Classical identity = `X25519_pub(32)‖Ed25519_pub(32)`, addr=`SHA256(...)[:16]` | X25519+Ed25519 | C | `identity.rs:42-94` | **Used** (canonical address) |
| Classical sign / verify | Ed25519 | C | `identity.rs:268-270` / `96-106` | Used |
| Classical encrypt / decrypt | X25519 ECDH + Fernet | C | `identity.rs:117-143` / `275-301`; ECDH `346-348` | Used |
| PQ identity = `MLKEM_pub(1184)‖MLDSA_pub(1952)`, addr=`SHA256(...)[:16]` | ML-KEM+ML-DSA | PQ | `pq_identity.rs:39-98` | Carried (minted, address used as secondary id) |
| PQ sign / verify | ML-DSA-65 | PQ | `pq_identity.rs:266-269` / `100-104` | Used **only** by tunnel + KEL + memo |
| PQ encrypt / decrypt | ML-KEM-768 + ChaCha20 | PQ | `pq_identity.rs:109-147` / `327-369` | Used **only** by PQ tunnel + memo store |
| `CryptoSuite` enum: `Ed25519=0x00`, `MlDsa65=0x01`, `MlDsa65Rotatable=0x02` | — | — | `crypto_suite.rs:12-22` | Dispatch tag for verify; `is_post_quantum()` `45-47` |

### harmony-client identity wiring

| Purpose | Algo | PQ/C | file:line | Used/Carried |
|---|---|---|---|---|
| `NodeIdentity` holds BOTH `pq` and `ed25519`, both `from_seed(seed)` | both | both | `identity.rs:59-89` | both minted every boot |
| **Canonical node address** (`node_addr`, mail owner, Reticulum) | Ed25519/X25519 | **C** | `lib.rs:3089-3090` | **Used** |
| `local_pq_identity_hash` (discovery query keyexpr, discovery-token validation) | ML-KEM+ML-DSA | PQ | `lib.rs:3092-3093` | secondary id |
| PQ pubkeys captured to stash on `NodeState` for friend handshake | ML-DSA/ML-KEM | PQ | `lib.rs:3094-3101` | **Carried** (handshake hint) |
| 64-byte combined identity pub for DM bootstrap / countersign | X25519‖Ed25519 | C | `lib.rs:3111` | Used |
| iroh transport secret key (independent random 32B) | Ed25519 (iroh proto) | C | `identity.rs:430-431` (`iroh_secret_key`) | Used (QUIC transport id) |
| at-rest envelope | Argon2id + XChaCha20-Poly1305 | C (sym) | `identity.rs:239-290` | Used (storage, not network) |

### Transport / handshakes

| Purpose | Algo | PQ/C | file:line | Used/Carried |
|---|---|---|---|---|
| **PQ tunnel handshake — sign** | ML-DSA-65 | **PQ Used** | `harmony-tunnel/src/handshake.rs:59` (sign) / `130` (verify) | **Used** |
| **PQ tunnel handshake — KEM** | ML-KEM-768 | **PQ Used** | `handshake.rs:45-46` (encap) / `201-202` (decap) | **Used** |
| PQ tunnel session traffic cipher | ChaCha20-Poly1305 keyed by ML-KEM secret via HKDF | C cipher / PQ secret | `harmony-tunnel/src/frame.rs:97-127`; keys `handshake.rs:306-329` | Used |
| PQ tunnel session identities | `PqPrivateIdentity`/`PqIdentity` | PQ | `harmony-node/src/tunnel_task.rs:103-114, 255` | Used |
| **What the PQ tunnel carries** | CRDT `ReplicationOp::Push`/`PullWithToken` | — | `harmony-node/src/event_loop.rs:1155-1199` | **Replication only — not DMs** |
| **Reticulum link handshake (primary transport)** | X25519 ECDH + Ed25519 sign | **C Used** | `harmony-reticulum/src/link.rs:259` (ECDH) / `246-251` (Ed25519 verify) | **Used (default path)** |
| Reticulum announce | classical `PrivateIdentity` sign | C | `harmony-reticulum/src/announce.rs:107` | Used |
| IFAC interface auth | classical `PrivateIdentity` | C | `harmony-reticulum/src/ifac.rs:30,72` | Used |

### Friend handshake / DM / voice (harmony-client)

| Purpose | Algo | PQ/C | file:line | Used/Carried |
|---|---|---|---|---|
| Friend rendezvous secret | X25519 ECDH + HKDF | **C Used** | `friend_rendezvous.rs:24-54` (ephemeral+ECDH), `90-112` (seal) | Used |
| Friend handshake carries device bundle = `X25519‖Ed25519` (64B) | C | C | `dm_tunnel_contact.rs:22-26` | Used (sig binding) |
| Friend handshake carries peer ML-DSA(1952)+ML-KEM(1184) | PQ | PQ | `dm_tunnel_contact.rs:41-47` (`RegisterTunnelPeer`), `52-79` (`build_tunnel_contact`) | **Carried** (tunnel hint) |
| **DM body seal** | ChaCha20-Poly1305 (sym content key) | **C Used** | `dm_crypto.rs:57-79` / `84-118` | Used |
| **DM epoch-key seal to recipient** | X25519 ECDH + ChaCha20 | **C Used** | `dm_signing.rs:50-113` (`seal_to_owner`, `.diffie_hellman` ~:88) | Used |
| **DM packet sign** | Ed25519 | **C Used** | `dm_signing.rs:~298-301`; envelope appends 64B Ed25519 sig `dm_envelope.rs:165-199` | Used |
| Voice frame seal | ChaCha20-Poly1305 (ChannelKey, sym) | C | `voice_crypto.rs:119-156`, `187-228` | Used |
| Voice frame sign | Ed25519 | C | `voice_crypto.rs:236-260` | Used |
| Owner-state at-rest | ChaCha20-Poly1305 (sym) | C | `owner_state_crypto.rs:8+` | Used |

### Owner / device enrollment / KEL / discovery / pkarr (core)

| Purpose | Algo | PQ/C | file:line | Used/Carried |
|---|---|---|---|---|
| Owner master identity | Ed25519 | C | `harmony-owner/src/pubkey_bundle.rs:41-50` (`classical_only`, `post_quantum: None`) | Used |
| `PubKeyBundle` *can* carry `Option<PqKeys>` (ml_dsa_verify, ml_kem_pub) | PQ | PQ | `pubkey_bundle.rs:4-23` | **Carried-capable but always `None` in mint path** |
| Device enrollment cert sign/verify | Ed25519 | C | `certs/enrollment.rs:78` (sign) / `108-131` (verify) | Used |
| Quorum / master enrollment | Ed25519 | C | `lifecycle/enroll_quorum.rs:91-94`; `enroll_master.rs:49-56`; `state.rs:268-280` | Used |
| Revocation / reclamation / vouching / liveness certs | Ed25519 | C | `certs/revocation.rs`, `reclamation.rs`, etc. | Used |
| **KEL inception/rotation/interaction** | ML-DSA-65 | **PQ Used** | `harmony-kel/src/log.rs:57, 110, 114, 149` (`ml_dsa::verify`) | **Used** |
| KEL encryption-key commitments | ML-KEM-768 | PQ | `harmony-kel/src/event.rs:16,27`; `commitment.rs:6-14` | Used (commitment hashing) |
| Discovery announce verify (suite-dispatched) | Ed25519 **or** ML-DSA-65 | both | `harmony-discovery/src/verify.rs:37-42` → `harmony-identity/src/verify.rs:32-66` | **Used** (dual-path) |
| Profile / endorsement verify (suite-dispatched) | Ed25519 **or** ML-DSA-65 | both | `harmony-profile/src/verify.rs:23-68` | Used (dual-path) |
| **pkarr BEP44 outer envelope** | Ed25519 | **C (protocol-mandated)** | `harmony-pkarr/src/wire.rs:44-75`, `derive.rs:65-73` | Used |
| **pkarr inner identity sig** | Ed25519, **fixed 64-byte `harmony_identity_pub` + 64-byte sig** | **C (hardcoded, no PQ variant)** | `harmony-pkarr/src/record.rs:13-33, 48-90` | Used; **cannot carry ML-DSA** |
| Community DFROST threshold sign | FROST-Ristretto255 / Schnorr | **C by design** | `community_dfrost_crypto.rs:142-159` | Used (intentionally classical) |
| Community Tier-3 ballot secrecy | threshold ElGamal + Chaum-Pedersen DLEQ over Ristretto255 | **C by design** | `community_voting_tier3_crypto.rs:12-100`, `..._nizk.rs:50-95` | Used (intentionally classical) |

---

## (b) Identity-system map

There are **two complete, parallel identity systems**, both implemented to the same standard (generate / from_seed / sign / verify / encrypt / decrypt / to_public_bytes / address_hash / PoW puzzle):

```
                 master 32-byte seed (the ONLY recovery root, stored at rest)
                          │  HKDF (disjoint info strings)
        ┌─────────────────┴───────────────────┐
        ▼                                       ▼
 CLASSICAL PrivateIdentity              PQ PqPrivateIdentity
  X25519 (enc) + Ed25519 (sig)          ML-KEM-768 (enc) + ML-DSA-65 (sig)
  addr = SHA256(X‖Ed)[:16]              addr = SHA256(KEM‖DSA)[:16]
  identity.rs:42-349                    pq_identity.rs:39-416
        │                                       │
        ▼                                       ▼
  CANONICAL node address                 secondary "pq identity hash"
  (lib.rs:3089) — used for:              (lib.rs:3093) — used for:
   • Reticulum link + announce            • discovery query key expressions
   • mail owner address                   • discovery-token validation
   • DM seal/sign                         • friend-handshake tunnel hint
   • friend rendezvous                    • PQ tunnel session (replication)
   • owner/device enrollment              • KEL events
   • pkarr routing record
```

**Which is canonical?** The **classical** one. The Reticulum address (`our_addr_bytes`, `lib.rs:3089`) is the node's network identity for the primary transport, mail, DMs, friend rendezvous, and pkarr publication. The PQ identity is a co-resident secondary identity whose address only keys discovery/tunnel paths.

**How they relate:** Same seed, four disjoint HKDF sub-keys (`harmony-identity-{ed25519,x25519,ml-kem,ml-dsa}-v1`), so loss/rotation is unified at the seed but the two identities are cryptographically independent. There is **no hybrid identity** and **no cross-binding signature** (no Ed25519 signs the ML-DSA key or vice versa); the owner `PubKeyBundle` has a slot for both (`pubkey_bundle.rs:4-7`) but the mint path leaves `post_quantum: None`.

---

## (c) Where PQ is real vs decorative

**PQ is REAL (an actual ML-KEM encapsulation or ML-DSA signature is performed and verified):**

1. **`harmony-tunnel` friend-to-friend tunnel** — the cleanest PQ path. Handshake signs with ML-DSA-65 (`handshake.rs:59/130`), establishes the session key by ML-KEM-768 encapsulation (`handshake.rs:45-46/201-202`), no classical keys anywhere in the handshake. **But** it only carries **CRDT replication** (`event_loop.rs:1155-1199`), not user DMs, and only opens when `try_initiate_tunnel` fires for a peer with known PQ keys (`runtime.rs:1559-1642`).
2. **`harmony-kel` key-event-log** — inception/rotation/interaction all ML-DSA-verified (`log.rs:57/110/114/149`). PQ-native, no classical fallback.
3. **`harmony-memo`** — built on `PqPrivateIdentity` (PQ sign + ML-KEM seal).
4. **Discovery/profile signature VERIFICATION** — suite-dispatched and genuinely runs ML-DSA verify when a record's `CryptoSuite == MlDsa65` (`identity/verify.rs:57-64`). This is real verify code; whether it fires depends on whether anyone signs PQ records (the client signs classical).

**PQ is DECORATIVE (keys minted/stored/transmitted, but every actual op on that path is classical):**

1. **Friend handshake** — advertises peer ML-DSA(1952)+ML-KEM(1184) (`dm_tunnel_contact.rs:41-47`) and stores them on the `Contact` (`harmony-contacts/contact.rs:19-22`), **but the friendship secret and the Case-D seal are X25519 ECDH + ChaCha20** (`friend_rendezvous.rs:24-54, 90-112`). The handshake itself is signed with **Ed25519** over the 64-byte classical bundle.
2. **Direct messages** — both peers hold PQ keys, but the DM body is ChaCha20 under a classical content key (`dm_crypto.rs:57-79`), epoch keys are sealed by X25519 ECDH (`dm_signing.rs:50-113`), and packets are Ed25519-signed (`dm_signing.rs`/`dm_envelope.rs`).
3. **Owner / device enrollment** — `PubKeyBundle` has a PQ slot but mint emits `post_quantum: None`; every cert is Ed25519 (`pubkey_bundle.rs:41-50`, `certs/enrollment.rs`).

**PQ is ABSENT / blocked:**

1. **pkarr** — outer BEP44 envelope is Ed25519 by protocol mandate (unavoidable). The **inner** identity record is hardcoded to a 64-byte `harmony_identity_pub` (X25519‖Ed25519) and a 64-byte Ed25519 sig (`record.rs:13-33, 48-90`) — there is **no record format** that can carry a 1952-byte ML-DSA key or 3309-byte signature. PQ identities literally cannot publish identity-bound routing via pkarr today.
2. **Voice, owner-state-at-rest** — classical symmetric only (correct; no asymmetric step to make PQ).
3. **DFROST / Tier-3 voting** — FROST-Ristretto255 / Schnorr / threshold-ElGamal — **classical by deliberate design** (no production-ready PQ threshold scheme), per the D-FROST construction note. Not a drift point.

**Net verdict:** PQ key material is generated, persisted, and put on the wire pervasively, which makes the system *look* post-quantum. But the **two transports a user actually communicates over** — the Reticulum link (DMs, announces) and the friend rendezvous — are classical X25519/Ed25519. PQ does real work only in the replication tunnel and the KEL, neither of which carries conversational content.

---

## (d) Fluctuation hotspots (where the design mixes/wavers)

1. **`NodeIdentity` carries both, picks classical as canonical** — `identity.rs:59-89` mints both; `lib.rs:3089` then anoints the **Ed25519** hash as the node address while the PQ hash (`:3093`) becomes a secondary id. This is the structural root of "fluctuation": both exist, neither is decommissioned, and the canonical pointer is classical.

2. **Friend handshake: classical signature binding PQ payload** — the handshake is **signed Ed25519** over a digest that *includes* the carried ML-DSA/ML-KEM keys (`dm_tunnel_contact.rs`, `friend_devices_digest`). So a classical signature attests PQ keys that are then used for a *different* (tunnel) path. Authenticity of the PQ keys rests on Ed25519.

3. **Two tunnels, two crypto classes for "talking to a friend":** the **Reticulum link** (X25519, carries DMs) and the **iroh PQ tunnel** (ML-KEM/ML-DSA, carries CRDT replication) run side by side for the same peer. Same conversation, two key classes, split by payload type.

4. **`hybrid_kem.rs` exists but is dead** (`harmony-crypto/src/hybrid_kem.rs`) — a proper X25519‖ML-KEM defense-in-depth combiner is implemented and tested but **called nowhere in production**. This is the strongest signal of an intended hybrid design that was never wired in — the place the design "wavered" and stopped.

5. **`PubKeyBundle.post_quantum: Option<PqKeys>`** — owner identity has a PQ slot that is **always `None`** (`pubkey_bundle.rs:48`). The struct says "PQ-ready"; the mint says "classical."

6. **pkarr fixed 64-byte layout** — `record.rs` hardcodes classical sizes, silently making the discovery-publish path classical-only regardless of identity class.

7. **Suite-dispatch verify with no PQ signer** — `identity/verify.rs` and discovery/profile verify can check ML-DSA, but the client only ever *signs* classical announces/profiles, so the PQ verify branch is exercised by tests, not by live records from this client.

---

## (e) Options for a consistent decision + rough cost

The code currently sits at roughly **"classical for all user-facing comms; PQ for replication + KEL + capability tokens; hybrid designed-but-unwired."** Three coherent destinations:

### Option 1 — "Classical now, PQ later" (formalize the status quo)
Declare classical the canonical comms identity; treat PQ tunnel + KEL as the PQ beachhead; keep minting PQ keys as forward-deployment. **Cost: ~0 (documentation + a decision).** Honest, shippable, but leaves DMs quantum-vulnerable and leaves the PQ key minting looking like security theater to an auditor.

### Option 2 — "Hybrid everywhere" (defense-in-depth, recommended end-state)
Wire `hybrid_kem.rs` into the Reticulum link + DM seal + friend rendezvous so every shared secret is `HKDF(X25519_ss ‖ ML-KEM_ss)`; add an ML-DSA co-signature alongside Ed25519 on DM packets / announces / enrollment certs (the `PubKeyBundle` PQ slot and suite-dispatch verify already anticipate this). Secure if *either* primitive holds; preserves Reticulum/pkarr Ed25519 compatibility for the outer envelope.
**Cost: medium-high.** The combiner and PQ verify paths exist, so the work is wiring + wire-format versioning + key-availability handling, not new crypto. Hotspots: DM envelope grows by ~1KB (ML-KEM ct) + ~3.3KB (ML-DSA sig); pkarr inner record needs a v2 format (the one true blocker — `record.rs:13-33`); friend handshake digest already covers the PQ keys, so binding is half-done.

### Option 3 — "PQ everywhere" (drop classical)
Make `PqIdentity` canonical, retire the Reticulum X25519 link in favor of the PQ tunnel for all traffic, re-key the node address to the PQ hash.
**Cost: high + compatibility break.** Loses Reticulum interop and pkarr DHT publication (both Ed25519-bound by external protocol), forces a node-address migration, and inflates every packet. Not advisable while Reticulum/pkarr interop is a goal.

**Recommendation:** Option 2. The repo is already ~40% of the way there (PQ primitives, PQ tunnel, suite-dispatch verify, hybrid combiner, PQ slots in bundles all exist). The remaining gap is *wiring the hybrid into the user-facing seal/sign paths* and *adding a PQ-capable pkarr/announce wire format* — not inventing crypto. The biggest single blocker to any PQ-for-discovery story is the fixed 64-byte pkarr inner record (`harmony-pkarr/src/record.rs`).

---

## Appendix: how to reconcile the apparent contradiction

A surface read produces two opposite conclusions — "the client never calls ml_kem/ml_dsa" vs "the tunnel is fully PQ." Both are true: the **harmony-client `src-tauri/*.rs` files** contain no ML-KEM/ML-DSA operations because the PQ crypto executes one layer down, inside `harmony-node`/`harmony-tunnel` (which the client drives via `RuntimeAction::InitiateTunnel`). The client *carries* PQ keys and *triggers* the PQ tunnel; the *operation* happens in core. The "defaulting back to Curve25519" the owner senses is real and specific: the **conversational** transport (Reticulum link + DM seal/sign + friend rendezvous) is X25519/Ed25519, and the PQ tunnel that does exist only moves CRDT replication frames.
