# ZEB-852 T-DISCOVERY — Discovery-Register Stamp Bounds Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound untrusted peer wall-clock stamps in five discovery/reachability registers so a fast-clocked peer/sibling can no longer pin a routing slot, rank itself first, or evict honest entries.

**Architecture:** Reuse the house `clock_trust` module (no new constants/helpers). Clamp local non-replicated projections (`min(stamp, now)` / `clamp_future`), reject at the one stored replicated register and the one ingest-verify site, and derive-local the field that lives outside the signed preimage. Design: `docs/superpowers/specs/2026-08-02-zeb-852-t-discovery-stamp-bounds-design.md`.

**Tech Stack:** Rust (workspace under `src-tauri/`), `cargo nextest`, existing `clock_trust` policy module.

## Global Constraints

- **One module:** `clock_trust`. Constants: `MAX_FORWARD_SKEW_MS` (5 min = 300_000, control tier), `DISPLAY_SKEW_TOLERANCE_MS` (30 min = 1_800_000, display tier). Helpers: `clamp_future(stamp, now, tolerance) -> u64` (= `stamp.min(now+tolerance)`), `reject_future(stamp, now, tolerance) -> bool` (= `stamp.saturating_sub(now) > tolerance`, inclusive boundary), `receiver_now_ms() -> Option<u64>`, `wall_exceeds_forward_skew(wall_ms, Option<u64>) -> bool` (control tier; `None ⇒ false`). **Never introduce a new constant or skew value.**
- **Tier by concern:** control tier (`MAX_FORWARD_SKEW_MS`) for stamps that gate routing (RB durable slot, D2 butler-set order + merge). Display tier (`DISPLAY_SKEW_TOLERANCE_MS`) for pure discovery ordering (C7). D4's sort clamp collapses to `now` (tier-agnostic).
- **Fail-open on a bad LOCAL clock:** ingest-reject sites (C7) read `receiver_now_ms()` and treat `None` as **apply-all** (never reject on an unreadable clock, never substitute `0`). Clamp sites reuse the resolver's existing `now_ms()` (symmetric with the clamp already present in the same function).
- **Reject vs clamp:** clamp only *local non-replicated projections*; **reject** the one *stored replicated* register (D2-MERGE `FleetNetDoc::merge_from`) — a clamped stored value is receiver-dependent and diverges.
- **Never grow a signed preimage** (ABK): derive `stamped_at_ms` locally at receipt; do not add it to the signed bytes.
- **Discrimination tests:** every task ships both halves — poison stopped **and** an honest in-range entry still wins/ranks — plus a fail-open pin wherever a receiver clock is threaded.
- **Gates (run from `src-tauri/`):** `cargo fmt --all -- --check`; `cargo clippy --locked -p harmony-app --all-targets --features test-fixtures --no-deps -- -D warnings`; scoped `cargo nextest run --locked --features test-fixtures` for the touched module. Full `--workspace --all-targets` sweep at the end (converge/final).

---

## File Structure

- `src-tauri/src/reachability_resolver.rs` — RB: clamp `DurableCrdt | FleetSibling` HLC arm in `update_with_source`.
- `src-tauri/src/fleet_net.rs` — D2: upper-bound filter + sort-key clamp in `butler_set_order`; D2-MERGE: reject future `seen_at` in `FleetNetDoc::merge_from`.
- `src-tauri/src/community_relay_resolver.rs` — D4: rank on `min(ad_at, now)` in `relays_for_community`; store-side clamp in `update`.
- `src-tauri/src/library_directory.rs` — C7: thread receiver clock into `verify_announce`, reject future `listed_at`; update call sites.
- `src-tauri/src/community_address_book.rs` + `src-tauri/src/address_book_sync.rs` — ABK: stamp `stamped_at_ms = now` on the peer-ingest path only.

Each task begins by **reading the target file** for the exact current code (line numbers below are from recon and may have drifted — locate by symbol). Then follow TDD: failing test → confirm fail → minimal implementation → confirm pass → commit.

---

### Task 1: RB — clamp durable/sibling HLC in the reachability resolver

**Files:**
- Modify: `src-tauri/src/reachability_resolver.rs` (`ReachabilityResolver::update_with_source`, the source→HLC match, ~line 428)
- Test: same file, `#[cfg(test)]` module

**Interfaces:**
- Consumes: `clock_trust::MAX_FORWARD_SKEW_MS` (already aliased locally as `FUTURE_SKEW_TOLERANCE_MS` at ~line 46), the existing `self.now_ms()`.
- Produces: no signature change — an internal clamp only.

**Context:** The forward-skew clamp already covers `effective_announced_at_ms` and the `PkarrLive` HLC arm, but the `DurableCrdt | FleetSibling` arm passes the HLC through untouched. Post-ZEB-815 that HLC is peer-signed (`ingest_verified_row` feeds `DurableCrdt` a peer-signed `row.at`), so a future `hlc.wall_ms` wins `lww_newer` and pins the durable routing slot for process life. The stale comment ("Durable-CRDT HLCs are authored by the owner's own device") must be corrected.

Current shape (recon quote):
```rust
let skew_ceiling = self.now_ms().saturating_add(FUTURE_SKEW_TOLERANCE_MS);
let effective_announced_at_ms = payload.announced_at_ms.min(skew_ceiling);
let hlc = match source {
    ReachabilitySource::PkarrLive => Hlc { wall_ms: hlc.wall_ms.min(skew_ceiling), ..hlc },
    ReachabilitySource::DurableCrdt | ReachabilitySource::FleetSibling => hlc, // NOT clamped
};
```

- [ ] **Step 1: Read the file** around `update_with_source`, the `ReachabilitySource` match, `lww_newer`, `now_ms`, and the existing tests. Confirm the exact arm and `skew_ceiling` variable name.

- [ ] **Step 2: Write the failing test.** Add to the test module. Drive the resolver so a `DurableCrdt` record with `hlc.wall_ms = now + 1 year` does NOT win the durable slot over an honest record at `hlc.wall_ms ≈ now`, and that an honest *newer in-range* record DOES win. Use the resolver's existing test constructors / `now_ms` seam (mirror an existing `update_with_source` test). Representative assertion intent:
```rust
// future durable HLC must not pin the slot:
assert_eq!(resolver.durable_slot_owner(community), honest_owner);
// honest in-range newer still wins:
assert_eq!(resolver.durable_slot_owner(community), newer_honest_owner);
```

- [ ] **Step 3: Run it, confirm it fails** (`cargo nextest run -p harmony-app <test_name> --features test-fixtures`). Expected: the future record currently pins the slot → assertion fails.

- [ ] **Step 4: Implement the clamp.** In the `DurableCrdt | FleetSibling` arm, clamp `wall_ms` symmetrically with the `PkarrLive` arm:
```rust
ReachabilitySource::DurableCrdt | ReachabilitySource::FleetSibling =>
    Hlc { wall_ms: hlc.wall_ms.min(skew_ceiling), ..hlc },
```
Replace the stale comment with one noting the durable/sibling HLC now arrives peer-signed via the address book (ZEB-815) and is bounded by the receiver's control-tier skew ceiling; every peer re-clamps at its own ingest (receiver-independent).

- [ ] **Step 5: Run the test, confirm it passes.** Then run the whole resolver test module to catch regressions in `freshest()`/display ordering.

- [ ] **Step 6: Commit.**
```bash
git add src-tauri/src/reachability_resolver.rs
git commit -m "fix(zeb-852): RB — clamp durable/sibling reachability HLC to control-tier skew ceiling"
```

---

### Task 2: D2 — fleet-net butler-set order (filter + sort clamp) and merge-register reject

**Files:**
- Modify: `src-tauri/src/fleet_net.rs` (`butler_set_order` ~lines 194-219; `FleetNetDoc::merge_from` ~line 142)
- Test: same file, `#[cfg(test)]` module

**Interfaces:**
- Consumes: `clock_trust::MAX_FORWARD_SKEW_MS`, the existing `fresh_butler_set` helper (`reachability_record.rs:37`, both-sided `bs_at` bound) as the pattern to mirror, a receiver `now` (use the same `now`/clock the surrounding code already has — read the file for it).
- Produces: no public signature change.

**Context (two parts, both in this file):**
- **D2 (read-side):** `butler_set_order` filters with only a lower freshness bound and sorts descending, so a fast-clocked sibling ranks slot 0 and is published to other owners (they route butler deposits to it). Recon quote:
```rust
.filter(|(_, row)| row.seen_at.wall_ms >= stale_before_ms)   // LOWER bound only
...
let w = row_b.seen_at.wall_ms.cmp(&row_a.seen_at.wall_ms);   // descending — future wins slot 0
```
- **D2-MERGE (stored register):** `FleetNetDoc::merge_from` LWWs rows by `seen_at.is_strictly_newer_than`; a future `seen_at` freezes the replicated row. This is a stored replicated register → **reject** (never clamp).

- [ ] **Step 1: Read the file** around `butler_set_order`, its `stale_before_ms`/`now` source, the sort closure, and `FleetNetDoc::merge_from`. Confirm variable names and where `now` is available in each. Read `fresh_butler_set` in `reachability_record.rs` to mirror its upper bound.

- [ ] **Step 2: Write the failing tests (both parts).**
  - `butler_set_order_sweeps_and_deranks_future_sibling`: a sibling with `seen_at = now + 1 year` is (a) filtered out (upper bound) and (b) even if within the window, does not rank ahead of an honest fresh sibling (sort clamp). An honest fresh sibling still ranks ahead of an honest stale one.
  - `merge_from_rejects_future_seen_at`: merging a row with `seen_at.wall_ms = now + 1 year` does NOT replace an honest current row; an honest *newer in-range* row IS adopted; unreadable/`0`-ish clock ⇒ apply-all (adopts) — match whatever clock seam `merge_from` uses.

- [ ] **Step 3: Run them, confirm they fail.**

- [ ] **Step 4a: Implement D2 read-side.** Add the upper-bound clause to the filter (mirror `fresh_butler_set`: drop rows with `seen_at.wall_ms > now + BUTLER_SET_FRESHNESS_MS`) and clamp the sort key to `min(seen_at.wall_ms, now)` in the comparator:
```rust
.filter(|(_, row)| {
    row.seen_at.wall_ms >= stale_before_ms
        && row.seen_at.wall_ms <= now.saturating_add(BUTLER_SET_FRESHNESS_MS) // upper bound (mirror fresh_butler_set)
})
...
let key = |r: &Row| r.seen_at.wall_ms.min(now); // clamp sort key to now
let w = key(row_b).cmp(&key(row_a));
```
(Use the actual freshness constant / row type names from the file.)

- [ ] **Step 4b: Implement D2-MERGE reject.** At the `merge_from` LWW, before adopting an incoming row, reject when its `seen_at.wall_ms` exceeds the receiver's control-tier ceiling. Fail-open when the receiver clock is unreadable. Sketch:
```rust
if clock_trust::wall_exceeds_forward_skew(incoming.seen_at.wall_ms, clock_trust::receiver_now_ms()) {
    continue; // do not adopt a future-stamped row (T-OWNER pattern)
}
```
(If `merge_from` already threads a `now`, reuse it via `reject_future(.., now, MAX_FORWARD_SKEW_MS)` instead of `receiver_now_ms()`; keep it symmetric with the file's existing clock source. `None`/unreadable ⇒ apply-all.)

- [ ] **Step 5: Run both tests + the fleet_net module, confirm pass.**

- [ ] **Step 6: Commit.**
```bash
git add src-tauri/src/fleet_net.rs
git commit -m "fix(zeb-852): D2 — bound butler-set order (filter+sort clamp) and reject future seen_at at merge"
```

---

### Task 3: D4 — community relay resolver sort clamp + store clamp

**Files:**
- Modify: `src-tauri/src/community_relay_resolver.rs` (`relays_for_community` ~lines 62-68; `update` ~line 39)
- Test: same file, `#[cfg(test)]` module

**Interfaces:**
- Consumes: the resolver's existing `now_ms` seam; `COMMUNITY_RELAY_ADVERTISERS_MAX` (= 4).
- Produces: no signature change.

**Context:** The read sort ranks by raw `ad_at` desc and truncates to 4 slots. The sole ingest caller already clamps `ad_at` to `now + 5 min`, but `fresh_relay_entry` accepts up to `now + 15 min`, so four ceiling-stamped advertisers fill all four rendezvous slots and censor honest advertisers at `now`. Recon quote:
```rust
// store (line 39): LWW by raw ad_at
Some(existing) if existing.ad_at >= payload.ad_at => {}
// read (62-68): sort by raw ad_at desc, truncate to 4
.filter_map(|(_, p)| fresh_relay_entry(p, now_ms).map(|e| (p.ad_at, e)))
fresh.sort_by(|a, b| b.0.cmp(&a.0));
fresh.truncate(COMMUNITY_RELAY_ADVERTISERS_MAX);
```

- [ ] **Step 1: Read the file** around `relays_for_community`, `update`, `fresh_relay_entry`, and `now_ms`.

- [ ] **Step 2: Write the failing test** `relay_slots_not_censored_by_future_advertisers`: with the 4 slots seeded by four advertisers stamped at `now + skew_ceiling` and one honest advertiser at `now`, the honest advertiser is NOT truncated out of the returned set; and among honest advertisers a fresher one still ranks ahead of a staler one.

- [ ] **Step 3: Run it, confirm it fails.**

- [ ] **Step 4: Implement.** In `relays_for_community`, rank on the clamped key `p.ad_at.min(now_ms)` instead of raw `p.ad_at` (both the tuple key and the sort). In `update` (`:39`), clamp the stored `ad_at` to the control-tier ceiling as defense-in-depth (safe — local non-replicated projection):
```rust
.filter_map(|(_, p)| fresh_relay_entry(p, now_ms).map(|e| (p.ad_at.min(now_ms), e)))
fresh.sort_by(|a, b| b.0.cmp(&a.0));
```

- [ ] **Step 5: Run the test + module, confirm pass.**

- [ ] **Step 6: Commit.**
```bash
git add src-tauri/src/community_relay_resolver.rs
git commit -m "fix(zeb-852): D4 — rank relay slots on min(ad_at, now) + store-side clamp"
```

---

### Task 4: C7 — reject future `listed_at` at library-directory ingest

**Files:**
- Modify: `src-tauri/src/library_directory.rs` (`verify_announce` ~line 437; its call sites)
- Test: same file, `#[cfg(test)]` module

**Interfaces:**
- Consumes: `clock_trust::receiver_now_ms() -> Option<u64>`, `clock_trust::reject_future` (or `wall_exceeds_forward_skew`-style), `clock_trust::DISPLAY_SKEW_TOLERANCE_MS`.
- Produces: `verify_announce` gains a receiver-clock parameter (e.g. `now_ms: Option<u64>`) and a new reject variant on `AnnounceVerifyError` (e.g. `ListedAtTooFarInFuture`). All call sites pass `clock_trust::receiver_now_ms()` (tests may pass an explicit `Some(now)` / `None`).

**Context:** `verify_announce` bounds name/description and verifies the signature but never bounds `announce.listed_at`, and has no clock parameter. `listed_at` wins the per-community LWW (pins top of discovery) AND is never the min in cap-eviction (immune → evicts honest libraries). It is inside the signed CBOR (authenticated) but self-attested. Fail-open on unreadable clock.

- [ ] **Step 1: Read the file** around `verify_announce`, `AnnounceVerifyError`, the LWW (`is_strictly_newer_than` ~line 705), the cap-eviction oldest-select (~lines 829-849), and `snapshot` sort (~lines 961-971). Grep for all `verify_announce(` call sites: `rg 'verify_announce\(' src-tauri/`.

- [ ] **Step 2: Write the failing tests:**
  - `verify_announce_rejects_future_listed_at`: future `listed_at` (`now + 1 year`) with `Some(now)` → `Err(ListedAtTooFarInFuture)`.
  - `verify_announce_accepts_in_range`: an in-range announce verifies; an older in-range announce verifies too.
  - `verify_announce_none_now_is_apply_all`: `now_ms = None` ⇒ the future announce **verifies** (fail-open).

- [ ] **Step 3: Run them, confirm they fail** (compile-fail first if the signature changes — update the test calls to the new arity, then observe the logic failure).

- [ ] **Step 4: Implement.** Add `now_ms: Option<u64>` to `verify_announce`; after the length/signature checks, add:
```rust
if let Some(now) = now_ms {
    if clock_trust::reject_future(
        announce.listed_at.wall_ms, now, clock_trust::DISPLAY_SKEW_TOLERANCE_MS,
    ) {
        return Err(AnnounceVerifyError::ListedAtTooFarInFuture);
    }
}
// None ⇒ apply-all (unreadable local clock never rejects an honest announce)
```
Add the `ListedAtTooFarInFuture` variant. Update every production call site to pass `clock_trust::receiver_now_ms()`.

- [ ] **Step 5: Run tests + module, confirm pass.** Also `rg 'verify_announce\(' src-tauri/` again to confirm no call site was missed (compile would fail otherwise).

- [ ] **Step 6: Commit.**
```bash
git add src-tauri/src/library_directory.rs
git commit -m "fix(zeb-852): C7 — reject future listed_at in library verify_announce (display tier, fail-open)"
```

---

### Task 5: ABK — derive `stamped_at_ms` locally at peer ingest

**Files:**
- Modify: `src-tauri/src/address_book_sync.rs` (`ingest_verified_row` ~line 207, the peer-ingest path) and/or `src-tauri/src/community_address_book.rs` (`upsert` ~lines 175-178)
- Test: `src-tauri/src/community_address_book.rs` `#[cfg(test)]` module (and/or address_book_sync tests)

**Interfaces:**
- Consumes: the receiver `now_ms` already available at ingest (the same `now_ms` used by the existing `upsert` clamp).
- Produces: no wire/signature change; `stamped_at_ms` on peer-ingested rows becomes the receipt time.

**Context:** `stamped_at_ms` is **outside** the signed preimage (`verify_inner_signature` covers `(payload, actor, at)` only), so an attacker re-seals a validly-signed row with a bumped `stamped_at_ms`; clamped to `now + 5 min` it still wins the book LWW (`:201`) and refreshes the TTL indefinitely. Fix = **Option 2**: on the peer-ingest path, set `stamped_at_ms = now_ms` (ignore the wire value). **Do NOT** re-stamp self-authored or disk-loaded rows (those must keep their own stamp, or every load refreshes the TTL). The existing 5-min clamp stays as belt-and-suspenders.

Current `upsert` (recon quote — note it is shared by self/disk/peer paths, so the derive must happen on the *peer* path, not unconditionally inside `upsert`):
```rust
let effective = row.stamped_at_ms.min(now_ms.saturating_add(ADDRBOOK_SKEW_TOLERANCE_MS));
row.stamped_at_ms = effective;
if existing.stamped_at_ms >= effective { return UpsertOutcome::IgnoredOlder; }
```

- [ ] **Step 1: Read** `ingest_verified_row` (address_book_sync.rs) and `upsert` (community_address_book.rs). Confirm which callers of `upsert` are peer-ingest vs self-authored vs disk-load. Decide the seam: set `row.stamped_at_ms = now_ms` in `ingest_verified_row` before calling `upsert` (cleanest — keeps `upsert` path-agnostic), OR thread an `origin`/`stamp_locally: bool` into `upsert`. Prefer the ingest-site assignment.

- [ ] **Step 2: Write the failing tests:**
  - `peer_ingest_stamps_receipt_time_not_wire_value`: ingest a peer row with `stamped_at_ms = now + 1 year`; assert the stored row's `stamped_at_ms ≈ now` (receipt), and that a replay with a bumped stamp does NOT win the LWW over the honest current row / does not refresh the TTL past it.
  - `self_authored_row_keeps_its_stamp` and `disk_loaded_row_not_restamped`: the non-peer paths preserve `stamped_at_ms` (guard against re-stamping regressions).

- [ ] **Step 3: Run them, confirm they fail.**

- [ ] **Step 4: Implement.** On the peer-ingest path only, set `row.stamped_at_ms = now_ms` before `upsert` (so the wire value is discarded at receipt). Leave `upsert`'s existing clamp in place. Add a short comment: `stamped_at_ms is outside the signed preimage (ZEB-852 ABK) — derive it locally at peer receipt; do not trust the wire value.`

- [ ] **Step 5: Run tests + both modules, confirm pass.**

- [ ] **Step 6: Commit.**
```bash
git add src-tauri/src/address_book_sync.rs src-tauri/src/community_address_book.rs
git commit -m "fix(zeb-852): ABK — derive address-book stamped_at_ms at peer ingest (outside signed preimage)"
```

---

## Self-Review

**Spec coverage:** RB → Task 1; D2 + D2-MERGE → Task 2; D4 → Task 3; C7 → Task 4; ABK → Task 5. D1 explicitly out of scope (already fixed #580) — no task, by design. All five in-scope findings covered.

**Placeholder scan:** each task names the exact file + symbol, the exact `clock_trust` call, the tier, and concrete test intents. Line numbers flagged as recon-drift — implementer reads the file first (Step 1 of every task).

**Type/name consistency:** `clock_trust` symbols (`MAX_FORWARD_SKEW_MS`, `DISPLAY_SKEW_TOLERANCE_MS`, `clamp_future`, `reject_future`, `receiver_now_ms`, `wall_exceeds_forward_skew`) verified to exist by recon. `verify_announce` arity change (Task 4) is threaded through all call sites in the same task. `butler_set_order`/`merge_from`/`relays_for_community`/`upsert` are the confirmed current symbols.

**Clamp-vs-reject discipline:** clamp for local projections (Tasks 1, 2 read-side, 3); reject for the stored replicated register (Task 2 merge) and the ingest-verify site (Task 4); derive-local for the outside-preimage field (Task 5). Matches the design's taxonomy.
