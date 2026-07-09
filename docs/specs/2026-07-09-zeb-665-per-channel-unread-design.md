# ZEB-665: Per-channel unread counts — design

**Status:** approved (Jake, 2026-07-09, brainstorm session)
**Ticket:** [ZEB-665](https://linear.app/zeblith/issue/ZEB-665) — the deferred ZEB-662/663 sequel
**Scope:** frontend-only, community channels only. DM/group-DM unread, viewport-bottom
clearing, and cross-device sync are explicit follow-ups (see §8).

## 1. Goal

Drive the currently-dead `NavNode.unreadCount` / `unreadLevel` scaffold for community
channels: a numeric badge per channel counting messages the owner hasn't seen, a quiet
dot on the owning community, precise clear on open. ZEB-663 made channels first-class
NavNodes; the missing piece is the read-position model, not UI.

## 2. Decisions (settled with Jake, 2026-07-09)

1. **Local-first v1.** Read cursors are per-device, owner-scoped. Cross-device sync via
   the OwnerState CRDT is a follow-up that swaps the storage layer only (§8).
2. **Open clears all.** Opening/viewing a channel marks everything in it read. Mirrors
   mention-clear behavior; no scroll tracking in v1.
3. **Numeric badge capped at "99+".** Bounds seed-query cost and in-memory tracking at
   100 message IDs per channel.
4. **Start clean on first sight.** A channel with no stored cursor (feature's first run,
   fresh device, newly joined community) stamps its cursor at "now" and starts at 0 —
   no wall-of-99+ on day one.

## 3. Codebase facts this design is built on (verified 2026-07-09)

- **Live and backfilled messages arrive on the same event** (`channel-message-received`)
  **with no distinguishing flag** (`community_channel_log_engine.rs` — backfill replies
  are wire-identical to live broadcasts; scroll-back paging and RBSR anti-entropy both
  re-emit historical messages). Counting events therefore overcounts; unread must be
  derived from a cursor comparison plus ID-level dedupe.
- **`list_channel_messages(communityId, channelId, since?: HlcDto, limit)`** (max 1000)
  already has exclusive strictly-newer-than-`since` semantics, counts only `Post` events,
  returns **oldest-first**. This is the seed primitive. Oldest-first matters: an
  overflowed seed sees the oldest 100 unread, not the newest (see §5 clear rule).
- **No count/head IPC exists** — computing a count requires fetching DTOs. Acceptable at
  v1 scale with the 100 cap; a `count_channel_messages_since` IPC is a follow-up
  optimization, not v1.
- **Mention pipeline hooks** (`mention-alert.ts`, App wiring): `onMessage` callback from
  `ChannelMessageService.ingest()` fires for live events AND `listMessages()` results;
  `isActiveChannel` resolver = messages mode + selected community + channels view +
  selected channel; `clearMention(channelId)` fires at three App call sites
  (`openCommunityChannel`, the channel-selection-resolution `$effect`, generic nav open).
- **`NavNode.unreadCount`/`unreadLevel` are never written nonzero in production**;
  `NavNodeRow` already renders `standard` → numeric badge and `quiet` → dot.
- **Owner-scoped localStorage pattern** (the ZEB-586/589 fix): `ownerKey(ownerId)` as in
  `theme-service.ts` / `profile-service.ts`; pre-identity reads return defaults, writes
  no-op.
- **`compareHlc` is duplicated** (private copies in `channel-message-service.ts` and
  `fork-timeline.ts`); this feature needs it a third time → extract shared module.
- **OwnerState CRDT already carries `markers: BTreeMap<SpaceId, ReadMarker>`** with
  monotonic-HLC merge (`apply_marker`), but it is per-Space (not per-channel) and has no
  frontend write IPC — the designed cross-device seam, deliberately not v1.

## 4. Data model & invariants

- **Cursor (persisted):** `"communityId:channelId" → Hlc` of the newest-seen message.
  One JSON blob per owner under `harmony-unread:owner-<ownerId>`, behind an
  `UnreadCursorStore` interface (get/set/connectOwner) so the CRDT swap is storage-only.
- **Unread set (session):** `channelId → Set<messageId>`, capped at 100. Rebuilt each
  session by seeding. The Set gives free idempotence against backfill re-emission.
- **`maxSeen` (session):** `channelId → Hlc`, the newest HLC among every message the
  service has seen for that channel (seed results and events), independent of counting.
- **Unread predicate:** a message is unread iff `hlc > cursor` AND `author ≠ self` AND it
  did not arrive while its channel was the focused, active channel.
- **Nav invariant (mirrors mentions):** channel `unreadCount` = set size (level
  `standard` when > 0); community `unreadCount` = Σ children (level `quiet` when > 0 —
  dot, not number). Display formats set-size ≥ 100 as "99+".

## 5. Components & data flow

### New: `src/lib/hlc.ts`
`compareHlc(a, b)` (wallMs → logical → deviceId lexical) and `hlcNewer(a, b)`.
`channel-message-service.ts` and `fork-timeline.ts` refactored to import it (behavior
unchanged — same algorithm both already use).

### New: `src/lib/channel-unread-service.ts`
`ChannelUnreadService` with injected deps (the `mention-alert.ts` testability pattern):

```ts
interface ChannelUnreadDeps {
  listMessagesSince(communityId: string, channelId: string,
                    since: Hlc | undefined, limit: number): Promise<ChannelMessageDto[]>;
  setUnread(channelId: string, count: number): void;          // → NavService
  isActiveChannel(communityId: string, channelId: string): boolean; // mention resolver
  isFocused(): boolean;
  selfOwnerId(): string | null;
  storage: UnreadCursorStore;
  now(): number;                                              // injectable wall clock
}
```

API and behavior:

- **`onChannelsMaterialized(communityId, channels)`** — for each channel not yet seeded
  this session: stored cursor → `listMessagesSince(cursor, 100)`, filter self-authored,
  seed the set, update `maxSeen`, push count. No cursor → stamp
  `{wallMs: now(), logical: 0, deviceId: ''}`, push 0. Always re-push known counts (nav
  nodes may have just been rebuilt). Seed failures warn (standard
  `e instanceof Error ? e.message : String(e)` extraction) and leave the channel at 0;
  retried on the next materialize.
- **`onMessage(communityId, channelId, message)`** — self-authored → update `maxSeen`
  only. Focused + active channel → advance cursor to max(cursor, msg.at) (persisted),
  no count. Otherwise `hlcNewer(msg.at, cursor)` → `set.add(messageId)` (≤100), push
  count on change. Cursor reads are synchronous (localStorage), so events racing the
  async seed still gate correctly; seed/event double-delivery unions in the Set. A
  channel with no stored cursor and no stamp yet ignores the event (start-clean covers
  it at materialize).
- **`markChannelRead(communityId, channelId)`** — cursor ← max(cursor, `maxSeen`,
  wall-clock stamp); wipe set; push 0. The wall-clock component is load-bearing: an
  overflowed seed saw only the *oldest* 100 unread, so `maxSeen` under-stamps and would
  leave a residual badge after opening, violating open-clears-all.
- **`onCommunityRemoved(communityId)`** — drop session state (sets, maxSeen, seeded
  marks). Stored cursors are kept: they're small, and rejoin-preserves-read-state is the
  better behavior.

### Modified: `src/lib/nav-service.ts`
`setUnread(channelId, count)`: set node `unreadCount` + `unreadLevel`
(`standard`/`none`), recompute owning community's rollup (Σ children, `quiet`/`none`).
Missing node → no-op (no pending queue: unlike mentions, counts are recomputable and
re-pushed on materialize). `setChannels` already preserves `unreadCount` across
rename/reorder; community rollup must be recomputed there for both counters.

### Modified: `src/lib/components/NavNodeRow.svelte`
Badge text: `count > 99 ? '99+' : count` (mention badge unchanged).

### Modified: `src/App.svelte`
Construct service; `storage.connectOwner(ownerId)` when identity lands; chain
`onChannelsMaterialized` in the existing `ChannelNavSyncDeps.setChannels` wrapper; call
`onMessage` beside `mentionAlerter.onMessage`; call `markChannelRead` at the three
existing channel `clearMention` call sites; `onCommunityRemoved` beside existing
community-removal handling.

## 6. Known caveats (accepted for v1, documented here deliberately)

- **Wall-clock stamps inherit clock skew.** Start-clean and overflow-clear both stamp
  from `now()`. A peer with a fast clock can straddle the stamp (message counted or
  missed near first-sight/clear). Bounded, rare, self-heals on next open. The CRDT
  follow-up replaces stamps with true channel-head HLCs.
- **Cursor writes are unbatched.** Active-channel traffic writes localStorage per
  message. Trivial at current scale; debounce if it ever shows up in profiles.
- **Unfocused-but-open channel counts as unread** (matches mention semantics). Focus
  regain does not auto-clear until the user re-opens/switches channels; re-selection
  paths all funnel through `markChannelRead`.

## 7. Testing

- **`channel-unread-service.test.ts`** (fake deps + in-memory store): seed with cursor;
  seed overflow (100 → badge contract "99+"); start-clean stamp written once; live
  count for non-active channel; backfill re-emission dedupe (same ID twice → 1);
  self-authored ignored everywhere; focused+active advances cursor without counting;
  unfocused+active counts; clear wipes + stamps (incl. overflow wall-clock rule);
  event-races-seed union; per-community isolation; owner gating (no reads/writes
  pre-identity); seed failure warns and stays at 0.
- **`nav-service.test.ts`**: setUnread node + community Σ rollup and levels;
  missing-node no-op; preservation + rollup across setChannels rename/reorder/remove.
- **`NavNodeRow.test.ts`**: "99+" display cap; standard badge and quiet dot rendering
  (activating the dead render paths).
- **`hlc.test.ts`**: compareHlc ordering + tie-breaks; refactored importers covered by
  their existing suites.

Gates: `npx tsc --noEmit` + `npx vitest run` (frontend-only; no Rust changes).

## 8. Follow-ups (not v1)

1. **Cross-device read markers** — per-channel entries in OwnerState CRDT (`markers` is
   per-Space today) + `set_read_marker` IPC + monotonic merge; replaces wall-clock
   stamps with true head HLCs. Storage-interface swap by design.
2. **DM/group-DM unread** — same service pattern; DM nav rows + `read_dm_thread`
   pagination differences (ZEB-244's full-HLC cursor caveat applies).
3. **Viewport-bottom clearing** — `ChannelMessageFeed.scrollAtBottom` transition hook
   recording the newest visible message HLC.
4. **`count_channel_messages_since` IPC** — drop seed body transfer if boot cost ever
   matters at real channel counts.
