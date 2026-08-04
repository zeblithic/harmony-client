# ZEB-856 — Butler-set rank residuals (T-DISCOVERY follow-up) Design

**Ticket:** ZEB-856 (follow-up to ZEB-852 / ZEB-831 T-DISCOVERY)
**File under change:** `src-tauri/src/fleet_net.rs` (plus `clock_trust` reuse — no new API)
**Date:** 2026-08-04

## Goal

Close the three residual ways a peer-self-stamped field can bias the advisory
**butler set** ranking or freeze an owner control, left open after ZEB-852's D2
fix. Do it with the fail-open discipline established across ZEB-846/847/852:
bound what the receiver can bound, reject (never clamp) poison stamps in a
*stored replicated register*, and never add a demotion that could route butler
deposits to a dead device.

## Background — what ZEB-852 already shipped

`butler_set_order(doc, stale_before_ms)` maps the replicated `FleetNetDoc` to an
ordered advisory butler set (max `BUTLER_SET_MAX_ENTRIES = 2`) advertised in the
pkarr blob. Other owners read it to decide **where to deposit DMs** for this
owner. ZEB-852 (D2):

- Recovered the receiver clock inside the function: `now = stale_before_ms +
  BUTLER_SET_FRESHNESS_MS` (all callers derive `stale_before_ms = now −
  FRESHNESS`). `FRESHNESS = 15 min`, `REFRESH = 7.5 min`.
- Added a two-sided freshness **filter** (drop rows below `stale_before_ms` or
  more than one window past `now`).
- **Clamped** the primary sort key to `min(wall_ms, now)` (Shape B — transient
  sort, clamp not reject).
- Added a `seen_at` forward-skew **reject** in `merge_from` (Shape A — stored
  register, reject not clamp; `wall_exceeds_forward_skew(..., receiver_now)`,
  apply-all when the local clock is unreadable).

The butler ranking key is **entirely peer-self-stamped** (`wall_ms`, `logical`
both chosen by the subject device). The only receiver-authored signals are
freshness-window membership (`now`) and the fixed `device_id` hash. There is no
independent liveness oracle in this pure CRDT projection. That framing drives
every decision below.

## Constants / invariants (do not change)

- `BUTLER_SET_MAX_ENTRIES = 2`, `BUTLER_SET_FRESHNESS_MS = 15·60·1000`,
  `BUTLER_SET_REFRESH_MS = FRESHNESS/2` (`src/butler_deposit.rs`).
- `Hlc` derives `Ord` over `(wall_ms, logical, device_id)`;
  `Hlc::is_strictly_newer_than == (self > other)` (`owner_state_types.rs:366`).
  **Untouched** — it stays the merge LWW comparator (same-device causality).
- `clock_trust::wall_exceeds_forward_skew(wall_ms, receiver_now: Option<u64>)`
  → control tier (`MAX_FORWARD_SKEW_MS = 5 min`), inclusive boundary, `None ⇒
  false` (apply-all). `receiver_now_ms()` is the only trusted-clock source.

## Verified-source correction to the ticket's R2 note

Jake's 2026-08-02 decision comment and the ticket say the `logical` fix applies
to **two** sort sites (`butler_set_order` + a `selection_view` sort at
"~1194–1258"). **That is stale.** The current file has **exactly one** sort site
(`butler_set_order`, the single `.sort_by` at ~271); `selection_view` (line 406)
delegates to `butler_set_order` and does no sorting of its own. Lines 1194–1258
are now test code. So R2 is a **single-location** change and `selection_view`
inherits it for free — the "keep the two policies in sync" concern is obsolete.

## The three residuals + the settled policy

### R2 — drop `logical` from the butler *ranking* tiebreak (settled by Jake 2026-08-02)

In `butler_set_order`'s `.sort_by`, the comparison is between **different**
devices' rows. `logical` is a per-device HLC counter meaningful only for
same-device causality; comparing it across devices is semantically empty **and**
peer-inflatable (`u32`, self-stamped → set `logical = u32::MAX` to win a
clamped-wall tie).

**Change:** remove the descending-`logical` secondary comparator. The sort key
becomes `(clamp(wall_ms), device_id)`:
- `clamp(wall_ms) = min(wall_ms, now)` — receiver-bounded (ZEB-852).
- `device_id` — key-fixed (a hash; not freely grindable), and unique per row, so
  it remains a **strict total order** → determinism preserved (no ties left).

`logical` stays in `Hlc::is_strictly_newer_than` for the merge, where it is a
legitimate same-device causal tiebreak. Replace the `fleet_net.rs:278` KNOWN
RESIDUAL comment with a "closed by ZEB-856 (R2)" note explaining why `device_id`
is the intended final tiebreak. `selection_view` inherits the fix.

### R3 — reject future `pinned_at` and petname `set_at` at merge (settled)

`merge_from_bounded` currently gates only the device-row `seen_at`. The
`pinned`+`pinned_at` LWW **pair** and each `petnames[k].set_at` LWW are ungated,
so a future-dated `pinned_at` (or `set_at`) wins its LWW and **freezes** — no
honest later stamp can ever be `is_strictly_newer_than` a far-future one, pinning
(or petname-freezing) permanently.

**Change:** gate both, mirroring the `seen_at` reject already in the same
function:
- Pin pair: skip the whole pin merge when
  `wall_exceeds_forward_skew(remote.pinned_at.wall_ms, receiver_now)`.
- Petnames: `continue` past any key whose
  `wall_exceeds_forward_skew(remote_pn.set_at.wall_ms, receiver_now)`.

Reject-not-clamp because these are **stored replicated registers** (a clamped
stored value is receiver-dependent → cross-peer divergence — the ZEB-852/847
doctrine). Same `receiver_now` already sampled once at the top of
`merge_from_bounded`; `None ⇒ apply-all` (a bad local clock must never drop
honest pin/petname updates). This restores the owner's pin as a **reliable
slot-0 override**, which is the mitigation R1 leans on.

Threat-surface note (into the code comment): `pinned_at` is owner-stamped (a
compromised/skewed *owner* device) vs. `seen_at` self-stamped by the subject
sibling — same freeze hazard, same control-tier reject.

### R1 — near-future clamp-to-top: **accept + document** (Jake's call)

After R2, a sibling that self-stamps `wall = now` (not even future) still leads
honest live siblings sitting at `now − Δ` (their stamp ages up to one 7.5-min
refresh interval between refreshes). It is bounded (≤ `now`, no `logical`, fixed
`device_id`) but a malicious *enrolled* device can reliably claim maximum
freshness.

**Decision: no demotion, no quantization.** Rationale, recorded in the code as an
accepted-residual comment:
1. Self-reported `wall = now` is indistinguishable from an honestly-just-
   refreshed device — there is no liveness oracle in `butler_set_order`.
2. Any structural demotion is **fail-open-risky**: it can push a mildly clock-
   skewed *honest* device below a genuinely-stale one, routing butler deposits to
   a dead device — an availability loss worse than the residual.
3. The set has 2 entries and depositors try the set; slot 0 is not a single
   point of failure.
4. The **owner's pin is the sanctioned override**, and R3 makes it un-freezable.
   An owner who cares pins their always-on device.

So R1 is closed by *documentation + the R3-hardened pin*, plus a test that pins
both the accepted ranking behavior and the pin-as-override.

### Minor — `now` recovery footgun: leave as-is

`butler_set_order` recovers `now = stale_before_ms + FRESHNESS`. Exact for all 3
callers; a future caller passing a different cutoff would mis-bound. **Decision:
keep the signature** (YAGNI — 3 callers, contract already documented in the
doc-comment). No plumbing change. (Optional, non-blocking: a `debug_assert` is
not added because the inversion is definitional, not a runtime condition.)

## Data flow (unchanged shape)

`FleetNetDoc` (replicated CRDT) → `merge_from` (R3 gates ingest of poison
pin/petname stamps) → `butler_set_order` (R2 removes the `logical` inflation
axis; R1 accepted) → `build_butler_set` / `selection_view` → pkarr blob / dial
selection. No signatures change; no new module; no wire-format change (all fields
already exist).

## Error handling / fail-open semantics

- Local clock unreadable (`receiver_now_ms() → None`): R3 applies **all** remote
  pin/petname updates (never drop honest state on a bad *local* clock). Identical
  to ZEB-852's `seen_at` behavior.
- R2 never drops or reorders honest rows relative to each other beyond removing a
  meaningless cross-device axis; determinism via unique `device_id`.
- R1 introduces **no** new drop/demote path — zero fail-open surface.

## Testing (discrimination per residual — vary the axis under test, not just `wall_ms`)

Add to `fleet_net.rs`'s existing `#[cfg(test)]` module (reuse `row`/`hlc`/
`petname` helpers):

1. **R2 — `logical` no longer wins a clamped-wall tie.** Two rows at the same
   clamped wall (e.g. both at `now`); the one with `logical = u32::MAX` has the
   **higher** `device_id`. Post-fix, slot 0 is the honest lower-`device_id` row,
   NOT the `logical`-inflated one. (Fixture must vary `logical` *and* set
   `device_id` order adversarially, or it masks the axis.)
2. **R2 — determinism/total-order.** Same clamped wall, distinct `device_id`s,
   equal `logical`: order is strictly by ascending `device_id` and stable across
   runs.
3. **R3 — future `pinned_at` rejected.** Local pin P0 @ `pinned_at = now`; merge a
   remote pin P1 @ `pinned_at = now + 6 min` (> control tier). Post-fix the pin
   stays P0 and a later **honest** pin (`now + 1 s`) still wins — proving the
   register did not freeze. Companion: an in-tolerance future pin (`now + 4 min`)
   is still applied (accept boundary).
4. **R3 — future petname `set_at` rejected.** Symmetric to (3) for one petname
   key; a later honest `set_at` still wins after a poison stamp is rejected.
5. **R3 — apply-all on `None` clock.** Drive `merge_from_bounded(.., None)` with a
   far-future `pinned_at`/`set_at`: both are applied (fail-open on a bad local
   clock).
6. **R1 — accepted residual is explicit + pin overrides.** (a) A sibling at
   `wall = now` leads an honest sibling at `now − Δ` (documents the accepted
   behavior, so a future "fix" that changes it trips this test and forces a
   decision). (b) With the owner's pin set to the honest `now − Δ` device, that
   device leads regardless — the pin is the override.

Gates: `cargo fmt --all --check`, `cargo clippy --locked --all-targets --features
test-fixtures --no-deps -- -D warnings`, and the `fleet_net` test module (single
crate). `fleet_net.rs` is lib code, so the touched-binary sweep is the lib +
its test module; the final pre-PR validation is the full-workspace command
`cd src-tauri && cargo nextest run --locked --workspace --all-targets --features
test-fixtures` (NOT `scripts/test-select`, per house rules).

## Out of scope

- Any change to `Hlc` / `is_strictly_newer_than` / the merge causal comparator.
- Liveness-signal fold-in for ranking (rejected — bigger architecture, and R1 is
  accepted).
- Quantization of the ranking key (considered for R1, rejected by decision).
- Wire-format / signature changes.
