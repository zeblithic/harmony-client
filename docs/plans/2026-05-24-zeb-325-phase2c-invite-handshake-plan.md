# ZEB-321 Phase 2c — Invite Handshake Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the cross-WAN invite-redemption handshake so `connectivity_redeem_invite_iroh` actually completes a join instead of returning `pkarr_resolved_no_handshake`.

**Architecture:** Per spec §7.2 + verified pre-conditions in ZEB-325. Option C: pure Zenoh-CRDT-sync. Bob seeds the ReachabilityResolver with the pkarr-resolved routing record so Phase 1's IrohZenohLinkManager can open the iroh connection on demand; Bob then calls the existing `redeem_invite_inner` with a new `allow_no_reticulum_destinations: bool` parameter that skips the Reticulum-destinations fast-fail. Bob's local PendingJoin insert + state-root publish reach Alice via the iroh-borne Zenoh link; her engine counter-signs and her state-root publish carries the counter-signed event back to Bob via the same link, firing his existing wait-for-counter-sig oneshot.

**Tech Stack:** Rust (Tokio), iroh 0.98, Zenoh 1, Svelte 5 runes, Tauri 2.

**Spec:** `docs/specs/2026-05-23-zeb-321-phase2-discovery-bootstrap-design.md` §7.2 (commit `cb5cca5`).

**Branch:** `zeb-321-phase2c-invite-handshake` (already created off main `3c4c21d`).

---

## File Structure

| Change | Path | Responsibility |
|---|---|---|
| Modify | `src-tauri/src/lib.rs` | `redeem_invite_inner` gets `allow_no_reticulum_destinations` param; `connectivity_redeem_invite_iroh` seeds resolver + calls it |
| Modify | `src-tauri/src/reachability_resolver.rs` | Add `seed_from_pkarr` method that writes a `ReachabilityAnnouncePayload` directly into the in-memory CRDT-sourced map with a `PkarrSeeded` provenance variant |
| Modify | `src-tauri/src/reachability_record.rs` | Helper `ReachabilityAnnouncePayload::resolver_key` returning the composite key used by ReachabilityResolver (if not already exposed) |
| Modify | `src/lib/components/RedeemInviteDialog.svelte` | Handle `joined` outcome from iroh path; remove "Found on network, handshake pending" path |
| Modify | `src/lib/connectivity-adapter.ts` | (Probably no change — RedemptionOutcome shape already supports `joined`) |
| Create | `src-tauri/tests/pkarr_iroh_redeem_full_integration.rs` | Two-process integration test exercising pure-iroh redeem (no Reticulum destinations) |

---

## Task 0: Pre-flight green baseline (no commit)

**Files:** none

- [ ] **Step 0.1: Verify on the right branch off the right commit**

Run:
```bash
git rev-parse --abbrev-ref HEAD  # expected: zeb-321-phase2c-invite-handshake
git merge-base HEAD origin/main  # expected: 3c4c21d (Phase 2b merge)
```

- [ ] **Step 0.2: Capture orphan-failure baseline (do NOT commit; reference only)**

Run from `src-tauri/`:
```bash
cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast 2>&1 | tee /tmp/zeb325_baseline.txt | tail -50
```

Note the FAIL lines into a mental list. New failures introduced by this PR are blocking; pre-existing orphans (`folder_ingest::tests`, `mint::tests`, `mint_sync::tests`, `folder_ingest_walker_integration`, `rename_content_integration`) are not.

- [ ] **Step 0.3: Verify fmt + clippy gates clean on baseline**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: both clean (post-PR-#158 main was green).

---

## Task 1: Add `allow_no_reticulum_destinations` parameter to `redeem_invite_inner`

**Files:**
- Modify: `src-tauri/src/lib.rs:14682-15028` (function signature + two fast-fail sites)
- Modify: every caller of `redeem_invite_inner` (Step 1.2 enumerates)

- [ ] **Step 1.1: Write the failing test**

Create test in `src-tauri/src/lib.rs` test module (or wherever existing redeem_invite_inner tests live — search for `mod tests` near line 14682). New test name: `redeem_invite_inner_allow_no_reticulum_destinations_skips_fast_fail`.

```rust
#[tokio::test]
async fn redeem_invite_inner_allow_no_reticulum_destinations_skips_fast_fail() {
    // ... full test scaffolding pattern from existing redeem_invite_inner tests ...
    // Set up state where resolve_destinations_for_owner returns empty Vec.
    // Call redeem_invite_inner with allow_no_reticulum_destinations = true.
    // Verify the function does NOT early-return; instead it proceeds to the
    // oneshot await (which will time out in 5s without a counter-sig — that's
    // fine, we're testing the gate, not the full success path).
    // Verify the returned Result is Ok(_) with pending:true (the PendingJoin
    // is on the wire), NOT Err("no known device for inviter").
}
```

Run: `cargo nextest run --locked --features test-fixtures -E 'test(=redeem_invite_inner_allow_no_reticulum_destinations_skips_fast_fail)'`
Expected: FAIL with compilation error (parameter does not exist).

- [ ] **Step 1.2: Add the parameter**

Edit `src-tauri/src/lib.rs:14682-14706` — the `redeem_invite_inner` signature. Add `allow_no_reticulum_destinations: bool` as the last parameter (after `identity_dir`).

Edit the two fast-fail sites:

At `src-tauri/src/lib.rs:15018-15028`, change:
```rust
if destinations.is_empty() {
    // existing rollback + Err return
}
```
To:
```rust
if destinations.is_empty() {
    if allow_no_reticulum_destinations {
        // No Reticulum destinations — but caller has guaranteed an alternate
        // transport (iroh-borne Zenoh CRDT sync). Skip the Reticulum unicast
        // fan-out entirely and proceed to the oneshot await; the engine's
        // state-root publisher will carry the PendingJoin to the inviter via
        // Zenoh. See ZEB-325 Phase 2c.
        tracing::debug!(
            inviter = %hex::encode(inviter_addr.0),
            "redeem_invite_inner: no Reticulum destinations; relying on CRDT sync (allow_no_reticulum_destinations=true)"
        );
    } else {
        // (Existing rollback + Err return.)
        let _ = community_registry
            .take_pending_redemption(&minted.bootstrap_join.id)
            .await;
        return Err(format!(
            "no known device for inviter {} — invite cannot route",
            hex::encode(inviter_addr.0)
        ));
    }
} else {
    // (Existing per-destination fan-out at lines 15029-15071, indented one level.)
}
```

The `any_sent` check at `15058-15071` only runs when `destinations` was non-empty — i.e., inside the `else` branch above. It does NOT need a flag; if we got into the fan-out at all, at least one send is required.

Update every caller of `redeem_invite_inner` to pass `false` (preserving existing behavior). Find them with:
```bash
grep -n "redeem_invite_inner(" src-tauri/src/lib.rs src-tauri/tests/ 2>/dev/null
```
Expected callers (from earlier exploration): lib.rs:15507, 15628, 15995, 16146, 16268, 16371. Update each one.

- [ ] **Step 1.3: Run the test, verify it passes**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(=redeem_invite_inner_allow_no_reticulum_destinations_skips_fast_fail)'`
Expected: PASS.

- [ ] **Step 1.4: Re-run full pkarr + redeem test suite to verify no regression**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(redeem_invite) or test(pkarr)'
```
Expected: all PASS.

- [ ] **Step 1.5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-325): allow_no_reticulum_destinations param on redeem_invite_inner

Adds an opt-in path for Phase 2c (cross-WAN iroh redemption) where the
caller has an alternate transport (iroh-borne Zenoh CRDT sync) and the
Reticulum unicast fast-path is not required.

When the new param is true and resolve_destinations_for_owner returns
empty, redeem_invite_inner skips the destinations.is_empty fast-fail
and proceeds straight to the oneshot await. The PendingJoin event is
already on the wire via the engine's state-root publisher; admins
counter-sign when they next come online (which, for iroh-borne sync,
is typically immediate once the iroh hole-punch completes).

Default false to preserve existing redeem_invite IPC semantics. All
existing callers updated.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 2: Add `seed_from_pkarr` method to `ReachabilityResolver`

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (new method + provenance enum extension if applicable)

- [ ] **Step 2.1: Write the failing test**

Add to existing test module in `src-tauri/src/reachability_resolver.rs`:

```rust
#[tokio::test]
async fn seed_from_pkarr_makes_record_resolvable_by_node_id() {
    let resolver = ReachabilityResolver::new();
    let payload = test_fixture_reachability_announce(/* random NodeId, etc. */);
    let owner_addr = test_fixture_owner_addr();
    let device_hash = test_fixture_device_hash();

    // Before seeding: resolver has no record for this addr.
    assert!(resolver.resolve(&owner_addr).is_none());

    // Seed from pkarr (Phase 2c entry point).
    resolver.seed_from_pkarr(owner_addr, device_hash, payload.clone()).await;

    // After seeding: resolver has the record and resolve_by_node_id
    // (used by IrohZenohLinkManager.new_link) finds it.
    let resolved = resolver.resolve(&owner_addr);
    assert_eq!(resolved.as_ref().map(|r| r.iroh_node_id), Some(payload.iroh_node_id));
    let by_node = resolver.resolve_by_node_id(&payload.iroh_node_id);
    assert!(by_node.is_some());
}
```

Run the test. Expected: FAIL (`seed_from_pkarr` does not exist).

- [ ] **Step 2.2: Implement `seed_from_pkarr`**

Add to `impl ReachabilityResolver` in `src-tauri/src/reachability_resolver.rs`:

```rust
/// Phase 2c: seed the in-memory map with a pkarr-resolved routing record
/// so IrohZenohLinkManager.new_link() (which uses the synchronous
/// resolve_by_node_id path) can find the inviter's iroh routing.
///
/// Distinct from the Phase 2b async fallback hook: that hook fires when
/// resolve() misses on lookup and is suitable for ongoing routing
/// resolution; seed_from_pkarr is a one-shot write used by
/// connectivity_redeem_invite_iroh right after a pkarr record has been
/// verified, so the subsequent redeem_invite_inner call's CRDT-sync
/// PendingJoin publish has a route through IrohZenohLinkManager.
///
/// Uses provenance `PkarrSeeded` so Phase 1's LWW projection logic can
/// distinguish CRDT-sourced records (highest priority) from pkarr-fallback
/// records (lower priority) from pkarr-seeded records (transient, expires
/// after the redemption completes — but for Phase 2c first cut we leave
/// these in the map; Phase 3 may add explicit eviction).
pub async fn seed_from_pkarr(
    &self,
    owner_addr: OwnerAddr,
    device_hash: DeviceIdentityHash,
    payload: ReachabilityAnnouncePayload,
) {
    let mut inner = self.inner.write().await;
    let key = (owner_addr, device_hash);
    inner.insert(key, ResolverEntry {
        payload,
        provenance: ResolverProvenance::PkarrSeeded,
        // ... other fields ...
    });
}
```

If `ResolverProvenance` doesn't already have a `PkarrSeeded` variant, add it (alongside whatever Phase 1 + 2b variants exist, e.g., `CrdtAnnounced`, `PkarrFallback`). Search for the enum definition first and follow the existing pattern.

- [ ] **Step 2.3: Run the test, verify it passes**

Run: `cargo nextest run --locked --features test-fixtures -E 'test(=seed_from_pkarr_makes_record_resolvable_by_node_id)'`
Expected: PASS.

- [ ] **Step 2.4: Commit**

```bash
git add src-tauri/src/reachability_resolver.rs
git commit -m "feat(zeb-325): ReachabilityResolver.seed_from_pkarr for Phase 2c

Adds a one-shot write API that connectivity_redeem_invite_iroh uses to
seed the in-memory routing map with a pkarr-resolved record. Once
seeded, Phase 1's IrohZenohLinkManager.new_link can open an iroh
connection to the inviter on demand (it uses the synchronous
resolve_by_node_id path), which makes the subsequent
redeem_invite_inner call's CRDT-sync PendingJoin publish routable.

Distinct from Phase 2b's async fallback hook: that hook is for ongoing
resolve() misses; this is a deterministic pre-seed before invoking the
redemption flow.

Provenance variant ResolverProvenance::PkarrSeeded so Phase 1's LWW
projection can distinguish CRDT-sourced records (highest priority)
from pkarr-fallback (lower) from pkarr-seeded (transient).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 3: Wire `connectivity_redeem_invite_iroh` to seed + redeem

**Files:**
- Modify: `src-tauri/src/lib.rs:28668-28777` (the `connectivity_redeem_invite_iroh` IPC body)

- [ ] **Step 3.1: Write the failing test**

Add an integration test in `src-tauri/tests/pkarr_invite_redemption_integration.rs` (extending the existing case-A test). New test name: `connectivity_redeem_invite_iroh_completes_join_via_crdt_sync`.

Test shape (per existing case-A pattern, but extended):
- Spawn Alice's state with an iroh endpoint + Zenoh session + community engine where she's the admin
- Publish Alice's pkarr record (case A, keyed by an invite token sig)
- Spawn Bob's state with an iroh endpoint + Zenoh session (no community engine yet)
- Bob's `connectivity_redeem_invite_iroh` is invoked with the invite URL
- Verify: Bob ends up with community state where he's a member (post-counter-sig)
- Verify: the returned outcome.status is `"joined"` (NOT `"pkarr_resolved_no_handshake"`)

Run the test. Expected: FAIL (currently returns `"pkarr_resolved_no_handshake"`).

- [ ] **Step 3.2: Implement the wiring**

Edit `src-tauri/src/lib.rs:28755-28777`. After step 7 (decode routing_blob), replace the stub:

```rust
// 8. Seed ReachabilityResolver so IrohZenohLinkManager.new_link can
//    open an iroh connection to the inviter on demand.
let reachability_resolver = {
    let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
    g.reachability_resolver.clone()
};
let Some(reachability_resolver) = reachability_resolver else {
    return Ok(RedemptionOutcome {
        status: "inviter_unreachable".to_string(),
        community_id: None,
    });
};
let routing: crate::reachability_record::ReachabilityAnnouncePayload =
    ciborium::from_reader(rec.routing_blob.as_slice())
        .map_err(|e| format!("decode routing_blob: {e}"))?;

// Derive the inviter's device hash from the iroh NodeId (or wherever
// it's available). The inviter's owner-addr is in payload.admin_addr.
let inviter_addr = payload.admin_addr;
let inviter_device_hash: DeviceIdentityHash = /* derive from routing.iroh_node_id or pkarr signer */;

reachability_resolver
    .seed_from_pkarr(inviter_addr, inviter_device_hash, routing.clone())
    .await;

// 9. Call redeem_invite_inner with allow_no_reticulum_destinations=true.
//    The CRDT-sync path will carry the PendingJoin to Alice via the
//    iroh-borne Zenoh link IrohZenohLinkManager.new_link opens.
let (crdt_state, hlc_tracker, device_id, self_owner, signing_key,
     community_registry, community_adapter_tx, unicast_send_tx,
     dm_outbox, channel_log_registry, identity_dir) = {
    let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
    (g.crdt_state.clone(),
     g.hlc_tracker.clone(),
     g.device_id.clone(),
     g.self_owner.expect("self_owner set"),
     g.signing_key.clone(),
     g.community_registry.clone(),
     g.community_adapter_tx.clone(),
     g.unicast_send_tx.clone(),
     g.dm_outbox.clone(),
     g.channel_log_registry.clone(),
     g.identity_dir.clone())
};

let result = redeem_invite_inner(
    invite_url,
    crdt_state,
    hlc_tracker,
    device_id,
    self_owner,
    signing_key,
    community_registry,
    community_adapter_tx,
    unicast_send_tx,
    dm_outbox,
    channel_log_registry,
    || Ok(()),  // no fence check for iroh redeem
    identity_dir,
    true,  // allow_no_reticulum_destinations — Phase 2c
).await;

match result {
    Ok(dto) => Ok(RedemptionOutcome {
        status: "joined".to_string(),
        community_id: Some(hex::encode(payload.community_id.0)),
        // ... whatever other fields RedemptionOutcome has ...
    }),
    Err(e) => {
        tracing::warn!(error = %e, "Phase 2c redeem_invite_inner failed");
        Ok(RedemptionOutcome {
            status: "inviter_unreachable".to_string(),
            community_id: None,
        })
    }
}
```

(Adapt the exact NodeState field names + RedemptionOutcome shape to match the actual code by reading lib.rs around the existing redeem_invite IPC at line 15420.)

- [ ] **Step 3.3: Run the test, verify it passes**

```bash
cargo nextest run --locked --features test-fixtures -E 'test(=connectivity_redeem_invite_iroh_completes_join_via_crdt_sync)'
```
Expected: PASS.

- [ ] **Step 3.4: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/pkarr_invite_redemption_integration.rs
git commit -m "feat(zeb-325): wire connectivity_redeem_invite_iroh to redeem_invite_inner

After pkarr resolves the inviter's routing record:
1. Seed ReachabilityResolver via seed_from_pkarr so IrohZenohLinkManager
   can open the iroh connection on demand.
2. Call redeem_invite_inner with allow_no_reticulum_destinations=true.
3. Return status='joined' on success (replacing 'pkarr_resolved_no_handshake').

The CRDT-sync path carries the PendingJoin to the inviter via the
iroh-borne Zenoh link; the existing engine post-Inserted hook
counter-signs and the state-root publish carries the response back to
the joiner via the same link.

Integration test extends pkarr_invite_redemption_integration.rs with
the full end-to-end flow: two engines + iroh + pkarr → join completes.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 4: Wire `connectivity-invite-resolution-progress` events

**Files:**
- Modify: `src-tauri/src/lib.rs` (emit progress events at each stage of `connectivity_redeem_invite_iroh`)
- Modify: `src-tauri/src/connectivity_events.rs` (if a dedicated module exists for connectivity events) — otherwise inline in lib.rs

- [ ] **Step 4.1: Write the failing test**

```rust
#[tokio::test]
async fn connectivity_redeem_invite_iroh_emits_progress_events() {
    // Set up state with a mock event emitter.
    // Trigger connectivity_redeem_invite_iroh.
    // Verify the emitter received: resolving → connecting → sending → awaiting_countersig → joined.
}
```

Run. Expected: FAIL (events not yet emitted at iroh path stages).

- [ ] **Step 4.2: Add progress emissions at the four iroh stages**

In `connectivity_redeem_invite_iroh`:
1. Before pkarr resolve: emit `{stage: "resolving"}`
2. After pkarr resolve + before seed_from_pkarr: emit `{stage: "connecting"}` (the iroh hole-punch may take 5-30s; this stage covers it)
3. Before redeem_invite_inner call: emit `{stage: "sending"}`
4. After redeem_invite_inner returns (PendingJoin inserted, awaiting counter-sig): emit `{stage: "awaiting_countersig"}`. Note: redeem_invite_inner currently waits internally for the oneshot, so "awaiting_countersig" might need to be emitted from inside redeem_invite_inner or via a callback. Simplest shape: emit before calling redeem_invite_inner with timeout > 0; the function blocks until completion or timeout.
5. On Ok result: emit `{stage: "joined"}`.

The frontend `onResolutionProgress` listener (already wired in RedeemInviteDialog.svelte) consumes these events.

- [ ] **Step 4.3: Run the test, verify it passes**

```bash
cargo nextest run --locked --features test-fixtures -E 'test(=connectivity_redeem_invite_iroh_emits_progress_events)'
```
Expected: PASS.

- [ ] **Step 4.4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-325): emit staged progress events from connectivity_redeem_invite_iroh

Four stage events: resolving → connecting → sending → awaiting_countersig
→ joined. Consumed by RedeemInviteDialog.svelte's onResolutionProgress
listener (already wired in PR #158). User now sees concrete progress
during the multi-second iroh hole-punch + CRDT sync handshake instead
of a single 'Looking up...' indicator.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 5: Update `RedeemInviteDialog.svelte` for the joined path

**Files:**
- Modify: `src/lib/components/RedeemInviteDialog.svelte`

- [ ] **Step 5.1: Write the failing test**

Add to `src/lib/components/__tests__/RedeemInviteDialog.test.ts`:

```ts
it('shows "Joined ✓" when iroh redeem returns status="joined"', async () => {
  vi.mocked(redeemInviteIroh).mockResolvedValue({
    status: 'joined',
    community_id: 'abc123',
  });
  // ... render component, trigger redeem, verify "Joined ✓" or whatever the
  // existing Reticulum success UI shows.
});
```

Run via `npx vitest run`. Expected: FAIL (current code shows "Found on network, handshake pending" for any success-shaped response).

- [ ] **Step 5.2: Implement the UI branch**

Edit `src/lib/components/RedeemInviteDialog.svelte:65-83` to add a `joined` branch:

```ts
if (outcome.status === 'joined') {
  irohStage = 'joined';
  // Existing post-redeem flow: close dialog, refresh community list, toast, etc.
  // Match whatever the existing Reticulum redeem success path does.
  return;
}
if (outcome.status === 'pkarr_resolved_no_handshake') {
  // Phase 2c is supposed to eliminate this status entirely, but keep the
  // fallback UI in case the iroh path falls back to a partial success.
  // ...
}
// (other branches as today)
```

- [ ] **Step 5.3: Run the test, verify it passes**

Run: `npx vitest run src/lib/components/__tests__/RedeemInviteDialog.test.ts`
Expected: PASS.

- [ ] **Step 5.4: Commit**

```bash
git add src/lib/components/RedeemInviteDialog.svelte src/lib/components/__tests__/RedeemInviteDialog.test.ts
git commit -m "feat(zeb-325): RedeemInviteDialog handles 'joined' from iroh redeem

Phase 2c's connectivity_redeem_invite_iroh now returns status='joined'
on full handshake success; surface that to the user with the same UI
the Reticulum redeem path uses. The 'pkarr_resolved_no_handshake' path
stays as a defensive fallback in case the iroh hole-punch fails after
pkarr resolves (Phase 3 will refine this; for now we fall through to
LAN fallback).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 6: Two-process integration test (no Reticulum destinations)

**Files:**
- Create: `src-tauri/tests/pkarr_iroh_redeem_full_integration.rs`

This is the highest-value test. It validates that the entire chain — pkarr publish → pkarr resolve → seed → iroh connect-on-demand → Zenoh CRDT sync → PendingJoin → counter-sign → state-root → membership state — works end-to-end with NO Reticulum destinations available.

- [ ] **Step 6.1: Write the test**

Follow Phase 1's two-process integration test pattern (search for the existing test under `src-tauri/tests/` exercising two distinct IrohEndpoints over DERP). Add a new integration test file:

```rust
//! Phase 2c full integration: Bob (no prior contact with Alice) joins
//! Alice's community using ONLY the pkarr+iroh+Zenoh path, with zero
//! Reticulum destinations available.

#[tokio::test]
async fn bob_joins_alice_community_via_pure_iroh_path() {
    // 1. Spawn Alice's NodeState with iroh endpoint, Zenoh session,
    //    community engine where she's a member with admin power.
    //    Activate Alice's pkarr identity publisher (case B) OR have
    //    Alice generate an invite + publish case-A record.
    // 2. Spawn Bob's NodeState with iroh endpoint, Zenoh session,
    //    EMPTY community list, EMPTY Reticulum destinations.
    // 3. Wait for Alice's pkarr publication to land on a mock relay.
    // 4. Bob invokes connectivity_redeem_invite_iroh with the invite URL.
    // 5. Assert: within 30s, Bob's NodeState has Alice's community in
    //    its space list with himself as a member.
    // 6. Assert: the returned RedemptionOutcome.status == "joined".
}
```

Test infrastructure needs:
- Mock pkarr relay (use `MockPkarrRelay` from harmony-pkarr test-fixtures)
- Two iroh endpoints (one per "process" — both in the same tokio runtime, distinct keys, both registered with their own IrohZenohLinkManager)
- Two Zenoh sessions configured to use the iroh transport
- DERP relay: real n0 hosted relay (existing Phase 1 integration test uses this)

- [ ] **Step 6.2: Run the test**

```bash
cargo nextest run --locked --features test-fixtures -E 'test(=bob_joins_alice_community_via_pure_iroh_path)'
```

Expected: PASS within 30s timeout. If it fails, the failure is the most informative thing in the whole plan — it'll point at which integration step actually breaks.

If the test reveals additional missing infrastructure (e.g., Zenoh peer-discovery doesn't fire on iroh-only setup, or seed_from_pkarr's provenance interacts badly with Phase 1's LWW projection), file the gap as a discovered-during-implementation note, fix it inline (this is the load-bearing test), and re-run.

- [ ] **Step 6.3: Commit**

```bash
git add src-tauri/tests/pkarr_iroh_redeem_full_integration.rs
git commit -m "test(zeb-325): two-process integration — pure-iroh community join

Bob (no prior contact with Alice, zero Reticulum destinations) joins
Alice's community via the full pkarr+iroh+Zenoh-CRDT-sync handshake.
End-to-end coverage of the Phase 2c path:

- Pkarr publish + resolve via mock relay
- ReachabilityResolver seed
- IrohZenohLinkManager opens iroh connection on demand
- Bob's spawned engine inserts PendingJoin locally
- State-root publish reaches Alice via the iroh-borne Zenoh link
- Alice's engine counter-signs; state-root publish carries back
- Bob's oneshot fires; redeem_invite_inner returns Ok
- Bob's NodeState shows him as a member of Alice's community

This is the load-bearing validation that Phase 2c actually works
end-to-end. If Phase 4's cross-WAN canary (ZEB-172) discovers a real
NAT topology that breaks, the failure is in transport, not in the
orchestration this test pins.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 7: Final 5-gate sweep + PR

- [ ] **Step 7.1: Run all five backend gates**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all clean (the 3 pre-existing folder_ingest::tests failures from Task 0 baseline are not blocking).

- [ ] **Step 7.2: Run the two frontend gates**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: 0 errors, all tests pass.

- [ ] **Step 7.3: Push the branch**

```bash
git push -u origin zeb-321-phase2c-invite-handshake
```

- [ ] **Step 7.4: Create the PR**

```bash
gh pr create --title "feat(zeb-325): Phase 2c invite handshake — complete iroh-redeem join" \
  --body "$(cat <<'EOF'
## Summary

Closes the Phase 2c stub from PR #158. `connectivity_redeem_invite_iroh` now completes the cross-WAN invite handshake end-to-end via the pure-CRDT-sync path:

1. Pkarr-resolve the inviter's routing record (Phase 2b — already shipped)
2. Seed ReachabilityResolver with the resolved record so Phase 1's IrohZenohLinkManager can open the iroh connection on demand
3. Call `redeem_invite_inner` with new `allow_no_reticulum_destinations: bool = true` parameter — skips the destinations-empty fast-fail
4. Bob's spawned community engine inserts PendingJoin locally → state-root publishes via Zenoh-over-iroh → reaches Alice → her engine counter-signs → state-root publishes back → Bob's oneshot fires → join complete

## Phasing context

Part of [ZEB-321](https://linear.app/zeblith/issue/ZEB-321) — closes [ZEB-325](https://linear.app/zeblith/issue/ZEB-325). Phase 2c of the cross-WAN connectivity epic.

| Phase | Status |
|---|---|
| Phase 1 (Iroh foundation) | Merged (PR #157) |
| Phase 2a (harmony-pkarr crate) | Merged (harmony PR #270) |
| Phase 2b (pkarr policies + IPCs) | Merged (PR #158) |
| **Phase 2c (invite handshake)** | **This PR** |
| Phase 4 (cross-WAN canary, [ZEB-172](https://linear.app/zeblith/issue/ZEB-172)) | Next |
| Phase 3 (rebinding + mobile) | After Phase 4 |
| Phase 5+ (relay governance) | Later |

## Design reference

Spec §7.2: \`docs/specs/2026-05-23-zeb-321-phase2-discovery-bootstrap-design.md\` (commit \`cb5cca5\`).

Per spec the architecture is option C (pure Zenoh-CRDT-sync, no new wire protocol). Verified pre-conditions:

- Bob's spawned engine subscribes to community Zenoh keyspace unconditionally (no membership gate)
- IrohZenohLinkManager.new_link opens iroh connections on demand from ReachabilityResolver
- verify_event rule P6 explicitly permits PendingJoin from non-members
- The existing wait-for-counter-sig oneshot fires regardless of transport

## Out of scope (separate tickets)

- Case-B identity-key publicness (CodeRabbit P1 deferred from PR #270) — separate design ticket
- Mobile push, liveness, rebinding — Phase 3
- Reticulum deprecation — Phase 3+ after empirical proof iroh path is solid

## Test plan

- [ ] cargo fmt --all -- --check
- [ ] cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
- [ ] cargo nextest run --locked --workspace --all-targets --features test-fixtures
- [ ] npx tsc --noEmit
- [ ] npx vitest run
- [ ] New integration test \`bob_joins_alice_community_via_pure_iroh_path\` validates full chain
- [ ] Manual smoke: redeem an invite across two machines on real WAN (Phase 4 canary will formalize)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Save the PR number, return it as the deliverable.

---

## Notes for implementer subagents

**HARD RULES (from user memory `feedback_implementer_gate_time_budget` + `feedback_cargo_fmt_gate` + `feedback_pipe_exit_codes_lie` + `feedback_long_running_background_supervision`):**

- Always include `cargo fmt --all -- --check` alongside clippy (not just clippy).
- When piping cargo output through tail/grep, use `set -o pipefail` or `${PIPESTATUS[0]}` — pipe exit codes lie.
- Every long-running cargo command (>10 min projected) must be wrapped in `timeout 600` or run with a ScheduleWakeup heartbeat safety net.
- Commit BEFORE running gates. If a gate fails, the diff is already preserved; fix forward.
- If any task hits the 10-min wall-clock kill switch repeatedly (3+ times), report BLOCKED and the controller will split it before reattempting.
- `feedback_tauri_error_extraction`: frontend rejections come back as strings in production but Error objects in tests. Always extract with `e instanceof Error ? e.message : String(e)`.
- `feedback_two_ipc_toctou`: `connectivity_redeem_invite_iroh` is single-IPC (no preview/commit pair), so no TOCTOU concern here.
- `feedback_test_drift_is_our_fault`: orphan failures captured in Task 0 baseline are not blocking; NEW failures introduced by this PR are.
- `feedback_second_order_correctness_review`: adding the `allow_no_reticulum_destinations` parameter changes a precondition; enumerate every reader of `destinations.is_empty()` and `any_sent` in `redeem_invite_inner` to ensure no logic depends on the early-return path beyond what we've fixed.
- `feedback_linear_pr_auto_close`: PR body uses markdown-linked refs `[ZEB-NNN](url)`. ZEB-325 is the sub-ticket; closing it is appropriate. ZEB-321 stays as a link (do NOT bare-reference; multi-phase work).
- `feedback_no_worktrees`: branch already created in main repo via `git checkout -b`; no worktree.
- `feedback_pull_before_work`: branch is off `3c4c21d` (latest origin/main as of plan-write time). Re-verify before Task 0.

**If iroh hole-punching turns out to take >5s (the default redeem_invite_inner timeout):** extend the timeout via `HARMONY_REDEEM_INVITE_TIMEOUT_MS` env var for the iroh path, or pass an explicit longer timeout parameter through. Don't change the default — only the iroh path needs more time.
