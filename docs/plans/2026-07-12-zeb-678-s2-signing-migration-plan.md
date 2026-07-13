# ZEB-678 S2 — Signing migration + migration marker: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate vine descriptor / follow-list / tombstone / reaction publishing from the feed's `#3` node key to the enrolled `#2` device key (`community_signing_key`), publish + maintain each feed's `FeedAuthorityRecord` on first migrated publish, self-stamp the active binding into the fleet-net row, and add a dual-path ingest verifier (authority-cached ⇒ require `#2` + `!revoked`; else legacy `#3`) so a revoked device's post-revocation records are rejected once its feed has migrated.

**Architecture:** Additive, backward-compatible. `vine_signing.rs` grows `-v2` domain constants + `sign_*_v2`/`verify_*_v2` producing/checking a new optional `device_sig` field (raw `ed25519_dalek` `#2` signatures, not `PrivateIdentity`). The four wire structs (`VineDescriptorPayload`/`VineFollowListPayload` in `lib.rs`, `VineReactionPayload` in `lib.rs`, `VineTombstonePayload` in `vine_tombstone.rs`) gain `device_sig`; the reaction additionally carries owner-anchoring cert material (cbor-hex, mirroring S1) so it self-verifies cross-actor. The publish paths reach `#2` via `guard.dm_outbox` (a `tokio::Mutex<DmOutbox>` behind the `std::Mutex<NodeState>`) and, on first migrated publish, build the feed's `FeedAuthorityRecord` (S1 `feed_authority.rs`) + publish it to `harmony/vines/{N}/authority` + stamp `feed_binding` into `FleetNetRow`. Ingest gains a `FeedAuthorityCache` on `VineFeedCache`, an `on_authority_sample` fed by a new `harmony/vines/*/authority` subscription, and dual-path branches in the four `on_*_sample` verifiers.

**Tech Stack:** Rust, `serde`/`serde_json` (vines are JSON), `ed25519_dalek` (`#2` raw signing), `harmony_identity::PrivateIdentity` (`#3`, retained for legacy verify + `n_sig`), `harmony_owner::certs` (`EnrollmentCert`), the S1 `feed_authority` module, `ciborium`+`hex` for cbor-hex certs.

## Global Constraints

- Spec: `docs/specs/2026-07-12-zeb-678-vine-follow-revocation-design.md` §3.2–3.5, §4, §5, §9, §10 S2. S2 is **signing migration + dual-path verify + authority publish/maintain + fleet-net self-stamp**. NO `revoke_device` wiring and NO `DevicesPanel` copy — those are S3.
- **Additive JSON only, no `FILE_VERSION` bump.** Every new field is `#[serde(default, skip_serializing_if = …)]` so old builds ignore it and the default-omitted encoding stays byte-identical. Signatures are wire-only (never persisted). The `FleetNetRow.feed_binding` addition follows the S4 petname precedent (short `#[serde(rename)]` + `default` + `skip_serializing_if`).
- **`-v2` domain separation:** the `#2` signatures cover the **same canonical field set** as the `#3` ones under bumped domain constants (`harmony-vine-descriptor-v2`, `-reaction-v2`, `-follows-v2`, `-tombstone-v2`). `feed_id`/`creator_address`/`owner_address` is already inside those bytes, so a `device_sig` cannot be replayed onto another feed.
- **Legacy fields retained.** `identity_pub`/`sig` (`#3`) stay on every struct for backward verify; migrated records simply stop *populating* them (leave `None`) except the tombstone, whose `#3` sig fields are required `String` and stay populated (a tombstone is "migrated" iff `device_sig` is present — it dual-signs).
- **Reach `#2` correctly (lock ordering).** `community_signing_key: Arc<ed25519_dalek::SigningKey>` and `enrollment_cert: EnrollmentCert` live on `DmOutbox` behind a `tokio::Mutex`, reachable only via `guard.dm_outbox` (`Option<Arc<tokio::Mutex<DmOutbox>>>`, `NodeState:875`). Publish paths must **clone the `dm_outbox` Arc out under the std `NodeState` lock, drop the std guard, then `.lock().await`** the tokio mutex to obtain `#2` + the cert. Never hold the std lock across an `.await`.
- **`#2`-unavailable fallback.** If `dm_outbox` is `None` (device not yet enrolled / pre-outbox boot), publish falls back to the legacy `#3` path with a `warn!` and does NOT publish an authority record. A feed with no authority record ingests via the legacy path (honest residual, §8).
- **Master-issued self-publish.** The self-published `FeedAuthorityRecord` uses the device's own `enrollment_cert` with **empty `signer_certs`** (the `dm_outbox` carries no signer bundle). This migrates master-issued devices (the fleet). A quorum-enrolled device whose own cert is quorum-issued would fail `verify_enrollment_any_issuer` with an empty bundle and therefore not migrate (stays legacy `#3`) — acceptable §8 residual; strengthening it is a §11 follow-up.
- **Verifier-controlled clock.** `verify_authority`/`FeedAuthorityCache::ingest`/`verify_reaction_v2` take `now_secs` supplied by the ingest boundary (derive from the same wall clock the descriptor path already builds as `now_ms`), never from a record field.
- **Publisher-key invariant.** At authority-build time assert `community_signing_key.verifying_key().to_bytes() == enrollment_cert.device_pubkeys.classical.ed25519_verify` (the `#2` signing key must correspond to the enrolled publisher key) — a mismatch is a build error, never published.
- **Keychain untouched (ZEB-428):** the `#2` key is already loaded at boot; no new keychain access.
- Gates (harmony-client `CLAUDE.md`): per-task `scripts/test-select --context task` (paste the `round=…/bucket=…` summary line into the task note); `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` before the PR opens. Frontend unaffected (no UI in S2). All cargo commands run from `src-tauri/`.
- Commit trailer on every commit:

  ```text
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc
  ```

---

## File structure

- **Modify `src-tauri/src/vine_signing.rs`** — `-v2` domain constants; `descriptor_canonical_bytes_v2`/`reaction_canonical_bytes_v2`/`follow_list_canonical_bytes_v2`; `sign_descriptor_v2`/`sign_reaction_v2`/`sign_follow_list_v2`; `verify_descriptor_v2`/`verify_reaction_v2`/`verify_follow_list_v2`; the shared `verify_device_sig` helper. Tests for each.
- **Modify `src-tauri/src/lib.rs`** — add `device_sig` to `VineDescriptorPayload`/`VineFollowListPayload`; add `device_sig` + owner-anchoring (`owner_id`/`enrollment_cbor_hex`/`signer_certs_cbor_hex`) to `VineReactionPayload`; the four publish paths (`publish_vine_descriptor`, `publish_vine_reaction_impl`, `build_signed_follow_list_with` + caller, `delete_vine_impl`) switch to `#2`; the authority-record builder + publish helper + fleet-net self-stamp; a `vine_authority_published` gate on `NodeState`.
- **Modify `src-tauri/src/vine_tombstone.rs`** — `device_sig` field on `VineTombstonePayload`; `-v2` canonical bytes; `sign_tombstone_v2`/`verify_tombstone_v2`.
- **Modify `src-tauri/src/feed_authority.rs`** — widen `decode_cert`/`decode_certs` to `pub(crate)` so the reaction verifier reuses them; add a public `build_active_authority(...)` constructor (feed builder) used by the publish path.
- **Modify `src-tauri/src/fleet_net.rs`** — `feed_binding: Option<FeedAuthorityRecord>` additive field on `FleetNetRow`; CBOR round-trip test.
- **Modify `src-tauri/src/vine_feed_cache.rs`** — `authority: FeedAuthorityCache` field on `VineFeedCache`; `on_authority_sample`; dual-path branches + `now_secs` threading in `on_descriptor_sample`/`on_reaction_sample`/`on_tombstone_sample`/`on_follow_list_sample`.
- **Modify `src-tauri/src/event_loop.rs`** — new `harmony/vines/*/authority` subscription (beside the others at `:2968–3017`); a `…/authority` routing arm in `emit_frontend_event` (before the descriptor fallthrough at `:8315`); hoist a single `now_secs` for all vine sample calls.

Reused, unchanged: `crate::vine_signing::{push_str, push_u64, push_bool, push_opt_str}` (`pub(crate)`), `crate::enrollment_verify::verify_enrollment_any_issuer`, `crate::feed_authority::{FeedAuthorityRecord, FeedAuthorityCache, sign_authority_binding, encode_cert, encode_certs}`.

**Cross-task interface summary (names later tasks rely on):**
- `vine_signing::verify_descriptor_v2(d: &VineDescriptorPayload, publisher_key: &[u8; 32]) -> Result<(), String>`
- `vine_signing::sign_descriptor_v2(sk: &ed25519_dalek::SigningKey, d: &mut VineDescriptorPayload)`
- `vine_signing::verify_follow_list_v2(p: &VineFollowListPayload, publisher_key: &[u8; 32]) -> Result<(), String>`
- `vine_signing::sign_follow_list_v2(sk: &ed25519_dalek::SigningKey, p: &mut VineFollowListPayload)`
- `vine_signing::verify_reaction_v2(r: &VineReactionPayload, now_secs: u64) -> Result<(), String>` (standalone, owner-anchored)
- `vine_signing::sign_reaction_v2(sk: &ed25519_dalek::SigningKey, r: &mut VineReactionPayload)`
- `vine_tombstone::verify_tombstone_v2(t: &VineTombstonePayload, publisher_key: &[u8; 32]) -> Result<(), String>`
- `vine_tombstone::sign_tombstone_v2(sk: &ed25519_dalek::SigningKey, t: &mut VineTombstonePayload)`
- `feed_authority::build_active_authority(node_identity: &harmony_identity::PrivateIdentity, sk: &ed25519_dalek::SigningKey, cert: &EnrollmentCert, updated_at_ms: u64) -> Result<FeedAuthorityRecord, String>`
- `feed_authority::decode_cert`/`decode_certs` become `pub(crate)`.
- `VineFeedCache.authority: FeedAuthorityCache`; `VineFeedCache::on_authority_sample(&mut self, key_expr: &str, payload: &[u8], now_secs: u64)`.

---

### Task 1: `-v2` domains + descriptor `#2` sign/verify + `device_sig` field

**Files:**
- Modify: `src-tauri/src/lib.rs` (`VineDescriptorPayload`, `:13883-13915`)
- Modify: `src-tauri/src/vine_signing.rs`

**Interfaces:**
- Produces: `sign_descriptor_v2`, `verify_descriptor_v2`, `descriptor_canonical_bytes_v2`, `verify_device_sig` (`pub(crate)`), constant `DESCRIPTOR_DOMAIN_V2`.
- Consumes: existing `push_str`/`push_u64`/`push_opt_str`, `VineDescriptorPayload`.

- [ ] **Step 1: Add the `device_sig` field.** In `lib.rs` `VineDescriptorPayload` (after the existing `sig` field):

```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_sig: Option<String>,
```

- [ ] **Step 2: Write the failing test** in `vine_signing.rs` `#[cfg(test)]`:

```rust
#[test]
fn descriptor_v2_sign_verify_roundtrip_and_wrong_key_rejected() {
    let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let pk = sk.verifying_key().to_bytes();
    let mut d = sample_descriptor(); // helper builds a minimal VineDescriptorPayload
    assert!(d.device_sig.is_none());
    sign_descriptor_v2(&sk, &mut d);
    assert!(d.device_sig.is_some());
    verify_descriptor_v2(&d, &pk).expect("valid #2 signature");

    let wrong = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng).verifying_key().to_bytes();
    assert!(verify_descriptor_v2(&d, &wrong).is_err(), "wrong publisher key rejected");

    // -v2 domain isolation: a v1 #3 signature is not a valid device_sig
    let mut d2 = sample_descriptor();
    sign_descriptor(&harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng), &mut d2);
    assert!(verify_descriptor_v2(&d2, &pk).is_err(), "missing device_sig rejected");
}
```

- [ ] **Step 3: Run it, expect fail** (`sign_descriptor_v2` undefined): `cargo nextest run --features test-fixtures -E 'test(descriptor_v2_sign_verify)'`.

- [ ] **Step 4: Implement** in `vine_signing.rs` (near the v1 constants + builders):

```rust
use ed25519_dalek::Signer as _; // brings SigningKey::sign into scope

const DESCRIPTOR_DOMAIN_V2: &str = "harmony-vine-descriptor-v2";

/// Same field set as `descriptor_canonical_bytes`, under the `-v2` domain.
pub fn descriptor_canonical_bytes_v2(d: &VineDescriptorPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    push_str(&mut out, DESCRIPTOR_DOMAIN_V2);
    push_str(&mut out, &d.id);
    push_str(&mut out, &d.creator_address);
    push_str(&mut out, &d.creator_name);
    push_u64(&mut out, d.created_at);
    push_str(&mut out, &d.video_cid);
    push_opt_str(&mut out, &d.title);
    push_opt_str(&mut out, &d.reshare_of);
    push_opt_str(&mut out, &d.original_creator_address);
    push_opt_str(&mut out, &d.original_creator_name);
    out
}

/// Sign a descriptor with the enrolled `#2` device key (sets `device_sig`).
pub fn sign_descriptor_v2(sk: &ed25519_dalek::SigningKey, d: &mut VineDescriptorPayload) {
    let bytes = descriptor_canonical_bytes_v2(d);
    d.device_sig = Some(hex::encode(sk.sign(&bytes).to_bytes()));
}

/// Verify a `#2` `device_sig` against the feed's authority `publisher_key`.
pub fn verify_descriptor_v2(d: &VineDescriptorPayload, publisher_key: &[u8; 32]) -> Result<(), String> {
    verify_device_sig(d.device_sig.as_deref(), publisher_key, &descriptor_canonical_bytes_v2(d), "descriptor")
}

/// Shared `#2` signature check: hex `device_sig` verified against `publisher_key`.
pub(crate) fn verify_device_sig(
    device_sig: Option<&str>,
    publisher_key: &[u8; 32],
    canonical: &[u8],
    what: &str,
) -> Result<(), String> {
    let sig = device_sig.ok_or_else(|| format!("{what} has no device signature"))?;
    let sig_bytes: [u8; 64] = hex::decode(sig)
        .map_err(|e| format!("{what} device_sig not hex: {e}"))?
        .try_into()
        .map_err(|_| format!("{what} device_sig wrong length"))?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(publisher_key)
        .map_err(|_| format!("{what} publisher key invalid"))?;
    vk.verify_strict(canonical, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
        .map_err(|_| format!("{what} device signature invalid"))
}
```

Add a `sample_descriptor()` test helper if one does not already exist (mirror the existing descriptor test fixtures around `:420`).

- [ ] **Step 5: Run to green**, then `scripts/test-select --context task` (paste the summary line). **Commit.**

---

### Task 2: follow-list `#2` sign/verify + `device_sig` field

**Files:**
- Modify: `src-tauri/src/lib.rs` (`VineFollowListPayload`, `:14014-14031`)
- Modify: `src-tauri/src/vine_signing.rs`

**Interfaces:**
- Produces: `sign_follow_list_v2`, `verify_follow_list_v2`, `follow_list_canonical_bytes_v2`, `FOLLOW_LIST_DOMAIN_V2`.

- [ ] **Step 1:** Add `device_sig: Option<String>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`) to `VineFollowListPayload`.

- [ ] **Step 2: Failing test** (mirror Task 1, plus the omission pin the spec §9 calls for):

```rust
#[test]
fn follow_list_v2_sign_verify_and_device_sig_omitted_when_none() {
    let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let pk = sk.verifying_key().to_bytes();
    let mut p = VineFollowListPayload {
        owner_address: "aabb".into(), follows: vec!["ccdd".into()], updated_at: 1_700_000_300,
        identity_pub: None, sig: None, device_sig: None,
    };
    let json_before = serde_json::to_value(&p).unwrap();
    assert!(json_before.get("deviceSig").is_none(), "deviceSig omitted when None");
    sign_follow_list_v2(&sk, &mut p);
    verify_follow_list_v2(&p, &pk).expect("valid");
    assert!(serde_json::to_value(&p).unwrap().get("deviceSig").is_some());
    let wrong = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng).verifying_key().to_bytes();
    assert!(verify_follow_list_v2(&p, &wrong).is_err());
}
```

- [ ] **Step 3: Run, expect fail.**

- [ ] **Step 4: Implement** `follow_list_canonical_bytes_v2` (domain `FOLLOW_LIST_DOMAIN_V2 = "harmony-vine-follows-v2"`, same field order as v1: `owner_address` ‖ `push_u64(updated_at)` ‖ `u32-LE follows.len()` ‖ each `push_str`), `sign_follow_list_v2` (sets `device_sig`), `verify_follow_list_v2` (forwards to `verify_device_sig`).

- [ ] **Step 5: Green → `scripts/test-select --context task` → Commit.**

---

### Task 3: reaction `#2` sign + owner-anchored standalone verify

**Files:**
- Modify: `src-tauri/src/lib.rs` (`VineReactionPayload`, `:13977-13996`)
- Modify: `src-tauri/src/vine_signing.rs`
- Modify: `src-tauri/src/feed_authority.rs` (widen `decode_cert`/`decode_certs` to `pub(crate)`)

**Interfaces:**
- Produces: `sign_reaction_v2`, `verify_reaction_v2` (standalone, owner-anchored), `reaction_canonical_bytes_v2`, `REACTION_DOMAIN_V2`.
- Consumes: `feed_authority::{decode_cert, decode_certs}` (now `pub(crate)`), `enrollment_verify::verify_enrollment_any_issuer`.

- [ ] **Step 1: Add owner-anchoring fields** to `VineReactionPayload` (cbor-hex, mirroring `FeedAuthorityRecord` — `EnrollmentCert` does not `serde_json` round-trip):

```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,          // hex-16 reactor owner_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment_cbor_hex: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signer_certs_cbor_hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_sig: Option<String>,
```

- [ ] **Step 2: Widen decoders.** In `feed_authority.rs` change `fn decode_cert`/`fn decode_certs` to `pub(crate) fn`.

- [ ] **Step 3: Failing test** (reuse `enrollment_verify::quorum_fixtures` — master + quorum):

```rust
#[test]
fn reaction_v2_self_verifies_master_and_quorum() {
    use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
    let world = mint_quorum_world(0xC1);
    // device #2 signing key = the enrolled A-cert's device key
    let sk = world.a_sk.clone();
    let pk = sk.verifying_key().to_bytes();
    let mut r = VineReactionPayload {
        vine_id: "vine-1".into(), reactor_address: "aabb".into(), reactor_name: "A".into(),
        liked: true, timestamp: 1_700_000_000, identity_pub: None, sig: None,
        owner_id: Some(hex::encode(world.owner_id)),
        enrollment_cbor_hex: Some(crate::feed_authority::encode_cert(&world.a_cert).unwrap()),
        signer_certs_cbor_hex: String::new(), // master-issued
        device_sig: None,
    };
    sign_reaction_v2(&sk, &mut r);
    verify_reaction_v2(&r, WORLD_NOW).expect("master-issued reaction self-verifies");
    // publisher-key mismatch: tamper device_sig by re-signing with a different key
    let mut bad = r.clone();
    sign_reaction_v2(&ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng), &mut bad);
    assert!(verify_reaction_v2(&bad, WORLD_NOW).is_err(), "device_sig not matching enrolled key rejected");
    let _ = pk;
}
```

(If `quorum_fixtures` exposes a quorum `c_sk`/`c_quorum_cert`, add a second case building `signer_certs_cbor_hex` from the signer bundle and asserting it verifies.)

- [ ] **Step 4: Run, expect fail.**

- [ ] **Step 5: Implement** in `vine_signing.rs`:

```rust
const REACTION_DOMAIN_V2: &str = "harmony-vine-reaction-v2";

pub fn reaction_canonical_bytes_v2(r: &VineReactionPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    push_str(&mut out, REACTION_DOMAIN_V2);
    push_str(&mut out, &r.vine_id);
    push_str(&mut out, &r.reactor_address);
    push_str(&mut out, &r.reactor_name);
    push_bool(&mut out, r.liked);
    push_u64(&mut out, r.timestamp);
    out
}

pub fn sign_reaction_v2(sk: &ed25519_dalek::SigningKey, r: &mut VineReactionPayload) {
    let bytes = reaction_canonical_bytes_v2(r);
    r.device_sig = Some(hex::encode(sk.sign(&bytes).to_bytes()));
}

/// Standalone owner-anchored verify: recover the reactor's enrolled `#2` key from
/// its carried enrollment (chokepoint), then check `device_sig` against it.
pub fn verify_reaction_v2(r: &VineReactionPayload, now_secs: u64) -> Result<(), String> {
    let owner_hex = r.owner_id.as_deref().ok_or("reaction missing owner_id")?;
    let owner_id: [u8; 16] = hex::decode(owner_hex)
        .map_err(|e| format!("reaction owner_id not hex: {e}"))?
        .try_into().map_err(|_| "reaction owner_id wrong length".to_string())?;
    let enrollment = crate::feed_authority::decode_cert(
        r.enrollment_cbor_hex.as_deref().ok_or("reaction missing enrollment")?,
    )?;
    let signer_certs = crate::feed_authority::decode_certs(&r.signer_certs_cbor_hex)?;
    let verified = crate::enrollment_verify::verify_enrollment_any_issuer(
        &enrollment, &signer_certs, Some(&owner_id), now_secs,
    ).map_err(|e| format!("reaction enrollment invalid: {e}"))?;
    verify_device_sig(r.device_sig.as_deref(), &verified.device_ed25519,
        &reaction_canonical_bytes_v2(r), "reaction")
}
```

- [ ] **Step 6: Green → `scripts/test-select --context task` → Commit.**

---

### Task 4: tombstone `#2` sign/verify + `device_sig` field

**Files:**
- Modify: `src-tauri/src/vine_tombstone.rs` (`VineTombstonePayload`, `:25`)

**Interfaces:**
- Produces: `vine_tombstone::sign_tombstone_v2`, `vine_tombstone::verify_tombstone_v2`, `TOMBSTONE_DOMAIN_V2`.
- Note: the tombstone keeps its required `#3` sig fields (dual-signs); `device_sig` is the additive optional migration marker.

- [ ] **Step 1: Add `device_sig: Option<String>`** (`#[serde(default, skip_serializing_if = "Option::is_none")]`) to `VineTombstonePayload`.

- [ ] **Step 2: Failing test** (mirror Task 1; sign with a fresh `#2` key, verify against its pubkey, reject wrong key; assert the existing `#3` `verify_tombstone` still passes on the same record — dual-sign).

- [ ] **Step 3: Run, expect fail.**

- [ ] **Step 4: Implement** `TOMBSTONE_DOMAIN_V2 = "harmony-vine-tombstone-v2"`, a `tombstone_canonical_bytes_v2` covering the same fields as the existing `#3` scheme under the `-v2` domain (reuse this module's existing length-prefix helpers), `sign_tombstone_v2(sk, t)` (sets `device_sig`), `verify_tombstone_v2(t, publisher_key)` (mirror `vine_signing::verify_device_sig` — either call it via `crate::vine_signing::verify_device_sig` made `pub(crate)` across modules, or inline the same check).

- [ ] **Step 5: Green → `scripts/test-select --context task` → Commit.**

---

### Task 5: `feed_authority::build_active_authority` + `FleetNetRow.feed_binding`

**Files:**
- Modify: `src-tauri/src/feed_authority.rs` (new public builder)
- Modify: `src-tauri/src/fleet_net.rs` (`FleetNetRow`, `:38-47`)

**Interfaces:**
- Produces: `feed_authority::build_active_authority(node_identity, sk, cert, updated_at_ms) -> Result<FeedAuthorityRecord, String>`; `FleetNetRow.feed_binding: Option<FeedAuthorityRecord>`.

- [ ] **Step 1: Failing test** for the builder in `feed_authority.rs` (verify the built record passes `verify_authority`):

```rust
#[test]
fn build_active_authority_produces_verifiable_record() {
    use crate::enrollment_verify::quorum_fixtures::{mint_quorum_world, WORLD_NOW};
    let world = mint_quorum_world(0xD2);
    let n = harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng);
    let sk = world.a_sk.clone(); // enrolled #2 key matching world.a_cert
    let rec = build_active_authority(&n, &sk, &world.a_cert, WORLD_NOW * 1000)
        .expect("builds");
    assert_eq!(rec.feed_id, hex::encode(n.public_identity().address_hash));
    assert!(rec.revocation_cbor_hex.is_none(), "active binding has no revocation");
    let v = verify_authority(&rec, WORLD_NOW).expect("self-built record verifies");
    assert!(!v.revoked);
}
```

- [ ] **Step 2: Run, expect fail.**

- [ ] **Step 3: Implement the builder** in `feed_authority.rs`:

```rust
/// Build a device's own *active* `FeedAuthorityRecord` for its feed `N`
/// (no revocation). `node_identity` is the feed's `#3` key (hashes to `N` and
/// signs `n_sig`); `sk` is the enrolled `#2` device key; `cert` is this device's
/// own enrollment. Master-issued self-publish: `signer_certs` is empty.
pub fn build_active_authority(
    node_identity: &harmony_identity::PrivateIdentity,
    sk: &ed25519_dalek::SigningKey,
    cert: &EnrollmentCert,
    updated_at_ms: u64,
) -> Result<FeedAuthorityRecord, String> {
    let publisher_key = sk.verifying_key().to_bytes();
    // Publisher-key invariant: the #2 signing key must be the enrolled key.
    if cert.device_pubkeys.classical.ed25519_verify != publisher_key {
        return Err("community_signing_key does not match enrollment publisher key".into());
    }
    let mut rec = FeedAuthorityRecord {
        feed_id: String::new(),      // set by sign_authority_binding
        owner_id: hex::encode(cert.owner_id),
        device_id: hex::encode(cert.device_id),
        publisher_key: hex::encode(publisher_key),
        n_identity_pub: String::new(), // set by sign_authority_binding
        enrollment_cbor_hex: encode_cert(cert)?,
        signer_certs_cbor_hex: String::new(),
        revocation_cbor_hex: None,
        updated_at: updated_at_ms,
        n_sig: String::new(),        // set by sign_authority_binding
    };
    sign_authority_binding(node_identity, &mut rec);
    Ok(rec)
}
```

- [ ] **Step 4: Add the fleet-net field** (mirror the S4 petname precedent `pt` at `fleet_net.rs:81`):

```rust
    #[serde(rename = "fb", default, skip_serializing_if = "Option::is_none")]
    pub feed_binding: Option<FeedAuthorityRecord>,
```

Update every `FleetNetRow { … }` literal in `fleet_net.rs` and `lib.rs` (boot self-row `:5522`, rebind self-row `:8719`, and any test constructors) to add `feed_binding: None`.

- [ ] **Step 5: CBOR round-trip test** in `fleet_net.rs` (the survey flagged nesting the camelCase-JSON `FeedAuthorityRecord` inside a CBOR canonical row — pin that it survives, since all its fields are `String`/`u64`):

```rust
#[test]
fn fleet_net_row_feed_binding_cbor_roundtrips() {
    let mut row = FleetNetRow { /* existing fields */, feed_binding: None };
    // encode/decode via the CanonicalPayload path used by the doc
    let bytes = row.to_canonical_bytes();       // or the module's encoder
    let back = FleetNetRow::from_canonical_bytes(&bytes).unwrap();
    assert_eq!(row, back);
    row.feed_binding = Some(/* a sample FeedAuthorityRecord */);
    let bytes2 = row.to_canonical_bytes();
    assert_eq!(FleetNetRow::from_canonical_bytes(&bytes2).unwrap(), row);
}
```

(Use whatever encode/decode entry `CanonicalPayload` exposes; confirm `feed_binding: None` omits the `fb` key so pre-migration rows are byte-identical.)

- [ ] **Step 6: Green → `scripts/test-select --context task` → Commit.**

---

### Task 6: publish paths switch to `#2` + authority publish/maintain + fleet-net stamp

**Files:**
- Modify: `src-tauri/src/lib.rs` (`publish_vine_descriptor` `:14042`, `publish_vine_reaction_impl` `:14332`, `build_signed_follow_list_with` `:15134` + its caller, `delete_vine_impl` `:14431`, `NodeState` gate field)

**Interfaces:**
- Consumes: `vine_signing::{sign_descriptor_v2, sign_reaction_v2, sign_follow_list_v2}`, `vine_tombstone::sign_tombstone_v2`, `feed_authority::build_active_authority`, `feed_authority::{encode_cert}` (reaction cert attach).
- Produces: `NodeState.vine_authority_published: bool`; helper `publish_feed_authority_if_needed(state, publish_tx) -> Result<(), String>`.

- [ ] **Step 1: Add the gate field.** On `NodeState`, add `pub vine_authority_published: bool` (default `false` in every constructor).

- [ ] **Step 2: Write the `#2`-reach helper** (used by all four paths) — a free async fn that returns the `#2` material or `None`:

```rust
/// Clone the dm_outbox Arc out under the std lock, then async-lock it to obtain
/// (#2 signing key, this device's enrollment cert). None ⇒ device not enrolled
/// (fall back to legacy #3).
async fn vine_publisher_material(
    state: &Mutex<NodeState>,
) -> Option<(Arc<ed25519_dalek::SigningKey>, harmony_owner::certs::EnrollmentCert)> {
    let outbox = { state.lock().ok()?.dm_outbox.clone()? };
    let g = outbox.lock().await;
    Some((g.community_signing_key.clone(), g.enrollment_cert.clone()))
}
```

- [ ] **Step 3: Descriptor path** (`publish_vine_descriptor`): after cloning `publish_tx` + `#3 identity` under the std lock, call `vine_publisher_material(state).await`. If `Some((sk, cert))`: `sign_descriptor_v2(&sk, &mut descriptor)` (leave `identity_pub`/`sig` `None`), then `publish_feed_authority_if_needed(state, &publish_tx, &identity, &sk, &cert).await?`. If `None`: keep the existing `sign_descriptor(&identity, &mut descriptor)` (legacy) + `warn!`. Keep the `creator_address == signer_addr` divergence guard (the `#3` identity still anchors the feed id `N`).

- [ ] **Step 4: `publish_feed_authority_if_needed`** — publishes the authority record + stamps fleet-net, once:

```rust
async fn publish_feed_authority_if_needed(
    state: &Mutex<NodeState>,
    publish_tx: &tokio::sync::mpsc::Sender<event_loop::PublishRequest>,
    identity: &harmony_identity::PrivateIdentity,
    sk: &ed25519_dalek::SigningKey,
    cert: &harmony_owner::certs::EnrollmentCert,
) -> Result<(), String> {
    // fast path: already published this boot
    if state.lock().map_err(|e| format!("lock: {e}"))?.vine_authority_published { return Ok(()); }
    let now_ms = /* same HLC wall_ms the ingest path uses */;
    let rec = feed_authority::build_active_authority(identity, sk, cert, now_ms)?;
    let key_expr = format!("harmony/vines/{}/authority", rec.feed_id);
    let payload = serde_json::to_vec(&rec).map_err(|e| format!("serialize authority: {e}"))?;
    // publish (best-effort, same PublishRequest path as descriptors)
    send_publish(publish_tx, key_expr, payload).await?;
    // self-stamp into the fleet-net row + notify_dirty, and set the gate
    {
        let mut g = state.lock().map_err(|e| format!("lock: {e}"))?;
        if let Some(doc) = g.fleet_net_doc.as_ref() { /* set this device's row.feed_binding = Some(rec.clone()); refresh snapshot */ }
        g.vine_authority_published = true;
    }
    if let Some(sync) = /* g.fleet_net_sync */ { sync.notify_dirty(); }
    Ok(())
}
```

Fill the fleet-net stamp by mirroring the boot self-row write at `lib.rs:5522-5542` (insert/modify this device's `FleetNetRow`, refresh `fleet_net_snapshot`, `fleet_net_sync.notify_dirty()`).

- [ ] **Step 5: Reaction path** (`publish_vine_reaction_impl`): reach `#2`; on `Some`, set `wire.owner_id = Some(hex::encode(cert.owner_id))`, `wire.enrollment_cbor_hex = Some(feed_authority::encode_cert(&cert)?)`, `wire.signer_certs_cbor_hex = String::new()`, then `sign_reaction_v2(&sk, &mut wire)` (leave `#3` `None`), and `publish_feed_authority_if_needed(...)` for the reactor's own feed. On `None`, legacy `sign_reaction`.

- [ ] **Step 6: Follow-list path** (`build_signed_follow_list_with` is **sync**): thread the `#2` material in from the async caller. Change the caller to obtain `(sk, cert)` via `vine_publisher_material` before taking the std guard, pass `Option<&SigningKey>` into `build_signed_follow_list_with`; inside, if `Some(sk)` → `sign_follow_list_v2(sk, &mut payload)`, else legacy `sign_follow_list`. Trigger `publish_feed_authority_if_needed` from the async caller after building the list.

- [ ] **Step 7: Tombstone path** (`delete_vine_impl`): reach `#2`; on `Some`, after the existing `sign_tombstone` (keep — `#3` fields are required), also `sign_tombstone_v2(&sk, &mut tomb)` (dual-sign; `device_sig` is the migration marker), and `publish_feed_authority_if_needed(...)`. On `None`, legacy only.

- [ ] **Step 8: Test** — a focused unit/integration test that a migrated descriptor carries `device_sig` and no `#3` `sig`, and that `publish_feed_authority_if_needed` sets the gate + produces a `verify_authority`-valid record (drive the builder directly if the publish channel is awkward to mock). Reuse existing `publish_*` test scaffolding if present; otherwise assert at the `build_active_authority` + `sign_*_v2` seam.

- [ ] **Step 9: Green → `scripts/test-select --context task` → Commit.**

---

### Task 7: dual-path ingest verifier on `VineFeedCache`

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs` (`VineFeedCache` struct `:306-331`; `on_descriptor_sample` `:532`, `on_reaction_sample` `:731`, `on_tombstone_sample` `:833`, `on_follow_list_sample` `:962`; new `on_authority_sample`)

**Interfaces:**
- Produces: `VineFeedCache.authority: FeedAuthorityCache`; `on_authority_sample(&mut self, key_expr, payload, now_secs)`; `now_secs` param added to the reaction/tombstone/follow sample methods.
- Consumes: `feed_authority::{FeedAuthorityCache, FeedAuthorityRecord}`, the `verify_*_v2` fns.

- [ ] **Step 1: Add the field.** `authority: FeedAuthorityCache` on `VineFeedCache` (derive `Default` already present — `FeedAuthorityCache: Default`). Persistence: authority cache is in-memory only (like S1), not written to `path`.

- [ ] **Step 2: Failing tests** (the S2 acceptance core):

```rust
#[test]
fn descriptor_legacy_accepted_pre_authority_and_rejected_post_authority() {
    let mut cache = VineFeedCache::default_for_test();
    let n = harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng);
    let feed = hex::encode(n.public_identity().address_hash);
    // 1) legacy #3 descriptor on this feed, no authority cached -> accepted
    let mut d = descriptor_on_feed(&feed);
    crate::vine_signing::sign_descriptor(&n, &mut d);
    assert!(cache.on_descriptor_sample(&format!("harmony/vines/{feed}"),
        &serde_json::to_vec(&d).unwrap(), &followed(&feed), NOW_MS).is_accepted());
    // 2) ingest the feed's authority record (device #2 = world.a) -> migrates
    let (rec, sk) = authority_and_key_for(&n); // build_active_authority + its #2 sk
    cache.on_authority_sample(&format!("harmony/vines/{feed}/authority"),
        &serde_json::to_vec(&rec).unwrap(), NOW_SECS);
    // 3) a #3-only descriptor is now REJECTED (feed migrated)
    let mut d3 = descriptor_on_feed(&feed);
    crate::vine_signing::sign_descriptor(&n, &mut d3);
    assert!(!cache.on_descriptor_sample(&format!("harmony/vines/{feed}"),
        &serde_json::to_vec(&d3).unwrap(), &followed(&feed), NOW_MS).is_accepted(),
        "#3-only rejected once authority cached");
    // 4) a #2 descriptor is accepted
    let mut d2 = descriptor_on_feed(&feed);
    crate::vine_signing::sign_descriptor_v2(&sk, &mut d2);
    assert!(cache.on_descriptor_sample(&format!("harmony/vines/{feed}"),
        &serde_json::to_vec(&d2).unwrap(), &followed(&feed), NOW_MS).is_accepted());
}
```

(Match the real `on_descriptor_sample` return type — inspect it and assert on the actual accept/reject signal, e.g. whether the descriptor lands in the cache via a follow-up `get`.)

- [ ] **Step 3: Run, expect fail.**

- [ ] **Step 4: Implement `on_authority_sample`:**

```rust
/// Ingest an authority record from `harmony/vines/{N}/authority`.
pub fn on_authority_sample(&mut self, key_expr: &str, payload: &[u8], now_secs: u64) {
    let feed = match key_expr.strip_prefix("harmony/vines/").and_then(|s| s.strip_suffix("/authority")) {
        Some(f) if !f.contains('/') => f,
        _ => return, // not an authority topic
    };
    let rec: FeedAuthorityRecord = match serde_json::from_slice(payload) {
        Ok(r) => r, Err(e) => { tracing::warn!(error=%e, "drop malformed authority record"); return; }
    };
    if rec.feed_id != feed { tracing::warn!("authority feed_id != topic; dropping"); return; }
    let _ = self.authority.ingest(&rec, now_secs); // outcome logged inside if desired
}
```

- [ ] **Step 5: Dual-path in the four verifiers.** In `on_descriptor_sample`, after computing `topic_creator` (= feed `N`), replace the bare `verify_descriptor` call with:

```rust
let now_secs = now_ms / 1000;
match self.authority.get(topic_creator) {
    Some(p) if p.revoked => { tracing::warn!("descriptor on revoked feed; rejecting"); return /* reject signal */; }
    Some(p) => crate::vine_signing::verify_descriptor_v2(&descriptor, &p.publisher_key),
    None => crate::vine_signing::verify_descriptor(&descriptor),
}
```

Apply the equivalent to `on_follow_list_sample` (feed = topic owner, `verify_follow_list_v2`) and `on_tombstone_sample` (feed = topic creator, `vine_tombstone::verify_tombstone_v2`). For `on_reaction_sample` use the **standalone** reaction rule: if `device_sig` present → `verify_reaction_v2(&reaction, now_secs)` then best-effort revocation (reject only if `self.authority.get(reactor_address)` is `Some(p)` with `p.revoked`); else legacy `verify_reaction`.

- [ ] **Step 6: Thread `now_secs`.** Add a `now_secs: u64` parameter to `on_reaction_sample`/`on_tombstone_sample`/`on_follow_list_sample` (descriptor already has `now_ms`). Update all existing test call sites in `vine_feed_cache.rs`.

- [ ] **Step 7: Green → `scripts/test-select --context task` → Commit.**

---

### Task 8: wire the authority sub-topic subscription + routing

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (subscriptions `:2968-3017`; `emit_frontend_event` vines arm `:8315-8481`)

**Interfaces:**
- Consumes: `VineFeedCache::on_authority_sample`.

- [ ] **Step 1: Add the subscription** beside the existing four (after the `…/follows` sub at `:3017`):

```rust
    // ZEB-678: subscribe to per-feed authority records (owner-anchoring +
    // revocation). Own key space — `harmony/vines/*` matches only `{owner}`.
    if let Err(e) = subscribe_tx.send(SubscribeRequest {
        key_expr: "harmony/vines/*/authority".to_string(),
        ..
    }) { /* mirror the sibling subscriptions' error handling */ }
```

- [ ] **Step 2: Add the routing arm** in `emit_frontend_event`, inside the `key_expr.starts_with("harmony/vines/")` block, **before** the descriptor fallthrough (place after the `/reactions/` arm at `:8356`):

```rust
        if key_expr.ends_with("/authority") {
            match vine_feed_cache.lock() {
                Ok(mut cache) => cache.on_authority_sample(key_expr, payload, now_secs),
                Err(e) => tracing::warn!(error=%e, "vine authority cache poisoned"),
            }
            return; // handled
        }
```

- [ ] **Step 3: Hoist `now_secs`.** Compute the wall-clock `now_ms` once at the top of the vines arm (the descriptor path already builds it at `:8390`); derive `now_secs = now_ms / 1000` and pass it into `on_authority_sample`, `on_reaction_sample`, `on_tombstone_sample`, `on_follow_list_sample` (per Task 7's new signatures). Keep passing `now_ms` to `on_descriptor_sample`.

- [ ] **Step 4: Test** — if `event_loop.rs` has an `emit_frontend_event` routing test harness, add a case asserting a `…/authority` key routes to `on_authority_sample` (and a `…/authority` with a trailing extra segment does not). Otherwise rely on the `vine_feed_cache.rs` Task 7 integration test (which drives `on_authority_sample` directly) and note the routing is a thin dispatch.

- [ ] **Step 5: Green → `scripts/test-select --context round` → Commit.**

---

## Pre-PR full sweep (before opening the PR)

- [ ] `cd src-tauri && cargo fmt --all -- --check`
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [ ] Confirm default-omitted encodings unchanged (the new omission tests) — no `FILE_VERSION` bump anywhere.
- [ ] Open PR to `zeblithic/harmony-client`; fire `@coderabbitai review` once at open; bot converge per standing loop rules.

## Self-review checklist (run after writing, before executing)

1. **Spec coverage:** §3.2 `device_sig` (T1/2/4), §3.3 reaction owner-anchoring (T3), §3.4 `-v2` domains (T1–4), §3.5 fleet-net self-stamp (T5/6), §4 dual-path + marker (T7/8), §5 publish switch + authority maintain (T6). ✅
2. **Type consistency:** `verify_*_v2(_, &[u8;32])` vs `verify_reaction_v2(_, now_secs)` (reaction recovers its own key). `publisher_key` read off `&PinnedAuthority` from `authority.get`. `build_active_authority` returns a record whose `publisher_key == sk.verifying_key()`.
3. **No placeholders:** each task has real code for the load-bearing fn + a concrete test; mechanical mirror-tasks (T2/T4) reference T1's shown code by exact analogy.

## §11 follow-up tickets (file at implementation end)

1. Quorum-enrolled self-publish: the authority builder currently uses empty `signer_certs` (master-issued only). File a ticket to thread a quorum device's signer bundle so its feed can migrate.
2. Authority maintenance across `#2`/epoch rotation (S2 publishes once per boot).
