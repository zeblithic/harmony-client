# Commons H — Gap-Fill Audit

> **Track:** ZEB-611 (Commons H), child of the Commons design-adoption epic ZEB-603.
> **Date:** 2026-07-06 · **Branch:** `zeb-611-commons-h-gap-fill-audit` off `main@2771d126`.
> **Method:** seven parallel read-only sweeps, one per surface cluster, each scoring every
> component against the Commons anatomy rubric (§2) below.

## 1. Summary

The Commons design covers ~20 of ~160 components. After the Track-B token flip, **every**
surface already inherits the Commons palette — so H is not "recolor 59 components," it is
"re-anatomize the ones that still wear Discord-era layout DNA" (type hierarchy, pill/chip
anatomy, radius scale, card elevation).

**Scope:** ≈59 in-scope components across 8 clusters (about a third of the UI).

**Drift split (of the ≈59):**

| Bucket | Share | Meaning |
|---|---|---|
| NONE | ~10% | Already Commons-aligned — skip. |
| TRIVIAL | ~25% | A small mechanical swap (token / radius / font / elevation). **Shipped in this track's sweep (§4).** |
| TICKET | ~55% | A surface's worth of real restyle. **Filed as follow-up tickets (§5).** |
| NEEDS-DESIGN | ~10% | A genuine semantic / visual-form call. **Grouped into one design-session ticket (§5).** |

**This track ships (per the approved "map + trivial sweep" shape):** this audit document (the
durable map) + one mechanical sweep PR (§4: the TRIVIAL bucket + the stray literals Track A
missed, ratcheting five allowlist entries to zero). Everything substantive is deferred to the
tickets in §5 — including the confirm-dialog family, which was kept out of the sweep because it
hinges on a shared `Modal.svelte` change with app-wide blast radius.

**Already aligned — excluded from the audit:** `PollMessage` (Track D), `RedeemInviteDialog` +
`WelcomeModal` (Track G), `ForkConfirmDialog` (already fixed), `governance/GovConfirmModal`,
`governance/StatusPill`, `governance/CountChip` (the reference idioms).

**Inventory corrections** (the 2026-07-01 list was slightly off):
- "pkarr panel" is not a component — it is the `.pkarr-relays` section inside `NetworkHealthView`
  (and pkarr status inside `DiagnosticsPanel`).
- "reactions" is not a component — the `.reaction-chip` / `.reaction-toolbar` / `.reaction-picker`
  rules live inline in `ChannelMessageFeed.svelte`.
- The real toast chrome lives in `Toast.svelte`, not `ToastHost.svelte` (which is pure layout).
- The confirm-dialog `*Confirm*` glob also surfaced `ReshareConfirmDialog` (same unaligned pattern,
  folded into the confirm-family ticket) and `ForkConfirmDialog` (already aligned).

## 2. The Commons anatomy rubric (the yardstick)

Scored per surface, from the design-system reference (`docs/design/commons/references/`), the
exemplar components (`WizardProgress`, `ConvictionProposalCard`, `CharterView`, `ForkLineageTree`,
`governance/StatusPill`, `governance/CountChip`), and the live tokens in `src/app.css`.

- **Type** — display text/headers → `var(--font-display)` (Newsreader); UI chrome/body/buttons →
  `var(--font-ui)` (Public Sans); IDs / tallies / timestamps / addresses → `var(--font-mono)`
  (IBM Plex Mono); eyebrow/section labels uppercase + letter-spaced.
- **Radius** — chips/tags/ID-badges 3px; inputs/rows 5px; cards/panels/modals 8px (≤11px
  tolerance for soft nested cards); round affordances (status pill, avatar, count badge) 20px /
  `50%`. (The `--radius-chip/input/card` tokens exist in `tokens.css` but were **never landed in
  `app.css`** — every shipped component uses literal `3/5/8px`, so this audit keeps that
  convention; threading the tokens is a separate optional cleanup.)
- **Pills/Chips** — status/lifecycle → the shared `StatusPill` idiom (20px, variant→token pairs);
  metric boxes → `CountChip` (sage/clay/neutral tone); short ID/code tags → 3px mono badge.
- **Cards/Elevation** — cards on `var(--surface-raised)` + `var(--border)` + radius 8px +
  `var(--shadow-e1/e2/e3)`; governance-attention cards use `border-left: 3px solid var(--gov-clay)`.
- **Spacing** — the 4px grid (4/8/12/16/24/32). Rem-at-14px-root spacing is the legacy outlier.
- **Accent semantics** — sage (`--accent`/`--primary-*`) = consensus/passing/active/brand; clay
  (`--gov-clay*`) = open/attention/deadline/"act now", used sparingly; red (`--danger`/
  `--vote-against`) = against/destructive/failed only.
- **Guardrails (budget-0)** — no raw hex/rgb/hsl/named-color in a `<style>` block except
  `transparent`, `currentcolor`, `var(--token)`, or `color-mix()` with all-`var()` color args; no
  Discord hexes anywhere. `style-token-guard.test.ts` enforces this via a shrinking per-file
  allowlist; new/restyled surfaces get budget 0.

## 3. Findings by cluster

Ratings: deviation NONE/LOW/MED/HIGH · bucket NONE/TRIVIAL/TICKET/NEEDS-DESIGN (ND).

### Network mode
| Component | Dev | Bucket | Notes |
|---|---|---|---|
| NetworkHealthView | HIGH | TICKET (+literals→sweep) | bare `<section>`s (no card), h1/h2 no display font, bespoke relay `.badge`, 7 stray literals (`orange`/`crimson`/`rgba`). |
| NetworkGraph | LOW | ND | canvas heat-map reuses governance triad on a paper canvas; `capabilityColor` borrows flashcard/category tokens. |
| NetworkStatusBar | NONE | NONE | tallies inherit UI font (house-consistent). |
| NodeDetail | MED | TRIVIAL + TICKET | address-chip 10px / link-item 4px / node-name no display font (sweep); metric tiles → CountChip gated by danger-tone Q. |
| NetworkDiscoverabilitySettings | LOW-MED | TRIVIAL + TICKET | relay-input 3px (sweep); `.relay-badge` byte-dup of NetworkHealthView's → shared StatusPill. |
| Sparkline | NONE | NONE | token-driven SVG. |

### Mail
| Component | Dev | Bucket | Notes |
|---|---|---|---|
| MailInbox | MED | TICKET | systemic rem-at-14px spacing; `.unread-badge` bespoke. |
| MailReader | MED | TICKET | rem spacing; `.subject` h2 no display font. |
| MailCompose | HIGH | TICKET (+#e55/radius→sweep) | rem spacing; `#e55` literal; input radius 4px. |

### Spellbook / Flashcards
| Component | Dev | Bucket | Notes |
|---|---|---|---|
| SpellbookMode | LOW | TRIVIAL | select radii 4px; tab-btn 6px padding. |
| FlashcardView | LOW | ND | `.ptt-hint.error` (mic-denied) uses warning-clay, not danger. |
| FlashcardGrid | LOW | TRIVIAL | grid-row 6px / byte-cell 4px. |
| FlashcardStats | NONE | NONE | on-grid, mono tallies. |

### DMs & social
| Component | Dev | Bucket | Notes |
|---|---|---|---|
| FriendsPanel | MED | TICKET (+#e0a23c→sweep) | 7× 4px radii, bespoke friend-badge, `#e0a23c` literal. |
| DmCreateDialog | HIGH | ND (+#f99→sweep) | chips/primary use `--library-accent` (blue), 12px chip radius, `#f99` literal. |
| ProfilePopover | MED | TRIVIAL | bg-tertiary card, ad-hoc shadow, name no display font. |
| ProfileEditor | MED | ND | boxed bg+radius but no border/shadow — card or flat section? |
| ProfilePanel | LOW | TRIVIAL | panel-name no display font. |
| TrustBadge | LOW | ND | bare 8px color dot, no label/chip form. |
| TrustEditor | LOW | TRIVIAL | editor-heading no display font; 4px radii. |
| TrustOverview | NONE | NONE | data table, eyebrow headers correct. |
| UntrustedMediaCard | MED | ND | attention card with no clay signaling; confirm-load button sage. |
| SensitivityBadge | LOW | TRIVIAL | badge radius 4px → 3px. |

### Calls
| Component | Dev | Bucket | Notes |
|---|---|---|---|
| IncomingCallToast | LOW | TRIVIAL | 6px card radius, ad-hoc shadow. |
| CallInProgressBar | MED | TRIVIAL + ND | elapsed timer no mono, 4px control radii (sweep); deafen-active-sage valence (ND). |
| GroupCallBar | MED | TRIVIAL + ND | same as CallInProgressBar. |
| GroupCallBanner | LOW | TRIVIAL | btn-join 4px radius. |
| PttButton | NONE | NONE | single-meaning active state, compliant. |

### Messaging niceties
| Component | Dev | Bucket | Notes |
|---|---|---|---|
| MentionAutocomplete | MED | TRIVIAL | bg-secondary dropdown, no elevation, 6px/4px radii. |
| NamedEmojiPicker | MED | TRIVIAL | inputs with zero chrome (raw UA default). |
| ThreadView | LOW | TRIVIAL | thread-close 4px radius. |
| FloatingThreadBar | MED | TRIVIAL | entry-count tally no mono, thread-entry 4px. |
| QuietMessageGroup | LOW | NONE | flat toggle, minor off-grid margin. |
| reactions (in ChannelMessageFeed) | HIGH | TRIVIAL + ND | count no mono, toolbar/picker bg-secondary + ad-hoc shadow + 6px (sweep); reaction-chip 10px magic radius (ND). |

### Membership / moderation + confirm-dialog family
| Component | Dev | Bucket | Notes |
|---|---|---|---|
| PendingJoinsPanel | HIGH | TICKET (+#c33→sweep) | Svelte-4, unstyled `<details>`, `#c33` literal, em spacing, classless kick button. |
| ModerationReasonDialog | LOW | TICKET (confirm family) | title no display font, 4px radii. |
| InviteLinkManager | LOW | TICKET (confirm family) | url-row 6px, button 4px. |
| Modal.svelte (shared) | MED | TICKET (confirm family) | `.modal` bg-secondary + no shadow → app-wide fix for ~20 dialogs. |
| ConfirmDialog / ConfirmationModal / DoubleConfirmDialog / TypeToConfirmDialog / TypedConfirmationModal | LOW | TICKET (confirm family) | title no display font, 4px button/input radii, flat cancel-btn. |

### System
| Component | Dev | Bucket | Notes |
|---|---|---|---|
| DiagnosticsPanel | HIGH | ND (+crimson→sweep) | dev readout: utilitarian or card? `crimson` literal + legacy alias. |
| FeedbackModal | MED | TRIVIAL | `crimson` literal, modal not surface-raised, title/textarea type. |
| NotificationSettingsPanel | MED | TRIVIAL | select 4px, header h3 no display font (coupled w/ SettingsPanel). |
| BridgingPanel | MED | TICKET | half-done: card/chip radii inconsistent, bespoke agree/diversity chips. |
| QuotaBar | LOW | TRIVIAL | "getting full" warning uses red, should be clay. |
| StorageBuddyList | LOW | TRIVIAL | rows/select 4px, header h3 no display font. |
| ToastHost | NONE | NONE | pure layout (real chrome in Toast.svelte). |

## 4. This track's mechanical sweep

Every change below is a single-file, pinned-target swap with no design question and no
shared-chrome blast radius. TICKET/ND files appear here **only** for their isolated stray literal
(the "fix what Track A missed" goal). All replacement tokens verified present in `app.css`.

**Stray literals (also ratchets the allowlist — 5 files → 0):**
- `NetworkHealthView.svelte`: L457 `orange`→`var(--net-warn-fg)`; L460/471/497/515 `crimson`→`var(--net-danger-fg)`; L464 `1px solid crimson`→`1px solid var(--net-danger-fg)`; L468 `rgba(220,20,60,.06)`→`var(--net-danger-bg)`. (allowlist 2→0)
- `DmCreateDialog.svelte`: L229 `#f99`→`var(--gov-clay)`. (1→0)
- `FriendsPanel.svelte`: L1405 `#e0a23c`→`var(--warning)`. (1→0)
- `PendingJoinsPanel.svelte`: L277 `#c33`→`var(--danger-text-muted)`. (1→0)
- `MailCompose.svelte`: L190 `#e55`→`var(--mail-error-text)`; L167 input radius 4px→5px. (1→0)
- `DiagnosticsPanel.svelte`: L232 `crimson`→`var(--danger)` (uncounted by the guard — hygiene).
- `FeedbackModal.svelte`: L285 `crimson`→`var(--danger)`.

**Trivial-bucket full fixes:**
- `FeedbackModal.svelte`: `.modal-content` bg `var(--bg-secondary)`→`var(--surface-raised)` + add `border: 1px solid var(--border)` + `box-shadow: var(--shadow-e3)`; `.modal-content h2` add `font-family: var(--font-display)`; textarea `font-mono`→`var(--font-ui)`, radius 4px→5px.
- `NotificationSettingsPanel.svelte`: `.policy-row select` radius 4px→5px; `.settings-header h3` add `font-family: var(--font-display)`. **Also `SettingsPanel.svelte`'s matching `h3`** (documented sibling — keep in sync; verify it lacks the rule first).
- `QuotaBar.svelte`: `.quota-fill.warning` `var(--danger-muted)`→`var(--gov-clay)`.
- `StorageBuddyList.svelte`: `.buddy-row` + `.peer-picker` radius 4px→5px; `.buddy-list-header h3` add `font-family: var(--font-display)`.
- `IncomingCallToast.svelte`: card radius 6px→8px; `box-shadow: 0 4px 12px var(--shadow-soft)`→`var(--shadow-e3)`.
- `CallInProgressBar.svelte`: `.elapsed` add `font-family: var(--font-mono)`; `.ctrl` + `.btn-end` radius 4px→5px.
- `GroupCallBar.svelte`: `.elapsed` add `font-family: var(--font-mono)`; `.ctrl` + `.btn-leave` radius 4px→5px.
- `GroupCallBanner.svelte`: `.btn-join` radius 4px→5px.
- `MentionAutocomplete.svelte`: dropdown bg `var(--bg-secondary)`→`var(--surface-raised)`, radius 6px→8px + add `box-shadow: var(--shadow-e2)`; `.option button` radius 4px→5px.
- `NamedEmojiPicker.svelte`: `.named-search, .named-rename` add `border: 1px solid var(--border); border-radius: 5px; background: var(--input-bg); color: var(--text-primary); font: inherit;`.
- `ThreadView.svelte`: `.thread-close` radius 4px→5px.
- `FloatingThreadBar.svelte`: `.entry-count` add `font-family: var(--font-mono)`; `.thread-entry` radius 4px→3px.
- `ChannelMessageFeed.svelte` (reaction rules only): `.reaction-count` add `font-family: var(--font-mono)`; `.reaction-toolbar` bg→`var(--surface-raised)`, radius 6px→8px, `box-shadow: 0 1px 4px var(--shadow-mid)`→`var(--shadow-e1)`; `.reaction-picker` bg→`var(--surface-raised)`, radius 6px→8px, `box-shadow: 0 2px 8px var(--shadow-strong)`→`var(--shadow-e2)`; shared toolbar-button radius 4px→5px. (Leave `.reaction-chip` 10px — ND.)
- `ProfilePopover.svelte`: bg `var(--bg-tertiary)`→`var(--surface-raised)`; `box-shadow: 0 4px 16px var(--shadow-mid)`→`var(--shadow-e2)`; `.popover-name` add `font-family: var(--font-display)`.
- `ProfilePanel.svelte`: `.panel-name` add `font-family: var(--font-display)`.
- `TrustEditor.svelte`: `.editor-heading` add `font-family: var(--font-display)`; `.level-value` + `.clear-button` radius 4px→5px.
- `SensitivityBadge.svelte`: `.sensitivity-badge` radius 4px→3px.
- `NodeDetail.svelte`: `.address-chip` radius 10px→3px; `.link-item` radius 4px→5px; `.node-name` add `font-family: var(--font-display)`.
- `NetworkDiscoverabilitySettings.svelte`: `.relay-input` radius 3px→5px.
- `SpellbookMode.svelte`: `.level-selector select` + `.express-selector select` radius 4px→5px; `.tab-btn` padding `6px 16px`→`8px 16px`.
- `FlashcardView.svelte`: `.hint-toggle` radius 4px→5px; `.ptt-container` gap 6px→8px. (Leave `.ptt-hint.error` color — ND.)
- `FlashcardGrid.svelte`: `.grid-row` radius 6px→5px; `.byte-cell` radius 4px→3px, padding `4px 6px`→`4px 8px`.

**After the edits:** regenerate the allowlist with `UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run
src/style-token-guard.test.ts`; the diff must be **removal-only** (the 5 files above drop out;
nothing added). Then the full gate: `npx tsc --noEmit && npx vitest run`.

## 5. Follow-up tickets (all children of ZEB-603)

**Restyle (TICKET bucket):**
- **ZEB-651** — Network mode: card scaffolding + shared relay-badge → StatusPill.
- **ZEB-652** — FriendsPanel: chip idiom + radii + button anatomy.
- **ZEB-653** — PendingJoinsPanel: Svelte-5 migration + card chrome + CountChip.
- **ZEB-654** — Mail cluster: px spacing grid + font-display + radii.
- **ZEB-655** — BridgingPanel: finish the half-done restyle (cards + CountChip).
- **ZEB-656** — confirm-dialog family + `Modal.svelte` shared elevation (the deferred "lead
  surface"; app-wide blast radius → needs a visual smoke-test).

**Design session (NEEDS-DESIGN, grouped):**
- **ZEB-657** — Commons H deferred design decisions (10 questions: NetworkGraph heat-map on paper,
  call-control deafen valence, DM library-blue vs sage, TrustBadge dot-vs-chip, UntrustedMediaCard
  caution recipe, ProfileEditor card-vs-flat, DiagnosticsPanel utilitarian-vs-card, reaction-chip
  anatomy, CountChip danger-tone, FlashcardView mic-denied color).

**Guardrail hardening:**
- **ZEB-658** — `style-token-guard`: catch `crimson` (+ other missing CSS named colors).

## 6. Guard blind spot (found during the audit)

`style-token-guard.test.ts`'s named-color regex **omits `crimson`**: `NetworkHealthView` carried 5
uncounted `crimson` literals while its allowlist budget of 2 only caught `orange` + one `rgba()`.
The sweep fixes the specific instances; ZEB-658 closes the detection gap.
