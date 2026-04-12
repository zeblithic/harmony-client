# Vine Reshare Improvements Design

## Goal

Improve the existing reshare feature with attribution display, reshare counts, a confirmation dialog, and navigation to the original vine. Currently reshare is fire-and-forget with minimal feedback — this adds the UX layer that makes resharing feel intentional and informative.

## Architecture

The reshare system builds on the existing vine descriptor infrastructure. No new Zenoh keyspaces or subscriptions are needed. Two optional fields are added to the wire format for original creator attribution. Reshare counts are derived from the local feed (counting vines where `reshareOf` matches a given vine ID) rather than tracked via a separate pub/sub mechanism. A new confirmation dialog component gates the reshare action.

## Wire Format Changes

### Rust Types

Add two optional fields to `VineDescriptorPayload` and `PublishVinePayload`:

```rust
// VineDescriptorPayload (wire format received from Zenoh)
pub original_creator_address: Option<String>,
pub original_creator_name: Option<String>,

// PublishVinePayload (input from frontend)
pub original_creator_address: Option<String>,
pub original_creator_name: Option<String>,
```

Both fields use `#[serde(skip_serializing_if = "Option::is_none")]` and `#[serde(default)]` respectively, matching the existing pattern for `reshare_of` and `title`. Non-reshare vines omit these fields entirely.

### TypeScript Types

Add matching fields to `VineVideo` and `VineDescriptorEvent`:

```typescript
originalCreatorAddress?: string;
originalCreatorName?: string;
```

## Frontend Service (VineService)

### Publishing Reshares

`vineService.publish()` already accepts `reshareOf`. When called with a reshare, the caller (App.svelte's `handleVineReshare`) now also passes `originalCreatorAddress` and `originalCreatorName` from the source vine. If the source vine is itself a reshare, the original creator info carries through (always traces to the true origin, not the intermediate resharer).

### Self-Reshare Prevention

`publish()` silently returns (no-op) when asked to reshare your own original content — if the vine's `creatorAddress === 'self'` and `reshareOf` is unset. The UI should also hide the Reshare button on your own original vines so this guard is rarely hit. Resharing someone else's reshare of your content is allowed.

### Reshare Count

`VineService` exposes a new method:

```typescript
getReshareCount(vineId: string): number
```

This counts vines in both `followedVines` and `discoverVines` where `reshareOf === vineId`. Computed on demand — no new state map. Only meaningful for original vines (where `reshareOf` is unset).

### Navigate to Original

`VineService` exposes a method to find the original vine:

```typescript
findVine(vineId: string): VineVideo | undefined
```

Searches both `followedVines` and `discoverVines` by ID. Used by UI components to open the original vine in the player when the attribution link is clicked.

## UI Components

### ReshareConfirmDialog (new component)

A modal dialog triggered when the user clicks the Reshare button in VinePlayer.

**Props:**
- `vine: VineVideo` — the vine being reshared
- `onConfirm: () => void` — fires when the user confirms
- `onCancel: () => void` — fires when the user cancels

**Behavior:**
- Shows vine title and original creator name (or resharer name if not a reshare)
- "Reshare this vine?" heading
- Cancel and Reshare buttons
- Dismissible with Escape key or clicking the backdrop
- Focus-trapped within the dialog

### VineCard Changes

**Attribution row:** When `vine.reshareOf` is set, replace the existing "reshare" badge with an attribution row:
```
↗ originally by {originalCreatorName}
```
The original creator name is clickable — clicking it fires a new `onViewOriginal` callback that opens the original vine in the player.

**Reshare count:** For original vines (no `reshareOf`), show a reshare count alongside the like count in the social stats row:
```
❤️ 5  ↗ 2
```
Only displayed when the count is greater than 0. Uses the same compact style as the like count.

**New props:**
- `reshareCount?: number` — number of reshares (only for originals)
- `onViewOriginal?: (vineId: string) => void` — callback to navigate to original

### VinePlayer Changes

**Attribution row:** Below the vine title, when the vine is a reshare, show:
```
↗ originally by {originalCreatorName}
```
Clickable to navigate to the original vine.

**Reshare button flow:** Clicking the Reshare button now opens `ReshareConfirmDialog` instead of immediately publishing. The existing loading/error state remains but only triggers after confirmation. The Reshare button is hidden on your own original vines (`creatorAddress === 'self'` and no `reshareOf`).

**New props:**
- `onViewOriginal?: (vineId: string) => void` — callback to navigate to original

### VineFeed Changes

VineFeed passes reshare count and `onViewOriginal` through to VineCard and VinePlayer. It receives `getReshareCount` and `findVine` callbacks from App.svelte.

**onViewOriginal handler:** When triggered, looks up the vine by ID via `findVine`. If found, opens it in the player. If not found, the click is silently ignored (the original creator may not be in your network).

### App.svelte Changes

- `handleVineReshare` updated to pass `originalCreatorAddress` and `originalCreatorName` when calling `vineService.publish()`
- New `vineGetReshareCount` reactive state function (same pattern as `vineGetReaction`)
- New `handleViewOriginal` callback that finds the vine and opens it in the player
- Both passed through VineFeed to child components

## Edge Cases

- **Resharing a reshare:** Attribution always traces to the original creator. If Alice reshares Bob's vine, and Carol reshares Alice's reshare, Carol's vine carries Bob's `originalCreatorAddress`/`originalCreatorName`.
- **Self-reshare prevention:** Publishing rejects resharing your own original content. Resharing someone else's reshare of your content is allowed.
- **Reshare count only on originals:** Only vines where `reshareOf` is unset display a reshare count. Reshares themselves don't show counts.
- **Original not in feed:** When clicking attribution to navigate to the original, if the vine isn't in the local feed, the click does nothing. No error toast needed — the absence is self-explanatory.
- **Offline reshare:** Confirmation dialog still appears. After confirmation, optimistic local append as before; won't reach the network.
- **Backward compatibility:** Old vine descriptors without `originalCreatorAddress`/`originalCreatorName` fields deserialize correctly via `serde(default)` — they'll be `None`/`undefined`. Existing reshares show attribution only if the fields are present.

## Testing

- **VineService tests:** getReshareCount, findVine, self-reshare prevention, publish with original creator fields, reshare-of-reshare attribution chain
- **ReshareConfirmDialog tests:** render with vine info, confirm callback, cancel callback, Escape dismissal, backdrop click dismissal
- **VineCard tests:** attribution row display, reshare count display, onViewOriginal callback, no reshare count on reshares
- **VinePlayer tests:** attribution row, reshare confirmation flow, onViewOriginal callback
- **VineFeed tests:** reshare count and onViewOriginal passed through to cards and player
- **Rust tests:** new fields serialize/deserialize correctly, backward compatibility with missing fields
