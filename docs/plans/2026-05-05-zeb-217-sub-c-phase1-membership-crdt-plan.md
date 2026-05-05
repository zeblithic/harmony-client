# ZEB-217 Sub-C Phase 1: Membership CRDT Primitives — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the type definitions, signing, verification, and materialization logic for the community membership CRDT. No IPC, no Zenoh, no UI. Pure-function tests only. Phase 1 of 5; Phases 2-5 each get their own plan when started.

**Architecture:** Two new modules — `community_membership.rs` (event types + materialization + verification) and `community_invite.rs` (invite payload types only — Reticulum send/receive lands in Phase 4). Extend `owner_state_types.rs` with the `MembershipKey` newtype and three new `Space` fields (`membership_key`, `admin_addr`, `is_invite_only`) plus matching `validate_invariants` rules for `SpaceKind::Community`. All canonical CBOR uses the same-length-keys invariant per nesting level (the codebase-wide rule that lets the wire encoding be byte-stable across implementations).

**Tech Stack:** Rust 2024 edition; `serde` + `ciborium` for canonical CBOR; `ed25519-dalek` for signing; existing `canonical_cbor_encode` / `CanonicalPayload` machinery from `owner_state_crypto.rs`; `zeroize::ZeroizeOnDrop` for key material; `rand::rngs::OsRng` for entropy.

**Spec:** [`docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md`](../specs/2026-05-05-zeb-217-sub-c-communities-design.md) (commit `8b8028a`)

**Branch:** `zeb-217-sub-c-phase1-membership-crdt` (branched from `origin/main` — current head is `a80b777`, ZEB-228 PR #81)

---

## File Structure

| File | Action | Responsibility | Approx. lines |
|---|---|---|---|
| `src-tauri/src/community_membership.rs` | Create | `SignedMembershipEvent`, `MembershipEventKind`, `CounterSignature`, `MaterializedMembership`, `MemberState`, `MemberStatus`, `POWER_THRESHOLDS`, `verify_event`, `materialize`, `sign_event` | ~450 |
| `src-tauri/src/community_invite.rs` | Create | `CommunityInvitePayload`, `InviteToken` (types + canonical CBOR only — Reticulum send path lands in Phase 4) | ~120 |
| `src-tauri/src/owner_state_types.rs` | Modify | Add `MembershipKey` newtype (after `DmContentKey` at line ~234); extend `Space` with `mk` / `ad` / `io` fields; extend `validate_invariants` for `SpaceKind::Community`; add `MembershipKey` to `impl_canonical!` list | ~80 net additions |
| `src-tauri/src/lib.rs` | Modify | Add `mod community_membership;` and `mod community_invite;` declarations only — no command registration in Phase 1 | ~2 |
| `src-tauri/tests/community_membership_unit.rs` | Create | Unit-style integration tests for materialization rules, verification rules, bootstrap rule, race-case convergence | ~400 |
| `src-tauri/tests/wire_format_community_fixtures.rs` | Create | CBOR golden-byte fixtures for `MembershipKey`, `SignedMembershipEvent` × 5 kinds, `CounterSignature`, `CommunityInvitePayload`, `InviteToken` | ~250 |

**Phase 1 scope boundary:** No `community_state_crdt.rs` (that's Phase 2). No Reticulum send/receive in `community_invite.rs` (that's Phase 4). No IPC commands. No frontend changes. Spec sections covered: "Data model" (entirely), "Materialization rules" + "Verification" (entirely), "Invite link payload" (types only, no encoding/decoding helpers — those land in Phase 3 with `generate_invite`).

---

## Pre-flight (Task 0)

### Task 0: Branch off latest origin/main and verify baseline gates

**Files:** none modified — branch creation + verification only

- [ ] **Step 0.1: Pull latest origin/main and verify baseline**

```bash
git fetch origin
git checkout main
git pull origin main
git log --oneline -3
```

Expected: HEAD is `a80b777` (ZEB-228 Phase 4 merge) or newer. If not, abort and ask the user — branching off an outdated main violates the pull-before-work hard rule.

- [ ] **Step 0.2: Create the Phase 1 branch**

```bash
git checkout -b zeb-217-sub-c-phase1-membership-crdt
```

Expected: `Switched to a new branch 'zeb-217-sub-c-phase1-membership-crdt'`.

- [ ] **Step 0.3: Verify baseline gates green on the branch**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
echo "FMT_EXIT=$?"
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
echo "CLIPPY_EXIT=$?"
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -5
echo "TEST_EXIT=$?"
```

Expected: all three exit codes 0; test summary matches the 611-passing baseline from PR #81.

If any gate fails on a fresh `origin/main`, that's test drift — file a Linear follow-up + fix on a separate branch BEFORE proceeding (per the "test drift is our fault" hard rule).

---

## Task 1: `MembershipKey` newtype (mirror `DmContentKey`)

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs:234` (insert after `DmContentKey`'s `impl Debug` block at line ~260)

- [ ] **Step 1.1: Write the failing test**

Append to `src-tauri/src/owner_state_types.rs` inside the existing `#[cfg(test)] mod tests` block (currently at line ~1000+; find the right `mod tests` and add to it):

```rust
#[test]
fn membership_key_round_trips_through_canonical_cbor() {
    use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};

    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = i as u8;
    }
    let key = MembershipKey::new(bytes);

    let encoded = canonical_cbor_encode(&key).expect("encode");
    let decoded: MembershipKey = canonical_cbor_decode(&encoded).expect("decode");

    assert_eq!(key.as_bytes(), decoded.as_bytes());
}

#[test]
fn membership_key_debug_is_redacted() {
    let key = MembershipKey::new([0xAB; 32]);
    let formatted = format!("{:?}", key);
    assert!(
        !formatted.contains("AB"),
        "MembershipKey Debug must not leak bytes; got: {formatted}"
    );
    assert!(formatted.contains("redacted"));
}

#[test]
fn membership_key_random_produces_distinct_values() {
    let a = MembershipKey::random();
    let b = MembershipKey::random();
    assert_ne!(a.as_bytes(), b.as_bytes(), "OsRng entropy was identical?!");
}
```

- [ ] **Step 1.2: Run tests to verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --lib owner_state_types::tests::membership_key 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: FAIL — `MembershipKey` is not defined.

- [ ] **Step 1.3: Define `MembershipKey` newtype**

Insert into `src-tauri/src/owner_state_types.rs` immediately after the `DmContentKey` `impl Debug` block (around line ~260):

```rust
/// 32-byte symmetric key for community membership-topic encryption
/// (ChaCha20-Poly1305). Wire format: bstr(32). In-memory: zeroized
/// on drop. Debug redacts bytes to avoid log leakage.
///
/// Mirrors DmContentKey precisely — same shape, different purpose.
/// Distributed via CommunityInvitePayload at invite-link generation;
/// stored in the community Space's `membership_key` field on every
/// member's owner-state CRDT (where it inherits encryption-at-rest
/// from ZEB-211).
///
/// See ZEB-217 spec §"Data model — Space struct additions".
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, zeroize::ZeroizeOnDrop)]
pub struct MembershipKey(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    [u8; 32],
);

impl MembershipKey {
    pub fn new(key: [u8; 32]) -> Self {
        Self(key)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Generate a fresh random key from OS entropy. Used when
    /// creating a new community.
    pub fn random() -> Self {
        use rand::RngCore;
        use zeroize::Zeroizing;
        let mut k = Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(k.as_mut());
        Self(*k)
    }
}

impl std::fmt::Debug for MembershipKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MembershipKey(<32 bytes redacted>)")
    }
}
```

- [ ] **Step 1.4: Add `MembershipKey` to `impl_canonical!` list**

Find the existing `impl_canonical!` macro invocation in `owner_state_types.rs` (around line ~988):

```rust
impl_canonical!(
    Hlc,
    SpaceId,
    OwnerAddr,
    ContentId,
    OutboxEntryId,
    DmContentKey,
    DeviceIdentityHash,
    OwnerDeviceCache,
    OwnerDeviceEntry,
    SpaceKind,
    NotificationPref,
    ReticulumDest,
    TransportBinding,
    Space,
    DedupeKey,
    DeliveryStatus,
    // ... more entries
);
```

Add `MembershipKey,` to the list (alphabetically sensible position is right after `DmContentKey,`):

```rust
impl_canonical!(
    Hlc,
    SpaceId,
    OwnerAddr,
    ContentId,
    OutboxEntryId,
    DmContentKey,
    MembershipKey,                  // ← NEW (Phase 1: ZEB-217)
    DeviceIdentityHash,
    // ... rest unchanged
);
```

- [ ] **Step 1.5: Run tests to verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --lib owner_state_types::tests::membership_key 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: 3/3 passing.

- [ ] **Step 1.6: Run full gate sweep**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Expected: all clean; test count is baseline + 3.

- [ ] **Step 1.7: Commit**

```bash
git add src-tauri/src/owner_state_types.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): MembershipKey newtype mirroring DmContentKey

32-byte symmetric key for community membership-topic encryption
(ChaCha20-Poly1305). Same shape as DmContentKey: bstr(32) on the
wire, ZeroizeOnDrop in memory, redacted Debug.

Added to impl_canonical! so canonical_cbor_encode/decode work.
Tests cover round-trip, Debug redaction, random-distinctness.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Extend `Space` struct with `mk` / `ad` / `io` fields

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs` — add three new fields after `prior_content_keys` (currently the last field, around line ~1353)

This task adds the fields with serde annotations + makes Phase 3+ able to construct community Spaces. The `validate_invariants` rules for `SpaceKind::Community` come in Task 3.

- [ ] **Step 2.1: Write the failing test**

Append to `owner_state_types.rs` test module:

```rust
#[test]
fn community_space_round_trips_with_new_fields() {
    use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};

    let admin = OwnerAddr([1u8; 16]);
    let community_id = SpaceId([2u8; 16]);
    let key = MembershipKey::new([3u8; 32]);

    let space = Space {
        id: community_id,
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,           // community Space IS the community
        name: "harmony-design".to_string(),
        transport: None,
        members: vec![],              // membership lives in CommunityState CRDT
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 100, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 100, logical: 0, device_id: "d".into() },
        content_key: None,
        prior_content_keys: vec![],
        membership_key: Some(key.clone()),
        admin_addr: Some(admin),
        is_invite_only: Some(true),
    };

    let encoded = canonical_cbor_encode(&space).expect("encode");
    let decoded: Space = canonical_cbor_decode(&encoded).expect("decode");

    assert_eq!(decoded.kind, SpaceKind::Community);
    assert_eq!(decoded.membership_key.as_ref().map(|k| *k.as_bytes()),
               Some(*key.as_bytes()));
    assert_eq!(decoded.admin_addr, Some(admin));
    assert_eq!(decoded.is_invite_only, Some(true));
}

#[test]
fn non_community_space_skips_membership_fields_in_wire() {
    use crate::owner_state_crypto::canonical_cbor_encode;

    let dm = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "dm".to_string(),
        transport: Some(TransportBinding::Reticulum { participants: vec![] }),
        members: vec![OwnerAddr([2u8; 16]), OwnerAddr([3u8; 16])],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 100, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 100, logical: 0, device_id: "d".into() },
        content_key: Some(DmContentKey::new([5u8; 32])),
        prior_content_keys: vec![],
        membership_key: None,
        admin_addr: None,
        is_invite_only: None,
    };

    let bytes = canonical_cbor_encode(&dm).expect("encode");
    // skip_serializing_if guarantees these byte sequences DON'T appear
    // in the encoded blob for non-community Spaces (defense against
    // wire-bloat regression). The literal text is the CBOR text(2)
    // string for each new field code.
    let needles = [b"mk", b"ad", b"io"];
    for needle in &needles {
        let found = bytes.windows(2).any(|w| w == *needle);
        assert!(
            !found,
            "non-community Space wire blob contained {:?} — \
             skip_serializing_if regression",
            std::str::from_utf8(*needle).unwrap()
        );
    }
}
```

- [ ] **Step 2.2: Run tests to verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --lib owner_state_types::tests::community_space_round_trips owner_state_types::tests::non_community_space_skips 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: FAIL — `Space` doesn't have `membership_key` / `admin_addr` / `is_invite_only` fields.

- [ ] **Step 2.3: Add the three new fields to `Space`**

Modify `Space` struct in `owner_state_types.rs` (currently around line ~1298). Update the doc comment AND add the three fields after `prior_content_keys`:

```rust
/// The unified Space CRDT entry — see ZEB-206 spec §"Space — unified
/// entry in owner-state CRDT".
///
/// Wire-format note: every field is renamed to a 2-char code so all
/// 17 keys at this nesting level have identical encoded length (CBOR
/// text(2) = 3 bytes per key). Mixing 1-char and 2-char renames here
/// would re-introduce the same-length-keys violation Hlc had before
/// PR #72 round 3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    #[serde(rename = "id")]
    pub id: SpaceId,
    #[serde(rename = "kn")]
    pub kind: SpaceKind,
    #[serde(rename = "pa")]
    pub parent: Option<SpaceId>,
    #[serde(rename = "ci")]
    pub community_id: Option<SpaceId>,
    #[serde(rename = "nm")]
    pub name: String,
    #[serde(rename = "tr")]
    pub transport: Option<TransportBinding>,
    #[serde(rename = "me")]
    pub members: Vec<OwnerAddr>,
    #[serde(rename = "cn")]
    pub custom_name: Option<String>,
    #[serde(rename = "np")]
    pub notification_pref: Option<NotificationPref>,
    #[serde(rename = "la")]
    pub left_at: Option<Hlc>,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
    #[serde(rename = "ua")]
    pub updated_at: Hlc,

    #[serde(rename = "ck", skip_serializing_if = "Option::is_none", default)]
    pub content_key: Option<DmContentKey>,

    #[serde(
        rename = "pk",
        skip_serializing_if = "Vec::is_empty",
        default,
        deserialize_with = "deserialize_prior_content_keys"
    )]
    pub prior_content_keys: Vec<DmContentKey>,

    /// Per-community symmetric key for membership topic encryption.
    /// MUST be Some for kind == Community; MUST be None otherwise.
    /// Wire: bstr(32) under "mk".  Zeroized on drop (via the
    /// MembershipKey newtype's ZeroizeOnDrop impl).
    /// See ZEB-217 spec §"Data model — Space struct additions".
    #[serde(rename = "mk", skip_serializing_if = "Option::is_none", default)]
    pub membership_key: Option<MembershipKey>,

    /// Initial admin (creator) — receives power 100 implicitly via the
    /// bootstrap rule (see ZEB-217 spec §"Materialization rules /
    /// Bootstrap"). MUST be Some for kind == Community; MUST be None
    /// otherwise. Wire: bstr(16) under "ad".
    #[serde(rename = "ad", skip_serializing_if = "Option::is_none", default)]
    pub admin_addr: Option<OwnerAddr>,

    /// Policy flag — false = open (peers publish join events directly),
    /// true = invite-only (join requires counter-sig from member with
    /// power ≥ POWER_THRESHOLDS.invite). MUST be Some for kind ==
    /// Community; MUST be None otherwise. Wire: bool under "io".
    #[serde(rename = "io", skip_serializing_if = "Option::is_none", default)]
    pub is_invite_only: Option<bool>,
}
```

- [ ] **Step 2.4: Run tests to verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --lib owner_state_types::tests::community_space_round_trips owner_state_types::tests::non_community_space_skips 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: 2/2 passing.

- [ ] **Step 2.5: Run full gate sweep**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Expected: all clean; baseline + 5 tests.

If clippy fires on the new docstrings (e.g., missing backticks), fix inline before commit. If clippy fires elsewhere (existing Space construction sites in tests now missing the three new fields), that's an expected error — go fix the construction sites by adding `membership_key: None, admin_addr: None, is_invite_only: None` to each. The compile errors will tell you exactly which file:line. Don't proceed until everything compiles + clippy is clean.

- [ ] **Step 2.6: Commit**

```bash
git add src-tauri/src/owner_state_types.rs
# ALSO add any other test files that needed Space-construction updates
git status  # review what changed
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): extend Space struct with community fields (mk/ad/io)

Three new optional fields on Space, all 2-char wire codes preserving
the same-length-keys CBOR invariant at this nesting level:

* membership_key (mk) — per-community ChaCha20-Poly1305 key
* admin_addr (ad) — initial admin / power-100 designation
* is_invite_only (io) — policy flag

All three skip_serializing_if Option::is_none so non-community Spaces
emit identical wire bytes to before this commit (regression-tested).
validate_invariants enforcement for SpaceKind::Community comes in
Task 3.

Existing Space construction sites in tests updated with explicit
None for the three new fields (no behavioral change).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Extend `validate_invariants` for `SpaceKind::Community`

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs:~1485` (the `validate_invariants` match block; add a new arm for `SpaceKind::Community` and tighten the existing arms to assert the new fields are `None` for non-community kinds)

- [ ] **Step 3.1: Write the failing tests**

Append to `owner_state_types.rs` test module:

```rust
#[test]
fn community_space_validates_when_all_required_fields_present() {
    let s = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: "ok".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: None,
        prior_content_keys: vec![],
        membership_key: Some(MembershipKey::new([0u8; 32])),
        admin_addr: Some(OwnerAddr([2u8; 16])),
        is_invite_only: Some(false),
    };
    assert!(s.validate_invariants().is_ok());
}

#[test]
fn community_space_rejects_missing_membership_key() {
    let mut s = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: "x".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: None,
        prior_content_keys: vec![],
        membership_key: None,    // ← invariant violation
        admin_addr: Some(OwnerAddr([2u8; 16])),
        is_invite_only: Some(false),
    };
    let err = s.validate_invariants().expect_err("must reject");
    assert!(err.0.contains("membership_key"));

    // Now also confirm missing admin_addr and missing is_invite_only fail.
    s.membership_key = Some(MembershipKey::new([0u8; 32]));
    s.admin_addr = None;
    let err = s.validate_invariants().expect_err("must reject");
    assert!(err.0.contains("admin_addr"));

    s.admin_addr = Some(OwnerAddr([2u8; 16]));
    s.is_invite_only = None;
    let err = s.validate_invariants().expect_err("must reject");
    assert!(err.0.contains("is_invite_only"));
}

#[test]
fn community_space_rejects_non_empty_members() {
    let s = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: "x".into(),
        transport: None,
        members: vec![OwnerAddr([99u8; 16])],   // ← invariant violation
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: None,
        prior_content_keys: vec![],
        membership_key: Some(MembershipKey::new([0u8; 32])),
        admin_addr: Some(OwnerAddr([2u8; 16])),
        is_invite_only: Some(false),
    };
    let err = s.validate_invariants().expect_err("must reject");
    assert!(
        err.0.contains("members=[]"),
        "expected error mentioning empty members invariant; got: {}",
        err.0
    );
}

#[test]
fn community_space_rejects_transport_present() {
    let s = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: "x".into(),
        transport: Some(TransportBinding::Zenoh { topic: "wat".into() }),
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: None,
        prior_content_keys: vec![],
        membership_key: Some(MembershipKey::new([0u8; 32])),
        admin_addr: Some(OwnerAddr([2u8; 16])),
        is_invite_only: Some(false),
    };
    let err = s.validate_invariants().expect_err("must reject");
    assert!(err.0.contains("transport=None"));
}

#[test]
fn dm_space_rejects_membership_key_present() {
    let s = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "dm".into(),
        transport: Some(TransportBinding::Reticulum { participants: vec![] }),
        members: vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: Some(DmContentKey::new([5u8; 32])),
        prior_content_keys: vec![],
        membership_key: Some(MembershipKey::new([7u8; 32])),  // ← wrong kind
        admin_addr: None,
        is_invite_only: None,
    };
    let err = s.validate_invariants().expect_err("must reject");
    assert!(
        err.0.contains("membership_key"),
        "expected error about non-community membership_key; got: {}",
        err.0
    );
}
```

- [ ] **Step 3.2: Run tests to verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --lib owner_state_types::tests::community_space owner_state_types::tests::dm_space_rejects 2>&1 | tail -15
echo "TEST_EXIT=$?"
```

Expected: FAIL — most tests pass on the missing-arm match (Community kind not handled), but the rejection tests fail because invariants don't fire.

- [ ] **Step 3.3: Extend `validate_invariants` match block**

Find the `validate_invariants` impl on `Space` (around line ~1389). Add a new arm for `SpaceKind::Community`, AND extend each existing arm with a check that the new community-only fields are `None`. The existing match body looks like:

```rust
pub fn validate_invariants(&self) -> Result<(), InvariantError> {
    match self.kind {
        SpaceKind::Folder => { /* ... existing checks ... */ }
        SpaceKind::Channel => { /* ... existing checks ... */ }
        SpaceKind::PublicChannel => { /* ... existing checks ... */ }
        SpaceKind::Dm => { /* ... existing checks ... */ }
        SpaceKind::GroupDm => { /* ... existing checks ... */ }
        // NO arm for Community → unreachable when kind=Community
    }
    Ok(())
}
```

Update to:

```rust
pub fn validate_invariants(&self) -> Result<(), InvariantError> {
    // Universal: community-only fields MUST be None unless kind == Community.
    // Checked before the per-kind match so every non-community kind gets
    // the same enforcement without per-arm duplication.
    if self.kind != SpaceKind::Community {
        if self.membership_key.is_some() {
            return Err(InvariantError(format!(
                "{:?} must have membership_key=None (only Community carries it)",
                self.kind
            )));
        }
        if self.admin_addr.is_some() {
            return Err(InvariantError(format!(
                "{:?} must have admin_addr=None (only Community carries it)",
                self.kind
            )));
        }
        if self.is_invite_only.is_some() {
            return Err(InvariantError(format!(
                "{:?} must have is_invite_only=None (only Community carries it)",
                self.kind
            )));
        }
    }

    match self.kind {
        SpaceKind::Folder => { /* ... existing checks unchanged ... */ }
        SpaceKind::Community => {
            if self.membership_key.is_none() {
                return Err(InvariantError(
                    "community must have membership_key".into(),
                ));
            }
            if self.admin_addr.is_none() {
                return Err(InvariantError(
                    "community must have admin_addr".into(),
                ));
            }
            if self.is_invite_only.is_none() {
                return Err(InvariantError(
                    "community must have is_invite_only".into(),
                ));
            }
            if !self.members.is_empty() {
                return Err(InvariantError(
                    "community must have members=[] in owner-state Space \
                     (real membership is in CommunityState CRDT)"
                        .into(),
                ));
            }
            if self.transport.is_some() {
                return Err(InvariantError(
                    "community must have transport=None".into(),
                ));
            }
            if self.community_id.is_some() {
                return Err(InvariantError(
                    "community must have community_id=None \
                     (community Space IS the community)"
                        .into(),
                ));
            }
            if self.content_key.is_some() {
                return Err(InvariantError(
                    "community must have content_key=None \
                     (membership_key is the community's symmetric key)"
                        .into(),
                ));
            }
        }
        SpaceKind::Channel => { /* ... existing checks unchanged ... */ }
        SpaceKind::PublicChannel => { /* ... existing checks unchanged ... */ }
        SpaceKind::Dm => { /* ... existing checks unchanged ... */ }
        SpaceKind::GroupDm => { /* ... existing checks unchanged ... */ }
    }
    Ok(())
}
```

(Keep the existing match arm bodies unchanged — only add the community arm and the universal community-fields-Must-Be-None check at the top.)

- [ ] **Step 3.4: Run tests to verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --lib owner_state_types::tests::community_space owner_state_types::tests::dm_space_rejects 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: 5/5 passing.

- [ ] **Step 3.5: Run full gate sweep**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Expected: all clean; baseline + 10 tests.

- [ ] **Step 3.6: Commit**

```bash
git add src-tauri/src/owner_state_types.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): validate_invariants for SpaceKind::Community

Adds the Community arm to the per-kind invariant match + a universal
check that community-only fields (mk/ad/io) are None on non-Community
kinds. Together these enforce the kind-field correspondence both
directions:

* Community kind requires membership_key + admin_addr + is_invite_only,
  forbids non-empty members, transport, community_id, content_key.
* Non-Community kinds forbid membership_key, admin_addr, is_invite_only.

Tests cover both happy path (valid community Space) and every
invariant violation in both directions.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `MembershipEventKind` enum

**Files:**
- Create: `src-tauri/src/community_membership.rs` (initial skeleton + the enum)
- Modify: `src-tauri/src/lib.rs` — add `mod community_membership;` declaration

- [ ] **Step 4.1: Write the failing test**

Create `src-tauri/tests/community_membership_unit.rs`:

```rust
//! Unit-style integration tests for community_membership.rs.
//! Phase 1 (ZEB-217 Sub-C) — types, materialization, verification.

use harmony_app::community_membership::{
    MembershipEventKind,
};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::OwnerAddr;

#[test]
fn membership_event_kind_round_trips_all_variants() {
    let target = OwnerAddr([7u8; 16]);

    let kinds = vec![
        MembershipEventKind::Join,
        MembershipEventKind::Leave,
        MembershipEventKind::Invite { target },
        MembershipEventKind::Kick {
            target,
            reason: Some("spam".to_string()),
        },
        MembershipEventKind::Kick {
            target,
            reason: None,
        },
        MembershipEventKind::SetPower { target, level: 50 },
    ];

    for k in kinds {
        let encoded = canonical_cbor_encode(&k).expect("encode");
        let decoded: MembershipEventKind = canonical_cbor_decode(&encoded).expect("decode");
        assert_eq!(decoded, k, "round-trip mismatch for {k:?}");
    }
}
```

- [ ] **Step 4.2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: FAIL — `harmony_app::community_membership` doesn't exist.

- [ ] **Step 4.3: Create `community_membership.rs` skeleton with the enum**

Create `src-tauri/src/community_membership.rs`:

```rust
//! Community membership CRDT primitives — ZEB-217 Sub-C Phase 1.
//!
//! Per-community signed-event CRDT replicated via the encrypted Zenoh
//! state-root topic (Phase 2). Phase 1 ships only the types,
//! materialization rules, and verification logic — no IPC, no
//! networking, no UI.
//!
//! See `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md`.

use serde::{Deserialize, Serialize};

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::OwnerAddr;

/// The five membership event kinds. Adjacently tagged so the wire
/// format is `{ "tg": "<variant>", "vl": <body> }` — both keys are
/// 2-char to satisfy the same-length-keys CBOR invariant at this
/// nesting level. Variant codes are 1-char (values, not keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tg", content = "vl")]
pub enum MembershipEventKind {
    #[serde(rename = "j")]
    Join,
    #[serde(rename = "l")]
    Leave,
    #[serde(rename = "i")]
    Invite {
        #[serde(rename = "tg")]
        target: OwnerAddr,
    },
    #[serde(rename = "k")]
    Kick {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
    #[serde(rename = "p")]
    SetPower {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "lv")]
        level: u8,
    },
}

impl CanonicalPayloadSealed for MembershipEventKind {}
impl CanonicalPayload for MembershipEventKind {}
```

- [ ] **Step 4.4: Wire the module into `lib.rs`**

Find the existing `mod` declarations in `src-tauri/src/lib.rs` (typically near the top, after `use` blocks). Add:

```rust
pub mod community_membership;
```

(Use `pub mod` so the integration test in `tests/community_membership_unit.rs` can `use harmony_app::community_membership::...`.)

- [ ] **Step 4.5: Run test to verify it passes**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: PASS.

- [ ] **Step 4.6: Run full gate sweep**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Expected: all clean; baseline + 11 tests.

- [ ] **Step 4.7: Commit**

```bash
git add src-tauri/src/community_membership.rs src-tauri/src/lib.rs src-tauri/tests/community_membership_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): MembershipEventKind enum + module skeleton

Adjacently-tagged CBOR enum with 5 variants (Join, Leave, Invite,
Kick, SetPower). Inner field codes (tg/rs/lv) all 2-char to satisfy
the same-length-keys CBOR invariant at this nesting level; variant
codes (j/l/i/k/p) are 1-char values, not keys, so not subject to
that rule.

Phase 1 module skeleton — verification + materialization land in
follow-up tasks. Wired into lib.rs as pub mod for integration tests.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `SignedMembershipEvent` + `CounterSignature`

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — add `SignedMembershipEvent`, `CounterSignature`, `EventId` type alias

- [ ] **Step 5.1: Write the failing test**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
use harmony_app::community_membership::{
    CounterSignature, EventId, SignedMembershipEvent,
};
use harmony_app::owner_state_types::{Hlc, SpaceId};

#[test]
fn signed_event_round_trips_through_canonical_cbor() {
    let event = SignedMembershipEvent {
        id: [9u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: OwnerAddr([1u8; 16]),
        at: Hlc {
            wall_ms: 12345,
            logical: 7,
            device_id: "phone".into(),
        },
        sig: [0xAA; 64],
        countersig: None,
    };

    let bytes = canonical_cbor_encode(&event).expect("encode");
    let decoded: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, event);
}

#[test]
fn signed_event_with_countersig_round_trips() {
    let countersig = CounterSignature {
        signer: OwnerAddr([42u8; 16]),
        sig: [0xBB; 64],
    };

    let event = SignedMembershipEvent {
        id: [9u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: OwnerAddr([1u8; 16]),
        at: Hlc {
            wall_ms: 12345,
            logical: 7,
            device_id: "phone".into(),
        },
        sig: [0xAA; 64],
        countersig: Some(countersig.clone()),
    };

    let bytes = canonical_cbor_encode(&event).expect("encode");
    let decoded: SignedMembershipEvent = canonical_cbor_decode(&bytes).expect("decode");
    assert_eq!(decoded, event);
    assert_eq!(decoded.countersig.as_ref().map(|c| c.signer), Some(countersig.signer));
}

#[test]
fn event_id_type_is_16_bytes() {
    let id: EventId = [0u8; 16];
    assert_eq!(std::mem::size_of_val(&id), 16);
}
```

- [ ] **Step 5.2: Run tests to verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit signed_event 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: FAIL — `SignedMembershipEvent` not defined.

- [ ] **Step 5.3: Add the new types**

Append to `src-tauri/src/community_membership.rs`:

```rust
use crate::owner_state_types::{serialize_bytes_as_bstr, deserialize_bytes_from_bstr};
use crate::owner_state_types::{Hlc, SpaceId};

/// 16-byte ULID identifying a single signed membership event within
/// a community's CRDT log. Generated client-side at event creation.
pub type EventId = [u8; 16];

/// One signed event in a community's membership CRDT.
///
/// Wire format: 8-key CBOR map. All keys are 2 chars (text(2) = 3 bytes
/// each) to satisfy the same-length-keys invariant at this nesting
/// level. Adjacently-tagged inner enums (MembershipEventKind,
/// CounterSignature) follow the same rule recursively.
///
/// `sig` covers the canonical-CBOR encoding of (id, community_id, kind,
/// actor, at) — countersig is excluded so an inviter can append their
/// counter-signature without invalidating the actor's signature. See
/// `sign_event` (Task 6) for the exact byte layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedMembershipEvent {
    #[serde(
        rename = "id",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub id: EventId,

    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    #[serde(rename = "kn")]
    pub kind: MembershipEventKind,

    #[serde(rename = "ac")]
    pub actor: OwnerAddr,

    #[serde(rename = "at")]
    pub at: Hlc,

    /// Ed25519 signature over canonical CBOR of
    /// `(id, community_id, kind, actor, at)`. 64 bytes.
    #[serde(
        rename = "sg",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],

    /// Required for Join events in invite-only communities. None
    /// otherwise. Verified at receive time against the signer's
    /// power level at the time of the join.
    #[serde(rename = "cs", skip_serializing_if = "Option::is_none", default)]
    pub countersig: Option<CounterSignature>,
}

/// Counter-signature appended by an existing community member to vouch
/// for a new joiner in an invite-only community. The signer's power
/// must be ≥ POWER_THRESHOLDS.invite at the time of signing.
///
/// `sig` covers the same canonical-CBOR bytes as `SignedMembershipEvent.sig`
/// — i.e., the joiner's signed `(id, community_id, kind, actor, at)`.
/// This means the countersig binds to the joiner's exact event, not
/// just to the community ID, preventing a malicious admin from
/// "reusing" a countersig across different join attempts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterSignature {
    #[serde(rename = "sg")]
    pub signer: OwnerAddr,

    #[serde(
        rename = "sx",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

impl CanonicalPayloadSealed for SignedMembershipEvent {}
impl CanonicalPayload for SignedMembershipEvent {}
impl CanonicalPayloadSealed for CounterSignature {}
impl CanonicalPayload for CounterSignature {}
```

⚠ **Important:** `serialize_bytes_as_bstr` / `deserialize_bytes_from_bstr` are private to `owner_state_types.rs` in the current codebase. To use them in `community_membership.rs`, either:
- (a) Make them `pub(crate)` in `owner_state_types.rs`
- (b) Re-export them via a `pub(crate) use` in `lib.rs`

Pick (a) — it's the smaller diff. Find the two helper functions in `owner_state_types.rs` and change their visibility:

```rust
// in owner_state_types.rs, change:
fn serialize_bytes_as_bstr<S, const N: usize>(...) { ... }
fn deserialize_bytes_from_bstr<'de, D, const N: usize>(...) { ... }

// to:
pub(crate) fn serialize_bytes_as_bstr<S, const N: usize>(...) { ... }
pub(crate) fn deserialize_bytes_from_bstr<'de, D, const N: usize>(...) { ... }
```

Verify the actual signatures by reading the existing code first; the changes above show only the visibility modifier and assume the helpers are generic over `N` (they are — used for both `[u8; 16]` and `[u8; 32]` and `[u8; 64]`).

- [ ] **Step 5.4: Run tests to verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit signed_event 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: 3/3 passing.

- [ ] **Step 5.5: Run full gate sweep**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Expected: all clean; baseline + 14 tests.

- [ ] **Step 5.6: Commit**

```bash
git add src-tauri/src/community_membership.rs src-tauri/src/owner_state_types.rs src-tauri/tests/community_membership_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): SignedMembershipEvent + CounterSignature types

8-key SignedMembershipEvent map (all 2-char keys for same-length-
keys CBOR invariant). Sig covers (id, community_id, kind, actor, at)
— countersig EXCLUDED so an inviter's counter-sig can be appended
without invalidating the actor's signature. Counter-sig itself
covers the same payload bytes, binding the vouching to the exact
joiner event (not just to the community).

Made serialize_bytes_as_bstr / deserialize_bytes_from_bstr pub(crate)
in owner_state_types.rs so community_membership.rs can use them for
the bstr-encoded EventId / sig / countersig fields.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `sign_event` helper

**Files:**
- Modify: `src-tauri/src/community_membership.rs` — add `sign_event` function

The signing helper takes the unsigned-event payload (everything except `sig` and `countersig`), encodes it canonically, and signs with an ed25519 key.

- [ ] **Step 6.1: Write the failing test**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use harmony_app::community_membership::{sign_event, EventPayload};

#[test]
fn sign_event_produces_signature_verifiable_with_pubkey() {
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let pubkey: VerifyingKey = signing_key.verifying_key();
    let actor = OwnerAddr({
        // OwnerAddr is the first 16 bytes of BLAKE3(pubkey) per existing
        // identity convention. For this test, just use the first 16
        // bytes of the raw pubkey as a simplified actor — sign_event
        // doesn't care, it just signs whatever bytes you hand it.
        let pk_bytes = pubkey.to_bytes();
        let mut a = [0u8; 16];
        a.copy_from_slice(&pk_bytes[..16]);
        a
    });

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "d".into(),
        },
    };

    let event = sign_event(&payload, &signing_key).expect("sign");
    assert_eq!(event.id, payload.id);
    assert_eq!(event.actor, payload.actor);
    assert_eq!(event.kind, payload.kind);
    assert_eq!(event.countersig, None);

    // Verify the signature manually using ed25519-dalek directly.
    let signed_bytes = canonical_cbor_encode(&payload).expect("encode payload");
    pubkey
        .verify_strict(&signed_bytes, &ed25519_dalek::Signature::from_bytes(&event.sig))
        .expect("signature must verify against signer pubkey");
}
```

- [ ] **Step 6.2: Run test to verify it fails**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit sign_event 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: FAIL — `sign_event` and `EventPayload` not defined.

- [ ] **Step 6.3: Add `EventPayload` + `sign_event`**

Append to `src-tauri/src/community_membership.rs`:

```rust
use ed25519_dalek::{Signer, SigningKey};

use crate::owner_state_crypto::{canonical_cbor_encode, CryptoError};

/// The unsigned portion of a SignedMembershipEvent. Encoded canonically
/// and signed; the resulting signature populates SignedMembershipEvent.sig.
///
/// Keeping this as a separate type (vs. signing SignedMembershipEvent
/// itself with sig=zero) means the signed bytes are unambiguous —
/// there's no place to put "the actual sig went here" in the encoded
/// form. Mirrors how dm_envelope::SignedDmCidNotify is signed in
/// ZEB-227 (Phase 3b).
///
/// All 5 field keys are 2 chars to satisfy the same-length-keys
/// invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPayload {
    #[serde(
        rename = "id",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub id: EventId,

    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    #[serde(rename = "kn")]
    pub kind: MembershipEventKind,

    #[serde(rename = "ac")]
    pub actor: OwnerAddr,

    #[serde(rename = "at")]
    pub at: Hlc,
}

impl CanonicalPayloadSealed for EventPayload {}
impl CanonicalPayload for EventPayload {}

/// Sign an unsigned event payload with the actor's ed25519 key.
/// Returns a SignedMembershipEvent ready for canonical encoding +
/// publication. The countersig field is None — invite-only Joins
/// must be counter-signed via `attach_countersig` (Task 7).
///
/// Errors only on canonical CBOR encoding failure (vanishingly rare
/// for in-memory values — would indicate a broken serde impl).
pub fn sign_event(
    payload: &EventPayload,
    signing_key: &SigningKey,
) -> Result<SignedMembershipEvent, CryptoError> {
    let bytes = canonical_cbor_encode(payload)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedMembershipEvent {
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig,
        countersig: None,
    })
}
```

- [ ] **Step 6.4: Run test to verify it passes**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit sign_event 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: PASS.

- [ ] **Step 6.5: Run full gate sweep + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Then:

```bash
git add src-tauri/src/community_membership.rs src-tauri/tests/community_membership_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): EventPayload type + sign_event helper

EventPayload is the unsigned portion of SignedMembershipEvent (5
fields, no sig/countersig). Signing it canonically and verifying with
the actor's pubkey gives an unambiguous bytes-to-sign — no "the sig
goes here, encode around it" sentinel needed. Mirrors how
dm_envelope::SignedDmCidNotify works in ZEB-227 Phase 3b.

sign_event takes &EventPayload + &SigningKey, returns the
SignedMembershipEvent ready for publication. Counter-sig attachment
for invite-only Joins lands in Task 7.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `verify_signature` + `attach_countersig` + `verify_countersig` helpers

**Files:**
- Modify: `src-tauri/src/community_membership.rs`

The verify path takes a `SignedMembershipEvent` and the actor's pubkey and confirms the sig binds. Counter-sig logic is symmetric.

- [ ] **Step 7.1: Write the failing tests**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
use harmony_app::community_membership::{
    attach_countersig, verify_countersig, verify_signature, VerifyError,
};

#[test]
fn verify_signature_accepts_valid_event() {
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let pubkey = signing_key.verifying_key();

    let pk_bytes = pubkey.to_bytes();
    let mut actor_bytes = [0u8; 16];
    actor_bytes.copy_from_slice(&pk_bytes[..16]);
    let actor = OwnerAddr(actor_bytes);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc { wall_ms: 1000, logical: 0, device_id: "d".into() },
    };

    let event = sign_event(&payload, &signing_key).expect("sign");
    verify_signature(&event, &pubkey).expect("must verify");
}

#[test]
fn verify_signature_rejects_tampered_event() {
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let pubkey = signing_key.verifying_key();
    let pk_bytes = pubkey.to_bytes();
    let mut actor_bytes = [0u8; 16];
    actor_bytes.copy_from_slice(&pk_bytes[..16]);
    let actor = OwnerAddr(actor_bytes);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc { wall_ms: 1000, logical: 0, device_id: "d".into() },
    };

    let mut event = sign_event(&payload, &signing_key).expect("sign");
    // Tamper with the kind: flip Join to Leave. Sig was over the
    // original payload; verify must reject.
    event.kind = MembershipEventKind::Leave;

    let err = verify_signature(&event, &pubkey).expect_err("must reject tampered");
    assert!(matches!(err, VerifyError::SignatureInvalid));
}

#[test]
fn verify_signature_rejects_wrong_pubkey() {
    let signing_key_a = SigningKey::from_bytes(&[42u8; 32]);
    let signing_key_b = SigningKey::from_bytes(&[99u8; 32]);
    let pubkey_b = signing_key_b.verifying_key();

    let pk_bytes = signing_key_a.verifying_key().to_bytes();
    let mut actor_bytes = [0u8; 16];
    actor_bytes.copy_from_slice(&pk_bytes[..16]);
    let actor = OwnerAddr(actor_bytes);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc { wall_ms: 1000, logical: 0, device_id: "d".into() },
    };

    let event = sign_event(&payload, &signing_key_a).expect("sign");
    let err = verify_signature(&event, &pubkey_b).expect_err("must reject wrong pubkey");
    assert!(matches!(err, VerifyError::SignatureInvalid));
}

#[test]
fn attach_and_verify_countersig_round_trip() {
    let actor_key = SigningKey::from_bytes(&[42u8; 32]);
    let inviter_key = SigningKey::from_bytes(&[55u8; 32]);
    let inviter_pubkey = inviter_key.verifying_key();

    let pk_bytes = actor_key.verifying_key().to_bytes();
    let mut actor_bytes = [0u8; 16];
    actor_bytes.copy_from_slice(&pk_bytes[..16]);
    let actor = OwnerAddr(actor_bytes);

    let inviter_pk_bytes = inviter_pubkey.to_bytes();
    let mut inviter_addr_bytes = [0u8; 16];
    inviter_addr_bytes.copy_from_slice(&inviter_pk_bytes[..16]);
    let inviter = OwnerAddr(inviter_addr_bytes);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc { wall_ms: 1000, logical: 0, device_id: "d".into() },
    };

    let event = sign_event(&payload, &actor_key).expect("sign");
    let event_with_cs = attach_countersig(&event, inviter, &inviter_key).expect("countersign");

    assert!(event_with_cs.countersig.is_some());
    let cs = event_with_cs.countersig.as_ref().unwrap();
    assert_eq!(cs.signer, inviter);

    verify_countersig(&event_with_cs, &inviter_pubkey).expect("countersig must verify");
}

#[test]
fn verify_countersig_rejects_when_payload_changed_after_countersign() {
    let actor_key = SigningKey::from_bytes(&[42u8; 32]);
    let inviter_key = SigningKey::from_bytes(&[55u8; 32]);
    let inviter_pubkey = inviter_key.verifying_key();

    let pk_bytes = actor_key.verifying_key().to_bytes();
    let mut actor_bytes = [0u8; 16];
    actor_bytes.copy_from_slice(&pk_bytes[..16]);
    let actor = OwnerAddr(actor_bytes);

    let inviter_pk_bytes = inviter_pubkey.to_bytes();
    let mut inviter_addr_bytes = [0u8; 16];
    inviter_addr_bytes.copy_from_slice(&inviter_pk_bytes[..16]);
    let inviter = OwnerAddr(inviter_addr_bytes);

    let payload = EventPayload {
        id: [11u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor,
        at: Hlc { wall_ms: 1000, logical: 0, device_id: "d".into() },
    };

    let event = sign_event(&payload, &actor_key).expect("sign");
    let mut event_with_cs = attach_countersig(&event, inviter, &inviter_key).expect("countersign");

    // Mutate the payload after counter-signing: change `at`. The
    // countersig was over the original payload bytes; verify must reject.
    event_with_cs.at = Hlc { wall_ms: 9999, logical: 0, device_id: "d".into() };

    let err = verify_countersig(&event_with_cs, &inviter_pubkey)
        .expect_err("must reject mutated payload");
    assert!(matches!(err, VerifyError::SignatureInvalid));
}
```

- [ ] **Step 7.2: Run tests to verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit verify 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: FAIL — `verify_signature`, `attach_countersig`, `verify_countersig`, `VerifyError` not defined.

- [ ] **Step 7.3: Add the verify + countersig functions + error type**

Append to `src-tauri/src/community_membership.rs`:

```rust
use ed25519_dalek::{Signature, VerifyingKey};

/// Errors that can fire during membership-event verification.
/// Wraps everything verify_event needs to surface — signature failure,
/// power insufficiency, counter-sig requirement, etc. Concrete variants
/// added per-task; Task 7 ships SignatureInvalid + CounterSigRequired
/// + CounterSigInvalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    SignatureInvalid,
    CounterSigRequired,
    CounterSigInvalid,
    CounterSigPowerInsufficient,
    ActorPowerInsufficient,
    KickTargetPowerNotLower,
    EncodeError(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::SignatureInvalid => write!(f, "signature invalid"),
            VerifyError::CounterSigRequired => write!(f, "invite-only Join requires countersig"),
            VerifyError::CounterSigInvalid => write!(f, "countersig invalid"),
            VerifyError::CounterSigPowerInsufficient => {
                write!(f, "countersig signer's power is below invite_threshold")
            }
            VerifyError::ActorPowerInsufficient => {
                write!(f, "actor's power is below the action's threshold")
            }
            VerifyError::KickTargetPowerNotLower => {
                write!(f, "kick requires actor.power > target.power")
            }
            VerifyError::EncodeError(s) => write!(f, "canonical encode failed: {s}"),
        }
    }
}

impl std::error::Error for VerifyError {}

impl From<CryptoError> for VerifyError {
    fn from(e: CryptoError) -> Self {
        VerifyError::EncodeError(format!("{e:?}"))
    }
}

/// Verify the actor's signature on a SignedMembershipEvent.
/// Returns Ok(()) only if the sig is valid for the actor's pubkey
/// over the canonical encoding of the event's payload (excluding sig
/// and countersig).
///
/// Use `verify_strict` (not `verify`) — strict mode rejects
/// signatures with non-canonical S values and small-order R points,
/// matching the EdDSA RFC 8032 strict subset and protecting against
/// signature malleability attacks. Mirrors how dm_envelope verifies
/// its own signed payloads.
pub fn verify_signature(
    event: &SignedMembershipEvent,
    actor_pubkey: &VerifyingKey,
) -> Result<(), VerifyError> {
    let payload = EventPayload {
        id: event.id,
        community_id: event.community_id,
        kind: event.kind.clone(),
        actor: event.actor,
        at: event.at.clone(),
    };
    let bytes = canonical_cbor_encode(&payload)?;
    let sig = Signature::from_bytes(&event.sig);
    actor_pubkey
        .verify_strict(&bytes, &sig)
        .map_err(|_| VerifyError::SignatureInvalid)
}

/// Attach a counter-signature to a Join event for an invite-only
/// community. The signer's key signs the SAME canonical bytes the
/// actor signed (the EventPayload), so the countersig binds to the
/// exact joiner event, not just to the community ID.
pub fn attach_countersig(
    event: &SignedMembershipEvent,
    signer: OwnerAddr,
    signer_key: &SigningKey,
) -> Result<SignedMembershipEvent, CryptoError> {
    let payload = EventPayload {
        id: event.id,
        community_id: event.community_id,
        kind: event.kind.clone(),
        actor: event.actor,
        at: event.at.clone(),
    };
    let bytes = canonical_cbor_encode(&payload)?;
    let sig = signer_key.sign(&bytes).to_bytes();
    let mut out = event.clone();
    out.countersig = Some(CounterSignature { signer, sig });
    Ok(out)
}

/// Verify the counter-signature on an event. Returns Ok(()) if a
/// countersig is present AND its signer's pubkey verifies the
/// signature over the same canonical bytes as the actor signed.
///
/// Returns CounterSigInvalid if the countersig is missing OR if the
/// signature doesn't verify. Power-level checking on the signer
/// happens elsewhere (verify_event in Task 10) — this function is
/// purely cryptographic.
pub fn verify_countersig(
    event: &SignedMembershipEvent,
    signer_pubkey: &VerifyingKey,
) -> Result<(), VerifyError> {
    let cs = event
        .countersig
        .as_ref()
        .ok_or(VerifyError::CounterSigRequired)?;
    let payload = EventPayload {
        id: event.id,
        community_id: event.community_id,
        kind: event.kind.clone(),
        actor: event.actor,
        at: event.at.clone(),
    };
    let bytes = canonical_cbor_encode(&payload)?;
    let sig = Signature::from_bytes(&cs.sig);
    signer_pubkey
        .verify_strict(&bytes, &sig)
        .map_err(|_| VerifyError::CounterSigInvalid)
}
```

- [ ] **Step 7.4: Run tests to verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit verify 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: 5/5 passing.

- [ ] **Step 7.5: Run full gate sweep + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Then:

```bash
git add src-tauri/src/community_membership.rs src-tauri/tests/community_membership_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): verify_signature + attach/verify_countersig + VerifyError

verify_signature uses ed25519-dalek's verify_strict (rejects non-
canonical S values and small-order R, matches RFC 8032 strict subset
— same anti-malleability defense dm_envelope uses).

attach_countersig signs the SAME canonical EventPayload bytes the
actor signed, so the countersig binds to the exact joiner event
(not just to the community ID — protects against an admin reusing
a countersig across different join attempts from the same person).

VerifyError covers the verification-rule space verify_event will
surface in Task 10. Power-level checks land in Task 10.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `MaterializedMembership` + `MemberState` types + `POWER_THRESHOLDS`

**Files:**
- Modify: `src-tauri/src/community_membership.rs`

Pure types + constants. No replay logic yet.

- [ ] **Step 8.1: Write the failing test**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
use harmony_app::community_membership::{
    MaterializedMembership, MemberState, MemberStatus, PowerThresholds, POWER_THRESHOLDS,
};

#[test]
fn materialized_membership_is_constructible_and_default_empty() {
    let m = MaterializedMembership::default();
    assert!(m.members.is_empty());
    assert!(m.power_levels.is_empty());
}

#[test]
fn member_status_round_trips_through_canonical_cbor() {
    use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
    let statuses = [
        MemberStatus::Joined,
        MemberStatus::Invited,
        MemberStatus::Left,
        MemberStatus::Banned,
    ];
    for s in &statuses {
        let bytes = canonical_cbor_encode(s).expect("encode");
        let decoded: MemberStatus = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(decoded, *s);
    }
}

#[test]
fn power_thresholds_match_spec_defaults() {
    assert_eq!(POWER_THRESHOLDS.invite, 0);
    assert_eq!(POWER_THRESHOLDS.kick, 50);
    assert_eq!(POWER_THRESHOLDS.set_power, 100);
    assert_eq!(POWER_THRESHOLDS.max, 100);
}

#[test]
fn power_thresholds_struct_constructible() {
    let custom = PowerThresholds {
        invite: 10,
        kick: 60,
        set_power: 90,
        max: 100,
    };
    assert_eq!(custom.invite, 10);
}
```

- [ ] **Step 8.2: Run tests to verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit materialized_membership member_status power_thresholds 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: FAIL — types not defined.

- [ ] **Step 8.3: Add the materialized-state types**

Append to `src-tauri/src/community_membership.rs`:

```rust
use std::collections::BTreeMap;

/// Materialized view computed from a community's signed event log.
/// Pure function of the log + the community Space's admin_addr (per
/// the bootstrap rule). Re-computed when needed; caching belongs at
/// the call site (Phase 2's CommunityState owns the cache + version
/// counter, mirroring the inbox_entries_for_space pattern from
/// owner_state_crdt.rs).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedMembership {
    pub members: BTreeMap<OwnerAddr, MemberState>,
    /// Per-actor power level. Unset key = 0 = default. The community
    /// admin (Space.admin_addr) starts at 100 implicitly via the
    /// bootstrap rule — see `materialize` (Task 9). SetPower events
    /// override.
    pub power_levels: BTreeMap<OwnerAddr, u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberState {
    #[serde(rename = "st")]
    pub status: MemberStatus,
    #[serde(rename = "ja")]
    pub joined_at: Hlc,
    #[serde(rename = "la", skip_serializing_if = "Option::is_none", default)]
    pub left_at: Option<Hlc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemberStatus {
    #[serde(rename = "j")]
    Joined,
    #[serde(rename = "i")]
    Invited,
    #[serde(rename = "l")]
    Left,
    #[serde(rename = "b")]
    Banned,
}

impl CanonicalPayloadSealed for MaterializedMembership {}
impl CanonicalPayload for MaterializedMembership {}
impl CanonicalPayloadSealed for MemberState {}
impl CanonicalPayload for MemberState {}
impl CanonicalPayloadSealed for MemberStatus {}
impl CanonicalPayload for MemberStatus {}

/// Per-community power thresholds. v1 hardcoded; per-community
/// customization is deferred to ZEB-251.
#[derive(Debug, Clone, Copy)]
pub struct PowerThresholds {
    pub invite: u8,
    pub kick: u8,
    pub set_power: u8,
    pub max: u8,
}

/// Sub-C v1 hardcoded defaults — see ZEB-217 spec §"Power thresholds".
pub const POWER_THRESHOLDS: PowerThresholds = PowerThresholds {
    invite: 0,
    kick: 50,
    set_power: 100,
    max: 100,
};
```

- [ ] **Step 8.4: Run tests + gates + commit**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit materialized_membership member_status power_thresholds 2>&1 | tail -10
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Then:

```bash
git add src-tauri/src/community_membership.rs src-tauri/tests/community_membership_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): MaterializedMembership + MemberState + POWER_THRESHOLDS

Pure types + the v1 hardcoded power-threshold constants. Per-community
customization is deferred to ZEB-251.

MaterializedMembership uses BTreeMap (deterministic iteration order
matters for canonical CBOR if ever serialized across the wire — the
v1 use is purely in-memory caching, but defensive). Replay logic
that populates these maps lands in Task 9.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: `materialize` — replay events into MaterializedMembership (per-event-kind effects)

**Files:**
- Modify: `src-tauri/src/community_membership.rs`

This task implements the materialization function ASSUMING all events are valid (no power-level checks during replay). Verification + power-rule enforcement comes in Task 10. The split keeps each function single-responsibility.

- [ ] **Step 9.1: Write the failing tests**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
use harmony_app::community_membership::materialize;

fn make_signed(
    id: u8,
    kind: MembershipEventKind,
    actor: OwnerAddr,
    at_ms: u64,
) -> SignedMembershipEvent {
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let payload = EventPayload {
        id: [id; 16],
        community_id: SpaceId([3u8; 16]),
        kind,
        actor,
        at: Hlc { wall_ms: at_ms, logical: 0, device_id: "d".into() },
    };
    sign_event(&payload, &signing_key).expect("sign")
}

#[test]
fn materialize_join_marks_actor_joined() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Join, alice, 200),
    ];

    let m = materialize(&events, admin);
    assert_eq!(m.members.get(&admin).map(|s| s.status), Some(MemberStatus::Joined));
    assert_eq!(m.members.get(&alice).map(|s| s.status), Some(MemberStatus::Joined));
}

#[test]
fn materialize_leave_marks_actor_left_with_left_at() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Join, alice, 200),
        make_signed(3, MembershipEventKind::Leave, alice, 300),
    ];

    let m = materialize(&events, admin);
    let alice_state = m.members.get(&alice).expect("alice present");
    assert_eq!(alice_state.status, MemberStatus::Left);
    assert_eq!(alice_state.left_at.as_ref().map(|h| h.wall_ms), Some(300));
}

#[test]
fn materialize_kick_marks_target_banned() {
    let admin = OwnerAddr([100u8; 16]);
    let bob = OwnerAddr([2u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Join, bob, 200),
        make_signed(
            3,
            MembershipEventKind::Kick { target: bob, reason: Some("spam".into()) },
            admin,
            300,
        ),
    ];

    let m = materialize(&events, admin);
    let bob_state = m.members.get(&bob).expect("bob present");
    assert_eq!(bob_state.status, MemberStatus::Banned);
    assert_eq!(bob_state.left_at.as_ref().map(|h| h.wall_ms), Some(300));
}

#[test]
fn materialize_invite_marks_target_invited() {
    let admin = OwnerAddr([100u8; 16]);
    let carol = OwnerAddr([3u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(
            2,
            MembershipEventKind::Invite { target: carol },
            admin,
            200,
        ),
    ];

    let m = materialize(&events, admin);
    let carol_state = m.members.get(&carol).expect("carol present");
    assert_eq!(carol_state.status, MemberStatus::Invited);
    assert!(carol_state.left_at.is_none());
}

#[test]
fn materialize_setpower_updates_power_level() {
    let admin = OwnerAddr([100u8; 16]);
    let bob = OwnerAddr([2u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Join, bob, 200),
        make_signed(
            3,
            MembershipEventKind::SetPower { target: bob, level: 75 },
            admin,
            300,
        ),
    ];

    let m = materialize(&events, admin);
    assert_eq!(m.power_levels.get(&bob).copied(), Some(75));
}

#[test]
fn materialize_bootstrap_grants_admin_power_100_even_with_zero_events() {
    let admin = OwnerAddr([100u8; 16]);
    let m = materialize(&[], admin);
    assert_eq!(m.power_levels.get(&admin).copied(), Some(100));
    // But admin is NOT a member until they Join (intentional — admin
    // is a power designation, not a membership status).
    assert!(m.members.is_empty());
}

#[test]
fn materialize_setpower_overrides_admin_bootstrap() {
    let admin = OwnerAddr([100u8; 16]);
    let new_admin = OwnerAddr([99u8; 16]);

    let events = vec![
        make_signed(1, MembershipEventKind::Join, admin, 100),
        make_signed(2, MembershipEventKind::Join, new_admin, 200),
        make_signed(
            3,
            MembershipEventKind::SetPower { target: new_admin, level: 100 },
            admin,
            300,
        ),
        make_signed(
            4,
            MembershipEventKind::SetPower { target: admin, level: 0 },
            admin,
            400,
        ),
    ];

    let m = materialize(&events, admin);
    assert_eq!(m.power_levels.get(&admin).copied(), Some(0));
    assert_eq!(m.power_levels.get(&new_admin).copied(), Some(100));
}

#[test]
fn materialize_replays_in_hlc_order_not_input_order() {
    // Events arrive in a different order than they should apply.
    // Materialization must re-sort by HLC.
    let admin = OwnerAddr([100u8; 16]);
    let bob = OwnerAddr([2u8; 16]);

    let events = vec![
        // Out of order: kick at 300 listed BEFORE join at 200.
        make_signed(
            3,
            MembershipEventKind::Kick { target: bob, reason: None },
            admin,
            300,
        ),
        make_signed(2, MembershipEventKind::Join, bob, 200),
        make_signed(1, MembershipEventKind::Join, admin, 100),
    ];

    let m = materialize(&events, admin);
    // Despite the input order, the replay walks HLC ascending, so:
    // 100: admin joins, 200: bob joins, 300: bob is kicked.
    assert_eq!(m.members.get(&bob).map(|s| s.status), Some(MemberStatus::Banned));
}
```

- [ ] **Step 9.2: Run tests to verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit materialize 2>&1 | tail -15
echo "TEST_EXIT=$?"
```

Expected: FAIL — `materialize` not defined.

- [ ] **Step 9.3: Implement `materialize`**

Append to `src-tauri/src/community_membership.rs`:

```rust
/// Replay a community's signed event log into a MaterializedMembership.
///
/// Implements the spec's "Materialization rules" verbatim:
///
/// 1. Bootstrap: power_levels[admin_addr] = 100 BEFORE replaying any
///    events. Admin can later SetPower themselves to a different value.
/// 2. Events are applied in HLC ascending order, regardless of input
///    order — the input may arrive partial-ordered from DAG-sync.
/// 3. Per-kind effects:
///    - Join: members[actor] = Joined / joined_at: at
///    - Leave: members[actor].status = Left, .left_at = at
///    - Invite { target }: members[target] = Invited / joined_at: at
///    - Kick { target }: members[target].status = Banned, .left_at = at
///    - SetPower { target, level }: power_levels[target] = level
///
/// Pure function — does NOT verify signatures or power rules. That's
/// `verify_event` (Task 10). Materialization assumes pre-verified
/// events; the Phase 2 sync layer rejects unverified events before
/// they reach this function.
pub fn materialize(
    events: &[SignedMembershipEvent],
    admin_addr: OwnerAddr,
) -> MaterializedMembership {
    let mut m = MaterializedMembership::default();

    // Bootstrap: admin holds power 100 implicitly. SetPower events
    // (replayed below) can override.
    m.power_levels.insert(admin_addr, 100);

    // HLC-sort. We don't assume the input is sorted because DAG-sync
    // delivers events partial-ordered. Cloning is fine here — the
    // event vec is small (community sizes are bounded; even very
    // active communities have O(thousands) of events at the long
    // tail, not millions).
    let mut sorted: Vec<&SignedMembershipEvent> = events.iter().collect();
    sorted.sort_by(|a, b| {
        // HLC tuple ordering: (wall_ms, logical, device_id) ascending.
        let key_a = (a.at.wall_ms, a.at.logical, &a.at.device_id);
        let key_b = (b.at.wall_ms, b.at.logical, &b.at.device_id);
        key_a.cmp(&key_b)
    });

    for event in sorted {
        match &event.kind {
            MembershipEventKind::Join => {
                m.members.insert(
                    event.actor,
                    MemberState {
                        status: MemberStatus::Joined,
                        joined_at: event.at.clone(),
                        left_at: None,
                    },
                );
            }
            MembershipEventKind::Leave => {
                if let Some(s) = m.members.get_mut(&event.actor) {
                    s.status = MemberStatus::Left;
                    s.left_at = Some(event.at.clone());
                } else {
                    // No prior Join → silent skip. A Leave from a
                    // non-member is a no-op; verify_event (Task 10)
                    // can choose to reject this case if we want
                    // stricter semantics, but materialization
                    // tolerates it because the alternative (unwrap
                    // or insert-with-Left) would corrupt state from
                    // a malformed event.
                }
            }
            MembershipEventKind::Invite { target } => {
                m.members.entry(*target).or_insert(MemberState {
                    status: MemberStatus::Invited,
                    joined_at: event.at.clone(),
                    left_at: None,
                });
                // If target was already Joined/Left/Banned, Invite is
                // a no-op — they're already past the "invited" stage.
            }
            MembershipEventKind::Kick { target, .. } => {
                let s = m.members.entry(*target).or_insert(MemberState {
                    status: MemberStatus::Banned,
                    joined_at: event.at.clone(),
                    left_at: Some(event.at.clone()),
                });
                s.status = MemberStatus::Banned;
                s.left_at = Some(event.at.clone());
            }
            MembershipEventKind::SetPower { target, level } => {
                m.power_levels.insert(*target, *level);
            }
        }
    }

    m
}
```

- [ ] **Step 9.4: Run tests to verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit materialize 2>&1 | tail -15
echo "TEST_EXIT=$?"
```

Expected: 8/8 passing.

- [ ] **Step 9.5: Run full gate sweep + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Then:

```bash
git add src-tauri/src/community_membership.rs src-tauri/tests/community_membership_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): materialize — replay events into MaterializedMembership

Pure function, no verification. Implements the spec's Materialization
rules verbatim:

* Bootstrap: power_levels[admin_addr] = 100 before any events
* Events replayed in HLC ascending order (DAG-sync delivers partial-
  ordered, so we re-sort defensively even when caller may have sorted)
* Per-kind effects per spec — Join/Leave/Invite/Kick/SetPower

Verification + power-rule enforcement is Task 10. This split keeps
materialize single-responsibility (pure replay) and verify_event
single-responsibility (cryptographic + power-rule gating).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: `verify_event` — full verification with power rules + countersig power check

**Files:**
- Modify: `src-tauri/src/community_membership.rs`

`verify_event` is what the Phase 2 sync layer will call before inserting any received event into the local Prolly Tree. Inputs: the event, the prior materialized state, the actor's pubkey lookup, the community Space's `is_invite_only` flag, the optional inviter pubkey lookup (if a countersig is present).

- [ ] **Step 10.1: Write the failing tests**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
use harmony_app::community_membership::{verify_event, VerifyContext};

#[test]
fn verify_event_accepts_valid_join_in_open_community() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    // Pre-existing materialized state: admin has joined.
    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 100)],
        admin,
    );

    // Alice signs her join event.
    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc { wall_ms: 200, logical: 0, device_id: "d".into() },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let alice_pubkey = alice_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: None,
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept");
}

#[test]
fn verify_event_rejects_invite_only_join_without_countersig() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    let prior_state = materialize(&[], admin);

    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc { wall_ms: 200, logical: 0, device_id: "d".into() },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let alice_pubkey = alice_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: true,    // invite-only requires countersig
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::CounterSigRequired);
}

#[test]
fn verify_event_accepts_invite_only_join_with_valid_countersig() {
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    // Prior state: admin has joined and holds power 100 (bootstrap).
    let prior_state = materialize(
        &[make_signed(1, MembershipEventKind::Join, admin, 100)],
        admin,
    );

    // Alice signs join, admin counter-signs.
    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc { wall_ms: 200, logical: 0, device_id: "d".into() },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");
    let event = attach_countersig(&event, admin, &admin_key).expect("countersign");

    let alice_pubkey = alice_key.verifying_key();
    let admin_pubkey = admin_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: true,
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: Some(&admin_pubkey),
    };

    verify_event(&event, &prior_state, &ctx).expect("must accept");
}

#[test]
fn verify_event_rejects_kick_when_actor_power_below_threshold() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let bob = OwnerAddr([2u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    // Alice is a member with default power 0. She tries to kick bob.
    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(2, MembershipEventKind::Join, alice, 200),
            make_signed(3, MembershipEventKind::Join, bob, 300),
        ],
        admin,
    );

    let payload = EventPayload {
        id: [4u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Kick { target: bob, reason: None },
        actor: alice,
        at: Hlc { wall_ms: 400, logical: 0, device_id: "d".into() },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let alice_pubkey = alice_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::ActorPowerInsufficient);
}

#[test]
fn verify_event_rejects_kick_when_target_power_equals_actor() {
    let admin = OwnerAddr([100u8; 16]);
    let admin_key = SigningKey::from_bytes(&[100u8; 32]);
    let bob = OwnerAddr([2u8; 16]);
    let admin2 = OwnerAddr([99u8; 16]);

    // Both admin and admin2 have power 100. admin tries to kick admin2
    // (same power level → MUST reject; otherwise kick wars at the top).
    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(2, MembershipEventKind::Join, admin2, 200),
            make_signed(
                3,
                MembershipEventKind::SetPower { target: admin2, level: 100 },
                admin,
                300,
            ),
        ],
        admin,
    );

    let payload = EventPayload {
        id: [4u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Kick { target: admin2, reason: None },
        actor: admin,
        at: Hlc { wall_ms: 400, logical: 0, device_id: "d".into() },
    };
    let event = sign_event(&payload, &admin_key).expect("sign");

    let admin_pubkey = admin_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &admin_pubkey,
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::KickTargetPowerNotLower);
}

#[test]
fn verify_event_rejects_setpower_when_actor_power_insufficient() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let bob = OwnerAddr([2u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);

    // Alice (power 0) tries to set bob's power. Must reject.
    let prior_state = materialize(
        &[
            make_signed(1, MembershipEventKind::Join, admin, 100),
            make_signed(2, MembershipEventKind::Join, alice, 200),
        ],
        admin,
    );

    let payload = EventPayload {
        id: [3u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::SetPower { target: bob, level: 50 },
        actor: alice,
        at: Hlc { wall_ms: 300, logical: 0, device_id: "d".into() },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let alice_pubkey = alice_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &alice_pubkey,
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::ActorPowerInsufficient);
}

// Note: VerifyError::CounterSigPowerInsufficient is unreachable
// under v1's hardcoded POWER_THRESHOLDS.invite = 0 because every
// owner address (whether a joined member or not) materializes to
// power ≥ 0. The variant is reserved for ZEB-251 (per-community
// threshold customization). When ZEB-251 ships, add a test here
// that constructs a custom-threshold scenario and exercises this
// rejection path.

#[test]
fn verify_event_rejects_when_actor_signature_invalid() {
    let admin = OwnerAddr([100u8; 16]);
    let alice = OwnerAddr([1u8; 16]);
    let alice_key = SigningKey::from_bytes(&[1u8; 32]);
    let bob_key = SigningKey::from_bytes(&[2u8; 32]);  // different signer

    let prior_state = materialize(&[make_signed(1, MembershipEventKind::Join, admin, 100)], admin);

    // Alice signs her own join, but we tell verify_event her pubkey
    // is bob's. Signature must reject.
    let payload = EventPayload {
        id: [2u8; 16],
        community_id: SpaceId([3u8; 16]),
        kind: MembershipEventKind::Join,
        actor: alice,
        at: Hlc { wall_ms: 200, logical: 0, device_id: "d".into() },
    };
    let event = sign_event(&payload, &alice_key).expect("sign");

    let bob_pubkey = bob_key.verifying_key();
    let ctx = VerifyContext {
        is_invite_only: false,
        actor_pubkey: &bob_pubkey,    // wrong pubkey for actor
        countersigner_pubkey: None,
    };

    let err = verify_event(&event, &prior_state, &ctx).expect_err("must reject");
    assert_eq!(err, VerifyError::SignatureInvalid);
}
```

- [ ] **Step 10.2: Run tests to verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit verify_event 2>&1 | tail -15
echo "TEST_EXIT=$?"
```

Expected: FAIL — `verify_event` and `VerifyContext` not defined.

- [ ] **Step 10.3: Implement `verify_event` + `VerifyContext`**

Append to `src-tauri/src/community_membership.rs`:

```rust
/// Caller-provided context for verify_event. Carries the prior
/// materialized state (so the function is pure — verify_event doesn't
/// load state from anywhere) plus the pubkeys needed for signature
/// checking.
///
/// `actor_pubkey` MUST be the ed25519 verifying key for `event.actor`.
/// Sub-A's owner-key cache is the canonical source — verify_event
/// itself doesn't resolve OwnerAddr → pubkey, the caller does.
///
/// `countersigner_pubkey` is None for open communities and for non-
/// Join events. For invite-only Joins it MUST be Some, with the key
/// matching `event.countersig.signer`.
pub struct VerifyContext<'a> {
    pub is_invite_only: bool,
    pub actor_pubkey: &'a VerifyingKey,
    pub countersigner_pubkey: Option<&'a VerifyingKey>,
}

/// Full membership-event verification per ZEB-217 spec §"Verification".
///
/// Run BEFORE materializing an event into the CRDT. Caller must:
/// 1. Compute the prior materialized state (using `materialize` over
///    all events strictly before `event` in HLC order).
/// 2. Resolve `event.actor` → pubkey via Sub-A's owner-key cache.
/// 3. For invite-only Joins, also resolve the countersig signer.
///
/// Verifies in this order:
/// 1. Actor's signature on the event payload.
/// 2. For invite-only Join: countersig present + valid + signer's
///    power ≥ invite_threshold.
/// 3. Action-specific power rules:
///    - Kick: actor's power ≥ kick_threshold AND > target's power
///    - SetPower: actor's power ≥ set_power_threshold
///    - Invite: actor's power ≥ invite_threshold (currently 0 — any
///      joined member can invite)
///    - Join, Leave: no power check (anyone can leave; join is gated
///      by invite-only countersig logic above)
///
/// Power lookups treat unset entries as 0 (the default per the spec).
/// Bootstrap (admin_addr → 100) is already baked into prior_state by
/// `materialize`, so the lookup here is uniform across all actors.
pub fn verify_event(
    event: &SignedMembershipEvent,
    prior_state: &MaterializedMembership,
    ctx: &VerifyContext,
) -> Result<(), VerifyError> {
    // 1. Actor's signature must verify.
    verify_signature(event, ctx.actor_pubkey)?;

    // 2. For invite-only Joins, countersig is required + valid + the
    //    signer must have sufficient power at the prior state's snapshot.
    //
    // Note: under v1's hardcoded POWER_THRESHOLDS.invite = 0, the
    // power check below is unreachable (any owner addr defaults to
    // power 0 ≥ 0). The check exists because per-community threshold
    // customization (ZEB-251) will make it firable when invite_threshold
    // > 0. Keeping the rule structurally in place now means ZEB-251
    // doesn't need to revisit verify_event.
    if matches!(event.kind, MembershipEventKind::Join) && ctx.is_invite_only {
        let cs = event
            .countersig
            .as_ref()
            .ok_or(VerifyError::CounterSigRequired)?;
        let cs_pubkey = ctx
            .countersigner_pubkey
            .ok_or(VerifyError::CounterSigRequired)?;
        verify_countersig(event, cs_pubkey)?;

        let signer_power = prior_state.power_levels.get(&cs.signer).copied().unwrap_or(0);
        if signer_power < POWER_THRESHOLDS.invite {
            return Err(VerifyError::CounterSigPowerInsufficient);
        }
    }

    // 3. Per-kind power rules.
    let actor_power = prior_state.power_levels.get(&event.actor).copied().unwrap_or(0);
    match &event.kind {
        MembershipEventKind::Join | MembershipEventKind::Leave => {
            // No power check — Join is gated by the countersig logic
            // above (invite-only) or unconditionally allowed (open).
            // Leave is always allowed for the actor themselves.
        }
        MembershipEventKind::Invite { .. } => {
            if actor_power < POWER_THRESHOLDS.invite {
                return Err(VerifyError::ActorPowerInsufficient);
            }
        }
        MembershipEventKind::Kick { target, .. } => {
            if actor_power < POWER_THRESHOLDS.kick {
                return Err(VerifyError::ActorPowerInsufficient);
            }
            let target_power = prior_state
                .power_levels
                .get(target)
                .copied()
                .unwrap_or(0);
            if actor_power <= target_power {
                return Err(VerifyError::KickTargetPowerNotLower);
            }
        }
        MembershipEventKind::SetPower { .. } => {
            if actor_power < POWER_THRESHOLDS.set_power {
                return Err(VerifyError::ActorPowerInsufficient);
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 10.4: Run tests to verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_membership_unit verify_event 2>&1 | tail -15
echo "TEST_EXIT=$?"
```

Expected: 7/7 passing.

- [ ] **Step 10.5: Run full gate sweep + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Then:

```bash
git add src-tauri/src/community_membership.rs src-tauri/tests/community_membership_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): verify_event with full power-rule + countersig logic

Caller passes prior MaterializedMembership state + pubkey lookups via
VerifyContext. verify_event:

1. Verifies actor's signature
2. For invite-only Joins: requires countersig present + valid + signer's
   power ≥ invite_threshold (looked up from prior_state at HLC time)
3. Applies per-kind power rules:
   - Kick: actor power ≥ kick_threshold AND > target's power
   - SetPower: actor power ≥ set_power_threshold
   - Invite: actor power ≥ invite_threshold (v1: 0)
   - Join/Leave: no power check (gated elsewhere)

The CounterSigPowerInsufficient variant is reserved but unreachable
under v1's hardcoded invite_threshold=0; it'll be exercised when
ZEB-251 ships per-community threshold customization. Documented
inline in the test file.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: `CommunityInvitePayload` + `InviteToken` types (in `community_invite.rs`)

**Files:**
- Create: `src-tauri/src/community_invite.rs`
- Modify: `src-tauri/src/lib.rs` — add `pub mod community_invite;`

Phase 1 ships ONLY the type definitions and canonical CBOR — no encoding/decoding helpers (those are Phase 3 with `generate_invite`), no Reticulum send/receive (Phase 4).

- [ ] **Step 11.1: Write the failing tests**

Create `src-tauri/tests/community_invite_unit.rs`:

```rust
//! Unit tests for community_invite.rs Phase 1 types.

use harmony_app::community_invite::{CommunityInvitePayload, InviteToken};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};

#[test]
fn community_invite_payload_round_trips_open_form() {
    let p = CommunityInvitePayload {
        community_id: SpaceId([1u8; 16]),
        membership_key: MembershipKey::new([2u8; 32]),
        admin_addr: OwnerAddr([3u8; 16]),
        community_name: "harmony-design".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };

    let bytes = canonical_cbor_encode(&p).expect("encode");
    let decoded: CommunityInvitePayload = canonical_cbor_decode(&bytes).expect("decode");

    assert_eq!(decoded.community_id, p.community_id);
    assert_eq!(decoded.community_name, p.community_name);
    assert!(!decoded.is_invite_only);
    assert!(decoded.invite_token.is_none());
}

#[test]
fn community_invite_payload_round_trips_invite_only_form() {
    let token = InviteToken {
        inviter: OwnerAddr([5u8; 16]),
        invitee_hint: Some(OwnerAddr([6u8; 16])),
        minted_at: Hlc { wall_ms: 100, logical: 0, device_id: "d".into() },
        sig: [0xCC; 64],
    };

    let p = CommunityInvitePayload {
        community_id: SpaceId([1u8; 16]),
        membership_key: MembershipKey::new([2u8; 32]),
        admin_addr: OwnerAddr([3u8; 16]),
        community_name: "private".into(),
        is_invite_only: true,
        expires_at: Some(Hlc { wall_ms: 9999, logical: 0, device_id: "d".into() }),
        invite_token: Some(token.clone()),
    };

    let bytes = canonical_cbor_encode(&p).expect("encode");
    let decoded: CommunityInvitePayload = canonical_cbor_decode(&bytes).expect("decode");

    assert_eq!(decoded.is_invite_only, true);
    assert_eq!(
        decoded.invite_token.as_ref().map(|t| t.invitee_hint),
        Some(Some(OwnerAddr([6u8; 16])))
    );
}

#[test]
fn invite_token_round_trips_with_hint_none() {
    let t = InviteToken {
        inviter: OwnerAddr([1u8; 16]),
        invitee_hint: None,    // open invite — anyone can redeem
        minted_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        sig: [0u8; 64],
    };

    let bytes = canonical_cbor_encode(&t).expect("encode");
    let decoded: InviteToken = canonical_cbor_decode(&bytes).expect("decode");

    assert_eq!(decoded.invitee_hint, None);
    assert_eq!(decoded.inviter, OwnerAddr([1u8; 16]));
}
```

- [ ] **Step 11.2: Run tests to verify they fail**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_invite_unit 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: FAIL — module doesn't exist.

- [ ] **Step 11.3: Create `community_invite.rs`**

Create `src-tauri/src/community_invite.rs`:

```rust
//! Community invite payload types — ZEB-217 Sub-C Phase 1.
//!
//! Phase 1 ships ONLY the type definitions + canonical CBOR. Encoding
//! to a `harmony://invite/...` URL (base64url + URL prefix) lives in
//! Phase 3 alongside the `generate_invite` IPC. Reticulum send/receive
//! for invite-only counter-sig flow lives in Phase 4.
//!
//! See `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md`
//! §"Invite system".

use serde::{Deserialize, Serialize};

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, MembershipKey, OwnerAddr, SpaceId,
};

/// The full payload an invite link carries. Encoded as canonical CBOR
/// (~120-180 bytes), then base64url-encoded into the URL form
/// `harmony://invite/{base64url}` (encoding helpers land in Phase 3).
///
/// Wire format: 7-key map. Field codes are 2 chars to satisfy the
/// same-length-keys CBOR invariant at this nesting level. Optional
/// fields use skip_serializing_if so non-applicable variants
/// (e.g., open communities have invite_token=None) don't bloat the
/// encoded URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityInvitePayload {
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    #[serde(rename = "mk")]
    pub membership_key: MembershipKey,

    #[serde(rename = "ad")]
    pub admin_addr: OwnerAddr,

    #[serde(rename = "nm")]
    pub community_name: String,

    #[serde(rename = "io")]
    pub is_invite_only: bool,

    #[serde(rename = "ex", skip_serializing_if = "Option::is_none", default)]
    pub expires_at: Option<Hlc>,

    /// Required for invite-only redemption (carries the inviter's
    /// pre-signed authorization). Optional for open communities (could
    /// still be present as an authenticity hint, but not required).
    #[serde(rename = "tk", skip_serializing_if = "Option::is_none", default)]
    pub invite_token: Option<InviteToken>,
}

/// The inviter's pre-signed authorization, embedded in the invite link
/// for invite-only communities. The redeemer presents this via
/// Reticulum to any community member with `power ≥ invite_threshold`,
/// who counter-signs the resulting Join event (Phase 4).
///
/// `sig` covers the canonical-CBOR encoding of `(inviter, invitee_hint,
/// minted_at, expires_at_in_outer_payload)` — bound to the outer
/// CommunityInvitePayload's expires_at so a token can't be replayed
/// past its outer expiry. (Sig construction lives in Phase 3 with
/// `generate_invite`.)
///
/// Wire format: 4-key map. Field codes 2 chars per the same-length-
/// keys rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteToken {
    #[serde(rename = "iv")]
    pub inviter: OwnerAddr,

    /// `None` = open redemption (anyone with the link can use this
    /// token). `Some(addr)` = bound to that owner addr; the joiner's
    /// signed Join.actor MUST equal this hint or verification rejects.
    #[serde(rename = "ih", skip_serializing_if = "Option::is_none", default)]
    pub invitee_hint: Option<OwnerAddr>,

    #[serde(rename = "mt")]
    pub minted_at: Hlc,

    #[serde(
        rename = "sg",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

impl CanonicalPayloadSealed for CommunityInvitePayload {}
impl CanonicalPayload for CommunityInvitePayload {}
impl CanonicalPayloadSealed for InviteToken {}
impl CanonicalPayload for InviteToken {}
```

- [ ] **Step 11.4: Wire the module into `lib.rs`**

Find `pub mod community_membership;` in `src-tauri/src/lib.rs` (added in Task 4) and add:

```rust
pub mod community_invite;
```

(Below the `community_membership` line, alphabetical-adjacent.)

- [ ] **Step 11.5: Run tests to verify they pass**

```bash
cd src-tauri
set -o pipefail
cargo test --test community_invite_unit 2>&1 | tail -10
echo "TEST_EXIT=$?"
```

Expected: 3/3 passing.

- [ ] **Step 11.6: Run full gate sweep + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Then:

```bash
git add src-tauri/src/community_invite.rs src-tauri/src/lib.rs src-tauri/tests/community_invite_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-217-phase1): CommunityInvitePayload + InviteToken types

Phase 1 ships types + canonical CBOR only. URL encoding (base64url +
harmony://invite/ prefix) lives in Phase 3 with generate_invite IPC.
Reticulum send/receive for counter-sig flow lives in Phase 4.

7-key CommunityInvitePayload, 4-key InviteToken — both follow the
same-length-keys CBOR invariant (all field codes 2 chars). Optional
fields use skip_serializing_if so non-applicable variants don't
bloat the encoded URL.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Wire-format CBOR golden fixtures

**Files:**
- Create: `src-tauri/tests/wire_format_community_fixtures.rs`

Pin the exact encoded bytes for: `MembershipKey`, each of the 5 `MembershipEventKind` variants wrapped in `SignedMembershipEvent`, `CounterSignature`, `CommunityInvitePayload` (open + invite-only forms), `InviteToken`. These golden fixtures break loudly if anyone changes the wire format inadvertently — same defense as `wire_format_fixture.rs` provides for owner-state.

This task is regression-test-shaped, not TDD-shaped: write the test, run it once to discover the canonical bytes, paste those bytes into the assertions, run again to confirm. Then any future change to encoding breaks the test.

- [ ] **Step 12.1: Create the fixture-discovery scaffold**

Create `src-tauri/tests/wire_format_community_fixtures.rs`:

```rust
//! Golden CBOR fixtures for ZEB-217 Sub-C Phase 1 wire types.
//! Pinned bytes prevent silent wire-format changes — if any of these
//! tests fail, treat it as a wire-protocol break and review carefully
//! (cross-version compatibility, peer interop, etc.).
//!
//! Mirrors src-tauri/tests/wire_format_fixture.rs (owner-state).

use harmony_app::community_invite::{CommunityInvitePayload, InviteToken};
use harmony_app::community_membership::{
    CounterSignature, EventPayload, MembershipEventKind, SignedMembershipEvent,
};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};

fn fixture_hlc() -> Hlc {
    Hlc { wall_ms: 1700000000000, logical: 0, device_id: "fix".into() }
}

fn fixture_payload(kind: MembershipEventKind) -> EventPayload {
    EventPayload {
        id: [0x42; 16],
        community_id: SpaceId([0x37; 16]),
        kind,
        actor: OwnerAddr([0x11; 16]),
        at: fixture_hlc(),
    }
}

#[test]
fn membership_key_wire_bytes_pinned() {
    let k = MembershipKey::new([0xAA; 32]);
    let bytes = canonical_cbor_encode(&k).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    // First run: print hex and copy into the assertion below.
    // After pinning, comparing hex strings gives a readable diff on
    // future failures.
    assert_eq!(
        hex,
        "5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "MembershipKey wire format changed — review carefully"
    );
}

#[test]
fn signed_event_join_wire_bytes_pinned() {
    let payload = fixture_payload(MembershipEventKind::Join);
    let event = SignedMembershipEvent {
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig: [0xBB; 64],
        countersig: None,
    };
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();

    // FIRST RUN: cargo test -- --nocapture and copy the printed hex.
    // SECOND RUN ONWARD: this assertion catches wire breaks.
    eprintln!("signed_event_join hex: {hex}");
    // TEMP: assert presence of the actor bytes (sanity); replace with
    // exact hex on second run after observing the output.
    assert!(hex.contains("11111111"));
}
```

- [ ] **Step 12.2: Run once to discover bytes for the JOIN fixture**

```bash
cd src-tauri
set -o pipefail
cargo test --test wire_format_community_fixtures signed_event_join_wire_bytes_pinned -- --nocapture 2>&1 | tail -10
```

Copy the printed `signed_event_join hex: ...` value.

- [ ] **Step 12.3: Pin the discovered bytes**

Replace the `assert!(hex.contains("11111111"))` line with the exact assertion. If the printed hex was `a76269645051...` (example), change to:

```rust
assert_eq!(
    hex,
    "<paste the actual printed hex here>",
    "SignedMembershipEvent (Join) wire format changed — review"
);
```

- [ ] **Step 12.4: Add the remaining fixtures using the same discover-then-pin loop**

Append to `tests/wire_format_community_fixtures.rs`. Each test follows this exact template — copy, adapt the kind/values, run with `--nocapture` to discover hex, paste hex into the assertion:

```rust
#[test]
fn signed_event_leave_wire_bytes_pinned() {
    let payload = fixture_payload(MembershipEventKind::Leave);
    let event = SignedMembershipEvent {
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig: [0xBB; 64],
        countersig: None,
    };
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_leave hex: {hex}");
    assert_eq!(hex, "<PASTE_DISCOVERED_HEX>", "Leave wire format changed");
}

#[test]
fn signed_event_invite_wire_bytes_pinned() {
    let target = OwnerAddr([0x99; 16]);
    let payload = fixture_payload(MembershipEventKind::Invite { target });
    let event = SignedMembershipEvent {
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig: [0xBB; 64],
        countersig: None,
    };
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_invite hex: {hex}");
    assert_eq!(hex, "<PASTE_DISCOVERED_HEX>", "Invite wire format changed");
}

#[test]
fn signed_event_kick_no_reason_wire_bytes_pinned() {
    let target = OwnerAddr([0x99; 16]);
    let payload = fixture_payload(MembershipEventKind::Kick { target, reason: None });
    let event = SignedMembershipEvent {
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig: [0xBB; 64],
        countersig: None,
    };
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_kick_no_reason hex: {hex}");
    assert_eq!(hex, "<PASTE_DISCOVERED_HEX>", "Kick (no reason) wire format changed");
}

#[test]
fn signed_event_kick_with_reason_wire_bytes_pinned() {
    let target = OwnerAddr([0x99; 16]);
    let payload = fixture_payload(MembershipEventKind::Kick {
        target,
        reason: Some("spam".to_string()),
    });
    let event = SignedMembershipEvent {
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig: [0xBB; 64],
        countersig: None,
    };
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_kick_with_reason hex: {hex}");
    assert_eq!(hex, "<PASTE_DISCOVERED_HEX>", "Kick (with reason) wire format changed");
}

#[test]
fn signed_event_setpower_wire_bytes_pinned() {
    let target = OwnerAddr([0x99; 16]);
    let payload = fixture_payload(MembershipEventKind::SetPower { target, level: 50 });
    let event = SignedMembershipEvent {
        id: payload.id,
        community_id: payload.community_id,
        kind: payload.kind.clone(),
        actor: payload.actor,
        at: payload.at.clone(),
        sig: [0xBB; 64],
        countersig: None,
    };
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_setpower hex: {hex}");
    assert_eq!(hex, "<PASTE_DISCOVERED_HEX>", "SetPower wire format changed");
}

#[test]
fn countersignature_wire_bytes_pinned() {
    let cs = CounterSignature {
        signer: OwnerAddr([0x77; 16]),
        sig: [0xCC; 64],
    };
    let bytes = canonical_cbor_encode(&cs).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("countersignature hex: {hex}");
    assert_eq!(hex, "<PASTE_DISCOVERED_HEX>", "CounterSignature wire format changed");
}

#[test]
fn community_invite_payload_open_wire_bytes_pinned() {
    let p = CommunityInvitePayload {
        community_id: SpaceId([0x37; 16]),
        membership_key: MembershipKey::new([0xAA; 32]),
        admin_addr: OwnerAddr([0x11; 16]),
        community_name: "fix".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
    };
    let bytes = canonical_cbor_encode(&p).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("community_invite_payload_open hex: {hex}");
    assert_eq!(
        hex,
        "<PASTE_DISCOVERED_HEX>",
        "CommunityInvitePayload (open) wire format changed"
    );
}

#[test]
fn community_invite_payload_invite_only_wire_bytes_pinned() {
    let token = InviteToken {
        inviter: OwnerAddr([0x11; 16]),
        invitee_hint: Some(OwnerAddr([0x22; 16])),
        minted_at: fixture_hlc(),
        sig: [0xDD; 64],
    };
    let p = CommunityInvitePayload {
        community_id: SpaceId([0x37; 16]),
        membership_key: MembershipKey::new([0xAA; 32]),
        admin_addr: OwnerAddr([0x11; 16]),
        community_name: "fix".into(),
        is_invite_only: true,
        expires_at: Some(fixture_hlc()),
        invite_token: Some(token),
    };
    let bytes = canonical_cbor_encode(&p).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("community_invite_payload_invite_only hex: {hex}");
    assert_eq!(
        hex,
        "<PASTE_DISCOVERED_HEX>",
        "CommunityInvitePayload (invite-only) wire format changed"
    );
}

#[test]
fn invite_token_targeted_wire_bytes_pinned() {
    let t = InviteToken {
        inviter: OwnerAddr([0x11; 16]),
        invitee_hint: Some(OwnerAddr([0x22; 16])),
        minted_at: fixture_hlc(),
        sig: [0xDD; 64],
    };
    let bytes = canonical_cbor_encode(&t).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("invite_token_targeted hex: {hex}");
    assert_eq!(
        hex,
        "<PASTE_DISCOVERED_HEX>",
        "InviteToken (targeted) wire format changed"
    );
}

#[test]
fn invite_token_open_wire_bytes_pinned() {
    let t = InviteToken {
        inviter: OwnerAddr([0x11; 16]),
        invitee_hint: None,
        minted_at: fixture_hlc(),
        sig: [0xDD; 64],
    };
    let bytes = canonical_cbor_encode(&t).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("invite_token_open hex: {hex}");
    assert_eq!(
        hex,
        "<PASTE_DISCOVERED_HEX>",
        "InviteToken (open) wire format changed"
    );
}
```

For each test:

1. Add it with `assert_eq!(hex, "<PASTE_DISCOVERED_HEX>", ...);` as a placeholder
2. Run: `cargo test --test wire_format_community_fixtures <test_name> -- --nocapture 2>&1 | grep "hex:"`
3. Copy the printed hex value
4. Replace `<PASTE_DISCOVERED_HEX>` with the actual hex string
5. Re-run to confirm the test now passes with the pinned bytes

Total: 10 fixtures (1 from Step 12.3 + 9 here). Mechanical, ~30 minutes.

- [ ] **Step 12.5: Run the full fixture suite**

```bash
cd src-tauri
set -o pipefail
cargo test --test wire_format_community_fixtures 2>&1 | tail -15
echo "TEST_EXIT=$?"
```

Expected: all golden-fixture tests pass.

- [ ] **Step 12.6: Run full gate sweep + commit**

```bash
cd src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --all-targets --all-features 2>&1 | grep "^test result:" | tail -3
```

Then:

```bash
git add src-tauri/tests/wire_format_community_fixtures.rs
git commit -m "$(cat <<'EOF'
test(zeb-217-phase1): wire-format CBOR golden fixtures for community types

Pinned exact encoded bytes for: MembershipKey, SignedMembershipEvent
(× 5 kinds, including kick with/without reason), CounterSignature,
CommunityInvitePayload (open + invite-only forms), InviteToken
(targeted + anyone-redeem forms).

Mirrors src-tauri/tests/wire_format_fixture.rs (owner-state). Any
silent encoding change in Phase 2+ will surface as a loud diff in
these tests. Cross-version + peer interop depends on these bytes
staying stable.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: Final phase verification + push + open Phase 1 PR

**Files:** none modified — verification + PR creation only

- [ ] **Step 13.1: Confirm full gate sweep is green**

```bash
cd src-tauri
set -o pipefail

cargo fmt --all -- --check
echo "FMT_EXIT=$?"

cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
echo "CLIPPY_EXIT=$?"

cargo test --all-targets --all-features 2>&1 | grep -E "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END{printf "TOTAL passed=%d failed=%d ignored=%d\n", p, f, i}'
echo "TEST_EXIT=$?"

cd ..
node_modules/.bin/tsc --noEmit
echo "TSC_EXIT=$?"

set -o pipefail
node_modules/.bin/vitest run 2>&1 | tail -10
echo "VITEST_EXIT=$?"
```

Expected:
- FMT_EXIT=0
- CLIPPY_EXIT=0
- TEST_EXIT=0 with passed = baseline + ~50 new tests (10 owner_state_types + 11 community_membership_unit + 3 community_invite_unit + ~12 wire fixtures + ~3 verify_event helpers, give or take)
- TSC_EXIT=0
- VITEST_EXIT=0 (frontend untouched in Phase 1; baseline 1392 should still pass)

If any of these fail, fix before pushing.

- [ ] **Step 13.2: Confirm local commits are atomic + clean**

```bash
git log --oneline origin/main..HEAD
```

Expected: 12 commits, one per task (Tasks 1-12). Each commit message follows the `feat(zeb-217-phase1): ...` or `test(zeb-217-phase1): ...` format. No fixup commits, no WIP commits.

If commits look messy, that's a sign the implementation diverged from TDD — worth pausing to review with the user before pushing rather than papering over with a squash.

- [ ] **Step 13.3: Push the branch**

```bash
git push -u origin zeb-217-sub-c-phase1-membership-crdt 2>&1 | tail -10
```

Expected: branch created on origin, up-to-date confirmation.

- [ ] **Step 13.4: Open the Phase 1 PR**

```bash
gh pr create --title "feat(zeb-217): Sub-C Phase 1 — community membership CRDT primitives" \
  --body "$(cat <<'EOF'
## Summary

ZEB-217 (Sub-C of ZEB-206) Phase 1 of 5: membership CRDT primitives.
Pure Rust types + verification + materialization. No IPC, no Zenoh,
no UI — those land in Phases 2-5.

## What's in Phase 1

* `MembershipKey` newtype (mirrors DmContentKey — bstr(32),
  ZeroizeOnDrop, redacted Debug)
* `Space` struct extension: `mk` / `ad` / `io` fields + invariants
* `MembershipEventKind` enum (5 variants: Join, Leave, Invite, Kick,
  SetPower) with adjacently-tagged CBOR
* `SignedMembershipEvent` + `CounterSignature` + `EventPayload` types
* `sign_event` + `verify_signature` + `attach_countersig` +
  `verify_countersig` helpers (ed25519-dalek, verify_strict per
  RFC 8032 strict subset)
* `MaterializedMembership` + `MemberState` + `MemberStatus` +
  `POWER_THRESHOLDS` (v1 hardcoded)
* `materialize` (pure replay, HLC-sorted) + `verify_event` (full
  power-rule + countersig-power-check verification)
* `CommunityInvitePayload` + `InviteToken` types (Phase 3 wires up
  generate_invite / redeem_invite IPCs)
* CBOR golden fixtures for every new wire type

## Phase boundary

Phase 1 ships ZERO IPC commands, ZERO networking, ZERO frontend.
That's intentional — see the spec's phasing section. Phase 2 builds
the per-community Prolly Tree + encrypted Zenoh sync on top of these
primitives.

## Test plan

- [x] `cargo fmt --all -- --check` clean
- [x] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [x] `cargo test --all-targets --all-features` — baseline + ~50 new tests pass
- [x] `tsc --noEmit` clean (frontend untouched)
- [x] `vitest run` — 1392 tests still pass

## Spec reference

`docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md` (commit
`8b8028a`). Phase 1 covers spec sections "Data model" (entirely),
"Materialization rules" (entirely), "Verification" (entirely), and
"Invite link payload" (types only).

## Phase 2 will add

* `community_state_crdt.rs` (per-community Prolly Tree)
* `community_state_sync.rs` (encrypted Zenoh state-root topic + DAG-sync)
* `event_loop.rs` integration

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Note the PR URL printed at the end — share with reviewers.

- [ ] **Step 13.5: Verify CI passes on the PR**

After ~5-10 minutes:

```bash
gh pr view --json statusCheckRollup -q '.statusCheckRollup[] | {name, conclusion}'
```

Expected: all required checks SUCCESS. If any fail, debug + push fix commits.

---

## Phase 1 done.

After this PR merges:

1. **Phase 2 plan** gets written at `docs/plans/2026-05-05-zeb-217-sub-c-phase2-state-crdt-sync-plan.md` (separate writing-plans invocation)
2. **Phase 2 branch** opens off the new `origin/main` (which now includes Phase 1)
3. The two-stage subagent review pattern continues per phase

The 5-phase structure keeps each PR reviewable in isolation. Phase 5 will land the ZEB-247 e2e Tauri::invoke harness alongside the admin UI.
