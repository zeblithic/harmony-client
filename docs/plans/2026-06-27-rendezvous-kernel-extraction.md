# Rendezvous Resolve Kernel → core `harmony-pkarr::rendezvous` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lift the generic DHT-rendezvous *resolve driver* out of `harmony-client`'s `community_rendezvous.rs` into core `harmony-pkarr::rendezvous`, so any P2P app shares one proven escalating-batch / first-responder-wins / hung-probe-immune resolver, and the client becomes a thin consumer.

**Architecture:** Two cross-repo PRs, **core merges first**. Phase 1 adds an async module to `harmony-pkarr` (mirroring the existing always-std `resolver.rs`), generic over the resolved payload `P`. Phase 2 bumps the client's `harmony-pkarr` git-rev and collapses `community_rendezvous.rs`'s driver onto the kernel, keeping the community-specific keying/info-layout/env-parsing client-side. The slot **keying** already lives in core (`derive_ephemeral_key`), so **no key bytes change** — the ZEB-570 cross-WAN integration suite must pass unchanged.

**Tech Stack:** Rust, `tokio` (rt/macros/sync/time — already an unconditional pkarr dep), `futures::FuturesUnordered`, `async-trait` (new pkarr dep; already a workspace dep), `tracing`. Test runner: `cargo nextest`. Source ticket: ZEB-579 (child of ZEB-571 Tier-1 #1). Design: `harmony-client/docs/specs/2026-06-26-rendezvous-extraction-to-core-design.md`.

## Global Constraints

- **Module, not a new crate:** `harmony-pkarr::rendezvous` (design open-decision 1, recommended).
- **Driver-only scope:** publisher slot-claim lifecycle (`refresh_slot`/`RendezvousSink`), `should_self_promote`, `RendezvousObservability` all stay client-side (deferred follow-ups).
- **Zero key bytes change:** the extraction moves only the driver; `derive_ephemeral_key`/`PkarrCase` are untouched.
- **Env-var config stays client-side:** `HARMONY_OPEN_JOIN_RESOLVE_*` names + clamping remain in the client; core `RendezvousResolveConfig` is pure (public fields, no env).
- **Merge order:** core PR (`harmony`) merges before the client rev-bump PR (`harmony-client`).
- **Keep ZEB IDs out of branch/commit/PR titles** (plain body reference only, no close-keyword — Linear auto-closes on keyword).
- **Core gates:** `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo nextest run --workspace`.
- **Client gates (5 CI jobs):** rust-check (fmt+clippy), rust-test (nextest `--all-targets --features test-fixtures`), msrv, frontend, CodeRabbit. `--all-targets` + `--locked` are load-bearing.
- **`RendezvousResolveOutcome<P>` must hand-impl `Default`** (a `#[derive(Default)]` would wrongly require `P: Default`).

## File Structure

**Phase 1 — `harmony` (core):**
- Create: `crates/harmony-pkarr/src/rendezvous.rs` — the generic kernel (trait, driver, config, outcome, pkarr-backed resolver, slot assignment) + its tests.
- Modify: `crates/harmony-pkarr/src/lib.rs` — `pub mod rendezvous;` + re-exports.
- Modify: `crates/harmony-pkarr/Cargo.toml` — add `async-trait` dependency.

**Phase 2 — `harmony-client`:**
- Modify: `src-tauri/Cargo.toml:109,204` — bump `harmony-pkarr` `rev` to the merged core SHA.
- Modify: `src-tauri/src/community_rendezvous.rs` — delete the driver (config/outcome/trait/`resolve_rendezvous_with`/`PkarrSlotResolver` + their tests); keep the community keying + `slot_for_advertiser` wrapper + `RENDEZVOUS_*` consts + their tests; rewrite `resolve_rendezvous` to delegate to the core kernel; add free fn `rendezvous_config_from_env()`.
- Modify: `src-tauri/src/open_join_dial.rs:24,106` — import + call-site update for the renamed config builder.

---

## Task 1: Core kernel — `harmony-pkarr::rendezvous` (Phase 1, core PR)

**Files:**
- Create: `crates/harmony-pkarr/src/rendezvous.rs`
- Modify: `crates/harmony-pkarr/src/lib.rs:25-37`
- Modify: `crates/harmony-pkarr/Cargo.toml:16-33`

**Interfaces:**
- Consumes (already in core): `derive::{derive_ephemeral_key, PkarrCase}` (PkarrCase: `Copy`), `epoch::epoch_tolerance_window(now_ms: u64) -> [u64; 3]`, `resolver::PkarrResolver::resolve(&vk) -> Result<Option<PkarrRoutingRecord>, _>`, `record::PkarrRoutingRecord{ pub routing_blob: Vec<u8>, pub fn verify_freshness(&self, now_ms: u64) -> Result<(), PkarrError> }`.
- Produces (used by Phase 2): `pub trait SlotResolver<P> { async fn resolve_slot(&self, slot_index: u16, epoch_id: u64) -> Option<P> }`; `pub async fn resolve_rendezvous_with<P, R: SlotResolver<P> + Sync>(resolver: &R, now_ms: u64, cfg: &RendezvousResolveConfig) -> RendezvousResolveOutcome<P>`; `pub struct RendezvousResolveConfig { pub batch_curve: Vec<usize>, pub per_batch_deadline: Duration }`; `pub struct RendezvousResolveOutcome<P> { pub payload: Option<P>, pub winning_slot: Option<u16>, pub elapsed_ms: u64, pub batches_tried: usize }`; `pub struct PkarrSlotResolver<P, F: Fn(&[u8]) -> Option<P>> { pub pkarr: Arc<PkarrResolver>, pub case: PkarrCase, pub ikm: Vec<u8>, pub info_for: Arc<dyn Fn(u16, u64) -> Vec<u8> + Send + Sync>, pub decode: F }`; `pub fn slot_for_advertiser<A: Ord + Copy>(advertisers: &[A], me: &A, cap: usize) -> Option<u16>`.

- [ ] **Step 1: Branch off clean core `origin/main`**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git fetch origin --prune
git checkout -b rendezvous-kernel-to-core origin/main   # origin/main = 3ef558f (#282)
```

- [ ] **Step 2: Add the `async-trait` dependency**

In `crates/harmony-pkarr/Cargo.toml`, under `[dependencies]` (after the `futures`/`pkarr` lines, ~line 31), add:

```toml
# async-trait: the rendezvous SlotResolver is an async trait (the rest of the
# crate uses inherent async fns; this is the one trait that needs it).
async-trait = { workspace = true }
```

(Confirm the root `harmony/Cargo.toml` `[workspace.dependencies]` table carries `async-trait = "0.1"`; if it lists it bare rather than in the workspace table, use `async-trait = "0.1"` here instead.)

- [ ] **Step 3: Write the kernel module (`crates/harmony-pkarr/src/rendezvous.rs`)**

```rust
//! Generic DHT-rendezvous resolve kernel.
//!
//! "Find a live serving peer for a topic via signed DHT slots derived from a
//! shared key." The subtle behaviors live here once — escalating concurrent
//! probe, first-responder-wins, hung-probe immunity, per-batch deadline, and
//! freshness re-sampled *after* the await — so every consumer (community
//! open-join, friend Case-D, …) shares one proven driver and supplies only its
//! own slot info-layout + payload decoder.
//!
//! The slot keying lives in [`crate::derive`]; this module is only the driver
//! on top, so a consumer migrating onto it changes **no key bytes**.

use crate::derive::{derive_ephemeral_key, PkarrCase};
use crate::epoch::epoch_tolerance_window;
use crate::resolver::PkarrResolver;
use std::sync::Arc;
use std::time::Duration;

/// Widening schedule + per-batch deadline for an escalating rendezvous resolve.
pub struct RendezvousResolveConfig {
    /// Widening curve of batch widths, e.g. `[1, 2, 4]`: probe slot 0, then
    /// slots 0..1, then slots 0..3. `[1]` is the degenerate single-slot resolve
    /// (the friend Case-D shape). Each width should already be clamped to the
    /// consumer's slot count — the driver probes `0..width` verbatim.
    pub batch_curve: Vec<usize>,
    /// Per-batch resolve deadline: on it elapsing, widen to the next batch
    /// rather than hanging on a slow/stuck probe.
    pub per_batch_deadline: Duration,
}

impl Default for RendezvousResolveConfig {
    /// A reasonable escalating default; consumers with a known slot count build
    /// an explicit curve (and own any env-var parsing).
    fn default() -> Self {
        Self {
            batch_curve: vec![1, 2, 4],
            per_batch_deadline: Duration::from_millis(2_500),
        }
    }
}

/// Result of an escalating-batch resolve, carrying the instrumentation a
/// consumer's tuning needs: which slot answered, how long it took, and how many
/// widening batches were probed.
#[derive(Debug)]
pub struct RendezvousResolveOutcome<P> {
    pub payload: Option<P>,
    pub winning_slot: Option<u16>,
    pub elapsed_ms: u64,
    pub batches_tried: usize,
}

// Hand-impl (NOT derive): a derived Default would wrongly require `P: Default`.
impl<P> Default for RendezvousResolveOutcome<P> {
    fn default() -> Self {
        Self {
            payload: None,
            winning_slot: None,
            elapsed_ms: 0,
            batches_tried: 0,
        }
    }
}

/// Probe one rendezvous slot at one epoch. Returns `Some` only for a live,
/// freshness-valid record decoded into `P`. The production impl
/// ([`PkarrSlotResolver`]) derives the slot verifying-key and queries pkarr;
/// tests inject a deterministic stub.
#[async_trait::async_trait]
pub trait SlotResolver<P> {
    async fn resolve_slot(&self, slot_index: u16, epoch_id: u64) -> Option<P>;
}

/// Escalating-batch rendezvous resolve over any [`SlotResolver`] (`now_ms` is
/// supplied so the driver stays clock-free apart from the per-batch deadline).
/// For each width `w` in `cfg.batch_curve`, probe slots `0..w` across the
/// epoch-tolerance window CONCURRENTLY and return on the FIRST live record — the
/// first slot to respond wins (not strictly the lowest), so one hung/slow probe
/// can never stall discovery. Each batch is bounded by `cfg.per_batch_deadline`:
/// on the deadline elapsing OR all probes returning `None`, widen to the next
/// width. Returns an empty outcome (cold start) if no slot answers.
pub async fn resolve_rendezvous_with<P, R: SlotResolver<P> + Sync>(
    resolver: &R,
    now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> RendezvousResolveOutcome<P> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let started = std::time::Instant::now();
    let epoch_window = epoch_tolerance_window(now_ms);
    let mut outcome = RendezvousResolveOutcome::default();

    for &width in &cfg.batch_curve {
        outcome.batches_tried += 1;
        // Probe every (slot, epoch) pair in this batch concurrently, draining
        // them as they complete so the FIRST live slot wins without waiting on
        // slower/hung probes. Bounded by the per-batch deadline.
        let mut probes: FuturesUnordered<_> = (0..width as u16)
            .flat_map(|slot| {
                epoch_window.iter().map(move |&epoch_id| async move {
                    resolver
                        .resolve_slot(slot, epoch_id)
                        .await
                        .map(|payload| (slot, payload))
                })
            })
            .collect();

        let winner = tokio::time::timeout(cfg.per_batch_deadline, async {
            while let Some(result) = probes.next().await {
                if let Some((slot, payload)) = result {
                    return Some((slot, payload));
                }
            }
            None
        })
        .await
        // On the batch deadline elapsing (Err), treat the batch as exhausted and
        // widen to the next width rather than hanging.
        .unwrap_or(None);

        if let Some((slot, payload)) = winner {
            outcome.winning_slot = Some(slot);
            outcome.payload = Some(payload);
            outcome.elapsed_ms = started.elapsed().as_millis() as u64;
            tracing::debug!(
                winning_slot = slot,
                elapsed_ms = outcome.elapsed_ms,
                batches_tried = outcome.batches_tried,
                "rendezvous resolved"
            );
            return outcome;
        }
    }

    outcome.elapsed_ms = started.elapsed().as_millis() as u64;
    tracing::debug!(
        elapsed_ms = outcome.elapsed_ms,
        batches_tried = outcome.batches_tried,
        "rendezvous resolve found no live slot (cold start)"
    );
    outcome
}

/// Production [`SlotResolver`]: derives the per-slot verifying-key from `ikm`
/// under `case` + the consumer's `info_for(slot, epoch)` layout, queries pkarr,
/// re-samples freshness AFTER the await, and decodes the routing blob into `P`.
/// The BEP44 envelope already proves the writer held the shared secret, so the
/// inner identity signature is intentionally NOT verified here — trust is
/// established at the consumer's handshake/admission layer.
pub struct PkarrSlotResolver<P, F>
where
    F: Fn(&[u8]) -> Option<P>,
{
    pub pkarr: Arc<PkarrResolver>,
    pub case: PkarrCase,
    pub ikm: Vec<u8>,
    pub info_for: Arc<dyn Fn(u16, u64) -> Vec<u8> + Send + Sync>,
    pub decode: F,
}

#[async_trait::async_trait]
impl<P, F> SlotResolver<P> for PkarrSlotResolver<P, F>
where
    P: Send,
    F: Fn(&[u8]) -> Option<P> + Send + Sync,
{
    async fn resolve_slot(&self, slot_index: u16, epoch_id: u64) -> Option<P> {
        let info = (self.info_for)(slot_index, epoch_id);
        let vk = derive_ephemeral_key(self.case, &self.ikm, &info).verifying_key();
        let rec = self.pkarr.resolve(&vk).await.ok()??;
        // Re-sample the wall clock AFTER the awaited resolve so freshness is
        // checked against "now", not a timestamp captured before a possibly long
        // network round-trip (the stale-clock bug fixed in PR#306).
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        rec.verify_freshness(now_ms).ok()?;
        (self.decode)(rec.routing_blob.as_slice())
    }
}

/// Deterministic slot claim: sort the advertiser set ascending, dedup, and
/// return the rank of `me` as its slot index — `None` if `me` is absent or ranks
/// at/beyond `cap`. Because the advertiser set is consumer-replicated (e.g. a
/// CRDT), every member computes the same ordering, so each slot has exactly one
/// writer.
pub fn slot_for_advertiser<A: Ord + Copy>(advertisers: &[A], me: &A, cap: usize) -> Option<u16> {
    let mut sorted: Vec<A> = advertisers.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let rank = sorted.iter().position(|a| a == me)?;
    if rank >= cap {
        return None;
    }
    Some(rank as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- slot_for_advertiser (generic over an Ord+Copy address) ---

    #[test]
    fn slot_assignment_is_deterministic_across_members() {
        // Two members compute the SAME ordering from the same (unordered) set.
        let set_a = [3u32, 1, 2];
        let set_b = [2u32, 3, 1];
        for who in [1u32, 2, 3] {
            assert_eq!(
                slot_for_advertiser(&set_a, &who, 4),
                slot_for_advertiser(&set_b, &who, 4),
                "ordering disagreed for {who}"
            );
        }
        assert_eq!(slot_for_advertiser(&set_a, &1, 4), Some(0));
        assert_eq!(slot_for_advertiser(&set_a, &2, 4), Some(1));
        assert_eq!(slot_for_advertiser(&set_a, &3, 4), Some(2));
    }

    #[test]
    fn not_in_set_returns_none() {
        assert_eq!(slot_for_advertiser(&[1u32, 2], &9, 4), None);
    }

    #[test]
    fn rank_beyond_cap_returns_none() {
        // cap=4 fills slots 0..3; a 5th (highest) ranks 4 >= cap → no slot.
        let set = [1u32, 2, 3, 4, 5];
        assert_eq!(slot_for_advertiser(&set, &5, 4), None);
        assert_eq!(slot_for_advertiser(&set, &4, 4), Some(3));
    }

    #[test]
    fn duplicate_addresses_do_not_shift_ranks() {
        let set = [1u32, 2, 2, 3];
        assert_eq!(slot_for_advertiser(&set, &1, 4), Some(0));
        assert_eq!(slot_for_advertiser(&set, &2, 4), Some(1));
        assert_eq!(slot_for_advertiser(&set, &3, 4), Some(2));
    }

    // --- escalating-batch driver (generic over a payload P) ---

    #[derive(Clone, Debug, PartialEq)]
    struct Beacon(u8);

    /// Deterministic resolver: answers only for one configured live slot (or
    /// never). Ignores `epoch_id` — the escalating-batch logic is under test,
    /// not epoch derivation.
    struct StubResolver {
        live_slot: Option<u16>,
    }

    #[async_trait::async_trait]
    impl SlotResolver<Beacon> for StubResolver {
        async fn resolve_slot(&self, slot_index: u16, _epoch_id: u64) -> Option<Beacon> {
            (Some(slot_index) == self.live_slot).then(|| Beacon(slot_index as u8))
        }
    }

    fn community_curve() -> RendezvousResolveConfig {
        RendezvousResolveConfig {
            batch_curve: vec![1, 2, 4],
            per_batch_deadline: Duration::from_millis(2_500),
        }
    }

    #[tokio::test]
    async fn returns_slot0_without_widening_when_slot0_is_live() {
        let stub = StubResolver { live_slot: Some(0) };
        let out = resolve_rendezvous_with(&stub, 1_000_000, &community_curve()).await;
        assert_eq!(out.winning_slot, Some(0));
        assert_eq!(out.batches_tried, 1, "should not widen past the first batch");
        assert_eq!(out.payload, Some(Beacon(0)));
    }

    #[tokio::test]
    async fn widens_to_find_a_live_slot_when_slot0_is_dead() {
        let stub = StubResolver { live_slot: Some(2) }; // only slot 2 answers
        let out = resolve_rendezvous_with(&stub, 1_000_000, &community_curve()).await;
        assert_eq!(out.winning_slot, Some(2));
        assert!(out.batches_tried >= 3, "had to widen to the full set");
        assert_eq!(out.payload, Some(Beacon(2)));
    }

    #[tokio::test]
    async fn cold_start_returns_none() {
        let stub = StubResolver { live_slot: None };
        let out = resolve_rendezvous_with(&stub, 1_000_000, &community_curve()).await;
        assert_eq!(out.payload, None);
        assert_eq!(out.winning_slot, None);
    }

    /// Resolver whose slot 0 NEVER completes (hangs) but whose slot 1 answers
    /// live — proves the resolve returns the first-responding slot and never
    /// blocks on the hung probe.
    struct HungSlot0Resolver;

    #[async_trait::async_trait]
    impl SlotResolver<Beacon> for HungSlot0Resolver {
        async fn resolve_slot(&self, slot_index: u16, _epoch_id: u64) -> Option<Beacon> {
            if slot_index == 0 {
                std::future::pending::<()>().await; // models a hung/dropped probe
                unreachable!("pending() never resolves");
            }
            (slot_index == 1).then(|| Beacon(1))
        }
    }

    #[tokio::test]
    async fn hung_probe_does_not_block_a_live_higher_slot() {
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            resolve_rendezvous_with(&HungSlot0Resolver, 1_000_000, &community_curve()),
        )
        .await
        .expect("resolve must not hang on a stuck slot-0 probe");
        assert_eq!(out.winning_slot, Some(1));
        assert_eq!(out.payload, Some(Beacon(1)));
    }

    /// Friend Case-D shape: a single-slot (`[1]`) curve resolves slot 0 — proves
    /// the kernel fits the degenerate N=1 consumer, not just the community
    /// N-slot one, and never probes a slot outside the curve width.
    #[tokio::test]
    async fn friend_shape_single_slot_curve_resolves() {
        let cfg = RendezvousResolveConfig {
            batch_curve: vec![1],
            per_batch_deadline: Duration::from_millis(2_500),
        };
        let live = StubResolver { live_slot: Some(0) };
        let out = resolve_rendezvous_with(&live, 1_000_000, &cfg).await;
        assert_eq!(out.winning_slot, Some(0));
        assert_eq!(out.batches_tried, 1);
        assert_eq!(out.payload, Some(Beacon(0)));
        // A single-slot curve must NOT probe slot 1 even if slot 1 is "live".
        let only_slot1 = StubResolver { live_slot: Some(1) };
        let out2 = resolve_rendezvous_with(&only_slot1, 1_000_000, &cfg).await;
        assert_eq!(out2.payload, None, "single-slot curve must not probe slot 1");
    }
}
```

- [ ] **Step 4: Wire the module into `lib.rs`**

In `crates/harmony-pkarr/src/lib.rs`, add `pub mod rendezvous;` (keep rough alpha order — after `pub mod record;`/`pub mod relay;`, before `pub mod resolver;`):

```rust
pub mod record;
pub mod relay;
pub mod rendezvous;
pub mod resolver;
```

And after the existing `pub use resolver::PkarrResolver;` re-export, add:

```rust
pub use rendezvous::{
    resolve_rendezvous_with, slot_for_advertiser, PkarrSlotResolver, RendezvousResolveConfig,
    RendezvousResolveOutcome, SlotResolver,
};
```

- [ ] **Step 5: Build + run the new tests (verify they pass)**

```bash
cd /Users/zeblith/work/zeblithic/harmony
cargo nextest run -p harmony-pkarr 2>&1 | tail -25
```

Expected: the 9 new `rendezvous::tests` cases pass (`slot_assignment_is_deterministic_across_members`, `not_in_set_returns_none`, `rank_beyond_cap_returns_none`, `duplicate_addresses_do_not_shift_ranks`, `returns_slot0_without_widening_when_slot0_is_live`, `widens_to_find_a_live_slot_when_slot0_is_dead`, `cold_start_returns_none`, `hung_probe_does_not_block_a_live_higher_slot`, `friend_shape_single_slot_curve_resolves`) alongside the pre-existing pkarr suite, 0 failures.

- [ ] **Step 6: Gate — fmt + clippy + full workspace tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony
cargo fmt --all -- --check
cargo clippy -p harmony-pkarr --all-targets -- -D warnings
cargo nextest run -p harmony-pkarr
```

Expected: fmt clean, clippy 0 warnings, all pkarr tests green. (Run the full `cargo nextest run --workspace` as the final pre-PR sweep if quick; the new module has no downstream core consumers yet, so a crate-scoped run is sufficient for correctness.)

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git add crates/harmony-pkarr/src/rendezvous.rs crates/harmony-pkarr/src/lib.rs crates/harmony-pkarr/Cargo.toml Cargo.lock
git commit  # message below
```

Commit message:
```
feat(pkarr): generic DHT-rendezvous resolve kernel (harmony-pkarr::rendezvous)

Lift the escalating-batch / first-responder-wins / hung-probe-immune
rendezvous resolve driver out of harmony-client's community_rendezvous.rs
into core, generic over the resolved payload P. Consumers supply a
SlotResolver<P> (or use PkarrSlotResolver with their own info-layout +
decoder); the driver owns the subtle concurrency: probe slots 0..w across
the epoch-tolerance window concurrently, return on the first live record,
widen on the per-batch deadline, never block on a hung probe. Slot keying
stays in derive::derive_ephemeral_key — no key bytes change.

Also adds slot_for_advertiser<A: Ord + Copy> (deterministic rank-based slot
claim) and an async-trait dependency (the one async trait in the crate).

Tests cover slot assignment, no-widen-on-slot0, widening, cold start, hung-
probe immunity, and a friend-shape single-slot ([1]) resolve proving the
kernel fits the degenerate N=1 consumer.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
```

- [ ] **Step 8: Push + open the core PR**

```bash
cd /Users/zeblith/work/zeblithic/harmony
git push -u origin rendezvous-kernel-to-core
gh pr create --repo zeblithic/harmony --title "feat(pkarr): generic DHT-rendezvous resolve kernel" --body "<body — see plan; reference ZEB-579 in body, NO close-keyword>"
```

PR body references ZEB-579 (plain, no close-keyword), the design doc path, and notes this is Phase 1 of 2 (client rev-bump follows). Then run the bot/CI loop (CodeRabbit manual-trigger after push; converge Qodo/CodeAnt). **Jake is the merge gate.**

---

## Task 2: Client collapse onto the kernel (Phase 2, client PR — gated on Task 1 merge)

> Develop against the Task-1 branch commit SHA, but the **final** client PR must pin the **merged** core main SHA. Do not merge the client PR before the core PR.

**Files:**
- Modify: `src-tauri/Cargo.toml:109,204`
- Modify: `src-tauri/src/community_rendezvous.rs`
- Modify: `src-tauri/src/open_join_dial.rs:24,106`

**Interfaces:**
- Consumes: the Task-1 `harmony_pkarr::rendezvous::*` surface.
- Produces (unchanged signatures, so callers outside `community_rendezvous.rs` are minimally touched): `community_rendezvous::resolve_rendezvous(pkarr, epoch_key, now_ms, cfg) -> harmony_pkarr::rendezvous::RendezvousResolveOutcome<ReachabilityAnnouncePayload>`; `community_rendezvous::rendezvous_config_from_env() -> harmony_pkarr::rendezvous::RendezvousResolveConfig`; `community_rendezvous::slot_for_advertiser(&[OwnerAddr], &OwnerAddr) -> Option<u16>` (kept 2-arg wrapper); `RENDEZVOUS_SLOT_COUNT`, `RENDEZVOUS_INFO_PREFIX`, `rendezvous_slot_key`, `rendezvous_slot_verifying_key` (kept).

- [ ] **Step 1: Branch off client `origin/main` + pin pkarr to the Task-1 branch SHA**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git fetch origin --prune
git checkout -b rendezvous-kernel-client-rev-bump origin/main   # d13105f0
```

In `src-tauri/Cargo.toml`, set BOTH `harmony-pkarr` lines (109 and 204) `rev` to the Task-1 branch HEAD SHA (temporary, for development). Replace with the merged core main SHA before the PR is marked ready.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri && cargo update -p harmony-pkarr
```

- [ ] **Step 2: Rewrite `community_rendezvous.rs` to delegate to the kernel**

Replace the driver block (the `RendezvousResolveConfig` struct/Default/`from_env`, `RendezvousResolveOutcome`, `SlotResolver` trait, `resolve_rendezvous_with`, `PkarrSlotResolver`, the production `resolve_rendezvous`, and the driver `#[cfg(test)]` cases: `returns_slot0_*`, `widens_*`, `cold_start_*`, `hung_probe_*`, plus the `StubResolver`/`HungSlot0Resolver`/`dummy_payload` helpers) with the thin community adapter below. **Keep** `RENDEZVOUS_SLOT_COUNT`, `RENDEZVOUS_INFO_PREFIX`, `rendezvous_info`, `rendezvous_slot_key`, `rendezvous_slot_verifying_key`, the community `slot_for_advertiser` wrapper, and the keying/disjointness/slot-assignment tests (`slot_key_is_deterministic`, `distinct_slots_and_epochs_give_distinct_keys`, `rendezvous_key_is_disjoint_from_member_keyed_record`, `slot_count_tracks_advertiser_cap`, `slot_assignment_is_deterministic_across_members`, `not_in_set_returns_none`, `rank_beyond_cap_returns_none`, `duplicate_addresses_do_not_shift_ranks`).

Updated imports (top of file):

```rust
use crate::community_relay_announce::COMMUNITY_RELAY_ADVERTISERS_MAX;
use crate::owner_state_types::EpochKey;
use crate::owner_state_types::OwnerAddr;
use crate::reachability_record::ReachabilityAnnouncePayload;
use harmony_pkarr::derive::{derive_ephemeral_key, PkarrCase};
use harmony_pkarr::rendezvous::{
    resolve_rendezvous_with, slot_for_advertiser as core_slot_for_advertiser, PkarrSlotResolver,
    RendezvousResolveConfig, RendezvousResolveOutcome,
};
use harmony_pkarr::PkarrResolver;
use std::sync::Arc;
use std::time::Duration;
```

Community `slot_for_advertiser` wrapper (replaces the hand-rolled body; delegates to core over the 16-byte address, keeps the 2-arg call sites in `community_rendezvous_publisher.rs` working):

```rust
/// Deterministic slot claim for the community advertiser set (the slot cap is
/// `RENDEZVOUS_SLOT_COUNT`). Thin wrapper over the generic core kernel keyed by
/// the 16-byte owner address.
pub fn slot_for_advertiser(advertisers: &[OwnerAddr], me: &OwnerAddr) -> Option<u16> {
    let addrs: Vec<[u8; 16]> = advertisers.iter().map(|a| a.0).collect();
    core_slot_for_advertiser(&addrs, &me.0, RENDEZVOUS_SLOT_COUNT)
}
```

Env-config builder (free fn — the type is now foreign, so it cannot be an inherent `impl`; keeps the `HARMONY_OPEN_JOIN_RESOLVE_*` names + `1..=RENDEZVOUS_SLOT_COUNT` clamp):

```rust
/// Build the core rendezvous resolve config from the open-join env knobs.
/// `HARMONY_OPEN_JOIN_RESOLVE_CURVE` (comma-separated batch widths, each clamped
/// to `1..=RENDEZVOUS_SLOT_COUNT`) and `HARMONY_OPEN_JOIN_RESOLVE_DEADLINE_MS`
/// (clamped `>= 1`). Defaults to the `[1, 2, N]` widening curve at 2500ms.
pub fn rendezvous_config_from_env() -> RendezvousResolveConfig {
    let mut cfg = RendezvousResolveConfig {
        batch_curve: vec![1, 2, RENDEZVOUS_SLOT_COUNT],
        per_batch_deadline: Duration::from_millis(2_500),
    };
    if let Ok(curve) = std::env::var("HARMONY_OPEN_JOIN_RESOLVE_CURVE") {
        // CLAMP each width to 1..=RENDEZVOUS_SLOT_COUNT rather than dropping
        // out-of-range entries: a user passing `8` with N=4 wants a full-width
        // batch, not a silently-dropped curve step.
        let parsed: Vec<usize> = curve
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .map(|w| w.clamp(1, RENDEZVOUS_SLOT_COUNT))
            .collect();
        if !parsed.is_empty() {
            cfg.batch_curve = parsed;
        }
    }
    if let Ok(ms) = std::env::var("HARMONY_OPEN_JOIN_RESOLVE_DEADLINE_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            cfg.per_batch_deadline = Duration::from_millis(ms.max(1));
        }
    }
    cfg
}
```

Production `resolve_rendezvous` (builds a core `PkarrSlotResolver` over the community keying + `ReachabilityAnnouncePayload` decoder, then runs the kernel):

```rust
/// Production entry point: resolve a live community beacon from the DHT via the
/// generic kernel, keyed by `PkarrCase::Community` + the community rendezvous
/// info-layout, decoding the routing blob as a `ReachabilityAnnouncePayload`.
pub async fn resolve_rendezvous(
    pkarr: &Arc<PkarrResolver>,
    epoch_key: &EpochKey,
    now_ms: u64,
    cfg: &RendezvousResolveConfig,
) -> RendezvousResolveOutcome<ReachabilityAnnouncePayload> {
    let resolver = PkarrSlotResolver {
        pkarr: Arc::clone(pkarr),
        case: PkarrCase::Community,
        ikm: epoch_key.as_bytes().to_vec(),
        info_for: Arc::new(|slot, epoch| rendezvous_info(slot, epoch)),
        decode: |blob: &[u8]| {
            ciborium::from_reader::<ReachabilityAnnouncePayload, _>(blob).ok()
        },
    };
    resolve_rendezvous_with(&resolver, now_ms, cfg).await
}
```

(`derive_ephemeral_key` import stays only if still used by `rendezvous_slot_key`; it is. Keep it.)

- [ ] **Step 3: Update the one external call site (`open_join_dial.rs`)**

Line 24 import:
```rust
use crate::community_rendezvous::{rendezvous_config_from_env, resolve_rendezvous};
```
Line ~106 call (inside the `resolve_rendezvous(...)` args):
```rust
        &rendezvous_config_from_env(),
```

- [ ] **Step 4: Build + scope-test the community surface**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(rendezvous) | test(open_join) | test(community_rendezvous)' 2>&1 | tail -25
```

Expected: the kept community keying/slot tests pass; `open_join_dial` + publisher compile against the new symbols.

- [ ] **Step 5: Run the ZEB-570 cross-WAN integration suite unchanged (the end-to-end proof)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(community_open_join_cross_wan)' 2>&1 | tail -25
```

Expected: PASS unchanged — the behavior is identical because the keys and the driver semantics are preserved.

- [ ] **Step 6: Full gate**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -25
```

Expected: fmt clean, clippy 0 warnings, full suite green.

- [ ] **Step 7: Re-pin to the merged core SHA (after Task 1's core PR merges), commit, push, PR**

Once the core PR is merged, set both `harmony-pkarr` `rev` lines to the merged `origin/main` SHA of `harmony`, `cargo update -p harmony-pkarr`, re-run Step 6, then:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add -A
git commit  # see message below
git push -u origin rendezvous-kernel-client-rev-bump
gh pr create --repo zeblithic/harmony-client --title "refactor(rendezvous): collapse community resolver onto harmony-pkarr kernel" --body "<body referencing ZEB-579, NO close-keyword, notes core PR dependency>"
```

Commit message:
```
refactor(rendezvous): collapse the community resolver onto the core kernel

Bump harmony-pkarr to the rev carrying the generic rendezvous kernel and
reduce community_rendezvous.rs to the community-specific parts: the slot
info-layout (RENDEZVOUS_INFO_PREFIX), the ReachabilityAnnouncePayload
decoder, the env-knob config builder, and a thin slot_for_advertiser
wrapper. The escalating-batch driver, SlotResolver trait, and
PkarrSlotResolver now come from harmony_pkarr::rendezvous. No key bytes
change; the ZEB-570 cross-WAN open-join integration suite passes unchanged.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_012opk5CDiSc47nBcxaADfsX
```

Then run the bot/CI loop. **Jake is the merge gate.**

---

## Self-Review

**1. Spec coverage** (design `2026-06-26-rendezvous-extraction-to-core-design.md`):
- "What to extract" — `SlotResolver<P>`, `resolve_rendezvous_with`, config, outcome, `PkarrSlotResolver<P,F>`, `slot_for_advertiser` → Task 1 Step 3. ✓
- "What stays app-specific" — `RENDEZVOUS_INFO_PREFIX`/info-layout, advertiser-set source, publisher lifecycle, self-promotion, payload types → kept client-side (Task 2 Step 2; publisher/self-promo untouched). ✓
- Invariants pinned by tests — key bytes (the kept community keying tests + unchanged `derive_ephemeral_key`), first-responder-wins, hung-probe, per-batch widen, freshness-after-await, friend-shape → Task 1 tests + Task 2 Step 5 integration. ✓
- Phasing core-first → Task 1 then Task 2; merge order constraint stated. ✓
- Crate-vs-module (module), scope-now (driver only), friend (fit + proving test now), env (client-side) → Global Constraints + Task 1/2. ✓

**2. Placeholder scan:** All code steps carry complete code; the only deferred literal is the core-merged SHA in Task 2 Step 7 (genuinely unknowable until the core PR merges — flagged as a re-pin step, not a placeholder). No TBD/TODO/"handle errors" left.

**3. Type consistency:** `RendezvousResolveOutcome<P>` (Task 1) ← `resolve_rendezvous` returns `RendezvousResolveOutcome<ReachabilityAnnouncePayload>` (Task 2) ✓. `slot_for_advertiser<A: Ord+Copy>(_, _, cap)` (core, 3-arg) ← community wrapper `slot_for_advertiser(&[OwnerAddr], &OwnerAddr)` (2-arg) delegates with `RENDEZVOUS_SLOT_COUNT` ✓. `rendezvous_config_from_env() -> RendezvousResolveConfig` ← `open_join_dial.rs` call ✓. `PkarrSlotResolver{ case, ikm, info_for, decode }` field names match between definition (Task 1) and construction (Task 2) ✓.
