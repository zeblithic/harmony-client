# ZEB-341 Profile Cards Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resolve a community member's display name + status from their `owner_id` (verified via the ZEB-339 `EnrollmentCert` model) and render it in the members list, message authors, and a click-to-view profile popover — self instantly/offline, others cross-peer.

**Architecture:** A new `owner_id`-keyed, cert-verified `ProfileCardBroadcast` (sibling to `ProfileMembershipBroadcast`), published signed by device key #2, resolved through a mirror of the existing subscriber-pool + cache + IPC machinery, overlaid onto the frontend members list / message authors by a new `member-card-service`.

**Tech Stack:** Rust (Tauri 2 backend, `src-tauri/`), Svelte 5 (frontend, `src/`), canonical CBOR, ed25519-dalek, Zenoh pub/sub, `harmony_owner::certs::EnrollmentCert`, cargo-nextest, vitest.

---

## Authoritative references (implementers MAY read these)

- **Spec:** `docs/specs/2026-05-30-zeb-341-profile-cards-design.md` (commit `64cdbab`). The spec governs intent; this plan governs the build sequence.
- **Template — broadcast primitive:** `src-tauri/src/profile_broadcast.rs` — `ProfileMembershipBroadcast` (struct L51-84), `sign_broadcast` (L114), `verify_broadcast` (L135), `DiscoveredProfileInfo` (L496), `ProfileBroadcastCache` (L518, impl L545: `register` L548, `drop_subscription` L559, `get_cached` L605), `ProfileBroadcastPublisher` (L201, `spawn` L215), consts (L19-40).
- **Template — cert verification:** `src-tauri/src/community_membership.rs` — `enrolled_key_from_cert` (L1183-1207). Mirror its cert checks exactly.
- **Template — subscriber pool:** `src-tauri/src/event_loop.rs` — `ProfileBroadcastRequest` (L255), pool task (L1070-1240).
- **Template — IPC trio:** `src-tauri/src/lib.rs` — `subscribe_peer_profile` (L18541), `unsubscribe_peer_profile` (L18586), `get_cached_peer_profile` (L18610); registration in the `tauri::generate_handler!` list (L31370-31372). `NodeState` fields `profile_broadcast_cache` / `profile_broadcast_request_tx` / `profile_broadcast_next_subscription_id`.
- **Template — runtime publish wiring:** `publish_profile` IPC (`lib.rs:5436`); the ZEB-339 device-#2 key + cert are on `DmOutbox` (`community_signing_key`, `enrollment_cert`) and wired at `start_node` — search `community_signing_key_arc` / `own_enrollment_cert` in `lib.rs`.
- **Template — frontend resolution service:** `src/lib/profile-broadcast-service.ts` (subscribe/poll/cache IPC client) and its consumer in `ProfilePopover.svelte` / `App.svelte`.
- **Render sites:** `src/lib/components/MemberRow.svelte:86`, `src/lib/components/ChannelMessageFeed.svelte:350`, `src/lib/components/ProfilePopover.svelte`, `src/lib/profile-service.ts` (local profile localStorage).

## HARD RULES (every coding task)

- **TDD:** failing test first → run it red → minimal impl → run green → commit. Each task ends with a commit.
- **Backend gates** (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (a scoped `-E 'test(...)'` subset is fine per-task; full sweep in the final task). `HARMONY_LARGE_TESTS=1` folder-ingest + MSRV `cargo check` only in the final task.
- **Frontend gates** (from repo root, `npx` NOT pnpm): `npx tsc --noEmit`; `npx vitest run` (scope per-task with a path; full run final task).
- **COMMIT BEFORE the long gate.** `timeout`/`gtimeout` are NOT on this macOS box — rely on the Bash tool's own 600000ms timeout. If any single gate exceeds ~10-min wall-clock, report `DONE_WITH_CONCERNS` with the hang point; do NOT silently stall.
- **Pipe exit codes lie:** use `set -o pipefail` or `${PIPESTATUS[0]}` whenever piping cargo through `tail`/`grep`.
- **Canonical CBOR:** `BTreeMap`/`BTreeSet` only where maps are involved; new serde field codes are exactly 2 chars.
- **NEVER worktrees** — work on the existing `zeb-341-profile-cards` branch in the main repo.
- **macOS XprotectService** is mitigated on this machine; if a cold nextest hangs >10 min, document it in `DONE_WITH_CONCERNS`.

## File map

| File | New/Mod | Responsibility |
|---|---|---|
| `src-tauri/src/profile_card_broadcast.rs` | **New** | `ProfileCardBroadcast` wire type, `sign_card`, `verify_card`, `CardVerifyError`, `DiscoveredCard`/`DiscoveredCardInfo`, `ProfileCardCache`, `ProfileCardPublisher`. |
| `src-tauri/tests/wire_format_profile_card_fixtures.rs` | **New** | Pin canonical CBOR bytes of `ProfileCardBroadcast`. |
| `src-tauri/tests/profile_card_cross_peer_integration.rs` | **New** | Cross-peer e2e: owner A publishes, owner B resolves. |
| `src-tauri/src/event_loop.rs` | Mod | `ProfileCardRequest` enum + subscriber-pool task (mirror L1070-1240). |
| `src-tauri/src/lib.rs` | Mod | 3 IPCs (`subscribe_member_card`/`get_cached_member_card`/`unsubscribe_member_card`), `NodeState` card-cache/request-tx fields, publisher spawn at `start_node`, owner_id-card publish in `publish_profile`, handler registration. |
| `src-tauri/src/community_membership.rs` | Mod (light) | Optional: extract a shared cert→device-key helper reused by `enrolled_key_from_cert` + `verify_card`. |
| `src/lib/member-card-service.ts` | **New** | Eager subscribe per visible `owner_id`, reactive `Map<ownerIdHex,{displayName,statusText}>`, self-seed from local profile. |
| `src/lib/components/MemberRow.svelte` | Mod | Populate `member.displayName` from the card map. |
| `src/lib/components/ChannelMessageFeed.svelte` | Mod | Resolve `msg.author` through the card map. |
| `src/lib/components/ProfilePopover.svelte` | Mod | `owner_id`-card variant (name, status, copyable owner_id, role/power; omit shared-communities). |
| `src/App.svelte` + members/channel wiring | Mod | Open popover on member/author click; drive the card-service lifecycle. |

---

## Task 0: Pre-flight baseline (NO commit)

**Purpose:** capture green baseline + confirm the API surfaces this plan assumes.

- [ ] **Step 1: Confirm branch + clean tree**

Run: `git -C /Users/zeblith/work/zeblithic/harmony-client branch --show-current && git status -s`
Expected: `zeb-341-profile-cards`, clean (only the committed spec/plan).

- [ ] **Step 2: Baseline backend tests (scoped, fast)**

Run from `src-tauri/`: `cargo nextest run --locked --features test-fixtures -E 'test(profile_broadcast) or test(community_membership) or test(profile)' 2>&1 | tail -20; echo EXIT=${PIPESTATUS[0]}`
Expected: pass (note any pre-existing orphan failures — transport/port flakes like `zenoh_iroh_*`, `rename_content_integration` port-4242 — these are NOT blocking; record them).

- [ ] **Step 3: Baseline frontend**

Run from repo root: `npx tsc --noEmit && npx vitest run src/lib 2>&1 | tail -15`
Expected: pass.

- [ ] **Step 4: Confirm API reachability (read, don't edit)**

Verify these exist and match the plan's assumptions; if any differ, report `DONE_WITH_CONCERNS` with the actual shape before proceeding:
- `harmony_owner::certs::EnrollmentCert` fields: `owner_id: [u8;16]`, `device_pubkeys.classical.ed25519_verify: [u8;32]`, `issuer: EnrollmentIssuer` (variant `Master`), method `verify()`. (Same surface ZEB-339 uses in `community_membership::enrolled_key_from_cert`, L1183-1207 — read it.)
- `DmOutbox` carries `community_signing_key` (device #2 `Arc<SigningKey>`) + `enrollment_cert: EnrollmentCert`; `start_node` loads them (grep `community_signing_key_arc`, `own_enrollment_cert` in `lib.rs`).
- `profile_broadcast.rs` `ProfileBroadcastCache` (L518) + `event_loop.rs` `ProfileBroadcastRequest` pool (L1070) are the templates to mirror.
- `owner_state_types::serialize_bytes_as_bstr` / `deserialize_bytes_from_bstr` exist (used by `ProfileMembershipBroadcast`).

No commit (read-only baseline).

---

## Task 1: Self-first slice — frontend `member-card-service` + self-seed (frontend only)

**Goal:** the viewer's OWN member row and own messages show the local display name immediately, zero network. Delivers visible value before any backend lands.

**Files:**
- Create: `src/lib/member-card-service.ts`
- Create: `src/lib/__tests__/member-card-service.test.ts`
- Modify: `src/lib/components/MemberRow.svelte` (populate `displayName`)
- Modify: `src/lib/components/ChannelMessageFeed.svelte` (resolve author)
- Modify: members-panel + channel-view wiring + `App.svelte` (instantiate service, pass self owner_id + local profile)

- [ ] **Step 1: Read the render sites + local profile source**

Read `MemberRow.svelte` (esp. L86 `member.displayName ?? member.address.slice(0,8)`), `ChannelMessageFeed.svelte` (esp. L350 `msg.author.slice(0,8)`), `profile-service.ts` (local profile `{displayName, statusText}`), and how the frontend learns its own community `owner_id` (grep `owner_id`, `selfOwner`, `ownerId`, the owner-state view that backs the Devices "OWNER IDENTITY 685e·4ba7"). Confirm a self `owner_id` hex is available client-side.

- [ ] **Step 2: Write the failing test**

`src/lib/__tests__/member-card-service.test.ts`:

```ts
import { describe, it, expect } from 'vitest';
import { MemberCardService } from '../member-card-service';

describe('MemberCardService self-seed', () => {
  it('resolves the self owner_id to the local profile name/status synchronously', () => {
    const svc = new MemberCardService();
    svc.seedSelf('685e4ba76a8fde38ecbd2ff5c138df8c', { displayName: 'Jake (Koya Dev)', statusText: 'building' });
    expect(svc.resolve('685e4ba76a8fde38ecbd2ff5c138df8c')).toEqual({
      displayName: 'Jake (Koya Dev)',
      statusText: 'building',
    });
  });

  it('returns undefined for an unknown owner_id (caller falls back to hash prefix)', () => {
    const svc = new MemberCardService();
    expect(svc.resolve('deadbeefdeadbeefdeadbeefdeadbeef')).toBeUndefined();
  });

  it('seedSelf overwrites the same owner_id on re-seed (profile edited)', () => {
    const svc = new MemberCardService();
    svc.seedSelf('aa'.repeat(16), { displayName: 'old', statusText: '' });
    svc.seedSelf('aa'.repeat(16), { displayName: 'new', statusText: 'hi' });
    expect(svc.resolve('aa'.repeat(16))).toEqual({ displayName: 'new', statusText: 'hi' });
  });
});
```

- [ ] **Step 3: Run red**

Run: `npx vitest run src/lib/__tests__/member-card-service.test.ts 2>&1 | tail -15`
Expected: FAIL (module not found).

- [ ] **Step 4: Implement the service (self-seed only this task)**

`src/lib/member-card-service.ts`:

```ts
export interface ResolvedCard {
  displayName: string;
  statusText: string;
}

/**
 * Resolves a community member's owner_id -> {displayName, statusText}.
 * Task 1: self-seed only (local profile, synchronous, no network).
 * Task 8 extends this to subscribe to peers' owner_id card broadcasts.
 *
 * owner_id keys are lowercase 32-char hex (matches MemberInfoDto.addr and
 * msg.author from the backend).
 */
export class MemberCardService {
  private cards: Map<string, ResolvedCard> = new Map();

  /** Seed (or overwrite) the self owner_id from the local profile. Synchronous. */
  seedSelf(ownerIdHex: string, profile: ResolvedCard): void {
    this.cards.set(ownerIdHex.toLowerCase(), { ...profile });
  }

  /** Resolve an owner_id to its card, or undefined if unresolved. */
  resolve(ownerIdHex: string): ResolvedCard | undefined {
    return this.cards.get(ownerIdHex.toLowerCase());
  }
}
```

- [ ] **Step 5: Run green**

Run: `npx vitest run src/lib/__tests__/member-card-service.test.ts 2>&1 | tail -15`
Expected: PASS (3/3).

- [ ] **Step 6: Wire the overlay into MemberRow + ChannelMessageFeed**

Make a single `MemberCardService` instance available to the members panel + channel view (instantiate in `App.svelte` or the community view container; seed self on mount + whenever the local profile saves). Pass a resolver fn (or the reactive map) down so:
- `MemberRow.svelte:86` becomes `member.displayName ?? cardResolve(member.address)?.displayName ?? member.address.slice(0, 8)`.
- `ChannelMessageFeed.svelte:350` author becomes `cardResolve(msg.author)?.displayName ?? msg.author.slice(0, 8)`.

Keep changes minimal + reactive (Svelte 5 runes — the resolver must re-read when the map updates; use a `$state`/`$derived` map so Task 8's async updates re-render). Seed self from `profile-service` on load + on profile-save (hook the existing `handleProfileSave` in `App.svelte`).

- [ ] **Step 7: Frontend gates + manual sanity**

Run from repo root: `npx tsc --noEmit && npx vitest run src/lib 2>&1 | tail -15`
Expected: pass. (Manual: in `cargo tauri dev` your own member row + messages should now show your display name instead of the hash — note this in the commit body.)

- [ ] **Step 8: Commit**

```bash
git add src/lib/member-card-service.ts src/lib/__tests__/member-card-service.test.ts src/lib/components/MemberRow.svelte src/lib/components/ChannelMessageFeed.svelte src/App.svelte
git commit -m "feat(zeb-341): self-first member-card-service + overlay (own name renders offline)"
```

---

## Task 2: `ProfileCardBroadcast` wire type + `sign_card` + canonical-CBOR fixture

**Files:**
- Create: `src-tauri/src/profile_card_broadcast.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod profile_card_broadcast;` near the other `mod` decls)
- Create: `src-tauri/tests/wire_format_profile_card_fixtures.rs`

- [ ] **Step 1: Write the failing unit test (in-module)**

In `profile_card_broadcast.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn sign_card_round_trips_and_signature_verifies_under_device_key() {
    use ed25519_dalek::{Signer, Verifier};
    let owner = crate::community_membership::mint_test_owner(0x41); // TestOwner{owner, device_key, cert}
    let card = sign_card(
        &owner.device_key,
        owner.owner.0,
        "Jake (Koya Dev)".into(),
        "building".into(),
        owner.cert.clone(),
        Hlc { wall_ms: 1_000, logical: 0, device_id: "dev".into() },
    )
    .expect("sign");
    assert_eq!(card.owner_id, owner.owner.0);
    assert_eq!(card.display_name, "Jake (Koya Dev)");
    // signature verifies under the device key over canonical CBOR with sig zeroed
    let mut for_sig = card.clone();
    for_sig.signature = [0u8; 64];
    let bytes = crate::owner_state_crypto::canonical_cbor_encode(&for_sig).unwrap();
    owner
        .device_key
        .verifying_key()
        .verify(&bytes, &ed25519_dalek::Signature::from_bytes(&card.signature))
        .expect("sig verifies");
}

#[test]
fn sign_card_rejects_overlong_name_and_status() {
    let owner = crate::community_membership::mint_test_owner(0x42);
    let long = "x".repeat(MAX_DISPLAY_NAME_BYTES + 1);
    let hlc = Hlc { wall_ms: 1, logical: 0, device_id: "d".into() };
    assert!(matches!(
        sign_card(&owner.device_key, owner.owner.0, long, "ok".into(), owner.cert.clone(), hlc.clone()),
        Err(CardError::DisplayNameTooLong)
    ));
    let longstatus = "y".repeat(MAX_STATUS_TEXT_BYTES + 1);
    assert!(matches!(
        sign_card(&owner.device_key, owner.owner.0, "ok".into(), longstatus, owner.cert, hlc),
        Err(CardError::StatusTextTooLong)
    ));
}

#[test]
fn card_topic_for_is_owner_id_hex() {
    let owner_id = [0xABu8; 16];
    assert_eq!(card_topic_for(&owner_id), format!("harmony/discovery/profile/owner/{}/card", hex::encode(owner_id)));
}
```

- [ ] **Step 2: Run red**

Run from `src-tauri/`: `cargo nextest run --locked --features test-fixtures -E 'test(profile_card_broadcast)' 2>&1 | tail -20; echo EXIT=${PIPESTATUS[0]}`
Expected: FAIL (module/symbols missing).

- [ ] **Step 3: Implement the wire type + sign_card**

`src-tauri/src/profile_card_broadcast.rs`:

```rust
//! ZEB-341 — owner_id-keyed, EnrollmentCert-verified profile card broadcast.
//! Sibling to `profile_broadcast.rs` (ProfileMembershipBroadcast). Carries a
//! peer's display name + status, bound to their harmony-owner `owner_id` via
//! the ZEB-339 cert model. Spec: docs/specs/2026-05-30-zeb-341-profile-cards-design.md

use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use crate::owner_state_types::Hlc;
use ed25519_dalek::{Signature, Signer, SigningKey};
use harmony_owner::certs::{EnrollmentCert, EnrollmentIssuer};
use serde::{Deserialize, Serialize};

/// Full topic: `{PREFIX}{owner_id_hex}/card` (owner_id_hex = lowercase 32-char hex of [u8;16]).
pub const PROFILE_CARD_TOPIC_PREFIX: &str = "harmony/discovery/profile/owner/";
pub const MAX_DISPLAY_NAME_BYTES: usize = 64;
pub const MAX_STATUS_TEXT_BYTES: usize = 128;
/// Wire-size sanity bound before CBOR decode (cert ~200B + bounded strings + framing).
#[allow(dead_code)]
pub const MAX_CARD_WIRE_BYTES: usize = 4_096;

pub fn card_topic_for(owner_id: &[u8; 16]) -> String {
    format!("{PROFILE_CARD_TOPIC_PREFIX}{}/card", hex::encode(owner_id))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCardBroadcast {
    #[serde(
        rename = "oi",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub owner_id: [u8; 16],
    #[serde(rename = "dn")]
    pub display_name: String,
    #[serde(rename = "st")]
    pub status_text: String,
    #[serde(rename = "en")]
    pub enrollment: EnrollmentCert,
    #[serde(rename = "sa")]
    pub shared_at: Hlc,
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub signature: [u8; 64],
}

impl CanonicalPayloadSealed for ProfileCardBroadcast {}
impl CanonicalPayload for ProfileCardBroadcast {}

#[derive(Debug, thiserror::Error)]
pub enum CardError {
    #[error("display_name exceeds {MAX_DISPLAY_NAME_BYTES} bytes")]
    DisplayNameTooLong,
    #[error("status_text exceeds {MAX_STATUS_TEXT_BYTES} bytes")]
    StatusTextTooLong,
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] CryptoError),
}

/// Build + Ed25519-sign a card over canonical CBOR with `signature` zeroed.
/// `signer` MUST be the enrolled device key #2 whose pub == `enrollment.device_pubkeys.classical.ed25519_verify`.
pub fn sign_card(
    signer: &SigningKey,
    owner_id: [u8; 16],
    display_name: String,
    status_text: String,
    enrollment: EnrollmentCert,
    shared_at: Hlc,
) -> Result<ProfileCardBroadcast, CardError> {
    if display_name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(CardError::DisplayNameTooLong);
    }
    if status_text.len() > MAX_STATUS_TEXT_BYTES {
        return Err(CardError::StatusTextTooLong);
    }
    let mut card = ProfileCardBroadcast {
        owner_id,
        display_name,
        status_text,
        enrollment,
        shared_at,
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&card)?;
    card.signature = signer.sign(&bytes).to_bytes();
    Ok(card)
}
```

Add `mod profile_card_broadcast;` to `lib.rs` (next to `mod profile_broadcast;`). NOTE: import `EnrollmentIssuer` is used in Task 3; if clippy flags it unused now, add it in Task 3 instead.

- [ ] **Step 4: Run green**

Run from `src-tauri/`: `cargo nextest run --locked --features test-fixtures -E 'test(profile_card_broadcast)' 2>&1 | tail -20; echo EXIT=${PIPESTATUS[0]}`
Expected: PASS (3/3).

- [ ] **Step 5: Wire-format fixture**

`src-tauri/tests/wire_format_profile_card_fixtures.rs` — pin canonical bytes so the format can't silently drift:

```rust
//! ZEB-341: pin the canonical CBOR wire format of ProfileCardBroadcast.
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::Hlc;
use harmony_app::profile_card_broadcast::ProfileCardBroadcast;

/// Deterministic card (fixed cert via mint_test_owner deterministic seed) →
/// stable canonical CBOR prefix. Asserts the 2-char field codes are present
/// (oi/dn/st/en/sa/sg) and the map header is stable.
#[test]
fn profile_card_canonical_cbor_pins_field_codes() {
    let owner = harmony_app::community_membership::mint_test_owner(0x7C);
    let card = ProfileCardBroadcast {
        owner_id: owner.owner.0,
        display_name: "Ann".into(),
        status_text: "hi".into(),
        enrollment: owner.cert,
        shared_at: Hlc { wall_ms: 1234, logical: 0, device_id: "d".into() },
        signature: [0u8; 64],
    };
    let bytes = canonical_cbor_encode(&card).expect("encode");
    // Map with 6 entries (0xA6) and the 2-char text keys appear in canonical order.
    assert_eq!(bytes[0], 0xA6, "expected 6-entry CBOR map header");
    for code in ["oi", "dn", "st", "en", "sa", "sg"] {
        let needle = [0x62, code.as_bytes()[0], code.as_bytes()[1]]; // text(2) + 2 chars
        assert!(
            bytes.windows(3).any(|w| w == needle),
            "missing field code {code} in canonical CBOR"
        );
    }
}

/// Round-trip decode equals the original (serde stability).
#[test]
fn profile_card_round_trips_through_canonical_cbor() {
    let owner = harmony_app::community_membership::mint_test_owner(0x7D);
    let card = ProfileCardBroadcast {
        owner_id: owner.owner.0,
        display_name: "Bo".into(),
        status_text: "".into(),
        enrollment: owner.cert,
        shared_at: Hlc { wall_ms: 9, logical: 1, device_id: "x".into() },
        signature: [0x11; 64],
    };
    let bytes = canonical_cbor_encode(&card).expect("encode");
    let back: ProfileCardBroadcast = ciborium::from_reader(&bytes[..]).expect("decode");
    assert_eq!(back, card);
}
```

(If `ciborium` isn't the in-repo decoder, use the same decode path `profile_broadcast.rs` tests use — grep `from_reader` / `canonical_cbor_decode` in tests and match it.)

- [ ] **Step 6: Run fixture + commit**

Run from `src-tauri/`: `cargo nextest run --locked --features test-fixtures -E 'test(profile_card)' 2>&1 | tail -20; echo EXIT=${PIPESTATUS[0]}` → PASS. Then `cargo fmt --all` and:

```bash
git add src-tauri/src/profile_card_broadcast.rs src-tauri/src/lib.rs src-tauri/tests/wire_format_profile_card_fixtures.rs
git commit -m "feat(zeb-341): ProfileCardBroadcast wire type + sign_card + canonical-CBOR fixture"
```

---

## Task 3: `verify_card` (cert model) + negative tests

**Files:**
- Modify: `src-tauri/src/profile_card_broadcast.rs`

- [ ] **Step 1: Write the failing tests**

Add to the in-module tests:

```rust
#[test]
fn verify_card_accepts_a_well_formed_card() {
    let owner = crate::community_membership::mint_test_owner(0x50);
    let card = sign_card(&owner.device_key, owner.owner.0, "Ann".into(), "hi".into(),
        owner.cert.clone(), Hlc { wall_ms: 1, logical: 0, device_id: "d".into() }).unwrap();
    assert_eq!(verify_card(&card).unwrap(), owner.owner.0);
}

#[test]
fn verify_card_rejects_owner_mismatch() {
    // Card claims owner X but carries owner Y's cert.
    let x = crate::community_membership::mint_test_owner(0x51);
    let y = crate::community_membership::mint_test_owner(0x52);
    let mut card = sign_card(&y.device_key, y.owner.0, "n".into(), "".into(),
        y.cert.clone(), Hlc { wall_ms: 1, logical: 0, device_id: "d".into() }).unwrap();
    card.owner_id = x.owner.0; // tamper: now owner_id != cert.owner_id
    assert!(matches!(verify_card(&card), Err(CardVerifyError::EnrollmentOwnerMismatch)));
}

#[test]
fn verify_card_rejects_tampered_signature() {
    let owner = crate::community_membership::mint_test_owner(0x53);
    let mut card = sign_card(&owner.device_key, owner.owner.0, "n".into(), "".into(),
        owner.cert.clone(), Hlc { wall_ms: 1, logical: 0, device_id: "d".into() }).unwrap();
    card.signature[0] ^= 0x01;
    assert!(matches!(verify_card(&card), Err(CardVerifyError::SignatureInvalid)));
}

#[test]
fn verify_card_rejects_oversize_fields() {
    let owner = crate::community_membership::mint_test_owner(0x54);
    let mut card = sign_card(&owner.device_key, owner.owner.0, "n".into(), "".into(),
        owner.cert.clone(), Hlc { wall_ms: 1, logical: 0, device_id: "d".into() }).unwrap();
    card.display_name = "z".repeat(MAX_DISPLAY_NAME_BYTES + 1);
    assert!(matches!(verify_card(&card), Err(CardVerifyError::DisplayNameTooLong)));
}
```

(If `mint_test_owner` can produce a Quorum-issuer cert, add a `verify_card_rejects_non_master_issuer` test; otherwise note it's covered structurally by the issuer gate and skip — do NOT fabricate an API.)

- [ ] **Step 2: Run red**

Run from `src-tauri/`: `cargo nextest run --locked --features test-fixtures -E 'test(verify_card)' 2>&1 | tail -20; echo EXIT=${PIPESTATUS[0]}`
Expected: FAIL.

- [ ] **Step 3: Implement `verify_card` + `CardVerifyError`**

Mirror `community_membership::enrolled_key_from_cert` (L1183-1207). Add to `profile_card_broadcast.rs`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum CardVerifyError {
    #[error("display_name exceeds {MAX_DISPLAY_NAME_BYTES} bytes")]
    DisplayNameTooLong,
    #[error("status_text exceeds {MAX_STATUS_TEXT_BYTES} bytes")]
    StatusTextTooLong,
    #[error("enrollment cert invalid")]
    EnrollmentCertInvalid,
    #[error("cert.owner_id does not match card.owner_id")]
    EnrollmentOwnerMismatch,
    #[error("Ed25519 signature verification failed")]
    SignatureInvalid,
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] CryptoError),
}

/// Verify a card end-to-end. Returns the bound `owner_id` on success.
/// Subscriber-side attribution (returned owner_id == topic owner_id) is the
/// CALLER's responsibility (see the event-loop pool, Task 6).
pub fn verify_card(card: &ProfileCardBroadcast) -> Result<[u8; 16], CardVerifyError> {
    // (1) bounds
    if card.display_name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(CardVerifyError::DisplayNameTooLong);
    }
    if card.status_text.len() > MAX_STATUS_TEXT_BYTES {
        return Err(CardVerifyError::StatusTextTooLong);
    }
    // (2) cert validity + Master-issuer-only gate (spec §10 / ZEB-339)
    card.enrollment
        .verify()
        .map_err(|_| CardVerifyError::EnrollmentCertInvalid)?;
    if !matches!(card.enrollment.issuer, EnrollmentIssuer::Master { .. }) {
        return Err(CardVerifyError::EnrollmentCertInvalid);
    }
    // (3) owner binding
    if card.enrollment.owner_id != card.owner_id {
        return Err(CardVerifyError::EnrollmentOwnerMismatch);
    }
    // (4) device signer key + (5) verify_strict over canonical CBOR (sig zeroed)
    let device_ed25519 = card.enrollment.device_pubkeys.classical.ed25519_verify;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&device_ed25519)
        .map_err(|_| CardVerifyError::SignatureInvalid)?;
    let mut for_sig = card.clone();
    for_sig.signature = [0u8; 64];
    let bytes = canonical_cbor_encode(&for_sig)?;
    vk.verify_strict(&bytes, &Signature::from_bytes(&card.signature))
        .map_err(|_| CardVerifyError::SignatureInvalid)?;
    Ok(card.owner_id)
}
```

(Use `ed25519_dalek::Verifier::verify_strict`; ensure the import is present. If extracting a shared cert-check helper into `community_membership.rs` is clean, do it and call it here; otherwise replicate as above — do NOT block on the refactor.)

- [ ] **Step 4: Run green + commit**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(verify_card) or test(profile_card)' 2>&1 | tail -20; echo EXIT=${PIPESTATUS[0]}` → PASS. `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -8; echo EXIT=${PIPESTATUS[0]}` → 0. `cargo fmt --all`. Then:

```bash
git add src-tauri/src/profile_card_broadcast.rs
git commit -m "feat(zeb-341): verify_card cert-model verification + negative tests"
```

---

## Task 4: `ProfileCardCache` + `DiscoveredCard`/`DiscoveredCardInfo`

**Files:**
- Modify: `src-tauri/src/profile_card_broadcast.rs`

Mirror `profile_broadcast.rs` `DiscoveredProfileInfo` (L496), `ProfileBroadcastCache` (L518), impl (L545: `register`/`drop_subscription`/`get_cached`) and its verified-insert path. Read that block first.

- [ ] **Step 1: Failing test**

```rust
#[tokio::test]
async fn card_cache_register_insert_get_roundtrip() {
    let cache = ProfileCardCache::default();
    let owner = crate::community_membership::mint_test_owner(0x60);
    cache.register(1, owner.owner.0).await;
    assert_eq!(cache.get_cached(1).await, None, "no broadcast yet -> None");
    let card = sign_card(&owner.device_key, owner.owner.0, "Cy".into(), "yo".into(),
        owner.cert.clone(), Hlc { wall_ms: 5, logical: 0, device_id: "d".into() }).unwrap();
    cache.insert_verified(1, &card).await;
    let got = cache.get_cached(1).await.expect("cached");
    assert_eq!(got.display_name, "Cy");
    assert_eq!(got.status_text, "yo");
    assert_eq!(got.owner_id_hex, hex::encode(owner.owner.0));
    cache.drop_subscription(1).await;
    assert_eq!(cache.get_cached(1).await, None);
}

#[tokio::test]
async fn card_cache_newer_hlc_wins() {
    let cache = ProfileCardCache::default();
    let owner = crate::community_membership::mint_test_owner(0x61);
    cache.register(2, owner.owner.0).await;
    let older = sign_card(&owner.device_key, owner.owner.0, "old".into(), "".into(),
        owner.cert.clone(), Hlc { wall_ms: 10, logical: 0, device_id: "d".into() }).unwrap();
    let newer = sign_card(&owner.device_key, owner.owner.0, "new".into(), "".into(),
        owner.cert.clone(), Hlc { wall_ms: 20, logical: 0, device_id: "d".into() }).unwrap();
    cache.insert_verified(2, &newer).await;
    cache.insert_verified(2, &older).await; // stale -> ignored
    assert_eq!(cache.get_cached(2).await.unwrap().display_name, "new");
}
```

- [ ] **Step 2: Run red**, then **Step 3: implement**:

```rust
use std::collections::HashMap;
use tokio::sync::Mutex;

pub type SubscriptionId = u64;

/// IPC/cache DTO (camelCase for the frontend via serde rename).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredCardInfo {
    #[serde(rename = "ownerIdHex")]
    pub owner_id_hex: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "statusText")]
    pub status_text: String,
}

#[derive(Debug, Clone)]
struct CachedCard {
    owner_id: [u8; 16],
    display_name: String,
    status_text: String,
    shared_at: Hlc,
}

#[derive(Default)]
pub struct ProfileCardCache {
    // subscription_id -> (expected owner_id, latest verified card or None)
    slots: Mutex<HashMap<SubscriptionId, (/*expected*/ [u8; 16], Option<CachedCard>)>>,
}

impl ProfileCardCache {
    pub async fn register(&self, sub: SubscriptionId, expected_owner: [u8; 16]) {
        self.slots.lock().await.entry(sub).or_insert((expected_owner, None));
    }
    pub async fn drop_subscription(&self, sub: SubscriptionId) {
        self.slots.lock().await.remove(&sub);
    }
    /// Insert a VERIFIED card (caller ran verify_card + attribution check).
    /// Newer-HLC-wins; ignores a card whose owner_id != the slot's expected owner.
    pub async fn insert_verified(&self, sub: SubscriptionId, card: &ProfileCardBroadcast) {
        let mut g = self.slots.lock().await;
        if let Some((expected, slot)) = g.get_mut(&sub) {
            if card.owner_id != *expected {
                return; // attribution mismatch defense-in-depth
            }
            let newer = match slot {
                Some(existing) => card.shared_at > existing.shared_at,
                None => true,
            };
            if newer {
                *slot = Some(CachedCard {
                    owner_id: card.owner_id,
                    display_name: card.display_name.clone(),
                    status_text: card.status_text.clone(),
                    shared_at: card.shared_at.clone(),
                });
            }
        }
    }
    pub async fn get_cached(&self, sub: SubscriptionId) -> Option<DiscoveredCardInfo> {
        let g = self.slots.lock().await;
        let (_, slot) = g.get(&sub)?;
        let c = slot.as_ref()?;
        Some(DiscoveredCardInfo {
            owner_id_hex: hex::encode(c.owner_id),
            display_name: c.display_name.clone(),
            status_text: c.status_text.clone(),
        })
    }
}
```

(Confirm `Hlc` implements `PartialOrd`/`Ord` for `shared_at > existing.shared_at`; `ProfileMembershipBroadcast`'s HLC newer-wins logic shows the comparison idiom — match it. If `Hlc` isn't `Ord`, compare `(wall_ms, logical)` tuples as that code does.)

- [ ] **Step 4: Run green + clippy + fmt + commit**

```bash
git add src-tauri/src/profile_card_broadcast.rs
git commit -m "feat(zeb-341): ProfileCardCache (verified-insert, newer-HLC-wins) + DiscoveredCardInfo"
```

---

## Task 5: Publisher — sign with device #2 + cert, publish on save/startup/refresh

**Files:**
- Modify: `src-tauri/src/profile_card_broadcast.rs` (`ProfileCardPublisher`, mirror `ProfileBroadcastPublisher` L201-end)
- Modify: `src-tauri/src/lib.rs` (`publish_profile` adds the owner_id-card publish; spawn publisher at `start_node`)

- [ ] **Step 1: Failing test (publisher emits a verifiable card)**

In `profile_card_broadcast.rs` tests, use a `MockSink` mirroring `profile_broadcast.rs`'s test sink:

```rust
#[tokio::test]
async fn publisher_emits_a_card_that_verifies() {
    // Mock sink captures (topic, payload). Mirror profile_broadcast.rs MockSink.
    let owner = crate::community_membership::mint_test_owner(0x70);
    let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<(String, Vec<u8>)>::new()));
    let sink = std::sync::Arc::new(CapturingSink { out: captured.clone() });
    publish_card_once(
        &owner.device_key, owner.owner.0, "Pat".into(), "afk".into(), owner.cert.clone(),
        Hlc { wall_ms: 1, logical: 0, device_id: "d".into() }, sink.as_ref(),
    ).await.expect("publish");
    let out = captured.lock().await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, card_topic_for(&owner.owner.0));
    let decoded: ProfileCardBroadcast = ciborium::from_reader(&out[0].1[..]).unwrap();
    assert_eq!(verify_card(&decoded).unwrap(), owner.owner.0);
}
```

- [ ] **Step 2: Run red**, then **Step 3: implement**. Provide a small `publish_card_once(signer, owner_id, name, status, cert, hlc, sink)` helper (build via `sign_card`, encode, `sink.publish(card_topic_for(&owner_id), bytes)`), and a `ProfileCardPublisher` that mirrors `ProfileBroadcastPublisher::spawn` (debounce `PUBLISHER_DEBOUNCE` 2s, refresh `PUBLISHER_REFRESH_INTERVAL` 600s, `notify()` on profile change, HLC-monotonic via the injected source). Reuse the `ProfileBroadcastPublishSink` trait from `profile_broadcast.rs` (import it) rather than redefining. Read `profile_broadcast.rs` L201-end and copy the structure.

- [ ] **Step 4: Wire into `lib.rs`**

- In `publish_profile` (`lib.rs:5436`): after the existing Reticulum-topic publish, ALSO build + publish the owner_id card. Source the device key #2 + cert from the already-loaded runtime (`DmOutbox.community_signing_key` + `enrollment_cert`, or the `community_signing_key_arc` + `own_enrollment_cert` held in `NodeState`/`start_node` — grep for them). Use the self `owner_id` (`loaded.state.owner_id`). Enforce bounds (the `sign_card` call returns `CardError::*` → map to a `String` IPC error). Bump a monotonic HLC.
- At `start_node`: spawn the `ProfileCardPublisher` (or, minimally, publish once on startup + rely on save-triggered publishes — pick the lower-risk path and note it). If the periodic refresh adds risk this cut, a startup publish + publish-on-save is acceptable; document the choice in the commit.

- [ ] **Step 5: Gates + commit**

`cargo nextest -E 'test(profile_card) or test(publish_profile)'` green, clippy 0, fmt. Commit:

```bash
git add src-tauri/src/profile_card_broadcast.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-341): publish owner_id profile card (device #2 + cert) on save/startup"
```

---

## Task 6: Subscriber pool + 3 IPCs

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (`ProfileCardRequest` enum + pool task — mirror `ProfileBroadcastRequest` L255 + pool L1070-1240)
- Modify: `src-tauri/src/lib.rs` (`NodeState` fields, 3 IPC handlers, handler registration)

- [ ] **Step 1: Failing test (IPC-level, owner-not-loaded + roundtrip via cache)**

A focused test is awkward for the full Zenoh pool; instead unit-test the cache wiring + add the cross-peer e2e in Task 7. For this task write a test asserting the 3 IPCs are registered and `subscribe_member_card` rejects bad hex:

```rust
// in lib.rs #[cfg(test)] (or a tests/ file) — mirror existing IPC tests
#[test]
fn member_card_owner_id_hex_parses_16_bytes() {
    let id = harmony_app::library_directory::parse_owner_addr_hex(&"ab".repeat(16)).unwrap();
    assert_eq!(id.0.len(), 16);
    assert!(harmony_app::library_directory::parse_owner_addr_hex("zz").is_err());
}
```

(The substantive behavioral coverage is the Task 7 cross-peer e2e; this task's correctness is "compiles + registered + mirrors the proven pool".)

- [ ] **Step 2: Implement**

Mirror exactly:
- `event_loop.rs`: add `ProfileCardRequest::{Subscribe{subscription_id, owner_id:[u8;16]}, Unsubscribe{subscription_id}}` (copy `ProfileBroadcastRequest` L255). Add a pool task copying L1070-1240 but: topic via `profile_card_broadcast::card_topic_for(&owner_id)`; wire-size bound `MAX_CARD_WIRE_BYTES`; decode `ProfileCardBroadcast`; **`verify_card(&card)` then attribution check (returned owner_id == subscribed owner_id) before `cache.insert_verified`** (drop + `warn!` on failure). Add `profile_card_cache: Option<Arc<ProfileCardCache>>` + `profile_card_request_rx: Option<Receiver<ProfileCardRequest>>` to the event-loop params next to the existing profile-broadcast ones.
- `lib.rs` `NodeState`: add `profile_card_cache: Option<Arc<ProfileCardCache>>`, `profile_card_request_tx: Option<Sender<ProfileCardRequest>>`, `profile_card_next_subscription_id: Arc<AtomicU64>` (mirror the profile-broadcast fields). Initialize them where the profile-broadcast equivalents are initialized at `start_node`, and pass the rx into the event loop.
- `lib.rs` IPCs — copy `subscribe_peer_profile`/`unsubscribe_peer_profile`/`get_cached_peer_profile` (L18541-18623) verbatim-with-renames:

```rust
#[tauri::command]
async fn subscribe_member_card(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    owner_id_hex: String,
) -> Result<u64, String> { /* mirror subscribe_peer_profile: parse_owner_addr_hex -> [u8;16], register, send ProfileCardRequest::Subscribe, rollback on send err */ }

#[tauri::command]
async fn unsubscribe_member_card(/* … */) -> Result<(), String> { /* mirror */ }

#[tauri::command]
async fn get_cached_member_card(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    subscription_id: u64,
) -> Result<Option<crate::profile_card_broadcast::DiscoveredCardInfo>, String> { /* mirror get_cached_peer_profile */ }
```

Register all three in the `tauri::generate_handler!` list next to the profile-broadcast trio (L31370-31372).

- [ ] **Step 3: Gates + commit**

`cargo nextest -E 'test(member_card) or test(profile_card)'` green, clippy 0, fmt. Commit:

```bash
git add src-tauri/src/event_loop.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-341): owner_id card subscriber pool + subscribe/get_cached/unsubscribe IPCs"
```

---

## Task 7: Cross-peer e2e integration test

**Files:**
- Create: `src-tauri/tests/profile_card_cross_peer_integration.rs`

Analogous to the ZEB-339 cross-owner e2e. Because spinning real Zenoh is heavy, test the FULL verify+cache pipeline directly (publish bytes → verify_card → cache.insert_verified → get_cached) across two distinct owners, proving owner B accepts A's card and rejects a spoof.

- [ ] **Step 1: Write the test**

```rust
//! ZEB-341 cross-peer: owner A's card verifies + caches under A's owner_id for
//! a subscriber; a card signed by B but claiming A's owner_id is rejected.
use harmony_app::community_membership::mint_test_owner;
use harmony_app::owner_state_types::Hlc;
use harmony_app::profile_card_broadcast::{sign_card, verify_card, ProfileCardCache};

#[tokio::test]
async fn owner_b_resolves_owner_a_card_and_rejects_spoof() {
    let a = mint_test_owner(0x0A);
    let b = mint_test_owner(0x0B);

    // A publishes a valid card.
    let a_card = sign_card(&a.device_key, a.owner.0, "Alice".into(), "hi".into(),
        a.cert.clone(), Hlc { wall_ms: 1, logical: 0, device_id: "a".into() }).unwrap();
    assert_eq!(verify_card(&a_card).unwrap(), a.owner.0);

    // B subscribes to A's owner_id; A's card caches.
    let cache = ProfileCardCache::default();
    cache.register(1, a.owner.0).await;
    cache.insert_verified(1, &a_card).await;
    let got = cache.get_cached(1).await.unwrap();
    assert_eq!(got.display_name, "Alice");
    assert_eq!(got.owner_id_hex, hex::encode(a.owner.0));

    // Spoof: B signs a card claiming A's owner_id (B's cert) -> verify rejects.
    let mut spoof = sign_card(&b.device_key, b.owner.0, "NotAlice".into(), "".into(),
        b.cert.clone(), Hlc { wall_ms: 2, logical: 0, device_id: "b".into() }).unwrap();
    spoof.owner_id = a.owner.0;
    assert!(verify_card(&spoof).is_err(), "spoof claiming A's owner_id must be rejected");

    // Even if a spoof slipped to insert under sub 1, attribution guard ignores it.
    cache.insert_verified(1, &spoof).await; // owner_id == A but cert mismatch already rejected at verify;
    assert_eq!(cache.get_cached(1).await.unwrap().display_name, "Alice", "cache unchanged");
}
```

- [ ] **Step 2: Run + commit**

`cargo nextest run --locked --features test-fixtures -E 'test(profile_card_cross_peer)' 2>&1 | tail -20; echo EXIT=${PIPESTATUS[0]}` → PASS. Commit:

```bash
git add src-tauri/tests/profile_card_cross_peer_integration.rs
git commit -m "test(zeb-341): cross-peer card resolution + spoof-rejection e2e"
```

---

## Task 8: Frontend cross-peer resolution in `member-card-service`

**Files:**
- Modify: `src/lib/member-card-service.ts` (subscribe per visible member, poll cache, populate map)
- Modify: `src/lib/__tests__/member-card-service.test.ts`
- Modify: members-panel + channel-view (drive subscribe lifecycle)

Mirror `src/lib/profile-broadcast-service.ts` (its IPC client + poll pattern). Read it first.

- [ ] **Step 1: Failing test** (mock the Tauri `invoke` for `subscribe_member_card`/`get_cached_member_card`; assert that calling `subscribeVisible([ownerA])` then a poll populates the reactive map and `resolve(ownerA)` returns the card; `unsubscribeAll()` calls `unsubscribe_member_card`). Mirror how `profile-broadcast-service`'s test mocks `invoke`.

- [ ] **Step 2: Implement** `subscribeVisible(ownerIdHexes: string[])` (diff against current subs: `subscribe_member_card` new ones, `unsubscribe_member_card` removed ones, track `ownerId -> subscriptionId`), a poll loop (`get_cached_member_card` per active sub on an interval matching `profile-broadcast-service`) that updates the reactive map (Svelte 5 `$state` map or a store), and `unsubscribeAll()`. Self-seed (Task 1) stays authoritative for the self entry (don't clobber it with a slower network card unless newer — simplest: skip subscribing to self).

- [ ] **Step 3: Drive lifecycle** from the members panel / channel view: on mount or when the visible member set changes, call `subscribeVisible(memberOwnerIds)`; on unmount, `unsubscribeAll()`. Names now fill in for other members as cards arrive (the overlay from Task 1 already reads the map).

- [ ] **Step 4: Frontend gates + commit**

`npx tsc --noEmit && npx vitest run src/lib 2>&1 | tail -15` → pass.

```bash
git add src/lib/member-card-service.ts src/lib/__tests__/member-card-service.test.ts src/lib/components/*.svelte src/App.svelte
git commit -m "feat(zeb-341): cross-peer card resolution (subscribe visible members + poll cache)"
```

---

## Task 9: Clickable members/authors → `ProfilePopover` owner_id-card variant

**Files:**
- Modify: `src/lib/components/ProfilePopover.svelte` (owner_id-card variant)
- Modify: `src/lib/components/MemberRow.svelte`, `ChannelMessageFeed.svelte` (clickable), `App.svelte` (open wiring)
- Modify/Create: popover tests

Read `ProfilePopover.svelte` + its current open path in `App.svelte` (L1816) first.

- [ ] **Step 1: Failing test** — render `ProfilePopover` in the new variant with props `{ ownerIdHex, displayName, statusText, power, role }` (no Reticulum address / no shared-communities). Assert it renders the name, status, copyable owner_id, and role; assert the shared-communities section is absent in this variant. Mirror existing `ProfilePopover.test.ts` structure.

- [ ] **Step 2: Implement** a variant (a discriminated prop, e.g. `mode: 'owner-card'` vs the existing reticulum-profile mode) that renders name/status/owner_id(copyable)/role and OMITS the shared-communities section. Resolve `displayName`/`statusText` from the card map (Task 8); `power`/`role` from the member's `MemberInfoDto`.

- [ ] **Step 3: Make rows clickable** — `MemberRow` name + `ChannelMessageFeed` author become buttons (`role="button"`, keyboard-accessible) that open the popover for that `owner_id`. Wire the open handler through `App.svelte` like the existing popover open path. Unresolved members still open the popover showing the owner_id + "name unavailable".

- [ ] **Step 4: Frontend gates + commit**

`npx tsc --noEmit && npx vitest run src/lib 2>&1 | tail -15` → pass.

```bash
git add src/lib/components/ProfilePopover.svelte src/lib/components/MemberRow.svelte src/lib/components/ChannelMessageFeed.svelte src/App.svelte src/lib/components/__tests__/ProfilePopover.test.ts
git commit -m "feat(zeb-341): clickable members/authors open owner_id profile popover"
```

---

## Task 10: Final gate sweep + push + PR

**Files:** none (verification + PR).

- [ ] **Step 1: Full backend sweep** (commit already done). From `src-tauri/`:

```bash
cargo fmt --all -- --check; echo FMT=$?
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -8; echo CLIPPY=${PIPESTATUS[0]}
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -30; echo NEXTEST=${PIPESTATUS[0]}
```

Expected: FMT=0, CLIPPY=0, NEXTEST=0 modulo the pre-existing orphan transport/port flakes recorded in Task 0 (any NEW membership/card/verify failure is blocking). If the full sweep risks >10 min cold, it's fine to rely on the Bash 600000ms timeout; on overrun report `DONE_WITH_CONCERNS`.

- [ ] **Step 2: Large-tests + MSRV.** From `src-tauri/`:

```bash
HARMONY_LARGE_TESTS=1 cargo nextest run --locked --features test-fixtures -E 'test(folder_ingest_walker_integration)' 2>&1 | tail -15; echo LARGE=${PIPESTATUS[0]}
cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -8; echo MSRV=${PIPESTATUS[0]}
```

Expected: both 0. (MSRV uses the declared toolchain; if a separate MSRV toolchain isn't installed locally, note it — CI runs it authoritatively.)

- [ ] **Step 3: Frontend gates.** From repo root:

```bash
npx tsc --noEmit; echo TSC=$?
npx vitest run 2>&1 | tail -20; echo VITEST=${PIPESTATUS[0]}
```

Expected: TSC=0, VITEST=0.

- [ ] **Step 4: Push + open PR**

```bash
git push -u origin zeb-341-profile-cards
```

Then `gh pr create` with title `ZEB-341: resolvable per-identity profile cards (display name + status) by owner_id` and a body that:
- markdown-links **ZEB-341** (so Linear attaches; per feedback_linear_pr_auto_close the parent **ZEB-218** must NOT be over-closed on merge — note to verify post-merge).
- Summary: new owner_id-keyed cert-verified `ProfileCardBroadcast`; publish via device #2 + cert; subscriber pool + cache + 3 IPCs; frontend `member-card-service` (self-seed + cross-peer) + clickable popover; name+status only; CAS avatar/profile-page reserved as additive fields (not implemented).
- Spec commit `64cdbab` + plan path.
- Test plan: the 5 backend gates + 2 frontend gates above, wire-format fixture, verify negatives, cross-peer e2e.
- 🤖 Generated with [Claude Code](https://claude.com/claude-code)

- [ ] **Step 5:** Report the PR URL. Controller then enters the autonomous bot-review loop (CodeRabbit/Cursor/CodeAnt/Qodo + 5 CI jobs), addresses findings as bundled pushes, NEVER triggers Greptile, and pings Jake when ready to merge. Do NOT merge.

---

## Plan self-review

- **Spec coverage:** §4 wire type → T2; §5 verify → T3; §6 publish → T5; §7 cache+IPCs → T4/T6; §7 frontend service → T1/T8; §8 render+clickable → T1/T8/T9; §9 CAS extensibility → honored by additive canonical-CBOR encoding (no fields added, documented); §10 error handling → T3/T5/T6 (drop+warn, bounds, graceful unresolved); §11 testing → T2 fixture, T3 negatives, T7 cross-peer, T1/T8/T9 vitest, T10 gates; §13 self-first-first → T1 ordered first. No gaps.
- **Type consistency:** `ProfileCardBroadcast`, `sign_card`, `verify_card`, `CardError`/`CardVerifyError`, `ProfileCardCache.{register,insert_verified,get_cached,drop_subscription}`, `DiscoveredCardInfo{owner_id_hex,display_name,status_text}`, `card_topic_for`, `MemberCardService.{seedSelf,resolve,subscribeVisible,unsubscribeAll}`, IPCs `subscribe_member_card`/`get_cached_member_card`/`unsubscribe_member_card` — names used consistently across tasks.
- **Risk gates:** T2 (cert API shape — if `EnrollmentCert` fields differ from ZEB-339's usage, report `DONE_WITH_CONCERNS` with actual shape, don't guess), T6 (mirroring the subscriber pool — read the L1070-1240 template; if the pool's param-threading differs, follow the actual code).
- **Placeholder scan:** machinery tasks (T5/T6/T8/T9) intentionally cite exact template file:line to mirror rather than re-inlining 150+ lines verbatim — implementers MAY read cited files (stated in the header), matching the ZEB-339 execution pattern. All novel logic (wire type, verify, cache, tests) has complete code.
