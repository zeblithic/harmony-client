# ZEB-291 Phase 2 (folded) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. This is a **roadmap-style plan** — task summaries describe what each subagent must build, but the implementer subagent is expected to write its own TDD ceremony (failing test → run → impl → run → commit) following the patterns from ZEB-290 Phase 1. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship Tier 2 Conviction voting + the Phase 1.5 deferrals from ZEB-290 (Zenoh sync engine, two-engine convergence tests, auto-close tick, daily archive sweep tick, chat-native poll dispatch).

**Architecture:** Three new Rust modules (`community_voting_conviction.rs`, `community_voting_log_engine.rs`, `community_voting_tick.rs`) + extensions to `community_voting_core.rs` + `community_voting_log.rs` + `community_membership.rs`. Two new Svelte components (`CommunityProposalsPanel.svelte`, `ConvictionProposalCard.svelte`). New wire-format variants for Signal/Delegate/Undelegate + `ChannelMessage::Poll`. Fixed-point i128 (Q96.32) conviction math for cross-engine determinism.

**Tech Stack:** Rust 1.85 / Tauri 2 / Svelte 5 / CBOR (ciborium) / Ed25519 / HLC / Zenoh (via existing engine pattern from ZEB-270).

---

## File structure

### New Rust files
- `src-tauri/src/community_voting_conviction.rs` — Tier 2 mechanism: types, fixed-point math, DelegationGraph CRDT, Tier2ProposalState, threshold compute
- `src-tauri/src/community_voting_log_engine.rs` — Zenoh broadcast + queryable backfill for `harmony/community/{id}/voting`, mirrors `community_channel_log_engine.rs` (~1000 LOC target)
- `src-tauri/src/community_voting_tick.rs` — periodic tick: Tier 1 auto-close, Tier 2 threshold-crossing detection + reversion, Tier 2 contestability finalize, daily archive sweep, auto-exec dispatch

### New Rust test files
- `src-tauri/tests/wire_format_zeb291_fixtures.rs` — CBOR fixture pinning for Signal/Delegate/Undelegate envelopes (regen-on-first-run pattern from ZEB-290)
- `src-tauri/tests/community_voting_tier1_two_engine.rs` — the missing ZEB-290 Task 15; two-engine ballot convergence
- `src-tauri/tests/community_voting_tier2_two_engine.rs` — two-engine Tier 2 conviction convergence with delegation
- `src-tauri/tests/community_voting_tier2_lifecycle_integration.rs` — full lifecycle: create → signal → threshold → 24h → finalize → auto-exec

### Modified Rust files
- `src-tauri/src/community_voting_core.rs` — add `Tier::Conviction = 2` (already in enum); add `PollEventKindCode::Signal/Delegate/Undelegate` (`sg`/`dg`/`ud`); add `Lifecycle::ThresholdReached`; extend `next_lifecycle` state machine; add `build_signed_poll_create_tier2`, `build_signed_signal`, `build_signed_delegate`, `build_signed_undelegate` helpers
- `src-tauri/src/community_voting_log.rs` — extend `TierState` enum with `Tier2(Tier2ProposalState)` variant; extend `apply_with_snapshot` to dispatch `sg`/`dg`/`ud` to Tier 2 apply path; preserve archive_sweep behavior for new `ThresholdReached` lifecycle
- `src-tauri/src/community_voting_approval.rs` — minor: ensure `archive_finalized_polls` semantics composed correctly with the tick (no per-tier coupling)
- `src-tauri/src/community_membership.rs` — add `apply_auto_exec_set_power(community_id, target_pubkey, new_power) -> Result<(), String>` — signs a SetPower membership event using the node's signing key and applies via existing CommunityStateCrdt path
- `src-tauri/src/lib.rs` — 6 new Tier 2 IPCs; 4 new Tauri events (`voting-tier2-proposal-created`, `voting-tier2-signal-cast`, `voting-threshold-reached`, `voting-proposal-finalized`); `voting-poll-closed` actually fires from tick; add `voting_log_engines` field to `NodeState`; wire engine registry into `start_node`/`stop_node`; wire `spawn_voting_tick` into start/stop; `voting_create_tier1_poll` also emits a poll-kind chat message in the host channel
- `src-tauri/Cargo.toml` — no new deps (all required crates already in workspace)
- `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` — §5 amendment from f64 pseudocode to fixed-point i128 Q96.32

### New / modified frontend files
- `src/lib/types/voting.ts` — Tier 2 types: `Tier2PollConfig`, `ProposalState`, `ConvictionDelta`, `AutoExecAction`, `convictionPercent` helper
- `src/lib/voting-adapter.ts` — 6 Tier 2 IPC wrappers; 4 new event subscribers (`subscribeProposalCreated`, `subscribeSignalCast`, `subscribeThresholdReached`, `subscribeProposalFinalized`); `subscribePollClosed` now receives real events
- `src/lib/components/CommunityProposalsPanel.svelte` — NEW; Tier 2 governance area (proposal list + new-proposal form)
- `src/lib/components/ConvictionProposalCard.svelte` — NEW; per-proposal card with conviction bar vs threshold, signal toggle, lifecycle badge
- `src/lib/components/CommunityView.svelte` — wire `votingAdapter` prop + Proposals tab
- `src/lib/components/ChannelMessageFeed.svelte` — fill Phase 1.5 seam at line 272; dispatch `kind === 'poll'` messages to `PollMessage.svelte`
- `src/lib/channel-message-service.ts` — extend `ChannelMessageDto` with optional `kind` + `pollId` discriminator
- `src/lib/components/__tests__/CommunityProposalsPanel.test.ts` — vitest
- `src/lib/components/__tests__/ConvictionProposalCard.test.ts` — vitest

---

## Open design decisions (baked in — user has reviewed)

### D1. Conviction precision: fixed-point i128 (Q96.32), NOT f64

Spec §5 pseudocode uses f64; cross-engine convergence (acceptance criterion #2) requires bit-identical state across x86/ARM, and f64 is non-deterministic across architectures (FMA hints, subnormal handling differ). Task 1 amends spec §5 to fixed-point.

Constants:
- `LN2_Q32 = 2_977_044_472` — `ln(2) * 2^32`, rounded up from `2_977_044_471.53`
- `CONVICTION_FRAC_BITS = 32` — 96 integer bits, 32 fractional bits
- All conviction values stored as `i128` with implicit `/ 2^32` factor

Charge function `charge(d, hl) = (1 - 0.5^(d/hl)) * hl / ln(2)` implemented as Taylor series for `2^-(d/hl)` with 7 terms (sufficient for `x ≤ 100` to error < 1e-9). Decay function `decay(c, dt, hl) = c * 0.5^(dt/hl)` via the same Taylor series.

### D2. Zenoh sync pattern: copy `community_channel_log_engine.rs` verbatim

Engine never touches `zenoh::Session` directly — mpsc-channel split with the adapter (`publisher_tx`, `subscriber_rx`, `backfill_req_tx`). The voting engine has the same shape: per-(community) engine task, publish-and-locally-apply ordering, queryable backfill on new-community-join.

Self-loopback race fix: `tracker.record(&event)` BEFORE `publisher_tx.try_send(packet)`. Reversed order causes the local event to loopback through the subscriber and be double-applied.

### D3. Contestability tick: polling sweep (NOT per-proposal tokio timer)

Same daily tick as archive sweep walks all `ThresholdReached` proposals, checks `(now - max(threshold_reached_at, last_unsignal_after_threshold)) >= 24h`, transitions to `Finalized`. One tokio task instead of N; survives process restart trivially; handles Unsignal-mid-window correctly without timer reset complexity.

Tick interval: 60s in prod, configurable (faster in tests via param). Up-to-60s finalization latency is acceptable since 24h contestability is the *minimum* anyway.

### D4. Auto-exec `set_power` wiring: direct call

Voting tick calls `community_membership::apply_auto_exec_set_power(community_id, target_pubkey, new_power)` directly. We don't have a generic event bus today; building one for a single action is gold-plating. Voting already depends on community_membership for eligibility checks; inverting that dependency is wrong direction.

### D5. Chat-native dispatch: new `MessageKind::Poll { poll_id }` variant on `ChannelMessage`

Phase 1 left a `TODO ZEB-290 Phase 1.5` block at `ChannelMessageFeed.svelte:272`. The fix: `ChannelMessageDto` gains an optional `kind: 'text' | 'poll'` discriminator and an optional `pollId: string` field. `voting_create_tier1_poll` IPC emits a poll-kind chat message alongside the PollCreate voting event (chat-native: the poll IS the message). `ChannelMessageFeed.svelte` dispatch branch routes `kind === 'poll'` messages to `<PollMessage>`.

---

## Hard rules (user memory — every task obeys these)

- HARD RULE: NO worktrees — implementer subagents use `git checkout -b` in the main repo (override writing-plans skill's worktree prelude)
- HARD RULE: cargo gates run from `src-tauri/`, frontend gates from repo root via `npx`
- HARD RULE: pull-before-work — every task verifies branch is on `origin/main` lineage
- HARD RULE: pipe exit codes lie — use `set -o pipefail` or `${PIPESTATUS[0]}`
- HARD RULE: cargo fmt gate runs alongside clippy in CI — always include `cargo fmt --all -- --check` in implementer task verification
- HARD RULE: metadata-before-irreversible-write — read-only verification (eligibility, validate_X) must precede irreversible writes (signing + applying + broadcasting)
- HARD RULE: never invent Linear IDs — if a task surfaces a needed follow-up ticket, use a descriptive phrase
- HARD RULE: test drift is our fault — broken tests on main are exclusively ours
- HARD RULE: Tauri error extraction — `e instanceof Error ? e.message : String(e)`
- HARD RULE: Tauri IPC param naming — snake_case Rust, boundary auto-converts to camelCase
- HARD RULE: Svelte 5 `$props()` discipline — destructure EVERY prop used in template/effects (ZEB-287 R4 critical bug)
- HARD RULE: cargo invocations — always include `--locked --features test-fixtures --all-targets` for nextest/clippy
- HARD RULE: byte-array IDs over Tauri JSON IPC are `number[]`, never reference-equality compared — use value-equality helpers (`pollIdEqual` etc.)
- HARD RULE: same-length CBOR keys within a struct (spec §3); Tier2PollConfig must use 2-char keys consistently

---

## Five CI gates (every task verifies)

Cargo gates run from `src-tauri/`:
1. `cargo fmt --all -- --check`
2. `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
3. `cargo nextest run --locked --workspace --all-targets --features test-fixtures`

Frontend gates run from repo root via `npx`:
4. `npx tsc --noEmit`
5. `npx vitest run`

---

## Pattern sources

- **Tier 2 module shape** → `src-tauri/src/community_voting_approval.rs` (Phase 1 Tier 1 — same shape, different mechanism)
- **VotingLog extension** → `src-tauri/src/community_voting_log.rs` (Phase 1, `TierState::Empty` placeholder ready to extend)
- **Zenoh engine** → `src-tauri/src/community_channel_log_engine.rs` (ZEB-270 — verbatim copy with type substitutions)
- **NodeState engine wiring** → `src-tauri/src/lib.rs` references to `channel_log_registry` (lines ~344-1331)
- **IPC pattern** → `src-tauri/src/lib.rs:17345-end` (Phase 1 voting IPCs)
- **Frontend adapter** → `src/lib/voting-adapter.ts` (Phase 1; extend with Tier 2 methods)
- **Svelte 5 component discipline** → `src/lib/components/PollMessage.svelte` (subscribeXxx pattern, $effect with cancellation tokens)
- **Wire fixture pinning** → `src-tauri/tests/wire_format_zeb290_fixtures.rs` (regen-on-first-run pattern)
- **Multi-engine integration** → `src-tauri/tests/community_admin_quorum_integration.rs`

---

## Tasks

### Task 0: Pre-flight verification (no commit)

**Files:** none

**Steps:**
- Verify `git rev-parse HEAD` is on `origin/main` lineage (branch already created off `083da73`)
- Run all 5 gates and confirm green baseline:
  - `cd src-tauri && cargo fmt --all -- --check`
  - `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (expect 1557 passed)
  - `cd .. && npx tsc --noEmit`
  - `npx vitest run` (expect 1780 passed)
- Use `set -o pipefail` for piped commands

No commit. If any gate is red, STOP — fix in a separate ticket per the test-drift-is-our-fault memory.

---

### Task 1: Spec amendment — §5 conviction math to fixed-point

**Files:**
- Modify: `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` (§5)

**Summary:** Replace the f64 charge/decay/conviction_at pseudocode in §5 with fixed-point i128 Q96.32 equivalents. Document the `LN2_Q32 = 2_977_044_472` constant. Document that all conviction values are stored as `i128 * 2^-32` seconds. Add a note: "Floating-point conviction math was rejected because cross-engine convergence (acceptance criterion #2) requires bit-identical state across architectures, and IEEE 754 fma/subnormal behavior differs between x86 and ARM."

**Commit:** `docs(voting): ZEB-291 Task 1 — spec §5 amendment to fixed-point i128 conviction math`

---

### Task 2: Add `kd` codes + `Lifecycle::ThresholdReached`

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs`

**Summary:** Add `Signal`, `Delegate`, `Undelegate` discriminants to `PollEventKindCode` (wire codes `sg`/`dg`/`ud`). Add `ThresholdReached` variant to `Lifecycle` enum. Extend `next_lifecycle` state machine to accept `Open → ThresholdReached` (on threshold-cross), `ThresholdReached → Open` (on conviction-drop reversion), `ThresholdReached → Finalized` (on 24h uncontested).

Write unit tests for: (a) `PollEventKindCode` serde round-trip preserves wire codes, (b) `next_lifecycle` accepts all Tier 2 transitions, (c) `next_lifecycle` rejects illegal Tier 2 transitions (e.g., `Archived → Open`).

**Commit:** `feat(voting): ZEB-291 Task 2 — add Signal/Delegate/Undelegate kd codes + ThresholdReached lifecycle`

---

### Task 3: `community_voting_conviction.rs` — types + config + fixed-point math helpers

**Files:**
- Create: `src-tauri/src/community_voting_conviction.rs`
- Modify: `src-tauri/src/lib.rs` (`pub mod community_voting_conviction;`)

**Summary:** New module. Define:

```rust
pub const LN2_Q32: i128 = 2_977_044_472;
pub const CONVICTION_FRAC_BITS: u32 = 32;
pub const Q32: i128 = 1 << CONVICTION_FRAC_BITS;

/// Q96.32 fixed-point conviction value.
pub type ConvictionQ32 = i128;

/// Auto-exec action that fires when a Tier 2 proposal finalizes with `ax="sp"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "lowercase")]
pub enum AutoExecAction {
    None,
    SetPower { target_pubkey: OwnerAddr, new_power: u32 },
}

/// Tier 2 PollConfig — payload of PollCreate (kd="cr", tr=2).
/// All keys 2 chars per spec §3 same-length-keys invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier2PollConfig {
    #[serde(rename = "pt")] pub proposal_text: String,
    #[serde(rename = "hl")] pub half_life_seconds: u32,
    #[serde(rename = "tn")] pub threshold_min_q32: ConvictionQ32,
    #[serde(rename = "tx")] pub threshold_max_q32: ConvictionQ32,
    #[serde(rename = "bb")] pub beta: u8,  // 2-char "bb" to satisfy same-length-keys
    #[serde(rename = "dl")] pub delegation_allowed: bool,
    #[serde(rename = "ax")] pub auto_exec: AutoExecAction,
    #[serde(rename = "el")] pub eligibility: Eligibility,
}

pub fn charge_q32(duration_ms: i128, half_life_ms: i128) -> ConvictionQ32 {
    // (1 - 0.5^(d/hl)) * hl / ln(2)
    let pow = pow_half_q32(duration_ms, half_life_ms);  // Q32
    let one_minus = Q32 - pow;
    (one_minus * half_life_ms) / LN2_Q32 * (Q32 / 1000)  // unit conversion ms→s
}

pub fn decay_q32(conviction: ConvictionQ32, dt_ms: i128, half_life_ms: i128) -> ConvictionQ32 {
    let pow = pow_half_q32(dt_ms, half_life_ms);
    (conviction * pow) >> CONVICTION_FRAC_BITS
}

fn pow_half_q32(t_ms: i128, hl_ms: i128) -> ConvictionQ32 {
    // 0.5^(t/hl) via Taylor series for exp(-x ln 2)
    // x = (t * LN2_Q32) / hl  (Q32)
    let x = (t_ms * LN2_Q32) / hl_ms;
    exp_neg_q32(x)
}

fn exp_neg_q32(x_q32: ConvictionQ32) -> ConvictionQ32 {
    // Taylor: exp(-x) ≈ Σ (-x)^n / n! for n=0..6
    let mut term = Q32;
    let mut sum = Q32;
    for n in 1..=7 {
        term = -(term * x_q32) / (Q32 * n as i128);
        sum += term;
    }
    sum
}
```

Write unit tests covering: (a) `charge_q32(0, hl) == 0`, (b) `charge_q32(hl, hl) ≈ hl * 0.5 / ln(2) * Q32` (one half-life ≈ half-coverage), (c) `decay_q32(c, hl, hl) == c / 2`, (d) `pow_half_q32(0, hl) == Q32` (2^0 == 1), (e) `pow_half_q32(hl, hl) ≈ Q32 / 2`.

**Commit:** `feat(voting): ZEB-291 Task 3 — community_voting_conviction.rs types + Q96.32 fixed-point math`

---

### Task 4: Wire-format fixture pinning for Signal/Delegate/Undelegate

**Files:**
- Create: `src-tauri/tests/wire_format_zeb291_fixtures.rs`

**Summary:** Mirror the ZEB-290 fixture pattern (`tests/wire_format_zeb290_fixtures.rs`). For each Tier 2 event kind:
- Build a deterministic test envelope (fixed HLC, fixed actor, fixed Ed25519 keypair via `test-fixtures` feature)
- Compute canonical CBOR encoding
- Compare against a pinned hex string constant
- On first run (constant is `""`), panic with the actual hex so the test author copies it in

Cover all 6 envelopes:
- Tier 2 `PollCreate` (kd=cr, payload=Tier2PollConfig)
- `Signal` (kd=sg)
- `Delegate` (kd=dg)
- `Undelegate` (kd=ud)
- Plus the 3 inner payloads (Tier2PollConfig, SignalPayload, DelegatePayload)

**Commit:** `test(voting): ZEB-291 Task 4 — pin canonical CBOR fixtures for Tier 2 event envelopes`

---

### Task 5: Conviction state per (voter, proposal) — `VoterConvictionState` + tests

**Files:**
- Modify: `src-tauri/src/community_voting_conviction.rs`

**Summary:** Add the per-voter state machine that tracks conviction over time:

```rust
#[derive(Debug, Clone, Default)]
pub struct VoterConvictionState {
    pub is_supporting: bool,
    pub support_started_at_ms: i128,
    pub accumulated_conviction_q32: ConvictionQ32,
    pub last_event_at_ms: i128,
}

impl VoterConvictionState {
    /// Apply a Signal event at event_hlc_ms.
    pub fn apply_signal(&mut self, support: bool, event_hlc_ms: i128, half_life_ms: i128) {
        if support && !self.is_supporting {
            self.is_supporting = true;
            self.support_started_at_ms = event_hlc_ms;
            self.last_event_at_ms = event_hlc_ms;
        } else if !support && self.is_supporting {
            let dt = event_hlc_ms - self.support_started_at_ms;
            self.accumulated_conviction_q32 += charge_q32(dt, half_life_ms);
            self.is_supporting = false;
            self.last_event_at_ms = event_hlc_ms;
        }
    }

    /// Compute conviction at a given wall-clock time.
    pub fn conviction_at(&self, t_ms: i128, half_life_ms: i128) -> ConvictionQ32 {
        if self.is_supporting {
            let active_charge = charge_q32(t_ms - self.support_started_at_ms, half_life_ms);
            // Decay any pre-existing accumulation since last_event_at.
            let decayed_prior = decay_q32(
                self.accumulated_conviction_q32,
                t_ms - self.last_event_at_ms,
                half_life_ms,
            );
            decayed_prior + active_charge
        } else {
            decay_q32(
                self.accumulated_conviction_q32,
                t_ms - self.last_event_at_ms,
                half_life_ms,
            )
        }
    }
}
```

Write unit tests covering: (a) single support pulse builds conviction, (b) toggle off + idle decays, (c) toggle on after decay continues from decayed base, (d) re-toggle off accumulates additional charge, (e) deterministic — identical event sequence on two state instances → identical conviction.

**Commit:** `feat(voting): ZEB-291 Task 5 — VoterConvictionState with apply_signal + conviction_at + determinism tests`

---

### Task 6: Dynamic threshold formula + `Tier2ProposalState`

**Files:**
- Modify: `src-tauri/src/community_voting_conviction.rs`

**Summary:** Add per-proposal state:

```rust
#[derive(Debug, Clone)]
pub struct Tier2ProposalState {
    pub config: Tier2PollConfig,
    pub total_supply: u32,
    pub per_voter: HashMap<OwnerAddr, VoterConvictionState>,
    /// Set when conviction first crosses threshold; reset to None on conviction-drop reversion.
    pub threshold_reached_at_ms: Option<i128>,
    /// Wall-clock of the most recent Unsignal that arrived after threshold was reached.
    /// Used by the tick to compute "24h uncontested since".
    pub last_unsignal_after_threshold_ms: Option<i128>,
}

impl Tier2ProposalState {
    pub fn total_conviction_at(&self, t_ms: i128) -> ConvictionQ32 {
        self.per_voter.values()
            .map(|v| v.conviction_at(t_ms, (self.config.half_life_seconds as i128) * 1000))
            .sum()
    }

    /// effective_supply = voters with at least one active Signal at t
    pub fn effective_supply_at(&self, _t_ms: i128) -> u32 {
        self.per_voter.values().filter(|v| v.is_supporting).count() as u32
    }

    pub fn threshold_conviction_at(&self, t_ms: i128) -> ConvictionQ32 {
        let ratio_q32 = if self.total_supply == 0 {
            0
        } else {
            (self.effective_supply_at(t_ms) as i128 * Q32) / self.total_supply as i128
        };
        let one_minus_ratio = Q32 - ratio_q32;
        let pow = match self.config.beta {
            1 => one_minus_ratio,
            2 => (one_minus_ratio * one_minus_ratio) >> CONVICTION_FRAC_BITS,
            3 => {
                let sq = (one_minus_ratio * one_minus_ratio) >> CONVICTION_FRAC_BITS;
                (sq * one_minus_ratio) >> CONVICTION_FRAC_BITS
            },
            _ => one_minus_ratio,  // Unknown beta defaults to linear
        };
        let span = self.config.threshold_max_q32 - self.config.threshold_min_q32;
        self.config.threshold_min_q32 + ((span * pow) >> CONVICTION_FRAC_BITS)
    }
}
```

Tests: full participation → threshold = min; zero participation → threshold = max; β=2 mid-participation falls between linear interpolation and full-decay.

**Commit:** `feat(voting): ZEB-291 Task 6 — Tier2ProposalState + dynamic threshold formula`

---

### Task 7: `DelegationGraph` CRDT + cycle detection

**Files:**
- Modify: `src-tauri/src/community_voting_conviction.rs`

**Summary:** Liquid democracy delegation. `Delegate{to}` and `Undelegate{}` events fold into a per-(community) graph keyed by delegator. HLC-LWW per delegator (latest event wins).

```rust
#[derive(Debug, Clone, Default)]
pub struct DelegationGraph {
    /// delegator → (delegate, event_hlc_ms). HLC-LWW.
    edges: HashMap<OwnerAddr, (OwnerAddr, i128)>,
}

impl DelegationGraph {
    pub fn apply_delegate(&mut self, delegator: OwnerAddr, delegate: OwnerAddr, hlc_ms: i128) -> Result<(), DelegationError> {
        // D2: cycle detection. Walk transitive closure from `delegate` looking for `delegator`.
        if self.would_create_cycle(delegator, delegate) {
            return Err(DelegationError::Cycle);
        }
        match self.edges.get(&delegator) {
            Some(&(_, existing_hlc)) if existing_hlc >= hlc_ms => return Err(DelegationError::StaleHlc),
            _ => {}
        }
        self.edges.insert(delegator, (delegate, hlc_ms));
        Ok(())
    }

    pub fn apply_undelegate(&mut self, delegator: OwnerAddr, hlc_ms: i128) {
        match self.edges.get(&delegator) {
            Some(&(_, existing_hlc)) if existing_hlc >= hlc_ms => return,
            _ => {}
        }
        self.edges.remove(&delegator);
    }

    /// Returns the effective vote-power multiplier for a delegate after
    /// resolving delegation chains. Used to weight conviction.
    pub fn delegator_count(&self, delegate: OwnerAddr) -> u32 {
        self.edges.values().filter(|(d, _)| *d == delegate).count() as u32
    }

    fn would_create_cycle(&self, new_delegator: OwnerAddr, new_delegate: OwnerAddr) -> bool {
        let mut current = new_delegate;
        let mut visited = HashSet::new();
        while let Some(&(next, _)) = self.edges.get(&current) {
            if next == new_delegator { return true; }
            if !visited.insert(next) { return true; }  // pre-existing cycle (defensive)
            current = next;
        }
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationError {
    Cycle,
    StaleHlc,
}
```

Tests: simple A→B accepted; A→B then A→C accepted (LWW); cycle A→B→A rejected; transitive cycle A→B→C→A rejected; undelegate after delegate clears edge.

**Commit:** `feat(voting): ZEB-291 Task 7 — DelegationGraph CRDT with HLC-LWW + cycle detection (D2)`

---

### Task 8: Apply delegation weight to conviction compute

**Files:**
- Modify: `src-tauri/src/community_voting_conviction.rs`

**Summary:** Extend `Tier2ProposalState::total_conviction_at` to weight each voter's conviction by `1 + delegator_count`. Override semantics: if voter A delegated to B but A also directly Signal'd on a specific proposal, A's direct Signal overrides A's delegation for that proposal only.

```rust
impl Tier2ProposalState {
    pub fn total_conviction_at_with_delegation(
        &self,
        t_ms: i128,
        delegation_graph: &DelegationGraph,
    ) -> ConvictionQ32 {
        let mut weighted = 0i128;
        for (voter, state) in self.per_voter.iter() {
            let weight = 1 + delegation_graph.delegator_count(*voter);
            // Direct Signal overrides delegation — A's per_voter entry takes precedence.
            // Delegators who haven't directly Signal'd inherit B's vote at B's weight.
            let conv = state.conviction_at(t_ms, (self.config.half_life_seconds as i128) * 1000);
            weighted += conv * weight as i128;
        }
        weighted
    }
}
```

Tests: A delegates to B, only B signals → conviction counted with B's weight + A's delegated weight; A delegates to B, both signal → A's direct overrides A's delegation; A delegates to B, B signals, A undelegates → next conviction-at-T no longer counts A's weight under B.

**Commit:** `feat(voting): ZEB-291 Task 8 — delegation-weighted conviction + override semantics`

---

### Task 9: Extend `TierState` enum + `VotingLog::apply` for Tier 2 events

**Files:**
- Modify: `src-tauri/src/community_voting_log.rs`

**Summary:** Replace `TierState::Empty` placeholder with concrete variants:

```rust
#[derive(Debug, Clone)]
pub enum TierState {
    Tier1(Tier1TallyState),
    Tier2(Tier2ProposalState),
}

impl TierState {
    pub fn as_tier1(&self) -> Option<&Tier1TallyState> {
        match self { TierState::Tier1(s) => Some(s), _ => None }
    }
    pub fn as_tier2(&self) -> Option<&Tier2ProposalState> {
        match self { TierState::Tier2(s) => Some(s), _ => None }
    }
    pub fn as_tier2_mut(&mut self) -> Option<&mut Tier2ProposalState> {
        match self { TierState::Tier2(s) => Some(s), _ => None }
    }
}
```

Extend `PollState` with optional `delegation_graph: Option<DelegationGraph>` — populated lazily per-community (one graph per community, shared across all Tier 2 proposals in that community; for simplicity Phase 2 stores it per-proposal — refactor to per-community in Phase 3 if needed).

Extend `VotingLog::apply_with_snapshot` to dispatch `Signal`/`Delegate`/`Undelegate` payloads to the Tier 2 apply paths. Tier 2 `PollCreate` creates a new `PollState` with `TierState::Tier2(Tier2ProposalState{...})`.

For **rolling eligibility** (spec §10), the IPC layer's `voting_signal_tier2` checks membership at event.hlc (not at PollCreate.hlc) BEFORE calling `apply`. The `apply` function itself takes the event on faith — the V6/S2 check is the IPC's responsibility.

For **kicked-member conviction** (spec §5): the apply path does NOT reset a kicked member's conviction. Their existing accumulated_conviction continues decaying naturally; they just can't emit new Signal events (V6 enforces that at IPC time on next attempt).

Tests: Tier 2 PollCreate → state created; Signal{true} → conviction starts; Signal{false} → conviction accumulates and decays; Delegate then Signal from delegate → weighted conviction.

**Commit:** `feat(voting): ZEB-291 Task 9 — TierState::Tier2 + apply path for Signal/Delegate/Undelegate`

---

### Task 10: `community_membership::apply_auto_exec_set_power`

**Files:**
- Modify: `src-tauri/src/community_membership.rs`
- Modify: `src-tauri/src/lib.rs` (callsite later in Task 22)

**Summary:** New public function that voting tick will call when a Tier 2 proposal finalizes with `ax=SetPower`:

```rust
pub async fn apply_auto_exec_set_power(
    node_state: &Arc<Mutex<NodeState>>,
    community_id: SpaceId,
    target_pubkey: OwnerAddr,
    new_power: u32,
) -> Result<(), String> {
    // Mirror set_power_level IPC pattern:
    // 1. Extract signing key + HLC from NodeState
    // 2. Build MembershipEvent { kind: SetPower { target, level: new_power } }
    // 3. Sign via Ed25519
    // 4. Apply to CommunityStateCrdt
    // 5. Publish via CommunitySyncRegistry
    // Return Err(...) on any failure; tick logs and continues.
}
```

This is the **direct call** per design decision D4 — no event bus, no IPC layer indirection. Tick code holds the NodeState lock briefly, computes the event, drops the lock, signs, applies.

Test: unit test that constructs a minimal NodeState in-memory and verifies a successful auto-exec call results in the target member's power changing.

**Commit:** `feat(membership): ZEB-291 Task 10 — apply_auto_exec_set_power for Tier 2 auto-exec wiring`

---

### Task 11: `community_voting_log_engine.rs` skeleton (Zenoh engine)

**Files:**
- Create: `src-tauri/src/community_voting_log_engine.rs`
- Modify: `src-tauri/src/lib.rs` (`pub mod community_voting_log_engine;`)

**Summary:** Copy `community_channel_log_engine.rs` structure verbatim, substituting types:
- `ChannelLogEvent` → `SignedVotingEvent`
- `community_channel_log::ChannelLog` → `community_voting_log::VotingLog`
- `harmony/community/{id}/channel-log/{channel_id}` → `harmony/community/{id}/voting`

Core types:
- `VotingLogEngineParams` (publisher_tx, subscriber_rx, backfill_req_tx, signing_key, device_id, community_id, voting_log handle)
- `VotingLogEngine` struct with `start()` that spawns the receive loop
- `VotingLogRegistry` parallel to `ChannelLogRegistry` — keyed by `SpaceId`, registers engines per community
- `VotingReplayTracker` parallel to `ChannelLogReplayTracker` — dedup by `(actor, device_id) → max_hlc_high_water_mark`

The self-loopback fix from D2: `tracker.record(&event)` is called BEFORE `publisher_tx.try_send(packet)` in the local-publish path.

Skeleton-only in this task — `publish_event` is `unimplemented!()` for now, `process_inbound` decodes CBOR + calls `voting_log.apply` with verify stub. Fill in Task 18.

**Commit:** `feat(voting): ZEB-291 Task 11 — community_voting_log_engine.rs skeleton (Zenoh engine, copies ZEB-270 pattern)`

---

### Task 12: `VotingLogEngine::publish_event` + verify path

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs`

**Summary:** Implement `publish_event`:
1. CBOR-encode the SignedVotingEvent into a packet
2. `tracker.record(&event)` (self-loopback dedup)
3. Locally apply via `voting_log.apply_with_snapshot(...)` — this is the same call the IPC layer makes; engine and IPC share the apply path
4. `publisher_tx.try_send(packet)` — drop on full channel (log warn)

Implement `process_inbound`:
1. CBOR-decode packet into SignedVotingEvent
2. `tracker.contains(&event)` — skip if seen (self-loopback or duplicate)
3. Verify V1-V6 + kind-specific via `voting_core::verify_event(...)`
4. `voting_log.apply_with_snapshot(...)` (None snapshot for peer-received — Phase 2 limitation; eligibility snapshot lookup-at-HLC is Phase 3 work)
5. On successful apply, emit Tauri event (`voting-ballot-cast`, `voting-tier2-signal-cast`, etc.) so frontend learns about it

**Commit:** `feat(voting): ZEB-291 Task 12 — VotingLogEngine publish_event + process_inbound with verify`

---

### Task 13: `community_voting_tick.rs` skeleton

**Files:**
- Create: `src-tauri/src/community_voting_tick.rs`
- Modify: `src-tauri/src/lib.rs` (`pub mod community_voting_tick;`)

**Summary:** New module. Public entry point:

```rust
pub async fn spawn_voting_tick<R: tauri::Runtime>(
    node_state: Arc<Mutex<NodeState>>,
    app: AppHandle<R>,
    interval: Duration,  // 60s prod, 100ms tests
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick_interval = tokio::time::interval(interval);
        loop {
            tick_interval.tick().await;
            if let Err(e) = run_voting_tick(&node_state, &app).await {
                tracing::warn!(?e, "voting tick failed (continuing)");
            }
        }
    })
}

async fn run_voting_tick<R: tauri::Runtime>(
    node_state: &Arc<Mutex<NodeState>>,
    app: &AppHandle<R>,
) -> Result<(), String> {
    let now_ms = current_wall_ms();
    // 1. Tier 1 auto-close (Task 14)
    // 2. Tier 2 threshold-cross detection + reversion (Task 15)
    // 3. Tier 2 contestability finalize (Task 16)
    // 4. Daily archive sweep (Task 17 — only if last_archive_sweep > 1d ago)
    Ok(())
}
```

Skeleton-only — all 4 sub-passes are `Ok(())` stubs in this task.

**Commit:** `feat(voting): ZEB-291 Task 13 — community_voting_tick.rs skeleton (periodic tick coordinator)`

---

### Task 14: Tier 1 auto-close pass in tick

**Files:**
- Modify: `src-tauri/src/community_voting_tick.rs`

**Summary:** Walk all voting_logs; for each Tier 1 poll with `lifecycle == Open && now_ms >= meta.closes_at.wall_ms`:
1. Build a signed `PollClose` event (using node's signing key, fresh HLC)
2. `voting_log.apply_with_snapshot` it
3. Build a signed `PollResult` event with the canonical tally
4. `voting_log.apply_with_snapshot` it
5. Publish both via `voting_log_engines[cid].publish_event`
6. Emit `voting-poll-closed` Tauri event with the result

Tests: a Tier 1 poll past closes_at → tick produces PollClose + PollResult; lifecycle becomes Finalized; `voting-poll-closed` event observed.

**Commit:** `feat(voting): ZEB-291 Task 14 — Tier 1 auto-close pass in voting tick (fires voting-poll-closed event)`

---

### Task 15: Tier 2 threshold-cross detection + reversion

**Files:**
- Modify: `src-tauri/src/community_voting_tick.rs`

**Summary:** Walk all Tier 2 proposals in `Open` lifecycle:
- If `total_conviction_at(now_ms) >= threshold_conviction_at(now_ms) && threshold_reached_at_ms.is_none()`:
  - Set `threshold_reached_at_ms = Some(now_ms)`, `last_unsignal_after_threshold_ms = None`
  - Set `meta.lifecycle = Lifecycle::ThresholdReached`
  - Emit `voting-threshold-reached` Tauri event

Walk Tier 2 proposals in `ThresholdReached` lifecycle:
- If `total_conviction_at(now_ms) < threshold_conviction_at(now_ms)`:
  - Reset `threshold_reached_at_ms = None`
  - Set `last_unsignal_after_threshold_ms = Some(now_ms)`
  - Set `meta.lifecycle = Lifecycle::Open`
  - Emit `voting-threshold-reverted` Tauri event (or just `voting-tier2-signal-cast` with new state; pick one)

Tests: conviction crosses → ThresholdReached + event fired; conviction drops back → Open + reversion event.

**Commit:** `feat(voting): ZEB-291 Task 15 — Tier 2 threshold-cross detection + reversion in tick`

---

### Task 16: Tier 2 contestability finalize

**Files:**
- Modify: `src-tauri/src/community_voting_tick.rs`

**Summary:** Walk Tier 2 proposals in `ThresholdReached` lifecycle:
- `uncontested_since = max(threshold_reached_at, last_unsignal_after_threshold or 0)`
- If `(now_ms - uncontested_since) >= 24h_ms`:
  1. Build signed `PollResult` event with final conviction tally + auto_exec result
  2. Apply + publish
  3. Set `meta.lifecycle = Lifecycle::Finalized`
  4. If `config.auto_exec == AutoExecAction::SetPower{target, new_power}`:
     - Call `community_membership::apply_auto_exec_set_power(node_state, cid, target, new_power).await`
     - Log result
  5. Emit `voting-proposal-finalized` Tauri event

Tests: ThresholdReached + 24h elapsed → Finalized + auto-exec fired; ThresholdReached + Unsignal at 12h → another 24h needed.

**Commit:** `feat(voting): ZEB-291 Task 16 — Tier 2 contestability finalize + auto-exec dispatch`

---

### Task 17: Daily archive sweep

**Files:**
- Modify: `src-tauri/src/community_voting_tick.rs`

**Summary:** Run-at-most-once-per-24h. Walk all voting_logs; for each, call `voting_log.archive_finalized_polls(now_ms)` (the API already exists in Phase 1). Counter on NodeState (or local AtomicI64) tracks `last_archive_sweep_ms`; tick only runs sweep if `(now_ms - last_archive_sweep_ms) >= 24h`.

Tests: archived poll lookup returns minimal meta only (events vec empty for that poll); ThresholdReached proposals NOT archived.

**Commit:** `feat(voting): ZEB-291 Task 17 — daily archive sweep wiring in voting tick`

---

### Task 18: 6 new Tier 2 IPCs + 4 new Tauri events

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Summary:** Add IPCs (after the Phase 1 voting block, ~line 17345+):

```rust
#[tauri::command]
async fn voting_create_tier2_proposal(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
    proposal_text: String,
    half_life_seconds: Option<u32>,
    threshold_min_q32: Option<i128>,
    threshold_max_q32: Option<i128>,
    beta: Option<u8>,
    delegation_allowed: Option<bool>,
    auto_exec: Option<AutoExecAction>,
    eligibility: Eligibility,
) -> Result<String, String> {
    // metadata-before-irreversible: validate config FIRST, then sign + apply + broadcast
    // ...
}

#[tauri::command]
async fn voting_signal_tier2(
    state: tauri::State<'_, Mutex<NodeState>>,
    proposal_id: String,
    support: bool,
) -> Result<(), String> {
    // metadata-before-irreversible: verify proposal Open + member at now_hlc
    // ...
}

#[tauri::command]
async fn voting_delegate_tier2(...) -> Result<(), String> { ... }

#[tauri::command]
async fn voting_undelegate_tier2(...) -> Result<(), String> { ... }

#[tauri::command]
async fn voting_list_tier2_proposals(
    state: tauri::State<'_, Mutex<NodeState>>,
    community_id: String,
) -> Result<Vec<ProposalStateExport>, String> { ... }

#[tauri::command]
async fn voting_get_tier2_proposal(
    state: tauri::State<'_, Mutex<NodeState>>,
    proposal_id: String,
) -> Result<ProposalStateExport, String> { ... }
```

Register all in `generate_handler!`. Emit Tauri events from the appropriate code paths:
- `voting-tier2-proposal-created` → on successful `voting_create_tier2_proposal`
- `voting-tier2-signal-cast` → on successful `voting_signal_tier2` AND on engine inbound Signal
- `voting-threshold-reached` → from tick (Task 15)
- `voting-proposal-finalized` → from tick (Task 16)

Tests: each IPC unit-tested in `mod tests` block; metadata-before-irreversible behavior verified.

**Commit:** `feat(voting): ZEB-291 Task 18 — 6 Tier 2 IPCs + 4 Tauri events`

---

### Task 19: Wire `VotingLogEngine` registry into `NodeState`

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Summary:** Add field:

```rust
pub voting_log_engines: Arc<Mutex<HashMap<SpaceId, Arc<community_voting_log_engine::VotingLogEngine>>>>,
```

Init to empty in `NodeState::default()`. In `start_node`, after the community registry reconcile loop, spawn a `VotingLogEngine` for each known community (mirrors `ChannelLogRegistry::reconcile_from_state` pattern from around lib.rs:1798-1800). In `stop_node`, call `engine.shutdown()` for each.

Tests: integration test that starts node with 2 known communities → both engines spawn; stop_node → both shut down cleanly.

**Commit:** `feat(voting): ZEB-291 Task 19 — VotingLogEngine registry on NodeState + start/stop wiring`

---

### Task 20: Wire `spawn_voting_tick` into start/stop lifecycle

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Summary:** In `start_node`, after engine registry init, spawn the voting tick task:

```rust
let voting_tick_handle = community_voting_tick::spawn_voting_tick(
    Arc::clone(&state),
    app.clone(),
    Duration::from_secs(60),
).await;
// Store handle in NodeState so stop_node can abort it.
```

Add `voting_tick_handle: Option<tokio::task::JoinHandle<()>>` to NodeState. In `stop_node`, abort the handle.

Tests: tick runs and produces no errors against an empty node state; tick correctly handles a poll past its closes_at.

**Commit:** `feat(voting): ZEB-291 Task 20 — wire voting tick into start_node/stop_node lifecycle`

---

### Task 21: Phase 1.5 chat dispatch — `ChannelMessageDto` kind discriminator

**Files:**
- Modify: `src/lib/channel-message-service.ts`
- Modify: `src-tauri/src/lib.rs` (channel-message IPC return shape)

**Summary:** Extend `ChannelMessageDto`:

```typescript
export interface ChannelMessageDto {
    messageId: string;
    communityId: string;
    channelId: string;
    author: string;
    at: HlcDto;
    body: Uint8Array | number[];
    replyTo?: string;
    /** Phase 1.5: message kind. Defaults to 'text' for backward compat. */
    kind?: 'text' | 'poll';
    /** Phase 1.5: present iff kind === 'poll'. Hex 32-byte poll ID. */
    pollId?: string;
}
```

Tag at the Rust side: the channel-log event already has an opaque body. The simplest path is to use a body-prefix convention: poll messages have body starting with a 1-byte magic `0x00` + 32-byte poll_id. The Rust IPC layer that returns `ChannelMessageDto` detects this prefix and sets `kind: 'poll', pollId: hex`. Alternative: add a `kind` field to the channel-log wire format itself — bigger change, defer.

For Phase 2 go with the body-prefix convention (no wire-format change to channel-log). Document the convention in the spec.

**Commit:** `feat(voting): ZEB-291 Task 21 — ChannelMessageDto kind discriminator for chat-native poll dispatch`

---

### Task 22: `voting_create_tier1_poll` posts a chat message

**Files:**
- Modify: `src-tauri/src/lib.rs` (the existing `voting_create_tier1_poll` IPC from Phase 1)

**Summary:** After successful PollCreate event publish, also publish a chat message in the host channel:

```rust
// After voting_log.apply + engine.publish_event succeed:
let body = build_poll_body_prefix(&poll_id);  // [0x00, poll_id[0..32]]
publish_channel_message(node_state, community_id, channel_id, body).await?;
```

Update existing Phase 1 vitests to assert the chat message is also visible. The Phase 1 PollMessage tests don't change (they test the component, not the IPC side effects).

**Commit:** `feat(voting): ZEB-291 Task 22 — voting_create_tier1_poll emits poll-kind chat message`

---

### Task 23: `ChannelMessageFeed.svelte` dispatch branch

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte`

**Summary:** Fill the TODO seam at line 272. When `msg.kind === 'poll' && msg.pollId && votingAdapter`:

```svelte
{:else if msg.kind === 'poll' && msg.pollId && votingAdapter}
  {@const pollMeta = pollMetaCache.get(msg.pollId)}
  {#if pollMeta}
    <PollMessage
      pollId={pollMeta.poll_id}
      meta={pollMeta}
      adapter={votingAdapter}
    />
  {:else}
    <p class="poll-loading">Loading poll…</p>
  {/if}
```

Pre-fetch poll metas into a `Map<hex, PollMeta>` cache via `$effect` that calls `listActivePolls(communityId)`. Refresh on `subscribePollCreated` events.

Add `votingAdapter?: VotingAdapter` prop (optional — feed still renders without voting wired).

Vitest: a chat feed with a poll-kind message renders `<PollMessage>` inline.

**Commit:** `feat(voting): ZEB-291 Task 23 — ChannelMessageFeed.svelte dispatch branch for poll-kind messages (Phase 1.5 seam filled)`

---

### Task 24: Frontend types for Tier 2

**Files:**
- Modify: `src/lib/types/voting.ts`

**Summary:** Add Tier 2 types:

```typescript
export interface Tier2PollConfig {
    proposalText: string;
    halfLifeSeconds: number;
    thresholdMinQ32: string;  // i128 serialized as decimal string
    thresholdMaxQ32: string;
    beta: number;
    delegationAllowed: boolean;
    autoExec: AutoExecAction;
    eligibility: Eligibility;
}

export type AutoExecAction =
    | { kind: 'none' }
    | { kind: 'set_power'; targetPubkey: number[]; newPower: number };

export interface ProposalState {
    proposalId: string;
    communityId: string;
    proposalText: string;
    lifecycle: 'Open' | 'ThresholdReached' | 'Finalized' | 'Archived';
    totalConvictionQ32: string;
    thresholdConvictionQ32: string;
    halfLifeSeconds: number;
    autoExec: AutoExecAction;
    totalSupply: number;
    voterCount: number;
    yourSignal?: boolean;
    thresholdReachedAtMs?: number;
}

export function convictionPercent(total: string, threshold: string): number {
    const t = BigInt(total);
    const th = BigInt(threshold);
    if (th === 0n) return 0;
    return Number((t * 1000n) / th) / 10;  // 1 decimal place
}
```

Note: i128 serialized as decimal string (JSON has no i128). Adapter converts back-and-forth.

**Commit:** `feat(voting): ZEB-291 Task 24 — frontend Tier 2 types (ProposalState, Tier2PollConfig, AutoExecAction)`

---

### Task 25: `voting-adapter.ts` Tier 2 methods + new event subscribers

**Files:**
- Modify: `src/lib/voting-adapter.ts`

**Summary:** Add 6 Tier 2 IPC wrappers + 4 new event subscribers using the existing `subscribeXxx` pattern from Phase 1:

```typescript
async createTier2Proposal(args: CreateTier2ProposalArgs): Promise<string> { ... }
async signalTier2(communityId: string, proposalId: string, support: boolean): Promise<void> { ... }
async delegateTier2(communityId: string, delegate: number[] | null): Promise<void> { ... }
async listTier2Proposals(communityId: string): Promise<ProposalState[]> { ... }
async getTier2Proposal(proposalId: string): Promise<ProposalState> { ... }

subscribeProposalCreated(handler: (p: ProposalCreatedPayload) => void): () => void { ... }
subscribeSignalCast(handler: (p: SignalCastPayload) => void): () => void { ... }
subscribeThresholdReached(handler: (p: ThresholdReachedPayload) => void): () => void { ... }
subscribeProposalFinalized(handler: (p: ProposalFinalizedPayload) => void): () => void { ... }
```

In `connectAdapter`, listen to all 4 new events. Use the same staged-unlisteners + subscribe-list pattern from Phase 1 (per ZEB-290 round 6 refactor).

Vitests parallel to Phase 1's: forwards events to subscribers; idempotent connect; staged-unlisteners on partial failure.

**Commit:** `feat(voting): ZEB-291 Task 25 — voting-adapter.ts Tier 2 IPC wrappers + 4 new event subscribers`

---

### Task 26: `ConvictionProposalCard.svelte`

**Files:**
- Create: `src/lib/components/ConvictionProposalCard.svelte`

**Summary:** Svelte 5 component per the pattern source `PollMessage.svelte`. Props:

```typescript
let {
    communityId,
    proposal,
    adapter,
}: {
    communityId: string;
    proposal: ProposalState;
    adapter: VotingAdapter;
} = $props();
```

UI: lifecycle badge (Open / Threshold reached — 24h window / Finalized), proposal text, conviction bar (filled width = `convictionPercent`, threshold line marker at 100%), signal toggle button ("Signal support" / "Withdraw signal"). Optimistic updates on signal click with rollback on error. Error extraction: `e instanceof Error ? e.message : String(e)`.

Per ZEB-287 R4: destructure EVERY prop used in template/effects (which is all 3 here).

**Commit:** `feat(voting): ZEB-291 Task 26 — ConvictionProposalCard.svelte with signal toggle + conviction bar`

---

### Task 27: `CommunityProposalsPanel.svelte`

**Files:**
- Create: `src/lib/components/CommunityProposalsPanel.svelte`

**Summary:** Svelte 5 component listing all Open + ThresholdReached Tier 2 proposals. Props: `communityId`, `adapter`, `myPower`. Renders:
- Proposal list (each via `<ConvictionProposalCard>`)
- "New proposal" form (gated on `myPower >= 1`)
- Empty state / loading state / error state

$effect subscribes to `subscribeProposalCreated`, `subscribeThresholdReached`, `subscribeProposalFinalized` and refetches on each. Returns unsubscribe closures.

**Commit:** `feat(voting): ZEB-291 Task 27 — CommunityProposalsPanel.svelte with list + new-proposal form`

---

### Task 28: Vitests for Tier 2 components + CommunityView wiring + final gates + PR

**Files:**
- Create: `src/lib/components/__tests__/ConvictionProposalCard.test.ts`
- Create: `src/lib/components/__tests__/CommunityProposalsPanel.test.ts`
- Modify: `src/lib/components/CommunityView.svelte` (add Proposals tab + `votingAdapter` prop)

**Summary:** Vitest coverage parallel to Phase 1's PollMessage tests:
- ConvictionProposalCard: renders text; shows signal button when open; shows withdraw when supporting; hides button when finalized; calls signalTier2 on click; shows error on rejection
- CommunityProposalsPanel: loads proposals on mount; shows form when `myPower >= 1`; submits form; refetches on subscribed events

CommunityView wires the Proposals tab (optional render — only if `votingAdapter` prop is set; backward-compatible).

**Then the final gate sweep:**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail

# Frontend
npx tsc --noEmit
echo "tsc: ${PIPESTATUS[0]}"
npx vitest run
echo "vitest: ${PIPESTATUS[0]}"

# Rust
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

All 5 gates must be green.

**Then push + PR creation:**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-291-phase2-tier2-conviction-folded
gh pr create \
  --base main \
  --title "ZEB-291 Phase 2: Tier 2 Conviction voting + Phase 1.5 fold-ins" \
  --body "$(cat <<'EOF'
## Summary

Phase 2 of the [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) voting/polling umbrella. Ships Tier 2 Conviction voting plus the Phase 1.5 deferrals from [ZEB-290](https://linear.app/zeblith/issue/ZEB-290) (Zenoh sync, two-engine convergence tests, auto-close tick, daily archive sweep tick, chat-native poll dispatch). Both tiers ship cross-peer-functional in this PR.

**Tier 2 core** (spec §5):
- `community_voting_conviction.rs` — fixed-point i128 (Q96.32) conviction math, DelegationGraph CRDT with cycle detection, dynamic threshold formula, ThresholdReached state machine
- 6 IPCs: voting_create_tier2_proposal, voting_signal_tier2, voting_delegate_tier2, voting_undelegate_tier2, voting_list_tier2_proposals, voting_get_tier2_proposal
- 4 Tauri events: voting-tier2-proposal-created, voting-tier2-signal-cast, voting-threshold-reached, voting-proposal-finalized
- Wire format extensions: Signal (kd=sg), Delegate (kd=dg), Undelegate (kd=ud) per spec §5
- Rolling eligibility per spec §10 (verify at event.hlc, not snapshot)
- Kicked-member conviction decays normally (no implicit Unsignal)
- Auto-exec set_power wired directly into community_membership

**Phase 1.5 fold-ins** (deferred from [ZEB-290](https://linear.app/zeblith/issue/ZEB-290) #130):
- `community_voting_log_engine.rs` — Zenoh sync engine copying the [ZEB-270](https://linear.app/zeblith/issue/ZEB-270) channel-log-engine pattern
- `community_voting_tick.rs` — periodic tick (60s prod): Tier 1 auto-close, Tier 2 threshold detection + reversion, Tier 2 contestability finalize, daily archive sweep
- Two-engine integration tests for both Tier 1 ballot convergence AND Tier 2 conviction convergence
- ChannelMessage gains poll-kind discriminator; ChannelMessageFeed.svelte:272 TODO seam filled
- voting-poll-closed Tauri event actually fires from auto-close tick (was wired but not emitted in Phase 1)

**Frontend**:
- `CommunityProposalsPanel.svelte` — Tier 2 governance area
- `ConvictionProposalCard.svelte` — per-proposal card with conviction bar
- Extended voting-adapter.ts with 6 Tier 2 methods + 4 event subscribers

**Spec amendment**: §5 conviction math changed from f64 pseudocode to fixed-point i128 Q96.32 for cross-engine determinism.

**Pattern sources**: [ZEB-290](https://linear.app/zeblith/issue/ZEB-290) (voting foundation), [ZEB-270](https://linear.app/zeblith/issue/ZEB-270) (channel-log engine pattern), [ZEB-287](https://linear.app/zeblith/issue/ZEB-287) (Svelte 5 \$props discipline).

**Phase 1 of 7** complete (ZEB-290). **Phase 2 of 7** this PR. Subsequent phases: [ZEB-292](https://linear.app/zeblith/issue/ZEB-292) Delegation UI, [ZEB-293](https://linear.app/zeblith/issue/ZEB-293) Sortition+STAR, [ZEB-294](https://linear.app/zeblith/issue/ZEB-294) Pol.is, [ZEB-295](https://linear.app/zeblith/issue/ZEB-295) D-FROST tally, [ZEB-296](https://linear.app/zeblith/issue/ZEB-296) TRIP kiosk.

## Test plan

- [ ] \`cargo fmt --all -- --check\` clean
- [ ] \`cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings\` clean
- [ ] \`cargo nextest run --locked --workspace --all-targets --features test-fixtures\` — 1557+ passed
- [ ] \`npx tsc --noEmit\` clean
- [ ] \`npx vitest run\` — 1780+ passed
- [ ] Two-engine Tier 1 convergence integration test
- [ ] Two-engine Tier 2 convergence integration test
- [ ] Full Tier 2 lifecycle integration test (create → signal → threshold → 24h → finalize → auto-exec)
- [ ] Manual smoke deferred (multi-node testing needs UI for Tier 1 poll posting in chat which this PR ships)

Closes [ZEB-291](https://linear.app/zeblith/issue/ZEB-291).
EOF
)"
```

**After PR opens**: hand control back to the controller agent for the autonomous bot-review monitoring loop (CodeRabbit, Cursor Bugbot, CodeAnt, Qodo — NOT Greptile, NOT CI per memory). Pushover when PR converges + becomes mergeable.

**Commit:** `feat(voting): ZEB-291 Task 28 — Tier 2 vitest + CommunityView wire + final gate sweep + PR`

---

## Build sequence checklist

- [ ] Task 0 — Pre-flight verification (no commit)
- [ ] Task 1 — Spec §5 amendment to fixed-point i128
- [ ] Task 2 — Tier 2 kd codes + Lifecycle::ThresholdReached
- [ ] Task 3 — community_voting_conviction.rs types + Q96.32 math
- [ ] Task 4 — Wire format fixtures for sg/dg/ud
- [ ] Task 5 — VoterConvictionState + conviction_at + determinism tests
- [ ] Task 6 — Tier2ProposalState + dynamic threshold
- [ ] Task 7 — DelegationGraph CRDT + cycle detection
- [ ] Task 8 — Delegation-weighted conviction + override semantics
- [ ] Task 9 — TierState::Tier2 + apply path
- [ ] Task 10 — apply_auto_exec_set_power in community_membership
- [ ] Task 11 — community_voting_log_engine.rs skeleton
- [ ] Task 12 — publish_event + process_inbound with verify
- [ ] Task 13 — community_voting_tick.rs skeleton
- [ ] Task 14 — Tier 1 auto-close pass (voting-poll-closed event)
- [ ] Task 15 — Tier 2 threshold detection + reversion
- [ ] Task 16 — Tier 2 contestability finalize + auto-exec dispatch
- [ ] Task 17 — Daily archive sweep wiring
- [ ] Task 18 — 6 Tier 2 IPCs + 4 Tauri events
- [ ] Task 19 — VotingLogEngine registry on NodeState + start/stop
- [ ] Task 20 — spawn_voting_tick wired into start_node/stop_node
- [ ] Task 21 — ChannelMessageDto kind discriminator
- [ ] Task 22 — voting_create_tier1_poll posts chat message
- [ ] Task 23 — ChannelMessageFeed.svelte dispatch branch
- [ ] Task 24 — Frontend Tier 2 types
- [ ] Task 25 — voting-adapter.ts Tier 2 methods + subscribers
- [ ] Task 26 — ConvictionProposalCard.svelte
- [ ] Task 27 — CommunityProposalsPanel.svelte
- [ ] Task 28 — Vitests + CommunityView wire + final gates + PR

## Integration test files (placed during the corresponding feature task)

- `tests/community_voting_tier1_two_engine.rs` (Task 12 or 19) — the missing ZEB-290 Task 15
- `tests/community_voting_tier2_two_engine.rs` (Task 12 or 19) — conviction convergence
- `tests/community_voting_tier2_lifecycle_integration.rs` (Task 16) — full lifecycle

Each integration test follows the `community_admin_quorum_integration.rs` pattern: helper fns for building SignedXEvent, deterministic HLC + signing keys via `test-fixtures` feature, snapshot_of helper for state comparison.

---

## Notes for implementer subagents

1. **TDD ceremony**: each task above is a roadmap — write a failing test first, then the minimal impl, then iterate. Use the Phase 1 plan (`docs/plans/2026-05-16-zeb-290-phase1-voting-core-tier1-approval-plan.md`) as a template for the bite-sized step structure inside each task. That plan is 3743 lines for 18 tasks; this plan is intentionally shorter and trusts you to write the steps.

2. **Per-task verification**: run all 5 gates (or at least the relevant subset) at the end of each task. Cargo fmt is load-bearing — must run alongside clippy.

3. **Commit cadence**: every task except Task 0 ends with a commit using the messages specified above.

4. **Spec is the source of truth** for design decisions you don't see covered here. Spec sections §2 / §3 / §5 / §7 / §8 / §9 / §10 / §11 are all load-bearing.

5. **Phase 1 patterns are the source of truth** for code style. When in doubt, mirror the Phase 1 module that does the analogous thing.

6. **Fixed-point math is the load-bearing precision call**. The Q96.32 representation is documented in Task 3. If the implementer hits a precision issue (test failure with off-by-one in least significant bits), the fix is to add a fractional-bit adjustment, NOT to switch to f64. The whole point is determinism.

7. **Engine pattern is verbatim copy**. Do not innovate on the Zenoh wire protocol — copy `community_channel_log_engine.rs` byte-for-byte and substitute types. The self-loopback fix from Task 11 is critical.

8. **Auto-exec is a direct call** (D4). Don't introduce an event bus. The voting tick holds the NodeState lock, computes the auto-exec action, drops the lock briefly, signs, applies. Hold-and-drop ordering matters for deadlock avoidance.

9. **CommunityVotingPolicy synthesized defaults**: communities without a CommunityVotingPolicy still need Tier 2 to work. Synthesize defaults at materialize time per Phase 1's pattern (community_voting_core.rs:?).

10. **No Linear ticket fabrication**: if a subagent's work surfaces a needed follow-up (e.g., "this also needs an X cleanup"), the subagent puts a `TODO ZEB-XXX(file_a_ticket_for_this_when_we_have_time)` comment in the code, NOT a fabricated ZEB number. User files Phase 1.5 sub-tickets themselves.
