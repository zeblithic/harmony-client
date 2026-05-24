# ZEB-321 Phase 2c — Invite Handshake Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

## Pivot note — 2026-05-24 (option C → option A)

**TL;DR.** Option C (pure Zenoh-CRDT-sync over the iroh-borne link) is blocked on two production gaps the original plan did not anticipate: (1) `event_loop.rs` Phase 1 still discards inbound iroh→zenoh links into a drain task (the real Zenoh-session ingestion is deferred), and (2) `community_state_sync::handle_incoming_publish` drops non-member publishes at the `PublisherNotJoined` gate before any per-event `verify_event` runs, so Bob's PendingJoin publish never reaches Alice's engine even when the link IS up. The reverted Task 6 test (commit `92997b9`, kept in git for archival via the revert in `1c65a87`) documented these gaps in `KNOWN-BLOCKED-PHASE-2C-PUBLISH-GATE`.

**Pivoted to option A: direct iroh bi-stream handshake on the unused `harmony/handshake/v1` ALPN** (already registered in `iroh_endpoint.rs:49,88,252` since Phase 1 Task 4; never wired). Bob opens a `harmony/handshake/v1` bi-stream to Alice's iroh NodeId, writes the existing `CommunityInviteSigned` packet (Reticulum unicast wire shape, length-prefixed), reads back a CBOR-encoded `JoinCountersign` event, and feeds it into a new `redeem_invite_inner_with_overrides` variant via `pre_minted` + `pre_delivered_countersign` so the existing oneshot await fires immediately on the engine's post-Inserted hook. Alice's side runs a new `IrohInviteHandshakeAcceptor` installed onto the existing `IrohZenohLinkManager` accept loop via a `OnceCell` slot populated late at boot once `community_registry` / `dm_outbox` / `crdt_state` are all available.

**Bugs surfaced + fixed during the option A test wire-up** (see `tests/pkarr_iroh_redeem_full_integration.rs`):
- `joiner_identity_pub` derivation mismatch between iroh inner (HKDF X25519) and `mint_redemption` (birational map). Fixed by deriving the envelope's `joiner_identity_pub` the SAME way mint does, plus computing `signing_device_hash` from the resulting composite. The same divergence may exist on the legacy Reticulum unicast invite-only path; not fixed here (out of scope for option A pivot), but worth a follow-up ticket if invite-only Reticulum redemption is observed failing with `JoinSigInvalid` in production.
- Connection close race: dropping the acceptor's `Connection` after `send.finish()` raced QUIC delivery, leaving Bob with "connection lost" on the response read. Fixed by `conn.closed().await` on the acceptor + explicit `conn.close()` + `conn.closed().await` on Bob's side. Mirrors `zenoh_iroh_link`'s `paired_stream_roundtrip_via_loopback` lesson.

**Tasks 1-3 below are kept** (the `allow_no_reticulum_destinations` param, `seed_from_pkarr`, and the `connectivity_redeem_invite_iroh_inner` extraction all remain useful — seed_from_pkarr in particular keeps the door open for option C once both production gaps are addressed). **Task 4 (progress events) is kept** structurally but the Sending/AwaitingCountersig stages now bracket the iroh write/read rather than the CRDT publish/await. **Task 5 (frontend) is kept**. **Task 6 (two-process test) was reverted and rewritten** against the option A flow.

The two single-engine tests
(`connectivity_redeem_invite_iroh_completes_join_via_crdt_sync`,
`connectivity_redeem_invite_iroh_emits_progress_events`) in
`pkarr_invite_redemption_integration.rs` were originally `#[ignore]`'d
because they relied on a single-engine masking quirk (Bob's own
PendingJoin insert fires the redemption oneshot regardless of whether
a counter-sign ever arrives) that the option A pivot makes
structurally impossible. PR #159 round-1 review (CodeRabbit NITPICK
F9) noted that the ignored bodies still asserted post-handshake
`"joined"` with `iroh_endpoint: None`, which can never succeed under
option A; both tests have been deleted. Two-process coverage in
`pkarr_iroh_redeem_full_integration.rs` supersedes them.

---

**Goal:** Complete the cross-WAN invite-redemption handshake so `connectivity_redeem_invite_iroh` actually completes a join instead of returning `pkarr_resolved_no_handshake`.

**Architecture:** Per spec §7.2 + verified pre-conditions in ZEB-325. ~~Option C: pure Zenoh-CRDT-sync.~~ See the pivot note above — implementation is now option A (direct iroh bi-stream handshake on `harmony/handshake/v1`). Bob seeds the ReachabilityResolver with the pkarr-resolved routing record so Phase 1's IrohZenohLinkManager can open the iroh connection on demand; ~~Bob then calls the existing `redeem_invite_inner` with a new `allow_no_reticulum_destinations: bool` parameter that skips the Reticulum-destinations fast-fail. Bob's local PendingJoin insert + state-root publish reach Alice via the iroh-borne Zenoh link; her engine counter-signs and her state-root publish carries the counter-signed event back to Bob via the same link, firing his existing wait-for-counter-sig oneshot.~~ Bob opens an iroh bi-stream on the handshake ALPN, sends the CommunityInviteSigned packet, receives the JoinCountersign in the response, and feeds it into `redeem_invite_inner_with_overrides` to complete the join.

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

## Task 3: Wire `connectivity_redeem_invite_iroh` to the iroh handshake

**Files:**

- Modify: `src-tauri/src/lib.rs` — the `connectivity_redeem_invite_iroh` IPC + its `_inner` helper

> **Option-A pivot note (see top of file for context).** The original plan described an option-C wiring that called `redeem_invite_inner` and relied on Bob's `PendingJoin` reaching Alice via Zenoh CRDT sync over the iroh-borne link. That path was blocked on two production gaps (event-loop iroh→zenoh ingestion deferral + `PublisherNotJoined` gate dropping non-member publishes). The replacement option-A flow shipped in commits `ccf7a55` (the handshake protocol + acceptor) and `8bbafd6` (the two-process integration test). The Step-3.1/3.2 sketches that previously lived here have been removed; see those two commits for the actual implementation. The high-level shape is:

- [ ] **Step 3.1 (option A): Add a `harmony/handshake/v1` ALPN bi-stream protocol**

The dialer (Bob) opens an iroh bi-stream on the existing-but-unused `harmony/handshake/v1` ALPN, writes a length-prefixed `CommunityInviteSigned` packet, reads back the CBOR-encoded `JoinCountersign`, and feeds both into `redeem_invite_inner_with_overrides` via `pre_minted` + `pre_delivered_countersign`. The acceptor (Alice) decodes the packet, delegates to `community_invite::handle_unicast` (which fires the existing auto-counter-sign post-Inserted hook), polls her engine state for the JoinCountersign filtered on `actor == self_owner`, and writes the response.

- [ ] **Step 3.2 (option A): Install the acceptor onto the existing `IrohZenohLinkManager` accept loop**

`IrohInviteHandshakeAcceptor` implements `IrohHandshakeDispatcher`; production wiring at lib.rs installs it via `link_mgr.install_handshake_dispatcher(..)` once `community_registry` / `dm_outbox` / `crdt_state` are all available at boot.

See commits `ccf7a55` (acceptor + dialer + ALPN dispatch) and `8bbafd6` (two-process integration test) for the actual code. The legacy Step-3.1 single-engine test was deleted in PR #159 (CodeRabbit NITPICK F9).

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

## Task 6: Two-process integration test (option A)

**Files:**

- Create: `src-tauri/tests/pkarr_iroh_redeem_full_integration.rs`

This is the highest-value test. It validates the option-A wire handshake end-to-end: two distinct iroh endpoints on loopback, Alice runs the production `IrohInviteHandshakeAcceptor` on her link manager's accept loop, Bob's IPC dials Alice's handshake ALPN, and both sides converge on the counter-signed `Join`.

> **Option-A pivot note (see top of file for context).** The original Task 6 sketch was for option C (pure CRDT sync; no acceptor required). That was reverted in commits `92997b9` / `1c65a87` once the production gaps surfaced. The replacement option-A test shipped in commit `8bbafd6` against the protocol introduced in `ccf7a55`. See those commits for the actual code; the high-level shape is below.

- [ ] **Step 6.1: Write the test (option A)**

The test wires up:

- Two hermetic iroh endpoints on loopback (no DERP, no IP transports cleared apart from loopback).
- Alice's `IrohZenohLinkManager` accept loop with the production `IrohInviteHandshakeAcceptor` installed via `install_handshake_dispatcher` (passes an explicit `HandshakeAcceptorConfig` so IO timeouts are short).
- Alice's `CommunitySyncRegistry` containing her admin-bootstrapped community.
- Bob's `CommunitySyncRegistry` (empty), a real `MockPkarrRelay` round-trip so the IPC exercises pkarr + iroh end-to-end without test-specific short-circuits, and `connectivity_redeem_invite_iroh_inner` driven with explicit `HandshakeDialConfig` (no env mutation — see PR #159 F10).

Load-bearing assertions: `outcome.status == "joined"`, Bob's CRDT contains the admin bootstrap + PendingJoin + JoinCountersign, Bob materializes as `Joined`, and Alice's CRDT shows Bob's PendingJoin + her own auto-counter-sign.

- [ ] **Step 6.2: Run the test**

Run:

```bash
cargo nextest run --locked --features test-fixtures --test pkarr_iroh_redeem_full_integration
```

Expected: PASS within ~20s on loopback. The outer `tokio::time::timeout(60s, ...)` guard catches any unbounded await.

- [ ] **Step 6.3: Commit**

The actual commit shipped as `8bbafd6` ("test(zeb-325): option A two-process integration test"). See the existing commit message for the rationale.

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

Run:

```bash
gh pr create --title "feat(zeb-325): Phase 2c invite handshake — complete iroh-redeem join" \
  --body "$(cat <<'EOF'
## Summary

Closes the Phase 2c stub from PR #158. `connectivity_redeem_invite_iroh` now completes the cross-WAN invite handshake end-to-end via a direct iroh bi-stream on the `harmony/handshake/v1` ALPN (option A — see pivot note in the plan doc):

1. Pkarr-resolve the inviter's routing record (Phase 2b — already shipped).
2. Seed ReachabilityResolver with the resolved record for future Phase 1 CRDT-sync paths (harmless on option A's direct-handshake flow).
3. Open an iroh bi-stream to the inviter's iroh NodeId on `harmony/handshake/v1`; write a length-prefixed `CommunityInviteSigned` packet.
4. Inviter's `IrohInviteHandshakeAcceptor` decodes the packet, runs `community_invite::handle_unicast` against her engine (which triggers the existing auto-counter-sign post-Inserted hook), polls for the JoinCountersign filtered on `actor == self_owner`, CBOR-encodes the response.
5. Joiner reads the response, calls `redeem_invite_inner_with_overrides` with `pre_minted` + `pre_delivered_countersign` so the inner's oneshot resolves immediately on the engine's post-Inserted hook. Return `status="joined"` on success or `status="join_failed"` if the local commit failed after a valid countersign was delivered.

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

This PR ships **option A** (direct iroh bi-stream on `harmony/handshake/v1`) rather than the spec's option C (pure Zenoh-CRDT-sync). The option-C path remains blocked on two production gaps — \`event_loop.rs\` Phase 1 discards inbound iroh→zenoh links into a drain task, and \`community_state_sync::handle_incoming_publish\` drops non-member publishes at the \`PublisherNotJoined\` gate before per-event verify — both of which are out of scope for ZEB-325. See the pivot note at the top of the plan doc for the full rationale; the option-A protocol is documented in \`src-tauri/src/iroh_invite_acceptor.rs\` and the dialer in \`src-tauri/src/lib.rs::connectivity_redeem_invite_iroh_inner\`.

## Out of scope (separate tickets)

- Case-B identity-key publicness (CodeRabbit P1 deferred from PR #270) — separate design ticket
- Mobile push, liveness, rebinding — Phase 3
- Reticulum deprecation — Phase 3+ after empirical proof iroh path is solid
- Option-C revival (Zenoh CRDT sync over iroh-borne link) — requires unblocking the two production gaps noted above

## Test plan

- [ ] cargo fmt --all -- --check
- [ ] cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
- [ ] cargo nextest run --locked --workspace --all-targets --features test-fixtures
- [ ] npx tsc --noEmit
- [ ] npx vitest run
- [ ] New two-process integration test \`bob_joins_alice_via_iroh_handshake_option_a\` validates the full option-A wire path
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
