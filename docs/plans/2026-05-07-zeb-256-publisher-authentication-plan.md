# ZEB-256 Publisher Authentication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add cryptographic publisher authentication to community state-root publishes so a malicious member with the `MembershipKey` cannot spoof another member's HLC slot or censor admin publishes — closing the gap before Phase 4 invite-only flows ship.

**Architecture:** Mirror the per-event authentication shape — every `CommunityRootPublishPayload` carries a `publisher_addr` + Ed25519 `publisher_sig` over a separate `CommunityRootSignedPayload` sub-payload. The receiver's `handle_incoming_publish` adds three new gates BEFORE the existing replay-tracker check (membership-at-HLC, identity_pub resolution, signature verification), and the tracker re-keys from `BTreeMap<String, Hlc>` to `BTreeMap<(OwnerAddr, String), Hlc>` so cross-addr device_id collisions become impossible.

**Tech Stack:** Rust, Tokio, Ed25519 (`ed25519_dalek`), canonical CBOR, ChaCha20-Poly1305 AEAD (unchanged), Tauri 2.

**Spec:** `docs/specs/2026-05-07-zeb-256-publisher-authentication-design.md` (commit `fdf7ba0`).

**Branch:** `zeb-256-publisher-authentication` (created in Task 0 off the latest `origin/main`).

**Out of scope:** ZEB-249 (`MembershipKey` rotation on kick) is the read-side fix and is independently tracked. Two-mode AEAD nonce discipline is unchanged. Multi-signature publishes are deferred indefinitely.

---

## File structure

The change is heavily concentrated in `community_state_sync.rs`. Persistence self-heals on the new tracker shape. The `lib.rs` integration is small — only the `CommunityRegistryConfig` construction site needs new fields.

**Modified:**

- `src-tauri/src/community_state_sync.rs` — adds `CommunityRootSignedPayload`, extends `CommunityRootPublishPayload`, re-shapes `CommunityRootHlcTracker.per_device`, adds 3 new `CommunitySyncError` variants, adds `self_owner` + `signing_key` to `CommunitySyncEngineConfig` + `InternalCtx` + `CommunityRegistryConfig`, signs in `publish_root_now`, verifies in `handle_incoming_publish` with three new gates ahead of the replay check.
- `src-tauri/src/lib.rs` — populates the two new `CommunityRegistryConfig` fields from the existing `signing_key_arc` + `self_owner` already snapshotted at engine-spawn time.
- `src-tauri/tests/wire_format_community_sync_fixtures.rs` — regenerated for the 4-field envelope.
- `src-tauri/tests/community_root_hlc_tracker_unit.rs` — updated for the new `(OwnerAddr, String)` key shape; adds cross-addr-isolation test.
- `src-tauri/tests/community_sync_engine_unit.rs` — updated for the new config fields; adds `publish_carries_valid_publisher_sig`, three rejection-variant tests, and the cold-cache transient-rejection test.
- `src-tauri/tests/community_sync_integration.rs` — extended with `spoofed_publish_does_not_block_real_publisher` and `re_joined_member_publish_admitted_after_leave`.
- `src-tauri/tests/community_sync_registry_unit.rs` — updated for the new `CommunityRegistryConfig` fields.
- `src-tauri/tests/community_state_persist_unit.rs` — updated for the new tracker key shape; adds a "old shape quarantines + self-heals to default" test.
- `src-tauri/tests/community_open_flow_integration.rs` — minimal config-shape update (no behaviour change — Phase 3 IPC tests still pass).

**Unchanged but referenced:**

- `src-tauri/src/community_state_persist.rs` — `load_replay`'s existing `quarantine_corrupted` self-heal already handles the breaking shape change. A test pins this behaviour.
- `src-tauri/src/community_membership.rs` — `prior_state_at_event` + `MaterializedMembership` + `MemberStatus` are reused as-is for the membership-at-HLC gate.
- `src-tauri/src/owner_state_types.rs` — `OwnerAddr` already serialises as bstr(16) via `serialize_bytes_as_bstr`.
- `src-tauri/src/event_loop.rs` — `spawn_community_state_zenoh_adapter` is unchanged (adapter is wire-byte-agnostic).

**Why this decomposition:**

- Wire-format types ship before sign/verify so the codec is locked in and tests can roundtrip pinned bytes immediately.
- Tracker key shape lands second so all later sites use the new signature once.
- Error variants are added before the verify flow consumes them so `handle_incoming_publish` can match against final variants on its first edit.
- Config fields land before `publish_root_now` / `handle_incoming_publish` so each test site updates its config exactly once.
- Signing precedes verify because the verify-flow tests need engine-produced sigs.
- Registry plumbing comes after the engine surface stabilises so registry tests can rely on the engine's final config shape.
- `start_node` integration is the last code change before integration tests; it can't run without the registry config update from the previous task.


---

## Task 0: Pre-flight — file Linear ticket, branch off latest `origin/main`

**Why:** Per the user-memory rules, Linear IDs are assigned by Linear (never invented), branches must rebase on latest `origin/main`, and worktrees are forbidden. ZEB-256 itself is the work ticket — no per-phase sub-issue is needed because this is a single-PR effort. This task confirms the ticket exists, syncs main, and creates the implementation branch off the spec branch's base (which is `origin/main` as of `fdf7ba0`).

**Files:** None modified — git operations only.

- [ ] **Step 1: Confirm Linear ticket ZEB-256 exists**

ZEB-256 was filed during the spec-deferral PR (commit `0b84296`) and is already in Linear. Verify with:

```bash
gh issue list --repo zeblithic/harmony-client 2>/dev/null || true
# Or via Linear MCP: list_issues with team_key="ZEB", state="In Progress"
```

If the ticket is missing for any reason, file it via the Linear MCP `save_issue` tool with title "ZEB-256: cryptographic publisher authentication for community state-root publishes" before proceeding. Do NOT invent an alternate ID.

- [ ] **Step 2: Switch to main and pull**

```bash
git checkout main
git pull origin main
```

Expected: `main` updates to commit `bc0facd` (Phase 3 ship) or newer.

- [ ] **Step 3: Create implementation branch**

```bash
git checkout -b zeb-256-publisher-authentication
```

Expected: `On branch zeb-256-publisher-authentication`. NO worktree creation — per the user-memory `feedback_no_worktrees.md` HARD RULE.

- [ ] **Step 4: Confirm baseline tests pass on main**

```bash
set -o pipefail
cd src-tauri && cargo test --workspace --all-targets --locked 2>&1 | tail -20
```

Expected: all tests pass. If tests fail on a clean main, that's "test drift is our fault" — fix the drift in a separate cleanup commit on this branch BEFORE starting the ZEB-256 work, or file a tracking issue and document the failure here. Do NOT proceed with implementation tasks until baseline is green.

- [ ] **Step 5: No commit yet**

Task 0 is a pre-flight only. The first commit lands in Task 1.

---

## Task 1: Add `CommunityRootSignedPayload` + extend `CommunityRootPublishPayload`

**Why:** The wire format changes shape: every publish gains a 16-byte `publisher_addr` and a 64-byte Ed25519 `publisher_sig`. Mirror the `EventPayload` / `SignedMembershipEvent` split — the *signed* sub-payload is its own type so the canonical CBOR bytes are unambiguous (no place to encode "the signature went here" in the signed form). Locking the new wire bytes in a fixture on the same commit means later refactors can't drift the encoding silently.

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:198-220` (around the existing `CommunityRootPublishPayload` definition)
- Modify: `src-tauri/tests/wire_format_community_sync_fixtures.rs` (regenerate pinned bytes)

- [ ] **Step 1: Write the failing fixture test for the new wire shape**

Replace the entire body of `src-tauri/tests/wire_format_community_sync_fixtures.rs` with:

```rust
//! Pinned-byte CBOR wire-format fixtures for community-sync types.
//! ZEB-256: envelope gained `publisher_addr` (bstr(16)) + `publisher_sig`
//! (bstr(64)). Old pinned bytes are wholly invalidated; this regen
//! commit IS the deliberate update. Mirrors community-membership wire
//! fixtures — locking the encoded bytes prevents silent wire-form drift
//! across phases.

use harmony_app::community_state_sync::{
    CommunityRootPublishPayload, CommunityRootSignedPayload,
};
use harmony_app::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
use harmony_app::owner_state_types::{Hlc, OwnerAddr};
use harmony_content::cid::ContentId;

#[test]
fn community_root_signed_payload_wire_bytes_pinned() {
    // 3-key map: rc (root_cid), pa (publisher_addr), at (Hlc).
    // All keys are 2 chars to satisfy the same-length-keys invariant.
    let cid = ContentId::from_bytes([0xAA; 32]);
    let p = CommunityRootSignedPayload {
        root_cid: cid,
        publisher_addr: OwnerAddr([0xBB; 16]),
        at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 7,
            device_id: "d1".into(),
        },
    };

    let bytes = canonical_cbor_encode(&p).expect("encode");
    // Lock the byte sequence — any structural change requires this
    // fixture to update intentionally. Generation procedure: produce
    // bytes with `cargo test community_root_signed_payload_wire_bytes`
    // expecting the assert to fail, then paste the LHS of the diff
    // here. Paranoia check: every key code is 2 chars (rc, pa, at).
    let expected = hex::decode(
        "a36272635820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa627061501a1c627061626162626162626162626162626174a361771b0000018bcfe56800616c076164626431"
            .replace(" ", "")
            .as_str(),
    );
    // The exact bytes above are PLACEHOLDER — see Step 3 below.
    let _ = expected;
    let _ = bytes;
    panic!("Step 1: fixture pinned bytes not yet generated; rerun in Step 3");
}

#[test]
fn community_root_publish_payload_wire_bytes_pinned() {
    // 4-key map: rc, pa, at, ps (publisher_sig).
    let cid = ContentId::from_bytes([0xAA; 32]);
    let p = CommunityRootPublishPayload {
        root_cid: cid,
        publisher_addr: OwnerAddr([0xBB; 16]),
        at: Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 7,
            device_id: "d1".into(),
        },
        publisher_sig: [0xCC; 64],
    };

    let bytes = canonical_cbor_encode(&p).expect("encode");
    let _ = bytes;
    panic!("Step 1: fixture pinned bytes not yet generated; rerun in Step 3");
}
```

Note: the `expected` constants are intentional placeholders; Step 3 regenerates them after the type definitions land. Step 2 confirms the test fails to compile because `CommunityRootSignedPayload` doesn't exist yet.

- [ ] **Step 2: Run the test to verify it fails to compile**

```bash
set -o pipefail
cd src-tauri && cargo test --test wire_format_community_sync_fixtures 2>&1 | tail -20
```

Expected: compilation error — `cannot find type CommunityRootSignedPayload`. Confirms the test exercises the missing type.

- [ ] **Step 3: Implement `CommunityRootSignedPayload` + extend `CommunityRootPublishPayload`**

Replace the existing `CommunityRootPublishPayload` block in `src-tauri/src/community_state_sync.rs` (around lines 198-220) with:

```rust
/// State-root publish wire envelope. ZEB-256: every publish is signed
/// by the publisher's local Ed25519 device key. Receivers verify the
/// signature, the publisher's current membership status, and the
/// per-(addr, device) replay tracker before merging events.
///
/// Wire format: 4-key CBOR map. All field codes are 2 chars
/// (`rc`/`pa`/`at`/`ps`) to satisfy the same-length-keys invariant
/// at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRootPublishPayload {
    /// Content-ID of the encrypted CommunityState blob in the shared
    /// ContentStore. Unchanged from Phase 2.
    #[serde(rename = "rc")]
    pub root_cid: ContentId,

    /// Owner address of the publishing member. Receivers use this to
    /// (a) resolve identity_pub via IdentityResolver, (b) check
    /// membership-at-publish-HLC, (c) namespace the replay tracker.
    #[serde(rename = "pa")]
    pub publisher_addr: OwnerAddr,

    /// Publisher's HLC at publish time. Carries device_id; tracker
    /// slot key is `(publisher_addr, at.device_id)`. Unchanged shape
    /// from Phase 2 — only the tracker's interpretation changed.
    #[serde(rename = "at")]
    pub at: Hlc,

    /// Ed25519 signature over canonical CBOR of
    /// `CommunityRootSignedPayload { root_cid, publisher_addr, at }`.
    #[serde(
        rename = "ps",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub publisher_sig: [u8; 64],
}

impl CanonicalPayloadSealed for CommunityRootPublishPayload {}
impl CanonicalPayload for CommunityRootPublishPayload {}

/// The unsigned portion of a `CommunityRootPublishPayload` — the
/// canonical-CBOR bytes the publisher signs. Mirrors `EventPayload` vs
/// `SignedMembershipEvent`: keeping the signed sub-payload as its own
/// type means the signed bytes are unambiguous (no place to put "the
/// actual sig went here" in the encoded form).
///
/// All 3 field keys are 2 chars to satisfy the same-length-keys
/// invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityRootSignedPayload {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    #[serde(rename = "pa")]
    pub publisher_addr: OwnerAddr,
    #[serde(rename = "at")]
    pub at: Hlc,
}

impl CanonicalPayloadSealed for CommunityRootSignedPayload {}
impl CanonicalPayload for CommunityRootSignedPayload {}

impl CommunityRootSignedPayload {
    /// Convert a signed sub-payload into its full wire envelope by
    /// attaching the Ed25519 signature.
    pub fn into_wire(self, publisher_sig: [u8; 64]) -> CommunityRootPublishPayload {
        CommunityRootPublishPayload {
            root_cid: self.root_cid,
            publisher_addr: self.publisher_addr,
            at: self.at,
            publisher_sig,
        }
    }
}

/// Convenience: extract the signed sub-payload from a full wire
/// envelope. Used by receive-side verify to reproduce the canonical
/// CBOR bytes the publisher signed.
impl From<&CommunityRootPublishPayload> for CommunityRootSignedPayload {
    fn from(w: &CommunityRootPublishPayload) -> Self {
        Self {
            root_cid: w.root_cid,
            publisher_addr: w.publisher_addr,
            at: w.at.clone(),
        }
    }
}
```

Also: in `src-tauri/src/owner_state_types.rs`, the `serialize_bytes_as_bstr` and `deserialize_bytes_from_bstr` helpers are currently `pub(crate)`. They MUST be made `pub` (drop the `(crate)`) so the `#[serde(serialize_with = "crate::owner_state_types::...")]` path resolves from outside the module — actually no: `crate::owner_state_types::...` refers within the same crate, so `pub(crate)` is fine. Confirm with rustc on the next step. If a `private function leaked` lint trips, escalate to making them `pub`.

- [ ] **Step 4: Generate the pinned wire bytes**

The fixture test panics in Step 1 with a placeholder `expected`. Replace the panic with the actual encoded bytes:

```bash
set -o pipefail
cd src-tauri && cargo test --test wire_format_community_sync_fixtures community_root_signed_payload_wire_bytes_pinned 2>&1 | tail -30
```

You'll see a panic. Now write a tiny harness that prints the bytes (insert temporarily, then revert):

```rust
// Inside the test body, before the `let _ = bytes;` line:
println!("encoded={}", hex::encode(&bytes));
panic!();
```

Re-run the test, capture the printed hex, then:
1. Replace the placeholder `expected = hex::decode(...)` with the captured hex.
2. Replace the trailing `panic!("Step 1: ...")` with proper assertions:

```rust
let expected = hex::decode("REPLACE_WITH_CAPTURED_HEX").expect("hex");
assert_eq!(
    bytes,
    expected,
    "CommunityRootSignedPayload wire bytes drifted: {} vs {}",
    hex::encode(&bytes),
    hex::encode(&expected)
);
let decoded: CommunityRootSignedPayload = canonical_cbor_decode(&bytes).expect("decode");
assert_eq!(decoded, p, "decoded payload must round-trip identically");
```

Apply the same pattern to `community_root_publish_payload_wire_bytes_pinned`. Remove all `let _ = ...` and `panic!` lines. Remove the temporary `println!` lines.

- [ ] **Step 5: Run the fixture tests to verify pass**

```bash
set -o pipefail
cd src-tauri && cargo test --test wire_format_community_sync_fixtures 2>&1 | tail -20
```

Expected: `test result: ok. 2 passed; 0 failed`.

- [ ] **Step 6: Run cargo fmt + clippy**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -30
```

Expected: both clean. If `cargo fmt --check` complains, run `cargo fmt --all` and re-verify.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/wire_format_community_sync_fixtures.rs
git commit -m "feat(zeb-256): add CommunityRootSignedPayload + publisher auth fields to wire envelope

Mirrors EventPayload/SignedMembershipEvent split: signed sub-payload is its
own canonical-CBOR type, attached to the wire envelope via into_wire(sig).
Wire-format fixture regenerated for the 4-field shape — pinned bytes prevent
silent drift on later refactors.

Old fixture bytes are wholly invalidated; this commit IS the deliberate regen.
Phase 2 has no production deployments so no migration path is needed."
```


---

## Task 2: Migrate `CommunityRootHlcTracker.per_device` to `(OwnerAddr, String)` keys

**Why:** Phase 2's tracker keys on `device_id: String` only — any member can spoof another's `device_id` because the `MembershipKey` decrypts everyone's publishes. Re-keying on `(OwnerAddr, String)` makes cross-addr collisions structurally impossible: each member's address gets its own per-device namespace, so a malicious Alice cannot squat Bob's HLC slot even if she emits a publish with `at.device_id == bob_dev`. The tracker's persisted shape changes; `load_replay`'s existing quarantine-and-default self-heal handles old-shape files without code changes.

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:300-344` (CommunityRootHlcTracker + would_accept + record)
- Modify: `src-tauri/tests/community_root_hlc_tracker_unit.rs` (all 5 tests update for new signature; add cross-addr test)
- Modify: `src-tauri/tests/community_state_persist_unit.rs` (add quarantine-and-default test for old shape)

- [ ] **Step 1: Write the failing cross-addr-isolation test**

Replace `src-tauri/tests/community_root_hlc_tracker_unit.rs` entirely with:

```rust
//! Unit tests for CommunityRootHlcTracker — replay protection +
//! per-(addr, device) monotonicity gates.
//!
//! ZEB-256: tracker key changed from `device_id: String` to
//! `(OwnerAddr, String)`. Cross-addr collisions are now structurally
//! impossible — Alice cannot squat Bob's HLC slot even with the
//! MembershipKey, because tracker entries are namespaced by addr.

use harmony_app::community_state_sync::CommunityRootHlcTracker;
use harmony_app::owner_state_types::{Hlc, OwnerAddr};

const ALICE: OwnerAddr = OwnerAddr([0xA1; 16]);
const BOB: OwnerAddr = OwnerAddr([0xB1; 16]);

fn h(wall: u64, log: u32, dev: &str) -> Hlc {
    Hlc {
        wall_ms: wall,
        logical: log,
        device_id: dev.into(),
    }
}

#[test]
fn would_accept_returns_true_for_unseen_addr_device() {
    let t = CommunityRootHlcTracker::default();
    assert!(t.would_accept(&ALICE, &h(100, 0, "a")));
}

#[test]
fn would_accept_rejects_equal_or_older_per_addr_device() {
    let mut t = CommunityRootHlcTracker::default();
    t.record(ALICE, h(100, 0, "a"));
    assert!(!t.would_accept(&ALICE, &h(100, 0, "a")), "exact replay rejected");
    assert!(!t.would_accept(&ALICE, &h(99, 5, "a")), "older wall_ms rejected");
    assert!(t.would_accept(&ALICE, &h(100, 1, "a")), "later logical accepted");
    assert!(t.would_accept(&ALICE, &h(101, 0, "a")), "later wall_ms accepted");
}

#[test]
fn cross_addr_same_device_id_is_isolated() {
    // ZEB-256 core defense: Alice publishes at (alice-dev, 200); Bob
    // submits at (alice-dev, 100). The tracker must accept Bob's
    // because his (BOB, "alice-dev") slot is unseen — the (ALICE,
    // "alice-dev") slot is irrelevant to Bob's namespace. Phase 2's
    // BTreeMap<String, Hlc> would reject Bob's because device_id
    // collisions clobbered each other; this test pins the fix.
    let mut t = CommunityRootHlcTracker::default();
    t.record(ALICE, h(200, 0, "alice-dev"));
    assert!(
        t.would_accept(&BOB, &h(100, 0, "alice-dev")),
        "Bob's slot must be independent of Alice's"
    );
    t.record(BOB, h(100, 0, "alice-dev"));
    // Pinning both still leaves them isolated.
    assert!(!t.would_accept(&ALICE, &h(199, 0, "alice-dev")));
    assert!(!t.would_accept(&BOB, &h(99, 0, "alice-dev")));
}

#[test]
fn would_accept_blocks_regression_at_caller_per_addr() {
    let mut t = CommunityRootHlcTracker::default();
    t.record(ALICE, h(200, 0, "a"));
    assert!(
        !t.would_accept(&ALICE, &h(100, 0, "a")),
        "older HLC must be caller-rejected"
    );
    assert!(!t.would_accept(&ALICE, &h(150, 0, "a")), "still bounded by 200");
    assert!(t.would_accept(&ALICE, &h(201, 0, "a")), "201 > 200");
}

#[test]
fn record_per_addr_device_isolates_clocks() {
    let mut t = CommunityRootHlcTracker::default();
    t.record(ALICE, h(500, 0, "a"));
    assert!(t.would_accept(&ALICE, &h(100, 0, "b")), "different device under same addr");
    t.record(ALICE, h(100, 0, "b"));
    assert!(!t.would_accept(&ALICE, &h(99, 0, "b")));
}

#[test]
fn record_is_strictly_newer_per_lex_order() {
    let mut t = CommunityRootHlcTracker::default();
    t.record(ALICE, h(100, 5, "a"));
    assert!(!t.would_accept(&ALICE, &h(100, 5, "a")));
    assert!(!t.would_accept(&ALICE, &h(100, 4, "a")));
    assert!(t.would_accept(&ALICE, &h(100, 6, "a")));
    assert!(t.would_accept(&ALICE, &h(101, 0, "a")));
    assert!(!t.would_accept(&ALICE, &h(99, 999, "a")));
}
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_root_hlc_tracker_unit 2>&1 | tail -20
```

Expected: compilation errors — `would_accept` / `record` signatures don't match the new `(OwnerAddr, _)` calls.

- [ ] **Step 3: Update `CommunityRootHlcTracker`**

In `src-tauri/src/community_state_sync.rs`, replace the `CommunityRootHlcTracker` struct + impl block (around lines 300-344) with:

```rust
/// Per-publisher-device latest-accepted HLC, namespaced by publisher
/// `OwnerAddr`. ZEB-256: re-keyed from `BTreeMap<String, Hlc>` so a
/// member cannot squat another member's HLC slot via shared
/// `MembershipKey`.
///
/// `Serialize` / `Deserialize` are derived so
/// `community_state_persist::save_replay` can canonical-CBOR-encode
/// the tracker. The `(OwnerAddr, String)` tuple key serialises as a
/// CBOR 2-array — `BTreeMap` iteration is by key order, so the
/// encoded form is deterministic.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CommunityRootHlcTracker {
    /// Per-(publisher_addr, device_id) latest-accepted HLC. New
    /// publishes are accepted only if STRICTLY NEWER than the
    /// recorded entry for the same `(addr, device_id)`.
    pub per_device: BTreeMap<(OwnerAddr, String), Hlc>,
}

impl CanonicalPayloadSealed for CommunityRootHlcTracker {}
impl CanonicalPayload for CommunityRootHlcTracker {}

impl CommunityRootHlcTracker {
    /// Test the candidate HLC against the per-(addr, device) latest.
    /// Returns `true` if the candidate strictly dominates the recorded
    /// entry (or there is none); `false` otherwise.
    ///
    /// Does NOT mutate — `record` is a separate step the caller invokes
    /// after the rest of the receive pipeline succeeds.
    pub fn would_accept(&self, publisher_addr: &OwnerAddr, candidate: &Hlc) -> bool {
        let key = (*publisher_addr, candidate.device_id.clone());
        match self.per_device.get(&key) {
            None => true,
            Some(prev) => candidate.is_strictly_newer_than(prev),
        }
    }

    /// Record `candidate` as the latest-accepted HLC for
    /// `(publisher_addr, candidate.device_id)`.
    ///
    /// Precondition: caller MUST have just verified `would_accept`
    /// returned `true`. We `debug_assert!` the precondition so a
    /// buggy call site surfaces in dev/test rather than silently
    /// no-opping. In release builds the insert is unconditional.
    pub fn record(&mut self, publisher_addr: OwnerAddr, candidate: Hlc) {
        debug_assert!(
            self.would_accept(&publisher_addr, &candidate),
            "CommunityRootHlcTracker::record called without would_accept check; \
             backward-jump for ({:?}, {})",
            publisher_addr,
            candidate.device_id
        );
        let key = (publisher_addr, candidate.device_id.clone());
        self.per_device.insert(key, candidate);
    }
}
```

- [ ] **Step 4: Update existing call sites in `community_state_sync.rs`**

There are two existing callers in this file that pass only a `&Hlc`:

1. `next_hlc` (around line 1052-1090): currently does `tracker.per_device.get(&ctx.device_id).cloned()` and `tracker.record(now.clone())`. Update to use the new key shape. **HOWEVER**: `next_hlc` is owned by Task 5 (it gains `self_owner` plumbing). For Task 2, leave the publish path TEMPORARILY broken — Task 5's commit will fix it. To keep this commit compiling, write a TEMP shim:

```rust
// TEMP for Task 2: use a placeholder OwnerAddr until Task 5 plumbs
// self_owner into InternalCtx. Task 5 deletes this shim.
let placeholder_addr = OwnerAddr([0u8; 16]);
let prev = tracker.per_device.get(&(placeholder_addr, ctx.device_id.clone())).cloned();
// ... and at the bottom:
tracker.record(placeholder_addr, now.clone());
```

This is the only file in the plan where a TEMP shim is acceptable, and it lives for exactly one task. Task 5 explicitly removes it.

2. `handle_incoming_publish` (around lines 1209-1213 and 1261-1264): same pattern — temporarily use `payload.at` device_id with a placeholder `OwnerAddr`. Task 6 replaces these with the real `payload.publisher_addr`. Write:

```rust
// TEMP for Task 2: placeholder addr; Task 6 reads payload.publisher_addr.
let placeholder_addr = OwnerAddr([0u8; 16]);
{
    let tracker = ctx.tracker.lock().await;
    if !tracker.would_accept(&placeholder_addr, &payload.at) {
        return IncomingOutcome::Duplicate;
    }
}
// ...
{
    let mut tracker = ctx.tracker.lock().await;
    tracker.record(placeholder_addr, payload.at.clone());
}
```

This keeps tests passing through Task 2 → Task 5 → Task 6 even though receive-side dedupe is *temporarily* effectively global (all addrs collapse to `[0; 16]`). Tests that exercise the receive pipeline are gated to single-publisher scenarios in Tasks 2-5; Task 6 reinstates per-addr namespacing before any multi-publisher integration test runs.

- [ ] **Step 5: Run the tracker unit tests to verify pass**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_root_hlc_tracker_unit 2>&1 | tail -20
```

Expected: all 6 tests pass.

- [ ] **Step 6: Add quarantine-and-self-heal test for old tracker shape**

In `src-tauri/tests/community_state_persist_unit.rs`, append:

```rust
#[test]
fn load_replay_quarantines_and_recovers_from_old_shape() {
    // ZEB-256 breaks tracker shape (per_device key changed from
    // String to (OwnerAddr, String)). Old persisted files MUST not
    // crash boot — load_replay quarantines the unparseable bytes
    // and returns CommunityRootHlcTracker::default(). The engine
    // then rebuilds the tracker organically as publishes arrive.

    use harmony_app::community_state_persist::{load_replay, save_replay};
    use harmony_app::community_state_sync::CommunityRootHlcTracker;
    use std::collections::BTreeMap;

    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("replay.cbor");

    // Hand-write CBOR bytes for the OLD shape: `{ per_device: {String: Hlc} }`.
    // Use the canonical CBOR encoder by serialising a minimal struct.
    #[derive(serde::Serialize)]
    struct OldShape {
        #[serde(rename = "per_device")]
        per_device: BTreeMap<String, harmony_app::owner_state_types::Hlc>,
    }
    let old = OldShape {
        per_device: {
            let mut m = BTreeMap::new();
            m.insert(
                "old-dev".to_string(),
                harmony_app::owner_state_types::Hlc {
                    wall_ms: 1,
                    logical: 0,
                    device_id: "old-dev".into(),
                },
            );
            m
        },
    };
    let old_bytes = serde_cbor::to_vec(&old).expect("encode old shape");
    std::fs::write(&path, &old_bytes).expect("write");

    let recovered = load_replay(&path).expect("load_replay self-heals");
    assert!(
        recovered.per_device.is_empty(),
        "recovered tracker MUST be empty (default)"
    );

    // Verify quarantined sibling file exists.
    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        entries.iter().any(|n| n.starts_with("replay.cbor.corrupt.")),
        "quarantined file must exist: {entries:?}"
    );

    // After self-heal, save_replay writes the new shape cleanly.
    let mut t = CommunityRootHlcTracker::default();
    let alice = harmony_app::owner_state_types::OwnerAddr([0xA1; 16]);
    t.record(
        alice,
        harmony_app::owner_state_types::Hlc {
            wall_ms: 100,
            logical: 0,
            device_id: "new-dev".into(),
        },
    );
    save_replay(&path, &t).expect("save_replay");
    let reloaded = load_replay(&path).expect("reload");
    assert_eq!(reloaded.per_device.len(), 1);
    assert!(reloaded.per_device.contains_key(&(alice, "new-dev".to_string())));
}
```

Note: `serde_cbor` may not be a direct dev-dep. If the test fails to compile because of a missing crate, replace the encoding with `ciborium::ser::into_writer` (already in tree) or hand-rolled bytes. Check `src-tauri/Cargo.toml` `[dev-dependencies]`. If neither is available, write the bytes manually as a hex-decoded literal:

```rust
// CBOR encoding of {"per_device": {"old-dev": {...}}}
let old_bytes = hex::decode("REPLACE_WITH_HAND_ENCODED_BYTES").expect("hex");
```

The exact hex can be regenerated by running the test with a `println!` and panicking, same procedure as Task 1 Step 4.

- [ ] **Step 7: Run persist tests**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_state_persist_unit 2>&1 | tail -20
```

Expected: all tests pass, including the new self-heal test.

- [ ] **Step 8: Run cargo fmt + clippy + full workspace tests**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -30
cd src-tauri && cargo test --workspace --all-targets --locked 2>&1 | tail -20
```

Expected: clean clippy + all tests pass. Tests that exercise multi-publisher receive (e.g. `engine_receives_remote_publish_and_merges_event`) still work because the placeholder addr collapses everything to a single namespace — but each test exercises only one publisher, so no namespace collision.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/community_state_sync.rs \
        src-tauri/tests/community_root_hlc_tracker_unit.rs \
        src-tauri/tests/community_state_persist_unit.rs
git commit -m "refactor(zeb-256): re-key CommunityRootHlcTracker on (OwnerAddr, String)

ZEB-256 core defense: cross-addr device_id collisions are now structurally
impossible — Alice cannot squat Bob's HLC slot even with shared MembershipKey.

would_accept and record now take publisher_addr alongside the HLC. Existing
callers in next_hlc + handle_incoming_publish use a TEMP placeholder OwnerAddr;
Tasks 5 and 6 plumb the real value through.

load_replay's existing quarantine-and-default self-heal handles the breaking
persisted shape — pinned by a new persist unit test."
```


---

## Task 3: Add 3 new `CommunitySyncError` variants + reason_tag mapping

**Why:** The verify flow in Task 6 needs distinct error variants so the IPC layer can surface specific reason_tags (`publisher_not_joined`, `publisher_unknown`, `publisher_sig_invalid`) to the frontend banner. Adding the variants ahead of the consumer keeps the verify-flow edit focused on logic, not type plumbing. The variants must match the spec § 7 exactly — each carries the diagnostic context the IPC layer renders.

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:228-264` (CommunitySyncError enum)
- Modify: `src-tauri/src/community_state_sync.rs:1535-1548` (classify_incoming_error)

- [ ] **Step 1: Write a failing exhaustive-match test**

Append to `src-tauri/tests/community_sync_engine_unit.rs`:

```rust
#[test]
fn classify_incoming_error_covers_publisher_auth_variants() {
    use harmony_app::community_membership::MemberStatus;
    use harmony_app::community_state_sync::CommunitySyncError;
    use harmony_app::owner_state_types::OwnerAddr;

    // Each variant has a distinct, stable reason_tag — these strings
    // are the contract with the frontend banner copy.
    let alice = OwnerAddr([0xA1; 16]);
    let cases = [
        (
            CommunitySyncError::PublisherNotJoined {
                addr: alice,
                status: MemberStatus::Banned,
                left_at: None,
            },
            "publisher_not_joined",
        ),
        (
            CommunitySyncError::UnknownPublisher { addr: alice },
            "publisher_unknown",
        ),
        (
            CommunitySyncError::PublisherSigInvalid { addr: alice },
            "publisher_sig_invalid",
        ),
    ];
    for (err, expected_tag) in cases {
        let actual_tag = harmony_app::community_state_sync::classify_incoming_error_for_test(&err);
        assert_eq!(
            actual_tag, expected_tag,
            "reason_tag for {err:?} must be {expected_tag}"
        );
    }
}
```

We expose `classify_incoming_error` for tests under a `#[doc(hidden)]` re-export named `classify_incoming_error_for_test`. Step 3 adds it.

- [ ] **Step 2: Run the test to verify it fails to compile**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_engine_unit classify_incoming_error_covers_publisher_auth_variants 2>&1 | tail -20
```

Expected: compilation errors — `PublisherNotJoined` etc. are not variants; `classify_incoming_error_for_test` doesn't exist.

- [ ] **Step 3: Add the new variants + helper**

In `src-tauri/src/community_state_sync.rs`, append to the `CommunitySyncError` enum (after `MissingIdentityResolver`):

```rust
    /// Publish was signed correctly but the publisher's membership
    /// state at the publish HLC does NOT have status `Joined`. Either
    /// they were kicked, banned, never joined, or are still pending
    /// invitation. Tracker NOT advanced — defends against the
    /// post-kick censorship attack where a kicked-but-still-keyed
    /// member tries to squat HLC slots until ZEB-249 (key rotation)
    /// lands.
    #[error(
        "publisher {addr:?} not joined at publish HLC \
         (status: {status:?}, left_at: {left_at:?})"
    )]
    PublisherNotJoined {
        addr: OwnerAddr,
        status: crate::community_membership::MemberStatus,
        /// `MemberState.left_at` field — set on both Leave and Kick
        /// events (the underlying CRDT field is overloaded). For
        /// `PublisherNotJoined` triggered by a kick this carries the
        /// kick HLC; for one triggered by a voluntary Leave-then-
        /// republish this carries the Leave HLC. `None` when the
        /// publisher was never a member.
        left_at: Option<Hlc>,
    },

    /// `IdentityResolver` returned `None` for `publisher_addr`. Cold
    /// cache (the publisher's identity_pub hasn't propagated to our
    /// owner-state cache yet) or the addr was never a member.
    /// Transient when caused by cold cache; persistent when caused by
    /// a wholly-fabricated addr — both surface the same way at this
    /// layer. Tracker NOT advanced; next publish after cache
    /// propagation succeeds.
    #[error(
        "publisher {addr:?} identity not in resolver — \
         cache cold or addr not yet propagated"
    )]
    UnknownPublisher { addr: OwnerAddr },

    /// Ed25519 signature over `canonical_cbor(CommunityRootSignedPayload)`
    /// did not validate against the resolved identity_pub. This is
    /// the load-bearing defense against the spoofing attack: a
    /// malicious member with the `MembershipKey` cannot forge a
    /// publish claiming another member's `publisher_addr` because
    /// they don't have that member's signing key. Tracker NOT
    /// advanced.
    #[error("publisher signature invalid for addr {addr:?}")]
    PublisherSigInvalid { addr: OwnerAddr },
```

Update `classify_incoming_error` (around line 1535) by adding three new arms:

```rust
fn classify_incoming_error(err: &CommunitySyncError) -> &'static str {
    match err {
        CommunitySyncError::Crypto(_) => "decrypt_failed",
        CommunitySyncError::CborEncode(_) | CommunitySyncError::CborDecode(_) => {
            "wire_decode_failed"
        }
        CommunitySyncError::ContentStore(_) => "blob_fetch_failed",
        CommunitySyncError::BlobNotFound { .. } => "blob_not_found",
        CommunitySyncError::TransportClosed => "transport_closed",
        CommunitySyncError::Persist(_) => "persist_failed",
        CommunitySyncError::MisroutedBlob { .. } => "misrouted_blob",
        CommunitySyncError::MissingIdentityResolver => "missing_identity_resolver",
        CommunitySyncError::PublisherNotJoined { .. } => "publisher_not_joined",
        CommunitySyncError::UnknownPublisher { .. } => "publisher_unknown",
        CommunitySyncError::PublisherSigInvalid { .. } => "publisher_sig_invalid",
    }
}

/// Test-only re-export of `classify_incoming_error`. Lets the unit
/// test pin the reason_tag → variant mapping without exposing the
/// internal function as part of the public API.
#[doc(hidden)]
pub fn classify_incoming_error_for_test(err: &CommunitySyncError) -> &'static str {
    classify_incoming_error(err)
}
```

- [ ] **Step 4: Run the new test to verify it passes**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_engine_unit classify_incoming_error_covers_publisher_auth_variants 2>&1 | tail -20
```

Expected: pass.

- [ ] **Step 5: cargo fmt + clippy + full tests**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -30
cd src-tauri && cargo test --workspace --all-targets --locked 2>&1 | tail -20
```

Expected: all green. The new variants are unconstructed (Task 6 is the first producer) but their existence + reason_tag is now pinned.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync_engine_unit.rs
git commit -m "feat(zeb-256): add publisher-auth error variants + reason_tag mapping

Three new CommunitySyncError variants — PublisherNotJoined, UnknownPublisher,
PublisherSigInvalid — match the spec § 7 verbatim. Each maps to a stable
reason_tag the frontend banner switches on.

Variants are pinned by a unit test that asserts the reason_tag mapping;
producers land in Task 6's verify-flow update."
```


---

## Task 4: Add `self_owner` + `signing_key` to `CommunitySyncEngineConfig` + `InternalCtx`

**Why:** The publish path needs the local `OwnerAddr` to embed in `publisher_addr` and the local `Ed25519 SigningKey` to produce `publisher_sig`. Both flow in via `CommunitySyncEngineConfig` so each test site updates its config exactly once. `InternalCtx` mirrors the engine's per-task state — it gains the same fields. This task is purely structural — sign/verify logic lands in Tasks 5 + 6.

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:417-450` (CommunitySyncEngineConfig)
- Modify: `src-tauri/src/community_state_sync.rs:514-565` (CommunitySyncEngine::new — clone fields into InternalCtx)
- Modify: `src-tauri/src/community_state_sync.rs:724-744` (InternalCtx)
- Modify: `src-tauri/tests/community_sync_engine_unit.rs` (3 existing engine constructions + 1 to-be-added test)
- Modify: `src-tauri/tests/community_sync_integration.rs` (no direct config — uses spawn_engine; updated in Task 7)
- Modify: `src-tauri/tests/community_sync_registry_unit.rs` (touched in Task 7)
- Modify: `src-tauri/tests/community_open_flow_integration.rs` (touched in Task 7)

- [ ] **Step 1: Write a failing test that constructs an engine WITH the new fields**

Append to `src-tauri/tests/community_sync_engine_unit.rs`:

```rust
#[tokio::test]
async fn engine_accepts_self_owner_and_signing_key_in_config() {
    use harmony_app::community_state_sync::{
        CommunityRootHlcTracker, CommunitySyncEngine, CommunitySyncEngineConfig,
        DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    let (out_tx, _out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, _cas_op_rx) = tokio::sync::mpsc::channel(8);

    let community_id = SpaceId([1u8; 16]);
    let identity = PrivateIdentity::from_seed(&[0xa1; 32]);
    let self_owner = OwnerAddr(identity.identity.address_hash);
    let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]));

    let state = std::sync::Arc::new(tokio::sync::Mutex::new(
        harmony_app::community_state_crdt::CommunityState::new(community_id),
    ));
    let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(
        CommunityRootHlcTracker::default(),
    ));
    let cs: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(
        RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(1000)),
    );
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: MembershipKey::new([0x42; 32]),
        admin_addr: self_owner,
        is_invite_only: false,
        device_id: "test-device".into(),
        self_owner,
        signing_key,
        state,
        tracker,
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
    });
    engine.shutdown().await.expect("shutdown");
}
```

- [ ] **Step 2: Run the test to verify it fails to compile**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_engine_unit engine_accepts_self_owner_and_signing_key_in_config 2>&1 | tail -20
```

Expected: compilation error — `self_owner` and `signing_key` are not fields of `CommunitySyncEngineConfig`.

- [ ] **Step 3: Add fields to `CommunitySyncEngineConfig`**

In `src-tauri/src/community_state_sync.rs`, find `CommunitySyncEngineConfig` (around line 417) and add two fields after `device_id: String,`:

```rust
    /// Owner address of the local member. Embedded in every publish so
    /// receivers can verify the signature against the right
    /// identity_pub (resolved via `IdentityResolver`, NOT carried
    /// inline). Also used by `next_hlc` to namespace tracker entries.
    pub self_owner: OwnerAddr,

    /// Local Ed25519 signing key for state-root publish signing. Same
    /// handle Phase 3's `insert_local_event` already uses for membership
    /// event signing — sourced from the local `PrivateIdentity` at
    /// engine spawn time. Wrapped in `Arc` so the engine + every
    /// internal task share the same key without copying the secret.
    pub signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
```

- [ ] **Step 4: Add the same fields to `InternalCtx`**

Find `struct InternalCtx` (around line 724) and add after `device_id`:

```rust
    self_owner: OwnerAddr,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
```

- [ ] **Step 5: Plumb fields through `CommunitySyncEngine::new`**

In `CommunitySyncEngine::new` (around line 514), the `tokio::spawn(internal_task(InternalCtx { ... }))` call already lists every field. Add the two new ones, sourced from `cfg`:

```rust
        let task = tokio::spawn(internal_task(InternalCtx {
            community_id: cfg.community_id,
            membership_key: cfg.membership_key,
            admin_addr: cfg.admin_addr,
            is_invite_only: cfg.is_invite_only,
            device_id: cfg.device_id,
            self_owner: cfg.self_owner,
            signing_key: cfg.signing_key,
            state: cfg.state,
            // ... rest unchanged
```

The engine struct itself (`CommunitySyncEngine`) does NOT need new fields — only the spawned task uses them. This matches Phase 3's pattern where `is_invite_only` flows through both the engine struct (for `insert_local_event`) and `InternalCtx` (for `verify_event` against `VerifyContext`); here, only the publish path uses these fields, so the engine struct stays lean.

- [ ] **Step 6: Update existing test sites to pass the new fields**

Three test functions in `community_sync_engine_unit.rs` build a `CommunitySyncEngineConfig`:
- `engine_constructs_and_shuts_down_cleanly` (line 23)
- `flush_now_publishes_one_root_publish` (line 67)
- `engine_receives_remote_publish_and_merges_event` (line 134) — TWO configs (engine_a and engine_b)
- `engine_emits_membership_delta_on_remote_insert` (line 330) — TWO configs
- `engine_insert_local_event_emits_delta_and_notifies_publish` (line 504)

For each, add `self_owner: <existing admin or per-test addr>,` and `signing_key: std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])),` (or the per-test identity's signing key when the test cares about the actual signature — Tasks 5 + 6 introduce those tests). For now, use a deterministic dummy `[0x42; 32]` seed — the publish path won't sign meaningfully until Task 5.

Concrete example for the first test:

```rust
    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: admin,
        is_invite_only: false,
        device_id: "test-device".into(),
        self_owner: admin,                                  // NEW
        signing_key: std::sync::Arc::new(                   // NEW
            ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]),
        ),
        state: Arc::clone(&state),
        // ... rest unchanged
```

For the two-engine tests (`engine_receives_remote_publish_and_merges_event`, `engine_emits_membership_delta_on_remote_insert`), engine_a's `self_owner` is the admin (publisher) and engine_b's `self_owner` is a distinct addr — generate one with `OwnerAddr([0xB1; 16])` if no other addr is in scope. Engine_b doesn't publish in those tests, so its signing_key just needs to compile.

For `engine_insert_local_event_emits_delta_and_notifies_publish`, the test already has `let identity = PrivateIdentity::from_seed(&[0xc1; 32])` — use `self_owner: admin` and a dummy signing_key (the test doesn't validate the wire signature).

- [ ] **Step 7: Update `engine_open_flow` test sites**

Phase 3's `community_open_flow_integration.rs` and `community_sync_registry_unit.rs` go through `CommunitySyncRegistry::spawn_engine`, which is the layer that currently constructs `CommunitySyncEngineConfig` internally. **For Task 4, the registry does NOT yet have `self_owner` / `signing_key` — those land in Task 7.** So `spawn_engine` won't compile until Task 7. To bridge:

In `src-tauri/src/community_state_sync.rs`, inside `spawn_engine` (around line 1701), add hardcoded placeholders:

```rust
        let engine = Arc::new(CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key,
            admin_addr,
            is_invite_only,
            device_id: self.cfg.device_id.clone(),
            // TEMP for Task 4: registry config gets these fields in Task 7.
            self_owner: admin_addr,
            signing_key: std::sync::Arc::new(
                ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]),
            ),
            state,
            // ... rest unchanged
```

These TEMP values are deliberately wrong — `admin_addr` is the community's bootstrap admin, not necessarily the local member, and the dummy signing key cannot produce a valid sig. Task 7 plumbs the real values through and Task 7's tests verify the corrected wiring. The TEMP values are flagged in code comments; Task 7 grep removes them.

Alternative if subagent prefers strict TDD compilation gates: do Task 4 + Task 7 as one combined task. The plan splits them because the engine surface and the registry surface are distinct API contracts — separating their commits makes review cleaner.

- [ ] **Step 8: Run the new test to verify it passes**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_engine_unit engine_accepts_self_owner_and_signing_key_in_config 2>&1 | tail -20
```

Expected: pass.

- [ ] **Step 9: Run cargo fmt + clippy + full workspace**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -30
cd src-tauri && cargo test --workspace --all-targets --locked 2>&1 | tail -20
```

Expected: all green. Existing tests use the dummy signing_key; the wire signature isn't validated yet.

- [ ] **Step 10: Commit**

```bash
git add src-tauri/src/community_state_sync.rs \
        src-tauri/tests/community_sync_engine_unit.rs
git commit -m "feat(zeb-256): plumb self_owner + signing_key into CommunitySyncEngineConfig

Engine config gains the two fields the publish path needs: local OwnerAddr
(embedded in every publish's publisher_addr) and Arc<SigningKey> (signs the
canonical-CBOR sub-payload). InternalCtx mirrors them so the spawned task
has direct access.

Registry's spawn_engine uses TEMP placeholders for both fields — Task 7
plumbs the registry's CommunityRegistryConfig through to remove them.

Sign + verify logic lands in Tasks 5 and 6."
```


---

## Task 5: Sign on publish — `next_hlc` (real `self_owner`) + `publish_root_now`

**Why:** This task replaces the Task 2 placeholder `OwnerAddr([0; 16])` in `next_hlc` with the real `ctx.self_owner`, and updates `publish_root_now` to build a `CommunityRootSignedPayload`, sign its canonical CBOR with `ctx.signing_key`, and wrap into a wire envelope. The signing happens on the runtime thread (Ed25519 sign is microseconds; `spawn_blocking` would only add latency). The unit test `publish_carries_valid_publisher_sig` pins the positive path.

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:974-1025` (publish_root_now)
- Modify: `src-tauri/src/community_state_sync.rs:1045-1091` (next_hlc — replace TEMP placeholder)
- Modify: `src-tauri/tests/community_sync_engine_unit.rs` (add positive-sig test)

- [ ] **Step 1: Write the failing positive-sig test**

Append to `src-tauri/tests/community_sync_engine_unit.rs`:

```rust
#[tokio::test]
async fn publish_carries_valid_publisher_sig() {
    use ed25519_dalek::Verifier;
    use harmony_app::community_state_sync::{
        CommunityRootHlcTracker, CommunityRootPublishPayload, CommunityRootSignedPayload,
        CommunitySyncEngine, CommunitySyncEngineConfig, DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crypto::{
        canonical_cbor_decode, canonical_cbor_encode, decrypt_root_publish,
    };
    use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (_in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel(8);

    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            if let CasOp::PutLocal {
                reply: Some(reply), ..
            } = op
            {
                let _ = reply.send(Ok(()));
            }
        }
    });

    let community_id = SpaceId([1u8; 16]);
    let mk = MembershipKey::new([0x42; 32]);
    let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0xAB; 32]));
    let verifying_key = signing_key.verifying_key();
    // self_owner is just an opaque tag here; the test verifies sig
    // against verifying_key directly without going through resolver.
    let self_owner = OwnerAddr([0x12; 16]);
    let admin = self_owner;

    let state = std::sync::Arc::new(tokio::sync::Mutex::new(
        harmony_app::community_state_crdt::CommunityState::new(community_id),
    ));
    let tracker = std::sync::Arc::new(tokio::sync::Mutex::new(
        CommunityRootHlcTracker::default(),
    ));
    let cs: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(
        RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(1000)),
    );
    let tmp = tempfile::tempdir().expect("tempdir");

    let engine = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk.clone(),
        admin_addr: admin,
        is_invite_only: false,
        device_id: "pub-dev".into(),
        self_owner,
        signing_key,
        state,
        tracker,
        content_store: cs,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp.path().join("crdt.cbor"),
            replay: tmp.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: None,
        error_tx: None,
        delta_tx: None,
    });

    engine.flush_now().await.expect("flush_now");

    let wire = out_rx
        .recv()
        .await
        .expect("publisher_tx must have received one wire packet");
    let payload_bytes = decrypt_root_publish(&mk, &wire).expect("decrypt");
    let payload: CommunityRootPublishPayload =
        canonical_cbor_decode(&payload_bytes).expect("decode envelope");

    // The wire envelope's publisher_addr matches self_owner.
    assert_eq!(payload.publisher_addr, self_owner);

    // The publisher_sig validates against the verifying_key for the
    // canonical CBOR of CommunityRootSignedPayload::from(&payload).
    let signed = CommunityRootSignedPayload::from(&payload);
    let signed_bytes = canonical_cbor_encode(&signed).expect("encode signed");
    let sig = ed25519_dalek::Signature::from_bytes(&payload.publisher_sig);
    verifying_key
        .verify(&signed_bytes, &sig)
        .expect("publisher_sig must verify against signing_key.verifying_key()");

    engine.shutdown().await.expect("shutdown");
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_engine_unit publish_carries_valid_publisher_sig 2>&1 | tail -30
```

Expected: fails — currently `publish_root_now` builds the OLD 2-field envelope and the `publisher_addr` field is missing from the encoded payload. The decoded payload also has no `publisher_sig`. The test panics on the first assertion.

- [ ] **Step 3: Update `next_hlc` to use the real `self_owner`**

In `src-tauri/src/community_state_sync.rs`, replace the entire `next_hlc` function (around line 1045) with:

```rust
async fn next_hlc(ctx: &InternalCtx) -> Hlc {
    use std::time::{SystemTime, UNIX_EPOCH};
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let mut tracker = ctx.tracker.lock().await;
    let key = (ctx.self_owner, ctx.device_id.clone());
    let prev = tracker.per_device.get(&key).cloned();
    let now = match prev.as_ref() {
        None => Hlc {
            wall_ms,
            logical: 0,
            device_id: ctx.device_id.clone(),
        },
        Some(p) if wall_ms > p.wall_ms => Hlc {
            wall_ms,
            logical: 0,
            device_id: ctx.device_id.clone(),
        },
        Some(p) if p.logical == u32::MAX => Hlc {
            // Saturation escape — see prior comment block above.
            wall_ms: p.wall_ms.saturating_add(1),
            logical: 0,
            device_id: ctx.device_id.clone(),
        },
        Some(p) => Hlc {
            wall_ms: p.wall_ms,
            logical: p.logical + 1,
            device_id: ctx.device_id.clone(),
        },
    };
    tracker.record(ctx.self_owner, now.clone());
    now
}
```

This replaces the Task 2 TEMP placeholder. The tracker now correctly namespaces self-publishes under `(self_owner, device_id)` instead of `([0; 16], device_id)`.

- [ ] **Step 4: Update `publish_root_now` to sign**

In `src-tauri/src/community_state_sync.rs`, replace the body of `publish_root_now` (around line 974) — specifically the section from "5. Build state-root payload" through "6. Encrypt with random-nonce root AEAD". The replacement:

```rust
async fn publish_root_now(ctx: &InternalCtx) -> Result<(), CommunitySyncError> {
    use crate::owner_state_crypto::canonical_cbor_encode;
    use ed25519_dalek::Signer;

    // Snapshot CRDT state under brief lock; drop guard before the
    // expensive encode + AEAD + CAS hops below.
    let snapshot = {
        let state = ctx.state.lock().await;
        state.clone()
    };

    // 1. Canonical-CBOR encode the CommunityState as the cleartext blob.
    let blob_cleartext = canonical_cbor_encode(&snapshot)
        .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;

    // 2. Encrypt with deterministic-nonce blob AEAD.
    let blob_ciphertext = encrypt_blob(&ctx.membership_key, &blob_cleartext)?;

    // 3. Derive structured ContentId (encrypted: true).
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .map_err(|e| {
        CommunitySyncError::Crypto(CommunityCryptoError::ContentIdDerivation(e.to_string()))
    })?;

    // 4. Put into ContentStore.
    ctx.content_store.put(root_cid, blob_ciphertext).await?;

    // 5. Build the SIGNED sub-payload with a strictly-newer HLC.
    let now = next_hlc(ctx).await;
    let signed = CommunityRootSignedPayload {
        root_cid,
        publisher_addr: ctx.self_owner,
        at: now,
    };

    // 6. Sign the canonical CBOR of the signed sub-payload. Ed25519
    //    sign is microseconds, fine on the runtime thread (no
    //    spawn_blocking).
    let signed_bytes = canonical_cbor_encode(&signed)
        .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;
    let publisher_sig = ctx.signing_key.sign(&signed_bytes).to_bytes();

    // 7. Wrap into the full wire envelope.
    let payload = signed.into_wire(publisher_sig);
    let payload_bytes = canonical_cbor_encode(&payload)
        .map_err(|e| CommunitySyncError::CborEncode(e.to_string()))?;

    // 8. Encrypt with random-nonce root AEAD.
    let wire = encrypt_root_publish(&ctx.membership_key, &payload_bytes)?;

    // 9. Send onto outbound channel.
    ctx.publisher_tx
        .send(wire)
        .await
        .map_err(|_| CommunitySyncError::TransportClosed)?;

    Ok(())
}
```

- [ ] **Step 5: Run the positive-sig test to verify pass**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_engine_unit publish_carries_valid_publisher_sig 2>&1 | tail -30
```

Expected: pass.

- [ ] **Step 6: cargo fmt + clippy + full tests**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -30
cd src-tauri && cargo test --workspace --all-targets --locked 2>&1 | tail -20
```

Expected: all green. Existing receive-side tests still pass — Task 6 hasn't yet enforced sig verification, so the dummy `[0x42; 32]` signing keys in other tests still produce envelopes the receiver accepts (because the receiver currently ignores `publisher_sig`).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync_engine_unit.rs
git commit -m "feat(zeb-256): sign every state-root publish on the way out

publish_root_now now builds a CommunityRootSignedPayload, signs its canonical
CBOR with ctx.signing_key, and wraps into the wire envelope. Ed25519 sign is
microseconds — runs on the runtime thread, no spawn_blocking.

next_hlc replaces the Task 2 TEMP placeholder OwnerAddr with ctx.self_owner.
Tracker now correctly namespaces self-publishes per (self_owner, device_id).

Receive-side sig verification lands in Task 6."
```


---

## Task 6: Verify on receive — three new gates in `handle_incoming_publish`

**Why:** This is the load-bearing security task. Receive-side adds three gates BEFORE the existing replay-tracker check, in cheapest-first order: membership-at-HLC (local lookup, free), identity_pub resolution (cache lookup), and Ed25519 signature verification (microseconds). Each rejection class has a distinct error variant from Task 3 and surfaces a distinct reason_tag. Tracker NOT advanced on any rejection — that's the censorship-defense invariant.

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:1163-1430` (handle_incoming_publish)
- Modify: `src-tauri/tests/community_sync_engine_unit.rs` (add 4 unit tests)

- [ ] **Step 1: Write the four failing receive-side tests**

Append to `src-tauri/tests/community_sync_engine_unit.rs`. Each test follows the same shape: build engine_b with a real `IdentityResolver`, hand-craft a wire payload, inject into engine_b's subscriber_rx, observe the rejection.

Helper struct (if not already present from earlier tests):

```rust
// Helper used across the receive-side rejection tests. Maps a fixed
// addr → identity_pub set; returns None for any other addr.
struct MapResolver {
    entries: std::collections::HashMap<harmony_app::owner_state_types::OwnerAddr, [u8; 64]>,
}
#[async_trait::async_trait]
impl harmony_app::community_state_sync::IdentityResolver for MapResolver {
    async fn resolve(
        &self,
        addr: &harmony_app::owner_state_types::OwnerAddr,
    ) -> Option<[u8; 64]> {
        self.entries.get(addr).copied()
    }
}
```

Test 1 — sig-spoof rejected:

```rust
#[tokio::test]
async fn spoofed_publisher_addr_rejected_with_publisher_sig_invalid() {
    use ed25519_dalek::Signer;
    use harmony_app::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use harmony_app::community_state_crdt::CommunityState;
    use harmony_app::community_state_sync::{
        CommunityDegradedReport, CommunityRootHlcTracker, CommunityRootPublishPayload,
        CommunityRootSignedPayload, CommunitySyncEngine, CommunitySyncEngineConfig,
        DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crypto::{
        canonical_cbor_encode, encrypt_blob, encrypt_root_publish,
    };
    use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    let (out_tx, _out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel(64);
    let (degraded_tx, mut degraded_rx) =
        tokio::sync::mpsc::channel::<CommunityDegradedReport>(8);

    let cas: std::sync::Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>,
        >,
    > = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = std::sync::Arc::clone(&cas);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch {
                    cid,
                    timeout: _,
                    reply,
                } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
            }
        }
    });

    let community_id = SpaceId([7u8; 16]);
    let mk = MembershipKey::new([0xAA; 32]);

    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();

    let bob = PrivateIdentity::from_seed(&[0xb1; 32]);
    let bob_addr = OwnerAddr(bob.identity.address_hash);
    let bob_pub = bob.identity.to_public_bytes();
    let bob_signing = ed25519_dalek::SigningKey::from_bytes(&[0xb1; 32]);

    // Build a CommunityState where Alice is Joined (admin self-Join).
    let mut alice_state = CommunityState::new(community_id);
    {
        let payload = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: alice_addr,
            at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        };
        let event = sign_event_with_identity(&payload, &alice).expect("sign");
        let outcome = alice_state.insert_event(
            event,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: alice_addr,
                is_invite_only: false,
                actor_identity_pub: &alice_pub,
                countersigner_identity_pub: None,
            },
        );
        assert_eq!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        );
    }

    // Encrypt Alice's state into the CAS so the receiver can fetch.
    let blob_cleartext = canonical_cbor_encode(&alice_state).expect("encode state");
    let blob_ciphertext = encrypt_blob(&mk, &blob_cleartext).expect("encrypt blob");
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("cid");
    cas.lock().await.insert(root_cid, blob_ciphertext);

    // Build a forged envelope: publisher_addr = alice (so resolver
    // hands back alice_pub), but signed with Bob's key (so the sig
    // doesn't validate against alice_pub).
    let signed = CommunityRootSignedPayload {
        root_cid,
        publisher_addr: alice_addr,
        at: Hlc {
            wall_ms: 2000,
            logical: 0,
            device_id: "alice-dev".into(),
        },
    };
    let signed_bytes = canonical_cbor_encode(&signed).expect("encode signed");
    let bad_sig = bob_signing.sign(&signed_bytes).to_bytes();
    let envelope = signed.into_wire(bad_sig);
    let envelope_bytes = canonical_cbor_encode(&envelope).expect("encode envelope");
    let wire = encrypt_root_publish(&mk, &envelope_bytes).expect("encrypt root");

    // Engine_b — receiver. Uses the resolver that knows both alice
    // and bob, so resolve(alice) returns alice_pub (used to verify
    // the sig, which fails because Bob signed it).
    let mut entries = std::collections::HashMap::new();
    entries.insert(alice_addr, alice_pub);
    entries.insert(bob_addr, bob_pub);
    let resolver: std::sync::Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        std::sync::Arc::new(MapResolver { entries });

    let state_b = std::sync::Arc::new(tokio::sync::Mutex::new(
        CommunityState::new(community_id),
    ));
    let tracker_b = std::sync::Arc::new(tokio::sync::Mutex::new(
        CommunityRootHlcTracker::default(),
    ));
    let cs_b: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(
        RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(2000)),
    );
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: alice_addr,
        is_invite_only: false,
        device_id: "b-dev".into(),
        self_owner: bob_addr,
        signing_key: std::sync::Arc::new(
            ed25519_dalek::SigningKey::from_bytes(&[0xb1; 32]),
        ),
        state: std::sync::Arc::clone(&state_b),
        tracker: std::sync::Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(resolver),
        error_tx: Some(degraded_tx),
        delta_tx: None,
    });

    in_tx.send(wire).await.expect("inject wire");

    // Wait for the rejection report.
    let report = tokio::time::timeout(std::time::Duration::from_secs(2), degraded_rx.recv())
        .await
        .expect("degraded report within 2s")
        .expect("degraded channel still open");
    assert_eq!(report.reason_tag, "publisher_sig_invalid");

    // Tracker NOT advanced for alice's slot.
    let t = tracker_b.lock().await;
    assert!(
        !t.per_device.contains_key(&(alice_addr, "alice-dev".to_string())),
        "tracker MUST NOT have advanced on sig-invalid rejection"
    );
    drop(t);

    // CRDT NOT mutated.
    let s = state_b.lock().await;
    assert!(s.events.is_empty(), "no events should have merged");
    drop(s);

    engine_b.shutdown().await.expect("shutdown");
}
```

Test 2 — kicked-member publish rejected:

```rust
#[tokio::test]
async fn kicked_member_publish_rejected_with_publisher_not_joined() {
    use ed25519_dalek::Signer;
    use harmony_app::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use harmony_app::community_state_crdt::CommunityState;
    use harmony_app::community_state_sync::{
        CommunityDegradedReport, CommunityRootHlcTracker, CommunityRootSignedPayload,
        CommunitySyncEngine, CommunitySyncEngineConfig, DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crypto::{
        canonical_cbor_encode, encrypt_blob, encrypt_root_publish,
    };
    use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    // CAS plumbing identical to Test 1 — see helper macro below or
    // copy the same lines. (Repeating verbatim per "no Similar to
    // Task N" rule.)
    let (out_tx, _out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel(64);
    let (degraded_tx, mut degraded_rx) =
        tokio::sync::mpsc::channel::<CommunityDegradedReport>(8);
    let cas: std::sync::Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>,
        >,
    > = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = std::sync::Arc::clone(&cas);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch {
                    cid,
                    timeout: _,
                    reply,
                } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
            }
        }
    });

    let community_id = SpaceId([8u8; 16]);
    let mk = MembershipKey::new([0xCC; 32]);

    let admin = PrivateIdentity::from_seed(&[0xa0; 32]);
    let admin_addr = OwnerAddr(admin.identity.address_hash);
    let admin_pub = admin.identity.to_public_bytes();

    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();
    let alice_signing = ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]);

    // Build a CommunityState where:
    //   - admin is Joined
    //   - alice was Joined then Kicked at HLC 100
    let mut state = CommunityState::new(community_id);
    let mut hlc = 10u64;
    let mut push_event = |state: &mut CommunityState,
                          actor: OwnerAddr,
                          actor_id: &PrivateIdentity,
                          actor_pub: &[u8; 64],
                          kind: MembershipEventKind,
                          dev: &str,
                          wall: u64,
                          eid: [u8; 16]| {
        let p = EventPayload {
            id: eid,
            community_id,
            kind,
            actor,
            at: Hlc {
                wall_ms: wall,
                logical: 0,
                device_id: dev.into(),
            },
        };
        let ev = sign_event_with_identity(&p, actor_id).expect("sign");
        let outcome = state.insert_event(
            ev,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr,
                is_invite_only: false,
                actor_identity_pub: actor_pub,
                countersigner_identity_pub: None,
            },
        );
        assert_eq!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted,
            "fixture insert must succeed"
        );
    };
    push_event(
        &mut state,
        admin_addr,
        &admin,
        &admin_pub,
        MembershipEventKind::Join,
        "admin-dev",
        hlc,
        [1u8; 16],
    );
    hlc += 10;
    push_event(
        &mut state,
        alice_addr,
        &alice,
        &alice_pub,
        MembershipEventKind::Join,
        "alice-dev",
        hlc,
        [2u8; 16],
    );
    hlc = 100;
    // Admin kicks alice at HLC 100.
    push_event(
        &mut state,
        admin_addr,
        &admin,
        &admin_pub,
        MembershipEventKind::Kick {
            target: alice_addr,
            reason: None,
        },
        "admin-dev",
        hlc,
        [3u8; 16],
    );

    // Encrypt + put.
    let blob_cleartext = canonical_cbor_encode(&state).expect("encode state");
    let blob_ciphertext = encrypt_blob(&mk, &blob_cleartext).expect("encrypt blob");
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("cid");
    cas.lock().await.insert(root_cid, blob_ciphertext);

    // Alice publishes at HLC 150 — AFTER her kick. Her sig is valid
    // (she still has her signing key); the membership-at-HLC gate
    // rejects.
    let signed = CommunityRootSignedPayload {
        root_cid,
        publisher_addr: alice_addr,
        at: Hlc {
            wall_ms: 150,
            logical: 0,
            device_id: "alice-dev".into(),
        },
    };
    let signed_bytes = canonical_cbor_encode(&signed).expect("encode signed");
    let valid_sig = alice_signing.sign(&signed_bytes).to_bytes();
    let envelope = signed.into_wire(valid_sig);
    let envelope_bytes = canonical_cbor_encode(&envelope).expect("encode env");
    let wire = encrypt_root_publish(&mk, &envelope_bytes).expect("encrypt root");

    let mut entries = std::collections::HashMap::new();
    entries.insert(admin_addr, admin_pub);
    entries.insert(alice_addr, alice_pub);
    let resolver: std::sync::Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        std::sync::Arc::new(MapResolver { entries });

    let state_b = std::sync::Arc::new(tokio::sync::Mutex::new(state.clone()));
    let tracker_b = std::sync::Arc::new(tokio::sync::Mutex::new(
        CommunityRootHlcTracker::default(),
    ));
    let cs_b: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(
        RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(2000)),
    );
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr,
        is_invite_only: false,
        device_id: "b-dev".into(),
        self_owner: admin_addr,
        signing_key: std::sync::Arc::new(
            ed25519_dalek::SigningKey::from_bytes(&[0xa0; 32]),
        ),
        state: std::sync::Arc::clone(&state_b),
        tracker: std::sync::Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(resolver),
        error_tx: Some(degraded_tx),
        delta_tx: None,
    });

    in_tx.send(wire).await.expect("inject wire");

    let report = tokio::time::timeout(std::time::Duration::from_secs(2), degraded_rx.recv())
        .await
        .expect("degraded report within 2s")
        .expect("degraded channel still open");
    assert_eq!(report.reason_tag, "publisher_not_joined");

    let t = tracker_b.lock().await;
    assert!(
        !t.per_device.contains_key(&(alice_addr, "alice-dev".to_string())),
        "tracker MUST NOT have advanced on PublisherNotJoined"
    );

    engine_b.shutdown().await.expect("shutdown");
}
```

Test 3 — unknown-publisher cold cache rejected, then admitted after propagation:

```rust
#[tokio::test]
async fn cold_cache_publish_rejected_then_succeeds_after_propagation() {
    // The same envelope is delivered twice. First time the resolver
    // returns None for alice → UnknownPublisher rejection. Then we
    // swap the resolver out (via a Mutex<HashMap> so the test owns
    // the entries) and re-deliver — the engine accepts.
    use ed25519_dalek::Signer;
    use harmony_app::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use harmony_app::community_state_crdt::CommunityState;
    use harmony_app::community_state_sync::{
        CommunityDegradedReport, CommunityRootHlcTracker, CommunityRootSignedPayload,
        CommunitySyncEngine, CommunitySyncEngineConfig, DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crypto::{
        canonical_cbor_encode, encrypt_blob, encrypt_root_publish,
    };
    use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    // Mutable resolver — entries can be inserted at runtime.
    struct MutableResolver {
        inner: tokio::sync::Mutex<std::collections::HashMap<OwnerAddr, [u8; 64]>>,
    }
    #[async_trait::async_trait]
    impl harmony_app::community_state_sync::IdentityResolver for MutableResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            self.inner.lock().await.get(addr).copied()
        }
    }

    let (out_tx, _out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel(64);
    let (degraded_tx, mut degraded_rx) =
        tokio::sync::mpsc::channel::<CommunityDegradedReport>(8);
    let cas: std::sync::Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>,
        >,
    > = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = std::sync::Arc::clone(&cas);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch {
                    cid,
                    timeout: _,
                    reply,
                } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
            }
        }
    });

    let community_id = SpaceId([9u8; 16]);
    let mk = MembershipKey::new([0xDD; 32]);

    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();
    let alice_signing = ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]);

    // CommunityState with alice Joined.
    let mut state = CommunityState::new(community_id);
    {
        let p = EventPayload {
            id: [1u8; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: alice_addr,
            at: Hlc {
                wall_ms: 10,
                logical: 0,
                device_id: "alice-dev".into(),
            },
        };
        let ev = sign_event_with_identity(&p, &alice).expect("sign");
        let outcome = state.insert_event(
            ev,
            &harmony_app::community_membership::VerifyContext {
                expected_community_id: community_id,
                admin_addr: alice_addr,
                is_invite_only: false,
                actor_identity_pub: &alice_pub,
                countersigner_identity_pub: None,
            },
        );
        assert_eq!(
            outcome,
            harmony_app::community_state_crdt::InsertOutcome::Inserted
        );
    }

    let blob_cleartext = canonical_cbor_encode(&state).expect("encode");
    let blob_ciphertext = encrypt_blob(&mk, &blob_cleartext).expect("encrypt blob");
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("cid");
    cas.lock().await.insert(root_cid, blob_ciphertext);

    let signed = CommunityRootSignedPayload {
        root_cid,
        publisher_addr: alice_addr,
        at: Hlc {
            wall_ms: 200,
            logical: 0,
            device_id: "alice-dev".into(),
        },
    };
    let signed_bytes = canonical_cbor_encode(&signed).expect("encode signed");
    let sig = alice_signing.sign(&signed_bytes).to_bytes();
    let envelope = signed.clone().into_wire(sig);
    let envelope_bytes = canonical_cbor_encode(&envelope).expect("encode env");
    let wire = encrypt_root_publish(&mk, &envelope_bytes).expect("encrypt root");

    let resolver = std::sync::Arc::new(MutableResolver {
        inner: tokio::sync::Mutex::new(std::collections::HashMap::new()),
    });
    let resolver_for_engine: std::sync::Arc<
        dyn harmony_app::community_state_sync::IdentityResolver,
    > = std::sync::Arc::clone(&resolver) as _;

    let state_b = std::sync::Arc::new(tokio::sync::Mutex::new(state));
    let tracker_b = std::sync::Arc::new(tokio::sync::Mutex::new(
        CommunityRootHlcTracker::default(),
    ));
    let cs_b: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(
        RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(2000)),
    );
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let engine_b = CommunitySyncEngine::new(CommunitySyncEngineConfig {
        community_id,
        membership_key: mk,
        admin_addr: alice_addr,
        is_invite_only: false,
        device_id: "b-dev".into(),
        self_owner: alice_addr,
        signing_key: std::sync::Arc::new(
            ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]),
        ),
        state: std::sync::Arc::clone(&state_b),
        tracker: std::sync::Arc::clone(&tracker_b),
        content_store: cs_b,
        publisher_tx: out_tx,
        subscriber_rx: in_rx,
        paths: harmony_app::community_state_sync::PersistPaths {
            crdt: tmp_b.path().join("crdt.cbor"),
            replay: tmp_b.path().join("replay.cbor"),
        },
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        identity_resolver: Some(resolver_for_engine),
        error_tx: Some(degraded_tx),
        delta_tx: None,
    });

    // 1. Cold cache: resolver empty → first delivery rejected.
    in_tx.send(wire.clone()).await.expect("inject 1");
    let report = tokio::time::timeout(std::time::Duration::from_secs(2), degraded_rx.recv())
        .await
        .expect("degraded report within 2s")
        .expect("degraded channel open");
    assert_eq!(report.reason_tag, "publisher_unknown");
    {
        let t = tracker_b.lock().await;
        assert!(
            !t.per_device.contains_key(&(alice_addr, "alice-dev".to_string())),
            "tracker MUST NOT have advanced on UnknownPublisher"
        );
    }

    // 2. Insert alice into resolver — simulating cache propagation.
    resolver.inner.lock().await.insert(alice_addr, alice_pub);

    // 3. Re-deliver the SAME wire packet — should now admit.
    in_tx.send(wire).await.expect("inject 2");
    // Poll for tracker advance with a deterministic 2s bound.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let t = tracker_b.lock().await;
            if t.per_device.contains_key(&(alice_addr, "alice-dev".to_string())) {
                break;
            }
            drop(t);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("tracker should advance within 2s after resolver populated");

    engine_b.shutdown().await.expect("shutdown");
}
```

- [ ] **Step 2: Run the four new tests to verify they fail**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_engine_unit \
    spoofed_publisher_addr_rejected_with_publisher_sig_invalid \
    kicked_member_publish_rejected_with_publisher_not_joined \
    cold_cache_publish_rejected_then_succeeds_after_propagation \
    2>&1 | tail -40
```

Expected: each test fails — current `handle_incoming_publish` doesn't perform the new gates. The engine merges Alice's published state without verification, no degraded report fires, and tests time out waiting for the rejection.

- [ ] **Step 3: Replace `handle_incoming_publish` with the verify-gated pipeline**

In `src-tauri/src/community_state_sync.rs`, replace `handle_incoming_publish` (around line 1163) entirely with:

```rust
async fn handle_incoming_publish(ctx: &InternalCtx, wire: Vec<u8>) -> IncomingOutcome {
    use crate::community_membership::{prior_state_at_event, MemberStatus};
    use crate::owner_state_crypto::canonical_cbor_encode;

    // 1. Decrypt the wire packet (random-nonce + AAD).
    let payload_bytes = match decrypt_root_publish(&ctx.membership_key, &wire) {
        Ok(b) => b,
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::Crypto(e)),
    };

    // 2. Decode CommunityRootPublishPayload.
    let payload: CommunityRootPublishPayload = match canonical_cbor_decode(&payload_bytes) {
        Ok(p) => p,
        Err(e) => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborDecode(e.to_string()))
        }
    };

    // 3. NEW: membership-at-HLC gate. Run BEFORE sig-verify because
    //    a stale-membership rejection is informational and we
    //    shouldn't pay sig-verify cost for a publish we'll reject
    //    anyway. The check is over our locally-trusted state, so
    //    there's no integrity risk in trusting it pre-sig.
    //
    //    Build a synthetic SignedMembershipEvent at the publish HLC
    //    so we can call `prior_state_at_event` and look at the
    //    resulting MaterializedMembership for `publisher_addr`.
    //    `prior_state_at_event` returns the state AS-OF (strictly
    //    less than) the target's sort key. We want the state AT
    //    `publish.at` — i.e., we want to know if `publisher_addr`
    //    was Joined just BEFORE the publish would land. If the
    //    publisher was Joined immediately before the publish, the
    //    publish is admitted; if Banned/Left/Invited/None, rejected.
    {
        let state = ctx.state.lock().await;
        let materialized = crate::community_membership::materialize(
            &state.events.values().cloned().collect::<Vec<_>>(),
            ctx.admin_addr,
        );
        // Probe state at publish.at by treating the local materialized
        // state as the source of truth — we do NOT have the publish's
        // SignedMembershipEvent, so we materialize the full local log
        // and inspect the publisher's status.
        let member_state = materialized.members.get(&payload.publisher_addr).cloned();
        let status_now = member_state.as_ref().map(|s| s.status);
        let is_joined = matches!(status_now, Some(MemberStatus::Joined));
        if !is_joined {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherNotJoined {
                addr: payload.publisher_addr,
                status: status_now.unwrap_or(MemberStatus::Left), // None → treat as Left for diagnostic
                left_at: member_state.and_then(|s| s.left_at),
            });
        }
    }
    // Note on simplification: the spec describes calling
    // prior_state_at_event(publish.at) to materialise state strictly
    // before the publish. That helper takes a target `SignedMembershipEvent`
    // and we don't have one — we have only the publish HLC. The
    // approximation here (materialize the full log, look at current
    // status) is correct for the kicked-member attack: a kicked
    // member's status is Banned in the materialized state, so the
    // gate rejects regardless of when their publish was issued. The
    // edge case is when a Join AND a Kick share the same wall_ms —
    // in that case, the order of arrival in our local log determines
    // status, which is also what materialize(prior...) would do.
    // For ZEB-256's threat model (kicked-but-still-keyed member tries
    // to publish AFTER kick), this is sufficient.

    // 4. NEW: resolve publisher_addr → identity_pub via IdentityResolver.
    let resolver = match ctx.identity_resolver.as_deref() {
        Some(r) => r,
        None => {
            return IncomingOutcome::ErrPreMutation(
                CommunitySyncError::MissingIdentityResolver,
            );
        }
    };
    let publisher_pub = match resolver.resolve(&payload.publisher_addr).await {
        Some(p) => p,
        None => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::UnknownPublisher {
                addr: payload.publisher_addr,
            });
        }
    };

    // 5. NEW: verify Ed25519 signature over canonical CBOR of
    //    CommunityRootSignedPayload::from(&payload). identity_pub is
    //    64 bytes (X25519_pub(32) || Ed25519_pub(32)); the Ed25519
    //    half is the second 32 bytes.
    {
        use ed25519_dalek::Verifier;
        let signed_bytes = match canonical_cbor_encode(&CommunityRootSignedPayload::from(&payload))
        {
            Ok(b) => b,
            Err(e) => {
                return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborEncode(
                    e.to_string(),
                ));
            }
        };
        let mut ed_pub_bytes = [0u8; 32];
        ed_pub_bytes.copy_from_slice(&publisher_pub[32..64]);
        let ed_pub = match ed25519_dalek::VerifyingKey::from_bytes(&ed_pub_bytes) {
            Ok(k) => k,
            Err(_) => {
                return IncomingOutcome::ErrPreMutation(
                    CommunitySyncError::PublisherSigInvalid {
                        addr: payload.publisher_addr,
                    },
                );
            }
        };
        let sig = ed25519_dalek::Signature::from_bytes(&payload.publisher_sig);
        if ed_pub.verify(&signed_bytes, &sig).is_err() {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::PublisherSigInvalid {
                addr: payload.publisher_addr,
            });
        }
    }

    // 6. Replay-protect via per-(addr, device) tracker. Read-only —
    //    record happens at step 10 after CRDT merge succeeds.
    {
        let tracker = ctx.tracker.lock().await;
        if !tracker.would_accept(&payload.publisher_addr, &payload.at) {
            return IncomingOutcome::Duplicate;
        }
    }

    // 7. Fetch the encrypted blob from CAS.
    let blob_ciphertext = match ctx.content_store.get(&payload.root_cid).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::BlobNotFound {
                cid: payload.root_cid,
            });
        }
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::ContentStore(e)),
    };

    // 8. Decrypt + decode the blob.
    let blob_cleartext = match decrypt_blob(&ctx.membership_key, &blob_ciphertext) {
        Ok(b) => b,
        Err(e) => return IncomingOutcome::ErrPreMutation(CommunitySyncError::Crypto(e)),
    };
    let remote: CommunityState = match canonical_cbor_decode(&blob_cleartext) {
        Ok(s) => s,
        Err(e) => {
            return IncomingOutcome::ErrPreMutation(CommunitySyncError::CborDecode(
                e.to_string(),
            ));
        }
    };

    // 8b. Misrouted-blob check — same as before.
    if remote.community_id != ctx.community_id {
        return IncomingOutcome::ErrPreMutation(CommunitySyncError::MisroutedBlob {
            expected: ctx.community_id,
            found: remote.community_id,
        });
    }

    // 9. Advance tracker — single state-mutation point.
    {
        let mut tracker = ctx.tracker.lock().await;
        tracker.record(payload.publisher_addr, payload.at.clone());
    }

    // 10+. Merge events. Identical to Phase 2's logic — Phase A
    //     pre-resolve, Phase B per-event insert under state lock,
    //     Phase C-pre delta emission, Phase C rejection reports.
    //     Suppress duplicate explanation: see existing comments
    //     left in place. (Keep the existing code blocks below
    //     verbatim — only the steps above the merge change.)

    // Phase A:
    let mut events_in_replay_order: Vec<SignedMembershipEvent> =
        remote.events.into_values().collect();
    events_in_replay_order.sort_by(|a, b| {
        crate::community_membership::event_sort_key(a)
            .cmp(&crate::community_membership::event_sort_key(b))
    });

    let mut resolved: Vec<(SignedMembershipEvent, [u8; 64], Option<[u8; 64]>)> = Vec::new();
    for event in events_in_replay_order {
        let actor_pub = match resolver.resolve(&event.actor).await {
            Some(p) => p,
            None => {
                tracing::warn!(
                    community_id = ?ctx.community_id,
                    actor = ?event.actor,
                    "skipping incoming event: unknown actor identity_pub"
                );
                continue;
            }
        };
        let cs_pub: Option<[u8; 64]> = match event.countersig.as_ref() {
            None => None,
            Some(cs) => match resolver.resolve(&cs.signer).await {
                Some(p) => Some(p),
                None => {
                    tracing::warn!(
                        community_id = ?ctx.community_id,
                        signer = ?cs.signer,
                        "skipping incoming event: unknown countersigner identity_pub"
                    );
                    continue;
                }
            },
        };
        resolved.push((event, actor_pub, cs_pub));
    }

    // Phase B:
    let mut inserted_any = false;
    let mut rejection_reports: Vec<crate::community_membership::VerifyError> = Vec::new();
    let mut inserted_events: Vec<SignedMembershipEvent> = Vec::new();
    {
        let mut state = ctx.state.lock().await;
        for (event, actor_pub, cs_pub_owned) in resolved {
            if state.events.contains_key(&event.id) {
                continue;
            }
            let cs_pub_ref: Option<&[u8; 64]> = cs_pub_owned.as_ref();
            let ctx_v = VerifyContext {
                expected_community_id: ctx.community_id,
                admin_addr: ctx.admin_addr,
                is_invite_only: ctx.is_invite_only,
                actor_identity_pub: &actor_pub,
                countersigner_identity_pub: cs_pub_ref,
            };
            let event_clone = event.clone();
            match state.insert_event(event, &ctx_v) {
                InsertOutcome::Inserted => {
                    inserted_any = true;
                    inserted_events.push(event_clone);
                }
                InsertOutcome::AlreadyKnown => {}
                InsertOutcome::Rejected(verr) => {
                    tracing::warn!(
                        community_id = ?ctx.community_id,
                        error = ?verr,
                        "skipping incoming event: verify_event rejected"
                    );
                    rejection_reports.push(verr);
                }
            }
        }
    }

    // Phase C-pre: deltas.
    if let Some(tx) = ctx.delta_tx.as_ref() {
        for event in inserted_events {
            let _ = tx.try_send(CommunityMembershipDelta {
                community_id: ctx.community_id,
                event,
            });
        }
    }

    // Phase C: rejection reports.
    for verr in rejection_reports {
        report_degraded(
            ctx.error_tx.as_ref(),
            ctx.community_id,
            "verify_event_rejected",
            format!("{verr:?}"),
        );
    }

    if inserted_any {
        IncomingOutcome::Mutated
    } else {
        IncomingOutcome::MutatedTrackerOnly
    }
}
```

- [ ] **Step 4: Run the four receive-side tests to verify pass**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_engine_unit \
    spoofed_publisher_addr_rejected_with_publisher_sig_invalid \
    kicked_member_publish_rejected_with_publisher_not_joined \
    cold_cache_publish_rejected_then_succeeds_after_propagation \
    2>&1 | tail -30
```

Expected: all three pass.

- [ ] **Step 5: Run the full engine unit test suite**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_engine_unit 2>&1 | tail -30
```

Expected: all engine unit tests pass — including the existing two-engine tests, which now exercise the full sig-verify pipeline. The dummy `[0x42; 32]` signing keys in those tests are STILL OK because each test's identity_resolver returns the corresponding verifying key (they were already constructed from the same identity bytes).

If existing tests fail because their signing_key doesn't match the resolver's identity_pub, fix them by using the engine's actual `PrivateIdentity`'s signing key. Specifically: replace `signing_key: std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]))` with the test's existing `identity` derived signing key. The harmony-identity crate exposes the secret bytes via `identity.identity.signing_seed()` or similar; if no such accessor exists, derive the signing key from the seed used to construct `PrivateIdentity::from_seed(&seed)` — re-derive the same way `harmony_identity` does internally. Worst case, expose a small test helper.

- [ ] **Step 6: cargo fmt + clippy + full workspace tests**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -30
cd src-tauri && cargo test --workspace --all-targets --locked 2>&1 | tail -20
```

Expected: all green. Phase 3 integration tests (`community_open_flow_integration.rs`) might fail because they don't construct a real signing key — Step 5's fix applies here too.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync_engine_unit.rs
git commit -m "feat(zeb-256): verify publisher membership + identity + signature on receive

handle_incoming_publish gains three gates ahead of the replay-tracker check:
  1. membership-at-HLC: publisher_addr MUST be Joined in our materialized state
  2. identity-resolve: IdentityResolver MUST return Some(identity_pub)
  3. ed25519 verify: publisher_sig MUST validate against publisher's identity_pub

Tracker is NOT advanced on any rejection — that's the censorship-defense
invariant. A kicked-but-still-keyed member can no longer squat HLC slots.

Tracker is now keyed on (publisher_addr, payload.at.device_id), removing the
Task 2 placeholder. Per-addr namespacing is structurally enforced.

Three unit tests pin the rejection paths (sig-spoof, kicked-member,
cold-cache-then-propagation) — each asserts both the reason_tag AND that the
tracker did not advance."
```


---

## Task 7: Plumb `self_owner` + `signing_key` through `CommunityRegistryConfig`

**Why:** Task 4 added the engine-config fields with TEMP placeholders inside `CommunitySyncRegistry::spawn_engine`. This task plumbs the real values through `CommunityRegistryConfig` so production callers (`start_node`, integration tests) supply them once at registry construction. Removes all TEMP markers introduced in Task 4.

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:1555-1586` (CommunityRegistryConfig)
- Modify: `src-tauri/src/community_state_sync.rs:1701-1717` (spawn_engine: replace TEMP placeholders)
- Modify: `src-tauri/tests/community_sync_registry_unit.rs` (update existing config-construction sites)
- Modify: `src-tauri/tests/community_sync_integration.rs` (update spawn_engine setup)
- Modify: `src-tauri/tests/community_open_flow_integration.rs` (Phase 3 tests; minimal config update)

- [ ] **Step 1: Write a failing registry-config test**

In `src-tauri/tests/community_sync_registry_unit.rs`, append:

```rust
#[tokio::test]
async fn registry_propagates_self_owner_and_signing_key_to_engine() {
    use ed25519_dalek::Verifier;
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunityRootPublishPayload, CommunityRootSignedPayload,
        CommunitySyncRegistry, DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crypto::{
        canonical_cbor_decode, canonical_cbor_encode, decrypt_root_publish,
    };
    use harmony_app::owner_state_types::{MembershipKey, OwnerAddr, SpaceId};

    let (cas_op_tx, _cas_op_rx) = tokio::sync::mpsc::channel(8);
    let cs: std::sync::Arc<dyn ContentStore> = std::sync::Arc::new(
        RuntimeContentStore::new(cas_op_tx, std::time::Duration::from_millis(1000)),
    );

    // Always-empty resolver (just enough to satisfy the trait bound).
    struct Empty;
    #[async_trait::async_trait]
    impl harmony_app::community_state_sync::IdentityResolver for Empty {
        async fn resolve(&self, _: &OwnerAddr) -> Option<[u8; 64]> {
            None
        }
    }

    let signing_key = std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0xAB; 32]));
    let verifying_key = signing_key.verifying_key();
    let self_owner = OwnerAddr([0x42; 16]);

    let tmp = tempfile::tempdir().expect("tempdir");
    let registry = CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "registry-test-dev".into(),
        content_store: cs,
        identity_resolver: std::sync::Arc::new(Empty),
        identity_dir: tmp.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner,
        signing_key,
    });

    // Spawn an engine; its publish should embed self_owner and a sig
    // that validates against verifying_key.
    let community_id = SpaceId([5u8; 16]);
    let mk = MembershipKey::new([0x01; 32]);
    let admin = self_owner;
    let (pub_tx, mut pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (_sub_tx, sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    registry
        .spawn_engine(community_id, mk.clone(), admin, false, pub_tx, sub_rx)
        .await
        .expect("spawn_engine");

    registry
        .flush_now(&community_id)
        .await
        .expect("flush_now");

    let wire = pub_rx.recv().await.expect("publisher_tx received one packet");
    let payload_bytes = decrypt_root_publish(&mk, &wire).expect("decrypt");
    let payload: CommunityRootPublishPayload =
        canonical_cbor_decode(&payload_bytes).expect("decode");
    assert_eq!(payload.publisher_addr, self_owner);
    let signed_bytes =
        canonical_cbor_encode(&CommunityRootSignedPayload::from(&payload)).expect("encode");
    let sig = ed25519_dalek::Signature::from_bytes(&payload.publisher_sig);
    verifying_key
        .verify(&signed_bytes, &sig)
        .expect("registry-spawned engine sig must validate");

    registry.shutdown_all().await.expect("shutdown");
}
```

- [ ] **Step 2: Run the test to verify it fails to compile**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_registry_unit registry_propagates_self_owner_and_signing_key_to_engine 2>&1 | tail -20
```

Expected: compilation error — `self_owner` / `signing_key` are not fields of `CommunityRegistryConfig`.

- [ ] **Step 3: Add fields to `CommunityRegistryConfig`**

In `src-tauri/src/community_state_sync.rs`, find `CommunityRegistryConfig` (around line 1555) and append two fields:

```rust
    /// Owner address of the local member. Cloned into every engine's
    /// `CommunitySyncEngineConfig.self_owner`. Stable across all
    /// communities for a single node — one identity, one address.
    pub self_owner: OwnerAddr,

    /// Local Ed25519 signing key, shared across every spawned engine.
    /// Wrapped in `Arc` so engine spawns are cheap (Arc bump, no
    /// secret-byte copy). Sourced from the local `PrivateIdentity` at
    /// `start_node` time; identical handle to the one Phase 3's
    /// `insert_local_event` uses for membership-event signing.
    pub signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
```

- [ ] **Step 4: Update `spawn_engine` to use the config values**

Replace the two TEMP lines from Task 4 in `spawn_engine` (around line 1701) with:

```rust
        let engine = Arc::new(CommunitySyncEngine::new(CommunitySyncEngineConfig {
            community_id,
            membership_key,
            admin_addr,
            is_invite_only,
            device_id: self.cfg.device_id.clone(),
            self_owner: self.cfg.self_owner,
            signing_key: Arc::clone(&self.cfg.signing_key),
            state,
            tracker,
            content_store: Arc::clone(&self.cfg.content_store),
            publisher_tx,
            subscriber_rx,
            paths,
            debounce_ms: self.cfg.debounce_ms,
            identity_resolver: Some(Arc::clone(&self.cfg.identity_resolver)),
            error_tx: self.cfg.error_tx.clone(),
            delta_tx: self.cfg.delta_tx.clone(),
        }));
```

Verify by greppping for "TEMP for Task 4" — it should yield zero matches:

```bash
set -o pipefail
grep -nR "TEMP for Task 4" src-tauri/src 2>&1 || echo "no TEMP markers found (expected)"
```

- [ ] **Step 5: Update existing test sites in `community_sync_registry_unit.rs`**

Existing registry tests construct a `CommunityRegistryConfig`. Add `self_owner: OwnerAddr([0x01; 16])` and `signing_key: std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]))` to each. The exact addr/seed values don't matter for tests that don't validate the wire signature (just need to compile).

For tests that DO require the resolver to map `self_owner` → identity_pub (e.g., a two-engine bridge), either:
- (a) build the resolver from the same seed used for the signing key, OR
- (b) skip the receive-side path in that test (publisher-only).

- [ ] **Step 6: Update `community_sync_integration.rs`**

Same pattern — every `CommunityRegistryConfig::new(...)` call gains the two new fields. The existing two-engine bridge tests use `PrivateIdentity::from_seed(&[0xa1; 32])` as the admin identity. For the registry config: `self_owner: <admin's OwnerAddr>` and `signing_key: <Arc<SigningKey> derived from the same seed>`. Search for how the test currently provides identity bytes:

```bash
set -o pipefail
grep -n "PrivateIdentity::from_seed\|identity.signing_seed\|SigningKey::from_bytes" \
    src-tauri/tests/community_sync_integration.rs 2>&1 | head -20
```

Use the same byte source. If `PrivateIdentity` doesn't expose a `signing_key()` accessor, the test can construct an `ed25519_dalek::SigningKey` directly from the seed bytes — `harmony_identity::PrivateIdentity::from_seed` derives its Ed25519 key the same way internally (HKDF-then-bytes); confirm by reading `harmony-identity/src/identity.rs`. Worst case, add a `pub(crate) fn signing_key(&self) -> &ed25519_dalek::SigningKey` accessor on `PrivateIdentity` in a small parallel commit and use it from the tests.

- [ ] **Step 7: Update Phase 3 IPC tests (`community_open_flow_integration.rs`)**

Same mechanical update — add the two new fields to the `CommunityRegistryConfig` site (likely one site in the test setup helper). Use the test's existing identity-derived signing key.

- [ ] **Step 8: Run the new registry test**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_registry_unit registry_propagates_self_owner_and_signing_key_to_engine 2>&1 | tail -20
```

Expected: pass.

- [ ] **Step 9: cargo fmt + clippy + full workspace tests**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -30
cd src-tauri && cargo test --workspace --all-targets --locked 2>&1 | tail -20
```

Expected: clippy + tests almost green. The `lib.rs` `start_node` registry config also needs the new mandatory fields. Both `self_owner` and `signing_key_arc` are already in scope at the construction site (`src-tauri/src/lib.rs:1192`, defined earlier around lines 1083-1101 alongside the `DmOutbox::new` call). Add both fields using the in-scope values — NO TEMP placeholder is needed:

```rust
                        let cfg = crate::community_state_sync::CommunityRegistryConfig {
                            device_id: device_id.clone(),
                            content_store: std::sync::Arc::clone(&content_store),
                            identity_resolver: resolver,
                            identity_dir: identity_dir.clone(),
                            debounce_ms: crate::community_state_sync::DEFAULT_DEBOUNCE_MS,
                            error_tx: Some(community_degraded_tx),
                            delta_tx: Some(community_delta_tx.clone()),
                            self_owner,
                            signing_key: std::sync::Arc::clone(&signing_key_arc),
                        };
```

Re-run cargo fmt + clippy + tests after this edit; everything should now be green.

- [ ] **Step 10: Final verification + commit**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -30
cd src-tauri && cargo test --workspace --all-targets --locked 2>&1 | tail -20
```

Expected: all green.

```bash
git add src-tauri/src/community_state_sync.rs \
        src-tauri/src/lib.rs \
        src-tauri/tests/community_sync_registry_unit.rs \
        src-tauri/tests/community_sync_integration.rs \
        src-tauri/tests/community_open_flow_integration.rs
git commit -m "feat(zeb-256): plumb self_owner + signing_key through CommunityRegistryConfig

CommunityRegistryConfig gains the two fields the engine config needs;
spawn_engine clones them into every per-engine CommunitySyncEngineConfig.

start_node populates them from the existing in-scope self_owner +
signing_key_arc snapshotted at engine-spawn time — same values DmOutbox
already uses for DM signing.

All TEMP placeholders from Task 4's spawn_engine are removed."
```


---

## Task 8: Integration test — `spoofed_publish_does_not_block_real_publisher`

**Why:** Per the spec § 8 acceptance criterion: "Spoofing test demonstrates the censorship attack is no longer possible." This integration test exercises the full two-engine receive pipeline through `community_sync_integration.rs`'s existing bridge: Engine A publishes legitimately, an attacker-injected forged publish claiming Alice's `publisher_addr` arrives at Engine B, B rejects it without advancing Alice's tracker entry, then A's NEXT legitimate publish admits successfully. This is the "Bob can't censor Alice" round-trip.

**Files:**
- Modify: `src-tauri/tests/community_sync_integration.rs` (append one new test fn)

- [ ] **Step 1: Write the failing integration test**

Append to `src-tauri/tests/community_sync_integration.rs`:

```rust
#[tokio::test]
async fn spoofed_publish_does_not_block_real_publisher() {
    use ed25519_dalek::Signer;
    use harmony_app::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use harmony_app::community_state_crdt::CommunityState;
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunityRootSignedPayload, CommunitySyncRegistry,
        DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_crypto::{
        canonical_cbor_encode, encrypt_blob, encrypt_root_publish,
    };
    use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();
    let alice_signing = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]));

    let bob = PrivateIdentity::from_seed(&[0xb1; 32]);
    let bob_addr = OwnerAddr(bob.identity.address_hash);
    let bob_pub = bob.identity.to_public_bytes();
    let bob_signing = ed25519_dalek::SigningKey::from_bytes(&[0xb1; 32]);

    let community_id = SpaceId([0xCC; 16]);
    let mk = MembershipKey::new([0x42; 32]);

    // Shared CAS — both engines hit it.
    let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel(64);
    let cas_map: Arc<Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = Arc::clone(&cas_map);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch {
                    cid,
                    timeout: _,
                    reply,
                } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
            }
        }
    });
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(2000),
    ));

    // Resolver: knows alice + bob.
    struct TwoIdResolver {
        alice_addr: OwnerAddr,
        alice_pub: [u8; 64],
        bob_addr: OwnerAddr,
        bob_pub: [u8; 64],
    }
    #[async_trait::async_trait]
    impl harmony_app::community_state_sync::IdentityResolver for TwoIdResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.alice_addr {
                Some(self.alice_pub)
            } else if *addr == self.bob_addr {
                Some(self.bob_pub)
            } else {
                None
            }
        }
    }
    let resolver: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::new(TwoIdResolver {
            alice_addr,
            alice_pub,
            bob_addr,
            bob_pub,
        });

    let tmp_a = tempfile::tempdir().expect("tempdir a");
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    // Engine A: Alice's perspective.
    let registry_a = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "a-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: tmp_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_signing),
    }));
    // Engine B: Alice's other device (or any peer); receives both
    // legitimate and forged publishes for the same community.
    let registry_b = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: tmp_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        // B is also Alice (multi-device) for this test — it doesn't
        // publish, just receives. self_owner just needs to compile.
        self_owner: alice_addr,
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0xc1; 32])),
    }));

    // Wire A's outbox to B's inbox via mpsc bridge.
    let (a_pub_tx, mut a_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (_a_sub_tx, a_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (b_pub_tx, _b_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (b_sub_tx, b_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let b_sub_tx_clone = b_sub_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_pub_rx.recv().await {
            let _ = b_sub_tx_clone.send(bytes).await;
        }
    });

    registry_a
        .spawn_engine(community_id, mk.clone(), alice_addr, false, a_pub_tx, a_sub_rx)
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine(community_id, mk.clone(), alice_addr, false, b_pub_tx, b_sub_rx)
        .await
        .expect("spawn b");

    // Seed both engines with Alice's bootstrap Join so the
    // membership-at-HLC gate passes when she publishes.
    let join_payload = EventPayload {
        id: [1u8; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: alice_addr,
        at: Hlc {
            wall_ms: 10,
            logical: 0,
            device_id: "a-dev".into(),
        },
    };
    let join_event = sign_event_with_identity(&join_payload, &alice).expect("sign");
    let engine_a = registry_a
        .engine_arc(&community_id)
        .await
        .expect("engine a");
    let engine_b = registry_b
        .engine_arc(&community_id)
        .await
        .expect("engine b");
    let _ = engine_a
        .insert_local_event(join_event.clone())
        .await
        .expect("a insert join");
    // Wait for B to merge Alice's Join.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let s = registry_b.state_for(&community_id).await.expect("state b");
            if s.lock().await.events.len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("B should merge Alice's bootstrap Join within 2s");

    // ── Phase 1: Alice publishes legitimately at HLC 100 ──
    registry_a
        .flush_now(&community_id)
        .await
        .expect("flush a 100");
    // Wait for B's tracker to record Alice's slot at the latest HLC.
    let alice_slot_after_first = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            // Reach into B's tracker. Need read access — for this test
            // expose via registry's #[doc(hidden)] tracker_for accessor;
            // if not present, add one parallel to state_for.
            // (See Step 2 below.)
            if let Some(hlc) =
                registry_b.tracker_snapshot(&community_id).await.and_then(|t| {
                    t.per_device.get(&(alice_addr, "a-dev".to_string())).cloned()
                })
            {
                break hlc;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("B's tracker must record Alice within 2s");

    // ── Phase 2: Forged publish from Bob claiming alice_addr ──
    // Build a CommunityState the receiver will fetch. Use the same
    // alice-Join state for simplicity.
    let mut state = CommunityState::new(community_id);
    let outcome = state.insert_event(
        join_event,
        &harmony_app::community_membership::VerifyContext {
            expected_community_id: community_id,
            admin_addr: alice_addr,
            is_invite_only: false,
            actor_identity_pub: &alice_pub,
            countersigner_identity_pub: None,
        },
    );
    assert_eq!(
        outcome,
        harmony_app::community_state_crdt::InsertOutcome::Inserted
    );
    let blob_cleartext = canonical_cbor_encode(&state).expect("encode state");
    let blob_ciphertext = encrypt_blob(&mk, &blob_cleartext).expect("encrypt blob");
    let root_cid = harmony_content::cid::ContentId::for_book(
        &blob_ciphertext,
        harmony_content::cid::ContentFlags {
            encrypted: true,
            ..Default::default()
        },
    )
    .expect("cid");
    cas_map.lock().await.insert(root_cid, blob_ciphertext);

    let forged = CommunityRootSignedPayload {
        root_cid,
        publisher_addr: alice_addr,
        at: Hlc {
            // Bob picks an arbitrarily high HLC to try to censor
            // Alice's future publishes via tracker squat.
            wall_ms: 1_000_000,
            logical: 0,
            device_id: "a-dev".into(),
        },
    };
    let forged_signed_bytes = canonical_cbor_encode(&forged).expect("encode forged");
    let forged_sig = bob_signing.sign(&forged_signed_bytes).to_bytes();
    let forged_envelope = forged.into_wire(forged_sig);
    let forged_envelope_bytes = canonical_cbor_encode(&forged_envelope).expect("encode env");
    let forged_wire = encrypt_root_publish(&mk, &forged_envelope_bytes).expect("encrypt root");
    b_sub_tx.send(forged_wire).await.expect("inject forged");

    // Give B a brief window to process the forged publish. Unlike the
    // unit tests, we don't have a degraded_rx channel here — we
    // verify the rejection by observing that B's tracker for Alice
    // does NOT advance to wall_ms=1_000_000.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let snap_after_forged = registry_b
        .tracker_snapshot(&community_id)
        .await
        .expect("tracker b");
    let alice_slot_after_forged = snap_after_forged
        .per_device
        .get(&(alice_addr, "a-dev".to_string()))
        .cloned()
        .expect("alice slot still present");
    assert_eq!(
        alice_slot_after_forged.wall_ms, alice_slot_after_first.wall_ms,
        "forged publish MUST NOT advance Alice's tracker slot"
    );

    // ── Phase 3: Alice publishes legitimately again ──
    // The legitimate publish has wall_ms = SystemTime::now() which is
    // far less than 1_000_000_000_000 (the forged wall) but far
    // greater than alice_slot_after_first. Engine A's next_hlc
    // increments past her own previous publish; the receiver admits.
    registry_a
        .flush_now(&community_id)
        .await
        .expect("flush a second");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snap = registry_b
                .tracker_snapshot(&community_id)
                .await
                .expect("tracker b loop");
            if let Some(hlc) = snap.per_device.get(&(alice_addr, "a-dev".to_string())) {
                if hlc.wall_ms > alice_slot_after_first.wall_ms {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("B should advance Alice's slot on her second legitimate publish");

    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}
```

- [ ] **Step 2: Add `tracker_snapshot` accessor on `CommunitySyncRegistry`**

The integration test peeks into B's tracker. Add a `#[doc(hidden)]` accessor on `CommunitySyncRegistry` parallel to `state_for`:

In `src-tauri/src/community_state_sync.rs`, after `state_for` (around line 1787):

```rust
    /// Snapshot the engine's `CommunityRootHlcTracker` for read-only
    /// inspection. **Test-only** — production callers don't need
    /// direct tracker access. Returns `None` if no engine is spawned.
    #[doc(hidden)]
    pub async fn tracker_snapshot(
        &self,
        community_id: &SpaceId,
    ) -> Option<CommunityRootHlcTracker> {
        let engine = self.engines.lock().await.get(community_id).cloned()?;
        Some(engine.tracker_arc().lock().await.clone())
    }
```

Add the `tracker_arc` accessor on `CommunitySyncEngine` (around the existing `state` accessor inside the impl block, line ~675ish):

```rust
    /// Test-only accessor for the engine's `CommunityRootHlcTracker`
    /// `Arc<Mutex<...>>`. Mirrors `state` — exposes the shared lock
    /// so tests can observe per-(addr, device) entries without
    /// reaching into private fields.
    #[doc(hidden)]
    pub fn tracker_arc(&self) -> Arc<Mutex<CommunityRootHlcTracker>> {
        Arc::clone(&self.tracker)
    }
```

This requires `CommunitySyncEngine` to retain a `tracker: Arc<Mutex<CommunityRootHlcTracker>>` field. Currently the engine struct does NOT keep one — the tracker only lives in `InternalCtx`. Add the field at the same place `state` is held (around line 474):

```rust
    /// Shared replay tracker handle. Retained on the engine so the
    /// registry can expose it via `tracker_snapshot` for test-only
    /// inspection without reaching into `InternalCtx`.
    tracker: Arc<Mutex<CommunityRootHlcTracker>>,
```

And in `CommunitySyncEngine::new`, populate it BEFORE the `cfg.tracker` move:

```rust
        let state_for_engine = Arc::clone(&cfg.state);
        let tracker_for_engine = Arc::clone(&cfg.tracker);    // NEW
        let community_id_for_engine = cfg.community_id;
        // ... (existing code)
```

And in the struct construction at the bottom of `new`:

```rust
        Self {
            notify_dirty,
            has_pending_dirty,
            flush_now_tx,
            shutdown_tx,
            task: Mutex::new(Some(task)),
            state: state_for_engine,
            tracker: tracker_for_engine,    // NEW
            community_id: community_id_for_engine,
            // ... rest unchanged
        }
```

- [ ] **Step 3: Run the integration test**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_integration spoofed_publish_does_not_block_real_publisher 2>&1 | tail -40
```

Expected: pass.

- [ ] **Step 4: cargo fmt + clippy + full workspace tests**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -30
cd src-tauri && cargo test --workspace --all-targets --locked 2>&1 | tail -20
```

Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/tests/community_sync_integration.rs
git commit -m "test(zeb-256): integration test demonstrates spoofing attack is blocked

Two-registry bridge: Alice publishes at HLC X, Bob injects a forged publish
claiming alice_addr at HLC X+huge with Bob's signing key, Alice publishes
again at HLC Y (X < Y < X+huge). Receiver MUST admit Alice's second publish.

Pins the spec § 11 acceptance criterion: \"Spoofing test demonstrates the
censorship attack is no longer possible.\"

Adds tracker_snapshot test-only accessor on CommunitySyncRegistry +
tracker_arc on CommunitySyncEngine so the test can observe per-(addr, device)
state without reaching into private fields."
```


---

## Task 9: Integration test — re-Join after Leave admits new publishes

**Why:** Per the spec § "Tracker reset on Leave" reasoning: tracker entries are NOT pruned when a member Leaves, but a re-Join must NOT be blocked by the stale entry. Because a legitimate re-Join's publish HLC is strictly later than the pre-Leave entry (wall_ms moves forward over real time + the publisher's own `next_hlc` enforces strict-newer per the publisher's own tracker), `would_accept` admits the new publish naturally. This test pins that invariant against future regressions — particularly someone "fixing" the tracker by clearing entries on Leave (which would re-open the censorship gap if a Leaving member raced a Joining peer).

**Files:**
- Modify: `src-tauri/tests/community_sync_integration.rs` (append one new test fn)

- [ ] **Step 1: Write the failing integration test**

Append to `src-tauri/tests/community_sync_integration.rs`:

```rust
#[tokio::test]
async fn re_joined_member_publish_admitted_after_leave() {
    // Sequence:
    //   1. Alice Joins, publishes (tracker[(alice, a-dev)] = HLC1).
    //   2. Alice Leaves; the tracker is NOT pruned.
    //   3. Alice re-Joins later (Phase 4 invite-only is post-ZEB-256;
    //      this test uses the open-Join path so it's runnable today).
    //   4. Alice publishes again (tracker[(alice, a-dev)] would-accept
    //      because HLC2 > HLC1).
    use harmony_app::community_membership::{
        sign_event_with_identity, EventPayload, MembershipEventKind,
    };
    use harmony_app::community_state_sync::{
        CommunityRegistryConfig, CommunitySyncRegistry, DEFAULT_DEBOUNCE_MS,
    };
    use harmony_app::content_store::{ContentStore, RuntimeContentStore};
    use harmony_app::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    let alice = PrivateIdentity::from_seed(&[0xa1; 32]);
    let alice_addr = OwnerAddr(alice.identity.address_hash);
    let alice_pub = alice.identity.to_public_bytes();
    let alice_signing = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]));

    // Resolver knows alice only.
    struct OneIdResolver {
        addr: OwnerAddr,
        pubk: [u8; 64],
    }
    #[async_trait::async_trait]
    impl harmony_app::community_state_sync::IdentityResolver for OneIdResolver {
        async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
            if *addr == self.addr {
                Some(self.pubk)
            } else {
                None
            }
        }
    }
    let resolver: Arc<dyn harmony_app::community_state_sync::IdentityResolver> =
        Arc::new(OneIdResolver {
            addr: alice_addr,
            pubk: alice_pub,
        });

    let community_id = SpaceId([0x77; 16]);
    let mk = MembershipKey::new([0x55; 32]);

    // CAS plumbing identical to Task 8's helper. Re-stated verbatim.
    let (cas_op_tx, mut cas_op_rx) = tokio::sync::mpsc::channel(64);
    let cas_map: Arc<Mutex<std::collections::HashMap<harmony_content::cid::ContentId, Vec<u8>>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let cas_for_servicer = Arc::clone(&cas_map);
    tokio::spawn(async move {
        use harmony_app::content_store::CasOp;
        while let Some(op) = cas_op_rx.recv().await {
            match op {
                CasOp::PutLocal { cid, blob, reply } => {
                    cas_for_servicer.lock().await.insert(cid, blob);
                    if let Some(r) = reply {
                        let _ = r.send(Ok(()));
                    }
                }
                CasOp::GetOrFetch {
                    cid,
                    timeout: _,
                    reply,
                } => {
                    let v = cas_for_servicer.lock().await.get(&cid).cloned();
                    let _ = reply.send(Ok(v));
                }
            }
        }
    });
    let cs: Arc<dyn ContentStore> = Arc::new(RuntimeContentStore::new(
        cas_op_tx,
        Duration::from_millis(2000),
    ));

    let tmp_a = tempfile::tempdir().expect("tempdir a");
    let tmp_b = tempfile::tempdir().expect("tempdir b");

    let registry_a = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "a-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: tmp_a.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: alice_addr,
        signing_key: Arc::clone(&alice_signing),
    }));
    let registry_b = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "b-dev".into(),
        content_store: Arc::clone(&cs),
        identity_resolver: Arc::clone(&resolver),
        identity_dir: tmp_b.path().to_path_buf(),
        debounce_ms: DEFAULT_DEBOUNCE_MS,
        error_tx: None,
        delta_tx: None,
        self_owner: alice_addr,
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0xc1; 32])),
    }));

    let (a_pub_tx, mut a_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (_a_sub_tx, a_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (b_pub_tx, _b_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let (b_sub_tx, b_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let b_sub_tx_clone = b_sub_tx.clone();
    tokio::spawn(async move {
        while let Some(bytes) = a_pub_rx.recv().await {
            let _ = b_sub_tx_clone.send(bytes).await;
        }
    });

    registry_a
        .spawn_engine(community_id, mk.clone(), alice_addr, false, a_pub_tx, a_sub_rx)
        .await
        .expect("spawn a");
    registry_b
        .spawn_engine(community_id, mk.clone(), alice_addr, false, b_pub_tx, b_sub_rx)
        .await
        .expect("spawn b");

    let engine_a = registry_a
        .engine_arc(&community_id)
        .await
        .expect("engine a");

    // Helper to insert + sign an event into engine_a.
    let mk_event = |id: [u8; 16], kind: MembershipEventKind, wall: u64| {
        let p = EventPayload {
            id,
            community_id,
            kind,
            actor: alice_addr,
            at: Hlc {
                wall_ms: wall,
                logical: 0,
                device_id: "a-dev".into(),
            },
        };
        sign_event_with_identity(&p, &alice).expect("sign")
    };

    // 1. Join + publish.
    engine_a
        .insert_local_event(mk_event([1u8; 16], MembershipEventKind::Join, 100))
        .await
        .expect("a join1");
    registry_a
        .flush_now(&community_id)
        .await
        .expect("flush 1");
    let first_slot = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snap = registry_b
                .tracker_snapshot(&community_id)
                .await
                .expect("tracker b");
            if let Some(hlc) = snap.per_device.get(&(alice_addr, "a-dev".to_string())) {
                break hlc.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("first slot recorded within 2s");

    // 2. Leave + publish (the leave gets propagated, but next legit
    //    publish from alice is post-leave so should be rejected by
    //    the membership-at-HLC gate).
    engine_a
        .insert_local_event(mk_event([2u8; 16], MembershipEventKind::Leave, 200))
        .await
        .expect("a leave");
    registry_a
        .flush_now(&community_id)
        .await
        .expect("flush 2");
    // Wait for B to merge the Leave.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let s = registry_b.state_for(&community_id).await.expect("state b");
            if s.lock().await.events.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("leave merged within 2s");

    // 3. Re-Join.
    engine_a
        .insert_local_event(mk_event([3u8; 16], MembershipEventKind::Join, 300))
        .await
        .expect("a re-join");
    registry_a
        .flush_now(&community_id)
        .await
        .expect("flush 3");

    // 4. Verify B's tracker advances PAST `first_slot` — i.e. the
    //    re-Join's accompanying publish was admitted, not blocked
    //    by the stale entry.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snap = registry_b
                .tracker_snapshot(&community_id)
                .await
                .expect("tracker b loop");
            if let Some(hlc) = snap.per_device.get(&(alice_addr, "a-dev".to_string())) {
                if hlc.is_strictly_newer_than(&first_slot) {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("B's tracker should advance past first_slot after re-Join");

    // Confirm alice's status is Joined again (re-Join took effect).
    let s = registry_b.state_for(&community_id).await.expect("state b");
    let materialized = harmony_app::community_membership::materialize(
        &s.lock().await.events.values().cloned().collect::<Vec<_>>(),
        alice_addr,
    );
    let alice_state = materialized.members.get(&alice_addr).expect("alice present");
    assert_eq!(
        alice_state.status,
        harmony_app::community_membership::MemberStatus::Joined,
        "alice should be Joined after re-Join"
    );

    registry_a.shutdown_all().await.expect("shutdown a");
    registry_b.shutdown_all().await.expect("shutdown b");
}
```

- [ ] **Step 2: Run the new test to verify pass**

```bash
set -o pipefail
cd src-tauri && cargo test --test community_sync_integration re_joined_member_publish_admitted_after_leave 2>&1 | tail -40
```

Expected: pass. The test exercises Alice's `next_hlc` producing strictly-newer values across each publish, so the tracker on B admits each one without needing tracker pruning.

- [ ] **Step 3: cargo fmt + clippy + full workspace**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -30
cd src-tauri && cargo test --workspace --all-targets --locked 2>&1 | tail -20
```

Expected: all green.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/tests/community_sync_integration.rs
git commit -m "test(zeb-256): re-Join after Leave is admitted by stale tracker entry

Pins the invariant: tracker entries are NOT pruned on Leave (so a kicked
member can't re-issue old publishes after rejoin via tracker reset), but a
legitimate re-Join's strictly-newer HLC naturally satisfies would_accept.

Defends against a future \"fix\" that clears tracker entries on Leave —
which would re-open the censorship gap because a malicious peer racing a
re-Join could still spoof the slot."
```


---

## Task 10: Final verification — push branch + open PR

**Why:** Last gate. Runs the full workspace test suite under `--locked` to catch dependency drift, double-checks every spec acceptance criterion is observable in code, then pushes the branch and opens a PR with body that links the spec PR + Phase 3 ship commit + Phase 4 unblock context.

**Files:** None modified. Git + GitHub operations only.

- [ ] **Step 1: Verify clean working tree**

```bash
git status
```

Expected: `nothing to commit, working tree clean`. If anything is uncommitted, address it before pushing.

- [ ] **Step 2: Verify branch lineage**

```bash
git fetch origin main
git log --oneline main..HEAD | head -20
git merge-base HEAD origin/main
```

Expected: every commit since `main` is a Task-N commit; `merge-base` returns a commit on `origin/main` (proving the branch hasn't diverged).

- [ ] **Step 3: Run the full workspace test suite under `--locked`**

```bash
set -o pipefail
cd src-tauri && cargo fmt --all -- --check 2>&1 | tail -10
cd src-tauri && cargo clippy --all-targets --all-features --locked -- -D warnings 2>&1 | tail -30
cd src-tauri && cargo test --workspace --all-targets --all-features --locked 2>&1 | tail -30
```

Expected: clean fmt + clean clippy + all tests pass. The `--locked` flag catches any `Cargo.lock` drift introduced by ed25519_dalek already-present dep usage.

- [ ] **Step 4: Verify each acceptance criterion is observable**

Walk through each bullet of the spec § 11 ("Acceptance criteria") and point to a concrete file:line range or test name. Example:

```
[x] CommunityRootPublishPayload carries publisher_addr + publisher_sig
    → src-tauri/src/community_state_sync.rs:208-220 (Task 1)
[x] CommunityRootHlcTracker.per_device keys on (OwnerAddr, String)
    → src-tauri/src/community_state_sync.rs:300-344 (Task 2)
[x] handle_incoming_publish runs the three new gates BEFORE replay
    → src-tauri/src/community_state_sync.rs (Task 6, steps 3-5 of pipeline)
[x] CommunitySyncEngineConfig + CommunityRegistryConfig expose the fields
    → Task 4 + Task 7
[x] All 3 new CommunitySyncError variants exist with distinct reason_tag
    → src-tauri/tests/community_sync_engine_unit.rs::classify_incoming_error_covers_publisher_auth_variants
[x] All 5 new tests pass
    → publish_carries_valid_publisher_sig
    → spoofed_publisher_addr_rejected_with_publisher_sig_invalid
    → kicked_member_publish_rejected_with_publisher_not_joined
    → cold_cache_publish_rejected_then_succeeds_after_propagation
    → spoofed_publish_does_not_block_real_publisher
    (plus the bonus re_joined_member_publish_admitted_after_leave)
[x] Wire-format fixture regenerated and pinned
    → src-tauri/tests/wire_format_community_sync_fixtures.rs (Task 1)
[x] All gates green
    → step 3 above
[x] Spoofing test demonstrates censorship is no longer possible
    → spoofed_publish_does_not_block_real_publisher (Task 8)
```

If any box can't be checked, STOP — escalate to the human. Don't push a partial implementation.

- [ ] **Step 5: Push the branch**

```bash
git push -u origin zeb-256-publisher-authentication
```

Expected: push succeeds; remote tracking branch is set.

- [ ] **Step 6: Open the PR**

```bash
gh pr create --title "feat(zeb-256): cryptographic publisher authentication for community state-root publishes" \
    --body "$(cat <<'EOF'
## Summary

Closes [ZEB-256](https://linear.app/zeblith/issue/ZEB-256/) — the censorship gap before Phase 4 invite-only flows ship. Phase 2 envelopes were authenticated only by the shared `MembershipKey`, so any community member could spoof another member's `device_id` in `at.device_id` and silently squat their HLC slot — censoring future publishes from the real publisher.

ZEB-256 closes the gap by signing every publish with the publisher's local Ed25519 device key, mirroring the per-event `SignedMembershipEvent` shape:

- `CommunityRootPublishPayload` gains `publisher_addr` + `publisher_sig` over a separate `CommunityRootSignedPayload` sub-payload (canonical-CBOR signed bytes are unambiguous).
- The replay tracker re-keys from `BTreeMap<String, Hlc>` to `BTreeMap<(OwnerAddr, String), Hlc>` — cross-addr `device_id` collisions are structurally impossible.
- `handle_incoming_publish` adds three gates BEFORE the replay-tracker check, in cheapest-first order: membership-at-HLC, identity_pub resolution, Ed25519 verify. **Tracker is NOT advanced on any rejection** — that's the censorship-defense invariant.
- Cold-cache rejections are transient soft-fails (next publish after cache propagation succeeds) — same UX as the existing `OwnerDeviceCache` propagation for membership events.

## Spec

[`docs/specs/2026-05-07-zeb-256-publisher-authentication-design.md`](../blob/main/docs/specs/2026-05-07-zeb-256-publisher-authentication-design.md) — design approved 2026-05-07.

## Phase context

- Spec PR: #88 (commit `fdf7ba0`)
- Phase 3 (open community flow) shipped at `bc0facd` (PR #87)
- After ZEB-256 ships, **Phase 4** of ZEB-217 Sub-C (invite-only flow + kick semantics) is unblocked — kicked members lose write capability via this work even before [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/) (key rotation on kick) closes the read gap.

## Test plan

- [x] All 5 acceptance-criterion tests pass (4 unit + 1 integration extension; see commit messages for the test→criterion mapping).
- [x] Bonus test pins re-Join-after-Leave invariant (tracker doesn't need pruning).
- [x] Wire-format fixture regenerated and pinned.
- [x] `cargo fmt --all -- --check` clean.
- [x] `cargo clippy --all-targets --all-features --locked -- -D warnings` clean.
- [x] `cargo test --workspace --all-targets --all-features --locked` all pass.

## Breaking changes

- `CommunityRootPublishPayload` wire format changes shape — old payloads fail to decode. Acceptable: Phase 2 has no production deployments.
- `CommunityRootHlcTracker.per_device` persisted shape breaks. `load_replay`'s existing quarantine-and-default self-heal handles old files; pinned by a new persist unit test.
- `CommunitySyncEngineConfig` + `CommunityRegistryConfig` gain `self_owner` + `signing_key` — internal API; all call sites updated in this PR.

## Out of scope

- ZEB-249 (`MembershipKey` rotation on kick) — read-side fix; complementary but independent. Filed under [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/).
EOF
)"
```

- [ ] **Step 7: Verify PR opened cleanly**

```bash
gh pr view --json number,title,headRefName,baseRefName,isDraft
```

Expected: PR number assigned; `baseRefName: main`; `isDraft: false`. The PR body links to the spec, Phase 3 ship, and ZEB-249 follow-up.

- [ ] **Step 8: STOP — wait for human merge**

Do NOT merge the PR. The implementation handoff is complete; the human reviews bot feedback, decides on iteration, and merges when ready.

