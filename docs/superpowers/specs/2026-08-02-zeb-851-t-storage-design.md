# ZEB-851 T-STORAGE — bounded-time hardening of storage/relay/DM-inbox eviction & TTL

**Ticket:** [ZEB-851](https://linear.app/zeblith/issue/ZEB-851) — the last remaining
High in the [ZEB-831](https://linear.app/zeblith/issue/ZEB-831) wall-clock threat
model (§4 HIGH). Three related GRIEF-LOCKOUT vectors where an eviction or TTL
decision is driven by a **peer-supplied stamp**.

**Goal:** Remove every peer-controlled stamp from an eviction/TTL *decision* so a
single faulty/malicious wall clock can neither flood honest rows out of a bounded
store nor backdate a live blob/DM into instant garbage collection.

**Non-goal:** Changing any wire format, CRDT convergence rule, or the metadata
stamps themselves (`updated_at` / `held_at` / `deposited_at` remain exactly as
they are on the wire and as CRDT-ordering keys). This is purely a fix to which
*clock* each eviction/TTL decision reads.

---

## Global Constraints

- **Workspace / commands:** all cargo commands run from `src-tauri/`.
  - Lint: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - Test: `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast`
  - Format: `cargo fmt --all -- --check`
- **No wire-format change.** `RelayHoldEntry`, `DmInboxEntry`, `RelayHoldDoc`,
  `DmInboxDoc` are `CanonicalPayload` types; their serialized bytes must stay
  byte-identical (existing `wire_format_*` pinning fixtures must not move). Any
  new local field is `#[serde(skip)]` (never serialized) so canonical bytes are
  unchanged.
- **No CRDT-convergence change.** `held_at` / `deposited_at` stay as the
  first-writer-wins metadata ordering key. Do **not** clamp or reject them inside
  `merge_from` — that is receiver-dependent and would diverge the *replicated*
  metadata across replicas.
- **Divergence-safe by construction.** Both relay-hold and DM-inbox GC are
  already documented churn-tolerant / resurrection-by-merge (each replica GCs
  independently). Every new clock is a *local, per-replica* clock, consistent
  with that existing model.
- **MSRV** unchanged; no new dependencies.

---

## Threat model recap

A participant's wall clock is untrusted (ZEB-831). A stamp copied from the wire is
attacker-controlled. Two distinct abuse shapes appear here:

1. **Flood-evict (ES).** A *local bounded store* evicts the row with the
   `min(peer stamp)`. An attacker minting throwaway addresses publishes
   far-future stamps; those rows are never the minimum, so **honest** rows are
   evicted from the cap instead. `owners_pledging_to` empties → storage-pact
   derivation loses real buddies.

2. **Backdate-expire (RH, C11).** A *replicated CRDT* sets each entry's GC
   deadline to `peer_stamp.wall_ms + TTL`. A backdated stamp (a skewed sibling
   relay, or a malicious/skewed butler) makes the deadline already in the past →
   the live held blob / undelivered DM is GC'd immediately, before the recipient
   ever pulls it. The sender may already hold a valid deposit ack → silent loss.

---

## Fix A — ES: flood-proof store eviction (`src-tauri/src/storage_records.rs`)

### Current (vulnerable)
`evict_stalest` (fn at line 772) selects the victim as `min(updated_at)`, where
`updated_at` is the **peer** stamp copied verbatim from the wire payload into the
record (lines 416 / 487 / 553). Called on every over-cap insert (lines 421 / 492 /
559) and on load (lines 247 / 248). `PledgeListRecord` and `BackupSetRecord` carry
**only** the peer `updated_at`; `HostingReportRecord` already additionally carries
a local `received_at_ms: now_ms` (line 554) — the asymmetry this fix removes.

### Fix
Mirror the sibling `evict_pins` (fn at line 744), already documented flood-proof
(local `pinned_at_ms` clock + newest-first, so a mint flood evicts its own
just-minted pins and never an established one):

1. Add `received_at_ms: u64` to `PledgeListRecord` and `BackupSetRecord`
   (`HostingReportRecord` already has it). It is a **local receipt clock** with no
   trust meaning — eviction ordering only (same doc-comment posture as
   `StorageSignerPin::pinned_at_ms`).
2. Stamp `received_at_ms = now_ms` when the record is inserted at ingest
   (`on_pledge_list_sample` line ~416, `on_backup_set_sample` line ~487).
3. Rewrite the overflow evictor to select the victim by **newest
   `received_at_ms` first** (ties broken by owner), i.e. `max(received_at_ms)`.
   Under a flood the attacker's freshly-received rows are the newest and evict
   *each other*; established honest rows (older receipt) survive. Rename the fn to
   reflect the new semantics (it no longer evicts the "stalest") — e.g.
   `evict_overflow` — with a doc comment mirroring `evict_pins`' flood-resistance
   rationale. The three call sites and the two load-path sites update to the new
   name/accessor.

### Decision — reloaded records default `received_at_ms = 0`
`new()` (the load path) has no `now_ms` and the on-disk format
(`PledgeListOnDisk` / `BackupSetOnDisk`, lines 100-114) carries only `updated_at`.
Reloaded records therefore default `received_at_ms = 0`. This is correct for
flood-resistance: `0` is *older* than any live `now_ms`, so records that survived
to disk are treated as "most-established = protected", and a post-restart flood
(fresh `now_ms`) is still the newest and evicts itself. The load-path eviction is
only tamper-defense (an over-cap disk file), so a `0`-tie broken by owner is
sufficient there. **Disk format is unchanged** (no new persisted field, version
stays `1`).

*Rejected alternative:* persist `received_at_ms` additively (`#[serde(default)]`).
More faithful ordering across restart, but the ordering only matters under an
over-cap condition that ingest already prevents before persisting — not worth the
format churn (YAGNI).

### Test
`evict_overflow` flood test: fill the store to `MAX_TRACKED_OWNERS` with honest
rows (older receipt), then ingest a flood of throwaway-owner rows carrying
far-future `updated_at` and fresh receipt; assert every honest row survives and
the flood rows are the ones evicted. (The pre-fix code fails this — honest rows
are evicted because their `updated_at` is the minimum.)

---

## Fix B — RH: local-receipt TTL (`src-tauri/src/community_relay_hold_crdt.rs`)

### Current (vulnerable)
`gc(&mut self, now_ms)` (line 155) expires an entry on
`held_at.wall_ms + RELAY_HOLD_TTL_MS < now_ms` (line 168). `held_at` is the
peer/relay stamp and is adopted **wholesale first-writer-wins** on merge — a
sibling sending the same key with an earlier `held_at` wins (line 104). A
backdated `held_at` → deadline already past → the live sealed blob is GC'd
immediately, recipient never pulls it.

### Fix
Key GC off a **local first-observation clock**, leaving `held_at` as pure
FWW metadata:

1. Add `#[serde(skip)] first_observed_ms: BTreeMap<String, u64>` to `RelayHoldDoc`
   (skip-serialized → canonical wire bytes unchanged; `CanonicalPayload` unaffected).
2. In `gc(now_ms)`, before the retain: for every entry key absent from
   `first_observed_ms`, insert `= now_ms` (lazy stamp — the first sweep that
   observes the entry starts its TTL). Change the expiry test to
   `first_observed_ms[key] + RELAY_HOLD_TTL_MS < now_ms`. After the retain, prune
   `first_observed_ms` for keys no longer in `entries` (bounded with the doc).
3. `held_at` / `held_by` / merge logic untouched.

### Divergence & restart
Per-replica local clocks are already the GC model here (churn-tolerant /
resurrection-by-merge — line 139 doc). A restart clears the skip-serialized
side-map, so post-restart the TTL restarts from the first sweep — an undelivered
blob lives slightly longer, bounded and strictly safer than the grief closed.
The same applies within a single fleet lifetime: a never-covered entry
resurrected by a still-holding peer re-stamps `first_observed_ms` on this
replica and gets a fresh TTL window, so it can persist beyond a single TTL in
a continuously-merging fleet — still bounded by the store's caps, and the
deliberately-safe direction.

### Test
`gc` with a **backdated `held_at`** (far in the past) but a *fresh* local
first-observation: assert the entry is **not** GC'd on the sweep that first
observes it, and is only removed once `first_observed_ms + TTL < now`. (Pre-fix:
removed on the first sweep because `held_at.wall_ms + TTL < now`.)

---

## Fix C — C11: local-receipt inbox TTL (`src-tauri/src/dm_inbox_ingest.rs`)

### Current (vulnerable)
The `ingest_pending` GC pass expires an entry on
`deposited_at.wall_ms + INBOX_TTL_MS < now` (line 412). `deposited_at` is
**butler-minted** and adopted earliest-wins on merge (`dm_inbox_crdt.rs` line
217). A malicious/skewed butler backdates `deposited_at`; the recipient's GC drops
the DM as pre-expired before it is delivered while the sender holds a valid ack →
silent loss. (The ingest loop itself does not pre-filter on TTL — it delivers then
GCs — so the GC pass at line 411 is the single TTL decision site.)

### Fix
Symmetric to Fix B:

1. Add `#[serde(skip)] first_observed_ms: BTreeMap<String, u64>` to `DmInboxDoc`.
2. In the `ingest_pending` GC pass (lines 409-415): lazily stamp
   `first_observed_ms[key] = now_ms` for any key not present, expire on
   `first_observed_ms[key] + INBOX_TTL_MS < now`, and prune the side-map for
   removed keys.
3. `deposited_at` untouched as FWW metadata. The `InboxEntry.received_at =
   deposited_at` display value (line 356) is left as-is (cosmetic; out of scope —
   it is what the sender claims, and does not gate delivery).

As with Fix B, this trades fleet-wide TTL determinism for a per-replica soft
deadline: a never-covered entry resurrected by a still-holding peer re-stamps
`first_observed_ms` on this replica and gets a fresh TTL window, so an
undelivered DM can persist beyond a single TTL in a continuously-merging
fleet — bounded by the inbox's store caps, and the deliberately-safe
direction (over-retaining an undelivered DM beats dropping a live one).

### Test
`ingest_pending` with a **backdated `deposited_at`**: assert the entry is
delivered (ingested) and **not** GC'd as pre-expired on the sweep that first
observes it; a later sweep past `first_observed_ms + INBOX_TTL_MS` removes it as
normal. (Pre-fix: GC'd immediately because `deposited_at.wall_ms + TTL < now`.)

---

## File / task map

| Task | File | Change |
|---|---|---|
| A (ES) | `src-tauri/src/storage_records.rs` | `received_at_ms` on two records + stamp at ingest; `evict_stalest` → newest-received-first (`evict_overflow`); update 5 call sites; flood test |
| B (RH) | `src-tauri/src/community_relay_hold_crdt.rs` | `#[serde(skip)] first_observed_ms` on doc; local-receipt TTL in `gc`; backdated-hold test |
| C (C11) | `src-tauri/src/dm_inbox_ingest.rs` (+ `dm_inbox_crdt.rs` doc field) | `#[serde(skip)] first_observed_ms` on `DmInboxDoc`; local-receipt TTL in `ingest_pending` GC; backdated-deposit test |

Each task is independently testable and independently reviewable; they share only
the threat model, not code. One PR (all harmony-client, per the one-PR-per-repo
rule).

## Testing strategy

- Unit tests colocated with each module (`#[cfg(test)]`), asserting the *pre-fix
  failure mode* is closed (each test fails on the current code and passes after
  the fix — TDD).
- Full CI-parity sweep before PR: `cargo fmt --all -- --check`, the clippy gate,
  and the full `--workspace --all-targets --no-fail-fast` nextest run.
- Wire-format pinning fixtures must remain green (proves the `#[serde(skip)]`
  fields did not move canonical bytes).

## Out of scope (tracked elsewhere)

- ZEB-853 T-MISC (Medium) — the lower-severity bounded-time cleanups.
- The `InboxEntry.received_at` display value (cosmetic).
- Observability of the new eviction/GC decisions (ZEB-855, Low).
