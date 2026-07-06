# ZEB-608 — Commons E: Charter View (net-new) + Settings Restyle (Design)

**Ticket:** ZEB-608 (Commons Track E, epic ZEB-603). **Branch:** `zeb-608-commons-e-charter` off main `50eb276e`.
**Sources:** three exploration reports (charter data sources; settings surfaces; design extraction from
`docs/design/commons/references/Harmony Charter & Settings.dc.html` frames A–C +
`07-charter-settings.png`), README/ADOPTION Phase 3–4 guidance, ZEB-607 landed primitives.

**Goal:** a member-facing `CharterView` that GENERATES a community's constitution from live
governance state — plus a Commons restyle of the "Manage community" panel and its two governance
dialogs. Functionality wins over mock: nothing rendered that isn't real or explicitly derived.

---

## §0 Ticket-premise corrections

1. **No capability×role data exists.** The repo's only "capabilities" are the Tauri IPC ACL
   (`src-tauri/capabilities/default.json`) and transport ALPN — unrelated. The matrix must be
   **derived**: global `POWER_THRESHOLDS` (`src/lib/types.ts:381`, backend
   `community_membership.rs:3904` — the types.ts comment's `:1108` backend ref is stale) plus a
   hand-authored action→threshold table matching the real consumer checks (invite ≥0; channel
   manage/moderate/join-approval ≥50; set-roles/kick-admin/change-quorum ≥100). Thresholds are v1
   GLOBAL constants (per-community customization deferred to ZEB-251) — the charter must not imply
   they're community-specific.
2. **No admin-quorum getter exists, and the settings panel has a latent bug.**
   `CommunitySettingsPanel.adminQuorum` defaults to 1 and is NEVER wired from CommunityView/App —
   "Current admin quorum" always shows 1 today. The only client path to the real value
   (`list_pending_admin_proposals[].quorum_required`) is admin-gated and empty without proposals.
   **This spec adds a small read-only IPC** (§1 D1) that fixes the settings bug and powers the
   charter for all members.
3. **No per-community Tier-1/2/3 voting config exists.** Tier-1 params are per-poll; Tier-2
   defaults are unexposed backend constants (7-day half-life etc.); Tier-3 has no backend defaults
   (the create form hardcodes its own). The design's tier-card numbers ("simple majority + 50%
   quorum", "⅔ supermajority + 60% quorum, 7-day floor") are **invented placeholders** (design
   sticky #2 admits this). Tier cards must describe the REAL mechanics in prose — no invented
   percentages. The design's **"Proposal quorum" meter card is unimplementable** (no such
   community value) and is omitted; only the **Admin quorum** k-of-n meter is real.
4. **No charter text or version storage exists.** "Ratified v4" is synthetic. The real amendment
   record is finalized Tier-3 polls (`listTier3Polls` → `stage === 'fi'`, `winnerText`,
   `proposalText`, `proposer`, `pollCreateHlcMs` — creation time; finalization HLC is not in the
   summary). The header shows a derived count ("{N} ratified amendment{s}") — never a fake "v4".
   The preamble is likewise generated-document framing, not stored community prose.
5. **`activeView` union is at `CommunityView.svelte:128`** (also `$bindable` at `:56`), not `:132`.
   The existing tier3 tab is labeled **"Constitutional"** — the new tab is labeled **"Charter"**.
6. **"Propose amendment" = switch to the Constitutional tab.** Tier3ProposalPanel's prefill seam
   (`retryFailed`) is private; adding an external prefill prop is YAGNI for v1 — the button sets
   `activeView = 'tier3'`, landing on the create form.
7. **The settings panel has 9 sections, not 7** — the ticket omits **Message relay** (ZEB-582) and
   **Join requests** (PendingJoinsPanel host, ≥50-gated). Both are in the restyle's section-chrome
   scope. The header is **"Manage community"** (test-pinned), not "Community settings".
8. **The cross-admin-threshold confirmation lives in the PARENT panel** (`crossesAdminThreshold` →
   `ConfirmationModal`), not in SetPowerDialog. It stays exactly where it is.
9. **The self-referential quorum note does NOT exist today** — the ticket says "keep" it, but
   ChangeQuorumDialog has no such copy. It is NET-NEW (§2.10), copy from design frame C2.
10. **The design's Set-role dialog has no number input** — the existing range+number pairing is
    kept (accessibility rule; explorer confirms both inputs bind `power`).
11. **ChangeQuorumDialog is a native `<dialog>`** and its test pins that element
    (`container.querySelector('dialog')`) — the substrate stays; the restyle is within it.

---

## §1 Design decisions

### D1 — New read-only IPC: `get_community_governance` (the one Rust change)

`src-tauri/src/lib.rs` (beside `list_pending_admin_proposals`):
`get_community_governance(community_id) → CommunityGovernanceDto { admin_quorum: u8 }`, readable
by ANY member (no power gate — the charter is member-facing; the value is already materialized at
`community_membership.rs:1463`). Client binding in `src/lib/community-service.ts`
(`getCommunityGovernance(communityIdHex): Promise<{ adminQuorum: number }>`, camelCase per IPC
convention). Wiring: CommunityView loads it per community (stale-guarded like its other loads) and
threads `adminQuorum` to BOTH CharterView and CommunitySettingsPanel — **fixing the latent
always-shows-1 bug**. Founded-date (`Space.created_at`) is deliberately NOT exposed — its
semantics across forks are unresolved; the charter header does without it.

### D2 — New shared primitives (governance/)

- **`RoleBadge.svelte`** — props `{ role: 'member'|'mod'|'admin' }`. Mono 600 uppercase label,
  padding 2px 8px, radius 20px; token pairs per the design: member = `--status-drafting-fg/bg`,
  mod = `--status-open-fg/bg`, admin = `--status-passed-fg/bg`. Replaces the verbatim-duplicated
  `.role-badge[data-role]` CSS in CommunitySettingsPanel and SetPowerDialog (StatusPill is NOT
  force-fit: role badges are mono/smaller/membership-semantic, not governance-lifecycle).
- **`PipMeter.svelte`** — props `{ filled: number, total: number, label?: string }`. k-of-n
  discrete pips (flex, equal segments, gap 5px, height 7px, radius 4px; filled = `--vote-for`,
  empty = `--vote-abstain`). Used by the charter's Admin-quorum card and ChangeQuorumDialog.
  Distinct from TallyBar (contiguous fills) by design.
- **PowerSlider banding is built in-place** in SetPowerDialog (single consumer — YAGNI on a
  primitive): a banded track (flex: member `--status-drafting-bg` / mod `--gov-clay-soft` /
  admin `--primary-soft`, widths from `POWER_THRESHOLDS`) rendered behind/below the existing
  range input; range + number inputs and their aria-labels unchanged.
- The capability×role matrix is CharterView-internal markup (single consumer).

### D3 — CharterView composition (doc column, all real/derived data)

`CharterView.svelte`, mounted from CommunityView when `activeView === 'charter'`; doc-column
`max-width: 780px; margin: 0 auto` (ZEB-607 civic-document grammar). Data in: `communityName`,
`members` (roster), `myPower`, `adminQuorum` (D1), `votingAdapter` (for `listTier3Polls`),
`communityKind`. Sections:

1. **Header** — 📜 + `<h1>` `{communityName} Charter` (Newsreader 500 32px); meta row (mono 12px):
   ratified pill (`--primary-deep` on `--primary-soft`, radius 20): `✓ {N} ratified amendment{s}`
   (N = finalized Tier-3 polls; `✓ No amendments yet` when 0) · `{M} members bound` (joined
   count). Right: **Propose amendment** button (`--gov-clay-soft` bg, `--gov-clay-deep` text,
   clay border via `color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised))`, radius 7) →
   `onProposeAmendment()` → CommunityView sets `activeView = 'tier3'`.
2. **Preamble** — eyebrow "Preamble"; italic Newsreader 15.5px/1.65 generated-document framing
   (exact copy): *"This charter is {communityName}'s constitution, generated from its live
   governance state. Every clause below reflects the rules as they are enforced today, and every
   clause can be changed by the members it governs."*
3. **Article I · Membership & roles** — lede: *"Roles are earned, granted, and revoked as a
   numeric power level. Three named bands:"* 3 role cards (`--surface-raised`, radius 8,
   RoleBadge + mono `power 0` / `power ≥ 50` / `power ≥ 100` from `POWER_THRESHOLDS` — real
   values), descriptions per design copy. Then the **capability matrix** (grid `1fr 92px 92px
   92px`; header row on `--bg-secondary`; ● = `--vote-for`, — = `--vote-abstain`) with the 6
   derived rows exactly as the real checks gate them: Post/vote/propose ●●●; Delegate & recall
   ●●●; Fork the community ●●●; Manage channels & invites —●●; Approve joins · remove members
   —●●; Set roles · change decision rules ——●. A mono footnote: *"Thresholds are platform-wide
   in v1."* (honesty per §0.1).
4. **Article II · How we decide** — lede *"Proposals move through three tiers. Higher stakes,
   higher bar."* 3 tier cards with REAL prose, no invented numbers: **Tier 1 · Poll** —
   *"Multi-option approval polls. Options, window and eligibility are set per poll. Non-binding
   sentiment."*; **Tier 2 · Motion** — *"Binding conviction votes. Support accumulates over time
   (7-day half-life by default) toward a dynamic threshold; delegable, recallable."*; **Tier 3 ·
   Charter** — *"Amends how the community works. A sortition-selected mini-public deliberates,
   drafts and ratifies by STAR ballot."* Below: ONE meter card — **Admin quorum**: header +
   mono `{k} of {n}` + PipMeter + caption *"{k} of {n} admins must co-sign admin actions. No
   single admin can act alone."* (n = joined members with power ≥ 100, k = D1 value).
5. **Article III · Amendment** — `--primary-soft` callout, `--primary-border`, radius 10: ✎ +
   *"No clause here is permanent. Any member may open a Tier-3 proposal to amend how
   {communityName} works; if it ratifies, the change is signed by the mini-public and recorded.
   Every ratified decision stays on the record."* Below, when N > 0: **On the record** list of
   finalized Tier-3 polls (mono date from `pollCreateHlcMs` (labeled "proposed"), `proposalText`
   600, `winnerText` as the ratified outcome, proposer via `shortAddr`).

Tab wiring: union + `$bindable` extended at `CommunityView.svelte:128/:56`, new nav button
**"Charter"** beside "Constitutional" (`:350-373`), render branch in `.three-cols` (`:443-457`),
App union at `App.svelte:1042`. Charter tab (like the others) renders only when `votingAdapter`
exists — amendment data needs it.

### D4 — Tokens: zero new hex

All charter/settings colors map to existing tokens (design-extraction map). The recurring
clay-tan border `#e6cfa3` → `color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised))`
(guard-whitelisted idiom). The design's cool-blue "admin multisig" block is NOT adopted (no dark
values; same reasoning as ZEB-607's trust-blue rejection) — Admin governance renders on
`--bg-secondary`/`--border`. `.healthy`'s grandfathered raw `#7acc7a` → `var(--presence-online)`
(allowlist ratchets DOWN). Meter tracks use `--tally-track`. Radii snap to the token scale
(cards 8, dialogs keep their current radii family) — no `--radius-panel` added.

### D5 — Settings panel restyle (chrome only; 9 sections; zero structural change)

Converge on the PendingAdminProposalsPanel reference aesthetic already inside the panel:
section eyebrows → uppercase 600 10.5px `.1em` `--text-muted` (danger zone `--vote-against`);
rows/cards → `--surface-raised` + `--border` + radius 8 (+ `--shadow-e1` where card-like);
member rows adopt **RoleBadge**; Set role / Kick become the design's borderless text-buttons
(`--vote-for` / `--vote-against`, 600 11px); Admin governance keeps its prose + gains a PipMeter
under the quorum line; Danger zone Leave button → soft danger treatment
(`color-mix(in srgb, var(--vote-against) 8%, var(--surface-raised))` bg, `--danger-border-muted`
border, `--vote-against` text). ALL test-pinned classes, copy, aria-labels, placeholders, and
section gating stay byte-identical (`.member-row`, `button.kick`, `button.set-role`,
`.pending-badge`, "Change quorum…", "Search members...", etc.). Embedded child components
(InviteLinkManager, PendingJoinsPanel, ForkLineageTree, ForkConfirmDialog, ConfirmationModal,
LastAdminWarningDialog, CommunityMembersPanel) are OUT of scope → ZEB-611 gap-fill audit.

### D6 — SetPowerDialog: band visualization

Keep Modal.svelte substrate, range+number pairing, aria-labels, `.thresholds` legend semantics,
and the parent-owned admin-threshold confirm. Add the banded track (D2) + move the current-value
readout to a mono `--vote-for` display; role preview adopts RoleBadge; helper line per design
("Moderator can manage channels, invites & join requests." style copy keyed to the previewed
role). Buttons → Commons grammar (Cancel outline / Set role filled `--accent`).

### D7 — ChangeQuorumDialog: Commons chrome + net-new self-referential note

Keep the native `<dialog>` (test-pinned), `Quorum slider`/`Quorum number` aria-labels, the N+1
copy verbatim, and the validation. Add: card chrome on the dialog (`--surface-raised`, `--border`,
radius 10, `--shadow-e2`, padded header "Change admin quorum" in Newsreader 500), PipMeter
preview of the PROPOSED k-of-n, styled buttons (Cancel outline / "Propose change" filled), and
the **net-new warning box** (`--gov-clay-soft` bg, clay color-mix border, `--gov-clay-deep`
text): *"⚖ This change is itself an admin action — it needs the current {currentQuorum}-of-
{currentAdminCount} quorum to take effect."*

---

## §2 Surface list

1. `src-tauri/src/lib.rs` — `get_community_governance` IPC + DTO + registration (+ Rust test).
2. `src/lib/community-service.ts` — `getCommunityGovernance` binding (+ TS test).
3. `src/lib/components/governance/RoleBadge.svelte` (+ tests).
4. `src/lib/components/governance/PipMeter.svelte` (+ tests).
5. `src/lib/components/CharterView.svelte` (net-new, + tests).
6. `src/lib/components/CommunityView.svelte` — union/`$bindable`/tab/branch + governance fetch +
   `adminQuorum` threading (fixes §0.2 bug).
7. `src/App.svelte` — `communityActiveView` union member `'charter'`.
8. `src/lib/components/CommunitySettingsPanel.svelte` — D5 restyle + RoleBadge + PipMeter +
   `#7acc7a` → token (+ allowlist ratchet DOWN).
9. `src/lib/components/SetPowerDialog.svelte` — D6.
10. `src/lib/components/ChangeQuorumDialog.svelte` — D7 (+ net-new copy).

## §3 Test lockstep

- MUST keep passing unedited: CommunitySettingsPanel.test.ts (~40 tests — all pinned selectors/
  copy per explorer list), SetPowerDialog.test.ts (range/number sync, MEMBER/MOD/ADMIN uppercase,
  clamp-on-blur, `.confirm-btn`), ChangeQuorumDialog.test.ts (native `dialog` element, exact
  aria-labels, `/N\+1/`, `/survivability/i`, Propose/Cancel), LastAdminWarningDialog.test.ts,
  CommunityView.test.ts:170 (gear opens panel). Exception: the settings panel quorum-copy test
  may need a props update if it asserts the default `1 of` (verify — wiring now supplies real
  values; the component default stays 1).
- New tests: RoleBadge (3 role→pair mappings), PipMeter (filled/total rendering, clamps),
  CharterView (derived amendment count incl. 0-state, matrix rows, admin-quorum card k-of-n,
  Propose amendment fires callback, renders without votingAdapter data gracefully),
  community-service getCommunityGovernance (camelCase key), CommunityView charter tab
  (tab renders + branch mounts + deep-link state), ChangeQuorumDialog warning-box copy,
  SetPowerDialog band widths.
- Rust: `get_community_governance` unit/integration test (returns materialized quorum; readable
  at power 0). Gates: `cargo fmt` + clippy `--all-targets` + `scripts/test-select --context task`
  iteratively, full sweep at the end (CLAUDE.md).

## §4 Out of scope

Per-community threshold config (ZEB-251); charter text storage/true versioning; prefill of the
Tier-3 create form; founded-date exposure; cool-blue admin-multisig palette; restyles of
InviteLinkManager/PendingJoinsPanel/ForkLineageTree/ForkConfirmDialog/ConfirmationModal/
LastAdminWarningDialog/CommunityMembersPanel (→ ZEB-611); nav "charter channel row" from the
design (reality = CommunityView tabs); Modal-substrate unification for ChangeQuorumDialog.

## §5 Constraints (binding)

- Frontend gates: `npx tsc --noEmit && npx vitest run`. Rust gates per CLAUDE.md (`--locked`,
  `--all-targets`, `test-fixtures`; test-select for iteration, full sweep final).
- No raw hex in Svelte `<style>`; allowlist ratchets DOWN only (`#7acc7a` removal).
- Tauri IPC naming: Rust snake_case params, JS camelCase call sites.
- No invented data in CharterView — every number traceable to POWER_THRESHOLDS, the roster,
  the D1 IPC, or finalized Tier-3 polls.
- ZEB-606/607 contracts untouched (AssemblyRail/MessagesRail tests unchanged; existing tabs'
  behavior identical).
- One PR; commit per task; no worktrees; branch `zeb-608-commons-e-charter`.
