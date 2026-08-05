# ZEB-827 Beacon Membership Binding — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bind a rendezvous beacon's transport identity to Joined community membership with an offline-verifiable, device-key-signed vouch, and reject (strict) any beacon that lacks a valid one in the member gateway-dial path.

**Architecture:** The beacon's enrolled community **device key** `D` signs a domain-separated **membership vouch** over `(community_id, transport_identity_pub, window)`. The vouch rides in the client-controlled rendezvous routing blob (covered by the existing pkarr inner-sig; no `harmony-pkarr`/`harmony` change). The gateway-dial resolver verifies the vouch and checks `D ∈ some Joined member's enrolled_device_keys` — all from persisted, materialized membership — **inside `resolve_slot`**, so a bad-vouch slot reads as empty and the escalating batch driver widens past it; a `membership_rejects` counter carries the "a beacon was present but rejected" signal up to `rejectedNonMember` telemetry.

**Tech Stack:** Rust, `ed25519_dalek`, `ciborium` (CBOR), `serde`/`serde_bytes`, `async_trait`, `tokio`. Client crate `harmony-app` under `src-tauri/`.

## Global Constraints

- **Client-only. No cross-repo change.** Do not edit `harmony-pkarr`, `harmony-reachability`, `harmony-identity`, or `harmony-owner` (pinned git revs). `ReachabilityAnnouncePayload` and `PkarrRoutingRecord` are untouched.
- **Strict enforcement.** A beacon with no valid vouch is rejected in the gateway-dial path (spec §2.3, §4). Open-join's unidentified resolve stays epoch-envelope (unchanged).
- **Vouch domain tag (verbatim):** `b"harmony.rendezvous.membership-vouch.v1"`.
- **Vouch version:** `MEMBERSHIP_VOUCH_V1: u8 = 1`.
- **Signature preimage is a fixed raw-byte layout** (not CBOR), for cross-node determinism: `domain_tag ‖ community_id[16] ‖ transport_pub[64] ‖ issued_at_ms.to_be_bytes() ‖ valid_until_ms.to_be_bytes()`.
- **Only the rendezvous publisher emits the vouch.** The shared `blob_builder` (Case-C member-keyed, Case-D friend publishers) is NOT modified.
- **Effective enrolled set:** in materialized state `enrolled_device_keys` is already post-revocation (materialize replay removes a revoked key and tombstones it in `revoked_device_keys`), so membership is a plain `enrolled_device_keys.contains(&D)` on a `Joined` member — no subtraction needed.
- **Cargo runs from `src-tauri/`.** Gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --features test-fixtures -E '<scoped>'` iteratively, then a final `--workspace --all-targets` sweep. Include `--all-targets` (integration tests) and `--locked`.
- **Commit trailers** on every commit:
  ```
  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
  ```

## File Structure

- **Create** `src-tauri/src/membership_vouch.rs` — the `MembershipVouch` type, `mint_membership_vouch`, `verify_membership_vouch`, constants, byte-serde helpers. One responsibility: the vouch primitive.
- **Modify** `src-tauri/src/community_rendezvous.rs` — add `RendezvousBeaconBlob` wire wrapper (encode/decode); teach `IdentifiedSlotResolver`/`resolve_rendezvous_identified` to carry the community id + enrolled-key set, run the strict vouch check in `resolve_slot`, and count `membership_rejects`.
- **Modify** `src-tauri/src/community_rendezvous_publisher.rs` — add `device_signing_key`; mint the vouch and emit the wrapped blob in `refresh_slot`.
- **Modify** `src-tauri/src/lib.rs` — thread `device_signing_key` into the `CommunityRendezvousPublisher::new` construction site (~:9658); register `pub mod membership_vouch;`.
- **Modify** `src-tauri/src/community_gateway_dial_driver.rs` — `GatewayDialCtx::enrolled_device_keys_of`; extend `BeaconResolver::resolve_beacon` + `ProdBeaconResolver` + `classify_resolution` for the enrolled set and `membership_rejects`; map to `RejectedNonMember`; add an integration test.

---

## Task 1: `membership_vouch` module (the vouch primitive)

**Files:**
- Create: `src-tauri/src/membership_vouch.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod membership_vouch;` near the other `pub mod community_*` declarations, ~:123)

**Interfaces:**
- Produces:
  - `pub const MEMBERSHIP_VOUCH_V1: u8 = 1;`
  - `pub struct MembershipVouch { pub version: u8, pub device_vk: [u8; 32], pub issued_at_ms: u64, pub valid_until_ms: u64, pub sig: [u8; 64] }` (CBOR serde, 2-char keys `vn`/`dv`/`ia`/`vu`/`sg`).
  - `pub fn mint_membership_vouch(device_sk: &ed25519_dalek::SigningKey, community_id: crate::owner_state_types::SpaceId, transport_pub: &[u8; 64], issued_at_ms: u64, valid_until_ms: u64) -> MembershipVouch`
  - `pub fn verify_membership_vouch(vouch: &MembershipVouch, community_id: crate::owner_state_types::SpaceId, record_transport_pub: &[u8; 64], enrolled_keys: &std::collections::HashSet<[u8; 32]>, now_ms: u64) -> bool`

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/src/membership_vouch.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::SpaceId;
    use ed25519_dalek::SigningKey;
    use std::collections::HashSet;

    fn sk(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }
    fn tpub(seed: u8) -> [u8; 64] {
        [seed; 64]
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let d = sk(1);
        let dvk = d.verifying_key().to_bytes();
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let v = mint_membership_vouch(&d, cid, &t, 1_000, 2_000);
        let enrolled: HashSet<[u8; 32]> = [dvk].into_iter().collect();
        assert!(verify_membership_vouch(&v, cid, &t, &enrolled, 1_500));
    }

    #[test]
    fn wrong_version_fails() {
        let d = sk(1);
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let mut v = mint_membership_vouch(&d, cid, &t, 1_000, 2_000);
        v.version = 2;
        let enrolled: HashSet<[u8; 32]> = [d.verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 1_500));
    }

    #[test]
    fn wrong_community_fails() {
        let d = sk(1);
        let t = tpub(9);
        let v = mint_membership_vouch(&d, SpaceId([7u8; 16]), &t, 1_000, 2_000);
        let enrolled: HashSet<[u8; 32]> = [d.verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, SpaceId([8u8; 16]), &t, &enrolled, 1_500));
    }

    #[test]
    fn wrong_transport_pub_fails() {
        let d = sk(1);
        let cid = SpaceId([7u8; 16]);
        let v = mint_membership_vouch(&d, cid, &tpub(9), 1_000, 2_000);
        let enrolled: HashSet<[u8; 32]> = [d.verifying_key().to_bytes()].into_iter().collect();
        // resolver passes the RECORD's transport pub, which differs from the signed one.
        assert!(!verify_membership_vouch(&v, cid, &tpub(10), &enrolled, 1_500));
    }

    #[test]
    fn stale_or_early_window_fails() {
        let d = sk(1);
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let v = mint_membership_vouch(&d, cid, &t, 1_000, 2_000);
        let enrolled: HashSet<[u8; 32]> = [d.verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 999)); // before issued_at
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 2_001)); // after valid_until
    }

    #[test]
    fn device_not_enrolled_fails() {
        let d = sk(1);
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let v = mint_membership_vouch(&d, cid, &t, 1_000, 2_000);
        let enrolled: HashSet<[u8; 32]> = [sk(2).verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 1_500));
    }

    #[test]
    fn tampered_sig_fails() {
        let d = sk(1);
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let mut v = mint_membership_vouch(&d, cid, &t, 1_000, 2_000);
        v.sig[0] ^= 0xFF;
        let enrolled: HashSet<[u8; 32]> = [d.verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 1_500));
    }

    #[test]
    fn substituted_device_key_fails() {
        // A valid vouch by D1, but device_vk swapped to an enrolled D2: the sig
        // no longer verifies under D2, so it must fail (can't borrow another
        // member's enrollment).
        let d1 = sk(1);
        let d2 = sk(2);
        let cid = SpaceId([7u8; 16]);
        let t = tpub(9);
        let mut v = mint_membership_vouch(&d1, cid, &t, 1_000, 2_000);
        v.device_vk = d2.verifying_key().to_bytes();
        let enrolled: HashSet<[u8; 32]> = [d2.verifying_key().to_bytes()].into_iter().collect();
        assert!(!verify_membership_vouch(&v, cid, &t, &enrolled, 1_500));
    }

    #[test]
    fn cbor_roundtrips() {
        let v = mint_membership_vouch(&sk(1), SpaceId([7u8; 16]), &tpub(9), 1_000, 2_000);
        let mut buf = Vec::new();
        ciborium::into_writer(&v, &mut buf).unwrap();
        let back: MembershipVouch = ciborium::from_reader(&buf[..]).unwrap();
        assert_eq!(v, back);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail (won't compile — module empty)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(membership_vouch)'`
Expected: FAIL — `MembershipVouch`/`mint_membership_vouch`/`verify_membership_vouch` not found.

- [ ] **Step 3: Implement the module**

Prepend to `src-tauri/src/membership_vouch.rs` (above the test module):

```rust
//! ZEB-827: membership vouch — a beacon's enrolled community device key `D`
//! signs a binding over its published transport identity `T`, so a resolving
//! member can confirm offline that the beacon belongs to a Joined member
//! without any live session or peer-interaction cache. See
//! `docs/superpowers/specs/2026-08-05-zeb-827-beacon-membership-binding-design.md`.

use crate::owner_state_types::SpaceId;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MEMBERSHIP_VOUCH_V1: u8 = 1;
const MEMBERSHIP_VOUCH_DOMAIN: &[u8] = b"harmony.rendezvous.membership-vouch.v1";

/// Proof, signed by an enrolled community device key, that a rendezvous
/// beacon's transport identity belongs to a Joined member. Rides in the
/// client-controlled rendezvous routing blob (spec §2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipVouch {
    #[serde(rename = "vn")]
    pub version: u8,
    /// D: the beacon's enrolled Ed25519 device VERIFY key (matches an entry in
    /// some Joined member's `enrolled_device_keys`).
    #[serde(rename = "dv", with = "vouch_bytes::arr32")]
    pub device_vk: [u8; 32],
    #[serde(rename = "ia")]
    pub issued_at_ms: u64,
    #[serde(rename = "vu")]
    pub valid_until_ms: u64,
    /// Ed25519 signature by `device_vk` over the raw-byte preimage (below).
    #[serde(rename = "sg", with = "vouch_bytes::arr64")]
    pub sig: [u8; 64],
}

/// Fixed raw-byte signature preimage (NOT CBOR — determinism without
/// canonicalization concerns): domain ‖ community_id(16) ‖ transport_pub(64)
/// ‖ issued_at_be(8) ‖ valid_until_be(8).
fn vouch_signed_bytes(
    community_id: SpaceId,
    transport_pub: &[u8; 64],
    issued_at_ms: u64,
    valid_until_ms: u64,
) -> Vec<u8> {
    let mut m = Vec::with_capacity(MEMBERSHIP_VOUCH_DOMAIN.len() + 16 + 64 + 16);
    m.extend_from_slice(MEMBERSHIP_VOUCH_DOMAIN);
    m.extend_from_slice(&community_id.0);
    m.extend_from_slice(transport_pub);
    m.extend_from_slice(&issued_at_ms.to_be_bytes());
    m.extend_from_slice(&valid_until_ms.to_be_bytes());
    m
}

pub fn mint_membership_vouch(
    device_sk: &SigningKey,
    community_id: SpaceId,
    transport_pub: &[u8; 64],
    issued_at_ms: u64,
    valid_until_ms: u64,
) -> MembershipVouch {
    let preimage = vouch_signed_bytes(community_id, transport_pub, issued_at_ms, valid_until_ms);
    let sig: Signature = device_sk.sign(&preimage);
    MembershipVouch {
        version: MEMBERSHIP_VOUCH_V1,
        device_vk: device_sk.verifying_key().to_bytes(),
        issued_at_ms,
        valid_until_ms,
        sig: sig.to_bytes(),
    }
}

/// The full strict check (spec §2.3 steps 2–6). `record_transport_pub` is the
/// resolving side's `PkarrRoutingRecord::harmony_identity_pub`; `enrolled_keys`
/// is the union of Joined (non-self) members' effective enrolled device keys.
pub fn verify_membership_vouch(
    vouch: &MembershipVouch,
    community_id: SpaceId,
    record_transport_pub: &[u8; 64],
    enrolled_keys: &HashSet<[u8; 32]>,
    now_ms: u64,
) -> bool {
    if vouch.version != MEMBERSHIP_VOUCH_V1 {
        return false;
    }
    if now_ms < vouch.issued_at_ms || now_ms > vouch.valid_until_ms {
        return false;
    }
    if !enrolled_keys.contains(&vouch.device_vk) {
        return false;
    }
    let Ok(vk) = VerifyingKey::from_bytes(&vouch.device_vk) else {
        return false;
    };
    let preimage = vouch_signed_bytes(
        community_id,
        record_transport_pub,
        vouch.issued_at_ms,
        vouch.valid_until_ms,
    );
    let sig = Signature::from_bytes(&vouch.sig);
    vk.verify_strict(&preimage, &sig).is_ok()
}

/// serde lacks built-in impls for `[u8; N>32]`; encode the fixed arrays as CBOR
/// byte strings (bstr) via `serialize_bytes`.
mod vouch_bytes {
    pub mod arr32 {
        pub fn serialize<S: serde::Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(b)
        }
        pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
            let v: serde_bytes::ByteBuf = serde::Deserialize::deserialize(d)?;
            v.into_vec()
                .try_into()
                .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
        }
    }
    pub mod arr64 {
        pub fn serialize<S: serde::Serializer>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(b)
        }
        pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
            let v: serde_bytes::ByteBuf = serde::Deserialize::deserialize(d)?;
            v.into_vec()
                .try_into()
                .map_err(|_| serde::de::Error::custom("expected 64 bytes"))
        }
    }
}
```

Then add `pub mod membership_vouch;` to `src-tauri/src/lib.rs` alongside the other `pub mod community_*;` declarations (~:123). Confirm `serde_bytes` is a dependency (it is — `PkarrRoutingRecord` uses `#[serde(with = "serde_bytes")]`); if `cargo` reports it missing as a direct dep, add `serde_bytes` to `src-tauri/Cargo.toml` `[dependencies]` at the version already in `Cargo.lock` and keep `--locked` happy.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(membership_vouch)'`
Expected: PASS (all 9 tests).

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/membership_vouch.rs src-tauri/src/lib.rs
git commit  # message: "ZEB-827: membership_vouch module (mint/verify device-key vouch)" + trailers
```

---

## Task 2: Rendezvous blob wire carriage (`RendezvousBeaconBlob`)

**Files:**
- Modify: `src-tauri/src/community_rendezvous.rs` (add the wrapper type + encode/decode helpers + tests near the top-level items)

**Interfaces:**
- Consumes: `MembershipVouch` (Task 1); `ReachabilityAnnouncePayload` (`crate::reachability_record`).
- Produces:
  - `pub struct RendezvousBeaconBlob { pub reachability: ReachabilityAnnouncePayload, pub membership_vouch: Option<MembershipVouch> }`
  - `pub fn encode_rendezvous_blob(reachability: &ReachabilityAnnouncePayload, vouch: Option<&MembershipVouch>) -> Vec<u8>`
  - `pub fn decode_rendezvous_blob(bytes: &[u8]) -> Option<(ReachabilityAnnouncePayload, Option<MembershipVouch>)>`

**Wire mechanism (spec §2.2):** the blob is a single CBOR map = `ReachabilityAnnouncePayload`'s fields (via `#[serde(flatten)]`) plus an optional `"mv"` key. A legacy bare-`ReachabilityAnnouncePayload` decode ignores the unknown `"mv"` key (it has no `deny_unknown_fields`), so an **old** resolver keeps dialing a **new** beacon. A vouchless (`None`) blob must encode **byte-identically** to the bare payload. **The byte-identical test below is the pin** — if `serde(flatten)` fails it (ciborium flatten edge cases), switch this task's implementation to the `ciborium::value::Value`-map-merge fallback (decode the bare payload to a `Value::Map`, push `("mv", vouch_value)`; encode) and keep the same tests.

- [ ] **Step 1: Write the failing tests**

Add to `community_rendezvous.rs` test module (create a `#[cfg(test)] mod blob_tests { ... }` if the file has no test module, else append):

```rust
#[cfg(test)]
mod rendezvous_blob_tests {
    use super::*;
    use crate::membership_vouch::mint_membership_vouch;
    use crate::owner_state_types::SpaceId;
    use crate::reachability_record::ReachabilityAnnouncePayload;
    use ed25519_dalek::SigningKey;

    fn payload() -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [0xAB; 32],
            home_relay_url: "https://derp.example/".to_string(),
            direct_addresses: vec![],
            announced_at_ms: 1_700_000_000_000,
            identity_signature: [0xCD; 64],
            butler_set: vec![],
            bs_at: 0,
        }
    }

    #[test]
    fn roundtrips_with_vouch() {
        let p = payload();
        let v = mint_membership_vouch(&SigningKey::from_bytes(&[3; 32]), SpaceId([1; 16]), &[9; 64], 1, 2);
        let bytes = encode_rendezvous_blob(&p, Some(&v));
        let (dp, dv) = decode_rendezvous_blob(&bytes).expect("decode");
        assert_eq!(dp, p);
        assert_eq!(dv, Some(v));
    }

    #[test]
    fn vouchless_blob_is_byte_identical_to_bare_payload() {
        let p = payload();
        let wrapped = encode_rendezvous_blob(&p, None);
        let mut bare = Vec::new();
        ciborium::into_writer(&p, &mut bare).unwrap();
        assert_eq!(wrapped, bare, "vouchless wrapper must equal legacy bare payload bytes");
    }

    #[test]
    fn legacy_bare_decode_ignores_vouch() {
        // Back-compat: an OLD resolver decoding a NEW (vouch-carrying) blob as a
        // bare payload still recovers reachability.
        let p = payload();
        let v = mint_membership_vouch(&SigningKey::from_bytes(&[3; 32]), SpaceId([1; 16]), &[9; 64], 1, 2);
        let bytes = encode_rendezvous_blob(&p, Some(&v));
        let bare: ReachabilityAnnouncePayload =
            ciborium::from_reader(&bytes[..]).expect("bare decode of wrapped");
        assert_eq!(bare, p);
    }

    #[test]
    fn decode_of_legacy_bare_yields_no_vouch() {
        // Forward-compat: a NEW resolver decoding an OLD (bare) blob sees no vouch.
        let p = payload();
        let mut bare = Vec::new();
        ciborium::into_writer(&p, &mut bare).unwrap();
        let (dp, dv) = decode_rendezvous_blob(&bare).expect("decode bare");
        assert_eq!(dp, p);
        assert_eq!(dv, None);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(rendezvous_blob_tests)'`
Expected: FAIL — `RendezvousBeaconBlob`/`encode_rendezvous_blob`/`decode_rendezvous_blob` not found.

- [ ] **Step 3: Implement**

Add near the top of `community_rendezvous.rs` (after imports; add `use crate::membership_vouch::MembershipVouch;` and ensure `use serde::{Serialize, Deserialize};`):

```rust
/// ZEB-827: the rendezvous slot's routing blob — a superset of
/// `ReachabilityAnnouncePayload` carrying an optional membership vouch. The
/// `flatten` places reachability's fields at the top level of the CBOR map, so
/// a legacy bare-payload decoder ignores the added `"mv"` key and a vouchless
/// blob is byte-identical to the legacy encoding (pinned by test).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RendezvousBeaconBlob {
    #[serde(flatten)]
    pub reachability: ReachabilityAnnouncePayload,
    #[serde(rename = "mv", default, skip_serializing_if = "Option::is_none")]
    pub membership_vouch: Option<MembershipVouch>,
}

pub fn encode_rendezvous_blob(
    reachability: &ReachabilityAnnouncePayload,
    vouch: Option<&MembershipVouch>,
) -> Vec<u8> {
    let blob = RendezvousBeaconBlob {
        reachability: reachability.clone(),
        membership_vouch: vouch.cloned(),
    };
    let mut out = Vec::new();
    // Fixed-size/serializable payload — encode cannot fail in practice.
    let _ = ciborium::into_writer(&blob, &mut out);
    out
}

pub fn decode_rendezvous_blob(
    bytes: &[u8],
) -> Option<(ReachabilityAnnouncePayload, Option<MembershipVouch>)> {
    let blob: RendezvousBeaconBlob = ciborium::from_reader(bytes).ok()?;
    Some((blob.reachability, blob.membership_vouch))
}
```

- [ ] **Step 4: Run to verify pass; if `vouchless_blob_is_byte_identical_to_bare_payload` fails, switch to the `Value`-merge fallback**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(rendezvous_blob_tests)'`
Expected: PASS (4 tests).

If the byte-identical test fails (flatten reorders/reshapes), replace `encode_rendezvous_blob`/`decode_rendezvous_blob` bodies with the `Value`-merge form (keep the struct only for typing):

```rust
pub fn encode_rendezvous_blob(
    reachability: &ReachabilityAnnouncePayload,
    vouch: Option<&MembershipVouch>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = ciborium::into_writer(reachability, &mut out);
    let Some(v) = vouch else { return out };
    // Merge "mv" into the payload's CBOR map without disturbing existing keys.
    let mut val: ciborium::value::Value = match ciborium::from_reader(&out[..]) {
        Ok(v) => v,
        Err(_) => return out,
    };
    if let ciborium::value::Value::Map(entries) = &mut val {
        let mut vbytes = Vec::new();
        let _ = ciborium::into_writer(v, &mut vbytes);
        if let Ok(vval) = ciborium::from_reader::<ciborium::value::Value, _>(&vbytes[..]) {
            entries.push((ciborium::value::Value::Text("mv".to_string()), vval));
        }
    }
    let mut merged = Vec::new();
    let _ = ciborium::into_writer(&val, &mut merged);
    merged
}

pub fn decode_rendezvous_blob(
    bytes: &[u8],
) -> Option<(ReachabilityAnnouncePayload, Option<MembershipVouch>)> {
    let reachability: ReachabilityAnnouncePayload = ciborium::from_reader(bytes).ok()?;
    let mut vouch = None;
    if let Ok(ciborium::value::Value::Map(entries)) = ciborium::from_reader::<ciborium::value::Value, _>(bytes) {
        for (k, v) in entries {
            if matches!(&k, ciborium::value::Value::Text(t) if t == "mv") {
                let mut vb = Vec::new();
                if ciborium::into_writer(&v, &mut vb).is_ok() {
                    vouch = ciborium::from_reader::<MembershipVouch, _>(&vb[..]).ok();
                }
            }
        }
    }
    Some((reachability, vouch))
}
```

Re-run; expected PASS. Record which mechanism shipped in the task report.

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/community_rendezvous.rs
git commit  # "ZEB-827: rendezvous blob wire carriage (vouch superset of reachability payload)" + trailers
```

---

## Task 3: Publisher mints the vouch and emits the wrapped blob

**Files:**
- Modify: `src-tauri/src/community_rendezvous_publisher.rs` (struct field, `new`/`new_with_sink`, `refresh_slot` record builder, test helper `publisher_for`)
- Modify: `src-tauri/src/lib.rs` (thread `device_signing_key` into `CommunityRendezvousPublisher::new` at ~:9658)

**Interfaces:**
- Consumes: `mint_membership_vouch` + `encode_rendezvous_blob` (Tasks 1–2).
- Produces: `CommunityRendezvousPublisher::new(publisher, identity_signing_key, identity_pub, device_signing_key: Arc<ed25519_dalek::SigningKey>, routing_blob_builder)` (one new positional param, appended before `routing_blob_builder`).

- [ ] **Step 1: Write the failing test**

Add to `community_rendezvous_publisher.rs` test module (there's a `MockPublisher` spy + `publisher_for`). The record builder is exercised through the sink's `register` — verify the emitted record's blob decodes to a payload + a vouch that verifies:

```rust
#[tokio::test]
async fn refresh_slot_emits_vouch_verifiable_under_device_key() {
    use crate::community_rendezvous::decode_rendezvous_blob;
    use crate::membership_vouch::verify_membership_vouch;
    use crate::owner_state_types::{EpochKey, OwnerAddr, SpaceId};
    use std::collections::HashSet;

    let spy = std::sync::Arc::new(MockPublisher::default());
    let device_sk = SigningKey::from_bytes(&[42u8; 32]);
    let id_sk = SigningKey::from_bytes(&[11u8; 32]);
    let id_pub = build_id_pub(&id_sk);
    let publisher = CommunityRendezvousPublisher::new_with_sink(
        spy.clone(),
        id_sk,
        id_pub,
        std::sync::Arc::new(device_sk.clone()),
        std::sync::Arc::new(|| {
            // A minimal valid ReachabilityAnnouncePayload blob.
            let p = crate::reachability_record::ReachabilityAnnouncePayload {
                iroh_node_id: [1u8; 32],
                home_relay_url: String::new(),
                direct_addresses: vec![],
                announced_at_ms: 1_000,
                identity_signature: [0u8; 64],
                butler_set: vec![],
                bs_at: 0,
            };
            let mut b = Vec::new();
            ciborium::into_writer(&p, &mut b).unwrap();
            b
        }),
    );

    let cid = SpaceId([7u8; 16]);
    let me = OwnerAddr([5u8; 16]);
    // `me` must rank as an advertiser so a slot is chosen.
    publisher
        .refresh_slot(cid, EpochKey::new([2u8; 32]), vec![me], me)
        .await;

    // The spy captured a RecordBuilder; run it and inspect the produced record.
    let rec = spy.last_built_record().expect("a record was registered");
    let (payload, vouch) = decode_rendezvous_blob(&rec.routing_blob).expect("blob decodes");
    let vouch = vouch.expect("vouch present");
    let enrolled: HashSet<[u8; 32]> = [device_sk.verifying_key().to_bytes()].into_iter().collect();
    assert!(verify_membership_vouch(
        &vouch,
        cid,
        &rec.harmony_identity_pub,
        &enrolled,
        rec.announced_at_ms,
    ));
}
```

If `MockPublisher` does not already expose the built record, extend the spy in this task to capture the `RecordBuilder`'s output (call the builder with a fixed `at_ms`, store the resulting `PkarrRoutingRecord`) and add `last_built_record()`. Keep that change inside the test module / spy.

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(refresh_slot_emits_vouch)'`
Expected: FAIL — `new_with_sink` arity mismatch / no `device_signing_key`.

- [ ] **Step 3: Implement**

In `community_rendezvous_publisher.rs`:

1. Add the field to the struct (after `identity_pub`):
   ```rust
       /// ZEB-827: the node's enrolled community device signing key, used to
       /// mint the membership vouch carried in the rendezvous blob.
       device_signing_key: Arc<ed25519_dalek::SigningKey>,
   ```
2. Add `device_signing_key: Arc<ed25519_dalek::SigningKey>` as a parameter to BOTH `new` and `new_with_sink` (positioned right after `identity_pub`), thread it through `new` → `new_with_sink`, and store it in the struct literal.
3. In `refresh_slot`'s `RecordBuilder` closure, replace the `blob_builder()` argument to `sign_new` with a vouch-wrapped blob. Capture `device_signing_key` + `community_id` into the closure:
   ```rust
       let id_sk = self.identity_signing_key.clone();
       let id_pub = self.identity_pub;
       let device_sk = Arc::clone(&self.device_signing_key);
       let blob_builder = Arc::clone(&self.routing_blob_builder);
       let vouch_community = community_id;
       let builder: RecordBuilder = Arc::new(move |at_ms| {
           let base = blob_builder();
           let ttl_at = at_ms + crate::reachability_record::REACHABILITY_RECORD_TTL_MS;
           // Wrap the reachability payload with a fresh membership vouch. If the
           // base blob can't be decoded (e.g. empty — no endpoint yet), publish
           // it unchanged (a vouchless blob; strict resolvers skip it, old ones
           // still read it).
           let routing = match ciborium::from_reader::<crate::reachability_record::ReachabilityAnnouncePayload, _>(base.as_slice()) {
               Ok(reach) => {
                   let vouch = crate::membership_vouch::mint_membership_vouch(
                       &device_sk, vouch_community, &id_pub, at_ms, ttl_at,
                   );
                   crate::community_rendezvous::encode_rendezvous_blob(&reach, Some(&vouch))
               }
               Err(_) => base,
           };
           PkarrRoutingRecord::sign_new(routing, id_pub, at_ms, ttl_at, &id_sk)
               .expect("sign — fixed-size buffers should not fail")
       });
   ```
4. Update the test helper `publisher_for` to pass a device key:
   ```rust
       fn publisher_for(spy: Arc<MockPublisher>) -> CommunityRendezvousPublisher {
           let sk = SigningKey::from_bytes(&[11u8; 32]);
           let id_pub = build_id_pub(&sk);
           CommunityRendezvousPublisher::new_with_sink(
               spy,
               sk,
               id_pub,
               Arc::new(SigningKey::from_bytes(&[42u8; 32])),
               Arc::new(|| b"routing".to_vec()),
           )
       }
   ```
   (Note: `publisher_for`'s `b"routing"` blob is not a valid `ReachabilityAnnouncePayload`, so those existing tests exercise the `Err(_) => base` fallback — the record is published vouchless. That is correct and keeps them green; they assert slot registration, not vouch content.)

In `lib.rs` at the `CommunityRendezvousPublisher::new(...)` call (~:9658), insert the device key argument after `identity_pub_64`:
```rust
       let community_rendezvous_pub = std::sync::Arc::new(
           community_rendezvous_publisher::CommunityRendezvousPublisher::new(
               std::sync::Arc::clone(&pkarr_publisher_arc),
               (*signing_key_arc).clone(),
               identity_pub_64,
               std::sync::Arc::clone(&device_signing_key_arc), // ZEB-827
               std::sync::Arc::clone(&blob_builder),
           ),
       );
```
`device_signing_key_arc` must be an `Arc<ed25519_dalek::SigningKey>` in scope at :9658. Locate the loaded owner's `device_signing_key` (from `owner_loaded`/`loaded`, e.g. `l.device_signing_key`) and, at the point it is available (near where `signing_key_arc`/`identity_pub_64` are prepared, ~:4185–:5411 or wherever the owner load result is destructured), bind `let device_signing_key_arc = std::sync::Arc::new(<owner>.device_signing_key.clone());` so it survives to :9658. If the owner may be absent (no-owner boot), the rendezvous publisher is only constructed on the owner-loaded path anyway (it needs the identity keys), so the device key is present there; use the same guard the surrounding block already applies. Do NOT modify the sibling `PkarrInvitePublisher`/`PkarrIdentityPublisher`/`PkarrCommunityPublisher::new` calls — only the rendezvous one.

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_rendezvous_publisher) | test(refresh_slot_emits_vouch)'`
Expected: PASS (new test + existing publisher tests).

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/community_rendezvous_publisher.rs src-tauri/src/lib.rs
git commit  # "ZEB-827: rendezvous publisher mints + emits membership vouch" + trailers
```

---

## Task 4: Resolver strict check with batch widening + `membership_rejects` counter

**Files:**
- Modify: `src-tauri/src/community_rendezvous.rs` (`IdentifiedSlotResolver`, `resolve_slot`, `IdentifiedResolve`, `resolve_rendezvous_identified`)

**Interfaces:**
- Consumes: `decode_rendezvous_blob` (Task 2), `verify_membership_vouch` (Task 1), `SpaceId`, `HashSet<[u8;32]>`.
- Produces (extended signatures):
  - `pub struct IdentifiedResolve { pub outcome: RendezvousResolveOutcome<IdentifiedBeacon>, pub resolve_errors: usize, pub membership_rejects: usize }`
  - `pub async fn resolve_rendezvous_identified(pkarr, epoch_key, self_endpoint_id, community_id: SpaceId, enrolled_keys: Arc<HashSet<[u8; 32]>>, now_ms, cfg) -> IdentifiedResolve`

- [ ] **Step 1: Write the failing test**

Add a `resolve_slot`-level test using a stub `PkarrResolver`? The existing tests drive `resolve_rendezvous_identified` against a mock relay. Follow the existing pattern in `community_rendezvous.rs` tests (there is already a publish/resolve harness used by ZEB-824). Add:

```rust
#[cfg(test)]
mod strict_vouch_resolve_tests {
    use super::*;
    use crate::membership_vouch::mint_membership_vouch;
    use crate::owner_state_types::SpaceId;
    use ed25519_dalek::SigningKey;
    use std::collections::HashSet;
    use std::sync::Arc;

    // Reuse the module's existing mock-relay publish helper (the one ZEB-824
    // tests use to seed rendezvous slots). Pseudocode names — bind to the real
    // helper in this file:
    //   publish_rendezvous_slot(relay, epoch_key, slot_index, record)
    //   make_pkarr_over(relay) -> Arc<PkarrResolver>

    #[tokio::test]
    async fn valid_vouch_is_found_bad_or_missing_is_rejected_and_widens() {
        // Slot 0: a beacon whose vouch device key is NOT enrolled -> rejected.
        // Slot 1: a beacon with a valid vouch by an enrolled key -> Found.
        // Assert: the resolve returns Found (widened past slot 0) AND
        // membership_rejects == 1.
        // Build using the file's existing publish helper + IdentifiedBeaconBlob
        // via encode_rendezvous_blob.
        // ... (implement against the real helpers in this file) ...
    }

    #[tokio::test]
    async fn all_slots_unvouched_yields_notfound_with_reject_count() {
        // A single beacon with no vouch (bare ReachabilityAnnouncePayload) ->
        // resolve returns outcome.payload == None AND membership_rejects >= 1.
    }
}
```

Implement these two tests concretely against this file's existing rendezvous test harness (the ZEB-824 mock-relay helpers). The load-bearing assertions: (a) a valid vouch at a later slot wins after an invalid one at an earlier slot (widening preserved); (b) `IdentifiedResolve.membership_rejects` counts the rejected beacons; (c) a bare (vouchless) record never yields a `Found`.

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(strict_vouch_resolve_tests)'`
Expected: FAIL — `resolve_rendezvous_identified` arity + `membership_rejects` missing.

- [ ] **Step 3: Implement**

1. Add fields to `IdentifiedSlotResolver`:
   ```rust
       /// ZEB-827: this community's id (the vouch binds it) and the union of
       /// Joined (non-self) members' effective enrolled device keys.
       community_id: crate::owner_state_types::SpaceId,
       enrolled_keys: std::sync::Arc<std::collections::HashSet<[u8; 32]>>,
       /// Beacons that verified transport/epoch but failed the membership vouch
       /// (missing, malformed, stale, bad sig, or device not enrolled). Read as
       /// an empty slot so the batch driver widens — mirrors `resolve_errors`.
       membership_rejects: std::sync::Arc<std::sync::atomic::AtomicUsize>,
   ```
2. In `resolve_slot`, replace the tail (from the `ReachabilityAnnouncePayload` decode onward) with:
   ```rust
       let (payload, vouch) = crate::community_rendezvous::decode_rendezvous_blob(
           rec.routing_blob.as_slice(),
       )?;
       if payload.iroh_node_id == self.self_endpoint_id {
           return None;
       }
       // ZEB-827 strict: a beacon must carry a vouch that (a) verifies under a
       // member's enrolled device key and (b) binds THIS record's transport
       // identity + community. A failure reads as an EMPTY slot so the
       // escalating batch driver widens to the other slots (same shape as the
       // self-filter above), while `membership_rejects` records that a beacon
       // WAS present — the caller maps that to `rejectedNonMember`.
       let ok = vouch.as_ref().is_some_and(|v| {
           crate::membership_vouch::verify_membership_vouch(
               v,
               self.community_id,
               &rec.harmony_identity_pub,
               &self.enrolled_keys,
               now_ms,
           )
       });
       if !ok {
           self.membership_rejects
               .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
           tracing::debug!(
               slot = slot_index,
               node_id = %hex::encode(&payload.iroh_node_id[..8]),
               has_vouch = vouch.is_some(),
               "ZEB-827: rendezvous beacon rejected — no valid membership vouch"
           );
           return None;
       }
       Some(IdentifiedBeacon {
           payload,
           beacon_identity_pub: rec.harmony_identity_pub,
       })
   ```
   (`now_ms` is the `SystemTime`-derived value already computed just above for the freshness re-check — reuse it.)
3. Extend `IdentifiedResolve` with `pub membership_rejects: usize`.
4. Extend `resolve_rendezvous_identified` signature with `community_id: SpaceId, enrolled_keys: Arc<HashSet<[u8; 32]>>` (place after `self_endpoint_id`), construct the resolver with them + a fresh `membership_rejects` counter, and populate the returned struct:
   ```rust
       let membership_rejects = Arc::new(AtomicUsize::new(0));
       let resolver = IdentifiedSlotResolver {
           pkarr: Arc::clone(pkarr),
           epoch_key_bytes: Zeroizing::new(epoch_key.as_bytes().to_vec()),
           self_endpoint_id,
           resolve_errors: Arc::clone(&resolve_errors),
           community_id,
           enrolled_keys,
           membership_rejects: Arc::clone(&membership_rejects),
       };
       let outcome = resolve_rendezvous_with(&resolver, now_ms, cfg).await;
       IdentifiedResolve {
           outcome,
           resolve_errors: resolve_errors.load(Ordering::Relaxed),
           membership_rejects: membership_rejects.load(Ordering::Relaxed),
       }
   ```

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(strict_vouch_resolve_tests) | test(community_rendezvous)'`
Expected: PASS. (The open-join `resolve_rendezvous` unidentified path is untouched — it still decodes a bare `ReachabilityAnnouncePayload`, which now happily ignores the `"mv"` key.)

- [ ] **Step 5: Gate + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/community_rendezvous.rs
git commit  # "ZEB-827: strict vouch check in resolve_slot with batch widening + reject counter" + trailers
```

---

## Task 5: Driver enforcement — enrolled-key ctx, resolver wiring, `rejectedNonMember` telemetry, integration

**Files:**
- Modify: `src-tauri/src/community_gateway_dial_driver.rs` (`GatewayDialCtx` + `ProdGatewayDialCtx`; `BeaconResolver` + `ProdBeaconResolver`; `classify_resolution`; `run_one_pass`; integration test)

**Interfaces:**
- Consumes: `resolve_rendezvous_identified` (Task 4, new params); `MemberState.enrolled_device_keys` / `MemberStatus::Joined` (`community_membership`).
- Produces (extended):
  - `GatewayDialCtx::enrolled_device_keys_of(&self, community: &SpaceId) -> HashSet<[u8; 32]>`
  - `BeaconResolver::resolve_beacon(&self, epoch_key: &EpochKey, community_id: SpaceId, enrolled_keys: Arc<HashSet<[u8; 32]>>, now_ms: u64) -> BeaconResolution`
  - `classify_resolution(payload, resolve_errors, membership_rejects)` → adds `BeaconResolution::RejectedNonMember`

- [ ] **Step 1: Write the failing tests**

Add to the driver's test module (it already has stub `GatewayDialCtx`/`BeaconResolver` for the ZEB-824 tests — extend those stubs). Two tests:

```rust
#[tokio::test]
async fn starved_pass_with_proven_beacon_seeds() {
    // ctx: one Joined member (non-self), enrolled key D; supervisor has no
    // connected owner -> starved. Stub BeaconResolver returns Found(beacon).
    // Assert: seed_from_pkarr called once; outcome BeaconSeeded.
    // (Extend the existing ZEB-824 seed test; the new resolve_beacon signature
    // now takes community_id + enrolled_keys — the stub ignores them.)
}

#[tokio::test]
async fn starved_pass_with_rejected_beacon_records_rejected_non_member() {
    // Stub BeaconResolver returns BeaconResolution::RejectedNonMember.
    // Assert: NO seed; telemetry per-community outcome == RejectedNonMember.
}

#[test]
fn classify_prefers_rejected_non_member_over_resolve_error() {
    assert!(matches!(classify_resolution(None, 0, 1), BeaconResolution::RejectedNonMember));
    assert!(matches!(classify_resolution(None, 2, 1), BeaconResolution::RejectedNonMember));
    assert!(matches!(classify_resolution(None, 2, 0), BeaconResolution::ResolveError));
    assert!(matches!(classify_resolution(None, 0, 0), BeaconResolution::NotFound));
}
```

Also add an in-process integration test `tests/community_misc/community_gateway_dial_vouch_integration.rs` (or extend an existing ZEB-824 integration file) mirroring the spec §8 headline: a member node with a mock pkarr relay carrying a rendezvous record **with** a valid vouch under the community epoch key seeds the resolver in one `run_one_pass`; the same record **without** a vouch records `rejectedNonMember` and does not seed. Reuse the ZEB-824 integration harness (`tests/misc/community_open_join_cross_wan_integration.rs:674` mock-relay publish helper referenced by the spec).

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_gateway_dial)'`
Expected: FAIL — `enrolled_device_keys_of` / `RejectedNonMember` / `classify_resolution` arity missing.

- [ ] **Step 3: Implement**

1. `BeaconResolution`: add a variant:
   ```rust
       /// A beacon verified transport+epoch but carried no valid membership
       /// vouch (ZEB-827 strict). Distinct from `NotFound` (no beacon at all)
       /// and `ResolveError` (pkarr/transport trouble).
       RejectedNonMember,
   ```
2. `classify_resolution`: add `membership_rejects: usize` and precedence (a present-but-rejected beacon is more informative than a probe error):
   ```rust
   fn classify_resolution(
       payload: Option<IdentifiedBeacon>,
       resolve_errors: usize,
       membership_rejects: usize,
   ) -> BeaconResolution {
       match payload {
           Some(hit) => BeaconResolution::Found(hit),
           None if membership_rejects > 0 => BeaconResolution::RejectedNonMember,
           None if resolve_errors > 0 => BeaconResolution::ResolveError,
           None => BeaconResolution::NotFound,
       }
   }
   ```
3. `GatewayDialCtx` trait: add
   ```rust
       /// ZEB-827: union of Joined (non-self) members' effective enrolled
       /// device verify keys — the set a beacon's vouch key must be in.
       async fn enrolled_device_keys_of(&self, community: &SpaceId) -> std::collections::HashSet<[u8; 32]>;
   ```
   `ProdGatewayDialCtx` impl (same engine access as `members_of`, but gather keys):
   ```rust
       async fn enrolled_device_keys_of(&self, community: &SpaceId) -> std::collections::HashSet<[u8; 32]> {
           let Some(engine) = self.registry.engine_arc(community).await else {
               return std::collections::HashSet::new();
           };
           let state_arc = engine.state();
           let st = state_arc.lock().await;
           let mat = st.materialized(engine.admin_addr());
           mat.members
               .iter()
               .filter(|(addr, m)| {
                   m.status == crate::community_membership::MemberStatus::Joined
                       && **addr != self.self_owner
               })
               .flat_map(|(_, m)| m.enrolled_device_keys.iter().copied())
               .collect()
       }
   ```
4. `BeaconResolver::resolve_beacon`: extend signature and `ProdBeaconResolver` impl:
   ```rust
   async fn resolve_beacon(
       &self,
       epoch_key: &EpochKey,
       community_id: SpaceId,
       enrolled_keys: Arc<std::collections::HashSet<[u8; 32]>>,
       now_ms: u64,
   ) -> BeaconResolution;
   // ProdBeaconResolver:
       let res = crate::community_rendezvous::resolve_rendezvous_identified(
           &self.pkarr,
           epoch_key,
           self.self_endpoint_id,
           community_id,
           enrolled_keys,
           now_ms,
           &rendezvous_config_from_env(),
       )
       .await;
       classify_resolution(res.outcome.payload, res.resolve_errors, res.membership_rejects)
   ```
5. `run_one_pass`: at the resolve site (~:382), fetch the enrolled set and pass it, then handle the new arm. Just before the `resolve_beacon` call:
   ```rust
       let enrolled_keys = Arc::new(self.ctx.enrolled_device_keys_of(&community).await);
       let hit = match self
           .beacons
           .resolve_beacon(&epoch_key, community, enrolled_keys, now_ms)
           .await
       {
           BeaconResolution::Found(hit) => hit,
           BeaconResolution::RejectedNonMember => {
               self.record(&community, GatewayBootstrapOutcome::RejectedNonMember);
               continue;
           }
           BeaconResolution::NotFound => {
               self.record(&community, GatewayBootstrapOutcome::NoBeacon);
               continue;
           }
           BeaconResolution::ResolveError => {
               self.record(&community, GatewayBootstrapOutcome::ResolveError);
               continue;
           }
       };
   ```
   Then in the seed tail (formerly the epoch-envelope block, ~:393–:454): every `hit` is now membership-proven, so the identity-decode `warn!` guard and the long epoch-envelope trust comment are replaced by a short note that the beacon is vouch-verified. Keep the seed exactly as-is (seed under the composite device-address owner + `DeviceIdentityHash([0u8; 16])` placeholder, then kick) — **the seed-owner "split" fix is out of scope (spec §9 / this plan's boundaries).** Concretely, keep:
   ```rust
       let Ok(identity) = harmony_identity::Identity::from_public_bytes(&hit.beacon_identity_pub) else {
           // Unreachable on the prod path (inner-sig already parsed this) —
           // a malformed identity means a wire/publisher bug, not an attacker.
           self.record(&community, GatewayBootstrapOutcome::RejectedNonMember);
           continue;
       };
       // ZEB-827: the beacon carried a membership vouch verified against this
       // community's enrolled device keys (resolve_slot), so it belongs to a
       // Joined member. Seed as before (owner-split fix deferred — spec §9).
       let beacon_owner = OwnerAddr(identity.address_hash);
       let node_id = hit.payload.iroh_node_id;
       self.reachability
           .seed_from_pkarr(beacon_owner, DeviceIdentityHash([0u8; 16]), hit.payload)
           .await;
       if let Some(sup) = self.reachability.supervisor() {
           sup.kick(node_id, ReconnectTrigger::NewPeer);
       }
       self.record(&community, GatewayBootstrapOutcome::BeaconSeeded);
       tracing::info!(community = ?community, "ZEB-827: vouch-verified rendezvous beacon seeded — reconnect supervisor kicked");
   ```
6. Update ALL test/stub `GatewayDialCtx` and `BeaconResolver` impls in the driver's test module (and any other file implementing these traits — grep `impl GatewayDialCtx`, `impl BeaconResolver`) to the new signatures: stubs add `enrolled_device_keys_of` (return a configurable set) and the new `resolve_beacon` params (ignore or assert them). Update the `network_health.rs` `GatewayBootstrapOutcome` doc on `RejectedNonMember` to note it is now the primary membership-rejection signal under ZEB-827 strict (its wire string `"rejectedNonMember"` and the `rejected_non_member` counter are unchanged).

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_gateway_dial) | test(gateway_dial_vouch)'`
Expected: PASS.

- [ ] **Step 5: Full gate + commit**

```bash
cd src-tauri && cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
git add -A
git commit  # "ZEB-827: strict membership enforcement in gateway dial + rejectedNonMember telemetry" + trailers
```

---

## Self-Review

**1. Spec coverage:**
- §2.1 vouch structure + domain tag + raw-byte preimage → Task 1. ✔
- §2.2 wire carriage (superset of ReachabilityAnnouncePayload, inner-sig-covered, legacy-decode-compat, scope guard: only rendezvous publisher) → Task 2 (blob) + Task 3 (only rendezvous publisher emits). ✔
- §2.3 resolver 6-step strict check → Task 1 (`verify_membership_vouch` does version/transport/freshness/sig/membership) + Task 4 (runs it in `resolve_slot`, self-filter first). ✔
- §3 security (forgery needs device sk; transport-swap rejected; revoked excluded via effective enrolled set; substituted-key test) → Task 1 tests. ✔
- §4 strict rollout (reject unproven; old-reader compat) → Task 2 legacy-decode tests + Task 4/5 strict path. ✔
- §5 publisher wiring (device_signing_key + community_id, own blob path, not shared builder) → Task 3. ✔
- §6 resolver changes (IdentifiedSlotResolver + gateway driver, engine access) → Tasks 4–5. ✔
- §7 telemetry (reuse `rejectedNonMember`) → Task 5. ✔
- §8 tests (unit vouch, wire carriage, resolver strict incl. widening + unknown-member, integration) → Tasks 1,2,4,5. ✔
- §9 boundaries (malicious member, unknown-new-member fall-through, open-join unchanged, no cross-repo, publisher-wart) → respected; **added boundary:** seed-owner split fix deferred (Task 5). ✔

**2. Placeholder scan:** Task 4/5 test bodies are described as "implement against the file's existing rendezvous/ZEB-824 harness" rather than fully written, because they depend on in-file mock-relay helper names not captured verbatim; the assertions and structure are specified concretely. This is a deliberate, bounded hand-off (the harness is local and self-evident), not a "write tests for the above" placeholder. Every production-code step carries real code.

**3. Type consistency:** `MembershipVouch`/`mint_membership_vouch`/`verify_membership_vouch` (Task 1) used verbatim in Tasks 2–5. `encode_rendezvous_blob`/`decode_rendezvous_blob` (Task 2) used in Tasks 3–4. `resolve_rendezvous_identified` new params (Task 4) consumed by `ProdBeaconResolver` (Task 5). `enrolled_keys: Arc<HashSet<[u8;32]>>` threaded consistently driver→resolver. `BeaconResolution::RejectedNonMember` + `classify_resolution` 3-arg form consistent Tasks 4→5. `GatewayBootstrapOutcome::RejectedNonMember` is pre-existing (network_health.rs) — reused, not redefined. ✔
