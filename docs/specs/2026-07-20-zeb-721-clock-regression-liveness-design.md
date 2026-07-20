# ZEB-721 — Handle a regressed host clock in the shared liveness refresh path

**Status:** design of record · **Ticket:** ZEB-721 (child of ZEB-410) · **Date:** 2026-07-20
**Author:** Koya · **Scope:** harmony-client only (no `harmony-owner` change)

## Context

`refresh_self_liveness` (`src-tauri/src/owner_state.rs:873`) re-signs this device's
`LivenessCert` when the existing cert has aged past `DEFAULT_FRESHNESS_WINDOW_SECS/2`
(~15 days), stamping it at the caller's wall-clock `now`. Peers accept a cert as fresh
while `cert.timestamp >= peer_now − 30d` (`harmony-owner` `trust.rs:44-57`); otherwise the
whole trust state is `Refused(StaleTrustState)`.

**The bug (Greptile P1 on ZEB-410 PR #509):** if the host clock **regresses behind an
already-signed cert** (dead RTC, VM snapshot restore, manual/NTP correction), the local
cert's `timestamp` is now in the *future* relative to `now`. The re-sign gate
(`cert.timestamp < now − 15d`) then reads the cert as "fresh" forever, so it **never
re-signs**. Meanwhile peers with correct clocks eventually see the cert age past their
30-day window and reject the node. The node cannot recover until its clock catches back up.

This is a property of the **shared** `refresh_self_liveness`, so it affects both call
sites — the on-panel-load path (`get_owner_state`) and the ZEB-410 heartbeat. ZEB-410 only
elevated its relevance: headless `serve` nodes now rely on the timer for renewal and have
no panel-load fallback.

### Two failure modes (they differ)

- **Transient** (NTP correction, VM snapshot-restore — the common case): the cert is
  briefly future-stamped; we correctly no-op; it **self-heals** once the clock corrects.
  Needs nothing beyond *not emitting a garbage cert in the meantime*.
- **Persistent** (dead RTC, no NTP): the cert stays future-stamped forever, ages out on
  peers at +30d, and the node cannot recover unattended. This is the case that forces a
  posture decision.

### Two facts that shape the design

1. **The device's own cert `timestamp` already IS the monotonic floor.** Only this device
   signs its own cert, and the existing re-sign gate only fires when `now > cert.timestamp
   + 15d` (i.e. `now > cert.timestamp`). So `refresh_self_liveness` **can never sign a
   timestamp below the current cert** — the LWW in `add_liveness` (`state.rs:336-341`,
   higher-timestamp-wins) and the sibling-merge pre-filter (`owner_trust_sync.rs:108-119`)
   further guarantee a lower cert would lose anyway. The bug is therefore **not** "signs a
   bad low value" — it is a **stuck no-op**. No new persisted floor is needed.
2. **A `LivenessCert` is a wall-clock *attestation*** ("this device was alive at time T").
   A node whose clock is broken genuinely cannot make a truthful *fresh* attestation.

## Decision — posture: honest degrade + surface (NOT fabricate time)

Chosen 2026-07-20. When a node truly can't tell the time, it **honestly degrades** rather
than fabricating forward time to stay trusted:

- **Never emit a regressed or pre-epoch cert.** Preserve the existing no-op-under-regression
  behavior (do not re-sign a lower/fabricated timestamp), and fix the pre-epoch footgun.
- **Detect once, in the shared path**, so both call sites behave identically.
- **Surface** the anomaly: structured warn logs (both paths) + a small `OwnerStateView`
  field that the Devices panel renders as a "clock regressed" banner.
- The **transient** case self-heals automatically. The **persistent** case honestly loses
  trust-freshness until the clock is fixed, but is now **visible** instead of silent.

**Rejected alternatives** (recorded so they aren't re-litigated):

- *Monotonic auto-recover* (`std::time::Instant`-anchored forward advance): would keep a
  dead-RTC headless node trusted unattended, but fabricates a wall-clock the device cannot
  attest — dubious for a trust primitive, and threads a clock source into a currently-pure
  function (larger invariant surface). Not for a Low-priority hardening ticket.
- *Peer-attested recover* (floor `now` by the max timestamp among active siblings' signed
  certs): an honest recovery path, but does nothing for an isolated/single-device node and
  reaches the sibling cert-set into the signing path (cross-cutting). Deferred as a possible
  future enhancement if unattended headless recovery is later required.

## Architecture

Four changes, all in harmony-client.

### 1. `refresh_self_liveness` returns a status enum (was `bool`)

Replace the `bool` return with an outcome enum that carries *why* it did or didn't write, so
callers can both persist-on-write and surface clock health from one source of truth.

```rust
/// Outcome of a self-liveness refresh attempt (ZEB-721).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessRefreshOutcome {
    /// Re-signed a fresh cert at `now`. Caller MUST persist + notify_dirty.
    Refreshed,
    /// Existing cert is still fresh (< freshness/2 old). Healthy steady state.
    Fresh,
    /// Our own cert is stamped in the FUTURE relative to `now` — the host clock
    /// regressed behind it. Not re-signed (a lower timestamp loses the CRDT
    /// merge, and fabricating time is not our posture). Self-heals on correction.
    ClockRegressed { skew_secs: u64 },
    /// `LivenessCert::sign` failed (already warn-logged). Treated as a no-op.
    SignFailed,
}

impl LivenessRefreshOutcome {
    /// True iff the call mutated `state` (caller must persist + notify_dirty).
    pub fn wrote(self) -> bool { matches!(self, Self::Refreshed) }
}
```

New behavior inside the function, keyed off the existing cert vs `now`:

- `cert.timestamp > now`  → `ClockRegressed { skew_secs: cert.timestamp − now }`. No write.
  The function does **not** log — logging is a caller concern so it can be deduplicated (the
  heartbeat warns only on the healthy→regressed transition; the panel surfaces a banner).
  This keeps the shared function a pure detection point and avoids per-call WARN spam under a
  persistently regressed clock (once/hour from the heartbeat + every panel load would be noise).
- `cert` absent, or `cert.timestamp < now − 15d` → sign; `Refreshed` (or `SignFailed`).
- otherwise (`now − 15d ≤ cert.timestamp ≤ now`) → `Fresh`. No write.

A tiny shared helper keeps the "is the clock regressed" computation DRY between the enum and
the DTO (component 3):

```rust
/// Seconds this device's own cert is stamped in the future vs `now`, if any.
/// `Some(skew)` ⇒ host clock regressed behind our own cert; `None` ⇒ healthy.
pub fn self_liveness_future_skew_secs(state: &OwnerState, device_sk: &SigningKey, now: u64) -> Option<u64> {
    let id = device_id_from_signing_key(device_sk);
    state.liveness.get(&id).and_then(|c| c.timestamp.checked_sub(now)).filter(|&d| d > 0)
}
```

### 2. Fix the pre-epoch footgun at the panel call site

The heartbeat already obtains `now` via `now_unix_secs() -> Option<u64>` and **skips** the
tick on a pre-epoch clock (`liveness_heartbeat.rs:29`). The panel path uses `now_unix()`
which `unwrap_or(0)`s (`owner_commands.rs:61`), so a pre-epoch clock at **first mint** would
stamp a `timestamp = 0` cert (instantly stale to every peer). Align the panel path to the
heartbeat: when the clock is pre-epoch, **skip the refresh** (do not stamp a cert) rather
than passing `now = 0`. `now_unix()` stays unchanged for its other callers; the skip is
local to the `get_owner_state` refresh block.

### 3. Surface clock health on `OwnerStateView` + Devices panel

Add one optional field to `OwnerStateView` (`owner_state.rs:16`, serializes camelCase):

```rust
/// ZEB-721: seconds by which THIS device's own liveness cert is stamped in the
/// future relative to the host clock at snapshot time — i.e. the host clock has
/// regressed behind an already-signed cert, pausing liveness renewal until the
/// clock recovers. `None` in the healthy case. Drives the DevicesPanel banner.
#[serde(default)]
pub self_clock_regressed_skew_secs: Option<u64>,
```

Populated at view-build time from the snapshot via `self_liveness_future_skew_secs(...)`
(same helper as component 1 — single source of truth). Serialized key:
`selfClockRegressedSkewSecs`.

`DevicesPanel.svelte` renders a minimal, non-blocking warning banner when the field is set:

> ⚠ This device's clock appears to have moved backwards (~N behind its own last check-in).
> Liveness renewal is paused until the clock is corrected — re-sync system time (NTP) to
> restore trust freshness on your other devices.

(`N` rendered with the existing relative-duration formatter.) No new action buttons — this
is informational; the remedy is fixing the host clock.

### 4. Heartbeat + call-site glue

- `run_liveness_heartbeat_once` (`liveness_heartbeat.rs:41`): drop its bespoke future-stamp
  warn and return the `LivenessRefreshOutcome`. The spawn loop keeps a `clock_was_regressed`
  flag and warns only on the healthy→regressed transition (dedup), and calls `notify_dirty()`
  + the info log only when `outcome.wrote()`.
- `get_owner_state` refresh block (`owner_commands.rs:727`): `engine.notify_dirty()` only
  when `outcome.wrote()`; the DTO field is computed at view-build (component 3), independent
  of the refresh outcome.

## Behavior matrix

| Host clock vs own cert | `refresh_self_liveness` | Re-sign? | DTO `selfClockRegressedSkewSecs` | Recovery |
|---|---|---|---|---|
| Healthy, cert fresh (≤15d) | `Fresh` | no | `None` | n/a |
| Healthy, cert stale (>15d) | `Refreshed` | yes, at `now` | `None` | n/a |
| Regressed (cert in future) | `ClockRegressed{skew}` (heartbeat warns on transition) | no | `Some(skew)` | self-heals when clock corrects |
| Pre-epoch (`now` invalid) | *not called* (skip at edge) | no | `None`* | self-heals when clock sane |
| Sign error | `SignFailed` + warn | no | `None` | retry next tick |

*Pre-epoch is caught at the call-site edge before the snapshot; the panel simply doesn't
refresh. A future-stamped cert (the regression case) is what the DTO surfaces.

## Error handling

- **Pre-epoch clock:** skip the refresh (emit no cert); heartbeat already skips, panel now
  skips too. Self-heals when the clock becomes sane.
- **Regressed clock:** no re-sign (never move the timestamp backward — preserves CRDT-merge
  safety), warn once per call, surface skew to the DTO. Self-heals on correction.
- **Sign failure:** `SignFailed`, warn-logged (unchanged behavior), no write.
- Every no-op outcome leaves the cert `timestamp` untouched.

## Testing

Rust (nextest, `--features test-fixtures`):

- `refresh_self_liveness` unit tests (`owner_state.rs:1739/1765/1779`): migrate `bool` → enum
  (`Refreshed` / `Fresh` / and the missing-cert `Refreshed`).
- New: a future-stamped cert yields `ClockRegressed{skew_secs}` with the correct skew, does
  **not** re-sign, and leaves the timestamp unchanged.
- New: `self_liveness_future_skew_secs` returns `Some(skew)` when future-stamped, `None` when
  healthy/absent.
- Heartbeat test `heartbeat_once_noop_on_regressed_clock` (`liveness_heartbeat.rs:190`):
  behavior unchanged (still a no-op, timestamp preserved) — update assertion to the enum.
- Panel path: a `get_owner_state` snapshot under a future-stamped cert exposes
  `self_clock_regressed_skew_secs = Some(skew)`; under a pre-epoch clock it does not stamp a
  `timestamp = 0` cert (regression test for the footgun).

Frontend (vitest / tsc):

- `tsc --noEmit` covers the new optional DTO field.
- DevicesPanel banner: presentational; add a focused vitest only if a natural service/logic
  seam exists (the field-present → banner-visible mapping), else covered by tsc + the manual
  testing checklist (ZEB-224).

Full CI-parity sweep before PR: `cargo fmt --all -- --check`, `cargo clippy --locked
--all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked
--workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`.

## Scope guardrails (YAGNI)

- **No fabricated / monotonic-`Instant` time** (rejected posture).
- **No new persisted state** — the cert `timestamp` is the floor; the replay-tracker sidecar
  and CRDT doc are untouched.
- **No change** to the 15-day re-sign threshold, the 30-day freshness window, `LivenessCert`,
  `add_liveness`, or the CRDT/LWW merge.
- **No `harmony-owner` change** — all edits are client-side.
- Frontend: **one** informational banner, no redesign, no new actions.

## Files

- `src-tauri/src/owner_state.rs` — `LivenessRefreshOutcome` enum; `refresh_self_liveness`
  signature + regressed-detection + warn; `self_liveness_future_skew_secs` helper;
  `OwnerStateView.self_clock_regressed_skew_secs`; view-builder populates it; unit tests.
- `src-tauri/src/liveness_heartbeat.rs` — map the enum; remove the duplicate future-warn;
  update the regressed-clock test.
- `src-tauri/src/owner_commands.rs` — panel path: pre-epoch skip + `outcome.wrote()` gate.
- `src/lib/components/DevicesPanel.svelte` — conditional clock-regressed banner.

## Out of scope / follow-ups

- Monotonic or peer-attested **auto-recovery** for a persistently-broken clock (rejected
  above) — revisit only if unattended headless recovery becomes a requirement.
- A general host-clock-health indicator beyond liveness (e.g. surfacing skew detected via
  sibling HLCs) — not needed here.
