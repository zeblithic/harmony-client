# ZEB-924: Bounded relay-hold retention — local expiry tombstones vs resurrection-by-merge

Third and final leg of ZEB-913 (R5, Freenet review). Stops a peer merge from
re-arming the 30-day relay-hold TTL indefinitely: once THIS replica expires a
never-acked hold, a bounded local tombstone suppresses its resurrection. No
wire/canonical-bytes change, no change to how non-tombstoned entries merge.

## 1. Verified premises (receipts, re-verified 2026-08-12 on main `3993ea00`)

1. **TTL + sweep cadence.** `RELAY_HOLD_TTL_MS = 30 days`
   (`community_relay.rs:132`), `RELAY_HOLD_GC_INTERVAL_MS = 10 min` (`:138`).
   The GC task (`lib.rs:12786-12857`) runs `gc(now)` under the doc lock every
   tick; `notify_dirty + flush_now` on removal, `persist_now` on stamp-only
   growth (ZEB-862).
2. **TTL clock = local first observation.** Lazily stamped
   `first_observed_ms.entry(key).or_insert(now_ms)` on the first sweep that
   sees each entry (`community_relay_hold_crdt.rs:199-201`); peer-supplied
   `held_at` deliberately does NOT drive TTL (ZEB-851 backdate-proofing).
3. **The defect mechanism.** After the retain pass, the side-map is pruned to
   live keys (`:210-212`) — the replica FORGETS when it first saw an expired
   entry. A still-holding peer's next anti-entropy merge re-inserts the entry
   (`merge_from` `:99-101`, insert-once → `Changed`), and the next sweep
   re-stamps it fresh. Documented in-code as bounded only by dataset caps
   (`:176-181`). Retention of a never-acked hold in a continuously-merging
   fleet is unbounded.
4. **Only never-acked holds are exposed.** Coverage GC destroys acked holds in
   ~10-20 min (one-sweep deferral `:184-193`; `mark_pulled` grow-only union
   `community_relay_prod.rs:316-341`). Coverage removal is a DETERMINISTIC
   function of (`pulled_by`, now) — every replica reaches the same verdict, so
   covered resurrections self-heal without tombstones (in-code doc `:153-159`).
   TTL expiry is per-replica and soft — exactly the class that needs local
   memory.
5. **Merge seam.** The fleet-sync engine invokes a closure
   `|local, remote| local.merge_from(remote)` (`lib.rs:6524-6526`) — a
   tombstone consult inside `RelayHoldDoc::merge_from` covers the anti-entropy
   path with zero engine changes.
6. **Sidecar precedent.** `relay_hold_first_observed.cbor`
   (`relay_hold_persist.rs:207-284`): leading schema-version byte, atomic
   write, trailing-bytes rejection, quarantine-recover (`ZEB-460` contract),
   written unconditionally in `RelayHoldPersist::persist` (`:300-311`),
   restored at boot before the engine starts (`lib.rs:6491-6510`).
7. **Deposit keys are fresh per send.** Key =
   `"{recipient_hex}:{content_id_hex}"` with `content_id = ContentId(sealed_blob)`
   and a fresh ephemeral key per seal (`community_relay_hold_crdt.rs:15-21`) —
   a legitimate re-send mints a NEW key. A tombstoned key can only reappear via
   fleet merge or a byte-identical replay.
8. **Serve path reads `entries` directly.** `held_for`
   (`community_relay_prod.rs:290-314`) filters `doc.entries` — keeping expired
   entries OUT of `entries` closes the re-serving window with no serve-path
   changes.
9. **DM-inbox twin.** `dm_inbox_crdt.rs:139,150` has the identical
   lazy-stamp + prune-to-live pattern → same resurrection re-arm for
   never-acked inbox entries. Out of scope here (different exposure: the
   butler is itself a recipient); filed as ZEB-925.

## 2. Design

### 2a. Tombstone store (`community_relay_hold_crdt.rs`)

New field on `RelayHoldDoc`, exactly mirroring the `first_observed_ms`
posture:

```rust
/// ZEB-924: LOCAL expiry memory — keys this replica TTL-expired, mapped to
/// the local wall-ms of expiry. Never serialized (canonical wire bytes
/// unchanged), excluded from PartialEq, restart-durable via a local sidecar.
#[serde(skip)]
expired_at_ms: BTreeMap<String, u64>,
```

Accessor `expired_at_ms()` + boot restore `restore_expired(map, now_ms)`
(mirrors `first_observed_ms()` / `restore_first_observed`).

### 2b. `gc()` — tombstone on TTL expiry, then bound the set

- Every key removed because `ttl_expired` is inserted into `expired_at_ms`
  with `now_ms`. Keys removed ONLY by coverage are NOT tombstoned — coverage
  convergence is already fleet-deterministic (premise 4) and tombstoning them
  would grow the set for zero benefit. A key that is BOTH covered-at-start and
  TTL-expired IS tombstoned (it met the TTL rule; harmless and simpler).
- Age-out: retain tombstones where
  `now_ms.saturating_sub(t) < RELAY_HOLD_TOMBSTONE_RETENTION_MS`.
- Cap: while `len > RELAY_HOLD_TOMBSTONE_CAP`, evict the OLDEST (smallest
  `expired_at_ms` value — the entry peers have most likely already expired
  themselves).

### 2c. `merge_from()` — suppress resurrection

In the `None =>` (new-key) arm: if the key is in `expired_at_ms`, skip the
insert entirely (no `changed`). Existing-entry merging (`pulled_by` union,
first-writer-wins metadata) is untouched — a live local entry is by invariant
never tombstoned (gc removes it from `entries` in the same call that
tombstones it).

Chosen over allow-reinsert-then-restamp (§5a) because suppression produces
zero churn and closes the serve window structurally: the entry never re-enters
`entries`, so `held_for` never sees it, no `changed` flush fires, and no
re-stamp can happen.

### 2d. Persistence (`relay_hold_persist.rs`, `lib.rs` boot)

- New sidecar `relay_hold_expired.cbor`
  (`RELAY_HOLD_EXPIRED_FILENAME`), schema byte
  `RELAY_HOLD_EXPIRED_SCHEMA_V1 = 1`, `save_expired` / `load_expired` /
  `load_expired_or_recover` — byte-for-byte the `first_observed` suite's
  contract (missing file ⇒ empty map; corruption ⇒ quarantine + empty;
  transient IO ⇒ propagate).
- `RelayHoldPersist` gains `expired_path`; `persist` writes the tombstones
  after the first-observed clock. Losing the file is safe-but-slower: the next
  resurrection re-arms one TTL window, then re-tombstones.
- Boot restore order (in the `lib.rs:6494` block):
  **doc → `restore_expired` → `restore_first_observed`**.
  `restore_expired` clamps future stamps to `now_ms` (backward-clock heal,
  mirrors ZEB-862 Q-1) and REMOVES from `entries` any key present in the
  tombstone map — expiry is monotone, so when a stale doc file (crash between
  the two atomic writes) resurrects an entry this replica already expired, the
  tombstone wins. Running before `restore_first_observed` lets its existing
  orphan-pruning (Q-2) drop the removed entries' stamps automatically.

### 2e. Sweep-task persistence detection (`lib.rs` GC task)

The stamp-only detection extends from "first-observed map grew" to "either
side-map changed length": tombstone ADDS always coincide with entry removal
(`changed = true` → full flush), so the length delta only needs to catch
age-out/eviction shrinkage. A balanced add+remove in one sweep is masked by
the delta but always accompanied by `changed = true` — documented at the site.

### 2f. Deposit path deliberately ungated — but acceptance clears the tombstone

`persist_hold` does NOT gate on tombstones. A fresh re-send mints a new key
(premise 7), so gating deposits would only affect byte-identical replays —
and would permanently black-hole a legitimate re-delivery attempt of the same
sealed bytes (e.g. sender retries a month-old undelivered message from its
outbox). An ungated byte-replay re-arms at most one TTL window on this
replica, bounded by the per-sender and global caps, and is then re-tombstoned.

**Amendment (PR #667 R2, Greptile):** acceptance must also CLEAR the key's
tombstone (`clear_tombstone`), on both the Inserted and Duplicate arms (the
latter defensively, healing pre-fix disk state). Without it, the persisted
pair (live entry + stale tombstone) lets `restore_expired` delete the
accepted — and already-acked — hold at the next boot. A rejected
(`CapExceeded`) deposit does NOT clear: a refusal must not weaken merge
suppression. This preserves the invariant `merge_from` relies on: a
tombstoned key is never live in `entries`.

### 2g. Constants (`community_relay.rs`)

| Constant | Value | Rationale |
|---|---|---|
| `RELAY_HOLD_TOMBSTONE_RETENTION_MS` | `2 * RELAY_HOLD_TTL_MS` (60 d) | A still-holding peer expires the entry within ITS OWN TTL of ITS first observation; 2× covers realistic cross-device observation skew. Past it, resurrection harm is bounded (§6). |
| `RELAY_HOLD_TOMBSTONE_CAP` | `4 * RELAY_HOLD_GLOBAL_CAP` (4096) | Live store caps at 1024; steady-state TTL expirations over one retention window fit comfortably; ~100 B/tombstone ⇒ ≤ ~400 KB worst case. |

Compile-time guard: `const _: () = assert!(RELAY_HOLD_TOMBSTONE_CAP >= RELAY_HOLD_GLOBAL_CAP);`

## 3. Behavior pins

1. A never-acked hold's lifetime on a given node is ≤ first-observation + TTL
   + one sweep interval, regardless of merge traffic (acceptance).
2. Suppressed resurrection is silent: `merge_from` returns `changed = false`
   for a tombstoned-only remote doc (no flush churn, no publish).
3. Coverage GC, one-sweep deferral, `held_for` recipient scoping, `pulled_by`
   union, first-writer-wins metadata: byte-for-byte unchanged.
4. Tombstones survive restart; a stale doc file cannot resurrect past them.
5. Wire bytes: `canonical_cbor_encode(doc)` unchanged for any doc (serde-skip).
6. Existing `gc_*` / `restore_*` / merge-convergence test families stay green.

## 4. Tests

- **T1** `gc_ttl_expiry_tombstones_the_key` — TTL removal records
  `expired_at_ms[key] = now`; coverage-only removal records nothing.
- **T2** `merge_suppresses_resurrection_of_tombstoned_key` — remote still-holding
  doc merges to `changed = false`, `entries` stays empty.
- **T3** `resurrection_lifetime_bound_across_merge_traffic` — interleaved
  merge/gc loop: after first expiry the key never re-enters `entries`
  (acceptance pin).
- **T4** `coverage_removal_is_not_tombstoned_and_still_converges` — covered
  entry removed, tombstone map empty, resurrected covered entry deterministically
  re-removed next sweep (existing semantics pinned).
- **T5** `tombstone_ages_out_after_retention_and_reopens` — past
  `RETENTION`, tombstone dropped; a subsequent merge re-inserts and gets a
  fresh TTL (documented bounded harm, pinned).
- **T6** `tombstone_cap_evicts_oldest_first`.
- **T7** `restore_expired_removes_tombstoned_entries_and_clamps_future_stamps` —
  stale-doc resurrection healed at boot; future stamp rebased to `now`.
- **T8** (persist) `expired_round_trips_missing_is_empty_trailing_rejected_corruption_quarantined` —
  mirrors the first-observed sidecar suite.
- **T9** (persist) `persist_writes_expired_sidecar`.

## 5. Declined alternatives

- **(a) Allow re-insert, restore the old stamp.** Every anti-entropy round
  from a still-holding peer re-inserts → `Changed` → durable flush + republish
  (removals never propagate through a grow-only union), forever; each round
  opens a ≤10-min `held_for` re-serving window. Suppression costs one map
  lookup and has neither problem.
- **(b) Wire-level tombstones.** Canonical-bytes + merge-semantics change,
  fleet-wide coordination, exactly what the ticket excludes; per-replica TTL
  is already soft (premise 4), so per-replica memory is the matching shape.
- **(c) Demand-aware early collapse.** Declined in the ticket (2026-08-12
  scope review): punishes exactly the offline recipients store-and-forward
  serves.
- **(d) Gating the deposit path.** Black-holes legitimate re-delivery of the
  same sealed bytes (§2f).
- **(e) RAM-only tombstones.** A restart forgets them; the next peer merge
  re-arms a full TTL per restart — on frequently-restarted nodes that
  reproduces the defect. The sidecar precedent makes durability cheap.

## 6. Accepted residuals

- **Post-retention resurrection.** After 2×TTL a tombstone ages out; a peer
  that STILL holds the entry (pathological: >2× observation skew) re-arms one
  more TTL window, then re-tombstones. Bounded and monotone-decaying.
- **Pre-ZEB-924 fleet members** keep resurrecting until they update and
  expire their own copies (fleet updates in lockstep; self-heals).
- **Length-delta masking** in the sweep task (§2e): only reachable together
  with `changed = true`, which flushes everything anyway.
