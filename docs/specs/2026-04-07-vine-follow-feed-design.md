# Vine Follow/Feed System Design

**Bead:** harmony-client-2hd
**Date:** 2026-04-07
**Status:** Design

## Summary

Add a follow/unfollow system for vine creators with a two-feed UI: a curated "Following" feed showing vines from people you follow, and a "Discover" feed showing the global network stream for organic creator discovery.

## Architecture

Three layers matching harmony-client's existing patterns:

- **Backend (Rust):** FollowManager owns the follow list with JSON persistence. The event loop manages per-creator Zenoh subscriptions. Vines are tagged with their source (`followed` or `discover`) before emission to the frontend.
- **Frontend Service (TypeScript):** VineService maintains two vine arrays (`followedVines`, `discoverVines`), routes incoming vines by source tag, and exposes follow/unfollow methods that call Tauri commands.
- **Frontend UI (Svelte):** VineFeed gets top-level Following/Discover tabs. VineCard gets creator name display and contextual follow/unfollow buttons.

## Data Model

### Follow List Persistence

Written to `follows.json` in Tauri's `app_data_dir`:

```json
{
  "version": 1,
  "follows": [
    {
      "address": "aa3f7b21...",
      "name": "Alice",
      "followed_at": 1712450000
    }
  ]
}
```

The `name` field is best-effort, populated from the `creator_name` wire field on the first vine received from that creator. Updated if a newer vine carries a different name. Display-only, not authoritative identity.

### Vine Source Tagging

The `vine-received` IPC event payload gains a `source` field:

```json
{
  "...existing VineDescriptorPayload fields...",
  "source": "followed" | "discover"
}
```

The backend determines source by which Zenoh subscriber delivered the message: per-creator subs emit `"followed"`, the wildcard sub emits `"discover"`.

### Deduplication

A vine from a followed creator arrives on both the per-creator subscription and the wildcard. The backend suppresses the wildcard duplicate: if a vine's creator is in the follow list and arrived on the wildcard, it is not emitted. This keeps dedup in one place.

## Backend (Rust)

### FollowManager

New struct held in Tauri app state alongside existing Mutex-protected state.

Methods:
- `load(app_data_dir) -> Self` — reads `follows.json` on startup; returns empty set if file missing.
- `save(&self, app_data_dir)` — writes atomically (temp file + rename).
- `follow(address, name) -> bool` — adds to set, saves, returns true if newly added.
- `unfollow(address) -> bool` — removes from set, saves, returns true if was present.
- `is_followed(address) -> bool`
- `list() -> Vec<FollowEntry>`
- `update_name(address, name)` — updates display name when a newer vine carries a different name.

### Tauri Commands

- `follow_creator(address: String, name: Option<String>)` — adds to FollowManager, sends `FollowCreator` message to event loop channel to create per-creator Zenoh subscriber.
- `unfollow_creator(address: String)` — removes from FollowManager, sends `UnfollowCreator` message to event loop channel to destroy per-creator Zenoh subscriber.
- `list_followed() -> Vec<FollowEntry>` — returns full follow list with addresses, names, and timestamps.
- `is_followed(address: String) -> bool` — lightweight check.

### Event Loop Changes

- **Startup:** iterate follow list, create a per-creator Zenoh subscriber for each address (`harmony/vines/{address}/announce/**`).
- **Wildcard stays:** `harmony/vines/*` subscription remains active for the Discover feed.
- **Vine handler:** on receiving a vine, parse the `creator_address` from the payload. If the creator is in the follow list and the vine arrived on the wildcard subscriber, suppress it (don't emit — the per-creator subscriber will deliver it separately). Otherwise, tag as `"followed"` if from a per-creator sub, or `"discover"` if from the wildcard, and emit `vine-received`.
- **Channel messages:** new `FollowCreator(address)` and `UnfollowCreator(address)` variants on the existing event loop channel, so the event loop creates/destroys Zenoh subscribers in its own async context without Mutex contention.

## Frontend Service (TypeScript)

### VineService Changes

**New state:**
- `followedVines: VineVideo[]` — vines from followed creators.
- `discoverVines: VineVideo[]` — vines from the wildcard (non-followed only).
- `followedAddresses: Set<string>` — local cache of follow list, synced from backend on startup.

The existing `vines` array is replaced by the two separate arrays. The `onChange` callback fires for either.

**Modified `connectAdapter()`:**
- On `vine-received`, route by `source` field: `"followed"` appends to `followedVines`, `"discover"` appends to `discoverVines`.
- Dedup via existing shared `seenIds` set.

**New methods:**
- `async follow(address, name?)` — calls `follow_creator` Tauri command, moves existing vines for that creator from `discoverVines` to `followedVines`, adds to `followedAddresses`.
- `async unfollow(address)` — calls `unfollow_creator` Tauri command, removes creator's vines from `followedVines`, removes from `followedAddresses`.
- `async loadFollowed()` — calls `list_followed` on startup, populates `followedAddresses`.
- `isFollowed(address) -> boolean` — local check against `followedAddresses` cache.

Existing methods unchanged: `publish()`, `markViewed()`, `wireToVine()`.

### App.svelte Integration

New reactive state:
- `activeTab: 'following' | 'discover'` — top-level feed mode.
- `followedVines` and `discoverVines` derived from VineService.

New handler callbacks:
- `handleFollow(address, name)` — calls `vineService.follow()`.
- `handleUnfollow(address)` — calls `vineService.unfollow()`.

## Frontend UI (Svelte)

### VineFeed

**Top-level tab bar** replaces the current filter tabs:

```
┌─────────────┬────────────┐
│  Following   │  Discover  │
└─────────────┴────────────┘
```

- **Following tab:** shows `followedVines`. Retains New/All sub-filter. Empty state: "Follow creators to build your feed" with nudge to Discover.
- **Discover tab:** shows `discoverVines` in reverse-chronological order. No sub-filter. Every card has a follow button.

New props:
- `activeTab` / `onTabChange` — controlled from App.svelte.
- `followedVines` / `discoverVines` — separate arrays.
- `followedAddresses` — for rendering follow state on cards.
- `onFollow(address, name)` / `onUnfollow(address)` — callbacks.

### VineCard

New visual elements:
- **Creator name** displayed prominently, with truncated hex address as subtitle/fallback.
- **Follow/unfollow button** — contextual:
  - In Discover: "Follow" button if not followed; "Following" badge (clickable to unfollow) if already followed.
  - In Following: "Following" badge, changes to "Unfollow" on hover.
- **Click handling:** follow button click stops propagation (does not trigger video playback).

New props:
- `isFollowed: boolean`
- `onFollow: (address, name) => void`
- `onUnfollow: (address) => void`
- `showFollowButton: boolean`

### VinePlayer

Minimal changes:
- Creator name display in player header.
- Follow/unfollow button in player header.

## Edge Cases

- **Follow during offline:** `follow_creator` persists to `follows.json` and updates local state. Per-creator subscription created when event loop reconnects. Follow state is durable even without network.
- **Vine arriving before follow list loads:** On startup, wildcard may deliver vines before `loadFollowed()` completes. These route to `discoverVines`. Once follow list loads, a one-time reconciliation pass moves misrouted vines to `followedVines`.
- **Unfollowing doesn't delete vines:** Vines removed from `followedVines`. May reappear in `discoverVines` via wildcard going forward. Historical vines from that creator are dropped.
- **Self-follow prevention:** `follow_creator` rejects your own address.
- **Creator name updates:** If a followed creator's name changes (different `creator_name` on a newer vine), backend calls `update_name()` to keep the follow entry current.

## Testing

- **FollowManager unit tests:** load/save round-trip, follow/unfollow idempotency, self-follow rejection, name updates.
- **Event loop integration tests:** per-creator sub creation on follow, teardown on unfollow, source tagging, wildcard dedup suppression.
- **VineService tests:** vine routing by source, follow moves vines between arrays, unfollow cleanup, startup reconciliation.
- **VineCard tests:** follow button renders in Discover, unfollow on hover in Following, creator name with hex fallback, click propagation stopped.
- **VineFeed tests:** tab switching, empty state in Following, follow button presence in Discover.

## Future Work (Out of Scope)

- Second-degree follows — see what people you follow are watching.
- Follower feed — see what people following you are watching.
- Follow suggestions — recommend creators based on network topology.
- Follow counts — show follower/following counts on profiles.
- Follow export/import — portability of the follow graph.
- Blocked creators — filter out specific addresses from Discover.
