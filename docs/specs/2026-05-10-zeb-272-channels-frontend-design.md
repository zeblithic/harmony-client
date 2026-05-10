# ZEB-272 — Sub-C v2 Phase 4: channels frontend (design)

**Status:** Draft (2026-05-10).
**Linear:** [ZEB-272](https://linear.app/zeblith/issue/ZEB-272).
**Parent ticket:** [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (Sub-C v2 — channels-within-communities).
**Parent spec:** `docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md` (commit `5145484`).
**Phase:** Final (4 of 4).
**Predecessors (all merged):**
* Phase 1 — channel-config CRDT — [ZEB-266](https://linear.app/zeblith/issue/ZEB-266) — PR #93 (2026-05-09).
* Phase 2 — ChannelLog data plane — [ZEB-269](https://linear.app/zeblith/issue/ZEB-269) — PR #95 (2026-05-09).
* Phase 3 — ChannelLog Zenoh transport + IPCs — [ZEB-270](https://linear.app/zeblith/issue/ZEB-270) — PR #96 (2026-05-10).
**Base commit:** `ec58fe2` (`origin/main` after PR #96 merge).

---

## 1. Context & motivation

Phases 1-3 shipped the backend for channels-within-communities: the channel-config CRDT (`MembershipEventKind::ChannelCreate/Modify/Delete`), the in-process ChannelLog data plane (signed events, AEAD, manifest+segments), the per-channel Zenoh transport (`ChannelLogEngine`, `ChannelLogRegistry`), and the three IPCs (`post_channel_message`, `list_channel_messages`, `request_channel_backfill`) plus two Tauri events (`channel-message-received`, `channel-backfill-progress`). All of this works end-to-end via integration tests and is invisible to the user — no UI surface consumes any of it.

Phase 4 wires the user-facing surface. The parent spec §12 (UI surface) prescribes the architecture: when a community NavNode is selected, mount `CommunityView` instead of today's direct `CommunitySettingsPanel` mount. `CommunityView` is a three-column layout (channel sub-sidebar | active channel feed | members panel) with the existing settings-and-admin surface relocated behind a ⚙️ icon. This phase is the final phase — its merge satisfies ZEB-248's full acceptance criteria and closes the parent epic.

## 2. Goals

* **Convert backend channels into user-visible channels.** Three-column `CommunityView` mounts on community-nav-select; channel sub-sidebar lists channels; selecting one shows its message feed; compose box posts; new messages appear live.
* **Relocate (not redesign) settings.** `CommunitySettingsPanel.svelte` (member list + admin actions: leave, kick, setpower, invite-link manager, name/kind header) keeps its current shape but moves into a modal overlay revealed by ⚙️ in `CommunityView`'s header. Members are also rendered in `CommunityView`'s right column for in-context visibility.
* **Channel CRUD via dialogs.** `CreateChannelDialog`, `ModifyChannelDialog`, and a typed-confirm channel-deletion confirm map to Phase 1's IPCs (`create_channel` / `modify_channel` / `delete_channel`).
* **Live message feed with scroll-trigger backfill.** Virtualized message feed; auto-scroll-to-bottom on new live message when user is already at bottom; scroll-to-top after 250 ms stable triggers `request_channel_backfill`; "Loading older messages…" skeleton.
* **Reactive power-level affordances.** "+ Create Channel" button and right-click "Rename / Set write_power / Delete" menu items appear/disappear within one event-loop tick of `community-members-changed` when the local user's power crosses the `kick` (50) threshold.
* **Session-scoped channel selection.** Selected channel persists across nav re-selection of the same community within a session; defaults to last-viewed-this-session, or `#general` on first visit.

## 3. Non-goals (explicit scope cuts)

These are out of scope for Phase 4 and remain deferred to v3 or sibling tickets:

* **Edits / deletes / reactions on individual messages.** Wire format reserves the variants (parent spec §5.2); v2 ships only `Post` UI affordances. No per-message "✎" button, no "🗑" button, no emoji reaction picker.
* **Threading UI.** `reply_to: Option<MessageId>` exists in the wire format; Phase 4 always submits `None` and does not render thread parents/indicators.
* **Non-text content kinds.** `kd: u8` content-kind code reserved (parent spec §3); Phase 4 ships only `kd=0` (text). No image upload, no attachment, no voice clip.
* **Pinned channels in nav.** Declined during ZEB-248 design (parent spec §3, layout-C variant).
* **Read receipts / unread tracking.** Separate concern; future ticket.
* **Private channels.** Subset-of-members read/write; v3+; ChannelKey HKDF derivation already supports per-channel key isolation.
* **Channel categories / nested folders.** Outside ZEB-248 scope.
* **Per-channel rate-limiting / DoS guards.** Parent spec §3 — covered by `limit` parameter on backfill IPC.
* **Backfill auto-retry / exponential backoff.** Parent spec §3 — Phase 4 surfaces the manual "retry" button on backfill failure and that's it.
* **Voice / video channels.** Voice Engine track; not part of ZEB-248.
* **localStorage persistence of selected channel.** Parent spec §12.5 says session-scoped; Phase 4 confirms.
* **Per-message profile popover for channel posts.** Existing `onAvatarClick` hook is reused, but enriched author UI (status, role badges) is separate.
* **Visual regression / screenshot tests.** No infrastructure for it on this project.
* **a11y audit beyond the basics.** `role="dialog"` on the modal + Esc-closes + focus trap is the bar; full a11y sweep is a separate ticket.

## 4. Architecture overview

Phase 4 introduces no new substrates. All backend interaction is mediated by the two existing IPC streams from Phases 1-3 (channel-config + channel-message + backfill-progress), which are consumed by two services in the frontend (`CommunityService`-extended + new `ChannelMessageService`), which are then consumed by four new Svelte components (`CommunityView` layout shell + three column components) plus two new dialogs (`CreateChannelDialog` + `ModifyChannelDialog`). The existing `CommunitySettingsPanel.svelte` is **not modified** — only its mount-point changes from "direct child of App.svelte" to "child of a modal inside CommunityView."

```
                          BACKEND IPCs                  FRONTEND CONSUMERS
                          ───────────────                ─────────────────────
channel-config-updated  ─────────────────────▶  CommunityService.onChannelConfigChanged
channel-message-received ────────────────────▶  ChannelMessageService.onMessage
channel-backfill-progress ───────────────────▶  ChannelMessageService.onBackfillProgress

                          FRONTEND COMPONENTS
                          ───────────────────
                          App.svelte
                            └─ CommunityView (mounts when selected NavNode is community)
                                 ├─ ChannelSubSidebar       (left column)
                                 ├─ ChannelMessageFeed       (center column)
                                 ├─ ChannelMembersPanel      (right column)
                                 ├─ {settings modal} CommunitySettingsPanel  (existing — unmodified)
                                 ├─ {open?} CreateChannelDialog
                                 └─ {open?} ModifyChannelDialog
```

## 5. File structure

### 5.1 New files (13 total)

```
src/lib/
├── channel-message-service.ts                         (~250L; mirrors message-service.ts shape)
└── components/
    ├── CommunityView.svelte                           (~120L; layout shell + ⚙️ click handler)
    ├── ChannelSubSidebar.svelte                       (~150L; channel list + "+" + right-click menu)
    ├── ChannelMessageFeed.svelte                      (~250L; virtualized feed + scroll-trigger backfill + ComposeBar)
    ├── ChannelMembersPanel.svelte                     (~80L; right-column member list, collapsible)
    ├── CreateChannelDialog.svelte                     (~100L; name + write_power-hidden-pair)
    ├── ModifyChannelDialog.svelte                     (~110L; same shape, partial-update aware)
    └── __tests__/
        ├── channel-message-service.test.ts            (~200L)
        ├── CommunityView.test.ts                      (~150L)
        ├── ChannelSubSidebar.test.ts                  (~120L)
        ├── ChannelMessageFeed.test.ts                 (~150L)
        ├── CreateChannelDialog.test.ts                (~80L)
        └── ModifyChannelDialog.test.ts                (~80L)
```

### 5.2 Modified files (3)

```
src/lib/community-service.ts                           (+~80L: createChannel/modifyChannel/deleteChannel/listChannels + onChannelConfigChanged + selectedChannelByCommunity Map)
src/lib/__tests__/community-service.test.ts            (+~80L: channel-config method + selected-channel tests)
src/App.svelte                                         (~-30L net: replace CommunitySettingsPanel mount at L1525 with CommunityView, route members-changed/myPower props through; add channelMessageService instantiation alongside communityService)
```

`CommunitySettingsPanel.svelte` and `TextFeed.svelte` are deliberately **not modified**: relocation happens at the mount-point (App.svelte), not inside the panel; the channel feed is a fork (`ChannelMessageFeed.svelte`), not a TextFeed extension.

## 6. Plan-time decisions (locked)

Eight plan-time questions were resolved during brainstorming on 2026-05-10. These are spec-locked; deviation requires re-opening the spec.

### 6.1 Message feed component shape — **fork, not extend**

**Decision:** New `ChannelMessageFeed.svelte`. Do not retrofit `TextFeed.svelte`.

**Rationale:** `TextFeed.svelte` is 253L and has DM-specific features (threads with drag-handle split, `FloatingThreadBar`, `ThreadIndicator`, optimistic-delete UX) that channels don't need. It also lacks features channels do need: true virtualization (`TextFeed` `{#each}`-renders all messages, which is fine for DM volumes but violates the "engineer for real scale" rule for channel volumes), and scroll-to-top backfill trigger. Forking is the smaller-blast-radius option; `TextFeed` keeps its proven DM shape unchanged. v3 will likely retrofit `ChannelMessageFeed` with thread support — at which point we evaluate whether to share a render-loop core between the two; not Phase 4's call.

### 6.2 Component decomposition — **layout shell + three column components**

**Decision:** `CommunityView.svelte` is a slim layout shell (~120L). The three columns are independent components: `ChannelSubSidebar`, `ChannelMessageFeed`, `ChannelMembersPanel`.

**Rationale:** Each column has a distinct responsibility (channel listing/selection, message rendering/posting, member presence) with a clear interface to the shell. Independent components are testable in isolation and respect the per-file size discipline. Inlining all three into a single `CommunityView.svelte` would push the file past 300L mixing layout, sidebar event handling, virtualization, and member rendering — exactly the unwieldy-file pattern the file-structure rule is meant to prevent.

### 6.3 Settings panel relocation — **modal overlay**

**Decision:** ⚙️ icon in `CommunityView`'s header opens `CommunitySettingsPanel` in a modal overlay. Esc-closes; click-outside-closes; focus-trap inside while open; `role="dialog"` + `aria-modal="true"` + `aria-labelledby` for the title.

**Rationale:** Familiar Discord/Slack idiom; users don't have to learn a new affordance. `CommunitySettingsPanel.svelte` keeps its current shape — only a wrapper `<div role="dialog">` is added at the mount-point. Tab-strip alternative was rejected as adding chrome users don't usually need; slide-over alternative was rejected as conflicting with the right-column members panel.

### 6.4 Channel-deleted-while-viewing UX — **cascade-fallback to `#general`**

**Decision:** When `channel-config-updated { action: 'Deleted', channelId }` arrives and `channelId` is the local user's currently active channel, cascade to a fallback in this order:

1. `#general` if it exists and is not the deleted channel.
2. Next-oldest channel by `created_at` HLC.
3. Empty-state placeholder ("No channels in this community yet — admin can create one with the + button.") if no channels remain.

A toast confirms the deletion was observed: `# {name} was deleted`. No attribution (no "by Bob") — keeps chrome low for routine admin actions.

**Rationale:** Discord-style auto-redirect minimizes disruption. `#general` is the natural fallback (auto-created at community creation, oldest channel, never absent under normal conditions). The cascade handles the unusual case where `#general` itself was deleted.

### 6.5 Selected-channel persistence — **`CommunityService` field, session-scoped**

**Decision:** Add `selectedChannelByCommunity: Map<string, string>` field to `CommunityService`. Lifetime tied to the service (which is session-scoped). Cleared by `destroy()`. No localStorage.

**Rationale:** Mirrors existing `memberCache` / `degraded` Map patterns on `CommunityService`. Naturally cleared by `destroy()`. Avoids introducing a new Svelte store module for one map. Per parent spec §12.5 confirmation — session-scoped, not cross-session.

### 6.6 Channel-rename-while-viewing UX — **silent re-render**

**Decision:** When `channel-config-updated { action: 'Modified', channelId, name: Some(newName) }` arrives, the header in `ChannelMessageFeed` and the entry in `ChannelSubSidebar` update silently to the new name. No toast, no inline system message.

**Rationale:** Matches Discord/Slack default. Less chrome for routine admin actions. Toast adds noise if admin batches renames; inline system messages would require a new wire-format event kind out of scope for Phase 4.

### 6.7 Backfill scroll-trigger debounce — **250 ms stable + single-in-flight**

**Decision:** Fire `requestBackfill` only after scroll position has been at top (scrollTop < 50px) for 250 ms uninterrupted. Single in-flight gate: if a backfill is in-flight, additional triggers are no-ops until the request completes (terminal `channel-backfill-progress` with `fetched == totalEstimate`, or 10 s timeout).

**Rationale:** 250 ms matches the debounce interval already in use in `community_state_sync.rs` and `dm_outbox.rs` tail-flush logic — one consistent magic number across backend and frontend. Single-in-flight gate prevents Page-Up-held storms. 10 s timeout matches Phase 3's backfill request driver timeout.

### 6.8 Reactive power-level recompute — **`myPower` prop from `App.svelte`**

**Decision:** `App.svelte` computes `myPower` for the selected community via `$derived` from `members[selectedCommunityId]`, mirroring the existing pattern that feeds `CommunitySettingsPanel`. Passes it as a prop to `CommunityView`, which forwards to `ChannelSubSidebar`. The existing `communityService.onMembersChanged` callback (`community-service.ts:42`) drives `App.svelte`'s roster re-fetch, which triggers the `$derived` recompute, which propagates the prop change.

**Rationale:** Consistent with existing `CommunitySettingsPanel` pattern. No new state surfaces. The reactivity chain is one already proven by Sub-C v1 Phase 5.

## 7. Component contracts

### 7.1 `CommunityView.svelte`

```typescript
props {
  communityId: string;
  communityName: string;
  communityKind: 'open' | 'invite-only' | 'unknown';
  myPower: number;
  ownAddress: string;
  members: CommunityMember[];
  isDegraded: boolean;
  communityService: CommunityService;
  channelMessageService: ChannelMessageService;
  trustService: TrustService;
  // Existing handlers passed-through to CommunitySettingsPanel inside the modal:
  onLeave: () => Promise<void>;
  onKickMember: (addr: string) => Promise<void>;
  onSetPowerLevel: (addr: string, power: number) => Promise<void>;
  onGenerateInvite: () => Promise<string>;
}
state owned {
  channels: ChannelInfo[];          // synced from communityService.listChannels + onChannelConfigChanged
  activeChannelId: string | null;   // session-scoped via communityService.getSelectedChannel()
  settingsModalOpen: boolean;
  showCreateDialog: boolean;
  modifyDialogChannel: ChannelInfo | null;
  deleteConfirmChannel: ChannelInfo | null;
}
```

### 7.2 `ChannelSubSidebar.svelte`

```typescript
props {
  channels: ChannelInfo[];          // already sorted oldest-first by parent
  activeChannelId: string | null;
  myPower: number;
  onSelect: (channelId: string) => void;
  onCreateClick: () => void;
  onModifyClick: (channel: ChannelInfo) => void;
  onDeleteClick: (channel: ChannelInfo) => void;
}
state owned {
  contextMenuOpen: { channelId: string; x: number; y: number } | null;
}
```

### 7.3 `ChannelMessageFeed.svelte`

```typescript
props {
  communityId: string;
  channelId: string;
  channelName: string;              // re-renders silently on rename (§6.6)
  channelMessageService: ChannelMessageService;
  ownAddress: string;
  trustService: TrustService;
  myPower: number;                  // for compose-disable when myPower < channel.write_power (v3 only; v2 always 0)
}
state owned {
  messages: ChannelMessageDto[];    // local mirror of service cache for this channel
  scrollAtBottom: boolean;
  scrollAtTop: boolean;
  backfillInFlight: boolean;
  backfillProgress: { fetched: number; totalEstimate?: number } | null;
}
lifecycle {
  onMount: subscribe to channelMessageService for (communityId, channelId)
           + listMessages(communityId, channelId, since=undefined, limit=100)
  onDestroy: unsubscribe
  $effect on channelId: switch subscription to new channel + listMessages fresh
}
behaviors {
  - Auto-scroll to bottom on new live message IF scrollAtBottom was true before append
  - Detect scrollAtTop (scrollTop < 50px) → after 250ms stable → fire requestBackfill if not in-flight (§6.7)
  - Show "Loading older messages…" skeleton at top while backfillInFlight
  - Hide skeleton on terminal backfill-progress (fetched == totalEstimate) or 10s timeout
  - ComposeBar at bottom: Enter posts via channelMessageService.postMessage; Shift+Enter newline
}
```

### 7.4 `ChannelMembersPanel.svelte`

```typescript
props {
  members: CommunityMember[];
  ownAddress: string;
  trustService: TrustService;
  collapsed: boolean;               // viewport-width-driven from App.svelte
  onAvatarClick?: (address: string, event: MouseEvent) => void;
}
```

### 7.5 `CreateChannelDialog.svelte`

```typescript
props {
  communityId: string;
  communityService: CommunityService;
  open: boolean;
  myPower: number;                  // for auto-close on demotion
  onClose: () => void;
  onCreated: (channelId: string) => void;   // parent uses this to switch active to new channel
}
state owned {
  name: string;                     // 1-32 char validation
  writePower: number;               // v2 always 0 (slider+number-input pair hidden behind `// v3 unhide`)
  submitting: boolean;
  error: string | null;
}
behaviors {
  - $effect on myPower: if drops below 50 while open, auto-close + onClose() (§10 "Power dropped mid-action")
}
```

### 7.6 `ModifyChannelDialog.svelte`

```typescript
props {
  communityId: string;
  channel: ChannelInfo;             // current values pre-fill the form
  communityService: CommunityService;
  open: boolean;
  myPower: number;                  // for auto-close on demotion
  onClose: () => void;
}
state owned {
  name: string;                     // pre-filled from channel.name
  writePower: number;               // pre-filled (hidden in v2)
  submitting: boolean;
  error: string | null;
}
behaviors {
  - On submit, build partial-update payload (only changed fields → Some, others → None)
  - Reject all-None as no-op before IPC dispatch
  - $effect on myPower: if drops below 50 while open, auto-close + onClose() (§10 "Power dropped mid-action")
}
```

### 7.7 `ChannelMessageService` (new — `src/lib/channel-message-service.ts`)

```typescript
class ChannelMessageService {
  // Consumed-event callbacks
  onMessage?: (communityId: string, channelId: string, message: ChannelMessageDto) => void;
  onBackfillProgress?: (communityId: string, channelId: string, fetched: number, totalEstimate?: number) => void;

  async connectAdapter(adapter: TauriAdapter): Promise<void>;

  // IPC method facades (all map 1:1 to Phase 3 IPCs)
  async postMessage(communityId: string, channelId: string, body: string, replyTo?: string): Promise<string>;
  async listMessages(communityId: string, channelId: string, since?: HlcDto, limit?: number): Promise<ChannelMessageDto[]>;
  async requestBackfill(communityId: string, channelId: string, since?: HlcDto): Promise<void>;

  // Per-channel subscriber registry
  subscribeToChannel(
    communityId: string,
    channelId: string,
    callback: (msg: ChannelMessageDto) => void,
  ): () => void;

  // Local cache accessors (read-only snapshot)
  getMessages(communityId: string, channelId: string): ChannelMessageDto[];

  destroy(): void;
}
```

Internal cache shape:

```typescript
private byChannel: Map<string, ChannelMessageDto[]>;       // key = `${communityId}:${channelId}`, sorted by HLC asc
private subscribers: Map<string, Set<(msg) => void>>;      // per-channel subscribers
private inFlightBackfill: Set<string>;                     // single-in-flight gate (§6.7)
private backfillProgress: Map<string, { fetched: number; totalEstimate?: number }>;
private seenIds: Map<string, Set<string>>;                 // per-channel dedup
```

### 7.8 `CommunityService` extensions (~80L added)

```typescript
class CommunityService {
  // Existing fields above + new:
  onChannelConfigChanged?: (
    communityId: string,
    action: 'Created' | 'Modified' | 'Deleted',
    channelId: string,
    name?: string,
    writePower?: number,
  ) => void;
  private channelCache: Map<string, ChannelInfo[]>;                 // per-community
  private selectedChannelByCommunity: Map<string, string>;          // §6.5

  // connectAdapter() additionally subscribes to 'channel-config-updated'

  // New IPC method facades
  async createChannel(communityId: string, name: string, writePower: number): Promise<string>;
  async modifyChannel(communityId: string, channelId: string, name?: string, writePower?: number): Promise<void>;
  async deleteChannel(communityId: string, channelId: string): Promise<void>;
  async listChannels(communityId: string): Promise<ChannelInfo[]>;

  // Selected-channel state (§6.5)
  getSelectedChannel(communityId: string): string | undefined;
  setSelectedChannel(communityId: string, channelId: string): void;
}
```

## 8. Data flow

### 8.1 Channel-config flow (rename / create / delete)

```
backend ChannelCreate/Modify/Delete event materializes
   └─▶ channel-config-updated IPC fires
          └─▶ communityService.onChannelConfigChanged(action, channelId, name?, writePower?)
                 ├─ invalidate channelCache.get(communityId)
                 ├─ if action == 'Created': insert sorted into channels
                 ├─ if action == 'Modified': update existing entry
                 ├─ if action == 'Deleted':
                 │     ├─ remove from channels
                 │     └─ if channelId === getSelectedChannel(communityId):
                 │            ├─ pick fallback per §6.4 cascade
                 │            ├─ setSelectedChannel(communityId, fallback)
                 │            └─ emit toast ("# {name} was deleted")
                 └─ notify channel-list subscribers (sub-sidebar re-renders)
```

### 8.2 Channel-message flow (live + backfill)

```
backend channel-message-received IPC fires
   └─▶ channelMessageService internal listener
          ├─ dedupe by message.id (per-channel seenIds set)
          ├─ insert into byChannel sorted by HLC ascending
          └─ notify per-channel subscribers
                 └─▶ ChannelMessageFeed.subscribeToChannel callback
                        ├─ append to local mirror
                        └─ if scrollAtBottom: scroll to new bottom
```

### 8.3 Backfill flow (scroll-trigger + progress)

```
user scrolls to scrollTop < 50px
   └─▶ ChannelMessageFeed scrollAtTop = true
          └─▶ 250ms stable timer
                 └─▶ if !backfillInFlight:
                        ├─ backfillInFlight = true
                        ├─ channelMessageService.requestBackfill(communityId, channelId, since=oldestHlc)
                        │      └─▶ backend ChannelLogEngine queryable backfill request
                        │             ├─ stream of channel-message-received events arrives
                        │             │     └─▶ live flow (§8.2) prepends them
                        │             └─ channel-backfill-progress events
                        │                    └─▶ channelMessageService.onBackfillProgress
                        │                           └─▶ ChannelMessageFeed updates skeleton state
                        └─ on terminal progress (fetched == totalEstimate) or 10s timeout:
                              ├─ backfillInFlight = false
                              └─ hide skeleton
```

### 8.4 Reactive cascade for myPower (§6.8)

```
backend Kick / SetPower event materializes
   └─▶ community-members-changed IPC fires
          └─▶ communityService.onMembersChanged(communityId)
                 └─▶ App.svelte refetches listCommunityMembers (existing pattern)
                       └─▶ App.svelte recomputes derivedMyPower via $derived
                             └─▶ <CommunityView myPower={...}> prop updates
                                   └─▶ <ChannelSubSidebar myPower={...}> prop updates
                                         └─▶ {#if myPower >= 50} "+" + context menu re-render
```

### 8.5 ComposeBar post flow

```
user types "hello" + presses Enter
   └─▶ ComposeBar onSend("hello")
          └─▶ ChannelMessageFeed handler
                 └─▶ channelMessageService.postMessage(communityId, channelId, "hello", undefined)
                        └─▶ post_channel_message IPC
                               ├─ backend mints SignedChannelEvent::Post + signs + encrypts + broadcasts
                               └─ self-loopback returns the event via channel-message-received
                                      └─▶ live flow (§8.2) appends; ComposeBar clears
```

### 8.6 ⚙️ settings modal

```
user clicks ⚙️ in CommunityView header
   └─▶ settingsModalOpen = true
          └─▶ <CommunitySettingsPanel> mounts inside <div role="dialog" aria-modal="true">
                 ├─ Esc keydown → settingsModalOpen = false
                 ├─ click outside dialog → settingsModalOpen = false
                 └─ focus trap (existing pattern from ConfirmDialog/ConfirmationModal)
```

## 9. App.svelte routing change

Today's mount at `App.svelte:1525-1577`:

```svelte
{:else if selectedNode && communityService.getKind(selectedNode.id) !== 'unknown'}
  <CommunitySettingsPanel
    communityId={selectedNode.id}
    communityName={selectedNode.name}
    communityKind={communityService.getKind(selectedNode.id)}
    myPower={...}
    members={...}
    isDegraded={...}
    onLeave={...}
    onKickMember={...}
    onSetPowerLevel={...}
    onGenerateInvite={...}
  />
{/if}
```

Replaced with:

```svelte
{:else if selectedNode && communityService.getKind(selectedNode.id) !== 'unknown'}
  <CommunityView
    communityId={selectedNode.id}
    communityName={selectedNode.name}
    communityKind={communityService.getKind(selectedNode.id)}
    myPower={...}
    ownAddress={...}
    members={...}
    isDegraded={...}
    communityService={communityService}
    channelMessageService={channelMessageService}
    trustService={trustService}
    onLeave={...}
    onKickMember={...}
    onSetPowerLevel={...}
    onGenerateInvite={...}
  />
{/if}
```

The existing handler bindings (`onLeave`, `onKickMember`, etc.) remain identical — `CommunityView` forwards them to the embedded `CommunitySettingsPanel` modal. No new App.svelte `$state` or `$derived` is added; `derivedMyPower` and `members` already exist.

`channelMessageService` is a new top-level instance constructed alongside `communityService` (`App.svelte:321`), connected via `connectAdapter` on Tauri-adapter-ready, and destroyed on signout.

## 10. Error handling

* **IPC errors:** all four channel-config IPCs (`create_channel`, `modify_channel`, `delete_channel`, `list_channels`) and three channel-message IPCs (`post_channel_message`, `list_channel_messages`, `request_channel_backfill`) follow the existing extract-error pattern: `e instanceof Error ? e.message : String(e)` (per `feedback_tauri_error_extraction` memory rule). Errors surface inline in dialogs or via toasts in the feed.
* **Channel-not-found on selection:** if `setSelectedChannel(communityId, channelId)` is called for a channel that no longer exists in `channelCache` (e.g., lagged delete event), fall back to the §6.4 cascade and emit a toast.
* **Backfill timeout:** 10 s timeout matches the Phase 3 adapter; on timeout, hide skeleton, show "Couldn't reach peers — retry?" with a retry button that re-fires `requestBackfill`.
* **Compose post failure:** keep text in compose box, show inline error below ("Failed to post: {error} — retry?"). User can retry or clear and try later.
* **Modify-with-no-changes:** rejected client-side before IPC dispatch (no-op short-circuit). Modal closes silently.
* **Power dropped mid-action:** if user opens `CreateChannelDialog` then is demoted before submit, the IPC will reject (backend-side `verify_event` enforces `actor_power >= kick`); error surfaces inline in the dialog. The dialog itself should also auto-close on `myPower` drop (reactive `$effect`).

## 11. Testing strategy

### 11.1 Unit tests (per file, vitest)

| File | What it verifies |
|---|---|
| `community-service.test.ts` (extends existing) | `createChannel` / `modifyChannel` / `deleteChannel` / `listChannels` round-trip via fake `TauriAdapter`; `channel-config-updated` event triggers cache invalidation + `onChannelConfigChanged` callback; `selectedChannelByCommunity` get/set + cleared on `destroy()` |
| `channel-message-service.test.ts` (new) | `postMessage` / `listMessages` / `requestBackfill` IPC dispatch; `channel-message-received` event appends to per-channel cache + dedup by `message.id`; `channel-backfill-progress` updates progress; `subscribeToChannel` returns working unsub; `destroy` clears all listeners |
| `CommunityView.test.ts` (new) | Three columns mount; ⚙️ click opens modal; modal closes on Esc/click-outside; channel-deleted-while-active triggers §6.4 cascade fallback (`#general` → next-oldest → empty); channel-rename triggers silent re-render of header (§6.6) |
| `ChannelSubSidebar.test.ts` (new) | Renders channels in `ChannelInfo[]` order (parent guarantees oldest-first); active highlight; "+" button visible iff `myPower >= 50` (§6.8); right-click menu visible iff `myPower >= 50`; menu items dispatch `onModifyClick` / `onDeleteClick` |
| `ChannelMessageFeed.test.ts` (new) | Auto-scroll-to-bottom on new live message when `scrollAtBottom`; suppressed when user scrolled up; 250ms stable-at-top fires `requestBackfill` (§6.7); single-in-flight gate (second trigger during in-flight is a no-op); skeleton shows while in-flight + hides on terminal progress; ComposeBar Enter posts |
| `CreateChannelDialog.test.ts` (new) | Name 1-32 char validation rejects out-of-range; Enter submits; Cancel closes without dispatch; `createChannel` error renders inline; `onCreated` fires with returned `channel_id` |
| `ModifyChannelDialog.test.ts` (new) | Pre-fills from `channel` prop; partial update sends only changed fields (`Some` for changed, `None` for unchanged); all-`None` payload rejected as no-op before IPC; success closes |

All tests use `@testing-library/svelte` + a fake `TauriAdapter` (matching the `community-service.test.ts` pattern).

### 11.2 Manual smoke

Two-device manual smoke at end of phase:

1. Device A: create community → `#general` auto-created, mounts in `CommunityView`.
2. Device A: create `#dev-talk` via "+" → channel appears in sub-sidebar on both A and B.
3. Device A: post message in `#dev-talk` → appears live on B's feed.
4. Device B: rename `#dev-talk` → `#dev-discussion` via right-click → header re-renders silently on A.
5. Device A: scroll to top → "Loading older messages…" skeleton briefly visible (no older messages exist) → skeleton hides.
6. Device B: cold-restart → channel list + most recent N messages backfill on reconnect.
7. Device A: delete `#dev-discussion` via right-click → typed-confirm → both A and B auto-redirect to `#general`; toast confirms.
8. Device A: open ⚙️ → `CommunitySettingsPanel` modal → admin actions (kick / setpower / invite link) all functional from inside modal.

### 11.3 CI gates

Mirror what `.github/workflows/ci.yml` actually runs (jobs: `rust`, `msrv`, `frontend`):

```bash
# rust (run from src-tauri/)
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures -- -D warnings
cargo test --locked --workspace --all-targets --features test-fixtures --no-fail-fast

# msrv (run from src-tauri/)
cargo check --locked --all-targets --features test-fixtures   # using rust-version from Cargo.toml

# frontend (run from repo root)
npm ci
npx tsc --noEmit
npx vitest run
```

All five gates required at every implementer-task verification (per `feedback_cargo_fmt_gate` memory rule). Cargo commands run from `src-tauri/`; npm/npx commands run from repo root. Phase 4 is a frontend-heavy phase, so the `frontend` job is the primary gate; the `rust` + `msrv` jobs should remain green throughout (no Rust changes expected, but verify locally to catch any test-fixtures-feature drift).

## 12. Acceptance criteria

1. `CommunityView.svelte` mounts when a community NavNode is selected. Three columns visible above 1024 px viewport; `ChannelMembersPanel` collapsible.
2. `ChannelSubSidebar` lists all channels (oldest-first, `#general` first); active highlight; "+" + right-click menu visible iff `myPower >= 50`.
3. `CreateChannelDialog`, `ModifyChannelDialog`, channel-deletion typed-confirm all functional and round-trip via the Phase 1 IPCs.
4. ComposeBar posts via `post_channel_message`; new messages appear in feed via `channel-message-received` subscription.
5. Scroll-to-top stable for 250 ms triggers `request_channel_backfill`; older messages appear as `channel-message-received` arrives; "Loading older messages…" skeleton hides on terminal progress.
6. `CommunitySettingsPanel` (members + admin) accessible behind ⚙️ icon as modal overlay; Esc/click-outside closes; `role="dialog"` + `aria-modal="true"` + `aria-labelledby` for the title; focus-trap inside while open (mirroring existing `ConfirmDialog`/`ConfirmationModal` pattern).
7. `selectedChannelByCommunity` persists across nav re-selection within session; defaults to last-viewed-this-session or `#general`.
8. Power demotion mid-session: "+" + right-click menu disappear within one event-loop tick of `community-members-changed`.
9. Channel-deleted-while-viewing: cascade fallback to `#general` → next-oldest → empty-state; toast confirms.
10. Channel-renamed-while-viewing: header + sub-sidebar entry silently re-render; no toast.
11. All vitest tests in §11.1 green.
12. Manual smoke per §11.2 complete.
13. All CI gates per §11.3 green.
14. PR cuts from latest `origin/main` (currently `ec58fe2`); branch named `zeb-272-channels-frontend`; PR body uses markdown-linked refs `[ZEB-248](url)` for parent epic per `feedback_linear_pr_auto_close` memory rule.
15. PR merge closes parent ZEB-248 (final phase).

## 13. Cross-repo

None — entirely in `harmony-client`.

## 14. References

* **Parent spec:** `docs/specs/2026-05-09-zeb-248-channels-within-communities-design.md` (commit `5145484`).
* **Parent ticket:** [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) — Sub-C v2: channels-within-communities.
* **Sibling Phase 1:** [ZEB-266](https://linear.app/zeblith/issue/ZEB-266) — channel-config CRDT — PR #93.
* **Sibling Phase 2:** [ZEB-269](https://linear.app/zeblith/issue/ZEB-269) — ChannelLog data plane — PR #95.
* **Sibling Phase 3:** [ZEB-270](https://linear.app/zeblith/issue/ZEB-270) — ChannelLog Zenoh transport + IPCs — PR #96.
* **Sibling deferred bug:** [ZEB-271](https://linear.app/zeblith/issue/ZEB-271) — channel-log registry transactional spawn — NOT addressed by Phase 4.
* **Frontend pattern reference:** [ZEB-263](https://linear.app/zeblith/issue/ZEB-263) Sub-C v1 Phase 5 — community frontend (NavService kinds + create/redeem dialogs + admin UI + invite manager) — PR #91.
* **Service pattern references:**
  * `src/lib/community-service.ts` — adapter ref + method facade + event subscription pattern.
  * `src/lib/message-service.ts` — closest analog for `channel-message-service.ts` (per-conversation cache + subscribe).
* **Component pattern references:**
  * `src/lib/components/CommunitySettingsPanel.svelte` — three-section layout + `myPower` reactive gating.
  * `src/lib/components/TextFeed.svelte` — feed rendering shape (deliberately not extended for channels).
  * `src/lib/components/ConfirmDialog.svelte`, `ConfirmationModal.svelte`, `DoubleConfirmDialog.svelte`, `SetPowerDialog.svelte`, `InviteLinkManager.svelte` — dialog patterns.
* **Test pattern references:**
  * `src/lib/__tests__/community-service.test.ts` — service test shape with fake `TauriAdapter`.
  * `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` — component test shape.
  * `src/lib/components/__tests__/TextFeed.integration.test.ts` — feed component test shape.
* **User memory rules applied:**
  * `feedback_design_for_eventual_state` — design for the post-rollout state where channels are populated, not the empty initial state.
  * `feedback_engineer_for_real_scale` — virtualization on `ChannelMessageFeed` is non-negotiable.
  * `feedback_slider_pair_with_number_input` — `write_power` slider in dialogs must include paired number input from day one (hidden behind `// v3 unhide`).
  * `feedback_severe_action_confirmation` — channel deletion uses tier-3 typed-confirm (UX-irreversible).
  * `feedback_linear_pr_auto_close` — PR body uses `[ZEB-248](url)` markdown-linked ref to avoid Linear cascade.
  * `feedback_cargo_fmt_gate` — all CI gates required (cargo fmt + clippy + test + msrv-check from `src-tauri/`; tsc + vitest from repo root via npx — see §11.3 for the canonical command list).
  * `feedback_tauri_error_extraction` — `e instanceof Error ? e.message : String(e)` for IPC error surfacing.
  * `feedback_no_worktrees` — branch via `git checkout -b` in main repo, no worktrees.
  * `feedback_pull_before_work` — base on latest `origin/main` (currently `ec58fe2`).
