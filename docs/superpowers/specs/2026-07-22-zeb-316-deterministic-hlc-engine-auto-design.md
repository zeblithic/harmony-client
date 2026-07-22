# ZEB-316 — Deterministic HLC for engine-auto mints (peer-only auto-orchestration)

**Status:** design (awaiting review)
**Ticket:** ZEB-316 (umbrella ZEB-289 voting) · **Branch:** `zeb-316-deterministic-hlc-engine-auto`
**Scope decision:** *Thorough* — deterministic HLC for every double-mintable engine-auto event
reachable from the re-enabled inbound path where determinism is achievable: **kd=cl and kd=sf
only**. kd=ts (per-committee-member, intentionally distinct) and **BOTH kd=rs modes** (pu- and
se-mode — qbug1 + C1 + Greptile-P1 refinement: a close-anchored HLC stalls below the receive
watermark under concurrent post-close events → non-monotonic stall; result converges via LWW, see
§3) are left on a **wall-clock reservation floored above the poll's live `last_received_hlc`**
(`reserve_next_local_hlc_above`), which is monotonic-safe even under clock skew / a future-walled
trigger (a plain `reserve_next_local_hlc` is not — see §3). Single repo (harmony-client).

## Problem

`community_voting_log_engine.rs::maybe_trigger_engine_auto_orchestration` is the hook that
auto-mints Tier-3 lifecycle events (kd=cl close, kd=rs result, kd=sf sortition-failed) when an
engine holds the `local_signing` key. It is fired from the **local publish path** (`:1831`) but
is **deliberately suppressed on the inbound path** (`process_inbound_dispatch`, NOTE at `:2792`).

The suppression exists because the auto-mint HLC comes from `reserve_next_local_hlc` (`:537`),
which bakes in **two per-engine values**:

- `wall_ms` from `SystemTime::now()`, and
- `device_id` from `self.device_id`.

Both land in the `Hlc` (`owner_state_types.rs:318`: `{ wall_ms, logical, device_id }`), and the
whole `Hlc` is part of `signing_bytes` → part of `event_hash = SHA256(signing_bytes)`. So when two
engines both hold the signing key and both react to the same triggering event, each mints a
*different* event; the LWW tuple `(wall_ms, logical, device_id)` picks a different winner per run,
and `close_event_hash` diverges. That divergence is what reverted the inbound re-enablement in
commit `83bfb11`, and it blocks the peer-only-mint robustness case (originator offline before the
ratification deadline).

## Why determinism is achievable

The triggering event is a **signed event** — byte-identical on every replica once it propagates
over Zenoh. Its `Hlc` is therefore identical everywhere. Everything else the mint consumes is
*already* replica-identical:

- **signing_key + actor**: both come from `self.local_signing` (`:874`, `Some((key, owner))`) —
  the owner is stored *with* the key. Two engines that installed the same key have identical
  `signing_key` **and** `self_owner`.
- **payload**: kd=cl carries just `poll_id`; kd=rs carries `poll_id` + a `StarResult` produced by
  a deterministic tally over canonically-ordered candidates/ballots. No timestamps, nonces, or
  device ids in the body (verified: no `SystemTime::now`/`rand`/`Uuid` in the production
  construction path).
- **Ed25519** signatures are deterministic (RFC 8032).

So the **HLC is the sole divergent input.** Derive it purely from something already identical on
every replica and the entire event becomes byte-identical → same `event_hash` → LWW is trivial
(one hash to pick).

## Design

### 1. Pure derivation helper

```rust
/// Derive a deterministic HLC for an engine-auto mint: strictly newer than
/// `base` and IDENTICAL across every replica that reacts to the same `base`.
///
/// Unlike `reserve_next_local_hlc`, this reads NO wall-clock, NO `self.device_id`,
/// and does NOT touch the hlc_tracker — all three diverge per replica. `lane`
/// must itself be a deterministic function of poll context (see §2).
fn engine_auto_hlc_from_base(base: &Hlc, lane: String) -> Hlc {
    // Strictly newer by (wall_ms, logical, device_id) tuple ordering.
    // logical+1 at equal wall is strictly newer regardless of device_id.
    // Saturation guard: if logical is at u32::MAX, bump wall instead so the
    // result is still strictly newer (astronomically unlikely — logical resets
    // to 0 whenever wall advances).
    if base.logical == u32::MAX {
        Hlc { wall_ms: base.wall_ms.saturating_add(1), logical: 0, device_id: lane }
    } else {
        Hlc { wall_ms: base.wall_ms, logical: base.logical + 1, device_id: lane }
    }
}
```

**Naming note.** The ticket sketched `reserve_next_local_hlc_from_base`, but there is no
reservation here (no tracker mutation) — "reserve" would be a misnomer. Proposed name:
`engine_auto_hlc_from_base`. (Open to a different name in review.)

**Why not touch the tracker.** `reserve_next_hlc_for_device` advances per-device state that differs
per replica; feeding it here would both re-introduce divergence and fail to guarantee
strictly-newer. The derivation must be a *pure function of `base` + `lane`.*

### 2. Deterministic device-id lanes

Following the existing D-FROST beacon precedent (`:738`, `device_id: format!("engine-{prefix}")`,
first 4 bytes of poll_id hex), each auto-mint kind gets its own poll-derived lane:

- kd=cl → `engine-auto-cl-{poll_prefix}`
- kd=sf → `engine-auto-sf-{poll_prefix}`
- kd=rs → **retired**: kd=rs (both pu- and se-mode) mints on wall-clock `reserve_next_local_hlc`,
  not this helper (§3). The helper's `"rs"` lane survives only in the helper's own unit test.

Same on every replica (function of poll id only). Distinct per kind so the events sort in a stable
causal order and never collide at the LWW layer.

### 3. Base-sourcing by causal role

| Event | Base HLC | Sourced from |
|-------|----------|--------------|
| kd=sf | triggering event's `.hlc` | **threaded** from caller |
| kd=cl | triggering event's `.hlc` | **threaded** from caller |
| kd=rs pu-mode | **EXCLUDED** — wall-clock, floored above watermark | `reserve_next_local_hlc_above(last_received.wall+1)` (qbug1 + Greptile-P1 fix) |
| kd=rs se-mode (`try_finalize_secret_tally`) | **EXCLUDED** — wall-clock, floored above watermark | `reserve_next_local_hlc_above(last_received.wall+1)` (C1 + Greptile-P1 fix) |

**close/sortition-failed anchor to the trigger** (threaded), pinning them to the exact event that
fired the hook — race-free even if another event applies between trigger-apply and mint under
concurrent dispatch.

**BOTH kd=rs modes are EXCLUDED from deterministic HLC** (qbug1 + C1 refinement — pu-mode was
originally slated to anchor on a `close_hlc` state field; it now stays on `reserve_next_local_hlc`,
same as se-mode). kd=rs must be minted **ABOVE the current receive watermark** (`last_received_hlc`)
or the apply-time monotonic gate (`HlcNotMonotonic`) rejects it. A close-anchored HLC — frozen
forever to `(close.wall, close.logical+1)` — can stall BELOW that watermark:

- **pu-mode:** the kd=cl→kd=rs cascade is NOT serialized. The `voting_log` mutex is released
  per-apply, and the post-apply hook re-fires after release with real yields at `persist_now().await`
  (which holds the log lock across a `spawn_blocking`) and `publisher_tx.send().await`. On the
  multi-threaded runtime a concurrent IPC ballot-cast or backfill apply can slip a higher-HLC event
  in between kd=cl and kd=rs, advancing `last_received_hlc` past the close HLC. Because the
  close-anchored HLC is frozen, EVERY re-mint (and every byte-identical peer copy) is then rejected
  forever → the poll never finalizes via engine-auto on that replica.
- **se-mode:** the committee kd=ts (tally-share) events that cross the ≥t threshold land AFTER the
  close carrying per-replica wall-clock HLCs whose `wall > close.wall`, and are not replica-identical
  — so no deterministic base is ≥ every applied kd=ts.

kd=cl is immune to this (it re-anchors on the moving close watermark each hook re-fire, so it
self-heals); only the close-*frozen* kd=rs stalls.

**Greptile-P1 refinement — floor the reservation above the watermark.** A plain
`reserve_next_local_hlc` (wall = `SystemTime::now()`) cures the real-time concurrent-event case but
is NOT always ≥ the receive watermark: kd=cl is deterministic and anchors on the triggering event's
HLC, so an accepted inbound trigger whose wall is AHEAD of this node's local clock (clock skew, or a
future-dated event) makes kd=cl — hence `last_received_hlc` — sit at that future wall, and a
`now`-reserved kd=rs is then BELOW it → the monotonic gate rejects it and the poll stays
closed-but-not-finalized until local time catches up. The fix mints via
`reserve_next_local_hlc_above(floor)` with `floor = last_received.wall_ms + 1`, snapshotting
`last_received_hlc` under the same `voting_log` lock that reads `t3`. Because
`reserve_next_hlc_for_device` returns `wall = max(wall_now_ms, own_prev_wall)`, flooring `wall_now_ms`
at `watermark.wall + 1` guarantees the reserved wall `> watermark.wall` → strictly newer than the
watermark by the wall field alone (no logical/device tiebreak). **Invariant:** every engine-auto mint
produces an HLC strictly greater than the poll's live `last_received_hlc` (kd=cl/kd=sf already satisfy
it by anchoring on the trigger, which just became the watermark; kd=rs now does too). Residual window:
between the snapshot (lock released) and the kd=rs apply (lock re-acquired) another event can advance
the watermark further — self-heals, because the interfering event re-fires the hook, which re-snapshots
the higher watermark and re-mints above it; inherent to the lock-release-before-publish architecture
and acceptable, since the point of the floor is to remove the clock-dependent *permanent* stall.

kd=rs remains wall-clock-based / non-deterministic — a deterministic HLC is unnecessary because the
kd=rs **result** converges bit-identically across replicas via the deterministic `StarResult` payload
(pu) / Lagrange invariance in `recover_secret_tally` (se) + the apply-time LWW/terminal-state gate
that keeps the first finalizing kd=rs and drops the rest.

### 4. State field: `close_hlc` — REMOVED (qbug1 fix)

The original design added `close_hlc: Option<Hlc>` to the Tier-3 poll state to anchor the pu-mode
kd=rs mint. The qbug1 fix reverted pu-mode kd=rs to wall-clock (§3), removing the field's only
production consumer, so the field was deleted from `Tier3PollState` (the decl, the `None`
initializer, the Debug-impl line, and the `PollClose` set-site). kd=cl and kd=sf source their base
HLC by threading the triggering event's `.hlc` from the caller (§5), so no persisted close HLC is
needed.

### 5. Re-enable the inbound hook + threading

- Change signature: `maybe_trigger_engine_auto_orchestration(self, pid, base_hlc: &Hlc)`.
  - Callers pass the **applied triggering event's** `.hlc`:
    - local publish path `:1831` → the applied `event.hlc`,
    - inbound path (re-enable at the NOTE site `:2804`, inside `if event.tier == Tier::Sortition`)
      → `event.hlc` (already in scope, destructured `:2760`).
  - Inside: kd=cl and kd=sf use `engine_auto_hlc_from_base(base_hlc, …)`; pu-kd=rs mints on
    `reserve_next_local_hlc` (wall-clock, §3). kd=sf remains its own trigger (Stage::Sortition,
    `proposer == self_owner`).
- `try_finalize_secret_tally` (se-mode kd=rs) keeps **`reserve_next_local_hlc` (wall-clock)** — it is
  EXCLUDED from deterministic HLC (§3, C1 refinement); no base param threaded (its call site in the
  orchestration tail stays as-is signature-wise).
- kd=ts (`maybe_emit_tally_share`, `:1466`) is **unchanged** — per-member shares are meant to be
  distinct; they must keep `reserve_next_local_hlc`.

**Recursion correctness.** kd=cl publishes via `publish_event`, which applies it locally (setting
`close_event_hash`) and re-fires the hook from `:1831` with `base = cl_ev.hlc`. That recursive
invocation sees `close_event_hash.is_some()`, skips kd=cl, and mints kd=rs on a freshly-reserved
wall-clock HLC (§3). The outer kd=rs attempt also mints; whichever applies first moves the poll to
`Finalized`, and the apply-time `PollInFinalizedState` gate rejects the later one. The kd=rs
*result* is byte-identical across both attempts and across replicas (deterministic `StarResult`),
so LWW/terminal-state resolves them to the same outcome.

### 6. Why the result is bit-identical (proof sketch)

For any auto-mint, on two engines A and B that both hold the same installed key and have both
applied the same triggering event:

- `signing_key` identical (same install), `actor` identical (owner stored with key),
- `payload` deterministic (poll_id; result via canonical tally / Lagrange),
- `hlc` identical (pure function of a base that is itself byte-identical across replicas),
- ∴ `signing_bytes` identical → Ed25519 `sig` identical → `event_hash` identical.

LWW then has a single hash to select; `close_event_hash` and `result` converge bit-identically.

**Scope of the byte-identity guarantee (I-1).** The proof above is conditional on its premise —
"both applied *the same* triggering event." Byte-identical `close_event_hash`/`kd=sf` therefore
holds when the same event first satisfies the trigger on every replica, which is the common case
(a single `kd=ss` pushing past the deadline fires both engines; verified by the strengthened
race-tolerant + 100× tests). Under network reordering a *different* event may first cross the
`kd=cl` deadline (`last_hlc.wall > created + window`) or the `kd=sf` decline-capacity threshold on
different replicas; those replicas then mint on the same lane but with different ordinals →
divergent `close_event_hash`. (pu-mode `kd=rs` is now wall-clock so its *event* was never
byte-identical anyway — only its *result* converges, via LWW.) This is **benign and
non-regressive**: the vote *outcome* always converges — the tally is
deterministic and both replicas reach `Finalized`/`Failed`, with late lifecycle duplicates absorbed
by the terminal-state + monotonic apply gates — and `close_event_hash` is terminal materialized
state, not compared in any cross-peer state-root, so a mismatch causes no fault. It is exactly the
pre-ZEB-316 wall-clock outcome (different lanes, LWW winner). **Future code must not treat a
`close_event_hash` mismatch across peers as corruption** (e.g. a backfill reconciler): only `result`
+ terminal `stage` are guaranteed peer-identical.

## Blast radius

All production `reserve_next_local_hlc` / `reserve_next_local_hlc_above` call sites are in
`community_voting_log_engine.rs`: kd=sf, kd=cl, kd=rs(pu), kd=ts (**keep**), kd=rs(se) (**keep**).
After the qbug1 fix only **kd=sf and kd=cl** are deterministic (`engine_auto_hlc_from_base`); kd=ts
stays on plain wall-clock `reserve_next_local_hlc`; and **kd=rs(pu) + kd=rs(se)** mint on a
wall-clock reservation floored above the poll's live watermark
(`reserve_next_local_hlc_above(last_received.wall+1)`, Greptile-P1 fix) — still non-deterministic,
their *results* converge via LWW. Plus: the hook signature + its two callers and the inbound
re-enable. (The `close_hlc` state field the original design added was removed by the qbug1 fix — see
§4.) No call sites outside the engine file.

## Testing

1. **Strengthen the existing race-tolerant test**
   (`community_voting_tier3_ipc_integration.rs:964` `ipc_tier3_engine_auto_kd_cl_kd_rs_race_tolerant`):
   today it asserts convergence that rides on bridge propagation + LWW. Add an assertion that the
   two engines' **independently minted** kd=cl are byte-identical (same `event_hash`) *before* any
   cross-engine propagation — the stronger property the test name gestures at.
2. **Determinism / flake**: run the strengthened test 100× (acceptance #3) to confirm no flake.
3. **Unit test** `engine_auto_hlc_from_base`: (a) strictly-newer than base for representative
   inputs, (b) identical output for identical `(base, lane)`, (c) the `logical == u32::MAX`
   saturation guard bumps `wall_ms`.
3a. **Unit test** `reserve_next_local_hlc_above` (Greptile-P1 fix): (a) a floor above `now` forces
   the reserved `wall_ms >= floor` (the guarantee that keeps kd=rs above the receive watermark under
   clock skew / a future-walled trigger), (b) a floor of `0`/below-now tracks the wall clock like the
   plain `reserve_next_local_hlc`, advancing the shared device lane monotonically. This is the
   mechanical guard for the fix; a full future-walled RED→GREEN integration fixture is real-clock
   flaky/awkward to construct, so we rely on this helper test plus the existing race-tolerant + se-mode
   integration tests staying green (they now drive the floored mint path).
4. **se-mode** (C1 refinement): a two-engine test where both cross the share threshold with the
   committee kd=ts at walls **strictly greater than the close** (the realistic production case), and
   assert both engines reach `Finalized` with an identical recovered **`StarResult`**. Do NOT assert
   the kd=rs `event_hash`/HLC is identical — se-mode kd=rs is intentionally wall-clock (non-
   deterministic HLC); convergence is on the result only, via Lagrange invariance + the LWW gate.
5. Full CI gates from `src-tauri/` (fmt, clippy `--all-targets`, nextest) before PR.

## Out of scope

- kd=ts determinism (intentionally per-member distinct).
- Any change to `reserve_next_local_hlc` itself (kept for kd=ts and non-engine callers).
- The single-writer-election alternative (rejected — contradicts the peer-only-robustness goal).

## Acceptance (from ticket)

1. Two engines holding local_signing converge on `close_event_hash` (byte-identical via the
   deterministic kd=cl) + `result` (byte-identical *value* — the kd=rs event itself is wall-clock,
   convergence is via the deterministic `StarResult` payload + LWW) **when they share the triggering
   event** (the common case); under a split trigger they still converge on `result` + terminal
   `stage` via LWW (see §6 "Scope of the byte-identity guarantee").
2. `maybe_trigger_engine_auto_orchestration` re-enabled from `process_inbound_dispatch`.
3. `ipc_tier3_engine_auto_kd_cl_kd_rs_race_tolerant` passes deterministically (100×).
