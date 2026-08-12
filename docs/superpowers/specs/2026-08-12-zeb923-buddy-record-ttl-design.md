# ZEB-923: TTL for buddy-pact authoritative records (pledge_lists / backup_sets)

Second leg of ZEB-913 (Freenet review R5). Parent premise for this leg ("buddy pins persist
without an in-use check") was **refuted** by the 2026-08-12 audit — the planner already runs a
level-triggered full reconciliation every 30 s. The real gap is one level up: the
reconciliation's authoritative inputs never expire. This spec adds lease discipline to those
inputs. Verified against `main` at `a7462147` (2026-08-12).

## 1. Verified premises (receipts)

1. **Storage model.** `StorageRecordStore` (`storage_records.rs:199-212`) keys all record
   families by owner address (lowercase hex). `pledge_lists` and `backup_sets` are persisted
   to `storage_records.json`; `hosting_reports` are RAM-only by design (`:12-15`, `:662`).
2. **No age-based removal for pledge/backup.** `sweep_hosting` (`:722-726`) prunes only
   `hosting_reports` (`HOSTING_REPORT_STALE_MS = 900_000`, drop at exactly the bound, keyed on
   the local `received_at_ms`). The only other removal paths are `purge_revoked` (`:736-753`,
   reachable only for owners with a v2 signer pin), cap-pressure eviction (`:897-943`,
   newest-`seq`-first, freeze-when-full at `MAX_TRACKED_OWNERS = 1024`), and load-time
   validation drops. A permanently-dark buddy's records live forever.
3. **The lease stamp already half-exists.** All three families stamp `received_at_ms = now_ms`
   at every winning ingest (`:514`, `:587`, `:656`; pinned by
   `ingest_stamps_local_received_clock_not_peer_updated_at` `:2083`). On pledge/backup it is
   currently write-only ("no trust meaning", `:97-100`, `:112-115`) and is **not persisted** —
   disk rows carry only `{owner, entries, updated_at}` (`:139-151`) and reload hardcodes
   `received_at_ms: 0` (`:268`, `:290`). `StorageSignerPin.pinned_at_ms` shows the round-trip
   precedent (`:161`, `:314-329`).
4. **`updated_at` is not a safe TTL basis.** It is peer-controlled UNIX **seconds** minted by
   `next_storage_updated_at` (`lib.rs:20047-20057`, forced strictly increasing against a
   persisted per-family floor), with **no skew or future-stamp validation** at ingest. A peer
   can stamp arbitrarily far-future values. The trustworthy clock is the local
   `received_at_ms`, exactly as `sweep_hosting` already assumes.
5. **LWW renewal trap.** `lww_insert` (`:813-832`) ignores equal `updated_at`
   (`IgnoredOlder`) and only re-stamps `received_at_ms`/`seq` on a strictly-newer record. A
   byte-identical republish would NOT renew a lease. Renewal must arrive with a freshly minted
   `updated_at` (and fresh signature) to take the `UpdatedNewer` path.
6. **No periodic republish exists (the ticket's spec-time question — resolved).**
   Pledge lists publish at exactly 3 sites: boot republish inside `start_node_inner`
   (`lib.rs:15018`), `set_buddy_pledge_impl` (`:20708`), `remove_storage_buddy_impl`
   (`:20747`). Backup sets at exactly 2: boot (`:15019`) and `set_backup_flag_impl`
   (`:20914`). The wire is a bare fire-and-forget Zenoh `session.put`
   (`event_loop.rs:4790-4796`) — no retention, no queryable, and the subscriber
   (`event_loop.rs:3600-3615`) is non-querying, so a late joiner receives nothing until the
   publisher next publishes. Hosting reports are the only family with a periodic publisher:
   `spawn_hosting_report_publisher` (`lib.rs:20444`, spawned at `:15030`), 30 s poll,
   republish on change or every `HOSTING_REFRESH_INTERVAL_MS = 300_000`, via the
   component-taking builder `build_signed_hosting_report_with`.
7. **Decay already flows to release with zero planner changes.** The buddy tick
   (`event_loop.rs:6624`, `BUDDY_SYNC_INTERVAL_MS = 30_000`) runs sweep/purge (step 2,
   `:6669-6682`) immediately before `buddy_pin_planner::plan` (step 3, `:6683-6705`) in the
   same tick. `plan` is a pure function reading only `owners_pledging_to` and `backup_set`
   (`buddy_pin_planner.rs:90-98`, `:118`, `:173-176`): a decayed pledge_list drops the buddy
   from `active` → `release_buddies` (release everything); a decayed backup_set alone empties
   `desired` via `unwrap_or_default()` → per-CID `release`. Both converge to released pins.
8. **Cap interaction is favorable.** Freeze-when-full means a genuinely new owner self-evicts
   at cap (`IgnoredAtCap`, `:888-896`) until a slot frees. TTL expiry of dark owners frees
   slots — the sweep *unfreezes* the working set rather than fighting eviction (ticket scope
   item 4 satisfied structurally).
9. **Cadence idioms in-tree.** Staleness = 3× refresh (`storage_records.rs:58-61`,
   `observed_holders.rs:18-21`); republish at ½ the freshness window
   (`butler_deposit.rs:102-106`). Hosting-report freshness is NOT a general liveness oracle —
   a buddy hosting nothing publishes no reports at all (`lib.rs:20506-20509`), which rules out
   "key pledge TTL off hosting freshness".

## 2. Design

Freenet shape: cheap periodic renewal by the authority, collapse by default on non-renewal.
Three parts.

### 2a. Publisher: periodic re-mint republish (the renewal signal)

Extend the existing hosting-report publisher task into a unified storage-record refresher
(rename `spawn_hosting_report_publisher` → `spawn_storage_record_publisher`; same spawn site
`lib.rs:15030`, same 30 s poll loop, same `publish_tx.is_closed()` shutdown condition):

- Hosting logic byte-for-byte unchanged (30 s check; publish on change or 300 s refresh).
- New: when `STORAGE_RECORD_REFRESH_INTERVAL_MS` has elapsed since the last pledge/backup
  refresh, rebuild, re-sign, and republish **both** the pledge list and the backup set, then
  reset the timer. Republishing unconditionally mirrors boot-publish semantics (no new
  emptiness conditionals); rebuilding from source (settings / content index) each time also
  heals any drift.
- Requires component-taking builder variants `build_signed_pledge_list_with(...)` /
  `build_signed_backup_set_with(...)` mirroring the existing
  `build_signed_hosting_report_with` (the task holds component clones, not `&NodeState`).
  Each call mints a fresh `updated_at` via the shared per-family `AtomicU64` clock and
  persists the floor before publishing, exactly as the guard-holding builders do today
  (`lib.rs:20160-20180`). The mint must use the *same shared atomic* as the user-action path
  so concurrent mints stay strictly increasing.
- Renewal therefore rides the existing `UpdatedNewer` ingest path at every receiver: fresh
  stamp + fresh signature → LWW replace → `received_at_ms` re-stamped. **Zero receiver-side
  ingest changes.** Renewal is genuine liveness (freshly signed each period), not a replayable
  blob.
- Bonus: late-joining subscribers now converge within one refresh interval instead of waiting
  for the publisher's next boot or mutation.

### 2b. Receiver: TTL sweep + stamp persistence + boot grace

1. **Sweep.** New `StorageRecordStore::sweep_stale_pledges_and_backups(&mut self, now_ms) ->
   bool`: retain entries in both families where
   `now_ms.saturating_sub(received_at_ms) < STORAGE_RECORD_TTL_MS` (same strict boundary as
   `sweep_hosting`: exactly-at-bound is dropped; `saturating_sub` makes clock rollback safe —
   nothing decays). Because these families are persisted, the sweep must `save()` when it
   removed anything (else expired records resurrect at reload) — the save-on-change + `bool`
   return shape mirrors `purge_revoked`. Called from buddy-tick step 2 beside `sweep_hosting`
   (`event_loop.rs:6678`); on `true`, the tick emits `storage-buddies-updated` (same event the
   ingest path emits) so the UI reflects the decayed pact.
2. **Persist the stamp.** Add `receivedAtMs` to `PledgeListOnDisk`/`BackupSetOnDisk` with
   `#[serde(default)]` — additive, tolerant read of legacy files (missing → 0), file version
   stays 1. Writer always includes it. (`SignerPinOnDisk.pinnedAtMs` precedent.)
3. **Boot grace.** New `StorageRecordStore::apply_boot_grace(&mut self, now_ms)`: floor both
   families' stamps to `now_ms.saturating_sub(STORAGE_RECORD_TTL_MS -
   STORAGE_RECORD_BOOT_GRACE_MS)` (raise-only). Called once at the production construction
   site (`lib.rs:4094-4095`) right after `StorageRecordStore::new(path)`; test stores
   (`new(None)`) are untouched. Rationale: with persisted stamps, a user offline longer than
   the TTL would otherwise mass-decay *alive* buddies' records on the first tick after boot —
   released pins and refetch churn of real content bytes — before those buddies' next
   republish can arrive. The floor guarantees every surviving record at least
   `BOOT_GRACE` of post-boot runway, during which any alive buddy republishes (their refresh
   interval is far shorter than the grace). Legacy-file stamps of 0 get the same floor. The
   formula saturates to 0 for small test clocks, leaving fixture stamps untouched. RAM-only
   raise; not saved (reload re-floors, which is consistent).

### 2c. Planner and pins: no changes

Decay flows through the existing 30 s reconciliation (premise 7). No new pin machinery, no
planner signature change, no clock added to `plan`.

### Constants (in `storage_records.rs`, beside the hosting pair)

| Constant | Value | Rationale |
|---|---|---|
| `STORAGE_RECORD_REFRESH_INTERVAL_MS` | `3_600_000` (1 h) | Renewal cadence. Cheap (two small signed puts/hour); also the late-joiner convergence bound. Checked by the existing 30 s poll. |
| `STORAGE_RECORD_TTL_MS` | `259_200_000` (3 days) | "Generous — days, not minutes" per ticket. 72 missed renewals before decay; a buddy online briefly once a day renews via boot publish alone. Deliberately not the in-file 3× idiom (that would be 3 h — an availability-blip decay hazard); the ratio buys robustness, matching the ZEB-922 growth-bounding posture. |
| `STORAGE_RECORD_BOOT_GRACE_MS` | `43_200_000` (12 h) | ≥ 12× the refresh interval — ample post-boot window for alive buddies to renew even across transient partitions. |

**Known residual (accepted):** a node that is never up for `BOOT_GRACE` continuously
re-floors dark records each boot and never decays them. Cost is bounded: the pin ledger
empties every restart anyway (boot honesty sweep), and fetches from a dark buddy fail into
backoff. Not worth extra machinery.

**Version-skew caveat (accepted):** a pre-ZEB-923 publisher never renews, so an updated
receiver decays an old-but-alive buddy after the TTL. Self-heals on the old node's next
boot/mutation publish; the fleet updates in lockstep.

## 3. What must stay green (behavior pins)

- `local_clock_rollback_keeps_established` (`storage_records.rs:1918`) — eviction stays
  `seq`-keyed; the sweep's `saturating_sub` never decays under rollback.
- `pledges_and_backup_sets_survive_disk_reload_hosting_does_not` (`:1299`) — reload semantics
  preserved; extended (not weakened) to pin `receivedAtMs` round-trip.
- `hosting_sweep_drops_stale_reports` (`:1550`), full LWW/ingest-validation/cap/eviction suite
  (`:945` onward), planner suite (`buddy_pin_planner.rs:187-540`, fixtures ingest at
  `now_ms = 9_000` — untouched by the grace floor's saturation), event-loop router tests
  (`event_loop.rs:13516+`), and the IPC classification tests (`lib.rs:20977`).

## 4. Test plan (new)

Receiver, `storage_records.rs`:
- **T1** boundary: pledge + backup at exactly `TTL` dropped, `TTL − 1` kept; hosting map
  untouched by the new sweep.
- **T2** resurrection guard: sweep on a path-backed store, reload → swept records stay gone
  (pins the internal `save()`).
- **T3** renewal: `UpdatedNewer` re-ingest re-stamps `received_at_ms`; the renewed record
  survives the sweep that kills its non-renewed cohort.
- **T4** boot grace: ancient/zero stamps floored to `now − TTL + GRACE`; fresh stamps
  untouched; small `now_ms` saturates to no-op.
- **T5** persistence: `receivedAtMs` round-trips through save/reload; hand-written legacy file
  without the field loads with 0.
- **T6** cap unfreeze: at-cap store, sweep expires an owner, the previously self-evicting
  newcomer is now admitted (`Inserted`).

Decay-to-release (planner-level): **T7** — seeded records aged past TTL, sweep, then `plan`:
expired pledge ⇒ buddy in `release_buddies`; expired backup_set alone ⇒ all ledger CIDs in
`release`.

Publisher, `lib.rs`:
- **T8** `build_signed_pledge_list_with` / `build_signed_backup_set_with`: strictly-increasing
  `updated_at` across consecutive calls, signatures verify via the existing
  `storage_signing` verifiers, floors persisted.
- **T9** refresher gating: pledge/backup republish fires only when the refresh interval has
  elapsed (loop-extracted decision helper, no wall-clock sleeps).

## 5. Declined alternatives

1. **Receiver-side equal-stamp "touch" renewal** — would renew on replayed blobs (no fresh
   signature ⇒ no liveness evidence) and needs new ingest semantics. The re-mint republish
   gives renewal with zero ingest changes.
2. **Keying the TTL on `updated_at`** — peer-controlled, no skew validation (premise 4); a
   malicious or skewed peer could pre-date or immortalize records.
3. **Hosting-report freshness as the liveness oracle** — silent when the buddy hosts nothing
   (premise 9); would decay pacts of honest-but-idle buddies.
4. **Profile-card-style queryable + boot-burst for storage records** — the right shape for
   fast convergence, but a separate feature; the TTL only needs renewal-within-TTL, which the
   hourly republish provides. Noted as future work.
5. **Stamp-sanity/skew gate on `updated_at` at ingest** — real hardening, but orthogonal to
   this ticket and interacts with the LWW floor design; would need its own premise audit.
6. **Not persisting `received_at_ms` (re-stamp all records at load)** — every restart would
   reset all leases; a daily-restart user never decays anything, defeating the ticket for the
   most common desktop usage pattern.
