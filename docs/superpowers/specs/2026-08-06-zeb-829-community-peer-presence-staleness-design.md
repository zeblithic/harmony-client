# ZEB-829 — communitySync staleness: gate on real per-community peer presence

**Status:** Design approved 2026-08-06. Branch `zeb-829-community-peer-presence-staleness` off `main@a615fdac`.

## Problem

`NetworkHealthSnapshot.community_sync: Vec<CommunitySyncHealth>` reports one row per
live community sync engine, each with a `staleness` tier (`fresh` / `quiet` / `dark`,
or `null`). The tier answers: *"is this community's sync wedged — should an operator
worry?"*

Today `derive_sync_staleness` (`src-tauri/src/network_health.rs:2032`) suppresses the
tier to `null` **iff `last_inbound_ms == None`** — i.e. when no publish has ever
arrived. The ZEB-805 spec described this rule as `null` "when the community has no
peers to sync with", mirroring ZEB-804's `null`-under-`noConnection`. But *"nothing
ever arrived"* and *"no peer to sync with"* are **not the same predicate** — the
shipped rule is a proxy, because the community sync engine has no per-community peer
count to consult.

The proxy has one materially annoying consequence (imprecision 1 in the ticket):

> A community that had traffic and then lost every peer keeps a non-`None`
> `lastInboundMs`, so it renders `dark` instead of `null`. Sync genuinely has not
> advanced, so the tier is not a lie — but it is not actionable, and it fires for
> **every** community on a node that goes offline for a while.

The doc comment at `network_health.rs:2028-2031` already records that gating on a
**global** connection state is the wrong fix (it would silence every community at once
on a signal none is individually keyed to) and names this ticket as the real one.

## Goal

Replace the "never received" proxy with a **real per-community reachable-peer signal**,
so a row goes `null` because there is genuinely no co-member to sync with — not merely
because nothing has arrived yet. Observability only; retry policy (ZEB-761) is unchanged.

## Chosen semantics — "peer-gate + inbound guard" (Option B)

Of the two candidate rules, **Option B** was chosen over the ticket's literal
"`null` iff zero reachable peers" (Option A) because A trips the ticket's *own*
imprecision-2 caution: a freshly-joined community that has peers but hasn't synced yet
would render `dark` — a new, non-actionable false alarm. Option B requires **positive
evidence** of a wedge before claiming one.

```
derive_sync_staleness(last_inbound_ms, last_advance_ms, reachable_peers, now_ms):
    if reachable_peers == 0 || last_inbound_ms is None:
        return None          # no one to sync with, or nothing has ever arrived
    match last_advance_ms:
        None      -> Dark    # arrivals, never merged (the ZEB-805 wedge shape)
        Some(a)   -> age = now_ms.saturating_sub(a)
                     age >= STALENESS_DARK_MS  -> Dark
                     age >= STALENESS_QUIET_MS -> Quiet
                     else                      -> Fresh
```

Equivalently: `null` iff `reachable_peers == 0 || last_inbound_ms.is_none()`.

### Behaviour table (only **bold** cells change from today)

| inbound | last-advance | reachable | Today | Option B (chosen) |
|---|---|---|---|---|
| None | None | 0 | null | null |
| None | None | >0 (fresh join) | null | null |
| Some | recent | >0 | fresh/quiet | fresh/quiet |
| Some | old | >0 (ZEB-805 wedge) | dark | dark |
| Some | old | **0 (lost all peers)** | **dark** | **null** |
| Some | None (arrivals, no merge) | >0 | dark | dark |
| Some | None | **0 (lost all peers)** | **dark** | **null** |

**Net behavioural change: exactly the two "lost all peers after having traffic"
cells flip `dark → null`.** That is imprecision 1, fixed. ZEB-805 wedge detection
(arrivals-without-merge, and advanced-then-stalled, both with peers present) is
preserved unchanged. The safe direction ZEB-804 established — never report healthy
while wedged — holds: no cell moves *toward* a healthier tier while a wedge is live.

## The signal — reachable co-members, folded over the existing `peers` vec

The key data-path finding: in `NetworkHealthService::snapshot()`
(`network_health.rs:2667`), the `peers: Vec<PeerHealth>` is fully built at line 2897
(via `filter_peers_by_shared_membership`) **before** the `community_sync` rows are
assembled at 3018-3026. Each `PeerHealth` already carries:

- `owner_addr` — hex of the peer's `OwnerAddr([u8;16])`,
- `connection_mode: ConnectionMode` (`Direct` / `Relay` / `Degraded` / `NoConnection`),
- `shared_communities: Vec<String>` — full-hex `SpaceId`s the peer shares with me,
  stamped from `my_memberships.communities_shared_with(&owner_addr)`.

`filter_peers_by_shared_membership` emits **every** non-self resolver record sharing
≥1 community with me and does **not** cap/truncate — so a fold over `peers` is a
complete, uncapped count.

### New helper

```rust
/// Count reachable co-member peers per community (full-hex SpaceId key).
/// "Reachable" = any live connection mode (Direct/Relay/Degraded); NoConnection
/// does not count. Pure fold over the already-built peers vec — needs no
/// membership handle and no identity translation, because shared_communities was
/// already resolved via communities_shared_with. This is also the per-community
/// signal ZEB-803's acceptor watchdog should adopt in place of its current global
/// count_peer_states().connected.
pub(crate) fn reachable_peers_by_community(peers: &[PeerHealth]) -> BTreeMap<String, u32> {
    let mut counts = BTreeMap::new();
    for p in peers {
        if p.connection_mode == ConnectionMode::NoConnection {
            continue;
        }
        for community_hex in &p.shared_communities {
            *counts.entry(community_hex.clone()).or_insert(0) += 1;
        }
    }
    counts
}
```

### Why this and not a new membership accessor

Three identity types are in play — membership is keyed by `OwnerAddr([u8;16])`, the
live-peer transport set by iroh `[u8;32]`, and a member's devices by ed25519
`[u8;32]`. A naive intersection on transport identity would need a translation table.
The `ResolverPeerRecord` **is** that table (carries both `owner_addr` and
`iroh_node_id`), and `filter_peers_by_shared_membership` already walked it to produce
`shared_communities`. Folding over `peers` reuses that join exactly once more, so the
fix adds **no** new `NetworkHealthService` field, **no** new `MyMembershipSet` trait
method, and **no** identity mapping.

## Threading + surfacing

1. **`snapshot()`** (~line 2903, after `peers` is built): compute
   `let reachable = reachable_peers_by_community(&peers);` once.
2. **`community_sync_row`** signature gains a `reachable_peers: u32` parameter:
   `community_sync_row(id, raw, now)` → `community_sync_row(id, raw, now, reachable_peers)`.
   The call site (~3023) passes
   `reachable.get(&hex::encode(id.0)).copied().unwrap_or(0)`.
3. **`derive_sync_staleness`** gains the `reachable_peers: u32` parameter (rule above).
   Its doc comment (2028-2031) is rewritten: the global-shortcut warning stays; the
   "gating is ZEB-829" pointer becomes a description of the now-implemented rule,
   including why the `inbound == None` guard keeps fresh-joins quiet.
4. **`CommunitySyncHealth`** DTO gains `reachable_peers: u32` (`#[serde(default)]`,
   camelCase `reachablePeers`) so an operator can see *why* a row is `null` — "zero
   peers" vs "no data yet". **`CommunitySyncRaw` is unchanged** — it is engine-sourced
   (registry reads engine atomics); `reachable_peers` is snapshot-assembly-sourced and
   threaded separately, preserving that boundary.
5. **TS mirror** `src/lib/types/network-health.ts` `CommunitySyncHealth`: add
   `reachablePeers?: number` (optional for tolerance of pre-field cached snapshots,
   matching the existing `staleness?` pattern).

## Implementation verification — hex-encoding parity (silent-failure guard)

The map keys are `PeerHealth.shared_communities` entries, produced by
`communities_shared_with`; the call-site lookup uses `hex::encode(id.0)`. **These two
encodings must be byte-identical** — same case, no prefix, same byte order. If they
diverge, every lookup misses, every row reads `reachable_peers == 0`, and every
community silently renders `null` with no crash and no failing test (except the
end-to-end one below, which is why it is mandatory). The plan must (a) confirm the
exact encoding `communities_shared_with` emits and match it at the call site — ideally
by hex-encoding the `SpaceId` the same way the row's `community_short`
(`hex::encode(community_id.0)[..8]`, line 2063) already does — and (b) rely on the
end-to-end test to prove a seeded reachable peer produces a non-null tier (not a
silent zero).

## Testing

- **`derive_sync_staleness` direct** (module `tests` in `network_health.rs`): update
  existing call sites for the new arg; add cases —
  - imprecision-1 fix: `(inbound=Some, advance=old, peers=0) → None`; same but
    `peers>0 → Dark`;
  - fresh-join: `(inbound=None, peers>0) → None`;
  - ZEB-805 shape retained: `(inbound=Some, advance=None, peers>0) → Dark`;
  - boundary test `sync_tier_boundaries_track_the_advance_stamp` passes `peers=1`.
- **`community_with_no_inbound_ever_has_no_tier`** (6474): still `None` under B
  (default has `peers=0` and `inbound=None`); extend to also assert `peers>0` with
  `inbound=None` is still `None`.
- **Serde** `community_sync_row_serde_is_camel_case` (6499): add `reachablePeers` to
  the camelCase key-list (6519-6534) and the no-snake-leak list (6537-6553); add a
  `#[serde(default)]` absence-tolerance test (JSON without `reachablePeers` → `0`).
- **End-to-end through `snapshot()`**: extend
  `snapshot_community_sync_section_present_iff_source_installed` (5217) — seed a
  `FakeResolver` record + membership so one community has a reachable co-member and
  assert the row's tier is derived from advance; a zero-peer variant asserts `null`.
  This is the only test exercising the full `peers → reachable_peers_by_community →
  community_sync_row → derive_sync_staleness` path.
- **Registry integration** `publish_retry_backoff_surfaces_in_community_sync_row_zeb762`
  (`community_state_sync.rs:8154`): update the `community_sync_row` call site for the
  new arg (pass a fixed non-zero value so its publish-retry assertions are unaffected).

No golden/byte fixtures pin the network-health DTO (the wire_format community fixtures
pin CRDT bytes, not this IPC DTO), so the only mandatory mirror updates are the two
serde key-lists and the TS interface.

## Non-goals

- No change to `CommunitySyncRaw` or any sync engine; no retry-policy change.
- **No ZEB-803 watchdog rewire.** `reachable_peers_by_community` is written to be the
  reusable signal ZEB-803 adopts (replacing its global `count_peer_states().connected`),
  but wiring it into the watchdog is that ticket's scope, not this PR's.
- No live-panel widget — wire + TS type only, exactly like `community_sync` /
  `fleet_sync`; reaches operators via the full-snapshot diagnostic export.

## Files

- `src-tauri/src/network_health.rs` — `reachable_peers_by_community` helper;
  `derive_sync_staleness` + `community_sync_row` signatures + rule; `CommunitySyncHealth`
  field; `snapshot()` wiring; doc comment; tests.
- `src-tauri/src/community_state_sync.rs` — one test call-site update.
- `src/lib/types/network-health.ts` — `reachablePeers?: number` on `CommunitySyncHealth`.
