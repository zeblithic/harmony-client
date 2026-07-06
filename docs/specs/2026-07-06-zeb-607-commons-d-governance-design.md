# ZEB-607 — Commons D: Governance Surfaces Restyle (Design)

**Ticket:** ZEB-607 (Commons Track D, epic ZEB-603). **Branch:** `zeb-607-commons-d-governance` off main `60ba4693`.
**Sources:** three exploration reports (Tier-1/Tier-2 code map; Tier-3 code map; design extraction from
`docs/design/commons/references/Harmony Desktop.dc.html` frames 2–4, `Harmony Vote Flow.dc.html`,
`screens/09-vote-flow.png`, `docs/design/commons/{README,ADOPTION}.md`), plus ZEB-606 landed seams
(`docs/specs/2026-07-06-zeb-606-commons-c-shell-nav-design.md`).

**Goal:** extend the Commons vocabulary (status pills, clay/sage semantics, tally anatomy, doc-column,
signed-vote feedback) across ALL governance surfaces — Tier-1 approval polls, Tier-2 conviction
proposals, delegation, and the full Tier-3 lifecycle — reconciled with the real voting models.
Functionality wins over mock: no invented fields, verbs, or stages.

---

## §0 Ticket-premise corrections

1. **CalibrationView.svelte is NOT governance — excluded.** It is Q8 Spellbook voice-syllable
   calibration (501 lines, phases intro→recording→done, mounted from `SpellbookMode.svelte:133`).
   The ticket's mention of a "calibration phase" in the Tier-3 lifecycle is wrong.
2. **The real Tier-3 stage union has 6 members, not 5:** `'so'|'de'|'dr'|'ra'|'fi'|'fa'`
   (`src/lib/types/voting.ts:540`, labels at `tier3StageLabel` `:687`) — sortition, deliberation,
   drafting, ratification, finalized, failed. `Tier3LifecycleStatus.svelte` renders only the 4
   pipeline stages `['so','de','dr','ra']`; `fi`/`fa` are terminal states shown elsewhere. The
   restyle must cover all 6.
3. **Two components the ticket omitted are in scope:** `BridgingPanel.svelte` (83 L, heat-bar tally)
   and `MiniPublicParticipationToggle.svelte` (75 L, participation decline). Both are Tier-3
   deliberation surfaces that would be left stranded on legacy tokens otherwise.
4. **The design's 3-segment for/against/abstain ballot is semantically wrong for ALL THREE real
   voting models.** Reconciliation, not transplant (§1 D1).
5. **"Contested" is a static hand-label**, present only in Desktop frame 3. The interactive vote-flow
   model computes exactly 4 verdicts: Passing / Failing / Tied / Quorum-not-met. We adopt computed
   states only, mapped to the real lifecycles (§1 D3).
6. **Delegation is community-wide, not per-topic or per-proposal.** The real API is
   `delegateTier2(communityId, delegate)` / `undelegateTier2(communityId)` (`voting-adapter.ts:699/:704`).
   Frame 4's "by topic" cards and the mobile per-proposal delegate sheet do not map. The
   DelegationWidget remains the single delegation + recall surface (severity-tiered confirmation);
   the card's proxied-to footer shows the community delegate and offers **Vote directly** — the
   real per-proposal override verb — not a card-level Recall.
7. **No proposal metadata beyond `proposal_text` exists** (Tier-2 DTO: `proposal_id, community_id,
   proposal_text, lifecycle, total_conviction_ms, threshold_conviction_ms, half_life_seconds,
   auto_exec, total_supply, voter_count`). The design's "Why now" callout, byline working-group,
   amendment counts, and closing timers have no data source — omitted (no invented fields). The
   "On the record" block IS buildable from real fields (§2.3).

---

## §1 Design decisions

### D1 — Semantic reconciliation per tier (the real work)

| Design element | Tier-2 conviction | Tier-1 approval poll | Tier-3 lifecycle |
|---|---|---|---|
| 3-segment tally | **NO.** Single-axis fill: `total_conviction_ms / threshold_conviction_ms` capped at 100%, fill `--vote-for` on `--tally-track` | **NO.** Per-option bars: each option a single fill `--vote-for` on `--tally-track`, mono count/% | Ratification results keep STAR score distribution; statement agree/disagree uses two-tone where two buckets genuinely exist |
| Vote buttons | `▲ Support` filled `--vote-for`; `Withdraw` outline (existing verbs; `signalTier2(id, support)`) | Per-option approve toggles: selected = filled `--vote-for`, unselected = outline `--primary-border`/`--vote-for` text | Stage-specific verbs unchanged (statement votes, scores) — restyled to the same filled/outline grammar |
| Quorum chip | **NO** (not a conviction concept — threshold is a dynamic Q96.32 band). Replace with **Threshold chip**: `--primary-soft` box, mono `NN% of threshold` | Participation chip where the component already shows voter counts | Mini-public / participation counts via CountChip |
| Conviction chip | **YES** — `--gov-clay-soft` box, label `--gov-clay-deep`, mono value (conviction trend/total) | n/a | n/a |
| Verdict pill | From lifecycle (D3) | n/a (options, not verdicts) | From stage (D3) |
| Timer `⏳ 2d 4h` | **NO** — conviction proposals have no closing time | Only if the poll DTO carries a deadline (it does not — omit) | Only where a stage deadline field exists (none — omit) |

### D2 — Shared governance primitives (the restyle spine)

None exist today: 3 verbatim `.confirm-modal` copies (Tier3ProposalPanel, StatementComposer,
StarRatificationBallot), 5+ count-chip variants, 2 independent pill systems, ~4 `shortAddr` copies.
New, under `src/lib/components/governance/` (+ one util):

- **`StatusPill.svelte`** — props `{ variant, label? }`. Anatomy: Public Sans 600 11px, padding
  4px 11px, border-radius 20px. Variant → token pair table (D3). Default label per variant,
  overridable (e.g. stage names from `tier3StageLabel`).
- **`TallyBar.svelte`** — props `{ segments: Array<{ pct: number, token: string }>, trackHeight? }`.
  Renders flex segments on `--tally-track`, radius 4, `overflow: hidden`, each segment
  `transition: width .35s ease`. Covers single-axis conviction, per-option approval, agree/disagree
  two-tone, and BridgingPanel's heat bar.
- **`CountChip.svelte`** — props `{ label, value, tone: 'sage'|'clay'|'neutral' }`. Design's
  quorum/conviction chip anatomy: soft bg box, 9.5px label, mono 13px value.
- **`GovConfirmModal.svelte`** — replaces the 3 verbatim copies; props for title/body/confirm-label
  plus `severity: 'click' | 'typed'` (typed requires a match string — DelegationWidget's
  typed-"revoke" recall path adopts it; the 3 existing click-confirms keep click semantics).
- **`src/lib/short-addr.ts`** — single `shortAddr()` util; the ~4 component-local copies migrate.

Existing behavior contracts (event subscriptions, load-token race guards, optimistic-cast handling,
ZEB-319 event-driven Tier3ProposalPanel — 11 subs, no polling) are untouched; this is presentation.

### D3 — Verdict/status mapping and tokens

Real lifecycles → pill variants (all pills via StatusPill):

| State | Variant | Tokens (fg/bg) |
|---|---|---|
| Tier-2 `Open` | `open` | `--status-open-fg/bg`, label `● Open` |
| Tier-2 `ThresholdReached` | `passing` | `--verdict-passing-fg/bg`, label `Threshold reached` |
| Tier-2 `Finalized` | `passed` | `--status-passed-fg/bg`, label `✓ Passed` |
| Tier-2 `Archived` | `archived` | `--status-drafting-fg/bg`, label `Archived` |
| Tier-3 `so/de/dr/ra` | `stage` | `--status-open-fg/bg` current, `--status-drafting-fg/bg` others (labels from `tier3StageLabel`) |
| Tier-3 `fi` | `passed` | `--status-passed-fg/bg` |
| Tier-3 `fa` | `failed` | `--status-failed-fg/bg` |

**New tokens in `src/app.css`: pure `var()` aliases only, zero new hex** (`:root` only — `var()`
references substitute at use time against cascaded theme values, so no dark-block duplicates):
`--verdict-passing-fg: var(--primary-deep); --verdict-passing-bg: var(--primary-soft);
--verdict-failing-fg: var(--danger-deep); --verdict-failing-bg: var(--status-recalled-bg);`
(failing/tied/quorum variants defined for completeness; the design's Tied/Quorum-not-met reuse the
open/drafting pairs and need no alias).

**Design gap-list resolution — reuse nearest existing token, add nothing:** warm muted `#8a8472` →
`--text-muted`; list-row mono meta `#959183` → `--faint`; warm inset `#f6f3ec` → `--paper`; warm
card border `#e6dcc6` → `--border`; against-outline `#e0b9b1` → `--danger-border-muted`;
"not voted" chip → drafting pair. **Trust-blue proxy family NOT adopted** (design has no dark-theme
values; proxied affordances render on `--paper` with `--text-muted` copy and `--vote-against` Recall
link, matching frame 2's footer).

### D4 — Legacy-token migration in governance components

All 10 Tier-3 components plus card/panel/delegation currently sit on legacy tokens; zero Commons
ballot tokens are consumed anywhere in governance today (only ProposalsNavRow/AssemblyRail from
ZEB-606). Migration map:

- `--success-gov` (agree chips, winner badges) → `--vote-for` family (`--status-passed-*` for
  winner pills).
- `--danger-alt` (disagree chips) → `--vote-against`; failed states → `--status-failed-*`.
- `--accent` primary CTAs stay `--accent` (≡ sage by Track B design).
- **`--gov-purple` + `--sortition-bg` RETAINED** for sortition/encryption surfaces
  (SortitionRevealView, StarRatificationBallot `.encryption-banner`). Rationale: both tokens are
  already theme-aware in app.css, shipped by the design's own tokens.css, and mark a real semantic
  (cryptographic sortition/secrecy) that folding into clay would erase. The design ships the tokens
  without frames — tokens without a frame beat a frame without data.
- Raw-hex removals: `DelegationGraph.svelte:312` `#facc15` → `--warning` (via `tokenColor()` if
  canvas/SVG-attr, `var()` if style); `StatementComposer.svelte:96` `.cap-warning #d9b438` →
  `var(--warning)`. Removing allowlisted literals requires the sanctioned ratchet update
  (`UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run`) with a diff showing **removals only** — the
  never-regenerate rule guards against admitting NEW hex, not against ratcheting down.

### D5 — Layout: doc-column detail + two-regime card

- **CommunityProposalsPanel detail state** adopts the civic-document layout: centered doc column
  `max-width: 860px`, grid `1fr 300px; gap: 28px; align-items: start` — description reads first,
  vote panel right ("Ballot, not dashboard"). **Must degrade to single column** (vote panel stacks
  below title, above description) under a container/media breakpoint (~720px effective width) —
  the middle column can be far narrower than 860px.
- **ConvictionProposalCard** renders in 2 regimes — CommunityView middle column AND AssemblyRail
  rail (< 500px) — both capped 520px. Commons anatomy per ADOPTION Phase 2 (header: mono ID pill on
  `--gov-clay` + StatusPill; title Public Sans 600; single-axis TallyBar; threshold + conviction
  CountChips; `▲ Support` filled / `Withdraw` outline; proxied-to footer with the Vote-directly
  override, §0.6) laid out to survive both regimes. ID pill shows the shortened `proposal_id` —
  no invented P-nn numbering.
- **Proposals hub rows** (CommunityProposalsPanel list state): white card, `--border`,
  `border-left: 3px var(--gov-clay)` for active proposals (`--vote-abstain` for archived), radius 8,
  `--shadow-e1`; row = ID pill + StatusPill + inline TallyBar + mono meta. Grouping/filters follow
  the panel's existing structure — restyle, don't re-architect.

### D6 — Signed-vote feedback (net-new toast)

New wiring in `src/lib/voting-toast-wiring.ts` (alongside `setupDelegateOnBehalfToast`, whose copy
is LOCKED by ZEB-298 Task 10 — untouched). Fired on successful mutation ack, ~2.1s auto-dismiss,
via the existing toastStore:

- Support cast → `✓ Support signaled · signed with your key`
- Support withdrawn → `✓ Support withdrawn · signed with your key`
- Delegate → `↪ Proxied to {name}`
- Recall → `↩ Delegation recalled — your vote is yours again`

(Design copy adapted where the design's stance vocabulary doesn't exist — conviction has no
▲for/▼against/—abstain stances.)

### D7 — Motion

Tally fills `transition: width .35s ease` (TallyBar built-in). No bottom sheets (desktop; delegate
flow stays in DelegationWidget). "Keep motion calm" — no other animation added.

---

## §2 Per-surface specs

Anatomy shorthand: *pill* = StatusPill, *chip* = CountChip, *bar* = TallyBar, *modal* = GovConfirmModal.

1. **ConvictionProposalCard.svelte** (365 L) — full Commons card per D5; existing optimistic-cast
   state, votingReady-gated hosts, and delegate-context props (PR #408 Greptile P1) preserved.
   Cast/withdraw success fires the D6 toast.
2. **CommunityProposalsPanel.svelte** (398 L) — hub rows per D5; detail state = doc-column per D5
   with **"On the record"** block on `--bg-secondary`: Method `conviction · half-life {d}d`,
   Threshold `{pct}% reached`, Signed by `✓ {voter_count} keys` (value `--vote-for`), plus the
   design's replication note verbatim: *"Every vote is signed by its caster's key and replicated
   peer-to-peer. No server can alter the tally."* No "Why now" (§0.7).
3. **PollMessage.svelte** — Tier-1 N-option approval: per-option bars + approve toggles per D1;
   mono counts; winner option gets `--status-passed-*` pill (existing `.badge.winner` semantics
   migrate to StatusPill).
4. **DelegationWidget.svelte** — frame-4 delegate-chip anatomy on `--paper` (me → arrow `--faint` →
   delegate avatar + name 600 13px + mono carry-count `--vote-for`), `Change` outline neutral +
   `Recall` outline `--danger-border-muted`/`--vote-against`; typed-"revoke" path adopts modal
   `severity: 'typed'`; the transitive-chain note renders on `--primary-soft` with `--primary-deep`
   ink. Community-scoped (§0.6).
5. **DelegationGraph.svelte** — `#facc15:312` → `--warning` (D4); node/connector colors from
   Commons tokens (`--accent` top nodes, `--vote-abstain` leaves, `--border-default` connectors),
   caption `--text-muted`.
6. **Tier3LifecycleStatus.svelte** (82 L) — `.stage-chip` grammar migrates to pill anatomy:
   current stage = open pair, completed/upcoming = drafting pair (kept distinct via opacity as
   today); labels stay `tier3StageLabel`. `.stage-chip.current` selector is test-pinned —
   lockstep (§3).
7. **Tier3ProposalPanel.svelte** (699 L) — stage-dispatched content untouched (ZEB-319 event
   contract); chrome migrates: header gets stage pill + mono ID pill; `.confirm-modal` copy →
   modal; `--success-gov`/`--danger-alt` per D4; winner announcement uses `--status-passed-*` +
   `winnerEventHash` mono on `--paper`.
8. **DraftingPanel.svelte** (179 L) — doc-ish reading column, drafting pill, compose affordances
   on `--paper` with `--border`.
9. **DeliberationView.svelte** (116 L, zero tokens today) — frame-2 deliberation grammar: section
   header uppercase 600 11px `.08em` `--text-muted` + mono count + hairline `--line-soft`; comment
   rows with 600 13px names.
10. **StatementComposer.svelte** (106 L) — `.cap-warning #d9b438:96` → `var(--warning)`;
    `.confirm-modal` → modal; compose pill on `--surface-raised` with `--border`.
11. **StatementVoteList.svelte** (162 L) — `.chip.agree` → `--vote-for` grammar (stance tag: mono
    10px, bordered `--primary-border`, text `--vote-for`, `▲ agree`), `.chip.disagree` →
    `--vote-against` / `--danger-border-muted`, `▼ disagree`; two-tone bar where both buckets shown.
12. **SortitionRevealView.svelte** (117 L) — keeps `--gov-purple`/`--sortition-bg` (D4); chrome
    (borders, text, buttons) migrates to Commons neutrals + `--accent` CTAs.
13. **StarRatificationBallot.svelte** (245 L) — paired slider+number inputs preserved (a11y rule);
    `.encryption-banner` keeps sortition purple; score distribution rendered with bar; submit CTA
    `--accent`; `.confirm-modal` → modal.
14. **BridgingPanel.svelte** (83 L) — `.heat-bar` → bar on `--tally-track` with `--gov-clay` fill;
    labels mono `--text-muted`.
15. **MiniPublicParticipationToggle.svelte** (75 L) — decline affordance as outline
    `--danger-border-muted`/`--vote-against`; confirm copy unchanged.
16. **PendingAdminProposalsPanel.svelte** (234 L, settings-mounted, local DTOs, direct invoke) —
    restyle only: row anatomy per D5-hub, approve/reject buttons to filled `--vote-for` / outline
    `--vote-against` grammar. No DTO/invoke changes.
17. **voting-toast-wiring.ts** — D6 wiring; `setupDelegateOnBehalfToast` untouched.

---

## §3 Test lockstep

- Pinned selectors/colors that MUST move with the restyle: `Tier3LifecycleStatus` `.stage-chip.current`;
  `StatementVoteList` agree/disagree chip classes; `PollMessage` `.badge.winner/.runner-up`;
  ConvictionProposalCard button labels/classes wherever tests query them; any test asserting
  legacy `--success-gov`/`--danger-alt` styles.
- **Must keep passing unchanged:** `AssemblyRail.test.ts` + `MessagesRail.test.ts` (ZEB-606
  contracts: ordering, delegate threading, remote-cast refetch, tab persistence) — the card restyle
  must not alter its props contract or the rail's text anchors (`No open proposals`,
  `View all proposals →`).
- New primitives get their own test files (StatusPill variant→token mapping, TallyBar segment
  widths + transition presence, CountChip render, GovConfirmModal click vs typed severity,
  short-addr).
- Toast wiring: new tests beside the existing delegate-toast tests; ZEB-298 copy assertions
  untouched.
- Style gates: raw hex removals ratchet the allowlist DOWN only (D4); `commons-hex-guard` and
  `style-token-guard` must pass without admitting new literals.

## §4 Out of scope

Per-topic delegation and delegate bottom-sheet (no API/mobile surface); "Why now"/byline/amendments/
timers (no data); trust-blue token family (no dark values); "Draft a proposal" civic wizard beyond
the existing create flow; sortition visual redesign (no frames — tokens retained as-is);
Tier-1 3-segment transplant; any Rust/DTO change; Tauri/Layout.svelte changes; ARIA-tabs completion
(ZEB-646); `--on-accent` contrast pass (ZEB-644).

## §5 Constraints (binding)

- Frontend gates from repo root: `npx tsc --noEmit && npx vitest run` (~3160 tests / 265 files).
- No raw hex in Svelte `<style>` (style-token-guard); allowlist ratchets down only; new app.css
  tokens are `var()` aliases only.
- Svelte 5 runes idioms; existing event-subscription/race-guard/optimistic patterns preserved.
- ZEB-606 seams preserved: votingReady gating, delegate-context props, remote-cast refetch self-skip.
- One PR; commit per task; no worktrees.
