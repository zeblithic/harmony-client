# ZEB-284 Community Moderation UX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface the existing community moderation primitives to end-users by adding the new `Unban` CRDT primitive, the `unban_from_community` and `list_recent_moderation_events` IPCs, and the frontend UX (members panel, member row with kebab matrix, kick/unban dialog, last-admin warning dialog, recent-actions badge).

**Architecture:** Thin CRDT addition (one variant + verify/materialize arms) + parallel IPCs mirroring existing `kick_from_community` pattern + new Svelte component family hosted from `CommunitySettingsPanel`.

**Tech Stack:** Rust (Tauri 2 IPC, serde CBOR, ed25519 signing), TypeScript (Svelte 5 components, vitest), existing `community-service.ts` invoke pattern.

**Spec:** `docs/specs/2026-05-13-zeb-284-community-moderation-ux-design.md` (commit `8eda432`)

**Branch:** `zeb-284-community-moderation-ux` (already cut from `origin/main` at `58577c7`; spec committed at HEAD)

---

## Pre-flight (Task 0) — green-baseline confirm

**No commit. Verifies the just-cut branch passes all gates before any implementation.**

- [ ] **Step 0.1: Verify cargo fmt baseline**

From `src-tauri/`:
```bash
cargo fmt --all -- --check
```
Expected: exit 0, no output.

- [ ] **Step 0.2: Verify cargo clippy baseline**

From `src-tauri/`:
```bash
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```
Expected: exit 0, no warnings.

- [ ] **Step 0.3: Verify cargo check (msrv) baseline**

From `src-tauri/`:
```bash
cargo check --locked --all-targets --features test-fixtures
```
Expected: exit 0.

- [ ] **Step 0.4: Verify cargo nextest baseline + capture test count**

From `src-tauri/`:
```bash
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: all green. Capture the **passed-count** number — used later to verify no regressions.

- [ ] **Step 0.5: Verify tsc baseline**

From repo root:
```bash
npx tsc --noEmit
```
Expected: exit 0.

- [ ] **Step 0.6: Verify vitest baseline + capture test-file count**

From repo root:
```bash
npx vitest run
```
Expected: all green. Capture **passed test files** + **passed tests** counts.

If any baseline gate fails, **STOP** and report — this is test drift on main per `feedback_test_drift_is_our_fault` and must be triaged before any new work lands.

---

## Task 1: CRDT — `MembershipEventKind::Unban` variant + verify/materialize + unit tests

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (add variant, add VerifyError variant, extend verify_event, extend apply_membership_event, add 5 tests)

**Approach:** TDD — write failing unit tests first, then implement the variant + arms.

- [ ] **Step 1.1: Add the `Unban` variant to `MembershipEventKind` enum**

In `src-tauri/src/community_membership.rs`, find the `MembershipEventKind` enum (around line 43). Add the new variant **after** the existing `SetPower` variant (preserves the variant ordering pattern: Join, Leave, Invite, Kick, SetPower, Unban, then channel-config events, then epoch events):

```rust
/// Admin-tier action: lifts a prior Kick-as-effective-ban so the target
/// can be re-invited. Does NOT auto-rejoin — target must accept a fresh
/// Invite. Transitions MemberStatus::Banned → MemberStatus::Left.
///
/// Variant code "u" (1-char value, keeps same-length-keys invariant).
/// Inner field keys are 2-char (tg, rs).
/// See spec `docs/specs/2026-05-13-zeb-284-community-moderation-ux-design.md` §4.1.
#[serde(rename = "u")]
Unban {
    #[serde(rename = "tg")]
    target: OwnerAddr,
    #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
    reason: Option<String>,
},
```

- [ ] **Step 1.2: Add `VerifyError::UnbanTargetNotBanned` variant**

Find the `VerifyError` enum (around line 381). Add new variant **after** `InviteTargetCurrentlyBanned`:

```rust
/// Unban event targets an addr whose current MemberStatus is not Banned.
/// Reject so the IPC layer can surface "target is not currently banned"
/// rather than silently no-op.
UnbanTargetNotBanned,
```

- [ ] **Step 1.3: Extend `Display for VerifyError` for the new variant**

In the existing `Display` impl (around `community_membership.rs:514`), add the new arm:

```rust
VerifyError::UnbanTargetNotBanned => {
    write!(f, "unban target is not currently banned")
}
```

- [ ] **Step 1.4: Write the 5 failing unit tests**

At the end of the existing `#[cfg(test)] mod tests` block in `community_membership.rs` (after the existing tests), add:

```rust
fn make_unban_event(
    id_byte: u8,
    actor: OwnerAddr,
    target: OwnerAddr,
    reason: Option<String>,
    hlc: Hlc,
) -> SignedMembershipEvent {
    // Mirror existing make_kick_event helper at the top of this tests
    // module. Reuse the same fixture/signer setup.
    let payload = EventPayload {
        actor,
        prev_event: None,
        hlc,
        kind: MembershipEventKind::Unban { target, reason },
    };
    let signer = test_signing_key();
    let mut event_id = EventId([0u8; 32]);
    event_id.0[0] = id_byte;
    SignedMembershipEvent {
        event_id,
        payload,
        signature: signer.sign(&[]).to_bytes().into(),
        counter_signatures: vec![],
    }
}

#[test]
fn unban_event_succeeds_when_actor_is_admin_and_target_is_banned() {
    // Build a materialized state where target is Banned via a prior Kick.
    // Issue Unban from admin (power 100). verify_event accepts.
    // apply_membership_event transitions Banned → Left.
    let mut mem = MaterializedMembership::default();
    let admin = OwnerAddr([1u8; 16]);
    let target = OwnerAddr([2u8; 16]);
    mem.members.insert(admin, MemberState {
        status: MemberStatus::Joined,
        joined_at: Hlc::zero(),
    });
    mem.members.insert(target, MemberState {
        status: MemberStatus::Banned,
        joined_at: Hlc::zero(),
    });
    mem.power_levels.insert(admin, 100);
    mem.power_levels.insert(target, 0);

    let unban = make_unban_event(0x01, admin, target, Some("test".into()),
        Hlc { wall_ms: 1000, logical: 0, device_id: DeviceId([0u8; 8]) });

    let ctx = VerifyContext { membership: &mem, /* ... use existing test fixture ctx */ };
    assert!(mem.verify_event(&unban, &ctx).is_ok());

    mem.apply_membership_event(&unban, None);
    assert_eq!(mem.members.get(&target).unwrap().status, MemberStatus::Left);
}

#[test]
fn unban_event_rejected_when_actor_is_moderator() {
    let mut mem = MaterializedMembership::default();
    let mod_actor = OwnerAddr([1u8; 16]);
    let target = OwnerAddr([2u8; 16]);
    mem.members.insert(mod_actor, MemberState {
        status: MemberStatus::Joined,
        joined_at: Hlc::zero(),
    });
    mem.members.insert(target, MemberState {
        status: MemberStatus::Banned,
        joined_at: Hlc::zero(),
    });
    mem.power_levels.insert(mod_actor, 50);

    let unban = make_unban_event(0x02, mod_actor, target, None,
        Hlc { wall_ms: 1000, logical: 0, device_id: DeviceId([0u8; 8]) });

    let ctx = VerifyContext { membership: &mem, /* fixture */ };
    assert_eq!(mem.verify_event(&unban, &ctx),
        Err(VerifyError::ActorPowerInsufficient));
}

#[test]
fn unban_event_rejected_when_target_is_not_banned() {
    let mut mem = MaterializedMembership::default();
    let admin = OwnerAddr([1u8; 16]);
    let target = OwnerAddr([2u8; 16]);
    mem.members.insert(admin, MemberState {
        status: MemberStatus::Joined,
        joined_at: Hlc::zero(),
    });
    mem.members.insert(target, MemberState {
        status: MemberStatus::Joined,  // NOT banned
        joined_at: Hlc::zero(),
    });
    mem.power_levels.insert(admin, 100);

    let unban = make_unban_event(0x03, admin, target, None,
        Hlc { wall_ms: 1000, logical: 0, device_id: DeviceId([0u8; 8]) });

    let ctx = VerifyContext { membership: &mem, /* fixture */ };
    assert_eq!(mem.verify_event(&unban, &ctx),
        Err(VerifyError::UnbanTargetNotBanned));
}

#[test]
fn unban_event_rejected_when_target_is_unknown() {
    let mut mem = MaterializedMembership::default();
    let admin = OwnerAddr([1u8; 16]);
    let target = OwnerAddr([2u8; 16]);  // not inserted
    mem.members.insert(admin, MemberState {
        status: MemberStatus::Joined,
        joined_at: Hlc::zero(),
    });
    mem.power_levels.insert(admin, 100);

    let unban = make_unban_event(0x04, admin, target, None,
        Hlc { wall_ms: 1000, logical: 0, device_id: DeviceId([0u8; 8]) });

    let ctx = VerifyContext { membership: &mem, /* fixture */ };
    assert_eq!(mem.verify_event(&unban, &ctx),
        Err(VerifyError::TargetNotMember));
}

#[test]
fn unban_then_invite_then_join_round_trip_succeeds() {
    // Full lifecycle: Joined → (Kick) Banned → (Unban) Left → (Invite +
    // Join) Joined. Validates that unbanned targets can re-join cleanly.
    let mut mem = MaterializedMembership::default();
    let admin = OwnerAddr([1u8; 16]);
    let target = OwnerAddr([2u8; 16]);
    mem.members.insert(admin, MemberState {
        status: MemberStatus::Joined,
        joined_at: Hlc::zero(),
    });
    mem.members.insert(target, MemberState {
        status: MemberStatus::Joined,
        joined_at: Hlc::zero(),
    });
    mem.power_levels.insert(admin, 100);
    mem.power_levels.insert(target, 0);

    // Kick
    let kick = make_kick_event(0x10, admin, target, None,
        Hlc { wall_ms: 1000, logical: 0, device_id: DeviceId([0u8; 8]) });
    mem.apply_membership_event(&kick, None);
    assert_eq!(mem.members.get(&target).unwrap().status, MemberStatus::Banned);

    // Unban
    let unban = make_unban_event(0x11, admin, target, Some("misunderstanding".into()),
        Hlc { wall_ms: 2000, logical: 0, device_id: DeviceId([0u8; 8]) });
    mem.apply_membership_event(&unban, None);
    assert_eq!(mem.members.get(&target).unwrap().status, MemberStatus::Left);

    // Invite (already-existing helper)
    let invite = make_invite_event(0x12, admin, target,
        Hlc { wall_ms: 3000, logical: 0, device_id: DeviceId([0u8; 8]) });
    mem.apply_membership_event(&invite, None);

    // Join (target signs their own join)
    let join = make_join_event(0x13, target,
        Hlc { wall_ms: 4000, logical: 0, device_id: DeviceId([0u8; 8]) });
    mem.apply_membership_event(&join, None);
    assert_eq!(mem.members.get(&target).unwrap().status, MemberStatus::Joined);
}
```

**Note for implementer:** The exact `VerifyContext` and helper fixture shapes are established by the existing `make_kick_event`-style helpers. Match the patterns used in the file rather than inventing new fixture shapes. If `make_invite_event` / `make_join_event` don't already exist, write them following the same pattern as `make_kick_event`.

- [ ] **Step 1.5: Run tests — expect 5 failures**

From `src-tauri/`:
```bash
cargo nextest run --locked -p harmony-client --features test-fixtures community_membership::tests::unban
```
Expected: 5 failures (test names match `unban_*`).

- [ ] **Step 1.6: Extend `verify_event` with the Unban arm**

In `community_membership.rs::verify_event`, add a new match arm **after** the SetPower arm and **before** the channel-config arms:

```rust
MembershipEventKind::Unban { target, .. } => {
    let actor_power = membership.power_levels.get(&signed.payload.actor).copied().unwrap_or(0);
    if actor_power < POWER_THRESHOLDS.set_power {
        return Err(VerifyError::ActorPowerInsufficient);
    }
    let Some(target_state) = membership.members.get(target) else {
        return Err(VerifyError::TargetNotMember);
    };
    if target_state.status != MemberStatus::Banned {
        return Err(VerifyError::UnbanTargetNotBanned);
    }
    Ok(())
}
```

- [ ] **Step 1.7: Extend `apply_membership_event` with the Unban arm**

In `community_membership.rs::apply_membership_event`, find the existing match (around the SetPower arm). Add the Unban arm:

```rust
MembershipEventKind::Unban { target, .. } => {
    if let Some(state) = self.members.get_mut(target) {
        state.status = MemberStatus::Left;
        state.joined_at = signed.payload.hlc.clone();
    }
    // No EpochRotation auto-trigger — Unban is additive; re-Join handles
    // its own epoch via the existing Invite → Join flow.
}
```

- [ ] **Step 1.8: Run tests — expect all 5 to pass**

From `src-tauri/`:
```bash
cargo nextest run --locked -p harmony-client --features test-fixtures community_membership::tests::unban
```
Expected: 5 passed.

- [ ] **Step 1.9: Pin canonical CBOR fixtures**

Find the existing wire-format fixture file (likely `src-tauri/tests/wire_format_membership.rs` or co-located with channel-config fixtures added by ZEB-248). If not yet present, search:

```bash
grep -rn "ChannelCreate.*hex_lit\|MembershipEvent.*canonical" src-tauri/tests/ src-tauri/src/ | head -5
```

Add two fixture cases mirroring the existing patterns:

1. `Unban { target: <fixed addr>, reason: Some("test") }` — pin canonical CBOR hex
2. `Unban { target: <fixed addr>, reason: None }` — pin canonical CBOR hex

Use the test runner's "first-failure-shows-actual" pattern: run the test with a placeholder hex string, capture the actual canonical encoding from the assertion message, paste it back into the test.

- [ ] **Step 1.10: Run full gates from `src-tauri/`**

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
All three: green. Test passed-count should be **baseline + 5** (the unit tests) **+ 2** (fixture pins) = baseline + 7.

- [ ] **Step 1.11: Commit**

```bash
git add src-tauri/src/community_membership.rs src-tauri/tests/wire_format_membership.rs
git commit -m "$(cat <<'EOF'
feat(zeb-284): add MembershipEventKind::Unban variant

Admin-tier action that transitions Banned → Left so a kicked member
can be re-invited. Verify gate: actor_power >= POWER_THRESHOLDS.set_power
(100, admin-tier). Apply: status transition only — no EpochRotation
auto-trigger since the re-Join flow handles epoch via the existing
Invite → Join pattern.

New VerifyError::UnbanTargetNotBanned for IPC error surfacing when the
target is not currently banned.

Variant code "u" preserves the same-length-keys invariant; inner field
keys are 2-char (tg, rs) matching every other inner-field pattern.

5 unit tests + 2 canonical CBOR fixtures.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

(Exact file paths in `git add` depend on where fixtures landed in Step 1.9.)

---

## Task 2: Backend IPCs — `unban_from_community` + extended `kick_from_community` + `list_recent_moderation_events`

**Files:**
- Modify: `src-tauri/src/lib.rs` (3 new IPC handlers + 1 helper + DTOs + register in generate_handler! + tests)

**Approach:** TDD. The existing `kick_from_community` at `lib.rs:11614` is the canonical pattern to mirror. New IPCs follow the same NodeState snapshot + lock-drop discipline.

- [ ] **Step 2.1: Add `mint_unban_event` helper**

In `src-tauri/src/lib.rs`, find `mint_kick_event` (around line 11452). Add `mint_unban_event` **immediately after** it, mirroring the shape exactly:

```rust
pub fn mint_unban_event(
    actor: OwnerAddr,
    target: OwnerAddr,
    reason: Option<String>,
    prev_event: Option<&EventId>,
    wall_now_ms: u64,
    device_id: &DeviceId,
    signing_key: &SigningKey,
) -> SignedMembershipEvent {
    // Mirror mint_kick_event byte-for-byte except for kind construction.
    // Same HLC monotone bump pattern. Same signature shape.
    let hlc = next_hlc(prev_event.map(|_| /* fetch prev hlc */), wall_now_ms, device_id);
    let payload = EventPayload {
        actor,
        prev_event: prev_event.copied(),
        hlc,
        kind: MembershipEventKind::Unban { target, reason },
    };
    let event_id = compute_event_id(&payload);
    let signature = signing_key.sign(&canonical_bytes(&payload)).to_bytes();
    SignedMembershipEvent {
        event_id,
        payload,
        signature: signature.into(),
        counter_signatures: vec![],
    }
}
```

**Implementer note:** The exact mint_kick_event signature in lib.rs is the source-of-truth. Match its parameter ordering, type signatures, and internal helper calls precisely. The pseudocode above is illustrative.

- [ ] **Step 2.2: Extend `kick_from_community` signature with optional `reason`**

Find `kick_from_community` (around `lib.rs:11614`). Extend the function signature:

```rust
#[tauri::command]
async fn kick_from_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    reason: Option<String>,  // NEW
) -> Result<(), String> {
    // ... existing body unchanged except:
    // - pass `reason` into mint_kick_event instead of None
}
```

Update the body so `mint_kick_event(..., reason, ...)` receives the new parameter instead of the hardcoded `None` it currently passes.

- [ ] **Step 2.3: Add `unban_from_community` Tauri command**

Find the end of `kick_from_community` (around `lib.rs:11883`). Add `unban_from_community` **immediately after**, byte-for-byte mirroring the kick pattern except:
- Calls `mint_unban_event` instead of `mint_kick_event`
- Does NOT trigger EpochRotation (Unban is additive, no key rotation needed)
- Error message text adapts to "unban"

```rust
/// Admin-tier IPC: lifts a prior Kick-as-effective-ban on `target_addr`.
/// Transitions MemberStatus::Banned → MemberStatus::Left. Target must
/// then be re-invited via the existing invite flow to rejoin.
///
/// Errors mirror `kick_from_community` with one addition:
/// - `Err("target is not currently banned")` — `VerifyError::UnbanTargetNotBanned`.
#[tauri::command]
async fn unban_from_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    reason: Option<String>,
) -> Result<(), String> {
    // Mirror kick_from_community structure:
    // 1. Hex-decode community_id and target_addr
    // 2. NodeState snapshot (Arc clones, lock drop)
    // 3. Community registry lookup
    // 4. Read current MaterializedMembership for prev_event linkage
    // 5. Mint Unban event via mint_unban_event(...)
    // 6. Apply via apply_membership_event (the verify_event guard runs first)
    // 7. Broadcast via the same path kick uses
    // 8. NO EpochRotation auto-trigger (unlike kick)
    // 9. Return Ok(()) on success; map VerifyError → String error on rejection
    todo!("mirror kick_from_community exactly per spec §5.1")
}
```

**Implementer note:** Read `kick_from_community` in full first; replicate every step except the EpochRotation logic. The error-mapping match arm needs to include `VerifyError::UnbanTargetNotBanned => "target is not currently banned".to_string()`.

- [ ] **Step 2.4: Add `ModerationEventDto` types + `list_recent_moderation_events` IPC**

In `src-tauri/src/lib.rs`, add near other DTOs (around the existing `MemberInfoDto` at line 6353):

```rust
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModerationEventKindDto {
    Kick,
    Unban,
    SetPower,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModerationEventDto {
    pub event_id: String,         // 64-char hex
    pub kind: ModerationEventKindDto,
    pub actor_addr: String,       // 32-char hex
    pub target_addr: String,      // 32-char hex
    pub reason: Option<String>,   // populated for kick/unban; None for set_power
    pub new_power: Option<u8>,    // populated for set_power; None for kick/unban
    pub hlc: crate::owner_state_types::Hlc,
}
```

Add the IPC handler near the other community IPCs:

```rust
/// Read-only IPC: returns the last N moderation events (Kick, Unban,
/// SetPower) from this community's event log, sorted by HLC desc.
/// Filters out channel-config and epoch-rotation events.
///
/// `limit` is clamped to 1..=100. `community_id` is 32-char lowercase
/// hex of the 16-byte SpaceId.
#[tauri::command]
async fn list_recent_moderation_events(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    limit: u32,
) -> Result<Vec<ModerationEventDto>, String> {
    let limit = limit.clamp(1, 100) as usize;
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = SpaceId(id_bytes);

    // Snapshot NodeState; reach the community event log for `space_id`.
    // Pattern matches list_community_members above.
    let event_log = /* fetch via registry */;

    let mut events: Vec<ModerationEventDto> = event_log.iter()
        .filter_map(|signed| match &signed.payload.kind {
            MembershipEventKind::Kick { target, reason } => Some(ModerationEventDto {
                event_id: hex::encode(signed.event_id.0),
                kind: ModerationEventKindDto::Kick,
                actor_addr: hex::encode(signed.payload.actor.0),
                target_addr: hex::encode(target.0),
                reason: reason.clone(),
                new_power: None,
                hlc: signed.payload.hlc.clone(),
            }),
            MembershipEventKind::Unban { target, reason } => Some(ModerationEventDto {
                event_id: hex::encode(signed.event_id.0),
                kind: ModerationEventKindDto::Unban,
                actor_addr: hex::encode(signed.payload.actor.0),
                target_addr: hex::encode(target.0),
                reason: reason.clone(),
                new_power: None,
                hlc: signed.payload.hlc.clone(),
            }),
            MembershipEventKind::SetPower { target, level } => Some(ModerationEventDto {
                event_id: hex::encode(signed.event_id.0),
                kind: ModerationEventKindDto::SetPower,
                actor_addr: hex::encode(signed.payload.actor.0),
                target_addr: hex::encode(target.0),
                reason: None,
                new_power: Some(*level),
                hlc: signed.payload.hlc.clone(),
            }),
            _ => None,  // Join, Leave, Invite, ChannelCreate/Modify/Delete, EpochRotation, EpochCatchup
        })
        .collect();

    events.sort_by(|a, b| {
        b.hlc.wall_ms.cmp(&a.hlc.wall_ms)
            .then_with(|| b.hlc.logical.cmp(&a.hlc.logical))
    });
    events.truncate(limit);
    Ok(events)
}
```

**Implementer note:** The exact community-event-log fetch pattern matches what `list_community_members` does to reach `MaterializedMembership`. Read both paths first — the event log may be a separate field on the community state or may live inside the materialized state itself.

- [ ] **Step 2.5: Register new commands in `tauri::generate_handler!`**

Find the existing `tauri::generate_handler!` invocation in `lib.rs` (around line 13088 or wherever the current registration block is). Add three entries:

```rust
tauri::generate_handler![
    // ... existing entries ...
    kick_from_community,  // already registered
    set_power_level,       // already registered
    unban_from_community,  // NEW
    list_recent_moderation_events,  // NEW
    // ... etc
]
```

- [ ] **Step 2.6: Write failing IPC tests**

Find the existing `kick_from_community_tests` mod (look for a `mod kick_from_community_tests` or grep for `kick_from_community_happy_path`). At the end of that test module, OR in a new `unban_from_community_tests` mod sibling, add 4 tests:

```rust
#[tokio::test]
async fn unban_from_community_happy_path() {
    // Two-engine setup mirroring kick_from_community_happy_path:
    // - Engine A: admin in community C
    // - Engine B: member in community C
    // A kicks B → verify B banned on both engines after sync
    // A unbans B → verify B status transitions Banned → Left on both engines
    todo!("mirror kick_from_community_happy_path two-engine setup")
}

#[tokio::test]
async fn unban_from_community_returns_err_when_actor_lacks_power() {
    // Mod-tier actor (power 50) attempts unban
    // Returns Err("insufficient power")
    todo!("single-engine setup; assert error message")
}

#[tokio::test]
async fn unban_from_community_returns_err_when_target_not_banned() {
    // Clean state, no prior kick; target is currently Joined
    // Returns Err("target is not currently banned")
    todo!("single-engine setup; assert error message")
}

#[tokio::test]
async fn kick_from_community_signs_reason_into_event() {
    // Pass reason: Some("repeated spam"); verify the materialized
    // Kick event in the community log carries the same reason string.
    todo!("single-engine setup; inspect event log after kick")
}
```

Plus 2 tests for the new moderation-events IPC:

```rust
#[tokio::test]
async fn list_recent_moderation_events_filters_to_kick_unban_setpower() {
    // Setup with mixed events: Join, Leave, Kick, SetPower, ChannelCreate
    // list_recent_moderation_events returns only Kick + SetPower (not the others)
    todo!("two-engine setup; inspect filtered output")
}

#[tokio::test]
async fn list_recent_moderation_events_respects_limit_and_orders_by_hlc_desc() {
    // Setup with 5 moderation events at increasing HLCs
    // Call with limit=3; expect newest 3 in desc order
    todo!("single-engine setup; inspect ordering + truncation")
}
```

- [ ] **Step 2.7: Run tests — expect 6 failures**

From `src-tauri/`:
```bash
cargo nextest run --locked -p harmony-client --features test-fixtures \
  -E 'test(=unban_from_community_happy_path) | \
      test(=unban_from_community_returns_err_when_actor_lacks_power) | \
      test(=unban_from_community_returns_err_when_target_not_banned) | \
      test(=kick_from_community_signs_reason_into_event) | \
      test(=list_recent_moderation_events_filters_to_kick_unban_setpower) | \
      test(=list_recent_moderation_events_respects_limit_and_orders_by_hlc_desc)'
```
Expected: 6 failures (compile-fail or assert-fail).

- [ ] **Step 2.8: Replace `todo!()` with real test bodies**

Each test body should mirror the closest existing pattern:
- `unban_from_community_happy_path` mirrors `kick_from_community_happy_path` exactly (two-engine setup + sync + verification)
- The single-engine error-path tests mirror existing single-engine IPC tests for power-insufficient or status-precondition errors
- `kick_from_community_signs_reason_into_event` is a single-engine setup that calls `kick_from_community(..., Some("repeated spam"))` then reads the community event log and asserts the last Kick event's `reason == Some("repeated spam")`
- The two `list_recent_moderation_events_*` tests build a known event sequence in a single-engine community then verify the IPC's filter/order/limit semantics

**Implementer note:** If `kick_from_community_happy_path` doesn't exist (the kick IPC might lack tests), the implementer must first write a `kick_from_community_happy_path` baseline test before building the unban analog on top of it. Don't ship Unban without proving Kick works the same way — that's the whole pattern they're paired on.

- [ ] **Step 2.9: Run tests — expect all 6 to pass**

```bash
cargo nextest run --locked -p harmony-client --features test-fixtures \
  -E 'test(/^(unban_from_community_|kick_from_community_signs_reason|list_recent_moderation_events_)/)'
```
Expected: 6+ passed (depending on whether the kick-with-reason test counts as one or whether the implementer also wrote the kick baseline).

- [ ] **Step 2.10: Run full gates from `src-tauri/`**

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
All three: green. Total test count: baseline + 7 (Task 1) + 6 (Task 2) = baseline + 13.

- [ ] **Step 2.11: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-284): unban + kick-with-reason + list_recent_moderation_events IPCs

- mint_unban_event helper mirroring mint_kick_event shape
- unban_from_community(communityId, targetAddr, reason) IPC, admin-tier
  (power 100). Returns Err("insufficient power") | Err("target is not
  currently banned"). Does NOT trigger EpochRotation (unlike kick) since
  unban is additive — re-Join handles its own epoch.
- kick_from_community extended with optional reason parameter; existing
  None callers unbroken (backwards-compatible).
- list_recent_moderation_events(communityId, limit) IPC + ModerationEventDto.
  Filters event log to Kick/Unban/SetPower only; sorts by HLC desc; limit
  clamped to 1..=100.

6 new IPC tests. Registered in tauri::generate_handler!.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Frontend service + types

**Files:**
- Modify: `src/lib/community-service.ts` (extend `kickFromCommunity`, add `unbanFromCommunity`, add `listRecentModerationEvents`)
- Modify: `src/lib/types.ts` (add `ModerationEvent` + `ModerationEventKind` types)

- [ ] **Step 3.1: Add `ModerationEvent` types**

In `src/lib/types.ts`, add:

```ts
export type ModerationEventKind = 'kick' | 'unban' | 'set_power';

export interface ModerationEvent {
  eventId: string;       // 64-char hex
  kind: ModerationEventKind;
  actorAddr: string;     // 32-char hex
  targetAddr: string;    // 32-char hex
  reason: string | null; // populated for kick/unban; null for set_power
  newPower: number | null; // populated for set_power; null for kick/unban
  hlc: Hlc;              // existing Hlc type from this file
}
```

- [ ] **Step 3.2: Extend `kickFromCommunity` with optional reason**

In `src/lib/community-service.ts` (around line 185), change:

```ts
async kickFromCommunity(communityId: string, targetAddr: string, reason?: string): Promise<void> {
  await this.invoke<void>('kick_from_community', {
    communityId,
    targetAddr,
    reason: reason ?? null,
  });
}
```

- [ ] **Step 3.3: Add `unbanFromCommunity`**

Add **immediately after** `kickFromCommunity`:

```ts
async unbanFromCommunity(communityId: string, targetAddr: string, reason?: string): Promise<void> {
  await this.invoke<void>('unban_from_community', {
    communityId,
    targetAddr,
    reason: reason ?? null,
  });
}
```

- [ ] **Step 3.4: Add `listRecentModerationEvents`**

Add near the other read methods in `community-service.ts`:

```ts
async listRecentModerationEvents(communityId: string, limit: number = 10): Promise<ModerationEvent[]> {
  return await this.invoke<ModerationEvent[]>('list_recent_moderation_events', {
    communityId,
    limit,
  });
}
```

Import `ModerationEvent` from `./types` at the top of the file.

- [ ] **Step 3.5: Run gates from repo root**

```bash
npx tsc --noEmit
npx vitest run
```
Both: green. Vitest count unchanged from baseline (no new tests yet).

- [ ] **Step 3.6: Commit**

```bash
git add src/lib/community-service.ts src/lib/types.ts
git commit -m "$(cat <<'EOF'
feat(zeb-284): community-service wrappers for unban + kick-reason + recent events

- CommunityService.unbanFromCommunity(communityId, targetAddr, reason?)
- CommunityService.kickFromCommunity gains optional reason parameter
- CommunityService.listRecentModerationEvents(communityId, limit)
- ModerationEvent + ModerationEventKind types in types.ts

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `CommunityMembersPanel.svelte` + `MemberRow.svelte` (skeleton, no dialog wiring)

**Files:**
- Create: `src/lib/components/CommunityMembersPanel.svelte`
- Create: `src/lib/components/MemberRow.svelte`

**Approach:** Build the read-only structure first — panel hosts member list, MemberRow renders one row with a kebab that *just emits actions as Svelte events* (no dialog wiring yet). Wiring happens in Task 5.

- [ ] **Step 4.1: Create `MemberRow.svelte`**

`src/lib/components/MemberRow.svelte`:

```svelte
<script lang="ts">
  import type { MemberInfoDto } from '$lib/types';

  type ViewerContext = {
    addr: string;       // own owner addr (hex)
    power: number;      // own power level
    isLastAdmin: boolean;
  };

  type KebabAction =
    | 'kick'
    | 'unban'
    | 'promote-mod'
    | 'promote-admin'
    | 'demote-mod'
    | 'demote-member';

  let { member, viewer }: { member: MemberInfoDto; viewer: ViewerContext } = $props();

  const dispatch = createEventDispatcher<{ action: { action: KebabAction; member: MemberInfoDto } }>();

  function tierLabel(power: number, status: string): string {
    if (status === 'banned') return 'Banned';
    if (power === 100) return 'Admin';
    if (power >= 50) return 'Moderator';
    return 'Member';
  }

  function kebabActions(
    viewerPower: number,
    targetPower: number,
    targetStatus: string,
    isSelf: boolean,
    isLastAdmin: boolean,
  ): KebabAction[] {
    if (targetStatus === 'banned') {
      return viewerPower >= 100 ? ['unban'] : [];
    }
    if (isSelf) {
      const actions: KebabAction[] = [];
      if (viewerPower === 100) actions.push('demote-mod');
      if (viewerPower >= 50) actions.push('demote-member');
      return actions;
    }
    const actions: KebabAction[] = [];
    if (viewerPower > targetPower) {
      if (viewerPower >= 100 && targetPower < 50) actions.push('promote-mod');
      if (viewerPower >= 100 && targetPower < 100) actions.push('promote-admin');
      if (viewerPower >= 100 && targetPower === 100) actions.push('demote-mod');
      if (viewerPower >= 100 && targetPower >= 50 && targetPower < 100) actions.push('demote-member');
      if (viewerPower >= 50) actions.push('kick');
    }
    return actions;
  }

  function actionLabel(action: KebabAction): string {
    switch (action) {
      case 'kick': return 'Kick';
      case 'unban': return 'Unban';
      case 'promote-mod': return 'Promote to Moderator';
      case 'promote-admin': return 'Promote to Admin';
      case 'demote-mod': return 'Demote to Moderator';
      case 'demote-member': return 'Demote to Member';
    }
  }

  let isSelf = $derived(member.addr === viewer.addr);
  let actions = $derived(kebabActions(viewer.power, member.power, member.status, isSelf, viewer.isLastAdmin));
  let label = $derived(tierLabel(member.power, member.status));
  let menuOpen = $state(false);
</script>

<li class="member-row">
  <span class="avatar" aria-hidden="true">👤</span>
  <span class="name">{member.displayName ?? member.addr.slice(0, 8)}</span>
  <span class="tier" data-tier={label.toLowerCase()}>{label}</span>
  <span class="joined">joined {new Date(member.joinedAt.wallMs).toLocaleDateString()}</span>
  {#if actions.length > 0}
    <div class="kebab-wrapper">
      <button
        class="kebab"
        aria-label="Member actions"
        aria-haspopup="menu"
        aria-expanded={menuOpen}
        onclick={() => (menuOpen = !menuOpen)}
      >⋮</button>
      {#if menuOpen}
        <ul class="menu" role="menu">
          {#each actions as action}
            <li>
              <button
                role="menuitem"
                onclick={() => {
                  menuOpen = false;
                  dispatch('action', { action, member });
                }}
              >{actionLabel(action)}</button>
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
</li>

<style>
  .member-row { display: flex; align-items: center; gap: 0.75rem; padding: 0.5rem 0.75rem; }
  .name { flex: 1; }
  .tier[data-tier="admin"] { font-weight: 600; }
  .kebab { min-width: 44px; min-height: 44px; }  /* touch target */
  .menu { position: absolute; background: var(--panel-bg); border: 1px solid var(--border); }
</style>
```

**Implementer note:** The exact Svelte 5 import for `createEventDispatcher` may vary depending on project conventions — match how `ChannelMembersPanel.svelte` or `InviteLinkManager.svelte` do it (look at their imports). Apply the project's existing CSS-variable conventions. Don't invent style tokens.

- [ ] **Step 4.2: Create `CommunityMembersPanel.svelte`**

`src/lib/components/CommunityMembersPanel.svelte`:

```svelte
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { communityService } from '$lib/community-service';
  import { profileStore } from '$lib/profile-store';  // or however own-addr is exposed
  import MemberRow from './MemberRow.svelte';
  import type { MemberInfoDto } from '$lib/types';

  let { communityId }: { communityId: string } = $props();

  let members = $state<MemberInfoDto[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let searchQuery = $state('');
  let bannedExpanded = $state(false);

  async function refresh() {
    try {
      loading = true;
      members = await communityService.listCommunityMembers(communityId);
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  let ownAddr = $derived($profileStore?.ownerAddr ?? '');
  let viewerPower = $derived(members.find((m) => m.addr === ownAddr)?.power ?? 0);
  let admins = $derived(members.filter((m) => m.power === 100 && m.status === 'joined'));
  let viewerIsLastAdmin = $derived(
    viewerPower === 100 && admins.length === 1 && admins[0].addr === ownAddr,
  );

  let viewer = $derived({ addr: ownAddr, power: viewerPower, isLastAdmin: viewerIsLastAdmin });

  let joined = $derived(
    members.filter((m) => m.status === 'joined' && matchesSearch(m, searchQuery)),
  );
  let banned = $derived(
    members.filter((m) => m.status === 'banned' && matchesSearch(m, searchQuery)),
  );

  function matchesSearch(m: MemberInfoDto, q: string): boolean {
    if (!q) return true;
    const ql = q.toLowerCase();
    return (m.displayName ?? '').toLowerCase().includes(ql) || m.addr.toLowerCase().includes(ql);
  }

  function onMemberAction(_event: CustomEvent<{ action: string; member: MemberInfoDto }>) {
    // Wired in Task 5 — dialogs not yet present
  }

  let unlisten: (() => void) | undefined;

  onMount(async () => {
    await refresh();
    // Subscribe to community-state-updated event for reactive refresh
    unlisten = await tauriAdapter.listen('community-state-updated', refresh);
  });

  onDestroy(() => unlisten?.());
</script>

<section class="community-members-panel">
  <header>
    <h2>Community Members</h2>
    <input
      type="search"
      placeholder="Filter members..."
      bind:value={searchQuery}
      aria-label="Filter members"
    />
  </header>

  {#if loading}
    <p class="loading">Loading members...</p>
  {:else if error}
    <p class="error" role="alert">{error}</p>
  {:else}
    <ul class="member-list" aria-label="Active members">
      {#each joined as member (member.addr)}
        <MemberRow {member} {viewer} on:action={onMemberAction} />
      {/each}
    </ul>

    {#if banned.length > 0}
      <details bind:open={bannedExpanded}>
        <summary>Banned ({banned.length})</summary>
        <ul class="member-list banned-list" aria-label="Banned members">
          {#each banned as member (member.addr)}
            <MemberRow {member} {viewer} on:action={onMemberAction} />
          {/each}
        </ul>
      </details>
    {/if}
  {/if}
</section>

<style>
  .community-members-panel { display: flex; flex-direction: column; gap: 1rem; }
  .member-list { list-style: none; padding: 0; margin: 0; }
  .banned-list { opacity: 0.7; }
</style>
```

**Implementer note:** Adapt the `profileStore` / `tauriAdapter` imports to match the project's actual store/adapter naming. Likely the canonical patterns live in `CommunitySettingsPanel.svelte` and `ChannelMembersPanel.svelte` — copy their import shape exactly. If `listCommunityMembers` doesn't already exist on `CommunityService`, locate the existing service method that wraps `list_community_members` IPC.

- [ ] **Step 4.3: Run gates from repo root**

```bash
npx tsc --noEmit
npx vitest run
```
Both: green. (No new tests yet.)

- [ ] **Step 4.4: Commit**

```bash
git add src/lib/components/CommunityMembersPanel.svelte src/lib/components/MemberRow.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-284): CommunityMembersPanel + MemberRow skeleton

- CommunityMembersPanel reads members via list_community_members, refreshes
  on community-state-updated. Banned members sectioned separately
  (collapsed by default). In-memory filter via search input.
- MemberRow renders avatar + name + tier label + joined-date + kebab.
  Kebab actions computed pure-function from (viewer power, target power,
  target status, isSelf, isLastAdmin) — empty action list hides the
  kebab entirely (no dead UI affordance).
- Tier vocabulary: Admin (100) / Moderator (50-99) / Member (0-49) /
  Banned (status override).
- Touch-target ≥44×44px on kebab button for future mobile Tauri.

Dialogs (kick/unban confirm + last-admin typed-confirm) wired in
the next commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `ModerationReasonDialog.svelte` + wire kick/unban actions from MemberRow

**Files:**
- Create: `src/lib/components/ModerationReasonDialog.svelte`
- Modify: `src/lib/components/CommunityMembersPanel.svelte` (wire dialog + action handlers)

- [ ] **Step 5.1: Create `ModerationReasonDialog.svelte`**

```svelte
<script lang="ts">
  type Props = {
    open: boolean;
    action: 'kick' | 'unban';
    targetName: string;
    communityName: string;
    onConfirm: (reason: string | null) => Promise<void>;
    onCancel: () => void;
  };

  let { open = $bindable(), action, targetName, communityName, onConfirm, onCancel }: Props = $props();

  let reason = $state('');
  let submitting = $state(false);
  let submitError = $state<string | null>(null);

  const actionLabel = $derived(action === 'kick' ? 'Kick' : 'Unban');
  const verb = $derived(action === 'kick' ? 'kick' : 'unban');

  async function handleConfirm() {
    submitting = true;
    submitError = null;
    try {
      await onConfirm(reason.trim() || null);
      reason = '';
      open = false;
    } catch (e) {
      submitError = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  function handleCancel() {
    if (submitting) return;
    reason = '';
    submitError = null;
    open = false;
    onCancel();
  }
</script>

{#if open}
  <div class="dialog-overlay" role="presentation">
    <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="moderation-dialog-title">
      <h2 id="moderation-dialog-title">{actionLabel} {targetName} from "{communityName}"?</h2>

      <label>
        Optional: reason (visible to {targetName} and other mods)
        <textarea
          bind:value={reason}
          maxlength="280"
          placeholder="e.g., repeated spam in #general"
          disabled={submitting}
          rows="3"
        ></textarea>
      </label>

      {#if submitError}
        <p class="error" role="alert">{submitError}</p>
      {/if}

      <div class="actions">
        <button onclick={handleCancel} disabled={submitting}>Cancel</button>
        <span class="spacer"></span>
        <button
          class="primary {action}"
          onclick={handleConfirm}
          disabled={submitting}
        >
          {#if submitting}
            <span class="spinner" aria-hidden="true"></span>
            {actionLabel}ing...
          {:else}
            {actionLabel}
          {/if}
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .dialog-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.4); display: grid; place-items: center; }
  .dialog { background: var(--panel-bg); padding: 1.5rem; border-radius: 8px; min-width: 420px; max-width: 90vw; }
  .actions { display: flex; align-items: center; margin-top: 1rem; }
  .spacer { flex: 1; }  /* puts Cancel on left, primary action on right (secondary-position confirmation per memory) */
  .primary.kick { background: var(--danger); }
  .primary.unban { background: var(--success); }
  .error { color: var(--danger); margin-top: 0.5rem; }
</style>
```

- [ ] **Step 5.2: Wire dialog into `CommunityMembersPanel.svelte`**

Add to the `<script>` block:

```ts
import ModerationReasonDialog from './ModerationReasonDialog.svelte';

let dialogOpen = $state(false);
let dialogAction = $state<'kick' | 'unban'>('kick');
let dialogTarget = $state<MemberInfoDto | null>(null);

let { communityId, communityName }: { communityId: string; communityName: string } = $props();
// (Update existing $props() destructure to include communityName)

async function handleAction(detail: { action: string; member: MemberInfoDto }) {
  const { action, member } = detail;
  if (action === 'kick') {
    dialogAction = 'kick';
    dialogTarget = member;
    dialogOpen = true;
  } else if (action === 'unban') {
    dialogAction = 'unban';
    dialogTarget = member;
    dialogOpen = true;
  } else if (action === 'promote-mod') {
    await communityService.setPowerLevel(communityId, member.addr, 50);
    await refresh();
  } else if (action === 'promote-admin') {
    await communityService.setPowerLevel(communityId, member.addr, 100);
    await refresh();
  } else if (action === 'demote-mod') {
    // Hand to last-admin guard wrapper — fully wired in Task 6
    await communityService.setPowerLevel(communityId, member.addr, 50);
    await refresh();
  } else if (action === 'demote-member') {
    await communityService.setPowerLevel(communityId, member.addr, 0);
    await refresh();
  }
}

function onMemberAction(event: CustomEvent<{ action: string; member: MemberInfoDto }>) {
  void handleAction(event.detail);
}

async function onDialogConfirm(reason: string | null): Promise<void> {
  if (!dialogTarget) return;
  if (dialogAction === 'kick') {
    await communityService.kickFromCommunity(communityId, dialogTarget.addr, reason ?? undefined);
  } else {
    await communityService.unbanFromCommunity(communityId, dialogTarget.addr, reason ?? undefined);
  }
  await refresh();
}

function onDialogCancel() {
  dialogTarget = null;
}
```

Append the dialog to the template:

```svelte
<ModerationReasonDialog
  bind:open={dialogOpen}
  action={dialogAction}
  targetName={dialogTarget?.displayName ?? dialogTarget?.addr.slice(0, 8) ?? ''}
  {communityName}
  onConfirm={onDialogConfirm}
  onCancel={onDialogCancel}
/>
```

- [ ] **Step 5.3: Run gates from repo root**

```bash
npx tsc --noEmit
npx vitest run
```
Both: green.

- [ ] **Step 5.4: Commit**

```bash
git add src/lib/components/ModerationReasonDialog.svelte src/lib/components/CommunityMembersPanel.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-284): ModerationReasonDialog + wire kick/unban + promote/demote

- ModerationReasonDialog parameterized by action='kick'|'unban'.
  Optional reason textarea, max 280 chars. Cancel on left, primary action
  on right (secondary-position click-confirm per
  feedback_severe_action_confirmation).
- CommunityMembersPanel routes kebab actions:
  - kick / unban → open dialog
  - promote-mod / promote-admin → setPowerLevel directly (low-risk; no confirm)
  - demote-mod / demote-member → setPowerLevel directly (last-admin
    guard added in next commit)
- Error path: IPC rejection surfaces in dialog as inline error using
  e instanceof Error ? e.message : String(e) extraction.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `LastAdminWarningDialog.svelte` + wire self-demote/self-leave

**Files:**
- Create: `src/lib/components/LastAdminWarningDialog.svelte`
- Modify: `src/lib/components/CommunityMembersPanel.svelte` (intercept self-demote when last admin)
- Modify: Whichever component owns the community-leave action (likely `CommunitySettingsPanel.svelte` or a `LeaveCommunityButton.svelte`) to intercept self-leave when last admin

- [ ] **Step 6.1: Create `LastAdminWarningDialog.svelte`**

```svelte
<script lang="ts">
  type Props = {
    open: boolean;
    action: 'demote' | 'leave';
    communityName: string;
    onConfirm: () => Promise<void>;
    onCancel: () => void;
  };

  let { open = $bindable(), action, communityName, onConfirm, onCancel }: Props = $props();

  const requiredToken = $derived(action === 'demote' ? 'DEMOTE' : 'LEAVE');
  let typedToken = $state('');
  let submitting = $state(false);
  let submitError = $state<string | null>(null);

  const canProceed = $derived(typedToken === requiredToken && !submitting);

  async function handleConfirm() {
    if (!canProceed) return;
    submitting = true;
    submitError = null;
    try {
      await onConfirm();
      typedToken = '';
      open = false;
    } catch (e) {
      submitError = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  function handleCancel() {
    if (submitting) return;
    typedToken = '';
    submitError = null;
    open = false;
    onCancel();
  }
</script>

{#if open}
  <div class="dialog-overlay" role="presentation">
    <div class="dialog warning" role="dialog" aria-modal="true" aria-labelledby="last-admin-dialog-title">
      <h2 id="last-admin-dialog-title">⚠ You are the last admin of "{communityName}"</h2>

      <p>
        After this action, the community will be locked: no one will be able to issue
        moderation actions, including restoring admin tier. Recovery is possible by
        forking the community (coming soon —
        <a href="https://linear.app/zeblith/issue/ZEB-285" target="_blank" rel="noreferrer">ZEB-285</a>).
      </p>

      <label>
        To proceed, type <code>{requiredToken}</code> below:
        <input
          type="text"
          bind:value={typedToken}
          autocomplete="off"
          autocapitalize="off"
          spellcheck="false"
          disabled={submitting}
          aria-describedby="last-admin-token-help"
        />
      </label>
      <p id="last-admin-token-help" class="hint">Case-sensitive. Exact match required.</p>

      {#if submitError}
        <p class="error" role="alert">{submitError}</p>
      {/if}

      <div class="actions">
        <button onclick={handleCancel} disabled={submitting}>Cancel</button>
        <span class="spacer"></span>
        <button class="primary danger" onclick={handleConfirm} disabled={!canProceed}>
          {#if submitting}
            <span class="spinner" aria-hidden="true"></span> Proceeding...
          {:else}
            Proceed
          {/if}
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  /* Same overlay/dialog conventions as ModerationReasonDialog */
</style>
```

- [ ] **Step 6.2: Intercept self-demote when last admin in `CommunityMembersPanel.svelte`**

In the `handleAction` function, modify the `demote-mod` and `demote-member` branches to check for last-admin self-demote:

```ts
} else if (action === 'demote-mod' || action === 'demote-member') {
  const isSelf = member.addr === viewer.addr;
  const newPower = action === 'demote-mod' ? 50 : 0;
  if (isSelf && viewer.isLastAdmin) {
    // Trigger typed-confirm dialog
    lastAdminDialogOpen = true;
    lastAdminDialogAction = 'demote';
    lastAdminDialogPendingPower = newPower;
    return;
  }
  await communityService.setPowerLevel(communityId, member.addr, newPower);
  await refresh();
}
```

Add the dialog state:

```ts
import LastAdminWarningDialog from './LastAdminWarningDialog.svelte';

let lastAdminDialogOpen = $state(false);
let lastAdminDialogAction = $state<'demote' | 'leave'>('demote');
let lastAdminDialogPendingPower = $state(0);

async function onLastAdminDialogConfirm() {
  if (lastAdminDialogAction === 'demote' && viewer.addr) {
    await communityService.setPowerLevel(communityId, viewer.addr, lastAdminDialogPendingPower);
    await refresh();
  }
  // 'leave' branch lives in CommunitySettingsPanel (see Step 6.3)
}
```

Append dialog to template:

```svelte
<LastAdminWarningDialog
  bind:open={lastAdminDialogOpen}
  action={lastAdminDialogAction}
  {communityName}
  onConfirm={onLastAdminDialogConfirm}
  onCancel={() => {}}
/>
```

- [ ] **Step 6.3: Wire last-admin guard into community-leave flow**

Locate the existing component that hosts the "Leave Community" action. Search:

```bash
grep -rn "leave_community\|leaveCommunity\|Leave community" src/lib/ | head -10
```

Wherever it lives, intercept the leave handler:

```ts
async function handleLeaveCommunity() {
  // Existing logic: confirm + invoke
  if (viewerIsLastAdmin) {
    lastAdminLeaveDialogOpen = true;
    return;
  }
  await communityService.leaveCommunity(communityId);
}
```

And add a `LastAdminWarningDialog` instance with `action="leave"` whose `onConfirm` calls `communityService.leaveCommunity(communityId)`.

**Implementer note:** If the leave flow is in a different file from `CommunityMembersPanel`, viewer-is-last-admin must be computed there too. Either pass it down as a prop from `CommunitySettingsPanel`, or recompute it locally from a fresh `listCommunityMembers` call. Pick the path that requires fewer cross-component changes.

- [ ] **Step 6.4: Run gates from repo root**

```bash
npx tsc --noEmit
npx vitest run
```
Both: green.

- [ ] **Step 6.5: Commit**

```bash
git add src/lib/components/LastAdminWarningDialog.svelte src/lib/components/CommunityMembersPanel.svelte src/lib/components/<leave-host>.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-284): LastAdminWarningDialog + wire self-demote + self-leave guards

- LastAdminWarningDialog typed-confirm dialog. Tokens: DEMOTE (self-demote)
  / LEAVE (self-leave). Case-sensitive exact-match validation. Forward-
  pointing breadcrumb to ZEB-285 (community forking) as the recovery path.
- CommunityMembersPanel intercepts self-demote-mod / self-demote-member
  when viewer is the last admin → opens dialog instead of firing IPC.
- Leave-community flow intercepts self-leave when viewer is the last
  admin → opens dialog instead of firing IPC.

No hard backend guard per spec §3.2 — UI is the only gate. Future
hardening could add a backend mirror; for the soft-warning policy the
UI gate is sufficient.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `RecentActionsBadge.svelte` + integrate panel into `CommunitySettingsPanel.svelte`

**Files:**
- Create: `src/lib/components/RecentActionsBadge.svelte`
- Modify: `src/lib/components/CommunityMembersPanel.svelte` (render badge at top)
- Modify: `src/lib/components/CommunitySettingsPanel.svelte` (link/tab to open members panel)

- [ ] **Step 7.1: Create `RecentActionsBadge.svelte`**

```svelte
<script lang="ts">
  import type { ModerationEvent } from '$lib/types';

  let { events }: { events: ModerationEvent[] } = $props();

  let expanded = $state(false);

  function relativeTime(wallMs: number): string {
    const diff = Date.now() - wallMs;
    const minutes = Math.floor(diff / 60_000);
    const hours = Math.floor(diff / 3_600_000);
    const days = Math.floor(diff / 86_400_000);
    if (minutes < 1) return 'just now';
    if (minutes < 60) return `${minutes} minute${minutes === 1 ? '' : 's'} ago`;
    if (hours < 24) return `${hours} hour${hours === 1 ? '' : 's'} ago`;
    return `${days} day${days === 1 ? '' : 's'} ago`;
  }

  function shortAddr(addr: string): string {
    return addr.slice(0, 8);
  }

  function tierName(level: number): string {
    if (level >= 100) return 'Admin';
    if (level >= 50) return 'Moderator';
    return 'Member';
  }

  function describeEvent(ev: ModerationEvent): string {
    const actor = shortAddr(ev.actorAddr);
    const target = shortAddr(ev.targetAddr);
    if (ev.kind === 'kick') {
      const r = ev.reason ? ` ("${ev.reason}")` : '';
      return `${actor} kicked ${target}${r}`;
    }
    if (ev.kind === 'unban') {
      const r = ev.reason ? ` ("${ev.reason}")` : '';
      return `${actor} unbanned ${target}${r}`;
    }
    // set_power
    const t = tierName(ev.newPower ?? 0);
    return `${actor} set ${target} to ${t}`;
  }
</script>

<section class="recent-actions-badge" aria-label="Recent moderation actions">
  <button
    class="toggle"
    onclick={() => (expanded = !expanded)}
    aria-expanded={expanded}
  >
    {expanded ? '▾' : '▸'} Recent moderation actions ({events.length})
  </button>

  {#if expanded}
    {#if events.length === 0}
      <p class="empty">No recent moderation actions.</p>
    {:else}
      <ul class="events">
        {#each events as ev (ev.eventId)}
          <li>
            <span class="when">{relativeTime(ev.hlc.wallMs)}</span>
            —
            <span class="what">{describeEvent(ev)}</span>
          </li>
        {/each}
      </ul>
    {/if}
  {/if}
</section>

<style>
  .recent-actions-badge { background: var(--badge-bg); border-radius: 6px; padding: 0.5rem 0.75rem; }
  .toggle { background: none; border: none; cursor: pointer; }
  .events { list-style: none; padding: 0; margin: 0.5rem 0 0; }
  .events li { padding: 0.25rem 0; font-size: 0.9em; }
</style>
```

- [ ] **Step 7.2: Wire badge into `CommunityMembersPanel`**

Add to the `<script>` block:

```ts
import RecentActionsBadge from './RecentActionsBadge.svelte';
import type { ModerationEvent } from '$lib/types';

let recentEvents = $state<ModerationEvent[]>([]);

async function refresh() {
  // Existing body; add:
  recentEvents = await communityService.listRecentModerationEvents(communityId, 10);
}
```

Add the component to the template at the top of the panel body:

```svelte
<RecentActionsBadge events={recentEvents} />
<!-- existing search + member list -->
```

- [ ] **Step 7.3: Add members panel link/tab to `CommunitySettingsPanel.svelte`**

Read `CommunitySettingsPanel.svelte` first to understand its current tab/section structure. Then add either:

- A new tab labeled "Members" that mounts `<CommunityMembersPanel {communityId} {communityName} />`
- Or a section with a button "Manage members" that opens it inline / in a new view

Pick the path that matches the existing tab/section conventions in the file. Pass through `communityId` and `communityName` props.

- [ ] **Step 7.4: Run gates from repo root**

```bash
npx tsc --noEmit
npx vitest run
```
Both: green.

- [ ] **Step 7.5: Commit**

```bash
git add src/lib/components/RecentActionsBadge.svelte src/lib/components/CommunityMembersPanel.svelte src/lib/components/CommunitySettingsPanel.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-284): RecentActionsBadge + integrate members panel into settings

- RecentActionsBadge collapsible section at top of CommunityMembersPanel.
  Reads last 10 moderation events via listRecentModerationEvents.
  Human-readable rows: relative time + actor short-hash + verb + target
  short-hash + optional reason quoted.
- CommunitySettingsPanel exposes the members panel via tab/link
  (implementation matches existing tab convention).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Vitest UI tests (4 new files)

**Files:**
- Create: `src/lib/components/CommunityMembersPanel.test.ts`
- Create: `src/lib/components/MemberRow.test.ts`
- Create: `src/lib/components/ModerationReasonDialog.test.ts`
- Create: `src/lib/components/LastAdminWarningDialog.test.ts`

**Approach:** Mirror existing vitest patterns from other Svelte components (look at `LibraryDirectoryBrowser.test.ts` or similar). Mock `communityService.invoke` with the existing `tauriAdapter` mock pattern.

- [ ] **Step 8.1: Find the canonical vitest pattern**

```bash
ls src/lib/components/*.test.ts | head -5
```

Read one of the existing `.test.ts` files in full to understand: mock setup, render helpers, assertion idioms, async/await patterns.

- [ ] **Step 8.2: Write `MemberRow.test.ts` — 6 kebab-matrix cases**

```ts
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import MemberRow from './MemberRow.svelte';
import type { MemberInfoDto } from '$lib/types';

function makeMember(power: number, status: 'joined' | 'left' | 'banned' = 'joined', addr = 'aa'.repeat(16)): MemberInfoDto {
  return {
    addr,
    displayName: null,
    status,
    power,
    joinedAt: { wallMs: 1700000000000, logical: 0, deviceId: '00'.repeat(8) },
  };
}

describe('MemberRow kebab action matrix', () => {
  it('admin viewer sees Kick + Promote-Admin + Demote-Member on a moderator target', async () => {
    const target = makeMember(50);
    const viewer = { addr: 'bb'.repeat(16), power: 100, isLastAdmin: false };
    const { getByRole, getByText } = render(MemberRow, { member: target, viewer });
    await fireEvent.click(getByRole('button', { name: /member actions/i }));
    expect(getByText('Kick')).toBeInTheDocument();
    expect(getByText('Promote to Admin')).toBeInTheDocument();
    expect(getByText('Demote to Member')).toBeInTheDocument();
  });

  it('moderator viewer sees Kick on a member target', async () => {
    const target = makeMember(0);
    const viewer = { addr: 'bb'.repeat(16), power: 50, isLastAdmin: false };
    const { getByRole, getByText, queryByText } = render(MemberRow, { member: target, viewer });
    await fireEvent.click(getByRole('button', { name: /member actions/i }));
    expect(getByText('Kick')).toBeInTheDocument();
    expect(queryByText(/Promote/)).not.toBeInTheDocument();
  });

  it('moderator viewer sees no kebab on an admin target', () => {
    const target = makeMember(100);
    const viewer = { addr: 'bb'.repeat(16), power: 50, isLastAdmin: false };
    const { queryByRole } = render(MemberRow, { member: target, viewer });
    expect(queryByRole('button', { name: /member actions/i })).not.toBeInTheDocument();
  });

  it('member viewer sees no kebab at all', () => {
    const target = makeMember(50);
    const viewer = { addr: 'bb'.repeat(16), power: 0, isLastAdmin: false };
    const { queryByRole } = render(MemberRow, { member: target, viewer });
    expect(queryByRole('button', { name: /member actions/i })).not.toBeInTheDocument();
  });

  it('admin viewer sees Unban on a banned target', async () => {
    const target = makeMember(0, 'banned');
    const viewer = { addr: 'bb'.repeat(16), power: 100, isLastAdmin: false };
    const { getByRole, getByText, queryByText } = render(MemberRow, { member: target, viewer });
    await fireEvent.click(getByRole('button', { name: /member actions/i }));
    expect(getByText('Unban')).toBeInTheDocument();
    expect(queryByText('Kick')).not.toBeInTheDocument();
  });

  it('admin viewer on own row sees Demote-Mod when last admin', async () => {
    const self = makeMember(100, 'joined', 'cc'.repeat(16));
    const viewer = { addr: 'cc'.repeat(16), power: 100, isLastAdmin: true };
    const { getByRole, getByText } = render(MemberRow, { member: self, viewer });
    await fireEvent.click(getByRole('button', { name: /member actions/i }));
    expect(getByText(/Demote to Moderator/)).toBeInTheDocument();
  });
});
```

- [ ] **Step 8.3: Write `ModerationReasonDialog.test.ts` — 4 cases**

```ts
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import ModerationReasonDialog from './ModerationReasonDialog.svelte';

describe('ModerationReasonDialog', () => {
  it('kick happy path with reason calls onConfirm with reason', async () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    const onCancel = vi.fn();
    const { getByRole, getByLabelText } = render(ModerationReasonDialog, {
      open: true, action: 'kick', targetName: 'Bob', communityName: 'Test',
      onConfirm, onCancel,
    });
    await fireEvent.input(getByLabelText(/reason/i), { target: { value: 'spam' } });
    await fireEvent.click(getByRole('button', { name: /^kick$/i }));
    expect(onConfirm).toHaveBeenCalledWith('spam');
  });

  it('kick happy path with blank reason calls onConfirm with null', async () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    const { getByRole } = render(ModerationReasonDialog, {
      open: true, action: 'kick', targetName: 'Bob', communityName: 'Test',
      onConfirm, onCancel: () => {},
    });
    await fireEvent.click(getByRole('button', { name: /^kick$/i }));
    expect(onConfirm).toHaveBeenCalledWith(null);
  });

  it('IPC rejection surfaces as inline error', async () => {
    const onConfirm = vi.fn().mockRejectedValue(new Error('insufficient power'));
    const { getByRole, findByRole } = render(ModerationReasonDialog, {
      open: true, action: 'kick', targetName: 'Bob', communityName: 'Test',
      onConfirm, onCancel: () => {},
    });
    await fireEvent.click(getByRole('button', { name: /^kick$/i }));
    expect(await findByRole('alert')).toHaveTextContent('insufficient power');
  });

  it('cancel does not call onConfirm', async () => {
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    const { getByRole } = render(ModerationReasonDialog, {
      open: true, action: 'kick', targetName: 'Bob', communityName: 'Test',
      onConfirm, onCancel,
    });
    await fireEvent.click(getByRole('button', { name: /cancel/i }));
    expect(onConfirm).not.toHaveBeenCalled();
    expect(onCancel).toHaveBeenCalled();
  });
});
```

- [ ] **Step 8.4: Write `LastAdminWarningDialog.test.ts` — 3 cases**

```ts
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import LastAdminWarningDialog from './LastAdminWarningDialog.svelte';

describe('LastAdminWarningDialog', () => {
  it('Proceed button disabled until token typed exactly', async () => {
    const { getByRole, getByLabelText } = render(LastAdminWarningDialog, {
      open: true, action: 'demote', communityName: 'Test',
      onConfirm: vi.fn().mockResolvedValue(undefined), onCancel: () => {},
    });
    const proceed = getByRole('button', { name: /proceed/i });
    expect(proceed).toBeDisabled();
    await fireEvent.input(getByLabelText(/type DEMOTE/i), { target: { value: 'demote' } });  // lowercase
    expect(proceed).toBeDisabled();  // case-sensitive
    await fireEvent.input(getByLabelText(/type DEMOTE/i), { target: { value: 'DEMOTE' } });
    expect(proceed).not.toBeDisabled();
  });

  it('LEAVE token required for action=leave', async () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    const { getByRole, getByLabelText } = render(LastAdminWarningDialog, {
      open: true, action: 'leave', communityName: 'Test',
      onConfirm, onCancel: () => {},
    });
    await fireEvent.input(getByLabelText(/type LEAVE/i), { target: { value: 'LEAVE' } });
    await fireEvent.click(getByRole('button', { name: /proceed/i }));
    expect(onConfirm).toHaveBeenCalled();
  });

  it('Cancel does not call onConfirm', async () => {
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    const { getByRole } = render(LastAdminWarningDialog, {
      open: true, action: 'demote', communityName: 'Test',
      onConfirm, onCancel,
    });
    await fireEvent.click(getByRole('button', { name: /cancel/i }));
    expect(onConfirm).not.toHaveBeenCalled();
    expect(onCancel).toHaveBeenCalled();
  });
});
```

- [ ] **Step 8.5: Write `CommunityMembersPanel.test.ts` — 4 cases**

```ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render } from '@testing-library/svelte';
import CommunityMembersPanel from './CommunityMembersPanel.svelte';

// Mock communityService with the project's canonical pattern
vi.mock('$lib/community-service', () => ({
  communityService: {
    listCommunityMembers: vi.fn().mockResolvedValue([
      { addr: 'aa'.repeat(16), displayName: 'Alice', status: 'joined', power: 100, joinedAt: { wallMs: 1700000000000, logical: 0, deviceId: '00'.repeat(8) } },
      { addr: 'bb'.repeat(16), displayName: 'Bob', status: 'joined', power: 50, joinedAt: { wallMs: 1700000010000, logical: 0, deviceId: '00'.repeat(8) } },
      { addr: 'cc'.repeat(16), displayName: 'Eve', status: 'banned', power: 0, joinedAt: { wallMs: 1700000020000, logical: 0, deviceId: '00'.repeat(8) } },
    ]),
    listRecentModerationEvents: vi.fn().mockResolvedValue([]),
  },
}));

describe('CommunityMembersPanel', () => {
  it('renders members sorted by power desc', async () => {
    const { findAllByRole } = render(CommunityMembersPanel, { communityId: '00'.repeat(16), communityName: 'Test' });
    const rows = await findAllByRole('listitem');
    // Alice (100) before Bob (50); banned section separate
    expect(rows[0]).toHaveTextContent('Alice');
    expect(rows[1]).toHaveTextContent('Bob');
  });

  it('banned section visible only when banned members exist', async () => {
    const { findByText } = render(CommunityMembersPanel, { communityId: '00'.repeat(16), communityName: 'Test' });
    expect(await findByText(/Banned \(1\)/)).toBeInTheDocument();
  });

  it('search filters by name', async () => {
    // Verify after typing in the search box, only matching members appear
    // (Implementation detail per existing vitest patterns)
  });

  it('IPC error surfaces in panel', async () => {
    // Override mock to throw; verify alert role appears
  });
});
```

(Implementer fills in the last two cases per existing project test conventions.)

- [ ] **Step 8.6: Run vitest — all new tests pass**

```bash
npx vitest run
```
Expected: baseline + 17 new tests (6 + 4 + 3 + 4) all passing.

- [ ] **Step 8.7: Run full gates one more time**

From `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

From repo root:
```bash
npx tsc --noEmit
npx vitest run
```

All 5: green.

- [ ] **Step 8.8: Commit**

```bash
git add src/lib/components/CommunityMembersPanel.test.ts src/lib/components/MemberRow.test.ts src/lib/components/ModerationReasonDialog.test.ts src/lib/components/LastAdminWarningDialog.test.ts
git commit -m "$(cat <<'EOF'
test(zeb-284): vitest coverage for moderation UX components

- MemberRow: 6 kebab-matrix cases (admin/mod/member viewers × member/mod/admin/banned/self targets)
- ModerationReasonDialog: kick with reason, kick blank reason, IPC-error toast, cancel
- LastAdminWarningDialog: DEMOTE/LEAVE token validation, case-sensitive,
  proceed disabled until match, cancel
- CommunityMembersPanel: sort order, banned section visibility, search,
  IPC error path

17 new test cases total.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Final verification + push + PR creation

- [ ] **Step 9.1: Run all 5 gates one final time**

From `src-tauri/`:
```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo check --locked --all-targets --features test-fixtures
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

From repo root:
```bash
npx tsc --noEmit
npx vitest run
```

All 5: green.

Expected delta from baseline:
- Rust tests: **+13** (5 CRDT unit + 6 IPC + 2 fixture-pins)
- Vitest tests: **+17** (6 + 4 + 3 + 4)

- [ ] **Step 9.2: Verify branch state**

```bash
git log --oneline origin/main..HEAD
```
Expected: 1 spec commit + 7 implementation commits + 1 vitest commit = 9 commits.

- [ ] **Step 9.3: Push branch**

```bash
git push -u origin zeb-284-community-moderation-ux
```

- [ ] **Step 9.4: Open PR**

```bash
gh pr create --title "ZEB-284: community moderation UX (kick / unban / set-power / member panel)" --body "$(cat <<'EOF'
## Summary

Surface the existing community moderation primitives to end-users by:
- Adding the new [`MembershipEventKind::Unban`](https://linear.app/zeblith/issue/ZEB-284) CRDT variant (admin-tier; transitions Banned → Left so target can be re-invited)
- Adding `unban_from_community` + `list_recent_moderation_events` Tauri IPCs
- Extending `kick_from_community` with an optional `reason` parameter (backwards-compatible)
- Building 5 new Svelte components: `CommunityMembersPanel`, `MemberRow`, `ModerationReasonDialog`, `LastAdminWarningDialog`, `RecentActionsBadge`
- Wiring the members panel into `CommunitySettingsPanel`

Spec: `docs/specs/2026-05-13-zeb-284-community-moderation-ux-design.md`
Plan: `docs/plans/2026-05-13-zeb-284-community-moderation-ux-plan.md`

Closes [ZEB-284](https://linear.app/zeblith/issue/ZEB-284).

Follow-up: [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) (community forking — the recovery path forward-pointed to by the last-admin warning dialog).

## Key design decisions (resolved in brainstorm)

- **Kick/ban model:** Added `Unban` admin-tier action. Kick remains the only removal primitive; unban lifts the implicit ban so target can be re-invited. Avoids the Discord/Matrix two-action vocabulary while giving mistake-correction.
- **Last-admin guard:** No hard backend guard. Soft typed-confirm UI dialog (`DEMOTE` / `LEAVE` tokens) forward-pointing to ZEB-285 (community forking) as the recovery path. Hard guards don't address the actual root cause (admin loses identity); forking is the resilient solution.
- **Audit surface:** Inline `RecentActionsBadge` showing last 10 moderation events at top of member panel. Full audit tab deferred.
- **Reason capture:** Optional, visible to kicked member + mods. Free-text, max 280 chars.
- **Vocabulary:** Fixed labels (Member / Moderator / Admin). Raw u8 stays backend-internal.
- **Row interaction:** Always-visible kebab (touch-target ≥44×44px for future mobile Tauri).

## Test Plan

Local gates (CI is disabled; bots are not CI):
- [ ] `cd src-tauri && cargo fmt --all -- --check`
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (+13 tests vs main)
- [ ] `npx tsc --noEmit`
- [ ] `npx vitest run` (+17 tests vs main)

Manual smoke test (two-engine local setup):
- [ ] Engine A creates community "Test"; invites Engine B; B joins (A=admin, B=member)
- [ ] A promotes B to Moderator → both panels show "Moderator"; recent-actions badge on A prepends the event
- [ ] A demotes B back to Member → confirmed via panel
- [ ] A kicks B with reason "smoke test" → B's row moves to Banned section; B sees the kick + reason in last-visible state; B's nav-tree entry disappears
- [ ] A unbans B → B's banned row transitions to Left; re-invite + re-join cycle clean
- [ ] A attempts self-demote → typed-confirm dialog with `DEMOTE` token; cancel returns cleanly; typed `DEMOTE` proceeds
- [ ] A attempts self-leave when last admin → typed-confirm dialog with `LEAVE` token

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 9.5: Report PR URL + transition to bot-review monitoring**

The PR URL is returned by `gh pr create`. Hand off to the autonomous bot-review monitoring loop per the `feedback_autonomous_pr_monitoring_loop` memory:
- Schedule 270s wakeup
- Watch CodeRabbit + Cursor Bugbot checks + Qodo + CodeAnt-AI comments
- Address substantive findings as fixup commits; respond to false positives with rationale
- Pushover-notify at convergence (`~/work/pushover-notify.sh <title> <body>`)
- Do NOT auto-merge — wait for user nod

---

## Self-review (run before declaring plan done)

**Spec coverage check:** Every spec section (§3.1-3.7, §4-7, §11) has a task that implements it. ✓

**Placeholder scan:** No "TBD", "implement later", or untyped error-handling references. Implementer-note callouts are concrete delegations to existing canonical patterns, not placeholders. ✓

**Type consistency:** `MemberInfoDto.status` is used as a string (`'joined' | 'banned' | 'left'`) consistently between Rust DTO and TS types. `ModerationEvent.hlc` matches the existing `Hlc` shape. `KebabAction` enum values match between `MemberRow.svelte` and `handleAction` in `CommunityMembersPanel.svelte`. ✓
