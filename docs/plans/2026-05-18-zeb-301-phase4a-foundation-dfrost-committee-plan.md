# ZEB-301 Phase 4a-Foundation: D-FROST Committee (DKG + Threshold VRF + Proactive Refresh)

> **Branch:** `zeb-301-phase4a-foundation-dfrost-committee`
> **Linear:** [ZEB-301](https://linear.app/zeblith/issue/ZEB-301) | Parent: [ZEB-293](https://linear.app/zeblith/issue/ZEB-293) | Umbrella: [ZEB-289](https://linear.app/zeblith/issue/ZEB-289)
> **Date:** 2026-05-18
>
> **For agentic implementers:** implement task-by-task using subagent-driven-development. Each task except Task 0 ends with a commit. Do NOT begin Phase 4a-main integration (sortition/STAR/drafting/UI) from this branch.

---

## 1. Goal + Architecture

**Goal:** Ship the cryptographic foundation for Harmony's D-FROST committee: distributed key generation (DKG) via FROST-Ristretto255, a threshold VRF beacon derived from the joint Schnorr key, and proactive share refresh that rotates member shares without changing the group public key. No voting-layer integration lands in this PR.

**Architecture (3 sentences):** A dedicated `community_dfrost_log.rs` (parallel to `community_voting_log.rs`) stores signed committee events per community in a `DfrostLog`, with five event kinds (`dr/dk/ts/vb/rf`) encoded in the same 8-field 2-char-key CBOR envelope as voting events. The `frost_ristretto255` crate (Zcash Foundation, Apache-2.0) provides `dkg::part1/part2/part3` for DKG, `round1::commit` + `round2::sign` + `aggregate` for threshold signing, and `keys::refresh::compute_refreshing_shares` + `refresh_dkg_shares` for proactive share refresh. The VRF output is `SHA-256(b"dfrost-vrf-v1" || R_compressed)` where `R` is the Schnorr nonce from `frost::aggregate`, making it publicly verifiable via `VerifyingKey::verify` with no additional ZK machinery.

**Tech Stack:** Rust 1.88 / frost-ristretto255 2.x / ciborium / Ed25519 (envelope sig) / X25519 via existing `dm_signing::seal_to_owner` (round-2 package encryption) / SHA-256 / nextest / tokio. No frontend in this PR.

---

## 2. File Structure

| File | Action | Responsibility | Est. LoC |
|---|---|---|---|
| `src-tauri/Cargo.toml` | Modify | Add `frost-ristretto255 = { version = "2", features = ["serde"] }` | +3 |
| `src-tauri/src/community_dfrost_types.rs` | Create | Wire types: `DfrostEventKind`, `SignedCommitteeEvent`, all 5 payload structs, `derive_ceremony_id`, `derive_vrf_seed`, `derive_vrf_output` | ~320 |
| `src-tauri/src/community_dfrost_log.rs` | Create | `DfrostLog`, `CommitteeState`, `PendingCeremony`, `PendingSignSession`, `apply()` dispatcher + all 5 kind handlers, `apply_with_identity` (round-2 decrypt) | ~480 |
| `src-tauri/src/community_dfrost_crypto.rs` | Create | FROST wrappers: `dkg_part1_local`, `dkg_part2_local`, `dkg_part3_local`, `threshold_sign_round1/round2`, `build_signing_package`, `aggregate_signatures`, `verify_schnorr_signature`, `compute_refresh_shares`, `identifier_for_index`, `verifying_key_to_bytes` | ~420 |
| `src-tauri/src/lib.rs` | Modify | `pub mod community_dfrost_{types,log,crypto}`; `dfrost_logs` field on `NodeState`; 5 `#[tauri::command]` IPCs; wire into `start_node`/`stop_inner` | ~130 |
| `src-tauri/tests/wire_format_dfrost_fixtures.rs` | Create | Byte-pinned CBOR fixtures for 5 event envelopes + key payload structs (regen-on-first-run pattern) | ~240 |
| `src-tauri/tests/dfrost_dkg_integration.rs` | Create | 2-of-2 DKG ceremony full convergence; 2-of-3 threshold sign + VRF derivation; proactive refresh epoch bump; two-engine apply_with_identity symmetry | ~430 |

**Total estimated new LoC: ~2020**

---

## 3. Wire Format

### 3.1 Envelope (all 5 event kinds share this structure)

`SignedCommitteeEvent` — 8-field CBOR map, all 2-char keys, structurally isomorphic to `SignedVotingEvent`:

```
tg: 'd'            // tag char for dfrost (vs 'p' for poll)
vr: 1u8            // version
tr: 0u8            // committee tier (not a voting tier; 0 is unused by Tier enum)
kd: <DfrostEventKind>  // 2-char string: "dr"|"dk"|"ts"|"vb"|"rf"
hc: <Hlc>          // HLC timestamp
ac: <OwnerAddr>    // actor (signer of this event)
pd: <bstr>         // CBOR-encoded kind-specific payload
sg: <bstr>         // Ed25519 sig over canonical CBOR of (tg,vr,tr,kd,hc,ac,pd); 64 bytes
```

### 3.2 `"dr"` — DKG Round (rn=1 commitments; rn=2 encrypted shares)

```
DkgRoundPayload {
  "ci": bstr[32],     // ceremony_id = SHA-256(community_id || wall_ms_le8 || b"dkg-v1")
  "rn": 1 | 2,        // round number
  // rn=1 only (skip_serializing_if None):
  "pk": bstr,         // ciborium-encoded dkg::round1::Package
  // rn=2 only (skip_serializing_if None):
  "rc": [{ "rc": OwnerAddr, "ct": bstr }],  // RecipientCiphertext list (seal_to_owner)
}
```

Round-1: 3 keys (`ci`, `rn`, `pk`). Round-2: 3 keys (`ci`, `rn`, `rc`). Inner `rc` list entries reuse the existing `RecipientCiphertext` type from `community_membership.rs`.

### 3.3 `"dk"` — DKG Complete

```
DkgCompletePayload {
  "ci": bstr[32],     // ceremony_id
  "vk": bstr[32],     // joint VerifyingKey compressed Ristretto point
  "vs": [{ "id": OwnerAddr, "vk": bstr[32] }],  // per-member VerifyingShare bytes
  "ep": u64,          // proposed epoch (= current_epoch + 1)
  "mb": [OwnerAddr],  // sorted committee member list
  "th": u16,          // threshold (min_signers)
  "mx": u16,          // max_signers
}
```

Reused for proactive refresh completion (same epoch-bump + invariant that `vk` bytes are unchanged).

### 3.4 `"ts"` — Threshold Sign Contribution

```
ThresholdSignPayload {
  "ci": bstr[32],   // signing ceremony_id = SHA-256(b"ts-v1" || poll_event_hash || epoch_le8)
  "ms": bstr[32],   // VRF seed message = derive_vrf_seed(poll_event_hash, epoch)
  "cm": bstr,       // ciborium-encoded round1::SigningCommitments
  "sh": bstr,       // ciborium-encoded round2::SignatureShare
}
```

One `ts` event per committee member per signing ceremony. Bundles both rounds (coordinator waits for `threshold` `cm` values before computing SigningPackage; members produce `sh` in the same event after seeing the coordinator's SigningPackage — see §8.4 for the two-step IPC flow).

### 3.5 `"vb"` — VRF Beacon

```
VrfBeaconPayload {
  "ci": bstr[32],   // signing ceremony_id
  "ms": bstr[32],   // VRF seed message (same as ThresholdSignPayload.ms)
  "sg": bstr[64],   // aggregated Schnorr signature: R_compressed(32) || s(32)
  "vf": bstr[32],   // VRF output = SHA-256(b"dfrost-vrf-v1" || R_compressed)
}
```

Note: the `"sg"` key in `VrfBeaconPayload` is the FROST Schnorr signature, distinct from the `"sg"` key in the outer `SignedCommitteeEvent` envelope (which is the actor's Ed25519 event signature).

### 3.6 `"rf"` — Proactive Refresh

```
RefreshRoundPayload {
  "ci": bstr[32],   // refresh ceremony_id = SHA-256(b"rf-v1" || community_id || wall_ms_le8)
  "rn": 1 | 2,
  // rn=1 only (encrypted new SecretShare from coordinator, one per member):
  "rc": [{ "rc": OwnerAddr, "ct": bstr }],
  // rn=2 only (encrypted refresh_dkg_part2 package, for fully-distributed refresh):
  "pk": bstr,
}
```

---

## 4. `CommitteeState` and `DfrostLog`

`DfrostLog` lives in `NodeState.dfrost_logs: Arc<Mutex<HashMap<SpaceId, Arc<Mutex<DfrostLog>>>>>`, parallel to `voting_logs`. NOT added to `CommunityState` (different concern; avoids complicating the membership CRDT's Clone/PartialEq impls).

```rust
pub struct DfrostLog {
    pub events: Vec<SignedCommitteeEvent>,
    // serde(skip) — derived from events, not persisted
    pub committee_state: CommitteeState,
    // In-memory only (never persisted): holds SecretPackages between DKG rounds
    pub local_dkg_secret: Option<frost_ristretto255::keys::dkg::round1::SecretPackage>,
    pub local_dkg_secret2: Option<frost_ristretto255::keys::dkg::round2::SecretPackage>,
    pub local_key_package: Option<frost_ristretto255::keys::KeyPackage>,
    pub local_pub_key_package: Option<frost_ristretto255::keys::PublicKeyPackage>,
    pub local_signing_nonces: HashMap<[u8; 32], frost_ristretto255::round1::SigningNonces>,
}

pub struct CommitteeState {
    pub active: bool,
    pub current_epoch: u64,
    pub joint_verifying_key: Option<[u8; 32]>,     // compressed Ristretto point
    pub verifying_shares: BTreeMap<OwnerAddr, [u8; 32]>,
    pub members: Vec<OwnerAddr>,                    // sorted
    pub threshold: u16,
    pub max_signers: u16,
    // serde(skip) — derived from members on load
    pub identifier_map: BTreeMap<OwnerAddr, frost_ristretto255::Identifier>,
    pub pending_dkg: Option<PendingCeremony>,
    pub pending_sign: BTreeMap<[u8; 32], PendingSignSession>,  // keyed by ceremony_id
    pub pending_refresh: Option<PendingCeremony>,
}

pub struct PendingCeremony {
    pub ceremony_id: [u8; 32],
    pub round1_packages: BTreeMap<OwnerAddr, Vec<u8>>,   // actor → r1 pkg bytes
    pub round2_packages: BTreeMap<OwnerAddr, Vec<u8>>,   // sender → decrypted r2 pkg bytes
    pub dk_confirmations: BTreeMap<OwnerAddr, [u8; 32]>, // actor → vk_bytes
    pub proposed_epoch: u64,
    pub members: Vec<OwnerAddr>,
    pub threshold: u16,
    pub max_signers: u16,
}

pub struct PendingSignSession {
    pub message_hash: [u8; 32],
    pub contributions: BTreeMap<OwnerAddr, (Vec<u8>, Vec<u8>)>, // actor → (cm, sh) bytes
}
```

`CommitteeState` identifier_map field contains `frost_ristretto255::Identifier` which is not itself serializable via ciborium. It must be `#[serde(skip)]` and rebuilt by `build_identifier_map(&members)` on every deserialization of `CommitteeState`. Add a custom `Deserialize` impl or a post-deserialize hook via `#[serde(from = "CommitteeStateRaw")]`.

Simplest pattern: define `CommitteeStateRaw` without `identifier_map`; implement `From<CommitteeStateRaw> for CommitteeState` that calls `build_identifier_map`. Use `#[serde(from = "CommitteeStateRaw")]` on `CommitteeState`.

---

## 5. DKG Ceremony Sequencing

```
Admin IPC:  dfrost_initiate_dkg(community_id, members, threshold)
            → computes ceremony_id
            → calls dkg_part1_local(self_identifier, max_signers, min_signers)
            → stores (SecretPackage) in DfrostLog.local_dkg_secret
            → signs + posts dr(rn=1) event with round1_package bytes

Apply path: dr(rn=1) from each member → pending_dkg.round1_packages[actor] = bytes

Member IPC: dfrost_contribute_round2(community_id, ceremony_id)
            → waits for: pending_dkg.round1_packages.len() >= threshold
            → calls dkg_part2_local(local_dkg_secret, received_r1_packages)
            → stores SecretPackage2 in DfrostLog.local_dkg_secret2
            → for each other member: seal_to_owner(their_x25519_pub, r2_pkg_bytes)
            → signs + posts dr(rn=2) event with recipient_ciphertexts

Apply path: dr(rn=2) via apply_with_identity → decrypts own ciphertext
            → pending_dkg.round2_packages[sender] = decrypted_bytes

Auto-trigger (or explicit IPC): when round2_packages.len() >= threshold - 1:
            → dkg_part3_local(local_dkg_secret2, all_r1_pkgs, received_r2_pkgs)
            → obtains (KeyPackage, PublicKeyPackage)
            → stores in DfrostLog.local_key_package / local_pub_key_package
            → signs + posts dk event (joint_vk, verifying_shares, epoch, members, threshold, max)

Apply path: dk event → pending_dkg.dk_confirmations[actor] = vk_bytes
            → when dk_confirmations.len() >= threshold: finalize CommitteeState
            → active=true, current_epoch=proposed_epoch, joint_verifying_key=vk
            → verifying_shares populated, identifier_map rebuilt, pending_dkg=None
```

**Conflict detection:** if `dk_confirmations` contains two different `vk` bytes values, `apply_dkg_complete` returns `ApplyError::InvariantViolation` and clears `pending_dkg`. Nodes must re-run the ceremony.

---

## 6. VRF Derivation

```
vrf_seed = SHA-256(b"dfrost-vrf-v1" || poll_create_signing_bytes_hash[32] || community_epoch_le8[8])

signing_ceremony_id = SHA-256(b"ts-v1" || vrf_seed)

Threshold signing protocol:
  Each committee member i:
    (nonces_i, commitments_i) = round1::commit(key_package.signing_share(), OsRng)
    posts ts event with {ci=signing_ceremony_id, ms=vrf_seed, cm=commitments_i, sh=<pending>}

  Coordinator (admin):
    collects threshold ts events with their cm values
    builds SigningPackage from all commitments + message=vrf_seed
    distributes SigningPackage (via separate IPC or bundled in next step)

  Each member i (second IPC call after coordinator publishes SigningPackage):
    sh_i = round2::sign(signing_package, nonces_i, key_package)
    posts updated ts event OR coordinator collects sh_i directly (foundation: coordinator
    calls dfrost_request_vrf_beacon which re-triggers each member's round2::sign locally)

  Coordinator:
    aggregated_sig = aggregate(signing_package, {id_i: sh_i, ...}, pub_key_package)
    R_compressed = aggregated_sig.serialize()[..32]  // first 32 bytes
    vrf_output = SHA-256(b"dfrost-vrf-v1" || R_compressed)
    posts vb event {ci, ms, sg=aggregated_sig.serialize(), vf=vrf_output}

Verification by any observer:
  VerifyingKey::verify(vrf_seed, schnorr_sig) → Ok
  SHA-256(b"dfrost-vrf-v1" || sig[..32]) == vb.vf  → output binding confirmed
```

**Foundation simplification:** in the foundation phase, the IPC `dfrost_request_vrf_beacon` performs both rounds: it runs `round1::commit` + stores nonces, waits for threshold commitments (collected from `ts` events in the log), builds the `SigningPackage`, runs `round2::sign`, posts a `ts` event with both `cm` and `sh`. The coordinator aggregates when threshold `ts` events with `sh` are present.

---

## 7. Proactive Refresh Protocol

**Trigger:** admin calls `dfrost_propose_refresh(community_id, new_members, new_threshold)`.

**Protocol (foundation phase — coordinator-mediated):**

```
Coordinator:
  new_members_sorted = sort(new_members)
  new_identifiers = [Identifier::try_from(1), Identifier::try_from(2), ...]
  (secret_shares, new_pub_key_pkg) = compute_refreshing_shares(old_pub_key_pkg, &new_identifiers, OsRng)
  For each member m_i with secret_shares[i]:
    sealed_i = seal_to_owner(m_i.x25519_pub, cbor(secret_shares[i]))
  Posts rf(rn=1) event with recipient_ciphertexts

Each member m_i on receiving rf(rn=1):
  Calls dfrost_contribute_round2(community_id, refresh_ceremony_id)
  Decrypts own SecretShare via open_from_owner
  Uses SecretShare + old_key_package to compute new_key_package:
    refresh_dkg_shares(r2_secret_pkg, r1_pkgs, r2_pkgs, old_pub_key_pkg, old_key_package)
    (Foundation simplification: call keys::split to get new KeyPackage from SecretShare)
  Stores new_key_package in DfrostLog.local_key_package
  Posts dk event with {ci=refresh_ci, vk=OLD_VK (unchanged!), vs=new_verifying_shares, ep=current_epoch+1}

Finalization:
  When threshold dk events confirm with same vk bytes AND ep == current_epoch + 1:
  CommitteeState.current_epoch bumped; members/threshold updated if changed
  joint_verifying_key MUST remain equal to old value (InvariantViolation if different)
```

**Invariant:** `apply_dkg_complete` checks: if `committee_state.active` AND `committee_state.joint_verifying_key == Some(existing_vk)` AND `payload.joint_verifying_key != existing_vk` → `Err(ApplyError::InvariantViolation)`.

---

## 8. IPC Surface

All 5 IPCs declared as `#[tauri::command]` in `src-tauri/src/lib.rs`. No frontend wiring in this PR (Phase 4a-main consumer).

### 8.1 `dfrost_initiate_dkg`
```rust
#[tauri::command]
async fn dfrost_initiate_dkg(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,      // hex-encoded SpaceId (32 hex chars)
    members: Vec<String>,      // hex-encoded OwnerAddrs (including self)
    threshold: u16,
) -> Result<String, String>    // Ok: hex-encoded ceremony_id[32]; Err: message
```
JS: `invoke('dfrost_initiate_dkg', { communityId, members, threshold }): Promise<string>`

Admin-only (checks calling node's power ≥ 100 in community_state). Runs `dkg_part1_local`, stores `SecretPackage` in `DfrostLog.local_dkg_secret`, signs + stores `dr(rn=1)` event.

### 8.2 `dfrost_contribute_round2`
```rust
#[tauri::command]
async fn dfrost_contribute_round2(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
    ceremony_id: String,       // hex-encoded [u8; 32]
) -> Result<(), String>
```
JS: `invoke('dfrost_contribute_round2', { communityId, ceremonyId }): Promise<void>`

Errors if: no pending ceremony, round1_packages count < threshold, already posted round-2. Runs `dkg_part2_local`, encrypts to each other member, posts `dr(rn=2)`. Also attempts `dkg_part3_local` if own round-2 packages have arrived for all other members.

### 8.3 `dfrost_finalize_dkg`
```rust
#[tauri::command]
async fn dfrost_finalize_dkg(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
    ceremony_id: String,
) -> Result<(), String>
```
JS: `invoke('dfrost_finalize_dkg', { communityId, ceremonyId }): Promise<void>`

Runs `dkg_part3_local` if all round-2 packages available, posts `dk` event. Idempotent.

### 8.4 `dfrost_request_vrf_beacon`
```rust
#[tauri::command]
async fn dfrost_request_vrf_beacon(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
    poll_event_hash: String,   // hex-encoded SHA-256 of PollCreate signing bytes
) -> Result<String, String>    // Ok: hex-encoded vrf_output[32]; Err: "pending:N/M" or error
```
JS: `invoke('dfrost_request_vrf_beacon', { communityId, pollEventHash }): Promise<string>`

Admin-only. Computes `vrf_seed`, checks if `ts` contributions ≥ threshold: if yes, aggregates + posts `vb` event + returns VRF output hex. If not, runs `round1::commit` + `round2::sign` (stores nonces), posts local `ts` event, returns `Err("pending:N/M")`.

### 8.5 `dfrost_propose_refresh`
```rust
#[tauri::command]
async fn dfrost_propose_refresh(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
    new_members: Vec<String>,
    new_threshold: u16,
) -> Result<String, String>    // Ok: hex-encoded refresh ceremony_id
```
JS: `invoke('dfrost_propose_refresh', { communityId, newMembers, newThreshold }): Promise<string>`

Admin-only. Calls `compute_refresh_shares`, encrypts per-member, posts `rf(rn=1)`.

**Note:** `dfrost_contribute_round2` is reused for both DKG and refresh round-2 posting (it inspects `pending_dkg` vs `pending_refresh` to determine which ceremony is active).

---

## 9. Resolved Open Questions

| # | Question | Decision |
|---|---|---|
| 1 | DKG round storage: voting log or dedicated log? | **Dedicated `community_dfrost_log.rs`**. Voting log's `tg='p'` and `PollEventKindCode` are poll-specific; mixing would require polluting poll semantics with ceremony events. |
| 2 | FROST Identifier → OwnerAddr binding? | **Sequential index**: sort members by `OwnerAddr` bytes, `Identifier::try_from(idx+1 as u16)`. Frozen at ceremony time; refresh with new membership produces new identifier mapping. |
| 3 | VerifyingShare storage: `dk` event or per-member in state? | **Embedded in `dk` payload** (`vs` array). Materialized into `CommitteeState.verifying_shares` on `dk` finalization. Single source of truth in the event log. |
| 4 | Epoch semantics? | **Count of finalized ceremonies**: starts at 0, increments to 1 on first `dk` quorum, increments again on each subsequent `dk` quorum (including refresh completions). VRF seed includes epoch to prevent cross-ceremony beacon replays. |
| 5 | Beacon request authorization? | **Admin-only** (power ≥ 100) in foundation phase. Phase 4a-main relaxes to Tier-3 poll-creator when sortition wires up. |

---

## 10. Tasks

### Task 0 — Pre-flight Verification (NO COMMIT)

**Purpose:** confirm FROST dep resolves without dalek conflict; verify API surface.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri

# Add dep (don't commit yet)
cargo add frost-ristretto255 --version "2" --features serde

# Verify no curve25519-dalek conflict
cargo check --locked --all-targets --features test-fixtures 2>&1 | head -60

# If conflict: check frost's dalek requirement
cargo tree -p frost-ristretto255 -i curve25519-dalek
# Resolution: if frost requires >=4.1.3, our pin "=4.1.3" satisfies it.
# If frost requires >=4.2, update pin to "=4.2.x" where x is the version
# frost actually resolves to.

# Confirm API exists (Identifier::try_from(u16), dkg::part1, keys::refresh::*)
cargo doc -p frost-ristretto255 --no-deps 2>&1 | grep -E "part1|refresh|try_from"

# Revert (Task 0 has NO commit)
git checkout Cargo.toml Cargo.lock
```

**Expected:** `cargo check` passes after resolution. Document any pin changes needed.

**No commit.**

---

### Task 1 — Cargo Dep + `community_dfrost_types.rs`

**Files:** `src-tauri/Cargo.toml` (modify), `src-tauri/src/community_dfrost_types.rs` (create), `src-tauri/src/lib.rs` (add `pub mod community_dfrost_types;`)

**Failing test first** (write at bottom of new file before impl):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dfrost_kind_codes_are_2_char_strings() {
        for (kind, expected) in [
            (DfrostEventKind::DkgRound, "dr"),
            (DfrostEventKind::DkgComplete, "dk"),
            (DfrostEventKind::ThresholdSign, "ts"),
            (DfrostEventKind::VrfBeacon, "vb"),
            (DfrostEventKind::ProactiveRefresh, "rf"),
        ] {
            let mut buf = Vec::new();
            ciborium::into_writer(&kind, &mut buf).unwrap();
            let val: ciborium::Value = ciborium::from_reader(&buf[..]).unwrap();
            let s = val.as_text().expect("kind encodes as text");
            assert_eq!(s, expected);
            assert_eq!(s.len(), 2);
        }
    }

    #[test]
    fn signed_committee_event_envelope_has_8_two_char_keys() {
        let ev = SignedCommitteeEvent {
            tag: 'd', version: 1, committee_tier: 0,
            kind: DfrostEventKind::DkgRound,
            hlc: crate::owner_state_types::Hlc {
                wall_ms: 1000, logical: 0, device_id: "t".into() },
            actor: crate::owner_state_types::OwnerAddr([0xaa; 16]),
            payload: vec![0x42],
            sig: vec![0u8; 64],
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&ev, &mut buf).unwrap();
        let val: ciborium::Value = ciborium::from_reader(&buf[..]).unwrap();
        let map = val.as_map().expect("map");
        assert_eq!(map.len(), 8);
        for (k, _) in &map {
            let s = k.as_text().unwrap();
            assert_eq!(s.len(), 2, "key {s:?} violates 2-char invariant");
        }
    }

    #[test]
    fn signing_bytes_exclude_sg_field() {
        let mut ev = SignedCommitteeEvent {
            tag: 'd', version: 1, committee_tier: 0,
            kind: DfrostEventKind::DkgComplete,
            hlc: crate::owner_state_types::Hlc {
                wall_ms: 1000, logical: 0, device_id: "t".into() },
            actor: crate::owner_state_types::OwnerAddr([0xaa; 16]),
            payload: vec![0xde, 0xad],
            sig: vec![0u8; 64],
        };
        let sb1 = ev.signing_bytes().unwrap();
        ev.sig = vec![0xff; 64];
        let sb2 = ev.signing_bytes().unwrap();
        assert_eq!(sb1, sb2, "signing_bytes must not depend on sig field");
        let val: ciborium::Value = ciborium::from_reader(&sb1[..]).unwrap();
        let map = val.as_map().unwrap();
        assert_eq!(map.len(), 7, "signing_bytes must have 7 fields (excludes sg)");
        assert!(!map.iter().any(|(k, _)| k.as_text() == Some("sg")));
    }

    #[test]
    fn derive_vrf_seed_is_deterministic() {
        let hash = [0x11u8; 32];
        let epoch = 3u64;
        let s1 = derive_vrf_seed(&hash, epoch);
        let s2 = derive_vrf_seed(&hash, epoch);
        assert_eq!(s1, s2);
        let s3 = derive_vrf_seed(&hash, epoch + 1);
        assert_ne!(s1, s3);
    }
}
```

Run:
```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
# Expect compile error: module not found
cargo nextest run --locked --features test-fixtures -E 'test(dfrost_kind_codes)'
```

**Implement `community_dfrost_types.rs`:**

Key items:
- `DfrostEventKind` enum with `#[serde(rename)]` for each 2-char code
- `SignedCommitteeEvent` struct with 8 fields named `tg/vr/tr/kd/hc/ac/pd/sg` via serde rename; `pd` and `sg` use `serde_bytes`
- `signing_bytes()` method serializing a 7-field inner struct (excludes `sg`)
- Payload structs: `DkgRoundPayload`, `DkgCompletePayload` (with `MemberVerifyingShare` inner type), `ThresholdSignPayload`, `VrfBeaconPayload`, `RefreshRoundPayload`
- All byte arrays use `#[serde(with = "serde_bytes")]`; optional fields use `skip_serializing_if = "Option::is_none"`
- `derive_ceremony_id(community_id, wall_ms, tag) -> [u8; 32]` using SHA-256
- `derive_vrf_seed(poll_hash, epoch) -> [u8; 32]`
- `derive_vrf_output(r_compressed) -> [u8; 32]`

**Cargo.toml addition:**
```toml
frost-ristretto255 = { version = "2", features = ["serde"] }
```

**Run after impl:**
```bash
cargo nextest run --locked --features test-fixtures -E 'test(dfrost_kind|signed_committee|signing_bytes|derive_vrf)'
# Expected: 4 tests pass
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

**Commit:**
```
feat(dfrost): Task 1 — frost-ristretto255 dep + community_dfrost_types.rs

5 DfrostEventKind variants (dr/dk/ts/vb/rf). SignedCommitteeEvent: 8-field 2-char-key
CBOR envelope (tg='d', tr=0). Payload structs for all 5 kinds with serde_bytes on
byte fields. signing_bytes() excludes sg. derive_ceremony_id/vrf_seed/vrf_output SHA-256
helpers. Tests: kind codes, 8-key envelope, signing_bytes excludes sg, vrf_seed deterministic.

ZEB-301
```

---

### Task 2 — `community_dfrost_log.rs` Skeleton

**Files:** `src-tauri/src/community_dfrost_log.rs` (create), `src-tauri/src/lib.rs` (add `pub mod community_dfrost_log;`)

**Failing test first:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::OwnerAddr;

    #[test]
    fn dfrost_log_starts_empty() {
        let log = DfrostLog::new();
        assert!(!log.committee_state.active);
        assert_eq!(log.committee_state.current_epoch, 0);
        assert!(log.committee_state.joint_verifying_key.is_none());
        assert!(log.events.is_empty());
    }

    #[test]
    fn build_identifier_map_uses_sorted_order() {
        let alice = OwnerAddr([0x01; 16]);
        let bob   = OwnerAddr([0x02; 16]);
        // alice < bob in byte order; even if passed unsorted, alice gets id=1
        let map = CommitteeState::build_identifier_map(&[bob, alice]);
        let alice_id = frost_ristretto255::Identifier::try_from(1u16).unwrap();
        let bob_id   = frost_ristretto255::Identifier::try_from(2u16).unwrap();
        assert_eq!(map[&alice], alice_id);
        assert_eq!(map[&bob], bob_id);
    }

    #[test]
    fn apply_unknown_ceremony_returns_error() {
        use crate::community_dfrost_types::{DkgRoundPayload, SignedCommitteeEvent, DfrostEventKind};
        use crate::owner_state_types::Hlc;

        // Post a dr(rn=1) event with no pending ceremony — should error.
        let payload = DkgRoundPayload {
            ceremony_id: [0x42u8; 32],
            round_num: 1,
            round1_package: Some(vec![0xde]),
            recipient_ciphertexts: None,
        };
        let mut pd = Vec::new();
        ciborium::into_writer(&payload, &mut pd).unwrap();
        let ev = SignedCommitteeEvent {
            tag: 'd', version: 1, committee_tier: 0,
            kind: DfrostEventKind::DkgRound,
            hlc: Hlc { wall_ms: 1000, logical: 0, device_id: "t".into() },
            actor: OwnerAddr([0xaa; 16]),
            payload: pd,
            sig: vec![0u8; 64],
        };
        let mut log = DfrostLog::new();
        assert_eq!(log.apply(ev), Err(ApplyError::UnknownCeremony));
    }
}
```

**Implementation:** full `DfrostLog`, `CommitteeState`, `PendingCeremony`, `PendingSignSession`, `ApplyError` types; `apply()` dispatcher calling 5 stub handlers that return `Ok(())`; `apply_dkg_round` checks for pending ceremony or returns `UnknownCeremony`; `build_identifier_map` on `CommitteeState`.

**Run:**
```bash
cargo nextest run --locked --features test-fixtures -E 'test(dfrost_log|build_identifier|apply_unknown)'
# Expected: 3 tests pass
```

**Commit:**
```
feat(dfrost): Task 2 — DfrostLog skeleton + CommitteeState + Identifier binding

DfrostLog with apply() dispatcher (dr/dk/ts/vb/rf handlers — dr checks ceremony,
others stub Ok). CommitteeState: active, current_epoch, joint_verifying_key,
verifying_shares, members, threshold, max_signers, identifier_map (serde skip),
pending_dkg, pending_sign, pending_refresh. PendingCeremony/PendingSignSession.
build_identifier_map: sort OwnerAddr → 1-indexed Identifier::try_from(u16).
Tests: empty log, sorted identifier map, UnknownCeremony on orphan event.

ZEB-301
```

---

### Task 3 — `community_dfrost_crypto.rs` + DKG Part1 Round-Trip

**Files:** `src-tauri/src/community_dfrost_crypto.rs` (create), `src-tauri/src/lib.rs` (add `pub mod community_dfrost_crypto;`)

**Failing test first:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use frost_ristretto255::keys::dkg;

    #[test]
    fn dkg_part1_round1_package_cbor_round_trips() {
        // Verifies: frost serde feature works + ciborium handles the package type.
        let id = identifier_for_index(0);
        let (_secret, r1_bytes) = dkg_part1_local(id, 2, 2).expect("part1");
        assert!(!r1_bytes.is_empty(), "round1 package must produce bytes");
        // Deserialize and re-serialize: must be byte-identical.
        let pkg: dkg::round1::Package = ciborium::from_reader(&r1_bytes[..]).expect("decode");
        let mut re_encoded = Vec::new();
        ciborium::into_writer(&pkg, &mut re_encoded).expect("re-encode");
        assert_eq!(r1_bytes, re_encoded, "round1::Package CBOR must round-trip");
    }

    #[test]
    fn identifier_for_index_is_1_indexed_and_deterministic() {
        let id0 = identifier_for_index(0);
        let id1 = identifier_for_index(1);
        assert_ne!(id0, id1);
        assert_eq!(identifier_for_index(0), id0); // deterministic
        // id0 == Identifier::try_from(1u16)
        assert_eq!(id0, frost_ristretto255::Identifier::try_from(1u16).unwrap());
    }

    #[test]
    fn dkg_part2_produces_one_package_per_other_participant() {
        use std::collections::BTreeMap;

        let id1 = identifier_for_index(0);
        let id2 = identifier_for_index(1);
        let id3 = identifier_for_index(2);

        let (sec1, r1_1) = dkg_part1_local(id1, 3, 2).unwrap();
        let (_sec2, r1_2) = dkg_part1_local(id2, 3, 2).unwrap();
        let (_sec3, r1_3) = dkg_part1_local(id3, 3, 2).unwrap();

        // id1 runs part2 with packages from id2 and id3
        let received: BTreeMap<frost_ristretto255::Identifier, Vec<u8>> = [
            (id2, r1_2), (id3, r1_3),
        ].into_iter().collect();

        let (_sec2_pkg, r2_map) = dkg_part2_local(sec1, &received).expect("part2");
        // part2 produces one package per other participant (2 here)
        assert_eq!(r2_map.len(), 2);
        assert!(r2_map.contains_key(&id2));
        assert!(r2_map.contains_key(&id3));
    }
}
```

**Implementation:** `identifier_for_index`, `dkg_part1_local`, `dkg_part2_local`, `dkg_part3_local`, `verifying_key_to_bytes`, `verifying_share_to_bytes`. Leave `threshold_sign_*` and `aggregate_signatures` as stubs for Task 7.

**Run:**
```bash
cargo nextest run --locked --features test-fixtures -E 'test(dkg_part1|identifier_for_index|dkg_part2_produces)'
# Expected: 3 tests pass
```

**Commit:**
```
feat(dfrost): Task 3 — community_dfrost_crypto.rs: DKG part1/part2/part3 wrappers

identifier_for_index(usize) → Identifier::try_from(n+1). dkg_part1_local → (SecretPackage,
ciborium-encoded round1::Package bytes). dkg_part2_local → (SecretPackage2, map of
identifier → ciborium-encoded round2::Package bytes). dkg_part3_local → (KeyPackage,
PublicKeyPackage). verifying_key_to_bytes / verifying_share_to_bytes: compressed Ristretto
point extraction. Tests: part1 CBOR round-trip, identifier 1-indexed deterministic,
part2 produces one package per other participant.

ZEB-301
```

---

### Task 4 — Full DKG Apply Path (Round 1 + Completion)

**Files:** `src-tauri/src/community_dfrost_log.rs` (implement `apply_dkg_round` rn=1 + `apply_dkg_complete`)

**Failing test first:**

```rust
#[test]
fn full_1of1_dkg_ceremony_finalizes() {
    // 1-of-1 committee: single member posts dr(rn=1) then dk → committee active.
    use crate::community_dfrost_types::{DkgRoundPayload, DkgCompletePayload,
        MemberVerifyingShare, DfrostEventKind, SignedCommitteeEvent};
    use crate::owner_state_types::{OwnerAddr, Hlc};

    let alice = OwnerAddr([0x01; 16]);
    let ceremony_id = [0x42u8; 32];
    let fake_vk = [0x55u8; 32];

    let mut log = DfrostLog::new();
    // Seed pending_dkg (normally done by initiate_dkg IPC).
    log.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id,
        members: vec![alice],
        threshold: 1,
        max_signers: 1,
        proposed_epoch: 1,
        ..Default::default()
    });

    // Apply dr(rn=1)
    let r1_payload = DkgRoundPayload {
        ceremony_id, round_num: 1,
        round1_package: Some(vec![0xde, 0xad]),
        recipient_ciphertexts: None,
    };
    let mut pd = Vec::new();
    ciborium::into_writer(&r1_payload, &mut pd).unwrap();
    log.apply(SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0, kind: DfrostEventKind::DkgRound,
        hlc: Hlc { wall_ms: 1000, logical: 0, device_id: "t".into() },
        actor: alice, payload: pd, sig: vec![0u8; 64],
    }).expect("apply dr rn=1");
    assert!(log.committee_state.pending_dkg.as_ref().unwrap()
        .round1_packages.contains_key(&alice));

    // Apply dk
    let dk_payload = DkgCompletePayload {
        ceremony_id, joint_verifying_key: fake_vk,
        verifying_shares: vec![MemberVerifyingShare { member: alice, verifying_share: [0xaa; 32] }],
        epoch: 1, members: vec![alice], threshold: 1, max_signers: 1,
    };
    let mut pd2 = Vec::new();
    ciborium::into_writer(&dk_payload, &mut pd2).unwrap();
    log.apply(SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0, kind: DfrostEventKind::DkgComplete,
        hlc: Hlc { wall_ms: 2000, logical: 0, device_id: "t".into() },
        actor: alice, payload: pd2, sig: vec![0u8; 64],
    }).expect("apply dk");

    assert!(log.committee_state.active);
    assert_eq!(log.committee_state.current_epoch, 1);
    assert_eq!(log.committee_state.joint_verifying_key, Some(fake_vk));
    assert_eq!(log.committee_state.verifying_shares[&alice], [0xaa; 32]);
    assert!(log.committee_state.pending_dkg.is_none());
    assert_eq!(log.events.len(), 2);
}

#[test]
fn dk_with_wrong_vk_after_active_returns_invariant_violation() {
    // After a committee is active, a dk with a different vk must be rejected.
    let mut log = DfrostLog::new();
    log.committee_state.active = true;
    log.committee_state.current_epoch = 1;
    log.committee_state.joint_verifying_key = Some([0x11u8; 32]);
    log.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id: [0xcc; 32],
        members: vec![OwnerAddr([0x01; 16])],
        threshold: 1, max_signers: 1, proposed_epoch: 2,
        ..Default::default()
    });

    // Apply dk(rn=2) with different vk
    use crate::community_dfrost_types::{DkgCompletePayload, DfrostEventKind, SignedCommitteeEvent, MemberVerifyingShare};
    use crate::owner_state_types::{OwnerAddr, Hlc};

    let dk_payload = DkgCompletePayload {
        ceremony_id: [0xcc; 32],
        joint_verifying_key: [0x22u8; 32],  // DIFFERENT from active [0x11]
        verifying_shares: vec![MemberVerifyingShare { member: OwnerAddr([0x01; 16]), verifying_share: [0; 32] }],
        epoch: 2, members: vec![OwnerAddr([0x01; 16])], threshold: 1, max_signers: 1,
    };
    let mut pd = Vec::new();
    ciborium::into_writer(&dk_payload, &mut pd).unwrap();

    let result = log.apply(SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0, kind: DfrostEventKind::DkgComplete,
        hlc: Hlc { wall_ms: 3000, logical: 0, device_id: "t".into() },
        actor: OwnerAddr([0x01; 16]), payload: pd, sig: vec![0u8; 64],
    });
    assert_eq!(result, Err(ApplyError::InvariantViolation));
}
```

**Run:**
```bash
cargo nextest run --locked --features test-fixtures -E 'test(full_1of1_dkg|dk_with_wrong_vk)'
# Expected: 2 tests pass
```

**Commit:**
```
feat(dfrost): Task 4 — full DKG round-1 + dk apply paths

apply_dkg_round rn=1: decodes DkgRoundPayload, finds pending ceremony by ceremony_id,
stores round1_packages[actor]. apply_dkg_complete: records dk_confirmations, checks
quorum (count >= threshold), verifies all confirmations match vk, finalizes CommitteeState
on quorum, rejects with InvariantViolation if active committee's vk changes. Tests: full
1-of-1 ceremony finalizes, wrong-vk-after-active rejected.

ZEB-301
```

---

### Task 5 — Round-2 Decrypt Path (`apply_with_identity`)

**Files:** `src-tauri/src/community_dfrost_log.rs` (add `apply_with_identity`)

**Failing test first:**

```rust
#[test]
fn apply_with_identity_decrypts_round2_package() {
    use crate::community_dfrost_types::{DkgRoundPayload, DfrostEventKind, SignedCommitteeEvent};
    use crate::community_membership::RecipientCiphertext;
    use crate::dm_signing;
    use crate::owner_state_types::{OwnerAddr, Hlc};
    use x25519_dalek::{PublicKey, StaticSecret};

    let alice = OwnerAddr([0x01; 16]);
    let alice_priv = [0x42u8; 32];
    let alice_x25519_pub = *PublicKey::from(&StaticSecret::from(alice_priv)).as_bytes();

    let fake_r2_pkg_bytes = vec![0xca, 0xfe, 0xba, 0xbe];
    let sealed = dm_signing::seal_to_owner(&alice_x25519_pub, &fake_r2_pkg_bytes)
        .expect("seal");

    let payload = DkgRoundPayload {
        ceremony_id: [0x42u8; 32],
        round_num: 2,
        round1_package: None,
        recipient_ciphertexts: Some(vec![
            RecipientCiphertext { recipient: alice, sealed },
        ]),
    };
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).unwrap();
    let ev = SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0, kind: DfrostEventKind::DkgRound,
        hlc: Hlc { wall_ms: 3000, logical: 0, device_id: "t".into() },
        actor: OwnerAddr([0x02; 16]),  // bob posts round-2
        payload: pd, sig: vec![0u8; 64],
    };

    let mut log = DfrostLog::new();
    log.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id: [0x42u8; 32],
        members: vec![alice, OwnerAddr([0x02; 16])],
        threshold: 1, max_signers: 2, proposed_epoch: 1,
        ..Default::default()
    });

    log.apply_with_identity(ev, &alice, &alice_priv).expect("apply with identity");

    let r2_pkgs = &log.committee_state.pending_dkg.as_ref().unwrap().round2_packages;
    assert_eq!(r2_pkgs.get(&OwnerAddr([0x02; 16])), Some(&fake_r2_pkg_bytes));
}
```

**Implementation:** `apply_with_identity` checks `event.kind == DkgRound && rn == 2`; finds `RecipientCiphertext` where `ct.recipient == self_addr`; calls `dm_signing::open_from_owner(self_x25519_priv, &ct.sealed)`; stores decrypted bytes in `pending_dkg.round2_packages[event.actor]`. Similarly for `ProactiveRefresh rn=1` (decrypts new SecretShare).

**Run:**
```bash
cargo nextest run --locked --features test-fixtures -E 'test(apply_with_identity_decrypts)'
# Expected: 1 test passes
```

**Commit:**
```
feat(dfrost): Task 5 — apply_with_identity: round-2 X25519 decrypt path

apply_with_identity(event, self_addr, self_x25519_priv): for DkgRound rn=2, finds
RecipientCiphertext where ct.recipient==self_addr, decrypts via dm_signing::open_from_owner,
stores in pending_dkg.round2_packages[actor]. For ProactiveRefresh rn=1 similarly decrypts
coordinator's SecretShare distribution. Non-round-2 events fall through to regular apply().
Test: sealed r2 package decrypts correctly, stored keyed by actor (the sender).

ZEB-301
```

---

### Task 6 — ThresholdSign + VrfBeacon Apply Paths

**Files:** `src-tauri/src/community_dfrost_log.rs` (implement `apply_threshold_sign`, `apply_vrf_beacon`), `src-tauri/src/community_dfrost_crypto.rs` (add threshold sign wrappers + `verify_schnorr_signature`)

**Failing test first:**

```rust
#[test]
fn ts_contributions_accumulate_in_pending_sign() {
    use crate::community_dfrost_types::{ThresholdSignPayload, DfrostEventKind, SignedCommitteeEvent};
    use crate::owner_state_types::{OwnerAddr, Hlc};

    let alice = OwnerAddr([0x01; 16]);
    let ceremony_id = [0xcc; 32];
    let msg_hash = [0xde; 32];

    let mut log = DfrostLog::new();
    log.committee_state.active = true;
    log.committee_state.members = vec![alice];

    let payload = ThresholdSignPayload {
        ceremony_id, message_hash: msg_hash,
        commitment_bytes: vec![0x01, 0x02],
        share_bytes: vec![0x03, 0x04],
    };
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).unwrap();

    log.apply(SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0, kind: DfrostEventKind::ThresholdSign,
        hlc: Hlc { wall_ms: 4000, logical: 0, device_id: "t".into() },
        actor: alice, payload: pd, sig: vec![0u8; 64],
    }).expect("apply ts");

    let session = log.committee_state.pending_sign.get(&ceremony_id).unwrap();
    assert_eq!(session.message_hash, msg_hash);
    let (cm, sh) = session.contributions.get(&alice).unwrap();
    assert_eq!(cm, &vec![0x01u8, 0x02]);
    assert_eq!(sh, &vec![0x03u8, 0x04]);
}

#[test]
fn vb_with_wrong_vrf_output_rejected() {
    use crate::community_dfrost_types::{VrfBeaconPayload, DfrostEventKind, SignedCommitteeEvent};
    use crate::owner_state_types::{OwnerAddr, Hlc};
    use crate::community_dfrost_types::derive_vrf_output;

    let mut log = DfrostLog::new();
    log.committee_state.active = true;

    let sig_bytes = vec![0xaau8; 64];  // fake 64-byte sig
    let correct_vrf = derive_vrf_output(&sig_bytes[..32].try_into().unwrap());
    let wrong_vrf = [0xff; 32];
    assert_ne!(correct_vrf, wrong_vrf);

    let payload = VrfBeaconPayload {
        ceremony_id: [0xcc; 32], message_hash: [0xde; 32],
        signature: sig_bytes, vrf_output: wrong_vrf,  // WRONG
    };
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).unwrap();

    let result = log.apply(SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0, kind: DfrostEventKind::VrfBeacon,
        hlc: Hlc { wall_ms: 5000, logical: 0, device_id: "t".into() },
        actor: OwnerAddr([0x01; 16]), payload: pd, sig: vec![0u8; 64],
    });
    assert_eq!(result, Err(ApplyError::InvariantViolation));
}
```

**Run:**
```bash
cargo nextest run --locked --features test-fixtures -E 'test(ts_contributions|vb_with_wrong_vrf)'
# Expected: 2 tests pass
```

**Commit:**
```
feat(dfrost): Task 6 — ThresholdSign + VrfBeacon apply paths

apply_threshold_sign: verifies active committee + actor is member, accumulates (cm,sh)
in pending_sign[ceremony_id].contributions. apply_vrf_beacon: verifies active committee,
checks VRF output binding (SHA-256(dfrost-vrf-v1||R_compressed) == payload.vf), clears
pending_sign session. crypto module: threshold_sign_round1, threshold_sign_round2,
build_signing_package, aggregate_signatures (FROST aggregate→64-byte Schnorr),
verify_schnorr_signature (VerifyingKey::verify). Tests: ts accumulates contributions,
vb wrong VRF output rejected.

ZEB-301
```

---

### Task 7 — ProactiveRefresh Apply Path

**Files:** `src-tauri/src/community_dfrost_log.rs` (implement `apply_proactive_refresh`), `src-tauri/src/community_dfrost_crypto.rs` (add `compute_refresh_shares`)

**Failing test first:**

```rust
#[test]
fn rf_rn1_event_starts_pending_refresh() {
    use crate::community_dfrost_types::{RefreshRoundPayload, DfrostEventKind, SignedCommitteeEvent};
    use crate::community_membership::RecipientCiphertext;
    use crate::owner_state_types::{OwnerAddr, Hlc};

    let alice = OwnerAddr([0x01; 16]);
    let ceremony_id = [0x77u8; 32];

    let mut log = DfrostLog::new();
    log.committee_state.active = true;
    log.committee_state.current_epoch = 1;
    log.committee_state.members = vec![alice];
    log.committee_state.threshold = 1;
    log.committee_state.max_signers = 1;

    let payload = RefreshRoundPayload {
        ceremony_id, round_num: 1,
        recipient_ciphertexts: Some(vec![
            RecipientCiphertext { recipient: alice, sealed: vec![0xde, 0xad] }
        ]),
        round2_package: None,
    };
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).unwrap();

    log.apply(SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0, kind: DfrostEventKind::ProactiveRefresh,
        hlc: Hlc { wall_ms: 6000, logical: 0, device_id: "t".into() },
        actor: alice, payload: pd, sig: vec![0u8; 64],
    }).expect("apply rf rn=1");

    assert!(log.committee_state.pending_refresh.is_some());
    let pr = log.committee_state.pending_refresh.as_ref().unwrap();
    assert_eq!(pr.ceremony_id, ceremony_id);
    assert_eq!(pr.proposed_epoch, 2);
}

#[test]
fn refresh_completion_preserves_joint_vk() {
    // dk event with ep=2 and SAME vk as current → accepted (refresh complete).
    use crate::community_dfrost_types::{DkgCompletePayload, MemberVerifyingShare, DfrostEventKind, SignedCommitteeEvent};
    use crate::owner_state_types::{OwnerAddr, Hlc};

    let alice = OwnerAddr([0x01; 16]);
    let existing_vk = [0x11u8; 32];

    let mut log = DfrostLog::new();
    log.committee_state.active = true;
    log.committee_state.current_epoch = 1;
    log.committee_state.joint_verifying_key = Some(existing_vk);
    log.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id: [0x88; 32],
        members: vec![alice], threshold: 1, max_signers: 1, proposed_epoch: 2,
        ..Default::default()
    });

    let dk_payload = DkgCompletePayload {
        ceremony_id: [0x88; 32],
        joint_verifying_key: existing_vk,  // SAME vk — refresh invariant satisfied
        verifying_shares: vec![MemberVerifyingShare { member: alice, verifying_share: [0xbb; 32] }],
        epoch: 2, members: vec![alice], threshold: 1, max_signers: 1,
    };
    let mut pd = Vec::new();
    ciborium::into_writer(&dk_payload, &mut pd).unwrap();

    log.apply(SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0, kind: DfrostEventKind::DkgComplete,
        hlc: Hlc { wall_ms: 7000, logical: 0, device_id: "t".into() },
        actor: alice, payload: pd, sig: vec![0u8; 64],
    }).expect("refresh dk accepted");

    assert_eq!(log.committee_state.current_epoch, 2);
    assert_eq!(log.committee_state.joint_verifying_key, Some(existing_vk));
}
```

**Run:**
```bash
cargo nextest run --locked --features test-fixtures -E 'test(rf_rn1_event|refresh_completion)'
# Expected: 2 tests pass
```

**Commit:**
```
feat(dfrost): Task 7 — ProactiveRefresh apply path + compute_refresh_shares

apply_proactive_refresh: rn=1 initializes pending_refresh (ceremony_id, current
members/threshold, proposed_epoch = current+1); rn=2 stores round2 packages. compute_refresh_shares
in crypto module: frost::keys::refresh::compute_refreshing_shares → per-member SecretShare
bytes + new PublicKeyPackage bytes. Refresh completion uses dk event with same vk bytes
(InvariantViolation if vk differs). Tests: rf rn=1 creates pending_refresh with ep=2;
refresh completion dk with same vk accepted + epoch bumped.

ZEB-301
```

---

### Task 8 — Wire Format Fixture Pinning

**Files:** `src-tauri/tests/wire_format_dfrost_fixtures.rs` (create)

**Content:** Write the fixture file with all 5 event kind envelope fixtures using the regen-on-first-run pattern. Initial constants set to `""`.

First run will panic and print hex. Second run after pasting will pass.

```rust
//! ZEB-301: byte-pinned CBOR wire format fixtures for all 5 D-FROST event kinds.
//! Pattern mirrors wire_format_zeb291_fixtures.rs.
//! Set any constant to "" to regenerate its fixture.

use harmony_app::community_dfrost_types::{
    DfrostEventKind, DkgCompletePayload, DkgRoundPayload, MemberVerifyingShare,
    RefreshRoundPayload, SignedCommitteeEvent, ThresholdSignPayload, VrfBeaconPayload,
};
use harmony_app::community_membership::RecipientCiphertext;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};

const FIXTURE_ACTOR: OwnerAddr = OwnerAddr([0xaa; 16]);
const FIXTURE_CID: [u8; 32] = [0xcc; 32];

// Set to "" to regenerate:
const EXPECTED_DKG_ROUND1_HEX: &str = "";
const EXPECTED_DKG_ROUND2_HEX: &str = "";
const EXPECTED_DKG_COMPLETE_HEX: &str = "";
const EXPECTED_THRESHOLD_SIGN_HEX: &str = "";
const EXPECTED_VRF_BEACON_HEX: &str = "";
const EXPECTED_PROACTIVE_REFRESH_HEX: &str = "";

fn hlc() -> Hlc { Hlc { wall_ms: 1_000, logical: 0, device_id: "d".into() } }

fn encode<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(v, &mut out).unwrap();
    out
}

fn make_envelope(kind: DfrostEventKind, payload: Vec<u8>) -> Vec<u8> {
    encode(&SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0, kind, hlc: hlc(),
        actor: FIXTURE_ACTOR, payload, sig: vec![0u8; 64],
    })
}

fn assert_2char_8keys(encoded: &[u8]) {
    let val: ciborium::Value = ciborium::from_reader(encoded).unwrap();
    let map = val.as_map().unwrap();
    assert_eq!(map.len(), 8, "must have 8 keys");
    for (k, _) in &map {
        let s = k.as_text().unwrap();
        assert_eq!(s.len(), 2, "key {s:?} must be 2 chars");
    }
}

#[test]
fn wire_format_dfrost_dkg_round1_pinned() {
    let encoded = make_envelope(DfrostEventKind::DkgRound, encode(&DkgRoundPayload {
        ceremony_id: FIXTURE_CID, round_num: 1,
        round1_package: Some(vec![0xde, 0xad]),
        recipient_ciphertexts: None,
    }));
    let actual = hex::encode(&encoded);
    if EXPECTED_DKG_ROUND1_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_DKG_ROUND1_HEX = \"{actual}\";");
    }
    assert_eq!(actual, EXPECTED_DKG_ROUND1_HEX);
    assert_2char_8keys(&encoded);
}

#[test]
fn wire_format_dfrost_dkg_round2_pinned() {
    let encoded = make_envelope(DfrostEventKind::DkgRound, encode(&DkgRoundPayload {
        ceremony_id: FIXTURE_CID, round_num: 2,
        round1_package: None,
        recipient_ciphertexts: Some(vec![
            RecipientCiphertext { recipient: OwnerAddr([0xbb; 16]), sealed: vec![0x11, 0x22] },
        ]),
    }));
    let actual = hex::encode(&encoded);
    if EXPECTED_DKG_ROUND2_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_DKG_ROUND2_HEX = \"{actual}\";");
    }
    assert_eq!(actual, EXPECTED_DKG_ROUND2_HEX);
    assert_2char_8keys(&encoded);
}

#[test]
fn wire_format_dfrost_dkg_complete_pinned() {
    let encoded = make_envelope(DfrostEventKind::DkgComplete, encode(&DkgCompletePayload {
        ceremony_id: FIXTURE_CID,
        joint_verifying_key: [0x11; 32],
        verifying_shares: vec![MemberVerifyingShare {
            member: OwnerAddr([0xaa; 16]), verifying_share: [0xbb; 32],
        }],
        epoch: 1, members: vec![OwnerAddr([0xaa; 16])], threshold: 1, max_signers: 2,
    }));
    let actual = hex::encode(&encoded);
    if EXPECTED_DKG_COMPLETE_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_DKG_COMPLETE_HEX = \"{actual}\";");
    }
    assert_eq!(actual, EXPECTED_DKG_COMPLETE_HEX);
    assert_2char_8keys(&encoded);
}

#[test]
fn wire_format_dfrost_threshold_sign_pinned() {
    let encoded = make_envelope(DfrostEventKind::ThresholdSign, encode(&ThresholdSignPayload {
        ceremony_id: FIXTURE_CID, message_hash: [0xde; 32],
        commitment_bytes: vec![0x01], share_bytes: vec![0x02],
    }));
    let actual = hex::encode(&encoded);
    if EXPECTED_THRESHOLD_SIGN_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_THRESHOLD_SIGN_HEX = \"{actual}\";");
    }
    assert_eq!(actual, EXPECTED_THRESHOLD_SIGN_HEX);
    assert_2char_8keys(&encoded);
}

#[test]
fn wire_format_dfrost_vrf_beacon_pinned() {
    let encoded = make_envelope(DfrostEventKind::VrfBeacon, encode(&VrfBeaconPayload {
        ceremony_id: FIXTURE_CID, message_hash: [0xde; 32],
        signature: vec![0xaa; 64], vrf_output: [0xbb; 32],
    }));
    let actual = hex::encode(&encoded);
    if EXPECTED_VRF_BEACON_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_VRF_BEACON_HEX = \"{actual}\";");
    }
    assert_eq!(actual, EXPECTED_VRF_BEACON_HEX);
    assert_2char_8keys(&encoded);
}

#[test]
fn wire_format_dfrost_proactive_refresh_pinned() {
    let encoded = make_envelope(DfrostEventKind::ProactiveRefresh, encode(&RefreshRoundPayload {
        ceremony_id: FIXTURE_CID, round_num: 1,
        recipient_ciphertexts: Some(vec![
            RecipientCiphertext { recipient: OwnerAddr([0xbb; 16]), sealed: vec![0x33, 0x44] },
        ]),
        round2_package: None,
    }));
    let actual = hex::encode(&encoded);
    if EXPECTED_PROACTIVE_REFRESH_HEX.is_empty() {
        panic!("REGENERATE EXPECTED_PROACTIVE_REFRESH_HEX = \"{actual}\";");
    }
    assert_eq!(actual, EXPECTED_PROACTIVE_REFRESH_HEX);
    assert_2char_8keys(&encoded);
}
```

**Run (first time — regenerate):**
```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures --test wire_format_dfrost_fixtures 2>&1 | grep REGENERATE
# Paste the hex values into the constants.
# Then run again — all 6 tests pass.
cargo nextest run --locked --features test-fixtures --test wire_format_dfrost_fixtures
# Expected: 6 tests pass
```

**Commit:**
```
feat(dfrost): Task 8 — wire format CBOR fixture pinning for all 5 event kinds

wire_format_dfrost_fixtures.rs: regen-on-first-run hex pin for DkgRound1, DkgRound2,
DkgComplete, ThresholdSign, VrfBeacon, ProactiveRefresh envelopes. Each fixture also
asserts: 8 top-level keys, all 2-char. Locks the wire format against silent serde renames.

ZEB-301
```

---

### Task 9 — Two-Engine DKG Integration Test (2-of-2 Full Ceremony)

**Files:** `src-tauri/tests/dfrost_dkg_integration.rs` (create)

**Test content — 2-of-2 DKG with real FROST crypto:**

```rust
//! ZEB-301: D-FROST multi-engine integration tests.
//!
//! Exercises the full DKG ceremony (2-of-2), threshold signing + VRF derivation,
//! and proactive refresh using real FROST crypto (no stubs). Two independent
//! DfrostLog instances (alice_log, bob_log) converge on identical CommitteeState.

use frost_ristretto255::{self as frost};
use harmony_app::community_dfrost_crypto::{
    dkg_part1_local, dkg_part2_local, dkg_part3_local, identifier_for_index,
    verifying_key_to_bytes, verifying_share_to_bytes,
    threshold_sign_round1, commitments_to_bytes, threshold_sign_round2,
    build_signing_package, aggregate_signatures, verify_schnorr_signature,
};
use harmony_app::community_dfrost_log::{DfrostLog, PendingCeremony, CommitteeState};
use harmony_app::community_dfrost_types::{
    DkgRoundPayload, DkgCompletePayload, MemberVerifyingShare,
    ThresholdSignPayload, VrfBeaconPayload, DfrostEventKind, SignedCommitteeEvent,
    derive_vrf_seed, derive_vrf_output, derive_ceremony_id,
};
use harmony_app::community_membership::RecipientCiphertext;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::dm_signing;
use std::collections::BTreeMap;
use x25519_dalek::{PublicKey, StaticSecret};

fn hlc(ms: u64) -> Hlc { Hlc { wall_ms: ms, logical: 0, device_id: "t".into() } }

fn encode<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(v, &mut out).unwrap();
    out
}

fn make_ev(kind: DfrostEventKind, actor: OwnerAddr, payload: Vec<u8>, ms: u64) -> SignedCommitteeEvent {
    SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0, kind, hlc: hlc(ms),
        actor, payload, sig: vec![0u8; 64],
    }
}

#[test]
fn two_of_two_dkg_full_ceremony_converges() {
    let alice = OwnerAddr([0x01; 16]);
    let bob   = OwnerAddr([0x02; 16]);
    let alice_x25519_priv = [0x10u8; 32];
    let alice_x25519_pub  = *PublicKey::from(&StaticSecret::from(alice_x25519_priv)).as_bytes();
    let bob_x25519_priv   = [0x20u8; 32];
    let bob_x25519_pub    = *PublicKey::from(&StaticSecret::from(bob_x25519_priv)).as_bytes();

    let community_id = SpaceId([0xc0; 16]);
    let ceremony_id = derive_ceremony_id(&community_id, 1000, b"dkg-v1");
    let members = vec![alice, bob]; // alice < bob in byte order
    let threshold: u16 = 2;
    let max_signers: u16 = 2;

    let alice_id = identifier_for_index(0);  // alice is index 0 (sorted)
    let bob_id   = identifier_for_index(1);

    // ── Round 1: each runs dkg_part1_local ──────────────────────────────────
    let (alice_sec1, alice_r1_bytes) = dkg_part1_local(alice_id, max_signers, threshold).unwrap();
    let (bob_sec1,   bob_r1_bytes)   = dkg_part1_local(bob_id,   max_signers, threshold).unwrap();

    // Both logs receive each other's round-1 events.
    let mut alice_log = DfrostLog::new();
    let mut bob_log   = DfrostLog::new();

    let seed_pending = |log: &mut DfrostLog| {
        log.committee_state.pending_dkg = Some(PendingCeremony {
            ceremony_id, members: members.clone(),
            threshold, max_signers, proposed_epoch: 1,
            ..Default::default()
        });
    };
    seed_pending(&mut alice_log);
    seed_pending(&mut bob_log);

    // Post dr(rn=1) events to both logs
    for (actor, r1_bytes) in [(alice, alice_r1_bytes.clone()), (bob, bob_r1_bytes.clone())] {
        let ev = make_ev(DfrostEventKind::DkgRound, actor,
            encode(&DkgRoundPayload { ceremony_id, round_num: 1,
                round1_package: Some(r1_bytes), recipient_ciphertexts: None }), 1000);
        alice_log.apply(ev.clone()).unwrap();
        bob_log.apply(ev).unwrap();
    }
    assert_eq!(alice_log.committee_state.pending_dkg.as_ref().unwrap().round1_packages.len(), 2);

    // ── Round 2: each runs dkg_part2_local, encrypts to other ──────────────
    let received_r1_for_alice: BTreeMap<frost::Identifier, Vec<u8>> = [
        (bob_id, bob_r1_bytes.clone()),
    ].into_iter().collect();
    let (alice_sec2, alice_r2_map) = dkg_part2_local(alice_sec1, &received_r1_for_alice).unwrap();

    let received_r1_for_bob: BTreeMap<frost::Identifier, Vec<u8>> = [
        (alice_id, alice_r1_bytes.clone()),
    ].into_iter().collect();
    let (bob_sec2, bob_r2_map) = dkg_part2_local(bob_sec1, &received_r1_for_bob).unwrap();

    // alice posts dr(rn=2) with r2 pkg for bob, encrypted
    let alice_r2_for_bob = alice_r2_map.get(&bob_id).unwrap();
    let sealed_alice_to_bob = dm_signing::seal_to_owner(&bob_x25519_pub, alice_r2_for_bob).unwrap();

    let alice_r2_ev = make_ev(DfrostEventKind::DkgRound, alice,
        encode(&DkgRoundPayload { ceremony_id, round_num: 2,
            round1_package: None,
            recipient_ciphertexts: Some(vec![
                RecipientCiphertext { recipient: bob, sealed: sealed_alice_to_bob },
            ]) }), 2000);

    alice_log.apply(alice_r2_ev.clone()).unwrap(); // alice sees her own (no decrypt needed)
    bob_log.apply_with_identity(alice_r2_ev, &bob, &bob_x25519_priv).unwrap();

    // bob posts dr(rn=2) with r2 pkg for alice, encrypted
    let bob_r2_for_alice = bob_r2_map.get(&alice_id).unwrap();
    let sealed_bob_to_alice = dm_signing::seal_to_owner(&alice_x25519_pub, bob_r2_for_alice).unwrap();

    let bob_r2_ev = make_ev(DfrostEventKind::DkgRound, bob,
        encode(&DkgRoundPayload { ceremony_id, round_num: 2,
            round1_package: None,
            recipient_ciphertexts: Some(vec![
                RecipientCiphertext { recipient: alice, sealed: sealed_bob_to_alice },
            ]) }), 2001);

    alice_log.apply_with_identity(bob_r2_ev.clone(), &alice, &alice_x25519_priv).unwrap();
    bob_log.apply(bob_r2_ev).unwrap();

    // ── Part 3: each runs dkg_part3_local ────────────────────────────────────
    let all_r1: BTreeMap<frost::Identifier, Vec<u8>> = [
        (alice_id, alice_r1_bytes), (bob_id, bob_r1_bytes),
    ].into_iter().collect();

    let alice_r2_received: BTreeMap<frost::Identifier, Vec<u8>> = [
        (bob_id, bob_r2_for_alice.clone()),
    ].into_iter().collect();
    let (alice_key_pkg, alice_pub_pkg) = dkg_part3_local(alice_sec2, &all_r1, &alice_r2_received).unwrap();

    let bob_r2_received: BTreeMap<frost::Identifier, Vec<u8>> = [
        (alice_id, alice_r2_for_bob.clone()),
    ].into_iter().collect();
    let (bob_key_pkg, bob_pub_pkg) = dkg_part3_local(bob_sec2, &all_r1, &bob_r2_received).unwrap();

    // Both should produce the same joint verifying key
    let alice_vk_bytes = verifying_key_to_bytes(alice_pub_pkg.verifying_key());
    let bob_vk_bytes   = verifying_key_to_bytes(bob_pub_pkg.verifying_key());
    assert_eq!(alice_vk_bytes, bob_vk_bytes, "joint verifying key must match across nodes");

    // ── dk events: each posts their completion ────────────────────────────────
    let verifying_shares_alice: Vec<MemberVerifyingShare> = alice_pub_pkg.verifying_shares()
        .iter()
        .map(|(id, vs)| {
            // Map identifier back to OwnerAddr (reverse lookup)
            let addr = if *id == alice_id { alice } else { bob };
            MemberVerifyingShare { member: addr, verifying_share: verifying_share_to_bytes(vs) }
        }).collect();

    let dk_payload = DkgCompletePayload {
        ceremony_id, joint_verifying_key: alice_vk_bytes,
        verifying_shares: verifying_shares_alice.clone(),
        epoch: 1, members: members.clone(), threshold, max_signers,
    };
    let dk_ev_alice = make_ev(DfrostEventKind::DkgComplete, alice, encode(&dk_payload), 3000);
    let dk_ev_bob   = make_ev(DfrostEventKind::DkgComplete, bob,
        encode(&DkgCompletePayload { ceremony_id, joint_verifying_key: alice_vk_bytes,
            verifying_shares: verifying_shares_alice.clone(), epoch: 1,
            members: members.clone(), threshold, max_signers }), 3001);

    for log in [&mut alice_log, &mut bob_log] {
        log.apply(dk_ev_alice.clone()).unwrap();
        log.apply(dk_ev_bob.clone()).unwrap();
    }

    // Both logs converge on identical CommitteeState
    assert!(alice_log.committee_state.active);
    assert!(bob_log.committee_state.active);
    assert_eq!(alice_log.committee_state.current_epoch, 1);
    assert_eq!(bob_log.committee_state.current_epoch, 1);
    assert_eq!(alice_log.committee_state.joint_verifying_key,
               bob_log.committee_state.joint_verifying_key);
    assert_eq!(alice_log.committee_state.joint_verifying_key, Some(alice_vk_bytes));
    assert_eq!(alice_log.events.len(), bob_log.events.len(),
               "both logs must have identical event counts");
}
```

**Run:**
```bash
cargo nextest run --locked --features test-fixtures --test dfrost_dkg_integration -E 'test(two_of_two_dkg)'
# Expected: 1 test passes (uses real FROST crypto — may take 1-2s)
```

**Commit:**
```
feat(dfrost): Task 9 — two-of-two DKG full ceremony integration test

dfrost_dkg_integration.rs: real FROST crypto (no stubs). alice + bob each run
dkg_part1 → broadcast dr(rn=1) → dkg_part2 → encrypt+post dr(rn=2) →
apply_with_identity decrypt → dkg_part3 → post dk. Both logs apply all events via
apply() / apply_with_identity(). Asserts: joint_verifying_key matches across nodes,
CommitteeState.active=true on both, same event count.

ZEB-301
```

---

### Task 10 — Threshold Sign + VRF Beacon Integration Test

**Files:** `src-tauri/tests/dfrost_dkg_integration.rs` (add tests)

**Add to existing integration test file:**

```rust
/// Run the full 2-of-2 DKG and return (alice_key_pkg, alice_pub_pkg, bob_key_pkg,
/// alice_log, bob_log) ready for signing tests.
/// (Helper shared across tasks 10 and 11.)
fn run_2of2_dkg() -> (
    frost::keys::KeyPackage, frost::keys::PublicKeyPackage,
    frost::keys::KeyPackage,
    DfrostLog, DfrostLog,
    OwnerAddr, OwnerAddr,
) {
    // ... (condensed version of the test above, returns the key packages + logs)
    // Factor out from two_of_two_dkg_full_ceremony_converges.
    todo!("extract from Task 9 test — copy the setup code here as a helper fn")
}

#[test]
fn threshold_sign_produces_verifiable_vrf_beacon() {
    let alice = OwnerAddr([0x01; 16]);
    let bob   = OwnerAddr([0x02; 16]);
    let alice_id = identifier_for_index(0);
    let bob_id   = identifier_for_index(1);

    // Run DKG to get key packages (inline the 2-of-2 DKG setup).
    // ... (same setup as Task 9 test)
    // [For brevity in the plan: implement this as a call to the helper above]

    let poll_event_hash = [0xde; 32];
    let epoch = 1u64;
    let vrf_seed = derive_vrf_seed(&poll_event_hash, epoch);
    let signing_ci = {
        let mut h = sha2::Sha256::new();
        sha2::Digest::update(&mut h, b"ts-v1");
        sha2::Digest::update(&mut h, &vrf_seed);
        let arr: [u8; 32] = sha2::Digest::finalize(h).into();
        arr
    };

    // Both members produce commitments and shares
    let (alice_nonces, alice_cm) = threshold_sign_round1(&alice_key_pkg).unwrap();
    let (bob_nonces, bob_cm)     = threshold_sign_round1(&bob_key_pkg).unwrap();
    let alice_cm_bytes = commitments_to_bytes(&alice_cm).unwrap();
    let bob_cm_bytes   = commitments_to_bytes(&bob_cm).unwrap();

    // Coordinator builds signing package from both commitments
    let all_cms: BTreeMap<frost::Identifier, Vec<u8>> = [
        (alice_id, alice_cm_bytes.clone()), (bob_id, bob_cm_bytes.clone()),
    ].into_iter().collect();
    let signing_pkg = build_signing_package(&vrf_seed, &all_cms).unwrap();

    // Each member produces their share
    let alice_sh_bytes = threshold_sign_round2(&alice_key_pkg, &alice_nonces, &signing_pkg).unwrap();
    let bob_sh_bytes   = threshold_sign_round2(&bob_key_pkg,   &bob_nonces,   &signing_pkg).unwrap();

    // Aggregate
    let all_shares: BTreeMap<frost::Identifier, Vec<u8>> = [
        (alice_id, alice_sh_bytes), (bob_id, bob_sh_bytes),
    ].into_iter().collect();
    let sig_bytes = aggregate_signatures(&signing_pkg, &all_shares, &alice_pub_pkg).unwrap();
    assert_eq!(sig_bytes.len(), 64);

    // Verify the Schnorr signature
    let joint_vk_bytes = verifying_key_to_bytes(alice_pub_pkg.verifying_key());
    verify_schnorr_signature(&joint_vk_bytes, &vrf_seed, &sig_bytes).expect("sig must verify");

    // Compute VRF output
    let r_compressed: [u8; 32] = sig_bytes[..32].try_into().unwrap();
    let vrf_output = derive_vrf_output(&r_compressed);
    assert_ne!(vrf_output, [0u8; 32], "VRF output must be non-trivial");

    // Apply ts + vb events to both logs
    let ts_alice = make_ev(DfrostEventKind::ThresholdSign, alice, encode(&ThresholdSignPayload {
        ceremony_id: signing_ci, message_hash: vrf_seed,
        commitment_bytes: alice_cm_bytes, share_bytes: all_shares[&alice_id].clone(),
    }), 4000);
    let ts_bob = make_ev(DfrostEventKind::ThresholdSign, bob, encode(&ThresholdSignPayload {
        ceremony_id: signing_ci, message_hash: vrf_seed,
        commitment_bytes: bob_cm_bytes, share_bytes: all_shares[&bob_id].clone(),
    }), 4001);
    let vb = make_ev(DfrostEventKind::VrfBeacon, alice, encode(&VrfBeaconPayload {
        ceremony_id: signing_ci, message_hash: vrf_seed,
        signature: sig_bytes.clone(), vrf_output,
    }), 5000);

    for log in [&mut alice_log, &mut bob_log] {
        log.apply(ts_alice.clone()).unwrap();
        log.apply(ts_bob.clone()).unwrap();
        log.apply(vb.clone()).unwrap();
        // After vb, pending_sign session must be cleared
        assert!(!log.committee_state.pending_sign.contains_key(&signing_ci));
    }
}
```

**Run:**
```bash
cargo nextest run --locked --features test-fixtures --test dfrost_dkg_integration -E 'test(threshold_sign_produces)'
# Expected: 1 test passes
```

**Commit:**
```
feat(dfrost): Task 10 — threshold sign + VRF beacon integration test

threshold_sign_produces_verifiable_vrf_beacon: runs full 2-of-2 FROST signing ceremony
using real round1::commit + round2::sign + aggregate. Verifies: aggregated Schnorr sig
passes VerifyingKey::verify, VRF output = SHA-256(dfrost-vrf-v1 || R_compressed),
both logs accept ts+vb events, pending_sign cleared after vb. Uses actual FROST crypto —
no stubs.

ZEB-301
```

---

### Task 11 — NodeState Extension + Module Wiring

**Files:** `src-tauri/src/lib.rs` (modify NodeState, Default, start_node, stop_inner)

**Failing test first** (in lib.rs inline tests or separate):

```rust
// In src-tauri/src/lib.rs bottom-of-file #[cfg(test)] block or existing test module:
#[cfg(test)]
mod dfrost_nodestate_tests {
    use super::*;
    #[test]
    fn node_state_has_dfrost_logs_field() {
        let state = NodeState::default();
        // dfrost_logs starts as an empty HashMap
        let logs = state.dfrost_logs.blocking_lock();
        assert!(logs.is_empty());
    }
}
```

**Implementation:** add to `NodeState`:

```rust
/// ZEB-301 Phase 4a-Foundation: per-community D-FROST committee event logs.
/// Lazy-populated on first dfrost_* IPC call for a community.
/// Parallel to voting_logs.
pub dfrost_logs: std::sync::Arc<
    tokio::sync::Mutex<
        std::collections::HashMap<
            crate::owner_state_types::SpaceId,
            std::sync::Arc<tokio::sync::Mutex<crate::community_dfrost_log::DfrostLog>>,
        >,
    >,
>,
```

Add to `NodeState::default()`:
```rust
dfrost_logs: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
```

No cleanup needed in `stop_inner` — `DfrostLog` is in-memory only (no background tasks in foundation phase; no Zenoh sync wiring — that's Phase 4a-main).

Also add helper (mirrors `ensure_voting_log_for`):
```rust
/// Get or create the DfrostLog for a community.
async fn ensure_dfrost_log_for(
    dfrost_logs: &tokio::sync::Mutex<HashMap<SpaceId, Arc<tokio::sync::Mutex<DfrostLog>>>>,
    community_id: SpaceId,
) -> Arc<tokio::sync::Mutex<DfrostLog>> {
    let mut map = dfrost_logs.lock().await;
    map.entry(community_id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(DfrostLog::new())))
        .clone()
}
```

**Run:**
```bash
cargo nextest run --locked --features test-fixtures -E 'test(node_state_has_dfrost)'
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

**Commit:**
```
feat(dfrost): Task 11 — NodeState.dfrost_logs field + ensure_dfrost_log_for helper

Adds dfrost_logs: Arc<Mutex<HashMap<SpaceId, Arc<Mutex<DfrostLog>>>>> to NodeState
parallel to voting_logs. Default initializes empty. ensure_dfrost_log_for helper
mirrors ensure_voting_log_for. No stop_inner cleanup (DfrostLog is in-memory; Zenoh
sync wiring is Phase 4a-main). Test: dfrost_logs starts empty.

ZEB-301
```

---

### Task 12 — IPC `dfrost_initiate_dkg` + `dfrost_contribute_round2`

**Files:** `src-tauri/src/lib.rs` (add 2 `#[tauri::command]` functions)

**Failing test first** (using `tauri::test::mock_builder`):

```rust
// In src-tauri/src/lib.rs #[cfg(test)] or a new test file:
#[cfg(test)]
mod dfrost_ipc_tests {
    // These tests exercise the IPC function logic directly (bypassing Tauri runtime)
    // by calling the async functions with a mock state.
    use super::*;
    // ... (use tokio::test + construct NodeState manually)
    // Due to Tauri State<> requiring a running app, these tests use the inner
    // async functions directly (extract business logic into non-tauri functions).
}
```

Note: The IPC functions wrap business logic. Extract logic into `dfrost_initiate_dkg_inner` async function for testability:

```rust
async fn dfrost_initiate_dkg_inner(
    dfrost_logs: Arc<tokio::sync::Mutex<HashMap<SpaceId, Arc<tokio::sync::Mutex<DfrostLog>>>>>,
    community_id: SpaceId,
    self_addr: OwnerAddr,
    self_signing_key: ed25519_dalek::SigningKey,
    members_sorted: Vec<OwnerAddr>,
    threshold: u16,
    hlc: Hlc,
) -> Result<String, String> {
    let max_signers = members_sorted.len() as u16;
    if threshold == 0 || threshold > max_signers {
        return Err(format!("threshold {threshold} out of range 1..={max_signers}"));
    }

    // Find self's index in sorted member list
    let self_idx = members_sorted.iter().position(|a| a == &self_addr)
        .ok_or("self_addr not in members list")?;
    let self_id = identifier_for_index(self_idx);

    let community_id_space = community_id;
    let ceremony_id = derive_ceremony_id(&community_id_space, hlc.wall_ms, b"dkg-v1");

    let dfrost_log = ensure_dfrost_log_for(&dfrost_logs, community_id_space).await;
    let mut log = dfrost_log.lock().await;

    // Seed pending_dkg
    log.committee_state.pending_dkg = Some(PendingCeremony {
        ceremony_id, members: members_sorted.clone(),
        threshold, max_signers, proposed_epoch: log.committee_state.current_epoch + 1,
        ..Default::default()
    });

    // Run DKG part1
    let (sec_pkg, r1_bytes) = dkg_part1_local(self_id, max_signers, threshold)
        .map_err(|e| e)?;
    log.local_dkg_secret = Some(sec_pkg);

    // Build + sign dr(rn=1) event
    use ed25519_dalek::Signer;
    let payload = DkgRoundPayload {
        ceremony_id, round_num: 1,
        round1_package: Some(r1_bytes),
        recipient_ciphertexts: None,
    };
    let mut pd = Vec::new();
    ciborium::into_writer(&payload, &mut pd).map_err(|e| format!("encode: {e:?}"))?;
    let mut ev = SignedCommitteeEvent {
        tag: 'd', version: 1, committee_tier: 0,
        kind: DfrostEventKind::DkgRound,
        hlc, actor: self_addr, payload: pd, sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|e| format!("signing_bytes: {e:?}"))?;
    ev.sig = self_signing_key.sign(&sb).to_bytes().to_vec();

    // Apply to own log
    log.apply(ev.clone()).map_err(|e| format!("apply: {e:?}"))?;

    // TODO ZEB-301 Phase 4a-main: broadcast via Zenoh dfrost log engine

    Ok(hex::encode(ceremony_id))
}
```

**The `#[tauri::command]` wrapper:**

```rust
#[tauri::command]
async fn dfrost_initiate_dkg(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
    members: Vec<String>,
    threshold: u16,
) -> Result<String, String> {
    // Extract needed fields from NodeState (minimal lock hold)
    let (dfrost_logs, self_addr, signing_key, hlc) = {
        let ns = state.lock().map_err(|e| format!("state lock: {e}"))?;
        let self_addr = ns.dm_self_owner.ok_or("node not running")?;
        let dfrost_logs = ns.dfrost_logs.clone();
        // signing_key from ns.crdt_state or node identity — placeholder
        // In full impl: extract from the running identity
        (dfrost_logs, self_addr, /*signing_key*/ todo!(), /*hlc*/ todo!())
    };
    // ... parse members, call dfrost_initiate_dkg_inner
    todo!("full impl wires signing key + HLC from NodeState")
}
```

**Note:** The full IPC impl requires the signing key to be accessible from NodeState. This is wired in the same pattern as other signed-event IPCs (extract from `crdt_state` or add a `dfrost_signing_key` field). The plan marks this as a `TODO ZEB-301-IPC-wiring` inline comment — the IPC test in Task 12 tests the inner function directly.

**Failing test (inner function):**

```rust
#[tokio::test]
async fn dfrost_initiate_dkg_inner_creates_pending_ceremony() {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    let alice = OwnerAddr([0x01; 16]);
    let bob   = OwnerAddr([0x02; 16]);
    let community_id = SpaceId([0xc0; 16]);
    let dfrost_logs: Arc<Mutex<HashMap<SpaceId, Arc<Mutex<DfrostLog>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let signing_key = SigningKey::generate(&mut OsRng);
    let hlc = Hlc { wall_ms: 1000, logical: 0, device_id: "t".into() };

    let ceremony_id_hex = dfrost_initiate_dkg_inner(
        dfrost_logs.clone(), community_id, alice,
        signing_key, vec![alice, bob], 2, hlc,
    ).await.expect("initiate dkg");

    assert_eq!(ceremony_id_hex.len(), 64, "ceremony_id must be 32 bytes hex-encoded");

    let map = dfrost_logs.lock().await;
    let log = map.get(&community_id).expect("log must exist").lock().await;
    assert!(log.committee_state.pending_dkg.is_some());
    assert_eq!(log.committee_state.pending_dkg.as_ref().unwrap().threshold, 2);
    assert_eq!(log.events.len(), 1, "one dr(rn=1) event must be in the log");
}
```

**Run:**
```bash
cargo nextest run --locked --features test-fixtures -E 'test(dfrost_initiate_dkg_inner)'
# Expected: 1 test passes
```

**Commit:**
```
feat(dfrost): Task 12 — dfrost_initiate_dkg inner + Tauri command stub

dfrost_initiate_dkg_inner: validates threshold, assigns sequential Identifier,
derive_ceremony_id, seeds pending_dkg in DfrostLog, runs dkg_part1_local, builds +
signs dr(rn=1) event, applies to own log. Tauri command wrapper stubs (signing key +
HLC wiring = TODO ZEB-301-IPC-wiring for full NodeState integration). Test: inner
function creates pending ceremony, dr(rn=1) event in log.

ZEB-301
```

---

### Task 13 — IPCs `dfrost_finalize_dkg`, `dfrost_request_vrf_beacon`, `dfrost_propose_refresh`

**Files:** `src-tauri/src/lib.rs` (add 3 more inner functions + Tauri command stubs)

**Failing test:**

```rust
#[tokio::test]
async fn dfrost_request_vrf_beacon_returns_pending_when_insufficient_ts() {
    // Set up an active committee but no ts events yet.
    // Expect Err("pending:0/2") or similar.
    let dfrost_logs = Arc::new(Mutex::new(HashMap::new()));
    let community_id = SpaceId([0xc0; 16]);
    let log_arc = Arc::new(Mutex::new(DfrostLog::new()));
    {
        let mut log = log_arc.lock().await;
        log.committee_state.active = true;
        log.committee_state.current_epoch = 1;
        log.committee_state.members = vec![OwnerAddr([0x01; 16]), OwnerAddr([0x02; 16])];
        log.committee_state.threshold = 2;
        log.committee_state.joint_verifying_key = Some([0x11; 32]);
    }
    dfrost_logs.lock().await.insert(community_id, log_arc);

    let poll_hash = [0xde; 32];
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let self_addr = OwnerAddr([0x01; 16]);

    let result = dfrost_request_vrf_beacon_inner(
        dfrost_logs, community_id, self_addr, signing_key,
        poll_hash, Hlc { wall_ms: 2000, logical: 0, device_id: "t".into() },
    ).await;

    // Should succeed (posts local ts contribution) and return "pending:1/2"
    match result {
        Err(msg) if msg.starts_with("pending:") => { /* expected */ }
        Ok(hex) => {} // or Ok if this node's ts makes threshold=1... adjust threshold
        other => panic!("unexpected result: {other:?}"),
    }
}
```

**Implementation outline:**

`dfrost_request_vrf_beacon_inner`:
1. Looks up DfrostLog for community
2. Computes `vrf_seed = derive_vrf_seed(&poll_hash, current_epoch)`
3. Computes `signing_ci = SHA-256(b"ts-v1" || vrf_seed)`
4. Checks `pending_sign[signing_ci].contributions.len() >= threshold`
5. If yes: builds SigningPackage, aggregates, derives VRF output, posts `vb` event, returns `Ok(hex(vrf_output))`
6. If no: runs `round1::commit` (if not already posted own ts), `round2::sign` (needs SigningPackage — deferred; posts ts with cm+empty sh first), posts `ts` event, returns `Err("pending:N/M")`

`dfrost_propose_refresh_inner`:
1. Calls `compute_refresh_shares(old_pub_key_pkg, &new_identifiers)`
2. Encrypts each share to the corresponding member's X25519 pub key
3. Posts `rf(rn=1)` event

`dfrost_finalize_dkg_inner`:
1. Checks `pending_dkg.round2_packages.len() >= threshold - 1`
2. Calls `dkg_part3_local`, stores key packages in DfrostLog
3. Posts `dk` event

**Run:**
```bash
cargo nextest run --locked --features test-fixtures -E 'test(dfrost_request_vrf_beacon_returns_pending)'
# Expected: 1 test passes
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

**Commit:**
```
feat(dfrost): Task 13 — dfrost_finalize_dkg, dfrost_request_vrf_beacon, dfrost_propose_refresh

dfrost_finalize_dkg_inner: checks round2_packages quorum, dkg_part3_local, stores
local KeyPackage, posts dk event. dfrost_request_vrf_beacon_inner: computes vrf_seed
+ signing ceremony_id, checks ts quorum; if met: aggregates + posts vb + returns VRF
output hex; if not: posts own ts contribution + returns Err("pending:N/M").
dfrost_propose_refresh_inner: compute_refresh_shares, encrypt per member, post rf(rn=1).
All three have Tauri command stubs. Test: beacon returns pending when ts count < threshold.

ZEB-301
```

---

### Task 14 — 5-Gate Sweep + Push + PR

**Files:** none (sweep + push only)

**Pre-push sweep:**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri

# Gate 1: fmt
cargo fmt --all
# Verify clean:
cargo fmt --all -- --check

# Gate 2: clippy
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings

# Gate 3: nextest (full suite)
cargo nextest run --locked --workspace --all-targets --features test-fixtures

# Gate 4: frontend type check (from repo root)
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit

# Gate 5: frontend tests
npx vitest run
```

**Expected output:**

- fmt: no output (clean)
- clippy: no warnings, no errors
- nextest: all tests pass including new dfrost_* tests
- tsc: no errors (no frontend changes in this PR)
- vitest: all existing tests pass

**Fix any issues before pushing.** Common issues:
- `unused import` warnings in new modules → add `#[allow(unused_imports)]` or remove
- Missing `use` declarations in integration tests
- `dead_code` warnings on stub IPC inner functions → add `#[allow(dead_code)]` temporarily or add a trivial test that calls each

**Push:**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-301-phase4a-foundation-dfrost-committee
```

**Create PR:**

```bash
gh pr create \
  --title "feat(dfrost): ZEB-301 Phase 4a-Foundation — D-FROST committee (DKG + threshold VRF + proactive refresh)" \
  --body "$(cat <<'EOF'
## Summary

Ships the cryptographic foundation for Harmony's D-FROST committee. No voting-layer
integration (sortition/STAR/drafting/UI) — those land in Phase 4a-main on top of this.

## What's in this PR

- `community_dfrost_types.rs`: 5 event kinds (`dr/dk/ts/vb/rf`), `SignedCommitteeEvent`
  envelope (8-field 2-char-key CBOR, `tg='d'`, `tr=0`), all payload structs
- `community_dfrost_log.rs`: `DfrostLog` + `CommitteeState` + `PendingCeremony` +
  `PendingSignSession`; `apply()` dispatcher; `apply_with_identity()` for X25519-sealed
  round-2 decryption
- `community_dfrost_crypto.rs`: FROST wrappers — DKG part1/part2/part3,
  threshold sign (commit/sign/aggregate), VRF derivation, proactive refresh
  (`compute_refreshing_shares`)
- `NodeState.dfrost_logs` field + `ensure_dfrost_log_for` helper
- 5 IPC functions (inner + Tauri stubs): `dfrost_initiate_dkg`,
  `dfrost_contribute_round2`, `dfrost_finalize_dkg`, `dfrost_request_vrf_beacon`,
  `dfrost_propose_refresh`
- Wire format fixture pinning (`wire_format_dfrost_fixtures.rs`): 6 CBOR fixtures
- Integration tests (`dfrost_dkg_integration.rs`): full 2-of-2 DKG ceremony with real
  FROST crypto; threshold sign + VRF beacon derivation + verification

## Architecture decisions

- **FROST-Ristretto255**: Zcash Foundation crate (`frost-ristretto255 = "2"`),
  Apache-2.0/MIT. Schnorr over Ristretto255.
- **Identifier binding**: sorted OwnerAddr → sequential 1-indexed `Identifier::try_from(u16)`.
  Frozen at ceremony time; refresh updates the mapping.
- **VRF derivation**: `SHA-256(b"dfrost-vrf-v1" || R_compressed)` where `R` is the Schnorr
  nonce from `frost::aggregate`. Verifiable via `VerifyingKey::verify`, no additional ZK.
- **Proactive refresh**: `keys::refresh::compute_refreshing_shares` (coordinator-mediated
  foundation-phase approach). Group public key invariant enforced in `apply_dkg_complete`.
- **Epoch**: count of finalized ceremonies (`dk` quorum events). VRF seed includes epoch.
- **Beacon auth**: admin-only (power ≥ 100) in this phase.

## Phase 4a-main follow-up (NOT in this PR)

- Sortition: VRF output → Fisher-Yates sampling of eligible electorate
- STAR ratification, deliberation, drafting phases
- Frontend: committee status panel, VRF beacon progress indicator
- Zenoh dfrost log engine (sync with peers)
- `dfrost_initiate_dkg` full IPC wiring (signing key from NodeState identity)

## Linear

[ZEB-301](https://linear.app/zeblith/issue/ZEB-301) — Phase 4a-Foundation
Parent: [ZEB-293](https://linear.app/zeblith/issue/ZEB-293)
Umbrella: [ZEB-289](https://linear.app/zeblith/issue/ZEB-289)
EOF
)"
```

No `Closes #N` — the PR number is not yet known at plan time. After `gh pr create` outputs the PR URL, add `Closes #N` to the PR description via `gh pr edit`.

**Final commit (sweep + fmt):**

```
chore(dfrost): Task 14 — 5-gate sweep passes, push + PR created

cargo fmt --all -- --check: clean.
cargo clippy --locked --all-targets --features test-fixtures --no-deps -D warnings: 0 warnings.
cargo nextest run --locked --workspace --all-targets --features test-fixtures: all pass.
npx tsc --noEmit: no errors.
npx vitest run: all pass.

ZEB-301
```

---

## 11. PR Body Template

```markdown
## Summary

Ships the cryptographic foundation for Harmony's D-FROST committee: distributed key
generation (DKG), threshold VRF beacon, and proactive share refresh. No voting-layer
wiring in this PR — sortition/STAR/deliberation/UI land in Phase 4a-main.

## What ships

| File | New/Modified | Purpose |
|---|---|---|
| `community_dfrost_types.rs` | New | 5 event kinds, wire envelope, all payload structs |
| `community_dfrost_log.rs` | New | DfrostLog, CommitteeState, apply() dispatcher |
| `community_dfrost_crypto.rs` | New | FROST DKG/sign/VRF/refresh wrappers |
| `lib.rs` | Modified | dfrost_logs NodeState field + 5 IPC commands |
| `wire_format_dfrost_fixtures.rs` | New | 6 CBOR fixture pins |
| `dfrost_dkg_integration.rs` | New | 2-of-2 DKG + threshold sign integration tests |

## Key decisions

- FROST-Ristretto255 (`frost-ristretto255 = "2"`, Zcash Foundation, Apache-2.0/MIT)
- FROST Identifier = sorted-member 1-indexed sequential u16 (deterministic, frozen at ceremony)
- VRF output = `SHA-256("dfrost-vrf-v1" || R_compressed)` — Schnorr nonce extraction
- Proactive refresh: `keys::refresh::compute_refreshing_shares` (coordinator-mediated v1)
- Epoch = count of finalized `dk` quorums; VRF seed includes epoch against replay
- Admin-only beacon requests for this phase

## Phase 4a-main consumer

`dfrost_request_vrf_beacon` VRF output feeds into sortition (Fisher-Yates seeded by VRF).
`dfrost_logs` NodeState field is the shared state. Zenoh committee log sync wires in Phase 4a-main.

## Linked issues

[ZEB-301](https://linear.app/zeblith/issue/ZEB-301) — this PR
[ZEB-293](https://linear.app/zeblith/issue/ZEB-293) — parent epic
[ZEB-289](https://linear.app/zeblith/issue/ZEB-289) — umbrella voting/polling

Closes #<PR_NUMBER>
```

---

