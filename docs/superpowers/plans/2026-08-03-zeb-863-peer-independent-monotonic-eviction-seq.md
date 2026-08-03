# ZEB-863 Peer-Independent Monotonic Eviction Seq — Implementation Plan

> **For agentic workers:** Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make bounded-store eviction order depend only on a local, strictly-monotonic
per-store sequence counter, removing the peer-controlled `owner` tiebreak and the
wall-clock-monotonicity assumption.

**Architecture:** Add `insert_seq: u64` to `StorageRecordStore`, a `seq: u64` field to
the four record structs, stamp it at every insert (live ingest, pin creation, reload),
and key `evict_overflow`/`evict_pins` on `seq` alone. Local, non-serialized — no
disk/wire/CRDT change.

**Tech Stack:** Rust, `cargo nextest`. Single file: `src-tauri/src/storage_records.rs`.

## Global Constraints

- Build/test from `src-tauri/`. Iterative gate: `scripts/test-select --context task`.
  Paste the printed `round=… bucket=…` summary line into the task report so the
  selection is auditable (per CLAUDE.md's iterative-test-selection convention).
  Final pre-PR sweep: `cargo nextest run --locked --workspace --all-targets --features test-fixtures` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo fmt --all -- --check`.
- `seq` is NOT persisted (no `*OnDisk` struct changes, no `RECORDS_FILE_VERSION` bump).
- Do not change `received_at_ms` (hosting staleness/UI) or `pinned_at_ms` (disk) semantics.
- `evict_pins` keeps dead-weight-first priority; only its wall-ms sub-key → seq, owner dropped.
- `owner` must not appear in any eviction comparison key.

---

### Task 1: seq infrastructure + eviction keyed on seq (regression-test-driven)

**Files:**
- Modify: `src-tauri/src/storage_records.rs`
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `StorageRecordStore.insert_seq: u64`, `StorageRecordStore::next_insert_seq(&mut self) -> u64`, `seq: u64` on `PledgeListRecord`/`BackupSetRecord`/`HostingReportRecord`/`StorageSignerPin`.
- `evict_overflow<R>(map, seq: impl Fn(&R) -> u64)` (closure param renamed from `received_at_ms`).

- [ ] **Step 1: Write the failing regression test.** In `mod tests`, add
  `same_ms_flood_does_not_evict_honest`: build a `StorageRecordStore::new(None)`; ingest
  one honest pledge list whose owner address sorts *small* (e.g. all-`00`) at `now_ms = T`;
  then ingest `MAX_TRACKED_OWNERS` attacker pledge lists with *larger* owner addresses,
  all at the same `now_ms = T`. Assert the honest owner is still present
  (`store.pledge_list(&honest).is_some()`). Use the public `on_pledge_list_sample` path
  with valid signed payloads (mirror the existing `signed_pledge_bytes` helper).

- [ ] **Step 2: Run it — expect FAIL.** `scripts/test-select --context task` (or
  `-E 'test(same_ms_flood_does_not_evict_honest)'`). Under the current
  `(Reverse(received_at_ms), owner)` key all rows share `received_at_ms = T`, so the
  smallest owner (honest) is evicted → assertion fails.

- [ ] **Step 3: Add the counter + field.** Add `insert_seq: u64` to `StorageRecordStore`
  (init `0` in `new()`); add `next_insert_seq(&mut self) -> u64`; add `seq: u64` to the
  four structs.

- [ ] **Step 4: Stamp seq at every insert.** Live ingest (pledge :434, backup :506,
  hosting :573): `let seq = self.next_insert_seq();` before the `lww_insert` borrow, set
  `seq` in the record literal. Pin `None` arm (:372): stamp before the insert. Reload:
  stamp `seq` via `store.next_insert_seq()`. **Pins** must be sorted by `pinned_at_ms`
  (their persisted local clock) BEFORE stamping, so the oldest pin gets the lowest `seq`
  and an established dead ratchet pin is never evicted by owner position after a restart
  (CodeRabbit, Major). **Pledge/backup** have no persisted local clock (`received_at_ms`
  resets to 0), so they are stamped in disk order — a reload-time eviction among them is
  tampered-file-only (honest saves never exceed the cap), not a peer-steering surface.

- [ ] **Step 5: Key eviction on seq.** `evict_overflow<R>`: rename the closure param to
  `seq`, replace the victim selection with
  `map.iter().min_by_key(|(_, r)| std::cmp::Reverse(seq(r))).map(|(o, _)| o.clone())`.
  `evict_pins`: `pins.iter().min_by_key(|(owner, pin)| { let live = …; (live, std::cmp::Reverse(pin.seq)) }).map(|(o, _)| o.clone())`.
  Update the three `evict_overflow` call closures `|r| r.received_at_ms` → `|r| r.seq`
  (reload :267/:268, live :442/:514/:581).

- [ ] **Step 6: Migrate existing direct-call tests to `seq`.** Fix the record literals in
  `evict_overflow_is_flood_proof_newest_received_first` (~:1618/:1631), the pledge
  eviction test (~:1272), and `pin` closure in `evict_pins_prefers_dead_then_newest_zeb679`
  (~:1575) to include `seq`, and assert on seq ordering (highest seq evicted first).
  Update the `evict_overflow(...)` closures in those tests to `|r| r.seq`.

- [ ] **Step 7: Run gates.** `scripts/test-select --context task` green (regression test
  now passes); `cargo fmt --all`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` clean.

- [ ] **Step 8: Commit.** `ZEB-863: local seq counter as sole eviction key (Task 1)`.

---

### Task 2: rollback + uniqueness + pin-parity hardening tests

**Files:**
- Test: `src-tauri/src/storage_records.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: everything from Task 1.

- [ ] **Step 1: Write `local_clock_rollback_keeps_established`.** Populate the store to
  near cap at a high `now_ms` (e.g. `10_000`), then ingest fresh rows at a *lower*
  `now_ms` (e.g. `1_000`) to exceed cap. Assert the established (earlier-inserted) rows
  survive and the rollback-era insert is the one evicted. Run — expect PASS (seq ignores
  wall clock).

- [ ] **Step 2: Write `insert_seq_is_unique_and_monotonic`.** Ingest a short sequence of
  records; assert their stamped `seq` values are strictly increasing and distinct
  (pin the uniqueness invariant the owner-drop relies on).

- [ ] **Step 3: Write `evict_pins_same_ms_prefers_oldest_seq`.** Same-`pinned_at_ms`
  pins across owners; assert dead-weight-first still holds and, within a liveness class,
  the highest-seq (newest) pin is evicted, honest oldest-seq pin survives — independent
  of owner address ordering.

- [ ] **Step 4: Run gates.** `scripts/test-select --context task` green; fmt; clippy clean.

- [ ] **Step 5: Full CI-parity sweep.**
  `cargo nextest run --locked --workspace --all-targets --features test-fixtures` +
  clippy `--all-targets` + `cargo fmt --all -- --check`. All green.

- [ ] **Step 6: Commit.** `ZEB-863: rollback + uniqueness + pin-parity tests (Task 2)`.
