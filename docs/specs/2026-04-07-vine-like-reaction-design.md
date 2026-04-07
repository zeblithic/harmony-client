# Vine Like/Reaction System Design

## Goal

Add a single like/heart toggle to vines, allowing users to react to content in their feed. Reactions are distributed over Zenoh pub/sub using the existing `harmony/vines/{creator}/reactions/{bundle_cid}/{reactor}` keyspace, with optimistic UI updates and in-memory state.

## Architecture

The system follows the same hybrid pattern as the follow/feed system: Rust owns the Zenoh publish path (building keys, serializing payloads, routing through the event loop's session), while TypeScript owns presentation state (reaction counts, liked-by-me tracking, optimistic updates). Reaction state is in-memory only — counts rebuild from the network on reconnect.

## Zenoh Protocol

### Key Structure

Reactions use the keyspace defined in `harmony-zenoh`:

- **Publish key**: `harmony/vines/{target_creator}/reactions/{vine_id}/{reactor_address}`
- **Subscription**: `harmony/vines/*/reactions/**` (single wildcard, catches all reactions)

The key structure provides natural deduplication — each reactor can only occupy one key per vine. Publishing to the same key overwrites the previous value.

### Wire Payload

```rust
// Rust (src-tauri/src/lib.rs)
struct VineReactionPayload {
    vine_id: String,
    reactor_address: String,
    reactor_name: String,
    liked: bool,
    timestamp: u64,
}
```

```typescript
// TypeScript (src/lib/vine-service.ts)
interface VineReactionEvent {
  vineId: string;
  reactorAddress: string;
  reactorName: string;
  liked: boolean;
  timestamp: number;
}
```

The `liked` field supports unlikes — publishing `liked: false` to the same key signals removal.

## Backend (Rust)

### New Tauri Command: `publish_vine_reaction`

Mirrors the `publish_vine` pattern:

1. Accepts `{ vineId, vineCreatorAddress, liked }` from the frontend
2. Builds the Zenoh key: `harmony/vines/{vine_creator_address}/reactions/{vine_id}/{own_node_addr}`
3. Serializes the `VineReactionPayload` with the reactor's own address, display name, and current timestamp
4. Sends through the existing `publish_tx` channel

### Event Loop Changes

**New subscription**: At startup, add `harmony/vines/*/reactions/**` alongside the existing `harmony/vines/*` vine descriptor subscription.

**Event routing in `emit_frontend_event`**: Add a new branch that matches keys containing `/reactions/`. When matched, deserialize as `VineReactionPayload` and emit a `vine-reaction-received` Tauri IPC event to the frontend.

### Payload Types

```rust
// Input from frontend
struct PublishReactionPayload {
    vine_id: String,
    vine_creator_address: String,
    liked: bool,
}

// Wire format (published to / received from Zenoh)
struct VineReactionPayload {
    vine_id: String,
    reactor_address: String,
    reactor_name: String,
    liked: bool,
    timestamp: u64,
}
```

## Frontend Service (TypeScript)

### VineService Additions

**New state**:
```typescript
// Per-vine reaction tracking
reactionMap: Map<string, { count: number; likedByMe: boolean; reactors: Set<string> }>
```

**New methods**:

- `toggleLike(vine: VineVideo)`: Optimistically updates `reactionMap` (flip `likedByMe`, adjust `count`), fires `onChange`, then calls `publish_vine_reaction` Tauri command. On failure, rolls back the optimistic state.

- `connectAdapter` addition: Register a listener for `vine-reaction-received` events. On receipt:
  1. If reactor is self (`reactorAddress === ownAddress` or `'self'`), skip (already applied optimistically)
  2. If vine ID not in local feed, ignore
  3. If reactor already in the vine's `reactors` set and the `liked` value hasn't changed, skip (dedup)
  4. Otherwise, update `count` and `reactors` set, fire `onChange`

**Helpers**:
- `getReaction(vineId: string): { count: number; likedByMe: boolean }` — returns reaction state for a vine, defaulting to `{ count: 0, likedByMe: false }` if not in the map.

### Offline Behavior

When no adapter is connected, `toggleLike` still applies the optimistic update locally. The heart fills/unfills and count adjusts. The reaction won't reach the network, matching the same pattern as offline vine publishing.

## UI Components

### VineCard Changes

Add a like row below the vine title:

```svelte
{#if reaction.count > 0 || reaction.likedByMe}
  <div class="card-like-row">
    <button class="card-heart" onclick={handleLikeClick} aria-label={...}>
      {reaction.likedByMe ? '❤️' : '🤍'}
    </button>
    <span class="card-like-count">{reaction.count}</span>
  </div>
{/if}
```

- Heart + count hidden when 0 likes (clean default state)
- `handleLikeClick` calls `stopPropagation()` to prevent opening the player (same pattern as follow button)
- New props: `reactionCount: number`, `likedByMe: boolean`, `onToggleLike: (vine: VineVideo) => void`

### VinePlayer Changes

Add a like button in the existing `footer-actions` row, alongside Reshare:

```svelte
<button class="action-btn like-btn" class:liked={likedByMe} onclick={handleLike}>
  <span class="heart">{likedByMe ? '❤️' : '🤍'}</span>
  <span class="like-count">{reactionCount}</span>
</button>
```

- New props: `reactionCount: number`, `likedByMe: boolean`, `onToggleLike: (vine: VineVideo) => void`

### VineFeed Changes

VineFeed passes reaction data down to VineCard and VinePlayer. It receives a `reactionMap` or a `getReaction` callback from App.svelte.

### App.svelte Changes

Wire `vineService.getReaction()` and `vineService.toggleLike()` through VineFeed to VineCard and VinePlayer. Add `reactionMap` to the reactive state snapshot in `vineService.onChange`.

## Edge Cases

- **Self-echo suppression**: Own reactions echoing back from Zenoh are skipped since the optimistic update already applied them.
- **Unlike**: Publishing `liked: false` to the same Zenoh key. Optimistic update decrements count immediately. Network failure rolls back.
- **Duplicate reactions**: The `reactors` Set per vine prevents double-counting from network replays.
- **Unknown vine**: Reactions for vine IDs not present in either `followedVines` or `discoverVines` are silently ignored.
- **Offline**: Optimistic update applies locally; reaction doesn't reach the network.

## Testing

- **VineService tests**: toggleLike optimistic update, self-echo dedup, incoming reaction count, unlike, offline fallback, rollback on publish failure
- **VineCard tests**: heart display states (liked/unliked/hidden), click handler with stopPropagation, aria labels
- **VinePlayer tests**: like button in footer, toggle behavior
- **VineFeed tests**: reaction data passed through to cards and player
- **Rust tests**: publish_vine_reaction command validation, VineReactionPayload serialization, event routing for reaction keys
