# ZEB-863: Peer-independent monotonic tiebreak for bounded-store eviction

**Ticket:** ZEB-863 (ZEB-851 follow-up). Surfaced by Qodo's review of PR #587.
**Scope:** `src-tauri/src/storage_records.rs` only (blast radius verified: all record
constructors and the eviction primitives live in this one file; external
`.received_at_ms`/`.pinned_at_ms` readers are unrelated types that merely share the
field name).

## Problem

The bounded-store eviction primitives pick the victim with an ordering key that mixes
a *non-monotonic* wall-clock stamp with a *peer-controlled* tiebreak:

- `evict_overflow<R>` (storage_records.rs:811): victim = `min (Reverse(received_at_ms), owner)`
  — evict newest receipt first, ties broken by lexicographically smallest `owner`.
- `evict_pins` (storage_records.rs:766): victim = `min (live, Reverse(pinned_at_ms), owner)`
  — dead-weight first, then newest pin, then smallest `owner`.

Two residuals (both verified against current code):

- **#3 peer-controlled tiebreak.** When two rows share the same `received_at_ms`
  (same wall-clock ms — realistic under a flood doing many inserts/ms), the tiebreak
  is a peer-controlled address. An attacker flooding in the same millisecond as an
  honest row, choosing keys that sort *above* the honest key, makes the honest
  (smaller) key the min-owner victim. A peer-controlled input influences the eviction
  *decision* — exactly what ZEB-851 set out to remove.
- **#4 local-clock rollback.** Newest-first assumes `now_ms` is monotonic. If this
  replica's local wall clock rolls backward while the store is populated, freshly
  received rows get *smaller* `received_at_ms` than established rows, inverting the
  protection: established rows look "newest" and are evicted.

Shared root cause: the eviction key depends on a peer-controlled value (`owner`) and a
non-monotonic value (wall-clock ms).

## Fix

Introduce a **local, strictly-monotonic, per-store sequence counter**, stamped at
every insert, and make it the *sole* eviction ordering key.

### 1. Counter

Add `insert_seq: u64` to `StorageRecordStore`. All store access is `&mut self`
(single-threaded), so a plain `u64` suffices — no atomics. A private helper hands out
and advances it:

```rust
fn next_insert_seq(&mut self) -> u64 {
    let s = self.insert_seq;
    self.insert_seq += 1;
    s
}
```

Strictly increasing, one stamp per insert ⇒ **unique by construction** (no two live
records ever share a seq).

### 2. Record field

Add `seq: u64` to `PledgeListRecord`, `BackupSetRecord`, `HostingReportRecord`, and
`StorageSignerPin` (stays `Copy`). This is an **addition, not a repurpose**:

- `HostingReportRecord.received_at_ms` is dual-use — it drives staleness pruning
  (`retain` at :646) and the UI "report age" (getter at :640). It keeps that job.
- `StorageSignerPin.pinned_at_ms` round-trips to disk and stays as-is.
- `received_at_ms` on pledge/backup keeps its "local receipt clock" doc meaning; only
  its role *as the eviction key* moves to `seq`.

`seq` is **not persisted** — local and re-derived on load, exactly like `received_at_ms`
already is for pledge/backup (they reload as `0`). No disk-schema change, no version
bump, no wire/CRDT impact.

### 3. Stamping sites

Stamp `seq = self.next_insert_seq()` (or `store.insert_seq` during reload) at every
record construction:

- Live ingest via `lww_insert`: pledge (:434), backup (:506), hosting (:573). Compute
  the seq *before* the `&mut self.<map>` borrow. A fresh seq is stamped on **every**
  insert including an LWW-replace (`UpdatedNewer`) — a re-received row correctly becomes
  "newest" in eviction order, consistent with `received_at_ms` already being reset to
  `now_ms` on replace.
- Pin creation: the `None` (first-pin) arm at :372. (Rebind-reject and matching-pin
  arms do not insert.)
- Reload in `new()`: pledge (:217), backup (:237), pin (:260). Stamped in the existing
  owner-sorted disk order (`save()` sorts by owner), so the assignment is deterministic
  and peer-independent. All reloaded rows are "established" and get low seq; a
  post-restart flood gets higher seq and self-evicts first — preserving the existing
  post-restart invariant (which today relies on `received_at_ms: 0` on reload).

### 4. Eviction keys on `seq`, owner dropped

`owner` (peer-controlled) leaves the comparison entirely. Because `seq` is unique, the
min is deterministic with no tiebreak:

```rust
// evict_overflow<R>(map, seq: impl Fn(&R) -> u64)
let victim = map
    .iter()
    .min_by_key(|(_, r)| std::cmp::Reverse(seq(r)))
    .map(|(owner, _)| owner.clone());
```

```rust
// evict_pins — keep the live/dead partition, seq within it, owner dropped
let victim = pins
    .iter()
    .min_by_key(|(owner, pin)| {
        let live = pledges.contains_key(*owner)
            || backups.contains_key(*owner)
            || hosting.contains_key(*owner);
        (live, std::cmp::Reverse(pin.seq))
    })
    .map(|(owner, _)| owner.clone());
```

`min_by_key` returns the first element with the minimum key on a tie; since `seq` is
unique (and, for pins, unique within each liveness class), no tie can occur, so the
result is deterministic despite `HashMap`'s unordered iteration. The `evict_overflow<R>`
closure parameter is renamed `received_at_ms` → `seq`; the three call sites change from
`|r| r.received_at_ms` to `|r| r.seq`; the four reload/live-ingest `evict_*` call sites
are otherwise unchanged.

## Testing

Black-box through the public ingest API where possible (the `on_*_sample` methods take
`now_ms` as a parameter, so a same-ms collision is forced deterministically):

1. **`same_ms_flood_does_not_evict_honest`** (the regression test — fails on current
   tiebreak). Insert an honest owner with a *small* address at `now_ms = T`; then flood
   `MAX_TRACKED_OWNERS` attacker owners with *larger* addresses, all at the same
   `now_ms = T`. Under today's `(Reverse(received_at_ms), owner)` key every row shares
   `received_at_ms = T`, so the victim is the smallest owner = the honest row → honest
   evicted (assert fails). With `seq`, the honest row was inserted first (lowest seq),
   so the newest attacker row is evicted → honest survives.
2. **`local_clock_rollback_keeps_established`**. Populate the store at a high `now_ms`,
   then insert fresh rows at a *lower* `now_ms` (simulated rollback) to push over cap.
   Under wall-clock ordering the established (high-ms) rows would be "newest" and evicted;
   with `seq` the established rows have lower seq and survive; the rollback-era insert
   self-evicts.
3. **`insert_seq_is_unique_and_monotonic`**. Pin the uniqueness invariant the
   owner-drop relies on: distinct seq across a sequence of inserts, strictly increasing.
4. **`evict_pins` parity**. A same-ms/rollback variant for the pin path (dead-weight
   priority preserved; among same-liveness pins, oldest seq survives).
5. **Migrate existing direct-call tests** (`evict_overflow_is_flood_proof_newest_received_first`,
   `evict_pins_prefers_dead_then_newest_zeb679`, and the pledge eviction test at ~:1272)
   to construct records with `seq` and assert on seq ordering. This is expected churn
   from changing the primitive, not incidental drift.

## Non-goals / invariants preserved

- No disk-schema, wire, or CRDT change. `seq` is local and non-serialized.
- `evict_pins` dead-weight-first priority and the `MAX_SIGNER_PINS`/`MAX_TRACKED_OWNERS`
  caps are unchanged.
- The "freeze-when-full / newcomer self-evicts" tradeoff (inherent to flood resistance)
  is unchanged — this only makes the *ordering within that policy* peer-independent and
  monotonic.
- `received_at_ms` (hosting staleness + UI age) and `pinned_at_ms` (disk round-trip)
  semantics untouched.
