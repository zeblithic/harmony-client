# ZEB-820 — Multi-device vine relay set: publish an aggregated ≤4-entry set

**Status:** design of record
**Ticket:** ZEB-820 (Medium, `harmony-client`)
**Author:** Koya (koya-zeblith.lan)
**Date:** 2026-08-04

## Problem

The case-E vines publisher (`src-tauri/src/pkarr_vines_publisher.rs`) advertises a
creator's serve-capable devices under the vines pkarr slot (slot key = creator
address + epoch). Today `build_blob` hardcodes `relay_set = vec![self]`
(`pkarr_vines_publisher.rs:50-53`): **each device publishes only itself**.

Because every device signs with the creator's **identity** key and writes the
**same** slot, the DHT/relay resolves last-writer-wins by BEP44 seq — so a
follower sees whichever single device published most recently, never the
`VINE_RELAY_SET_MAX = 4` set spanning the creator's devices that ZEB-811 §1
envisioned. Follow-only delivery therefore works only while that one device is
online; the other devices' serve capacity is invisible.

### Premise refinement (verified against source)

The ticket names the ZEB-173 enrollment CRDT as the device-set source. That
layer is **identity-only** — `OwnerState.enrollments` carries device *pubkeys*,
not transport coordinates — so it alone cannot build a `VineRelayEntry`
(`iroh_endpoint_id` + `home_relay`). The correct source already exists:
`FleetNetDoc` (`src-tauri/src/fleet_net.rs`), a fleet-replicated own-device
roster whose `FleetNetRow` carries exactly `iroh_endpoint_id: [u8; 32]` +
`home_relay: String` per device, plus a `seen_at: Hlc` staleness clock. It
already feeds the `ReachabilityResolver` (ZEB-510) and the butler-set
advertisement. So this is **wiring + reuse**, not blocked on unbuilt binding
work.

## Approved approach — mirror the butler-set (reuse the hardened sort)

`build_butler_set` / `butler_set_order` (`fleet_net.rs`) already solve both
"needs design" questions the ticket flagged. We reuse them.

### Who publishes (merge authority): every device, no leader

Every device independently publishes the **full aggregated set** built from its
local `FleetNetDoc` snapshot. Since all devices share the same fleet-replicated
CRDT and all sign the same slot, last-writer-wins now converges on a *complete*
set (the freshest full set wins), not a single-device set. This is exactly the
butler-set model — no leader election, no failover hole. A leader-publish
variant was rejected: it adds coordination and a "leader offline → set goes
stale" failure mode for no benefit, since redundant publishes of a convergent
set are harmless.

### Staleness aging: reuse `butler_set_order` wholesale

`butler_set_order(doc, stale_before_ms)` filters `FleetNetRow`s by `seen_at`
(lower bound `stale_before_ms`, upper bound a future-skew clamp), sorts
freshest-first with an ascending-`device_id` tiebreak, and promotes the owner's
pinned device to slot 0. That sort was hardened against peer-inflation twice —
ZEB-852 (clamp `wall_ms` to receiver `now`) and ZEB-856 R2 (remove the
peer-settable `logical` axis) — so reusing it inherits that hardening for free.

**Key coupling (load-bearing):** `butler_set_order` recovers the caller's `now`
internally as `stale_before_ms + BUTLER_SET_FRESHNESS_MS` (`fleet_net.rs:277`).
A caller MUST pass `stale_before_ms = now − BUTLER_SET_FRESHNESS_MS` or the
upper clamp is computed against the wrong `now`. The vine aggregator
encapsulates this subtraction so no call site can get the inversion wrong.

### The aggregator — `build_vine_relay_set`

New pure function in `fleet_net.rs`, beside `build_butler_set`:

```rust
/// Aggregate the creator's active devices into a vine relay set
/// (max VINE_RELAY_SET_MAX). Mirrors `build_butler_set`: `self_entry`
/// (the publishing device's live transport data — online by definition at
/// publish time) is force-included exactly once; sibling rows fill the
/// remaining slots freshest-first via the shared `butler_set_order` sort;
/// stale rows are excluded. `now_ms` is the receiver clock; the freshness
/// window is `BUTLER_SET_FRESHNESS_MS` (same as butlers — same devices,
/// same heartbeat).
pub fn build_vine_relay_set(
    doc: &FleetNetDoc,
    self_device_id: &str,
    self_entry: VineRelayEntry,
    now_ms: u64,
) -> Vec<VineRelayEntry>
```

Semantics (mirroring `build_butler_set`'s self logic, minus the `vk_lookup`
layer vines don't need — `VineRelayEntry` carries only `iroh_endpoint_id` +
`home_relay`, both present directly in `FleetNetRow`):

1. `stale_before_ms = now_ms.saturating_sub(BUTLER_SET_FRESHNESS_MS)`.
2. Iterate `butler_set_order(doc, stale_before_ms)`, taking at most
   `VINE_RELAY_SET_MAX` entries.
3. When the ordered device is `self_device_id`, push `self_entry` (its live
   iroh endpoint + home relay, fresher than any snapshot row) and mark
   `saw_self`.
4. Otherwise map the `FleetNetRow` to
   `VineRelayEntry { iroh_endpoint_id, home_relay }`.
5. If `self` was never seen (its row is stale/missing, or fresh siblings filled
   the cap), force-insert `self_entry` at the front, evicting the
   lowest-priority (last) entry if the set is already full. The publisher is
   online by definition, so its live entry always belongs.

**Pin promotion is inherited.** Reusing `butler_set_order` means the owner's
pinned device leads the ordering. `VineRelayEntry` has no `pinned` field, so no
pin metadata is transmitted — but the pin is still observable *in effect*: it
sets the serialized **order** (a dialing-preference hint) and, when there are
more fresh devices than the cap, **which** devices make the set. When self must
be force-included, a fresh pinned sibling keeps slot 0 (mirroring
`build_butler_set`), rather than being displaced by self.

## Wiring

`PkarrVinesPublisher::new` gains two parameters (mirroring its existing
`has_own_vines` closure pattern):

- `self_device_id: String` — the SP1 64-hex fleet-net device id (the same
  binding the butler blob builder passes as `self_device_id` to
  `build_butler_set`; in scope at the `lib.rs:9749` construction site).
- `fleet_snapshot: Arc<dyn Fn() -> FleetNetDoc + Send + Sync>` — captures
  `Arc::clone(&fleet_net_snapshot)` (the `Arc<RwLock<FleetNetDoc>>` already in
  scope at `lib.rs:9749`) and returns a read-locked clone. A closure (not the
  `RwLock` itself) keeps the publisher decoupled from the lock type and keeps
  tests trivial (`Arc::new(FleetNetDoc::default)` → `[self]`).

`build_blob` / `build_blob_or_retraction` change to accept the
already-aggregated `relay_set: Vec<VineRelayEntry>` instead of a single
`endpoint_id` + `home_relay`. The `record_builder` closure in
`reconcile_locked` (the per-tick hot path) builds `self_entry` from its live
endpoint read (unchanged — still fresh every tick, ZEB-521), takes a fresh
`fleet_snapshot()`, and calls `build_vine_relay_set(&doc, &self_device_id,
self_entry, now_ms)`. The aggregated set is built only on the gate-open path;
the gate-closed **retraction** stays the empty-set record it is today
(unchanged — ZEB-811/822 retraction behavior is untouched).

Everything else in the publisher — the share gate, the reconcile lock, the
retraction paths, `enable`/`disable`/`republish`, the `no-endpoint → nothing to
advertise` short-circuit — is unchanged.

## Convergence & churn (accepted)

Two devices with momentarily-divergent `FleetNetDoc` views can publish slightly
different sets (e.g. one includes a borderline-fresh sibling the other has aged
out), and LWW churns between them. This is bounded and benign: every published
set is complete-enough (each contains its publisher + the freshest siblings it
knows), the freshest publish wins, and the fleet CRDT converges. The butler-set
advertisement has the identical property and accepts it.

## Security

Every row in `FleetNetDoc` is there only because it decrypted under the owner's
fleet KeyTree (`sibling_reachability_payload`'s doc: the trust boundary is the
symmetric-key decrypt, not a per-record identity signature). So only the
creator's *own* trusted devices ever enter the vine set, and the whole record
is signed by the creator's identity key. A compromised/buggy sibling inflating
its `seen_at` to hog slot 0 is handled by the inherited ZEB-852/856 clamp +
device-id tiebreak. No untrusted transport data reaches a follower.

## Wire format

**No change.** `VineRelayRecordPayload.relay_set` is already a `Vec` and
`build_vines_record_blob` already **rejects** a set larger than
`VINE_RELAY_SET_MAX = 4` (`pkarr_vines.rs:86`) — the aggregator caps at exactly
4, so an encode can never reject. `verify_vines_record` already bounds the
decoded set the same way. Followers that already parse a multi-entry
`relay_set` (the resolve/consume path at `pkarr_vines.rs:169`,
`vine_pull_driver.rs`) need no change.

## Testing

**Pure `build_vine_relay_set` (unit, in `fleet_net.rs` tests):**

- Empty/default doc → exactly `[self_entry]` (force-included).
- Doc with 3 fresh siblings + a self row → 4 entries including `self_entry`;
  self's snapshot row replaced by the live `self_entry`.
- Doc with 6 fresh siblings → capped at 4, `self_entry` force-included.
- A stale sibling (`seen_at` below the floor) is excluded.
- Self row stale/missing → `self_entry` force-inserted at the front.
- A future-skewed sibling does not out-rank an honest present row (inherits the
  ZEB-852 clamp — assert ordering, pinning the reuse).

**Publisher level (extend existing `pkarr_vines_publisher.rs` tests):**

- Existing tests construct `PkarrVinesPublisher::new(..)` with a default fleet
  snapshot (`Arc::new(FleetNetDoc::default)`) + a self device id → the set
  stays `[self]`, so `blob_contains_self_entry_when_enabled` and the squat /
  retraction tests keep asserting the single-entry shape unchanged.
- New: with a fleet snapshot of N fresh siblings, the published record resolves
  to the aggregated set (self + siblings, capped at 4).

## Scope

- **Touches:** `fleet_net.rs` (new `build_vine_relay_set` + tests),
  `pkarr_vines_publisher.rs` (`build_blob` signature, `reconcile_locked`
  wiring, `new` params, test call sites), `lib.rs` (the `:9749` construction
  site: pass `self_device_id` + a `fleet_snapshot` closure).
- **No change to:** the wire format, `VINE_RELAY_SET_MAX`, the slot-key
  derivation, the share gate / retraction behavior, `butler_set_order` /
  `build_butler_set` themselves, or any follower-side resolve/consume code.
- One PR.

## Out of scope

- Any change to butler-set behavior (we only *read* `butler_set_order`).
- A vine-specific pin concept (`VineRelayEntry` gains no `pinned` field).
- Cross-device fleet-doc convergence latency (a `FleetSyncEngine` property, not
  ours to change here).

## Convergence refinement (PR #611, review round 1)

CodeRabbit (1 Minor) + Qodo (3 bugs) + CodeAnt (1 Logic) landed; Greptile
author-excluded. Four distinct findings addressed, one declined — all
second-order consequences of "reuse `butler_set_order` wholesale":

1. **Silent encode→retraction fallback** (Qodo #1, Action-required) —
   **fixed.** With aggregation, one toxic oversized sibling `home_relay` could
   blow `VINES_RECORD_BLOB_MAX_BYTES`, and `build_blob`'s `.ok()` collapsed that
   to an empty-set retraction — suppressing *every* device's vine serving, not
   just self's. Replaced `build_blob_or_retraction` with `build_publish_blob`:
   on aggregated-encode failure it logs a `warn` and retries **self-only**
   before retracting, so a bad sibling row can never suppress this device's own
   serving. Pinned by `oversized_sibling_relay_falls_back_to_self_only`.
2. **Pin promotion mis-documented + force-insert displaces the pin** (Qodo #3
   Strong + CodeAnt Logic, same root) — **fixed.** The doc claimed pin was
   "invisible / order-only"; in fact it also decides membership under the cap,
   and the `!saw_self` branch inserted self at index 0, shoving a pinned sibling
   out of slot 0 (unlike `build_butler_set`). The force-insert is now pin-aware
   (keeps a fresh pinned sibling at slot 0); comment + this spec corrected.
   Pinned by `pinned_sibling_leads_when_self_force_inserted`.
3. **E2E test didn't assert the live self entry** (CodeRabbit Minor) —
   **fixed.** `aggregated_set_includes_fresh_siblings` now captures the
   publisher's endpoint id and asserts the resolved set is exactly 3 entries and
   contains it.
4. **`FleetNetDoc` clone per publish tick** (Qodo #2, Performance) —
   **declined.** Premature: the snapshot is one owner's device roster (a handful
   of rows) and the publish cadence is hours (core `compute_next_publish_at` +
   the explicit republish hooks), so it's a few-KB clone off a cold path. The
   closure abstraction also keeps the publisher decoupled from the lock type and
   test construction trivial. If profiling ever shows it, the `Arc<RwLock<…>>`
   read-and-borrow alternative is a drop-in.
