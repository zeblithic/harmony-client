# Community Presence (online/reachable members) — Design Spec

**Ticket:** ZEB-537 (child of ZEB-533 fleet-collaboration epic). Siblings: ZEB-534 mentions (done), ZEB-535 CAS artifacts (done backend), ZEB-536 reactions (AVALON).

**Goal:** Surface which members of a community are currently online/reachable, so a user knows before asking or handing off. A member is "present" in a community when their node is actively beaconing liveness for that community within a staleness window; surfaced to the frontend as an event and rendered against the member list.

**Approved design (2026-06-21):** Generalize the proven voice-presence beacon pattern (`voice_presence.rs`, ZEB-350) from per-call scope to per-community scope. **Active community only** — a node beacons + subscribes presence for the one community currently in view (chosen for power/bandwidth at scale).

---

## 1. Semantics

- **Presence = "actively reachable in this community right now."** A node publishes a periodic signed+sealed liveness beacon on the community's presence topic *only while that community is the active/subscribed one*. Peers mark an owner **online** while a fresh beacon (within the staleness window) exists; **offline** once it goes stale (TTL eviction) or the node stops beaconing (switched away / app closed).
- **Owner-level surfaced, device-level tracked.** Internally the roster is keyed by device (a member may run multiple devices); the frontend event aggregates to owner-level `online` plus a `deviceCount`.
- **No explicit "left" tombstone in v1.** Unlike voice (which needs immediate call-leave), presence relies purely on TTL staleness — simpler, and "stop beaconing → peers TTL you out in ~staleness-window" is the correct semantic for active-community presence. (Explicit fast-offline on leave is a possible later enhancement.)

## 2. Wire format & crypto (mirrors voice_presence.rs)

New module `src-tauri/src/community_presence.rs`.

- `PresenceBeacon` — canonical CBOR, 2-char same-length keys:
  - `ow`: `[u8;16]` owner addr
  - `dv`: `[u8;32]` device verifying key
  - `sh`: `Hlc` `started_hlc` — the publisher's process/session start HLC (advances on restart)
  - `sq`: `u64` `seq` — monotonic per session, restarts at 0 each (re)start
- `SignedPresenceBeacon { bc: PresenceBeacon, sg: [u8;64] }` — detached ed25519 device-key signature over `canonical_cbor_encode(beacon)`.
- Both registered as `CanonicalPayload` (sealed-trait), same as the voice beacons.
- **Seal under the community epoch key**, not a voice `ChannelKey`. Presence packets are sealed/opened with the same members-only community epoch key used to seal community state-root / channel-log packets, with a **distinct AAD** (`COMMUNITY_PRESENCE_AAD`) so a presence packet can never be confused with a state-root or channel-event packet. Framing `[nonce][ct+tag]`.
- `sign_presence_beacon` / `verify_presence_beacon_sig` / `seal_presence_beacon` / `open_presence_beacon` mirror the voice equivalents. A `*_with_nonce` deterministic variant gated behind `#[cfg(any(test, feature = "test-fixtures"))]` backs a wire-format pinning fixture.

**Authorization (two gates, same as voice):**
1. **Seal gate:** only epoch-key holders (current members) can open a beacon — non-members are cryptographically excluded.
2. **Signature + membership gate:** `verify_presence_beacon_sig` proves the holder of `device`'s key signed it; additionally require `device ∈ enrolled_device_keys(owner)` AND `owner ∈ current community members`, via the CRDT (`owner_device_cache` + materialized `space.members`) — reusing the voice-presence membership-gate helper pattern. Defeats intra-member spoofing (a member forging another member's presence).

## 3. Roster map (`CommunityPresenceMap`)

Simplified from `VoicePresenceMap` (no `muted`, no gravestones):

- Inner: `BTreeMap<SpaceId, BTreeMap<[u8;32], PresenceEntry>>` (community → device → entry).
- `PresenceEntry { owner:[u8;16], started_hlc:Hlc, seq:u64, last_seen_ms:u64 }`.
- `apply(community, &beacon, now_ms) -> bool` (roster changed?): accept iff `beacon.started_hlc` is strictly newer than stored, OR (same `started_hlc` AND `beacon.seq > stored.seq`) — the proven freshness rule (defends replay/reorder; allows seq reset on restart). On accept, set `last_seen_ms = now_ms`. **Roster-visible change** = a new device appeared (owner newly online) — a bare liveness refresh of an already-online device returns `false` (avoids per-heartbeat event spam, incl. our own beacon echoed on the shared session).
- `sweep(now_ms, ttl_ms) -> Vec<(SpaceId,[u8;16],[u8;32])>`: evict stale, return affected so the caller re-emits; reclaim emptied community sub-maps (Greptile P2 lesson).
- `roster(community) -> Vec<RosterEntry { owner, device }>` and an owner-aggregated view for the event.
- `remove_community(community)`: drop a community's roster on unsubscribe so the sweep stops emitting for it.

## 4. Transport wiring (active-community-only)

Mirrors `spawn_voice_presence_publisher` / `spawn_voice_presence_subscriber` and the state-root adapter, but spawned/torn-down per the **subscribed** community rather than per call.

- **Topic:** `harmony/presence/{community_id_hex}/beacons`.
- **Publisher task** (per subscribed community): every `BEACON_INTERVAL` (~10s) build → sign → seal under epoch key → `session.put(topic, bytes)`. `seq++` per tick; `started_hlc` fixed for the task's lifetime.
- **Subscriber task** (per subscribed community): `session.declare_subscriber(topic)` → `open_presence_beacon` → `verify_presence_beacon_sig` → membership gate → `map.apply` → on change emit `presence-updated`.
- **Sweeper:** periodic (~`BEACON_INTERVAL`) `map.sweep(now, STALE_MS)`; re-emit affected communities.
- **Constants:** `BEACON_INTERVAL_MS = 10_000`, `STALE_MS = 30_000` (≈3 missed beacons → offline). Slower than voice (4s/12s) per power/scale.
- **Lifecycle:** driven by the IPC subscription (§5). Subscribing to a community spawns its publisher+subscriber and registers it for sweeping; unsubscribing tears both down and `remove_community`s the roster. At most one active community subscription is expected, but the design holds N (keyed by community) without change.

## 5. IPC + RPC surface

Mirrors the member-card subscription lifecycle:

- `subscribe_community_presence(community_id: String)` — start beaconing + subscribing presence for this community; emits an initial (empty) `presence-updated` so the UI has a baseline. Idempotent per community.
- `unsubscribe_community_presence(community_id: String)` — stop + drop roster.
- `get_community_presence(community_id: String) -> PresenceSnapshot` — pull the current roster (for initial paint / reconnection without waiting for the next event).

All three: `#[tauri::command]` (snake_case params), registered in `generate_handler!`, AND registered in the headless `api/rpc.rs` registry (camelCase args) + added to the curated v1 surface allowlist test.

## 6. Event surfacing

- Event name `presence-updated`, payload (camelCase DTO):
  ```
  PresenceUpdatedPayload { communityId: String, members: [PresenceMemberDto { ownerIdHex: String, online: bool, lastSeenMs: u64, deviceCount: u32 }] }
  ```
- Emitted via `NodeEventSink::emit` / `emit_ser` (the established pattern), fire-and-forget, mirrored to the API WS for headless.
- `members` carries the **full current roster** for the community on each change (frontend replaces its per-community presence state) — no frontend polling, mirroring `voice-presence-changed`.

## 7. Frontend service

New `src/lib/presence-service.ts` (mirrors `member-card-service.ts` / `channel-message-service.ts`):

- `subscribe(communityId, onUpdate)` → invokes `subscribe_community_presence`, registers a `listen('presence-updated')` filtered to that community, seeds via `get_community_presence`.
- `unsubscribe(communityId)` → invokes `unsubscribe_community_presence`, drops the listener.
- Exposes `isOnline(ownerIdHex)` / a presence map for the active community; `onUpdate` fires the consumer (member-list view renders an online dot).
- IPC rejection normalization (`e instanceof Error ? e.message : String(e)`), camelCase invoke args.

GUI rendering of the dot against the member list is in-scope minimally (wire the service to the existing member-list view so presence is visible); richer affordances are follow-ups.

## 8. Testing

- **Unit (Rust):** `CommunityPresenceMap` apply/sweep/freshness (new device → online; refresh no-emit; stale → offline+emit; restart seq-reset accepted; reordered old beacon rejected); beacon sign/verify (good + tampered); seal/open round-trip + wrong-key drop; membership gate (non-member device rejected).
- **Wire-format fixture:** pin canonical CBOR of `PresenceBeacon`/`SignedPresenceBeacon` + a sealed packet (deterministic nonce) under `test-fixtures`.
- **Integration:** two in-process engines, one subscribes+beacons a shared community → the other (subscribed) sees it online; on stop, TTL → offline. Reuse the e2e/two-engine harness pattern; assert on camelCase DTO keys (`communityId`/`ownerIdHex`).
- **Frontend (vitest):** presence-service subscribe/seed/update/unsubscribe + rejection normalization.

## 9. Out of scope (v1)

- Explicit fast-offline on community-leave / app-close (TTL covers it).
- Presence for non-active communities / the community switcher (active-community-only by design).
- Per-device UI breakdown beyond a `deviceCount`.
- Rich status (away/busy) — only online/offline.

## 10. Files

- Create: `src-tauri/src/community_presence.rs` (types, crypto, map), `src/lib/presence-service.ts`, `src/lib/__tests__/presence-service.test.ts`, a wire-format fixture test.
- Modify: `src-tauri/src/event_loop.rs` (spawn publisher/subscriber/sweeper per subscription; topic; membership gate wiring; emit), `src-tauri/src/lib.rs` (3 IPC commands + DTOs + `generate_handler!`), `src-tauri/src/api/rpc.rs` (3 RPC registrations + curated-surface test), the member-list view to render the dot.
