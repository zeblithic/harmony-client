# Community Presence Enrichment — Design

**Ticket:** ZEB-600 (child of ZEB-533; follow-up to ZEB-537)
**Status:** Approved (brainstorm 2026-06-30)
**Repo:** harmony-client (single-repo; no cross-repo changes)

## Goal

Make the already-shipped community presence (ZEB-537) **useful at a glance**, and give the user control over what they broadcast. Four pieces: an online count, presence-first sorting, cross-community surfacing (sidebar + DM dots), and an "appear offline" (invisible) mode.

## Background — what ZEB-537 already ships

ZEB-537 (merged 2026-06-22) built the full presence substrate. This design **extends** it; it does not rebuild any of it.

- **Backend** (`src-tauri/src/community_presence.rs`): signed + AEAD-sealed `PresenceBeacon`s on `harmony/presence/{community_hex}/beacons`, `BEACON_INTERVAL_MS = 10_000`, `STALE_MS = 30_000`, per-community roster `CommunityPresenceMap` (apply / sweep / `online_owners`), key re-derived per tick to follow epoch rotation. `spawn_community_presence_publisher` is a 10s tick loop that build→sign→seal→`session.put`s a beacon, honoring a `closing: Arc<AtomicBool>`. `spawn_community_presence_subscriber` receives → opens → verifies sig → membership-gates → applies → emits `presence-updated`.
- **Event loop** (`src-tauri/src/event_loop.rs`): `CommunityPresenceRequest::{Subscribe,Unsubscribe}`, a shared `Arc<Mutex<CommunityPresenceMap>>` across **all** communities, per-community pub/sub spawn, a **global** TTL sweep every 10s, `presence-updated` emission. The backend already supports concurrent multi-community subscription.
- **IPCs:** `subscribe_community_presence` / `unsubscribe_community_presence` / `get_community_presence`.
- **Frontend** (`src/lib/presence-service.ts`): `PresenceService` with `byCommunity: Map<communityId, Map<ownerIdHex(lc), PresenceMemberDto>>`, per-community `subscribe`/`unsubscribe`, and `isOnline(ownerIdHex)` scoped to `activeCommunityId`.
- **UI:** a green/grey dot in `MemberRow.svelte` (l.187–192; `isSelf` forces a solid dot at l.150), passed the `isOnline` resolver through `CommunityMembersPanel` / `ChannelMembersPanel` / `CommunityView` / `DmCreateDialog`.
- **App wiring** (`src/App.svelte`): subscribes the **active** community on switch and unsubscribes the previous one (l.1082–1093, 1349).

**The gap:** presence only ever reflects the *active* community, there's no count, and no presence-based sort.

## Requirements

### Piece 1 — Online count (member-panel header)
`CommunityMembersPanel` header shows "· N online" next to the title, where **N = number of online joined members, including yourself** (you are online in your own community unless invisible). Updates live with `presence-updated`.

### Piece 2 — Sort online-first
The `joined` derived list (`CommunityMembersPanel.svelte:107`) sorts **online-first**: primary key `online` (desc), secondary key = the existing relative order (stable sort preserves the backend's power/join ordering within each group). Self (always online unless invisible) floats into the online group.

### Piece 3 — Cross-community surfacing (subscribe-all)
- **Subscribe-all model:** drive `PresenceService.subscribe(...)` for **every joined community** — at boot (once the community list + adapter are ready) and on join. Unsubscribe **only** on leave. A community-switch must **no longer** unsubscribe the others. The active-community concept remains only for backward-compatible `isOnline()`.
- **New `PresenceService` accessors:**
  - `onlineCount(communityId): number` — count of `online` members in that community's map (+1 for self if visible and a member — see Edge cases).
  - `hasOthersOnline(communityId): boolean` — true iff ≥1 member **other than self** is online there.
  - `isOnlineAnywhere(ownerIdHex): boolean` — online in **any** subscribed community.
- **Sidebar:** a small presence dot on each community in the rail when `hasOthersOnline(communityId)` (someone besides you is around).
- **DM list:** an online dot per DM counterparty via `isOnlineAnywhere(counterpartyOwnerIdHex)`.
- **Caveat (documented, not fixed):** a DM contact who shares **no** joined community with you always shows offline — inherent to community-scoped presence.

### Piece 4 — Invisible mode ("appear offline")
- **Backend gate:** a single global `presence_visible: Arc<AtomicBool>` (default `true`), created at boot, cloned into **every** `spawn_community_presence_publisher`. The publisher checks it each tick and **skips the `session.put`** when `false` (build/sign work may be skipped too; the subscriber is untouched — you keep seeing others). Effect is live (no task re-spawn); peers evict you within the 30s TTL after you go invisible.
- **Persistence:** add `presence_invisible: bool` (serde `#[serde(default)]` → `false`) to `PkarrSettings` (`src-tauri/src/pkarr_settings.rs`, file `connectivity-settings.json`). **Fail-closed = `invisible` (`presence_invisible: true`)** in `fail_closed_defaults()` — a corrupt/unreadable settings file must never silently re-broadcast a user who had opted to hide. NB the fail-closed direction is the **inverse** of `identity_discoverable` (whose restrictive value is `false`): here the restrictive value is `true`.
- **Boot order:** load settings → set the atomic to `!presence_invisible` **before** any presence publisher spawns, so an invisible user emits no stray launch beacon.
- **IPCs:** `set_presence_visibility(visible: bool)` (flip the atomic + persist `presence_invisible = !visible`), `get_presence_visibility() -> bool` (seed the UI).
- **UI:** an "Appear offline" toggle in the self/identity menu; when invisible, your **own** dot renders in a distinct hollow/outline style so you remember you're hidden (overrides `MemberRow`'s `isSelf` solid-dot special-case). The exact mount component for the toggle is confirmed at plan time (there is no existing settings view; candidates are the self-avatar/identity menu).

## Architecture & data flow

```
                 connectivity-settings.json (presence_invisible)
                              │ load at boot
                              ▼
   set_presence_visibility ─► presence_visible: Arc<AtomicBool> ─► [every publisher tick: gate session.put]
        ▲ (IPC + persist)                                          
        │                                                          
   self/identity menu toggle                                       
                                                                   
   boot / join ─► subscribe_community_presence (ALL joined) ─► backend roster map ─► presence-updated
                                                                   │
   PresenceService.byCommunity ◄──────────────────────────────────┘
        │ onlineCount / hasOthersOnline / isOnlineAnywhere / isOnline
        ├─► CommunityMembersPanel: "N online" + sort-online-first
        ├─► communities sidebar: dot when hasOthersOnline
        └─► DM list: dot when isOnlineAnywhere
```

**Self-presence note:** zenoh does not loop a node's own beacon back, so the local roster never contains self. All "include self" logic (count, sort, `isOnline` for self) is a frontend special-case keyed on the self owner id — as `MemberRow` already does (`isSelf`). When invisible, self is treated as offline for the count and rendered hollow.

## Error handling & edge cases

- **Corrupt/unreadable `connectivity-settings.json`:** `load_or_default` already fails closed + loud; `fail_closed_defaults()` must set `presence_invisible: true`. Add a unit test pinning this direction.
- **Legacy settings file (pre-ZEB-600, no `presence_invisible` key):** serde field default → `false` (visible) — an existing user keeps broadcasting as before. Unit-tested.
- **subscribe-all failure for one community:** a failed `subscribe` for community X must not abort the others (the boot loop subscribes each independently; a rejection is logged and skipped, matching `PresenceService.subscribe`'s existing rollback-on-failure contract).
- **Online count / self when invisible:** invisible ⇒ self excluded from your own "N online" and rendered hollow. Others still count normally.
- **Toggle while offline / no adapter:** `set_presence_visibility` persists regardless; the atomic is applied when publishers exist. `PresenceService` no-ops gracefully with no adapter (existing pattern).

## Testing strategy

**Rust (`cargo nextest`, `--features test-fixtures`):**
- `pkarr_settings`: round-trip with `presence_invisible`; **corrupt file ⇒ `presence_invisible == true`** (fail-closed); absent file ⇒ `false` (visible); legacy file missing the key ⇒ `false`.
- Publisher gate: with `presence_visible == false`, no beacon is published ⇒ a peer's roster stays empty; flipping to `false` mid-run ⇒ peer evicts within TTL. Use / extend `tests/community_presence_two_engine_integration.rs`.

**Frontend (`vitest` + `tsc --noEmit`):**
- `PresenceService`: subscribe-all lifecycle (all joined subscribed; add on join; remove on leave; **switch does not unsubscribe others**); `onlineCount`, `hasOthersOnline` (excludes self), `isOnlineAnywhere`.
- `CommunityMembersPanel`: header count; sort-online-first (stable within groups).
- Sidebar dot: shown iff `hasOthersOnline` (not merely self-online).
- DM list dot: driven by `isOnlineAnywhere`.
- Invisible toggle: calls `set_presence_visibility`; self dot switches to hollow when invisible; seeds initial state from `get_presence_visibility`.

## Non-goals (explicitly deferred)

- Auto idle/away (a third state via activity tracking). Binary online/offline only.
- Per-channel "who's here" text-channel presence.
- Retained last-seen for offline members (swept at 30s; no "last seen 5m ago").

## Global constraints

- CI gates (all must pass): `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`.
- Tauri IPC: Rust `snake_case` params, JS `camelCase` callers.
- Tauri IPC error extraction: `e instanceof Error ? e.message : String(e)`.
- Keychain isolation in tests (ZEB-428): never construct `KeychainStore::new()`; inject via `*_inner` seams; set `HARMONY_PASSPHRASE` / `HARMONY_DISABLE_KEYCHAIN=1` as needed.
- New keychain/settings code is loaded at boot — apply the visibility atomic before publishers spawn.

## PR structure

Cohesive, single-repo — default is **one PR**. The trivial count + sort (Piece 1 + 2, active-community only) could split into a fast standalone PR first if preferred; decided at plan time.
