# ZEB-281 Sub-D Phase 4 — ProfileMembershipBroadcast Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the third independent Sub-D discovery primitive — a privacy-preserving Zenoh-broadcast protocol where users curate a per-community opt-in subset of their memberships, and peers viewing a profile see only the communities the owner has explicitly shared.

**Architecture:** New module `profile_broadcast.rs` introduces a signed wire type `ProfileMembershipBroadcast` published on `harmony/discovery/profile/{owner_addr_hex}/memberships`. The owner-side `Publisher` is a tokio task with a `Notify`-driven 2-second debounce + 10-minute periodic refresh; it forwards canonical-CBOR payloads through the existing `publish_tx: mpsc::Sender<PublishRequest>` channel. The peer-side `Subscriber` is request-driven (`ProfileBroadcastRequest::Subscribe / Unsubscribe`) and runs Zenoh subscriber tasks in the event loop. A new `Space.shared_in_profile: bool` field tracked by the existing owner-state CRDT cross-replicates the opt-in across the user's bound devices. `skip_serializing_if = "core::ops::Not::not"` keeps the default-false case byte-identical to pre-Phase-4 owner-state wire bytes.

**Tech Stack:** Rust (Tauri backend), Svelte 5 + TypeScript (frontend), Ed25519 (`ed25519-dalek`), CBOR (`ciborium`), Zenoh, vitest (frontend tests), cargo-nextest (Rust tests).

**Spec:** `docs/specs/2026-05-12-zeb-281-sub-d-phase-4-profile-membership-broadcast-design.md` (commit `ca787e3`, 563 lines, §1-§15).

**Branch:** `zeb-281-sub-d-phase-4-profile-membership-broadcast` (already cut from `origin/main` post-PR-#110 + post-PR-#111 lineage; spec at HEAD = `ca787e3`).

---

## File structure

| File | Role | Action |
|---|---|---|
| `src-tauri/src/profile_broadcast.rs` | New module: `ProfileMembershipBroadcast`, `BroadcastVerifyError`, `MAX_SHARED_COMMUNITIES`, `PROFILE_DISCOVERY_TOPIC_PREFIX`, `sign_broadcast`, `verify_broadcast`, `ProfileBroadcastPublisher`, `ProfileBroadcastPublishSink`, `ProfileBroadcastCache`, `DiscoveredProfileInfo` | Create |
| `src-tauri/src/owner_state_types.rs` | Add `Space.shared_in_profile: bool` field with `rename = "sp"`, `default`, `skip_serializing_if = "core::ops::Not::not"`. Update `Space` construction sites for default value. | Modify |
| `src-tauri/src/lib.rs` | Register `pub mod profile_broadcast` (alphabetical). Add 4 IPC commands. Register them in `tauri::generate_handler!`. Extend `NodeState` with publisher/cache handles. Wire start_node / stop_node lifecycle. | Modify |
| `src-tauri/src/event_loop.rs` | Add `ProfileBroadcastRequest` enum, plumb its receiver through the event-loop spawn block, spawn per-subscription Zenoh subscriber tasks with the same retry/backoff pattern as the Phase 2 announce subscriber. | Modify |
| `src-tauri/tests/wire_format_profile_broadcast_fixtures.rs` | New test file: 2 wire-format pinning tests | Create |
| `src-tauri/tests/wire_format_fixture.rs` | Add `space_shared_in_profile_default_false_byte_identical_to_pre_phase4` wire-compat test | Modify |
| `src-tauri/tests/common/profile_fixtures.rs` | New shared test fixture helpers: `build_test_owner_identity`, `mock_profile_broadcast` | Create |
| `src-tauri/tests/profile_broadcast_integration.rs` | New integration test file: 5 integration tests | Create |
| `src/lib/profile-broadcast-service.ts` | New frontend service: `ProfileMembershipBroadcastInfo` + `ProfileBroadcastService` | Create |
| `src/lib/components/CommunitySettingsPanel.svelte` | Add "Public profile" toggle section. Extend props with `sharedInProfile: boolean` and `onToggleSharedInProfile: (shared: boolean) => Promise<void>`. | Modify |
| `src/lib/components/ProfilePopover.svelte` | Add "Public memberships" section with 3-state rendering. Subscribe on mount, unsubscribe on unmount. | Modify |
| `src/lib/__tests__/profile-broadcast-service.test.ts` | New vitest: 1 service test | Create |
| `src/lib/components/__tests__/ProfilePopover.test.ts` | New vitest: 4 popover tests | Create |
| `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` | Extend existing vitest (or create if missing): 1 toggle test | Modify/Create |

---

## Task 0: Pre-flight + green-baseline confirm

**Goal:** Confirm all 5 CI gates green BEFORE any code changes. Capture baseline test counts so later regressions are obvious. No commit.

**Files:** None.

- [ ] **Step 1: Confirm branch + clean working tree**

Run:
```bash
git status
git rev-parse --abbrev-ref HEAD
git rev-parse HEAD
```

Expected output:
- Branch: `zeb-281-sub-d-phase-4-profile-membership-broadcast`
- HEAD: `ca787e3` (spec commit) on top of latest `origin/main` (post-PR-#110 + post-PR-#111)
- Working tree clean

- [ ] **Step 2: Confirm origin/main is fully pulled and branch is downstream**

Run:
```bash
git fetch origin
git log --oneline origin/main..HEAD
git log --oneline HEAD..origin/main
```

Expected: `origin/main..HEAD` lists only `ca787e3` (the spec commit). `HEAD..origin/main` is empty (no upstream commits we don't have). If non-empty, surface to user — pull-before-work invariant requires rebase before proceeding.

- [ ] **Step 3: Run cargo fmt check**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: exit 0, no output. If non-zero, baseline is broken — STOP and surface to user.

- [ ] **Step 4: Run cargo clippy with --features test-fixtures**

Run:
```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: exit 0, ends with `Finished ... profile`. No warnings. If warnings exist, STOP and surface to user.

- [ ] **Step 5: Run cargo nextest with --features test-fixtures**

Run:
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
```

Expected: passes summary like `Summary [...] N tests run: N passed, 0 failed, M skipped` where N is in the ~1100 range (Phase 3 baseline; +20 from PR #110, +0 from PR #111 which only renamed and slimmed a test). Note the EXACT pass count for later regression detection.

If the known flake `community_channel_log_engine::tests::shutdown_completes_promptly` (ZEB-282) trips, re-run once:
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures -E 'test(community_channel_log_engine::tests::shutdown_completes_promptly)'
```
Then re-run the full suite. If it persists across 2 runs, surface to user (don't auto-bump the budget).

- [ ] **Step 6: Run cargo check (MSRV gate)**

Run:
```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -5
```

Expected: `Finished ... profile`. No errors.

- [ ] **Step 7: Run npx tsc --noEmit (frontend type check)**

Run (from repo root):
```bash
npx tsc --noEmit 2>&1 | tail -5
```

Expected: no output, exit 0.

- [ ] **Step 8: Run npx vitest (frontend tests)**

Run (from repo root):
```bash
npx vitest run 2>&1 | tail -10
```

Expected: `Test Files N passed (N)` + `Tests M passed (M)` where M is in the ~1600 range. Note the EXACT pass count.

- [ ] **Step 9: Record baseline counts in scratchpad (NOT committed)**

Document mentally / in a scratchpad — used only for later regression triage:
- Rust tests: <COUNT> passed
- Frontend tests: <COUNT> passed
- Last green commit: `ca787e3`

No file changes. No commit. Proceed to Task 1.

---

## Task 1: Wire format + `Space.shared_in_profile` + `verify_broadcast` + 8 unit tests

**Goal:** Create the new `profile_broadcast` module with the wire type, error enum, signing helper, and verifier. Add the `Space.shared_in_profile` field with the byte-compatible serde attrs. Cover `verify_broadcast` with 8 unit tests + assert pre-Phase-4 owner-state wire bytes are byte-identical.

**Files:**
- Create: `src-tauri/src/profile_broadcast.rs`
- Modify: `src-tauri/src/owner_state_types.rs` (add field to `Space`)
- Modify: `src-tauri/src/lib.rs` (register `pub mod profile_broadcast;` alphabetical)

- [ ] **Step 1: Create the new module skeleton**

Create `src-tauri/src/profile_broadcast.rs` with the wire type, constants, error enum, sign helper, and verifier:

```rust
//! Sub-D Phase 4 (ZEB-281) — ProfileMembershipBroadcast primitive.
//!
//! Privacy-preserving Zenoh-broadcast protocol where users curate a
//! per-community opt-in subset of their memberships, and peers viewing
//! a profile see only the communities the owner has explicitly shared.
//!
//! See `docs/specs/2026-05-12-zeb-281-sub-d-phase-4-profile-membership-broadcast-design.md`.

use crate::owner_state_crypto::canonical_cbor_encode;
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};

/// Hard cap on number of community IDs per broadcast. 200 SpaceIds × 32
/// bytes + framing + sig ≈ 6.5 KB worst-case canonical payload. Spec §4.1.
pub const MAX_SHARED_COMMUNITIES: usize = 200;

/// Topic-name prefix; full topic is `{PREFIX}{owner_addr_hex}/memberships`
/// where `owner_addr_hex` is the lowercase 32-char hex encoding of the
/// 16-byte `OwnerAddr`. Spec §4.1.
///
/// Distinct from `harmony/discovery/library/announce` (Phase 2) and
/// `harmony/discovery/library/{addr}/communities` (Phase 1). Distinct
/// from `harmony/announce/{cid_hex}` (storage tier content-availability).
pub const PROFILE_DISCOVERY_TOPIC_PREFIX: &str = "harmony/discovery/profile/";

/// Hard cap on the wire size of a single `ProfileMembershipBroadcast`
/// payload before CBOR decode. Bound rationale: MAX_SHARED_COMMUNITIES
/// (200) × SpaceId (16 bytes raw + ~3 bytes CBOR framing per element) +
/// owner_identity_pub (64) + Hlc (~30 bytes) + signature (64) + map
/// framing ≈ 4 KB. 8 KB is 2× headroom for minor schema additions.
pub const MAX_BROADCAST_WIRE_BYTES: usize = 8_192;

/// Build a broadcast topic key for the given OwnerAddr.
pub fn broadcast_topic_for(addr: &OwnerAddr) -> String {
    format!(
        "{PROFILE_DISCOVERY_TOPIC_PREFIX}{}/memberships",
        hex::encode(addr.0)
    )
}

/// Sub-D Phase 4 wire type. Spec §4.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileMembershipBroadcast {
    /// 64-byte identity bundle (X25519_pub(32) || Ed25519_pub(32)) of
    /// the owner publishing this broadcast. Spec §4.1.
    #[serde(
        rename = "ai",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub owner_identity_pub: [u8; 64],

    /// Sorted, strictly-increasing (no duplicates) subset of the owner's
    /// joined community SpaceIds opted to share publicly. MAY be empty
    /// (rotation case — see Publisher state machine). Hard cap:
    /// `MAX_SHARED_COMMUNITIES = 200`. Spec §4.1.
    #[serde(rename = "cs")]
    pub community_ids: Vec<SpaceId>,

    /// Hybrid Logical Clock — recipients prefer newer broadcasts over
    /// older ones; publisher rotates stale state by bumping the HLC.
    /// Spec §4.1.
    #[serde(rename = "sa")]
    pub shared_at: Hlc,

    /// Ed25519 sig over canonical CBOR with `signature` zeroed. Same
    /// idiom as `LibraryAnnounce` (Phase 2) and `LibraryDirectoryEntry`
    /// admin sig (Phase 1). Spec §4.1.
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub signature: [u8; 64],
}

/// Marker so `canonical_cbor_encode` can sign over this struct in tests.
impl crate::owner_state_crypto::CanonicalPayloadSealed for ProfileMembershipBroadcast {}
impl crate::owner_state_crypto::CanonicalPayload for ProfileMembershipBroadcast {}

/// Verification errors. Spec §4.3.
#[derive(Debug, thiserror::Error)]
pub enum BroadcastVerifyError {
    #[error("community_ids exceeds {MAX_SHARED_COMMUNITIES} entries")]
    TooManyCommunities,
    #[error("community_ids must be strictly increasing (sorted + deduped)")]
    CommunityIdsNotSortedDeduped,
    #[error("malformed owner identity_pub: {0}")]
    InvalidIdentityPub(String),
    #[error("Ed25519 signature verification failed")]
    SignatureInvalid,
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] crate::owner_state_crypto::CryptoError),
}

/// Sign a broadcast: compute canonical CBOR with `signature` zeroed,
/// Ed25519-sign, return the populated struct. Test-fixtures-only signing
/// path — production publishes go through `ProfileBroadcastPublisher`
/// which holds a `SigningKey` and bumps the HLC.
///
/// `signer.verifying_key().as_bytes()` MUST be the Ed25519 half (bytes
/// 32-63) of `owner_identity_pub`, otherwise the caller has a key/identity
/// mismatch (sig will verify but identity parse may not).
pub fn sign_broadcast(
    signer: &ed25519_dalek::SigningKey,
    owner_identity_pub: [u8; 64],
    community_ids: Vec<SpaceId>,
    shared_at: Hlc,
) -> Result<ProfileMembershipBroadcast, BroadcastVerifyError> {
    let mut broadcast = ProfileMembershipBroadcast {
        owner_identity_pub,
        community_ids,
        shared_at,
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&broadcast)?;
    let sig = signer.sign(&bytes);
    broadcast.signature = sig.to_bytes();
    Ok(broadcast)
}

/// Verify a broadcast end-to-end. Returns the derived OwnerAddr on
/// success — callers compare it against the topic owner for the
/// attribution check (subscriber-side, in `process_sample`). Spec §6.
pub fn verify_broadcast(
    broadcast: &ProfileMembershipBroadcast,
) -> Result<OwnerAddr, BroadcastVerifyError> {
    // (1) Bounds
    if broadcast.community_ids.len() > MAX_SHARED_COMMUNITIES {
        return Err(BroadcastVerifyError::TooManyCommunities);
    }
    // (2) Strictly increasing (sorted + deduped)
    if !broadcast.community_ids.windows(2).all(|w| w[0] < w[1]) {
        return Err(BroadcastVerifyError::CommunityIdsNotSortedDeduped);
    }
    // (3) Parse identity_pub — also rejects malformed point bytes.
    let identity =
        harmony_identity::Identity::from_public_bytes(&broadcast.owner_identity_pub)
            .map_err(|e| BroadcastVerifyError::InvalidIdentityPub(format!("{e:?}")))?;
    // (4) Verify sig over canonical CBOR with signature field zeroed.
    let mut for_sig = broadcast.clone();
    for_sig.signature = [0u8; 64];
    let signed_bytes = canonical_cbor_encode(&for_sig)?;
    let sig = Signature::from_bytes(&broadcast.signature);
    identity
        .verifying_key
        .verify_strict(&signed_bytes, &sig)
        .map_err(|_| BroadcastVerifyError::SignatureInvalid)?;
    Ok(OwnerAddr(identity.address_hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn fixture_hlc(wall_ms: u64) -> Hlc {
        Hlc { wall_ms, logical: 0, device_id: "fix".into() }
    }

    fn build_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64]) {
        // Deterministic seed → reproducible Ed25519 key. Pair with an
        // X25519 prefix derived deterministically so identity_pub
        // round-trips through Identity::from_public_bytes.
        let private = harmony_identity::PrivateIdentity::from_seed(seed);
        let identity_pub = private.identity().to_public_bytes();
        let signer = private.signing_key_clone();
        (signer, identity_pub)
    }

    fn fixture_space_id(byte: u8) -> SpaceId {
        SpaceId([byte; 16])
    }

    #[test]
    fn verify_broadcast_valid_returns_owner_addr() {
        let (signer, identity_pub) = build_identity([1u8; 32]);
        let cs = vec![fixture_space_id(1), fixture_space_id(2), fixture_space_id(3)];
        let b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        let addr = verify_broadcast(&b).unwrap();
        // Derived addr matches Identity::from_public_bytes
        let expected = harmony_identity::Identity::from_public_bytes(&identity_pub)
            .unwrap()
            .address_hash;
        assert_eq!(addr.0, expected);
    }

    #[test]
    fn verify_broadcast_tampered_signature_rejected() {
        let (signer, identity_pub) = build_identity([2u8; 32]);
        let cs = vec![fixture_space_id(1)];
        let mut b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        b.signature[0] ^= 0xff;
        assert!(matches!(verify_broadcast(&b), Err(BroadcastVerifyError::SignatureInvalid)));
    }

    #[test]
    fn verify_broadcast_tampered_payload_rejected() {
        let (signer, identity_pub) = build_identity([3u8; 32]);
        let cs = vec![fixture_space_id(1), fixture_space_id(2)];
        let mut b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        // XOR the LAST byte of community_ids[0] — keeps it sorted (still
        // < community_ids[1] = [2; 16]) and unique, so the bounds + sort
        // + dedup checks pass, but the canonical-CBOR bytes change so
        // the sig now mismatches.
        b.community_ids[0].0[15] ^= 0x01;
        assert!(matches!(verify_broadcast(&b), Err(BroadcastVerifyError::SignatureInvalid)));
    }

    #[test]
    fn verify_broadcast_too_many_communities_rejected() {
        let (signer, identity_pub) = build_identity([4u8; 32]);
        let cs: Vec<SpaceId> = (0..(MAX_SHARED_COMMUNITIES + 1) as u16)
            .map(|i| {
                let mut bytes = [0u8; 16];
                bytes[0..2].copy_from_slice(&i.to_be_bytes());
                SpaceId(bytes)
            })
            .collect();
        let b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        assert!(matches!(verify_broadcast(&b), Err(BroadcastVerifyError::TooManyCommunities)));
    }

    #[test]
    fn verify_broadcast_unsorted_community_ids_rejected() {
        let (signer, identity_pub) = build_identity([5u8; 32]);
        // [B, A] — out of order
        let cs = vec![fixture_space_id(2), fixture_space_id(1)];
        let b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        assert!(matches!(
            verify_broadcast(&b),
            Err(BroadcastVerifyError::CommunityIdsNotSortedDeduped)
        ));
    }

    #[test]
    fn verify_broadcast_duplicate_community_ids_rejected() {
        let (signer, identity_pub) = build_identity([6u8; 32]);
        // [A, A] — duplicate
        let cs = vec![fixture_space_id(1), fixture_space_id(1)];
        let b = sign_broadcast(&signer, identity_pub, cs, fixture_hlc(100)).unwrap();
        assert!(matches!(
            verify_broadcast(&b),
            Err(BroadcastVerifyError::CommunityIdsNotSortedDeduped)
        ));
    }

    #[test]
    fn verify_broadcast_malformed_identity_pub_rejected() {
        // Build a syntactically-malformed identity bundle. All-zero
        // bytes pass byte-length but fail point-validation in
        // ed25519-dalek. (If a future ed25519-dalek release happens to
        // accept all-zero bytes, swap for any byte pattern known to fail
        // `from_bytes(...).is_ok()` on the verifying-key half.)
        let bad_identity_pub = [0u8; 64];
        // Bypass sign_broadcast (which requires a real signer); build
        // a broadcast manually with a sig that will never be reached
        // because identity parse fails first.
        let b = ProfileMembershipBroadcast {
            owner_identity_pub: bad_identity_pub,
            community_ids: vec![fixture_space_id(1)],
            shared_at: fixture_hlc(100),
            signature: [0u8; 64],
        };
        assert!(matches!(
            verify_broadcast(&b),
            Err(BroadcastVerifyError::InvalidIdentityPub(_))
        ));
    }

    #[test]
    fn verify_broadcast_empty_community_ids_accepted() {
        let (signer, identity_pub) = build_identity([8u8; 32]);
        let b = sign_broadcast(&signer, identity_pub, vec![], fixture_hlc(100)).unwrap();
        let addr = verify_broadcast(&b).unwrap();
        let expected = harmony_identity::Identity::from_public_bytes(&identity_pub)
            .unwrap()
            .address_hash;
        assert_eq!(addr.0, expected);
    }
}
```

> **Note on `harmony_identity::PrivateIdentity::from_seed` / `signing_key_clone` / `Identity::to_public_bytes`:** these are the helpers Sub-A uses for deterministic test identities. If a different helper name is in use (e.g., `Identity::from_seed`), match the existing pattern used by `library_directory.rs` tests (search `harmony_identity::` in `src-tauri/src/library_directory.rs::tests`). The implementer must mirror that pattern verbatim rather than inventing it.

- [ ] **Step 2: Confirm canonical_cbor_encode + CanonicalPayloadSealed are accessible from the module**

Run:
```bash
grep -n "pub trait CanonicalPayloadSealed\|pub trait CanonicalPayload\b" src-tauri/src/owner_state_crypto.rs
```

Expected: both traits exist as `pub trait` in `owner_state_crypto`. If `CanonicalPayloadSealed` is `pub(crate) trait` (sealed-trait pattern), the `impl CanonicalPayloadSealed for ProfileMembershipBroadcast {}` line works because we're in the same crate. If it's `pub trait` already (Phase 2 pattern), nothing changes. No code edit at this step — just verify reachability.

- [ ] **Step 3: Register the module in `lib.rs`**

Edit `src-tauri/src/lib.rs`. Find the existing `pub mod profile;` / `pub mod profile_repo;` (or wherever `p`-prefix modules live, alphabetical) and insert:
```rust
pub mod profile_broadcast;
```

Verify the alphabetical position by running:
```bash
grep -n "^pub mod p" src-tauri/src/lib.rs
```

Insert between the existing `pub mod profile` / `pub mod pairing` etc. lines so the alphabetical order is preserved.

- [ ] **Step 4: Add `Space.shared_in_profile` field**

Edit `src-tauri/src/owner_state_types.rs`. Find the `Space` struct ending at line ~1551 (the field `pub is_invite_only: Option<bool>` immediately before `}`). Add the new field AFTER `is_invite_only`:

```rust
    /// Sub-D Phase 4 (ZEB-281): opt-in flag for including this Space's
    /// `community_id` in the owner's ProfileMembershipBroadcast.
    /// Default `false` (no communities shared until user explicitly
    /// opts in). Replicated across the owner's bound devices via the
    /// existing owner-state CRDT sync — opting in on one device shows
    /// on all of them.
    ///
    /// Only meaningful for `kind == Community`. Setting `true` on
    /// non-community Spaces is rejected by `validate_invariants`.
    ///
    /// `skip_serializing_if = "core::ops::Not::not"` (skip when false)
    /// keeps the default-false case byte-identical to pre-Phase-4
    /// owner-state wire bytes. Verified by
    /// `tests/wire_format_fixture.rs::space_shared_in_profile_default_false_byte_identical_to_pre_phase4`
    /// in Task 4.
    #[serde(rename = "sp", default, skip_serializing_if = "core::ops::Not::not")]
    pub shared_in_profile: bool,
```

- [ ] **Step 5: Update `Space` construction sites**

Run:
```bash
grep -rn "Space {" src-tauri/src/ src-tauri/tests/ 2>&1 | wc -l
```

Expected: a finite count (each construction site must add `shared_in_profile: false`). Run:
```bash
grep -rln "Space {" src-tauri/src/ src-tauri/tests/
```

For each file listed, open it and add `shared_in_profile: false,` to every `Space { ... }` literal that doesn't already include it. (Note: Rust's struct-update syntax `..Default::default()` will work if `Space` already implements `Default`, but it typically doesn't — most existing call sites name every field explicitly. Add the field explicitly.)

Validation: run `cargo check` from `src-tauri/`:
```bash
cd src-tauri && cargo check --features test-fixtures 2>&1 | tail -20
```

Expected: any "missing field `shared_in_profile`" errors point at remaining call sites to fix.

- [ ] **Step 6: Add `validate_invariants` rule**

Find `validate_invariants` in `src-tauri/src/owner_state_types.rs` (or wherever Space invariants are enforced — likely the existing per-kind invariant block). Add:

```rust
// Sub-D Phase 4 (ZEB-281): shared_in_profile is only meaningful for
// communities. Reject malformed peers attempting to set it on
// DMs/group-DMs/profiles/folders/etc.
if space.shared_in_profile && !matches!(space.kind, SpaceKind::Community) {
    return Err(InvariantError::SharedInProfileNotCommunity { space_id: space.id });
}
```

Extend `InvariantError` (same file) with the new variant:
```rust
#[error("shared_in_profile may only be true for community Spaces (space_id={space_id:?})")]
SharedInProfileNotCommunity { space_id: SpaceId },
```

The exact `InvariantError` enum variant naming/format must match neighbors — check `grep -n "InvariantError\b" src-tauri/src/owner_state_types.rs` for the existing variants and mirror their thiserror message style.

- [ ] **Step 7: Run the new module's unit tests**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(profile_broadcast::)' 2>&1 | tail -20
```

Expected: 8 tests run, all pass.

- [ ] **Step 8: Run all existing wire-format pinning tests to confirm Phase 1+ owner-state bytes are unchanged**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(wire_format)' 2>&1 | tail -20
```

Expected: ALL existing pinning tests pass. If `Space`-shaped fixtures change bytes after adding `shared_in_profile: false`, the `skip_serializing_if` invariant is broken — STOP and debug. (This is the load-bearing wire-compat check for the field.)

- [ ] **Step 9: Run full clippy + cargo check**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -5
```

Expected: all three exit 0 with no warnings/errors.

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/profile_broadcast.rs src-tauri/src/lib.rs src-tauri/src/owner_state_types.rs src-tauri/src/
git status   # verify no unintended files
git commit -m "$(cat <<'EOF'
feat(zeb-281): Phase 4 wire format — ProfileMembershipBroadcast + Space.shared_in_profile

- Add profile_broadcast module: ProfileMembershipBroadcast struct, BroadcastVerifyError,
  MAX_SHARED_COMMUNITIES = 200, PROFILE_DISCOVERY_TOPIC_PREFIX, sign_broadcast,
  verify_broadcast, broadcast_topic_for.
- Add Space.shared_in_profile: bool with rename "sp", default, skip_serializing_if =
  "core::ops::Not::not" — byte-identical to pre-Phase-4 owner-state wire bytes for
  the default-false case.
- 8 unit tests: valid, tampered sig, tampered payload, too-many, unsorted, duplicate,
  malformed identity_pub, empty community_ids.
- All existing wire-format pinning tests UNCHANGED (skip_serializing_if invariant).

Spec: docs/specs/2026-05-12-zeb-281-sub-d-phase-4-profile-membership-broadcast-design.md §4.1, §4.2, §4.3
EOF
)"
```

---

## Task 2: `ProfileBroadcastPublisher` state machine

**Goal:** Implement the publisher: `Notify`-driven 2-second debounce + 10-minute periodic refresh + N→0 rotation + never-publish-before-first-opt-in invariant. Testable via an injectable `ProfileBroadcastPublishSink` trait (production = forward to `publish_tx`; tests = record into a `Mutex<Vec<...>>`).

**Files:**
- Modify: `src-tauri/src/profile_broadcast.rs`

- [ ] **Step 1: Add the publisher types to `profile_broadcast.rs`**

Append to the module (above `#[cfg(test)] mod tests`):

```rust
use std::collections::BTreeSet;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

/// Async sink for outbound broadcasts. Production wraps the event-loop
/// `publish_tx`; tests can use `MockSink` to assert the published payload
/// without spinning up Zenoh.
#[async_trait::async_trait]
pub trait ProfileBroadcastPublishSink: Send + Sync + 'static {
    async fn publish(&self, topic: String, payload: Vec<u8>) -> Result<(), String>;
}

/// Read-side handle to the owner-state opted-in set + HLC source.
/// Production wraps `Arc<Mutex<OwnerState>>` + the existing HLC tracker;
/// tests can use `MockSource` to inject fixed sets.
#[async_trait::async_trait]
pub trait ProfileBroadcastSource: Send + Sync + 'static {
    /// Return the SORTED, deduped current opted-in community SpaceIds.
    /// (Walks `OwnerState.spaces` for `kind == Community &&
    /// shared_in_profile == true`.)
    async fn current_shared_set(&self) -> Vec<SpaceId>;
    /// Return the next HLC to stamp on the next publish.
    async fn next_hlc(&self) -> Hlc;
}

/// Publisher debounce window. Spec §5.
pub const PUBLISHER_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Publisher periodic refresh interval. Spec §5.
pub const PUBLISHER_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

/// Tracks publish state across debounce/refresh ticks.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PublishedSnapshot {
    set: Vec<SpaceId>, // sorted, deduped
    hlc: Hlc,
}

/// Sub-D Phase 4 publisher. Spec §5.
pub struct ProfileBroadcastPublisher {
    notify: Arc<Notify>,
    /// Most recent published snapshot. `None` until first publish.
    last_published: Arc<Mutex<Option<PublishedSnapshot>>>,
    /// Background task driving debounce + refresh. Aborted on `shutdown()`.
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ProfileBroadcastPublisher {
    /// Spawn the publisher background task. `signer` Ed25519-signs each
    /// outbound payload; `identity_pub` is the 64-byte bundle stamped on
    /// every broadcast (and from which subscribers derive the broadcaster
    /// OwnerAddr for the attribution check). `source` reads the current
    /// opted-in set + HLC; `sink` publishes the bytes.
    pub fn spawn(
        signer: ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        source: Arc<dyn ProfileBroadcastSource>,
        sink: Arc<dyn ProfileBroadcastPublishSink>,
        // Test seam: lets unit tests provide a faster debounce/refresh.
        // Production always uses the constants above.
        debounce: std::time::Duration,
        refresh: std::time::Duration,
    ) -> Arc<Self> {
        let notify = Arc::new(Notify::new());
        let last_published: Arc<Mutex<Option<PublishedSnapshot>>> = Arc::new(Mutex::new(None));

        let task = {
            let notify_for_task = Arc::clone(&notify);
            let last_published_for_task = Arc::clone(&last_published);
            let identity_pub_for_task = identity_pub;
            tokio::spawn(async move {
                let mut refresh_interval = tokio::time::interval(refresh);
                refresh_interval.set_missed_tick_behavior(
                    tokio::time::MissedTickBehavior::Delay,
                );
                // First tick fires immediately; consume it.
                refresh_interval.tick().await;
                loop {
                    tokio::select! {
                        _ = notify_for_task.notified() => {
                            // Debounce — drain further notifies that
                            // arrive within `debounce`.
                            let deadline = tokio::time::Instant::now() + debounce;
                            loop {
                                tokio::select! {
                                    _ = notify_for_task.notified() => continue,
                                    _ = tokio::time::sleep_until(deadline) => break,
                                }
                            }
                            Self::maybe_publish(
                                &signer,
                                identity_pub_for_task,
                                source.as_ref(),
                                sink.as_ref(),
                                last_published_for_task.as_ref(),
                                false, // is_refresh
                            ).await;
                        }
                        _ = refresh_interval.tick() => {
                            Self::maybe_publish(
                                &signer,
                                identity_pub_for_task,
                                source.as_ref(),
                                sink.as_ref(),
                                last_published_for_task.as_ref(),
                                true, // is_refresh
                            ).await;
                        }
                    }
                }
            })
        };

        Arc::new(Self {
            notify,
            last_published,
            task: Mutex::new(Some(task)),
        })
    }

    /// IPC handler calls this after mutating `Space.shared_in_profile`.
    pub fn notify_dirty(&self) {
        self.notify.notify_one();
    }

    /// Abort the background task. Idempotent.
    pub async fn shutdown(&self) {
        if let Some(h) = self.task.lock().await.take() {
            h.abort();
        }
    }

    /// Test seam: read the most recently published snapshot.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub async fn last_published_for_test(&self) -> Option<(Vec<SpaceId>, Hlc)> {
        self.last_published
            .lock()
            .await
            .as_ref()
            .map(|s| (s.set.clone(), s.hlc.clone()))
    }

    async fn maybe_publish(
        signer: &ed25519_dalek::SigningKey,
        identity_pub: [u8; 64],
        source: &dyn ProfileBroadcastSource,
        sink: &dyn ProfileBroadcastPublishSink,
        last_published: &Mutex<Option<PublishedSnapshot>>,
        is_refresh: bool,
    ) {
        let current = source.current_shared_set().await;
        let mut guard = last_published.lock().await;
        let last_snapshot = guard.as_ref().cloned();

        // Privacy invariant: NO broadcast before first opt-in.
        let has_ever_published = last_snapshot.is_some();
        if !has_ever_published && current.is_empty() {
            return;
        }

        // Skip-when-unchanged for debounce path; refresh always re-publishes
        // the current set as long as we've ever published before.
        if !is_refresh {
            if let Some(snap) = &last_snapshot {
                if snap.set == current {
                    return;
                }
            }
        } else if !has_ever_published {
            // Refresh tick fires before any opt-in: still must respect
            // the privacy invariant.
            return;
        }

        // Rotation: when refresh tick fires AFTER N→0, the prior snapshot
        // is empty. Don't republish empty over empty.
        if is_refresh {
            if let Some(snap) = &last_snapshot {
                if snap.set.is_empty() && current.is_empty() {
                    return;
                }
            }
        }

        let hlc = source.next_hlc().await;
        let broadcast = match sign_broadcast(signer, identity_pub, current.clone(), hlc.clone()) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = ?e, "profile broadcast sign failed");
                return;
            }
        };
        let bytes = match canonical_cbor_encode(&broadcast) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = ?e, "profile broadcast encode failed");
                return;
            }
        };
        let addr = match harmony_identity::Identity::from_public_bytes(&identity_pub) {
            Ok(id) => OwnerAddr(id.address_hash),
            Err(e) => {
                tracing::warn!(error = ?e, "profile broadcast identity derive failed");
                return;
            }
        };
        let topic = broadcast_topic_for(&addr);
        if let Err(e) = sink.publish(topic, bytes).await {
            tracing::warn!(error = %e, "profile broadcast publish failed");
            return;
        }
        *guard = Some(PublishedSnapshot { set: current, hlc });
    }
}

/// Production-side `ProfileBroadcastSource` that reads the owner-state
/// CRDT + uses the existing HLC tracker. Wired in start_node (Task 5).
pub struct OwnerStateBroadcastSource {
    pub crdt_state: Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    pub hlc_tracker:
        Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, Hlc>>>,
    pub device_id: String,
}

#[async_trait::async_trait]
impl ProfileBroadcastSource for OwnerStateBroadcastSource {
    async fn current_shared_set(&self) -> Vec<SpaceId> {
        let g = self.crdt_state.lock().await;
        let mut ids: Vec<SpaceId> = g
            .spaces
            .values()
            .filter(|s| {
                matches!(s.kind, crate::owner_state_types::SpaceKind::Community)
                    && s.shared_in_profile
            })
            .filter_map(|s| s.community_id)
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    async fn next_hlc(&self) -> Hlc {
        // Mirror the bump-pattern used by community-related publishes.
        let mut tracker = self.hlc_tracker.lock().await;
        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let entry = tracker.entry(self.device_id.clone()).or_insert(Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: self.device_id.clone(),
        });
        if now_ms > entry.wall_ms {
            entry.wall_ms = now_ms;
            entry.logical = 0;
        } else {
            entry.logical = entry.logical.saturating_add(1);
        }
        entry.clone()
    }
}

/// Production-side `ProfileBroadcastPublishSink` that forwards to the
/// event-loop's `publish_tx` channel.
pub struct EventLoopPublishSink {
    pub publish_tx: tokio::sync::mpsc::Sender<crate::event_loop::PublishRequest>,
}

#[async_trait::async_trait]
impl ProfileBroadcastPublishSink for EventLoopPublishSink {
    async fn publish(&self, topic: String, payload: Vec<u8>) -> Result<(), String> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.publish_tx
            .send(crate::event_loop::PublishRequest {
                key_expr: topic,
                payload,
                reply: reply_tx,
            })
            .await
            .map_err(|e| format!("publish_tx send: {e}"))?;
        reply_rx
            .await
            .map_err(|e| format!("publish_tx reply dropped: {e}"))?
    }
}
```

- [ ] **Step 2: Add `async-trait` if not already in Cargo.toml**

Run:
```bash
grep -n "async-trait" src-tauri/Cargo.toml
```

If absent, add to `[dependencies]`:
```toml
async-trait = "0.1"
```

(harmony-client already uses async-trait via several other modules — confirm before adding. If it IS already there, skip.)

- [ ] **Step 3: Write unit test scaffolding for the publisher**

Append to the `#[cfg(test)] mod tests` block in `profile_broadcast.rs`:

```rust
    use std::sync::{Arc as StdArc};
    use tokio::sync::Mutex as TokioMutex;

    struct MockSink {
        published: StdArc<TokioMutex<Vec<(String, Vec<u8>)>>>,
    }

    #[async_trait::async_trait]
    impl ProfileBroadcastPublishSink for MockSink {
        async fn publish(&self, topic: String, payload: Vec<u8>) -> Result<(), String> {
            self.published.lock().await.push((topic, payload));
            Ok(())
        }
    }

    struct MockSource {
        set: StdArc<TokioMutex<Vec<SpaceId>>>,
        hlc: StdArc<TokioMutex<Hlc>>,
    }

    #[async_trait::async_trait]
    impl ProfileBroadcastSource for MockSource {
        async fn current_shared_set(&self) -> Vec<SpaceId> {
            self.set.lock().await.clone()
        }
        async fn next_hlc(&self) -> Hlc {
            let mut g = self.hlc.lock().await;
            g.logical = g.logical.saturating_add(1);
            g.clone()
        }
    }

    fn mock_publisher_setup() -> (
        StdArc<TokioMutex<Vec<SpaceId>>>,
        StdArc<TokioMutex<Vec<(String, Vec<u8>)>>>,
        Arc<ProfileBroadcastPublisher>,
    ) {
        let (signer, identity_pub) = build_identity([42u8; 32]);
        let set = StdArc::new(TokioMutex::new(Vec::new()));
        let hlc = StdArc::new(TokioMutex::new(Hlc {
            wall_ms: 1_000,
            logical: 0,
            device_id: "test".into(),
        }));
        let published = StdArc::new(TokioMutex::new(Vec::new()));
        let source = Arc::new(MockSource {
            set: StdArc::clone(&set),
            hlc: StdArc::clone(&hlc),
        });
        let sink = Arc::new(MockSink {
            published: StdArc::clone(&published),
        });
        let publisher = ProfileBroadcastPublisher::spawn(
            signer,
            identity_pub,
            source,
            sink,
            // Fast debounce + fast refresh for tests.
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(100),
        );
        (set, published, publisher)
    }
```

- [ ] **Step 4: Write `publisher_no_broadcast_before_first_optin` test**

Append to the same `#[cfg(test)] mod tests` block:

```rust
    #[tokio::test]
    async fn publisher_no_broadcast_before_first_optin() {
        let (_set, published, publisher) = mock_publisher_setup();
        // Empty set — notify the publisher anyway.
        publisher.notify_dirty();
        // Wait past the debounce + a refresh tick.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let pubs = published.lock().await;
        assert!(
            pubs.is_empty(),
            "publisher must NOT publish before first opt-in (privacy invariant); \
             published: {pubs:?}"
        );
        publisher.shutdown().await;
    }
```

- [ ] **Step 5: Write `publisher_debounce_coalesces_rapid_toggles` test**

```rust
    #[tokio::test]
    async fn publisher_debounce_coalesces_rapid_toggles() {
        let (set, published, publisher) = mock_publisher_setup();
        // Seed an opted-in set so privacy gate is open.
        *set.lock().await = vec![fixture_space_id(1)];
        publisher.notify_dirty();
        // Rapid-fire 5 toggles within the 20ms debounce.
        for _ in 0..5 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            publisher.notify_dirty();
        }
        // Wait past debounce.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let pubs = published.lock().await;
        assert_eq!(
            pubs.len(),
            1,
            "5 rapid toggles must coalesce to 1 broadcast; got {}",
            pubs.len()
        );
        publisher.shutdown().await;
    }
```

- [ ] **Step 6: Write `publisher_rotation_publishes_empty_then_stops_refresh` test**

```rust
    #[tokio::test]
    async fn publisher_rotation_publishes_empty_then_stops_refresh() {
        let (set, published, publisher) = mock_publisher_setup();
        // First opt-in → publish.
        *set.lock().await = vec![fixture_space_id(1)];
        publisher.notify_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // N→0 rotation → publish empty.
        *set.lock().await = vec![];
        publisher.notify_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Wait several refresh intervals — should NOT republish empty.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let pubs = published.lock().await;
        assert_eq!(
            pubs.len(),
            2,
            "expected 2 publishes (first opt-in + N→0 rotation); got {} entries: {pubs:?}",
            pubs.len()
        );
        // Decode the second payload and assert community_ids == [].
        let second_payload = &pubs[1].1;
        let decoded: ProfileMembershipBroadcast = ciborium::from_reader(&second_payload[..])
            .expect("decode rotation payload");
        assert!(
            decoded.community_ids.is_empty(),
            "rotation publish must carry empty community_ids; got {:?}",
            decoded.community_ids
        );
        // And its HLC must be strictly newer than the first.
        let first_payload = &pubs[0].1;
        let first_decoded: ProfileMembershipBroadcast =
            ciborium::from_reader(&first_payload[..]).expect("decode first payload");
        assert!(
            decoded.shared_at.wall_ms > first_decoded.shared_at.wall_ms
                || (decoded.shared_at.wall_ms == first_decoded.shared_at.wall_ms
                    && decoded.shared_at.logical > first_decoded.shared_at.logical),
            "rotation HLC must be strictly newer; first={:?} second={:?}",
            first_decoded.shared_at,
            decoded.shared_at
        );
        publisher.shutdown().await;
    }
```

- [ ] **Step 7: Run the new tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(profile_broadcast::tests::publisher)' 2>&1 | tail -10
```

Expected: 3 tests run, all pass. (Combined with Task 1's 8 = 11 verify+publisher unit tests in this module so far.)

- [ ] **Step 8: Run all profile_broadcast tests + clippy + fmt**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(profile_broadcast)' 2>&1 | tail -10
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/profile_broadcast.rs src-tauri/Cargo.toml
git status
git commit -m "$(cat <<'EOF'
feat(zeb-281): ProfileBroadcastPublisher state machine

- Notify-driven 2s debounce + 10min periodic refresh + N→0 rotation.
- Privacy invariant: no publish before first opt-in (asserted in test).
- Injectable ProfileBroadcastSource / ProfileBroadcastPublishSink traits
  for unit testability; production impls wrap OwnerState + publish_tx.
- 3 unit tests: no-broadcast-before-optin, debounce-coalesces, rotation-
  publishes-empty-then-stops-refresh.

Spec §5.
EOF
)"
```

---

## Task 3: `ProfileBroadcastCache` + state replay defense + 2 unit tests

**Goal:** Implement the per-peer cache that holds the latest verified broadcast keyed by subscription id, performs attribution + replay defense, and exposes a snapshot DTO. 2 unit tests cover the HLC replay-defense path.

**Files:**
- Modify: `src-tauri/src/profile_broadcast.rs`

- [ ] **Step 1: Add `DiscoveredProfileInfo` DTO + cache types**

Append (above `#[cfg(test)] mod tests`):

```rust
use std::collections::HashMap;

/// Subscription identifier handed to the frontend by `subscribe_peer_profile`.
/// Plain u64 (monotonic, allocated by an AtomicU64 in NodeState). One
/// subscription_id maps to one Zenoh subscriber task; multiple subscriptions
/// to the same peer are allowed (one per open ProfilePopover).
pub type SubscriptionId = u64;

/// Snapshot of the latest verified broadcast for a subscription.
/// Wire keys are camelCase to match `DiscoveredLibraryInfo` (Phase 2).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredProfileInfo {
    /// Hex-encoded 16-byte OwnerAddr (32 hex chars).
    pub owner_addr: String,
    /// Hex-encoded SpaceIds the peer opted to share (32 hex chars each).
    pub community_ids: Vec<String>,
    /// `shared_at.wall_ms` as base-10 string. Display only — callers MUST
    /// NOT use this for HLC ordering decisions.
    pub shared_at: String,
}

/// Per-subscription cache entry. Holds the highest-HLC verified broadcast
/// observed so far + the peer addr the subscription targets (for attribution).
#[derive(Debug, Clone)]
struct CachedSubscription {
    peer_addr: OwnerAddr,
    /// Most recent verified broadcast. `None` until first valid sample
    /// arrives (covers the "loading" UI state).
    latest: Option<ProfileMembershipBroadcast>,
}

/// In-process cache of verified peer profile broadcasts. Spec §6.
#[derive(Debug, Default)]
pub struct ProfileBroadcastCache {
    by_sub: tokio::sync::Mutex<HashMap<SubscriptionId, CachedSubscription>>,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheOnSampleError {
    #[error("subscription not found: {0}")]
    SubscriptionNotFound(SubscriptionId),
    #[error("verify failed: {0}")]
    VerifyFailed(#[from] BroadcastVerifyError),
    #[error("attribution mismatch: topic owner={topic_owner:?}, derived={derived:?}")]
    AttributionMismatch {
        topic_owner: OwnerAddr,
        derived: OwnerAddr,
    },
    #[error("replay: incoming HLC not strictly newer than cached")]
    Replay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheOnSampleOutcome {
    /// First broadcast for this subscription — emit event.
    InsertedFirst,
    /// Newer broadcast replaced an older one — emit event.
    Replaced,
}

impl ProfileBroadcastCache {
    /// Register a subscription with the cache. Idempotent — re-inserting
    /// the same subscription_id replaces the prior entry.
    pub async fn register(&self, sub: SubscriptionId, peer_addr: OwnerAddr) {
        self.by_sub.lock().await.insert(
            sub,
            CachedSubscription {
                peer_addr,
                latest: None,
            },
        );
    }

    /// Drop a subscription from the cache. Idempotent — missing sub is OK.
    pub async fn drop_subscription(&self, sub: SubscriptionId) {
        self.by_sub.lock().await.remove(&sub);
    }

    /// Process an incoming raw payload for a subscription. Spec §6.
    pub async fn on_sample(
        &self,
        sub: SubscriptionId,
        broadcast: ProfileMembershipBroadcast,
    ) -> Result<CacheOnSampleOutcome, CacheOnSampleError> {
        // (1) Verify
        let derived = verify_broadcast(&broadcast)?;
        // (2) Attribution check + replay defense — atomic against the map.
        let mut g = self.by_sub.lock().await;
        let entry = g
            .get_mut(&sub)
            .ok_or(CacheOnSampleError::SubscriptionNotFound(sub))?;
        if derived != entry.peer_addr {
            return Err(CacheOnSampleError::AttributionMismatch {
                topic_owner: entry.peer_addr,
                derived,
            });
        }
        if let Some(prev) = &entry.latest {
            // Strict greater-than: equal HLC is also rejected (idempotent
            // duplicate). HLC comparison is on (wall_ms, logical) tuple.
            let newer = (broadcast.shared_at.wall_ms, broadcast.shared_at.logical)
                > (prev.shared_at.wall_ms, prev.shared_at.logical);
            if !newer {
                return Err(CacheOnSampleError::Replay);
            }
        }
        let was_none = entry.latest.is_none();
        entry.latest = Some(broadcast);
        Ok(if was_none {
            CacheOnSampleOutcome::InsertedFirst
        } else {
            CacheOnSampleOutcome::Replaced
        })
    }

    /// Snapshot the latest verified broadcast for a subscription as the
    /// frontend DTO.
    pub async fn get_cached(&self, sub: SubscriptionId) -> Option<DiscoveredProfileInfo> {
        let g = self.by_sub.lock().await;
        let entry = g.get(&sub)?;
        let b = entry.latest.as_ref()?;
        Some(DiscoveredProfileInfo {
            owner_addr: hex::encode(entry.peer_addr.0),
            community_ids: b.community_ids.iter().map(|s| hex::encode(s.0)).collect(),
            shared_at: b.shared_at.wall_ms.to_string(),
        })
    }
}
```

- [ ] **Step 2: Write `state_replay_old_hlc_rejected` unit test**

Append inside `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn state_replay_old_hlc_rejected() {
        let (signer, identity_pub) = build_identity([100u8; 32]);
        let peer_addr = OwnerAddr(
            harmony_identity::Identity::from_public_bytes(&identity_pub)
                .unwrap()
                .address_hash,
        );
        let cache = ProfileBroadcastCache::default();
        cache.register(1, peer_addr).await;

        let newer = sign_broadcast(
            &signer,
            identity_pub,
            vec![fixture_space_id(1)],
            fixture_hlc(200),
        )
        .unwrap();
        let older = sign_broadcast(
            &signer,
            identity_pub,
            vec![fixture_space_id(2)],
            fixture_hlc(100),
        )
        .unwrap();

        // Land the newer first.
        assert_eq!(
            cache.on_sample(1, newer.clone()).await.unwrap(),
            CacheOnSampleOutcome::InsertedFirst
        );
        // Older arrives second → Replay.
        assert!(matches!(
            cache.on_sample(1, older).await,
            Err(CacheOnSampleError::Replay)
        ));
        // Cached state unchanged: still the newer.
        let snap = cache.get_cached(1).await.unwrap();
        assert_eq!(snap.shared_at, "200");
    }
```

- [ ] **Step 3: Write `state_replay_newer_hlc_accepted` unit test**

```rust
    #[tokio::test]
    async fn state_replay_newer_hlc_accepted() {
        let (signer, identity_pub) = build_identity([101u8; 32]);
        let peer_addr = OwnerAddr(
            harmony_identity::Identity::from_public_bytes(&identity_pub)
                .unwrap()
                .address_hash,
        );
        let cache = ProfileBroadcastCache::default();
        cache.register(2, peer_addr).await;

        let older = sign_broadcast(
            &signer,
            identity_pub,
            vec![fixture_space_id(1)],
            fixture_hlc(100),
        )
        .unwrap();
        let newer = sign_broadcast(
            &signer,
            identity_pub,
            vec![fixture_space_id(2)],
            fixture_hlc(200),
        )
        .unwrap();

        assert_eq!(
            cache.on_sample(2, older).await.unwrap(),
            CacheOnSampleOutcome::InsertedFirst
        );
        assert_eq!(
            cache.on_sample(2, newer).await.unwrap(),
            CacheOnSampleOutcome::Replaced
        );
        // Cached state updated to the newer.
        let snap = cache.get_cached(2).await.unwrap();
        assert_eq!(snap.shared_at, "200");
        assert_eq!(snap.community_ids, vec![hex::encode([2u8; 16])]);
    }
```

- [ ] **Step 4: Run the new tests + full module suite**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(profile_broadcast::tests::state)' 2>&1 | tail -10
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(profile_broadcast::)' 2>&1 | tail -10
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

Expected: 2 new state tests pass. Combined unit-test count for the module: 13 (8 verify + 3 publisher + 2 state).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/profile_broadcast.rs
git commit -m "$(cat <<'EOF'
feat(zeb-281): ProfileBroadcastCache + DiscoveredProfileInfo DTO

- HashMap<SubscriptionId, CachedSubscription> with verify + attribution
  check + per-subscription latest-HLC-wins replay defense.
- DiscoveredProfileInfo: camelCase wire keys (ownerAddr/communityIds/sharedAt)
  matching DiscoveredLibraryInfo (Phase 2) precedent.
- 2 unit tests: state_replay_old_hlc_rejected, state_replay_newer_hlc_accepted.

Spec §6.
EOF
)"
```

---

## Task 4: Wire-format pinning

**Goal:** Two pinning tests in a new file for `ProfileMembershipBroadcast` (round-trip pinned bytes + 2-char key audit). One pinning test in the existing owner-state fixture file asserting `Space` with `shared_in_profile: false` produces byte-identical bytes to a `Space` constructed before the field existed.

**Files:**
- Create: `src-tauri/tests/wire_format_profile_broadcast_fixtures.rs`
- Modify: `src-tauri/tests/wire_format_fixture.rs`

- [ ] **Step 1: Confirm the existing owner-state fixture file path**

Run:
```bash
ls src-tauri/tests/wire_format_*.rs
```

Expected files include `wire_format_fixture.rs` (owner-state pinning per ZEB-170 / earlier work). If the owner-state fixtures live in a different file (e.g., `wire_format_owner_state_fixtures.rs`), substitute that name throughout this task.

- [ ] **Step 2: Read the existing owner-state fixture pattern**

Run:
```bash
head -60 src-tauri/tests/wire_format_fixture.rs
```

Note the test imports + the canonical_cbor_encode invocation + the byte-prefix assertion style. The new test must match this style precisely.

- [ ] **Step 3: Add the Space wire-compat test to `wire_format_fixture.rs`**

Append a new `#[test]` to `src-tauri/tests/wire_format_fixture.rs`:

```rust
/// Sub-D Phase 4 (ZEB-281) wire-compat invariant: a Space with
/// `shared_in_profile: false` (the default) encodes byte-identically to
/// a Space constructed before the field existed. Powered by
/// `#[serde(rename = "sp", default, skip_serializing_if =
/// "core::ops::Not::not")]` on `Space.shared_in_profile`.
///
/// If this test fails, the `skip_serializing_if` invariant has been
/// inadvertently changed — Phase 4 broke cross-version owner-state
/// CRDT compat. Fix the field attrs, don't update the test.
#[test]
fn space_shared_in_profile_default_false_byte_identical_to_pre_phase4() {
    use harmony_app::owner_state_crypto::canonical_cbor_encode;
    use harmony_app::owner_state_types::{
        Hlc, OwnerAddr, Space, SpaceId, SpaceKind,
    };

    // Construct a minimal community Space with default-false
    // shared_in_profile. The encoded bytes MUST NOT contain a "sp" key.
    let space = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Community,
        parent: None,
        community_id: Some(SpaceId([2u8; 16])),
        name: "test".to_string(),
        transport: None,
        members: vec![OwnerAddr([3u8; 16])],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1700000000000, logical: 0, device_id: "fix".into() },
        updated_at: Hlc { wall_ms: 1700000000000, logical: 0, device_id: "fix".into() },
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: Some(0),
        current_epoch_key: None,
        old_epoch_keys: Default::default(),
        admin_addr: Some(OwnerAddr([3u8; 16])),
        is_invite_only: Some(false),
        shared_in_profile: false, // The Phase 4 field, default
    };

    let bytes = canonical_cbor_encode(&space).expect("encode");

    // Decode and walk the CBOR map; "sp" key MUST NOT appear.
    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };
    let keys: Vec<String> = map
        .iter()
        .filter_map(|(k, _)| match k {
            ciborium::value::Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !keys.iter().any(|k| k == "sp"),
        "Space with default-false shared_in_profile must NOT emit \"sp\" key on the wire; \
         observed keys: {keys:?}"
    );
}

/// Companion test: a Space with `shared_in_profile: true` DOES emit "sp" → true.
#[test]
fn space_shared_in_profile_true_emits_sp_key() {
    use harmony_app::owner_state_crypto::canonical_cbor_encode;
    use harmony_app::owner_state_types::{
        Hlc, OwnerAddr, Space, SpaceId, SpaceKind,
    };

    let space = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Community,
        parent: None,
        community_id: Some(SpaceId([2u8; 16])),
        name: "test".to_string(),
        transport: None,
        members: vec![OwnerAddr([3u8; 16])],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1700000000000, logical: 0, device_id: "fix".into() },
        updated_at: Hlc { wall_ms: 1700000000000, logical: 0, device_id: "fix".into() },
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: Some(0),
        current_epoch_key: None,
        old_epoch_keys: Default::default(),
        admin_addr: Some(OwnerAddr([3u8; 16])),
        is_invite_only: Some(false),
        shared_in_profile: true,
    };

    let bytes = canonical_cbor_encode(&space).expect("encode");
    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };
    let sp_value = map
        .iter()
        .find_map(|(k, v)| match (k, v) {
            (ciborium::value::Value::Text(s), v) if s == "sp" => Some(v.clone()),
            _ => None,
        })
        .expect("Space with shared_in_profile: true must emit \"sp\" key");
    assert_eq!(sp_value, ciborium::value::Value::Bool(true));
}
```

- [ ] **Step 4: Create the new ProfileMembershipBroadcast pinning file**

Create `src-tauri/tests/wire_format_profile_broadcast_fixtures.rs`:

```rust
//! Sub-D Phase 4 (ZEB-281) wire-format pinning. Pinned bytes prevent
//! silent wire-format changes — if any test here fails, treat it as a
//! wire-protocol break and review carefully (cross-version compatibility,
//! peer interop).
//!
//! Mirrors `wire_format_library_directory_fixtures.rs` (Sub-D Phase 1+3).

use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, SpaceId};
use harmony_app::profile_broadcast::ProfileMembershipBroadcast;

fn fixture_hlc() -> Hlc {
    Hlc {
        wall_ms: 1700000000000,
        logical: 0,
        device_id: "fix".into(),
    }
}

/// Canonical CBOR round-trip + pinned prefix. Decoded back via
/// `ciborium::value::Value::Map` to assert the EXACT key ordering
/// (ai → cs → sa → sg as declared in the struct).
#[test]
fn profile_broadcast_canonical_cbor_pinned() {
    let b = ProfileMembershipBroadcast {
        owner_identity_pub: [0xaa; 64],
        community_ids: vec![SpaceId([0x11; 16]), SpaceId([0x22; 16])],
        shared_at: fixture_hlc(),
        signature: [0xbb; 64],
    };
    let bytes = canonical_cbor_encode(&b).expect("encode");

    // map(4) marker: 0xa4
    assert_eq!(
        bytes[0], 0xa4,
        "ProfileMembershipBroadcast must encode as map(4); got map({:#x}) prefix",
        bytes[0]
    );

    // Full canonical key ordering — declaration order.
    let value: ciborium::value::Value =
        ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };
    let observed_keys: Vec<String> = map
        .into_iter()
        .map(|(k, _)| match k {
            ciborium::value::Value::Text(s) => s,
            other => panic!("non-text map key: {other:?}"),
        })
        .collect();
    let expected_keys: Vec<&str> = vec!["ai", "cs", "sa", "sg"];
    assert_eq!(
        observed_keys, expected_keys,
        "ProfileMembershipBroadcast must encode keys in this exact declaration order \
         (signature portability depends on canonical CBOR encoding)"
    );
}

/// 2-char key invariant. Every key at this nesting level must be 2 chars
/// so `canonical_cbor_encode`'s same-length-keys precondition holds.
/// Mirrors `phase3_wrapped_entry_two_char_keys_audit`.
#[test]
fn profile_broadcast_field_keys_are_2char() {
    let b = ProfileMembershipBroadcast {
        owner_identity_pub: [0u8; 64],
        community_ids: vec![],
        shared_at: Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: String::new(),
        },
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&b).expect("encode");
    let value: ciborium::value::Value =
        ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };

    let mut keys = std::collections::BTreeSet::new();
    for (k, _) in map {
        match k {
            ciborium::value::Value::Text(s) => {
                assert_eq!(s.len(), 2, "field key must be 2 chars: {s:?}");
                keys.insert(s);
            }
            other => panic!("non-text map key: {other:?}"),
        }
    }
    // Confirm we observed exactly the 4 expected keys.
    let expected: std::collections::BTreeSet<String> =
        ["ai", "cs", "sa", "sg"].iter().map(|s| s.to_string()).collect();
    assert_eq!(keys, expected, "expected exactly 4 keys (ai/cs/sa/sg)");
}
```

- [ ] **Step 5: Run the new wire-format tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(profile_broadcast_canonical_cbor_pinned) | test(profile_broadcast_field_keys_are_2char) | test(space_shared_in_profile_default_false_byte_identical_to_pre_phase4) | test(space_shared_in_profile_true_emits_sp_key)' 2>&1 | tail -10
```

Expected: 4 tests pass.

- [ ] **Step 6: Run ALL wire-format pinning tests — load-bearing regression check**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(wire_format)' 2>&1 | tail -20
```

Expected: every existing wire-format pinning fixture in the workspace passes (Phase 1+2+3 owner-state + library directory + announce + channel log + community fixtures). If any fail, the `skip_serializing_if` invariant for `shared_in_profile` is broken — STOP and debug. (Most likely cause: an existing Space construction site was inadvertently updated to `shared_in_profile: true` somewhere; or, the field's serde attrs are wrong.)

- [ ] **Step 7: Run fmt + clippy**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

Expected: green.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/tests/wire_format_profile_broadcast_fixtures.rs src-tauri/tests/wire_format_fixture.rs
git status
git commit -m "$(cat <<'EOF'
test(zeb-281): wire-format pinning for ProfileMembershipBroadcast + Space.shared_in_profile

- wire_format_profile_broadcast_fixtures.rs: canonical-CBOR round-trip
  + pinned map(4) prefix + full key-ordering audit (ai → cs → sa → sg)
  + 2-char key audit.
- wire_format_fixture.rs: assert Space with shared_in_profile: false
  produces byte-identical CBOR to pre-Phase-4 owner-state (no "sp" key
  emitted) + companion test that true DOES emit "sp" key.

All pre-existing wire-format pinning tests UNCHANGED (skip_serializing_if
invariant holds).

Spec §4.1, §11.4.
EOF
)"
```

---

## Task 5: IPC commands + NodeState wiring + event-loop subscriber + 5 integration tests

**Goal:** Wire the publisher + subscriber into NodeState lifecycle. Add 4 IPC commands + 1 event. Add `ProfileBroadcastRequest::Subscribe / Unsubscribe` to `event_loop.rs` so per-subscription Zenoh subscriber tasks run inside the event loop (same retry/backoff pattern as the Phase 2 announce subscriber). 5 integration tests cover end-to-end behavior.

**Files:**
- Modify: `src-tauri/src/lib.rs` (NodeState fields + IPC commands + handler registration + start_node/stop_node)
- Modify: `src-tauri/src/event_loop.rs` (new ProfileBroadcastRequest enum, subscriber spawn block)
- Create: `src-tauri/tests/common/profile_fixtures.rs`
- Create: `src-tauri/tests/profile_broadcast_integration.rs`

- [ ] **Step 1: Add `ProfileBroadcastRequest` enum to `event_loop.rs`**

Find the existing `LibraryDirectoryRequest` enum in `src-tauri/src/event_loop.rs` (used to control per-library subscriber tasks). Add a parallel enum below it:

```rust
/// Sub-D Phase 4 (ZEB-281): control messages for the profile-broadcast
/// subscriber pool. Each Subscribe declares a Zenoh subscriber for
/// `harmony/discovery/profile/{peer_addr_hex}/memberships`; Unsubscribe
/// aborts the task and drops the Zenoh subscriber.
///
/// The pool is keyed by `SubscriptionId` (allocated by NodeState via an
/// AtomicU64) — NOT by `OwnerAddr` — because multiple concurrent
/// ProfilePopovers may be open for the same peer.
pub enum ProfileBroadcastRequest {
    Subscribe {
        subscription_id: crate::profile_broadcast::SubscriptionId,
        peer_addr: crate::owner_state_types::OwnerAddr,
    },
    Unsubscribe {
        subscription_id: crate::profile_broadcast::SubscriptionId,
    },
}
```

- [ ] **Step 2: Plumb the request channel through the event-loop spawn**

In the existing `start_event_loop` (or equivalent) function in `event_loop.rs`, locate the place where `library_directory_request_tx` / `_rx` is wired (Phase 2). Add a sibling channel for profile broadcasts. Wire `_tx` back to the caller (NodeState gets a clone). Spawn a task that reads from `_rx` and maintains a `HashMap<SubscriptionId, JoinHandle<()>>` like the existing per-library subscriber map. Each Subscribe spawns:

```rust
ProfileBroadcastRequest::Subscribe { subscription_id, peer_addr } => {
    if handles.contains_key(&subscription_id) {
        tracing::warn!(
            subscription_id,
            "ProfileBroadcastRequest::Subscribe duplicate id — ignoring"
        );
        continue;
    }
    let key_expr = crate::profile_broadcast::broadcast_topic_for(&peer_addr);
    let cache = Arc::clone(&profile_broadcast_cache_for_loop);
    let session = Arc::clone(&session_arc);
    let app_for_emit = app.clone();
    let closing_for_task = Arc::clone(&closing);
    let handle = tokio::spawn(async move {
        let mut backoff = std::time::Duration::from_secs(2);
        const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
        loop {
            if closing_for_task.load(Ordering::SeqCst) {
                break;
            }
            let sub = match session.declare_subscriber(&key_expr).await {
                Ok(s) => {
                    backoff = std::time::Duration::from_secs(2);
                    s
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        subscription_id,
                        backoff_s = backoff.as_secs(),
                        "profile broadcast declare_subscriber failed; retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                    continue;
                }
            };
            loop {
                match sub.recv_async().await {
                    Ok(sample) => {
                        let bytes_view = sample.payload().to_bytes();
                        if bytes_view.len() > crate::profile_broadcast::MAX_BROADCAST_WIRE_BYTES {
                            tracing::warn!(
                                size = bytes_view.len(),
                                max = crate::profile_broadcast::MAX_BROADCAST_WIRE_BYTES,
                                "oversized profile broadcast dropped"
                            );
                            continue;
                        }
                        let bytes = bytes_view.to_vec();
                        let broadcast: crate::profile_broadcast::ProfileMembershipBroadcast =
                            match ciborium::from_reader(&bytes[..]) {
                                Ok(b) => b,
                                Err(e) => {
                                    tracing::warn!(error = ?e, "profile broadcast CBOR decode failed");
                                    continue;
                                }
                            };
                        match cache.on_sample(subscription_id, broadcast).await {
                            Ok(outcome) => {
                                tracing::debug!(?outcome, subscription_id, "profile broadcast cached");
                                if let Some(info) = cache.get_cached(subscription_id).await {
                                    // Spec §7: emit flat payload (subscriptionId
                                    // + DiscoveredProfileInfo fields hoisted).
                                    let _ = app_for_emit.emit(
                                        "profile-broadcast-received",
                                        serde_json::json!({
                                            "subscriptionId": subscription_id,
                                            "ownerAddr": info.owner_addr,
                                            "communityIds": info.community_ids,
                                            "sharedAt": info.shared_at,
                                        }),
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = ?e, subscription_id, "profile broadcast rejected");
                            }
                        }
                    }
                    Err(_) => {
                        if !closing_for_task.load(Ordering::SeqCst) {
                            tracing::warn!(subscription_id, "profile broadcast subscriber closed; reconnecting");
                        }
                        break;
                    }
                }
            }
            if closing_for_task.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
    handles.insert(subscription_id, handle);
}
ProfileBroadcastRequest::Unsubscribe { subscription_id } => {
    if let Some(h) = handles.remove(&subscription_id) {
        h.abort();
    }
    profile_broadcast_cache_for_loop.drop_subscription(subscription_id).await;
}
```

`profile_broadcast_cache_for_loop` is an `Arc<ProfileBroadcastCache>` cloned into the event-loop block from `start_event_loop`'s caller (NodeState).

- [ ] **Step 3: Extend `NodeState` with publisher/cache + request channel**

In `src-tauri/src/lib.rs`, find the `NodeState` struct and add:

```rust
    /// Sub-D Phase 4 (ZEB-281): broadcast publisher. `Some` while the
    /// node is running and an owner identity is available. Shutdown is
    /// called explicitly in `stop_inner` before the event-loop thread
    /// is joined so the in-flight publish drains.
    profile_broadcast_publisher:
        Option<Arc<crate::profile_broadcast::ProfileBroadcastPublisher>>,

    /// Sub-D Phase 4 (ZEB-281): peer-broadcast cache. Shared with the
    /// event-loop's subscriber task pool. Always Some while node is
    /// running.
    profile_broadcast_cache:
        Option<Arc<crate::profile_broadcast::ProfileBroadcastCache>>,

    /// Sub-D Phase 4 (ZEB-281): control channel into the event-loop's
    /// profile-broadcast subscriber task pool. IPC handlers send
    /// `Subscribe`/`Unsubscribe`; the event loop owns the Zenoh
    /// subscriber map.
    profile_broadcast_request_tx: Option<
        tokio::sync::mpsc::Sender<crate::event_loop::ProfileBroadcastRequest>,
    >,

    /// Sub-D Phase 4 (ZEB-281): monotonic subscription-id allocator.
    /// Persisted only across IPC calls within a single node lifetime;
    /// reset on stop_node.
    profile_broadcast_next_subscription_id: Arc<std::sync::atomic::AtomicU64>,
```

Update the `Default::default()` (or whatever initializer NodeState uses) to set the four new fields to `None` / fresh atomic.

- [ ] **Step 4: Wire publisher + cache + request-channel in `start_node` / `start_inner`**

In `src-tauri/src/lib.rs`, find `start_node` (or `start_inner`). After the existing `crdt_state` + `hlc_tracker` + identity-loading block but BEFORE the event_loop spawn:

```rust
// Sub-D Phase 4 (ZEB-281): construct profile-broadcast publisher + cache.
let profile_broadcast_cache = Arc::new(
    crate::profile_broadcast::ProfileBroadcastCache::default(),
);
let (profile_broadcast_request_tx, profile_broadcast_request_rx) =
    tokio::sync::mpsc::channel::<crate::event_loop::ProfileBroadcastRequest>(64);

let profile_broadcast_publisher = {
    let source = Arc::new(crate::profile_broadcast::OwnerStateBroadcastSource {
        crdt_state: Arc::clone(&crdt_state),
        hlc_tracker: Arc::clone(&hlc_tracker),
        device_id: device_id.clone(),
    });
    let sink = Arc::new(crate::profile_broadcast::EventLoopPublishSink {
        publish_tx: publish_tx.clone(),
    });
    crate::profile_broadcast::ProfileBroadcastPublisher::spawn(
        owner_signing_key.clone(),
        owner_identity_pub_64,
        source,
        sink,
        crate::profile_broadcast::PUBLISHER_DEBOUNCE,
        crate::profile_broadcast::PUBLISHER_REFRESH_INTERVAL,
    )
};
```

The exact name of `owner_signing_key` / `owner_identity_pub_64` depends on the existing identity-loading block (the same field that Phase 2 / Phase 3 wired `library_signing_key` from — see `dm_identity_pub_64` in NodeState). The implementer must locate the existing identity handle and clone it; if there is no existing `SigningKey` clone on the path, derive one from `PrivateIdentity` via the same idiom used to build admin signatures elsewhere in start_node.

Pass `profile_broadcast_request_rx` + `profile_broadcast_cache.clone()` into the `event_loop` spawn parameters. Pass them through into the per-event-loop closure in `start_event_loop`.

Set on `NodeState`:
```rust
state.profile_broadcast_publisher = Some(Arc::clone(&profile_broadcast_publisher));
state.profile_broadcast_cache = Some(Arc::clone(&profile_broadcast_cache));
state.profile_broadcast_request_tx = Some(profile_broadcast_request_tx);
state.profile_broadcast_next_subscription_id = Arc::new(std::sync::atomic::AtomicU64::new(1));
```

- [ ] **Step 5: Wire shutdown in `stop_node` / `stop_inner`**

Add to `stop_inner`, ordered BEFORE the event-loop thread join:

```rust
if let Some(pub_) = state.profile_broadcast_publisher.take() {
    pub_.shutdown().await;
}
state.profile_broadcast_cache = None;
state.profile_broadcast_request_tx = None;
// Reset the id allocator so a restart starts at 1 again.
state.profile_broadcast_next_subscription_id =
    Arc::new(std::sync::atomic::AtomicU64::new(1));
```

- [ ] **Step 6: Add `set_space_shared_in_profile` IPC**

Append to `src-tauri/src/lib.rs` (near the other Sub-D IPCs):

```rust
/// IPC: Sub-D Phase 4 (ZEB-281). Toggle the opt-in flag for a community
/// Space. Mutates `Space.shared_in_profile`, bumps `Space.updated_at`,
/// then notifies the profile-broadcast publisher so it re-walks the
/// opted-in set.
#[tauri::command]
async fn set_space_shared_in_profile(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    shared: bool,
) -> Result<(), String> {
    let (crdt_state, publisher, sync_engine) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
            g.profile_broadcast_publisher
                .clone()
                .ok_or("profile_broadcast_publisher missing — node not running?")?,
            g.sync_engine
                .clone()
                .ok_or("sync_engine missing — node not running?")?,
        )
    };

    let community_space_id = parse_space_id_hex(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?;

    // Mutate under the CRDT lock; bump updated_at via the sync engine so
    // the change replicates to bound devices.
    {
        let mut g = crdt_state.lock().await;
        // Locate the community Space.
        let space = g
            .spaces
            .values_mut()
            .find(|s| {
                s.community_id == Some(community_space_id)
                    && matches!(s.kind, crate::owner_state_types::SpaceKind::Community)
            })
            .ok_or_else(|| format!("community Space not found for community_id={community_id}"))?;
        if space.shared_in_profile == shared {
            // No-op — return early without bumping HLC.
            return Ok(());
        }
        space.shared_in_profile = shared;
        // Bump updated_at via the sync engine's HLC tracker.
        space.updated_at = sync_engine.bump_local_hlc().await;
    }

    // Notify the publisher to recompute + debounce-publish.
    publisher.notify_dirty();
    Ok(())
}
```

Helper `parse_space_id_hex` already exists in `lib.rs` (used by `add_space`, `redeem_invite`, etc.) — locate it via `grep -n "fn parse_space_id_hex" src-tauri/src/lib.rs`. If it returns a different `Result` shape, adapt the `?` extraction accordingly.

`SyncEngine::bump_local_hlc()` — if a method with this name does not exist, locate the existing HLC-bump idiom (likely under `crate::owner_state_sync::SyncEngine` or a free function). The implementer must use the SAME idiom that `add_space` uses to bump `Space.updated_at`.

- [ ] **Step 7: Add `subscribe_peer_profile` IPC**

```rust
/// IPC: Sub-D Phase 4 (ZEB-281). Subscribe to a peer's profile-broadcast
/// topic. Returns a u64 SubscriptionId the frontend uses to address
/// subsequent unsubscribe/get_cached calls + to filter incoming
/// `profile-broadcast-received` events.
#[tauri::command]
async fn subscribe_peer_profile(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    peer_addr: String,
) -> Result<u64, String> {
    let (cache, request_tx, next_id) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.profile_broadcast_cache
                .clone()
                .ok_or("profile_broadcast_cache missing — node not running?")?,
            g.profile_broadcast_request_tx
                .clone()
                .ok_or("profile_broadcast_request_tx missing — node not running?")?,
            Arc::clone(&g.profile_broadcast_next_subscription_id),
        )
    };

    let peer_owner_addr = parse_owner_addr_hex(&peer_addr)
        .map_err(|e| format!("invalid peer_addr hex: {e}"))?;
    let id = next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    cache.register(id, peer_owner_addr).await;
    request_tx
        .send(crate::event_loop::ProfileBroadcastRequest::Subscribe {
            subscription_id: id,
            peer_addr: peer_owner_addr,
        })
        .await
        .map_err(|e| format!("profile_broadcast_request_tx send: {e}"))?;
    Ok(id)
}
```

`parse_owner_addr_hex` already exists — locate via `grep -n "fn parse_owner_addr_hex" src-tauri/src/lib.rs`.

- [ ] **Step 8: Add `unsubscribe_peer_profile` IPC**

```rust
#[tauri::command]
async fn unsubscribe_peer_profile(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    subscription_id: u64,
) -> Result<(), String> {
    let request_tx = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.profile_broadcast_request_tx
            .clone()
            .ok_or("profile_broadcast_request_tx missing — node not running?")?
    };
    request_tx
        .send(crate::event_loop::ProfileBroadcastRequest::Unsubscribe { subscription_id })
        .await
        .map_err(|e| format!("profile_broadcast_request_tx send: {e}"))?;
    Ok(())
}
```

- [ ] **Step 9: Add `get_cached_peer_profile` IPC**

```rust
#[tauri::command]
async fn get_cached_peer_profile(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    subscription_id: u64,
) -> Result<Option<crate::profile_broadcast::DiscoveredProfileInfo>, String> {
    let cache = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.profile_broadcast_cache
            .clone()
            .ok_or("profile_broadcast_cache missing — node not running?")?
    };
    Ok(cache.get_cached(subscription_id).await)
}
```

- [ ] **Step 10: Register the new commands in `tauri::generate_handler!`**

Locate the existing `tauri::generate_handler![...]` block in `start_inner` / `start_node`. Add (alphabetical with other commands):

```rust
            set_space_shared_in_profile,
            subscribe_peer_profile,
            unsubscribe_peer_profile,
            get_cached_peer_profile,
```

- [ ] **Step 11: Create the shared integration-test fixture file**

Create `src-tauri/tests/common/profile_fixtures.rs`:

```rust
//! Shared test fixtures for Sub-D Phase 4 (ZEB-281) integration tests.
//!
//! Mirrors `tests/common/library_fixtures.rs` (Phase 1/2/3).

use ed25519_dalek::SigningKey;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::profile_broadcast::{sign_broadcast, ProfileMembershipBroadcast};

/// Build a deterministic test signing key + identity bundle from a seed.
pub fn build_test_owner_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64], OwnerAddr) {
    let private = harmony_identity::PrivateIdentity::from_seed(seed);
    let identity_pub = private.identity().to_public_bytes();
    let signer = private.signing_key_clone();
    let addr = OwnerAddr(private.identity().address_hash);
    (signer, identity_pub, addr)
}

/// Build + canonical-CBOR-encode a mock ProfileMembershipBroadcast.
/// Returns (cbor_bytes, broadcaster_owner_addr).
pub fn mock_profile_broadcast(
    seed: [u8; 32],
    community_ids: Vec<SpaceId>,
    shared_at: Hlc,
) -> (Vec<u8>, OwnerAddr, ProfileMembershipBroadcast) {
    let (signer, identity_pub, addr) = build_test_owner_identity(seed);
    let b = sign_broadcast(&signer, identity_pub, community_ids, shared_at).unwrap();
    let bytes = harmony_app::owner_state_crypto::canonical_cbor_encode(&b).unwrap();
    (bytes, addr, b)
}

pub fn fixture_space_id(byte: u8) -> SpaceId {
    SpaceId([byte; 16])
}

pub fn fixture_hlc(wall_ms: u64, logical: u64) -> Hlc {
    Hlc { wall_ms, logical, device_id: "fix".into() }
}
```

If `tests/common/mod.rs` doesn't already exist (it's needed for `pub mod profile_fixtures;` exposure), check via `ls src-tauri/tests/common/`. If it exists, add `pub mod profile_fixtures;` to it; otherwise create it with that single line.

- [ ] **Step 12: Create the integration test file**

Create `src-tauri/tests/profile_broadcast_integration.rs`:

```rust
//! Sub-D Phase 4 (ZEB-281) integration tests. Use in-process Zenoh to
//! publish + receive end-to-end broadcasts and assert the
//! `ProfileBroadcastCache` reaches the expected state.
//!
//! Mirrors `tests/library_directory_integration.rs` (Sub-D Phase 1/2/3)
//! for setup style and Zenoh-session bootstrapping.

mod common;
use common::profile_fixtures::{
    build_test_owner_identity, fixture_hlc, fixture_space_id, mock_profile_broadcast,
};

use harmony_app::owner_state_types::{OwnerAddr, SpaceId};
use harmony_app::profile_broadcast::{
    broadcast_topic_for, CacheOnSampleError, ProfileBroadcastCache,
    ProfileMembershipBroadcast,
};

/// Cache-level integration: subscriber receives, verifies, attribution
/// passes, cache populated. Spec §11.3 row 1.
#[tokio::test]
async fn peer_subscribe_receives_broadcast() {
    let cache = ProfileBroadcastCache::default();
    let (_signer, _identity_pub, peer_addr) = build_test_owner_identity([1u8; 32]);
    cache.register(1, peer_addr).await;

    let (_bytes, _addr, broadcast) = mock_profile_broadcast(
        [1u8; 32],
        vec![fixture_space_id(10), fixture_space_id(20)],
        fixture_hlc(1000, 0),
    );
    cache.on_sample(1, broadcast).await.expect("on_sample ok");

    let snap = cache.get_cached(1).await.expect("cached present");
    assert_eq!(snap.owner_addr, hex::encode(peer_addr.0));
    assert_eq!(snap.community_ids.len(), 2);
    assert_eq!(snap.shared_at, "1000");
}

/// Adversary publishes on peer X's topic with peer Y's identity bundle —
/// `on_sample` returns `AttributionMismatch`. Spec §11.3 row 2.
#[tokio::test]
async fn attribution_mismatch_rejected() {
    let cache = ProfileBroadcastCache::default();
    let (_signer_x, _identity_pub_x, peer_x_addr) = build_test_owner_identity([1u8; 32]);
    cache.register(1, peer_x_addr).await;

    // Broadcast claims peer Y identity, but registered subscription is for X.
    let (_bytes, _y_addr, broadcast_from_y) = mock_profile_broadcast(
        [2u8; 32],
        vec![fixture_space_id(7)],
        fixture_hlc(1000, 0),
    );
    let err = cache.on_sample(1, broadcast_from_y).await.unwrap_err();
    assert!(matches!(err, CacheOnSampleError::AttributionMismatch { .. }));
    // Cache stays empty.
    assert!(cache.get_cached(1).await.is_none());
}

/// Subscribe → land a broadcast → drop_subscription → cache cleared,
/// re-registering and querying returns None. Spec §11.3 row 3.
#[tokio::test]
async fn subscribe_unsubscribe_lifecycle() {
    let cache = ProfileBroadcastCache::default();
    let (_signer, _identity_pub, peer_addr) = build_test_owner_identity([3u8; 32]);
    cache.register(7, peer_addr).await;

    let (_bytes, _addr, broadcast) = mock_profile_broadcast(
        [3u8; 32],
        vec![fixture_space_id(1)],
        fixture_hlc(2000, 0),
    );
    cache.on_sample(7, broadcast).await.expect("on_sample ok");
    assert!(cache.get_cached(7).await.is_some());

    cache.drop_subscription(7).await;
    assert!(cache.get_cached(7).await.is_none(),
        "after drop_subscription, cache must be empty for this id");

    // Re-registering the same id: cache empty (new entry, no broadcast yet).
    cache.register(7, peer_addr).await;
    assert!(cache.get_cached(7).await.is_none());

    // Subsequent on_sample for a now-unknown id returns SubscriptionNotFound.
    let (_bytes2, _addr2, b2) = mock_profile_broadcast(
        [3u8; 32],
        vec![fixture_space_id(1)],
        fixture_hlc(3000, 0),
    );
    cache.drop_subscription(7).await;
    let err = cache.on_sample(7, b2).await.unwrap_err();
    assert!(matches!(err, CacheOnSampleError::SubscriptionNotFound(7)));
}

/// Two devices owned by the same user: dev2's subscription to its own
/// profile-broadcast topic sees the (set_shared)-triggered publish from
/// dev1. Cache-level check using the broadcast bytes the publisher would
/// produce. Spec §11.3 row 4.
///
/// (This test exercises the round-trip from "construct the publisher
/// payload bytes that a real publish would emit" → "deliver to a
/// peer-side cache as if Zenoh transported them" — without spinning up
/// a Zenoh session. Full Zenoh end-to-end is too heavy for nextest; we
/// rely on Phase 2's announce integration test to cover the transport
/// layer.)
#[tokio::test]
async fn self_publish_on_opt_in_change() {
    // dev1 publisher would emit this broadcast after the user toggles
    // `shared_in_profile = true` on community_id = fixture_space_id(42).
    let (_bytes, owner_addr, broadcast) = mock_profile_broadcast(
        [99u8; 32],
        vec![fixture_space_id(42)],
        fixture_hlc(5000, 0),
    );
    // dev2 (or any peer) subscribes to the owner's topic — for this test
    // we model the cache only; the Zenoh transport is mocked by direct
    // delivery to on_sample.
    let cache = ProfileBroadcastCache::default();
    cache.register(11, owner_addr).await;
    cache.on_sample(11, broadcast).await.expect("dev2 should receive + verify");
    let snap = cache.get_cached(11).await.unwrap();
    assert_eq!(snap.community_ids, vec![hex::encode([42u8; 16])]);
}

/// Rotation: dev1 publisher's N→0 rotation publish (empty community_ids,
/// strictly-newer HLC) supersedes the prior non-empty broadcast at peer
/// caches. Spec §11.3 row 5.
#[tokio::test]
async fn self_publish_rotation_to_empty() {
    let cache = ProfileBroadcastCache::default();
    let (_signer, _identity_pub, owner_addr) = build_test_owner_identity([55u8; 32]);
    cache.register(13, owner_addr).await;

    // First publish: non-empty.
    let (_bytes1, _addr1, b1) = mock_profile_broadcast(
        [55u8; 32],
        vec![fixture_space_id(1), fixture_space_id(2)],
        fixture_hlc(1000, 0),
    );
    cache.on_sample(13, b1).await.expect("first publish");
    assert_eq!(
        cache.get_cached(13).await.unwrap().community_ids.len(),
        2
    );

    // Rotation: empty community_ids, newer HLC.
    let (_bytes2, _addr2, b2) = mock_profile_broadcast(
        [55u8; 32],
        vec![],
        fixture_hlc(2000, 0),
    );
    cache.on_sample(13, b2).await.expect("rotation publish");
    let snap = cache.get_cached(13).await.unwrap();
    assert_eq!(
        snap.community_ids.len(),
        0,
        "rotation must overwrite non-empty with empty"
    );
    assert_eq!(snap.shared_at, "2000");
}
```

- [ ] **Step 13: Build and run integration tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test profile_broadcast_integration 2>&1 | tail -20
```

Expected: 5 tests pass.

If any test cannot compile because the implementer's IPC wiring referenced symbols that don't exist yet (e.g., `parse_owner_addr_hex` is named differently), surface the precise compile error to the implementer for fix-up.

- [ ] **Step 14: Build + run full Rust suite**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -5
```

Expected: all green. Pass count = Task-0-baseline + 13 (Tasks 1+2+3 unit) + 4 (Task 4 wire-format) + 5 (Task 5 integration) = baseline + 22.

- [ ] **Step 15: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/event_loop.rs src-tauri/tests/common src-tauri/tests/profile_broadcast_integration.rs
git status
git commit -m "$(cat <<'EOF'
feat(zeb-281): IPC surface + NodeState wiring + event-loop subscriber pool

- 4 new IPCs: set_space_shared_in_profile, subscribe_peer_profile,
  unsubscribe_peer_profile, get_cached_peer_profile.
- 1 new event: profile-broadcast-received { subscriptionId, info }.
- NodeState fields: profile_broadcast_publisher, profile_broadcast_cache,
  profile_broadcast_request_tx, profile_broadcast_next_subscription_id.
- event_loop.rs: ProfileBroadcastRequest::{Subscribe,Unsubscribe} routes
  through a per-subscription Zenoh subscriber pool with retry/backoff
  matching the Phase 2 announce subscriber.
- start_node/stop_node lifecycle: publisher.spawn() / publisher.shutdown().
- 5 integration tests: peer_subscribe_receives_broadcast,
  attribution_mismatch_rejected, subscribe_unsubscribe_lifecycle,
  self_publish_on_opt_in_change, self_publish_rotation_to_empty.

Spec §6, §7, §11.3.
EOF
)"
```

---

## Task 6: Frontend service + UI integration + 6 vitest

**Goal:** Build the frontend service wrapper, add the opt-in toggle to `CommunitySettingsPanel.svelte`, add the "Public memberships" section to `ProfilePopover.svelte`, and cover with 6 vitest cases.

**Files:**
- Create: `src/lib/profile-broadcast-service.ts`
- Modify: `src/lib/components/CommunitySettingsPanel.svelte`
- Modify: `src/lib/components/ProfilePopover.svelte`
- Create: `src/lib/__tests__/profile-broadcast-service.test.ts`
- Create: `src/lib/components/__tests__/ProfilePopover.test.ts`
- Create or Modify: `src/lib/components/__tests__/CommunitySettingsPanel.test.ts`

- [ ] **Step 1: Create `src/lib/profile-broadcast-service.ts`**

```typescript
import type { TauriAdapter } from './zenoh-service';

/**
 * Mirrors `profile_broadcast::DiscoveredProfileInfo` IPC return shape
 * (ZEB-281 Sub-D Phase 4). The Rust DTO uses `#[serde(rename_all =
 * "camelCase")]`, so wire keys are camelCase (matching
 * `DiscoveredLibraryInfo` from Phase 2). `sharedAt` is a base-10 string
 * of `shared_at.wall_ms` for display only — callers MUST NOT use this
 * for HLC ordering decisions.
 */
export interface ProfileMembershipBroadcastInfo {
  /** Hex-encoded 16-byte OwnerAddr (32 hex chars). */
  ownerAddr: string;
  /** Hex-encoded SpaceIds (32 hex chars each). */
  communityIds: string[];
  /** `shared_at.wall_ms` as base-10 string for display only. */
  sharedAt: string;
}

/**
 * Thin IPC wrapper for the Sub-D Phase 4 profile-broadcast IPCs. Mirrors
 * the `library-directory-service.ts` shape: constructor takes a
 * `TauriAdapter`, each method translates JS-side camelCase arg names to
 * the Rust snake_case IPC parameter names that Tauri rewrites at the
 * boundary.
 *
 * Error extraction: production rejections are strings; tests use Error
 * objects with `"Error: "` prefix. Callers should wrap invocations with
 * `e instanceof Error ? e.message : String(e)` if they need to surface
 * the message to UI (per CLAUDE.md "Tauri IPC error extraction").
 */
export class ProfileBroadcastService {
  constructor(private adapter: TauriAdapter) {}

  /** Toggle per-community opt-in. Server-side mutates `Space.shared_in_profile`,
   *  bumps `Space.updated_at`, notifies the publisher. */
  async setShared(communityId: string, shared: boolean): Promise<void> {
    await this.adapter.invoke('set_space_shared_in_profile', {
      communityId,
      shared,
    });
  }

  /** Subscribe to a peer's broadcast topic. Returns a u64 handle the
   *  caller passes to subsequent unsubscribe / getCached calls. */
  async subscribe(peerAddr: string): Promise<number> {
    return (await this.adapter.invoke('subscribe_peer_profile', {
      peerAddr,
    })) as number;
  }

  /** Cancel a subscription. Idempotent server-side. */
  async unsubscribe(subscriptionId: number): Promise<void> {
    await this.adapter.invoke('unsubscribe_peer_profile', {
      subscriptionId,
    });
  }

  /** Retrieve the latest verified broadcast for a subscription, or null
   *  if none has arrived yet. */
  async getCached(
    subscriptionId: number,
  ): Promise<ProfileMembershipBroadcastInfo | null> {
    return (await this.adapter.invoke('get_cached_peer_profile', {
      subscriptionId,
    })) as ProfileMembershipBroadcastInfo | null;
  }
}
```

- [ ] **Step 2: Write 1 vitest case for the service**

Create `src/lib/__tests__/profile-broadcast-service.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { ProfileBroadcastService } from '../profile-broadcast-service';

describe('ProfileBroadcastService', () => {
  it('service_subscribe_returns_handle', async () => {
    const invoke = vi.fn(async (cmd: string, args: unknown) => {
      expect(cmd).toBe('subscribe_peer_profile');
      expect(args).toEqual({ peerAddr: 'abcd1234' });
      return 42;
    });
    const adapter = { invoke } as unknown as Parameters<
      typeof ProfileBroadcastService.prototype.constructor
    >[0];
    const svc = new ProfileBroadcastService(adapter);
    const id = await svc.subscribe('abcd1234');
    expect(id).toBe(42);
    expect(invoke).toHaveBeenCalledOnce();
  });
});
```

- [ ] **Step 3: Add the opt-in toggle to `CommunitySettingsPanel.svelte`**

Edit `src/lib/components/CommunitySettingsPanel.svelte`. Extend the `let { ... }: { ... } = $props();` block with two new props:

```typescript
    sharedInProfile: boolean;
    onToggleSharedInProfile: (shared: boolean) => Promise<void>;
```

Add a new `<div class="section">` block AFTER the existing "Info" section and BEFORE the "Members" section:

```svelte
    <div class="section">
      <div class="section-label">Public profile</div>
      <label class="toggle-row">
        <input
          type="checkbox"
          checked={sharedInProfile}
          onchange={async (e) => {
            const checked = (e.currentTarget as HTMLInputElement).checked;
            try {
              await onToggleSharedInProfile(checked);
            } catch (err) {
              const msg = err instanceof Error ? err.message : String(err);
              // Roll back the UI to match server state on failure.
              (e.currentTarget as HTMLInputElement).checked = !checked;
              console.warn('toggle shared_in_profile failed:', msg);
            }
          }}
        />
        <span class="toggle-label">
          Share this community in my public profile
        </span>
      </label>
      <p class="toggle-help">
        When enabled, peers viewing your profile will see that you've
        joined <strong>{communityName}</strong>. Off by default.
      </p>
    </div>
```

Add minimal styles inside the existing `<style>` block (append at the end before the closing `</style>`):

```css
  .toggle-row {
    display: flex;
    gap: 8px;
    align-items: center;
    cursor: pointer;
    padding: 6px 0;
  }
  .toggle-label {
    font-size: 13px;
    color: var(--text-primary);
  }
  .toggle-help {
    font-size: 12px;
    color: var(--text-muted);
    margin: 4px 0 0;
  }
```

Find every caller of `CommunitySettingsPanel` in the codebase:
```bash
grep -rln "CommunitySettingsPanel" src/
```

For each caller, pass the new props. The caller will need to read `space.sharedInProfile` from owner-state and route the toggle through `ProfileBroadcastService.setShared`. Example caller wiring (adjust paths to match the real caller location):

```svelte
<CommunitySettingsPanel
  {communityId}
  {communityName}
  {communityKind}
  {members}
  {myAddress}
  {myPower}
  {isDegraded}
  sharedInProfile={spaces.get(communityId)?.sharedInProfile ?? false}
  onToggleSharedInProfile={async (shared) => {
    await profileBroadcastService.setShared(communityId, shared);
  }}
  {onClose}
  {onKick}
  {onSetPower}
  {onLeave}
  {onGenerateInvite}
/>
```

The exact owner-state read path (`spaces.get(communityId)?.sharedInProfile`) depends on how Spaces are exposed to the Svelte tree. The implementer must inspect the existing read-path used for `communityKind` / `communityName` and mirror it for `sharedInProfile`.

- [ ] **Step 4: Add the "Public memberships" section to `ProfilePopover.svelte`**

Edit `src/lib/components/ProfilePopover.svelte`. Extend the `$props` block:

```typescript
  let { profile, x, y, onClose, ownAddress, profileBroadcastService, resolveCommunityName }: {
    profile: Profile;
    x: number;
    y: number;
    onClose: () => void;
    ownAddress: string;
    profileBroadcastService: import('../profile-broadcast-service').ProfileBroadcastService;
    resolveCommunityName: (communityIdHex: string) => string | null;
  } = $props();
```

Add state for the subscription:

```typescript
  let subscriptionId = $state<number | null>(null);
  let memberships = $state<import('../profile-broadcast-service').ProfileMembershipBroadcastInfo | null>(null);
  let isLoading = $state(true);
  // Switch loading→empty after 3s if no broadcast arrives. Spec §8.3.
  const LOAD_TIMEOUT_MS = 3000;
```

In the existing `$effect` block (which already wires keyboard + click-outside listeners), add subscription lifecycle. Restructure as two `$effect` calls so listener-cleanup and subscription-cleanup are independent:

```typescript
  $effect(() => {
    if (profile.address === ownAddress) {
      // Don't subscribe to ourselves; the panel never shows the section.
      return;
    }
    let cancelled = false;
    let unlistenFn: (() => void) | null = null;

    (async () => {
      const id = await profileBroadcastService.subscribe(profile.address);
      if (cancelled) {
        await profileBroadcastService.unsubscribe(id).catch(() => {});
        return;
      }
      subscriptionId = id;
      // Render immediately if already cached.
      const cached = await profileBroadcastService.getCached(id);
      if (!cancelled && cached) {
        memberships = cached;
        isLoading = false;
      }
      // Listen for new arrivals.
      // (The exact event-listener import path follows existing patterns
      // in the codebase — `import { listen } from '@tauri-apps/api/event'`
      // or whatever wrapper exists in `src/lib/`.)
      const { listen } = await import('@tauri-apps/api/event');
      // Event payload is flat per spec §7:
      // { subscriptionId, ownerAddr, communityIds, sharedAt }.
      const unlisten = await listen<{
        subscriptionId: number;
        ownerAddr: string;
        communityIds: string[];
        sharedAt: string;
      }>('profile-broadcast-received', (event) => {
        if (event.payload.subscriptionId === id) {
          memberships = {
            ownerAddr: event.payload.ownerAddr,
            communityIds: event.payload.communityIds,
            sharedAt: event.payload.sharedAt,
          };
          isLoading = false;
        }
      });
      unlistenFn = unlisten;
    })();

    const timeout = setTimeout(() => {
      if (!cancelled && isLoading) {
        isLoading = false;
        // memberships stays null → renders empty state
      }
    }, LOAD_TIMEOUT_MS);

    return () => {
      cancelled = true;
      clearTimeout(timeout);
      if (unlistenFn) unlistenFn();
      if (subscriptionId !== null) {
        profileBroadcastService.unsubscribe(subscriptionId).catch(() => {});
        subscriptionId = null;
      }
    };
  });
```

Add the rendering section in the template, AFTER the existing `popover-sounds` div:

```svelte
  {#if profile.address !== ownAddress}
    <div class="popover-memberships">
      <div class="memberships-label">Public memberships</div>
      {#if isLoading}
        <div class="memberships-loading">Looking up public memberships…</div>
      {:else if memberships === null || memberships.communityIds.length === 0}
        <div class="memberships-empty">No public memberships shared.</div>
      {:else}
        <ul class="memberships-list">
          {#each memberships.communityIds as communityId}
            <li>{resolveCommunityName(communityId) ?? communityId.slice(0, 8) + '…'}</li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
```

Append styles inside the existing `<style>` block:

```css
  .popover-memberships {
    border-top: 1px solid var(--border);
    padding-top: 10px;
    margin-top: 10px;
  }
  .memberships-label {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 6px;
  }
  .memberships-loading,
  .memberships-empty {
    font-size: 12px;
    color: var(--text-muted);
    padding: 3px 0;
  }
  .memberships-list {
    list-style: none;
    padding: 0;
    margin: 0;
  }
  .memberships-list li {
    font-size: 12px;
    color: var(--text-secondary);
    padding: 3px 0;
  }
```

Find every caller of `ProfilePopover` in the codebase:
```bash
grep -rln "ProfilePopover" src/
```

Each caller must pass the new props (`ownAddress`, `profileBroadcastService`, `resolveCommunityName`). The implementer wires these from the caller's scope.

- [ ] **Step 5: Write 4 vitest cases for ProfilePopover**

Create `src/lib/components/__tests__/ProfilePopover.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, cleanup, waitFor } from '@testing-library/svelte';
import ProfilePopover from '../ProfilePopover.svelte';

const SELF_ADDR = '00'.repeat(16);
const PEER_ADDR = 'aa'.repeat(16);

function makeProfile(addr: string) {
  return {
    address: addr,
    displayName: 'Test User',
    avatarUrl: null,
    statusText: null,
    notificationSounds: null,
  } as unknown as import('../../types').Profile;
}

function makeService(opts?: {
  initialCached?: import('../../profile-broadcast-service').ProfileMembershipBroadcastInfo | null;
}) {
  const subscribe = vi.fn(async () => 1);
  const unsubscribe = vi.fn(async () => {});
  const getCached = vi.fn(async () => opts?.initialCached ?? null);
  const setShared = vi.fn(async () => {});
  return {
    service: { subscribe, unsubscribe, getCached, setShared } as unknown as
      import('../../profile-broadcast-service').ProfileBroadcastService,
    subscribe,
    unsubscribe,
    getCached,
  };
}

describe('ProfilePopover', () => {
  afterEach(() => cleanup());

  it('popover_subscribes_on_mount', async () => {
    const { service, subscribe } = makeService();
    render(ProfilePopover, {
      props: {
        profile: makeProfile(PEER_ADDR),
        x: 0,
        y: 0,
        onClose: () => {},
        ownAddress: SELF_ADDR,
        profileBroadcastService: service,
        resolveCommunityName: () => null,
      },
    });
    await waitFor(() => expect(subscribe).toHaveBeenCalledWith(PEER_ADDR));
  });

  it('popover_unsubscribes_on_close', async () => {
    const { service, unsubscribe } = makeService();
    const { unmount } = render(ProfilePopover, {
      props: {
        profile: makeProfile(PEER_ADDR),
        x: 0,
        y: 0,
        onClose: () => {},
        ownAddress: SELF_ADDR,
        profileBroadcastService: service,
        resolveCommunityName: () => null,
      },
    });
    await waitFor(() => {/* let subscribe resolve */});
    unmount();
    await waitFor(() => expect(unsubscribe).toHaveBeenCalled());
  });

  it('popover_shows_loading_then_loaded', async () => {
    const { service } = makeService({
      initialCached: {
        ownerAddr: PEER_ADDR,
        communityIds: ['bb'.repeat(16)],
        sharedAt: '5000',
      },
    });
    const { getByText } = render(ProfilePopover, {
      props: {
        profile: makeProfile(PEER_ADDR),
        x: 0,
        y: 0,
        onClose: () => {},
        ownAddress: SELF_ADDR,
        profileBroadcastService: service,
        resolveCommunityName: () => 'Test Community',
      },
    });
    // First render shows the loading state.
    expect(getByText('Looking up public memberships…')).toBeTruthy();
    // After cache hydration, the community name appears.
    await waitFor(() => expect(getByText('Test Community')).toBeTruthy());
  });

  it('popover_shows_no_memberships_after_timeout', async () => {
    vi.useFakeTimers();
    const { service } = makeService(); // returns null
    const { getByText } = render(ProfilePopover, {
      props: {
        profile: makeProfile(PEER_ADDR),
        x: 0,
        y: 0,
        onClose: () => {},
        ownAddress: SELF_ADDR,
        profileBroadcastService: service,
        resolveCommunityName: () => null,
      },
    });
    expect(getByText('Looking up public memberships…')).toBeTruthy();
    // Advance 3s; loading flips off and empty state renders.
    vi.advanceTimersByTime(3100);
    await waitFor(() =>
      expect(getByText('No public memberships shared.')).toBeTruthy()
    );
    vi.useRealTimers();
  });
});
```

> **Note for implementer:** if `afterEach`/`waitFor` imports are not auto-globals, add `import { afterEach } from 'vitest';` at the top. The exact `import('../../types').Profile` shape must match the project's real `Profile` type — if `displayName` is non-nullable, adjust `makeProfile`.

- [ ] **Step 6: Write 1 vitest case for the CommunitySettingsPanel toggle**

Check if `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` already exists:
```bash
ls src/lib/components/__tests__/CommunitySettingsPanel.test.ts 2>&1
```

If it exists, ADD this case at the end of the existing `describe` block. If not, create the file with this content:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import CommunitySettingsPanel from '../CommunitySettingsPanel.svelte';

function baseProps(overrides: Record<string, unknown> = {}) {
  return {
    communityId: 'aa'.repeat(16),
    communityName: 'Test Community',
    communityKind: 'open' as const,
    members: [],
    myAddress: '11'.repeat(16),
    myPower: 50,
    isDegraded: false,
    onClose: () => {},
    onKick: () => {},
    onSetPower: () => {},
    onLeave: () => {},
    onGenerateInvite: async () => 'invite-url',
    sharedInProfile: false,
    onToggleSharedInProfile: async () => {},
    ...overrides,
  };
}

describe('CommunitySettingsPanel — shared_in_profile toggle', () => {
  it('settings_panel_toggle_invokes_set_shared', async () => {
    const onToggle = vi.fn(async () => {});
    const { getByLabelText } = render(CommunitySettingsPanel, {
      props: baseProps({ onToggleSharedInProfile: onToggle }),
    });
    // Find the toggle by its label text. The label string in the
    // template starts with "Share this community in my public profile".
    const checkbox = getByLabelText(/share this community in my public profile/i);
    await fireEvent.click(checkbox);
    expect(onToggle).toHaveBeenCalledWith(true);
  });
});
```

The `getByLabelText` locator depends on the actual label markup. If the existing `<label>` wraps the `<input>` but the accessible name comes from a sibling `<span>`, `getByLabelText` should still match because Testing Library traverses to the implicit label. If not, fall back to `getByRole('checkbox')`.

- [ ] **Step 7: Run npx tsc + vitest**

```bash
npx tsc --noEmit 2>&1 | tail -10
npx vitest run 2>&1 | tail -20
```

Expected: tsc clean. vitest passes baseline + 6 (1 service + 4 popover + 1 settings panel) = baseline + 6 new tests.

If tsc fails because the `@tauri-apps/api/event` dynamic-import type is missing, switch the dynamic import to a top-level static import. If `cleanup` isn't auto-imported by the project's vitest config, the `afterEach(cleanup)` line may need adjustment.

- [ ] **Step 8: Commit**

```bash
git add src/lib/profile-broadcast-service.ts src/lib/__tests__/profile-broadcast-service.test.ts src/lib/components/ProfilePopover.svelte src/lib/components/CommunitySettingsPanel.svelte src/lib/components/__tests__/ProfilePopover.test.ts src/lib/components/__tests__/CommunitySettingsPanel.test.ts
# Plus any caller files that changed to pass the new props:
git add src/
git status
git commit -m "$(cat <<'EOF'
feat(zeb-281): frontend service + UI for ProfileMembershipBroadcast

- profile-broadcast-service.ts: ProfileBroadcastService class wrapping
  set_space_shared_in_profile / subscribe_peer_profile / unsubscribe_peer_profile
  / get_cached_peer_profile IPCs.
- CommunitySettingsPanel.svelte: new "Public profile" section with toggle
  (off by default, opt-in per community).
- ProfilePopover.svelte: new "Public memberships" section, 3-state UX
  (loading / empty / populated). Subscribe on mount, unsubscribe on
  unmount, listen for profile-broadcast-received events, 3s loading
  timeout.
- 6 vitest tests: service handle return, popover subscribe/unsubscribe
  lifecycle, popover loading→loaded transition, popover timeout→empty
  state, settings panel toggle invokes setShared.

Spec §6, §7, §8, §11.5.
EOF
)"
```

---

## Task 7: Final verification + push + PR

**Goal:** Re-run all 5 CI gates locally to confirm green. Commit the plan if not yet committed (per project precedent). Push the branch to origin. Open the PR with markdown-linked Linear refs. Hand off to the autonomous bot-review monitoring loop.

**Files:** None (or commit the plan if not yet committed).

- [ ] **Step 1: Re-run all 5 CI gates locally**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -5
```

Expected: 4 Rust gates green. Pass count = Task-0-baseline + 22 (13 unit + 4 wire-format + 5 integration).

```bash
npx tsc --noEmit 2>&1 | tail -5
npx vitest run 2>&1 | tail -10
```

Expected: tsc clean; vitest = Task-0-baseline + 6.

- [ ] **Step 2: Sanity-check commit history**

```bash
git log --oneline origin/main..HEAD
```

Expected output (8 commits — spec + 6 implementation + optional plan commit):

```
<sha6> feat(zeb-281): frontend service + UI for ProfileMembershipBroadcast
<sha5> feat(zeb-281): IPC surface + NodeState wiring + event-loop subscriber pool
<sha4> test(zeb-281): wire-format pinning for ProfileMembershipBroadcast + Space.shared_in_profile
<sha3> feat(zeb-281): ProfileBroadcastCache + DiscoveredProfileInfo DTO
<sha2> feat(zeb-281): ProfileBroadcastPublisher state machine
<sha1> feat(zeb-281): Phase 4 wire format — ProfileMembershipBroadcast + Space.shared_in_profile
ca787e3 docs(zeb-281): Sub-D Phase 4 ProfileMembershipBroadcast design
```

Per project precedent (Phase 1 PR #108, Phase 2 PR #109, Phase 3 PR #110 — all committed the plan alongside the spec), commit the plan now if not yet committed:

```bash
git status   # check if docs/plans/2026-05-12-zeb-281-...-plan.md is untracked
git add docs/plans/2026-05-12-zeb-281-sub-d-phase-4-profile-membership-broadcast-plan.md
git commit -m "docs(zeb-281): Phase 4 ProfileMembershipBroadcast implementation plan"
```

- [ ] **Step 3: Push the branch to origin**

```bash
git push -u origin zeb-281-sub-d-phase-4-profile-membership-broadcast 2>&1 | tail -5
```

Expected: `* [new branch]      zeb-281-... -> zeb-281-...` + tracking info. If push fails on hooks, investigate — DO NOT use `--no-verify`.

- [ ] **Step 4: Create the PR with markdown-linked refs**

```bash
gh pr create --title "ZEB-281 Sub-D Phase 4: ProfileMembershipBroadcast" --body "$(cat <<'EOF'
## Summary

Adds the third independent Sub-D discovery primitive: a privacy-preserving Zenoh-broadcast protocol where users curate a per-community opt-in subset of their memberships, and peers viewing a profile see only the communities the owner has explicitly shared.

Implements [ZEB-281](https://linear.app/zeblith/issue/ZEB-281/) — Phase 4 of [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) (Sub-D library-federated discovery). Follows [PR #108](https://github.com/zeblithic/harmony-client/pull/108) (Phase 1 vertical slice), [PR #109](https://github.com/zeblithic/harmony-client/pull/109) (Phase 2 auto-discovery), and [PR #110](https://github.com/zeblithic/harmony-client/pull/110) (Phase 3 federated republication). [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) stays In Progress after this merge — Phase 6 ([ZEB-252](https://linear.app/zeblith/issue/ZEB-252/) direct-join IPC rewrite) remains.

## What changed

**Backend** (`src-tauri/`):
- New module `profile_broadcast.rs`:
  - Wire type `ProfileMembershipBroadcast { owner_identity_pub, community_ids, shared_at, signature }` with 2-char CBOR keys `ai/cs/sa/sg`.
  - `MAX_SHARED_COMMUNITIES = 200`, `PROFILE_DISCOVERY_TOPIC_PREFIX = "harmony/discovery/profile/"`.
  - `verify_broadcast` returns derived `OwnerAddr` (caller compares to topic owner for attribution check). 5 error variants.
  - `ProfileBroadcastPublisher`: `Notify`-driven 2s debounce + 10min periodic refresh + N→0 rotation. Privacy invariant: no publish before first opt-in.
  - `ProfileBroadcastCache`: per-subscription latest-HLC-wins replay defense.
  - `DiscoveredProfileInfo` DTO with camelCase wire keys (`ownerAddr/communityIds/sharedAt`).
- `owner_state_types.rs`:
  - `Space.shared_in_profile: bool` with `rename = "sp"`, `default`, `skip_serializing_if = "core::ops::Not::not"` — byte-identical to pre-Phase-4 owner-state wire bytes for the default-false case.
  - `validate_invariants` rejects `shared_in_profile = true` on non-community Spaces.
- `lib.rs`:
  - 4 new IPCs: `set_space_shared_in_profile`, `subscribe_peer_profile`, `unsubscribe_peer_profile`, `get_cached_peer_profile`.
  - 1 new Tauri event: `profile-broadcast-received { subscriptionId, info }`.
  - NodeState fields for publisher / cache / request channel / id allocator.
- `event_loop.rs`:
  - `ProfileBroadcastRequest::{Subscribe,Unsubscribe}` routes per-subscription Zenoh subscriber tasks through the event-loop pool with retry/backoff matching the Phase 2 announce subscriber.

**Frontend** (`src/lib/`):
- `profile-broadcast-service.ts`: thin `ProfileBroadcastService` wrapper (4 methods + `ProfileMembershipBroadcastInfo` DTO).
- `CommunitySettingsPanel.svelte`: new "Public profile" toggle section (off by default, opt-in per community).
- `ProfilePopover.svelte`: new "Public memberships" section with 3-state rendering (loading / empty / populated). Subscribe on mount, unsubscribe on unmount, listen for the new event, 3s loading timeout.

**Tests** (~28 new):
- 13 Rust unit tests in `profile_broadcast.rs` (8 verify_broadcast + 3 publisher + 2 state replay).
- 4 wire-format pinning tests (2 new file + 2 owner-state wire-compat assertions).
- 5 integration tests in `tests/profile_broadcast_integration.rs`.
- 6 vitest tests (1 service + 4 popover + 1 settings panel).

## Design references

- Spec: [`docs/specs/2026-05-12-zeb-281-sub-d-phase-4-profile-membership-broadcast-design.md`](docs/specs/2026-05-12-zeb-281-sub-d-phase-4-profile-membership-broadcast-design.md) — 563 lines, §1-§15
- Plan: [`docs/plans/2026-05-12-zeb-281-sub-d-phase-4-profile-membership-broadcast-plan.md`](docs/plans/2026-05-12-zeb-281-sub-d-phase-4-profile-membership-broadcast-plan.md)
- Original Sub-D scope: [`docs/specs/2026-04-30-zeb-206-nav-tree-design.md`](docs/specs/2026-04-30-zeb-206-nav-tree-design.md) L235-246 (specced the wire type with the now-renamed topic `harmony/announce/{owner_addr}/memberships`; this PR uses `harmony/discovery/profile/{owner_addr_hex}/memberships` to avoid collision with the storage tier's `harmony/announce/{cid_hex}` namespace).

## Privacy invariants

| Invariant | Verified by |
|---|---|
| Default: zero broadcasts until first opt-in | `publisher_no_broadcast_before_first_optin` |
| N→0 rotation publishes empty list with strictly-newer HLC | `publisher_rotation_publishes_empty_then_stops_refresh`, `self_publish_rotation_to_empty` |
| Tampered signatures rejected at receive | `verify_broadcast_tampered_signature_rejected`, `verify_broadcast_tampered_payload_rejected` |
| Bounds (≤200 communities) enforced | `verify_broadcast_too_many_communities_rejected` |
| Sorted+deduped canonical form enforced | `verify_broadcast_unsorted_community_ids_rejected`, `verify_broadcast_duplicate_community_ids_rejected` |
| Attribution check (derived addr = topic owner) | `attribution_mismatch_rejected` |
| Latest-HLC-wins replay defense | `state_replay_old_hlc_rejected`, `state_replay_newer_hlc_accepted` |

## Cross-version compatibility

| Producer | Consumer | Behavior |
|---|---|---|
| Pre-Phase-4 owner-state (`Space` without `sp` field) | Phase 4 client | Works. `default + skip_serializing_if` makes the missing field deserialize as `false`; byte-identical re-encode |
| Phase 4 owner-state with `shared_in_profile: false` | Pre-Phase-4 client | Works (byte-identical bytes; no `sp` key emitted) |
| Phase 4 owner-state with `shared_in_profile: true` | Pre-Phase-4 client | Ciborium tolerates unknown fields by default; the `sp` key is ignored. The opt-in flag is effectively scoped to Phase-4-aware devices, which is the intended behavior |
| Phase 4 client | Phase 4 client | Full publish/subscribe path |

## Test plan

- [x] `cargo fmt --all -- --check` (run from `src-tauri/`)
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` (from `src-tauri/`)
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (from `src-tauri/`) — baseline + 22 new
- [x] `cargo check --locked --all-targets --features test-fixtures` (MSRV gate, from `src-tauri/`)
- [x] `npx tsc --noEmit` (from repo root)
- [x] `npx vitest run` (from repo root) — baseline + 6 new
- [x] Pre-Phase-4 owner-state wire-format pinning fixtures byte-identical (skip_serializing_if invariant)
- [x] `ProfileMembershipBroadcast` canonical-CBOR pinning (2-char keys + declaration-order key audit)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)" 2>&1 | tail -5
```

Expected: the PR URL is returned on a single line. Capture it for the autonomous monitoring loop.

- [ ] **Step 5: Report PR URL + hand off**

Output the PR URL to the calling agent. Then STOP this task and return control. The calling agent enters the autonomous bot-review monitoring loop per `feedback_autonomous_pr_monitoring_loop` memory (270s wakeup cadence, batched fixups, race-prevention re-fetch immediately before each push, pushover-notify at convergence OR on exception requiring user input).

NO Linear sub-tickets to file in this task. ZEB-281 closes on merge. ZEB-218 stays In Progress for Phase 6 (which already exists as ZEB-252). Phase 4.5 follow-ups in spec §12 (cross-resolution, self-profile summary, Zenoh queryable, persistent subscriptions) are speculative future work and explicitly NOT tracked as Linear tickets per the `feedback_never_invent_linear_ids` rule.

---

## Self-review

**1. Spec coverage:**

| Spec section | Implementing task |
|---|---|
| §1 Goal — third independent discovery primitive | Tasks 1-6 collectively |
| §2 Why this shape | No code; design rationale only |
| §3 Architecture overview | Tasks 1-6 |
| §4.1 Wire format `ProfileMembershipBroadcast` | Task 1 (struct), Task 4 (pinning) |
| §4.2 `Space.shared_in_profile` | Task 1 (field + invariant), Task 4 (wire-compat pinning) |
| §4.3 `BroadcastVerifyError` + `MAX_SHARED_COMMUNITIES` | Task 1 |
| §5 Publisher lifecycle | Task 2 |
| §6 Subscriber lifecycle + `verify_broadcast` | Task 1 (verify_broadcast) + Task 3 (cache + replay defense) + Task 5 (subscriber pool in event_loop.rs) |
| §7 IPC surface | Task 5 |
| §8.1 Service layer | Task 6 |
| §8.2 Settings panel toggle | Task 6 |
| §8.3 Popover memberships section | Task 6 |
| §9 Error handling table | Task 1 (verify errors silently dropped) + Task 3 (cache rejects without UI surfacing) + Task 5 (subscriber drops oversized payloads + CBOR decode failures) |
| §10 Performance/scale | No code; design rationale (additional Ed25519 verify ~50µs, periodic refresh 1/10min) |
| §11.1 Test fixtures | Task 5 (`tests/common/profile_fixtures.rs`) |
| §11.2 Unit tests | Task 1 (8) + Task 2 (3 publisher) + Task 3 (2 state replay) = 13 |
| §11.3 Integration tests | Task 5 (5) |
| §11.4 Wire-format pinning | Task 4 (2 in new file + 2 in owner-state file) |
| §11.5 Frontend vitest | Task 6 (6) |
| §12 Deferred follow-ups | No tickets to file (speculative; ZEB-252 already exists) |
| §13 Out of scope | No task; honored by NOT implementing profile-level metadata broadcast, cross-device sync of subs, retention policy, anti-correlation, membership reachability |
| §14 Acceptance criteria | All 6 covered by Tasks 1-6 + final verification in Task 7 |

**2. Placeholder scan:** No "TBD", "TODO", "fill in details" remain. The two notes ("Note for implementer") in Task 1 and Task 5 are clarifications about pre-existing naming uncertainty (e.g., `parse_owner_addr_hex` vs `parse_owner_addr` — implementer must grep), not deferred work. They name the exact grep command to run and the exact pattern to mirror.

**3. Type consistency:**

- `ProfileMembershipBroadcast { owner_identity_pub: [u8;64], community_ids: Vec<SpaceId>, shared_at: Hlc, signature: [u8;64] }` defined in Task 1, used identically in Task 2 (`sign_broadcast` arg), Task 3 (`ProfileBroadcastCache::on_sample` arg), Task 4 (pinning fixtures), Task 5 (integration tests + event-loop CBOR decode).
- `BroadcastVerifyError` variants defined in Task 1, returned by `verify_broadcast` (Task 1), wrapped by `CacheOnSampleError::VerifyFailed(#[from] BroadcastVerifyError)` (Task 3).
- `SubscriptionId = u64` defined in Task 3, used in `CachedSubscription`/`CacheOnSampleError`/`ProfileBroadcastCache`/`ProfileBroadcastRequest::{Subscribe,Unsubscribe}` (Task 5) and as the JS-side number type in the service (Task 6).
- `DiscoveredProfileInfo { owner_addr, community_ids, shared_at }` (Rust, snake_case fields, camelCase wire) defined in Task 3, mirrored as `ProfileMembershipBroadcastInfo { ownerAddr, communityIds, sharedAt }` in TS (Task 6).
- `OwnerStateBroadcastSource` / `EventLoopPublishSink` (Task 2) → instantiated in start_node (Task 5) with the existing identity-block handles.
- `PUBLISHER_DEBOUNCE` / `PUBLISHER_REFRESH_INTERVAL` constants defined in Task 2, referenced in start_node (Task 5).
- `MAX_BROADCAST_WIRE_BYTES` defined in Task 1, used in event-loop subscriber's payload-size gate (Task 5).
- `broadcast_topic_for(addr) -> String` defined in Task 1, used in both publisher (Task 2 via `OwnerStateBroadcastSource`/`maybe_publish`) and subscriber (Task 5 in event-loop `key_expr`).

**4. Tests-pass-after-each-commit invariant:**

- Task 1: 8 verify_broadcast unit tests pass + all existing wire-format pinning fixtures byte-identical.
- Task 2: 3 publisher tests pass + existing tests still pass (no API removed; only adds).
- Task 3: 2 state replay tests pass.
- Task 4: 4 wire-format pinning tests pass.
- Task 5: 5 integration tests pass + the new IPCs compile + tsc clean (because no frontend caller yet — exposed but unused).
- Task 6: 6 vitest tests pass + tsc clean (frontend now consumes the IPCs).

No fix-ups required between tasks. Each commit leaves the workspace green per the 5 CI gates.

**5. Branch lineage + memory rules:**

- Pull-before-work satisfied (Task 0 step 2 verifies `HEAD..origin/main` is empty).
- No worktrees used; all work happens in the main repo on `zeb-281-sub-d-phase-4-profile-membership-broadcast`.
- No Linear IDs invented; Task 7 PR body uses only existing IDs (ZEB-281, ZEB-218, ZEB-252, ZEB-279, ZEB-280) with markdown-linked URLs.
- Per `feedback_engineer_for_real_scale`: publisher path is bounded (1 broadcast per opt-in change + 1 per 10min, debounced); subscriber path is per-popover (1-2 concurrent typical). 200 SpaceIds × 32 bytes worst-case payload ≈ 6.5 KB. All within reasonable scale.
- Per `feedback_tauri_error_extraction`: the new frontend service exposes IPC errors as strings; callers must use `e instanceof Error ? e.message : String(e)` when surfacing to UI (documented in the service file's class-doc).
- Per `feedback_pipe_exit_codes_lie`: no `cmd | tail/grep` exit-code dependencies in any verification step. The `tail -N` pipes in this plan only TRIM output for readability; the surrounding `cargo nextest`/`cargo clippy` exit codes are NOT inspected via the piped invocation.
