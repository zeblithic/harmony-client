# ZEB-918: Rendezvous slot keys — live membership-epoch key + engineered rotation overlap

**Status:** approved direction (Jake, 2026-08-11); design refined after code verification (2026-08-12)
**Ticket:** ZEB-918 · **Branch:** `zeblith/zeb-918-rendezvous-slot-keys-derive-from-spawn-pinned-membership` · **Base:** main @ `3d96d672`

## Problem

Community rendezvous beacons are published and resolved under slot keys derived
from the **membership epoch key**. Both sides of the rendezvous pair use the
engine's **spawn-time** capture (`CommunitySyncEngine::membership_key()`), never
the live key:

- **Publisher** — `lib.rs:12640`: the relay-publish loop passes
  `engine.membership_key()` into `CommunityRendezvousPublisher::refresh_slot`.
- **Member resolver** — `community_gateway_dial_driver.rs:206-213`:
  `ProdGatewayDialCtx::epoch_key_of` returns the spawn-time key, with a comment
  making the pin *deliberate* so it matches the pinned publisher ("a mismatch
  derives a different slot keypair and silently resolves nothing, forever").

The pin-to-match-pin coherence holds only within one process lifetime. After a
membership epoch rotation (Kick → `EpochRotation`):

1. Un-restarted members keep publishing beacons under the **old** key
   indefinitely — an audience that includes the revoked member, whom the
   rotation exists to exclude.
2. Restarted members re-capture the **new** key at spawn, so their beacons
   become invisible to every un-restarted member's resolver.
3. The old-key/new-key compatibility window is an accident of who restarted
   when — not an engineered tolerance.

The state-root path already fixed this class: `live_epoch_key()`
(ZEB-249 §10.6) reads `Space.current_epoch_key` from the owner-state CRDT, and
the case-C pkarr publisher degrades to the spawn key only on error (ZEB-597,
`community_publish_epoch_key`). `pkarr_resolver_adapter.rs:215-217` already
resolves with the live key. Rendezvous is the straggler.

## Verified current state (main @ `3d96d672`)

| Fact | Where |
|---|---|
| Publisher passes spawn-pinned key | `lib.rs:12640` |
| Member resolver deliberately pins to match | `community_gateway_dial_driver.rs:206-213`, doc at `:51` |
| Live-read helper exists; seeker-skips vs publisher-degrades semantics established | `community_state_sync.rs:3312` (`live_epoch_key`), `:3353` (`community_publish_epoch_key`, ZEB-597) |
| Previous epoch keys are already archived on both rotation-apply paths | `lib.rs:49711` (local kick path), `lib.rs:53279` (`apply_remote_epoch_event`, rotation), `lib.rs:53347` (catchup) → `Space.old_epoch_keys: BTreeMap<u64, EpochKey>` |
| `refresh_slot` bakes epoch-key bytes into the registered publish closure; only the weekly **time** epoch is re-derived per publish | `community_rendezvous_publisher.rs:228-236` |
| Rotation force-wakes the relay/rendezvous publish loop (membership-change trigger), so a live-read caller converges within one refresh | `lib.rs:12620-12645` (loop doc) |
| Core resolve already scans a time-epoch tolerance window per probe | `harmony-pkarr/src/rendezvous.rs` (`resolve_rendezvous_with`, `epoch_tolerance_window`) |
| Beacon freshness gate: records valid for 7 days from `announced_at` | `reachability_record.rs:18` (`REACHABILITY_RECORD_TTL_MS`) |

Two independent epoch axes exist and must not be conflated: the weekly
**time** epoch (`current_epoch_id(now_ms)`, already tolerance-windowed on
resolve, already re-derived per publish) and the **membership** epoch
(rotation counter + key; the subject of this ticket).

## Design

**Invariant after this change:** a publisher publishes a community beacon ONLY
under the community's live membership epoch key; member resolvers may read one
membership epoch back. All old-key discoverability is bounded by record
freshness, not by process lifetime.

### 1. Publisher: live key at every refresh

In the relay-publish loop (`lib.rs` slot-refresh arm), replace
`engine.membership_key()` with a live read:

```rust
let (epoch_key, _epoch) = match crate::community_state_sync::live_epoch_key(
    c, crdt_state.as_ref(), &engine.membership_key(),
).await {
    Ok(pair) => pair,
    Err(_) => (engine.membership_key(), None), // ZEB-597 mirror: publisher degrades, still publishes
};
rendezvous_publisher.refresh_slot(c, epoch_key, advertisers, actor).await;
```

Publisher-degrades (not seeker-skips) semantics deliberately mirror ZEB-597:
in the common case live == spawn so this is a strict improvement; on a
degraded read the node still publishes *something* and the resolver's
previous-epoch rung (below) covers the skew. The owner-state CRDT handle
(`crdt_state`) is available in `start_node` scope and is threaded into the
publish closure.

No `encode_root_packet`-style recheck-retry loop: a rotation landing mid-refresh
means at most one publish cycle under the outgoing key — the membership-change
force-wake re-runs `refresh_slot` with the new key immediately after, and a
briefly-stale beacon is a bounded discovery-capability exposure (identical in
kind to the natural overlap window), not a backward-secrecy encryption gap.

### 2. Member resolver: live key + previous-epoch fallback rung

`GatewayDialCtx::epoch_key_of` becomes an ordered candidate list:

```rust
/// Ordered membership-epoch key candidates for beacon resolution: the LIVE
/// current key first, then (when one exists) the immediately-previous epoch's
/// key from `Space.old_epoch_keys`. Never more than one epoch back.
async fn epoch_key_candidates_of(&self, community: &SpaceId) -> Vec<EpochKey>;
```

`ProdGatewayDialCtx` (gains a `crdt_state` handle) resolves candidates as:

- Live read OK → `[current]`, plus `old_epoch_keys[current_epoch − 1]` when
  present → `[current, previous]`.
- Live read unavailable/incomplete → `[spawn_time_key]` (degrade, matching the
  publisher, so pub/resolver stay coherent in degraded mode; the ZEB-596
  seeker-skip rationale — don't probe under a known-stale key — loses to
  coherence here because the gateway ladder IS the community's healing path).

The ladder's resolve attempt tries candidates in order and stops at the first
live beacon. The previous-key attempt runs only when the current-key resolve
yields nothing (no extra probe cost on the healthy path). Both attempts share
one ladder rung's starvation bookkeeping — candidates are an implementation
detail of a single resolve attempt, not extra rungs.

**Why the resolver-side fallback is load-bearing (not optional):** rotation
propagates *through* connectivity. Without it, a member that ingests the
rotation instantly loses beacon-discovery of every not-yet-rotated member —
partitioning the community into rotated/un-rotated islands exactly when the
un-rotated island needs a connection to *receive* the rotation event. The
previous-key rung lets rotated members keep finding un-rotated members'
beacons; once the rotation event reaches them, current-key resolution takes
over and the rung goes quiet.

### 3. No explicit dual-publish (design change from the initial lean)

The initial proposal included publishing under the previous epoch key for a
bounded window. Verification shows this is unnecessary and strictly worse:

- **Coverage it would add is already covered.** Un-rotated resolvers finding a
  rotated publisher: the publisher's last pre-rotation record remains on the
  relay under the old slot key and stays freshness-valid for up to
  `REACHABILITY_RECORD_TTL_MS` (7 days) from its `announced_at` — and the core
  resolver's time-epoch tolerance window keeps probing it across the weekly
  time-epoch boundary. Rotated resolvers finding un-rotated publishers: the
  previous-epoch rung (§2).
- **Cost it would add is real.** Dual-publish extends the revoked member's
  old-key read window with *actively refreshed* (always-fresh) records, doubles
  rendezvous publish traffic during the window, and adds publisher-side window
  state. The natural window decays instead of being renewed.

Post-rotation exposure summary: the revoked member (who holds the old key and
can derive old slot keys forever) can read only beacons that were last
published before their publisher rotated, for at most 7 days of record
freshness — versus today's *indefinite* fresh-beacon exposure from every
un-restarted member.

### 4. Comment and doc rewrites

The two "deliberately pinned" comments become statements of the new invariant:
`community_gateway_dial_driver.rs:51` (ctx field doc) and `:206-213`
(`epoch_key_of` body), plus the `refresh_slot` doc note that the caller now
supplies the live key and the force-wake bounds staleness to one cycle.

## Out of scope (audited, deliberately untouched)

- **Capability resolvers** — invite `epoch_snapshot` (ZEB-911 witness ladder)
  and open-join link capabilities hold a key minted at issuance time. Their
  post-rotation hard cut (once the natural window decays) is the *intended*
  security posture; ZEB-911's design explicitly accepted it. No change.
- **Other spawn-pin sites** — presence key derivation
  (`community_presence.rs:483,577`), address-book seal keys
  (`address_book_sync.rs:347,769`, `lib.rs:12559`), open-join acceptor
  admission key (`iroh_invite_acceptor.rs:716`), and invite-mint sites
  (`lib.rs:32383,32441,36611,9037,9798`). Each is a separate
  publisher/consumer coherence pair with its own rotation story; folding them
  in would balloon the change. A follow-up ticket enumerating these sites is
  filed as part of this work and referenced in the PR.

## Testing

1. **Unit — candidate ordering:** `epoch_key_candidates_of` returns
   `[current]` pre-rotation, `[current, previous]` post-rotation,
   `[spawn]` when live state is unavailable; never more than one epoch back.
2. **Unit — publisher live read:** the refresh arm passes the live key when
   `Space` is complete and degrades to the spawn key on
   `LiveEpochKeyMissing` (both directions pinned).
3. **Ladder — rotation skew both ways:** with the dial driver's existing stub
   seams: (a) rotated resolver + un-rotated publisher → beacon found via the
   previous-key candidate; (b) un-rotated resolver + rotated publisher →
   beacon found via the still-fresh old-key record; (c) healthy path → single
   candidate, no extra probes.
4. **E2E regression (reuses the #657 harness — real
   `CommunityRendezvousPublisher` → `PkarrPublisher` → strict
   `MockPkarrRelay` → `PkarrResolver`):** publish, rotate the epoch key,
   force a refresh, assert the new-key slot record appears; assert the old-key
   record still resolves (natural window) and the new-key record carries the
   vouch/bounded payload invariants from #657.
5. Full local gates: workspace nextest `--all-targets`, clippy
   `--all-targets -D warnings`, fmt; frontend untouched.

## Rollout / compatibility

No wire-format, CRDT-schema, or IPC changes. `old_epoch_keys` already exists
and replicates. Mixed-version fleets degrade gracefully: an old-binary
publisher behaves like today's un-restarted member (pinned key) and remains
discoverable to new-binary resolvers via the previous-epoch rung until it
restarts or upgrades; an old-binary resolver behaves like today and benefits
from nothing but loses nothing.
