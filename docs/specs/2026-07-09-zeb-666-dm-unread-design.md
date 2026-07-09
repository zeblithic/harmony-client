# ZEB-666: DM unread badges — design

**Status:** APPROVED (Jake, 2026-07-09 — full scope: IPC + rehydrate + badges; spec sign-off same day). Plan: `docs/plans/2026-07-09-zeb-666-dm-unread-plan.md`.
**Origin:** ZEB-665 spec §8.2 deferred item. Extends the read-cursor unread model from community channels to DM / group-DM nav rows.

## 1. Decisions

1. **Full scope (Jake, 2026-07-09):** includes a new `list_owner_dm_spaces` IPC and boot rehydration of DM nav rows — the nav currently boots with NO DM rows (push-only; only runtime `nav-updated` events re-create them), which would make persisted read cursors near-worthless.
2. **Semantics parity with ZEB-665:** open-clears-all; numeric badge capped "99+"; start-clean stamp on first sight; unfocused-but-open counts until markThreadRead; focused arrivals uncount only their own message.
3. **Cursor = `receivedAt` wall-clock watermark** (strict `>`), stored in the existing `UnreadCursorStore` blob under a `dm` namespace (`get('dm', spaceId)` → key `dm:<spaceId>`). Local-arrival ordering is the right unread semantic (immune to sender clock skew) and matches `read_dm_thread`'s own pagination key.
4. **Group DMs ride the same path** (`type: 'group-chat'`, same spaceId keying). No DM-section aggregate badge — DMs have no container node to roll up onto (only optional user folders; no rollup, YAGNI).

## 2. Codebase facts this design rests on

* **DM nav rows** are `NavNode { type: 'dm' | 'group-chat' }` built by `NavService.addOrUpdateNavSpace` (nav-service.ts:276+) from either a runtime `nav-updated` emit (`kind: 'dm' | 'group-dm'`, invite-accept only — lib.rs:49278) or App's `handleDmCreate`. Duplicate `added` replays preserve UI state + counters (PR #81 Fix G) → boot rehydration is idempotent against later runtime re-emits.
* **No DM enumeration IPC exists.** `list_owner_communities` (lib.rs:19589, ZEB-393 Bug B) is the exact precedent: read-only over the in-memory owner-state CRDT; `Space` carries `kind`, `name`, `custom_name`, `members: Vec<OwnerAddr>`, `left_at`.
* **`dm-received`** (dm_outbox.rs:3260) carries `{ spaceId, messageCid, from, sentAt, receivedAt, body, mimeType }` — `receivedAt`/`sentAt` are **bare wall_ms, not full HLCs**. It fires from every delivery path (live tunnel, butler deposit, relay pull) but ONLY on first insert (`apply_inbox` is idempotent on `(space_id, message_cid)`) — no duplicate emissions, unlike `channel-message-received`. MessageService already listens and dedups by `messageCid` (message-service.ts:154-166).
* **`read_dm_thread(space_id, limit, before_hlc)`** returns newest-first (hardcoded), paginates by `received_at.wall_ms < before_hlc` (bare u64 — the ZEB-244 flaw lives on in this IPC), and its `DmThreadMessage` DTO carries `messageCid`, `from`, `receivedAt`, and an explicit `isSelfOutbound` flag.
* **Clear site:** `handleNodeClick`'s DM branch (App.svelte:3040-3062) already calls `navService.clearMention(node.id)` and `loadDmThread(node.id)`; ZEB-665 left the unread hook unwired here. Active-DM identity = `activeChannel === spaceId && activeChannelType ∈ {'dm','group-chat'}` (+ `appMode === 'messages'`); no resolver exists yet.
* **`NavService.setUnread` is hardwired to `type === 'channel'`** (nav-service.ts:545) with community delta rollup; `NavNodeRow` renders unread badges for ALL node types already.

## 3. Components

### 3.1 Rust: `list_owner_dm_spaces` (+ RPC seam)

Mirror of `list_owner_communities` / `communities_for_nav`:

```rust
#[derive(Serialize)] #[serde(rename_all = "camelCase")]
pub struct DmNavDto {
    pub space_id: String,          // 32-char hex
    pub kind: String,              // "dm" | "group-dm" (nav-updated vocabulary)
    pub name: String,              // custom_name.unwrap_or(name)
    pub members: Vec<String>,      // hex OwnerAddrs (peer derivation frontend-side)
}
```

`dm_spaces_for_nav(state)` filters `kind ∈ {Dm, GroupDm} && left_at.is_none()`. IPC + `_impl` seam + `api/rpc.rs` registration + command-list entry, same errors as the community sibling (`OWNER_NOT_LOADED_MSG`, poisoned lock).

### 3.2 App boot rehydration

Immediately after the community rehydration loop (App.svelte:2146-2152), same shape: `for (const d of await listOwnerDmSpaces()) navService.addOrUpdateNavSpace({ action: 'added', spaceId: d.spaceId, kind: d.kind, name: d.name, members: d.members })`. Non-fatal on error (warn, sidebar stays DM-empty — today's behavior).

### 3.3 `src/lib/dm-unread-service.ts` — `DmUnreadService`

Sibling of `ChannelUnreadService`, consuming DM shapes (raw `dm-received` payload + `DmThreadMessage`), NOT a generalization — the channel service stays untouched. Shares `UNREAD_TRACK_CAP`, `hlc.ts` compare (cursor stored as `{ wallMs: receivedAt, logical: 0, deviceId: '' }` so the existing store/validators work unchanged; comparisons degenerate to wallMs), and `UnreadCursorStore` under namespace `'dm'`.

Deps: `listThreadPage(spaceId, limit)` (raw `read_dm_thread` invoke, newest-first page, `before_hlc: null`); `setUnread(spaceId, count)`; `isActiveThread(spaceId)`; `isFocused()`; `selfOwnerId()`; `storage`; `now()`.

Behavior (mirrors ZEB-665, adjusted):
* `onDmSpaceMaterialized(spaceId)` — start-clean stamp if no cursor; else seed from the newest-first page, keeping entries with `receivedAt > cursor.wallMs && !isSelfOutbound`, capped (newest-first ⇒ overflow naturally keeps the newest — no ZEB-602-style ordering work needed). Tracks materialized spaces for `connectOwner` replay; un-marks seeded on failure (warn) and always pushes.
* `onDmReceived(payload)` — bump maxSeen(receivedAt); skip `from === selfOwnerId`; skip no-cursor spaces (start-clean at materialize covers them — channel-service parity; a cursor-bearing space that hasn't seeded yet this session counts normally and the seed unions by cid); focused+active → advance cursor + `set.delete(messageCid)` (push on change); else add `messageCid` if `receivedAt > cursor.wallMs`, capped.
* `markThreadRead(spaceId)` — stamp `max(cursor, maxSeen, now)`; clear set; push 0.
* `connectOwner(ownerId)` — store connect, wipe session state, replay materialized spaces (same as channels).
* `onDmSpaceRemoved(spaceId)` — drop session state (cursors kept; nav-updated `removed` action is the trigger).

### 3.4 Wiring

* **Event:** MessageService's existing `dm-received` listener gains an optional post-dedup hook (`onDmReceived?: (payload) => void`), wired to the service — reuses the `seenIds` cid dedup instead of a second listener.
* **Materialize/remove hook:** `NavService` gains an optional `onDmSpaceChange?: (action: 'added' | 'removed', spaceId: string) => void` fired from `addOrUpdateNavSpace`'s dm/group-dm path. **Init order (the ZEB-665 Qodo lesson): construct `DmUnreadService` and assign both hooks BEFORE `messageService.connectAdapter` (App.svelte ~2099 — it runs before nav's connect at ~2136, and the `dm-received` listener goes live there)** so no runtime event can slip past a null hook; the DM rehydration loop then runs after construction by definition.
* **Shared cursor store (planning discovery):** `DmUnreadService` and `ChannelUnreadService` must share ONE `LocalStorageUnreadCursorStore` instance — both persist into the same owner-scoped localStorage blob, and each instance serializes only its own in-memory map on `set()`, so two instances would silently clobber each other's keys on every write.
* **Clear:** `dmUnread?.markThreadRead(node.id)` in `handleNodeClick`'s DM branch beside `loadDmThread`.
* **NavService.setUnread:** widen the node lookup to `type ∈ {'channel','dm','group-chat'}`; community delta rollup runs ONLY for channel nodes (DM rows have no aggregation target).
* **Owner connect:** same `$effect` pattern as channelUnread (`connectOwner` on selfOwnerId).

## 4. Caveats (accepted, v1)

1. **Equal-millisecond tie swallow:** cursor is bare wall_ms with strict `>`; a message whose `receivedAt` equals the stamped cursor at a restart boundary is not counted. Rare (local-arrival ms resolution) and self-heals on open. Root fix — full HLC on the DM path (event payload + `read_dm_thread` cursor, which has the identical ZEB-244 flaw today) — is follow-up scope (§6.1).
2. **Live-path counting trusts `apply_inbox` idempotency** (no duplicate `dm-received`); the capped cid-set still bounds memory and dedupes defensively.
3. Rehydrated DM rows show the persisted Space name until peer profiles resolve — existing behavior, unchanged.

## 5. Testing

* **Rust:** `dm_spaces_for_nav` unit tests (filters left/kind, custom_name preference — mirror `zeb393_communities_for_nav_tests`); IPC owner-not-loaded error test; RPC command-list presence.
* **Vitest — DmUnreadService:** start-clean; seed newest-first with cap; self-skip (seed `isSelfOutbound` + live `from === self`); unseeded-ignores; focused-active advances cursor + uncounts own cid only (unfocused backlog preserved); markThreadRead stamps past maxSeen; connectOwner replay; removal drops session state; owner/store isolation.
* **Vitest — NavService:** setUnread drives dm/group-chat nodes (badge + level), does NOT touch community rollup for DM nodes, channel behavior unchanged.
* Full gates: tsc + vitest; cargo fmt/clippy/nextest per CLAUDE.md.

## 6. Follow-ups (not this PR)

1. Full HLC on the DM path: `dm-received` + `DmThreadMessage` timestamps and the `read_dm_thread` cursor (fixes §4.1 and the pre-existing ZEB-244-class pagination flaw).
2. Cross-device cursor sync via OwnerState CRDT (shared with ZEB-665 §8.1 — the store namespace keeps DM keys ready).
3. `Space.notification_pref`-aware muting of unread badges.
4. DM-section aggregate badge if the nav ever grows a DM container node.
