# Commons H — Deferred Design Decisions (resolved)

> **Track:** ZEB-657 (design session), child of the Commons design-adoption epic ZEB-603.
> **Date:** 2026-07-07 · **Branch:** `zeb-657-commons-h-deferred-design-decisions` off `main`.
> **Source:** the 10 NEEDS-DESIGN calls surfaced by the ZEB-611 gap-fill audit
> (`docs/design/commons/gap-fill-audit.md` §5).
> **Method:** each call was grounded in the live code + the Commons anatomy rubric (audit §2),
> then resolved in session. This document is the durable decision record; implementation is
> downstream (§4).

## 1. The governing principle — the valence ladder

Most of these calls are one question wearing ten hats: **when does a surface earn clay
(caution) vs red (danger)?** The Commons rubric (audit §2) already fixes the endpoints —
sage = active/consensus/brand, clay = "attention / act-now, used sparingly", red =
"against / destructive / failed only" — but never says how *severity* or *chosen-vs-failure*
states map onto them. This session fixes that:

> **neutral → sage (permissive-active / consensus) → clay (a caution state the user *chose* or
> should *watch*) → red (a *failure/blocker* the user did not choose, or a *critical* threshold).**

The distinguishing test between clay and red: **did the user opt into this state, and is the
feature still working?** A deafened call control (chosen, working) is clay. A denied mic that
blocks Practice entirely (not chosen, broken) is red. A metric at 88% (elevated, watch it) is
clay; at 97% (failure-adjacent) it is red.

Five decisions (#1, #2, #5, #9, #10) fall out of this ladder — though **#1 and #9 turn out to
serve only the mock-dominated Network Viz surface and are recorded-but-deferred** (§3, §4); the other five
(#3, #4, #6, #7, #8) are pure form/anatomy calls.

## 2. Guardrails (bind every change below)

- **Budget-0 color tokens.** No new hex/rgb/hsl/named-color literals. Every color is a
  `var(--*)` **already defined** in `src/app.css`. Each token named below was verified present
  in both the light (`:root`) and warm-dark blocks. A mistyped `var(--token)` renders nothing
  and **no test catches it** — verify each token resolves before shipping.
- **Allowlist ratchets down only.** `src/style-token-allowlist.json` stays byte-identical or
  shrinks; never grows. The ND surfaces' stray literals were already swept in ZEB-611 (§4).
- **Radius scale** (audit §2): chips/tags/ID-badges 3px; inputs/rows 5px; cards/panels 8px;
  round affordances (status pill, avatar, count badge, reaction) 20px / `50%`.
- **Type** (audit §2): display headers → `--font-display`; UI chrome/buttons → `--font-ui`;
  IDs/tallies/timestamps → `--font-mono`.
- **Gates:** `npx tsc --noEmit && npx vitest run` clean; `style-token-guard` green; any test
  that pins a changed color/label (`graph-utils.test.ts`, TrustOverview/TrustBadge,
  NodeDetail) updated in the same change.

## 3. The ten resolved decisions

### Theme A — the valence ladder

#### 9. CountChip danger-tone + three-tier severity ladder — **RECORDED, DEFERRED (mock-dominated consumer)**

**Correction (2026-07-07 review):** the audit framed this as "the keystone gating ZEB-653/655."
That framing was wrong. `governance/CountChip.svelte` already ships `sage | clay | neutral`, and
both consumers use exactly those: ZEB-653's "Awaiting counter-sign (N)" / "Recent joins (N)" are
**neutral** tallies; ZEB-655's agree-count is **sage**, diversity% is **neutral**. Neither needs a
danger tone, so **neither was ever blocked** — both proceed today against the existing CountChip.

The `danger` tone + the three-tier resource-severity ladder (CPU/Mem/Disk 85/95) have **exactly one
consumer: `NodeDetail`**, which renders **only** in the standalone "Network Viz" webview window
(`network-main.ts` → `NetworkApp.svelte`). That window always seeds 8–12 fabricated
`MockNetworkDataService` nodes and — under Tauri with Zenoh connected — *merges* live-discovered
nodes in (`NetworkApp.svelte:94` `mergeNodes`); the fabricated nodes always dominate, so the graph
is not a faithful topology and its metric tiles are largely synthetic. `NetworkHealthView` (the real
IPC surface) shows relay/transport *states*, not resource percentages — so it is **not** a consumer.
Building a resource-severity ladder for this mock-dominated surface isn't worth it now.

**Decision:** **record the design, defer the build.** When a real resource-severity surface exists,
apply: CountChip gains a 4th `danger` tone (`.danger { background: var(--status-recalled-bg); }` /
`.danger .cc-value { color: var(--danger-deep); }` — zero new tokens, reusing the failing/recalled
pair) driven by a shared `severityTone(pct): 'neutral' | 'clay' | 'danger'` helper with the ladder
`neutral <85 · clay 85–95 (elevated) · danger ≥95 (critical)`. Do **not** build it against
`NodeDetail`'s mock data now.

**Downstream:** ZEB-653 / ZEB-655 are **unblocked and independent of this doc**. (ZEB-655's
low-confidence "bridging score = positive consensus → sage not clay?" note is a separate optional
valence call, not resolved here.)

#### 2. Call-control valence — clay for restrictive-engaged

**Decision:** In the call bars, an *engaged* **restrictive** control (mic **muted** / audio
**deafened**) reads **clay**; an *engaged* **permissive** control (live mic, live speaker) keeps
**sage**.

**Rationale:** Today both light `--accent` (sage = "active/positive"), so a deafened bar looks
identical to a live one — sage misdescribes "you have cut your audio." Clay ("caution: audio
cut") is a chosen, working state, not a failure → clay, not red.

**Change:** `CallInProgressBar.svelte`, `GroupCallBar.svelte`, `VoiceChannelView.svelte` — the
active-state fill/outline of the mute + deafen toggles switches `--accent` → `--gov-clay` (or
`--gov-clay-soft` fill / `--gov-clay-deep` glyph, matching the surrounding control weight); live
mic/speaker toggles stay `--accent`. Idle/off controls unchanged.

#### 5. UntrustedMediaCard — clay attention-card + clay confirm

**Decision:** Adopt the **attention-card recipe** (`--surface-raised` + `--border` +
`border-left: 3px solid var(--gov-clay)` + `--shadow-e1`) and colour the **"Confirm load"**
button **clay**, not sage.

**Rationale:** Untrusted media is the textbook "act deliberately" state — exactly what the
rubric's clay border-left card is for. A sage "Confirm load" reads "safe / go"; clay reads
"caution — you are revealing blocked content," which is honest. Still not red: loading is a
permitted, reversible choice, not a destructive action.

**Change:** `UntrustedMediaCard.svelte` — card container gets the attention recipe; the
confirm/reveal button uses clay (`--gov-clay` fill or outline per the card's button weight).

#### 10. FlashcardView mic-denied — promote clay → red

**Decision:** `.ptt-hint.error` (mic permission **denied**, which blocks Practice entirely)
changes from `--text-warning` (clay) to the **red** family — specifically
`--danger-text-muted`, a paper-legible muted red (the same idea as the mail-error text — the two
tokens coincide in the light theme and diverge slightly in warm-dark, so pin `--danger-text-muted`).

**Rationale:** A denied mic is an unchosen hard blocker that breaks the feature → red, per the
ladder. `--danger-text-muted` keeps the hint legible as body text on the light paper canvas
while unmistakably reading as an error (matching the mail-error convention), rather than the
harsher pure `--danger`.

**Change:** `FlashcardView.svelte` — `.ptt-hint.error { color: var(--danger-text-muted); }`.

#### 1. NetworkGraph heat-map — **RECORDED, DEFERRED (mock-dominated surface)**

**Correction (2026-07-07 review):** `NetworkGraph` renders **only** in the standalone "Network Viz"
webview window (`network-main.ts` → `NetworkApp.svelte`). That window always seeds 8–12 fabricated
`MockNetworkDataService` nodes and — under Tauri with Zenoh connected — *merges* live-discovered
nodes in (`NetworkApp.svelte:94` `mergeNodes`), but the fabricated nodes always dominate, so the
graph is not a faithful reflection of how the network coalesces. Restyling it now is low-value
while the topology is mock-dominated.

**Decision:** **record the design, defer the build.** If/when Network Viz is backed by real topology
data, apply: keep the intuitive green → clay → red load ramp (cool → hot) but source it from the
network token family (`--net-ok-* → --net-warn-* → --net-danger-*`, all already in `app.css`
L118–124 — **zero new tokens**) instead of the governance tokens, so graph-load semantics don't
couple to consensus/attention/against; add a min-contrast floor for the small filled circles on the
paper canvas; leave `capabilityColor` categorical (borrowing flashcard/category tokens for
distinctness is fine — YAGNI on a dedicated capability palette). Until then, no `graph-utils.ts` /
`NetworkGraph.svelte` changes.

**Related disposition:** the "Network Viz" window itself is mock-dominated — see §4 for the proposed
hide/gate follow-up.

### Theme B — form / anatomy

#### 3. DmCreateDialog — sage identity + recipient pills

**Decision:** Replace the Library-blue identity with **Commons sage**, and shape the recipient
chips as **20px pills** (they are removable person-tokens, not ID-badges).

**Rationale:** DM creation is a core social action, not a Library-feature surface — the
`--library-accent` blue is drift. The `.chip` elements render `labelFor(addr)` and are removable
on click (`DmCreateDialog.svelte:96–106`): that is a selection/recipient token → the 20px pill
affordance, not the 3px ID-badge.

**Change:** `DmCreateDialog.svelte` — `.chip` background
`color-mix(in srgb, var(--library-accent) 20%, transparent)` → `…var(--accent) 20%…`, radius
`12px` → `20px`; the primary/"Start DM" action button switches `--library-accent` → `--accent`
and its radius `4px` → `5px`.

#### 4. TrustBadge — dot → labeled chip (StatusPill anatomy)

**Decision:** Promote the bare 8px colour dot to a **labeled chip using StatusPill anatomy**,
keeping TrustBadge's own four-tone palette.

**Rationale:** TrustBadge renders only in `TrustOverview.svelte:138` — a **data table**, one
badge per row, encoding four levels (`low / cautious / trusted / highly` →
`--danger / --warning / --presence-online / --info`, from `trust-score.ts`). A colour-only dot
is a real accessibility gap for colour-blind sighted users (the `aria-label` only serves screen
readers), and a table has room for a label. The four trust tones do **not** map to StatusPill's
governance variants, so we adopt its *anatomy*, not its variant set.

**Change:** `TrustBadge.svelte` — render the derived `label` as **visible text** inside a
`span.trust-badge` styled as a pill (`display:inline-block; font-family:var(--font-ui);
font-weight:600; font-size:11px; padding:4px 11px; border-radius:20px`), with the existing
`trustScoreColor` driving the tone (background tint + readable foreground). Keep `role="img"` /
`aria-label` for parity. Verify `TrustOverview` row layout accommodates the wider chip; update
its test if it asserts the dot.

#### 6. ProfileEditor — flat inline section

**Decision:** Make `.profile-editor` a **flat inline section** (drop the box), matching
FriendsPanel's `.friends-section`.

**Rationale:** ProfileEditor renders as one section **inside** the Settings tabbed panel
(`SettingsPanel.svelte:148`), not a standalone modal. Its current
`background: var(--bg-secondary) + border-radius: 8px` (`ProfileEditor.svelte:512`) reads as a
half-card; promoting it to a real elevated card would nest a card inside the Settings panel. Flat
is the coherent choice for a sub-section.

**Change:** `ProfileEditor.svelte` — `.profile-editor` drops `background` and `border-radius`
(keep `gap`/`padding`); `.section-title` (`h3`) gains `font-family: var(--font-display)`.

#### 7. DiagnosticsPanel — keep utilitarian

**Decision:** **Leave it utilitarian** (dashed border, mono readout). No Commons card treatment.

**Rationale:** DiagnosticsPanel is a dev-mode raw readout. The mono/dashed "technical" aesthetic
is *intentional and appropriate* — it signals "diagnostic data, not a designed civic surface."
Newsreader headers + an e3 shadow would make debug output masquerade as a feature. Semantic
honesty + YAGNI: dev tools should look like dev tools.

**Change:** none beyond token hygiene already done in the ZEB-611 sweep (the `crimson` →
`--danger` swap). This decision closes the question; it does not schedule work.

#### 8. reaction-chip — 20px pill

**Decision:** `.reaction-chip` radius `10px` (magic, pill-by-coincidence) → **20px** (the
round-affordance token).

**Rationale:** A reaction chip (emoji + count) is a small round count-affordance; 20px makes the
pill intent explicit and height-independent, matching StatusPill. 3px would read as a tiny
button, which is wrong for a reaction.

**Change:** `ChannelMessageFeed.svelte` — `.reaction-chip { border-radius: 20px; }` (the
surrounding reaction toolbar/picker chrome was already handled in the ZEB-611 sweep §4).

## 4. Downstream implementation impact

This document is decision-only. Two of the ten calls (#1, #9) are **recorded but deferred** because
their only consumers live in the mock-dominated "Network Viz" webview (`NetworkGraph` / `NodeDetail`
— which seeds fabricated nodes and merges live Zenoh discoveries under Tauri, but the fakes always
dominate). The remaining eight are real-surface work.

**Already unblocked, independent of this doc:** ZEB-653 (PendingJoinsPanel) and ZEB-655
(BridgingPanel) use `CountChip`'s existing `sage`/`clay`/`neutral` tones — they were never blocked
by a danger-tone decision and can proceed now.

**Real-surface restyles from this session** — proposed as **two PRs** (per one-PR-per-repo /
bundle-small), each independently shippable and reviewable:
- **Valence PR:** #2 call bars (clay for restrictive-engaged) + #5 UntrustedMediaCard (clay
  attention-card + clay confirm) + #10 FlashcardView (mic-denied → red). One theme (the ladder).
- **Social-forms PR:** #3 DmCreateDialog (sage + recipient pills) + #4 TrustBadge (labeled chip) +
  #6 ProfileEditor (flat section) + #8 reaction-chip (20px pill).
- #7 DiagnosticsPanel: decision = *keep utilitarian* → **no work**.

**Deferred (mock-dominated, no work now):** #1 NetworkGraph heat-map; #9 CountChip danger tone +
three-tier severity ladder + NodeDetail tiles. Designs recorded in §3 for if/when the surfaces get
real data.

**Proposed follow-up (product call, outside Commons scope):** the "Network Viz" window
(`network-main.ts` → `NetworkApp.svelte`) is fabricated-node-dominated — it seeds
`MockNetworkDataService` nodes and merges live Zenoh discoveries under Tauri, but the fakes dominate,
so it does not faithfully reflect real network coalescence. Recommend a separate ticket to
**hide/gate its entry points** —
the `NavPanel.svelte` "Network Viz" affordance + the `network-viz` `WebviewWindow` — behind a dev
flag until it is backed by real data. Treatment (remove vs dev-flag-gate) is Jake's call.

New tickets for the two real-surface PRs to be filed as ZEB-603 children once this doc is approved.

## 5. Self-review

- **Coverage:** all 10 audit ND items resolved (§3 #1–#10); #1 and #9 recorded-but-deferred as
  mock-dominated, the other eight scoped for two real-surface PRs (§4). ✅
- **Token safety:** every token named (`--status-recalled-bg`, `--danger-deep`,
  `--net-ok/warn/danger-*`, `--gov-clay*`, `--accent`, `--danger-text-muted`,
  `--presence-online`, `--info`, `--surface-raised`, `--border`, `--shadow-e1`, `--font-display`)
  verified present in both `app.css` theme blocks. Zero new tokens. ✅
- **No contradictions:** the clay-vs-red split (#2 clay / #10 red) is governed by one stated test
  (chosen+working → clay; unchosen/broken/critical → red); #9's three-tier ladder is the same
  test applied to a continuous metric. ✅
- **Scope:** decision-only doc; implementation deferred to §4's grouped PRs + new tickets. ✅
