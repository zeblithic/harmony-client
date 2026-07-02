# ADOPTION.md — phase-by-phase plan

A pragmatic order for landing "Commons" in harmony-client without a big-bang
rewrite. Each phase is independently shippable and visibly improves the app.

---

## Phase 0 — Tokens & type (the instant reskin)

**Goal:** the entire app becomes Commons (light + dark) with zero component logic changes.

1. Replace the `:root { … }` block in `src/app.css` with the contents of `tokens.css`.
   - It reuses every existing variable name (`--bg-primary`, `--accent`, `--danger`, `--border`, `--bg-hover`, …), so all current components inherit the new palette immediately.
   - Update the `font-family` on `:root` to `var(--font-ui)` (Public Sans) — already set in `tokens.css`.
2. Add the Google Fonts `<link>`s (in `tokens.css` header comment) to `index.html`. For offline/Tauri, self-host the three families instead.
3. Add a **theme toggle**: a tiny store that sets `document.documentElement.dataset.theme = 'dark' | ''` and persists to localStorage; surface it in Settings → Appearance. (Or default to `@media (prefers-color-scheme)` by changing the dark selector.)
4. Sweep scoped `<style>` blocks for **hard-coded** Discord hexes (`#5865f2`, `#1e1f22`, `#2b2d31`, `#313338`, `#f23f42`, etc.) and replace with the matching `var(--…)`. Grep: `#5865f2|#1e1f22|#2b2d31|#313338|#36393f|#f23f42`.

**Done when:** the app looks Commons in both themes and nothing references a raw Discord hex.

---

## Phase 1 — Shell & nav polish + Assembly rail

**Files:** `Layout.svelte`, `NavPanel.svelte`, `App.svelte`
**Reference:** `Harmony Desktop.dc.html` frame 1.

- `NavPanel.svelte`: tighten the header (search + `＋` FAB + settings), the unified community→channel tree (community row expands in place), the pinned `📝 Notes` row (already present), and the footer mode switcher. Active channel row uses `--primary-soft` bg + `--primary-deep` text; the `⚖ proposals` row uses `--gov-clay` with a count badge.
- `Layout.svelte`: keep the existing 3-column grid and the resizable/collapsible right column (`media-area` + `media-resizer` + `media-rail`). **Repurpose / add** the right column in messages mode as the **Assembly rail** (live proposal cards). Reuse `media-panel-prefs.ts` for its width/open state.
- Window chrome: the centered global search + `● connected · N peers` indicator (wire to the existing connectivity adapter).

**Done when:** the shell matches frame 1 and the Assembly rail shows live proposals and collapses cleanly.

---

## Phase 2 — Governance components (the heart)

**Files:** `CommunityProposalsPanel.svelte`, `ConvictionProposalCard.svelte`, `Tier3ProposalPanel.svelte`, `DelegationWidget.svelte`, `DelegationGraph.svelte`
**References:** Design System (governance section), `Harmony Vote Flow.dc.html`, Desktop frames 2–4.

- **Proposal card** (`ConvictionProposalCard.svelte`): rebuild to the Commons anatomy — header (ID pill `--gov-clay` + status pill + ⏳ timer), Public Sans 600 title, 3-segment tally bar on `--tally-track`, mono counts/percentages, quorum + conviction chips, vote buttons (`▲ for` filled `--vote-for`, `▼ against` outline `--vote-against`, `— abstain` outline muted), proxied-to footer with **Recall**.
- **Ballot / detail** (`CommunityProposalsPanel.svelte` detail state): the centered doc column + right vote panel from Desktop frame 2, incl. the "On the record" cryptographic-trust block.
- **Vote interactions**: match `Harmony Vote Flow.dc.html` exactly (cast → live tally + animated bars + "signed with your key" toast; delegate sheet; recall). Wire to `votingAdapter`.
- **Delegation** (`DelegationWidget.svelte` / `DelegationGraph.svelte`): the per-topic `me → delegate` cards (Change/Recall), the "Your standing" card, and the node graph from Desktop frame 4.

**Status pill tokens:** drafting `--status-drafting-*`, open `--status-open-*`, passed `--status-passed-*`, failed `--status-failed-*`, recalled `--status-recalled-*`.

---

## Phase 3 — Charter view (new)

**Files:** `CommunityView.svelte` (+ new `CharterView.svelte`)
**Reference:** `Harmony Charter & Settings.dc.html` frame A.

- Extend `CommunityView`'s `activeView` union to `'channels' | 'proposals' | 'tier3' | 'charter'` and add a `📜 Charter` tab/nav entry.
- Build `CharterView.svelte` to **generate** the constitution from existing data: roles from `powerToRole` + `POWER_THRESHOLDS`, the admin quorum from the community's governance state, the proposal tiers from your Tier-2/Tier-3 machinery. The supermajority/quorum percentages in the mock are placeholder defaults — surface them as real per-community charter fields.
- "Propose amendment" opens a Tier-3 proposal flow.

**Done when:** every community has a readable, accurate, amendable charter page.

---

## Phase 4 — Community settings + dialogs

**Files:** `CommunitySettingsPanel.svelte`, `ChangeQuorumDialog.svelte`, `SetPowerDialog`
**Reference:** `Harmony Charter & Settings.dc.html` frames B & C.

- Restyle the existing sections (Info, Public profile, Members, Invites, Admin governance, Forks, Danger zone) to Commons; no structural change required.
- **Set role dialog**: add the MEMBER/MOD/ADMIN band visualization to the power slider; keep the existing cross-admin-threshold `ConfirmationModal`.
- **ChangeQuorumDialog**: restyle; keep the "N+1 for survivability" copy and the self-referential note (the change needs current quorum).

---

## Phase 5 — Fork & lineage polish

**Files:** `ForkLineageTree.svelte`, `ChannelMessageFeed.svelte` (+ `fork-timeline.ts`), fork dialog
**Reference:** `Harmony Forks & Lineage.dc.html`.

- **Lineage tree** (`ForkLineageTree.svelte`): node cards + connectors (sage = amicable, clay = dispute), "You are here", ✓ Member badges, clickable for locally-known communities (you already gate this via `localNavIds`).
- **Fork divider**: style the `ForkDivider` row that `buildUnifiedTimeline()` already inserts in `ChannelMessageFeed.svelte` — the clay band with "Forked from … · N messages carried", carried history tinted/read-only above it.
- **Fork dialog**: name + carry-over checklist (history snapshot required) + mandatory "why".

---

## Phase 6 — Onboarding & identity/backup

**Files:** `App.svelte` (onboarding/owner-identity states), `DevicesPanel.svelte`, `BackupReminderBanner.svelte`
**Reference:** `Harmony Onboarding.dc.html`.

- Restyle the mint → **back up** → redeem-invite flow to the 5-step wizard; the backup step is the one "clay" moment (recovery phrase + encrypted file + keychain).
- **Identity & devices** (`DevicesPanel.svelte`): DID + self-sovereign badge, backup status, key rotation, linked devices (revoke), danger zone.
- `BackupReminderBanner.svelte`: the gentle, dismissible clay banner (never modal).

---

## Phase 7 — Mobile (greenfield)

**Reference:** `Harmony Mobile.dc.html` + `Harmony Vote Flow.dc.html`.

No mobile client exists yet — treat this as the design target when you build one.
Key IA: bottom tabs **Chat · Assembly · Activity · You**, a swipe-in Spaces
drawer reusing the same community→channel model, and a thumb-zone sticky vote
bar on the ballot. Layout.svelte's existing `collapsed` responsive mode is the
seed for a narrow breakpoint.

---

## Phase 8 — Town Hall & voice

**Files:** `VoiceChannelView.svelte` (+ new `TownHallView.svelte`)
**Reference:** `Harmony Town Hall.dc.html` (screens `11-town-hall.png`).

- **Voice channel** (`VoiceChannelView.svelte`): a faithful restyle of the
  shipped component — keep the join-muted flow, the Mute / PTT / Deafen / Leave
  control bar, the avatar-grid stage (≤12) → list collapse, speaking rings, and
  the power-gated mod controls (mute / remove-with-confirm). Restyle to Commons:
  speaking ring = `--accent`, mod-muted glyph on `--status-recalled-bg`, control
  bar on `--bg-secondary`. The four real states (join pane, PTT-held,
  self-mod-muted, channel-full soft-cap) are all in frame C — match them.
- **Town Hall** (new `TownHallView.svelte`) is the distinctive surface: voice
  fused with the Assembly. It composes the voice session with governance state —
  an active-speaker spotlight, the in-room avatar grid, a **speaker queue** mods
  invite from (raise-hand → queue → invite-to-speak), a backchannel chat, and a
  **"Call this to a motion"** card. The motion is **quorum-aware**: if enough
  members are present it can be voted live; otherwise it opens as an async
  proposal (reuse the Phase-2 proposal flow). This is net-new UI over existing
  voice + proposal services — no new backend.

## Phase 9 — Vines & Files (the content feeds)

**Files:** `VineFeed.svelte`, `VineCard`, `VinePlayer`, `VinePublishDialog.svelte`, `FileBrowser.svelte`, `FileDetailPanel.svelte`
**References:** `Harmony Vines & Files.dc.html` (static, `12-vines-files.png`) and **`Harmony Vines Feed.dc.html`** (the interactive spec, `13-vines-feed-interactive.png`).

- **Thesis:** one content-addressed store, many feeds = lenses filtered by media
  type. Ship **Vines** (video ≤6s, loops) first; Gallery (images) and Posts
  (text) reuse the same feed engine, follows, reshare, and store later. Keep the
  feed-type switch in the nav (`🎞 Vines` active, `🖼 Gallery` / `✍ Posts` as
  "soon").
- **Vines feed** (`VineFeed.svelte`) — restyle faithfully: Following / Discover
  tabs, All / Unviewed filter, "N new", looping thumbnails with duration + ↻,
  reshare-with-attribution ("view original by @kit"), reactions + loop counts,
  unviewed dot / viewed-dim.
- **Interactive behavior — match `Harmony Vines Feed.dc.html` exactly:**
  - **Endless, auto-playing feed**: vertical scroll-snap; the loop snapped to
    center auto-plays (gradient/video plays + a 6s loop-ring), others pause/dim.
    Implement autoplay with an IntersectionObserver (or a center-distance scroll
    handler) driving a single `playingId`; pause all others. Append more as the
    user nears the bottom.
  - **Discover = transitive follows, not an algorithm** (a NEW model — today's
    `discoverVines` is a flat list): build Discover from **2nd- and 3rd-degree**
    follows only (capped at 3rd for scalability). Each card shows a **degree
    chip** (2nd/3rd) and **provenance** — the exact path, e.g. "Mara follows
    @kit" or "Priya → @lena → @iris".
  - **Tunable**: degree chips toggle 2nd/3rd; a **Tune** sheet lists your direct
    follows with a per-follow switch to mute their propagation into Discover
    (muting recomputes the feed live). This needs `VineService` to expose the
    follow graph 2–3 hops out, plus a per-source mute set (persist locally).
- **Publish** (`VinePublishDialog.svelte`): pick/trim-to-6s, caption, the
  sovereign "only you can delete it" note; ingests to the content store and
  returns a CID (the backend-owned picker already exists).
- **Files** (`FileBrowser.svelte` / `FileDetailPanel.svelte`): the store made
  browsable — every artifact with its CID, size, type, what uses it, and
  **replication health** (×N healthy / at-risk, against the replication target).
  Surface the "storage buddies" model as a contribution meter. This is a restyle
  of existing surfaces; the replication + storage-budget data already exist.

---

## Guardrails (from the codebase's own CLAUDE.md)

- Tauri IPC: Rust params are `snake_case`, JS callers `camelCase` — the IPC layer converts. Get it wrong and the value arrives `undefined`.
- Run the frontend gates before pushing: `npx tsc --noEmit` and `npx vitest run`.
- Keep changes scoped per phase; each phase is independently reviewable and shippable.
