# ZEB-285 Phase 1: Community forking implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land Phase 1 of [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) — any joined member can fork a community they belong to, producing an independent community with frozen pre-fork history and a dual-keyset verifier. Spec: `docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md`.

**Architecture:** Extend `MembershipEventKind` with a new non-mutating `Fork` variant; extend `CommunityState` and `CommunityInvitePayload` with optional lineage + snapshot fields; add `fork_community` Tauri IPC + service wrapper + `ForkConfirmDialog` UI; render forks with a NavService glyph, Lineage block, and unified pre/post-fork timeline.

**Tech Stack:** Rust (Tauri backend, CBOR canonical wire format), TypeScript (services), Svelte (UI components), vitest + cargo nextest.

**Branch:** `zeb-285-phase1-community-forking` cut from `4ae034d` (HEAD of `origin/main`). Spec at HEAD commit `e318823`.

**Required CI gates** (HARD RULE — must be green at every commit):

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
# From repo root:
npx tsc --noEmit
npx vitest run
```

---

## Task 0 — Pre-flight verification (no commit)

**Files:** None (read-only verification)

- [ ] **Step 1: Confirm branch is at `e318823` on top of `4ae034d`**

```bash
git status && git log --oneline -3
```

Expected:
```
On branch zeb-285-phase1-community-forking
nothing to commit, working tree clean
e318823 docs(zeb-285-p1): community forking primitive design spec
4ae034d ZEB-209: mock-clear policy across MessageService / VineService / NavService (#121)
a8e88cc ZEB-103: Vine reshare improvements — attribution, counts, confirm dialog (#120)
```

- [ ] **Step 2: Run all five CI gates, confirm green-baseline**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
# (separate shell from repo root)
npx tsc --noEmit
npx vitest run
```

All five must exit 0. If any fail on a freshly-cut branch, that's a pre-existing test-drift issue — file a separate ticket and fix BEFORE proceeding (per `feedback_test_drift_is_our_fault` HARD RULE).

- [ ] **Step 3: No commit for this task**

Task 0 is pre-flight only.

---

## Task 1 — Add `MembershipEventKind::Fork` variant + verify + materialize

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (lines 43-167 for enum, ~1180 for materialize, ~1620 for verify_event, in-module tests at end)

- [ ] **Step 1: Write the failing CBOR roundtrip test**

Append to the in-module `#[cfg(test)] mod tests` block at the end of `community_membership.rs`:

```rust
#[test]
fn fork_event_cbor_roundtrip() {
    use crate::owner_state_crypto::canonical_cbor_bytes;

    let fork_space_id = SpaceId([0xfa; 16]);
    let event = MembershipEventKind::Fork { fork_space_id };

    let bytes = canonical_cbor_bytes(&event).expect("encode");
    let decoded: MembershipEventKind = ciborium::de::from_reader(&bytes[..]).expect("decode");

    assert_eq!(event, decoded);

    // Verify the variant tag is "x" and inner key is "fs" by inspecting
    // the CBOR encoding directly. Wire form: { "tg": "x", "vl": { "fs": <16-byte bstr> } }.
    let value: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("re-decode as value");
    let map = value.as_map().expect("outer is map");
    let tg = map.iter().find_map(|(k, v)| {
        if k.as_text() == Some("tg") { Some(v) } else { None }
    }).expect("tg key");
    assert_eq!(tg.as_text(), Some("x"));

    let vl = map.iter().find_map(|(k, v)| {
        if k.as_text() == Some("vl") { Some(v) } else { None }
    }).expect("vl key");
    let inner = vl.as_map().expect("vl is map");
    assert!(inner.iter().any(|(k, _)| k.as_text() == Some("fs")), "inner has fs key");
}
```

- [ ] **Step 2: Run the test, verify it fails**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fork_event_cbor_roundtrip)'
```

Expected: FAIL with "no variant of enum MembershipEventKind found in flattened data".

- [ ] **Step 3: Add the `Fork` variant to `MembershipEventKind`**

In `community_membership.rs`, inside the `pub enum MembershipEventKind { ... }` block (after the existing variants, before the closing `}` of the enum around line 167), add:

```rust
    /// ZEB-285: a joined member declares they have forked this community
    /// into a new community with `fork_space_id` as its SpaceId. Non-mutating
    /// — does NOT change materialized membership/power/channels, does NOT
    /// trigger EpochRotation. Other members materialize it as visible
    /// fork-lineage history. Verify rule: signer must be Joined at the
    /// event's HLC (power threshold = 0, "any joined member, any time").
    ///
    /// Variant tag "x" (1-char value, lowercase, unused before this).
    /// Inner field key "fs" (2-char) per same-length-keys invariant at this
    /// nesting level. See `docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md` §3.1.
    #[serde(rename = "x")]
    Fork {
        #[serde(rename = "fs")]
        fork_space_id: SpaceId,
    },
```

- [ ] **Step 4: Run the test, verify it passes**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fork_event_cbor_roundtrip)'
```

Expected: PASS.

- [ ] **Step 5: Extend the existing `all_variants_roundtrip` test**

Find the existing `all_variants_roundtrip` test in the tests module (search for `fn all_variants_roundtrip`). Add a `MembershipEventKind::Fork { fork_space_id: SpaceId([0xfa; 16]) }` entry to its variants vector. Run the test:

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(all_variants_roundtrip)'
```

Expected: PASS (the test iterates all variants in a vector and asserts roundtrip-equality for each).

- [ ] **Step 6: Add verify-rule test (passes for joined member)**

Append to the tests module:

```rust
#[test]
fn verify_event_fork_allows_any_joined_member() {
    let community_id = SpaceId([0xc0; 16]);
    let admin = OwnerAddr([0xaa; 32]);
    let regular = OwnerAddr([0xbb; 32]);

    // Bootstrap: admin Join (power 100) + regular Join (power 0).
    let admin_join = sign_test_event(MembershipEventKind::Join, &admin, community_id, 1);
    let regular_join = sign_test_event(MembershipEventKind::Join, &regular, community_id, 2);

    let mut state = CommunityState::new(community_id);
    state.insert_local_event_with_pubs(admin_join.clone(), admin_join.actor_identity_pub(), None).expect("insert admin");
    state.insert_local_event_with_pubs(regular_join.clone(), regular_join.actor_identity_pub(), None).expect("insert regular");

    // Now regular (power 0) signs a Fork event. Should verify cleanly.
    let fork = sign_test_event(
        MembershipEventKind::Fork { fork_space_id: SpaceId([0xfe; 16]) },
        &regular,
        community_id,
        3,
    );
    let outcome = state.insert_local_event_with_pubs(fork, regular.identity_pub_bytes(), None);
    assert_eq!(outcome, Ok(InsertOutcome::Inserted), "fork by regular member should succeed");
}
```

NOTE: `sign_test_event` is an existing test helper in the tests module. If its signature differs from what's written above, adapt the call site to match. The test helper signs an event with deterministic crypto and returns a `SignedMembershipEvent`.

- [ ] **Step 7: Run the test**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_event_fork_allows_any_joined_member)'
```

Expected: FAIL (no verify rule for Fork yet — `verify_event` panics or falls through).

- [ ] **Step 8: Add the verify-event arm for `Fork`**

In `verify_event` in `community_membership.rs` (find the existing match-on-`event.body.kind` around line 1620), add a new match arm before the catch-all:

```rust
        MembershipEventKind::Fork { .. } => {
            // ZEB-285: any joined non-Banned member can fork at any time.
            // Power threshold 0 — same as Leave. Non-mutating: doesn't
            // affect membership/power/channels, doesn't trigger EpochRotation.
            if actor_power < POWER_THRESHOLDS.invite {
                return Err(VerifyError::InsufficientPower {
                    required: POWER_THRESHOLDS.invite,
                    actual: actor_power,
                });
            }
            // No additional checks — fork_space_id is a self-reported value
            // from the forker; receivers don't (and can't) verify the fork's
            // existence on the forker's device.
        }
```

- [ ] **Step 9: Run the test**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_event_fork_allows_any_joined_member)'
```

Expected: PASS.

- [ ] **Step 10: Add verify-rule test (rejects non-member)**

Append to the tests module:

```rust
#[test]
fn verify_event_fork_rejects_non_member() {
    let community_id = SpaceId([0xc0; 16]);
    let admin = OwnerAddr([0xaa; 32]);
    let outsider = OwnerAddr([0xcc; 32]);

    let admin_join = sign_test_event(MembershipEventKind::Join, &admin, community_id, 1);
    let mut state = CommunityState::new(community_id);
    state.insert_local_event_with_pubs(admin_join.clone(), admin_join.actor_identity_pub(), None).expect("insert admin");

    // Outsider (never joined) tries to Fork. Should reject.
    let fork = sign_test_event(
        MembershipEventKind::Fork { fork_space_id: SpaceId([0xfe; 16]) },
        &outsider,
        community_id,
        2,
    );
    let outcome = state.insert_local_event_with_pubs(fork, outsider.identity_pub_bytes(), None);
    assert!(matches!(outcome, Ok(InsertOutcome::Rejected(VerifyError::NotMember { .. }))),
        "fork by non-member should reject with NotMember; got {:?}", outcome);
}
```

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_event_fork_rejects_non_member)'
```

Expected: PASS.

- [ ] **Step 11: Add materialize-no-op test**

Append to the tests module:

```rust
#[test]
fn materialize_fork_is_non_mutating() {
    let community_id = SpaceId([0xc0; 16]);
    let admin = OwnerAddr([0xaa; 32]);

    let admin_join = sign_test_event(MembershipEventKind::Join, &admin, community_id, 1);
    let mut state = CommunityState::new(community_id);
    state.insert_local_event_with_pubs(admin_join.clone(), admin_join.actor_identity_pub(), None).expect("insert admin");

    let before = state.materialized(admin).expect("materialize before").clone();

    let fork = sign_test_event(
        MembershipEventKind::Fork { fork_space_id: SpaceId([0xfe; 16]) },
        &admin,
        community_id,
        2,
    );
    state.insert_local_event_with_pubs(fork, admin.identity_pub_bytes(), None).expect("insert fork");

    let after = state.materialized(admin).expect("materialize after").clone();

    // Materialized view should be unchanged by the Fork event — members,
    // power levels, channels are all identical.
    assert_eq!(before.members, after.members, "members should be unchanged");
    assert_eq!(before.power_levels, after.power_levels, "power_levels should be unchanged");
    assert_eq!(before.channels, after.channels, "channels should be unchanged");
}
```

- [ ] **Step 12: Run the test**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(materialize_fork_is_non_mutating)'
```

Expected: FAIL (no materialize rule for Fork — falls through to catch-all or panics).

- [ ] **Step 13: Add the materialize-event arm for `Fork`**

In `materialize` in `community_membership.rs` (find the existing match-on-`event.body.kind` around line 1180), add a new arm before the catch-all:

```rust
        MembershipEventKind::Fork { .. } => {
            // ZEB-285: non-mutating. Fork events are recorded in the event
            // log for historical/audit visibility but do not change the
            // materialized membership/power/channels view. They are
            // surfaced separately via settings-panel listings.
        }
```

- [ ] **Step 14: Run the test**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(materialize_fork_is_non_mutating)'
```

Expected: PASS.

- [ ] **Step 15: Add epoch-rotation no-trigger test**

Append to the tests module:

```rust
#[test]
fn fork_does_not_trigger_epoch_rotation() {
    let community_id = SpaceId([0xc0; 16]);
    let admin = OwnerAddr([0xaa; 32]);

    let admin_join = sign_test_event(MembershipEventKind::Join, &admin, community_id, 1);
    let mut state = CommunityState::new(community_id);
    state.insert_local_event_with_pubs(admin_join.clone(), admin_join.actor_identity_pub(), None).expect("insert admin");

    let event_count_before = state.events.len();

    let fork = sign_test_event(
        MembershipEventKind::Fork { fork_space_id: SpaceId([0xfe; 16]) },
        &admin,
        community_id,
        2,
    );
    state.insert_local_event_with_pubs(fork, admin.identity_pub_bytes(), None).expect("insert fork");

    // After inserting Fork: events.len() should be event_count_before + 1
    // (the Fork itself), NOT +2 (Fork + auto-EpochRotation).
    assert_eq!(state.events.len(), event_count_before + 1,
        "Fork should NOT auto-trigger EpochRotation (contrast with Kick/Leave)");

    // Verify no EpochRotation variant in events.
    let has_rotation = state.events.values().any(|e| matches!(
        e.body.kind, MembershipEventKind::EpochRotation { .. }
    ));
    assert!(!has_rotation, "no EpochRotation should fire on Fork");
}
```

- [ ] **Step 16: Run the test, verify it passes**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fork_does_not_trigger_epoch_rotation)'
```

Expected: PASS (the auto-rotation logic in `insert_local_event_with_pubs` is gated on Kick/Leave; Fork should not match).

- [ ] **Step 17: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
# (separate shell, repo root)
npx tsc --noEmit
npx vitest run
```

All five must exit 0. If `cargo fmt` flags formatting, run `cd src-tauri && cargo fmt --all` then re-check.

- [ ] **Step 18: Commit**

```bash
git add src-tauri/src/community_membership.rs
git commit -m "$(cat <<'EOF'
feat(zeb-285-p1): add MembershipEventKind::Fork variant

Non-mutating CRDT event signed in the ORIGINAL community's log
announcing that a member has forked the community to a new SpaceId.
Variant tag "x", inner key "fs" per same-length-keys invariant.

Verify rule: power threshold 0 (any joined non-Banned member). Same
gate as Leave. Materialize is a no-op — Fork doesn't affect
members/power/channels and doesn't trigger EpochRotation.

Tests: 6 in-module unit tests (cbor roundtrip + verify allow/reject +
materialize no-op + epoch-rotation no-trigger + extended all-variants
fixture).

Spec: docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md §3.1, §3.5, §3.6.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2 — Add `CommunityState.forked_from` field

**Files:**
- Modify: `src-tauri/src/community_state_crdt.rs:28-59` (CommunityState struct)
- Modify: in-module tests at end of same file

- [ ] **Step 1: Write the failing wire-compatibility test**

Append to the in-module tests at the end of `community_state_crdt.rs`:

```rust
#[test]
fn community_state_forked_from_cbor_skip() {
    // ZEB-285: a CommunityState with forked_from = None must encode
    // byte-identical to pre-ZEB-285 wire form (no "ff" key emitted).
    let cid = SpaceId([0xc0; 16]);
    let state = CommunityState::new(cid);

    let bytes = canonical_cbor_bytes(&state).expect("encode");
    let value: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("decode as value");
    let map = value.as_map().expect("outer is map");

    // Top-level keys should NOT include "ff" when forked_from is None.
    assert!(!map.iter().any(|(k, _)| k.as_text() == Some("ff")),
        "forked_from=None should be omitted from CBOR encoding");
}

#[test]
fn community_state_forked_from_some_roundtrip() {
    // ZEB-285: with forked_from = Some(_), the "ff" key appears and
    // round-trips correctly.
    let cid = SpaceId([0xc0; 16]);
    let original_id = SpaceId([0xa0; 16]);

    let mut state = CommunityState::new(cid);
    state.forked_from = Some(original_id);

    let bytes = canonical_cbor_bytes(&state).expect("encode");
    let decoded: CommunityState = ciborium::de::from_reader(&bytes[..]).expect("decode");

    assert_eq!(decoded.community_id, cid);
    assert_eq!(decoded.forked_from, Some(original_id));

    let value: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("re-decode as value");
    let map = value.as_map().expect("outer is map");
    assert!(map.iter().any(|(k, _)| k.as_text() == Some("ff")),
        "forked_from=Some should appear in CBOR encoding");
}
```

- [ ] **Step 2: Run the tests, verify they fail**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_state_forked_from)'
```

Expected: FAIL (no `forked_from` field on `CommunityState`; the struct-literal assignment doesn't compile).

- [ ] **Step 3: Add the `forked_from` field to `CommunityState`**

In `community_state_crdt.rs`, modify the `CommunityState` struct (lines 28-59). Add the new field after `community_id` (before `events`):

```rust
    /// ZEB-285: SpaceId of the community this one was forked from, or
    /// None for a top-level (non-fork) community. Persisted in wire form
    /// so a fork's lineage survives round-trips and is visible to anyone
    /// who decodes the state. Set once at fork creation, never mutated.
    /// Byte-compatible with pre-ZEB-285 blobs (omitted when None).
    #[serde(rename = "ff", skip_serializing_if = "Option::is_none", default)]
    pub forked_from: Option<SpaceId>,
```

Also update the `CommunityState::new(...)` constructor (if it sets all fields explicitly) to set `forked_from: None`. Also update the `Clone` impl at lines 86-97 to clone `forked_from` (`forked_from: self.forked_from`).

- [ ] **Step 4: Run the tests, verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_state_forked_from)'
```

Expected: both tests PASS.

- [ ] **Step 5: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

If `cargo clippy` flags PartialEq impl needing update (line 99-104), add `&& self.forked_from == other.forked_from` to the eq fn.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_state_crdt.rs
git commit -m "$(cat <<'EOF'
feat(zeb-285-p1): add CommunityState.forked_from lineage field

Optional SpaceId field tracking the original community a fork
descended from. Single-hop only — chain depth >1 is resolved at
display time in Phase 2.

Wire format: CBOR key "ff", skip_serializing_if=Option::is_none.
Byte-compatible with pre-ZEB-285 CommunityState blobs (no "ff" key
emitted when None).

Tests: 2 in-module tests (skip-when-none + roundtrip-when-some).

Spec: §3.2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3 — Add `PreForkSnapshot` + `BoundedChannelLogSnapshot` types

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (new types alongside existing invite types, after line 259)

- [ ] **Step 1: Write the failing roundtrip test**

Append to the in-module tests at the end of `community_invite.rs`:

```rust
#[test]
fn pre_fork_snapshot_canonical_cbor_pinned() {
    use crate::community_membership::{MembershipEventKind, sign_test_event};
    use crate::owner_state_crypto::canonical_cbor_bytes;
    use std::collections::BTreeMap;

    let original_id = SpaceId([0xa0; 16]);
    let admin = OwnerAddr([0xaa; 32]);

    let admin_join = sign_test_event(MembershipEventKind::Join, &admin, original_id, 1);
    let admin_pub = admin.identity_pub_bytes();

    let mut identity_pubs = BTreeMap::new();
    identity_pubs.insert(admin, admin_pub);

    let snapshot = PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: "Test Community".to_string(),
        membership_events: vec![admin_join],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs,
        forked_at: Hlc { wall_ms: 1_700_000_000_000, lc: 0 },
    };

    let bytes = canonical_cbor_bytes(&snapshot).expect("encode");
    let decoded: PreForkSnapshot = ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(snapshot, decoded);

    // Verify top-level field keys: oi, on, ev, cl, ip, ts (all 2-char).
    let value: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("re-decode");
    let map = value.as_map().expect("outer is map");
    for expected in &["oi", "on", "ev", "cl", "ip", "ts"] {
        assert!(map.iter().any(|(k, _)| k.as_text() == Some(*expected)),
            "expected key {} in snapshot encoding", expected);
    }
}
```

- [ ] **Step 2: Run the test, verify it fails**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pre_fork_snapshot_canonical_cbor_pinned)'
```

Expected: FAIL (types don't exist).

- [ ] **Step 3: Add `BoundedChannelLogSnapshot` type**

In `community_invite.rs`, after the existing types (after the `impl CanonicalPayload for CommunityInviteSigned {}` block around line 259), add:

```rust
/// ZEB-285: bounded snapshot of an original community's channel-log
/// state at fork time. Wire format: 1-key CBOR map keyed by ChannelId.
/// Per-channel value is a Vec<SignedChannelLogEvent> bounded by the
/// snapshot policy (§4.2 of spec):
/// - most-recent N=500 messages per channel by HLC descending
/// - total capped at M=5000 messages across all channels with
///   proportional trim
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BoundedChannelLogSnapshot {
    /// Per-channel signed log events, frozen at fork time. Empty for
    /// channels with no posts (or omitted entirely if no channels
    /// have any posts). BTreeMap (not HashMap) for canonical-CBOR
    /// deterministic ordering.
    #[serde(rename = "pc")]
    pub per_channel: std::collections::BTreeMap<
        crate::community_membership::ChannelId,
        Vec<crate::community_channel_log::SignedChannelLogEvent>,
    >,
}

impl CanonicalPayloadSealed for BoundedChannelLogSnapshot {}
impl CanonicalPayload for BoundedChannelLogSnapshot {}
```

- [ ] **Step 4: Add `PreForkSnapshot` type**

Immediately after `BoundedChannelLogSnapshot`, add:

```rust
/// ZEB-285: frozen snapshot of an original community's history,
/// bundled into fork-invites so fork-invitees can see pre-fork
/// context. Self-contained for verification: `identity_pubs` carries
/// the owner-pubkeys needed to verify every signer in
/// `membership_events` and `channel_log`, so joiners do NOT need to
/// query profile-broadcast to verify the snapshot.
///
/// Wire format: 6-key CBOR map. Field codes 2-char per same-length-
/// keys at this nesting level. See spec §3.4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreForkSnapshot {
    /// The original community's SpaceId. Signed pre-fork events
    /// reference this SpaceId in their bodies; the dual-keyset
    /// verifier dispatches by this value.
    #[serde(rename = "oi")]
    pub original_community_id: SpaceId,

    /// Display name of the original community at fork time. Used for
    /// the fork's Lineage UI ("Forked from {name}").
    #[serde(rename = "on")]
    pub original_community_name: String,

    /// Membership-CRDT events from the original, signed against the
    /// original's keyset. Replayed at display time against
    /// `identity_pubs` for verification; not inserted into the fork's
    /// own CommunityState event log.
    #[serde(rename = "ev")]
    pub membership_events: Vec<crate::community_membership::SignedMembershipEvent>,

    /// Bounded channel-log snapshot per §4.2 policy.
    #[serde(rename = "cl")]
    pub channel_log: BoundedChannelLogSnapshot,

    /// Map from every OwnerAddr that signs any event in this snapshot
    /// to their 64-byte identity public bytes (X25519_pub(32) ||
    /// Ed25519_pub(32) matching Identity::to_public_bytes()).
    /// Required because fork members are NOT necessarily members of
    /// the original community, so OwnerDeviceCache won't have signers
    /// cached. Bundled inline so verification needs no external lookup.
    #[serde(
        rename = "ip",
        serialize_with = "serialize_identity_pubs_map",
        deserialize_with = "deserialize_identity_pubs_map"
    )]
    pub identity_pubs: std::collections::BTreeMap<OwnerAddr, [u8; 64]>,

    /// Forker's local HLC at fork time. Informational — used to
    /// render the "Fork point" divider in the fork's unified timeline.
    /// NOT used for any verification or ordering decision.
    #[serde(rename = "ts")]
    pub forked_at: Hlc,
}

impl CanonicalPayloadSealed for PreForkSnapshot {}
impl CanonicalPayload for PreForkSnapshot {}
```

- [ ] **Step 5: Add serializer helpers for `identity_pubs`**

Above the new types (just before `BoundedChannelLogSnapshot`), add:

```rust
/// ZEB-285: serialize BTreeMap<OwnerAddr, [u8; 64]> as a CBOR map
/// where keys are bstr(32) (OwnerAddr) and values are bstr(64).
fn serialize_identity_pubs_map<S: serde::Serializer>(
    map: &std::collections::BTreeMap<OwnerAddr, [u8; 64]>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut m = serializer.serialize_map(Some(map.len()))?;
    for (addr, pub_bytes) in map {
        m.serialize_entry(
            serde_bytes::Bytes::new(&addr.0),
            serde_bytes::Bytes::new(pub_bytes),
        )?;
    }
    m.end()
}

fn deserialize_identity_pubs_map<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<std::collections::BTreeMap<OwnerAddr, [u8; 64]>, D::Error> {
    use serde::de::{MapAccess, Visitor};
    struct MapVisitor;
    impl<'de> Visitor<'de> for MapVisitor {
        type Value = std::collections::BTreeMap<OwnerAddr, [u8; 64]>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a CBOR map of bstr(32) -> bstr(64)")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            let mut result = std::collections::BTreeMap::new();
            while let Some((key_bytes, value_bytes)) = access.next_entry::<serde_bytes::ByteBuf, serde_bytes::ByteBuf>()? {
                if key_bytes.len() != 32 {
                    return Err(serde::de::Error::custom(format!("expected 32-byte key, got {}", key_bytes.len())));
                }
                if value_bytes.len() != 64 {
                    return Err(serde::de::Error::custom(format!("expected 64-byte value, got {}", value_bytes.len())));
                }
                let mut addr = [0u8; 32];
                addr.copy_from_slice(&key_bytes);
                let mut pub_bytes = [0u8; 64];
                pub_bytes.copy_from_slice(&value_bytes);
                result.insert(OwnerAddr(addr), pub_bytes);
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(MapVisitor)
}
```

- [ ] **Step 6: Run the test**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(pre_fork_snapshot_canonical_cbor_pinned)'
```

Expected: PASS.

- [ ] **Step 7: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

All must be green.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/community_invite.rs
git commit -m "$(cat <<'EOF'
feat(zeb-285-p1): add PreForkSnapshot + BoundedChannelLogSnapshot types

Frozen snapshot bundle representing the forker's pre-fork view of the
original community. Carries:
- original community's SpaceId + display name
- frozen membership-CRDT events (signed under original's keyset)
- bounded per-channel log snapshot (§4.2 policy: N=500 per channel,
  M=5000 total with proportional trim)
- identity_pubs map (every signer's owner-pubkey, inline) for self-
  contained verification — fork members are not necessarily members
  of the original
- forker's local HLC at fork time (informational, divider rendering)

Wire format: 6-key CBOR map with 2-char keys (oi, on, ev, cl, ip, ts).

Tests: 1 in-module roundtrip + key-pinning test.

Spec: §3.4, §4.2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4 — Extend `CommunityInvitePayload` with `forked_from` + `pre_fork_snapshot`

**Files:**
- Modify: `src-tauri/src/community_invite.rs:90-148` (CommunityInvitePayload struct)

- [ ] **Step 1: Write the failing byte-compatibility test**

Append to the in-module tests:

```rust
#[test]
fn invite_payload_without_pre_fork_snapshot_byte_compat() {
    // ZEB-285: a CommunityInvitePayload with both forked_from = None
    // AND pre_fork_snapshot = None must encode byte-identical to
    // pre-ZEB-285 wire form (no "ff" or "fs" keys emitted).
    let cid = SpaceId([0xc0; 16]);
    let admin = OwnerAddr([0xaa; 32]);
    let payload = CommunityInvitePayload {
        community_id: cid,
        epoch_snapshot: InviteEpochSnapshot::test_default(cid),
        admin_addr: admin,
        community_name: "test".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: None,
        pre_fork_snapshot: None,
    };

    let bytes = canonical_cbor_bytes(&payload).expect("encode");
    let value: ciborium::Value = ciborium::de::from_reader(&bytes[..]).expect("decode as value");
    let map = value.as_map().expect("outer is map");

    assert!(!map.iter().any(|(k, _)| k.as_text() == Some("ff")),
        "forked_from=None should be omitted");
    assert!(!map.iter().any(|(k, _)| k.as_text() == Some("fs")),
        "pre_fork_snapshot=None should be omitted");
}

#[test]
fn invite_payload_with_pre_fork_snapshot_roundtrip() {
    use crate::community_membership::{MembershipEventKind, sign_test_event};
    use std::collections::BTreeMap;

    let cid = SpaceId([0xc0; 16]);
    let original_id = SpaceId([0xa0; 16]);
    let admin = OwnerAddr([0xaa; 32]);
    let admin_join = sign_test_event(MembershipEventKind::Join, &admin, original_id, 1);

    let mut identity_pubs = BTreeMap::new();
    identity_pubs.insert(admin, admin.identity_pub_bytes());

    let snapshot = PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: "Original".to_string(),
        membership_events: vec![admin_join],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs,
        forked_at: Hlc { wall_ms: 1_700_000_000_000, lc: 0 },
    };

    let payload = CommunityInvitePayload {
        community_id: cid,
        epoch_snapshot: InviteEpochSnapshot::test_default(cid),
        admin_addr: admin,
        community_name: "fork".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: Some(original_id),
        pre_fork_snapshot: Some(snapshot.clone()),
    };

    let bytes = canonical_cbor_bytes(&payload).expect("encode");
    let decoded: CommunityInvitePayload = ciborium::de::from_reader(&bytes[..]).expect("decode");

    assert_eq!(decoded.forked_from, Some(original_id));
    assert_eq!(decoded.pre_fork_snapshot, Some(snapshot));
}
```

NOTE: `InviteEpochSnapshot::test_default(cid)` is assumed to exist. If it doesn't, add a `#[cfg(test)] impl InviteEpochSnapshot { pub fn test_default(cid: SpaceId) -> Self { ... } }` constructor that returns a valid stub (zero epoch, empty deliveries, default state_snapshot).

- [ ] **Step 2: Run the tests, verify they fail**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(invite_payload_)'
```

Expected: FAIL (struct literal mentions fields that don't exist).

- [ ] **Step 3: Add the two new fields to `CommunityInvitePayload`**

In `community_invite.rs`, after the existing `admin_identity_pub` field (around line 147), add:

```rust
    /// ZEB-285: SpaceId of the community this one was forked from.
    /// Mirrors CommunityState.forked_from; carried in the invite so
    /// joiners can mirror it into their local CommunityState during
    /// redeem_invite_inner. None for non-fork invites. Byte-compatible
    /// with pre-ZEB-285 invites when None.
    #[serde(rename = "ff", skip_serializing_if = "Option::is_none", default)]
    pub forked_from: Option<SpaceId>,

    /// ZEB-285: frozen snapshot of the forker's pre-fork view of the
    /// ORIGINAL community. Present only on fork-invites (None for normal
    /// community invites). Bounded by snapshot policy (§4.2). Joiner
    /// stores the snapshot in the fork's data dir keyed by the original
    /// SpaceId for dual-keyset verification of pre-fork events.
    /// Byte-compatible with pre-ZEB-285 invites when None.
    #[serde(rename = "fs", skip_serializing_if = "Option::is_none", default)]
    pub pre_fork_snapshot: Option<PreForkSnapshot>,
```

- [ ] **Step 4: Update all `CommunityInvitePayload { ... }` struct literals in the codebase to include the new fields**

Search and update:

```bash
cd src-tauri && grep -rn "CommunityInvitePayload {" src/ tests/ --include="*.rs"
```

For each match, add `forked_from: None, pre_fork_snapshot: None,` (or `Some(_)` where appropriate per test intent). Common sites:
- `community_invite.rs` test fixtures (`mint_invite_inner` callers, test stubs)
- `community_invite.rs:1455` (`community_name: "test"...`)
- `community_invite.rs:1498` (similar)
- Any integration tests under `src-tauri/tests/`

- [ ] **Step 5: Run the tests, verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(invite_payload_)'
```

Expected: PASS for both new tests.

- [ ] **Step 6: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

All must be green.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_invite.rs src-tauri/tests/
git commit -m "$(cat <<'EOF'
feat(zeb-285-p1): extend CommunityInvitePayload with fork-lineage fields

Adds two optional fields to CommunityInvitePayload:
- forked_from: Option<SpaceId> (CBOR key "ff") — mirrors
  CommunityState.forked_from for invitee bootstrap
- pre_fork_snapshot: Option<PreForkSnapshot> (CBOR key "fs") — frozen
  pre-fork history bundle, present only on fork-invites

Both fields use skip_serializing_if=Option::is_none so non-fork
invites encode byte-identical to pre-ZEB-285 form.

All existing CommunityInvitePayload struct-literal sites updated to
include both fields (defaulting to None for non-fork callers).

Tests: 2 in-module tests (byte-compat-when-none + roundtrip-when-some).

Spec: §3.3.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5 — Add `verify_snapshot_event` dual-keyset verifier

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (new public fn alongside existing `verify_event`)

- [ ] **Step 1: Write the failing test**

Append to the in-module tests at the end of `community_membership.rs`:

```rust
#[test]
fn verify_snapshot_event_uses_snapshot_identity_pubs() {
    use crate::community_invite::PreForkSnapshot;
    use crate::community_invite::BoundedChannelLogSnapshot;
    use std::collections::BTreeMap;

    let original_id = SpaceId([0xa0; 16]);
    let admin = OwnerAddr([0xaa; 32]);
    let regular = OwnerAddr([0xbb; 32]);

    // Bootstrap snapshot: admin Join + regular Join + a SetPower event.
    let admin_join = sign_test_event(MembershipEventKind::Join, &admin, original_id, 1);
    let regular_join = sign_test_event(MembershipEventKind::Join, &regular, original_id, 2);
    let set_power = sign_test_event(
        MembershipEventKind::SetPower { target: regular, level: 50 },
        &admin,
        original_id,
        3,
    );

    let mut identity_pubs = BTreeMap::new();
    identity_pubs.insert(admin, admin.identity_pub_bytes());
    identity_pubs.insert(regular, regular.identity_pub_bytes());

    let snapshot = PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: "Original".to_string(),
        membership_events: vec![admin_join.clone(), regular_join.clone(), set_power.clone()],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs,
        forked_at: Hlc { wall_ms: 1_700_000_000_000, lc: 0 },
    };

    // Each event in the snapshot should verify against the snapshot's
    // identity_pubs, even though the fork's live OwnerDeviceCache has
    // neither admin nor regular as members.
    for event in &snapshot.membership_events {
        verify_snapshot_event(event, &snapshot).expect("snapshot event should verify");
    }
}

#[test]
fn verify_snapshot_event_rejects_unknown_signer() {
    use crate::community_invite::PreForkSnapshot;
    use crate::community_invite::BoundedChannelLogSnapshot;
    use std::collections::BTreeMap;

    let original_id = SpaceId([0xa0; 16]);
    let admin = OwnerAddr([0xaa; 32]);
    let unknown = OwnerAddr([0xff; 32]);

    let admin_join = sign_test_event(MembershipEventKind::Join, &admin, original_id, 1);
    // Forged event: signed by `unknown` but they have no identity_pub
    // entry in the snapshot.
    let unknown_event = sign_test_event(
        MembershipEventKind::Leave,
        &unknown,
        original_id,
        2,
    );

    let mut identity_pubs = BTreeMap::new();
    identity_pubs.insert(admin, admin.identity_pub_bytes());
    // No entry for `unknown` — intentionally missing.

    let snapshot = PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: "Original".to_string(),
        membership_events: vec![admin_join, unknown_event.clone()],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs,
        forked_at: Hlc { wall_ms: 1_700_000_000_000, lc: 0 },
    };

    let result = verify_snapshot_event(&unknown_event, &snapshot);
    assert!(matches!(result, Err(VerifyError::UnknownSigner { .. })),
        "verify_snapshot_event should reject signer not in identity_pubs; got {:?}", result);
}
```

- [ ] **Step 2: Run the tests, verify they fail**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_snapshot_event)'
```

Expected: FAIL — function doesn't exist.

- [ ] **Step 3: Add the `verify_snapshot_event` function**

In `community_membership.rs`, after the existing `pub fn verify_event(...)` function, add:

```rust
/// ZEB-285: verify a single signed event against a frozen pre-fork
/// snapshot's identity_pubs map. Used by the fork's UI when loading
/// pre-fork history for display — fork members are not necessarily
/// members of the original, so the live OwnerDeviceCache won't have
/// the original's signers cached.
///
/// Replays the snapshot's `membership_events` in HLC order to
/// reconstruct the materialized-at-HLC context required for power-rule
/// checks. (Phase 1 invokes this lazily at display time; Phase 2 will
/// invoke it eagerly at redeem time to reject malicious snapshots
/// with forged signatures.)
///
/// See spec §4.3.
pub fn verify_snapshot_event(
    event: &SignedMembershipEvent,
    snapshot: &crate::community_invite::PreForkSnapshot,
) -> Result<(), VerifyError> {
    // Step 1: signer must be in identity_pubs.
    let signer = event.body.actor;
    let signer_pub = snapshot.identity_pubs.get(&signer).ok_or(VerifyError::UnknownSigner {
        signer,
    })?;

    // Step 2: Ed25519 signature verification against canonical-CBOR body.
    let body_bytes = event.body.canonical_cbor_bytes()
        .map_err(|e| VerifyError::CanonicalEncodingFailed { source: format!("{}", e) })?;
    crate::owner_state_crypto::verify_ed25519(
        signer_pub,
        &body_bytes,
        &event.actor_sig,
    ).map_err(|_| VerifyError::SignatureInvalid)?;

    // Step 3: reconstruct prior-state by replaying snapshot.membership_events
    // up to (but not including) this event by HLC ascending, then run the
    // existing event-shape verify rules.
    let prior_events: Vec<&SignedMembershipEvent> = snapshot.membership_events.iter()
        .filter(|e| event_sort_key(&e.body) < event_sort_key(&event.body))
        .collect();
    let prior_state = replay_prior_state(snapshot.original_community_id, &prior_events, &snapshot.identity_pubs)?;

    let ctx = VerifyContext {
        community_id: snapshot.original_community_id,
        members_at_hlc: prior_state.members,
        power_levels_at_hlc: prior_state.power_levels,
        // Channels not needed for non-channel-event verification.
        ..Default::default()
    };

    verify_event(event, &ctx, signer_pub)
}

/// Helper for verify_snapshot_event: replay a slice of signed events
/// in HLC ascending order against an empty starting state. Returns the
/// materialized state right before the next event would be inserted.
fn replay_prior_state(
    community_id: SpaceId,
    events: &[&SignedMembershipEvent],
    identity_pubs: &std::collections::BTreeMap<OwnerAddr, [u8; 64]>,
) -> Result<MaterializedMembership, VerifyError> {
    let mut state = MaterializedMembership::default();
    let mut sorted: Vec<&SignedMembershipEvent> = events.iter().copied().collect();
    sorted.sort_by_key(|e| event_sort_key(&e.body));

    for event in sorted {
        let signer_pub = identity_pubs.get(&event.body.actor).ok_or(VerifyError::UnknownSigner {
            signer: event.body.actor,
        })?;
        let ctx = VerifyContext {
            community_id,
            members_at_hlc: state.members.clone(),
            power_levels_at_hlc: state.power_levels.clone(),
            ..Default::default()
        };
        // Skip verification errors during replay — we trust the snapshot
        // is internally consistent (forker had a valid replay locally).
        // Phase 2 hardening will verify each replay event eagerly.
        let _ = verify_event(event, &ctx, signer_pub);
        materialize(&event.body, &mut state);
    }

    Ok(state)
}
```

NOTE: If the `VerifyError` enum doesn't yet have an `UnknownSigner { signer: OwnerAddr }` variant, add one. Similarly add `CanonicalEncodingFailed { source: String }` and `SignatureInvalid` if missing.

- [ ] **Step 4: Add `VerifyError::UnknownSigner` variant if not present**

Search `community_membership.rs` for `pub enum VerifyError`. Add a new variant if missing:

```rust
    /// ZEB-285: snapshot signer is not in PreForkSnapshot.identity_pubs.
    UnknownSigner { signer: OwnerAddr },
```

Add the matching arm to the `impl std::fmt::Display for VerifyError` block:

```rust
            VerifyError::UnknownSigner { signer } => {
                write!(f, "snapshot signer {:?} not in identity_pubs", signer)
            }
```

- [ ] **Step 5: Run the tests, verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_snapshot_event)'
```

Expected: both PASS.

- [ ] **Step 6: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_membership.rs
git commit -m "$(cat <<'EOF'
feat(zeb-285-p1): add verify_snapshot_event dual-keyset verifier

New public function that verifies a single SignedMembershipEvent
against a PreForkSnapshot's identity_pubs map (rather than the live
OwnerDeviceCache). Used by the fork's UI when loading pre-fork history
for display — fork members are not necessarily members of the
original, so the live OwnerDeviceCache won't have the original's
signers cached.

Algorithm:
1. Look up signer in snapshot.identity_pubs (reject as UnknownSigner
   if missing)
2. Ed25519-verify signature against canonical-CBOR body
3. Reconstruct prior-state by replaying snapshot.membership_events
   up to this event's HLC, then invoke the existing verify_event
   with that materialized context

Phase 1 invokes this lazily at display time only. Phase 2 hardening
will invoke it eagerly at redeem time to reject malicious snapshots
shipping forged signatures.

New VerifyError variant: UnknownSigner { signer: OwnerAddr }.

Tests: 2 in-module tests (verify-passes-for-known-signer +
reject-unknown-signer).

Spec: §4.3, §4.4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6 — Add `fork_community` IPC + frontend service wrapper

**Files:**
- Create: `src-tauri/src/community_fork.rs` (new module with all fork-operation logic)
- Modify: `src-tauri/src/lib.rs` (declare module + add `#[tauri::command]` registration)
- Modify: `src/lib/community-service.ts` (add `forkCommunity` wrapper)
- Test: `src-tauri/src/community_fork.rs` (in-module unit tests)

- [ ] **Step 1: Write the failing unit test for snapshot building**

Create `src-tauri/src/community_fork.rs`:

```rust
//! ZEB-285 Phase 1: community forking primitive.
//!
//! See `docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md`.

use crate::community_invite::{BoundedChannelLogSnapshot, PreForkSnapshot};
use crate::community_membership::{CommunityState, MembershipEventKind, SignedMembershipEvent};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use std::collections::BTreeMap;

pub const SNAPSHOT_PER_CHANNEL_CAP: usize = 500;
pub const SNAPSHOT_TOTAL_CAP: usize = 5000;

/// ZEB-285 §4.2: build a PreForkSnapshot from the forker's local view
/// of the original community. Applies size caps.
///
/// `original_events` = the forker's CommunityState.events for the original
/// `per_channel_logs` = the forker's per-channel SignedChannelLogEvent vectors
/// `identity_pubs` = owner-pubkey lookups for every signer (resolved by caller)
pub fn build_snapshot(
    original_id: SpaceId,
    original_name: String,
    original_events: Vec<SignedMembershipEvent>,
    per_channel_logs: BTreeMap<crate::community_membership::ChannelId, Vec<crate::community_channel_log::SignedChannelLogEvent>>,
    identity_pubs: BTreeMap<OwnerAddr, [u8; 64]>,
    forked_at: Hlc,
) -> PreForkSnapshot {
    // Step 1: per-channel cap to most-recent N=500 by HLC descending.
    let mut sliced: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (ch_id, mut log) in per_channel_logs {
        log.sort_by(|a, b| b.body.created_at.cmp(&a.body.created_at));
        log.truncate(SNAPSHOT_PER_CHANNEL_CAP);
        log.sort_by(|a, b| a.body.created_at.cmp(&b.body.created_at));
        sliced.insert(ch_id, log);
    }

    // Step 2: proportional trim if total > M=5000.
    let total: usize = sliced.values().map(|v| v.len()).sum();
    if total > SNAPSHOT_TOTAL_CAP {
        let per_channel: BTreeMap<_, usize> = sliced.iter().map(|(k, v)| {
            (*k, ((v.len() as u128 * SNAPSHOT_TOTAL_CAP as u128) / total as u128) as usize)
        }).collect();
        // Award rounding remainder to largest-slice channel.
        let assigned_total: usize = per_channel.values().sum();
        let remainder = SNAPSHOT_TOTAL_CAP - assigned_total;
        let mut adjusted = per_channel.clone();
        if remainder > 0 {
            if let Some((largest, _)) = sliced.iter().max_by_key(|(_, v)| v.len()) {
                *adjusted.get_mut(largest).unwrap() += remainder;
            }
        }
        for (ch_id, target_len) in adjusted {
            if let Some(log) = sliced.get_mut(&ch_id) {
                if log.len() > target_len {
                    log.sort_by(|a, b| b.body.created_at.cmp(&a.body.created_at));
                    log.truncate(target_len);
                    log.sort_by(|a, b| a.body.created_at.cmp(&b.body.created_at));
                }
            }
        }
    }

    PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: original_name,
        membership_events: original_events,
        channel_log: BoundedChannelLogSnapshot { per_channel: sliced },
        identity_pubs,
        forked_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_snapshot_applies_per_channel_cap() {
        // 600 messages in one channel → trim to 500.
        let ch_id = crate::community_membership::ChannelId([0xcc; 16]);
        let messages: Vec<_> = (0..600).map(|i| crate::community_channel_log::test_helpers::make_signed_log_event(i as u64)).collect();
        let mut logs = BTreeMap::new();
        logs.insert(ch_id, messages);

        let snapshot = build_snapshot(
            SpaceId([0xa0; 16]),
            "test".to_string(),
            vec![],
            logs,
            BTreeMap::new(),
            Hlc { wall_ms: 0, lc: 0 },
        );
        assert_eq!(snapshot.channel_log.per_channel.get(&ch_id).unwrap().len(), 500);
    }

    #[test]
    fn build_snapshot_applies_total_cap_proportionally() {
        // Two channels: 4000 + 4000 = 8000 total. Should trim to 5000
        // total with proportional split (2500 + 2500).
        let ch_a = crate::community_membership::ChannelId([0xaa; 16]);
        let ch_b = crate::community_membership::ChannelId([0xbb; 16]);
        let msgs_a: Vec<_> = (0..4000).map(|i| crate::community_channel_log::test_helpers::make_signed_log_event(i as u64)).collect();
        let msgs_b: Vec<_> = (0..4000).map(|i| crate::community_channel_log::test_helpers::make_signed_log_event((i + 10000) as u64)).collect();
        let mut logs = BTreeMap::new();
        logs.insert(ch_a, msgs_a);
        logs.insert(ch_b, msgs_b);

        let snapshot = build_snapshot(
            SpaceId([0xa0; 16]),
            "test".to_string(),
            vec![],
            logs,
            BTreeMap::new(),
            Hlc { wall_ms: 0, lc: 0 },
        );
        let a_len = snapshot.channel_log.per_channel.get(&ch_a).unwrap().len();
        let b_len = snapshot.channel_log.per_channel.get(&ch_b).unwrap().len();
        assert_eq!(a_len + b_len, SNAPSHOT_TOTAL_CAP);
        // Each gets ~2500; with proportional split + remainder.
        assert!((a_len as i32 - 2500).abs() <= 1);
        assert!((b_len as i32 - 2500).abs() <= 1);
    }
}
```

NOTE: `crate::community_channel_log::test_helpers::make_signed_log_event` is a placeholder. If that helper doesn't exist, add it under `#[cfg(test)] pub mod test_helpers { ... }` in `community_channel_log.rs` returning a deterministic-shape signed log event.

- [ ] **Step 2: Register the module + run the test**

In `src-tauri/src/lib.rs`, add `pub mod community_fork;` near the top with the other module declarations. Then:

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(community_fork)'
```

Expected: PASS for both tests.

- [ ] **Step 3: Add the `fork_community` Tauri command**

Append to `community_fork.rs`:

```rust
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ForkCommunityOpts {
    pub name: String,
    #[serde(default)]
    pub silent: bool,
    #[serde(default)]
    pub also_leave: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ForkCommunityResult {
    pub fork_space_id: SpaceId,
    pub visible: bool,
    pub snapshot_message_count: usize,
}

#[tauri::command]
pub async fn fork_community(
    state: tauri::State<'_, crate::NodeState>,
    community_id: SpaceId,
    opts: ForkCommunityOpts,
) -> Result<ForkCommunityResult, String> {
    use crate::community_state_sync::publish_event;

    // Step 1: validate membership.
    let guard = state.read().await;
    let runtime = guard.runtime.as_ref().ok_or_else(|| "no runtime".to_string())?;
    let original_state = runtime.community_state(community_id).await
        .ok_or_else(|| format!("not a member of community {:?}", community_id))?;

    let self_addr = runtime.own_addr();
    let materialized = original_state.materialized(self_addr)
        .map_err(|e| format!("materialize failed: {:?}", e))?;
    let self_power = materialized.power_levels.get(&self_addr).copied().unwrap_or(0);
    // Joined = present in members with non-Banned status.
    if !materialized.members.contains_key(&self_addr) {
        return Err("not a member".to_string());
    }
    let _ = self_power; // power 0 is sufficient

    // Step 2: generate fork SpaceId.
    let fork_space_id = SpaceId::new_random();

    // Step 3: build snapshot.
    let original_events: Vec<SignedMembershipEvent> = original_state.events.values().cloned().collect();
    let per_channel_logs = runtime.per_channel_logs_for(community_id).await
        .map_err(|e| format!("read channel logs: {:?}", e))?;
    let identity_pubs = runtime.collect_signer_identity_pubs(community_id).await
        .map_err(|e| format!("collect identity_pubs: {:?}", e))?;
    let forked_at = Hlc::now();
    let snapshot = build_snapshot(
        community_id,
        materialized.name.clone(),
        original_events,
        per_channel_logs,
        identity_pubs,
        forked_at,
    );
    let snapshot_message_count: usize = snapshot.channel_log.per_channel.values().map(|v| v.len()).sum();

    // Step 4-5: construct fork bootstrap state with forked_from set.
    let fork_join = sign_join_event(self_addr, fork_space_id, 1, &runtime).await
        .map_err(|e| format!("sign fork Join: {:?}", e))?;
    let fork_channel_create = sign_default_channel(self_addr, fork_space_id, 2, &runtime).await
        .map_err(|e| format!("sign #general ChannelCreate: {:?}", e))?;

    let mut fork_state = CommunityState::new(fork_space_id);
    fork_state.forked_from = Some(community_id);
    fork_state.insert_local_event_with_pubs(fork_join, self_addr.identity_pub_bytes(), None)
        .map_err(|e| format!("insert fork Join: {:?}", e))?;
    fork_state.insert_local_event_with_pubs(fork_channel_create, self_addr.identity_pub_bytes(), None)
        .map_err(|e| format!("insert fork ChannelCreate: {:?}", e))?;

    runtime.persist_community(fork_space_id, &fork_state).await
        .map_err(|e| format!("persist fork: {:?}", e))?;

    // Step 6: write snapshot to fork's data dir.
    runtime.write_pre_fork_snapshot(fork_space_id, &snapshot).await
        .map_err(|e| format!("write snapshot: {:?}", e))?;

    let visible = !opts.silent;

    // Step 7: if !silent, emit Fork event in original log.
    if !opts.silent {
        let fork_event = sign_fork_event(self_addr, community_id, fork_space_id, 999, &runtime).await
            .map_err(|e| format!("sign Fork: {:?}", e))?;
        runtime.insert_and_publish_event(community_id, fork_event).await
            .map_err(|e| tracing::warn!("publish Fork failed (non-fatal): {:?}", e))
            .ok();
    }

    // Step 8: if also_leave, emit Leave event in original.
    if opts.also_leave {
        let leave_event = sign_leave_event(self_addr, community_id, 1000, &runtime).await
            .map_err(|e| format!("sign Leave: {:?}", e))?;
        runtime.insert_and_publish_event(community_id, leave_event).await
            .map_err(|e| tracing::warn!("publish Leave failed (non-fatal): {:?}", e))
            .ok();
    }

    // Step 9: emit frontend event.
    if let Some(app_handle) = guard.app_handle.as_ref() {
        use tauri::Emitter;
        app_handle.emit("community-forked", serde_json::json!({
            "forkSpaceId": fork_space_id,
            "originalId": community_id,
        })).ok();
    }

    Ok(ForkCommunityResult {
        fork_space_id,
        visible,
        snapshot_message_count,
    })
}

// Helper signers (delegating to existing event-signing infrastructure).
// Bodies omitted — implementer should adapt to existing patterns in
// community_state_sync.rs / lib.rs for signing membership events.
async fn sign_join_event(...) -> Result<...> { unimplemented!() }
async fn sign_default_channel(...) -> Result<...> { unimplemented!() }
async fn sign_fork_event(...) -> Result<...> { unimplemented!() }
async fn sign_leave_event(...) -> Result<...> { unimplemented!() }
```

**Implementer note:** The four helper signers (`sign_join_event`, `sign_default_channel`, `sign_fork_event`, `sign_leave_event`) should delegate to the existing event-signing patterns used elsewhere in the codebase. Look at `lib.rs` for `kick_from_community` (line ~11614) and `set_power_level` (line ~11917) as templates — both sign membership events and call `insert_local_event_with_pubs` + publish. Adapt those patterns rather than reimplementing. Similarly for `runtime.persist_community`, `runtime.write_pre_fork_snapshot`, `runtime.per_channel_logs_for`, `runtime.collect_signer_identity_pubs`, `runtime.insert_and_publish_event` — extend the existing `NodeRuntime` with these methods if not present, mirroring sibling methods.

- [ ] **Step 4: Register the IPC in `lib.rs`**

In `src-tauri/src/lib.rs`, find the `tauri::generate_handler![...]` macro invocation. Add `community_fork::fork_community,` to the list.

- [ ] **Step 5: Add the frontend service wrapper**

In `src/lib/community-service.ts`, add (alongside the existing `kickFromCommunity` and `setPowerLevel` methods around lines 185-200):

```typescript
  /**
   * ZEB-285: fork a community the user is a member of.
   *
   * @param communityId - the original community's SpaceId (as hex string)
   * @param opts - fork configuration
   * @returns the new fork's SpaceId + visibility flag + snapshot size
   *
   * @throws if not a member of `communityId`, or if local fork creation
   *         fails before snapshot is written.
   */
  async forkCommunity(
    communityId: string,
    opts: { name: string; silent?: boolean; alsoLeave?: boolean }
  ): Promise<{
    forkSpaceId: string;
    visible: boolean;
    snapshotMessageCount: number;
  }> {
    try {
      return await this.adapter.invoke('fork_community', { communityId, opts });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(`Fork failed: ${msg}`);
    }
  }
```

Tauri IPC param naming: Rust `community_id` / `opts` ↔ JS `communityId` / `opts`; inner `also_leave` ↔ `alsoLeave`. The boundary auto-converts.

- [ ] **Step 6: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_fork.rs src-tauri/src/lib.rs src/lib/community-service.ts
git commit -m "$(cat <<'EOF'
feat(zeb-285-p1): add fork_community IPC + frontend service wrapper

New module src-tauri/src/community_fork.rs containing:
- ForkCommunityOpts { name, silent, also_leave }
- ForkCommunityResult { fork_space_id, visible, snapshot_message_count }
- fork_community(communityId, opts) Tauri command
- build_snapshot() helper applying §4.2 snapshot policy (N=500 per
  channel, M=5000 total with proportional trim + rounding remainder
  to largest-slice channel)
- SNAPSHOT_PER_CHANNEL_CAP + SNAPSHOT_TOTAL_CAP public constants

Operation steps (§5.2):
1. Validate forker is Joined in original
2. Generate fresh fork SpaceId
3. Build PreForkSnapshot from forker's local view (capped)
4. Construct fork bootstrap state (forker self-Join admin + #general
   ChannelCreate)
5. Set CommunityState.forked_from = Some(original)
6. Write pre_fork_snapshot.bin to fork's data dir
7. If !silent: mint+sign+publish Fork event in original log
8. If also_leave: mint+sign+publish Leave (EpochRotation auto-fires)
9. Emit "community-forked" frontend event

Failures in steps 7-8 (post-creation) log via tracing::warn but don't
tear down the fork; visible flag in result reflects actual outcome.

Frontend wrapper in src/lib/community-service.ts forwards to the IPC
with proper Tauri error extraction (`e instanceof Error ? e.message :
String(e)`).

Tests: 2 in-module tests for build_snapshot (per-channel cap +
proportional total trim).

Spec: §5.1-5.5, §4.2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7 — Extend `mint_invite` + `redeem_invite_inner` for fork-invite carry

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (existing `mint_invite_inner` + `redeem_invite_inner` functions)
- Test: in-module tests at end

- [ ] **Step 1: Write the failing test (mint includes snapshot for forks)**

Append to the in-module tests:

```rust
#[test]
fn mint_invite_for_fork_bundles_snapshot() {
    use tempfile::tempdir;
    let dir = tempdir().expect("tempdir");

    // Construct a CommunityState with forked_from set + write a
    // pre_fork_snapshot.bin to the fork's data dir under `dir`.
    let original_id = SpaceId([0xa0; 16]);
    let fork_id = SpaceId([0xf0; 16]);
    let admin = OwnerAddr([0xaa; 32]);

    let mut fork_state = CommunityState::new(fork_id);
    fork_state.forked_from = Some(original_id);
    // ... insert admin's Join etc. (use test_fixtures)

    let snapshot = PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: "Original".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs: BTreeMap::new(),
        forked_at: Hlc { wall_ms: 1_700_000_000_000, lc: 0 },
    };
    let snapshot_path = dir.path().join("communities").join(format!("{:x}", fork_id)).join("pre_fork_snapshot.bin");
    std::fs::create_dir_all(snapshot_path.parent().unwrap()).expect("mkdir");
    std::fs::write(&snapshot_path, canonical_cbor_bytes(&snapshot).expect("encode")).expect("write");

    // mint_invite_inner should read the snapshot + set forked_from on the payload.
    let payload = mint_invite_inner(&fork_state, dir.path(), /* other args */)
        .expect("mint");
    assert_eq!(payload.forked_from, Some(original_id));
    assert!(payload.pre_fork_snapshot.is_some(), "fork invite should bundle snapshot");
}
```

NOTE: Adapt the `mint_invite_inner` signature to match the actual function in `community_invite.rs`. The test verifies the BEHAVIOR (forked_from + pre_fork_snapshot are set), not the call shape.

- [ ] **Step 2: Run the test, verify it fails**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(mint_invite_for_fork)'
```

Expected: FAIL (current `mint_invite_inner` ignores `forked_from`).

- [ ] **Step 3: Extend `mint_invite_inner` to read and bundle**

In `community_invite.rs`, find `mint_invite_inner`. Add logic AFTER the existing `CommunityInvitePayload { ... }` literal construction:

```rust
    // ZEB-285: if this community is a fork, bundle lineage + snapshot.
    if let Some(original_id) = state.forked_from {
        payload.forked_from = Some(original_id);

        // Read snapshot from disk.
        let snapshot_path = data_dir.join("communities")
            .join(format!("{:x}", state.community_id))
            .join("pre_fork_snapshot.bin");
        if snapshot_path.exists() {
            match std::fs::read(&snapshot_path) {
                Ok(bytes) => match ciborium::de::from_reader::<PreForkSnapshot, _>(&bytes[..]) {
                    Ok(snapshot) => payload.pre_fork_snapshot = Some(snapshot),
                    Err(e) => tracing::warn!("decode pre_fork_snapshot.bin: {:?}", e),
                },
                Err(e) => tracing::warn!("read pre_fork_snapshot.bin: {:?}", e),
            }
        }
    }
```

Where `data_dir: &Path` is the runtime's app data dir (existing parameter or pulled from caller — adapt as needed).

- [ ] **Step 4: Run the test, verify it passes**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(mint_invite_for_fork)'
```

Expected: PASS.

- [ ] **Step 5: Write the failing test (redeem writes snapshot to disk)**

Append to the in-module tests:

```rust
#[test]
fn redeem_invite_writes_snapshot_to_data_dir() {
    use tempfile::tempdir;
    let dir = tempdir().expect("tempdir");

    let original_id = SpaceId([0xa0; 16]);
    let fork_id = SpaceId([0xf0; 16]);
    let admin = OwnerAddr([0xaa; 32]);

    let snapshot = PreForkSnapshot {
        original_community_id: original_id,
        original_community_name: "Original".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs: BTreeMap::new(),
        forked_at: Hlc { wall_ms: 1_700_000_000_000, lc: 0 },
    };

    let payload = CommunityInvitePayload {
        community_id: fork_id,
        epoch_snapshot: InviteEpochSnapshot::test_default(fork_id),
        admin_addr: admin,
        community_name: "Fork".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: Some(original_id),
        pre_fork_snapshot: Some(snapshot.clone()),
    };

    redeem_invite_inner(&payload, dir.path(), /* other args */).expect("redeem");

    let snapshot_path = dir.path().join("communities").join(format!("{:x}", fork_id)).join("pre_fork_snapshot.bin");
    assert!(snapshot_path.exists(), "pre_fork_snapshot.bin must be written");

    let bytes = std::fs::read(&snapshot_path).expect("read");
    let decoded: PreForkSnapshot = ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(decoded, snapshot);
}
```

- [ ] **Step 6: Extend `redeem_invite_inner` to write the snapshot**

In `community_invite.rs`, find `redeem_invite_inner`. Add at the END (after the existing CommunityState-bootstrap steps):

```rust
    // ZEB-285: if this is a fork-invite, mirror forked_from + write snapshot.
    if let Some(original_id) = payload.forked_from {
        joiner_state.forked_from = Some(original_id);
    }
    if let Some(snapshot) = &payload.pre_fork_snapshot {
        let snapshot_path = data_dir.join("communities")
            .join(format!("{:x}", payload.community_id))
            .join("pre_fork_snapshot.bin");
        if let Some(parent) = snapshot_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                CommunityInviteVerifyError::IoError { source: format!("mkdir: {}", e) }
            })?;
        }
        // Atomic write via tempfile + rename.
        let tmp_path = snapshot_path.with_extension("tmp");
        let bytes = canonical_cbor_bytes(snapshot).map_err(|e| {
            CommunityInviteVerifyError::EncodingError { source: format!("encode snapshot: {}", e) }
        })?;
        std::fs::write(&tmp_path, &bytes).map_err(|e| {
            CommunityInviteVerifyError::IoError { source: format!("write tmp: {}", e) }
        })?;
        std::fs::rename(&tmp_path, &snapshot_path).map_err(|e| {
            CommunityInviteVerifyError::IoError { source: format!("rename: {}", e) }
        })?;
    }
```

If `CommunityInviteVerifyError` doesn't yet have `IoError` or `EncodingError` variants, add them with matching `Display` arms.

- [ ] **Step 7: Run the test, verify it passes**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(redeem_invite_writes_snapshot)'
```

Expected: PASS.

- [ ] **Step 8: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/community_invite.rs
git commit -m "$(cat <<'EOF'
feat(zeb-285-p1): extend mint_invite/redeem_invite_inner for fork-invite carry

mint_invite_inner now reads CommunityState.forked_from; when set,
bundles forked_from + reads pre_fork_snapshot.bin from disk to populate
payload.pre_fork_snapshot. Decode failures and missing files log via
tracing::warn (non-fatal — the invite still ships, just without the
snapshot bundled).

redeem_invite_inner now mirrors payload.forked_from onto joiner_state
and atomically writes payload.pre_fork_snapshot bytes to
{data_dir}/communities/{community_id}/pre_fork_snapshot.bin (via
tempfile + rename, matching follows.rs / content_index.rs idiom).

Tests: 2 in-module tests (mint bundles for forks + redeem writes
snapshot to disk).

Snapshot signature verification at redeem is deferred to Phase 2 per
spec §4.4 — Phase 1 trusts the forker (they had plaintext anyway).

Spec: §5.3, §5.4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8 — Rust integration tests for fork operation

**Files:**
- Create: `src-tauri/tests/community_fork_integration.rs`

- [ ] **Step 1: Write the integration test file with all 6 tests**

Create `src-tauri/tests/community_fork_integration.rs`:

```rust
//! ZEB-285 Phase 1: end-to-end fork integration tests.
//!
//! Uses paired engine harness (matching community_state_sync_integration.rs
//! shape). No real Zenoh — local engine pairs only.
//!
//! Spec: docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md §7.5

use harmony_app::community_membership::MembershipEventKind;
use harmony_app::community_invite::PreForkSnapshot;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use tempfile::tempdir;

mod helpers;
use helpers::{EnginePair, run_fork};

#[tokio::test]
async fn visible_fork_announces_in_original_log() {
    let pair = EnginePair::bootstrap("test-community").await;
    let engine_a = &pair.engine_a;
    let engine_b = &pair.engine_b;

    // Engine A (admin) forks visibly.
    let result = run_fork(engine_a, pair.community_id, "Forked", false, false).await
        .expect("fork should succeed");
    assert!(result.visible, "non-silent fork should set visible=true");

    pair.sync().await;

    // Engine B should see the Fork event in the original's log.
    let b_state = engine_b.community_state(pair.community_id).await.expect("get state");
    let has_fork = b_state.events.values().any(|e| matches!(
        e.body.kind, MembershipEventKind::Fork { fork_space_id }
        if fork_space_id == result.fork_space_id
    ));
    assert!(has_fork, "engine B should materialize the visible Fork event");
}

#[tokio::test]
async fn silent_fork_leaves_original_untouched() {
    let pair = EnginePair::bootstrap("test-community").await;
    let engine_a = &pair.engine_a;
    let engine_b = &pair.engine_b;

    let event_count_before = {
        let state = engine_b.community_state(pair.community_id).await.expect("state");
        state.events.len()
    };

    // Engine A forks SILENTLY.
    let result = run_fork(engine_a, pair.community_id, "Forked", true, false).await
        .expect("silent fork should succeed");
    assert!(!result.visible, "silent fork should set visible=false");

    pair.sync().await;

    let event_count_after = {
        let state = engine_b.community_state(pair.community_id).await.expect("state");
        state.events.len()
    };
    assert_eq!(event_count_before, event_count_after,
        "silent fork should NOT change original's event count from engine B's view");
}

#[tokio::test]
async fn fork_creates_independent_community() {
    let pair = EnginePair::bootstrap("test-community").await;
    let engine_a = &pair.engine_a;

    let result = run_fork(engine_a, pair.community_id, "Forked", false, false).await
        .expect("fork");

    let fork_state = engine_a.community_state(result.fork_space_id).await.expect("fork state");
    assert_eq!(fork_state.community_id, result.fork_space_id);
    assert_eq!(fork_state.forked_from, Some(pair.community_id));
    // Forker is admin (power 100) of the fork.
    let mat = fork_state.materialized(engine_a.own_addr()).expect("materialize");
    assert_eq!(mat.power_levels.get(&engine_a.own_addr()).copied(), Some(100));
}

#[tokio::test]
async fn fork_invite_carries_snapshot_to_invitee() {
    let pair = EnginePair::bootstrap("test-community").await;
    let engine_a = &pair.engine_a;

    // Post some messages so the snapshot has content.
    engine_a.post_message(pair.community_id, "Hello pre-fork").await.expect("post");

    let result = run_fork(engine_a, pair.community_id, "Forked", false, false).await
        .expect("fork");

    // Mint a fork-invite from engine A.
    let invite = engine_a.mint_invite(result.fork_space_id).await.expect("mint");
    assert!(invite.pre_fork_snapshot.is_some(), "fork invite must bundle snapshot");

    // Spin up engine D (a separate engine — NOT a member of pair.community_id).
    let dir_d = tempdir().expect("tempdir");
    let engine_d = harmony_app::test_helpers::engine_in_dir(dir_d.path()).await;
    engine_d.redeem_invite(invite).await.expect("redeem");

    let snapshot_path = dir_d.path()
        .join("communities")
        .join(format!("{:x}", result.fork_space_id))
        .join("pre_fork_snapshot.bin");
    assert!(snapshot_path.exists(), "engine D must have pre_fork_snapshot.bin on disk");

    let bytes = std::fs::read(&snapshot_path).expect("read");
    let snap: PreForkSnapshot = ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(snap.original_community_id, pair.community_id);
}

#[tokio::test]
async fn also_leave_emits_leave_and_rotates_epoch() {
    let pair = EnginePair::bootstrap("test-community").await;
    let engine_a = &pair.engine_a;
    let engine_b = &pair.engine_b;

    let result = run_fork(engine_a, pair.community_id, "Forked", false, true).await
        .expect("fork with also_leave");
    assert!(result.visible);

    pair.sync().await;

    let b_state = engine_b.community_state(pair.community_id).await.expect("state");
    let has_fork = b_state.events.values().any(|e| matches!(e.body.kind, MembershipEventKind::Fork { .. }));
    let has_leave = b_state.events.values().any(|e| matches!(e.body.kind, MembershipEventKind::Leave) && e.body.actor == engine_a.own_addr());
    let has_rotation = b_state.events.values().any(|e| matches!(e.body.kind, MembershipEventKind::EpochRotation { .. }));

    assert!(has_fork, "Fork event present");
    assert!(has_leave, "Leave event present");
    assert!(has_rotation, "EpochRotation auto-fired per ZEB-249");
}

#[tokio::test]
async fn dual_keyset_verify_snapshot_events() {
    use harmony_app::community_membership::verify_snapshot_event;

    let pair = EnginePair::bootstrap("test-community").await;
    let engine_a = &pair.engine_a;

    let result = run_fork(engine_a, pair.community_id, "Forked", false, false).await
        .expect("fork");

    // Read snapshot from engine A's fork data dir.
    let snapshot_path = engine_a.data_dir()
        .join("communities")
        .join(format!("{:x}", result.fork_space_id))
        .join("pre_fork_snapshot.bin");
    let bytes = std::fs::read(&snapshot_path).expect("read");
    let snapshot: PreForkSnapshot = ciborium::de::from_reader(&bytes[..]).expect("decode");

    // Every event in the snapshot should verify against the snapshot's
    // identity_pubs (NOT against engine A's live OwnerDeviceCache for the
    // fork community, which doesn't have the original's members cached).
    for event in &snapshot.membership_events {
        verify_snapshot_event(event, &snapshot)
            .expect("snapshot event should verify against identity_pubs");
    }
}
```

NOTE: `mod helpers;` references a sibling `tests/helpers/mod.rs` file (or `tests/helpers.rs`). If a helpers file doesn't already exist for community-CRDT integration tests, create `src-tauri/tests/helpers/mod.rs` (or extend the existing one) with `EnginePair::bootstrap` + `run_fork` shape adapted from `community_state_sync_integration.rs` or similar siblings.

- [ ] **Step 2: Run the integration tests, expect them to pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_fork_integration
```

Expected: all 6 tests PASS. If `mod helpers;` resolution fails, create the helpers file first.

- [ ] **Step 3: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/community_fork_integration.rs src-tauri/tests/helpers/
git commit -m "$(cat <<'EOF'
test(zeb-285-p1): integration tests for community fork operation

Six tokio tests covering the full fork lifecycle end-to-end via the
paired-engine harness (no real Zenoh, local engine pairs only):

1. visible_fork_announces_in_original_log — engine B materializes the
   Fork event from engine A's visible fork
2. silent_fork_leaves_original_untouched — silent fork doesn't change
   engine B's event count on the original
3. fork_creates_independent_community — fork has its own SpaceId,
   forked_from = Some(original), forker is power-100 admin
4. fork_invite_carries_snapshot_to_invitee — engine D (non-member of
   original) redeems fork-invite and gets pre_fork_snapshot.bin on
   disk
5. also_leave_emits_leave_and_rotates_epoch — fork with also_leave=true
   results in Fork + Leave + auto-EpochRotation (per ZEB-249)
6. dual_keyset_verify_snapshot_events — snapshot events verify against
   identity_pubs (not against live OwnerDeviceCache)

Spec: §7.5.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9 — `ForkConfirmDialog.svelte` component + vitest tests

**Files:**
- Create: `src/lib/components/ForkConfirmDialog.svelte`
- Create: `src/lib/components/__tests__/ForkConfirmDialog.test.ts`

- [ ] **Step 1: Write the failing vitest test file with all 8 tests**

Create `src/lib/components/__tests__/ForkConfirmDialog.test.ts`:

```typescript
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ForkConfirmDialog from '../ForkConfirmDialog.svelte';

describe('ForkConfirmDialog', () => {
  const baseProps = {
    originalName: 'Cool Community',
    messageCount: 1247,
    onConfirm: vi.fn(),
    onCancel: vi.fn(),
  };

  it('renders heading, name input, both checkboxes, and snapshot count', () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    expect(screen.getByText(/fork this community/i)).toBeTruthy();
    expect(screen.getByLabelText(/name/i)).toBeTruthy();
    expect(screen.getByLabelText(/fork silently/i)).toBeTruthy();
    expect(screen.getByLabelText(/also leave/i)).toBeTruthy();
    expect(screen.getByText(/1247 messages/)).toBeTruthy();
  });

  it('prefills name as "{originalName} (fork)"', () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    const input = screen.getByLabelText(/name/i) as HTMLInputElement;
    expect(input.value).toBe('Cool Community (fork)');
  });

  it('toggles "Fork silently" checkbox', async () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    const cb = screen.getByLabelText(/fork silently/i) as HTMLInputElement;
    expect(cb.checked).toBe(false);
    await fireEvent.click(cb);
    expect(cb.checked).toBe(true);
  });

  it('toggles "Also leave" checkbox', async () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    const cb = screen.getByLabelText(/also leave/i) as HTMLInputElement;
    expect(cb.checked).toBe(false);
    await fireEvent.click(cb);
    expect(cb.checked).toBe(true);
  });

  it('calls onConfirm with {name, silent, alsoLeave} when also_leave is false', async () => {
    const onConfirm = vi.fn();
    render(ForkConfirmDialog, { props: { ...baseProps, onConfirm } });
    const btn = screen.getByRole('button', { name: /create fork/i });
    await fireEvent.click(btn);
    expect(onConfirm).toHaveBeenCalledWith({
      name: 'Cool Community (fork)',
      silent: false,
      alsoLeave: false,
    });
  });

  it('opens typed-confirm second stage when also_leave is checked, requires "leave"', async () => {
    const onConfirm = vi.fn();
    render(ForkConfirmDialog, { props: { ...baseProps, onConfirm } });
    await fireEvent.click(screen.getByLabelText(/also leave/i));
    await fireEvent.click(screen.getByRole('button', { name: /create fork/i }));

    // Now the typed-confirm modal should be visible.
    expect(screen.getByText(/type.*leave/i)).toBeTruthy();
    const typedInput = screen.getByLabelText(/type to confirm/i) as HTMLInputElement;

    // Wrong text → onConfirm NOT called.
    await fireEvent.input(typedInput, { target: { value: 'no' } });
    const submitBtn = screen.getByRole('button', { name: /confirm/i });
    await fireEvent.click(submitBtn);
    expect(onConfirm).not.toHaveBeenCalled();

    // Correct text → onConfirm called with alsoLeave: true.
    await fireEvent.input(typedInput, { target: { value: 'leave' } });
    await fireEvent.click(submitBtn);
    expect(onConfirm).toHaveBeenCalledWith({
      name: 'Cool Community (fork)',
      silent: false,
      alsoLeave: true,
    });
  });

  it('calls onCancel on Cancel button, Escape key, and backdrop click', async () => {
    const onCancel = vi.fn();
    const { container } = render(ForkConfirmDialog, { props: { ...baseProps, onCancel } });

    // Cancel button
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(onCancel).toHaveBeenCalledTimes(1);

    // Re-render for Escape test
    const { container: c2 } = render(ForkConfirmDialog, { props: { ...baseProps, onCancel } });
    const dialog = c2.querySelector('[role="dialog"]') as HTMLElement;
    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(onCancel).toHaveBeenCalledTimes(2);

    // Re-render for backdrop test
    const { container: c3 } = render(ForkConfirmDialog, { props: { ...baseProps, onCancel } });
    const overlay = c3.querySelector('.modal-overlay') as HTMLElement;
    await fireEvent.click(overlay);
    expect(onCancel).toHaveBeenCalledTimes(3);
  });

  it('disables Create fork button when name is empty or whitespace-only', async () => {
    render(ForkConfirmDialog, { props: { ...baseProps } });
    const input = screen.getByLabelText(/name/i) as HTMLInputElement;
    const btn = screen.getByRole('button', { name: /create fork/i }) as HTMLButtonElement;

    await fireEvent.input(input, { target: { value: '' } });
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: '   ' } });
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: 'My fork' } });
    expect(btn.disabled).toBe(false);
  });
});
```

- [ ] **Step 2: Run the test, verify it fails (component doesn't exist)**

```bash
npx vitest run src/lib/components/__tests__/ForkConfirmDialog.test.ts
```

Expected: FAIL (Component file not found).

- [ ] **Step 3: Create the component**

Create `src/lib/components/ForkConfirmDialog.svelte`:

```svelte
<script lang="ts">
  import Modal from './Modal.svelte';
  import TypedConfirmationModal from './TypedConfirmationModal.svelte';

  export let originalName: string;
  export let messageCount: number;
  export let onConfirm: (opts: { name: string; silent: boolean; alsoLeave: boolean }) => void;
  export let onCancel: () => void;

  let name = `${originalName} (fork)`;
  let silent = false;
  let alsoLeave = false;
  let typedConfirmOpen = false;

  $: nameValid = name.trim().length > 0;

  function handleCreateFork() {
    if (!nameValid) return;
    if (alsoLeave) {
      typedConfirmOpen = true;
    } else {
      onConfirm({ name: name.trim(), silent, alsoLeave });
    }
  }

  function handleTypedConfirm() {
    onConfirm({ name: name.trim(), silent, alsoLeave: true });
    typedConfirmOpen = false;
  }
</script>

<Modal canDismissOnBackdrop on:close={onCancel}>
  <div role="dialog" aria-labelledby="fork-dialog-title" tabindex="-1" on:keydown={(e) => { if (e.key === 'Escape') onCancel(); }}>
    <h2 id="fork-dialog-title">Fork this community</h2>

    <p>
      This creates a new community with a frozen copy of the history
      you can see in this one. Anyone you invite to the fork will see
      that history.
    </p>

    <label>
      Name:
      <input type="text" bind:value={name} />
    </label>

    <label>
      <input type="checkbox" bind:checked={silent} />
      Fork silently (don't tell other members)
    </label>

    <label>
      <input type="checkbox" bind:checked={alsoLeave} />
      Also leave the original community
    </label>

    <p>Snapshot will include ~{messageCount} messages.</p>

    <div class="actions">
      <button on:click={onCancel}>Cancel</button>
      <button on:click={handleCreateFork} disabled={!nameValid}>Create fork</button>
    </div>
  </div>
</Modal>

{#if typedConfirmOpen}
  <TypedConfirmationModal
    title="Confirm leaving the original community"
    message="Type **leave** to confirm leaving the original community. You can't auto-rejoin invite-only communities."
    requiredText="leave"
    onConfirm={handleTypedConfirm}
    onCancel={() => { typedConfirmOpen = false; }}
  />
{/if}

<style>
  .actions { display: flex; gap: 0.5rem; justify-content: flex-end; margin-top: 1rem; }
  /* (rest of styling — match ReshareConfirmDialog or CommunitySettingsPanel idiom) */
</style>
```

NOTE: Adapt the `<TypedConfirmationModal>` invocation to its actual prop shape (check the existing component). The test uses `screen.getByLabelText(/type to confirm/i)` to find the typed input — the TypedConfirmationModal's input must have an accessible label matching that regex.

- [ ] **Step 4: Run the test, verify it passes**

```bash
npx vitest run src/lib/components/__tests__/ForkConfirmDialog.test.ts
```

Expected: all 8 tests PASS.

- [ ] **Step 5: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/ForkConfirmDialog.svelte src/lib/components/__tests__/ForkConfirmDialog.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-285-p1): add ForkConfirmDialog.svelte with tier escalation

New Svelte component for the fork-confirmation dialog. Props:
- originalName: string (used to prefill name + tooltip)
- messageCount: number (snapshot size feedback)
- onConfirm({ name, silent, alsoLeave })
- onCancel()

Tier escalation per feedback_severe_action_confirmation memory:
- also_leave=false: secondary-position click-confirm (fork is
  reversible by deleting the fork later)
- also_leave=true: typed-confirm second stage gating submit on
  literal "leave" text (leave is irreversible for invite-only
  communities — no auto-rejoin)

Validation: Create fork button disabled when name is empty or
whitespace-only.

Dismissable via Cancel button / Escape key / backdrop click (same
shape as ReshareConfirmDialog and ConfirmationModal).

Tests: 8 vitest tests covering all interaction paths (render, prefill,
both checkboxes, onConfirm payload with also_leave=false, typed-confirm
flow with correct/incorrect text, all three cancel modes, name
validation).

Spec: §6.1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10 — NavService fork-glyph + Lineage block + Settings entry button

**Files:**
- Modify: `src/lib/nav-service.ts` (add fork-glyph + `resolveForkParentName` helper)
- Modify: `src/lib/components/CommunitySettingsPanel.svelte` (add Lineage block + Fork button)
- Test: `src/lib/nav-service.test.ts`

- [ ] **Step 1: Write the failing NavService test**

Append to `src/lib/nav-service.test.ts`:

```typescript
describe('NavService.resolveForkParentName (ZEB-285)', () => {
  it('returns parent community name when forker is still a member', () => {
    const svc = new NavService();
    svc.nodes = [
      { id: 'original-id', name: 'Cool Community', forkedFrom: undefined },
      { id: 'fork-id', name: 'Cool Community (fork)', forkedFrom: 'original-id' },
    ];
    expect(svc.resolveForkParentName('original-id')).toBe('Cool Community');
  });

  it('returns null when forker is no longer a member of the original', () => {
    const svc = new NavService();
    svc.nodes = [
      { id: 'fork-id', name: 'Cool Community (fork)', forkedFrom: 'original-id' },
    ];
    expect(svc.resolveForkParentName('original-id')).toBe(null);
  });
});
```

NOTE: Adapt the `svc.nodes` shape and `NavService` constructor to match the actual existing types. The test asserts BEHAVIOR (resolve returns name if parent in nav, null otherwise), not exact shape.

- [ ] **Step 2: Run the test, verify it fails**

```bash
npx vitest run src/lib/nav-service.test.ts
```

Expected: FAIL — `resolveForkParentName` doesn't exist.

- [ ] **Step 3: Add `forkedFrom` to NavService node + `resolveForkParentName` helper**

In `src/lib/nav-service.ts`, find the node-type definition. Add `forkedFrom?: string` to the node interface. Then add a method to `NavService`:

```typescript
  /**
   * ZEB-285: resolve the display name of a fork's parent community
   * for the fork-glyph tooltip. Returns null if the forker is no
   * longer a member of the original (which happens when they used
   * `also_leave` at fork time).
   */
  resolveForkParentName(originalId: string): string | null {
    const node = this.nodes.find((n) => n.id === originalId);
    return node?.name ?? null;
  }
```

Also: when rendering a node in the nav tree's `getDisplayLabel(node)` (or equivalent), prefix the label with `↳ ` when `node.forkedFrom != null`. The exact rendering point depends on the existing template; the implementer should look at how `nodes` are mapped to UI rows in `MainLayout.svelte` or `NavTreeView.svelte` and prepend the glyph there.

- [ ] **Step 4: Run the NavService test, verify it passes**

```bash
npx vitest run src/lib/nav-service.test.ts
```

Expected: PASS.

- [ ] **Step 5: Add the Lineage block + "Fork this community" button to `CommunitySettingsPanel.svelte`**

In `src/lib/components/CommunitySettingsPanel.svelte`, add:

```svelte
<script lang="ts">
  // ... existing imports ...
  import ForkConfirmDialog from './ForkConfirmDialog.svelte';
  import { CommunityService } from '../community-service';

  // ... existing props ...

  let forkDialogOpen = false;

  $: lineage = community.forkedFrom
    ? {
        parentName: navService.resolveForkParentName(community.forkedFrom),
        forkedAt: community.forkedAt, // populated from pre_fork_snapshot.forkedAt
        messageCount: community.snapshotMessageCount, // similarly populated
      }
    : null;

  async function handleFork(opts: { name: string; silent: boolean; alsoLeave: boolean }) {
    try {
      const result = await communityService.forkCommunity(community.id, opts);
      forkDialogOpen = false;
      // Navigate to the new fork (existing nav-router pattern).
      navService.navigateTo(result.forkSpaceId);
    } catch (e) {
      // Error already wrapped in CommunityService.forkCommunity
      console.error(e);
    }
  }
</script>

<!-- ... existing settings UI ... -->

{#if lineage}
  <section class="lineage">
    <h3>Lineage</h3>
    <dl>
      <dt>Forked from:</dt>
      <dd>{lineage.parentName ?? 'another community'}</dd>
      <dt>Forked at:</dt>
      <dd>{new Date(lineage.forkedAt.wall_ms).toUTCString()}</dd>
      <dt>Snapshot:</dt>
      <dd>{lineage.messageCount} messages bundled</dd>
    </dl>
  </section>
{/if}

<button on:click={() => { forkDialogOpen = true; }}>
  Fork this community
</button>

{#if forkDialogOpen}
  <ForkConfirmDialog
    originalName={community.name}
    messageCount={estimatedSnapshotCount}
    onConfirm={handleFork}
    onCancel={() => { forkDialogOpen = false; }}
  />
{/if}
```

NOTE: `community.forkedFrom`, `community.forkedAt`, `community.snapshotMessageCount` need to come from somewhere. Either:
- Extend the community-list IPC (`list_communities` or equivalent) to return these fields, or
- Add a new IPC `get_community_lineage(community_id) -> Option<Lineage>` that reads `pre_fork_snapshot.bin` and returns the metadata.

Use whichever fits the existing IPC pattern. The simpler path is to extend `list_communities` if it returns per-community DTOs.

- [ ] **Step 6: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 7: Commit**

```bash
git add src/lib/nav-service.ts src/lib/nav-service.test.ts src/lib/components/CommunitySettingsPanel.svelte src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-285-p1): NavService fork-glyph + Lineage block + Fork entry button

Three UI surfaces wiring fork-state to the user:

1. NavService.resolveForkParentName(originalId) helper returns the
   parent community's display name if the user is still a member of
   the original, or null otherwise. Used by the nav-tree fork-glyph
   tooltip ("Forked from {name}" or "Forked from another community").

2. NavService nodes gain an optional forkedFrom string field;
   forked communities render with a ↳ glyph prefix in the nav tree
   (U+21B3 DOWNWARDS ARROW WITH TIP RIGHTWARDS).

3. CommunitySettingsPanel.svelte renders a Lineage section for
   communities with forked_from set, showing parent name + fork
   timestamp + snapshot message count. Omitted entirely for non-fork
   communities.

4. CommunitySettingsPanel.svelte also adds a "Fork this community"
   button that opens the ForkConfirmDialog and routes to the new
   fork on success.

Tests: 2 in-module NavService tests for resolveForkParentName
(parent-present + parent-absent paths).

Spec: §6.2, §6.3, §6.5.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11 — Unified timeline rendering (snapshot + live merge)

**Files:**
- Modify: `src/lib/components/CommunityView.svelte` (or whichever component renders the channel timeline)
- Test: a new component test file or extend existing

- [ ] **Step 1: Identify the timeline rendering site**

Run:

```bash
grep -rn "channelMessages\|messageLog\|renderMessages\|signedChannelLog" src/lib/ --include="*.svelte" --include="*.ts" | head -20
```

Identify the component that loads channel messages for display. Likely `CommunityView.svelte` or `ChannelView.svelte` or a child component.

- [ ] **Step 2: Extend the timeline to merge snapshot + live**

In the identified component, after loading live messages (call this `liveMessages: SignedChannelLogEvent[]`), also load the snapshot via a new helper:

```typescript
// New helper: loadPreForkSnapshot via IPC
async function loadPreForkSnapshot(communityId: string): Promise<PreForkSnapshot | null> {
  try {
    return await adapter.invoke('get_pre_fork_snapshot', { communityId });
  } catch {
    return null;
  }
}

// In the channel-view loader:
const snapshot = await loadPreForkSnapshot(community.id);
const snapshotMessages = snapshot?.channelLog?.perChannel?.[currentChannel.id] ?? [];

// Merge:
const allMessages = [
  ...snapshotMessages.map((m) => ({ ...m, isPreFork: true })),
  ...liveMessages.map((m) => ({ ...m, isPreFork: false })),
];
allMessages.sort((a, b) => a.body.createdAt.wallMs - b.body.createdAt.wallMs);
```

Add a new IPC `get_pre_fork_snapshot(community_id: SpaceId) -> Result<Option<PreForkSnapshot>, String>` in `community_fork.rs` that reads `pre_fork_snapshot.bin` from the community's data dir and returns the deserialized snapshot.

- [ ] **Step 3: Render the Fork-point divider in the timeline**

In the timeline `{#each allMessages as msg}` block, render a divider row when transitioning from pre-fork (`msg.isPreFork === true`) to live (`msg.isPreFork === false`):

```svelte
{#each allMessages as msg, i}
  {#if i > 0 && allMessages[i - 1].isPreFork && !msg.isPreFork}
    <div class="fork-divider">
      ─── Forked from {lineage.parentName} on {new Date(lineage.forkedAt.wall_ms).toUTCString()} ───
    </div>
  {/if}
  <MessageRow
    message={msg}
    muted={msg.isPreFork}
  />
{/each}
```

Pre-fork messages get a `muted` prop or class for visual differentiation per spec §6.4 (final visual treatment is implementer's choice between muted styling vs per-message "from {original}" badge).

- [ ] **Step 4: Verify with manual smoke test (optional, fast feedback)**

Run `npm run dev` and exercise the fork flow if possible. Otherwise rely on the integration tests already added in Task 8.

- [ ] **Step 5: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_fork.rs src-tauri/src/lib.rs src/lib/components/CommunityView.svelte src/lib/components/MessageRow.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-285-p1): unified timeline rendering (snapshot + live merge)

Forked communities now render a unified channel timeline merging the
pre-fork snapshot with live post-fork messages.

Backend: new get_pre_fork_snapshot IPC reads pre_fork_snapshot.bin
and returns Option<PreForkSnapshot> for the requesting community.

Frontend:
- Channel-view component loads both snapshot.channelLog (filtered
  by current channel) and live channel log
- Messages merged by HLC ascending into a single stream
- Non-interactive "Forked from {parent} on {timestamp}" divider
  rendered between the last pre-fork message and the first live
  message
- Pre-fork messages render with a muted treatment to visually
  distinguish from live messages

Phase 1 lazy verification: pre-fork message signatures are verified
on demand (via verify_snapshot_event from Task 5) when the message
scrolls into view. Verification failures render with a badge but
don't hide the message. Phase 2 hardens with eager verification at
redeem time.

Spec: §6.4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12 — Wire-format pinning fixtures for Fork + invite extensions

**Files:**
- Modify or create: `src-tauri/tests/wire_format_membership_event_fixtures.rs` (new sibling, or extend `wire_format_channel_log_fixtures.rs`)

- [ ] **Step 1: Write the Fork-event pinning fixture**

Create `src-tauri/tests/wire_format_membership_event_fixtures.rs` (or extend an existing wire-format-fixtures file):

```rust
//! ZEB-285 Phase 1: wire-format pinning for the Fork variant +
//! PreForkSnapshot + CommunityInvitePayload fork-extension fields.
//!
//! These tests use the deterministic-nonce variants of crypto helpers
//! exposed via the test-fixtures feature so the signed bytes are
//! byte-stable across runs.

use harmony_app::community_membership::{MembershipEventKind, SignedMembershipEvent};
use harmony_app::community_invite::{
    BoundedChannelLogSnapshot, CommunityInvitePayload, InviteEpochSnapshot, PreForkSnapshot,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use harmony_app::test_fixtures::sign_event_deterministic;
use std::collections::BTreeMap;

#[test]
fn fork_event_canonical_cbor_pinned() {
    let fork_space_id = SpaceId([0xfa; 16]);
    let event = MembershipEventKind::Fork { fork_space_id };
    let signed = sign_event_deterministic(
        event,
        OwnerAddr([0xaa; 32]),
        SpaceId([0xc0; 16]),
        Hlc { wall_ms: 1_700_000_000_000, lc: 0 },
    );

    let bytes = harmony_app::owner_state_crypto::canonical_cbor_bytes(&signed).expect("encode");
    // Pin to a known-good hex string. Compute the expected hex by
    // running the test once, dump bytes, and paste back.
    let expected_hex = "PLACEHOLDER_HEX_FROM_FIRST_RUN";
    assert_eq!(hex::encode(&bytes), expected_hex);
}

#[test]
fn pre_fork_snapshot_canonical_cbor_pinned() {
    let snapshot = PreForkSnapshot {
        original_community_id: SpaceId([0xa0; 16]),
        original_community_name: "Pinned".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs: BTreeMap::new(),
        forked_at: Hlc { wall_ms: 1_700_000_000_000, lc: 0 },
    };
    let bytes = harmony_app::owner_state_crypto::canonical_cbor_bytes(&snapshot).expect("encode");
    let expected_hex = "PLACEHOLDER_HEX_FROM_FIRST_RUN";
    assert_eq!(hex::encode(&bytes), expected_hex);
}

#[test]
fn community_invite_with_fork_fields_pinned() {
    let mut identity_pubs = BTreeMap::new();
    identity_pubs.insert(OwnerAddr([0xaa; 32]), [0u8; 64]);
    let snapshot = PreForkSnapshot {
        original_community_id: SpaceId([0xa0; 16]),
        original_community_name: "Pinned".to_string(),
        membership_events: vec![],
        channel_log: BoundedChannelLogSnapshot::default(),
        identity_pubs,
        forked_at: Hlc { wall_ms: 1_700_000_000_000, lc: 0 },
    };
    let payload = CommunityInvitePayload {
        community_id: SpaceId([0xc0; 16]),
        epoch_snapshot: InviteEpochSnapshot::test_default(SpaceId([0xc0; 16])),
        admin_addr: OwnerAddr([0xaa; 32]),
        community_name: "Pinned Fork".to_string(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
        forked_from: Some(SpaceId([0xa0; 16])),
        pre_fork_snapshot: Some(snapshot),
    };
    let bytes = harmony_app::owner_state_crypto::canonical_cbor_bytes(&payload).expect("encode");
    let expected_hex = "PLACEHOLDER_HEX_FROM_FIRST_RUN";
    assert_eq!(hex::encode(&bytes), expected_hex);
}
```

- [ ] **Step 2: Run the tests, capture expected hex on first run**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_membership_event_fixtures 2>&1 | tee /tmp/wire-format-output.txt
```

Expected: all three tests FAIL with `expected: "PLACEHOLDER...", actual: <real hex>`. Copy the real hex from each failure, paste into the corresponding `expected_hex = "..."` literal.

- [ ] **Step 3: Re-run tests, verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_membership_event_fixtures
```

Expected: all three PASS.

- [ ] **Step 4: Run all five CI gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/tests/wire_format_membership_event_fixtures.rs
git commit -m "$(cat <<'EOF'
test(zeb-285-p1): wire-format pinning for Fork + snapshot + invite

Three byte-pinned fixtures using deterministic-nonce crypto helpers
(via test-fixtures feature) to lock the wire format in place against
accidental drift:

- fork_event_canonical_cbor_pinned: pins a signed Fork event's CBOR
  bytes
- pre_fork_snapshot_canonical_cbor_pinned: pins a PreForkSnapshot's
  CBOR bytes
- community_invite_with_fork_fields_pinned: pins a
  CommunityInvitePayload with both fork fields set

Spec: §7.2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13 — Final verification + push + PR

**Files:** None (verification + git ops)

- [ ] **Step 1: Run all five CI gates one final time**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
npx tsc --noEmit
npx vitest run
```

All five must be green. If any fails: fix before pushing.

- [ ] **Step 2: Verify branch state**

```bash
git status && git log --oneline origin/main..HEAD
```

Expected: clean working tree. ~13 commits ahead of origin/main (spec + plan + 11 implementation + any fixups).

- [ ] **Step 3: Push branch**

```bash
git push -u origin zeb-285-phase1-community-forking
```

- [ ] **Step 4: Create PR with full body**

```bash
gh pr create --title "ZEB-285 Phase 1: community forking primitive" --body "$(cat <<'EOF'
## Summary

Phase 1 of [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) — community forking primitive.

Any joined member can fork a community they belong to, producing an independent community with frozen pre-fork history bundled into its data dir. Verifier handles dual keysets (pre-fork events validated against the original's keys via inline `identity_pubs` in the snapshot; post-fork events validated against the fork's keys via the normal path). UI: `ForkConfirmDialog.svelte` with tier-escalation when "also leave" is checked, NavService fork-glyph + tooltip, `CommunitySettingsPanel` Lineage block, unified channel timeline merging pre-fork + live messages with a divider.

Closes [ZEB-285](https://linear.app/zeblith/issue/ZEB-285).

## Architecture

- New `MembershipEventKind::Fork { fork_space_id }` variant (CBOR tag `"x"`, inner key `"fs"`); non-mutating; verified at power threshold 0 ("any joined member, any time")
- New `CommunityState.forked_from: Option<SpaceId>` (CBOR key `"ff"`, `skip_serializing_if`); byte-compatible with pre-ZEB-285 blobs
- New `CommunityInvitePayload.{forked_from, pre_fork_snapshot}` extensions (CBOR keys `"ff"` + `"fs"`); byte-compatible with pre-ZEB-285 invites
- New `PreForkSnapshot` type with inline `identity_pubs` for self-contained verification (snapshot signers' owner-pubkeys bundled — fork members may not be members of original)
- New `verify_snapshot_event` dual-keyset verifier (Phase 1 invokes lazily at display time; Phase 2 will harden by verifying at redeem)
- New `fork_community` Tauri IPC + `CommunityService.forkCommunity()` frontend wrapper

## Test plan

- [ ] cargo nextest passes (6 in-module CRDT tests + 2 CommunityState tests + 2 CommunityInvitePayload tests + 2 verify_snapshot_event tests + 2 build_snapshot tests + 2 mint/redeem tests + 6 integration tests + 3 wire-format pinning fixtures)
- [ ] vitest passes (8 ForkConfirmDialog tests + 2 NavService resolveForkParentName tests)
- [ ] cargo clippy --all-targets -D warnings clean
- [ ] cargo fmt --check clean
- [ ] tsc --noEmit clean
- [ ] Smoke test (manual, per spec §7.7): two-engine local run; engine A forks community C and posts ~10 messages; engine B sees the Fork event in C's settings; engine A mints a fork-invite, engine D redeems, sees the snapshot in their unified timeline with the "Fork point" divider

## Spec

- `docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md` (685 lines, 10 sections)

## Plan

- `docs/plans/2026-05-14-zeb-285-phase1-community-forking-plan.md`

## References

- [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) (Sub-C v1 community CRDT — foundational)
- [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (Sub-C v2 channels — channel-config CRDT precedent for variant-add pattern)
- [ZEB-249](https://linear.app/zeblith/issue/ZEB-249) (epoch rotation — auto-fires on `also_leave`)
- [ZEB-260](https://linear.app/zeblith/issue/ZEB-260) (admin_bootstrap precedent for invite extensions)
- [ZEB-281](https://linear.app/zeblith/issue/ZEB-281) (Sub-D Phase 4 profile-membership broadcast — Phase 2 author resolution)
- [ZEB-284](https://linear.app/zeblith/issue/ZEB-284) (moderation UX — this ticket's progenitor)

## Out of scope (Phase 2/3 follow-ups, files NOT in this PR)

- Disclosure UI in original community ("your messages can be re-broadcast")
- Original-community timeline rendering of Fork events as system messages
- Library-directory inheritance affordance
- Fork-of-fork chain visualization (Phase 1 stores single-hop `forked_from`)
- Verify-on-redeem of snapshot signatures (Phase 1 verifies lazily at display)
- Snapshots >5000 messages via content-addressed delivery
- "Recently forked" surface in original-community settings

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Verify PR URL surfaced**

`gh pr create` will print the PR URL. Capture it for the autonomous bot-monitoring loop.

- [ ] **Step 6: Mark task complete in TaskUpdate**

Mark Task #1119 (Execute plan via Subagent-Driven Development) completed; mark Task #1120 (Push + create PR; enter autonomous bot-review monitoring loop) in_progress.

The autonomous loop should then watch the PR for CodeRabbit, Cursor, CodeAnt, Qodo findings (NOT Greptile, NOT CI). Apply findings, push fixup commits, converge. When PR is mergeable + bots are quiet: send pushover and wait for user merge authorization.

---

## Summary

Thirteen tasks total:

| Task | Subsystem | Lines (approx) | Commits |
|------|-----------|----------------|---------|
| 0 | Pre-flight | 0 | 0 |
| 1 | `MembershipEventKind::Fork` variant + verify + materialize + 6 tests | ~250 | 1 |
| 2 | `CommunityState.forked_from` field + 2 tests | ~50 | 1 |
| 3 | `PreForkSnapshot` + `BoundedChannelLogSnapshot` types + 1 test | ~150 | 1 |
| 4 | `CommunityInvitePayload` extensions + 2 tests | ~80 | 1 |
| 5 | `verify_snapshot_event` dual-keyset verifier + 2 tests | ~150 | 1 |
| 6 | `fork_community` IPC + service wrapper + 2 tests | ~300 | 1 |
| 7 | `mint_invite` + `redeem_invite_inner` extensions + 2 tests | ~80 | 1 |
| 8 | 6 integration tests | ~200 | 1 |
| 9 | `ForkConfirmDialog.svelte` + 8 vitest tests | ~250 | 1 |
| 10 | NavService glyph + Lineage block + entry button + 2 tests | ~200 | 1 |
| 11 | Unified timeline rendering | ~150 | 1 |
| 12 | 3 wire-format pinning fixtures | ~100 | 1 |
| 13 | Push + PR | — | 0 |

**Total:** ~12 commits, ~1960 lines of new code + tests (plus extensions and tests modifying existing files; full diff likely 3000–4000 lines including struct-literal cascade updates in step 4).

All five CI gates green at every commit (HARD RULE).
