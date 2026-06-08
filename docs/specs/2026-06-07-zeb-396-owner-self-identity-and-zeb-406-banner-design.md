# ZEB-396 (owner self-power) + ZEB-406 (backup-banner overlay) — design

**Status:** approved 2026-06-07 (Jake). One PR, branched off `main` (post-#203).
**Tickets:** ZEB-396 (P2, owner can't moderate own community), ZEB-406 (P2, banner overlay clips/intercepts). Subsumes the surfaced symptom of ZEB-394 (no discoverable way to create a channel).

## Goal

A freshly-minted community **owner** can immediately use every moderation affordance (create / rename / delete channel, kick, set-power), and an **un-backed-up** user can click the top toolbar (search, "+") without the backup banner blocking them or clipping page headers.

## Background — why "owner sees nothing" (root cause, proven live on Ildwyn)

The community world is keyed by **owner_id**; the materialized roster (`listCommunityMembers` → `MemberInfoDto.addr`), channel-message authors, and power levels are all owner_id-keyed. But `App.svelte` derives the viewer's self-identity for the community world from `myAddress`, which is set from `get_node_addr` — the **node/transport address**, a different identity.

Live values (fresh owner-minted community): `myAddress = a888ba9e…02a8` (node addr) vs `selfOwnerId = cb7026bb…f857` (owner_id); the owner's own roster row is keyed `cb7026bb…` and renders as `admin`.

`App.svelte:870`:
```js
let myCommunityPower = $derived(
  communityMembers.find((m) => m.address === myAddress)?.power ?? 0,
);
```
matches an owner_id-keyed roster against a node address → never matches → `?? 0` → owner power reads 0 → `canModerate` (`myPower >= POWER_THRESHOLDS.kick`) is false everywhere → all moderation affordances (incl. the already-wired `.create-channel-btn`) are hidden. Confirmed: on a fresh community `.create-channel-btn` does not render although the members panel labels my row `admin`.

## Design

### Part A — ZEB-396: thread `selfOwnerId` as the community-world self-identity

`selfOwnerId` already exists in `App.svelte` (`$state<string|null>`, set from `get_owner_state.ownerId`). It is the correct self-key for the community world. Two App-level edits; everything downstream inherits via `CommunityView`'s props:

1. `App.svelte` `myCommunityPower` (≈ line 870): match `selfOwnerId` instead of `myAddress`.
   ```js
   let myCommunityPower = $derived(
     selfOwnerId
       ? (communityMembers.find((m) => m.address === selfOwnerId)?.power ?? 0)
       : 0,
   );
   ```
2. `App.svelte` `<CommunityView>` (≈ line 2522): `ownAddress={selfOwnerId ?? ''}` instead of `ownAddress={myAddress}`.

Downstream community consumers receive the corrected identity transitively and need no per-call change (but each is covered by a test):
- `ChannelMembersPanel` — self-highlight (`.self`) + self-sort-to-top.
- `CommunitySettingsPanel` — `myAddress` prop drives "(you)" marker (line 345) and self-power moderation gating (line 140).
- `CommunityMembersPanel` — `ownAddress` drives `viewerPower` (line 85) and the last-admin guard (line 91).
- `ChannelMessageFeed` — `ownAddress` drives own-message detection (`author === ownAddress`, line 307; authors are owner_id-keyed).
- `Tier3ProposalPanel` / `CommunityProposalsPanel` — `myAddr={ownAddress}` voting self-identity.

**Explicitly left as `myAddress` (node addr — correct there):** VineFeed (`ownAddress`, 2713), ProfilePopover (`ownAddress`, 2881), and the `messageService` / `vineService` / `navService` `ownAddress` assignments (DM / vine / nav / Reticulum-profile / network worlds).

**Load-order:** `selfOwnerId` resolves asynchronously after `start_node`. Because `myCommunityPower` is a `$derived` and `selfOwnerId` is `$state`, the gate recomputes automatically when the owner_id arrives; the brief pre-load window correctly yields power 0 (same transient as today).

### Part B — ZEB-406: put the backup banner in normal flow

Today (`App.svelte`): the banner is a sibling rendered after `<Layout>` as `.backup-banner-overlay { position: fixed; top:0; left:0; right:0; z-index:40 }`, floating over the top strip — clipping page headers and intercepting pointer events on the top toolbar (search + `.fab-btn` "+").

Fix: render the banner in document flow above the layout so it reserves its own height. Wrap `<Layout>` and the banner in a flex-column app-shell with the banner first; have `.layout` fill the remaining height instead of overlaying:
```svelte
<div class="app-shell">
  {#if ownerIdentityState === 'present'}
    <BackupReminderBanner />
  {/if}
  <Layout …> … </Layout>
</div>
```
```css
.app-shell { display: flex; flex-direction: column; height: 100vh; }
```
`Layout.svelte`'s `.layout` changes from a fixed viewport height to filling the shell (`flex: 1; min-height: 0;`, or `height: 100%`). The fixed-position overlay modals/popovers (call session, profile/card popovers, dialogs) remain separate top-level roots — they are `position: fixed` and unaffected. `BackupReminderBanner` self-gates its own visibility, so when not shown it collapses to zero height and the layout is pixel-identical to today.

## Files

- `src/App.svelte` — `myCommunityPower` derivation; `CommunityView ownAddress`; app-shell wrapper + remove `.backup-banner-overlay` overlay CSS.
- `src/lib/components/Layout.svelte` — `.layout` height model (fill shell instead of `100vh`).
- Tests: `src/lib/components/__tests__/CommunityView.test.ts` (or App-level test) for self-power; `CommunitySettingsPanel` "(you)"/self-power; a banner-in-flow assertion. New/extended as needed.

## Testing

TDD per change.
- **Unit/component (vitest + @testing-library/svelte):**
  - `myCommunityPower`: with a roster row keyed by `selfOwnerId` at power 100, the create/moderation gate is enabled; with no matching row, it is 0.
  - `CommunitySettingsPanel`: owner row shows "(you)" and self-power controls when `myAddress`(prop)=`selfOwnerId`.
  - Banner renders as a flow child above `.layout` (not a fixed overlay) when visible; absent → layout unchanged.
- **Live (Playwright on Ildwyn, existing `ZEB394 Probe` community):**
  - `.create-channel-btn` flips **false → true** for the owner after the fix.
  - `.fab-btn` is clickable **without** dismissing the banner; no header clipping at the top of Notes / Network / Mail.

## Risks / coordination

- `App.svelte` is also edited by Koya's PR #202 (ZEB-404), but in the roster-refresh region, not the self-power keying or the banner/app-shell — a clean 3-way merge is expected; rebase if needed.
- Swapping `CommunityView ownAddress` to owner_id affects voting (`myAddr`) and own-message detection; both are owner_id-keyed, but each is covered by a test and a live check.

## Out of scope

- ZEB-394 discoverability polish (promoting the buried "Create channel" button to a header "+") — defer; once the gate works the existing affordance is reachable. Can be a fast follow-up.
- Any backend change: the backend already enforces `create_channel` at power ≥ 50 correctly; this is purely a frontend self-identity fix.
