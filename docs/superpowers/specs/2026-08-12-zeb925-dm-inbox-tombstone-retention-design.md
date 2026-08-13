# ZEB-925: DM-inbox bounded retention — local expiry tombstones stop resurrection-by-merge (design)

Port of the ZEB-924 relay-hold mechanism (see
`docs/superpowers/specs/2026-08-12-zeb924-relay-hold-tombstone-retention-design.md`)
to the butler dm-inbox CRDT. This document records the verified premises, the
DM-specific deltas from the relay design, and the declined alternatives. Both
ZEB-924 review-round findings (R1 sidecar-write ordering + retry latch; R2
acceptance-clears-tombstone) are designed in from the start.

## 1. Verified premises (receipts against main `15b633a4`)

1. **The defect is present and identical in shape.**
   `dm_inbox_crdt.rs:139` lazily stamps `first_observed_ms.entry(key).or_insert(now_ms)`
   on the first sweep that sees each entry; `:149-150` prunes the side-map to
   live keys after the retain. `merge_from`'s `None` arm (`:252-254`) is
   insert-once + `changed = true`. An expired-and-removed entry resurrected by
   a still-holding sibling's merge is re-inserted, re-flagged, and re-stamped
   fresh on the next sweep — a fresh 30-day window per resurrection,
   documented in-code as bounded only by caps (`dm_inbox_ingest.rs:408-416`).
2. **TTL and caps.** `butler_deposit::INBOX_TTL_MS = 30 days`
   (`butler_deposit.rs:165`); `INBOX_GLOBAL_CAP = 1024` (`:125`),
   `INBOX_PER_SENDER_CAP` alongside. GC runs inside the ingest sweep
   (`dm_inbox_ingest.rs:428`), which is **nudge-driven** (startup sweep + one
   debounced sweep per `on_applied` burst, `:474-495`), not interval-driven
   like the relay's 10-minute GC task.
3. **Coverage GC is fleet-deterministic and needs no tombstones.** The
   coverage criterion (`ingested_by` ⊇ enrolled set at sweep start) is a
   deterministic function of grow-only union state — a resurrected covered
   entry arrives already covered and is removed again next sweep; the fleet
   converges without tombstones (`dm_inbox_ingest.rs:397-406`). Only
   **never-covered** entries are exposed: an enrolled-but-dead sibling device
   that never acks keeps `ingested_by` short of coverage forever, so TTL is
   the only remover — and TTL is exactly what resurrection re-arms.
4. **Exposure is narrower than the relay's but the same shape.** The butler is
   itself a recipient: entries for live devices are drained + acked locally
   within minutes. The exposed population is entries pending on a permanently
   silent enrolled device — narrower than the relay's offline-recipient
   population, but retention is equally unbounded under continuous merge.
5. **Sidecar precedent exists in this module.** `dm_inbox_persist.rs` already
   carries the ZEB-862 `first_observed` local sidecar trio
   (`load_first_observed` / `load_first_observed_or_recover` /
   `save_first_observed`, filename `dm_inbox_first_observed.cbor`) with
   schema-byte + trailing-bytes-reject + ZEB-460 quarantine-recover semantics.
6. **Boot restore site and ordering dependency.** `lib.rs:6195-6211` loads the
   doc then calls `restore_first_observed`, whose orphan-prune (Q-2,
   `dm_inbox_crdt.rs:172-178`) drops stamps for keys not in `entries` — so a
   `restore_expired` that removes tombstoned entries MUST run before it, and
   the removed entries' stamps then self-clean. Same ordering as the relay
   boot block (`lib.rs` relay section).
7. **The stamp-only persist path has the R1 no-retry gap.** `sweep_once`
   (`dm_inbox_ingest.rs:1520-1548`) computes `(changed, fo_grew)`; the
   `fo_grew`-only arm calls `persist_now()` and on failure **warns and drops**
   (`:1544-1546`). The stamps are already in memory, so `fo_grew` never
   re-fires for them — a failed sidecar write is never retried. (The `changed`
   arm is fire-and-forget `notify_dirty()`; retry there is the engine's own
   dirty-latch semantics, out of scope.)
8. **The acceptance path has the three-verdict shape the R2 fix needs.**
   `iroh_butler_acceptor.rs::persist_entry` (`:371-444`): occupied key →
   `Duplicate` (with ZEB-483 invite-heal); caps checked inside the doc lock →
   `CapExceeded` early-return (`:419`, nothing inserted); else insert →
   `Inserted`. An ungated insert of a tombstoned key would persist a
   (live entry + stale tombstone) pair that `restore_expired` deletes at next
   boot — the exact ZEB-924 R2 bug, prevented here from the start.
9. **Redelivery keys are stable.** A DM deposit key is
   `{space_id_hex}:{message_cid_hex}` (invite/revoke/grant variants likewise
   deterministic), stable across sender retries — so the `Duplicate` arm is
   the normal lost-ack path, and a post-expiry redeposit of the same message
   reuses the tombstoned key (motivating §2f).

## 2. Design

### 2a. Constants (`butler_deposit.rs`, after `INBOX_TTL_MS`)

```rust
pub const INBOX_TOMBSTONE_RETENTION_MS: u64 = 2 * INBOX_TTL_MS;   // 60 d
pub const INBOX_TOMBSTONE_CAP: usize = 4 * INBOX_GLOBAL_CAP;      // 4096
const _: () = assert!(INBOX_TOMBSTONE_CAP >= INBOX_GLOBAL_CAP);
```

Same ratios as the relay (`RELAY_HOLD_TOMBSTONE_*`): retention 2×TTL bounds
tombstone age; cap 4× the live-entry cap bounds tombstone count with headroom
for churn; the const assert pins cap ≥ live cap so eviction can never make the
tombstone set smaller than what a full inbox can expire.

### 2b. Tombstone map (`dm_inbox_crdt.rs`)

`DmInboxDoc` gains `#[serde(skip)] expired_at_ms: BTreeMap<String, u64>` —
local-only, never on the wire, excluded from the existing entries-only manual
`PartialEq` (canonical bytes unchanged). Accessor `expired_at_ms()`, plus
`clear_tombstone(&mut self, key: &str)` and the private
`prune_tombstones(now_ms)` (retention age-out, then oldest-first eviction down
to `INBOX_TOMBSTONE_CAP`).

### 2c. GC tombstones TTL removals ONLY (split the retain predicate)

The **delta from the relay design**: `gc_expired` decides TTL and coverage in
one `retain` closure. The port splits the removal reason:

- `covered` (regardless of TTL state) → remove, **no tombstone**. Coverage is
  fleet-deterministic (premise 3); suppression is unnecessary and tombstoning
  covered keys would be pure dead state.
- `ttl_expired && !covered` → remove **and tombstone** at `now_ms`.

After the retain: `prune_tombstones(now_ms)`, then the existing live-key prune
of `first_observed_ms`. Return value (entries-changed bool) is unchanged.

### 2d. Merge suppression (`merge_from` `None` arm)

A remote key present in `expired_at_ms` is skipped: no insert, no
`changed` flag — no ingest wakeup, no flush churn, and `ingest_pending` never
sees it. Tombstoned keys are thereby invisible to resurrection until the
tombstone ages out (§2b retention), giving a hard per-replica lifetime bound:
first-observation + TTL + one sweep, plus at most one reopened window per
retention period (§6).

**R1 amendment (PR #668, CodeRabbit).** Because a suppressed re-merge flags no
change, it schedules no sweep — and unlike the relay twin (whose GC task runs
on an unconditional 10-minute timer) the DM-inbox sweeper is purely
event-driven, so on a quiet inbox nothing would ever prune an aged-out
tombstone and suppression could outlive retention until the next unrelated
event or boot. The production merger closure (lib.rs) therefore calls
`prune_tombstones(now)` by wall clock BEFORE every inbound merge: the merge
that would be wrongly suppressed is itself the pruner, so suppression never
outlives retention by more than the gap to the next inbound merge — and that
merge re-admits. The CRDT stays time-free (the clock lives at the adapter
boundary); a real re-send was never affected either way, since the deposit
path clears the tombstone on acceptance (§2f). A dedicated timer task was
declined as strictly heavier for a weaker bound.

### 2e. Persistence (`dm_inbox_persist.rs` + boot + sweep)

- New sidecar trio mirroring `first_observed`: `DM_INBOX_EXPIRED_FILENAME =
  "dm_inbox_expired.cbor"`, schema byte V1, atomic write, trailing-bytes
  reject, ZEB-460 quarantine-recover. Missing ⇒ empty ⇒ safe-but-slower
  (worst case: one extra TTL window, exactly today's behavior).
- `DmInboxPersist` gains `expired_path`; `persist` writes `save_expired`
  **first** (ZEB-924 R1): a crash between writes then leaves
  tombstone-present + stale-doc — healed at restore — instead of
  fresh-doc + missing-tombstone, which resurrects.
- **Boot** (`lib.rs`): `load_doc_or_recover` → `restore_expired(map, now_ms)`
  → `restore_first_observed(...)`. `restore_expired`: clamp future stamps to
  `now_ms`, install, `prune_tombstones(now_ms)` (an aged-out tombstone must
  neither suppress nor delete), then `entries.retain(!tombstoned)` — the
  tombstone wins over a stale doc file. The subsequent
  `restore_first_observed` orphan-prune drops the removed entries' stamps
  (premise 6 ordering).
- **Sweep** (`dm_inbox_ingest.rs::sweep_once`): the sidecar-delta check
  extends from `fo_grew` to `sidecars_changed` = (fo len delta) OR (expired
  len delta) — tombstones both grow (new expiries ride the `changed` arm, but
  age-out can shrink them on an otherwise no-op sweep). R1 retry latch: a
  `sidecar_persist_pending: &mut bool` owned by `run_dm_inbox_ingest_sweeper`
  and threaded into `sweep_once`; set on `persist_now` failure, cleared on
  success, and OR-ed into the persist condition so a failed sidecar write
  retries on the next sweep. The `changed` arm's `notify_dirty` retry
  semantics (engine dirty latch) are untouched.

### 2f. Acceptance clears the tombstone (`iroh_butler_acceptor.rs::persist_entry`)

Accepting a deposit is a fresh local decision to hold the entry, so it must
clear the key's expiry memory — otherwise the persisted (live entry + stale
tombstone) pair is deleted by `restore_expired` at the next boot *after the
butler acked it to the sender* (the ZEB-924 R2 bug class):

- `Inserted` arm: `doc.clear_tombstone(&key)` alongside the insert.
- `Duplicate` arm: defensive `clear_tombstone` (heals any pre-existing
  inconsistent disk state; restores the invariant `merge_from` relies on —
  a tombstoned key is never live in `entries`).
- `CapExceeded` arm: returns without clearing — a rejected deposit must not
  weaken suppression.

The deposit path stays **ungated**: rejecting deposits for tombstoned keys
would black-hole a legitimate sender retry/resend of an undelivered DM. The
residual (a byte-identical redeposit of an expired key re-opens ≤ 1 TTL
window) is accepted, as in the relay design.

## 3. Pins

- Canonical wire bytes unchanged (`#[serde(skip)]`, entries-only `PartialEq`);
  `cbor_round_trips_canonically` and all existing merge/`gc_*`/`restore_*`
  families stay green.
- Coverage GC semantics byte-for-byte unchanged (§2c covered arm).
- `DepositPersistVerdict` variants and ack semantics (D7) unchanged.
- `INBOX_GLOBAL_CAP` / `INBOX_PER_SENDER_CAP` enforcement unchanged
  (tombstones are not entries; they never count against inbox occupancy).

## 4. Tests

CRDT (`dm_inbox_crdt.rs`): ttl-expiry-tombstones vs coverage-removal-does-not
(split predicate, incl. the covered-AND-expired case);
merge-suppresses-resurrection (no insert, no `changed`);
resurrection-lifetime-bound across merge traffic;
covered-resurrection-converges-without-tombstone;
tombstone-ages-out-after-retention-and-reopens; cap-evicts-oldest-first;
restore-prunes-aged-out-tombstones-and-lets-their-entries-live;
restore-removes-tombstoned-entries-and-clamps-future-stamps.

Persist (`dm_inbox_persist.rs`): expired round-trip + missing-is-empty;
trailing-bytes reject + quarantine-recover; `persist` writes the expired
sidecar (seeded via `restore_expired` with boot time near the stamp).

Acceptor (`iroh_butler_acceptor.rs`): redeposit-of-expired-key clears the
tombstone on `Inserted` and `Duplicate`; `CapExceeded` leaves it.

Sweeper (`dm_inbox_ingest.rs`): tombstone-delta-only sweep persists via
`persist_now`; failed sidecar persist sets the latch and retries next sweep.

## 5. Declined alternatives

Identical dispositions to ZEB-924 §5, which apply unchanged: allow-reinsert +
restamp (perpetual churn; removals never propagate through the grow-only
union); wire/replicated tombstones (canonical-bytes change, cross-fleet
semantics for a local concern); deposit-path gating (black-holes legitimate
resends); RAM-only tombstones (restart forgets exactly when the doc file
remembers). One DM-specific decline: **tombstoning covered removals** — see
§2c; coverage converges by determinism and needs no suppression.

## 6. Residuals

- A byte-identical redeposit of an expired key re-opens ≤ 1 TTL window
  (accepted, §2f).
- After `INBOX_TOMBSTONE_RETENTION_MS` a still-merging sibling can reopen one
  window per retention period — the lifetime bound is per-replica
  first-observation + TTL + one sweep, with at most one reopened window per
  60 d, not a fleet-global guarantee (same residual as the relay). Reopen
  latency is bounded by the next inbound merge after retention (the §2d R1
  merge-path prune), not by sweep availability.
- Cap eviction (oldest-first beyond 4096) can early-expire suppression for
  the evicted keys under pathological churn; the const assert keeps the cap
  ≥ a full inbox's worth of expiries.
