# ZEB-536 Spec 2: Message reactions — frontend UI — design spec

**Status:** approved direction (Jake, 2026-06-22) — hover quick-react + picker grid; reactor tooltip in v1.
**Ticket:** [ZEB-536](https://linear.app/zeblith/issue/ZEB-536) (Spec 2 of 3 — follows Spec 1 backend, PR #314). Parent epic [ZEB-533](https://linear.app/zeblith/issue/ZEB-533).
**Branch:** `zeb-536-spec2-reactions-ui` off `zeb-536-message-reactions@b6faae9c` (the Spec-1 backend tip). Stacked PR on #314; rebase onto `main` when #314 lands.

> **Scope.** Spec 2 is the **Svelte/Tauri frontend** for reactions, on top of the complete Spec-1 backend. Spec 3 (custom/hosted emoji via CAS, full Unicode picker) stays out of scope. Symbol/line references are indicative; the implementation plan pins them.

---

## Goal

Surface reactions in the desktop UI: render each message's reaction chips (emoji + count, highlighted when mine, click-to-toggle), let a member react via a hover toolbar (one-click 👍/👎 quick-react + a small picker-grid popover for the rest), show who reacted on hover, and keep counts live via the `channel-reaction-received` event. Built to match the existing Discord-like chat surface; small and iterative (we'll feel out the picker grid in practice and can trim toward quick-react-only if it's bloated).

---

## Background — what the frontend already has

Spec 1 already shipped the TS-facing contract; nothing backend changes here.

- **`ChannelMessageDto.reactions?`** (`src/lib/channel-message-service.ts:30-34`): `{ emoji: string; count: number; mine: boolean; reactors: string[] }[]` — already on the DTO type; populated by `list_channel_messages`.
- **`ChannelMessageService`** (`src/lib/channel-message-service.ts`): per-channel HLC-sorted cache (`byChannel`), dedupe-by-`messageId` (`ingest`, `seenIds`), `connectAdapter` (installs the `channel-message-received` + `channel-backfill-progress` listeners), `subscribeToChannel` (per-channel callbacks), `getMessages` (snapshot), `postMessage`/`listMessages`/`requestBackfill` facades. The component subscribes and re-reads `getMessages` on each notification.
- **`TauriAdapter`** (`src/lib/zenoh-service.ts:1-5`): `invoke(cmd, args)` + `listen(event, handler)` — the IPC seam; tests inject a mock.
- **`ZenohService.ownAddress`** (`src/lib/zenoh-service.ts:51`): the local owner address, already used to filter our own published profile. Source of `selfOwnerId` for computing `mine` on live events.
- **`ZenohService.peerProfiles`** (`address → ProfilePayload{displayName,…}`): resolves owner addresses to display names (Koya/Ildwyn/AVALON) — the source for the reactor tooltip.
- **`ChannelMessageFeed.svelte`**: renders the feed; each message is an `<article class="channel-message">` with a header + body; a passive `.channel-message:hover` background already exists (no action toolbar yet). `app.css` holds the Discord-like CSS vars (`--bg-secondary`, `--bg-tertiary`, `--accent`, `--border`, `--text-*`).

**Reactions don't ride the message path.** `channel-message-received` *adds* a message (dedup by id); a reaction *mutates* an existing message's `reactions`. So this needs a distinct event listener + an in-place update, not the `ingest` path.

---

## Design

### Component 1 — service: live reaction application (`channel-message-service.ts`)

**New event listener** in `connectAdapter`:

```ts
const unlistenReaction = await adapter.listen('channel-reaction-received', (event) => {
  const p = event.payload as ChannelReactionReceivedPayload; // {communityId, channelId, messageId, reactor, emoji, add, at}
  this.applyReaction(p);
});
this.unlisteners.push(unlistenReaction);
```

**`applyReaction(p)`** — find the cached message in `byChannel[chKey]` by `messageId`; if absent, drop (the message isn't loaded; `list` will carry the materialized reactions when it loads). Otherwise mutate `message.reactions` (initialize to `[]` if undefined):
- locate the entry for `p.emoji`;
- if `p.add`: ensure the entry exists and add `p.reactor` to `reactors` (set semantics — no dup);
- if `!p.add`: remove `p.reactor` from `reactors`; if `reactors` is now empty, remove the emoji entry;
- recompute `count = reactors.length` and `mine = reactors.includes(this.selfOwnerId)`.
Then notify the channel's subscribers (same fan-out `ingest` uses) so the feed re-renders.

**`selfOwnerId`** — set on the service at connect time from `ZenohService.ownAddress` (confirm the two are the same owner-address hex space during planning; if not, derive `selfOwnerId` from `get_owner_state`/the owner card). Needed only to compute `mine` for live events; `list` already supplies authoritative `mine`.

**`reactToMessage(communityId, channelId, messageId, emoji, add)`** facade → `adapter.invoke('set_message_reaction', { communityId, channelId, messageId, emoji, add })`.

**Convergence note.** Live apply is a plain set add/remove, not a re-fetch (rejected alternative: debounce-refetch the channel on every reaction — wasteful). `list_channel_messages.reactions` is authoritative and reseeds on channel open, so any drift from out-of-order events self-heals. Strict frontend LWW-by-HLC (tracking per-(reactor,emoji) `at`) is a **follow-up**, not v1 — reactions are low-frequency and the backend already orders them.

`destroy()` unwinds the new listener with the others.

### Component 2 — feed UI (`ChannelMessageFeed.svelte`)

**Reaction chips** under each message body: one pill per `message.reactions` entry, `emoji` + `count`, `class:mine={r.mine}` (accent fill when mine). **Click toggles**: `reactToMessage(emoji, !r.mine)`. **Hover** a chip → a tooltip listing reactors, each resolved via `peerProfiles[addr]?.displayName ?? shortHex(addr)`.

**Hover toolbar** on `.channel-message:hover` — a small floating action bar (top-right of the message): inline quick-react **👍** and **👎**, plus a **😊/＋** button that opens the picker. Quick-react buttons **toggle** (same semantics as clicking a chip): `reactToMessage(emoji, !alreadyMine)` where `alreadyMine` is whether that emoji is in the message's reactions with `mine === true`; a quick button reflects that state (highlighted when you've reacted with it). Selecting from the picker is an add (`reactToMessage(emoji, true)`).

**Picker popover** — a small **fixed grid** of the palette (no search, no emoji-picker library): click an emoji → react + close; click-outside and `Esc` close; positioned anchored to the toolbar button. Keyboard-focusable buttons.

**Palette (v1):**
- quick-react (inline): `👍 👎`
- picker grid: `👍 👎 ✅ ❌ 👀 🎉 🙏 🚀 ❤️ 😄`

(We can trim the grid toward quick-react-only later if it feels bloated — the grid is a const array.)

**Teardown safety:** reaction updates arrive via the existing channel subscription; any `await` resume point added in the component (e.g. resolving `reactToMessage`) is guarded against post-teardown state writes (per the project's Svelte teardown rule).

### Component 3 — styling (`app.css` + component `<style>`)

Reuse the existing CSS vars — no new color system:
- chips: rounded pills, `--bg-secondary` / `1px --border`; when `.mine`, `--accent` border + tinted fill; hover lightens to `--bg-tertiary`.
- hover toolbar: floating bar, `--bg-secondary`, subtle shadow/border, small icon buttons (`--text-secondary` → `--accent` on hover).
- picker popover: `--bg-secondary` card, grid of emoji buttons, `--border`, light elevation.
- reactor tooltip: small `--bg-tertiary` tooltip with the resolved names.

---

## Testing

- **Service unit tests** (vitest, the existing mock-`TauriAdapter` pattern): `applyReaction` add → chip appears with `count=1`/correct `mine`; second reactor → `count=2`; remove → decrement / entry drops at zero; `mine` tracks `selfOwnerId`; event for an unloaded message is a no-op; `reactToMessage` invokes `set_message_reaction` with exact camelCase args; `destroy` removes the listener.
- **Component interaction tests** *if* the harness supports Svelte component testing — confirm `@testing-library/svelte` (or the repo's existing component-test approach) during planning; if present, cover chip render + click-toggle, hover toolbar quick-react, picker open/select/close, reactor tooltip resolution. If the harness has no component-test path, scope to the service tests + a manual `tauri dev` smoke check and say so.
- **Gate:** `npx tsc --noEmit` + `npx vitest run` (repo root) clean.

## Manual / fleet validation

- `tauri dev` smoke-look on AVALON: react via quick button + picker, see the chip toggle, hover the chip for reactors.
- Two-party live test with **Ildwyn** once stacked: AVALON reacts → Ildwyn's chip count updates live via `channel-reaction-received` (and vice-versa); toggle off converges.

## Scope, branch, dev

- Stacked on Spec 1: branch `zeb-536-spec2-reactions-ui` off `zeb-536-message-reactions`; its own PR on top of #314; rebase onto `main` when #314 merges.
- **Exercises AVALON's frontend toolchain** (Vite / `tsc` / `vitest` / `tauri dev`) — the JS-side counterpart to Spec 1's Rust build exercise; surface any speed bumps. (Note: the fleet `serve` node can stay up during frontend work — Vite/tsc/vitest don't relink `harmony-app.exe`.)

## Non-goals (v1)

- No custom/hosted emoji and no full-Unicode picker (Spec 3).
- No strict frontend LWW-by-HLC (reseed-on-open covers drift; follow-up).
- No emoji search, skin-tone variants, or recently-used tracking.
- No backend changes — Spec 1 is the contract.

## Open questions

1. **`selfOwnerId` source** — confirm `ZenohService.ownAddress` is the same owner-address hex as `reactions[].reactors` / the `reactor` event field. If not, derive from `get_owner_state`. (Resolve in planning; doesn't change the design shape.)
2. Component-test harness availability (`@testing-library/svelte`) — determines whether component interaction is unit-tested or smoke-tested. (Resolve in planning.)
