# ZEB-969: Below-head hole healing for the channel log — design

**Status:** approved (Jake, 2026-08-21, in-session)
**Ticket:** ZEB-969 · **Branch:** `zeblith/zeb-969-below-head-heal`

## Problem

A device that misses channel messages while its link is down, then receives a
*newer* live message from the same author lane on session recovery, ends up
with a **hole below the lane head** — and both catch-up paths are structurally
unable to heal it:

1. The watermark-vector path (ZEB-585) only requests events **above** each
   `(author, device)` lane's max HLC; it cannot see the hole.
2. RBSR (ZEB-592) *does* negotiate the hole's events, but they are ingested
   through `process_inbound_packet`, where `ChannelLogReplayTracker`
   (spec §7 step 6: per-`(channel, author, device)` strict HLC monotonicity)
   rejects every below-head event as `Replay` — silently, at `debug!` level.

The tracker is rebuilt from the persisted log at boot, so restarts do not
clear the seal. Real-world incident: 0.2.9 smoke test, Krile permanently
missing four of Koya's messages while newer traffic flows fine (details in
ZEB-969).

Root cause, stated once: **the replay tracker conflates duplicate suppression
with ordering enforcement, and `ChannelLog::append` has no dedup of its own**
— strict per-lane monotonicity is the log's *only* duplicate guard, so it
cannot be relaxed without a replacement.

## Design

Three changes, all in `community_channel_log.rs` / `community_channel_log_engine.rs`:

### 1. Authoritative dedup moves into `ChannelLog::append` (ReconcileKey presence)

RBSR already defines per-event identity:
`ReconcileKey = (wall_ms, logical, device_id, event_element_hash)` — unique
per event including `React`s (whose `event.id()` is the *target's* id and was
never a usable dedup key). The log already maintains a sorted
`reconcile_entries` index with O(log n) `partition_point` presence lookup,
idempotent insert, and boot rebuild (`rebuild_reconcile_index`).

`append` gains an internal presence check on that index and **skips the tail
push for a key already present**. Return type changes from `Ok(bool)`
(seal-ready) to:

```rust
pub struct AppendOutcome {
    /// False when the event's ReconcileKey was already in the log —
    /// nothing was pushed, nothing to emit.
    pub newly_appended: bool,
    /// Tail reached the seal threshold (previous `Ok(true)`).
    pub seal_ready: bool,
}
```

Only the three production call sites destructure the return (engine local
publish ×2, inbound step 3); test call sites use `.expect("append")` and
compile unchanged. The inbound path's step 4 emits **only when
`newly_appended`** — a duplicate must never re-emit to the UI.

This is defense-in-depth: no tracker state, in any path, can duplicate the
log anymore. It also closes the TOCTOU between any earlier presence check and
the append itself (the check runs under the same `log` lock as the push).

### 2. `ChannelLogReplayTracker::record` becomes max-aware; boot rebuild loses its order-dependence

`record` currently overwrites `last_seen` unconditionally, and the engine's
boot rebuild (walk segments, then tail, `record` each event) depends on
"storage order = per-lane HLC order" — the last-walked event wins. A healed
below-head event sits *late in storage order* with an *old* HLC, so under
blind-overwrite semantics it would **regress the lane head on respawn**,
re-opening a duplicate-accept window on the live path.

Fix: `record` advances only when the event's HLC is strictly newer than the
stored `last_seen` (max-fold). The live 2c path is unaffected
(`would_accept` already guarantees strictly-newer there); the boot rebuild
becomes order-independent by construction. The segments-then-tail ordering
comment in the engine and the "overwrites unconditionally" contract comment
are updated to match.

### 3. Provenance-gated below-head accept on the reconcile ingest path

`process_inbound_packet` gains a provenance parameter:

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IngestProvenance {
    /// Live zenoh subscriber (also carries legacy watermark-GET reply
    /// pages, which are above-watermark by construction and never need
    /// below-head admission).
    Live,
    /// RBSR reconcile recovery (`rbsr_ingest_and_next`) — the only path
    /// that can deliver below-head hole events.
    Reconcile,
}
```

Exactly two call sites: the subscriber loop passes `Live` (unchanged
behavior, byte-for-byte identical semantics and cost); `rbsr_ingest_and_next`
passes `Reconcile`.

Modified steps, `Reconcile` only:

1. **2a fast-path** (tracker lock): on `Err(Replay)`, do not drop yet —
   take a short `log` lock and check `contains_reconcile_key(&event)`.
   Present → true duplicate, drop exactly as today (cheap-drop preserved
   for re-served events). Absent → below-head candidate; continue to 2b
   with a `below_head` flag.
2. **2b full verify:** unchanged and load-bearing — signature, membership at
   the event's **own** HLC, and the ZEB-846 forward-skew gate all still
   apply. Below-head admission never weakens verification.
3. **2c commit** (tracker lock): `check_and_advance` returning `Err(Replay)`
   is not a drop for `Reconcile` — skip the tracker advance (the lane head
   already dominates this event; max-aware `record` makes advance a no-op
   anyway) and proceed to append.
4. **Step 3 append:** the authoritative dedup from change 1 decides. A
   concurrent duplicate that slipped past 2a loses here, under the log lock.
5. **Step 4:** emit only on `newly_appended`; when `below_head` is set, log
   at **INFO**: `below-head heal (ZEB-969)` with community, channel, author,
   device, and the healed HLC. Reconcile-path duplicate drops stay `debug!`
   (they are normal RBSR round noise).

`Live` provenance behavior is exactly today's: strict per-lane monotonicity,
same drop sites, same costs.

## Accepted semantics & trade-offs (approved)

1. **Member backdating becomes symmetric.** A valid member can now insert
   correctly-signed, backdated events below peers' lane heads by answering a
   reconcile — the same thing fresh joiners already accept from any member
   serving history. Membership-at-event-HLC remains the real boundary: a
   member kicked at T still cannot sign into the post-kick past. A signed
   log with backfill and no consensus ordering cannot distinguish "honest
   heal" from "member backdating"; we choose healing honest users over
   pseudo-protection. Documented here as a deliberate protocol-semantics
   decision.
2. **The vector fallback stays hole-blind.** Only RBSR heals holes. The
   watermark path is the legacy fallback for pre-RBSR peers and is
   above-watermark by construction; no change.
3. **Healed events land mid-history.** A day-old healed message may bump
   unread counts / render mid-timeline. Shipped as-is; polish only if it
   proves annoying in practice.
4. **Requester-side fix.** Healing requires only the *requesting* device to
   run this code; responders already serve hole events correctly. Krile on
   this build reconciling against Koya recovers the four lost messages.

## Test plan

All engine-level, in `community_channel_log_engine.rs` tests (two-fixture
pattern already used by the RBSR tests) + `community_channel_log.rs` unit
tests. TDD per task; Rust gates per CLAUDE.md.

1. **The Krile repro (headline):** requester holds `[e1, e4]` of a lane,
   responder holds `[e1, e2, e3, e4]`; RBSR round via `rbsr_ingest_and_next`
   heals `e2, e3`; log contains all four; tracker head remains `e4`.
2. **Duplicate re-serve still drops:** re-ingesting `e2` via `Reconcile`
   after the heal appends nothing and re-emits nothing.
3. **Live path unchanged:** a below-head event via `Live` provenance is
   still dropped as `Replay` (both 2a and 2c sites).
4. **Append-level dedup:** direct `append` of an event whose ReconcileKey is
   present → `newly_appended == false`, tail length unchanged.
5. **Boot-rebuild regression:** log with a healed below-head event late in
   the tail → rebuilt tracker's lane head is the lane **max**, not the
   last-walked event.
6. **`record` max-fold unit test:** recording older-than-last-seen does not
   regress `last_seen`.
7. **Verification still gates:** a below-head event failing signature verify
   is dropped (no heal bypass).

## Out of scope

ZEB-970 (watchdog restart wedge), ZEB-971 (watchdog false-positive/layering),
ZEB-972 (presence vs peers UI), backfill-driver observability beyond the
heal/drop lines above, any re-arming/cadence changes to the reconcile
drivers, and frontend changes (no UI code is touched).
