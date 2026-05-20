# ZEB-298+ZEB-312 PR 1 (Foundation) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Production-activate the voting engine — wire Zenoh adapter for outbound/inbound on `harmony/community/{id}/voting`, add `verify_voting_event` with membership snapshot + identity resolver, refactor `process_inbound` to verify-then-apply, remove the feature-gate that blocks peer events, and install all 4 previously-dormant engine fields (`hlc_tracker`, `device_id`, `app_handle`, `local_signing`).

**Architecture:** Mirror the `DfrostLogEngine` Zenoh adapter pattern (ZEB-307, PR #146). Voting verify needs two new abstractions: `VotingIdentityResolver` (`OwnerAddr → VerifyingKey` for signature check, mirroring `ChannelIdentityResolver`) and `MembershipSnapshotResolver` (snapshot-at-HLC for PollCreate events). Non-PollCreate inbound events reuse the snapshot frozen on the poll's state at create time.

**Tech Stack:** Rust (Tauri 2.x, tokio, ciborium, ed25519-dalek, zenoh-rs), TypeScript (Svelte 5, vitest). No frontend changes in PR 1.

---

## Spec reference

Design spec: `docs/specs/2026-05-20-zeb-298-zeb-312-engine-production-wiring-design.md` (commit `2af32ec` on this branch).

PR 1 is the **foundation** — no Tier 3 IPC routing changes; no engine-auto hook firing on inbound (that's PR 2's scope). PR 1 makes the engine production-active so PR 2's consumer changes can layer on top.

## File structure

| File | Responsibility | Change kind |
|---|---|---|
| `src-tauri/src/community_voting_core.rs` | Add `VotingIdentityResolver` trait + `verify_voting_event` function + `VotingVerifyError` enum | Modify (add ~250 LOC) |
| `src-tauri/src/community_voting_log_engine.rs` | Refactor `process_inbound` (resolve snapshot + resolver, call `verify_voting_event`, remove feature-gate); add resolver fields to `VotingLogEngine` + `VotingLogEngineParams` | Modify (add ~150 LOC, delete ~10 LOC for gate) |
| `src-tauri/src/community_voting_log.rs` | Add `MembershipSnapshotResolver` trait + production-side helper to resolve snapshot at HLC from `community_registry` + `crdt_state` | Modify (add ~100 LOC) |
| `src-tauri/src/lib.rs` | Upgrade `ensure_voting_engine_for` signature + body to plumb all 4 dormant fields + 2 resolvers; replace stub mpsc pair with real Zenoh adapter wiring | Modify (add ~200 LOC; net replaces ~30 LOC) |
| `src-tauri/src/event_loop.rs` | Spawn per-community voting Zenoh adapter task (outbound publisher_rx → `session.put`, inbound subscriber → `subscriber_tx`) | Modify (add ~80 LOC) — OR if event_loop is the wrong home, inline into `ensure_voting_engine_for` |
| `src-tauri/tests/community_voting_zenoh_integration.rs` | New file: real-Zenoh two-engine integration test (outbound→inbound loop, NOT the mpsc test bridge) | Create (~300 LOC) |
| `src-tauri/tests/community_voting_process_inbound_prod.rs` | New file: production-build test (no `--features test-fixtures`) that proves the gate is gone — calls `process_inbound` directly with a synthesized peer event + snapshot + resolver | Create (~150 LOC) |
| `src-tauri/src/community_voting_core.rs` `verify_event_tests` (existing module) | Add unit tests for `verify_voting_event` (snapshot mismatch / match / forged signature / no resolver entry) | Modify (add ~150 LOC) |

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

Expected: `On branch zeb-298-zeb-312-foundation`, working tree clean, HEAD = `2af32ec docs(zeb-298+zeb-312): combined design`, parent = `3739d72 ZEB-310 ... (#149)` from origin/main.

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
- fmt: zero output
- clippy: zero warnings
- nextest: 2095 passed / 28 pre-existing orphans (folder_ingest 3 + mint/mint_sync 4 + folder_ingest_walker_integration 9 + rename_content_integration 12)
- tsc: zero output
- vitest: 1921/1921

**NO COMMIT — Task 0 is verification only.**

---

## Task 1: Add `VotingIdentityResolver` trait + `verify_voting_event` function

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs` (add trait + function + tests)

Mirror the `ChannelIdentityResolver` pattern from `community_channel_log_engine.rs:1914`. The trait maps `OwnerAddr → VerifyingKey` so the apply path can verify signatures on peer-delivered voting events. Production impl reads from harmony-identity state; test impl is a `FixedVotingIdentityResolver` with a `HashMap`.

`verify_voting_event` does three things:
1. Membership check: actor exists in `MembershipSnapshot`.
2. Signature check: lookup actor's `VerifyingKey` via resolver, verify Ed25519 sig against `signing_bytes()`.
3. (Eligibility check for create events deferred — happens in `check_eligibility` at apply layer with the same snapshot.)

- [ ] **Step 1: Read existing pattern**

Read `src-tauri/src/community_channel_log_engine.rs:1900-2000` to see `ChannelIdentityResolver` trait shape + `FixedIdentityResolver` test impl. Mirror this for voting.

- [ ] **Step 2: Write failing unit tests**

Add to `src-tauri/src/community_voting_core.rs` near the existing `mod build_tests` or as a sibling `mod voting_verify_tests`:

```rust
#[cfg(test)]
mod voting_verify_tests {
    use super::*;
    use crate::community_membership::ChannelId;
    use crate::community_voting_approval::Tier1PollConfig;
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use rand::rngs::OsRng;
    use std::collections::HashMap;
    use std::sync::Arc;

    struct FixedVotingIdentityResolver {
        map: HashMap<OwnerAddr, VerifyingKey>,
    }

    #[async_trait::async_trait]
    impl VotingIdentityResolver for FixedVotingIdentityResolver {
        async fn verifying_key_for(&self, owner: &OwnerAddr) -> Option<VerifyingKey> {
            self.map.get(owner).copied()
        }
    }

    fn snapshot_of(addrs: &[OwnerAddr]) -> MembershipSnapshot {
        let mut members = HashMap::new();
        for a in addrs {
            members.insert(*a, MemberAttrs { power: 1, vouching_depth: 1 });
        }
        MembershipSnapshot { members }
    }

    fn sample_tier1_event(keypair: &SigningKey, actor: OwnerAddr) -> SignedVotingEvent {
        let cfg = Tier1PollConfig {
            options: vec!["a".into(), "b".into()],
            window_seconds: 600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility { min_power: 0, min_vouching_depth: None, sortition_size: None },
            channel_id: ChannelId([0; 16]),
        };
        let hlc = Hlc { wall_ms: 1, logical: 0, device_id: "a".into() };
        build_signed_poll_create_tier1(keypair, actor, &cfg, hlc).expect("build")
    }

    #[tokio::test]
    async fn verify_voting_event_accepts_valid_event() {
        let keypair = SigningKey::generate(&mut OsRng);
        let vk = keypair.verifying_key();
        let actor = OwnerAddr([0xaa; 16]);
        let ev = sample_tier1_event(&keypair, actor);

        let snapshot = snapshot_of(&[actor]);
        let resolver = Arc::new(FixedVotingIdentityResolver {
            map: HashMap::from([(actor, vk)]),
        });

        assert!(verify_voting_event(&ev, &snapshot, resolver.as_ref()).await.is_ok());
    }

    #[tokio::test]
    async fn verify_voting_event_rejects_actor_not_in_membership() {
        let keypair = SigningKey::generate(&mut OsRng);
        let vk = keypair.verifying_key();
        let actor = OwnerAddr([0xbb; 16]);
        let ev = sample_tier1_event(&keypair, actor);

        // Snapshot contains a DIFFERENT actor.
        let snapshot = snapshot_of(&[OwnerAddr([0xcc; 16])]);
        let resolver = Arc::new(FixedVotingIdentityResolver {
            map: HashMap::from([(actor, vk)]),
        });

        assert!(matches!(
            verify_voting_event(&ev, &snapshot, resolver.as_ref()).await,
            Err(VotingVerifyError::ActorNotInMembership)
        ));
    }

    #[tokio::test]
    async fn verify_voting_event_rejects_forged_signature() {
        let actor = OwnerAddr([0xdd; 16]);
        let real_keypair = SigningKey::generate(&mut OsRng);
        let forger_keypair = SigningKey::generate(&mut OsRng);

        // Forger signs but claims to be `actor`.
        let mut ev = sample_tier1_event(&forger_keypair, actor);
        // sample_tier1_event already signed with forger_keypair — but the
        // build_signed_* helper uses the keypair passed; the test's "forge"
        // intent is that the resolver maps actor → real_keypair's pubkey,
        // so the forger_keypair signature won't verify against real_keypair's
        // verifying key.
        let snapshot = snapshot_of(&[actor]);
        let resolver = Arc::new(FixedVotingIdentityResolver {
            map: HashMap::from([(actor, real_keypair.verifying_key())]),
        });

        assert!(matches!(
            verify_voting_event(&ev, &snapshot, resolver.as_ref()).await,
            Err(VotingVerifyError::InvalidSignature)
        ));
    }

    #[tokio::test]
    async fn verify_voting_event_rejects_no_resolver_entry() {
        let keypair = SigningKey::generate(&mut OsRng);
        let actor = OwnerAddr([0xee; 16]);
        let ev = sample_tier1_event(&keypair, actor);

        let snapshot = snapshot_of(&[actor]);
        // Resolver has NO entry for actor.
        let resolver = Arc::new(FixedVotingIdentityResolver { map: HashMap::new() });

        assert!(matches!(
            verify_voting_event(&ev, &snapshot, resolver.as_ref()).await,
            Err(VotingVerifyError::IdentityNotResolvable)
        ));
    }
}
```

- [ ] **Step 3: Run tests to verify compile failure**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest list --features test-fixtures -E 'test(voting_verify_tests)' 2>&1 | tail -10
```

Expected: compile error — `VotingIdentityResolver`, `verify_voting_event`, `VotingVerifyError` undefined.

- [ ] **Step 4: Add the trait + error enum + function**

Insert into `src-tauri/src/community_voting_core.rs` (near `check_eligibility` around line 868):

```rust
/// Resolves `OwnerAddr` (16-byte truncated hash) to the full Ed25519
/// `VerifyingKey` needed for signature verification on inbound voting
/// events. Production impl reads from `harmony_identity` state; tests
/// use `FixedVotingIdentityResolver`. Mirrors `ChannelIdentityResolver`
/// from `community_channel_log_engine.rs`.
#[async_trait::async_trait]
pub trait VotingIdentityResolver: Send + Sync {
    /// Look up the Ed25519 public key for `owner`. Returns `None` if the
    /// owner is not known to this node (e.g. local state is behind on
    /// joins). Callers MUST treat `None` as "cannot verify, reject" —
    /// never as "accept anyway".
    async fn verifying_key_for(
        &self,
        owner: &OwnerAddr,
    ) -> Option<ed25519_dalek::VerifyingKey>;
}

/// Why a voting event failed verification.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VotingVerifyError {
    #[error("actor not in membership snapshot")]
    ActorNotInMembership,
    #[error("identity not resolvable for actor (resolver returned None)")]
    IdentityNotResolvable,
    #[error("invalid Ed25519 signature")]
    InvalidSignature,
    #[error("malformed event (signing_bytes encode failed)")]
    MalformedEvent,
    #[error("signature length is not 64 bytes")]
    BadSignatureLength,
}

/// Verify an inbound voting event:
///   1. Actor is in the membership snapshot.
///   2. Resolver returns a `VerifyingKey` for the actor.
///   3. The Ed25519 signature on the envelope's `signing_bytes()` checks
///      out against that key.
///
/// Note: This does NOT do eligibility (`check_eligibility`) — apply
/// layer handles that with the same snapshot. Membership is V6 per spec
/// §8 (actor is a community member); signature is the underlying
/// authentication.
pub async fn verify_voting_event(
    event: &SignedVotingEvent,
    snapshot: &MembershipSnapshot,
    resolver: &dyn VotingIdentityResolver,
) -> Result<(), VotingVerifyError> {
    use ed25519_dalek::Verifier;

    // V6: actor must be in the membership snapshot.
    if !snapshot.members.contains_key(&event.actor) {
        return Err(VotingVerifyError::ActorNotInMembership);
    }

    // Resolve actor → VerifyingKey.
    let vk = resolver
        .verifying_key_for(&event.actor)
        .await
        .ok_or(VotingVerifyError::IdentityNotResolvable)?;

    // Reconstruct signing bytes (must match what the originator signed).
    let sb = event
        .signing_bytes()
        .map_err(|_| VotingVerifyError::MalformedEvent)?;

    // Signature must be exactly 64 bytes.
    let sig_bytes: [u8; 64] = event
        .sig
        .clone()
        .try_into()
        .map_err(|_| VotingVerifyError::BadSignatureLength)?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);

    // Verify.
    vk.verify(&sb, &sig)
        .map_err(|_| VotingVerifyError::InvalidSignature)
}
```

Add `thiserror = "..."` to `Cargo.toml` if not already a dep (it likely is — it's used elsewhere).

If `async_trait` isn't already a dep, add `async-trait = "0.1"` to `Cargo.toml` `[dependencies]`. (Likely already a dep too.)

- [ ] **Step 5: Run tests to verify pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(voting_verify_tests)' 2>&1 | tail -10
```

Expected: 4 tests pass.

- [ ] **Step 6: cargo fmt + clippy**

```bash
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_voting_core.rs src-tauri/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(zeb-298+zeb-312): VotingIdentityResolver + verify_voting_event

Adds the trait + async fn needed for production-build signature
verification on inbound voting events. Membership-check via the
MembershipSnapshot (V6 per spec §8); identity lookup via resolver
mirrors ChannelIdentityResolver from community_channel_log_engine.rs.
4 unit tests cover valid / not-member / forged-sig / no-resolver-entry.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add `MembershipSnapshotResolver` trait + production impl

**Files:**
- Modify: `src-tauri/src/community_voting_log.rs` (add trait near existing `MembershipSnapshot` usage)
- Modify: `src-tauri/src/lib.rs` (add production impl `NodeStateMembershipResolver` that reads `community_registry` + `crdt_state`)

The resolver is needed for inbound PollCreate events — non-PollCreate inbound events use the snapshot frozen on the poll's state at create time. Pattern: keep this resolver minimal-but-async (resolves snapshot at a specific HLC).

- [ ] **Step 1: Add trait to `community_voting_log.rs`**

Insert near the top of the file (after imports, before `pub struct VotingLog`):

```rust
/// Resolves the community's membership snapshot at a specific HLC,
/// used by `process_inbound` for PollCreate events (other event kinds
/// reuse the snapshot frozen on the poll's state). Production impl
/// reads from `community_registry` + `crdt_state` via NodeState; tests
/// use `FixedMembershipSnapshotResolver` with a fixed map.
#[async_trait::async_trait]
pub trait MembershipSnapshotResolver: Send + Sync {
    /// Resolve the per-community membership snapshot AT `hlc`. Returns
    /// `Err` if the community is not loaded locally (e.g., we never
    /// joined). Apply layer treats this as "reject the inbound event"
    /// rather than "accept anyway."
    async fn snapshot_at(
        &self,
        community_id: crate::owner_state_types::SpaceId,
        hlc: &crate::owner_state_types::Hlc,
    ) -> Result<crate::community_voting_core::MembershipSnapshot, SnapshotResolverError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotResolverError {
    #[error("community {0:?} not loaded locally")]
    CommunityNotLoaded(crate::owner_state_types::SpaceId),
    #[error("failed to read membership state: {0}")]
    BackendError(String),
}
```

- [ ] **Step 2: Add production impl to `lib.rs`**

Insert near `ensure_voting_engine_for` (around line 22134). The implementer should examine the existing pattern for resolving membership-at-HLC — `community_voting_log.rs::apply_with_snapshot` already does this for local creates via `community_registry` + `crdt_state`. Reuse that resolution logic:

```rust
/// Production `MembershipSnapshotResolver` that reads from the live
/// NodeState handles (community_registry + crdt_state).
pub struct NodeStateMembershipResolver {
    pub community_registry: std::sync::Arc<crate::community_registry::CommunityRegistry>,
    pub crdt_state: std::sync::Arc<tokio::sync::Mutex<crate::crdt_state::CrdtState>>,
}

#[async_trait::async_trait]
impl crate::community_voting_log::MembershipSnapshotResolver for NodeStateMembershipResolver {
    async fn snapshot_at(
        &self,
        community_id: crate::owner_state_types::SpaceId,
        hlc: &crate::owner_state_types::Hlc,
    ) -> Result<
        crate::community_voting_core::MembershipSnapshot,
        crate::community_voting_log::SnapshotResolverError,
    > {
        // Reuse the same resolution that voting_build_snapshot_for_community
        // uses for IPC-side eligibility checks. Implementer: examine the actual
        // signature in lib.rs and adapt — likely just calls
        // voting_build_snapshot_for_community + maps errors.
        voting_build_snapshot_for_community(
            self.crdt_state.clone(),
            self.community_registry.clone(),
            community_id,
        )
        .await
        .map_err(|e| {
            crate::community_voting_log::SnapshotResolverError::BackendError(e)
        })
    }
}
```

**IMPORTANT:** `voting_build_snapshot_for_community` may not take HLC currently; check the actual signature. If it builds an "at-HEAD" snapshot, that's fine for now (PollCreate from a peer would already-be-historical by the time we receive it). If HLC-precise resolution is needed, plumb the HLC through.

- [ ] **Step 3: cargo fmt + clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_voting_log.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-298+zeb-312): MembershipSnapshotResolver trait + production impl

Trait for resolving the per-community membership snapshot at a given
HLC — used by process_inbound for PollCreate events (non-PollCreate
events use the poll's frozen snapshot). Production impl reads via
voting_build_snapshot_for_community.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Refactor `process_inbound` — verify-then-apply + remove feature-gate

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (rewrite `process_inbound`; add resolver fields to engine + params)
- Create: `src-tauri/tests/community_voting_process_inbound_prod.rs` (production-build test — no `--features test-fixtures` flag)

This is the load-bearing task: it removes the production feature-gate that refuses peer events.

- [ ] **Step 1: Extend `VotingLogEngine` + `VotingLogEngineParams` with resolver fields**

In `community_voting_log_engine.rs`, add two new optional fields:

```rust
pub struct VotingLogEngineParams<R: tauri::Runtime> {
    // ... existing fields ...

    /// Production wiring (ZEB-298+ZEB-312 PR 1): resolves identity for
    /// Ed25519 signature verification on inbound events.
    pub identity_resolver: Option<std::sync::Arc<dyn crate::community_voting_core::VotingIdentityResolver>>,

    /// Production wiring: resolves per-community membership snapshot at
    /// an HLC for PollCreate inbound events.
    pub membership_resolver: Option<std::sync::Arc<dyn crate::community_voting_log::MembershipSnapshotResolver>>,
}

pub struct VotingLogEngine<R: tauri::Runtime> {
    // ... existing fields ...

    pub(crate) identity_resolver: Option<std::sync::Arc<dyn crate::community_voting_core::VotingIdentityResolver>>,
    pub(crate) membership_resolver: Option<std::sync::Arc<dyn crate::community_voting_log::MembershipSnapshotResolver>>,
}
```

Plumb `params.identity_resolver` and `params.membership_resolver` into engine construction at `VotingLogEngine::start`.

The existing inbound-loop spawn (around line 220-225) currently calls `Self::process_inbound(community_id, &log_for_loop, &tracker_for_loop, &packet)`. Update it to pass the resolvers (clone-Arc into the closure).

- [ ] **Step 2: Rewrite `process_inbound`**

Replace the body (lines 1401-1462 of community_voting_log_engine.rs):

```rust
async fn process_inbound(
    community_id: SpaceId,
    voting_log: &Arc<Mutex<VotingLog>>,
    tracker: &Arc<Mutex<VotingReplayTracker>>,
    identity_resolver: Option<&Arc<dyn crate::community_voting_core::VotingIdentityResolver>>,
    membership_resolver: Option<&Arc<dyn crate::community_voting_log::MembershipSnapshotResolver>>,
    packet: &[u8],
) -> Result<(), String> {
    // Decode.
    let event: SignedVotingEvent =
        ciborium::from_reader(packet).map_err(|e| format!("decode: {e}"))?;

    // Dedup gate.
    {
        let tracker = tracker.lock().await;
        if tracker.contains(&event) {
            // Self-loopback or peer redelivery; drop silently.
            return Ok(());
        }
    }

    // Resolve the membership snapshot — case-split on event kind:
    //   - PollCreate: build a fresh snapshot via membership_resolver.
    //   - All others: use the snapshot frozen on the poll's state at
    //     create time (cached, no fresh lookup needed).
    let snapshot = match event.kind {
        PollEventKindCode::PollCreate => {
            let resolver = membership_resolver.ok_or(
                "process_inbound: membership_resolver not installed (engine wiring incomplete)"
                    .to_string(),
            )?;
            resolver
                .snapshot_at(community_id, &event.hlc)
                .await
                .map_err(|e| format!("snapshot resolve: {e}"))?
        }
        _ => {
            // Non-PollCreate: look up the poll, use its frozen snapshot.
            let pid = derive_poll_id_for_event(&event);
            let log_g = voting_log.lock().await;
            let state = log_g
                .poll_state(&pid)
                .ok_or_else(|| format!("process_inbound: poll {} not found for non-PollCreate event", hex::encode(pid.0)))?;
            // Implementer: check the actual field on PollState that holds
            // the snapshot. For Tier 1 it's likely `tier1_snapshot`. For
            // Tier 3 it's `eligible_electorate_snapshot` (Vec<OwnerAddr>).
            // The function should return a MembershipSnapshot consistent
            // across both tiers — either store the full snapshot on every
            // PollState, OR re-resolve via membership_resolver using the
            // poll's create_hlc. Pick whatever's cleanest given the actual
            // struct layout.
            extract_snapshot_for_inbound(state)
                .map_err(|e| format!("process_inbound: snapshot for poll: {e}"))?
        }
    };

    // V6 + signature check via verify_voting_event.
    let identity_resolver = identity_resolver.ok_or(
        "process_inbound: identity_resolver not installed (engine wiring incomplete)".to_string(),
    )?;
    crate::community_voting_core::verify_voting_event(
        &event,
        &snapshot,
        identity_resolver.as_ref(),
    )
    .await
    .map_err(|e| format!("verify: {e}"))?;

    // Apply.
    {
        let mut log = voting_log.lock().await;
        log.apply_with_snapshot(event.clone(), &community_id, Some(snapshot))
            .map_err(|e| format!("apply: {e:?}"))?;
    }

    // Record AFTER successful apply on the inbound path.
    {
        let mut tracker = tracker.lock().await;
        tracker.record(&event);
    }

    Ok(())
}
```

**IMPORTANT:** `derive_poll_id_for_event` and `extract_snapshot_for_inbound` — these may not exist; implementer adapts to real struct layout. The `derive_poll_id` import already exists per `community_voting_log.rs:17`. For snapshot extraction from PollState, examine the actual field set: PollState carries `tier1_snapshot: Option<MembershipSnapshot>` (per voting_log.rs:65), so for non-PollCreate Tier 1 events use that. For Tier 3 events the snapshot is `eligible_electorate_snapshot: Vec<OwnerAddr>` — convert to MembershipSnapshot or change `verify_voting_event` to take a slim "is-member" check input.

**Pragmatic alternative:** if the snapshot shape mismatch is awkward, the resolver pattern can handle it: process_inbound ALWAYS calls `membership_resolver.snapshot_at(community_id, &event.hlc)` — fresh snapshot per event. The freshness cost is small (in-memory lookup); the snapshot-shape uniformity is worth it. Pick this if simpler.

**Remove the entire `#[cfg(not(any(test, feature = "test-fixtures")))]` block** at lines 1434-1442. With verify-then-apply in place, the gate is no longer needed.

- [ ] **Step 3: Update the receive-loop spawn site**

Around line 220-225 in `community_voting_log_engine.rs`, the existing spawn calls `Self::process_inbound(community_id, &log_for_loop, &tracker_for_loop, &packet)`. Update to pass clones of `identity_resolver` and `membership_resolver`:

```rust
let identity_resolver_loop = self_struct.identity_resolver.clone();
let membership_resolver_loop = self_struct.membership_resolver.clone();
// ... inside spawn:
Self::process_inbound(
    community_id,
    &log_for_loop,
    &tracker_for_loop,
    identity_resolver_loop.as_ref(),
    membership_resolver_loop.as_ref(),
    &packet,
).await
```

**IMPORTANT:** the resolver field is `Option<Arc<dyn Trait>>`; the call passes `Option<&Arc<dyn Trait>>` to process_inbound. Make sure the lifetimes work — Arcs need to live for the loop's duration. Easiest: clone the Arc into the loop's owned variables, then pass `.as_ref()`.

- [ ] **Step 4: Write production-build integration test**

Create `src-tauri/tests/community_voting_process_inbound_prod.rs`:

```rust
//! ZEB-298+ZEB-312 PR 1: production-build verification that the inbound
//! voting feature-gate is gone. This file is NOT under `cfg(any(test,
//! feature = "test-fixtures"))` — it must compile + pass under a vanilla
//! `cargo nextest run` invocation that DOES NOT pass `--features
//! test-fixtures`. That's the load-bearing assertion that the gate is
//! removed for production builds.
//!
//! Why this file instead of an inline #[test]: integration tests in
//! tests/* are compiled as separate crates against the LIB's public API,
//! so `cfg(test)` doesn't apply. Without `--features test-fixtures`,
//! this test exercises the exact same production code path a real
//! Zenoh-delivered peer event would hit.

use harmony_app::community_voting_core::{
    build_signed_poll_create_tier1, Eligibility, MembershipSnapshot, MemberAttrs,
    VotingIdentityResolver,
};
use harmony_app::community_voting_log::{MembershipSnapshotResolver, SnapshotResolverError};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use std::collections::HashMap;
use std::sync::Arc;
use async_trait::async_trait;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;

struct FixedResolvers {
    identity: HashMap<OwnerAddr, VerifyingKey>,
    snapshot: MembershipSnapshot,
}

#[async_trait]
impl VotingIdentityResolver for FixedResolvers {
    async fn verifying_key_for(&self, owner: &OwnerAddr) -> Option<VerifyingKey> {
        self.identity.get(owner).copied()
    }
}

#[async_trait]
impl MembershipSnapshotResolver for FixedResolvers {
    async fn snapshot_at(
        &self,
        _community_id: SpaceId,
        _hlc: &Hlc,
    ) -> Result<MembershipSnapshot, SnapshotResolverError> {
        Ok(self.snapshot.clone())
    }
}

#[tokio::test]
async fn process_inbound_peer_apply_succeeds_in_production_build() {
    // This test compiles WITHOUT `--features test-fixtures`. If the feature-gate
    // is still in place, process_inbound would short-circuit with the "refused
    // until ZEB-291 Task 19.1" error string and the assertion below fails.

    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0xaa; 16]);

    // Build a peer PollCreate event (Tier 1).
    let cfg = harmony_app::community_voting_approval::Tier1PollConfig {
        options: vec!["a".into(), "b".into()],
        window_seconds: 600,
        quorum: None,
        threshold_percent: None,
        multi_winner: None,
        eligibility: Eligibility { min_power: 0, min_vouching_depth: None, sortition_size: None },
        channel_id: harmony_app::community_membership::ChannelId([0xbb; 16]),
    };
    let hlc = Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: "peer".into() };
    let event = build_signed_poll_create_tier1(&keypair, actor, &cfg, hlc).expect("build");
    let mut packet = Vec::new();
    ciborium::ser::into_writer(&event, &mut packet).expect("encode");

    let community_id = SpaceId([0xcc; 16]);
    let members = HashMap::from([(actor, MemberAttrs { power: 1, vouching_depth: 1 })]);
    let resolvers = Arc::new(FixedResolvers {
        identity: HashMap::from([(actor, keypair.verifying_key())]),
        snapshot: MembershipSnapshot { members },
    });

    // Invoke process_inbound directly — note: it's currently a private associated
    // function on VotingLogEngine. We need a pub(crate) test seam, OR we go
    // through the full engine start() with a synthetic Zenoh packet. The former
    // is simpler; expose a pub fn `process_inbound_for_test` in the engine module
    // that delegates to the real one. Implementer: pick whichever shape compiles
    // cleanly under no-test-fixtures.

    let voting_log = Arc::new(tokio::sync::Mutex::new(
        harmony_app::community_voting_log::VotingLog::new(),
    ));
    let tracker = Arc::new(tokio::sync::Mutex::new(
        harmony_app::community_voting_log_engine::VotingReplayTracker::new(),
    ));

    let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
    let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();

    let result = harmony_app::community_voting_log_engine::process_inbound_for_test(
        community_id,
        &voting_log,
        &tracker,
        Some(&id_resolver),
        Some(&mem_resolver),
        &packet,
    )
    .await;

    assert!(result.is_ok(), "process_inbound should succeed under production build; got: {result:?}");

    // Verify the event was applied (log now contains the poll).
    let log = voting_log.lock().await;
    let derived_pid = harmony_app::community_voting_core::derive_poll_id(&event).expect("derive");
    assert!(log.has_poll(&derived_pid), "poll should be applied after inbound");
}
```

To support direct invocation from the integration test, add a thin `pub fn process_inbound_for_test` to `community_voting_log_engine.rs` that just calls the private associated function (this is a test seam, gated by neither cfg nor feature — it's safe because the function is well-encapsulated):

```rust
#[doc(hidden)]
pub async fn process_inbound_for_test(
    community_id: SpaceId,
    voting_log: &Arc<Mutex<VotingLog>>,
    tracker: &Arc<Mutex<VotingReplayTracker>>,
    identity_resolver: Option<&Arc<dyn crate::community_voting_core::VotingIdentityResolver>>,
    membership_resolver: Option<&Arc<dyn crate::community_voting_log::MembershipSnapshotResolver>>,
    packet: &[u8],
) -> Result<(), String> {
    VotingLogEngine::<tauri::Wry>::process_inbound(
        community_id,
        voting_log,
        tracker,
        identity_resolver,
        membership_resolver,
        packet,
    )
    .await
}
```

- [ ] **Step 5: Run the production-build test**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
# IMPORTANT: NO --features test-fixtures flag. This must pass under the
# production feature set.
cargo nextest run --locked --test community_voting_process_inbound_prod 2>&1 | tail -15
```

Expected: `process_inbound_peer_apply_succeeds_in_production_build` PASSES. If it fails with "inbound voting events are refused until ZEB-291 Task 19.1 ..." — the feature-gate is still in place; remove it.

- [ ] **Step 6: Full scope nextest with test-fixtures + fmt + clippy**

```bash
cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast 2>&1 | tail -15
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

Expected: 28 pre-existing orphans unchanged; ZEB-298+ZEB-312 contributions (new test added) pass.

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_voting_log_engine.rs src-tauri/tests/community_voting_process_inbound_prod.rs
git commit -m "$(cat <<'EOF'
feat(zeb-298+zeb-312): process_inbound verifies + applies; gate removed

Refactors VotingLogEngine::process_inbound to: resolve membership
snapshot (fresh for PollCreate via membership_resolver; cached for
non-PollCreate via the poll's frozen state), call verify_voting_event
with snapshot + identity_resolver, then apply. Adds Optional resolver
fields to VotingLogEngine + VotingLogEngineParams.

The #[cfg(not(any(test, feature = "test-fixtures")))] block that
unconditionally rejected inbound events in production builds is REMOVED.
Production-build integration test (no test-fixtures flag) verifies a
peer PollCreate event applies cleanly via process_inbound.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Zenoh outbound wiring (publisher_rx → session.put)

**Files:**
- Modify: `src-tauri/src/lib.rs` (replace the drain-task in `ensure_voting_engine_for`)

Mirror the DfrostLog adapter pattern. Look at `community_dfrost_log_engine.rs` for the existing Zenoh-publish forwarder.

- [ ] **Step 1: Read DfrostLog adapter pattern**

```bash
grep -nB2 -A20 "publisher_rx\|session.put\|harmony/community.*dfrost" src-tauri/src/community_dfrost_log_engine.rs | head -60
```

Find the pattern that forwards `publisher_rx` → `session.put` on the dfrost topic. This is the model.

- [ ] **Step 2: Replace `ensure_voting_engine_for`'s drain-task**

In `lib.rs` around lines 22163-22181, the current code:

```rust
let (publisher_tx, publisher_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
tokio::spawn(async move {
    let mut rx = publisher_rx;
    while rx.recv().await.is_some() {
        // Drop on the floor. TODO Task 19.1: forward to Zenoh adapter.
    }
});
```

Replace the spawn body to forward to Zenoh. Note: `ensure_voting_engine_for` will need a new `zenoh_session: Arc<zenoh::Session>` parameter (added in Task 5). For Task 4, write the forwarder body assuming the session is in scope:

```rust
let (publisher_tx, publisher_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);  // bump to 64 to match dfrost
let topic = format!("harmony/community/{}/voting", hex::encode(community_id.0));
let zenoh_session_handle = zenoh_session.clone();
tokio::spawn(async move {
    let mut rx = publisher_rx;
    while let Some(bytes) = rx.recv().await {
        if let Err(e) = zenoh_session_handle.put(&topic, bytes).await {
            tracing::warn!(error = %e, topic = %topic, "voting publisher → Zenoh put failed");
        }
    }
    tracing::debug!(topic = %topic, "voting outbound forwarder exiting (publisher_tx dropped)");
});
```

**IMPORTANT:** the exact Zenoh `.put()` call signature — implementer should examine `community_dfrost_log_engine.rs`'s real call. May use `session.put(&topic, &bytes).await` or `session.put(&topic).payload(bytes).await` depending on the zenoh-rs version.

- [ ] **Step 3: cargo fmt + clippy (likely fails until Task 5 adds the param)**

It's OK if this task's code doesn't compile yet — Task 5 adds the `zenoh_session` param. Commit per-task discipline says each task ends with a commit; if compile-fail, split this into the param-addition (Task 5) first. **Alternative:** combine Task 4 + Task 5 + Task 6 into one task. Let's go with that — Tasks 4+5+6 logically need to land together. See revised Task 4 below.

**RESTRUCTURE: Tasks 4+5+6 combined.**

---

## Task 4 (revised): `ensure_voting_engine_for` upgrade + Zenoh outbound + inbound wiring

**Files:**
- Modify: `src-tauri/src/lib.rs` (signature change + body rewrite)

This task lands all 3 changes that must land together: param signature, outbound forwarder, inbound subscriber.

- [ ] **Step 1: Add new params to `ensure_voting_engine_for`**

Current signature (lib.rs:22134):

```rust
async fn ensure_voting_engine_for(
    voting_logs: &VotingLogsMap,
    voting_log_engines: &VotingLogEnginesMap,
    community_id: crate::owner_state_types::SpaceId,
    dfrost_log_registry: Option<...>,
    beacon_requester: Option<...>,
) -> Result<(), String>
```

New signature:

```rust
async fn ensure_voting_engine_for(
    voting_logs: &VotingLogsMap,
    voting_log_engines: &VotingLogEnginesMap,
    community_id: crate::owner_state_types::SpaceId,
    // NEW for ZEB-298+ZEB-312 PR 1:
    zenoh_session: std::sync::Arc<zenoh::Session>,
    hlc_tracker: std::sync::Arc<crate::owner_state_types::HlcTracker>,
    device_id: std::sync::Arc<String>,
    app_handle: tauri::AppHandle<tauri::Wry>,
    local_signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    local_owner: crate::owner_state_types::OwnerAddr,
    identity_resolver: std::sync::Arc<dyn crate::community_voting_core::VotingIdentityResolver>,
    membership_resolver: std::sync::Arc<dyn crate::community_voting_log::MembershipSnapshotResolver>,
    // (existing dfrost params unchanged)
    dfrost_log_registry: Option<std::sync::Arc<crate::community_dfrost_log_engine::DfrostLogRegistry<tauri::Wry>>>,
    beacon_requester: Option<crate::community_voting_log_engine::BeaconRequester>,
) -> Result<(), String>
```

Update all call sites in `lib.rs` (grep for `ensure_voting_engine_for(`) — pass the new params from `NodeState` handles. Most likely call sites: `start_node`, the 6 Tier 3 IPCs, the Tier 2 IPCs. The NodeState already holds: `zenoh_session`, `hlc_tracker`, `dm_device_id`, `app_handle`, `dm_outbox.signing_key`, `dm_self_owner`, `community_registry`, `crdt_state`. Construct `NodeStateMembershipResolver { community_registry, crdt_state }` once and reuse.

- [ ] **Step 2: Replace publisher drain with Zenoh outbound**

```rust
let (publisher_tx, publisher_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
let outbound_topic = format!("harmony/community/{}/voting", hex::encode(community_id.0));
let zenoh_outbound = zenoh_session.clone();
let outbound_topic_for_loop = outbound_topic.clone();
tokio::spawn(async move {
    let mut rx = publisher_rx;
    while let Some(bytes) = rx.recv().await {
        if let Err(e) = zenoh_outbound.put(&outbound_topic_for_loop, bytes).await {
            tracing::warn!(
                error = %e,
                topic = %outbound_topic_for_loop,
                "voting publisher → Zenoh put failed"
            );
        }
    }
    tracing::debug!(
        topic = %outbound_topic_for_loop,
        "voting outbound forwarder exiting"
    );
});
```

- [ ] **Step 3: Replace inbound stub with Zenoh subscriber**

```rust
let (subscriber_tx, subscriber_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
let inbound_topic = outbound_topic.clone();
let zenoh_inbound = zenoh_session.clone();
tokio::spawn(async move {
    match zenoh_inbound.declare_subscriber(&inbound_topic).await {
        Ok(mut subscriber) => {
            // Implementer: examine the actual zenoh-rs subscriber API. May use
            // `.next().await`, `.recv().await`, or stream-style. Mirror DfrostLog
            // exactly — look at community_dfrost_log_engine.rs Zenoh subscriber
            // task for the canonical pattern.
            while let Ok(sample) = subscriber.recv_async().await {
                let payload_bytes: Vec<u8> = sample.payload().to_bytes().into_owned();
                if subscriber_tx.send(payload_bytes).await.is_err() {
                    tracing::debug!(
                        topic = %inbound_topic,
                        "voting subscriber → engine.subscriber_tx closed; exiting"
                    );
                    break;
                }
            }
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                topic = %inbound_topic,
                "voting Zenoh declare_subscriber failed"
            );
        }
    }
});
```

- [ ] **Step 4: Plumb all engine fields in `VotingLogEngineParams`**

```rust
let engine = crate::community_voting_log_engine::VotingLogEngine::start(
    crate::community_voting_log_engine::VotingLogEngineParams {
        community_id,
        voting_log: log_arc,
        publisher_tx,
        subscriber_rx,
        // NEW: all 4 previously-dormant fields are now installed.
        hlc_tracker: Some(hlc_tracker),
        device_id: Some(device_id),
        app_handle: Some(app_handle.clone()),
        identity_resolver: Some(identity_resolver),
        membership_resolver: Some(membership_resolver),
    },
).await;

// Install local signing key (ZEB-310 Task 9).
crate::community_voting_log_engine::VotingLogEngine::install_local_signing_key(
    &engine,
    local_signing_key,
    local_owner,
).await;
```

- [ ] **Step 5: Run nextest + fmt + clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast 2>&1 | tail -15
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

Expected: 28 pre-existing orphans unchanged; existing voting tests still pass (the engine is now fully wired, but Tier 1/2/3 IPCs still apply directly to log — that's PR 2's change).

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-298+zeb-312): ensure_voting_engine_for installs full engine wiring

Plumbs zenoh_session, hlc_tracker, device_id, app_handle, local_signing,
identity_resolver, membership_resolver. Replaces the publisher drain task
with a real Zenoh put forwarder on harmony/community/{id}/voting. Replaces
the closed-channel inbound stub with a real Zenoh subscriber.

All call sites of ensure_voting_engine_for updated to pass the new params
from NodeState handles. The voting engine is now production-active —
outbound mints reach Zenoh, inbound peer events arrive at subscriber_rx
and flow into process_inbound (which Task 3 already wired to verify-
then-apply).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Two-engine real-Zenoh integration test

**Files:**
- Create: `src-tauri/tests/community_voting_zenoh_integration.rs`

The mpsc test bridge from ZEB-309/310 verifies the engine logic but NOT the Zenoh transport. This new test exercises the full outbound→inbound loop through a real Zenoh session.

- [ ] **Step 1: Examine the DfrostLog Zenoh test pattern**

```bash
grep -nR "zenoh::Session\|zenoh::open\|peer_open" src-tauri/tests/ 2>&1 | head -10
```

Look for the existing pattern — `community_dfrost_transport_integration.rs` likely has the model.

- [ ] **Step 2: Write the test**

```rust
//! ZEB-298+ZEB-312 PR 1: end-to-end test of voting outbound→inbound
//! through a REAL Zenoh session (not the mpsc test bridge from
//! ZEB-309/310). Verifies that a peer-delivered voting event arrives
//! via the Zenoh subscriber, passes through verify_voting_event, and
//! applies on the receiving engine.

#![cfg(feature = "test-fixtures")]

use harmony_app::community_voting_core::{
    build_signed_poll_create_tier1, Eligibility, MembershipSnapshot, MemberAttrs,
    VotingIdentityResolver,
};
use harmony_app::community_voting_log::{MembershipSnapshotResolver, SnapshotResolverError};
use harmony_app::community_voting_log_engine::{VotingLogEngine, VotingLogEngineParams};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use std::collections::HashMap;
use std::sync::Arc;
use async_trait::async_trait;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;

struct FixedResolvers {
    identity: HashMap<OwnerAddr, VerifyingKey>,
    snapshot: MembershipSnapshot,
}

#[async_trait]
impl VotingIdentityResolver for FixedResolvers {
    async fn verifying_key_for(&self, owner: &OwnerAddr) -> Option<VerifyingKey> {
        self.identity.get(owner).copied()
    }
}

#[async_trait]
impl MembershipSnapshotResolver for FixedResolvers {
    async fn snapshot_at(&self, _: SpaceId, _: &Hlc) -> Result<MembershipSnapshot, SnapshotResolverError> {
        Ok(self.snapshot.clone())
    }
}

async fn open_peer_session() -> Arc<zenoh::Session> {
    let config = zenoh::Config::default();
    // Implementer: use the same peer-mode config as
    // community_dfrost_transport_integration.rs. Likely needs to set
    // mode=peer + ephemeral listen endpoints.
    Arc::new(
        zenoh::open(config).await.expect("zenoh::open"),
    )
}

#[tokio::test]
async fn voting_event_flows_through_real_zenoh() {
    // Skip on CI if Zenoh isn't reachable in this env (matching DfrostLog
    // transport-integration's skip pattern). Implementer: copy the skip
    // logic verbatim from community_dfrost_transport_integration.rs.

    let session_a = open_peer_session().await;
    let session_b = open_peer_session().await;
    // Give the two peers a moment to discover each other.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let community_id = SpaceId([0xab; 16]);
    let topic = format!("harmony/community/{}/voting", hex::encode(community_id.0));

    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0xcd; 16]);
    let resolvers = Arc::new(FixedResolvers {
        identity: HashMap::from([(actor, keypair.verifying_key())]),
        snapshot: MembershipSnapshot {
            members: HashMap::from([(actor, MemberAttrs { power: 1, vouching_depth: 1 })]),
        },
    });
    let id_resolver: Arc<dyn VotingIdentityResolver> = resolvers.clone();
    let mem_resolver: Arc<dyn MembershipSnapshotResolver> = resolvers.clone();

    // Spawn engine A (outbound) and engine B (inbound).
    let (a_pub_tx, a_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (b_sub_tx, b_sub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    // Engine A: forward publisher_rx → session_a.put.
    let session_a_outbound = session_a.clone();
    let topic_for_a = topic.clone();
    tokio::spawn(async move {
        let mut rx = a_pub_rx;
        while let Some(bytes) = rx.recv().await {
            session_a_outbound.put(&topic_for_a, bytes).await.expect("put");
        }
    });

    // Engine B: subscribe via session_b, forward to b_sub_tx.
    let session_b_inbound = session_b.clone();
    let topic_for_b = topic.clone();
    tokio::spawn(async move {
        let subscriber = session_b_inbound.declare_subscriber(&topic_for_b).await.expect("declare_subscriber");
        while let Ok(sample) = subscriber.recv_async().await {
            let payload: Vec<u8> = sample.payload().to_bytes().into_owned();
            if b_sub_tx.send(payload).await.is_err() { break; }
        }
    });

    // Engine A doesn't actually receive its own events here — we just
    // use its publisher_tx to push outbound. Build a stub engine_a if
    // needed, OR just call session_a.put(...) directly to bypass.

    // Engine B: full engine that processes inbound.
    let log_b = Arc::new(tokio::sync::Mutex::new(harmony_app::community_voting_log::VotingLog::new()));
    let (engine_b_pub_tx, _engine_b_pub_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let engine_b = VotingLogEngine::<tauri::test::MockRuntime>::start(VotingLogEngineParams {
        community_id,
        voting_log: log_b.clone(),
        publisher_tx: engine_b_pub_tx,
        subscriber_rx: b_sub_rx,
        hlc_tracker: None,
        device_id: None,
        app_handle: None,
        identity_resolver: Some(id_resolver),
        membership_resolver: Some(mem_resolver),
    }).await;

    // Mint a Tier 1 PollCreate, push it onto a_pub_tx (which goes to Zenoh).
    let cfg = harmony_app::community_voting_approval::Tier1PollConfig {
        options: vec!["a".into(), "b".into()],
        window_seconds: 600,
        quorum: None, threshold_percent: None, multi_winner: None,
        eligibility: Eligibility { min_power: 0, min_vouching_depth: None, sortition_size: None },
        channel_id: harmony_app::community_membership::ChannelId([0xef; 16]),
    };
    let event = build_signed_poll_create_tier1(
        &keypair,
        actor,
        &cfg,
        Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: "a".into() },
    ).expect("build");
    let mut packet = Vec::new();
    ciborium::ser::into_writer(&event, &mut packet).expect("encode");
    a_pub_tx.send(packet).await.expect("a_pub_tx send");

    // Wait for engine B to apply the event.
    let derived_pid = harmony_app::community_voting_core::derive_poll_id(&event).expect("derive");
    let pid = derived_pid;

    // Poll log_b for up to 3 seconds.
    let mut applied = false;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let log = log_b.lock().await;
        if log.has_poll(&pid) {
            applied = true;
            break;
        }
    }
    assert!(applied, "engine B should apply the peer event via Zenoh within 3s");

    drop(engine_b);
}
```

- [ ] **Step 3: Run the test**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures --test community_voting_zenoh_integration 2>&1 | tail -20
```

Expected: test passes. If it fails with "Zenoh unavailable", investigate the open-session config (probably needs ephemeral peer mode).

- [ ] **Step 4: cargo fmt + clippy + commit**

```bash
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cd ..
git add src-tauri/tests/community_voting_zenoh_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-298+zeb-312): two-engine real-Zenoh integration test

Peer voting event flows outbound→inbound via two distinct zenoh::Session
instances, passes through verify_voting_event on the receiving engine,
and applies. Verifies the full Zenoh adapter wiring beyond what the mpsc
test bridge can cover.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Final 5-gate sweep + push + PR creation

**Files:** none (verification + PR creation)

- [ ] **Step 1: Full 5-gate sweep**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast 2>&1 | tail -15
cd ..
npx tsc --noEmit
npx vitest run 2>&1 | tail -5
```

Expected:
- fmt: zero
- clippy: zero warnings
- nextest: all NEW tests pass + 28 pre-existing orphans unchanged
- tsc: zero
- vitest: 1921/1921 (no frontend changes in PR 1)

- [ ] **Step 2: Production-build test confirmation**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
# CRITICAL: this confirms the feature-gate is gone.
cargo nextest run --locked --test community_voting_process_inbound_prod 2>&1 | tail -10
```

Expected: 1 test passes WITHOUT `--features test-fixtures`.

- [ ] **Step 3: Sanity-check git state**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git log --oneline origin/main..HEAD
git diff --stat origin/main..HEAD
git status
```

Expected:
- 6-7 commits on top of `3739d72` (1 spec + 5-6 implementation)
- Working tree clean

- [ ] **Step 4: Push branch**

```bash
git push -u origin zeb-298-zeb-312-foundation
```

- [ ] **Step 5: Create PR**

```bash
gh pr create --title "ZEB-298+ZEB-312 foundation: production-wire voting engine (Zenoh adapter + verify_voting_event)" --body "$(cat <<'EOF'
## Summary

PR 1 of 2 for the combined ZEB-298 + ZEB-312 work. **Foundation only — does NOT close either ticket** (PR 2 carries the Closes lines).

Production-activates the voting engine. The Tier 3 IPCs still apply directly to VotingLog in this PR; PR 2 routes them through engine.publish_event so the engine-auto orchestration from ZEB-309/ZEB-310 fires from real user actions.

- Adds `VotingIdentityResolver` trait (OwnerAddr → VerifyingKey for signature check) + `MembershipSnapshotResolver` trait (snapshot-at-HLC for PollCreate)
- Adds `verify_voting_event` async function — membership check + Ed25519 signature verify
- Refactors `VotingLogEngine::process_inbound` to verify-then-apply with snapshot + identity resolver
- **Removes** the `#[cfg(not(any(test, feature = "test-fixtures")))]` block at `community_voting_log_engine.rs:1434-1442` that rejected ALL peer voting events in production builds
- Wires real Zenoh outbound (publisher_rx → `session.put` on `harmony/community/{id}/voting`) and inbound (Zenoh subscriber → `subscriber_tx`)
- Upgrades `ensure_voting_engine_for` to plumb 7 new params from NodeState handles: `zenoh_session`, `hlc_tracker`, `device_id`, `app_handle`, `local_signing_key`, `local_owner`, `identity_resolver`, `membership_resolver` — all 4 previously-dormant engine fields are now installed
- Production-build integration test (no \`--features test-fixtures\` flag) verifies a peer PollCreate applies via the inbound path
- Two-engine real-Zenoh integration test exercises the full outbound→inbound loop

## Test plan

- [x] \`cargo fmt --all -- --check\`
- [x] \`cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings\`
- [x] \`cargo nextest run --locked --workspace --all-targets --features test-fixtures\` — all green except 28 pre-existing orphans (ZEB-302/306/308)
- [x] \`cargo nextest run --locked --test community_voting_process_inbound_prod\` — passes in production build, proving the feature-gate is gone
- [x] \`npx tsc --noEmit\`
- [x] \`npx vitest run\` — 1921/1921 (no frontend changes in PR 1)

## References

- Spec: [\`docs/specs/2026-05-20-zeb-298-zeb-312-engine-production-wiring-design.md\`](docs/specs/2026-05-20-zeb-298-zeb-312-engine-production-wiring-design.md)
- Plan: [\`docs/plans/2026-05-20-zeb-298-zeb-312-foundation-plan.md\`](docs/plans/2026-05-20-zeb-298-zeb-312-foundation-plan.md)
- Related: [ZEB-298](https://linear.app/zeblith/issue/ZEB-298) (Tier 2 delegate-on-behalf surface — closed in PR 2)
- Related: [ZEB-312](https://linear.app/zeblith/issue/ZEB-312) (Tier 3 engine-auto production wiring — closed in PR 2)
- Pattern source: ZEB-307 PR #146 (DfrostLog Zenoh adapter)
- Removed gate: previously at \`src-tauri/src/community_voting_log_engine.rs:1434-1442\` (referenced as "ZEB-291 Task 19.1 follow-up" in the source comment)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 6: Capture PR URL**

```bash
gh pr view --json url -q '.url'
```

Return URL. Autonomous bot-review monitoring loop takes over.

---

## Self-review

**Spec coverage:** Each spec section maps to at least one task — verify_voting_event (Task 1), MembershipSnapshotResolver (Task 2), process_inbound + gate removal (Task 3), ensure_voting_engine_for upgrade + Zenoh adapter wiring (Task 4 revised), two-engine real-Zenoh test (Task 5), final gates + PR (Task 6).

**Placeholder scan:** Several spots flag "Implementer: examine the actual..." with concrete-enough guidance for the implementer to adapt. These are explicit invitations to look at real types — not laziness. Zenoh API specifics (e.g., `.put()` vs `.put().payload()`) genuinely depend on the zenoh-rs version pinned in Cargo.toml; the implementer should mirror the DfrostLog usage exactly.

**Type consistency:** trait name `VotingIdentityResolver` (Task 1) consistent through Tasks 3-5; `MembershipSnapshotResolver` (Task 2) consistent through Tasks 3-5; `VotingVerifyError` consistent; `SnapshotResolverError` consistent; function names `verify_voting_event`, `process_inbound`, `process_inbound_for_test`, `ensure_voting_engine_for` consistent across tasks.

**Known sharp edge:** Task 3's snapshot extraction for non-PollCreate events references `extract_snapshot_for_inbound(state)` — a function this plan defines abstractly. Implementer has two options: (a) implement that helper to fetch from the poll state's various snapshot fields (Tier 1's `tier1_snapshot`, Tier 3's `eligible_electorate_snapshot`, etc.); (b) just call `membership_resolver.snapshot_at(...)` for all event kinds (uniformly fresh). Pragmatic option (b) is simpler; trust the implementer to pick the simpler path that compiles.
