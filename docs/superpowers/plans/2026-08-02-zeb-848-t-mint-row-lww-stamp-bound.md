# ZEB-848 T-MINT row-LWW stamp-bound Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound peer-supplied `updated_at` at the mint ingest boundary so a far-future or unparseable stamp can no longer win the per-row LWW (or trigger a poison hard-delete) and revert honest ledger edits forever.

**Architecture:** Add two RFC-3339 helpers to the shared `clock_trust` policy module, then gate every peer-`updated_at` consumer in `mint_sync::apply_remote_snapshot` (3 `upsert_*_lww` fns + the deletion-floor loop) to drop remote rows whose stamp is unparseable or beyond `now + MAX_FORWARD_SKEW_MS`. Ingest-only; no wire change; REJECT (not clamp) because this is a stored replicated register.

**Tech Stack:** Rust, `chrono` 0.4 (already a dep), `rusqlite`, existing `clock_trust` module.

## Global Constraints

- CI gates (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Implementers gate lib-only (`cargo nextest run -p harmony-app --lib` / `clippy -p harmony-app --lib`) during iteration; the controller runs the full `--all-targets` sweep at converge/final.
- Reuse `clock_trust`; introduce NO new skew constants. The bound is `clock_trust::MAX_FORWARD_SKEW_MS` (5 min, control tier) — NOT the display tier.
- `reject_future(stamp, now, tol) = stamp.saturating_sub(now) > tol` — inclusive boundary (a stamp exactly at `now + tol` is accepted). Do not change this.
- Fail-open contract: `receiver_now_ms() == None` ⇒ skip the forward-skew check (never drop honest state on a bad *local* clock); an unparseable stamp is STILL rejected (that check needs no clock).
- REJECT, not clamp: drop the offending remote row; never store a clamped receiver-specific value (would diverge across replicas).
- No wire-format change; no per-row HLC; ingest-only. Local edits (in `mint.rs`) never flow through `apply_remote_snapshot`, so the bound cannot reject the user's own `now`-stamped rows.

---

### Task 1: `clock_trust` RFC-3339 helpers

**Files:**
- Modify: `src-tauri/src/clock_trust.rs`
- Test: same file, `#[cfg(test)] mod tests` (add if absent, else extend).

**Interfaces:**
- Consumes: existing `reject_future(stamp: u64, now: u64, tol: u64) -> bool` and `pub const MAX_FORWARD_SKEW_MS: u64` in this module; `chrono` 0.4.
- Produces (used by Task 2):
  - `pub fn parse_rfc3339_ms(stamp: &str) -> Option<u64>`
  - `pub fn reject_rfc3339_future(stamp: &str, now_ms: Option<u64>, tol_ms: u64) -> bool`

- [ ] **Step 1: Write the failing tests**

Add to the module's test section (create `#[cfg(test)] mod tests { use super::*; ... }` if none exists):

```rust
#[test]
fn parse_rfc3339_ms_accepts_both_honest_encodings() {
    // `Z` and `+00:00` at the same instant parse equal.
    let z = parse_rfc3339_ms("2026-08-02T15:00:00Z").unwrap();
    let off = parse_rfc3339_ms("2026-08-02T15:00:00+00:00").unwrap();
    assert_eq!(z, off);
    // Variable fractional precision (production `to_rfc3339()` shape).
    assert!(parse_rfc3339_ms("2026-08-02T15:00:00.789+00:00").is_some());
    // Epoch literal → 0; pre-epoch floors to 0 (ancient, never future).
    assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339_ms("1960-01-01T00:00:00Z"), Some(0));
    // Garbage / empty → None.
    assert_eq!(parse_rfc3339_ms("zzzz"), None);
    assert_eq!(parse_rfc3339_ms(""), None);
}

#[test]
fn reject_rfc3339_future_drops_unparseable_and_far_future() {
    let now: u64 = 1_760_000_000_000;
    let tol = MAX_FORWARD_SKEW_MS;
    let ms_to_str =
        |ms: u64| chrono::DateTime::from_timestamp_millis(ms as i64).unwrap().to_rfc3339();

    // Unparseable → reject regardless of clock (poison; and it sorts above real stamps).
    assert!(reject_rfc3339_future("zzzz", Some(now), tol));
    assert!(reject_rfc3339_future("zzzz", None, tol));

    // Parseable far-future → reject when the clock is readable.
    assert!(reject_rfc3339_future("9999-12-31T23:59:59Z", Some(now), tol));
    // …but fail-open on skew when the local clock is unreadable.
    assert!(!reject_rfc3339_future("9999-12-31T23:59:59Z", None, tol));

    // Inclusive boundary: exactly now+tol is accepted; just beyond is rejected.
    assert!(!reject_rfc3339_future(&ms_to_str(now + tol), Some(now), tol));
    assert!(reject_rfc3339_future(&ms_to_str(now + tol + 1_000), Some(now), tol));
    // Within tolerance and in the past are accepted.
    assert!(!reject_rfc3339_future(&ms_to_str(now + tol - 1_000), Some(now), tol));
    assert!(!reject_rfc3339_future(&ms_to_str(now - 60_000), Some(now), tol));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo nextest run -p harmony-app --lib -E 'test(rfc3339)'`
Expected: FAIL — `parse_rfc3339_ms` / `reject_rfc3339_future` not found.

- [ ] **Step 3: Implement the helpers**

Add near the other `clock_trust` functions:

```rust
/// Parse an RFC-3339 timestamp to non-negative epoch milliseconds.
///
/// Accepts both honest mint encodings — `+00:00` with variable fractional
/// precision (`chrono::Utc::now().to_rfc3339()`) and the fixed `...Z` epoch
/// literal. Pre-epoch instants floor to `0` (ancient — never future).
/// Returns `None` when `stamp` is not valid RFC-3339.
pub fn parse_rfc3339_ms(stamp: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

/// Reject a peer-supplied RFC-3339 wall stamp used as a stored, replicated LWW
/// ordering key. Returns `true` when the remote row MUST be dropped at ingest:
///
/// * the stamp does not parse as RFC-3339 (never a valid ordering key — e.g.
///   `"zzzz"`, which also sorts above every real stamp under a raw string
///   compare), or
/// * it parses to more than `tol_ms` beyond the receiver clock `now_ms`.
///
/// `now_ms == None` (receiver clock unreadable) ⇒ the forward-skew half is
/// skipped (fail-open on skew, matching `receiver_now_ms`'s contract), but an
/// unparseable stamp is still rejected — that check needs no clock.
pub fn reject_rfc3339_future(stamp: &str, now_ms: Option<u64>, tol_ms: u64) -> bool {
    match parse_rfc3339_ms(stamp) {
        None => true,
        Some(ms) => now_ms.is_some_and(|now| reject_future(ms, now, tol_ms)),
    }
}
```

Note: if the crate's MSRV predates `Option::is_some_and` (1.70), use
`now_ms.map_or(false, |now| reject_future(ms, now, tol_ms))` instead — pick
whichever the crate already uses and clippy accepts.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd src-tauri && cargo nextest run -p harmony-app --lib -E 'test(rfc3339)'`
Expected: PASS (both tests).

- [ ] **Step 5: Lint + format**

Run: `cd src-tauri && cargo clippy -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && cargo fmt --all`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/clock_trust.rs
git commit -m "feat(zeb-848): add clock_trust RFC-3339 parse + forward-skew reject helpers"
```

---

### Task 2: Gate peer `updated_at` at the mint ingest boundary

**Files:**
- Modify: `src-tauri/src/mint_sync.rs`
- Test: same file, `#[cfg(test)] mod tests` (`:1284`).

**Interfaces:**
- Consumes: `clock_trust::reject_rfc3339_future`, `clock_trust::receiver_now_ms`, `clock_trust::MAX_FORWARD_SKEW_MS` (Task 1 + existing).
- Changes signatures (internal, all callers updated in this task):
  - `apply_remote_snapshot(conn, remote, account_deletion_floor, now_ms: Option<u64>)`
  - `upsert_account_lww(tx, r, floor, now_ms: Option<u64>)`
  - `upsert_transaction_lww(tx, r, now_ms: Option<u64>)`
  - `upsert_setting_lww(tx, r, now_ms: Option<u64>)`

- [ ] **Step 1: Write the failing discrimination tests**

Add to `#[cfg(test)] mod tests`. Mirror the MintSnapshot / row construction of the existing `apply_lww_replaces_older_local` (`:1395`) and `apply_lww_keeps_newer_local` (`:1369`) tests for boilerplate (row structs: `AccountRow{id,name,created_at,updated_at}`, `TransactionRow{id,transaction_date,amount,currency,account_id,description,metadata,created_at,updated_at,deleted_at}`, `SettingRow{key,value,updated_at}`; `MintSnapshot{accounts,transactions,settings,account_deletion_floor,..}`). Use a fixed receiver clock so there is no wall-clock flakiness:

```rust
// Fixed receiver "now" for all bound tests (≈2025-10-09).
const T_NOW: u64 = 1_760_000_000_000;
fn ms_to_rfc3339(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64).unwrap().to_rfc3339()
}

#[test]
fn poison_far_future_account_row_loses_to_local() {
    let mut conn = fresh_db();
    let local_ts = ms_to_rfc3339(T_NOW - 60_000);
    seed_account(&conn, "a1", "honest", &local_ts);
    // Remote poison: same id, far-future stamp, different name.
    let remote = /* MintSnapshot with one AccountRow{id:"a1", name:"POISON",
                    created_at: local_ts.clone(), updated_at:"9999-12-31T23:59:59Z"} */;
    apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
    // Local name survives (poison rejected, no LWW overwrite).
    assert_eq!(account_name(&conn, "a1"), "honest");
}

#[test]
fn poison_unparseable_account_row_loses_to_local() {
    let mut conn = fresh_db();
    let local_ts = ms_to_rfc3339(T_NOW - 60_000);
    seed_account(&conn, "a1", "honest", &local_ts);
    let remote = /* AccountRow{id:"a1", name:"POISON", updated_at:"zzzz", ..} */;
    apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
    assert_eq!(account_name(&conn, "a1"), "honest");
}

#[test]
fn poison_far_future_transaction_row_does_not_overwrite_amount() {
    let mut conn = fresh_db();
    let local_ts = ms_to_rfc3339(T_NOW - 60_000);
    seed_account(&conn, "a1", "acct", &local_ts);
    seed_tx(&conn, "t1", "a1", "honest", &local_ts); // sets amount, e.g. "10.00"
    let remote = /* TransactionRow{id:"t1", account_id:"a1", amount:"999.99",
                    updated_at:"9999-12-31T23:59:59Z", ..} */;
    apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
    assert_eq!(tx_amount(&conn, "t1"), /* seeded amount */);
}

#[test]
fn poison_setting_row_loses_to_local() {
    let mut conn = fresh_db();
    let local_ts = ms_to_rfc3339(T_NOW - 60_000);
    seed_setting(&conn, "default_currency", "USD", &local_ts);
    let remote = /* SettingRow{key:"default_currency", value:"XXX", updated_at:"zzzz"} */;
    apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
    assert_eq!(setting_value(&conn, "default_currency"), "USD");
}

#[test]
fn legit_newer_remote_still_wins_control() {
    // Gate must NOT over-reject an honest newer edit within tolerance.
    let mut conn = fresh_db();
    seed_account(&conn, "a1", "old", &ms_to_rfc3339(T_NOW - 60_000));
    let remote = /* AccountRow{id:"a1", name:"new", updated_at: ms_to_rfc3339(T_NOW - 1_000)} */;
    apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
    assert_eq!(account_name(&conn, "a1"), "new");
}

#[test]
fn none_clock_fails_open_on_skew_but_still_drops_unparseable() {
    // Unreadable local clock: far-future applies (fail-open on skew)…
    let mut conn = fresh_db();
    seed_account(&conn, "a1", "old", &ms_to_rfc3339(T_NOW - 60_000));
    let remote_future = /* AccountRow{id:"a1", name:"future", updated_at:"9999-12-31T23:59:59Z"} */;
    apply_remote_snapshot(&mut conn, &remote_future, &HashMap::new(), None).unwrap();
    assert_eq!(account_name(&conn, "a1"), "future");
    // …but an unparseable stamp is still dropped even with no clock.
    let remote_garbage = /* AccountRow{id:"a1", name:"GARBAGE", updated_at:"zzzz"} */;
    apply_remote_snapshot(&mut conn, &remote_garbage, &HashMap::new(), None).unwrap();
    assert_eq!(account_name(&conn, "a1"), "future"); // unchanged
}

#[test]
fn control_tier_boundary_discriminates_from_display_tier() {
    let mut conn = fresh_db();
    seed_account(&conn, "a1", "old", &ms_to_rfc3339(T_NOW - 60_000));
    // now + 4 min (< 5 min control tol) → accepted (overwrite).
    let within = /* AccountRow{id:"a1", name:"within", updated_at: ms_to_rfc3339(T_NOW + 4*60_000)} */;
    apply_remote_snapshot(&mut conn, &within, &HashMap::new(), Some(T_NOW)).unwrap();
    assert_eq!(account_name(&conn, "a1"), "within");
    // now + 10 min → rejected under the control tier (would be ACCEPTED at the
    // 30-min display tier — this is the tier discriminator).
    let tier_probe = /* AccountRow{id:"a1", name:"probe", updated_at: ms_to_rfc3339(T_NOW + 10*60_000)} */;
    apply_remote_snapshot(&mut conn, &tier_probe, &HashMap::new(), Some(T_NOW)).unwrap();
    assert_eq!(account_name(&conn, "a1"), "within"); // unchanged — probe rejected
}

#[test]
fn poison_deletion_floor_does_not_hard_delete_or_merge() {
    let mut conn = fresh_db();
    seed_account(&conn, "a1", "keepme", &ms_to_rfc3339(T_NOW - 60_000));
    let mut floor = HashMap::new();
    let remote = /* empty MintSnapshot with account_deletion_floor = {"a1": "9999-12-31T23:59:59Z"} */;
    let merged = apply_remote_snapshot(&mut conn, &remote, &floor, Some(T_NOW)).unwrap();
    // Account NOT hard-deleted; poison NOT merged into the floor.
    assert!(account_exists(&conn, "a1"));
    assert!(!merged.contains_key("a1"));
}

#[test]
fn honest_deletion_floor_still_deletes() {
    // Control for the previous test: an in-tolerance floor stamp still deletes.
    let mut conn = fresh_db();
    seed_account(&conn, "a1", "deleteme", &ms_to_rfc3339(T_NOW - 60_000));
    let remote = /* empty accounts; account_deletion_floor = {"a1": ms_to_rfc3339(T_NOW - 30_000)} */;
    let merged = apply_remote_snapshot(&mut conn, &remote, &HashMap::new(), Some(T_NOW)).unwrap();
    assert!(!account_exists(&conn, "a1"));
    assert!(merged.contains_key("a1"));
}
```

Add small read helpers if the test module lacks them (`account_name`,
`account_exists`, `tx_amount`, `setting_value`, `seed_setting`) — mirror the
existing `seed_account`/`seed_tx` query style.

- [ ] **Step 2: Run to verify they fail (signature + behavior)**

Run: `cd src-tauri && cargo nextest run -p harmony-app --lib -E 'test(poison) + test(tier) + test(none_clock) + test(legit_newer) + test(deletion_floor)'`
Expected: FAIL — first as compile errors (the new `now_ms` arg doesn't exist yet), then, once Step 3 lands, the behavior assertions gate the fix.

- [ ] **Step 3: Thread `now_ms` and add the gates**

3a. `apply_remote_snapshot` (`:89`) — add `now_ms: Option<u64>` as the last
param. Gate the deletion-floor loop; pass `now_ms` to each upsert:

```rust
pub(crate) fn apply_remote_snapshot(
    conn: &mut Connection,
    remote: &MintSnapshot,
    account_deletion_floor: &HashMap<String, String>,
    now_ms: Option<u64>,
) -> Result<HashMap<String, String>, MintSyncError> {
    // …
    for r in &remote.accounts {
        if upsert_account_lww(&tx, r, account_deletion_floor, now_ms)? == UpsertOutcome::Suppressed {
            suppressed_account_ids.insert(r.id.clone());
        }
    }
    for r in &remote.transactions {
        if suppressed_account_ids.contains(&r.account_id) { continue; }
        upsert_transaction_lww(&tx, r, now_ms)?;
    }
    for r in &remote.settings {
        upsert_setting_lww(&tx, r, now_ms)?;
    }
    // …
    for (id, remote_ts) in &remote.account_deletion_floor {
        // Poison deletion stamp: don't hard-delete, don't merge into the floor.
        if crate::clock_trust::reject_rfc3339_future(
            remote_ts, now_ms, crate::clock_trust::MAX_FORWARD_SKEW_MS,
        ) {
            continue;
        }
        // …existing body (local lookup, conditional hard-delete, floor merge)…
    }
    // …
}
```

3b. `upsert_account_lww` (`:165`) — add `now_ms: Option<u64>`; gate at the very
top (before the floor-suppress at `:171`). Return `Applied` on poison-reject so
the account is NOT added to `suppressed_account_ids` (its transactions must be
evaluated on their own merit):

```rust
fn upsert_account_lww(
    tx: &rusqlite::Transaction,
    r: &AccountRow,
    floor: &HashMap<String, String>,
    now_ms: Option<u64>,
) -> Result<UpsertOutcome, MintSyncError> {
    if crate::clock_trust::reject_rfc3339_future(
        &r.updated_at, now_ms, crate::clock_trust::MAX_FORWARD_SKEW_MS,
    ) {
        return Ok(UpsertOutcome::Applied); // poison-rejected; not a deletion suppression
    }
    // …existing body…
}
```

3c. `upsert_transaction_lww` (`:200`) — add `now_ms: Option<u64>`; gate at top,
`return Ok(())` on reject:

```rust
fn upsert_transaction_lww(
    tx: &rusqlite::Transaction,
    r: &TransactionRow,
    now_ms: Option<u64>,
) -> Result<(), MintSyncError> {
    if crate::clock_trust::reject_rfc3339_future(
        &r.updated_at, now_ms, crate::clock_trust::MAX_FORWARD_SKEW_MS,
    ) {
        return Ok(());
    }
    // …existing body…
}
```

3d. `upsert_setting_lww` (`:256`) — add `now_ms: Option<u64>`; gate before the
SQL statement:

```rust
fn upsert_setting_lww(
    tx: &rusqlite::Transaction,
    r: &SettingRow,
    now_ms: Option<u64>,
) -> Result<(), MintSyncError> {
    if crate::clock_trust::reject_rfc3339_future(
        &r.updated_at, now_ms, crate::clock_trust::MAX_FORWARD_SKEW_MS,
    ) {
        return Ok(());
    }
    // …existing INSERT … ON CONFLICT …
}
```

3e. `handle_incoming_decoded` (`:561`) — at the `apply_remote_snapshot(...)`
call, source and pass the receiver clock. Compute
`let now_ms = crate::clock_trust::receiver_now_ms();` and thread it into the call
(if the call is inside a `spawn_blocking`, compute `now_ms` before the closure
and move it in, or call `receiver_now_ms()` inside the closure — either is fine;
one read per snapshot).

3f. Update any remaining `#[cfg(test)]` callers of these four fns (e.g. the
existing `apply_lww_*` / tombstone tests) to pass `Some(<their existing ts basis>)`
or `None`. Existing tests that use ordinary in-range stamps should pass a `now_ms`
large enough not to reject them (e.g. `Some(u64::MAX)` where the test only cares
about relative ordering, or a fixed `Some(T_NOW)` if their stamps predate it) —
choose per test so no existing assertion flips. `None` also works for any test
whose stamps are all parseable and it only exercises ordering (fail-open on skew
keeps them all in play), but `None` would let an unparseable-stamped existing
fixture through unchecked — there are none today, but prefer an explicit
`Some(..)` for tests that assert a *rejection* and `None`/`Some(u64::MAX)` for
pure-ordering fixtures.

- [ ] **Step 4: Run the new + existing mint_sync tests**

Run: `cd src-tauri && cargo nextest run -p harmony-app --lib -E 'test(mint_sync) + test(poison) + test(tier) + test(none_clock) + test(legit_newer) + test(deletion_floor)'`
Expected: PASS — new discrimination tests green; the existing
`apply_lww_*`, `apply_propagates_tombstone`, `apply_resurrects_after_tombstone`,
and `poisoned_envelope_wall_does_not_affect_row_merge_lww` tests still green.

- [ ] **Step 5: Lint + format**

Run: `cd src-tauri && cargo clippy -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings && cargo fmt --all`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/mint_sync.rs
git commit -m "fix(zeb-848): bound peer updated_at at mint ingest (reject unparseable + forward-skew)

Gate every peer updated_at consumer in apply_remote_snapshot — the three
upsert_*_lww fns and the deletion-floor hard-delete/merge — via
clock_trust::reject_rfc3339_future at the control tier. Drops poison rows
(9999.../zzzz) that otherwise win the string LWW forever and revert honest
ledger edits; None-clock fails open on skew but still drops unparseable.
ZEB-831 T-MINT / C3."
```

## Self-Review notes (for the reviewer)

- Spec coverage: Task 1 = the two helpers + unit tests; Task 2 = the 4 gate points + the `now_ms` seam + discrimination tests (poison far-future, poison unparseable, control-newer-wins, None-clock fail-open, deletion-floor poison, control-tier boundary). Every spec test intent maps to a step.
- Type consistency: `reject_rfc3339_future(stamp, now_ms, tol_ms)` signature identical in Task 1 (defn) and Task 2 (call sites); `now_ms: Option<u64>` threaded uniformly.
- The `#[cfg(test)]` caller updates (Step 3f) are the one place existing behavior could regress — the reviewer should confirm no existing assertion flipped and that the full `--all-targets` sweep (controller) is green.
