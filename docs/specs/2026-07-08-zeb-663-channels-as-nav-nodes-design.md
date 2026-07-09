# ZEB-663 — Community channels as first-class NavNodes (unified nav tree)

**Status:** Design approved 2026-07-08. Frontend-only. Parent: ZEB-603 (Commons). Completes the Commons C (ZEB-606) unified-nav IA and delivers the deferred ZEB-662 follow-up (per-channel mention precision).

## Goal

Make community channels **first-class `NavService` NavNodes** so the main nav renders one **unified community→channel tree** — the community row expands to its channels in place — and retire the per-community `ChannelSubSidebar` (the "Discord double-rail"). This:

1. Completes the Commons information-architecture decision (`docs/design/commons/references/Harmony Desktop.dc.html`: *"one unified nav tree — communities expand to channels in place. No separate server rail"*), which ZEB-606 began but couldn't finish.
2. Delivers the deferred ZEB-662 follow-up: **per-channel mention badges + precise per-channel clear**, retiring the community-aggregate approximation ZEB-662 shipped as a stopgap.

## What already exists (grounding, verified 2026-07-08)

- **The nav tree already renders channels inline.** `NavTree.svelte:74` recurses into a community's children (`{#if (child.type === 'folder' || child.type === 'community') && child.expanded}`); `NavPanel.svelte:238` — *"Communities render their children (channels) like folders since ZEB-263."* This works in the **DEV mock seed** (which contains `type:'channel'` nodes) and is inert in production.
- **Channels are never `NavNode`s in production.** Communities reach `NavService` via App's `nav-updated` listener → `addOrUpdateNavSpace` (handles `community`/`dm`/`group-dm`; **channel kind is silently ignored** — the backend never emits it). Channels flow through a *separate* pipe: `communityService.listChannels(communityId)` (cached) + the `channel-config-updated` event (created/modified/deleted → cache invalidation), consumed only by `CommunityView` / `ChannelSubSidebar`.
- **The current channel picker is `ChannelSubSidebar.svelte`** — rendered inside `CommunityView`'s `.three-cols` layout (`CommunityView.svelte:476`). It owns select (`onSelect` → `handleSelect` → `setSelectedChannel`), create/rename/delete (power-gated by `POWER_THRESHOLDS.kick`), text/voice glyphs, and a §6.8 guard that closes its moderation menu on demotion.
- **The "special row under a community" pattern already exists:** `ProposalsNavRow.svelte`, appended by `NavTree.svelte:76-82` after a community's channel children, routed by `App.openCommunityProposals` (select community + `communityActiveView='proposals'`). Channel selection mirrors this.
- **ZEB-662 mention plumbing:** `NavNode.mentionCount` (community node = sum of descendant channels — the invariant); `NavService.incMention(communityId, channelId)` (channel-node-else-community fallback) / `clearMention(id)`; `MentionAlertService` deps `isActiveChannel` / `getChannelName` resolve from `communityService`. ZEB-662 also added a community-open `clearMention` as the aggregate-clear stopgap.

## Scope

**Full unification, mentions-only, frontend-only.** No backend/CRDT change — channels already exist server-side.

### Non-goals (deferred)

- **General per-message unread** (`NavNode.unreadCount`) — needs a per-channel last-read cursor / HLC-watermark model (ZEB-662 deferred #4). Channel rows show mention badges only; no "unread channel" bolding.
- **Governance-in-nav** — Proposals already has its nav row; Constitutional/Charter stay `CommunityView` tabs.

## Architecture

`NavService` gains one reconcile method (`setChannels`) and stays a pure tree store. A new injected **`ChannelNavSync`** module owns the bridge from the channel pipe (`communityService` + `channel-config-updated`) into `NavService` — mirroring the dep-injection pattern of `mention-alert.ts` / `incoming-call-alert.ts` (all side effects injected → deterministic, unit-testable). App instantiates it and wires the `channel-config-updated` event to it.

```
communityService.listChannels ─┐
channel-config-updated event ──┼──▶ ChannelNavSync ──▶ navService.setChannels ──▶ nav tree renders
nav-updated (community added) ─┘        (the bridge)         (reconcile)          channel rows inline
```

Selection and management move to App level (the nav is App-scoped), making the nav self-sufficient and shrinking `CommunityView` to feed + governance + members.

## Components / files

| File | Change |
|---|---|
| `src/lib/types.ts` | `NavNode.channelKind?: 'text' \| 'voice'` (set only on channel nodes). |
| `src/lib/nav-service.ts` | `setChannels(communityId, ChannelInfo[])` reconcile; community-remove also drops channel children; remove the ZEB-662 community-open clear path (clear is per-channel now). |
| `src/lib/channel-nav-sync.ts` | **New.** `ChannelNavSyncService` (injected deps) + `start()` / `resync(communityId)`. (Post-boot community joins are covered by `resync` on community-switch rather than a dedicated `onCommunityAdded` — see the bridge note below.) |
| `src/lib/components/NavNodeRow.svelte` | Channel-row rendering: `#`/`🔊` glyph by `channelKind`, per-channel mention badge (existing ZEB-662 badge), active styling (`--primary-soft` bg + `--primary-deep` text per Commons), power-gated context menu (rename/delete) + §6.8 demotion guard. |
| `src/lib/components/NavTree.svelte` | Channel-row `onClick` → `openCommunityChannel`; per-community ＋ add-channel affordance (power-gated, selected community only). |
| `src/lib/components/CommunityView.svelte` | Remove `ChannelSubSidebar` + channel-management dialogs/state/handlers; `.three-cols` → 2-col (feed \| members); drop the "Channels" view-tab. |
| `src/lib/components/ChannelSubSidebar.svelte` | **Deleted** (+ its test); coverage moves to nav-row + management tests. |
| `src/App.svelte` | Instantiate `ChannelNavSync`; wire `channel-config-updated` → `resync`; `openCommunityChannel(communityId, channelId)`; hoist `CreateChannelDialog`/`ModifyChannelDialog`/delete-confirm + `pickFallbackChannel`; context-menu/create state gated by `myCommunityPower`. |

## Behavior

### Data core — `NavService.setChannels(communityId, channels)`

Reconcile the community's channel children against the incoming `ChannelInfo[]` (deleted-filtered by the caller):

- **Add** each new `channelId`: `{ id: channelId, parentId: communityId, type: 'channel', channelKind: kind, name, expanded: false, unreadCount: 0, mentionCount: 0, unreadLevel: 'none' }`.
- **Update** `name` + `channelKind` on survivors; **preserve their `mentionCount`** (and `expanded`).
- **Remove** channel nodes whose id is absent from the list, subtracting each removed node's `mentionCount` from the community bubble (reuse the `clearMention`/`applyMentionDelta` math so the invariant holds).
- **Order:** channel children follow `listChannels` order (backend sorts `created_at` asc, general-first); no activity re-sort for `type:'channel'`.
- Fire `onChange()` once.

**Community removal:** `addOrUpdateNavSpace` `community`/`removed` (and any community-drop path) must also remove nodes whose `parentId === spaceId`, not just the community node.

### The bridge — `ChannelNavSync`

Injected deps (no direct service refs beyond what's passed):

```ts
interface ChannelNavSyncDeps {
  listChannels(communityId: string): Promise<ChannelInfo[]>;      // communityService.listChannels
  setChannels(communityId: string, channels: ChannelInfo[]): void; // navService.setChannels
  listCommunityIds(): string[];                                    // nav community node ids
}
```

- `start()` — eager boot: for each joined community id, `listChannels` → filter `deletedAt` → `setChannels`. Per-community try/catch (see edges). Overlapping resyncs for one community apply last-write-wins (a per-community issue counter drops a stale in-flight snapshot).
- `resync(communityId)` — on `channel-config-updated` (create/rename/delete): the event already invalidated `communityService`'s cache, so `listChannels` re-fetches → `setChannels`.

App wires: call `start()` once nav communities are hydrated; subscribe the `channel-config-updated` listener to `resync(p.communityId)`. **Post-boot community joins** are covered by calling `resync(id)` from `changeSelectedCommunity` (the first switch into a freshly-joined community populates its channels) rather than a dedicated `onCommunityAdded` hook — one fewer bridge method, and a community you never open needs no channel nodes yet.

### Selection — `openCommunityChannel(communityId, channelId)`

Mirrors `openCommunityProposals`:

```
if (appMode !== 'messages') switchMode('messages');
changeSelectedCommunity(communityId);
void refreshCommunityMembers(communityId);
communityService.setSelectedChannel(communityId, channelId);
communityActiveView = 'channels';
```

Wired as the nav channel-row `onClick`. No `kind` argument is needed: `CommunityView` still lists the community's channels (via `listChannels`) and resolves the selected one — reactively from `communityService.getSelectedChannel` instead of its retired internal `activeChannelId` — then renders `VoiceChannelView` vs `ChannelMessageFeed` off that channel's `kind`, exactly as today. The nav row's `channelKind` is only for the glyph. **Active highlight:** a channel row is active iff `selectedCommunityId === itsCommunity && communityActiveView === 'channels' && communityService.getSelectedChannel(itsCommunity) === itsId`.

`CommunityView`'s internal `activeChannelId` bookkeeping (persist on select, fallback re-select on delete) moves to App so nothing regresses (see edges).

### Management migration

Hoist `CreateChannelDialog` / `ModifyChannelDialog` / delete-confirm to App (App already has `communityService` + `myCommunityPower`). Triggers move to the nav:

- **Channel-row context menu** (rename / delete) + a per-community **＋ add-channel** affordance.
- Power-gated by `myCommunityPower >= POWER_THRESHOLDS.kick` (create, rename, delete), and **shown only on the selected community's rows** — App resolves `myCommunityPower` from the *selected* community's roster only, so restricting management to it avoids resolving power for unopened communities and matches the "manage the community you're in" model. Clicking any channel selects its community first.
- Reuse the existing dialog components verbatim; only mount point + trigger move.
- Preserve the `ChannelSubSidebar` §6.8 guard: close the moderation context menu when `myCommunityPower` drops below `kick`.

### CommunityView retirement

Remove `ChannelSubSidebar` and the channel-management dialogs/state/handlers. `.three-cols` becomes 2-col (**feed | members**). The **"Channels" view-tab is removed** — a channel is reached by clicking its nav row (`activeView='channels'` shows the feed). **Proposals / Constitutional / Charter tabs stay** (governance-in-nav is a non-goal; Proposals also keeps its existing nav row).

### Mentions reconciliation

- `incMention(communityId, channelId)` keeps **channel-else-community** targeting. Now the channel node normally exists, so the badge lands per-channel and bubbles to the community (collapsed community shows the rollup; expanded shows per-channel). The community fallback is retained **only** as a boot-race safety net (a mention arriving before `ChannelNavSync` populated that community).
- **Remove the ZEB-662 community-open `clearMention`.** Clearing is per-channel on channel-row select; the community rollup decrements naturally. (Clearing the community node directly would zero it without touching children → break the sum invariant.)
- `isActiveChannel` and `getCachedChannelName` are unchanged (both resolve from `communityService`).

## Edge cases

- **Delete the active channel:** `pickFallbackChannel` (re-select #general / first surviving) moves to App, driven off `channel-config-updated: deleted` → `resync` drops the node → App re-selects if the deleted one was active.
- **`listChannels` failure:** `ChannelNavSync` try/catches per community — that community renders childless (community-node rollup only), logs a warning, and retries on the next `channel-config-updated` or community re-open. Never throws into boot.
- **Community removed:** NavService drops the community node **and** its channel children.
- **Demotion while context menu open:** close the moderation menu when power < `kick` (preserve `ChannelSubSidebar` §6.8).
- **Boot-race residual:** a mention landing on a community node (channel not yet populated) stays there until restart (session-ephemeral) — a rare, acceptable, self-healing edge; do not add a community-open clear that would break the invariant.
- **Ordering / persistence:** channels in `listChannels` order; selected-channel (`setSelectedChannel`) and community-`expanded` state already persist — no new persistence.

## Testing

- **`channel-nav-sync.test.ts` (new):** `start()` eager-populates every joined community; `resync` reconciles add/rename/delete; a stale overlapping resync is dropped (last-write-wins); a `listChannels` rejection is swallowed and doesn't block other communities; all deps injected.
- **`nav-service.test.ts`:** `setChannels` reconcile (add / update-name / remove, **preserve `mentionCount`**, `listChannels` order); community-remove drops channel children; per-channel `incMention` bubbles to community; boot-race fallback (channel absent) still lands on the community node.
- **Selection:** `openCommunityChannel` sets community + selected channel + `activeView='channels'`; voice vs text resolves in the feed pane.
- **Management:** context-menu power-gating (only the selected community, `power >= kick`); create/rename/delete open the hoisted dialogs; §6.8 demotion closes the menu.
- **Retirement:** update/retire `CommunityView` + delete `ChannelSubSidebar` tests; move coverage to nav-row + management tests.
- **Gates:** `npx tsc --noEmit` + `npx vitest run` + `style-token-guard` (new channel-row glyph/badge/active styling uses `var(--*)` tokens only).

## Suggested plan slices

One epic, sliced for review: (1) `NavNode.channelKind` + `NavService.setChannels` + community-remove children (data core, tested). (2) `ChannelNavSync` module + App wiring + eager boot (channels appear in nav, read-only). (3) Selection routing `openCommunityChannel` + active highlight. (4) Management migration (hoist dialogs + nav context menu/＋, power-gating). (5) Retire `ChannelSubSidebar` + CommunityView 2-col + drop Channels tab. (6) Mentions revert (per-channel clear; remove community-open clear) + final sweep.
