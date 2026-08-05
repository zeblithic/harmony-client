# ZEB-854 — Reject future-stamped sibling liveness certs at the client ingest funnel — Design

**Status:** design of record · **Ticket:** ZEB-854 (ZEB-831 D3 dossier finding) · **Date:** 2026-08-04
**Author:** Koya · **Scope:** harmony-client only (no `harmony-owner` change)
**Follows:** ZEB-721 (self-cert `ClockRegressed` guard) · ZEB-847 (T-OWNER reject-at-ingest for the owner-**state** merge) · ZEB-831 (bounded-time trust policy, `clock_trust`)

## Problem (verified against current source)

`harmony-owner`'s two liveness-freshness reads are **one-sided lower bounds** on
`LivenessCert::timestamp` (checkout `b904b0b`, the pinned rev):

- `trust.rs:44` (`evaluate_trust`): `cutoff = now.saturating_sub(freshness_window); … any(|l| l.timestamp >= cutoff)`
- `state.rs:539` (`active_devices`): `cutoff = now.saturating_sub(active_window); … l.timestamp >= cutoff`

A cert stamped in the *future* satisfies `timestamp >= cutoff` **forever**. So a
sibling device with a fast clock — or a stolen device key minting a cert at
`now + 1y` — reads permanently `active`, pinning the whole fleet's trust state to
`Full`/"fresh" and defeating the ~15-day liveness aging entirely. This is a
**fail-open** trust defect.

`add_liveness` (`state.rs:329`, upstream) verifies the cert signature but accepts
its `timestamp` verbatim (LWW-max: the higher timestamp wins and sticks). Upstream
is frozen (no external-communication rule → dossier only), so the freshness reads
and `add_liveness` cannot be changed here.

### The asymmetry this closes

The client already guards its **own** cert: `refresh_self_liveness`
(`owner_state.rs:970`) returns `LivenessRefreshOutcome::ClockRegressed` when
`cert.timestamp > now` (ZEB-721) and refuses to re-sign a future/fabricated stamp.
But it accepts a **sibling's** future-stamped cert unguarded. ZEB-854 is the
sibling-side mirror of that guard.

### Verified ingest topology (why one choke point suffices)

`LivenessCert`s reach local `state.liveness` through exactly two writers:

1. **Own cert** — `refresh_self_liveness` (`owner_state.rs`), already ZEB-721-guarded.
2. **Sibling certs** — `merge_trust_remote_into_local` (`owner_trust_sync.rs:64`),
   the `trust_merger()` wired into `FleetSyncEngine`. This is the **single
   production funnel** for all inbound sibling trust records; its liveness fold is
   lines 108–119. Every other `add_liveness` call in the client is a
   `#[cfg(test)]` fixture (verified: `owner_quorum_sync.rs:2793` is inside
   `fn sweep_fleet_at`).

The ~6 production **read** sites (`owner_commands.rs:162/434/457`,
`owner_quorum_commands.rs:60`, `owner_quorum_sync.rs:1115`,
`pairing/state_machine.rs:1067`) all call the frozen upstream
`active_devices`/`evaluate_trust`, which iterate `state.liveness` with the
one-sided cutoff. Bounding at every reader is whack-a-mole across frozen APIs;
bounding at the single writer funnel closes the hole for all of them.

## Approach

Three options were considered (approach chosen with the ticket author 2026-08-04):

- **(A) Reject at the ingest funnel** — a control-tier forward-skew reject in
  `merge_trust_remote_into_local`'s liveness loop, before `add_liveness`. **Chosen.**
  One place; fail-closed for liveness (a rejected sibling reads *inactive* — the
  conservative direction, matching ZEB-721's honest-degrade posture); reuses the
  auditable `clock_trust` bounds; and extends the ZEB-847 reject-at-ingest *pattern*
  (established for the owner-**state** merge, `owner_state_sync.rs`) to the
  owner-**trust** merge's liveness records, which ZEB-847's sweep did not cover.
- **(B) Gate the view at the readers** — keep the raw cert in the CRDT (store stays
  convergent) and treat future-stamped certs as not-fresh at each of the ~6 read
  sites. Rejected: it neutralizes even a pre-stored cert, but touches 6
  frozen-upstream call sites via a bounded-view wrapper — miss one and the hole
  re-opens. Too much surface for a hardening fix.
- **(C) Both** — reject at ingest *and* gate the view. Rejected as YAGNI for a
  Medium-priority hardening ticket absent any evidence of pre-fix poisoning.

### Why reject, not clamp

Clamping a future cert down to `now + tolerance` and storing it is doubly wrong for
liveness: (1) two receivers with different clocks store different clamped values →
CRDT divergence, and (2) the clamped value still sits at/above the freshness cutoff,
so it **still reads as fresh** — the fail-open bug survives the clamp. Rejecting
(declining to `add_liveness`) is the only fix. This matches the ZEB-847 convergence
ruling (reject future stamps, never clamp-and-store).

### Why reject-at-ingest is not the "gate the store, not the view" anti-pattern

The ZEB-621/ZEB-831-P1 lesson ("gate the display view, never delete at a
load/persist boundary") targets a *write-back deletion*: a load path that filters
then `save()`s the filtered set, permanently dropping data, with a slow clock
purging everything. Reject-at-ingest here does neither: it **declines to add** an
incoming merge record — nothing on disk is removed, the device's own signed records
are untouched, and the sibling re-publishes, so the record is re-evaluated every
sync round. It is fail-open on an unreadable local clock (below), so a bad *local*
clock never drops honest sibling state. This is the same validating-merge posture
`merge_trust_remote_into_local` already applies (it drops records that fail the
`add_*` validators), extended with a forward-skew check.

## The change

### 1. `clock_trust.rs` — seconds-domain control-tier bound

`LivenessCert::timestamp` is epoch **seconds**; the freshness policy is a **control**
concern (it gates trust state + active-device sets consumed by revocation planning,
quorum, and pairing), so it takes the **control tier** (`MAX_FORWARD_SKEW_MS`, 5 min),
never the 30-min display tier. `clock_trust` has a ms control-tier helper
(`wall_exceeds_forward_skew`) and an ms-stamp/seconds-now variant
(`wall_exceeds_forward_skew_secs`), but no all-seconds control-tier helper. Add one,
symmetric with the existing `DISPLAY_SKEW_TOLERANCE_SECS`:

```rust
/// [`MAX_FORWARD_SKEW_MS`] in whole seconds, for control-tier stamps whose
/// native unit is epoch-seconds (e.g. a `LivenessCert.timestamp`). Mirrors
/// [`DISPLAY_SKEW_TOLERANCE_SECS`] for the control tier.
pub const MAX_FORWARD_SKEW_SECS: u64 = MAX_FORWARD_SKEW_MS / 1000; // 300

/// `true` iff a control-tier epoch-**seconds** `stamp` is implausibly far in the
/// receiver's future (> [`MAX_FORWARD_SKEW_SECS`] ahead of `now_secs`).
/// `now_secs == 0` (unreadable / pre-epoch local clock) ⇒ `false` (apply-all):
/// a bad LOCAL clock must never drop honest state. Seconds-native sibling of
/// [`wall_exceeds_forward_skew`]; boundary inclusive.
#[inline]
pub fn secs_exceeds_forward_skew(stamp_secs: u64, now_secs: u64) -> bool {
    now_secs != 0 && reject_future(stamp_secs, now_secs, MAX_FORWARD_SKEW_SECS)
}
```

The `now_secs == 0` sentinel matches how `merge_trust_remote_into_local` derives
`now` (`SystemTime::now()…unwrap_or_default().as_secs()` → `0` when pre-epoch), and
mirrors the `now_secs != 0` guard already in `wall_exceeds_forward_skew_secs`.
`reject_future` is pure `saturating_sub`, so there is no overflow path and no
`*1000` impedance (the reason a new all-seconds helper is cleaner than reusing the
ms-stamp `wall_exceeds_forward_skew_secs`).

### 2. `owner_trust_sync.rs` — reject in the liveness fold, before LWW

```rust
for (id, cert) in liveness {
    // ZEB-854: harmony-owner's freshness reads (trust.rs / state.rs) are
    // one-sided lower bounds, so a sibling cert stamped in our future reads as
    // "active"/"fresh" forever. Reject a beyond-tolerance future stamp at this
    // ingest funnel — the sibling-side mirror of the ZEB-721 self-cert
    // ClockRegressed guard, extending the ZEB-847 reject-at-ingest pattern to
    // this (trust) merge. Fail-open when our own clock is unreadable (now == 0).
    if crate::clock_trust::secs_exceeds_forward_skew(cert.timestamp, now) {
        tracing::warn!(
            skew_secs = cert.timestamp.saturating_sub(now),
            "trust merge: sibling liveness cert rejected (future-stamped beyond skew tolerance)"
        );
        continue;
    }
    let known_newer = local
        .liveness
        .get(&id)
        .is_some_and(|l| l.timestamp >= cert.timestamp);
    if known_newer {
        continue;
    }
    if let Err(e) = local.add_liveness(cert) {
        tracing::warn!(error = %e, "trust merge: liveness dropped");
    }
}
```

**Reject-before-LWW ordering is load-bearing:** a future cert that *is* strictly
newer than a stored honest one (`known_newer == false`) must be refused, not fall
through to `add_liveness`. Placing the reject first refuses every beyond-tolerance
future cert regardless of LWW state.

### 3. Testability seam

`merge_trust_remote_into_local` reads `SystemTime::now()` internally, so its
forward-skew branch (especially the `now == 0` fail-open path) is not
deterministically testable as written. Extract a delegating inner that takes `now`,
following the codebase's `_at` idiom:

```rust
pub fn merge_trust_remote_into_local(local: &mut OwnerState, remote: OwnerState) -> MergeOutcome {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    merge_trust_remote_into_local_at(local, remote, now)
}

fn merge_trust_remote_into_local_at(local: &mut OwnerState, remote: OwnerState, now: u64) -> MergeOutcome {
    // …existing body, using the passed `now`…
}
```

The public function keeps its signature and the same runtime behavior, so all
production callers (`trust_merger()`, the persist/load merges at
`owner_trust_sync.rs:225/299`) are untouched. Only tests call `_at` with a fixed
`now`. (`_at` may stay private to the module — the only non-test caller is the
public wrapper.)

## Behavior matrix

| Incoming sibling cert vs local `now` | Local clock | Result |
|---|---|---|
| `timestamp ≤ now + 300s` | readable (`now ≠ 0`) | normal path: LWW pre-filter, then `add_liveness` |
| `timestamp > now + 300s` | readable (`now ≠ 0`) | **rejected** (warn, `continue`); not added |
| any `timestamp` | unreadable (`now == 0`) | apply-all: normal path (fail-open — a bad *local* clock never drops honest state) |
| future cert, but a stored cert is `≥` it | readable | rejected first (never reaches the LWW branch) |

## Error handling & the one residual

- **Unreadable/pre-epoch local clock** (`now == 0`): apply-all (accept), per the
  `clock_trust` contract — a bad *local* clock must never drop honest sibling state.
- **Residual (out of scope, documented):** reject-at-ingest prevents any *new*
  future cert from entering, but does **not** neutralize a future cert already
  stored before this fix ships — the LWW pre-filter (`known_newer`) then blocks a
  later honest lower cert from that signer. This is narrow: it requires an enrolled,
  non-revoked device to have synced a future-stamped cert *before* the fix, and
  every post-fix ingest is bounded. If pre-fix poisoning is ever observed, the
  follow-up is a view-gate (approach B) or a one-time store sweep — a separate
  ticket, not this one.
- **Other trust-merge records** (enrollments, revocations, vouching) are **not**
  forward-bounded by ZEB-854. They are not freshness-gated the way liveness is (an
  enrollment does not "age out"; vouching is LWW by `issued_at`; revocations are
  remove-wins), so the one-sided-freshness defect does not apply to them. A
  forward-skew sweep of those records, if warranted, is separate scope.

## Testing

Rust (nextest, `--features test-fixtures`):

- **`clock_trust.rs` unit tests** for `secs_exceeds_forward_skew`, mirroring the
  existing `wall_exceeds_forward_skew` tests:
  - `now == 0` ⇒ apply-all (returns `false` even for `u64::MAX`).
  - Inclusive boundary: `now + MAX_FORWARD_SKEW_SECS` accepted; `+ 1` rejected.
  - Past/present accepted (`now`, `now − 10_000`).
  - `MAX_FORWARD_SKEW_SECS == 300`.
- **`owner_trust_sync.rs` merge tests** via `merge_trust_remote_into_local_at`
  (fixed `now`), alongside the existing `remote_liveness` merge test:
  - A remote whose sibling liveness cert is stamped `now + 3600` ⇒ **not** present
    in `local.liveness` after merge.
  - An in-window cert (`now + 60`) ⇒ merged.
  - `now == 0` ⇒ the future-stamped cert is accepted (fail-open).
  - A rejected future cert does **not** clobber a stored honest cert for the same
    signer.

Full CI-parity sweep before PR: `cargo fmt --all -- --check`; `cargo clippy --locked
--all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run
--locked --workspace --all-targets --features test-fixtures`; `npx tsc --noEmit`;
`npx vitest run` (frontend gates are parity only — this change is Rust-only).

## Scope guardrails (YAGNI)

- **No `harmony-owner` change** (frozen) — all edits are client-side.
- **No change** to the 15-day re-sign threshold, the 30-day freshness / 90-day
  active windows, `LivenessCert`, `add_liveness`, or the CRDT/LWW merge semantics.
- **No new persisted state** and **no frontend change.**
- **No view-gate** and no bound on the merge's non-liveness records (separate scope).
- One new `clock_trust` constant + one helper; one reject branch + one test seam in
  the merge.

## Files

- `src-tauri/src/clock_trust.rs` — `MAX_FORWARD_SKEW_SECS` const;
  `secs_exceeds_forward_skew` helper; unit tests.
- `src-tauri/src/owner_trust_sync.rs` — forward-skew reject in the liveness fold of
  `merge_trust_remote_into_local`; extract `merge_trust_remote_into_local_at(now)`
  test seam; merge tests.

## Out of scope / follow-ups

- View-gate / one-time store sweep to neutralize a *pre-fix* stored future cert
  (only if such poisoning is ever observed).
- Forward-skew bounds on the trust merge's enrollment/revocation/vouching records
  (different semantics; not freshness-gated).
- The upstream `harmony-owner` fix (bound both freshness windows on the future side)
  — recorded in the ZEB-854 dossier for whenever the crate is next revised.
