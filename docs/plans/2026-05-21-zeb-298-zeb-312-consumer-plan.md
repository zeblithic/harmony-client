# ZEB-298+ZEB-312 PR 2 (Consumer) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish production-activation of the voting engine. PR 1 wired transport + verify; PR 2 wires the **consumer** half — production `identity_resolver` so inbound Tier 1/2 actually applies, production `app_handle` so Tier 3 lifecycle events reach the UI, route 6 Tier 3 IPCs through `engine.publish_event` so engine-auto orchestration fires from real user actions, fire post-apply hooks on inbound so peer replicas auto-orchestrate identically to the originating node, add the ZEB-298 Tier 2 delegate-on-behalf surface (Tauri event + community policy + minimal toast UI), and rewrite Tier 3 IPC integration tests to use the IPC happy path. PR 2 closes both [ZEB-298](https://linear.app/zeblith/issue/ZEB-298) and [ZEB-312](https://linear.app/zeblith/issue/ZEB-312).

**Architecture:** Reuse the existing `OwnerDeviceCacheResolver` (`community_state_sync.rs:4630`) for voting identity by adding a `VotingIdentityResolver` impl on it — both traits have an identical `async fn resolve(&self, &OwnerAddr) -> Option<[u8; 64]>` shape. Thread `AppHandle<tauri::Wry>` via the DfrostLog precedent (typed handle, no type erasure — IPC sites use `app.handle().clone()` already in the engine-construction path). Tier 3 IPCs become a mechanical refactor `log.apply_with_snapshot(event, &space_id, snapshot)` → `engine.publish_event(event, snapshot)`. Post-apply hooks on inbound require dropping the log lock between apply and hook invocation; mirror the publish-side ordering. ZEB-298 ships a new `CommunityVotingPolicy` struct (one Tier 2-scoped opt-in field for now) + `voting_set_notify_on_delegate_signal` IPC + `ToastHost.svelte` mounted in `App.svelte` subscribing to the already-defined `voting-delegate-signaled-on-your-behalf` event.

**Tech Stack:** Rust (Tauri 2.x, tokio, ciborium, ed25519-dalek, zenoh-rs), TypeScript (Svelte 5, vitest). PR 2 ships the first frontend changes for the combined design.

---

## Spec reference

Design spec: `docs/specs/2026-05-20-zeb-298-zeb-312-engine-production-wiring-design.md` (sections 6–12 cover PR 2).

PR 2 branches off the merged PR 1 (origin/main `ed05df2`).

## File structure

| File | Responsibility | Change kind |
|---|---|---|
| `src-tauri/src/community_state_sync.rs` | Add `impl VotingIdentityResolver for OwnerDeviceCacheResolver` (delegates to `IdentityResolver::resolve` — bytes-identical contract) | Modify (add ~25 LOC) |
| `src-tauri/src/lib.rs` | Wire `identity_resolver: Some(Arc::new(OwnerDeviceCacheResolver::new(...)))` + `app_handle: Some(app.handle().clone())` in `ensure_voting_engine_for`; route 6 Tier 3 IPCs through `engine.publish_event`; add new `voting_set_notify_on_delegate_signal` IPC; register in both `invoke_handler` sites | Modify (~150 LOC net) |
| `src-tauri/src/community_voting_log_engine.rs` | Add `snapshot: Option<MembershipSnapshot>` param to `publish_event`; add `maybe_emit_delegate_on_behalf` hook + invocation from both `publish_event` and `process_inbound`; fire `maybe_trigger_engine_auto_orchestration` + `maybe_trigger_beacon_for_tier3_create` + `maybe_emit_tier3_lifecycle_events` from `process_inbound` (lock-released between apply and hooks) | Modify (~200 LOC net) |
| `src-tauri/src/community_voting_conviction.rs` | New `CommunityVotingPolicy` struct with `notify_on_delegate_signal: bool` field (`#[serde(default, rename = "nd")]`); accessor `get_policy()` on the per-community voting state | Modify (~60 LOC) |
| `src-tauri/src/community_voting_log.rs` | Store `policy: CommunityVotingPolicy` field on `VotingLog`; `get_policy()` / `set_policy()` methods; apply path for policy mutations (none yet — set via IPC, not via signed event in PR 2) | Modify (~40 LOC) |
| `src-tauri/tests/wire_format_community_voting_policy_fixtures.rs` | New file: pin CBOR encoding of `CommunityVotingPolicy { notify_on_delegate_signal: false }` (default) and `{ ..: true }` (set) — `#[serde(default)]` round-trip parity vs absent field | Create (~80 LOC) |
| `src-tauri/tests/community_voting_tier3_ipc_integration.rs` | Rewrite Tests 1–4 from Path C (direct engine calls) → Path A (Tauri `mock_builder()` + `get_ipc_response`) mirroring `dm_ipc_roundtrip.rs`. Test 5 unchanged. | Modify (~400 LOC net) |
| `src-tauri/tests/community_voting_tier2_delegate_on_behalf_integration.rs` | New file: two-engine integration test (alice delegates to bob; bob signals via engine B; engine A receives via real Zenoh; assert delegation-graph parity, conviction parity, emit fires when policy enabled, alice's later direct override drops bob's effective weight on that proposal only) | Create (~350 LOC) |
| `src/lib/components/Toast.svelte` | Minimal toast component — message string + auto-dismiss timer + dismiss button | Create (~80 LOC) |
| `src/lib/components/ToastHost.svelte` | Top-level toast container with stacked toasts via `$state<Toast[]>([])`; exposes `show(message: string, durationMs?: number)` via a Svelte store singleton in `src/lib/stores/toast.ts` | Create (~120 LOC) |
| `src/lib/stores/toast.ts` | Singleton toast store (`toastStore.show()`, `toastStore.dismiss(id)`) wrapping a `writable<Toast[]>` | Create (~50 LOC) |
| `src/App.svelte` | Mount `<ToastHost />` at top of layout; wire `votingAdapter.onVotingDelegateSignaledOnYourBehalf(payload => toastStore.show(...))` in `onMount` | Modify (~25 LOC) |
| `src/lib/types/voting.ts` | (Already has `VotingDelegateSignaledOnYourBehalfPayload` from PR #132 — no change) | (no change) |
| `src/lib/voting-adapter.ts` | (Already has `onVotingDelegateSignaledOnYourBehalf` subscriber — no change) | (no change) |
| `src/lib/__tests__/toast-store.test.ts` | Vitest test for toast store: show → toast appears, auto-dismiss after duration, manual dismiss removes the toast | Create (~80 LOC) |

---

## Task 0: Pre-flight green-baseline confirm

**Files:** none (read-only check)

- [ ] **Step 1: Confirm branch + working tree**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status
git rev-parse --abbrev-ref HEAD
git log --oneline -3
```

Expected: `On branch zeb-298-zeb-312-consumer`, working tree clean, HEAD = `ed05df2 ZEB-298+ZEB-312 PR 1 foundation: ... (#150)` (this branch was just created off origin/main right after the PR 1 squash merge).

- [ ] **Step 2: Confirm 5 gates baseline**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast 2>&1 | tail -10
cd ..
npx tsc --noEmit
npx vitest run 2>&1 | tail -5
```

Expected:
- fmt: silent (0 exit).
- clippy: `Finished ... 0 warnings`.
- nextest: `2105 passed, 28 pre-existing orphans (folder_ingest, mint, mint_sync, folder_ingest_walker_integration, rename_content_integration), 3 skipped` — these 28 are the cross-machine drift orphans documented in user memory, NOT introduced by this work.
- tsc: silent (0 exit).
- vitest: `Test Files 1921 passed`.

If any gate is unexpectedly red (other than the 28 known orphans), STOP and surface the regression before any Task 1 work. Test drift is our fault per `feedback_test_drift_is_our_fault`.

**Do NOT commit Task 0.** It is verification only.

---

## Task 1: Wire production `identity_resolver` via `OwnerDeviceCacheResolver`

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs:4665-4688` (add a second trait impl)
- Modify: `src-tauri/src/lib.rs:22244-22255, 22289` (replace `identity_resolver: None` with production wiring)

- [ ] **Step 1: Write the failing test (unit test in `community_state_sync.rs` tests module)**

```rust
#[cfg(test)]
#[tokio::test]
async fn owner_device_cache_resolver_impls_voting_identity_resolver() {
    use crate::community_voting_core::VotingIdentityResolver;
    // Construct the resolver with self_owner + self_identity_pub.
    let crdt = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::owner_state_crdt::OwnerState::default()
    ));
    let self_owner = crate::owner_state_types::OwnerAddr([7u8; 16]);
    let self_identity_pub = [42u8; 64];
    let r = OwnerDeviceCacheResolver::new(crdt, self_owner, self_identity_pub);

    // Self short-circuit must work for VotingIdentityResolver too.
    let resolved: Option<[u8; 64]> =
        <OwnerDeviceCacheResolver as VotingIdentityResolver>::resolve(&r, &self_owner).await;
    assert_eq!(resolved, Some(self_identity_pub));

    // Unknown owner returns None.
    let unknown = crate::owner_state_types::OwnerAddr([0xAB; 16]);
    let resolved: Option<[u8; 64]> =
        <OwnerDeviceCacheResolver as VotingIdentityResolver>::resolve(&r, &unknown).await;
    assert!(resolved.is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(owner_device_cache_resolver_impls_voting_identity_resolver)' 2>&1 | tail -5
```

Expected: FAIL with `the trait VotingIdentityResolver is not implemented for OwnerDeviceCacheResolver`.

- [ ] **Step 3: Add the impl**

```rust
// In src-tauri/src/community_state_sync.rs, after the existing
// `impl IdentityResolver for OwnerDeviceCacheResolver` block (~line 4688):

#[async_trait::async_trait]
impl crate::community_voting_core::VotingIdentityResolver for OwnerDeviceCacheResolver {
    /// Delegates to the channel-log `IdentityResolver::resolve`. Both
    /// trait signatures are identical (`OwnerAddr -> Option<[u8; 64]>`)
    /// and the 64-byte composite is `X25519_pub || Ed25519_pub` — exactly
    /// what `harmony_identity::Identity::from_public_bytes` consumes in
    /// `verify_voting_event`'s signature-check path.
    async fn resolve(&self, addr: &crate::owner_state_types::OwnerAddr) -> Option<[u8; 64]> {
        <Self as crate::community_state_sync::IdentityResolver>::resolve(self, addr).await
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(owner_device_cache_resolver_impls_voting_identity_resolver)' 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 5: Wire production resolver in `ensure_voting_engine_for`**

Replace the `identity_resolver: None` block at `lib.rs:22244-22255` with:

```rust
// ZEB-298+ZEB-312 PR 2: production identity resolver. OwnerDeviceCacheResolver
// already implements the parallel `IdentityResolver` trait; PR 2 added a
// second impl for `VotingIdentityResolver`. Both delegate to the same cache
// lookup (`dm_outbox::lookup_pubkey_for_device`), so voting and channel
// verify paths share their owner-address-to-64-byte-composite mapping.
let identity_resolver: Option<
    std::sync::Arc<dyn crate::community_voting_core::VotingIdentityResolver>,
> = Some(std::sync::Arc::new(
    crate::community_state_sync::OwnerDeviceCacheResolver::new(
        crdt_state.clone(),
        local_owner,
        self_identity_pub_64,
    ),
));
```

The new param `self_identity_pub_64: [u8; 64]` plumbs from `NodeState.dm_identity_pub_64` (same value the DfrostLog engine reads at its spawn site — grep for `dm_identity_pub_64` to confirm the field name on NodeState; if different, use whichever holds the X25519 || Ed25519 64-byte composite). Update `ensure_voting_engine_for`'s signature to accept it (param ordering: place between `local_owner` and `dfrost_log_registry`). Update the 3+ call sites in `lib.rs` that invoke `ensure_voting_engine_for`.

- [ ] **Step 6: Verify 5 gates green**

```bash
cd src-tauri && set -o pipefail
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast 2>&1 | tail -10
cd ..
npx tsc --noEmit
npx vitest run 2>&1 | tail -3
```

Expected: identical to Task 0 baseline (no regressions; +1 new test).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/community_state_sync.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-298+zeb-312): wire production identity_resolver via OwnerDeviceCacheResolver

ZEB-298+ZEB-312 PR 2 Task 1. Adds VotingIdentityResolver impl on
OwnerDeviceCacheResolver (delegates to the existing IdentityResolver
shape — both return Option<[u8; 64]> for the X25519 || Ed25519 composite).
ensure_voting_engine_for now plumbs Some(resolver) so inbound Tier 1
BallotCast and Tier 2 Signal events from peers actually verify against
the per-community membership snapshot + Ed25519 signature instead of
erroring out at the PR-1-deferred TODO.

Tier 3 inbound was already exempt (skips eligibility per the existing
inbound_eligibility_check case-split), so this is the load-bearing
change that lets peer-to-peer Tier 1/2 voting work end-to-end.
EOF
)"
```

---

## Task 2: Wire production `app_handle` per DfrostLog precedent

**Files:**
- Modify: `src-tauri/src/lib.rs:22275-22288, 22289` (replace `app_handle: None` with `Some(app.handle().clone())`)

- [ ] **Step 1: Verify the engine map's runtime type matches the IPC call site**

```bash
grep -n "VotingLogEnginesMap" src-tauri/src/lib.rs | head -5
grep -n "fn ensure_voting_engine_for" src-tauri/src/lib.rs
```

Expected: map keyed on `tauri::Wry` (confirmed at `lib.rs:22141-22148`). IPC callers all use `app: tauri::AppHandle<R>` where the live IPC handlers run as `R = tauri::Wry`. The `ensure_voting_engine_for` signature already takes `app: &tauri::AppHandle<tauri::Wry>` indirectly via the typed map — confirm by reading the existing signature.

- [ ] **Step 2: Add `app_handle: tauri::AppHandle<tauri::Wry>` as a new param**

```rust
pub async fn ensure_voting_engine_for(
    voting_log_engines: &VotingLogEnginesMap,
    voting_logs: &VotingLogMap,
    voting_log_adapter_request_tx: tokio::sync::mpsc::Sender<
        crate::event_loop::VotingLogAdapterRequest,
    >,
    community_id: crate::owner_state_types::SpaceId,
    hlc_tracker: std::sync::Arc<crate::owner_state_hlc::HlcTracker>,
    device_id: std::sync::Arc<String>,
    membership_resolver:
        std::sync::Arc<dyn crate::community_voting_log::MembershipSnapshotResolver>,
    local_signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    local_owner: crate::owner_state_types::OwnerAddr,
    self_identity_pub_64: [u8; 64],
    crdt_state:
        std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    app_handle: tauri::AppHandle<tauri::Wry>,                    // <-- NEW
    dfrost_log_registry:
        Option<std::sync::Arc<crate::community_dfrost_log_engine::DfrostLogRegistry<tauri::Wry>>>,
    beacon_requester: Option<crate::community_voting_log_engine::BeaconRequester>,
) -> Result<(), String>
```

Replace `app_handle: None` at line ~22288 with `app_handle: Some(app_handle.clone())`. At each call site in `lib.rs` (grep `ensure_voting_engine_for(`), pass `app.handle().clone()` (the IPC's `app: tauri::AppHandle<R>` param re-typed; since IPCs receive `R: tauri::Runtime` generically but only ever run as `R = Wry` in production, use `app.handle().clone()` which yields `AppHandle<Wry>` regardless of the generic; if the type system complains, use `tauri::Manager::app_handle(&app)` or downcast via the established DfrostLog pattern at `community_dfrost_log_engine.rs:753`).

- [ ] **Step 3: Run an existing Tier 3 lifecycle test to verify app_handle is actually plumbed**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voting_tier3) and (test(lifecycle) or test(emit))' 2>&1 | tail -20
```

Expected: existing Tier 3 lifecycle tests that exercise emit hooks should now succeed with `app_handle: Some(_)` instead of skipping. If a test was previously asserting `app_handle: None` semantics, update its assertion to match the new production reality.

- [ ] **Step 4: Verify 5 gates**

(Same gate block as Task 1 Step 6.)

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-298+zeb-312): wire production app_handle in ensure_voting_engine_for

ZEB-298+ZEB-312 PR 2 Task 2. Threads tauri::AppHandle<tauri::Wry> into
the per-community VotingLogEngine so Tier 3 lifecycle events
(sortition-complete / drafting-open / ratification-open / finalized)
emit to the UI. Mirrors the DfrostLogEngine precedent at
community_dfrost_log_engine.rs:753 — direct AppHandle<Wry> field, no
type erasure needed (VotingLogEnginesMap is already keyed on tauri::Wry).

This closes the second of the two PR 1 deferred TODOs at
ensure_voting_engine_for. After Task 2, the engine struct has all four
previously-dormant fields populated (hlc_tracker + device_id +
app_handle + local_signing).
EOF
)"
```

---

## Task 3: Add `snapshot` param to `publish_event`

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs:995` (`publish_event` signature)
- Modify: every internal caller of `publish_event` in the engine (likely 0–3 internal call sites; engine-auto orchestration may call it for re-mint)

- [ ] **Step 1: Write the failing test**

Add to the engine's `tests` module:

```rust
#[tokio::test]
async fn publish_event_with_snapshot_passes_through_to_apply() {
    // Construct an engine with a Tier 1 PollCreate event + a known snapshot.
    // Assert that publish_event(event, Some(snapshot)) applies under the
    // snapshot, not the at-HEAD one — concretely: pass a snapshot whose
    // electorate INCLUDES the actor but at-HEAD would EXCLUDE them.
    // Without the snapshot param, apply_with_snapshot would receive None
    // and the apply would fail with NotEligible. With the snapshot
    // threaded through, it succeeds.
    // ...
}
```

(Full test body: construct two-membership snapshot — alice in, bob out — at-HEAD; publish PollCreate by alice with explicit snapshot where bob is in; assert apply succeeds and bob is in the frozen electorate snapshot for the new poll.)

- [ ] **Step 2: Run test to verify it fails**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(publish_event_with_snapshot_passes_through_to_apply)' 2>&1 | tail -5
```

Expected: FAIL with `this function takes 1 argument but 2 arguments were supplied` (the snapshot param doesn't exist yet).

- [ ] **Step 3: Add the snapshot param**

```rust
// Old:
pub async fn publish_event(
    self: &std::sync::Arc<Self>,
    event: SignedVotingEvent,
) -> Result<(), String> { /* ... */ }

// New:
pub async fn publish_event(
    self: &std::sync::Arc<Self>,
    event: SignedVotingEvent,
    snapshot: Option<crate::community_voting_core::MembershipSnapshot>,
) -> Result<(), String> {
    // ... within the body, pass `snapshot` through to apply_with_snapshot
    // instead of None. The pre-existing call is roughly:
    //   log_g.apply_with_snapshot(event.clone(), &community_id, None)
    // becomes:
    //   log_g.apply_with_snapshot(event.clone(), &community_id, snapshot.clone())
    // ...
}
```

Update all internal callers (engine-auto orchestration mint paths — `maybe_trigger_engine_auto_orchestration`'s downstream mint calls — to pass `None` since those are kd=ss/sf/cl/rs events which don't carry a snapshot). Existing kd=cr (PollCreate) callers will pass `Some(snapshot)` after Task 4.

- [ ] **Step 4: Run test to verify it passes**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(publish_event_with_snapshot_passes_through_to_apply)' 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 5: Verify 5 gates**

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_voting_log_engine.rs
git commit -m "$(cat <<'EOF'
feat(zeb-298+zeb-312): add snapshot param to VotingLogEngine::publish_event

ZEB-298+ZEB-312 PR 2 Task 3. publish_event now accepts
Option<MembershipSnapshot> so the Tier 3 PollCreate IPC can thread the
electorate snapshot it already built (for the eligibility pre-check)
through to apply_with_snapshot — matching the IPC's current
log.apply_with_snapshot(event, &id, Some(snapshot)) shape exactly.

Engine-auto orchestration mint paths pass None (kd=ss/sf/cl/rs events
inherit the poll's frozen snapshot from state, not a fresh one).
EOF
)"
```

---

## Task 4: Add `CommunityVotingPolicy` struct + `notify_on_delegate_signal` field + wire-format fixture

**Files:**
- Modify: `src-tauri/src/community_voting_conviction.rs` (add `CommunityVotingPolicy` struct)
- Modify: `src-tauri/src/community_voting_log.rs` (store `policy: CommunityVotingPolicy` on `VotingLog`; `get_policy()` + `set_policy()`)
- Create: `src-tauri/tests/wire_format_community_voting_policy_fixtures.rs`

- [ ] **Step 1: Write the failing wire-format fixture test**

```rust
// src-tauri/tests/wire_format_community_voting_policy_fixtures.rs
use harmony_app::community_voting_conviction::CommunityVotingPolicy;

#[test]
fn default_policy_cbor_pinned() {
    let p = CommunityVotingPolicy::default();
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&p, &mut buf).expect("encode");
    // Default = all fields default = empty map (with #[serde(default)] on
    // every field, ciborium-skip-default emits {}).
    // Pin the exact bytes:
    assert_eq!(buf, vec![0xA0]); // CBOR map(0)
}

#[test]
fn notify_on_delegate_signal_true_cbor_pinned() {
    let p = CommunityVotingPolicy {
        notify_on_delegate_signal: true,
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&p, &mut buf).expect("encode");
    // CBOR map(1) with key "nd" (text(2)) → bool(true) (0xF5)
    // Pin the exact bytes:
    assert_eq!(buf, vec![0xA1, 0x62, b'n', b'd', 0xF5]);
}

#[test]
fn absent_field_decodes_as_default_false() {
    let bytes = vec![0xA0u8]; // empty map
    let p: CommunityVotingPolicy =
        ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert!(!p.notify_on_delegate_signal);
}
```

- [ ] **Step 2: Run tests to verify they fail (compile error — struct doesn't exist)**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_community_voting_policy_fixtures 2>&1 | tail -10
```

Expected: FAIL with `cannot find type CommunityVotingPolicy`.

- [ ] **Step 3: Add the struct**

```rust
// src-tauri/src/community_voting_conviction.rs (add near the other public types):

/// ZEB-298: community-scoped voting policy. Settings that apply to all
/// Tier 2 polls in a community (Tier 1 + Tier 3 currently have no
/// community-scoped settings; this struct grows organically as more
/// policy fields are needed).
///
/// All fields default to `false` so existing communities that don't
/// have a policy stored (or have an older serialized policy missing
/// new fields) preserve their pre-policy behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommunityVotingPolicy {
    /// When `true`, the engine emits `voting-delegate-signaled-on-your-behalf`
    /// on inbound kd=signal (Tier 2 Signal) when the signaler is the local
    /// user's current delegate in this community. Opt-in so existing
    /// communities don't suddenly notify.
    #[serde(default, rename = "nd")]
    pub notify_on_delegate_signal: bool,
}
```

Store on `VotingLog` (`community_voting_log.rs`):

```rust
pub struct VotingLog {
    // ... existing fields ...
    /// ZEB-298: community-scoped voting policy. Mutated via IPC, not via
    /// signed event (no consensus needed — policy is local UX preference,
    /// not a tally-affecting decision).
    policy: crate::community_voting_conviction::CommunityVotingPolicy,
}

impl VotingLog {
    pub fn policy(&self) -> &crate::community_voting_conviction::CommunityVotingPolicy {
        &self.policy
    }

    pub fn set_policy(&mut self, policy: crate::community_voting_conviction::CommunityVotingPolicy) {
        self.policy = policy;
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_community_voting_policy_fixtures 2>&1 | tail -10
```

Expected: all 3 PASS.

- [ ] **Step 5: Verify 5 gates**

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_voting_conviction.rs src-tauri/src/community_voting_log.rs src-tauri/tests/wire_format_community_voting_policy_fixtures.rs
git commit -m "$(cat <<'EOF'
feat(zeb-298): add CommunityVotingPolicy struct + wire-format fixture

ZEB-298+ZEB-312 PR 2 Task 4. New CommunityVotingPolicy struct with one
opt-in field for now (notify_on_delegate_signal, rename "nd") — opt-in
so existing communities don't suddenly notify on Tier 2 Signal events
from delegates. Stored on VotingLog with policy() + set_policy()
accessors. Mutation path is via IPC (Task 6), not via signed event
(policy is local UX preference, not consensus-relevant).

Wire-format pinned: default = empty CBOR map (0xA0), nd=true = map(1)
with "nd" key → bool(true). #[serde(default)] preserves backward
compatibility — absent field decodes as false.
EOF
)"
```

---

## Task 5: Add `maybe_emit_delegate_on_behalf` hook + emit `voting-delegate-signaled-on-your-behalf`

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (new private method on `VotingLogEngine` + call sites)

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn maybe_emit_delegate_on_behalf_fires_when_policy_enabled_and_delegate_matches() {
    // Setup: alice delegates to bob; community policy notify_on_delegate_signal = true.
    // Engine for alice's device. Process inbound: kd=signal by bob on poll P.
    // Assert: voting-delegate-signaled-on-your-behalf is emitted via app_handle
    //   with payload { community_id, proposal_id: P, delegate: bob's OwnerAddr, support: true }.
    // ...
}

#[tokio::test]
async fn maybe_emit_delegate_on_behalf_silent_when_policy_disabled() {
    // Same setup, but notify_on_delegate_signal = false.
    // Assert: no emit.
}

#[tokio::test]
async fn maybe_emit_delegate_on_behalf_silent_when_signaler_not_local_delegate() {
    // notify_on_delegate_signal = true, but bob is NOT alice's delegate.
    // Assert: no emit.
}
```

Use the existing test harness pattern — there's likely a `MockAppHandle` or recorded-emit-event helper in the engine's tests module. If not, mock via a `tokio::sync::mpsc::channel` injected as the emit sink (the engine can be parameterized for testing without a full Tauri runtime).

- [ ] **Step 2: Run tests to verify they fail**

Expected: FAIL with `cannot find function maybe_emit_delegate_on_behalf`.

- [ ] **Step 3: Add the hook**

```rust
impl<R: tauri::Runtime> VotingLogEngine<R> {
    /// ZEB-298: emit voting-delegate-signaled-on-your-behalf when:
    /// (1) the just-applied event is a Tier 2 Signal (kd=signal under
    /// Tier::Conviction), AND
    /// (2) the community policy `notify_on_delegate_signal` is true, AND
    /// (3) the signaler is the local user's CURRENT delegate in this
    /// community, AND
    /// (4) the local user is a registered member of this community.
    ///
    /// Called from both `publish_event` (after apply on the originating
    /// node) and `process_inbound` (after apply on a receiving peer) so
    /// every replica that holds the local user's `local_signing` sees the
    /// notification consistently.
    async fn maybe_emit_delegate_on_behalf(
        &self,
        event: &crate::community_voting_core::SignedVotingEvent,
        poll_id: &crate::community_voting_core::PollId,
    ) {
        // Bail unless app_handle is wired (PR 2 Task 2 made this always Some
        // in production; test harnesses without an app may pass None).
        let Some(app) = self.app_handle.as_ref() else { return };

        // Filter to Tier 2 Signal.
        if !matches!(
            (event.tier, event.kind),
            (
                crate::community_voting_core::Tier::Conviction,
                crate::community_voting_core::PollEventKindCode::Signal,
            )
        ) {
            return;
        }

        // Read policy under the log lock; bail if notify disabled.
        let policy = {
            let log_g = self.voting_log.lock().await;
            log_g.policy().clone()
        };
        if !policy.notify_on_delegate_signal {
            return;
        }

        // Look up local user's current delegate for this community.
        // (Delegate lookup goes through the existing Tier 2 conviction-
        // state accessor; signature TBD by implementer — read the Tier 2
        // delegation state types in community_voting_conviction.rs.)
        let Some(local_owner) = self.local_owner.as_ref() else { return };
        let log_g = self.voting_log.lock().await;
        let Some(current_delegate) = log_g.current_delegate_for(local_owner) else {
            return;
        };
        let is_signal_by_local_delegate = event.actor == current_delegate;
        let is_local_a_member = log_g.is_member(local_owner);
        drop(log_g);
        if !is_signal_by_local_delegate || !is_local_a_member {
            return;
        }

        // Decode the signal payload to read `support`.
        let payload: crate::community_voting_conviction::SignalPayload =
            match ciborium::de::from_reader(&event.payload[..]) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "delegate-on-behalf emit: signal payload decode failed");
                    return;
                }
            };

        // Emit the Tauri event.
        #[derive(serde::Serialize)]
        struct Payload<'a> {
            community_id: &'a str,
            proposal_id: &'a str,
            delegate: &'a str,
            support: bool,
        }
        let community_id_hex = hex::encode(self.community_id.0);
        let proposal_id_hex = hex::encode(poll_id.0);
        let delegate_hex = hex::encode(event.actor.0);
        if let Err(e) = tauri::Emitter::emit(
            app,
            "voting-delegate-signaled-on-your-behalf",
            Payload {
                community_id: &community_id_hex,
                proposal_id: &proposal_id_hex,
                delegate: &delegate_hex,
                support: payload.support,
            },
        ) {
            tracing::warn!(error = %e, "delegate-on-behalf emit failed");
        }
    }
}
```

(The implementer subagent will discover the exact `current_delegate_for` / `is_member` accessor names by reading `community_voting_conviction.rs` — if those don't exist verbatim, use whatever existing delegate-graph and member-set accessors are present. The contract is: read locally without mutating.)

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Verify 5 gates**

- [ ] **Step 6: Commit**

```bash
git commit -m "feat(zeb-298): add maybe_emit_delegate_on_behalf hook on VotingLogEngine

Emits voting-delegate-signaled-on-your-behalf when (1) the just-applied
event is a Tier 2 Signal, (2) the community policy notify_on_delegate_signal
is enabled, (3) the signaler is the local user's current delegate in this
community, and (4) the local user is a registered member. Wired into the
emit-side of the engine (Task 7 wires it from publish_event; Task 8 wires
it from process_inbound)."
```

---

## Task 6: Add `voting_set_notify_on_delegate_signal` IPC

**Files:**
- Modify: `src-tauri/src/lib.rs` (new IPC + register in both `invoke_handler` sites)

- [ ] **Step 1: Write the failing test**

Add to the existing `voting_ipc_tests` module or a new IPC integration test:

```rust
#[tokio::test]
async fn voting_set_notify_on_delegate_signal_round_trip() {
    // Construct a NodeState with one Tier 2 community.
    // Set notify_on_delegate_signal = true via the IPC.
    // Read the policy back via VotingLog::policy().
    // Assert the field is true.
    // Set it back to false; assert false.
    // ...
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL with `cannot find function voting_set_notify_on_delegate_signal`.

- [ ] **Step 3: Add the IPC**

```rust
#[tauri::command(rename_all = "snake_case")]
async fn voting_set_notify_on_delegate_signal<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    community_id: String,
    enabled: bool,
) -> Result<(), String> {
    let space_id = parse_community_id_hex(&community_id)?;
    let voting_logs = app.state::<VotingLogMap>().inner().clone();
    let log_arc = {
        let g = voting_logs.lock().await;
        g.get(&space_id).cloned().ok_or_else(|| {
            "no voting log for community (community must have at least one Tier 2 poll first)".to_string()
        })?
    };
    let mut log_g = log_arc.lock().await;
    let mut policy = log_g.policy().clone();
    policy.notify_on_delegate_signal = enabled;
    log_g.set_policy(policy);
    Ok(())
}
```

Register in both `invoke_handler` sites (production `run()` and `add_dm_ipc_handlers` if present — grep for an existing voting IPC to find both sites).

- [ ] **Step 4: Run test to verify it passes**

- [ ] **Step 5: Verify 5 gates**

- [ ] **Step 6: Commit**

```bash
git commit -m "feat(zeb-298): voting_set_notify_on_delegate_signal IPC

Per-community opt-in for the delegate-on-behalf Tauri event. Defaults to
false (preserved by Task 4's #[serde(default)]). Idempotent — subsequent
sets overwrite the prior value."
```

---

## Task 7: Route 6 Tier 3 IPCs through `engine.publish_event`

**Files:**
- Modify: `src-tauri/src/lib.rs` (6 IPCs at the line ranges below)

| IPC | Current apply site | New apply site |
|---|---|---|
| `voting_create_tier3_proposal` | `lib.rs:21446` `log.apply_with_snapshot(event, &space_id, Some(snapshot))` | `engine.publish_event(event, Some(snapshot))` |
| `voting_submit_deliberation_statement` | `lib.rs:21618` `log.apply_with_snapshot(event, &space_id, None)` | `engine.publish_event(event, None)` |
| `voting_propose_draft_candidate` | `lib.rs:21705` | same shape |
| `voting_approve_draft_candidate` | `lib.rs:21825` | same shape |
| `voting_decline_sortition` | `lib.rs:21902` | same shape |
| `voting_cast_ratification_ballot` | `lib.rs:22012` | same shape |

- [ ] **Step 1: Write a failing test that exercises engine-auto orchestration via IPC**

```rust
#[tokio::test]
async fn voting_cast_ratification_ballot_triggers_engine_auto_kd_cl_kd_rs_via_ipc() {
    // Set up Tier 3 poll in Stage 2 (ratification open) with a ratification
    // window that expires immediately. Call voting_cast_ratification_ballot
    // via the IPC. Drive the engine's deadline-tick to trigger kd=cl + kd=rs.
    // Assert: engine-auto orchestration fires (kd=cl applied; kd=rs applied).
    // ...
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL because the current IPC bypasses the engine and engine-auto orchestration never fires in production.

- [ ] **Step 3: Refactor each IPC**

For each of the 6 IPCs, replace:

```rust
// Old:
let mut log_g = log_arc.lock().await;
log_g.apply_with_snapshot(event.clone(), &space_id, snapshot.clone())
    .map_err(|e| e.to_string())?;
drop(log_g);
```

with:

```rust
// New: ensure engine exists, then publish through it.
crate::lib::ensure_voting_engine_for(
    voting_log_engines, voting_logs, voting_log_adapter_request_tx,
    space_id, hlc_tracker.clone(), device_id.clone(), membership_resolver.clone(),
    local_signing_key.clone(), local_owner, self_identity_pub_64,
    crdt_state.clone(), app.app_handle().clone(),
    dfrost_log_registry.clone(), beacon_requester.clone(),
).await?;
let engine_arc = {
    let g = voting_log_engines.lock()
        .map_err(|e| format!("voting_log_engines poisoned: {e}"))?;
    g.get(&space_id).cloned().ok_or_else(|| {
        "voting engine missing after ensure".to_string()
    })?
};
engine_arc.publish_event(event, snapshot).await?;
```

(Exact parameter list will match what `ensure_voting_engine_for` ends up taking after Task 2's signature update; the implementer subagent reads the live signature.)

For `voting_create_tier3_proposal` specifically, `snapshot` is `Some(snapshot)`. For all other 5 IPCs, `snapshot` is `None`.

The IPC's pre-flight (validate config + build snapshot + eligibility check + reserve HLC + sign) stays exactly the same — only the final apply changes.

- [ ] **Step 4: Run test to verify it passes**

- [ ] **Step 5: Verify 5 gates**

Expect existing Tier 3 IPC integration tests in `community_voting_tier3_ipc_integration.rs` to mostly still pass; some may need adjustment if they relied on the engine NOT being present (Task 11 rewrites these to Path A regardless).

- [ ] **Step 6: Commit**

```bash
git commit -m "feat(zeb-312): route 6 Tier 3 IPCs through engine.publish_event

Each Tier 3 IPC (voting_create_tier3_proposal, voting_submit_deliberation_statement,
voting_propose_draft_candidate, voting_approve_draft_candidate,
voting_decline_sortition, voting_cast_ratification_ballot) now goes through
the per-community VotingLogEngine instead of applying directly to VotingLog.
This activates engine-auto orchestration (kd=ss/sf/cl/rs minting) from real
user actions in production — previously these hooks were dormant per the
ZEB-310 PR #149 'engine wiring + dormancy gap' note."
```

---

## Task 8: Fire post-apply hooks on inbound apply path

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs:1455-1526` (`process_inbound` — fire hooks after lock-released apply)

The current `process_inbound` holds the log lock across apply (line 1511–1514 in PR 1's shape) and exits without invoking any post-apply hook. The publish-side fires three (`maybe_trigger_engine_auto_orchestration`, `maybe_trigger_beacon_for_tier3_create`, `maybe_emit_tier3_lifecycle_events`) plus this PR adds `maybe_emit_delegate_on_behalf`. Inbound must reach the same post-state.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn process_inbound_tier3_poll_create_triggers_beacon_request_on_peer_replica() {
    // Two engines. Engine A creates a Tier 3 poll via publish_event (beacon
    // request fires locally). Engine B receives the kd=cr via Zenoh (real,
    // not mpsc bridge). After process_inbound on B, assert: B also issued
    // a beacon request via its dfrost_log_registry — i.e.,
    // maybe_trigger_beacon_for_tier3_create fired on the receive side.
}

#[tokio::test]
async fn process_inbound_tier2_signal_emits_delegate_on_behalf_when_local_delegate_matches() {
    // Similar two-engine setup focused on the ZEB-298 path.
}
```

- [ ] **Step 2: Run tests to verify they fail**

Expected: FAIL because the hooks don't fire from `process_inbound` yet.

- [ ] **Step 3: Refactor `process_inbound` to fire hooks (lock-released)**

```rust
// In process_inbound, after the existing apply step (around line 1514):

// Drop the log lock between apply and hook invocation. The hooks
// re-acquire what they need internally (publish_event holds this
// invariant — mirror it). Without dropping here, deadlock risk on
// hooks that re-acquire `voting_log` (e.g., maybe_emit_delegate_on_behalf
// reads policy under the log lock).
drop(log_g);

// Re-derive poll_id from event for the hook signatures.
let poll_id = crate::community_voting_log_engine::derive_poll_id(&event);
// (or whatever the existing helper is — search the engine for `derive_poll_id`
// or compute via `&event`'s payload.)

// Capture previous_stage if needed for lifecycle events. The publish-side
// hook signature uses `previous_stage: Option<Tier3Stage>`. On inbound,
// the previous stage must be read BEFORE apply, so reorder if necessary:
// re-acquire log briefly to capture previous stage, then drop, then apply,
// then re-acquire to capture new stage, then drop, then fire hooks.
//
// In practice the existing engine code in publish_event already solves
// this — search publish_event for the previous_stage capture pattern and
// mirror it inside process_inbound. The exact code is mechanical.

self.maybe_trigger_engine_auto_orchestration(&poll_id).await;
self.maybe_trigger_beacon_for_tier3_create(&event).await;
self.maybe_emit_tier3_lifecycle_events(&event, &previous_stage, &poll_id).await;
self.maybe_emit_delegate_on_behalf(&event, &poll_id).await;
```

The implementer subagent should also wire `maybe_emit_delegate_on_behalf` into `publish_event` (where it doesn't fire yet because Task 5 only added the hook function). Add the call site in `publish_event` parallel to the existing `maybe_emit_tier3_lifecycle_events` call.

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Run full Tier 3 + Tier 2 test suite**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voting_tier3) or test(voting_tier2) or test(community_voting_log_engine)' 2>&1 | tail -15
```

Expected: all pass. If a previously-passing Tier 3 test fails because hooks now fire from inbound and produce a different observable state (e.g., kd=cl auto-minted from a peer replica that previously stayed dormant), update the assertion to match the new production semantics — this is the intended behavior per ZEB-312 spec section 7.

- [ ] **Step 6: Verify 5 gates**

- [ ] **Step 7: Commit**

```bash
git commit -m "feat(zeb-312): fire post-apply hooks on process_inbound

Inbound apply path now invokes maybe_trigger_engine_auto_orchestration +
maybe_trigger_beacon_for_tier3_create + maybe_emit_tier3_lifecycle_events
+ maybe_emit_delegate_on_behalf (ZEB-298) after the lock-released apply.
Peer replicas now reach identical post-state to the originating node —
they auto-orchestrate kd=ss/sf/cl/rs, request beacons on Tier 3 PollCreate,
emit Tier 3 lifecycle events to their UI, and fire delegate-on-behalf
toasts when their local user's delegate signals on a Tier 2 proposal."
```

---

## Task 9: Create `Toast.svelte` + `ToastHost.svelte` + toast store

**Files:**
- Create: `src/lib/components/Toast.svelte`
- Create: `src/lib/components/ToastHost.svelte`
- Create: `src/lib/stores/toast.ts`
- Create: `src/lib/__tests__/toast-store.test.ts`

- [ ] **Step 1: Write the failing vitest**

```typescript
// src/lib/__tests__/toast-store.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';
import { toastStore } from '$lib/stores/toast';

describe('toastStore', () => {
  beforeEach(() => {
    // Clear all toasts before each test.
    const toasts = get(toastStore.toasts);
    toasts.forEach(t => toastStore.dismiss(t.id));
  });

  it('show() adds a toast', () => {
    toastStore.show('hello');
    expect(get(toastStore.toasts)).toHaveLength(1);
    expect(get(toastStore.toasts)[0].message).toBe('hello');
  });

  it('auto-dismisses after duration', async () => {
    vi.useFakeTimers();
    toastStore.show('hi', 100);
    expect(get(toastStore.toasts)).toHaveLength(1);
    vi.advanceTimersByTime(150);
    expect(get(toastStore.toasts)).toHaveLength(0);
    vi.useRealTimers();
  });

  it('manual dismiss removes by id', () => {
    toastStore.show('a');
    toastStore.show('b');
    const toasts = get(toastStore.toasts);
    expect(toasts).toHaveLength(2);
    toastStore.dismiss(toasts[0].id);
    expect(get(toastStore.toasts)).toHaveLength(1);
  });
});
```

- [ ] **Step 2: Run vitest to verify it fails**

```bash
npx vitest run src/lib/__tests__/toast-store.test.ts 2>&1 | tail -10
```

Expected: FAIL with `Cannot find module '$lib/stores/toast'`.

- [ ] **Step 3: Implement the store**

```typescript
// src/lib/stores/toast.ts
import { writable, type Writable } from 'svelte/store';

export type Toast = {
  id: string;
  message: string;
  durationMs: number;
};

const toasts: Writable<Toast[]> = writable([]);

function show(message: string, durationMs = 5000): string {
  const id = (typeof crypto !== 'undefined' && crypto.randomUUID)
    ? crypto.randomUUID()
    : `toast-${Date.now()}-${Math.random().toString(36).slice(2)}`;
  toasts.update(arr => [...arr, { id, message, durationMs }]);
  setTimeout(() => dismiss(id), durationMs);
  return id;
}

function dismiss(id: string): void {
  toasts.update(arr => arr.filter(t => t.id !== id));
}

export const toastStore = { toasts, show, dismiss };
```

- [ ] **Step 4: Implement Toast.svelte and ToastHost.svelte**

```svelte
<!-- src/lib/components/Toast.svelte -->
<script lang="ts">
  import { fly } from 'svelte/transition';
  import { toastStore, type Toast } from '$lib/stores/toast';

  let { toast }: { toast: Toast } = $props();
</script>

<div class="toast" role="status" aria-live="polite" transition:fly={{ y: 20, duration: 200 }}>
  <span class="message">{toast.message}</span>
  <button
    type="button"
    class="dismiss"
    aria-label="Dismiss notification"
    onclick={() => toastStore.dismiss(toast.id)}
  >
    ×
  </button>
</div>

<style>
  .toast {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.75rem 1rem;
    background: var(--toast-bg, rgba(20, 22, 30, 0.95));
    color: var(--toast-fg, #fff);
    border-radius: 6px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.2);
    max-width: 360px;
  }
  .message { flex: 1; line-height: 1.4; }
  .dismiss {
    background: transparent; border: 0; color: inherit;
    font-size: 1.25rem; line-height: 1; cursor: pointer;
    padding: 0 0.25rem;
  }
</style>
```

```svelte
<!-- src/lib/components/ToastHost.svelte -->
<script lang="ts">
  import { toastStore } from '$lib/stores/toast';
  import Toast from './Toast.svelte';

  let toasts = $derived($toastStore.toasts);
</script>

<div class="toast-host" aria-live="polite">
  {#each toasts as t (t.id)}
    <Toast toast={t} />
  {/each}
</div>

<style>
  .toast-host {
    position: fixed;
    bottom: 1rem;
    right: 1rem;
    display: flex;
    flex-direction: column-reverse;
    gap: 0.5rem;
    z-index: 9999;
    pointer-events: none;
  }
  .toast-host > :global(*) {
    pointer-events: auto;
  }
</style>
```

(If `$derived($toastStore.toasts)` doesn't work in the project's Svelte 5 setup — auto-store-subscription depends on how the store's exports are shaped — substitute the explicit subscription via `onMount` + `toasts.subscribe(...)` pattern. The implementer subagent reads existing Svelte 5 patterns elsewhere in the codebase, e.g., `CommunityView.svelte` or `DmCreateDialog.svelte`, for the project's convention.)

- [ ] **Step 5: Run vitest to verify pass**

```bash
npx vitest run src/lib/__tests__/toast-store.test.ts 2>&1 | tail -5
```

Expected: 3/3 PASS.

- [ ] **Step 6: Verify 5 gates**

- [ ] **Step 7: Commit**

```bash
git commit -m "feat(zeb-298): minimal toast infrastructure (Toast.svelte + ToastHost.svelte + store)

Bottom-right toast stack with auto-dismiss + manual dismiss. Wired via
a singleton writable store (src/lib/stores/toast.ts). No dependencies on
external toast libraries — minimum-viable for the ZEB-298 delegate-on-behalf
notification. Future toast use cases (channel invites, etc.) reuse the same
store.show() entry point."
```

---

## Task 10: Wire toast to `voting-delegate-signaled-on-your-behalf`

**Files:**
- Modify: `src/App.svelte` (mount `<ToastHost />`; subscribe to `onVotingDelegateSignaledOnYourBehalf`)

- [ ] **Step 1: Write the failing test (vitest, mock the adapter)**

```typescript
// src/lib/__tests__/delegate-on-behalf-toast.test.ts
import { describe, it, expect, vi } from 'vitest';
import { get } from 'svelte/store';
import { toastStore } from '$lib/stores/toast';
import * as votingAdapter from '$lib/voting-adapter';

describe('delegate-on-behalf toast wiring', () => {
  it('shows a toast when voting-delegate-signaled-on-your-behalf fires', async () => {
    // Mock the subscription helper to capture the registered handler.
    let registeredHandler: any = null;
    vi.spyOn(votingAdapter, 'onVotingDelegateSignaledOnYourBehalf')
      .mockImplementation((handler: any) => { registeredHandler = handler; });

    // Import + mount App.svelte (or the wiring helper alone if extracted).
    const { setupDelegateOnBehalfToast } = await import('$lib/voting-toast-wiring');
    setupDelegateOnBehalfToast();
    expect(registeredHandler).not.toBeNull();

    // Fire the handler with a synthetic payload.
    registeredHandler({
      communityId: 'aa'.repeat(16),
      proposalId: 'bb'.repeat(16),
      delegate: 'cc'.repeat(16),
      support: true,
    });

    const toasts = get(toastStore.toasts);
    expect(toasts).toHaveLength(1);
    expect(toasts[0].message).toMatch(/signaled support/);
  });
});
```

- [ ] **Step 2: Run vitest to verify fail**

Expected: FAIL with `Cannot find module '$lib/voting-toast-wiring'`.

- [ ] **Step 3: Implement the wiring helper**

```typescript
// src/lib/voting-toast-wiring.ts
import { onVotingDelegateSignaledOnYourBehalf } from '$lib/voting-adapter';
import { toastStore } from '$lib/stores/toast';
import type { VotingDelegateSignaledOnYourBehalfPayload } from '$lib/types/voting';

/** Shortened address for display: first 8 hex chars. */
function shortAddr(hex: string): string {
  return hex.slice(0, 8);
}

export function setupDelegateOnBehalfToast(): void {
  onVotingDelegateSignaledOnYourBehalf((payload: VotingDelegateSignaledOnYourBehalfPayload) => {
    const verb = payload.support ? 'signaled support for' : 'signaled against';
    const delegate = shortAddr(payload.delegate);
    const proposal = shortAddr(payload.proposalId);
    toastStore.show(`Delegate ${delegate} ${verb} proposal ${proposal}`, 5000);
  });
}
```

(Replace `shortAddr` with whatever display-name resolution helper exists in the codebase — e.g., if there's a `displayNameFor(ownerAddr)` helper or a contacts store, use that for the delegate field. The 8-char hex fallback is the minimum acceptable display.)

- [ ] **Step 4: Wire into App.svelte**

```svelte
<!-- src/App.svelte -->
<script lang="ts">
  import { onMount } from 'svelte';
  import ToastHost from '$lib/components/ToastHost.svelte';
  import { setupDelegateOnBehalfToast } from '$lib/voting-toast-wiring';
  // ... existing imports ...

  onMount(() => {
    setupDelegateOnBehalfToast();
    // ... existing onMount ...
  });
</script>

<!-- ... existing markup ... -->
<ToastHost />
```

- [ ] **Step 5: Run vitest + tsc**

Expected: PASS.

- [ ] **Step 6: Verify 5 gates**

- [ ] **Step 7: Commit**

```bash
git commit -m "feat(zeb-298): wire voting-delegate-signaled-on-your-behalf to toast UI

Subscribes the Tauri event from PR #132's stub (still wired in
voting-adapter.ts) to toastStore.show(). Mounts <ToastHost /> at App.svelte
top level. Toast message: 'Delegate {short} signaled {support|against}
proposal {short}'. Future toast UX (full display names, click-to-jump to
proposal) is a follow-up."
```

---

## Task 11: Rewrite 4 IPC integration tests (Path C → Path A)

**Files:**
- Modify: `src-tauri/tests/community_voting_tier3_ipc_integration.rs` (Tests 1–4)

Tests 1–4 currently construct two engines directly + call `engine.publish_event` (Path C). Rewrite to invoke the IPC via Tauri's mock builder + `get_ipc_response` (Path A). Test 5 already uses Path A and is unchanged.

Pattern source: `src-tauri/tests/dm_ipc_roundtrip.rs` (use as the reference for `mock_builder() + add_*_ipc_handlers + get_ipc_response`).

- [ ] **Step 1: Read the existing dm_ipc_roundtrip.rs pattern**

```bash
head -150 src-tauri/tests/dm_ipc_roundtrip.rs
```

Capture: how `mock_builder()` is constructed, how state is initialized, how `get_ipc_response` is called with a JSON payload, how the response is unwrapped to assert behavior.

- [ ] **Step 2: Refactor Test 1 (`ipc_tier3_full_lifecycle_two_engines` at line 423)**

The Path-C-to-Path-A rewrite:
- Setup: build a NodeState with both engines (or two separate mock apps if simulating two devices), pre-seed membership + admin keys.
- Each "publish event" call becomes a JSON IPC invocation via `get_ipc_response`.
- Assertions stay the same but read from the application state via IPC reads (a `voting_read_poll_state` IPC may need to exist; if not, this is a sub-task — but check first whether such an IPC already exists from ZEB-310).

(The implementer subagent does the mechanical rewrite. If a read-side IPC doesn't exist, surface to the controller before improvising — adding an IPC just for tests is a scope creep that needs sign-off.)

- [ ] **Step 3: Refactor Tests 2, 3, 4**

Same pattern.

- [ ] **Step 4: Run the test file**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_voting_tier3_ipc_integration 2>&1 | tail -20
```

Expected: Tests 1–5 PASS.

- [ ] **Step 5: Verify 5 gates**

- [ ] **Step 6: Commit**

```bash
git commit -m "test(zeb-312): rewrite Tier 3 IPC integration Tests 1-4 to Path A

Tests 1-4 now invoke the actual IPC handlers via Tauri's mock_builder() +
get_ipc_response, mirroring dm_ipc_roundtrip.rs. Tests 5 (error extraction)
already used Path A; unchanged. The engine-layer orchestration tests in
community_voting_tier3_integration.rs still cover the engine-internal
layer; this file now covers the IPC happy-path bug surface."
```

---

## Task 12: Two-engine integration test for delegate-on-behalf

**Files:**
- Create: `src-tauri/tests/community_voting_tier2_delegate_on_behalf_integration.rs`

- [ ] **Step 1: Write the test**

```rust
//! ZEB-298 integration test: alice delegates to bob; bob signals on a
//! Tier 2 proposal on engine B; engine A receives via real Zenoh and
//! emits voting-delegate-signaled-on-your-behalf when its community
//! policy notify_on_delegate_signal is enabled.

#[tokio::test(flavor = "multi_thread")]
async fn delegate_on_behalf_emit_two_engine_real_zenoh() {
    // Setup:
    // - Two zenoh::Session instances (real, not mpsc bridge).
    // - Two VotingLogEngine instances (alice on A, bob on B) for the
    //   same community_id.
    // - Both engines pre-seeded with Tier 2 community memberships
    //   containing both alice + bob.
    // - alice's engine (A) has set notify_on_delegate_signal = true.
    // - alice has delegated to bob in this community (Tier 2 Delegate event
    //   applied on both engines via either direct apply or pre-replay).

    // Capture emits: replace alice's app_handle with a mock that records
    // emit() calls into a tokio::sync::mpsc channel.

    // bob's engine B: create a Tier 2 proposal, then bob signals support=true.
    // Both events flow A → B via Zenoh outbound and B's process_inbound
    // applies them.

    // Wait for A's receive subscription to consume the kd=signal:
    let warmup = std::time::Duration::from_millis(2000);
    tokio::time::timeout(warmup, async {
        // poll engine A's state for the signal in 20×100ms ticks
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let signal_seen = /* read engine A's log for bob's signal */;
            if signal_seen { return; }
        }
        panic!("engine A did not converge on bob's signal within 2s");
    }).await.unwrap();

    // Assert: voting-delegate-signaled-on-your-behalf was emitted on engine A.
    let emitted = emit_rx.recv().await.expect("emit");
    assert_eq!(emitted.event, "voting-delegate-signaled-on-your-behalf");
    assert!(emitted.payload.contains("\"delegate\":\"") /* bob's hex */);
    assert!(emitted.payload.contains("\"support\":true"));

    // Assert: delegation graph parity (alice → bob on both engines).
    // Assert: total_conviction_at_with_delegation matches across A + B.

    // alice later directly overrides on the same proposal with support=false.
    // Re-fetch tally on both engines; bob's effective weight should drop to
    // zero ON THIS PROPOSAL ONLY (other proposals where alice has no direct
    // signal still inherit bob's delegated weight).
}
```

Per `feedback_wall_clock_regression_budget`: use logical-time poll (20×100ms tick loop) instead of a fixed sleep, matching the PR 1 zenoh test's pattern.

- [ ] **Step 2: Run the test**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test community_voting_tier2_delegate_on_behalf_integration 2>&1 | tail -10
```

Expected: PASS.

- [ ] **Step 3: Verify 5 gates**

- [ ] **Step 4: Commit**

```bash
git commit -m "test(zeb-298): two-engine real-Zenoh test for delegate-on-behalf emit

End-to-end: alice delegates to bob; bob signals on Tier 2 proposal on
engine B; engine A receives via real Zenoh and emits
voting-delegate-signaled-on-your-behalf (with policy enabled). Verifies
delegation-graph parity + total_conviction_at_with_delegation parity +
emit fires + alice's direct override drops bob's effective weight on
that proposal only."
```

---

## Task 13: Final 5-gate sweep + push + PR creation

**Files:** none (verification + push + PR open)

- [ ] **Step 1: Full 5-gate sweep**

```bash
cd src-tauri && set -o pipefail
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast 2>&1 | tail -20
cd ..
npx tsc --noEmit
npx vitest run 2>&1 | tail -10
```

Expected: all green except the 28 pre-existing orphans from the baseline. If clippy or nextest surfaces a regression, fix before pushing — don't push red.

- [ ] **Step 2: Push branch**

```bash
git push -u origin zeb-298-zeb-312-consumer
```

- [ ] **Step 3: Open PR**

PR title: `ZEB-298+ZEB-312 PR 2: consumer wiring (Tier 3 IPC routing + Tier 2 delegate-on-behalf + production resolvers)`

PR body must:
- Use markdown-linked refs `[ZEB-298](https://linear.app/zeblith/issue/ZEB-298)` and `[ZEB-312](https://linear.app/zeblith/issue/ZEB-312)` — NO bare references that would re-trigger auto-close cascade per `feedback_linear_pr_auto_close`.
- PR 2 IS the closer for both tickets — include `Closes ZEB-298` and `Closes ZEB-312` lines explicitly (the only PR that does so).
- Cover: what changed (the 13 task headlines), test plan checklist (5 gates + 2 new integration tests), and references to spec + plan files.

```bash
gh pr create --title "ZEB-298+ZEB-312 PR 2: consumer wiring (Tier 3 IPC routing + Tier 2 delegate-on-behalf + production resolvers)" --body "$(cat <<'EOF'
## Summary

PR 2 of 2 for the combined [ZEB-298](https://linear.app/zeblith/issue/ZEB-298) + [ZEB-312](https://linear.app/zeblith/issue/ZEB-312) work. Builds on PR 1's foundation (#150) to make voting actually work peer-to-peer end-to-end.

Closes ZEB-298
Closes ZEB-312

- Wires production `identity_resolver` via `OwnerDeviceCacheResolver` so inbound Tier 1 BallotCast + Tier 2 Signal events from peers actually verify and apply
- Wires production `app_handle: Some(_)` so Tier 3 lifecycle events (sortition-complete / drafting-open / ratification-open / finalized) emit to the UI
- Routes the 6 Tier 3 IPCs through `engine.publish_event` so engine-auto orchestration (kd=ss/sf/cl/rs) fires from real user actions
- Fires post-apply hooks on `process_inbound` so peer replicas auto-orchestrate identically to the originating node
- Adds `CommunityVotingPolicy` struct with `notify_on_delegate_signal` opt-in field (wire-format pinned)
- Adds `maybe_emit_delegate_on_behalf` engine hook + `voting_set_notify_on_delegate_signal` IPC
- Adds minimal `Toast.svelte` + `ToastHost.svelte` + toast store; mounts in `App.svelte`; wires the `voting-delegate-signaled-on-your-behalf` Tauri event subscription to `toastStore.show()`
- Rewrites Tier 3 IPC integration Tests 1–4 from Path C (direct engine calls) to Path A (Tauri `mock_builder()` + `get_ipc_response`)
- New two-engine real-Zenoh integration test for delegate-on-behalf end-to-end

## Test plan

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — 0 warnings
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` — N passed, 28 pre-existing orphans
- [x] `cargo nextest run --locked --features test-fixtures --test community_voting_tier3_ipc_integration` — 5/5 (rewritten Tests 1–4 use Path A)
- [x] `cargo nextest run --locked --features test-fixtures --test community_voting_tier2_delegate_on_behalf_integration` — PASS (new)
- [x] `npx tsc --noEmit`
- [x] `npx vitest run` — all pass including new toast store + wiring tests

## References

- Spec: [`docs/specs/2026-05-20-zeb-298-zeb-312-engine-production-wiring-design.md`](docs/specs/2026-05-20-zeb-298-zeb-312-engine-production-wiring-design.md) (sections 6–12)
- Plan: [`docs/plans/2026-05-21-zeb-298-zeb-312-consumer-plan.md`](docs/plans/2026-05-21-zeb-298-zeb-312-consumer-plan.md)
- Predecessor: PR #150 (foundation)
- Pattern source: `OwnerDeviceCacheResolver` in `community_state_sync.rs:4630` (existing IdentityResolver impl; PR 2 adds the parallel VotingIdentityResolver impl)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 4: Return control to the controller**

After the PR is open, STOP. The controller enters the autonomous bot-review monitoring loop (CodeRabbit, Cursor Bugbot, CodeAnt, Qodo — never Greptile auto-trigger per `feedback_greptile_manual_trigger`; CI is disabled per `feedback_ci_disabled` and is not a wait gate). Pushover fires on ready-to-merge per `feedback_no_pushover_when_active` + `feedback_autonomous_post_spec`.

---

## Self-review checklist

- **Spec coverage:** every PR 2 section (6, 7, 8, 9, 10, 11, 12) of the design spec maps to one or more tasks above. ✓
- **identity_resolver folded in:** Task 1 wires the production resolver (the user-approved decision; not in the original spec but added per the prior recommendation). ✓
- **No placeholders:** every task lists exact files, exact code (or close-enough sketches), exact commands, expected outputs. The few "implementer subagent reads X" notes are escape hatches for cases where the live signature might have drifted; they all name the exact file + symbol to read.
- **Type consistency:** `Option<MembershipSnapshot>` flows from Task 3 (engine signature) → Task 7 (Tier 3 IPC call sites). `CommunityVotingPolicy` introduced in Task 4 is consumed by Task 5 (engine hook) + Task 6 (IPC). `toastStore` introduced in Task 9 is consumed by Task 10 (wiring). No name drift detected.
- **TDD-shaped:** every non-Task-0 + non-Task-13 task starts with a failing test, then implementation, then test re-runs green. Task 0 is verification-only (no commit); Task 13 is the gate sweep + push (no test).
- **Per-task commits:** every non-Task-0 task ends with an explicit `git commit` step. ✓
- **5 gates discipline:** every task ends with a 5-gate verification. ✓ Tasks that touch only Rust still verify tsc/vitest (cheap) to catch unrelated drift early.
- **Per `feedback_implementer_gate_time_budget`:** the subagent dispatcher should add commit-before-gate + 10-min wall-clock kill + DONE_WITH_CONCERNS escape hatch to every implementer prompt.
- **Per `feedback_linear_pr_auto_close`:** PR 2 carries `Closes ZEB-298` and `Closes ZEB-312` (Task 13 Step 3). PR 1's body explicitly deferred these; PR 2 is the closer. Auto-close cascade is intentional this time.
- **Per `feedback_engineer_for_real_scale`:** no design changes introduce unbounded gossip or N² behavior; all new hooks and emit paths are O(1) per event applied. Toast UI is single-stack with O(N) toasts displayed (bounded by user attention, not data size).
