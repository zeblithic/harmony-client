# ZEB-248 Phase 1 — Channel-config CRDT Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend Sub-C v1's per-community state-CRDT with channel-config events (`ChannelCreate`, `ChannelModify`, `ChannelDelete`) so admins can carve communities into named channels — wire format, materialization, verify gates, IPCs, default-`#general` auto-creation, two-engine integration test. Backend-only — no UI, no message storage.

**Architecture:** Three new variants on `MembershipEventKind` (1-char codes `c`/`m`/`d`, 2-char inner field keys), gated on `actor_power >= POWER_THRESHOLDS.kick (50)` in `verify_event`. `MaterializedMembership` gains a `channels: BTreeMap<ChannelId, ChannelInfo>` field; `materialize` extends with three new branches (`ChannelDelete` tombstones via `deleted_at: Option<Hlc>`, never removes). Four new Tauri IPCs in `lib.rs` (`create_channel`, `modify_channel`, `delete_channel`, `list_channels`) emit events through the engine's existing `insert_local_event` path. The community-state-sync delta consumer learns to fan out a new `channel-config-updated` Tauri event from channel-config deltas (alongside the existing `community-members-changed` for membership deltas). `create_community_inner` is extended to atomically emit a `ChannelCreate { name: "general", write_power: 0 }` immediately after the founding `Join`, with the same shutdown-and-cleanup rollback path on failure.

**Tech Stack:** Rust 1.x · serde + canonical CBOR (`harmony_app::owner_state_crypto::canonical_cbor_encode`) · Ed25519 (`ed25519-dalek`) · Tauri 2 IPC + events · `tokio::sync::Mutex` for async state · `harmony_identity::PrivateIdentity` for signing.

**Spec:** [`docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md`](../specs/2026-05-09-zeb-248-channels-within-communities-design.md) (commit `5145484`). Phase 1 scope = spec §14 phase-table row 1; supporting detail §5.1, §7, §10, §11, §13.1.

**Branch:** `zeb-248-phase1-channel-config-crdt` (already cut from spec branch which was cut from `origin/main` at `0d4fca4`). The Phase 1 PR atomically delivers spec commit (`5145484`) + this plan + implementation commits.

**Linear:** parent [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (Sub-C v2). The Phase 1 sub-ticket will be filed by the user before/during PR open — implementer should NOT invent a ZEB-NNN ID. Use the descriptive phrase "Phase 1 channel-config CRDT" in commit messages until the user provides the ID.

**Memory rules in force** (HARD):
- `cargo fmt --all -- --check` AND `cargo clippy --all-targets -- -D warnings` gates required at every task verification, not just clippy
- No worktrees — `git checkout` in main repo, never create or remove worktrees
- Pull-before-work satisfied (branch on `origin/main` lineage)
- TDD-shaped tasks; every task ends with a commit
- Pipe exit codes lie — never trust `cmd | tail/grep` exit codes; use `set -o pipefail` or `${PIPESTATUS[0]}`
- Test drift is our fault; broken tests on main are exclusively ours
- DO NOT use Monitor for `cargo test` (subagents wait synchronously)
- Tauri IPC param naming: snake_case Rust, the boundary auto-converts to camelCase
- Metadata-before-irreversible-write: `delete_channel`'s "channel exists" lookup MUST precede the irreversible `insert_local_event(ChannelDelete)` call
- Slider/typed-confirm/Two-IPC-TOCTOU rules: NOT applicable in Phase 1 (no UI; no preview-commit IPC pairs)

---

## File Structure

This is the lock-in for what each file does after Phase 1.

### Modified files

| File | What changes | Why |
|---|---|---|
| `src-tauri/src/community_membership.rs` | Add `pub type ChannelId = [u8; 16];` and `pub struct ChannelInfo { name, write_power, created_at, deleted_at }`; extend `MembershipEventKind` with `ChannelCreate`/`ChannelModify`/`ChannelDelete` variants; extend `MaterializedMembership` with `channels: BTreeMap<ChannelId, ChannelInfo>` field; extend `materialize` with three new match branches; extend `verify_event` with channel-config gate; add `VerifyError::ChannelAdminInsufficientPower`. | Single-file extension of the existing CRDT primitives. Channel-config events are SAME substrate as membership events (low-frequency governance). |
| `src-tauri/src/community_state_sync.rs` | No structural changes. Existing `delta_tx` already carries every `SignedMembershipEvent` insert; new variants flow through unchanged. (One-line touch: in `prior_state_at_event` and any sort logic that already uses `event_sort_key`, no change needed — the new variants ride the existing comparator.) | The state-sync engine doesn't care about variant identity; only the IPC layer does. |
| `src-tauri/src/lib.rs` | Add `ChannelInfoDto`, `ChannelConfigChangedPayload`, `ChannelConfigChangeAction` (DTOs); extend `delta_to_change` to RETURN None for channel-config variants (so they don't fire `community-members-changed`); add `delta_to_channel_config_change` projector; extend `run_community_delta_consumer` to take a SECOND emit callback for `channel-config-updated`; add IPCs `create_channel`/`modify_channel`/`delete_channel`/`list_channels`; extend `create_community_inner` to atomically emit a default `#general` `ChannelCreate` after the bootstrap `Join`; register the four new IPCs in `tauri::generate_handler!`. | All IPC + DTO layer in one place, mirroring how Sub-C v1's IPCs landed. |
| `src-tauri/tests/community_membership_unit.rs` | Append unit tests: `channel_create_event_kind_round_trips`, `channel_modify_*_round_trips` (×3 permutations), `channel_delete_event_kind_round_trips`; `materialize_channel_create_adds_to_map`, `materialize_channel_modify_partial_update_preserves_unmodified_field`, `materialize_channel_delete_tombstones_in_place`; `verify_event_channel_create_rejects_below_mod_power`, `verify_event_channel_modify_accepts_at_kick_threshold`, `verify_event_channel_delete_rejects_open_community_no_membership` (negative case); `verify_event_channel_create_succeeds_for_admin`. | Unit-level coverage of the new wire/materialize/verify code paths. |
| `src-tauri/tests/wire_format_community_fixtures.rs` | Append `signed_event_channel_create_wire_bytes_pinned`, `signed_event_channel_modify_full_wire_bytes_pinned`, `signed_event_channel_modify_name_only_wire_bytes_pinned`, `signed_event_channel_modify_power_only_wire_bytes_pinned`, `signed_event_channel_delete_wire_bytes_pinned`. | Pin canonical CBOR bytes for every new variant. Catches accidental wire-format drift in any future refactor. |

### New files

| File | Responsibility |
|---|---|
| `src-tauri/tests/community_channel_config_integration.rs` | Two-engine cross-publish round-trip tests: (1) Alice creates channel → state-CRDT sync → Bob materializes the same `ChannelInfo`; (2) sub-mod actor's `ChannelCreate` is rejected end-to-end; (3) `create_community_inner` auto-creates `#general` and Bob sees it on Alice's bootstrap publish. Mirrors the structure of `community_invite_only_integration.rs` (two engines, `delta_tx` channels, `wait_for_materialized` polling helper). |

### Code that does NOT change in Phase 1

- `src-tauri/src/community_state_crdt.rs` — `insert_event` and `materialize_now` already take any `SignedMembershipEvent` and re-call `materialize`/`verify_event`; no structural change. Materialization cache invalidation already happens on every successful insert.
- `src-tauri/src/community_invite.rs` — channel-config doesn't intersect invite/join paths.
- Frontend (`src/`) — Phase 4 deliverable; this PR is backend-only.

---

## Tasks

Each task ends with a commit. After every task, the implementer runs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test -p harmony-app` (or scoped to the new test files where faster), and ensures all three pass before committing.

---

### Task 0 — Pre-flight + green-baseline confirmation

**Goal:** Confirm the workspace is on the right branch with a clean build and all existing tests passing before making any change. Catches drift early; avoids "is this my failure or pre-existing?" debugging later.

**Files:** none (read-only).

- [ ] **Step 1: Confirm branch + clean tree.**

```bash
git branch --show-current
# Expected: zeb-248-phase1-channel-config-crdt

git status -sb
# Expected: ## zeb-248-phase1-channel-config-crdt
# (no other changes — clean tree on the design-spec commit)

git log --oneline -3
# Expected: 5145484 docs(zeb-248): Sub-C v2 channels-within-communities design spec
#           0d4fca4 Merge pull request #92 from zeblithic/zeb-265-...
#           ...
```

If the branch is wrong or the tree dirty, STOP and surface the issue.

- [ ] **Step 2: cargo fmt baseline.**

```bash
cargo fmt --all -- --check
# Expected: exits 0 with no output
```

- [ ] **Step 3: cargo clippy baseline.**

```bash
cargo clippy --all-targets -- -D warnings 2>&1 | tail -40
# Expected: warning-free + "Compiling ..." then "Finished" lines.
echo "clippy exit: ${PIPESTATUS[0]}"
# Expected: clippy exit: 0
```

The `${PIPESTATUS[0]}` check is mandatory — `cargo clippy 2>&1 | tail` returns `tail`'s exit code, which is always 0 even if clippy failed. Memory rule: pipe exit codes lie.

- [ ] **Step 4: cargo test baseline.**

```bash
cargo test --workspace --no-fail-fast 2>&1 | tail -30
echo "test exit: ${PIPESTATUS[0]}"
# Expected: all tests pass; "test exit: 0"
```

If anything fails on this clean baseline, STOP and surface the failure to the user. Do NOT silently fix unrelated breakage; if it's truly orphaned drift, the user will file a separate ticket per memory rules.

- [ ] **Step 5: No commit.** Task 0 is read-only verification. The next task starts with a code change + test.

---

### Task 1 — `ChannelId`, `ChannelInfo`, and `MembershipEventKind::ChannelCreate`

**Goal:** Land the smallest atomic chunk: the `ChannelId` type alias, the `ChannelInfo` struct, the `ChannelCreate` enum variant, the `materialize` branch that builds a fresh `ChannelInfo` into the channels map, and unit tests for round-trip + materialize-create. No verify gate yet (Task 3); no Modify/Delete (Task 2).

**Files:**
- Modify: `src-tauri/src/community_membership.rs`
- Test: `src-tauri/tests/community_membership_unit.rs` (append-only)

- [ ] **Step 1: Write a failing test for `MembershipEventKind::ChannelCreate` round-trip.**

Append to `src-tauri/tests/community_membership_unit.rs` (after the existing `membership_event_kind_round_trips_all_variants` test):

```rust
use harmony_app::community_membership::ChannelId;

#[test]
fn channel_create_event_kind_round_trips() {
    let ch: ChannelId = [0xAB; 16];
    let kind = MembershipEventKind::ChannelCreate {
        ch,
        nm: "general".to_string(),
        wp: 0,
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let decoded: MembershipEventKind = canonical_cbor_decode(&encoded).expect("decode");
    assert_eq!(decoded, kind);
}
```

- [ ] **Step 2: Run the test to verify it fails.**

```bash
cargo test -p harmony-app --test community_membership_unit channel_create_event_kind_round_trips 2>&1 | tail -20
echo "test exit: ${PIPESTATUS[0]}"
# Expected: FAIL with compile error "no associated item named `ChannelCreate`" or "ChannelId not found"
# (the variant + type don't exist yet)
```

- [ ] **Step 3: Add `ChannelId` type and `ChannelInfo` struct.**

In `src-tauri/src/community_membership.rs`, locate the existing `pub type EventId = [u8; 16];` (around line 56) and add immediately after:

```rust
/// 16-byte ULID identifying a single channel within a community.
/// Generated client-side at `ChannelCreate` time. Same shape as
/// `EventId` but a distinct type so the type system catches accidental
/// substitution between event-IDs and channel-IDs at IPC boundaries.
pub type ChannelId = [u8; 16];
```

Then locate the `MaterializedMembership` struct (around line 540) and add this struct definition immediately after `MemberStatus` (around line 569, BEFORE the `impl CanonicalPayloadSealed` blocks):

```rust
/// Materialized state for one channel in a community. Built by
/// `materialize` from `ChannelCreate`/`ChannelModify`/`ChannelDelete`
/// event replay. `deleted_at` is `Some` once a `ChannelDelete` has been
/// processed for this channel — the channel stays in the map after
/// deletion (tombstone semantics) so historical messages with this
/// `channel_id` can still render their breadcrumb. v3+ may garbage-
/// collect old tombstones; Phase 1 retains them indefinitely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    #[serde(rename = "nm")]
    pub name: String,
    #[serde(rename = "wp")]
    pub write_power: u8,
    #[serde(rename = "ca")]
    pub created_at: Hlc,
    #[serde(rename = "da", skip_serializing_if = "Option::is_none", default)]
    pub deleted_at: Option<Hlc>,
}

impl CanonicalPayloadSealed for ChannelInfo {}
impl CanonicalPayload for ChannelInfo {}
```

- [ ] **Step 4: Add `ChannelCreate` variant to `MembershipEventKind`.**

In `src-tauri/src/community_membership.rs`, locate the `MembershipEventKind` enum (around lines 22–46) and append a new variant inside the enum BEFORE the closing brace, AFTER `SetPower`:

```rust
    /// Channel-config event: a mod-tier+ actor creates a new channel
    /// in this community. `ch` is a fresh ChannelId (ULID); `nm` is
    /// the display name; `wp` is the per-channel write_power threshold
    /// (Phase 1 frontend always submits 0 = anyone-Joined posts; v2
    /// reserves the field so v3 announcement-channel UI is wire-stable).
    /// Variant code "c" (1-char value, not a key — keeps the same-
    /// length-keys invariant intact). Inner field keys are 2-char.
    /// See `docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md` §5.1.
    #[serde(rename = "c")]
    ChannelCreate {
        #[serde(rename = "ch")]
        ch: ChannelId,
        #[serde(rename = "nm")]
        nm: String,
        #[serde(rename = "wp")]
        wp: u8,
    },
```

- [ ] **Step 5: Run the round-trip test to verify it now passes.**

```bash
cargo test -p harmony-app --test community_membership_unit channel_create_event_kind_round_trips 2>&1 | tail -10
echo "test exit: ${PIPESTATUS[0]}"
# Expected: 1 passed; test exit: 0
```

- [ ] **Step 6: Write a failing test for `materialize` building a `channels` entry from `ChannelCreate`.**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
use harmony_app::community_membership::{materialize, ChannelInfo, MaterializedMembership};

#[test]
fn materialize_channel_create_adds_to_map() {
    // Build admin's bootstrap Join + a ChannelCreate by admin.
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc { wall_ms: 1_000, logical: 0, device_id: "admin-dev".into() },
        sig: [0; 64],
        countersig: None,
    };
    let ch_id: ChannelId = [0xAB; 16];
    let ch_create = SignedMembershipEvent {
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            ch: ch_id,
            nm: "general".to_string(),
            wp: 0,
        },
        actor: admin,
        at: Hlc { wall_ms: 2_000, logical: 0, device_id: "admin-dev".into() },
        sig: [0; 64],
        countersig: None,
    };

    let m: MaterializedMembership = materialize(&[admin_join, ch_create.clone()], admin);

    let info = m.channels.get(&ch_id).expect("channel materialized");
    assert_eq!(info.name, "general");
    assert_eq!(info.write_power, 0);
    assert_eq!(info.created_at.wall_ms, 2_000);
    assert!(info.deleted_at.is_none());
}
```

- [ ] **Step 7: Run the test to verify it fails.**

```bash
cargo test -p harmony-app --test community_membership_unit materialize_channel_create_adds_to_map 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"
# Expected: FAIL with compile error "no field `channels` on type `MaterializedMembership`"
```

- [ ] **Step 8: Extend `MaterializedMembership` with `channels` field.**

In `src-tauri/src/community_membership.rs`, modify the `MaterializedMembership` struct (around line 540) to add the `channels` field:

```rust
pub struct MaterializedMembership {
    pub members: BTreeMap<OwnerAddr, MemberState>,
    /// Per-actor power level. Unset key = 0 = default. The community
    /// admin (Space.admin_addr) starts at 100 implicitly via the
    /// bootstrap rule — see `materialize` (Task 9). SetPower events
    /// override.
    pub power_levels: BTreeMap<OwnerAddr, u8>,
    /// Per-channel materialized state. Built by `materialize` from
    /// `ChannelCreate`/`ChannelModify`/`ChannelDelete` event replay
    /// (ZEB-248 Phase 1). `BTreeMap` (not `HashMap`) so iteration order
    /// is deterministic — needed by callers that hash the materialized
    /// view (e.g., a future test fixture pinning a multi-channel state).
    pub channels: BTreeMap<ChannelId, ChannelInfo>,
}
```

`MaterializedMembership` already derives `Default` via field defaults — `BTreeMap::default()` is empty, so the new field needs no constructor change.

- [ ] **Step 9: Add the `materialize` branch for `ChannelCreate`.**

Locate the `materialize` function in `src-tauri/src/community_membership.rs` (around line 627). Inside the `for event in sorted` loop, the existing match handles `Join`/`Leave`/`Invite`/`Kick`/`SetPower`. After the `SetPower` arm (close to line 870 area — implementer should locate exact position), add:

```rust
            MembershipEventKind::ChannelCreate { ch, nm, wp } => {
                // Idempotent on duplicate channel_id: first create wins
                // (replays + reorderings under DAG-sync may deliver the
                // same ChannelCreate twice; the second one must NOT
                // overwrite name/write_power/created_at — that would let
                // a duplicate-emit refresh created_at and reset history
                // markers). A subsequent ChannelModify is the right path
                // to update fields; a duplicate ChannelCreate is a no-op.
                m.channels.entry(*ch).or_insert_with(|| ChannelInfo {
                    name: nm.clone(),
                    write_power: *wp,
                    created_at: event.at.clone(),
                    deleted_at: None,
                });
            }
```

(`ChannelModify` and `ChannelDelete` arms ship in Task 2.)

- [ ] **Step 10: Run both Task 1 tests to confirm they pass.**

```bash
cargo test -p harmony-app --test community_membership_unit channel_create 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"
# Expected: 2 passed (round-trip + materialize); test exit: 0
```

- [ ] **Step 11: Verify the workspace still compiles + clippy/fmt clean.**

The `MembershipEventKind` enum gained a variant — every existing `match event.kind` site must be exhaustive. Step 9's match also lacks the Modify/Delete arms (those land in Task 2). Add `_ => {}` placeholder in the materialize arm during Task 1 only, OR have the Task 2 implementation already extend the match. Cleaner: add the Modify/Delete arms NOW as `_ => {}` no-ops so the match stays exhaustive without having to write a temporary catch-all:

In the `materialize` function, AFTER the `ChannelCreate` arm from Step 9, append:

```rust
            MembershipEventKind::ChannelModify { .. } => {
                // Implementation lands in Task 2; placeholder no-op so
                // the match stays exhaustive without a catch-all.
            }
            MembershipEventKind::ChannelDelete { .. } => {
                // Implementation lands in Task 2; placeholder no-op so
                // the match stays exhaustive without a catch-all.
            }
```

Same treatment in `verify_event` — the existing match (around line 1014–1027) needs new arms. For Task 1 the arms can be no-ops returning Ok():

```rust
        MembershipEventKind::ChannelCreate { .. }
        | MembershipEventKind::ChannelModify { .. }
        | MembershipEventKind::ChannelDelete { .. } => {
            // Verify gate ships in Task 3. Placeholder allow-all keeps
            // the match exhaustive; Task 3 replaces with the
            // mod-tier power check.
        }
```

The other match in `verify_event` (around line 1036, the per-kind power rules) needs the same treatment — add a placeholder arm for the three new variants. Look for `MembershipEventKind::SetPower { level, .. } => { ... }` and after it add:

```rust
        MembershipEventKind::ChannelCreate { .. }
        | MembershipEventKind::ChannelModify { .. }
        | MembershipEventKind::ChannelDelete { .. } => {
            // Per-kind power gate ships in Task 3.
        }
```

If there's also an `event_sort_key` callsite that pattern-matches on `kind` (there shouldn't be — the comparator only reads `at`/`id`/`sig` — verify by `grep -n 'match.*kind' src-tauri/src/`), no further changes needed.

Run:

```bash
cargo build -p harmony-app 2>&1 | tail -10
echo "build exit: ${PIPESTATUS[0]}"
# Expected: build exit: 0

cargo fmt --all -- --check
# Expected: exits 0

cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
echo "clippy exit: ${PIPESTATUS[0]}"
# Expected: clippy exit: 0
```

If clippy complains about the new fields/variants being "unused," add `#[allow(dead_code)]` or — better — let Task 2/3 immediately exercise them so the warnings disappear naturally. Don't merge `dead_code` allowances into the final commit.

- [ ] **Step 12: Run the full membership unit test suite to catch any regressions.**

```bash
cargo test -p harmony-app --test community_membership_unit 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"
# Expected: all existing tests still pass + the 2 new ones; test exit: 0
```

- [ ] **Step 13: Commit.**

```bash
git add src-tauri/src/community_membership.rs src-tauri/tests/community_membership_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-248-p1): add ChannelId, ChannelInfo, MembershipEventKind::ChannelCreate

First slice of Phase 1 channel-config CRDT (ZEB-248 Sub-C v2). Lands the
ChannelCreate variant + materialize branch + round-trip and materialize
unit tests. ChannelModify/Delete + verify gate land in Tasks 2-3.

Per spec docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md
§5.1, §10. ChannelInfo tombstone semantics (deleted_at: Option<Hlc>) are in
place for ChannelDelete in Task 2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2 — `ChannelModify`, `ChannelDelete`, materialize tombstone

**Goal:** Add the remaining two channel-config variants. `ChannelModify` is partial-update (`Option<String>` for name, `Option<u8>` for write_power); `ChannelDelete` is a tombstone (sets `deleted_at`, never removes from the map). Replace the Task-1 placeholder no-op arms in `materialize` with the real implementations. Verify gate still lives in Task 3.

**Files:**
- Modify: `src-tauri/src/community_membership.rs`
- Test: `src-tauri/tests/community_membership_unit.rs` (append-only)

- [ ] **Step 1: Write failing tests for `ChannelModify` round-trip + the three Some/None permutations.**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
#[test]
fn channel_modify_event_kind_round_trips_full() {
    let kind = MembershipEventKind::ChannelModify {
        ch: [0xAB; 16],
        nm: Some("general-renamed".to_string()),
        wp: Some(50),
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let decoded: MembershipEventKind = canonical_cbor_decode(&encoded).expect("decode");
    assert_eq!(decoded, kind);
}

#[test]
fn channel_modify_event_kind_round_trips_name_only() {
    let kind = MembershipEventKind::ChannelModify {
        ch: [0xAB; 16],
        nm: Some("renamed".to_string()),
        wp: None,
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let decoded: MembershipEventKind = canonical_cbor_decode(&encoded).expect("decode");
    assert_eq!(decoded, kind);
}

#[test]
fn channel_modify_event_kind_round_trips_power_only() {
    let kind = MembershipEventKind::ChannelModify {
        ch: [0xAB; 16],
        nm: None,
        wp: Some(50),
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let decoded: MembershipEventKind = canonical_cbor_decode(&encoded).expect("decode");
    assert_eq!(decoded, kind);
}

#[test]
fn channel_delete_event_kind_round_trips() {
    let kind = MembershipEventKind::ChannelDelete { ch: [0xAB; 16] };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let decoded: MembershipEventKind = canonical_cbor_decode(&encoded).expect("decode");
    assert_eq!(decoded, kind);
}
```

- [ ] **Step 2: Run them — expect compile failures.**

```bash
cargo test -p harmony-app --test community_membership_unit channel_modify channel_delete 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"
# Expected: FAIL with "no associated item named `ChannelModify`" / `ChannelDelete`
```

- [ ] **Step 3: Add `ChannelModify` and `ChannelDelete` variants to `MembershipEventKind`.**

In `src-tauri/src/community_membership.rs`, append to `MembershipEventKind` AFTER the `ChannelCreate` variant from Task 1:

```rust
    /// Channel-config event: a mod-tier+ actor modifies an existing
    /// channel's name and/or write_power. Either field may be `None` to
    /// leave that field unchanged. If both are `None` the IPC layer
    /// rejects the call before signing (no-op). Variant code "m".
    /// See spec §5.1.
    #[serde(rename = "m")]
    ChannelModify {
        #[serde(rename = "ch")]
        ch: ChannelId,
        #[serde(rename = "nm", skip_serializing_if = "Option::is_none", default)]
        nm: Option<String>,
        #[serde(rename = "wp", skip_serializing_if = "Option::is_none", default)]
        wp: Option<u8>,
    },

    /// Channel-config event: a mod-tier+ actor deletes a channel.
    /// Tombstone semantics — the channel is NOT removed from the
    /// materialized `channels` map; instead `deleted_at` is set. Future
    /// posts to this channel are rejected by Phase 2's verify_channel_event;
    /// historical messages still render with their breadcrumb intact.
    /// Variant code "d". See spec §5.1.
    #[serde(rename = "d")]
    ChannelDelete {
        #[serde(rename = "ch")]
        ch: ChannelId,
    },
```

- [ ] **Step 4: Run the four round-trip tests — expect them to pass.**

```bash
cargo test -p harmony-app --test community_membership_unit channel_modify channel_delete 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"
# Expected: 4 passed; test exit: 0
```

- [ ] **Step 5: Write failing tests for `materialize` Modify (partial update) and Delete (tombstone).**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
#[test]
fn materialize_channel_modify_partial_update_preserves_unmodified_field() {
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
        sig: [0; 64],
        countersig: None,
    };
    let ch_id: ChannelId = [0xAB; 16];
    let ch_create = SignedMembershipEvent {
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            ch: ch_id,
            nm: "general".to_string(),
            wp: 0,
        },
        actor: admin,
        at: Hlc { wall_ms: 2_000, logical: 0, device_id: "a".into() },
        sig: [0; 64],
        countersig: None,
    };
    // Only modify name — write_power should stay at 0.
    let ch_modify = SignedMembershipEvent {
        id: [0x03; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelModify {
            ch: ch_id,
            nm: Some("renamed".to_string()),
            wp: None,
        },
        actor: admin,
        at: Hlc { wall_ms: 3_000, logical: 0, device_id: "a".into() },
        sig: [0; 64],
        countersig: None,
    };

    let m = materialize(&[admin_join, ch_create, ch_modify], admin);
    let info = m.channels.get(&ch_id).expect("channel still present");
    assert_eq!(info.name, "renamed");
    assert_eq!(info.write_power, 0);            // preserved
    assert_eq!(info.created_at.wall_ms, 2_000); // preserved
    assert!(info.deleted_at.is_none());
}

#[test]
fn materialize_channel_delete_tombstones_in_place() {
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
        sig: [0; 64],
        countersig: None,
    };
    let ch_id: ChannelId = [0xAB; 16];
    let ch_create = SignedMembershipEvent {
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelCreate {
            ch: ch_id,
            nm: "general".to_string(),
            wp: 0,
        },
        actor: admin,
        at: Hlc { wall_ms: 2_000, logical: 0, device_id: "a".into() },
        sig: [0; 64],
        countersig: None,
    };
    let ch_delete = SignedMembershipEvent {
        id: [0x03; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelDelete { ch: ch_id },
        actor: admin,
        at: Hlc { wall_ms: 4_000, logical: 0, device_id: "a".into() },
        sig: [0; 64],
        countersig: None,
    };

    let m = materialize(&[admin_join, ch_create, ch_delete], admin);
    let info = m.channels.get(&ch_id).expect("channel still in map (tombstone, not removed)");
    assert_eq!(info.name, "general");
    assert_eq!(info.deleted_at.as_ref().map(|h| h.wall_ms), Some(4_000));
}

#[test]
fn materialize_channel_modify_on_unknown_channel_is_noop() {
    // ChannelModify referencing a channel that doesn't exist is silently
    // ignored — defense-in-depth against an event arriving before its
    // ChannelCreate (DAG-sync may reorder; verify_event will reject this
    // case in Task 3 anyway, but materialize must be safe even if a
    // malformed event slips past).
    let admin = OwnerAddr([0x10; 16]);
    let admin_join = SignedMembershipEvent {
        id: [0x01; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: admin,
        at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
        sig: [0; 64],
        countersig: None,
    };
    let ch_modify = SignedMembershipEvent {
        id: [0x02; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::ChannelModify {
            ch: [0xCC; 16], // never created
            nm: Some("ghost".into()),
            wp: None,
        },
        actor: admin,
        at: Hlc { wall_ms: 2_000, logical: 0, device_id: "a".into() },
        sig: [0; 64],
        countersig: None,
    };

    let m = materialize(&[admin_join, ch_modify], admin);
    assert!(m.channels.is_empty(), "modify on unknown channel must not synthesize a ghost entry");
}
```

- [ ] **Step 6: Run them — expect failures (the placeholder no-op arms from Task 1 currently swallow Modify/Delete).**

```bash
cargo test -p harmony-app --test community_membership_unit materialize_channel 2>&1 | tail -25
echo "test exit: ${PIPESTATUS[0]}"
# Expected: materialize_channel_modify_partial_update_preserves_unmodified_field FAIL
#           (because Modify is a no-op; name stays "general", not "renamed")
#           materialize_channel_delete_tombstones_in_place FAIL
#           (because Delete is a no-op; deleted_at stays None)
#           materialize_channel_modify_on_unknown_channel_is_noop PASS
#           (the no-op happens to satisfy this case)
```

- [ ] **Step 7: Replace placeholder arms with real implementations in `materialize`.**

In `src-tauri/src/community_membership.rs`, locate the placeholder arms added in Task 1 (in the `for event in sorted` loop in `materialize`) and replace them:

```rust
            MembershipEventKind::ChannelModify { ch, nm, wp } => {
                // Partial update: only apply fields that are Some.
                // Unknown ChannelId is silently ignored — verify_event
                // (Task 3) does NOT gate Modify on the channel existing
                // (a malicious actor could otherwise pre-trigger a verify
                // failure to leak existence info), so materialize stays
                // safe by default. A reordered Modify-before-Create
                // would be discarded here; the eventual sort means the
                // re-replay after the missing Create arrives still does
                // the right thing.
                if let Some(info) = m.channels.get_mut(ch) {
                    if let Some(new_name) = nm {
                        info.name = new_name.clone();
                    }
                    if let Some(new_wp) = wp {
                        info.write_power = *new_wp;
                    }
                }
            }
            MembershipEventKind::ChannelDelete { ch } => {
                // Tombstone: set deleted_at, do NOT remove. Idempotent
                // on duplicate: first delete wins (preserves the original
                // deleted_at HLC). Subsequent ChannelModify can still
                // mutate name/write_power on a tombstoned channel —
                // intentional, so admins can correct the name of an
                // accidentally-deleted-then-renamed channel without an
                // un-delete primitive (deferred to v3).
                if let Some(info) = m.channels.get_mut(ch) {
                    if info.deleted_at.is_none() {
                        info.deleted_at = Some(event.at.clone());
                    }
                }
            }
```

- [ ] **Step 8: Run the materialize tests — expect all to pass.**

```bash
cargo test -p harmony-app --test community_membership_unit materialize_channel 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"
# Expected: 3 passed (modify partial-update + delete tombstone + modify unknown is-noop); test exit: 0
```

- [ ] **Step 9: Run the FULL membership unit suite + cargo fmt + clippy.**

```bash
cargo test -p harmony-app --test community_membership_unit 2>&1 | tail -10
echo "test exit: ${PIPESTATUS[0]}"
# Expected: all existing + 7 new tests pass; test exit: 0

cargo fmt --all -- --check
# Expected: exits 0

cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
echo "clippy exit: ${PIPESTATUS[0]}"
# Expected: clippy exit: 0
```

- [ ] **Step 10: Commit.**

```bash
git add src-tauri/src/community_membership.rs src-tauri/tests/community_membership_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-248-p1): add ChannelModify + ChannelDelete with tombstone materialize

Second slice of Phase 1 (ZEB-248 Sub-C v2). Lands the partial-update
ChannelModify variant and tombstone-shaped ChannelDelete (sets
deleted_at; channel stays in materialized map for breadcrumb rendering
and v3 un-delete-via-modify ergonomics). Verify gate ships in Task 3.

Per spec §5.1, §10. Materialize-on-unknown-channel is a silent no-op as
a defense-in-depth against reordered DAG-sync delivery.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3 — `verify_event` channel-config gate + `VerifyError::ChannelAdminInsufficientPower`

**Goal:** Replace the placeholder arms in `verify_event` with the real mod-tier (`actor_power >= POWER_THRESHOLDS.kick`) gate. Add `VerifyError::ChannelAdminInsufficientPower` variant + Display impl. Add unit tests for accept-at-mod, reject-below-mod, and admin-power flow.

**Files:**
- Modify: `src-tauri/src/community_membership.rs`
- Test: `src-tauri/tests/community_membership_unit.rs` (append-only)

- [ ] **Step 1: Write failing tests covering accept-mod, reject-sub-mod, and admin-creates-from-bootstrap-power.**

Append to `src-tauri/tests/community_membership_unit.rs`:

```rust
use harmony_app::community_membership::{verify_event, VerifyContext, VerifyError, EventPayload, sign_event_with_identity};

/// Build a signed ChannelCreate event by `actor_identity` referencing
/// `community_id`. `actor_id` is the EventId; `at` is the HLC.
fn signed_channel_create(
    actor_identity: &PrivateIdentity,
    actor: OwnerAddr,
    community_id: SpaceId,
    actor_id: EventId,
    ch_id: ChannelId,
    name: &str,
    wp: u8,
    at: Hlc,
) -> SignedMembershipEvent {
    let payload = EventPayload {
        id: actor_id,
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            ch: ch_id,
            nm: name.to_string(),
            wp,
        },
        actor,
        at,
    };
    sign_event_with_identity(&payload, actor_identity).expect("sign")
}

#[test]
fn verify_event_channel_create_succeeds_for_admin_at_bootstrap_power() {
    let (admin_priv, admin_pub, admin_addr) = make_test_identity(0xAA);
    let community_id = SpaceId([0x37; 16]);

    // Admin's bootstrap power is 100 (set in materialize). prior_state
    // built from just the admin's Join.
    let admin_join = SignedMembershipEvent {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
        sig: [0; 64],
        countersig: None,
    };
    let prior_state = materialize(&[admin_join], admin_addr);

    let event = signed_channel_create(
        &admin_priv,
        admin_addr,
        community_id,
        [0x02; 16],
        [0xAB; 16],
        "general",
        0,
        Hlc { wall_ms: 2_000, logical: 0, device_id: "a".into() },
    );

    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
        actor_identity_pub: &admin_pub,
        countersigner_identity_pub: None,
    };

    assert_eq!(verify_event(&event, &prior_state, &ctx), Ok(()));
}

#[test]
fn verify_event_channel_create_rejects_below_mod_power() {
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let (sub_priv, sub_pub, sub_addr) = make_test_identity(0xBB);
    let community_id = SpaceId([0x37; 16]);

    // Build prior state: admin Join + sub Join (sub has default power = 0).
    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");

    let sub_join_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: sub_addr,
        at: Hlc { wall_ms: 1_500, logical: 0, device_id: "b".into() },
    };
    let sub_join = sign_event_with_identity(&sub_join_payload, &sub_priv).expect("sign");

    let prior_state = materialize(&[admin_join, sub_join], admin_addr);
    // Sub-actor's power is 0 (default, well below kick=50).

    let event = signed_channel_create(
        &sub_priv,
        sub_addr,
        community_id,
        [0x03; 16],
        [0xAB; 16],
        "spam-channel",
        0,
        Hlc { wall_ms: 2_000, logical: 0, device_id: "b".into() },
    );

    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
        actor_identity_pub: &sub_pub,
        countersigner_identity_pub: None,
    };

    assert_eq!(
        verify_event(&event, &prior_state, &ctx),
        Err(VerifyError::ChannelAdminInsufficientPower)
    );
}

#[test]
fn verify_event_channel_modify_accepts_at_kick_threshold() {
    // A mod (power exactly 50) is allowed to modify channels.
    let (admin_priv, _admin_pub, admin_addr) = make_test_identity(0xAA);
    let (mod_priv, mod_pub, mod_addr) = make_test_identity(0xBB);
    let community_id = SpaceId([0x37; 16]);

    let admin_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: admin_addr,
        at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
    };
    let admin_join = sign_event_with_identity(&admin_join_payload, &admin_priv).expect("sign");

    let mod_join_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: mod_addr,
        at: Hlc { wall_ms: 1_500, logical: 0, device_id: "b".into() },
    };
    let mod_join = sign_event_with_identity(&mod_join_payload, &mod_priv).expect("sign");

    // Admin SetPower to bring mod_addr to power 50 (kick threshold).
    let setpower_payload = EventPayload {
        id: [0x03; 16],
        community_id,
        kind: MembershipEventKind::SetPower { target: mod_addr, level: 50 },
        actor: admin_addr,
        at: Hlc { wall_ms: 2_000, logical: 0, device_id: "a".into() },
    };
    let setpower = sign_event_with_identity(&setpower_payload, &admin_priv).expect("sign");

    let prior_state = materialize(&[admin_join, mod_join, setpower], admin_addr);

    // Mod creates a channel.
    let event = signed_channel_create(
        &mod_priv,
        mod_addr,
        community_id,
        [0x04; 16],
        [0xAB; 16],
        "mods-channel",
        0,
        Hlc { wall_ms: 3_000, logical: 0, device_id: "b".into() },
    );

    let ctx = VerifyContext {
        expected_community_id: community_id,
        admin_addr,
        is_invite_only: false,
        actor_identity_pub: &mod_pub,
        countersigner_identity_pub: None,
    };

    assert_eq!(verify_event(&event, &prior_state, &ctx), Ok(()));
}
```

- [ ] **Step 2: Run them — expect compile failures + 1 wrong-result.**

```bash
cargo test -p harmony-app --test community_membership_unit verify_event_channel 2>&1 | tail -25
echo "test exit: ${PIPESTATUS[0]}"
# Expected: compile error "no variant `ChannelAdminInsufficientPower` in VerifyError"
```

- [ ] **Step 3: Add the new `VerifyError` variant + Display impl.**

In `src-tauri/src/community_membership.rs`, locate the `VerifyError` enum (around line 259) and append a new variant just before `EncodeError(String)`:

```rust
    /// Channel-config event (`ChannelCreate`/`Modify`/`Delete`) was
    /// signed by an actor whose power is below `POWER_THRESHOLDS.kick`
    /// (mod-tier). v2 hardcodes mod-tier as the channel-admin gate;
    /// per-community customization is deferred to ZEB-251. Distinct
    /// from `ActorPowerInsufficient` so the IPC layer can emit a clean
    /// "you don't have permission to manage channels" error string
    /// without overloading the membership-level diagnostic.
    ChannelAdminInsufficientPower,
```

Then locate the `impl std::fmt::Display for VerifyError` block (around line 340) and append the new arm before the existing `EncodeError(s)` arm:

```rust
            VerifyError::ChannelAdminInsufficientPower => write!(
                f,
                "channel-config events require power >= POWER_THRESHOLDS.kick (mod-tier)"
            ),
```

- [ ] **Step 4: Replace the Task-1 placeholder arms in `verify_event` with the real gate.**

In `src-tauri/src/community_membership.rs`, locate the placeholder arm in `verify_event` that handles channel-config events in the FIRST match block (around line 1014–1027 area, the moderation-actions block) and remove it:

```rust
        // REMOVE THIS BLOCK FROM TASK 1:
        // MembershipEventKind::ChannelCreate { .. }
        // | MembershipEventKind::ChannelModify { .. }
        // | MembershipEventKind::ChannelDelete { .. } => {
        //     // Verify gate ships in Task 3. Placeholder allow-all keeps
        //     // the match exhaustive; Task 3 replaces with the
        //     // mod-tier power check.
        // }
```

The first match block (Joined-membership check) is for `Invite`/`Kick`/`SetPower` requiring `ActorNotJoined` checks. Channel-config doesn't need its own arm here — but the match must stay exhaustive. Add a single combined arm to that match:

```rust
        MembershipEventKind::ChannelCreate { .. }
        | MembershipEventKind::ChannelModify { .. }
        | MembershipEventKind::ChannelDelete { .. } => {
            // Channel-config requires actor to be Joined AND power >=
            // kick. Joined-check first so a non-member with high power
            // (e.g. former admin after Kick) can't create channels.
            // The power check fires in the per-kind power-rules block
            // below; this block establishes membership.
            if !is_joined_member(prior_state, &event.actor) {
                return Err(VerifyError::ActorNotJoined);
            }
        }
```

Then locate the SECOND match block (per-kind power rules, around line 1036) and replace its placeholder arm with the real channel-admin gate:

```rust
        MembershipEventKind::ChannelCreate { .. }
        | MembershipEventKind::ChannelModify { .. }
        | MembershipEventKind::ChannelDelete { .. } => {
            if actor_power < POWER_THRESHOLDS.kick {
                return Err(VerifyError::ChannelAdminInsufficientPower);
            }
        }
```

- [ ] **Step 5: Run the new verify_event tests — expect them to pass.**

```bash
cargo test -p harmony-app --test community_membership_unit verify_event_channel 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"
# Expected: 3 passed; test exit: 0
```

- [ ] **Step 6: Run the FULL membership unit suite — verify no regressions.**

```bash
cargo test -p harmony-app --test community_membership_unit 2>&1 | tail -10
echo "test exit: ${PIPESTATUS[0]}"
# Expected: all pre-existing + 10 new tests (4 round-trip + 3 materialize + 3 verify_event) pass

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
echo "clippy exit: ${PIPESTATUS[0]}"
# Expected: clippy exit: 0
```

- [ ] **Step 7: Commit.**

```bash
git add src-tauri/src/community_membership.rs src-tauri/tests/community_membership_unit.rs
git commit -m "$(cat <<'EOF'
feat(zeb-248-p1): verify_event gate channel-config on actor_power >= kick

Third slice of Phase 1 (ZEB-248 Sub-C v2). Replaces the Task-1 placeholder
verify_event arms with the real mod-tier gate (POWER_THRESHOLDS.kick = 50).
Adds VerifyError::ChannelAdminInsufficientPower variant + Display impl.
Channel-config events also get the standard ActorNotJoined membership
check — power without membership is meaningless.

Per spec §7, §10. v3+ may add per-channel admin scoping; v1 hardcodes
the threshold per ZEB-251 deferral pattern.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4 — Wire-format CBOR fixtures for the three new variants

**Goal:** Pin canonical CBOR bytes for `ChannelCreate`, `ChannelModify` (full + name-only + power-only), and `ChannelDelete`. Catches accidental wire drift in any future refactor. Mirrors the existing `signed_event_*_wire_bytes_pinned` pattern.

**Files:**
- Modify: `src-tauri/tests/wire_format_community_fixtures.rs` (append-only)

- [ ] **Step 1: Write the five fixture tests with placeholder hex strings.**

Append to `src-tauri/tests/wire_format_community_fixtures.rs`:

```rust
use harmony_app::community_membership::ChannelId;

#[test]
fn signed_event_channel_create_wire_bytes_pinned() {
    let ch_id: ChannelId = [0x42; 16];
    let event = fixture_signed_event(MembershipEventKind::ChannelCreate {
        ch: ch_id,
        nm: "general".to_string(),
        wp: 0,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_channel_create hex: {hex}");
    assert_eq!(
        hex, "<COMPUTE>",
        "ChannelCreate wire format changed"
    );
}

#[test]
fn signed_event_channel_modify_full_wire_bytes_pinned() {
    let ch_id: ChannelId = [0x42; 16];
    let event = fixture_signed_event(MembershipEventKind::ChannelModify {
        ch: ch_id,
        nm: Some("renamed".to_string()),
        wp: Some(50),
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_channel_modify_full hex: {hex}");
    assert_eq!(
        hex, "<COMPUTE>",
        "ChannelModify (full) wire format changed"
    );
}

#[test]
fn signed_event_channel_modify_name_only_wire_bytes_pinned() {
    let ch_id: ChannelId = [0x42; 16];
    let event = fixture_signed_event(MembershipEventKind::ChannelModify {
        ch: ch_id,
        nm: Some("renamed".to_string()),
        wp: None,
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_channel_modify_name_only hex: {hex}");
    assert_eq!(
        hex, "<COMPUTE>",
        "ChannelModify (name-only) wire format changed"
    );
}

#[test]
fn signed_event_channel_modify_power_only_wire_bytes_pinned() {
    let ch_id: ChannelId = [0x42; 16];
    let event = fixture_signed_event(MembershipEventKind::ChannelModify {
        ch: ch_id,
        nm: None,
        wp: Some(50),
    });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_channel_modify_power_only hex: {hex}");
    assert_eq!(
        hex, "<COMPUTE>",
        "ChannelModify (power-only) wire format changed"
    );
}

#[test]
fn signed_event_channel_delete_wire_bytes_pinned() {
    let ch_id: ChannelId = [0x42; 16];
    let event = fixture_signed_event(MembershipEventKind::ChannelDelete { ch: ch_id });
    let bytes = canonical_cbor_encode(&event).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("signed_event_channel_delete hex: {hex}");
    assert_eq!(
        hex, "<COMPUTE>",
        "ChannelDelete wire format changed"
    );
}
```

- [ ] **Step 2: Run the fixtures with `--nocapture` to extract the actual hex bytes.**

```bash
cargo test -p harmony-app --test wire_format_community_fixtures channel -- --nocapture 2>&1 | tail -30
# Each test will fail with "ChannelCreate wire format changed" but the
# eprintln will show the actual hex BEFORE the panic. Copy each hex
# string into the matching assertion.
```

- [ ] **Step 3: Replace each `<COMPUTE>` placeholder with the printed hex value.**

Use the Edit tool to replace each `<COMPUTE>` literal with the printed hex from Step 2. Five replacements in total. Order does not matter.

- [ ] **Step 4: Re-run the fixture tests — expect all five to pass.**

```bash
cargo test -p harmony-app --test wire_format_community_fixtures channel 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"
# Expected: 5 passed; test exit: 0
```

- [ ] **Step 5: Run the full fixture suite — verify no existing fixtures regressed.**

```bash
cargo test -p harmony-app --test wire_format_community_fixtures 2>&1 | tail -10
echo "test exit: ${PIPESTATUS[0]}"
# Expected: all pre-existing + 5 new pinned; test exit: 0

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
echo "clippy exit: ${PIPESTATUS[0]}"
# Expected: clippy exit: 0
```

- [ ] **Step 6: Commit.**

```bash
git add src-tauri/tests/wire_format_community_fixtures.rs
git commit -m "$(cat <<'EOF'
test(zeb-248-p1): pin canonical CBOR fixtures for channel-config events

Five new fixtures: ChannelCreate, ChannelModify (full / name-only /
power-only) — exercises the same-length-keys invariant under each
Some/None permutation — and ChannelDelete.

Catches silent wire-format drift in any future refactor; mirrors the
ZEB-217 Sub-C v1 fixture pattern in this file.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5 — IPC `create_channel` + `channel-config-updated` event plumbing

**Goal:** Land the first IPC (`create_channel`) end-to-end: signs a `ChannelCreate` event with the caller's identity, inserts via the engine's `insert_local_event`, the existing delta channel carries it to the Tauri layer, and a NEW `channel-config-updated` Tauri event fires (separate from `community-members-changed`). Add the new DTO types + the projection function + the consumer-extension that fans out to two emit callbacks. Test: an IPC call results in a `channel-config-updated` event being emitted (using a captured-payload mock).

**Files:**
- Modify: `src-tauri/src/lib.rs`
- Test: inline `#[cfg(test)] mod create_channel_ipc_tests` in `src-tauri/src/lib.rs` mirroring the existing `list_community_members_ipc_tests` module

- [ ] **Step 1: Add the new DTO types `ChannelInfoDto`, `ChannelConfigChangeAction`, `ChannelConfigChangedPayload`.**

In `src-tauri/src/lib.rs`, locate the existing `MembershipChange*` types (around line 7935) and append AFTER the `MembershipChangeDetail` enum:

```rust
/// Materialized channel info row for the `list_channels` IPC and the
/// `channel-config-updated` Tauri event payload. Mirrors
/// `ChannelInfo` in `community_membership.rs` but with stringified
/// hex `channel_id` and camelCase fields for the JS bridge.
/// `created_at` and `deleted_at` are passed as the wire `Hlc` shape
/// (same convention as `MemberInfoDto.joined_at`); the frontend is
/// responsible for any further reshaping.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelInfoDto {
    pub channel_id: String,
    pub name: String,
    pub write_power: u8,
    pub created_at: crate::owner_state_types::Hlc,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<crate::owner_state_types::Hlc>,
}

/// Action discriminator for a `channel-config-updated` Tauri event.
/// Distinct enum (vs. reusing MembershipChangeType) so the frontend's
/// `channel-config-updated` listener doesn't have to re-discriminate
/// against unrelated membership variants.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ChannelConfigChangeAction {
    Created,
    Modified,
    Deleted,
}

/// Wire payload for the `channel-config-updated` Tauri event. Emitted
/// by the community-state-CRDT delta consumer when materialization
/// detects a `ChannelCreate`/`ChannelModify`/`ChannelDelete` mutation.
/// `name` and `write_power` are populated for `Created` (always, both
/// fields are required on the event) and `Modified` (only the fields
/// the modify event actually carried — None means unchanged). Both
/// are omitted for `Deleted`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelConfigChangedPayload {
    pub community_id: String,
    pub channel_id: String,
    pub action: ChannelConfigChangeAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_power: Option<u8>,
    pub at_wall_ms: u64,
}
```

- [ ] **Step 2: Add the `delta_to_channel_config_change` projector + extend `delta_to_change` to return `None` for channel-config variants.**

In `src-tauri/src/lib.rs`, locate `pub fn delta_to_change` (around line 7985). The existing function returns `Some` for every variant — extend it so channel-config variants return `None` (signalling "this delta is not a membership change; the channel-config consumer will handle it"). Modify the match in `delta_to_change`:

```rust
pub fn delta_to_change(
    delta: &crate::community_state_sync::CommunityMembershipDelta,
) -> Option<(String, MembershipChange)> {
    let cid_hex = hex::encode(delta.community_id.0);
    let actor_hex = hex::encode(delta.event.actor.0);
    let at_wall_ms = delta.event.at.wall_ms;
    let change = match &delta.event.kind {
        crate::community_membership::MembershipEventKind::Join => MembershipChange { /* ... existing ... */ r#type: MembershipChangeType::Joined, target: actor_hex, by: None, detail: None, at_wall_ms },
        crate::community_membership::MembershipEventKind::Leave => MembershipChange { r#type: MembershipChangeType::Left, target: actor_hex, by: None, detail: None, at_wall_ms },
        crate::community_membership::MembershipEventKind::Invite { target } => MembershipChange { r#type: MembershipChangeType::Invited, target: hex::encode(target.0), by: Some(actor_hex), detail: None, at_wall_ms },
        crate::community_membership::MembershipEventKind::Kick { target, reason } => MembershipChange { r#type: MembershipChangeType::Kicked, target: hex::encode(target.0), by: Some(actor_hex), detail: reason.clone().map(MembershipChangeDetail::Reason), at_wall_ms },
        crate::community_membership::MembershipEventKind::SetPower { target, level } => MembershipChange { r#type: MembershipChangeType::PowerChanged, target: hex::encode(target.0), by: Some(actor_hex), detail: Some(MembershipChangeDetail::Level(*level)), at_wall_ms },
        // Channel-config events are projected into ChannelConfigChangedPayload by
        // delta_to_channel_config_change; this function returns None so the
        // consumer fan-out fires the right event.
        crate::community_membership::MembershipEventKind::ChannelCreate { .. }
        | crate::community_membership::MembershipEventKind::ChannelModify { .. }
        | crate::community_membership::MembershipEventKind::ChannelDelete { .. } => {
            return None;
        }
    };
    Some((cid_hex, change))
}
```

(The implementer should preserve the existing block bodies verbatim — the snippet above abbreviates them with `/* ... existing ... */` for clarity but the actual edit must keep every field.)

Then, IMMEDIATELY AFTER `delta_to_change`, add the new projector:

```rust
/// Project a `CommunityMembershipDelta` into a `ChannelConfigChangedPayload`.
/// Returns `None` for membership-event kinds (those are handled by
/// `delta_to_change`). Symmetric to `delta_to_change`.
pub fn delta_to_channel_config_change(
    delta: &crate::community_state_sync::CommunityMembershipDelta,
) -> Option<ChannelConfigChangedPayload> {
    let cid_hex = hex::encode(delta.community_id.0);
    let at_wall_ms = delta.event.at.wall_ms;
    let (channel_id, action, name, write_power) = match &delta.event.kind {
        crate::community_membership::MembershipEventKind::ChannelCreate { ch, nm, wp } => (
            hex::encode(ch),
            ChannelConfigChangeAction::Created,
            Some(nm.clone()),
            Some(*wp),
        ),
        crate::community_membership::MembershipEventKind::ChannelModify { ch, nm, wp } => (
            hex::encode(ch),
            ChannelConfigChangeAction::Modified,
            nm.clone(),
            *wp,
        ),
        crate::community_membership::MembershipEventKind::ChannelDelete { ch } => (
            hex::encode(ch),
            ChannelConfigChangeAction::Deleted,
            None,
            None,
        ),
        _ => return None,
    };
    Some(ChannelConfigChangedPayload {
        community_id: cid_hex,
        channel_id,
        action,
        name,
        write_power,
        at_wall_ms,
    })
}
```

- [ ] **Step 3: Extend `run_community_delta_consumer` to take a SECOND emit callback for channel-config events.**

In `src-tauri/src/lib.rs`, locate `pub async fn run_community_delta_consumer` (around line 8042). Replace its signature + body with the two-callback version:

```rust
/// Drain `delta_rx`. Each delta is projected as EITHER:
///   - `MembershipChange` → `community-members-changed` Tauri event
///     (membership variants: Join/Leave/Invite/Kick/SetPower)
///   - `ChannelConfigChangedPayload` → `channel-config-updated` Tauri
///     event (ZEB-248 Phase 1 channel-config variants)
///
/// Stops cleanly when the channel closes (last sender dropped — typically
/// on `stop_node`).
pub async fn run_community_delta_consumer<FM, FutM, FC, FutC>(
    mut delta_rx: tokio::sync::mpsc::Receiver<
        crate::community_state_sync::CommunityMembershipDelta,
    >,
    mut emit_membership: FM,
    mut emit_channel_config: FC,
) where
    FM: FnMut(CommunityMembersChangedPayload) -> FutM + Send + 'static,
    FutM: std::future::Future<Output = ()> + Send + 'static,
    FC: FnMut(ChannelConfigChangedPayload) -> FutC + Send + 'static,
    FutC: std::future::Future<Output = ()> + Send + 'static,
{
    while let Some(delta) = delta_rx.recv().await {
        if let Some((community_id, change)) = delta_to_change(&delta) {
            let payload = CommunityMembersChangedPayload {
                community_id,
                changes: vec![change],
            };
            emit_membership(payload).await;
        } else if let Some(payload) = delta_to_channel_config_change(&delta) {
            emit_channel_config(payload).await;
        }
    }
}
```

- [ ] **Step 4: Update the consumer-spawn site in `start_node` to pass both callbacks.**

In `src-tauri/src/lib.rs`, locate the existing `tokio::spawn(run_community_delta_consumer(community_delta_rx, move |payload| { ... emit("community-members-changed", ...) }))` block (around line 1265–1280). Replace with:

```rust
                    {
                        let app_for_membership = app.clone();
                        let app_for_channel_config = app.clone();
                        tokio::spawn(run_community_delta_consumer(
                            community_delta_rx,
                            move |payload| {
                                let app = app_for_membership.clone();
                                async move {
                                    if let Err(e) = app.emit("community-members-changed", &payload)
                                    {
                                        tracing::warn!(
                                            error = ?e,
                                            "failed to emit community-members-changed"
                                        );
                                    }
                                }
                            },
                            move |payload| {
                                let app = app_for_channel_config.clone();
                                async move {
                                    if let Err(e) = app.emit("channel-config-updated", &payload) {
                                        tracing::warn!(
                                            error = ?e,
                                            "failed to emit channel-config-updated"
                                        );
                                    }
                                }
                            },
                        ));
                    }
```

- [ ] **Step 5: Add the `create_channel` IPC.**

In `src-tauri/src/lib.rs`, locate a logical place to insert the new IPC — near the existing community IPCs. A good spot is right AFTER the existing `list_community_members` IPC (around line 5460) so all community-management IPCs cluster together. Add:

```rust
/// IPC: create a new channel in a community. Power-gated at mod-tier
/// (POWER_THRESHOLDS.kick = 50). Generates a fresh ChannelId (ULID),
/// signs a ChannelCreate event with the caller's identity, inserts via
/// the community engine's `insert_local_event` (which runs verify_event
/// + materialize + delta-emit, firing `channel-config-updated`).
///
/// Returns the channel_id as a 32-char lowercase hex string. The
/// frontend should rely on the `channel-config-updated` event for
/// state updates (not on the return value alone) — the event carries
/// `name` + `writePower` + `at` for incremental UI updates.
#[tauri::command]
async fn create_channel(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    name: String,
    write_power: u8,
) -> Result<String, String> {
    // Parse community_id hex.
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    // Snapshot NodeState handles.
    let (registry, hlc_tracker, device_id, self_owner, dm_outbox, crdt_state) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.community_registry
                .clone()
                .ok_or("no community_registry — node not running?")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.dm_outbox
                .clone()
                .ok_or("dm_outbox missing — no owner identity?")?,
            g.crdt_state
                .clone()
                .ok_or("crdt_state missing — node not running?")?,
        )
    }; // std lock guard dropped here.

    // Look up the engine.
    let engine_arc = registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| format!("no engine for community {community_id} — not joined or not yet started"))?;

    // Look up admin_addr from owner-state Space row (needed for VerifyContext).
    let admin_addr = {
        let st = crdt_state.lock().await;
        st.spaces
            .get(&space_id)
            .ok_or_else(|| format!("no Space for community {community_id} in owner-state"))?
            .admin_addr
            .ok_or("community Space missing admin_addr (corrupt row?)")?
    };

    // Generate a fresh ChannelId (ULID-shaped). Reuse owner-state-types' EventId
    // generator pattern (16 random bytes — Phase 1 MVP; v3+ may switch to ULID
    // wallclock-based ordering for human-debuggable ids).
    let channel_id: crate::community_membership::ChannelId = {
        let mut buf = [0u8; 16];
        rand::Rng::fill(&mut rand::thread_rng(), &mut buf[..]);
        buf
    };

    // Generate a fresh EventId (same pattern).
    let event_id: crate::community_membership::EventId = {
        let mut buf = [0u8; 16];
        rand::Rng::fill(&mut rand::thread_rng(), &mut buf[..]);
        buf
    };

    // Build + sign the ChannelCreate event.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let prev_hlc = {
        let t = hlc_tracker.lock().await;
        t.get(&device_id).cloned()
    };
    let at = crate::owner_state_types::Hlc {
        wall_ms: now_ms.max(prev_hlc.as_ref().map(|h| h.wall_ms).unwrap_or(0)),
        logical: prev_hlc
            .as_ref()
            .filter(|h| h.wall_ms == now_ms)
            .map(|h| h.logical + 1)
            .unwrap_or(0),
        device_id: device_id.clone(),
    };

    let signing_key = {
        let outbox_g = dm_outbox.lock().await;
        std::sync::Arc::clone(&outbox_g.signing_key)
    };

    let payload = crate::community_membership::EventPayload {
        id: event_id,
        community_id: space_id,
        kind: crate::community_membership::MembershipEventKind::ChannelCreate {
            ch: channel_id,
            nm: name,
            wp: write_power,
        },
        actor: self_owner,
        at: at.clone(),
    };
    let signed = crate::community_membership::sign_event(&payload, signing_key.as_ref())
        .map_err(|e| format!("sign_event: {e}"))?;

    // Insert via the engine. This runs verify_event (returns ChannelAdminInsufficientPower
    // if caller is below mod-tier) and fires the delta on success → consumer emits
    // channel-config-updated.
    let outcome = engine_arc
        .insert_local_event(signed)
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;

    match outcome {
        crate::community_state_crdt::InsertOutcome::Inserted => {
            // Advance the HLC tracker so the next event from this device
            // sees the bumped logical/wall.
            let mut t = hlc_tracker.lock().await;
            t.insert(device_id, at);
            Ok(hex::encode(channel_id))
        }
        crate::community_state_crdt::InsertOutcome::AlreadyKnown => {
            // ChannelId collision with an existing event_id is vanishingly
            // unlikely (16 random bytes); surface as opaque error.
            Err(format!("channel event_id collision: {}", hex::encode(event_id)))
        }
        crate::community_state_crdt::InsertOutcome::Rejected(e) => {
            Err(format!("verify_event rejected ChannelCreate: {e}"))
        }
    }
}
```

- [ ] **Step 6: Register the new IPC in `tauri::generate_handler!`.**

In `src-tauri/src/lib.rs`, locate the `tauri::generate_handler!` macro call (around line 8268, where `list_community_members` is registered). Append `create_channel,` to the handler list — preserving alphabetical order if the existing list is sorted, otherwise appending at the end.

- [ ] **Step 7: Write a captured-payload test for the IPC inline in `src-tauri/src/lib.rs`.**

In `src-tauri/src/lib.rs`, locate the existing inline test module `mod list_community_members_ipc_tests` (around line 8921 — confirm with `grep -n 'mod list_community_members_ipc_tests' src-tauri/src/lib.rs`). Append a new sibling module:

```rust
#[cfg(test)]
mod create_channel_delta_tests {
    use super::*;
    use crate::community_membership::{
        ChannelId, MembershipEventKind, SignedMembershipEvent,
    };
    use crate::community_state_sync::CommunityMembershipDelta;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    #[tokio::test]
    async fn delta_to_channel_config_change_projects_create_modify_delete() {
        let community_id = SpaceId([0x37; 16]);
        let actor = OwnerAddr([0x10; 16]);
        let ch_id: ChannelId = [0xAB; 16];

        // Create.
        let create_event = SignedMembershipEvent {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::ChannelCreate {
                ch: ch_id,
                nm: "general".into(),
                wp: 0,
            },
            actor,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
            sig: [0; 64],
            countersig: None,
        };
        let create_delta = CommunityMembershipDelta {
            community_id,
            event: create_event,
        };
        let payload = delta_to_channel_config_change(&create_delta).expect("create");
        assert_eq!(payload.action, ChannelConfigChangeAction::Created);
        assert_eq!(payload.channel_id, hex::encode(ch_id));
        assert_eq!(payload.name.as_deref(), Some("general"));
        assert_eq!(payload.write_power, Some(0));

        // Modify (name only).
        let modify_event = SignedMembershipEvent {
            id: [0x02; 16],
            community_id,
            kind: MembershipEventKind::ChannelModify {
                ch: ch_id,
                nm: Some("renamed".into()),
                wp: None,
            },
            actor,
            at: Hlc { wall_ms: 2_000, logical: 0, device_id: "a".into() },
            sig: [0; 64],
            countersig: None,
        };
        let payload = delta_to_channel_config_change(&CommunityMembershipDelta {
            community_id,
            event: modify_event,
        })
        .expect("modify");
        assert_eq!(payload.action, ChannelConfigChangeAction::Modified);
        assert_eq!(payload.name.as_deref(), Some("renamed"));
        assert_eq!(payload.write_power, None);

        // Delete.
        let delete_event = SignedMembershipEvent {
            id: [0x03; 16],
            community_id,
            kind: MembershipEventKind::ChannelDelete { ch: ch_id },
            actor,
            at: Hlc { wall_ms: 3_000, logical: 0, device_id: "a".into() },
            sig: [0; 64],
            countersig: None,
        };
        let payload = delta_to_channel_config_change(&CommunityMembershipDelta {
            community_id,
            event: delete_event,
        })
        .expect("delete");
        assert_eq!(payload.action, ChannelConfigChangeAction::Deleted);
        assert_eq!(payload.name, None);
        assert_eq!(payload.write_power, None);
    }

    #[tokio::test]
    async fn delta_to_change_returns_none_for_channel_config() {
        // Channel-config deltas are NOT projected through delta_to_change —
        // they go through delta_to_channel_config_change instead. This
        // guarantees the consumer fan-out fires the right event.
        let community_id = SpaceId([0x37; 16]);
        let actor = OwnerAddr([0x10; 16]);
        let create_event = SignedMembershipEvent {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::ChannelCreate {
                ch: [0xAB; 16],
                nm: "general".into(),
                wp: 0,
            },
            actor,
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
            sig: [0; 64],
            countersig: None,
        };
        let delta = CommunityMembershipDelta {
            community_id,
            event: create_event,
        };
        assert!(delta_to_change(&delta).is_none());
    }

    #[tokio::test]
    async fn run_community_delta_consumer_routes_channel_config_to_correct_callback() {
        // Drive a single ChannelCreate delta through run_community_delta_consumer
        // and assert the channel-config callback fires (not the membership one).
        let (tx, rx) = tokio::sync::mpsc::channel::<CommunityMembershipDelta>(8);

        let captured_membership: Arc<TokioMutex<Vec<CommunityMembersChangedPayload>>> =
            Arc::new(TokioMutex::new(Vec::new()));
        let captured_channel: Arc<TokioMutex<Vec<ChannelConfigChangedPayload>>> =
            Arc::new(TokioMutex::new(Vec::new()));

        let m_clone = captured_membership.clone();
        let c_clone = captured_channel.clone();

        let handle = tokio::spawn(run_community_delta_consumer(
            rx,
            move |payload| {
                let m = m_clone.clone();
                async move {
                    m.lock().await.push(payload);
                }
            },
            move |payload| {
                let c = c_clone.clone();
                async move {
                    c.lock().await.push(payload);
                }
            },
        ));

        let community_id = SpaceId([0x37; 16]);
        let create_event = SignedMembershipEvent {
            id: [0x01; 16],
            community_id,
            kind: MembershipEventKind::ChannelCreate {
                ch: [0xAB; 16],
                nm: "general".into(),
                wp: 0,
            },
            actor: OwnerAddr([0x10; 16]),
            at: Hlc { wall_ms: 1_000, logical: 0, device_id: "a".into() },
            sig: [0; 64],
            countersig: None,
        };
        tx.send(CommunityMembershipDelta {
            community_id,
            event: create_event,
        })
        .await
        .expect("send");

        drop(tx); // close channel so consumer exits cleanly
        handle.await.expect("consumer");

        assert_eq!(captured_membership.lock().await.len(), 0);
        assert_eq!(captured_channel.lock().await.len(), 1);
        assert_eq!(captured_channel.lock().await[0].action, ChannelConfigChangeAction::Created);
    }
}
```

- [ ] **Step 8: Run the new tests.**

```bash
cargo test -p harmony-app --lib create_channel_delta_tests 2>&1 | tail -20
echo "test exit: ${PIPESTATUS[0]}"
# Expected: 3 passed; test exit: 0
```

- [ ] **Step 9: Run the full suite + clippy + fmt.**

```bash
cargo test -p harmony-app --no-fail-fast 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
echo "clippy exit: ${PIPESTATUS[0]}"
# Expected: all green, exit 0 everywhere.
```

- [ ] **Step 10: Commit.**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-248-p1): create_channel IPC + channel-config-updated event

Adds the create_channel Tauri command + the ChannelInfoDto /
ChannelConfigChangedPayload / ChannelConfigChangeAction DTOs.

run_community_delta_consumer now takes two emit callbacks: membership
deltas fan out to the existing community-members-changed event;
channel-config deltas fan out to the new channel-config-updated event.
delta_to_change returns None for channel-config variants (clean
separation; no double-fires).

Per spec §11. Tests cover the projector + consumer fan-out routing.
The end-to-end IPC + emit path is exercised in Task 7's two-engine
integration test.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6 — IPCs `modify_channel`, `delete_channel`, `list_channels`

**Goal:** Three remaining IPCs. `modify_channel` mirrors `create_channel` shape but with partial-update semantics (rejects all-None at the IPC boundary as a no-op error). `delete_channel` follows the metadata-before-irreversible-write rule: read-only verify the channel exists in the materialized state BEFORE signing the irreversible `ChannelDelete`. `list_channels` is a pure read against the engine's materialized state.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add `modify_channel` IPC.**

In `src-tauri/src/lib.rs`, append after `create_channel` (or wherever Task 5 placed it):

```rust
/// IPC: modify a channel's name and/or write_power. Power-gated at
/// mod-tier. At least ONE of `name` or `write_power` must be Some;
/// all-None is rejected at the IPC boundary as a no-op error before
/// any signing.
#[tauri::command]
async fn modify_channel(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    channel_id: String,
    name: Option<String>,
    write_power: Option<u8>,
) -> Result<(), String> {
    // Boundary validation: reject all-None up-front.
    if name.is_none() && write_power.is_none() {
        return Err("modify_channel: must provide name and/or write_power".to_string());
    }

    // Parse hex IDs.
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let ch_bytes: [u8; 16] = hex::decode(&channel_id)
        .map_err(|e| format!("invalid channel_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "channel_id must be 16 bytes (32 hex chars)".to_string())?;

    // Snapshot NodeState handles (same pattern as create_channel).
    let (registry, hlc_tracker, device_id, self_owner, dm_outbox, crdt_state) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.community_registry.clone().ok_or("no community_registry")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.dm_outbox.clone().ok_or("dm_outbox missing")?,
            g.crdt_state.clone().ok_or("crdt_state missing")?,
        )
    };

    let engine_arc = registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| format!("no engine for community {community_id}"))?;

    let admin_addr = {
        let st = crdt_state.lock().await;
        st.spaces.get(&space_id).ok_or_else(|| format!("no Space for community {community_id}"))?.admin_addr.ok_or("admin_addr missing")?
    };

    let event_id: crate::community_membership::EventId = {
        let mut buf = [0u8; 16];
        rand::Rng::fill(&mut rand::thread_rng(), &mut buf[..]);
        buf
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let prev_hlc = {
        let t = hlc_tracker.lock().await;
        t.get(&device_id).cloned()
    };
    let at = crate::owner_state_types::Hlc {
        wall_ms: now_ms.max(prev_hlc.as_ref().map(|h| h.wall_ms).unwrap_or(0)),
        logical: prev_hlc
            .as_ref()
            .filter(|h| h.wall_ms == now_ms)
            .map(|h| h.logical + 1)
            .unwrap_or(0),
        device_id: device_id.clone(),
    };

    let signing_key = {
        let outbox_g = dm_outbox.lock().await;
        std::sync::Arc::clone(&outbox_g.signing_key)
    };

    let payload = crate::community_membership::EventPayload {
        id: event_id,
        community_id: space_id,
        kind: crate::community_membership::MembershipEventKind::ChannelModify {
            ch: ch_bytes,
            nm: name,
            wp: write_power,
        },
        actor: self_owner,
        at: at.clone(),
    };
    let signed = crate::community_membership::sign_event(&payload, signing_key.as_ref())
        .map_err(|e| format!("sign_event: {e}"))?;

    let outcome = engine_arc
        .insert_local_event(signed)
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;

    match outcome {
        crate::community_state_crdt::InsertOutcome::Inserted => {
            let mut t = hlc_tracker.lock().await;
            t.insert(device_id, at);
            Ok(())
        }
        crate::community_state_crdt::InsertOutcome::AlreadyKnown => {
            Err(format!("channel event_id collision: {}", hex::encode(event_id)))
        }
        crate::community_state_crdt::InsertOutcome::Rejected(e) => {
            Err(format!("verify_event rejected ChannelModify: {e}"))
        }
    }
}
```

- [ ] **Step 2: Add `delete_channel` IPC with the metadata-before-irreversible-write rule.**

```rust
/// IPC: delete (tombstone) a channel. Power-gated at mod-tier.
/// Tombstone semantics: the channel stays in the materialized map
/// with `deleted_at` set. Per memory rule "metadata-before-irreversible-
/// write", verify the channel exists in the materialized state BEFORE
/// signing the irreversible ChannelDelete event — so a delete on a
/// nonexistent channel returns an error WITHOUT polluting the CRDT log
/// with a no-op event.
#[tauri::command]
async fn delete_channel(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    channel_id: String,
) -> Result<(), String> {
    // Parse hex IDs.
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let ch_bytes: [u8; 16] = hex::decode(&channel_id)
        .map_err(|e| format!("invalid channel_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "channel_id must be 16 bytes (32 hex chars)".to_string())?;

    // Snapshot NodeState handles.
    let (registry, hlc_tracker, device_id, self_owner, dm_outbox, crdt_state) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.community_registry.clone().ok_or("no community_registry")?,
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.dm_outbox.clone().ok_or("dm_outbox missing")?,
            g.crdt_state.clone().ok_or("crdt_state missing")?,
        )
    };

    let engine_arc = registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| format!("no engine for community {community_id}"))?;

    let admin_addr = {
        let st = crdt_state.lock().await;
        st.spaces.get(&space_id).ok_or_else(|| format!("no Space for community {community_id}"))?.admin_addr.ok_or("admin_addr missing")?
    };

    // METADATA-BEFORE-IRREVERSIBLE-WRITE: read-only verify the channel
    // exists (and isn't already deleted) BEFORE signing the irreversible
    // ChannelDelete event. Mirrors the rule ZEB-265 reinforced — surface
    // the "no such channel" error here, not after the engine inserted a
    // no-op event into the log.
    {
        let materialized = engine_arc.materialized(admin_addr).await;
        match materialized.channels.get(&ch_bytes) {
            None => return Err(format!("no channel {channel_id} in community {community_id}")),
            Some(info) if info.deleted_at.is_some() => {
                return Err(format!("channel {channel_id} is already deleted"));
            }
            Some(_) => {} // ok to proceed
        }
    }

    let event_id: crate::community_membership::EventId = {
        let mut buf = [0u8; 16];
        rand::Rng::fill(&mut rand::thread_rng(), &mut buf[..]);
        buf
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let prev_hlc = {
        let t = hlc_tracker.lock().await;
        t.get(&device_id).cloned()
    };
    let at = crate::owner_state_types::Hlc {
        wall_ms: now_ms.max(prev_hlc.as_ref().map(|h| h.wall_ms).unwrap_or(0)),
        logical: prev_hlc
            .as_ref()
            .filter(|h| h.wall_ms == now_ms)
            .map(|h| h.logical + 1)
            .unwrap_or(0),
        device_id: device_id.clone(),
    };

    let signing_key = {
        let outbox_g = dm_outbox.lock().await;
        std::sync::Arc::clone(&outbox_g.signing_key)
    };

    let payload = crate::community_membership::EventPayload {
        id: event_id,
        community_id: space_id,
        kind: crate::community_membership::MembershipEventKind::ChannelDelete { ch: ch_bytes },
        actor: self_owner,
        at: at.clone(),
    };
    let signed = crate::community_membership::sign_event(&payload, signing_key.as_ref())
        .map_err(|e| format!("sign_event: {e}"))?;

    let outcome = engine_arc
        .insert_local_event(signed)
        .await
        .map_err(|e| format!("engine.insert_local_event: {e}"))?;

    match outcome {
        crate::community_state_crdt::InsertOutcome::Inserted => {
            let mut t = hlc_tracker.lock().await;
            t.insert(device_id, at);
            Ok(())
        }
        crate::community_state_crdt::InsertOutcome::AlreadyKnown => {
            Err(format!("channel event_id collision: {}", hex::encode(event_id)))
        }
        crate::community_state_crdt::InsertOutcome::Rejected(e) => {
            Err(format!("verify_event rejected ChannelDelete: {e}"))
        }
    }
}
```

If `engine_arc.materialized(admin_addr).await` doesn't exist as an exposed method — check the actual `CommunitySyncEngine` API. If it's not directly exposed, the implementer may need to add a thin pass-through or use the existing `materialized_membership_for` helper if there is one (`grep -n 'pub.*materialized\|pub.*materialize_now' src-tauri/src/community_state_sync.rs`). Worst case, add a public method on the engine that returns `MaterializedMembership` from its inner `CommunityState`.

- [ ] **Step 3: Add `list_channels` IPC.**

```rust
/// IPC: list all channels in a community (including tombstoned ones).
/// Read-only; does not require any power level beyond Joined membership
/// (frontend filters tombstones for default view; admin UI surfaces them
/// as deletable-only).
#[tauri::command]
async fn list_channels(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<ChannelInfoDto>, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (registry, crdt_state) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.community_registry.clone().ok_or("no community_registry")?,
            g.crdt_state.clone().ok_or("crdt_state missing")?,
        )
    };

    let engine_arc = registry
        .engine_arc(&space_id)
        .await
        .ok_or_else(|| format!("no engine for community {community_id}"))?;

    let admin_addr = {
        let st = crdt_state.lock().await;
        st.spaces.get(&space_id).ok_or_else(|| format!("no Space for community {community_id}"))?.admin_addr.ok_or("admin_addr missing")?
    };

    let materialized = engine_arc.materialized(admin_addr).await;
    let mut rows: Vec<ChannelInfoDto> = materialized
        .channels
        .iter()
        .map(|(ch_id, info)| ChannelInfoDto {
            channel_id: hex::encode(ch_id),
            name: info.name.clone(),
            write_power: info.write_power,
            created_at: info.created_at.clone(),
            deleted_at: info.deleted_at.clone(),
        })
        .collect();
    // Sort by created_at ascending so #general (auto-created first) is
    // always at the top of the list.
    rows.sort_by(|a, b| {
        a.created_at
            .wall_ms
            .cmp(&b.created_at.wall_ms)
            .then_with(|| a.created_at.logical.cmp(&b.created_at.logical))
            .then_with(|| a.channel_id.cmp(&b.channel_id))
    });
    Ok(rows)
}
```

- [ ] **Step 4: Register the three new IPCs in `tauri::generate_handler!`.**

In `src-tauri/src/lib.rs`, locate the `tauri::generate_handler!` macro call and append `modify_channel,`, `delete_channel,`, `list_channels,` to the list (alongside the `create_channel` added in Task 5).

- [ ] **Step 5: Add a unit test for `delete_channel`'s metadata-before-write guard.**

In the same `mod create_channel_delta_tests` (rename or extend), append:

```rust
    #[tokio::test]
    async fn list_channels_returns_empty_for_fresh_community() {
        // No engine available in unit-test scope — this is exercised in
        // the Task 7 integration test. Sketch left as a documentation
        // anchor.
    }
```

(The IPC-bound logic is hard to test without a full engine harness; the integration test in Task 7 covers the happy path. Step 5 is mostly bookkeeping — the implementer can omit the `list_channels_returns_empty_for_fresh_community` placeholder if they prefer to leave the integration test as the sole coverage.)

- [ ] **Step 6: Run the suite + clippy + fmt.**

```bash
cargo test -p harmony-app --no-fail-fast 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
echo "clippy exit: ${PIPESTATUS[0]}"
# Expected: green; exit 0.
```

- [ ] **Step 7: Commit.**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-248-p1): modify_channel + delete_channel + list_channels IPCs

modify_channel: partial-update with all-None rejection at IPC boundary.
delete_channel: metadata-before-irreversible-write rule — read-only
verify the channel exists (and isn't already tombstoned) BEFORE signing
the irreversible ChannelDelete event.
list_channels: pure read against the engine's materialized state, sorted
by created_at ascending so #general (auto-created first in Task 7) is
always top of list.

All three IPCs registered in tauri::generate_handler!. Engine-bound
end-to-end coverage lives in the Task 7 integration test.

Per spec §11; metadata-before-write rule applied per user memory.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7 — Default-`#general` auto-creation in `create_community_inner`

**Goal:** Extend `create_community_inner` (lib.rs:5734) to atomically emit a `ChannelCreate { name: "general", write_power: 0 }` event immediately after the founding `Join` succeeds. Same engine, same rollback path: if the channel-create insert fails, `shutdown_engine_and_cleanup_persistence` runs to keep the engine + persistence dir consistent. Per spec §11 atomicity language.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Locate the post-`bootstrap_join` insert site.**

In `src-tauri/src/lib.rs`, the `create_community_inner` function (line ~5734) currently does (around lines 5855–5895):

```rust
    let outcome = match engine_arc
        .insert_local_event(minted.bootstrap_join.clone())
        .await
    {
        Ok(o) => o,
        Err(e) => { /* shutdown + return err */ }
    };
    if !matches!(outcome, crate::community_state_crdt::InsertOutcome::Inserted) {
        /* shutdown + return err */
    }

    // ZEB-258: SNAPSHOT-THEN-COMMIT FENCE. ...
```

The default-channel insert lands BETWEEN the `bootstrap_join` insert and the SNAPSHOT-THEN-COMMIT FENCE. This way, the rollback path (`shutdown_engine_and_cleanup_persistence`) handles failure of EITHER insert, and a fence-abort still happens with both events already in the engine (which is fine — both are torn down together).

- [ ] **Step 2: Add the default-channel mint + insert immediately after the bootstrap_join success check.**

After the `if !matches!(outcome, ... Inserted) { ... return Err(...); }` block (around line 5895), but BEFORE the `// ZEB-258: SNAPSHOT-THEN-COMMIT FENCE.` comment (around line 5897), insert:

```rust
    // ZEB-248 Phase 1: atomically auto-create the default #general channel.
    // Same engine-transaction window as the bootstrap_join: if this insert
    // fails, the same shutdown_engine_and_cleanup_persistence rollback runs.
    // The wall_ms is derived from the bootstrap_join's wall_ms + 1 to keep
    // the events deterministically ordered (Join < ChannelCreate) without
    // depending on system-clock progression between the two signs.
    let default_channel_id: crate::community_membership::ChannelId = {
        let mut buf = [0u8; 16];
        rand::Rng::fill(&mut rand::thread_rng(), &mut buf[..]);
        buf
    };
    let default_channel_event_id: crate::community_membership::EventId = {
        let mut buf = [0u8; 16];
        rand::Rng::fill(&mut rand::thread_rng(), &mut buf[..]);
        buf
    };
    let default_channel_at = crate::owner_state_types::Hlc {
        wall_ms: minted.bootstrap_join.at.wall_ms,
        logical: minted.bootstrap_join.at.logical + 1,
        device_id: minted.bootstrap_join.at.device_id.clone(),
    };
    let default_channel_payload = crate::community_membership::EventPayload {
        id: default_channel_event_id,
        community_id: minted.community_id,
        kind: crate::community_membership::MembershipEventKind::ChannelCreate {
            ch: default_channel_id,
            nm: "general".to_string(),
            wp: 0,
        },
        actor: self_owner,
        at: default_channel_at,
    };
    let default_channel_signed =
        crate::community_membership::sign_event(&default_channel_payload, signing_key.as_ref())
            .map_err(|e| {
                // Rollback engine before returning — same pattern as bootstrap_join error path.
                let _ = futures::executor::block_on(
                    community_registry.shutdown_engine_and_cleanup_persistence(&minted.community_id),
                );
                format!("sign default-channel ChannelCreate: {e}")
            })?;

    let default_channel_outcome = match engine_arc
        .insert_local_event(default_channel_signed)
        .await
    {
        Ok(o) => o,
        Err(e) => {
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown_engine_and_cleanup_persistence failed during create_community \
                     rollback (default-channel insert error)"
                );
            }
            return Err(format!("engine.insert_local_event (default channel): {e}"));
        }
    };
    if !matches!(
        default_channel_outcome,
        crate::community_state_crdt::InsertOutcome::Inserted
    ) {
        if let Err(stop_err) = community_registry
            .shutdown_engine_and_cleanup_persistence(&minted.community_id)
            .await
        {
            tracing::warn!(
                error = %stop_err,
                community_id = %hex::encode(minted.community_id.0),
                "shutdown_engine_and_cleanup_persistence failed during create_community \
                 rollback (default-channel not inserted)"
            );
        }
        return Err(format!(
            "default-channel ChannelCreate not inserted (got {default_channel_outcome:?})"
        ));
    }
```

NOTE: the `futures::executor::block_on` in the `sign_event` error path is awkward — `sign_event` is sync but the closure is invoked from async context with `?`. If `futures` isn't already in `harmony-app`'s deps, the cleaner pattern is to hoist the sign to a separate `let signed = ... .map_err(...)?;` block ABOVE the engine insert and handle the rollback separately, like:

```rust
    let default_channel_signed = match crate::community_membership::sign_event(&default_channel_payload, signing_key.as_ref()) {
        Ok(s) => s,
        Err(e) => {
            if let Err(stop_err) = community_registry.shutdown_engine_and_cleanup_persistence(&minted.community_id).await {
                tracing::warn!(error = %stop_err, community_id = %hex::encode(minted.community_id.0), "shutdown failed during sign-default-channel rollback");
            }
            return Err(format!("sign default-channel ChannelCreate: {e}"));
        }
    };
```

The implementer should pick whichever shape mirrors the existing surrounding code better. The KEY invariant is: any error from sign_event → engine.insert_local_event must trigger `shutdown_engine_and_cleanup_persistence` before returning Err.

- [ ] **Step 3: Compile + run unit tests + clippy.**

```bash
cargo build -p harmony-app 2>&1 | tail -10
echo "build exit: ${PIPESTATUS[0]}"
# Expected: build exit: 0

cargo test -p harmony-app --no-fail-fast 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
echo "clippy exit: ${PIPESTATUS[0]}"
# Expected: green; exit 0.
```

End-to-end coverage of the auto-creation lands in Task 8's integration test. This task's verification is "compiles + nothing existing breaks."

- [ ] **Step 4: Commit.**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-248-p1): atomic default #general channel on community create

create_community_inner extends to insert a ChannelCreate {name="general",
write_power=0} event immediately after the founding Join, in the same
engine transaction. Failure at sign or insert triggers the same
shutdown_engine_and_cleanup_persistence rollback as the bootstrap-Join
failure path (ZEB-258 atomicity pattern preserved).

Default channel HLC is bootstrap_join.at + (logical+1) for deterministic
event ordering without system-clock dependence.

Per spec §11. End-to-end round-trip ships in Task 8.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8 — Two-engine integration test

**Goal:** Land `src-tauri/tests/community_channel_config_integration.rs` — the canonical Phase 1 acceptance gate. Two engines wired to the same in-process forwarder; Alice creates a channel; Bob's engine materializes it via the existing state-CRDT sync; mod-tier gating is verified end-to-end (a sub-mod actor's ChannelCreate is rejected when inserted locally — won't even reach the wire); default-#general round-trip is verified by spawning `create_community`-equivalent setup (synthesizing the bootstrap_join + default-channel events directly since the IPC needs full Tauri state).

**Files:**
- Create: `src-tauri/tests/community_channel_config_integration.rs`

- [ ] **Step 1: Stand up the test file with two-engine boilerplate.**

Create `src-tauri/tests/community_channel_config_integration.rs`. Use the structure from `community_invite_only_integration.rs` lines 1–110 as a starting point — `TwoIdentityResolver`, `signing_key_from`, `dup_identity` helpers. Then write:

```rust
//! Two-engine integration test for ZEB-248 Phase 1 channel-config CRDT.
//!
//! Round-trips: Alice creates a channel → state-CRDT publish → Bob's
//! engine merges the published events → Bob materializes the same
//! ChannelInfo. Also verifies (a) mod-tier gating end-to-end (a sub-mod
//! actor's ChannelCreate is locally rejected before publish); (b) the
//! default-#general auto-creation pattern (ChannelCreate immediately
//! after bootstrap_join is materialized on both sides).
//!
//! Test plumbing mirrors community_invite_only_integration.rs and
//! community_open_flow_integration.rs — same TwoIdentityResolver,
//! same publisher_tx ↔ subscriber_rx forwarding shape.

use harmony_app::community_membership::{
    materialize, ChannelId, EventPayload, MembershipEventKind, SignedMembershipEvent,
};
use harmony_app::community_state_sync::{
    CommunityRegistryConfig, CommunitySyncRegistry, IdentityResolver, DEFAULT_DEBOUNCE_MS,
};
use harmony_app::content_store::{ContentStore, RuntimeContentStore};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId, MembershipKey};
use harmony_identity::PrivateIdentity;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

struct TwoIdentityResolver {
    a: (OwnerAddr, [u8; 64]),
    b: (OwnerAddr, [u8; 64]),
}

#[async_trait::async_trait]
impl IdentityResolver for TwoIdentityResolver {
    async fn resolve(&self, addr: &OwnerAddr) -> Option<[u8; 64]> {
        if *addr == self.a.0 {
            Some(self.a.1)
        } else if *addr == self.b.0 {
            Some(self.b.1)
        } else {
            None
        }
    }
}

fn signing_key_from(identity: &PrivateIdentity) -> ed25519_dalek::SigningKey {
    let priv_bytes = identity.to_private_bytes();
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&priv_bytes[32..64]);
    ed25519_dalek::SigningKey::from_bytes(&secret)
}

fn make_test_identity(seed: u8) -> (PrivateIdentity, [u8; 64], OwnerAddr) {
    let private = PrivateIdentity::from_seed(&[seed; 32]);
    let identity_pub = private.identity.to_public_bytes();
    let owner_addr = OwnerAddr(private.identity.address_hash);
    (private, identity_pub, owner_addr)
}
```

- [ ] **Step 2: Add the happy-path test: Alice creates a channel, Bob materializes it.**

Append:

```rust
#[tokio::test]
async fn alice_creates_channel_bob_materializes_via_state_sync() {
    let (alice_priv, alice_pub, alice_addr) = make_test_identity(0xAA);
    let (_bob_priv, bob_pub, bob_addr) = make_test_identity(0xBB);
    let community_id = SpaceId([0x37; 16]);
    let membership_key = MembershipKey::new([0x55; 32]);

    // Build identity resolver shared by both engines.
    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (alice_addr, alice_pub),
        b: (bob_addr, bob_pub),
    });

    // Two CAS stores, two registries.
    let alice_cas = Arc::new(RuntimeContentStore::new());
    let bob_cas = Arc::new(RuntimeContentStore::new());

    let alice_dir = tempfile::tempdir().expect("alice tmpdir");
    let bob_dir = tempfile::tempdir().expect("bob tmpdir");

    // Channels: each engine emits via publisher_tx; we forward A→B and B→A.
    let (alice_pub_tx, mut alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_pub_tx, mut bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);

    let (alice_delta_tx, mut alice_delta_rx) =
        mpsc::channel::<harmony_app::community_state_sync::CommunityMembershipDelta>(32);
    let (bob_delta_tx, mut bob_delta_rx) =
        mpsc::channel::<harmony_app::community_state_sync::CommunityMembershipDelta>(32);

    // Build registries.
    let alice_registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "alice-dev".into(),
        content_store: Arc::clone(&alice_cas) as Arc<dyn ContentStore>,
        identity_resolver: Arc::clone(&resolver),
        identity_dir: alice_dir.path().to_path_buf(),
        debounce_ms: 50,
        error_tx: None,
        delta_tx: Some(alice_delta_tx),
        self_owner: alice_addr,
        signing_key: Arc::new(signing_key_from(&alice_priv)),
    }));
    let bob_registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "bob-dev".into(),
        content_store: Arc::clone(&bob_cas) as Arc<dyn ContentStore>,
        identity_resolver: Arc::clone(&resolver),
        identity_dir: bob_dir.path().to_path_buf(),
        debounce_ms: 50,
        error_tx: None,
        delta_tx: Some(bob_delta_tx),
        self_owner: bob_addr,
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x33; 32])),
    }));

    alice_registry
        .spawn_engine(community_id, membership_key.clone(), alice_addr, false, alice_pub_tx, alice_sub_rx)
        .await
        .expect("alice spawn");
    bob_registry
        .spawn_engine(community_id, membership_key.clone(), alice_addr, false, bob_pub_tx, bob_sub_rx)
        .await
        .expect("bob spawn");

    // Forwarder: alice_pub_rx → bob_sub_tx; bob_pub_rx → alice_sub_tx.
    let _forwarder = tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = alice_pub_rx.recv() => {
                    if let Some(m) = msg {
                        let _ = bob_sub_tx.send(m).await;
                    } else { break; }
                }
                msg = bob_pub_rx.recv() => {
                    if let Some(m) = msg {
                        let _ = alice_sub_tx.send(m).await;
                    } else { break; }
                }
            }
        }
    });

    // Step 1: Alice inserts her bootstrap_join (so she has admin power).
    let alice_engine = alice_registry.engine_arc(&community_id).await.expect("alice engine");

    let alice_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: alice_addr,
        at: Hlc { wall_ms: 1_000, logical: 0, device_id: "alice-dev".into() },
    };
    let alice_join = harmony_app::community_membership::sign_event_with_identity(
        &alice_join_payload, &alice_priv,
    ).expect("sign alice_join");
    alice_engine
        .insert_local_event(alice_join)
        .await
        .expect("alice_join insert");

    // Drain the membership delta (Joined event for alice).
    tokio::time::timeout(Duration::from_secs(2), alice_delta_rx.recv())
        .await
        .expect("alice membership delta timeout")
        .expect("alice membership delta");

    // Step 2: Alice creates a channel.
    let ch_id: ChannelId = [0xAB; 16];
    let alice_create_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            ch: ch_id,
            nm: "general".into(),
            wp: 0,
        },
        actor: alice_addr,
        at: Hlc { wall_ms: 2_000, logical: 0, device_id: "alice-dev".into() },
    };
    let alice_create = harmony_app::community_membership::sign_event_with_identity(
        &alice_create_payload, &alice_priv,
    ).expect("sign alice_create");
    alice_engine
        .insert_local_event(alice_create)
        .await
        .expect("alice_create insert");

    // Step 3: Wait for Alice's channel-config delta.
    let alice_ch_delta = tokio::time::timeout(Duration::from_secs(2), alice_delta_rx.recv())
        .await
        .expect("alice channel delta timeout")
        .expect("alice channel delta");
    assert!(matches!(
        alice_ch_delta.event.kind,
        MembershipEventKind::ChannelCreate { .. }
    ));

    // Step 4: Wait for Bob's engine to materialize the channel via state-sync.
    // Forwarder pumps alice's publish → bob's subscribe; bob debounces, merges,
    // and fires Joined+ChannelCreate deltas. Drain Bob's deltas until we see ChannelCreate.
    let mut saw_channel = false;
    for _ in 0..6 {
        if let Ok(Some(delta)) =
            tokio::time::timeout(Duration::from_secs(2), bob_delta_rx.recv()).await
        {
            if matches!(delta.event.kind, MembershipEventKind::ChannelCreate { ch, .. } if ch == ch_id) {
                saw_channel = true;
                break;
            }
        }
    }
    assert!(saw_channel, "Bob did not materialize Alice's ChannelCreate within 6 deltas");

    // Step 5: Confirm Bob's materialized state has the channel.
    let bob_engine = bob_registry.engine_arc(&community_id).await.expect("bob engine");
    let bob_materialized = bob_engine.materialized(alice_addr).await;
    let info = bob_materialized.channels.get(&ch_id).expect("bob has channel");
    assert_eq!(info.name, "general");
    assert_eq!(info.write_power, 0);
    assert!(info.deleted_at.is_none());
}
```

(Implementer note: if `engine_arc.materialized(admin_addr).await` is not exposed on `CommunitySyncEngine`, add a thin pass-through method on the engine that locks its inner `CommunityState` and calls `.materialized(admin_addr)`. This is part of the IPC-substrate work in Task 5/6 — the engine API needs the same surface either way.)

- [ ] **Step 3: Add the mod-tier rejection test.**

Append:

```rust
#[tokio::test]
async fn sub_mod_actor_channel_create_locally_rejected() {
    // Sub-mod actor (Bob with default power 0) tries to ChannelCreate.
    // verify_event rejects with ChannelAdminInsufficientPower; the engine
    // returns InsertOutcome::Rejected and nothing is published.
    use harmony_app::community_membership::VerifyError;
    use harmony_app::community_state_crdt::InsertOutcome;

    let (alice_priv, alice_pub, alice_addr) = make_test_identity(0xAA);
    let (bob_priv, bob_pub, bob_addr) = make_test_identity(0xBB);
    let community_id = SpaceId([0x37; 16]);
    let membership_key = MembershipKey::new([0x55; 32]);

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (alice_addr, alice_pub),
        b: (bob_addr, bob_pub),
    });
    let bob_cas = Arc::new(RuntimeContentStore::new());
    let bob_dir = tempfile::tempdir().expect("bob tmpdir");
    let (pub_tx, _pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (_sub_tx, sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (delta_tx, _delta_rx) =
        mpsc::channel::<harmony_app::community_state_sync::CommunityMembershipDelta>(32);

    let bob_registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "bob-dev".into(),
        content_store: Arc::clone(&bob_cas) as Arc<dyn ContentStore>,
        identity_resolver: Arc::clone(&resolver),
        identity_dir: bob_dir.path().to_path_buf(),
        debounce_ms: 50,
        error_tx: None,
        delta_tx: Some(delta_tx),
        self_owner: bob_addr,
        signing_key: Arc::new(signing_key_from(&bob_priv)),
    }));
    bob_registry
        .spawn_engine(community_id, membership_key.clone(), alice_addr, false, pub_tx, sub_rx)
        .await
        .expect("bob spawn");

    let bob_engine = bob_registry.engine_arc(&community_id).await.expect("bob engine");

    // Bob has no Join — synthetic non-member ChannelCreate. verify_event
    // should reject as ActorNotJoined first (since Joined is checked
    // before the power gate).
    let bob_payload = EventPayload {
        id: [0x10; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            ch: [0xCC; 16],
            nm: "spam-channel".into(),
            wp: 0,
        },
        actor: bob_addr,
        at: Hlc { wall_ms: 1_000, logical: 0, device_id: "bob-dev".into() },
    };
    let bob_create = harmony_app::community_membership::sign_event_with_identity(
        &bob_payload, &bob_priv,
    ).expect("sign");

    let outcome = bob_engine.insert_local_event(bob_create).await.expect("insert");
    assert!(
        matches!(outcome, InsertOutcome::Rejected(VerifyError::ActorNotJoined)),
        "expected ActorNotJoined rejection, got {outcome:?}"
    );
}
```

- [ ] **Step 4: Add a default-#general round-trip test.**

Append:

```rust
#[tokio::test]
async fn default_general_channel_round_trips_through_state_sync() {
    // Synthesize the create_community_inner pattern: alice's bootstrap_join +
    // default ChannelCreate {name="general", wp=0}, both inserted in the same
    // session. Bob materializes both via state-sync; assert he sees
    // channels["general-channel-id"].name == "general".
    let (alice_priv, alice_pub, alice_addr) = make_test_identity(0xAA);
    let (_bob_priv, bob_pub, bob_addr) = make_test_identity(0xBB);
    let community_id = SpaceId([0x37; 16]);
    let membership_key = MembershipKey::new([0x55; 32]);

    let resolver: Arc<dyn IdentityResolver> = Arc::new(TwoIdentityResolver {
        a: (alice_addr, alice_pub),
        b: (bob_addr, bob_pub),
    });
    let alice_cas = Arc::new(RuntimeContentStore::new());
    let bob_cas = Arc::new(RuntimeContentStore::new());
    let alice_dir = tempfile::tempdir().expect("alice tmpdir");
    let bob_dir = tempfile::tempdir().expect("bob tmpdir");

    let (alice_pub_tx, mut alice_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_sub_tx, alice_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_pub_tx, mut bob_pub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (bob_sub_tx, bob_sub_rx) = mpsc::channel::<Vec<u8>>(64);
    let (alice_delta_tx, _alice_delta_rx) =
        mpsc::channel::<harmony_app::community_state_sync::CommunityMembershipDelta>(32);
    let (bob_delta_tx, mut bob_delta_rx) =
        mpsc::channel::<harmony_app::community_state_sync::CommunityMembershipDelta>(32);

    let alice_registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "alice-dev".into(),
        content_store: Arc::clone(&alice_cas) as Arc<dyn ContentStore>,
        identity_resolver: Arc::clone(&resolver),
        identity_dir: alice_dir.path().to_path_buf(),
        debounce_ms: 50,
        error_tx: None,
        delta_tx: Some(alice_delta_tx),
        self_owner: alice_addr,
        signing_key: Arc::new(signing_key_from(&alice_priv)),
    }));
    let bob_registry = Arc::new(CommunitySyncRegistry::new(CommunityRegistryConfig {
        device_id: "bob-dev".into(),
        content_store: Arc::clone(&bob_cas) as Arc<dyn ContentStore>,
        identity_resolver: Arc::clone(&resolver),
        identity_dir: bob_dir.path().to_path_buf(),
        debounce_ms: 50,
        error_tx: None,
        delta_tx: Some(bob_delta_tx),
        self_owner: bob_addr,
        signing_key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x33; 32])),
    }));

    alice_registry
        .spawn_engine(community_id, membership_key.clone(), alice_addr, false, alice_pub_tx, alice_sub_rx)
        .await
        .expect("alice spawn");
    bob_registry
        .spawn_engine(community_id, membership_key.clone(), alice_addr, false, bob_pub_tx, bob_sub_rx)
        .await
        .expect("bob spawn");

    let _forwarder = tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = alice_pub_rx.recv() => { if let Some(m) = msg { let _ = bob_sub_tx.send(m).await; } else { break; } }
                msg = bob_pub_rx.recv() => { if let Some(m) = msg { let _ = alice_sub_tx.send(m).await; } else { break; } }
            }
        }
    });

    let alice_engine = alice_registry.engine_arc(&community_id).await.expect("alice engine");

    // Insert bootstrap_join then default ChannelCreate (mirrors create_community_inner).
    let alice_join_payload = EventPayload {
        id: [0x01; 16],
        community_id,
        kind: MembershipEventKind::Join,
        actor: alice_addr,
        at: Hlc { wall_ms: 1_000, logical: 0, device_id: "alice-dev".into() },
    };
    let alice_join = harmony_app::community_membership::sign_event_with_identity(
        &alice_join_payload, &alice_priv,
    ).expect("sign join");
    alice_engine.insert_local_event(alice_join).await.expect("join insert");

    let default_ch_id: ChannelId = [0xCD; 16];
    let alice_default_payload = EventPayload {
        id: [0x02; 16],
        community_id,
        kind: MembershipEventKind::ChannelCreate {
            ch: default_ch_id,
            nm: "general".into(),
            wp: 0,
        },
        actor: alice_addr,
        at: Hlc { wall_ms: 1_000, logical: 1, device_id: "alice-dev".into() },
    };
    let alice_default = harmony_app::community_membership::sign_event_with_identity(
        &alice_default_payload, &alice_priv,
    ).expect("sign default");
    alice_engine.insert_local_event(alice_default).await.expect("default insert");

    // Wait for Bob's deltas to include the default ChannelCreate.
    let mut saw_default = false;
    for _ in 0..8 {
        if let Ok(Some(delta)) =
            tokio::time::timeout(Duration::from_secs(2), bob_delta_rx.recv()).await
        {
            if matches!(delta.event.kind, MembershipEventKind::ChannelCreate { ch, .. } if ch == default_ch_id) {
                saw_default = true;
                break;
            }
        }
    }
    assert!(saw_default, "Bob did not materialize default #general within 8 deltas");

    let bob_engine = bob_registry.engine_arc(&community_id).await.expect("bob engine");
    let bob_materialized = bob_engine.materialized(alice_addr).await;
    let info = bob_materialized.channels.get(&default_ch_id).expect("default channel materialized");
    assert_eq!(info.name, "general");
}
```

- [ ] **Step 5: Run the new integration tests.**

```bash
cargo test -p harmony-app --test community_channel_config_integration -- --nocapture 2>&1 | tail -40
echo "test exit: ${PIPESTATUS[0]}"
# Expected: 3 passed; test exit: 0
```

If tests time out, increase the per-recv timeout from 2 s to 5 s — Reticulum-free in-process forwarding should be fast, but CI machines vary. Do NOT silently disable the integration test.

- [ ] **Step 6: Run the full suite + clippy + fmt.**

```bash
cargo test -p harmony-app --no-fail-fast 2>&1 | tail -15
echo "test exit: ${PIPESTATUS[0]}"

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
echo "clippy exit: ${PIPESTATUS[0]}"
# Expected: green; exit 0.
```

- [ ] **Step 7: Commit.**

```bash
git add src-tauri/tests/community_channel_config_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-248-p1): two-engine integration test for channel-config CRDT

Three test scenarios:
  1. alice_creates_channel_bob_materializes_via_state_sync — happy path:
     Alice signs ChannelCreate → state-CRDT publish → forwarder → Bob
     debounces + merges → Bob materializes the same ChannelInfo.
  2. sub_mod_actor_channel_create_locally_rejected — Bob (no Join =
     non-member) attempts ChannelCreate; verify_event rejects with
     ActorNotJoined before publish (the Joined check fires before the
     power gate per verify_event ordering).
  3. default_general_channel_round_trips_through_state_sync — synthesize
     the create_community_inner pattern (bootstrap_join + default
     ChannelCreate); Bob materializes both via state-sync.

Forwarder shape mirrors community_invite_only_integration.rs and
community_open_flow_integration.rs.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9 — Final verification + push + PR

**Goal:** Last green-baseline confirmation across the whole workspace, then push the branch and open the PR.

**Files:** none (verification + push only).

- [ ] **Step 1: Final cargo fmt + clippy + test sweep.**

```bash
cargo fmt --all -- --check
echo "fmt exit: $?"
# Expected: fmt exit: 0

cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
echo "clippy exit: ${PIPESTATUS[0]}"
# Expected: clippy exit: 0

cargo test --workspace --no-fail-fast 2>&1 | tail -20
echo "test exit: ${PIPESTATUS[0]}"
# Expected: all green; test exit: 0
```

If any test fails on this final sweep, STOP and surface to the user — do NOT push a red branch.

- [ ] **Step 2: Verify the commit history is clean.**

```bash
git log --oneline 0d4fca4..HEAD
# Expected (in order, topmost is most recent):
#   <task 8 commit>  test(zeb-248-p1): two-engine integration test ...
#   <task 7 commit>  feat(zeb-248-p1): atomic default #general ...
#   <task 6 commit>  feat(zeb-248-p1): modify_channel + delete_channel + list_channels ...
#   <task 5 commit>  feat(zeb-248-p1): create_channel IPC + channel-config-updated ...
#   <task 4 commit>  test(zeb-248-p1): pin canonical CBOR fixtures ...
#   <task 3 commit>  feat(zeb-248-p1): verify_event gate channel-config ...
#   <task 2 commit>  feat(zeb-248-p1): add ChannelModify + ChannelDelete ...
#   <task 1 commit>  feat(zeb-248-p1): add ChannelId, ChannelInfo, MembershipEventKind::ChannelCreate ...
#   5145484          docs(zeb-248): Sub-C v2 channels-within-communities design spec
```

- [ ] **Step 3: Push the branch.**

```bash
git push -u origin zeb-248-phase1-channel-config-crdt
```

- [ ] **Step 4: Open the PR.**

```bash
gh pr create --title "ZEB-248 Phase 1: channel-config CRDT (backend)" --body "$(cat <<'EOF'
## Summary

Phase 1 of ZEB-248 (Sub-C v2 channels-within-communities). Backend-only: extends the per-community state-CRDT with channel-config events (`ChannelCreate`/`ChannelModify`/`ChannelDelete`), wires four IPCs (`create_channel`/`modify_channel`/`delete_channel`/`list_channels`), and atomically auto-creates a default `#general` channel on community creation. **No UI, no message storage** — those land in Phases 2–4.

This PR atomically delivers the design spec (commit `5145484`) + the per-phase plan + the implementation. Spec at `docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md`; plan at `docs/plans/2026-05-09-zeb-248-phase1-channel-config-crdt-plan.md`.

## What changed

- **`community_membership.rs`** — adds `ChannelId` type, `ChannelInfo` struct, three new `MembershipEventKind` variants (`ChannelCreate`/`ChannelModify`/`ChannelDelete`), `MaterializedMembership.channels` field, materialize branches (incl. tombstone semantics for `ChannelDelete`), `verify_event` gate (`actor_power >= POWER_THRESHOLDS.kick`), new `VerifyError::ChannelAdminInsufficientPower`.
- **`lib.rs`** — `ChannelInfoDto` + `ChannelConfigChangedPayload` + `ChannelConfigChangeAction` DTOs; `delta_to_channel_config_change` projector + extended `delta_to_change` (returns `None` for channel-config); two-callback `run_community_delta_consumer` fans out `community-members-changed` (membership) vs `channel-config-updated` (channel-config); four new IPCs registered in `tauri::generate_handler!`; `create_community_inner` extended for atomic default-`#general` insert with the same shutdown-and-cleanup rollback pattern.
- **`tests/community_membership_unit.rs`** — 10 new tests: 4 round-trip + 3 materialize + 3 verify_event.
- **`tests/wire_format_community_fixtures.rs`** — 5 new pinned canonical CBOR fixtures.
- **`tests/community_channel_config_integration.rs`** (new file) — 3 two-engine integration scenarios: happy-path channel materialization, sub-mod rejection, default-#general round-trip.

## Test plan

- [ ] `cargo test --workspace --no-fail-fast` passes locally
- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] All five wire-format fixtures pin specific hex bytes (catches future drift)
- [ ] CI workflows (Rust fmt/clippy/test, MSRV) all pass

## Phase context

Parent: [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) Sub-C v2 channels-within-communities.
Predecessor: [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) Sub-C v1 (governance primitive — shipped 2026-05-09).
Phase 1 sub-ticket: filed by reviewer at PR open.

Phases 2–4 are gated on this merge:
- **Phase 2:** ChannelLog data plane (`SignedChannelEvent`, `ChannelKey` HKDF, segmented persistence, replay tracker, `verify_channel_event` — backend, no Zenoh)
- **Phase 3:** ChannelLog Zenoh transport (live broadcast + queryable backfill, `ChannelLogRegistry`, message IPCs + events)
- **Phase 4:** Channel UI (`CommunityView` three-column layout, dialogs, `ChannelMessageService`, App.svelte routing)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Capture PR URL + report to user.**

The `gh pr create` command prints the PR URL on success. Report it back to the user as the task-completion artifact.

- [ ] **Step 6: No commit — Task 9 is push + PR only.**

---

## Spec coverage check (controller-only)

After all tasks land, the controller verifies each spec §14 Phase 1 deliverable maps to a task:

| Spec §14 Phase 1 deliverable | Implementing task |
|---|---|
| Add `ChannelId` type + `MembershipEventKind` variants | Tasks 1 + 2 |
| Add `ChannelInfo` struct | Task 1 |
| Extend `MaterializedMembership` with `channels: BTreeMap<ChannelId, ChannelInfo>` | Task 1 |
| Extend `materialize` (3 new branches; ChannelDelete tombstones) | Tasks 1 + 2 |
| Extend `verify_event` (mod-tier gate; new VerifyError variant) | Task 3 |
| IPC `create_channel` | Task 5 |
| IPC `modify_channel` | Task 6 |
| IPC `delete_channel` (with metadata-before-irreversible-write) | Task 6 |
| IPC `list_channels` | Task 6 |
| Tauri event `channel-config-updated` (consumer fan-out) | Task 5 |
| Default `#general` auto-create in `create_community_inner` (atomic) | Task 7 |
| Wire-format fixtures (5 new pinned) | Task 4 |
| Two-engine integration test (3 scenarios) | Task 8 |

All deliverables covered.
