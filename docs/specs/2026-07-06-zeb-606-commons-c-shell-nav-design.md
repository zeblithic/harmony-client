# ZEB-606 Commons C: shell & nav restyle + Assembly rail — design

**Ticket:** ZEB-606 (parent ZEB-603 Commons adoption). **Branch:** `zeb-606-commons-c-shell-nav` off `55921a54`.
**Design reference:** `docs/design/commons/references/Harmony Desktop.dc.html` frame 1 (lines 24–116), screenshot `references/screens/03-desktop.png`.
**Depends on:** ZEB-605 (Commons token flip — landed, PR #407). All styling below uses existing `src/app.css` tokens (both themes carry them); the `style-token-guard` ratchet forbids raw color literals in `<style>` blocks.

## 0. Ticket-premise corrections (verified against code 2026-07-06)

1. **`connectivity-adapter.ts` is the wrong source for "● connected · N peers".** It exposes pkarr/iroh discovery, relay-pool config, and reachability diagnostics only — no connection state, no peer count (its own header defers "peer-into-status-bar" to a future phase). The real source is **`src/lib/network-health-adapter.ts`**: `snapshot(): Promise<NetworkHealthSnapshot>` + `onNetworkHealthChanged(cb)` (event `network-health-changed`). `NetworkHealthSnapshot.peers: PeerHealth[]` each carry `connectionMode: 'direct'|'relay'|'noConnection'|'degraded'`; `myNetwork.reachability: 'reachable'|'degraded'|'unreachable'`; `transportDisabledReason?`.
2. **The design's window-chrome elements cannot exist as drawn.** The app uses native decorations (`tauri.conf.json` has no `decorations`/`titleBarStyle` override; no drag-region anywhere in `src/`). There is no titlebar to host the centered search pill or the right-side status. The search pill stays a nav-header restyle (matching the ticket's explicit non-goal); the connection status relocates to the nav-column footer (§5).

## 1. Scope decisions

1. **Nav header keeps search + create-FAB + settings gear** (ticket text: "header (search + create + settings)" — governs over the design frame, whose header layout assumed window-chrome search). No logo/wordmark row (the Harmony mark already lives in WelcomeModal from ZEB-605; adding a logo row spends vertical space the ticket doesn't ask for).
2. **Section headers** ("Communities", "Direct messages") are added by render-time partitioning of top-level nodes in NavPanel — NavService data and `NavNode` DTO are untouched. Partition: `type === 'community'` → Communities; `type === 'dm' | 'group-chat'` → Direct messages (the `NavNodeType` union is `'folder' | 'channel' | 'dm' | 'group-chat' | 'community'`, `src/lib/types.ts:91`); anything else (user folders, root channels) renders first, un-headed, preserving today's behavior. Empty groups omit their header.
3. **Active-state unification**: every nav-column "active/pressed" state (nav rows, Notes row, footer mode toggles, settings gear) moves to `--primary-soft` bg + `--primary-deep` text. Action buttons stay `--accent`-filled (FAB, empty-state CTA); unread badges stay `--accent`. The ZEB-569 gear mechanism (`class:active` + `aria-pressed={settingsActive}`) and the ZEB-600 presence-dot wiring are preserved exactly — only colors change.
4. **Community rows get a letter avatar chip** (first grapheme of the name, uppercased; `--accent` bg, `--text-bright` text, 20px, radius 6px) replacing the 🏛️ type-icon. Chevron toggle, color bands (functional ancestry stripes — design predates them, functionality wins), and all badges are preserved.
5. **Proposals row** is a synthetic, render-level row appended inside each expanded community's subtree (not a `NavNode`; NavService never sees it). ⚖ glyph in `--gov-clay`, lowercase label "proposals", count badge (`--gov-clay` bg, `--text-bright` text, `--font-mono` 10px, radius 9px) shown when count > 0. Rendered only when a `proposalCount` resolver is provided (i.e. votingAdapter exists). Click → `onSelectProposals(communityId)`: selects the community AND opens its Proposals view (§4.4). Row is active (`--primary-soft`) when that community's Proposals view is showing.
6. **Badge/rail count = Tier-2 conviction proposals with `lifecycle ∈ {Open, ThresholdReached}`.** Tier-1 polls and Tier-3 (Town Hall) are out — Tier-3 is ZEB-612's scope, and Tier-1 has no design anchor in frame 1.
7. **Assembly rail = a third occupant of the existing messages-mode right rail, implemented entirely App-side.** `Layout.svelte` is not modified: the rail cell's content is already the App-provided `mediaFeed` snippet; that snippet now renders a new `MessagesRail.svelte` host with a two-tab header (Assembly ⚖ / Media) swapping `AssemblyRail` vs the existing `MediaFeed`. Resize, collapse, edge tab, `media-panel-prefs.ts` width/open persistence, and the pinned aria-labels (`Show/Hide media panel`) all keep working unchanged. Settings still wins the cell (existing behavior).
8. **Assembly tab is community-gated**: shown only when a community is selected and votingAdapter exists; otherwise the rail is media-only (DM contexts have no assembly). Last-selected tab persists device-scoped via a new key in `media-panel-prefs.ts` (default `assembly` — the design's "always one glance away").
9. **Connection status** renders as a slim mono strip at the very bottom of the nav column (below the identity chip): `● connected · 14 peers` / `● degraded · 3 peers` / `● offline`, using the `--net-ok/-warn/-danger` token families. Self-contained component owning its network-health subscription (no prop threading through App).
10. **Identity chip** (net-new, nav footer): initials avatar (`--accent` bg) with a presence ring when self is online (`presenceService.isOnline(ownerIdHex)`), display name, and a `--font-mono` microline `● self-sovereign` when the owner identity is minted and loaded. The settings gear does NOT move into the chip (ticket keeps it in the header). Hidden in the collapsed (narrow) icon rail, like the rest of the footer.
11. **"Network Viz" footer button stays** (functionality; design predates it) — restyled, not removed.

## 2. NavPanel restyle (`src/lib/components/NavPanel.svelte`, `NavNodeRow.svelte`, `NavTree.svelte`)

Region-by-region (current anchors from the 2026-07-06 exploration):

- **Header** (NavPanel 246–278): keep search input, divider, FAB, gear. Restyle: search input picks up `--surface` bg, placeholder stays `"Search"` (test-pinned); FAB stays `--accent`; gear `.active` → `--primary-soft`/`--primary-deep` (was `--accent`/`--text-primary`).
- **Notes row** (288–299): `.active` → `--primary-soft` bg / `--primary-deep` text.
- **Tree** (279–324): NavPanel partitions `filteredNodes` into the three groups of §1.2 and renders `<NavTree>` per group under `.nav-section-header` labels (uppercase, 10.5px, letter-spacing 0.1em, `--text-faint`). Search filtering runs before partitioning (unchanged `filteredNodes` derivation).
- **NavNodeRow**: `.nav-row.active` → `--primary-soft`/`--primary-deep` (was `--bg-tertiary`/`--text-primary`). Community `typeIcon` 🏛️ → letter chip (§1.4); `#`, `@`, folder chevrons unchanged. Presence dot, unread badges, color bands, brackets, folder controls untouched.
- **Proposals row**: new `ProposalsNavRow.svelte` rendered by `NavTree` after an expanded community's children, driven by two new optional props threaded like `presenceOnline`: `proposalCount?: (node: NavNode) => number | undefined` and `onSelectProposals?: (communityId: string) => void`, plus `proposalsActiveFor?: string` (community id whose Proposals view is open) for the active state. Indented to channel depth; keyboard-activatable (role="button", Enter/Space) matching NavNodeRow's pattern.
- **Footer** (328–366): mode toggles `.active` → `--primary-soft`/`--primary-deep`. Then (new) `IdentityChip`, then (new) `ConnectionStatusChip`. More-menu and Network Viz button unchanged structurally.

## 3. Proposal counts — `src/lib/proposal-count-service.ts` (new)

Mirrors the `presence-service.ts` shape (version counter + onChange):

- `connectAdapter(votingAdapter: VotingAdapter)`: subscribes `subscribeProposalCreated`, `subscribeThresholdReached`, `subscribeThresholdReverted`, `subscribeProposalFinalized` — each handler refetches the affected `communityId` only (payloads carry it).
- `ensure(communityId: string)`: lazy first fetch via `listTier2Proposals(cid)`; count = `filter(p => p.lifecycle === 'Open' || p.lifecycle === 'ThresholdReached').length` (DTO is snake_case: `p.lifecycle` on `Tier2ProposalExport`).
- `countFor(communityId): number | undefined` (undefined = never fetched → row renders without badge until known), `version` (bump per change) for Svelte invalidation, `onChange?: () => void`.
- App owns the singleton, calls `ensure()` for each top-level community node (communities are few; one IPC each, event-refreshed after), and threads `proposalCount` into NavPanel. AssemblyRail does NOT use this service (it holds full proposal lists itself); they stay consistent because both refetch on the same events.

## 4. Assembly rail

1. **`MessagesRail.svelte`** (new; mounted inside App's existing `mediaFeed` snippet, App.svelte ~3246): tab header (⚖ Assembly / Media) when the Assembly tab is available (§1.8), else renders MediaFeed directly with no tab chrome (today's DM/no-community experience is pixel-idempotent modulo tokens). Tab pref: `loadRailTab()/saveRailTab(tab)` added to `media-panel-prefs.ts` (key `harmony-rail-tab`, values `'assembly'|'media'`, default `'assembly'`, same try/catch-guarded idiom).
2. **`AssemblyRail.svelte`** (new): props `{ communityId, adapter, onViewAllProposals }`. Copies `CommunityProposalsPanel`'s proven lifecycle: `$effect` keyed on `communityId` — reset → fetch `listTier2Proposals` → subscribe created/threshold-reached/threshold-reverted/finalized filtered by `p.community_id === communityId` → cleanup (cancelled flag + unsubs); load-token race guard. Renders active proposals (`Open`/`ThresholdReached`, ThresholdReached first, then `total_conviction_ms` desc — BigInt compare of the decimal strings) as `ConvictionProposalCard`s (reused as-is — full signal-toggle works from the rail), an empty state ("No open proposals"), and a footer link `View all proposals →` calling `onViewAllProposals()`.
3. **Signal-cast flicker rule**: like CommunityProposalsPanel, do NOT refetch on `voting-tier2-signal-cast` (the card handles its own optimistic state).
4. **Deep-link seam**: `CommunityView.svelte`'s internal `activeView` state (:132) becomes a bindable prop (`let { activeView = $bindable('channels') } = $props()` pattern), so App can (a) drive it from the proposals nav row / rail's "View all", and (b) know it for the nav row's active state (§1.5). App resets it to `'channels'` when the selected community changes, preserving current UX.

## 5. Connection status — `src/lib/components/ConnectionStatusChip.svelte` (new)

Self-contained: `onMount` → `snapshot()` + `onNetworkHealthChanged` subscribe (mirroring `NetworkHealthView.svelte`'s destroyed-flag cleanup). Mapping:

| Condition | Text | Tokens (dot + text) |
| --- | --- | --- |
| `transportDisabledReason` set OR `reachability === 'unreachable'` | `● offline` | `--net-danger-*` |
| `reachability === 'degraded'` | `● degraded · N peers` | `--net-warn-*` |
| `reachability === 'reachable'` | `● connected · N peers` | `--net-ok-*` |

`N = peers.filter(p => p.connectionMode !== 'noConnection').length`. `--font-mono`, 10px, single line; `title` tooltip carries `transportDisabledReason` when present. Pre-snapshot state renders nothing (no flash of "offline" during boot).

## 6. Identity chip — `src/lib/components/IdentityChip.svelte` (new)

Props `{ displayName, ownerIdHex, selfOnline, selfSovereign }` (App already holds all four signals: `myProfile.displayName`, `selfOwnerId`, presence visibility, owner-gate state). **Implementation caveat:** `selfOnline` cannot come from `presenceService` — presence rosters never contain self (zenoh doesn't loop our own beacon), so App computes `selfOnline = presenceVisible && ownerIdentityState === 'present'` ("you appear online to others") instead. Initials avatar 30px (`--accent` bg, `--text-bright`), presence ring = 2px `--bg-secondary`-bordered `--presence-online` dot overlapping the avatar corner when `selfOnline`; name 600/13px `--text-primary`; microline `● self-sovereign` (`--font-mono` 10px, `--presence-online`) when `selfSovereign`. Pure presentational — no service subscriptions. App computes `selfSovereign = ownerIdentityState === 'present'` (`OwnerIdentityState = 'unknown' | 'present' | 'missing' | 'error'`, `src/lib/owner-gate.ts:21`).

## 7. Out of scope

- Global search backend (ticket non-goal); window-chrome/custom titlebar work; the design's traffic-light styling.
- Tier-3/Town Hall nav row and rail content (ZEB-612); Tier-1 polls in rail/badge.
- `--on-accent` contrast token work (ZEB-644) — new chips reuse the existing `--accent`+`--text-bright` idiom and inherit whatever ZEB-644 decides.
- Assembly rail outside messages mode (the right column only exists in messages mode; cross-mode rail would rebuild every mode's grid).
- Moving the settings gear into the identity chip; removing color bands or Network Viz.

## 8. Test impact (lockstep updates, no contract breaks)

- `NavPanel.test.ts`: 🏛️ textContent assertion (:404) → letter-chip assertion; new assertions for section headers, proposals row (badge count, click callback, zero-count no-badge), identity chip, status strip presence. Pinned contracts kept: placeholder `"Search"`, `Create new`, gear name/`aria-pressed`/`.active`, More-menu testids, mode-button names, empty-state text.
- `NavNodeRow.test.ts`: community typeIcon case → chip; `.color-band`/`.nav-presence-dot`/badge classes unchanged.
- `NavTree.test.ts`: unchanged (partitioning happens in NavPanel above it); new NavTree pass-through props covered via NavPanel tests.
- `Layout.test.ts`: untouched (Layout unmodified).
- `CommunityView` tests: `activeView` bindable — existing tab-click tests must still pass; add one test for external drive.
- New test files: `proposal-count-service.test.ts` (multi-handler mock à la `voting-adapter-tier3.test.ts` — `createMockAdapter`'s single handler slot is insufficient for VotingAdapter), `AssemblyRail.test.ts` (fetch/order/empty/view-all/race), `MessagesRail.test.ts` (tab gating + persistence), `ConnectionStatusChip.test.ts` (three states + peer filter), `IdentityChip.test.ts`, `ProposalsNavRow` covered via NavPanel tests.
- `media-panel-prefs` tests (if present) extend for the rail-tab key.

## 9. Risks / notes

- **DTO casing trap**: Tier-2 exports are snake_case (`proposal_id`, `community_id`, `lifecycle`, `total_conviction_ms` as decimal string). Any camelCase access fails silently (undefined). Tests must use realistic fixtures.
- **NavPanel prop surface grows** (proposalCount/onSelectProposals/proposalsActiveFor + identity/status). All optional — NetworkApp does not mount NavPanel, but tests construct it bare; defaults must keep bare construction rendering.
- **Section partitioning changes DOM order** when top-level nodes are heterogeneous; NavPanel search/filter tests use community fixtures and assert per-node text, not global order — verify during implementation.
- Bot-cadence note for the PR: same converge protocol as #407 (Qodo re-reviews every push; CodeRabbit one pass at open; Greptile only if Jake triggers).
