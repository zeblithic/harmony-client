# Handoff: Harmony "Commons" design system → harmony-client

## Overview

This package adapts **harmony-client** (Tauri 2 + Svelte 5) from its current
out-of-the-box Discord-style dark theme to **"Commons"** — a warm, civic design
system built specifically for Harmony's mission: communities that *form, grow,
and govern their commons*. It restyles the existing shell, elevates the
governance surfaces (proposals, delegation, conviction, quorum, fork/lineage),
and introduces a member-facing **Charter** view plus a self-sovereign onboarding
flow and a first-pass mobile layout.

The design leans into what makes Harmony unlike Discord/Slack: liquid-democracy
voting, exit-via-fork with history intact, self-sovereign identity, and rules
that are legible and amendable by the people they bind.

## About the design files

The files in `references/` are **design references created in HTML** (Design
Components — `*.dc.html`, rendered by `support.js`). They are prototypes that
show the intended **look, layout, and behavior** — they are **not** production
code to copy verbatim.

Your task is to **recreate these designs inside harmony-client's existing
Svelte 5 + Tauri environment**, using its established patterns: Svelte
components, `$state`/`$derived` runes, the Tauri IPC adapter, and CSS custom
properties in `src/app.css`. Where this doc gives exact hex/spacing/type values,
match them; where it describes structure, fit it to the components you already
have. The fastest, lowest-risk win (Phase 0) is purely a token swap — no
component logic changes at all.

To view a reference: open the design project these came from, or render a
`*.dc.html` next to `support.js`. They will not render standalone without
`support.js` (included here).

## Fidelity

**High-fidelity.** Final colors, typography, spacing, radii, shadows, and
interaction behavior are specified. Recreate pixel-faithfully using the
codebase's own components and styling layer (`app.css` variables + scoped
Svelte `<style>`). The one exception is the **mobile** set, which is a
greenfield direction (no mobile client exists yet) — treat it as a hi-fi target
for whatever mobile surface you build next, not a literal spec for today.

## Adoption at a glance — recommended order

| Phase | Scope | Risk | Files |
|---|---|---|---|
| **0** | Tokens + type (instant reskin) | very low | `src/app.css`, `index.html`, new theme store |
| **1** | Shell & nav polish + Assembly rail | low | `Layout.svelte`, `NavPanel.svelte`, `App.svelte` |
| **2** | Governance components (proposal card, ballot, delegation, vote flow) | med | `CommunityProposalsPanel.svelte`, `ConvictionProposalCard.svelte`, `Tier3ProposalPanel.svelte`, `DelegationWidget.svelte`, `DelegationGraph.svelte` |
| **3** | Charter view (new) | med | `CommunityView.svelte` (+ new `CharterView.svelte`) |
| **4** | Community settings + role/quorum dialogs | low | `CommunitySettingsPanel.svelte`, `ChangeQuorumDialog.svelte`, `SetPowerDialog` |
| **5** | Fork & lineage polish | med | `ForkLineageTree.svelte`, `ChannelMessageFeed.svelte` + `fork-timeline.ts`, fork dialog |
| **6** | Onboarding & identity/backup | med | `App.svelte` onboarding states, `DevicesPanel.svelte`, `BackupReminderBanner.svelte` |
| **7** | Mobile layout (greenfield) | high | new responsive layer / mobile target |
| **8** | Town Hall & voice | med | `VoiceChannelView.svelte` (+ new `TownHallView.svelte`) |
| **9** | Vines & Files (content feeds) | med | `VineFeed.svelte`, `VinePlayer`, `VinePublishDialog.svelte`, `FileBrowser.svelte`, `FileDetailPanel.svelte` |

See **ADOPTION.md** for the detailed, file-by-file plan. (Phases 8–9 cover the
side-modes. **Mint is intentionally out of scope** — it gets its own session
when Bezelbaum / FinanceStudio integration — "ReplaceMint" — is designed.)

---

## Design tokens

All tokens are in **`tokens.css`** — a drop-in replacement for the `:root` block
in `src/app.css`. It deliberately **keeps every variable name the codebase
already uses** (`--bg-primary`, `--accent`, `--danger`, `--border`, …) so
pasting it in reskins the whole app with no component edits, and **adds** new
semantic tokens (governance colors, fonts, radii, elevation) and a warm
**dark** theme under `:root[data-theme="dark"]`.

Headline values (light):

- **Paper / app bg** `#f4f1ea` · **surface (rails)** `#efeadf` · **raised (feed/cards)** `#fbf9f4` / `#ffffff`
- **Ink** `#20241c` · **ink-2** `#4b4f44` · **muted** `#767a6c` · **line** `#e3ddcf`
- **Primary (consensus green)** `#466b4c` (deep `#2f4a35`, soft `#e4ece2`)
- **Governance clay** `#b9742c` (soft `#f1e2cc`) — open proposals, deadlines, "act now"
- **Dissent red** `#b1402f`
- **Vote colors**: for `#466b4c` · against `#b1402f` · abstain `#cdc6b4` on track `#eadfc8`

Type:

- **Display / community names** — Newsreader (serif), weight 500, 28–44px
- **UI** — Public Sans (the U.S. government's UI typeface; "civic" without a word), 400/600/700, body 14px / 1.55
- **Data / IDs / tallies / timestamps** — IBM Plex Mono, 500, 11–13px

Shape & elevation: radii 3 (chips/bars) / 5 (inputs/rows) / 8 (cards) / pill;
shadows `e1 0 1px 3px rgba(40,30,10,.07)`, `e2 0 2px 10px rgba(40,30,10,.10)`,
`e3 0 8px 28px rgba(40,30,10,.16)`. Spacing on a 4px base.

The full, annotated reference is **`references/Harmony Design System.dc.html`**.

---

## Screens / views

Each entry names the design, what it's for, and the **reference file** to open
for pixel-level detail.

### 1. Core shell — `references/Harmony Desktop.dc.html` (frame 1)
- **Purpose**: the 90%-of-the-time view — read & chat with governance one glance away.
- **Layout**: 3-column CSS grid `260px | 1fr | 330px` = unified nav · channel feed · **Assembly rail**. Top window chrome (38px) with centered global search and a `● connected · N peers` status.
- **Nav** (`--bg-secondary`): logo + `＋` create; a single tree where the community row expands to channels in place (no Discord double-rail); `📝 Notes` pinned row; DMs; footer mode switcher (Messages / Vines / Files / `···`); identity chip showing `● self-sovereign`.
- **Channel** (`--bg-primary`): header `# general` + topic + member count; message rows (36px round avatar, name + verified `✓` chip + mono timestamp, body 14/1.55); inline proposal reference cards; compose bar (raised, pill radius `8px`, `＋ … ⚖ ☺`).
- **Assembly rail** (`--bg-secondary`): "The Assembly" (Newsreader), a stack of live proposal cards, "View all proposals →". Collapses to a slim edge tab (Layout already supports a resizable/collapsible right column — repurpose it).

### 2. Proposal detail / ballot — `Harmony Desktop.dc.html` (frame 2) + `Harmony Vote Flow.dc.html`
- **Purpose**: read a proposal and vote; reads like a civic document, votes like a poll.
- **Layout**: centered max-width ~860px doc column with a right `300px` vote panel. Breadcrumb + status pill; Newsreader title; description; "Why now" callout (`border-left:3px var(--accent)` on `--primary-soft`); deliberation thread; compose.
- **Vote panel** (sticky feel): "Live tally" with three labelled bars (for/against/abstain, counts + %), quorum + conviction chips, a "Currently passing" line, vote buttons, and a proxied-to footer with **Recall**. "On the record" meta block makes the cryptographic trust tangible (`✓ 142 keys`).

### 3. Proposals hub — `Harmony Desktop.dc.html` (frame 3)
- **Purpose**: the community's civic agenda.
- **Layout**: header + "Draft a proposal"; filter tabs (Open / Drafting / Decided / My delegations); proposal rows with `border-left:3px var(--gov-clay)`, status + passing/contested pills, a tally bar, and delegation note. Decided items render compact with ✓ Passed / ✕ Failed pills.

### 4. Delegation — `Harmony Desktop.dc.html` (frame 4)
- **Purpose**: manage who carries your vote, per topic (liquid democracy).
- **Layout**: "By topic" list — each topic card shows a `me → delegate` chip with Change / Recall; a "Your standing" card ("9 members delegate to you"); a simple node **delegation graph**; a transitive/cycle-safe explainer.

### 5. Vote flow (INTERACTIVE) — `Harmony Vote Flow.dc.html`
- **Purpose**: the canonical interaction spec for voting. This one is a working prototype.
- **Behavior** (replicate exactly): tap a proposal → ballot; **cast** for/against/abstain → the chosen bucket increments live, the bar animates (`transition: width .35s ease`), status recomputes (Passing/Failing/Quorum-not-met), and a toast confirms **"✓ Vote ▲ for · signed with your key"**; **delegate** → bottom sheet of trusted members, your vote follows their stance, tally reflects it; **recall** → returns the vote to you. The list shows per-proposal state (You voted / ↪ Proxied / Not voted).

### 6. Fork & lineage — `Harmony Forks & Lineage.dc.html`
- **A · Lineage tree**: a community genealogy — root → forks → fork-of-a-fork, connectors color-coded (sage = amicable, clay = dispute), "You are here" highlighted, member nodes badged ✓, with a detail panel of each fork's reason.
- **B · Fork dialog**: name the branch; "carry over" checklist (charter copy, **required** full-history snapshot, opt-in member invites); a **mandatory** "why" recorded on the lineage; consent-not-capture framing.
- **C · Fork divider**: in the new community's channels, carried (tinted, read-only) history sits above an immutable `⑂ Forked from … · N messages carried` band, live messages below. This maps directly onto `fork-timeline.ts`'s existing `ForkDivider` row.
- **D · Settings → Lineage**: "this is a fork of…", forks-of-this-community, "Fork this community".

### 7. Charter — `Harmony Charter & Settings.dc.html` (frame A) **[new view]**
- **Purpose**: a member-facing constitution generated from the community's real rules.
- **Sections**: Ratified-version header + "Propose amendment"; italic Newsreader **Preamble**; **Article I Membership & roles** (Member/Mod/Admin cards mapped to power 0 / ≥50 / ≥100 + a capability × role permissions matrix); **Article II How we decide** (Tier 1 Pulse / Tier 2 Motion / Tier 3 Charter cards, proposal quorum + admin quorum meters); **Article III Amendment** (any clause amendable by a Tier-3 proposal; versioned, never overwritten).
- Surfaces today's buried values as headlines: power numbers → named roles, "Admin governance" → "**3 of 4 · no single admin can act alone**."

### 8. Community settings — `Harmony Charter & Settings.dc.html` (frame B) + dialogs (frame C)
- Faithful restyle of the existing "Manage community" panel: Info grid (Name/Type/Members/Your role+power/Sync status), Public profile toggle, Members list with role badges + Set role/Kick, Invites, Admin governance (quorum), Forks, Danger zone (Leave).
- **Set role dialog**: a power slider with visible MEMBER/MOD/ADMIN bands; crossing into ADMIN keeps the existing confirm step. **Change admin quorum**: "3 of 4 admins" with the N+1 survivability note; the change is itself an admin action needing quorum.

### 9. Onboarding & identity — `Harmony Onboarding.dc.html`
- 5-step self-sovereign first run: Welcome → Create identity (local keypair, `did:harmony:…`) → **Save recovery kit** (12-word phrase + encrypted file + keychain; the only "clay" step) → Redeem invite (resolved community preview) → You're in. Plus **Identity & devices** settings (DID, backup status, key rotation, linked devices, danger zone), a gentle **backup reminder** banner, and the mobile backup step.

### 10. Mobile (greenfield) — `Harmony Mobile.dc.html`
- Bottom tab bar **Chat · Assembly · Activity · You** (governance is a first-class tab); a swipe-in **Spaces drawer** (same community→channel tree as desktop); the **Assembly** list; the ballot with a thumb-zone **sticky vote bar**; delegation. Phone frame 390×844.

### 11. Town Hall & voice — `Harmony Town Hall.dc.html`
- **Voice channel** (B): faithful restyle of `VoiceChannelView` — join-muted, Mute/PTT/Deafen/Leave bar, avatar-grid stage (≤12) → list, speaking rings, power-gated mod controls; states (C): join pane, PTT-held, self-mod-muted, channel-full.
- **Town Hall** (A, new): voice fused with the Assembly — active-speaker spotlight + waveform, in-room avatar grid (speaking rings / raised hands / muted), a mod **speaker queue**, a backchannel, and a **quorum-aware "Call this to a motion"** card (live vote if quorum present, else opens an async proposal). Mobile in D.

### 12. Vines & Files — `Harmony Vines & Files.dc.html`
- **Thesis**: one content-addressed store; feeds are lenses by media type (Vines = video ≤6s loops, ship first; Gallery / Posts later, same engine).
- **Vines feed** (A): faithful to `VineFeed` — Following/Discover, All/Unviewed, "N new", looping thumbs, reshare-with-attribution, reactions/loops.
- **Immersive player + publish** (B); **Files** (C): the store browsable — CID, size, usage, **replication health** (×N healthy / at-risk) + a "storage buddies" contribution meter. Mobile full-bleed loop player in D.

### 13. Vines feed (INTERACTIVE) — `Harmony Vines Feed.dc.html`
- The canonical spec for the feed's behavior. **Endless** vertical scroll-snap with **autoplay-on-view** (center card plays a 6s loop-ring, others pause/dim; more append near the bottom). **Discover = transitive follows, not an algorithm**: 2nd- and 3rd-degree only (capped at 3rd), each card showing a **degree chip** + **provenance path** ("Mara follows @kit" / "Priya → @lena → @iris"). **Tunable**: degree toggles + a **Tune** sheet to mute which follows propagate into Discover (recomputes live). Verified: muting a follow drops the Discover count live.

---

## Interactions & behavior

- **Voting** (see Vote Flow): cast → `tally[bucket] += 1`, animate bar widths, recompute status & quorum, emit a signed-confirmation toast (~2.1s). Wire to the existing `votingAdapter` / Tauri IPC; the prototype's `cast/delegate/recall` map to your `vote` / `delegate` / `recall` adapter calls.
- **Delegation**: per-topic, transitive, cycle-blocked, recallable any time before close. "Recall" always adjacent so delegation never reads as surrender.
- **Theme toggle**: `document.documentElement.dataset.theme = 'dark' | ''`, persisted (Settings → Appearance). Tokens handle the rest.
- **Assembly rail**: reuse Layout's existing resizable/collapsible right-column machinery; collapsed state shows a slim reveal tab.
- **Fork divider**: already produced by `buildUnifiedTimeline()` in `fork-timeline.ts` — this is a styling task on the `ForkDivider` row, not new logic.
- **Transitions**: tally bars `width .35s ease`; toast slide/fade; bottom sheet `translateY` in `.26s`. Keep motion calm.

## State management

Nothing here demands new global state beyond what exists. Net-new local state:
- a **theme** preference (store + persistence),
- the **Charter view** as an added member of `CommunityView`'s `activeView` union (`'channels' | 'proposals' | 'tier3' | 'charter'`),
- the Assembly rail's open/width (mirror the media-panel prefs already in `media-panel-prefs.ts`).

Vote/delegate/recall, quorum, roles, fork lineage, and backup state all already
have services/adapters in the codebase — these designs are presentation layers
over them.

## Assets

No raster assets required. The **Harmony mark** is three overlapping circles
(sage `#466b4c`, navy `#283450`, clay `#c56a46`) meeting at one point — inline
SVG, reproduced in every reference's header; lift it directly. Avatars in the
mocks are flat color circles (placeholders for real profile images). Icons are
system glyphs/emoji in the mocks — swap for your existing icon set.

## Files

- `tokens.css` — drop-in `:root` (+ dark) for `src/app.css`.
- `ADOPTION.md` — phase-by-phase, file-by-file implementation plan.
- `references/*.dc.html` — the 8 hi-fi design references.
- `references/support.js` — runtime needed to render the references locally.

Target codebase files referenced throughout: `src/app.css`, `index.html`,
`src/App.svelte`, `src/lib/components/{Layout,NavPanel,CommunityView,
CommunityProposalsPanel,ConvictionProposalCard,Tier3ProposalPanel,
DelegationWidget,DelegationGraph,CommunitySettingsPanel,ChangeQuorumDialog,
ChannelMessageFeed,ForkLineageTree,BackupReminderBanner,DevicesPanel}.svelte`,
`src/lib/{fork-timeline,media-panel-prefs}.ts`.
