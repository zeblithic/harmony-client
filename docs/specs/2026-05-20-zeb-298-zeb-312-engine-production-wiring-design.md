# ZEB-298 + ZEB-312: Voting Engine Production Wiring Design

**Status:** Combined design for the two-PR sequence that production-activates the voting engine. User-approved direction 2026-05-20.

**Scope summary:**
- **[ZEB-298](https://linear.app/zeblith/issue/ZEB-298)** — Tier 2 delegate-on-behalf notification (Tauri event + community policy + toast UI + two-engine integration test). Blocked on the inbound-voting feature-gate.
- **[ZEB-312](https://linear.app/zeblith/issue/ZEB-312)** — Tier 3 IPC routing through `engine.publish_event` + fire post-apply hooks on inbound + rewrite IPC integration tests.
- **Implicit "ZEB-291 Task 19.1"** — wire Zenoh adapter + plumb `verify_event` with per-community membership snapshot + remove feature-gate. No standalone Linear ticket (referenced as deferred work in `community_voting_log_engine.rs:1434-1439`).

## PR shape

**Two sequential PRs** (user-approved):

1. **PR 1 — Foundation** (~1000-1200 LOC): Zenoh adapter wiring, `verify_event` with membership snapshot, gate removal, engine-spawn upgrade. No semantic change for end-users; the engine becomes production-active (accepts peer events, broadcasts outbound) but Tier 3 IPCs still apply directly to log so engine-auto orchestration stays dormant until PR 2.
2. **PR 2 — Consumer** (~1300-1500 LOC): Route Tier 3 IPCs through `engine.publish_event`, fire post-apply hooks on inbound, add Tier 2 delegate-on-behalf emit + policy field + `ToastHost.svelte`, rewrite IPC integration tests, two-engine integration test for delegate-on-behalf. PR 2 closes both [ZEB-298](https://linear.app/zeblith/issue/ZEB-298) + [ZEB-312](https://linear.app/zeblith/issue/ZEB-312).

PR 1 lands first + bakes under bot review; PR 2 branches off the merged PR 1.

## Current state

| Component | State |
|---|---|
| `publisher_tx` (outbound) | Drained to floor in `ensure_voting_engine_for` (`lib.rs:22171-22181`); never reaches Zenoh |
| `subscriber_rx` (inbound) | Closed-channel stub (`lib.rs:22182-22183`); never receives peer events |
| `process_inbound` | Feature-gated with `#[cfg(not(any(test, feature = "test-fixtures")))]` — production builds early-return with "inbound voting events are refused until ZEB-291 Task 19.1 wires verify_event with the per-community membership snapshot" (`community_voting_log_engine.rs:1434-1439`) |
| `verify_event` | No `snapshot` parameter; per-actor membership check not enforced for inbound |
| Engine fields | `hlc_tracker: None`, `device_id: None`, `app_handle: None`, `local_signing: None` — all dormant per ZEB-310 Task 9 comments at `ensure_voting_engine_for` |
| Tier 3 IPCs | Apply directly via `log.apply_with_snapshot`, bypassing `engine.publish_event` |
| Hooks | Only fire from `publish_event`, not from inbound apply path |
| Tier 2 delegate-on-behalf Tauri event | Stub exists in `voting-adapter.ts` (PR #132); emit logic in engine is dead code behind the feature-gate |
| Toast/notification UI | None — only "would be a toast here" comments in `CommunityView.svelte`, `DmCreateDialog.svelte`, `FileBrowser.svelte` |

## PR 1: Foundation

### 1. Zenoh adapter wiring

Mirror the `DfrostLogEngine` Zenoh wiring pattern (PR #146, ZEB-307). Each per-community voting engine subscribes to `harmony/community/{id}/voting` for inbound and publishes via `put` on the same topic for outbound. The adapter lives in (or hooks into) `event_loop.rs` or `community_dfrost_log_engine.rs`'s sibling pattern.

**Replace** `ensure_voting_engine_for`'s stub mpsc-pair (drained `publisher_tx`, closed `subscriber_rx`) with real Zenoh-backed pairs:

```rust
// In ensure_voting_engine_for:
let (publisher_tx, publisher_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
let (subscriber_tx, subscriber_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

// Outbound: spawn task that forwards publisher_rx → Zenoh put on topic.
let topic = format!("harmony/community/{}/voting", hex::encode(community_id.0));
let zenoh_session_handle = zenoh_session.clone();  // already in NodeState
tokio::spawn(async move {
    let mut rx = publisher_rx;
    while let Some(bytes) = rx.recv().await {
        if let Err(e) = zenoh_session_handle.put(&topic, bytes).await {
            tracing::warn!(error = %e, topic = %topic, "voting publisher → Zenoh put failed");
        }
    }
});

// Inbound: subscribe to topic, forward into subscriber_tx.
let sub_topic = topic.clone();
let zenoh_session_handle = zenoh_session.clone();
tokio::spawn(async move {
    match zenoh_session_handle.declare_subscriber(&sub_topic).await {
        Ok(subscriber) => {
            while let Some(sample) = subscriber.next().await {
                if let Err(e) = subscriber_tx.send(sample.payload.into()).await {
                    tracing::warn!(error = %e, topic = %sub_topic, "voting subscriber → engine.subscriber_tx closed");
                    break;
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, topic = %sub_topic, "voting Zenoh declare_subscriber failed");
        }
    }
});
```

Exact Zenoh API calls match the DfrostLog adapter's pattern (`community_dfrost_log_engine.rs` lines ~700-900 in PR #146).

### 2. `verify_event` with membership snapshot

Refactor signature to take the snapshot needed for actor membership / eligibility checks:

```rust
// Before:
pub fn verify_event(event: &SignedVotingEvent) -> Result<(), VerifyError>;

// After:
pub fn verify_event(
    event: &SignedVotingEvent,
    membership_snapshot: &MembershipSnapshot,
) -> Result<(), VerifyError>;
```

`MembershipSnapshot` is the existing per-community snapshot type used by Tier 1's `check_eligibility` (in `community_voting_core.rs`). All ~30+ existing `verify_event` call sites get a snapshot argument plumbed through. For most internal callers (IPCs that already build a snapshot for eligibility), this is mechanical.

### 3. `process_inbound` resolves snapshot + removes feature-gate

```rust
async fn process_inbound(
    community_id: SpaceId,
    log: &Arc<Mutex<VotingLog>>,
    tracker: &PublishedEventTracker,
    membership_snapshot_resolver: &Arc<dyn MembershipSnapshotResolver>,  // NEW
    packet: &[u8],
) -> Result<(), ProcessInboundError> {
    // Decode event from packet.
    let event = decode_signed_voting_event(packet)?;

    // Self-loopback drop (unchanged).
    if tracker.contains(&event) {
        return Ok(());
    }

    // Resolve membership snapshot — case-split on event kind:
    //   - PollCreate: build fresh snapshot at event.hlc.
    //   - All others: look up the poll, use its frozen snapshot (eligible_electorate_snapshot).
    let snapshot = match event.kind {
        PollEventKindCode::PollCreate => {
            membership_snapshot_resolver
                .snapshot_at(community_id, &event.hlc)
                .await?
        }
        _ => {
            // Non-PollCreate events use the poll's frozen snapshot from state.
            let poll_id = derive_poll_id(&event);
            let log_g = log.lock().await;
            log_g.poll_state(&poll_id)
                .map(|s| s.eligible_electorate_snapshot.clone())
                .ok_or(ProcessInboundError::PollNotFound)?
        }
    };

    // Verify with snapshot. Replaces the feature-gate early-return.
    crate::community_voting_core::verify_event(&event, &snapshot)
        .map_err(ProcessInboundError::VerifyFailed)?;

    // Apply (existing path).
    let mut log_g = log.lock().await;
    log_g.apply_with_snapshot(event, &community_id, Some(snapshot))
        .map_err(ProcessInboundError::ApplyFailed)?;

    Ok(())
}
```

`MembershipSnapshotResolver` is a new trait the engine holds via `Arc<dyn MembershipSnapshotResolver>`. Production impl reads from `community_registry` + `crdt_state` (already in `NodeState`); test impl is a fixed map.

```rust
#[async_trait]
pub trait MembershipSnapshotResolver: Send + Sync {
    async fn snapshot_at(
        &self,
        community_id: SpaceId,
        hlc: &Hlc,
    ) -> Result<MembershipSnapshot, SnapshotResolverError>;
}
```

The `#[cfg(not(any(test, feature = "test-fixtures")))]` block at `community_voting_log_engine.rs:1434-1439` is **removed entirely**.

### 4. Engine spawn upgrade

`ensure_voting_engine_for` now plumbs everything:

```rust
async fn ensure_voting_engine_for(
    voting_logs: &VotingLogsMap,
    voting_log_engines: &VotingLogEnginesMap,
    community_id: SpaceId,
    // NEW:
    zenoh_session: Arc<zenoh::Session>,
    hlc_tracker: Arc<HlcTracker>,
    device_id: Arc<String>,
    app_handle: AppHandle<tauri::Wry>,
    local_signing_key: Arc<ed25519_dalek::SigningKey>,
    local_owner: OwnerAddr,
    membership_resolver: Arc<dyn MembershipSnapshotResolver>,
    // (existing dfrost params unchanged)
    dfrost_log_registry: Option<Arc<DfrostLogRegistry<tauri::Wry>>>,
    beacon_requester: Option<BeaconRequester>,
) -> Result<(), String>
```

After engine construction, install local signing key:

```rust
crate::community_voting_log_engine::VotingLogEngine::install_local_signing_key(
    &engine, local_signing_key, local_owner,
).await;
```

The 6 Tier 3 IPCs (and any other callers of `ensure_voting_engine_for`) get the new params from `NodeState` handles (all of which are already there).

### 5. Tests

- **Unit test (production build):** Verify peer event applies via inbound path when membership snapshot is provided. Use `cargo nextest run` WITHOUT `--features test-fixtures` on a focused test that exercises `process_inbound` directly. This confirms the gate is gone.
- **Multi-engine integration test:** Two-engine bridge with REAL Zenoh adapter (not the mpsc test bridge). Engine A publishes a Tier 1 PollCreate via `publish_event`; engine B receives via Zenoh and applies. Verifies the full outbound→inbound loop.
- **verify_event unit tests:** Snapshot mismatch (actor not in membership) → Err. Snapshot match → Ok. Re-verify all existing verify-related tests under the new signature.

### Acceptance criteria (PR 1)

1. Five CI gates green from `src-tauri/`: fmt, clippy, nextest --workspace --all-targets --features test-fixtures, tsc, vitest.
2. `cargo nextest run --locked -p harmony-app -E 'test(process_inbound_peer_apply)'` passes in a **production build** (no `--features test-fixtures` flag), proving the gate is removed.
3. Two-engine Zenoh integration test: peer event flows through real Zenoh, applies on receiving engine, state diverges then converges as expected.
4. `verify_event` with snapshot rejects events whose actor is not in membership.
5. `ensure_voting_engine_for` installs `hlc_tracker` + `device_id` + `app_handle` + `local_signing` — verified via existing engine-auto orchestration tests under the new wiring (they should still pass with `app_handle: Some(_)` rather than `None`).
6. Outbound Zenoh publish from `publisher_tx` → `harmony/community/{id}/voting` topic verified by integration test.
7. No regression on existing voting tests (Tier 1 + Tier 2 + Tier 3 unit + integration).

## PR 2: Consumer

Branches off PR 1's merged main. PR 2 closes both [ZEB-298](https://linear.app/zeblith/issue/ZEB-298) + [ZEB-312](https://linear.app/zeblith/issue/ZEB-312).

### 6. Route Tier 3 IPCs through `engine.publish_event`

Each of the 6 Tier 3 IPCs (currently calling `log.apply_with_snapshot` directly):
- `voting_create_tier3_proposal`
- `voting_submit_deliberation_statement`
- `voting_propose_draft_candidate`
- `voting_approve_draft_candidate`
- `voting_decline_sortition`
- `voting_cast_ratification_ballot`

Switches to `engine.publish_event(event)`. The IPC's pre-flight (validate config + snapshot + check eligibility + reserve HLC + sign) stays the same; only the final apply step changes.

Special case: `voting_create_tier3_proposal` needs to thread the eligible-electorate snapshot through `engine.publish_event` so the engine can pass it to `apply_with_snapshot`. Either:
- (a) `publish_event` grows a `snapshot: Option<MembershipSnapshot>` param (currently absent), OR
- (b) The engine resolves the snapshot internally using its `MembershipSnapshotResolver`.

**Recommendation: (a)** — IPC already has the snapshot from eligibility check; pass it through. Simpler than re-resolving.

### 7. Fire post-apply hooks on inbound

In `process_inbound` (after the apply step from PR 1), call the existing hooks:

```rust
// ... existing apply + verify ...

// ZEB-312: fire engine-auto orchestration on inbound apply too
self.maybe_trigger_engine_auto_orchestration(&pid).await;
self.maybe_trigger_beacon_for_tier3_create(&event).await;
self.maybe_emit_tier3_lifecycle_events(&event, &previous_stage, &pid).await;
self.maybe_emit_delegate_on_behalf(&event, &pid).await;  // NEW for ZEB-298
```

This requires `maybe_trigger_engine_auto_orchestration` to be safely re-entrant from the inbound apply path (where the log lock is already held differently than the publish_event path). Need to verify lock-ordering — likely will need to refactor to drop the log lock between apply and hook invocation in `process_inbound`.

### 8. ZEB-298 Tier 2 delegate-on-behalf emit

New hook `maybe_emit_delegate_on_behalf(&event, &pid)` in the engine:
- Triggers on apply of kd=signal (Tier 2 Signal event).
- Loads the community's voting policy.
- If `notify_on_delegate_signal == true` AND the signaler is the local user's current delegate in this community AND the local user is a registered member:
  - Emit `voting-delegate-signaled-on-your-behalf` with `VotingDelegateSignaledOnYourBehalfPayload`.

The payload struct already exists in `lib.rs` from PR #132. The Tauri event name + subscriber already exist in `voting-adapter.ts`. This step wires the production emit logic.

### 9. `notify_on_delegate_signal` community policy field

New field on `CommunityVotingPolicy` (or wherever Tier 2 policy lives — check `community_voting_conviction.rs`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommunityVotingPolicy {
    // ... existing fields ...

    /// ZEB-298: when true, notify members of a delegate's signal on their
    /// behalf via the `voting-delegate-signaled-on-your-behalf` Tauri event.
    /// Opt-in (default false) so existing communities don't suddenly notify.
    #[serde(default, rename = "nd")]
    pub notify_on_delegate_signal: bool,
}
```

Wire-format update: if the policy struct is byte-pinned in `wire_format_zeb291_fixtures.rs`, regenerate the affected fixture. `#[serde(default)]` ensures backward compat.

Add a setter IPC or community-admin path so the policy can be changed. For PR 2 scope, a simple IPC `voting_set_notify_on_delegate_signal(community_id, enabled)` suffices.

### 10. `ToastHost.svelte`

Create `src/lib/components/ToastHost.svelte`:

```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { getContext } from 'svelte';
  import { fade, fly } from 'svelte/transition';

  type Toast = {
    id: string;
    message: string;
    durationMs: number;
  };

  let toasts = $state<Toast[]>([]);

  function show(message: string, durationMs = 5000): void {
    const id = crypto.randomUUID();
    toasts = [...toasts, { id, message, durationMs }];
    setTimeout(() => dismiss(id), durationMs);
  }

  function dismiss(id: string): void {
    toasts = toasts.filter(t => t.id !== id);
  }

  // Expose `show` to consumers via setContext-based singleton pattern.
  // ...
</script>

<div class="toast-host" aria-live="polite">
  {#each toasts as t (t.id)}
    <div class="toast" transition:fly={{ y: 20, duration: 200 }}>
      {t.message}
      <button onclick={() => dismiss(t.id)} aria-label="Dismiss">×</button>
    </div>
  {/each}
</div>

<style>
  /* tasteful toast styling — bottom-right corner, max-width 320px, etc. */
</style>
```

Place the host in the top-level app layout (likely `App.svelte` or `+layout.svelte` depending on routing). Subscribe to `voting-delegate-signaled-on-your-behalf` and call `show()` with the formatted message:

> "@{delegateName} signaled {support ? 'support for' : 'against'} '{proposalText}'"

Dismissible via × button or auto-dismiss after 5 seconds.

### 11. Two-engine integration test (ZEB-298 #4)

Add to `src-tauri/tests/community_voting_tier2_integration.rs` (or new file):
- alice delegates to bob in community C
- bob signals on a Tier 2 proposal on engine B
- engine A receives via Zenoh
- Assert: delegation graph consistent across A + B
- Assert: `total_conviction_at_with_delegation` matches across engines
- Assert: `voting-delegate-signaled-on-your-behalf` fires on engine A (with policy enabled)
- alice directly overrides on the same proposal → bob's effective weight drops on that proposal only

### 12. Rewrite IPC integration tests (ZEB-312 #9)

`community_voting_tier3_ipc_integration.rs` Tests 1-4 currently use Path C (engine-layer invocation). Rewrite to Path A or B (invoke IPCs as functions or through Tauri mock app). Requires constructing a mock `NodeState` with the new wiring (zenoh_session, hlc_tracker, etc.) — heavier setup but exercises the real IPC happy path.

Test 5 (error extraction) already uses Path A; unchanged.

### Acceptance criteria (PR 2)

1. Five CI gates green.
2. Tier 3 IPCs route through `engine.publish_event` (verified by inspecting code + by integration tests).
3. `voting_cast_ratification_ballot` IPC successfully triggers engine-auto kd=cl + kd=rs in a production-like test.
4. Peer-delivered events fire post-apply hooks (engine-auto orchestration + Tauri lifecycle events).
5. `voting-delegate-signaled-on-your-behalf` fires on engine A when bob signals on engine B with policy enabled.
6. `ToastHost.svelte` renders + dismisses + appears in app layout.
7. Two-engine integration test passes end-to-end (delegation + signal + notification + override).
8. `community_voting_tier3_ipc_integration.rs` Tests 1-4 exercise IPC happy path.
9. `notify_on_delegate_signal` policy field round-trips through CBOR; existing communities default to false.

## Out of scope (both PRs)

- ZEB-311 UI (Tier 3 governance flow components). Separate ticket.
- Phase 5/6/7 (Pol.is deliberation, ballot-secret, TRIP receipt-free).
- Tier 1 / Tier 2 IPC re-wiring through engine (no engine-auto orchestration depends on them; current direct-apply pattern is fine).
- Wire-format changes (no on-the-wire schema changes; just production-wiring).
- D-FROST beacon orchestration UI surfacing.
- General-purpose "toast service" with stack management, queue, or rich content. ToastHost is minimal MVP.

## References

- [ZEB-298](https://linear.app/zeblith/issue/ZEB-298) — Tier 2 delegate-on-behalf notification + community policy + integration test
- [ZEB-312](https://linear.app/zeblith/issue/ZEB-312) — Tier 3 engine-auto orchestration production wiring
- [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) — umbrella voting/polling spec
- [ZEB-310](https://linear.app/zeblith/issue/ZEB-310) PR #149 — ZEB-310 Phase 4a-main shipped with documented dormancy in PR body's "Engine wiring + dormancy gap" section
- DfrostLog Zenoh adapter pattern: [ZEB-307](https://linear.app/zeblith/issue/ZEB-307) PR #146 — `community_dfrost_log_engine.rs` (foundation for VotingLog adapter)
- Current feature-gate: `src-tauri/src/community_voting_log_engine.rs:1434-1439`
- Current engine spawn: `src-tauri/src/lib.rs:22134` (`ensure_voting_engine_for`)
