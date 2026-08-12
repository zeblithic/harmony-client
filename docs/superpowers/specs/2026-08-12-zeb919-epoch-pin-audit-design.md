# ZEB-919: Spawn-pinned membership_key() audit — per-site verdicts + live-key fixes

**Status:** audited against main @ `ebc9a795` (post-#659); design follows the ZEB-918 pattern
**Ticket:** ZEB-919 · **Branch:** `zeblith/zeb-919-audit-remaining-spawn-pinned-membership_key-consumers-for`

## Ground truth (verified)

- `CommunitySyncEngine::membership_key()` returns a key **bound at
  `spawn_engine` time and never changed for the engine's lifetime**
  (`community_state_sync.rs:1654-1663`). Rotation
  (`apply_remote_epoch_event`, `lib.rs:53281`; local kick/leave paths)
  updates ONLY the owner-state `Space.current_epoch_key` /
  `old_epoch_keys` — no engine re-spawn, so the engine's copy goes stale
  the moment a rotation lands mid-session.
- Boot re-pins: engine spawn reads `space.current_epoch_key`
  (`lib.rs:8915`), so a restart captures the then-current key. The failure
  shape is therefore ZEB-918's exactly: mid-session rotation leaves every
  pinned consumer coherently on the OLD key (an audience that includes the
  revoked member); restarts then split members into old-key/new-key
  islands by accident of who restarted when.
- The comments at `community_presence.rs:428-431/478/572` and
  `address_book_sync.rs:763` claim per-tick re-derivation "follows epoch
  rotation automatically". **The claim is false**: they re-fetch the
  engine Arc per tick, but the key inside the engine never changes.
- Live-read helpers already exist and are tested:
  `live_epoch_key` (ZEB-249 §10.6), `community_publish_epoch_key`
  (ZEB-597 publisher-degrades), `epoch_key_candidates` (ZEB-918,
  `[current, previous]` with `[spawn]` degrade).

## Verdict table (every `membership_key()` call site at `ebc9a795`)

| Site | Role | Verdict |
|---|---|---|
| `community_presence.rs:483` | presence beacon seal | **FIX** — seal live |
| `community_presence.rs:577` | presence beacon open | **FIX** — open candidates |
| `lib.rs:9798` | addrbook Reachability-row seal (announce arm) | **FIX** — seal live |
| `lib.rs:12596` | addrbook Relay-row seal (relay-publish arm) | **FIX** — seal live |
| `address_book_sync.rs:769` | addrbook snapshot-queryable seal | **FIX** — seal live |
| `address_book_sync.rs:347` | addrbook packet open (sole peer-ingest path) | **FIX** — open candidates |
| `iroh_invite_acceptor.rs:716` | open-join admission verify | **FIX** — verify live, current-only |
| `lib.rs:9037`, `lib.rs:32449`, `lib.rs:32507` | channel-log key derivation (`derive_channel_key`) | **DEFER** — follow-up ticket (see §5) |
| `lib.rs:36677` (create hook), `lib.rs:42474` (redeem hook) | case-C pkarr registration | **HYGIENE** — normalize to live helper |
| `lib.rs:10522`, `lib.rs:10834` | case-C publish (ZEB-597) | verified correct (live w/ fallback) |
| `lib.rs:12692`, `community_gateway_dial_driver.rs:216` | rendezvous (ZEB-918) | verified correct |
| `pkarr_resolver_adapter.rs:215` | seeker resolve | verified correct |
| `lib.rs:53775` | catchup-synthesis observer | verified correct — but the TODO at `lib.rs:53756-53760` is STALE (contradicted by `apply_remote_epoch_event`); delete it |
| `generate_invite_impl` (`lib.rs:35450`) | invite mint | verified correct — seals `Space.current_epoch_key` (live) |
| `community_state_sync.rs:3463/3478/4295` | state-root encode | verified correct (live + TOCTOU recheck) |

## 1. Shared helper

Add to `community_state_sync.rs`, next to `community_publish_epoch_key`:

```rust
/// Typed sibling of [`community_publish_epoch_key`]: the LIVE
/// `Space.current_epoch_key` when available, else the spawn-time
/// `fallback` (publisher-degrades, ZEB-597). For call sites that need
/// an `EpochKey` (key derivation) rather than raw bytes (pkarr registration).
pub(crate) async fn community_publish_epoch_key_typed(
    community_id: SpaceId,
    crdt_state: Option<&Arc<Mutex<crate::owner_state_crdt::OwnerState>>>,
    fallback: &EpochKey,
) -> EpochKey
```

`community_publish_epoch_key` delegates to it (`*typed(...).as_bytes()`).
Open sites reuse `epoch_key_candidates` unchanged.

## 2. Presence (both sides)

- `spawn_community_presence_publisher` / `spawn_community_presence_subscriber`
  gain a `crdt_state: Option<Arc<Mutex<OwnerState>>>` parameter
  (`None` keeps existing tests running in spawn-key mode — that is the
  documented degraded mode, not a silent behavior fork).
- Publisher per tick: `community_publish_epoch_key_typed` →
  `derive_presence_key`. Degrades to the spawn key on live-miss and still
  publishes (ZEB-597 mirror).
- Subscriber per packet: `epoch_key_candidates` → derive a presence key
  per candidate → try `open_presence_beacon` under each, first success
  wins. `[current, previous]` heals rotation skew both directions the
  membership CRDT itself needs to propagate (same load-bearing argument
  as ZEB-918 §2: an instant hard-cut would partition rotated from
  un-rotated members' presence exactly while the rotation event is in
  flight).
- Rewrite the false "follows epoch rotation automatically" comments to
  state the real mechanism.
- Cost note: candidates take the owner-state mutex per packet for a ≤2-key
  clone; presence traffic is ~0.1 Hz per member, so contention is noise.

## 3. Address book (four sites)

- `ingest_sealed_packet` gains `crdt_state: Option<&Arc<Mutex<OwnerState>>>`;
  opens with `epoch_key_candidates` → `derive_addrbook_key` per candidate.
  This is the SOLE peer-ingest path (live rows + snapshot replies), so one
  change covers both consumers. Its two callers live in
  `address_book_sync.rs` spawned tasks — thread the handle through their
  spawn fns from `event_loop`.
- `spawn_addrbook_snapshot_queryable` gains the handle; serves sealed
  under `community_publish_epoch_key_typed`.
- The two `lib.rs` seal arms (`:9798` announce, `:12596` relay-publish)
  switch their `derive_addrbook_key(&engine.membership_key(), …)` to the
  typed helper (the enclosing closures already hold a `crdt_state` clone
  for the rendezvous fix, ZEB-918).
- Directional limit, accepted as in ZEB-918: an un-rotated member cannot
  open rows sealed under the NEW key until the rotation event reaches it
  via membership sync. The previous-key rung covers the other direction
  (rotated readers keep ingesting un-rotated members' rows), which is the
  direction rotation propagation depends on.

## 4. Open-join acceptor: live key, current-only (posture decision)

`handle_open_join_inbound` verifies the joiner's `epoch_auth` MAC against
the spawn-pinned key. The mint side (`generate_invite_impl`) already
embeds the LIVE key in open-community links, so mint and verify are
incoherent today: an un-restarted acceptor admits pre-rotation links
forever; a restarted acceptor rejects them. Neither is a chosen posture.

**Decision: verify against the live key, current-only, degrading to the
spawn key only on live-read failure** (`community_publish_epoch_key_typed`
with `self.crdt_state` — the acceptor already holds the handle).

- **Why hard-cut (no previous-key rung) when §2/§3 admit candidates:**
  the previous epoch key is precisely the artifact rotation exists to
  invalidate (ZEB-911 accepted this posture for capability resolvers;
  ZEB-918 documented it as out-of-scope-but-intended). Presence/addrbook
  candidates exist to heal *member↔member* skew while rotation propagates
  — open-join is *outsider→member* admission, not a propagation path, and
  a joiner holding a fresh link simply retries against a member whose
  state has rotated.
- **Why degrade-to-spawn rather than reject on live-miss:** the spawn key
  was legitimately current at engine spawn; degraded behavior equals
  today's behavior (never worse), while rejecting would brick open-join
  on a transiently incomplete Space row.
- Extract the key choice into a helper on the acceptor
  (`admission_epoch_key(&self, community_id, &engine) -> EpochKey`) so the
  decision is unit-testable without an iroh `Connection`.

## 5. Channel-log key family — deferred (follow-up ticket)

`derive_channel_key(membership_key, cid, chid)` is consumed at engine
spawn (`register_channel_log_engine`, boot + in-session reconciles) and
the resulting `ChannelKey` is held for the channel engine's lifetime,
encrypting **wire** packets (posts, watermark seals). At-rest segments
store plaintext `SignedChannelEvent` CBOR (`flush_tail` /
`seal_and_persist`), so rotation NEVER strands history — the impact is
live-traffic coherence: post-rotation the revoked member can keep
*decrypting* channel traffic until members restart (their posts are
already rejected by the membership gate), and restart-split members
silently drop each other's posts.

Fixing it means threading a key *provider* (live + previous candidates)
through `ChannelLogRegistry::spawn` / the engine's `channel_key` and
re-keying live engines on rotation — a different seam with its own
migration story, PR-sized on its own. Filed as a follow-up ticket
(referenced in the PR body); folding it in would repeat the mistake
ZEB-918 explicitly avoided.

## 6. Hygiene

- `lib.rs:36677` / `lib.rs:42474`: both fire at moments where spawn==live
  by construction, but normalize to `community_publish_epoch_key_typed`
  so no direct-pin registration site survives grep.
- Delete the stale `TODO(zeb-249-followup)` at `lib.rs:53756-53760` — it
  claims remote rotations don't update `crdt_state`, which
  `apply_remote_epoch_event` (same file, below it) has done since ZEB-249
  §10.6 landed.

## Testing

1. **Helper:** `community_publish_epoch_key_typed` returns live when
   Space complete, spawn fallback when absent/incomplete (both pinned).
2. **Presence rotation:** packet sealed under OLD key opens on a node
   whose crdt shows `current=NEW, old_epoch_keys[n-1]=OLD` (previous-rung);
   packet sealed under NEW opens (current); publisher with rotated crdt
   seals under NEW even though its engine still pins OLD; `None`
   crdt_state preserves spawn-key behavior (degraded-mode pin).
3. **Addrbook rotation:** `ingest_sealed_packet` opens an OLD-sealed
   packet post-rotation via candidates; the snapshot queryable serves
   NEW-sealed; existing ingest tests keep passing with `None`.
4. **Acceptor:** `admission_epoch_key` returns live post-rotation (a
   pre-rotation link's `epoch_auth` now fails `verify_epoch_auth`),
   spawn key on live-miss. Existing `verify_and_admit_open_join` tests
   are unaffected (key is a parameter there).
5. **Stale-comment removal** is comment-only; no test.
6. Gates: fmt, clippy `--all-targets`, `scripts/test-select --context task`
   per task, full `--workspace --all-targets` sweep pre-PR.

## Rollout / compatibility

No wire-format, CRDT-schema, or IPC changes — every packet format is
unchanged; only *which* key seals/opens/verifies moves. Mixed-version
behavior mirrors ZEB-918: an old binary behaves like today's un-restarted
member (pinned key) and is covered by new binaries' previous-key rungs
until the natural record/window decay; a new binary is never worse than
today in degraded mode because every degrade lands on the spawn key.
