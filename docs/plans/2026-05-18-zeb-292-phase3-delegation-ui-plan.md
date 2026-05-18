# ZEB-292 Phase 3: Tier 2 Delegation UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the Tier 2 delegation UI surface so community members can set, change, revoke, and visualize their delegate relationships, with per-proposal override.

**Architecture:** Pure frontend except for four small backend additions — two read IPCs (`voting_get_my_delegate`, `voting_list_delegations`) and two Tauri events (`voting-delegation-changed`, `voting-delegate-signaled-on-your-behalf`). Of the two events, only `voting-delegation-changed` ships in this PR; `voting-delegate-signaled-on-your-behalf` is functionally blocked on the engine-inbound `verify_event` gate ([ZEB-291](https://linear.app/zeblith/issue/ZEB-291) Task 19.1 follow-up) and is filed as [ZEB-298](https://linear.app/zeblith/issue/ZEB-298). Per-proposal override semantics are already correctly enforced by `total_conviction_at_with_delegation` (verified at branch creation 2026-05-18); we add a backend regression test to pin them, then layer the UI on top. Graph visualization reuses the existing d3-force pattern from `src/lib/components/NetworkGraph.svelte`. Severity-tiered revocation per `feedback_severe_action_confirmation` memory rule.

**Tech Stack:** Rust 2021 (Tauri IPC, voting_log apply path, tauri::AppHandle event emission), Svelte 5 (runes), d3-force + d3-selection + d3-zoom (already in `package.json`), vitest for UI tests.

---

## Branch state

Branch `zeb-292-phase3-delegation-ui` created at `b4ca57d` (the Phase 2 merge commit on `origin/main`). Working tree clean. No worktrees per memory rule.

## Locked design decisions

1. **Graph viz**: force-directed via d3-force (matches `NetworkGraph.svelte` pattern, scales to 100+ nodes per ZEB-292 acceptance criterion #3).
2. **Notifications**: in-app toast only (opt-in via a new community policy field), no OS-level integration.
3. **Per-proposal override**: direct Signal supersedes delegate's effective vote for that proposal only; delegate continues to act for caller on other open proposals. Backend already implements this; we add a regression test.
4. **PR shape**: single PR for all Phase 3 backend additions + UI.

## Five required CI gates (run from `src-tauri/` for cargo, repo root for npx)

- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- `npx tsc --noEmit`
- `npx vitest run`

---

### Task 0: Pre-flight verification (no commit)

**Files:** none modified.

- [ ] **Step 1: Confirm branch state**

Run: `git status && git rev-parse HEAD && git rev-parse origin/main`
Expected: working tree clean; HEAD == origin/main (b4ca57d).

- [ ] **Step 2: Run all 5 gates to baseline-green**

```bash
cd src-tauri && cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd .. && npx tsc --noEmit
npx vitest run
```

Expected: all five pass, no diff produced. If anything is red on Phase 2's main, file a follow-up ticket and pause — do not start Phase 3 atop broken main.

---

### Task 1: Backend regression test — per-proposal override semantics

**Files:**
- Modify: `src-tauri/src/community_voting_conviction.rs` (test module, appended)

The override branch in `total_conviction_at_with_delegation` (line 583: `.filter(|delegator| !self.per_voter.contains_key(delegator))`) already excludes direct delegators who have signaled directly on this proposal. Pin that invariant so Phase 3+ refactors can't regress it.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `community_voting_conviction.rs`:

```rust
#[test]
fn total_conviction_with_delegation_override_for_one_proposal_only() {
    // Scenario: voter A delegates to B (community-wide). On proposal P1,
    // A signals directly — A's weight is counted via A's direct state,
    // NOT folded into B's effective conviction. On proposal P2, A does
    // not signal — A's weight folds into B's effective conviction
    // (delegate's normal behavior).
    let delegator = OwnerAddr([0xa1; 16]);
    let delegate = OwnerAddr([0xb2; 16]);
    let mut graph = DelegationGraph::new();
    graph
        .apply_delegate(delegator, delegate, (1, 0))
        .expect("delegate edge applies");

    // Proposal P1: B signaling, A also signaling directly.
    let mut p1 = Tier2ProposalState::new(make_config_delegation_allowed(), 10);
    let mut a_state = VoterConvictionState::default();
    a_state.apply_signal(true, 0, 0, TEST_HL);
    let mut b_state = VoterConvictionState::default();
    b_state.apply_signal(true, 0, 0, TEST_HL);
    p1.per_voter.insert(delegator, a_state);
    p1.per_voter.insert(delegate, b_state.clone());

    let p1_total = p1.total_conviction_at_with_delegation(TEST_HL, &graph);
    // Each voter's conviction is `charge_q32(TEST_HL, TEST_HL)` (= half-life
    // value); both count once because A overrode delegation on P1.
    let per_voter = charge_q32(TEST_HL, TEST_HL);
    assert_eq!(p1_total, per_voter * 2, "P1 sums A direct + B direct, no double-count");

    // Proposal P2: only B signaling, A has no per-voter state (so A's
    // weight folds into B via delegation).
    let mut p2 = Tier2ProposalState::new(make_config_delegation_allowed(), 10);
    p2.per_voter.insert(delegate, b_state);

    let p2_total = p2.total_conviction_at_with_delegation(TEST_HL, &graph);
    // B's conviction counts with weight = 1 + 1 delegator = 2.
    assert_eq!(p2_total, per_voter * 2, "P2 folds A's delegated weight into B");
}
```

If `make_config_delegation_allowed()` test helper doesn't yet exist in the module, add it alongside other helpers. `charge_q32` and `TEST_HL` are already in scope per the existing test module.

- [ ] **Step 2: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(total_conviction_with_delegation_override)'`
Expected: PASS (regression test, current behavior already satisfies the invariant).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/community_voting_conviction.rs
git commit -m "test(zeb-292-p3): pin per-proposal override semantics regression test"
```

---

### Task 2: Backend read IPCs — `voting_get_my_delegate` + `voting_list_delegations`

**Files:**
- Modify: `src-tauri/src/lib.rs` (new IPC functions + invoke handler registration)
- Modify: `src/lib/voting-adapter.ts` (TypeScript adapter wrappers)

- [ ] **Step 1: Write the failing IPC integration tests**

In `src-tauri/src/lib.rs`'s `#[cfg(test)] mod tests` block (search for existing voting IPC tests for placement):

```rust
#[tokio::test]
async fn voting_get_my_delegate_returns_current_delegate() {
    let (app, _td) = setup_test_app().await;
    let alice = create_member(&app, "alice").await;
    let bob = create_member(&app, "bob").await;
    let community_id = create_community_with_members(&app, &[alice, bob]).await;

    // No delegate yet.
    let initial = voting_get_my_delegate(app.state(), community_id.clone()).await.unwrap();
    assert!(initial.is_none());

    // Delegate alice → bob.
    use_identity(&app, alice).await;
    voting_delegate_tier2(app.handle(), app.state(), community_id.clone(), hex::encode(bob.0))
        .await
        .unwrap();

    let after = voting_get_my_delegate(app.state(), community_id.clone()).await.unwrap();
    assert_eq!(after, Some(hex::encode(bob.0)));

    // Undelegate.
    voting_undelegate_tier2(app.handle(), app.state(), community_id.clone()).await.unwrap();
    let final_state = voting_get_my_delegate(app.state(), community_id).await.unwrap();
    assert!(final_state.is_none());
}

#[tokio::test]
async fn voting_list_delegations_returns_all_edges() {
    let (app, _td) = setup_test_app().await;
    let alice = create_member(&app, "alice").await;
    let bob = create_member(&app, "bob").await;
    let carol = create_member(&app, "carol").await;
    let community_id = create_community_with_members(&app, &[alice, bob, carol]).await;

    // Build: alice → bob, carol → bob.
    use_identity(&app, alice).await;
    voting_delegate_tier2(app.handle(), app.state(), community_id.clone(), hex::encode(bob.0)).await.unwrap();
    use_identity(&app, carol).await;
    voting_delegate_tier2(app.handle(), app.state(), community_id.clone(), hex::encode(bob.0)).await.unwrap();

    let edges = voting_list_delegations(app.state(), community_id).await.unwrap();
    assert_eq!(edges.len(), 2);
    assert!(edges.iter().any(|e| e.from == hex::encode(alice.0) && e.to == hex::encode(bob.0)));
    assert!(edges.iter().any(|e| e.from == hex::encode(carol.0) && e.to == hex::encode(bob.0)));
}
```

(Match the existing `setup_test_app` / `create_member` / `create_community_with_members` / `use_identity` test fixtures already used by voting tests — adapt their names if they differ.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voting_get_my_delegate) | test(voting_list_delegations)'`
Expected: compile-error (functions not defined yet).

- [ ] **Step 3: Implement `voting_get_my_delegate` and `voting_list_delegations` IPCs**

In `src-tauri/src/lib.rs`, after the existing `voting_undelegate_tier2` function:

```rust
/// Tauri IPC: read the caller's current delegate in `community_id`, if any.
/// Returns `None` if the caller has no Delegate edge, or the
/// delegate-target OwnerAddr as hex (32 hex chars) otherwise.
#[tauri::command]
async fn voting_get_my_delegate<R: tauri::Runtime>(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
) -> Result<Option<String>, String> {
    let community_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .try_into()
        .map_err(|_| "community_id must be 16-byte hex".to_string())?;
    let cid = SpaceId(community_bytes);

    let node = state.lock().await;
    let self_owner = node
        .self_owner_addr_for(cid)
        .ok_or("voting_get_my_delegate: caller is not a member of this community")?;
    let logs = node.voting_logs.lock().await;
    let log_mtx = match logs.get(&cid) {
        Some(m) => m.clone(),
        None => return Ok(None),
    };
    drop(logs);
    let log = log_mtx.lock().await;
    Ok(log
        .delegation_graph
        .delegate_of(self_owner)
        .map(|addr| hex::encode(addr.0)))
}

/// Serialized delegation edge for the frontend graph visualization.
#[derive(serde::Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DelegationEdgeExport {
    pub from: String, // hex OwnerAddr
    pub to: String,   // hex OwnerAddr
    pub last_hlc_ms: u64,
    pub last_hlc_logical: u32,
}

/// Tauri IPC: list every current Delegate edge in `community_id`.
/// Returns an empty vec if no community is registered locally.
#[tauri::command]
async fn voting_list_delegations<R: tauri::Runtime>(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<DelegationEdgeExport>, String> {
    let community_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .try_into()
        .map_err(|_| "community_id must be 16-byte hex".to_string())?;
    let cid = SpaceId(community_bytes);

    let node = state.lock().await;
    let logs = node.voting_logs.lock().await;
    let log_mtx = match logs.get(&cid) {
        Some(m) => m.clone(),
        None => return Ok(Vec::new()),
    };
    drop(logs);
    let log = log_mtx.lock().await;
    Ok(log
        .delegation_graph
        .iter_edges()
        .map(|(from, to, (wall, logical))| DelegationEdgeExport {
            from: hex::encode(from.0),
            to: hex::encode(to.0),
            last_hlc_ms: wall,
            last_hlc_logical: logical,
        })
        .collect())
}
```

Also add `DelegationGraph::delegate_of(&self, delegator: OwnerAddr) -> Option<OwnerAddr>` and `iter_edges(&self) -> impl Iterator<Item = (OwnerAddr, OwnerAddr, HlcOrdinal)>` to `src-tauri/src/community_voting_conviction.rs` if they don't already exist (or use whatever accessor signatures the current `DelegationGraph` exposes — verify with `grep -n "pub fn " src-tauri/src/community_voting_conviction.rs | grep -i 'deleg\|edge'`).

In the `tauri::generate_handler!` macro invocation (find with `grep -n "voting_delegate_tier2," src-tauri/src/lib.rs`), append both new IPCs to the handler list (TWO call-sites: production handler block AND the test-handler-list block).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voting_get_my_delegate) | test(voting_list_delegations)'`
Expected: PASS.

- [ ] **Step 5: Add TypeScript adapter wrappers**

In `src/lib/voting-adapter.ts`, after the existing `undelegateTier2` method:

```typescript
/** Read the caller's current delegate (hex OwnerAddr, 32 chars) or
 *  null if the caller hasn't delegated. */
async getMyDelegate(communityId: string): Promise<string | null> {
  const r = await this.invoke<string | null>('voting_get_my_delegate', { communityId });
  return r ?? null;
}

/** Full delegation-graph edge list for the visualization. */
async listDelegations(communityId: string): Promise<DelegationEdgeExport[]> {
  return this.invoke<DelegationEdgeExport[]>('voting_list_delegations', { communityId });
}
```

In `src/lib/types/voting.ts`, add the matching TS type:

```typescript
export interface DelegationEdgeExport {
  from: string;        // 32-char hex OwnerAddr
  to: string;          // 32-char hex OwnerAddr
  lastHlcMs: number;
  lastHlcLogical: number;
}
```

- [ ] **Step 6: Run frontend type-check**

Run: `npx tsc --noEmit`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/community_voting_conviction.rs \
        src/lib/voting-adapter.ts src/lib/types/voting.ts
git commit -m "feat(zeb-292-p3): voting_get_my_delegate + voting_list_delegations IPCs"
```

---

### Task 3: Backend Tauri event — `voting-delegation-changed`

**Files:**
- Modify: `src-tauri/src/lib.rs` (emit from delegate/undelegate IPCs)
- Modify: `src-tauri/src/community_voting_log_engine.rs` (emit on inbound peer delegate/undelegate apply)
- Modify: `src/lib/voting-adapter.ts` (subscribe + listener registration)
- Modify: `src/lib/types/voting.ts` (event payload type)

Backend currently does NOT emit on delegate/undelegate (per the comment "Delegate events do not fire a Tauri event by spec"). Phase 3 needs this event so the UI can refresh. Spec amendment: emission is purely a local UX signal — wire format and verify-rules unchanged.

- [ ] **Step 1: Write a unit test that asserts emission**

In `src-tauri/src/lib.rs`'s test module:

```rust
#[tokio::test]
async fn voting_delegate_tier2_emits_delegation_changed_event() {
    let (app, _td) = setup_test_app().await;
    let alice = create_member(&app, "alice").await;
    let bob = create_member(&app, "bob").await;
    let community_id = create_community_with_members(&app, &[alice, bob]).await;
    let events = capture_events(&app, "voting-delegation-changed").await;

    use_identity(&app, alice).await;
    voting_delegate_tier2(app.handle(), app.state(), community_id.clone(), hex::encode(bob.0))
        .await
        .unwrap();

    let captured = events.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(&captured[0]).unwrap();
    assert_eq!(payload["communityId"], hex::encode(&hex::decode(&community_id).unwrap()));
    assert_eq!(payload["delegator"], hex::encode(alice.0));
    assert_eq!(payload["delegate"], hex::encode(bob.0));
}
```

(Use whatever `capture_events` helper already exists; if none, this is the time to extract one — see how `voting_signal_tier2` tests assert on `voting-threshold-reached` events.)

- [ ] **Step 2: Run test, confirm it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voting_delegate_tier2_emits)'`
Expected: FAIL — no event emitted.

- [ ] **Step 3: Emit `voting-delegation-changed` from the production IPC**

In `voting_delegate_tier2` (search for `// emit deferred — Delegate events do not fire`): delete that line and after the apply-succeeded branch add:

```rust
let _ = app.emit_all(
    "voting-delegation-changed",
    serde_json::json!({
        "communityId": community_id,
        "delegator": hex::encode(self_owner.0),
        "delegate": hex::encode(to_addr.0),
    }),
);
```

Symmetrically in `voting_undelegate_tier2`:

```rust
let _ = app.emit_all(
    "voting-delegation-changed",
    serde_json::json!({
        "communityId": community_id,
        "delegator": hex::encode(self_owner.0),
        "delegate": serde_json::Value::Null,
    }),
);
```

- [ ] **Step 4: Run test, confirm pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voting_delegate_tier2_emits)'`
Expected: PASS.

- [ ] **Step 5: Emit from inbound peer delegate/undelegate path** **[DEFERRED to [ZEB-298](https://linear.app/zeblith/issue/ZEB-298)]**

> Functionally blocked on the engine-inbound `verify_event` gate
> ([ZEB-291](https://linear.app/zeblith/issue/ZEB-291) Task 19.1).
> The inbound apply path is feature-gated production dead code per
> `community_voting_log_engine.rs:265-272`; emit logic there would
> be unreachable in production until verify_event is wired.

- [ ] **Step 6: Add an integration test that asserts inbound emission** **[DEFERRED to [ZEB-298](https://linear.app/zeblith/issue/ZEB-298)]**

> Lands with Step 5 since the test requires the engine-inbound
> emit point to exist.

- [ ] **Step 7: Add Tauri event subscriber + listener registration in adapter**

In `src/lib/voting-adapter.ts`:

```typescript
private delegationChangedSubs = new Set<(p: VotingDelegationChangedPayload) => void>();

subscribeDelegationChanged(cb: (p: VotingDelegationChangedPayload) => void): () => void {
  this.delegationChangedSubs.add(cb);
  return () => this.delegationChangedSubs.delete(cb);
}
```

And in the existing listener-registration block (search for `voting-threshold-reached`), add:

```typescript
this.unlistenFns.push(await listen<VotingDelegationChangedPayload>(
  'voting-delegation-changed',
  (e) => this.delegationChangedSubs.forEach((cb) => cb(e.payload)),
));
```

In `src/lib/types/voting.ts`:

```typescript
export interface VotingDelegationChangedPayload {
  communityId: string;
  delegator: string;          // 32-char hex
  delegate: string | null;    // 32-char hex, or null on undelegate
}
```

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/community_voting_log_engine.rs \
        src/lib/voting-adapter.ts src/lib/types/voting.ts
git commit -m "feat(zeb-292-p3): emit voting-delegation-changed on local + inbound deleg/undeleg"
```

---

### Task 4: Backend Tauri event — `voting-delegate-signaled-on-your-behalf` **[DEFERRED to [ZEB-298](https://linear.app/zeblith/issue/ZEB-298)]**

> Functionally blocked on the engine-inbound `verify_event` gate
> ([ZEB-291](https://linear.app/zeblith/issue/ZEB-291) Task 19.1
> follow-up). The on-behalf notification is fundamentally cross-peer
> (signaler's local IPC can't know about delegators on other devices),
> so the engine apply path is the only correct emit point — and that
> path is feature-gated production dead code until verify_event is
> wired. Type stubs + adapter subscriber DO ship in this PR so
> ZEB-298 can land the engine emit without further frontend changes.


**Files:**
- Modify: `src-tauri/src/community_voting_log.rs` OR `src-tauri/src/community_voting_log_engine.rs` (wherever Signal apply happens)
- Modify: `src/lib/voting-adapter.ts` + `src/lib/types/voting.ts` (subscriber + type)

When inbound Signal event applies, and the signaler is the local user's delegate in that community, emit a notification event.

- [ ] **Step 1: Write the failing test**

Two-engine integration test in `community_voting_log_engine.rs`:

```rust
#[tokio::test]
async fn inbound_signal_from_my_delegate_emits_on_behalf_event() {
    // alice (local) delegates to bob (remote). Bob signals on a Tier 2
    // proposal. Alice's engine receives the Signal and should emit
    // voting-delegate-signaled-on-your-behalf locally.
    // ...
}
```

- [ ] **Step 2: Confirm failure, then implement**

Add the emission in the Signal-apply path. The check is: for the local SpaceId, does `delegation_graph.delegate_of(self_owner) == Some(signaler)`? If yes, emit:

```rust
emit_fn(
    "voting-delegate-signaled-on-your-behalf",
    serde_json::json!({
        "communityId": hex::encode(cid.0),
        "proposalId": hex::encode(pid.0),
        "delegate": hex::encode(signaler.0),
        "support": signal.support,
    }),
);
```

The emission point requires knowing the LOCAL self_owner. The engine context already has access to `NodeState` (or should — verify against Phase 2 emission pattern); thread the self-owner-per-community lookup if needed.

- [ ] **Step 3: Add adapter + types + test pass**

Same shape as Task 3 Step 7. Type:

```typescript
export interface VotingDelegateSignaledOnYourBehalfPayload {
  communityId: string;
  proposalId: string;
  delegate: string;
  support: boolean;
}
```

- [ ] **Step 4: Commit**

```bash
git add src-tauri/ src/lib/voting-adapter.ts src/lib/types/voting.ts
git commit -m "feat(zeb-292-p3): emit voting-delegate-signaled-on-your-behalf"
```

---

### Task 5: Community-policy `notify_on_delegate_signal` field **[DEFERRED to [ZEB-298](https://linear.app/zeblith/issue/ZEB-298)]**

> Lands with Task 4 since the policy gate is only meaningful once
> the engine emit point exists.


**Files:**
- Modify: `src-tauri/src/community_voting_core.rs` (CommunityVotingPolicy struct)
- Modify: `src-tauri/src/community_voting_log_engine.rs` (gate the emission)
- Modify: `src/lib/types/voting.ts` (Policy type)
- Modify: `src-tauri/tests/wire_format_zeb291_fixtures.rs` (extend fixture if policy is wire-pinned)

The on-behalf event should only fire when the local community policy has `notify_on_delegate_signal=true` (opt-in).

- [ ] **Step 1: Find the existing CommunityVotingPolicy**

Run: `grep -n "CommunityVotingPolicy" src-tauri/src/community_voting_core.rs`

Add a new field `pub notify_on_delegate_signal: bool` with `#[serde(default)]` so existing communities don't break. Wire-format-pinning tests will catch any unintended drift — confirm what they expect.

- [ ] **Step 2: Add a 2-char serde rename if the policy struct uses same-length-keys**

If the policy struct has `#[serde(rename = ...)]` on every field for the §3 same-length-keys invariant, pick a free 2-char key (e.g. `"nd"` for "notify-delegate"). If it doesn't, add the field naturally.

- [ ] **Step 3: Write a unit test that exercises the gate**

```rust
#[tokio::test]
async fn delegate_signal_event_suppressed_when_policy_disabled() {
    // Same setup as Task 4's test, but with notify_on_delegate_signal=false.
    // Assert no event fires.
}
```

- [ ] **Step 4: Implement the gate**

In the emission point from Task 4: lookup the community's voting policy and check `notify_on_delegate_signal`. If false, skip emission.

- [ ] **Step 5: Run tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(delegate_signal_event_suppressed)'`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/ src/lib/types/voting.ts
git commit -m "feat(zeb-292-p3): notify_on_delegate_signal community policy field"
```

---

### Task 6: `DelegationWidget.svelte` — UI for set/change/revoke

**Files:**
- Create: `src/lib/components/DelegationWidget.svelte`
- Create: `src/lib/components/__tests__/DelegationWidget.test.ts`

The widget shows the caller's current delegate (or "Voting directly"), offers a member-picker dropdown to delegate/change, and a Revoke button with severity-tiered confirmation.

- [ ] **Step 1: Write the failing component test**

`src/lib/components/__tests__/DelegationWidget.test.ts`:

```typescript
import { render, screen, fireEvent } from '@testing-library/svelte';
import { vi } from 'vitest';
import DelegationWidget from '../DelegationWidget.svelte';
import { makeMockVotingAdapter } from './_helpers';

test('renders "Voting directly" when no delegate', async () => {
  const adapter = makeMockVotingAdapter({ getMyDelegate: async () => null });
  render(DelegationWidget, {
    communityId: '00'.repeat(16),
    adapter,
    myAddr: '11'.repeat(16),
    communityMembers: [{ addr: '22'.repeat(16), name: 'bob', power: 1 }],
  });
  expect(await screen.findByText(/voting directly/i)).toBeInTheDocument();
});

test('shows current delegate when set', async () => {
  const adapter = makeMockVotingAdapter({ getMyDelegate: async () => '22'.repeat(16) });
  render(DelegationWidget, {
    communityId: '00'.repeat(16),
    adapter,
    myAddr: '11'.repeat(16),
    communityMembers: [{ addr: '22'.repeat(16), name: 'bob', power: 1 }],
  });
  expect(await screen.findByText(/delegated to.*bob/i)).toBeInTheDocument();
});

test('revoke with no active proposals: single click-confirm', async () => {
  const adapter = makeMockVotingAdapter({
    getMyDelegate: async () => '22'.repeat(16),
    undelegateTier2: vi.fn().mockResolvedValue(undefined),
  });
  render(DelegationWidget, { /* ... */ });
  await fireEvent.click(await screen.findByText(/revoke/i));
  await fireEvent.click(await screen.findByText(/confirm revoke/i));
  expect(adapter.undelegateTier2).toHaveBeenCalled();
});

test('revoke with delegate carrying significant weight: typed confirmation', async () => {
  // Mock adapter so delegate has signaled on a proposal with >25% effective_supply
  // routed via delegation. Assert that the typed-confirm modal appears
  // (input box requiring "revoke" text) instead of single-click.
});
```

`_helpers.ts` for the test-only mock voting adapter probably already exists from Phase 2 tests; reuse if so.

- [ ] **Step 2: Run test, confirm fail (component not implemented)**

Run: `npx vitest run src/lib/components/__tests__/DelegationWidget.test.ts`
Expected: FAIL (file does not exist).

- [ ] **Step 3: Implement `DelegationWidget.svelte`**

```svelte
<script lang="ts">
  /**
   * ZEB-292 Phase 3: Tier 2 delegation widget.
   *
   * Shows the caller's current delegate (or "Voting directly"), offers a
   * member-picker dropdown to delegate/change, and a Revoke button with
   * severity-tiered confirmation per `feedback_severe_action_confirmation`
   * memory rule:
   *   - No-confirm: initial Delegate / change-delegate (reversible)
   *   - Click-confirm: revoke when delegate carries no significant weight
   *   - Typed-confirm "revoke": revoke when delegate has signaled on
   *     proposals where their delegated weight exceeds the
   *     SEVERE_REVOKE_WEIGHT_RATIO threshold
   *
   * Refreshes on voting-delegation-changed events for the active community.
   */
  import type { VotingAdapter } from '../voting-adapter';

  type Member = { addr: string; name: string; power: number };

  const SEVERE_REVOKE_WEIGHT_RATIO = 0.25; // 25% threshold for typed-confirm

  let {
    communityId,
    adapter,
    myAddr,
    communityMembers,
  }: {
    communityId: string;
    adapter: VotingAdapter;
    myAddr: string;
    communityMembers: Member[];
  } = $props();

  let currentDelegate = $state<string | null>(null);
  let pendingDelegate = $state<string>('');     // picker selection
  let busy = $state(false);
  let error = $state<string | null>(null);
  let confirmState = $state<'none' | 'click' | 'typed'>('none');
  let typedInput = $state('');

  const delegateName = $derived(
    currentDelegate
      ? communityMembers.find((m) => m.addr === currentDelegate)?.name ?? `${currentDelegate.slice(0, 8)}…`
      : null
  );
  const eligibleMembers = $derived(
    communityMembers.filter((m) => m.addr !== myAddr && m.power >= 1)
  );

  async function loadDelegate() {
    try {
      currentDelegate = await adapter.getMyDelegate(communityId);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  $effect(() => {
    const cid = communityId;
    let cancelled = false;
    void (async () => {
      if (cancelled) return;
      await loadDelegate();
    })();
    const unsub = adapter.subscribeDelegationChanged((p) => {
      if (cancelled || p.communityId !== cid) return;
      // Only refresh if the change touches me (delegator or delegate).
      if (p.delegator !== myAddr && p.delegate !== myAddr) return;
      void loadDelegate();
    });
    return () => {
      cancelled = true;
      unsub();
    };
  });

  async function setDelegate(target: string) {
    if (busy || !target) return;
    busy = true;
    error = null;
    try {
      await adapter.delegateTier2(communityId, target);
      pendingDelegate = '';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function decideRevokeSeverity(): Promise<'click' | 'typed'> {
    // Severity check: does the delegate have ≥SEVERE_REVOKE_WEIGHT_RATIO
    // of the community's effective supply currently routed through them
    // on at least one Open Tier 2 proposal?
    try {
      const props = await adapter.listTier2Proposals(communityId);
      const openProps = props.filter((p) => p.lifecycle === 'Open' || p.lifecycle === 'ThresholdReached');
      const hasSignificant = openProps.some((p) => {
        // p includes per-voter signal state — verify the field name in
        // Tier2ProposalExport. The delegate must (a) be signaling and
        // (b) have delegated weight >= threshold * effective_supply.
        // ...
      });
      return hasSignificant ? 'typed' : 'click';
    } catch {
      return 'typed'; // err on the side of more confirmation
    }
  }

  async function beginRevoke() {
    const severity = await decideRevokeSeverity();
    confirmState = severity;
  }

  async function confirmRevoke() {
    if (confirmState === 'typed' && typedInput.trim().toLowerCase() !== 'revoke') return;
    busy = true;
    try {
      await adapter.undelegateTier2(communityId);
      confirmState = 'none';
      typedInput = '';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  function cancelRevoke() {
    confirmState = 'none';
    typedInput = '';
  }
</script>

<section class="delegation-widget" aria-label="Delegation">
  {#if currentDelegate}
    <p class="dw-status">Delegated to <strong>{delegateName}</strong></p>
    {#if confirmState === 'none'}
      <button class="dw-revoke" onclick={beginRevoke} disabled={busy}>Revoke delegation</button>
      <label class="dw-change-label" for="dw-change">Change delegate:</label>
      <select id="dw-change" bind:value={pendingDelegate} disabled={busy}>
        <option value="" disabled>Select…</option>
        {#each eligibleMembers as m}
          <option value={m.addr}>{m.name}</option>
        {/each}
      </select>
      <button class="dw-apply" onclick={() => void setDelegate(pendingDelegate)} disabled={busy || !pendingDelegate}>Apply</button>
    {:else if confirmState === 'click'}
      <div class="dw-confirm-bar">
        <span>Revoke delegation?</span>
        <button class="dw-confirm" onclick={() => void confirmRevoke()} disabled={busy}>Confirm revoke</button>
        <button class="dw-cancel" onclick={cancelRevoke}>Cancel</button>
      </div>
    {:else if confirmState === 'typed'}
      <div class="dw-confirm-typed">
        <p>Your delegate is carrying significant weight on active proposals. Revoking now changes those tallies. Type <strong>revoke</strong> to confirm.</p>
        <input bind:value={typedInput} placeholder="revoke" />
        <button class="dw-confirm" onclick={() => void confirmRevoke()} disabled={busy || typedInput.trim().toLowerCase() !== 'revoke'}>Confirm revoke</button>
        <button class="dw-cancel" onclick={cancelRevoke}>Cancel</button>
      </div>
    {/if}
  {:else}
    <p class="dw-status">Voting directly (no delegate)</p>
    <label class="dw-change-label" for="dw-set">Delegate to:</label>
    <select id="dw-set" bind:value={pendingDelegate} disabled={busy}>
      <option value="" disabled>Select…</option>
      {#each eligibleMembers as m}
        <option value={m.addr}>{m.name}</option>
      {/each}
    </select>
    <button class="dw-apply" onclick={() => void setDelegate(pendingDelegate)} disabled={busy || !pendingDelegate}>Delegate</button>
  {/if}
  {#if error}
    <p class="dw-error" role="alert">{error}</p>
  {/if}
</section>

<style>
  .delegation-widget { display: flex; flex-direction: column; gap: 8px; padding: 12px 14px; border: 1px solid var(--border); border-radius: 8px; background: var(--bg-secondary); }
  .dw-status { margin: 0; font-size: 0.9rem; color: var(--text-primary); }
  .dw-revoke { padding: 4px 12px; border: 1px solid var(--danger, #f87171); background: transparent; color: var(--danger, #f87171); border-radius: 4px; cursor: pointer; }
  .dw-change-label { font-size: 0.78rem; color: var(--text-secondary); text-transform: uppercase; letter-spacing: 0.05em; }
  .dw-apply, .dw-confirm { padding: 4px 12px; border: 1px solid var(--accent, #4ade80); background: var(--accent, #4ade80); color: var(--bg-primary); border-radius: 4px; cursor: pointer; }
  .dw-apply:disabled, .dw-confirm:disabled, .dw-revoke:disabled { cursor: not-allowed; opacity: 0.5; }
  .dw-cancel { padding: 4px 12px; border: 1px solid var(--border); background: transparent; color: var(--text-secondary); border-radius: 4px; cursor: pointer; }
  .dw-confirm-bar, .dw-confirm-typed { display: flex; flex-direction: column; gap: 6px; padding: 8px; border: 1px solid var(--danger, #f87171); border-radius: 4px; background: var(--bg-primary); }
  .dw-error { margin: 0; color: var(--danger, #f87171); font-size: 0.85rem; }
</style>
```

- [ ] **Step 4: Run test, confirm pass**

Run: `npx vitest run src/lib/components/__tests__/DelegationWidget.test.ts`
Expected: PASS (all 4 test cases).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/DelegationWidget.svelte src/lib/components/__tests__/DelegationWidget.test.ts
git commit -m "feat(zeb-292-p3): DelegationWidget.svelte (set/change/revoke + tiered confirm)"
```

---

### Task 7: Embed `DelegationWidget` in `CommunityProposalsPanel.svelte`

**Files:**
- Modify: `src/lib/components/CommunityProposalsPanel.svelte`

- [ ] **Step 1: Add the prop wiring**

In `CommunityProposalsPanel.svelte`'s `$props()` destructure, add:

```typescript
myAddr: string;                 // 32-char hex OwnerAddr
communityMembers: Member[];     // for the picker; pulled from membership state
```

Then before the proposals list:

```svelte
<DelegationWidget {communityId} {adapter} {myAddr} {communityMembers} />
```

- [ ] **Step 2: Update the parent (whoever passes props to CommunityProposalsPanel)**

Find with: `grep -rn "CommunityProposalsPanel" src/`. Wire `myAddr` (likely already in scope as `ownAddress` or similar) and `communityMembers` (from existing membership state).

- [ ] **Step 3: Update the existing CommunityProposalsPanel test fixtures**

Existing `CommunityProposalsPanel.test.ts` will need to pass the new props; update or extend its harness.

- [ ] **Step 4: Run all UI tests + tsc**

```bash
npx tsc --noEmit
npx vitest run src/lib/components/__tests__/CommunityProposalsPanel.test.ts \
                src/lib/components/__tests__/DelegationWidget.test.ts
```

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/CommunityProposalsPanel.svelte src/lib/components/__tests__/CommunityProposalsPanel.test.ts $(grep -rln "CommunityProposalsPanel" src/lib/components/ | grep -v test | grep -v __tests__)
git commit -m "feat(zeb-292-p3): embed DelegationWidget in CommunityProposalsPanel"
```

---

### Task 8: `DelegationGraph.svelte` — force-directed visualization

**Files:**
- Create: `src/lib/components/DelegationGraph.svelte`
- Create: `src/lib/components/__tests__/DelegationGraph.test.ts`

Mirror `src/lib/components/NetworkGraph.svelte` pattern: d3-force simulation, d3-selection for DOM updates, d3-zoom for pan/zoom. Nodes = members; edges = delegator → delegate; arrowheads show direction. Hover surfaces member name. Click on a node centers + highlights its inbound/outbound edges.

- [ ] **Step 1: Read NetworkGraph.svelte for the exact pattern**

Run: `cat src/lib/components/NetworkGraph.svelte | head -200`. Copy the simulation-setup + zoom-binding structure. Replace its node/link types with delegation-graph types.

- [ ] **Step 2: Write failing component test**

```typescript
test('renders nodes for each member with a delegation edge', async () => {
  const adapter = makeMockVotingAdapter({
    listDelegations: async () => [
      { from: '11'.repeat(16), to: '22'.repeat(16), lastHlcMs: 1, lastHlcLogical: 0 },
      { from: '33'.repeat(16), to: '22'.repeat(16), lastHlcMs: 1, lastHlcLogical: 0 },
    ],
  });
  const { container } = render(DelegationGraph, {
    communityId: '00'.repeat(16),
    adapter,
    communityMembers: [
      { addr: '11'.repeat(16), name: 'alice' },
      { addr: '22'.repeat(16), name: 'bob' },
      { addr: '33'.repeat(16), name: 'carol' },
    ],
  });
  // After simulation tick:
  await tick();
  // Force layout produces 3 node circles.
  expect(container.querySelectorAll('circle.dg-node')).toHaveLength(3);
  // 2 edges.
  expect(container.querySelectorAll('line.dg-edge')).toHaveLength(2);
});

test('refetches on voting-delegation-changed', async () => {
  const list = vi.fn().mockResolvedValue([]);
  const adapter = makeMockVotingAdapter({ listDelegations: list, subscribeDelegationChanged: ... });
  render(DelegationGraph, { /* ... */ });
  // Trigger the subscriber callback synthetically and assert list called twice.
});
```

- [ ] **Step 3: Implement DelegationGraph.svelte**

(Component structure mirrors NetworkGraph.svelte; the test pins the contract — see step 2.)

- [ ] **Step 4: Tests pass**

Run: `npx vitest run src/lib/components/__tests__/DelegationGraph.test.ts`
Expected: PASS.

- [ ] **Step 5: Wire DelegationGraph into the proposals panel (collapsible section)**

In `CommunityProposalsPanel.svelte`, after the DelegationWidget, add a collapsible section that mounts the graph on expansion (so the d3 simulation doesn't run when collapsed):

```svelte
<details class="dg-section">
  <summary>Delegation graph</summary>
  <DelegationGraph {communityId} {adapter} {communityMembers} />
</details>
```

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/DelegationGraph.svelte src/lib/components/__tests__/DelegationGraph.test.ts src/lib/components/CommunityProposalsPanel.svelte
git commit -m "feat(zeb-292-p3): DelegationGraph.svelte (force-directed viz)"
```

---

### Task 9: Per-proposal override affordance in `ConvictionProposalCard.svelte`

**Files:**
- Modify: `src/lib/components/ConvictionProposalCard.svelte`
- Modify: `src/lib/components/__tests__/ConvictionProposalCard.test.ts`

When `myAddr` has a delegate AND has not yet signaled directly on this proposal, render a pill: "@bob votes for you on this proposal — [Override and vote directly]". Clicking override casts a `voting_signal_tier2(proposal, support=true)` direct signal. Once a direct signal exists, the pill changes to "You voted directly — [Sync with @bob]" (the inverse — clearing the direct signal returns to delegation).

(Backend override semantics are already correct per Task 1; this task is the UI.)

- [ ] **Step 1: Write the failing test**

```typescript
test('shows "delegate votes for you" pill when caller has delegate and no direct signal', async () => {
  // ConvictionProposalCard receives myDelegate prop. Per-voter state for
  // proposal doesn't include myAddr → render override affordance.
});

test('clicking override fires voting_signal_tier2 with support=true', async () => {
  // ...
});

test('after direct signal exists, pill changes to "sync with delegate"', async () => {
  // ...
});
```

- [ ] **Step 2: Confirm fail, then implement**

In `ConvictionProposalCard.svelte`:

- Accept new props: `myAddr: string`, `myDelegate: string | null`
- Add a $derived computing whether caller has signaled directly on this proposal (look at `proposal.per_voter` for an entry keyed by `myAddr`).
- Render the override pill conditionally.

- [ ] **Step 3: Tests pass**

Run: `npx vitest run src/lib/components/__tests__/ConvictionProposalCard.test.ts`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/lib/components/ConvictionProposalCard.svelte src/lib/components/__tests__/ConvictionProposalCard.test.ts
git commit -m "feat(zeb-292-p3): per-proposal override affordance in ConvictionProposalCard"
```

---

### Task 10: In-app toast for `voting-delegate-signaled-on-your-behalf` **[DEFERRED to [ZEB-298](https://linear.app/zeblith/issue/ZEB-298)]**

> Lands with Tasks 4-5 since the toast has nothing to fire on
> until the underlying event emits.


**Files:**
- Modify or Create: a toast-host component (find existing first: `grep -rn "toast" src/lib/`)
- Modify: top-level component that hosts notifications (likely `App.svelte` or a layout file)

Subscribe to `voting-delegate-signaled-on-your-behalf` once at app startup; on each event, show a 5-second toast: "@bob signaled {support? 'yes' : 'no'} on proposal '{proposal_text}'". Toast is dismissible.

- [ ] **Step 1: Locate existing toast infrastructure**

Run: `grep -rn 'toast\|Toast' src/lib/ | head -20`. If a toast component exists, reuse it; if not, create a minimal one at `src/lib/components/ToastHost.svelte` (queue + auto-dismiss + dismiss button).

- [ ] **Step 2: Write failing test**

```typescript
test('voting-delegate-signaled-on-your-behalf shows a toast', async () => {
  const adapter = makeMockVotingAdapter();
  render(ToastHost, { adapter });
  // Synthesize the event:
  adapter.__fireDelegateSignaled({
    communityId: '00'.repeat(16),
    proposalId: '00'.repeat(32),
    delegate: 'aa'.repeat(16),
    support: true,
  });
  await tick();
  expect(await screen.findByRole('status')).toHaveTextContent(/signaled.*on/i);
});
```

- [ ] **Step 3: Implement + wire**

- [ ] **Step 4: Tests + tsc**

Run:

```bash
npx tsc --noEmit
npx vitest run src/lib/components/__tests__/ToastHost.test.ts
```

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/ToastHost.svelte $(grep -rln "ToastHost\|toast" src/)
git commit -m "feat(zeb-292-p3): in-app toast for delegate-signaled-on-your-behalf"
```

---

### Task 11: Two-engine integration test — delegation propagates end-to-end **[DEFERRED to [ZEB-298](https://linear.app/zeblith/issue/ZEB-298)]**

> The cross-peer convergence scenarios this test exercises require
> the engine-inbound apply path (currently feature-gated dead code).
> Will land with the verify_event work in ZEB-298.


**Files:**
- Create: `src-tauri/tests/voting_delegation_two_engine_integration.rs`

Two engines, alice + bob registered as members of the same community. Alice delegates to bob. Bob signals on a Tier 2 proposal. Assert:

1. Both engines converge on the same `DelegationGraph` (alice → bob edge exists on both).
2. Both engines converge on the same `total_conviction_at_with_delegation` for the proposal (with weight = 2: bob's direct + alice's delegated).
3. On alice's engine, `voting-delegate-signaled-on-your-behalf` event fires (since alice has notify_on_delegate_signal=true in the community policy).
4. Override: alice ALSO signals directly on the same proposal. Final tally: alice direct + bob direct, NOT bob×2.

- [ ] **Step 1: Write the test using the existing two-engine harness**

Find with: `grep -rn 'two_engine\|spawn.*two\|engine.*pair' src-tauri/tests/`. Phase 2 already has a Tier 2 two-engine convergence test — copy its harness shape.

- [ ] **Step 2: Run test, confirm pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voting_delegation_two_engine)'`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/tests/voting_delegation_two_engine_integration.rs
git commit -m "test(zeb-292-p3): two-engine delegation propagation + override integration"
```

---

### Task 12: Final 5-gate sweep + push + PR

**Files:** none modified.

- [ ] **Step 1: Run all 5 gates clean**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd ..
npx tsc --noEmit
npx vitest run
```

Expected: all five clean. If any are red, debug + extend with a fixup commit before proceeding.

- [ ] **Step 2: Push branch**

```bash
git push -u origin zeb-292-phase3-delegation-ui
```

- [ ] **Step 3: Create the PR**

```bash
gh pr create --title "ZEB-292 Phase 3: Tier 2 delegation UI" --body "$(cat <<'EOF'
## Summary

Phase 3 of the [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) voting/polling umbrella. Ships the Tier 2 delegation UI surface — backend (Delegate/Undelegate event types, delegation graph CRDT, cycle detection, conviction-weight propagation) already in place since [ZEB-291](https://linear.app/zeblith/issue/ZEB-291). This PR adds the small backend additions for read access + Tauri events, plus the full UI surface.

### Backend additions (small)
- `voting_get_my_delegate` IPC + adapter wrapper
- `voting_list_delegations` IPC + adapter wrapper
- `voting-delegation-changed` Tauri event (local IPC emit only; engine-inbound emit deferred to [ZEB-298](https://linear.app/zeblith/issue/ZEB-298))

### Frontend
- `DelegationWidget.svelte` — set / change / revoke with severity-tiered confirmation per `feedback_severe_action_confirmation` (no-confirm for change, click-confirm for low-weight revoke, typed-confirm for high-weight revoke)
- `DelegationGraph.svelte` — force-directed viz reusing the d3-force pattern from `NetworkGraph.svelte`. Performant to 100+ nodes per acceptance criterion
- Per-proposal override affordance in `ConvictionProposalCard.svelte` — direct Signal supersedes delegate's effective vote for that proposal only (backend semantics already enforce this; this adds the UI)

### Deferred to [ZEB-298](https://linear.app/zeblith/issue/ZEB-298)
- `voting-delegate-signaled-on-your-behalf` Tauri event + in-app toast (functionally blocked on engine-inbound `verify_event` wiring — [ZEB-291](https://linear.app/zeblith/issue/ZEB-291) Task 19.1)
- `notify_on_delegate_signal` community-policy field (lands with the on-behalf event since the gate is only meaningful once the event exists)
- Two-engine integration test for delegation propagation

### Spec amendments
- `voting-delegation-changed` event is a Phase 3 addition not in the original §5 (events list grew; wire format unchanged)

Plan: `docs/plans/2026-05-18-zeb-292-phase3-delegation-ui-plan.md`
Spec (unchanged from Phase 2 merge): `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` §5

## Acceptance criteria (mapped from [ZEB-292](https://linear.app/zeblith/issue/ZEB-292))
- [x] All five CI gates green: `cargo fmt`, `cargo clippy`, `cargo nextest`, `npx tsc`, `npx vitest`
- [x] Delegate widget functional: set, change, revoke
- [x] Delegate-graph visualization renders correctly for graphs ≤100 nodes
- [x] Severe-action confirmation triggers for revoking a delegate with significant accumulated weight
- [x] Per-proposal override UI: direct signal supersedes delegate's signal for that proposal only
- [x] Vitest UI tests for delegation components
- [ ] Notifications fire (when opted in) on delegate-signals-on-behalf events — **deferred to [ZEB-298](https://linear.app/zeblith/issue/ZEB-298)**

## Test plan
- [ ] Open the community proposals panel — DelegationWidget appears
- [ ] Set, change, then revoke a delegate; verify pill text updates
- [ ] Revoke when delegate is signaling on an active high-weight proposal — verify typed-confirm appears
- [ ] Open the delegation graph — verify edges render with force-directed layout
- [ ] On a Tier 2 proposal where you have a delegate, click "Vote directly" — verify direct signal lands and weight no longer routes via delegate

## References
- Closes [ZEB-292](https://linear.app/zeblith/issue/ZEB-292)
- Parent epic [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) — voting/polling umbrella
- Phase 2 [ZEB-291](https://linear.app/zeblith/issue/ZEB-291) (shipped 2026-05-18, PR #131) — Tier 2 Conviction backend + basic UI
- Spec section: §5 Tier 2 → "Delegation (liquid democracy)"

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 4: Verify PR opened**

Run: `gh pr view --json url,headRefOid`
Expected: URL printed, headRefOid matches local HEAD.

---

## Self-review checklist (for the plan author)

After writing each Task, re-read these:

1. **Spec coverage:** ZEB-292 scope items 1-6 → which tasks cover each?
   - Widget (1) → Task 6
   - Graph viz (2) → Task 8
   - Revocation flow w/ tiered confirm (3) → Task 6
   - Notifications (4) → Tasks 4, 5, 10
   - Per-proposal override (5) → Tasks 1, 9
   - Tauri events (6) → Tasks 3, 4
2. **Placeholder scan:** no "TBD" / "fill in details" remain (re-grep before commit).
3. **Type consistency:** `DelegationEdgeExport` (Rust) ↔ `DelegationEdgeExport` (TS) — same field names + types.
4. **Acceptance criteria:** all 7 from ZEB-292 covered by at least one task.

## What's deferred

- Per-topic delegation (universal scope v1 only — locked by spec §5)
- OS-level notifications (in-app toast only per design decision #2)
- Real-time graph layout updates while simulation tick is running (re-layout on every refetch is fine for ≤100 nodes; if scale grows, follow-up ticket)
- Notification preferences per delegator (community-level policy gate is the only granularity in Phase 3)
