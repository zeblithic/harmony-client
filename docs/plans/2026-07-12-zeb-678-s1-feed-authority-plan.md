# ZEB-678 S1 — Feed authority record foundation: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce the per-feed `FeedAuthorityRecord` type, its `#3`-signed binding, chokepoint-backed verification, and an in-memory LWW cache with first-write-wins binding + sticky revocation — data + verify only, no publish/engine wiring.

**Architecture:** A new self-contained module `src-tauri/src/feed_authority.rs` mirrors `vine_signing.rs`: a JSON wire record whose `n_sig` binds `feed_id → owner_id/device_id/publisher_key` under the feed's `#3` node key, plus a full verifier that reuses the `enrollment_verify` chokepoint for the `#2` enrollment/revocation and a `FeedAuthorityCache` that pins the binding on first valid sight and treats `revoked` as monotonic-true. No disk, no Zenoh, no `revoke_device` wiring — those are S2/S3.

**Tech Stack:** Rust, `serde`/`serde_json` (vines are JSON, not CBOR), `harmony_owner::certs` (`EnrollmentCert`/`RevocationCert`), `harmony_identity` (`#3` `PrivateIdentity`), `ed25519_dalek`, `hex`.

## Global Constraints

- Spec: `docs/specs/2026-07-12-zeb-678-vine-follow-revocation-design.md` §3.1, §4 steps 1-4, §10 S1. S1 is **data + verify only**: no reactions, no signing migration, no fleet-net self-stamp, no `revoke_device` wiring.
- New wire fields are additive JSON: `#[serde(rename_all = "camelCase")]`, `#[serde(default, skip_serializing_if = ...)]` on optional/vec fields, declared to keep default-omitted encoding. Signatures are wire-only (never persisted). No `FILE_VERSION` bump anywhere.
- Binding bytes are length-prefixed exactly like `vine_signing` (`push_str` = `u32-LE len ‖ bytes`); domain constant `"harmony-vine-authority-v1"`. The `n_sig` covers ONLY the immutable binding fields (`feed_id, owner_id, device_id, publisher_key`) — never `updated_at` or `revocation`.
- Chokepoint reuse only — never re-implement issuer policy: `enrollment_verify::verify_enrollment_any_issuer(cert, signer_certs, Some(&owner_id), now_secs)`; `verify_revocation_any_issuer(cert, target_enrollment, signer_certs, now_secs)`. `now_secs = updated_at / 1000` for enrollment; `revocation.issued_at` for revocation (issued-at semantics).
- Cache discipline: active binding (`device_id, publisher_key, n_identity_pub`) is **first-write-wins**; `revoked` is **sticky/monotonic-true**, set only by a record carrying a valid `RevocationCert` whose `target == pinned device_id`, and never cleared regardless of `updated_at`.
- Gates (harmony-client `CLAUDE.md`): per-task `scripts/test-select --context task` (paste the `round=…/bucket=…` summary line into the task note); `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` before the PR opens. All cargo commands run from `src-tauri/`.
- Commit trailer on every commit:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc
  ```

---

## File structure

- **Create `src-tauri/src/feed_authority.rs`** — the entire S1 deliverable: `FeedAuthorityRecord`, `authority_binding_bytes`, `sign_authority_binding`, `verify_authority` (+ `VerifiedAuthority`), `FeedAuthorityCache` (+ `PinnedAuthority`, `IngestOutcome`), and the `#[cfg(test)]` module.
- **Modify `src-tauri/src/lib.rs`** — add `mod feed_authority;` beside the other `mod` declarations (near `mod vine_signing;` / `mod enrollment_verify;`).

Reused, unchanged: `crate::vine_signing::{push_str, push_u64}` (`pub(crate)`), `crate::enrollment_verify::{verify_enrollment_any_issuer, verify_revocation_any_issuer, EnrollmentVerifyError}`, `enrollment_verify::quorum_fixtures` (test cert/revocation minting), `harmony_identity::{PrivateIdentity, Identity}`.

**Test identity-threading (load-bearing for Tasks 3-4):** the cache tests build **multiple records on the same feed**, which requires reusing the *same* `#3` `PrivateIdentity` (same `feed_id`) across records. Do NOT rely on RNG-seed determinism. Instead generate one identity per feed and thread it explicitly — the test helper's real signature is `record_for(world, cert, signer_certs, revocation, updated_at_secs, n: &harmony_identity::PrivateIdentity)`, and same-feed tests create `let n = harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng);` once and pass `&n` to every record on that feed (`sign_authority_binding(&n, …)` recomputes the identical `feed_id`/`n_identity_pub` each call). The `n_seed: u8` params shown in the task test snippets are shorthand for "an identity generated for this feed" — replace them with an explicit threaded `&PrivateIdentity` when writing the tests. This keeps the tests RNG-agnostic and is what makes first-write-wins / sticky assertions exercise a genuinely shared feed identity.

---

### Task 1: `FeedAuthorityRecord` type + JSON round-trip + default-omitted encoding

**Files:**
- Create: `src-tauri/src/feed_authority.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod feed_authority;`)

**Interfaces:**
- Produces: `pub struct FeedAuthorityRecord { pub feed_id: String, pub owner_id: String, pub device_id: String, pub publisher_key: String, pub n_identity_pub: String, pub enrollment: EnrollmentCert, pub signer_certs: Vec<EnrollmentCert>, pub revocation: Option<RevocationCert>, pub updated_at: u64, pub n_sig: String }` (serde camelCase; `signer_certs`/`revocation` default+skip-if-empty/none).

- [ ] **Step 1: Register the module.** In `src-tauri/src/lib.rs`, add next to the other module declarations:

```rust
mod feed_authority;
```

- [ ] **Step 2: Write the failing test.** Create `src-tauri/src/feed_authority.rs` with the struct (below) plus this test module. The test builds a record with an empty bundle + no revocation and asserts the JSON omits `signerCerts`/`revocation`; a populated record includes them; and a round-trip is byte-identical after re-serialization.

```rust
use harmony_owner::certs::{EnrollmentCert, RevocationCert};
use serde::{Deserialize, Serialize};

/// Domain-separation prefix + version for the authority binding bytes.
const AUTHORITY_DOMAIN: &str = "harmony-vine-authority-v1";

/// ZEB-678 §3.1 — the per-feed record that owner-anchors a vine feed.
/// JSON on the wire (vines are `serde_json`, not CBOR). `n_sig` binds the
/// feed's `#3` node identity to an owner + `#2` publisher key; the optional
/// `revocation` marks the publisher device revoked.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedAuthorityRecord {
    /// Hex node address `N` — equals `hex(hash(n_identity_pub))` and the feed topic.
    pub feed_id: String,
    /// Hex 16-byte harmony-owner `owner_id` (O).
    pub owner_id: String,
    /// Hex 16-byte `EnrollmentCert.device_id` (D).
    pub device_id: String,
    /// Hex 32-byte enrolled `#2` ed25519 key (K).
    pub publisher_key: String,
    /// Hex 64-byte `#3` pubkey (X25519(32) || Ed25519(32)) whose hash is `feed_id`.
    pub n_identity_pub: String,
    /// Proves `publisher_key`/`device_id` are enrolled under `owner_id`.
    pub enrollment: EnrollmentCert,
    /// Quorum signer-cert bundle (empty ⇒ master-issued).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signer_certs: Vec<EnrollmentCert>,
    /// Present ⇒ the publisher device is revoked (self- or master-signed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation: Option<RevocationCert>,
    /// LWW clock (HLC wall_ms).
    pub updated_at: u64,
    /// Hex 64-byte `#3` signature over `authority_binding_bytes`.
    pub n_sig: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enrollment_verify::quorum_fixtures::mint_quorum_world;

    fn sample(revocation: Option<RevocationCert>, signer_certs: Vec<EnrollmentCert>) -> FeedAuthorityRecord {
        let world = mint_quorum_world(0x80);
        FeedAuthorityRecord {
            feed_id: "aa".into(),
            owner_id: hex::encode(world.owner_id),
            device_id: hex::encode(world.a_cert.device_id),
            publisher_key: hex::encode(world.a_cert.device_pubkeys.classical.ed25519_verify),
            n_identity_pub: "bb".into(),
            enrollment: world.a_cert.clone(),
            signer_certs,
            revocation,
            updated_at: 1_700_000_000_000,
            n_sig: "cc".into(),
        }
    }

    #[test]
    fn serde_omits_empty_signer_certs_and_revocation() {
        let json = serde_json::to_string(&sample(None, Vec::new())).unwrap();
        assert!(!json.contains("signerCerts"), "empty bundle must be omitted: {json}");
        assert!(!json.contains("revocation"), "None revocation must be omitted: {json}");
        assert!(json.contains("feedId") && json.contains("nSig"), "camelCase keys: {json}");
    }

    #[test]
    fn serde_includes_populated_optional_fields() {
        let world = mint_quorum_world(0x84);
        let rev = crate::enrollment_verify::quorum_fixtures::mint_quorum_revocation(
            &world, world.c_quorum_cert.device_id,
            crate::enrollment_verify::quorum_fixtures::WORLD_NOW,
        );
        let json = serde_json::to_string(&sample(Some(rev), world.bundle.clone())).unwrap();
        assert!(json.contains("signerCerts"), "populated bundle present: {json}");
        assert!(json.contains("revocation"), "Some revocation present: {json}");
    }

    #[test]
    fn json_round_trips() {
        let rec = sample(None, Vec::new());
        let json = serde_json::to_string(&rec).unwrap();
        let back: FeedAuthorityRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }
}
```

- [ ] **Step 3: Run the test to verify it compiles + passes.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(feed_authority)'`
Expected: 3 tests pass. (If `mint_quorum_world`/`mint_quorum_revocation` paths need adjusting, they live in `enrollment_verify::quorum_fixtures` — see `enrollment_verify.rs:179`.)

- [ ] **Step 4: Gate + commit.**

Run: `cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && ../scripts/test-select --context task`
(Paste the printed `round=…/bucket=…` line into the task note.)

```bash
git add src-tauri/src/feed_authority.rs src-tauri/src/lib.rs
git commit  # message: "feat(zeb-678-s1): FeedAuthorityRecord type (additive JSON, default-omitted optionals)" + trailer
```

---

### Task 2: Binding bytes + `#3` sign / verify (`n_sig`)

**Files:**
- Modify: `src-tauri/src/feed_authority.rs`

**Interfaces:**
- Consumes: `crate::vine_signing::{push_str, signer_address}`; `harmony_identity::{PrivateIdentity, Identity}`.
- Produces: `pub fn authority_binding_bytes(r: &FeedAuthorityRecord) -> Vec<u8>`; `pub fn sign_authority_binding(private: &harmony_identity::PrivateIdentity, r: &mut FeedAuthorityRecord)` (sets `feed_id`, `n_identity_pub`, `n_sig`); `pub(crate) fn verify_binding(r: &FeedAuthorityRecord) -> Result<(), String>`.

- [ ] **Step 1: Write the failing test.** Add to the `tests` module:

```rust
fn gen_identity(seed: u8) -> harmony_identity::PrivateIdentity {
    // Deterministic per seed via a seeded RNG so worlds don't collide.
    let mut rng = <rand::rngs::StdRng as rand::SeedableRng>::seed_from_u64(seed as u64);
    harmony_identity::PrivateIdentity::generate(&mut rng)
}

fn signed_record(seed_world: u8, seed_n: u8) -> (FeedAuthorityRecord, harmony_identity::PrivateIdentity) {
    let world = mint_quorum_world(seed_world);
    let n = gen_identity(seed_n);
    let mut rec = FeedAuthorityRecord {
        feed_id: String::new(),
        owner_id: hex::encode(world.owner_id),
        device_id: hex::encode(world.a_cert.device_id),
        publisher_key: hex::encode(world.a_cert.device_pubkeys.classical.ed25519_verify),
        n_identity_pub: String::new(),
        enrollment: world.a_cert.clone(),
        signer_certs: Vec::new(),
        revocation: None,
        updated_at: crate::enrollment_verify::quorum_fixtures::WORLD_NOW * 1000,
        n_sig: String::new(),
    };
    sign_authority_binding(&n, &mut rec);
    (rec, n)
}

#[test]
fn binding_signs_and_verifies() {
    let (rec, _n) = signed_record(0x88, 1);
    assert_eq!(rec.feed_id, hex::encode(_n.public_identity().address_hash));
    verify_binding(&rec).expect("valid binding verifies");
}

#[test]
fn binding_rejects_wrong_feed_id() {
    let (mut rec, _n) = signed_record(0x8C, 2);
    rec.feed_id = "00".repeat(20); // no longer matches hash(n_identity_pub)
    assert!(verify_binding(&rec).is_err());
}

#[test]
fn binding_rejects_tampered_bound_field() {
    let (mut rec, _n) = signed_record(0x90, 3);
    rec.owner_id = "11".repeat(16); // covered by n_sig ⇒ signature no longer matches
    assert!(verify_binding(&rec).is_err());
}
```

- [ ] **Step 2: Run to verify it fails** (functions undefined).

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(binding)'`
Expected: compile error / FAIL.

- [ ] **Step 3: Implement.** Add above the `tests` module:

```rust
use crate::vine_signing::{push_str, signer_address};

/// Length-prefixed bytes the `n_sig` covers — ONLY the immutable binding
/// fields (§3.1). `updated_at`/`revocation` are authenticated separately.
pub fn authority_binding_bytes(r: &FeedAuthorityRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    push_str(&mut out, AUTHORITY_DOMAIN);
    push_str(&mut out, &r.feed_id);
    push_str(&mut out, &r.owner_id);
    push_str(&mut out, &r.device_id);
    push_str(&mut out, &r.publisher_key);
    out
}

/// Set `feed_id` (= the `#3` address), `n_identity_pub`, and `n_sig` in
/// place. Mirrors `vine_signing::sign_descriptor`.
pub fn sign_authority_binding(
    private: &harmony_identity::PrivateIdentity,
    r: &mut FeedAuthorityRecord,
) {
    r.feed_id = signer_address(private);
    r.n_identity_pub = hex::encode(private.public_identity().to_public_bytes());
    let bytes = authority_binding_bytes(r);
    r.n_sig = hex::encode(private.sign(&bytes));
}

/// Verify the `#3` binding: `n_identity_pub` hashes to `feed_id`, and `n_sig`
/// is a strict Ed25519 signature over the binding bytes. Mirrors
/// `vine_signing::verify_signed`.
pub(crate) fn verify_binding(r: &FeedAuthorityRecord) -> Result<(), String> {
    let pub_vec = hex::decode(&r.n_identity_pub)
        .map_err(|e| format!("authority n_identity_pub not hex: {e}"))?;
    let identity = harmony_identity::Identity::from_public_bytes(&pub_vec)
        .map_err(|_| "authority n_identity_pub invalid".to_string())?;
    if hex::encode(identity.address_hash) != r.feed_id {
        return Err("authority n_identity_pub does not match feed_id".to_string());
    }
    let sig_bytes: [u8; 64] = hex::decode(&r.n_sig)
        .map_err(|e| format!("authority n_sig not hex: {e}"))?
        .try_into()
        .map_err(|_| "authority n_sig must be 64 bytes".to_string())?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    identity
        .verifying_key
        .verify_strict(&authority_binding_bytes(r), &sig)
        .map_err(|_| "authority binding signature invalid".to_string())
}
```

- [ ] **Step 4: Run to verify it passes.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(binding)'`
Expected: 3 pass. (If `rand::rngs::StdRng` needs the `rand` `std_rng` feature or a different RNG import, match whatever `vine_signing.rs`'s test module uses — `vine_signing.rs:229` uses `rand::rngs::OsRng`; if a deterministic seed is unavailable, use `OsRng` and drop the determinism.)

- [ ] **Step 5: Gate + commit** (same gate as Task 1).

```bash
git add src-tauri/src/feed_authority.rs
git commit  # "feat(zeb-678-s1): #3-signed authority binding (n_sig sign/verify)" + trailer
```

---

### Task 3: Full `verify_authority` (chokepoint-backed enrollment + revocation)

**Files:**
- Modify: `src-tauri/src/feed_authority.rs`

**Interfaces:**
- Consumes: `crate::enrollment_verify::{verify_enrollment_any_issuer, verify_revocation_any_issuer}`.
- Produces: `pub struct VerifiedAuthority { pub device_id: [u8;16], pub publisher_key: [u8;32], pub revoked: bool }`; `pub fn verify_authority(r: &FeedAuthorityRecord) -> Result<VerifiedAuthority, String>`.

- [ ] **Step 1: Write the failing test.** Add to `tests`:

```rust
use crate::enrollment_verify::quorum_fixtures::{mint_quorum_revocation, WORLD_NOW};
use harmony_owner::certs::{EnrollmentCert, RevocationCert, RevocationReason};

// Build a fully valid record for device `cert` under `world`, signed by `#3` `n`.
fn record_for(
    world: &crate::enrollment_verify::quorum_fixtures::QuorumWorld,
    cert: &EnrollmentCert,
    signer_certs: Vec<EnrollmentCert>,
    revocation: Option<RevocationCert>,
    updated_at_secs: u64,
    n_seed: u8,
) -> FeedAuthorityRecord {
    let n = gen_identity(n_seed);
    let mut rec = FeedAuthorityRecord {
        feed_id: String::new(),
        owner_id: hex::encode(world.owner_id),
        device_id: hex::encode(cert.device_id),
        publisher_key: hex::encode(cert.device_pubkeys.classical.ed25519_verify),
        n_identity_pub: String::new(),
        enrollment: cert.clone(),
        signer_certs,
        revocation,
        updated_at: updated_at_secs * 1000,
        n_sig: String::new(),
    };
    sign_authority_binding(&n, &mut rec);
    rec
}

#[test]
fn verify_authority_accepts_master_and_quorum() {
    let world = mint_quorum_world(0x94);
    let m = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, 10);
    let vm = verify_authority(&m).expect("master authority verifies");
    assert_eq!(vm.device_id, world.a_cert.device_id);
    assert!(!vm.revoked);
    let q = record_for(&world, &world.c_quorum_cert, world.bundle.clone(), None, WORLD_NOW, 11);
    verify_authority(&q).expect("quorum authority verifies with bundle");
}

#[test]
fn verify_authority_rejects_owner_mismatch() {
    let world = mint_quorum_world(0x98);
    let mut rec = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, 12);
    // Re-sign binding over a foreign owner_id; enrollment is still under world.owner_id.
    rec.owner_id = hex::encode([0xEEu8; 16]);
    let n = gen_identity(12);
    sign_authority_binding(&n, &mut rec); // rebind so binding is valid but owner claim is foreign
    assert!(verify_authority(&rec).is_err());
}

#[test]
fn verify_authority_rejects_publisher_key_and_device_id_mismatch() {
    let world = mint_quorum_world(0x9C);
    let mut pk = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, 13);
    pk.publisher_key = hex::encode([0x01u8; 32]);
    let n = gen_identity(13);
    sign_authority_binding(&n, &mut pk);
    assert!(verify_authority(&pk).is_err(), "publisher_key mismatch rejected");

    let mut did = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, 14);
    did.device_id = hex::encode([0x02u8; 16]);
    let n2 = gen_identity(14);
    sign_authority_binding(&n2, &mut did);
    assert!(verify_authority(&did).is_err(), "device_id mismatch rejected");
}

#[test]
fn verify_authority_rejects_expired_enrollment() {
    let world = mint_quorum_world(0xA0);
    // A master cert that expires before the record's updated_at/1000.
    let d_sk = ed25519_dalek::SigningKey::from_bytes(&[0xB4; 32]);
    let d_bundle = harmony_owner::pubkey_bundle::PubKeyBundle {
        classical: harmony_owner::pubkey_bundle::ClassicalKeys {
            ed25519_verify: d_sk.verifying_key().to_bytes(),
            x25519_pub: [0u8; 32],
        },
        post_quantum: None,
    };
    let d_id = d_bundle.identity_hash();
    let expiring = EnrollmentCert::sign_master(
        &world.master_sk, world.master_bundle.clone(), d_id, d_bundle,
        crate::enrollment_verify::quorum_fixtures::SIGNER_ISSUED_AT,
        Some(crate::enrollment_verify::quorum_fixtures::SIGNER_ISSUED_AT + 50),
    ).unwrap();
    let rec = record_for(&world, &expiring, Vec::new(), None, WORLD_NOW, 15);
    assert!(verify_authority(&rec).is_err(), "expired enrollment rejected");
}

#[test]
fn verify_authority_accepts_revocation_and_rejects_target_mismatch() {
    let world = mint_quorum_world(0xA4);
    let good_rev = mint_quorum_revocation(&world, world.a_cert.device_id, WORLD_NOW);
    let ok = record_for(&world, &world.a_cert, world.bundle.clone(), Some(good_rev), WORLD_NOW, 16);
    let v = verify_authority(&ok).expect("valid revocation verifies");
    assert!(v.revoked, "revocation sets revoked");

    let wrong_rev = mint_quorum_revocation(&world, world.c_quorum_cert.device_id, WORLD_NOW);
    let bad = record_for(&world, &world.a_cert, world.bundle.clone(), Some(wrong_rev), WORLD_NOW, 17);
    assert!(verify_authority(&bad).is_err(), "revocation targeting a different device rejected");
    let _ = RevocationReason::Lost; // keep import used if fixtures change
}
```

- [ ] **Step 2: Run to verify it fails.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_authority)'`
Expected: compile error / FAIL.

- [ ] **Step 3: Implement.** Add above `tests`:

```rust
use crate::enrollment_verify::{verify_enrollment_any_issuer, verify_revocation_any_issuer};

/// The verified core of an authority record: the pinned identity and whether
/// this record carries a valid revocation of that device.
#[derive(Debug, Clone, Copy)]
pub struct VerifiedAuthority {
    pub device_id: [u8; 16],
    pub publisher_key: [u8; 32],
    pub revoked: bool,
}

fn decode_hex16(s: &str, what: &str) -> Result<[u8; 16], String> {
    hex::decode(s)
        .map_err(|e| format!("authority {what} not hex: {e}"))?
        .try_into()
        .map_err(|_| format!("authority {what} must be 16 bytes"))
}

fn decode_hex32(s: &str, what: &str) -> Result<[u8; 32], String> {
    hex::decode(s)
        .map_err(|e| format!("authority {what} not hex: {e}"))?
        .try_into()
        .map_err(|_| format!("authority {what} must be 32 bytes"))
}

/// Full §4 verification of an authority record: (1) `#3` binding, (2) `#2`
/// enrollment through the chokepoint against the claimed owner (with
/// `publisher_key`/`device_id` cross-checks), (3) optional revocation.
pub fn verify_authority(r: &FeedAuthorityRecord) -> Result<VerifiedAuthority, String> {
    verify_binding(r)?;
    let owner_id = decode_hex16(&r.owner_id, "owner_id")?;
    let device_id = decode_hex16(&r.device_id, "device_id")?;
    let publisher_key = decode_hex32(&r.publisher_key, "publisher_key")?;

    let now_secs = r.updated_at / 1000;
    let verified = verify_enrollment_any_issuer(&r.enrollment, &r.signer_certs, Some(&owner_id), now_secs)
        .map_err(|e| format!("authority enrollment invalid: {e}"))?;
    if verified.device_ed25519 != publisher_key {
        return Err("authority publisher_key does not match enrollment device key".to_string());
    }
    if r.enrollment.device_id != device_id {
        return Err("authority device_id does not match enrollment".to_string());
    }

    let revoked = match &r.revocation {
        None => false,
        Some(rev) => {
            verify_revocation_any_issuer(rev, &r.enrollment, &r.signer_certs, rev.issued_at)
                .map_err(|e| format!("authority revocation invalid: {e}"))?;
            if rev.target != device_id {
                return Err("authority revocation target does not match device_id".to_string());
            }
            true
        }
    };
    Ok(VerifiedAuthority { device_id, publisher_key, revoked })
}
```

- [ ] **Step 4: Run to verify it passes.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_authority)'`
Expected: all pass.

- [ ] **Step 5: Gate + commit.**

```bash
git add src-tauri/src/feed_authority.rs
git commit  # "feat(zeb-678-s1): chokepoint-backed verify_authority (enrollment + revocation)" + trailer
```

---

### Task 4: `FeedAuthorityCache` — first-write-wins binding + sticky revoked

**Files:**
- Modify: `src-tauri/src/feed_authority.rs`

**Interfaces:**
- Consumes: `verify_authority`.
- Produces: `pub struct PinnedAuthority { pub device_id: [u8;16], pub publisher_key: [u8;32], pub n_identity_pub: String, pub revoked: bool, pub updated_at: u64 }`; `pub enum IngestOutcome { Pinned, RevokedSet, BenignRefresh, Dropped(String) }`; `pub struct FeedAuthorityCache` with `pub fn get(&self, feed_id: &str) -> Option<&PinnedAuthority>` and `pub fn ingest(&mut self, r: &FeedAuthorityRecord) -> IngestOutcome`.

- [ ] **Step 1: Write the failing test.** Add to `tests`:

```rust
#[test]
fn cache_pins_binding_first_write_wins() {
    let world = mint_quorum_world(0xA8);
    let mut cache = FeedAuthorityCache::default();
    let a = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, 20);
    assert_eq!(cache.ingest(&a), IngestOutcome::Pinned);
    // A second record for the SAME feed (same #3) but a DIFFERENT device is dropped.
    let mut b = record_for(&world, &world.b_cert, Vec::new(), None, WORLD_NOW + 1, 20);
    b.feed_id = a.feed_id.clone();
    b.n_identity_pub = a.n_identity_pub.clone();
    // Re-sign under the SAME #3 identity (seed 20) so the binding is valid but rebinds device.
    let n = gen_identity(20);
    // feed_id is set by sign; force it back to a's feed and re-sign covers new device_id.
    sign_authority_binding(&n, &mut b);
    assert!(matches!(cache.ingest(&b), IngestOutcome::Dropped(_)), "rebinding a pinned feed is dropped");
    assert_eq!(cache.get(&a.feed_id).unwrap().device_id, world.a_cert.device_id);
}

#[test]
fn cache_revocation_is_sticky_and_rollback_proof() {
    let world = mint_quorum_world(0xAC);
    let mut cache = FeedAuthorityCache::default();
    let active = record_for(&world, &world.a_cert, world.bundle.clone(), None, WORLD_NOW, 21);
    assert_eq!(cache.ingest(&active), IngestOutcome::Pinned);
    assert!(!cache.get(&active.feed_id).unwrap().revoked);

    // Same binding + a valid revocation ⇒ revoked set.
    let rev = mint_quorum_revocation(&world, world.a_cert.device_id, WORLD_NOW);
    let mut revoked_rec = record_for(&world, &world.a_cert, world.bundle.clone(), Some(rev), WORLD_NOW + 10, 21);
    revoked_rec.feed_id = active.feed_id.clone();
    revoked_rec.n_identity_pub = active.n_identity_pub.clone();
    let n = gen_identity(21);
    sign_authority_binding(&n, &mut revoked_rec);
    assert_eq!(cache.ingest(&revoked_rec), IngestOutcome::RevokedSet);
    assert!(cache.get(&active.feed_id).unwrap().revoked);

    // A newer record with NO revocation must NOT clear it (sticky).
    let mut newer = record_for(&world, &world.a_cert, world.bundle.clone(), None, WORLD_NOW + 20, 21);
    newer.feed_id = active.feed_id.clone();
    newer.n_identity_pub = active.n_identity_pub.clone();
    let n2 = gen_identity(21);
    sign_authority_binding(&n2, &mut newer);
    cache.ingest(&newer);
    assert!(cache.get(&active.feed_id).unwrap().revoked, "revoked stays sticky after a newer clean record");

    // An OLDER (rollback) clean record likewise cannot clear it.
    let mut older = record_for(&world, &world.a_cert, world.bundle.clone(), None, WORLD_NOW - 5, 21);
    older.feed_id = active.feed_id.clone();
    older.n_identity_pub = active.n_identity_pub.clone();
    let n3 = gen_identity(21);
    sign_authority_binding(&n3, &mut older);
    cache.ingest(&older);
    assert!(cache.get(&active.feed_id).unwrap().revoked, "rollback cannot un-revoke");
}

#[test]
fn cache_benign_refresh_advances_clock() {
    let world = mint_quorum_world(0xB0);
    let mut cache = FeedAuthorityCache::default();
    let a = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, 22);
    assert_eq!(cache.ingest(&a), IngestOutcome::Pinned);
    let mut refresh = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW + 100, 22);
    refresh.feed_id = a.feed_id.clone();
    refresh.n_identity_pub = a.n_identity_pub.clone();
    let n = gen_identity(22);
    sign_authority_binding(&n, &mut refresh);
    assert_eq!(cache.ingest(&refresh), IngestOutcome::BenignRefresh);
    assert_eq!(cache.get(&a.feed_id).unwrap().updated_at, (WORLD_NOW + 100) * 1000);
}

#[test]
fn cache_drops_invalid_record() {
    let world = mint_quorum_world(0xB4);
    let mut cache = FeedAuthorityCache::default();
    let mut bad = record_for(&world, &world.a_cert, Vec::new(), None, WORLD_NOW, 23);
    bad.n_sig = "00".repeat(64); // invalid signature
    assert!(matches!(cache.ingest(&bad), IngestOutcome::Dropped(_)));
    assert!(cache.get(&bad.feed_id).is_none());
}
```

> **Note for the executor:** the `first_write_wins` and sticky tests need a *second* valid record on the same `feed_id` (same `#3` identity) that either rebinds the device or advances the clock. `record_for` generates a fresh `#3` identity per `n_seed`, so pass the **same** `n_seed` and then overwrite `feed_id`/`n_identity_pub` from the first record and re-sign with that seed's identity (as shown) — `sign_authority_binding` recomputes `feed_id` from the identity, so same seed ⇒ same `feed_id`, and the re-sign covers the mutated `device_id`/`publisher_key`. Verify the helper produces a matching `feed_id` before asserting; adjust seeds if `mint_quorum_world` collides.

- [ ] **Step 2: Run to verify it fails.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(cache)'`
Expected: compile error / FAIL.

- [ ] **Step 3: Implement.** Add above `tests`:

```rust
use std::collections::HashMap;

/// The pinned per-feed state a follower keeps. The binding is set once
/// (first-write-wins); `revoked` is monotonic-true.
#[derive(Debug, Clone)]
pub struct PinnedAuthority {
    pub device_id: [u8; 16],
    pub publisher_key: [u8; 32],
    pub n_identity_pub: String,
    pub revoked: bool,
    pub updated_at: u64,
}

/// Outcome of feeding one authority record into the cache.
#[derive(Debug, PartialEq)]
pub enum IngestOutcome {
    /// First valid record for this feed — binding pinned.
    Pinned,
    /// A verified revocation flipped `revoked` false → true.
    RevokedSet,
    /// Agreeing record with a newer clock — clock advanced, nothing else.
    BenignRefresh,
    /// Invalid, a rebinding attempt, or a stale/no-op record.
    Dropped(String),
}

/// In-memory `feed_id → PinnedAuthority` cache (§4 step 4). No disk, no
/// engine wiring — S2/S3 add those.
#[derive(Debug, Default)]
pub struct FeedAuthorityCache {
    feeds: HashMap<String, PinnedAuthority>,
}

impl FeedAuthorityCache {
    pub fn get(&self, feed_id: &str) -> Option<&PinnedAuthority> {
        self.feeds.get(feed_id)
    }

    pub fn ingest(&mut self, r: &FeedAuthorityRecord) -> IngestOutcome {
        let verified = match verify_authority(r) {
            Ok(v) => v,
            Err(e) => return IngestOutcome::Dropped(format!("invalid: {e}")),
        };
        match self.feeds.get_mut(&r.feed_id) {
            None => {
                self.feeds.insert(
                    r.feed_id.clone(),
                    PinnedAuthority {
                        device_id: verified.device_id,
                        publisher_key: verified.publisher_key,
                        n_identity_pub: r.n_identity_pub.clone(),
                        revoked: verified.revoked,
                        updated_at: r.updated_at,
                    },
                );
                IngestOutcome::Pinned
            }
            Some(pinned) => {
                // First-write-wins: the binding never changes.
                if pinned.device_id != verified.device_id
                    || pinned.publisher_key != verified.publisher_key
                {
                    return IngestOutcome::Dropped("binding mismatch (first-write-wins)".to_string());
                }
                // Sticky revoked: a verified revocation flips it true, forever.
                if verified.revoked && !pinned.revoked {
                    pinned.revoked = true;
                    pinned.updated_at = pinned.updated_at.max(r.updated_at);
                    return IngestOutcome::RevokedSet;
                }
                // Benign refresh: advancing clock on an agreeing record. Never clears revoked.
                if r.updated_at > pinned.updated_at {
                    pinned.updated_at = r.updated_at;
                    IngestOutcome::BenignRefresh
                } else {
                    IngestOutcome::Dropped("stale (no clock advance)".to_string())
                }
            }
        }
    }
}
```

- [ ] **Step 4: Run to verify it passes.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(feed_authority)'`
Expected: the full `feed_authority` suite passes.

- [ ] **Step 5: Full gate + commit.**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check \
 && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
 && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: clean fmt, zero clippy warnings, full suite green.

```bash
git add src-tauri/src/feed_authority.rs
git commit  # "feat(zeb-678-s1): FeedAuthorityCache (first-write-wins binding, sticky revoked)" + trailer
```

---

## Post-plan: PR

After all four tasks are green + the full sweep passes, open the S1 PR to `zeblithic/harmony-client` (base `main`), fire `@coderabbitai review` ONCE at open, and converge per the standing loop rules (scan all three comment buckets; Qodo auto-re-reviews on push; never trigger Greptile; never auto-merge). PR body: link ZEB-678, summarize S1 (authority record type + `#3` binding + chokepoint verify + LWW/sticky cache, data+verify only), and note S2 (signing migration) + S3 (revocation wiring) follow.
