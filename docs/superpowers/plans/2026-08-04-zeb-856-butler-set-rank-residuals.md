# ZEB-856 Butler-Set Rank Residuals Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the three butler-set ranking residuals in `fleet_net.rs` — drop the peer-inflatable `logical` ranking tiebreak (R2), reject future-dated `pinned_at`/`set_at` at merge (R3), and document the accepted near-future-clamp residual with the pin as its mitigation (R1) — with zero new fail-open surface.

**Architecture:** One file, `src-tauri/src/fleet_net.rs`. R2 edits the single `.sort_by` in `butler_set_order` (which `selection_view` delegates to). R3 adds two control-tier `clock_trust::wall_exceeds_forward_skew` guards in `merge_from_bounded`, mirroring the `seen_at` reject already there. R1 is a doc-comment + a canary test — no ranking-code change. No signatures, no wire format, no new modules change.

**Tech Stack:** Rust, `cargo nextest`, the existing `crate::clock_trust` module, `Hlc` (`owner_state_types`).

## Global Constants (verbatim, do not change)

- `BUTLER_SET_MAX_ENTRIES = 2`, `BUTLER_SET_FRESHNESS_MS = 15*60*1000`, `BUTLER_SET_REFRESH_MS = FRESHNESS/2` (`src/butler_deposit.rs`). `butler_set_order` returns ALL fresh rows sorted; the cap is applied by callers, not here.
- `Hlc { wall_ms: u64, logical: u32, device_id: String }`; derives `Ord` on `(wall_ms, logical, device_id)`; `is_strictly_newer_than == (self > other)`. **Untouched** — stays the merge LWW comparator.
- `clock_trust::wall_exceeds_forward_skew(wall_ms: u64, receiver_now: Option<u64>) -> bool`: control tier `MAX_FORWARD_SKEW_MS = 5*60*1000`, inclusive boundary, `None ⇒ false` (apply-all).
- `merge_from_bounded(&mut self, remote: FleetNetDoc, receiver_now: Option<u64>) -> MergeOutcome` — `receiver_now` already sampled once at the top; reuse it.
- Test helpers already in the `#[cfg(test)] mod tests`: `hlc(wall_ms: u64, device_id: &str) -> Hlc` (logical hardcoded 0), `row(ep_byte: u8, relay: &str, seen_at: Hlc) -> FleetNetRow`, `petname(name: &str, set_at: Hlc) -> FleetNetPetname`. `use crate::owner_state_types::Hlc;` is already in scope in that module, so building `Hlc { .. }` directly (to set a non-zero `logical`) is available.

## Gates (every task's verify + the final sweep)

- Fmt: `cd src-tauri && cargo fmt --all -- --check`
- Single-crate test during dev: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fleet_net)'`
- Clippy (before PR): `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- Final pre-PR: `scripts/test-select --full` (CI-parity full sweep) — `fleet_net.rs` is lib code, so a lib change relinks integration binaries; the full sweep is the backstop.

---

### Task 1: R2 — drop `logical` from the butler ranking tiebreak

**Files:**
- Modify: `src-tauri/src/fleet_net.rs` (the `.sort_by` in `butler_set_order`, ~271–292)
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `butler_set_order(doc: &FleetNetDoc, stale_before_ms: u64) -> Vec<(String, FleetNetRow)>`
- Produces: no signature change. Post-task the sort key is `(clamp(wall_ms), device_id)` — `logical` no longer participates in ranking.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests`:

```rust
#[test]
fn butler_rank_logical_inflation_does_not_win_slot_zero() {
    let window = crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
    let now: u64 = 2_000_000_000_000;
    let stale_before = now - window;

    let mut doc = FleetNetDoc::default();
    // Honest present row: wall = now, logical = 0, LOWER device-id key.
    doc.devices
        .insert("dev-honest".into(), row(0x01, "relay.honest", hlc(now, "d")));
    // Inflation attempt: SAME clamped wall (now), logical = u32::MAX, HIGHER
    // device-id key. Pre-fix the descending-`logical` secondary ranked this at
    // slot 0; post-fix (logical dropped) the device-id tiebreak keeps honest ahead.
    doc.devices.insert(
        "dev-zzz-evil".into(),
        row(
            0x02,
            "relay.evil",
            Hlc {
                wall_ms: now,
                logical: u32::MAX,
                device_id: "evil".into(),
            },
        ),
    );

    let order = butler_set_order(&doc, stale_before);
    assert_eq!(
        order[0].0, "dev-honest",
        "a self-inflated `logical` must NOT win butler slot 0 over an honest present row"
    );
}

#[test]
fn butler_rank_clamped_wall_tie_orders_by_device_id_deterministically() {
    let window = crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
    let now: u64 = 2_000_000_000_000;
    let stale_before = now - window;

    let mut doc = FleetNetDoc::default();
    // Identical clamped wall and logical → only device-id decides.
    for key in ["dev-ccc", "dev-aaa", "dev-bbb"] {
        doc.devices
            .insert(key.into(), row(0x01, "relay", hlc(now, "d")));
    }
    let keys: Vec<String> = butler_set_order(&doc, stale_before)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        keys,
        vec![
            "dev-aaa".to_string(),
            "dev-bbb".to_string(),
            "dev-ccc".to_string()
        ],
        "clamped-wall ties must order by ascending device-id, deterministically"
    );
}
```

- [ ] **Step 2: Run the tests to verify the first FAILS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(butler_rank_logical_inflation)'`
Expected: FAIL — pre-fix the `logical = u32::MAX` row wins slot 0, so `order[0].0 == "dev-zzz-evil"`, not `"dev-honest"`. (The determinism test passes pre-fix too — all `logical == 0` — that's fine; it guards the post-fix behavior.)

- [ ] **Step 3: Remove the `logical` secondary comparator**

In `butler_set_order`'s `.sort_by`, replace this block:

```rust
        // Secondary: descending logical.
        // KNOWN RESIDUAL (ZEB-856): `logical` is a peer-self-stamped HLC counter, so
        // after the wall clamp a future-dated sibling can set `logical = u32::MAX` to
        // win a clamped-wall tie against an honest present row. Left as-is here on
        // purpose: clamp-to-now is the fail-open-safe choice (it never deprioritizes a
        // live device), and closing this peer-controlled tiebreak — plus the
        // near-future clamp-to-top and the pin/petname freeze — is tracked together in
        // ZEB-856 so the butler-rank residuals are addressed with one coherent policy
        // rather than piecemeal. (Also applies to the `selection_view` ordering below.)
        let l = row_b.seen_at.logical.cmp(&row_a.seen_at.logical);
        if l != std::cmp::Ordering::Equal {
            return l;
        }
        // Tertiary: ascending device_id (deterministic tiebreak)
        id_a.cmp(id_b)
```

with:

```rust
        // Final tiebreak: ascending device_id.
        // ZEB-856 (R2): the descending-`logical` secondary was REMOVED here. In this
        // cross-device ranking `logical` is a per-device HLC counter with no
        // cross-device meaning, and it is peer-self-stamped (a sibling could set
        // `logical = u32::MAX` to win a clamped-wall tie for butler slot 0). The
        // remaining key `(clamp(wall_ms), device_id)` is fully bounded/fixed:
        // clamped-wall is receiver-capped (ZEB-852) and `device_id` is an
        // identity-bound hash (not freely grindable) that is unique per row → a
        // strict total order, so determinism is preserved. `logical` stays in
        // `Hlc::is_strictly_newer_than` for the merge LWW, where same-device
        // causality legitimately needs it. `selection_view` delegates here, so it
        // inherits this policy (there is exactly one sort site).
        id_a.cmp(id_b)
```

- [ ] **Step 4: Run tests to verify they PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fleet_net)'`
Expected: PASS — both new tests green; the existing `butler_set_order_sweeps_and_deranks_future_sibling` (all `logical == 0`) still green.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && cargo fmt --all --manifest-path src-tauri/Cargo.toml
git add src-tauri/src/fleet_net.rs
git commit -m "ZEB-856 (R2): drop peer-inflatable logical from butler ranking tiebreak"
```

---

### Task 2: R3 — reject future `pinned_at` / petname `set_at` at merge

**Files:**
- Modify: `src-tauri/src/fleet_net.rs` (`merge_from_bounded`, pin block ~187 + petname loop ~201)
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: `merge_from_bounded(&mut self, remote, receiver_now: Option<u64>)`, `crate::clock_trust::wall_exceeds_forward_skew`.
- Produces: no signature change. Post-task a `pinned_at`/`set_at` more than `MAX_FORWARD_SKEW_MS` ahead of `receiver_now` is dropped; `receiver_now == None` applies all.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests`:

```rust
#[test]
fn merge_rejects_future_pinned_at_then_honest_later_pin_wins() {
    let now: u64 = 2_000_000_000_000;
    let six_min = 6 * 60 * 1000;

    let mut local = FleetNetDoc::default();
    local.pinned = Some("dev-p0".into());
    local.pinned_at = hlc(now, "owner");

    // Far-future (> 5-min control tier) stamp: must be rejected, not win the LWW.
    let mut poison = FleetNetDoc::default();
    poison.pinned = Some("dev-evil".into());
    poison.pinned_at = hlc(now + six_min, "owner");
    local.merge_from_bounded(poison, Some(now));
    assert_eq!(
        local.pinned.as_deref(),
        Some("dev-p0"),
        "a future-dated pinned_at must be REJECTED, not win the LWW"
    );

    // Register must remain LIVE (not frozen): an honest later pin still wins.
    let mut honest = FleetNetDoc::default();
    honest.pinned = Some("dev-p2".into());
    honest.pinned_at = hlc(now + 1000, "owner");
    local.merge_from_bounded(honest, Some(now + 2000));
    assert_eq!(
        local.pinned.as_deref(),
        Some("dev-p2"),
        "the pin register must stay live after rejecting a poison stamp (not frozen)"
    );
}

#[test]
fn merge_accepts_in_tolerance_future_pinned_at() {
    let now: u64 = 2_000_000_000_000;
    let four_min = 4 * 60 * 1000;

    let mut local = FleetNetDoc::default();
    local.pinned = Some("dev-p0".into());
    local.pinned_at = hlc(now, "owner");

    let mut near = FleetNetDoc::default();
    near.pinned = Some("dev-p1".into());
    near.pinned_at = hlc(now + four_min, "owner");
    local.merge_from_bounded(near, Some(now));
    assert_eq!(
        local.pinned.as_deref(),
        Some("dev-p1"),
        "a pin within the 5-min control tier must still be applied (reject is > tier only)"
    );
}

#[test]
fn merge_rejects_future_petname_set_at_then_honest_later_wins() {
    let now: u64 = 2_000_000_000_000;
    let six_min = 6 * 60 * 1000;
    let key = "dev-x".to_string();

    let mut local = FleetNetDoc::default();
    local
        .petnames
        .insert(key.clone(), petname("orig", hlc(now, "owner")));

    let mut poison = FleetNetDoc::default();
    poison
        .petnames
        .insert(key.clone(), petname("evil", hlc(now + six_min, "owner")));
    local.merge_from_bounded(poison, Some(now));
    assert_eq!(
        local.petnames.get(&key).map(|p| p.name.as_str()),
        Some("orig"),
        "a future-dated petname set_at must be REJECTED"
    );

    let mut honest = FleetNetDoc::default();
    honest
        .petnames
        .insert(key.clone(), petname("real", hlc(now + 1000, "owner")));
    local.merge_from_bounded(honest, Some(now + 2000));
    assert_eq!(
        local.petnames.get(&key).map(|p| p.name.as_str()),
        Some("real"),
        "the petname register must stay live after rejecting a poison stamp"
    );
}

#[test]
fn merge_none_clock_applies_future_pin_and_petname() {
    let now: u64 = 2_000_000_000_000;
    let one_year: u64 = 365 * 24 * 60 * 60 * 1000;
    let key = "dev-x".to_string();

    let mut local = FleetNetDoc::default();
    local.pinned = Some("dev-p0".into());
    local.pinned_at = hlc(now, "owner");
    local
        .petnames
        .insert(key.clone(), petname("orig", hlc(now, "owner")));

    // Unreadable local clock (None) ⇒ apply-all: a bad LOCAL clock must never
    // drop honest pin/petname updates, even far-future ones.
    let mut remote = FleetNetDoc::default();
    remote.pinned = Some("dev-far".into());
    remote.pinned_at = hlc(now + one_year, "owner");
    remote
        .petnames
        .insert(key.clone(), petname("far", hlc(now + one_year, "owner")));
    local.merge_from_bounded(remote, None);
    assert_eq!(
        local.pinned.as_deref(),
        Some("dev-far"),
        "None clock ⇒ apply-all for the pin"
    );
    assert_eq!(
        local.petnames.get(&key).map(|p| p.name.as_str()),
        Some("far"),
        "None clock ⇒ apply-all for the petname"
    );
}
```

- [ ] **Step 2: Run the tests to verify they FAIL**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(merge_rejects_future) + test(merge_accepts_in_tolerance) + test(merge_none_clock)'`
Expected: `merge_rejects_future_pinned_at...` and `merge_rejects_future_petname...` FAIL (pre-fix the poison stamp wins the LWW, so the pin/petname is `dev-evil`/`evil`). `merge_accepts_in_tolerance...` and `merge_none_clock...` pass pre-fix (they assert the poison-free / apply paths, which are unchanged) — they guard against over-rejecting.

- [ ] **Step 3: Add the pin-pair reject guard**

In `merge_from_bounded`, replace:

```rust
        // Pin LWW pair: remote strictly newer by pinned_at → take both fields.
        if remote.pinned_at.is_strictly_newer_than(&self.pinned_at) {
```

with:

```rust
        // Pin LWW pair: remote strictly newer by pinned_at → take both fields.
        // ZEB-856 (R3): reject a future-dated pin stamp before the LWW, mirroring
        // the seen_at reject above. `pinned_at` is a STORED replicated register, so
        // a stamp implausibly ahead of the receiver clock would win the LWW and
        // FREEZE the pin (no honest later pinned_at could be strictly-newer),
        // pinning butler routing permanently. Reject — never clamp: a clamped
        // stored value is receiver-dependent and would diverge across peers.
        // Control tier; `receiver_now == None` ⇒ apply-all (a bad LOCAL clock must
        // never drop an honest owner pin). `pinned_at` is owner-stamped (vs
        // `seen_at` self-stamped by the subject sibling) — same freeze hazard.
        if !crate::clock_trust::wall_exceeds_forward_skew(remote.pinned_at.wall_ms, receiver_now)
            && remote.pinned_at.is_strictly_newer_than(&self.pinned_at)
        {
```

- [ ] **Step 4: Add the petname reject guard**

In the petname loop, replace:

```rust
        for (device_id, remote_pn) in remote.petnames {
            match self.petnames.get(&device_id) {
```

with:

```rust
        for (device_id, remote_pn) in remote.petnames {
            // ZEB-856 (R3): drop a petname whose self-stamped set_at is implausibly
            // future — same stored-register freeze hazard as the pin and seen_at.
            // Reject-not-clamp; `receiver_now == None` ⇒ apply-all.
            if crate::clock_trust::wall_exceeds_forward_skew(remote_pn.set_at.wall_ms, receiver_now)
            {
                continue;
            }
            match self.petnames.get(&device_id) {
```

- [ ] **Step 5: Run tests to verify they PASS**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(fleet_net)'`
Expected: PASS — all four new tests green; existing `merge_from_rejects_future_seen_at`, `petname_lww_*` still green.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client && cargo fmt --all --manifest-path src-tauri/Cargo.toml
git add src-tauri/src/fleet_net.rs
git commit -m "ZEB-856 (R3): reject future-dated pinned_at/set_at at merge (freeze defense)"
```

---

### Task 3: R1 — document the accepted near-future residual + canary test

**Files:**
- Modify: `src-tauri/src/fleet_net.rs` (`butler_set_order` doc-comment ~235)
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: `butler_set_order`, `crate::butler_deposit::BUTLER_SET_REFRESH_MS`.
- Produces: no code-behavior change — a doc-comment and a canary test only.

- [ ] **Step 1: Write the canary test**

Append to `mod tests`:

```rust
#[test]
fn butler_rank_r1_now_stamper_leads_then_pin_overrides() {
    let window = crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
    let refresh = crate::butler_deposit::BUTLER_SET_REFRESH_MS;
    let now: u64 = 2_000_000_000_000;
    let stale_before = now - window;

    let mut doc = FleetNetDoc::default();
    // A "now"-stamper (a sibling always claiming maximum freshness).
    doc.devices
        .insert("dev-nowstamper".into(), row(0x01, "relay.ns", hlc(now, "d")));
    // An honest device, one refresh interval stale.
    doc.devices.insert(
        "dev-honest".into(),
        row(0x02, "relay.h", hlc(now - refresh, "d")),
    );

    // (a) ACCEPTED RESIDUAL (ZEB-856 R1): the fresher self-stamp leads. This is
    // deliberately UNFIXED — a demotion here would be fail-open (could route
    // deposits to a dead device). Canary: if a future change alters ranking so
    // the now-stamper no longer leads, this trips and forces a fresh decision.
    let order = butler_set_order(&doc, stale_before);
    assert_eq!(
        order[0].0, "dev-nowstamper",
        "R1: the freshest self-stamp leads (accepted residual)"
    );

    // (b) MITIGATION: the owner's pin overrides freshness ranking (and R3 keeps
    // the pin un-freezable). Pinning the honest device puts it at slot 0.
    doc.pinned = Some("dev-honest".into());
    let order = butler_set_order(&doc, stale_before);
    assert_eq!(
        order[0].0, "dev-honest",
        "R1 mitigation: the owner pin overrides freshness ranking"
    );
}
```

- [ ] **Step 2: Run it to verify it PASSES as-is**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(butler_rank_r1)'`
Expected: PASS — R1 needs no code change; the test documents current behavior. (It is a canary, not a red→green test. This is the deliberate exception to red-first: R1's deliverable is documentation of accepted behavior, so the test must be green on unchanged code.)

- [ ] **Step 3: Add the accepted-residual doc paragraph**

In `butler_set_order`'s doc-comment, insert before the final paragraph. Replace:

```rust
/// This is the heart of the fleet-net-v1 contribution: it maps the
/// replicated `FleetNetDoc` to an ordered advisory butler-set for the
/// pkarr advertisement.
```

with:

```rust
/// **Accepted residual (ZEB-856 R1 — near-future clamp-to-top).** The ranking
/// key is peer-self-stamped and this function has no independent liveness
/// signal, so a sibling that stamps `seen_at.wall_ms = now` leads honest
/// siblings sitting at `now − Δ` (their stamp ages up to one
/// `BUTLER_SET_REFRESH_MS` between refreshes). Left UNFIXED by decision:
/// `wall = now` is indistinguishable from an honestly-just-refreshed device,
/// and any structural demotion is fail-open — it could push a mildly
/// clock-skewed honest device below a stale one and route butler deposits to a
/// dead device. The exposure is bounded (the clamp caps inflation at `now`, R2
/// removed the `logical` axis, `device_id` is fixed) and the sanctioned
/// override is the owner's PIN, which ZEB-856 R3 makes un-freezable. Pinned by
/// the `butler_rank_r1_now_stamper_leads_then_pin_overrides` canary test.
///
/// This is the heart of the fleet-net-v1 contribution: it maps the
/// replicated `FleetNetDoc` to an ordered advisory butler-set for the
/// pkarr advertisement.
```

- [ ] **Step 4: Verify fmt + the whole module still green**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo nextest run --locked --features test-fixtures -E 'test(fleet_net)'`
Expected: fmt clean; all `fleet_net` tests PASS.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/fleet_net.rs
git commit -m "ZEB-856 (R1): document accepted near-future clamp residual + pin-override canary"
```

---

### Task 4: Final gate + PR

- [ ] **Step 1: Clippy (compiles every target, incl. the test module)**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: no warnings.

- [ ] **Step 2: CI-parity full sweep**

Run: `scripts/test-select --full`
Expected: green (lib change relinks integration binaries; this is the backstop). Paste the summary line into the PR/report.

- [ ] **Step 3: Push + open PR (fire CodeRabbit once)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeblith/zeb-856-butler-set-rank-residuals
gh pr create --repo zeblithic/harmony-client --base main \
  --title "ZEB-856: butler-set rank residuals (drop logical tiebreak, reject future pin/petname stamps)" \
  --body "<disposition body: R2/R3/R1 summary, the single-sort-site correction, Closes ZEB-856>"
```

Then one `@coderabbitai review` comment (exactly once; never re-fire).

## Self-Review

**Spec coverage:** R2 → Task 1. R3 (pin + petname) → Task 2. R1 accept+document → Task 3. Minor (`now` recovery) → intentionally no task (spec decision: leave signature). Tests 1–6 from the spec → Task 1 (2 tests: logical-discrimination, determinism), Task 2 (4 tests: pin-reject, pin-accept-boundary, petname-reject, none-apply-all), Task 3 (R1 canary with pin-override). All covered.

**Placeholder scan:** all code blocks are concrete and compilable; the only free-text placeholder is the PR body (`<disposition body...>`), filled at Task 4 Step 3 — acceptable (it is prose written at PR time, not code).

**Type consistency:** `Hlc.logical` is `u32` → tests use `u32::MAX`. `merge_from_bounded` takes `receiver_now: Option<u64>` → tests pass `Some(now)` / `None`. `wall_exceeds_forward_skew(u64, Option<u64>)` arg order matches. `butler_set_order` returns `Vec<(String, FleetNetRow)>` → tests index `order[0].0` (the String key) and map `.0`. Helper `row(u8, &str, Hlc)` / `hlc(u64, &str)` / `petname(&str, Hlc)` signatures match every call.
