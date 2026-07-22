# ZEB-316 — Deterministic HLC for engine-auto mints (peer-only auto-orchestration)

**Status:** design (awaiting review)
**Ticket:** ZEB-316 (umbrella ZEB-289 voting) · **Branch:** `zeb-316-deterministic-hlc-engine-auto`
**Scope decision:** *Thorough* — deterministic HLC for every double-mintable engine-auto event
reachable from the re-enabled inbound path (kd=cl, kd=rs pu-mode, kd=sf, kd=rs se-mode).
kd=ts (per-committee-member, intentionally distinct) is explicitly left as-is.
Single repo (harmony-client).

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
- kd=rs → `engine-auto-rs-{poll_prefix}` (both pu- and se-mode)

Same on every replica (function of poll id only). Distinct per kind so the events sort in a stable
causal order and never collide at the LWW layer.

### 3. Base-sourcing by causal role

| Event | Base HLC | Sourced from |
|-------|----------|--------------|
| kd=sf (`:941`) | triggering event's `.hlc` | **threaded** from caller |
| kd=cl (`:1057`) | triggering event's `.hlc` | **threaded** from caller |
| kd=rs pu-mode (`:1161`) | close event's `.hlc` | `t3.close_hlc` (state) |
| kd=rs se-mode (`:1566`, `try_finalize_secret_tally`) | close event's `.hlc` | `t3.close_hlc` (state) |

**close/sortition-failed anchor to the trigger** (threaded), pinning them to the exact event that
fired the hook — race-free even if another event applies between trigger-apply and mint under
concurrent dispatch.

**result events anchor to the close** because se-mode kd=rs's *real* trigger is "crossing the ≥t
share threshold," which fires on a **different kd=ts per replica** and thus cannot be a base. The
close (kd=cl) HLC is replica-canonical once kd=cl is deterministic, and is the natural causal
parent of the result. Anchoring pu-kd=rs the same way is uniform and makes the recursive/outer
kd=rs attempts (see §5) byte-identical duplicates rather than an HLC race.

### 4. New state field: `close_hlc`

Add `close_hlc: Option<Hlc>` to the Tier-3 poll state, set in the `PollClose` apply arm
(`community_voting_tier3.rs:1023-1025`) alongside `close_event_hash`, from the close event `ev.hlc`
(already in scope). `close_hlc` is immutable once set (close is one-time), so reading it as a base
is race-free.

### 5. Re-enable the inbound hook + threading

- Change signature: `maybe_trigger_engine_auto_orchestration(self, pid, base_hlc: &Hlc)`.
  - Callers pass the **applied triggering event's** `.hlc`:
    - local publish path `:1831` → the applied `event.hlc`,
    - inbound path (re-enable at the NOTE site `:2804`, inside `if event.tier == Tier::Sortition`)
      → `event.hlc` (already in scope, destructured `:2760`).
  - Inside: kd=cl and kd=sf use `engine_auto_hlc_from_base(base_hlc, …)`; pu-kd=rs reads
    `t3.close_hlc`. kd=sf remains its own trigger (Stage::Sortition, `proposer == self_owner`).
- `try_finalize_secret_tally` (se-mode kd=rs) reads `t3.close_hlc` internally — **no base param
  threaded** (its two call sites `:1212` orchestration-tail and `:2868` inbound stay as-is
  signature-wise).
- kd=ts (`maybe_emit_tally_share`, `:1466`) is **unchanged** — per-member shares are meant to be
  distinct; they must keep `reserve_next_local_hlc`.

**Recursion correctness.** kd=cl publishes via `publish_event`, which applies it locally (setting
`close_hlc`) and re-fires the hook from `:1831` with `base = cl_ev.hlc`. That recursive invocation
sees `close_event_hash.is_some()`, skips kd=cl, and mints kd=rs from `t3.close_hlc`. The outer
kd=rs attempt (`:1091+`) also reads `t3.close_hlc` → **identical HLC** → the apply-time
`PollInFinalizedState` gate rejects the second as a byte-identical duplicate. Deterministic on
every replica.

### 6. Why the result is bit-identical (proof sketch)

For any auto-mint, on two engines A and B that both hold the same installed key and have both
applied the same triggering event:

- `signing_key` identical (same install), `actor` identical (owner stored with key),
- `payload` deterministic (poll_id; result via canonical tally / Lagrange),
- `hlc` identical (pure function of a base that is itself byte-identical across replicas),
- ∴ `signing_bytes` identical → Ed25519 `sig` identical → `event_hash` identical.

LWW then has a single hash to select; `close_event_hash` and `result` converge bit-identically.

## Blast radius

All production `reserve_next_local_hlc` call sites are in `community_voting_log_engine.rs`:
`:941` kd=sf, `:1057` kd=cl, `:1161` kd=rs(pu), `:1466` kd=ts (**keep**), `:1566` kd=rs(se). This
change touches four of the five (all but kd=ts) plus: the hook signature + its two callers, the
inbound re-enable, and the `close_hlc` field + its set-site. No call sites outside the engine file.

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
4. **se-mode**: a two-engine test that both cross the share threshold and assert the kd=rs
   `event_hash` (not just the result) is identical across replicas.
5. Full CI gates from `src-tauri/` (fmt, clippy `--all-targets`, nextest) before PR.

## Out of scope

- kd=ts determinism (intentionally per-member distinct).
- Any change to `reserve_next_local_hlc` itself (kept for kd=ts and non-engine callers).
- The single-writer-election alternative (rejected — contradicts the peer-only-robustness goal).

## Acceptance (from ticket)

1. Two engines holding local_signing converge bit-identically on `close_event_hash` + `result`.
2. `maybe_trigger_engine_auto_orchestration` re-enabled from `process_inbound_dispatch`.
3. `ipc_tier3_engine_auto_kd_cl_kd_rs_race_tolerant` passes deterministically (100×).
