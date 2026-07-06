# ZEB-609 — Commons F: fork & lineage restyle — design

**Ticket:** ZEB-609 (Commons F: fork & lineage restyle) · parent epic **ZEB-603** (Commons design adoption)
**Branch:** `zeb-609-commons-f-fork-lineage` off `main` @ `2c7fb60b`
**Design reference:** `docs/design/commons/references/Harmony Forks & Lineage.dc.html` (4 frames A–D); screenshots `docs/design/commons/references/screens/06-forks-lineage.png`, `10-lineage-prototype.png`
**Depends on:** ZEB-605 (Commons token flip — the sage/clay `var()` layer already ships; the style-token guard forbids raw hex). Follows the C→D→E restyle cadence (ZEB-606/607/608).
**Scope decision (approved):** *Honest restyle + follow-up.* Restyle every surface that is genuinely backed by data; drop or defer everything the mock invents; file a separate follow-up ticket for the fork-reason data model + the 2D genealogy graph.

---

## §0 — Ticket-premise corrections (verified against code)

The `.dc.html` shows data the app does not persist. Per the Commons convention (**functionality wins over mock**), F renders only what is real and states the gap here. Verified against `src/lib/types.ts:345-378`, `src/lib/fork-timeline.ts`, `src/lib/components/ForkConfirmDialog.svelte`.

1. **Fork "why"/reason — DEFERRED (no data model).** The mock's centerpiece is a mandatory "Why are you forking? — recorded on the lineage, permanently." No `reason` field exists on `CommunityLineageDto`, `ParentLineageDto`, `ForkDescendantDto`, `ForkDivider`, or the `onConfirm` payload (`{ name, silent, alsoLeave }`). Capturing + persisting it is a cross-repo feature (dialog → `forkCommunity` IPC → harmony-core Fork event → materialized lineage). **F does not add the "why" field.** It goes to the follow-up ticket.
2. **Sage=amicable / clay=dispute coding — DROPPED (no classifier).** The mock color-codes each fork/edge as amicable (sage) or dispute (clay). No such field exists anywhere; the nearest real signal is the `silent` boolean, which is a notification choice, not a dispute flag, and is not even persisted onto the lineage. **F does not classify forks.** Clay stays the structural "fork" accent (`⑂`, divider, fork CTA); sage stays the "you / member" accent (self card, ✓ Member). Neither encodes dispute-vs-amicable.
3. **Per-fork member counts — DROPPED (unknowable).** "142 members", "Direct forks 2", "38 members" appear on node cards and the inspect panel. Fork DTOs carry no roster or member count; for a fork you are not a member of it is unknowable client-side. **F shows no member counts.**
4. **"signed by 38 founders" — DROPPED (no signer data).** The divider sub-line in the mock names a signer count. No signer set is carried on `ForkDivider`. **F omits it.**
5. **Forker display names — unchanged stub.** `ForkDescendantDto.forkerDisplayName` is typed `string | null` but is *"Phase 2: always null pending ZEB-281"*, so descendant rows render `forkerDisplayName ?? 'an unknown member'` today. **F keeps that exact fallback** — no change until ZEB-281 lands.
6. **"N messages carried" on the divider — KEPT (real).** `ChannelMessageFeed` already receives `snapshotMessages: ChannelMessageDto[]`; `snapshotMessages.length` is the honest carried-count. F surfaces it on the restyled divider band.
7. **✓ Member / not-joined badges — KEPT (real).** `ForkDescendantDto.locallyKnown` + membership in `localNavIds` already gate clickability; F promotes that same real signal to an explicit "✓ Member" / "not joined" badge.
8. **2D genealogy graph + inspect panel — NOT BUILT (form decision).** The mock is a 2D SVG node-graph (root up top, connectors fanning to horizontally-spread child cards, a right-hand "INSPECTING" panel). Almost everything that makes that graph compelling — member counts, dispute/amicable edge labels, reasons — is dropped by items 1–4. The current `ForkLineageTree` is a flat `role="tree"` list whose semantics tests pin. **F restyles the existing vertical `role=tree` rows into Commons node cards** (no net-new layout engine). The full 2D graph + inspect panel is folded into the follow-up ticket (build the compelling graph once there is compelling data to fill it).

**Follow-up ticket to file** (scope A's other half): *"Fork reason & richer lineage — capture mandatory 'why' in the fork dialog → `forkCommunity` IPC → harmony-core Fork event → persist a `reason` on the lineage; then the 2D genealogy graph + inspect panel + per-fork reason surfacing."* Reference this design's §0.

---

## §1 — Design decisions

### D1 · Tokens: zero new hex

Every design color maps to an existing `var()` token (all present in `src/app.css`, light + dark) or a guard-whitelisted `color-mix(in srgb, var(--x) N%, …)`. **No new hex, no new `app.css` tokens.** The sage↔clay pair the restyle needs:

| Role | Token | Light | Dark |
|---|---|---|---|
| Sage deep ink (pill text, member lead) | `--primary-deep` | `#2f4a35` | `#7fa886` |
| Sage tint bg (You-are-here / ✓ Member pill, active) | `--primary-soft` | `#e4ece2` | `#2a342a` |
| Sage border (member card, consent box) | `--primary-border` | `#c9d6c6` | `#3c4a3d` |
| Sage accent (self card 2px border, filled CTA) | `--accent` | `#466b4c` | `#7fa886` |
| Clay (`⑂` glyph chip, divider, fork CTA fill) | `--gov-clay` | `#b9742c` | `#d39450` |
| Clay soft bg (divider band, fork button, clay chip) | `--gov-clay-soft` | `#f1e2cc` | `#3a2f1f` |
| Clay deep text (divider title) | `--gov-clay-deep` | `#5a4321` | `#e2b888` |

Supporting tokens in scope: surfaces `--surface-raised` (cards), `--bg-primary` / `--bg-secondary` (canvas / panel), `--paper` (window), `--line-soft` (card hairline), `--border` / `--border-default`; text `--text-primary` / `--text-secondary` / `--text-muted` / `--faint` / `--text-bright`; type `--font-display` (Newsreader — names/headers), `--font-ui` (Public Sans — labels/body), `--font-mono` (IBM Plex Mono — IDs/dates/counts); elevation `--shadow-e1`/`-e2`/`-e3`; `--overlay` (dialog scrim); `--surface-highlight` (existing self-row tint).

**Design colors with no dark value are remapped, not adopted.** The mock's cool "trust-blue" consent callout (`#e8eef0` / `#3d5560`) is the same family D/E explicitly rejected → **remap to the sage/primary trio** (`--primary-soft` bg, `--primary-border` border, `--primary-deep` text). Clay-tan borders (`#e6cfa3` etc.) → `color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised))` (the guard-encouraged idiom, per ZEB-608 §D4).

### D2 · Primitives

Reuse the existing governance primitives where they fit; add nothing that only one surface needs.

- **Lineage badges** (`● You are here`, `✓ Member`, `not joined`) are lineage-specific and stay **bespoke markup inside `ForkLineageTree`** — `<span class="lineage-badge you-here|member|not-joined">`. The sage pills use `--primary-deep` on `--primary-soft` (radius 20px, mono 600 ~10px), matching `StatusPill`/`RoleBadge` grammar without forcing a new `StatusPill` variant. "not joined" is muted `--faint` text (no pill), matching the mock.
- The divider's inline **facilitator** tag (mock Frame C) is out of scope for F — it belongs to message-row role rendering, not the fork surfaces.
- `CountChip` / `PipMeter` / `TallyBar` are **not** used (member counts / k-of-n dropped). No new shared primitive is introduced.

---

## §2 — Per-surface specs

### Surface 1 — `ForkLineageTree.svelte`

**File:** `src/lib/components/ForkLineageTree.svelte` (currently 188 L, a flat `<ul role="tree">` of indented `<li>` rows).
**Interface — unchanged:**
```ts
lineage: CommunityLineageDto;
descendants?: ForkDescendantDto[];
localNavIds?: Set<string>;
resolveLocalName?: (spaceId: string) => string | null | undefined;
onNavigate?: (spaceId: string) => void;
```

**Restyle (markup + tokens only; no interface / data change):** keep `<ul role="tree" aria-label="Fork lineage tree">` and one `<li role="treeitem">` per ancestor + self + descendant. Turn each row from a flat indented line into a Commons **node card**:

- Card: `--surface-raised` bg, radius ~11px, `--shadow-e1`; a letter-avatar chip (first letter of the name) + name in `--font-display`; a mono `--font-mono` sub-line (`root · founded {date}` for the root/self, `forked {date}` for others); a hairline `border-top: 1px solid var(--line-soft)` footer holding the relationship badge.
- **Relationship coding (structural — see §0.2):**
  - **Self / "You are here"** (`lineage.selfName`, `aria-current="page"`): `border: 2px solid var(--accent)`, accent-tinted `--shadow-e1`, avatar `--accent`; footer badge `<span class="lineage-badge you-here">● You are here</span>` (sage: `--primary-deep` on `--primary-soft`). Retains the "You are here" copy.
  - **Descendant, member** (`desc.locallyKnown && localNavIds.has(desc.forkSpaceId)`): `border: 1px solid var(--primary-border)` (sage); footer `<span class="lineage-badge member">✓ Member</span>`; rendered as `<button class="lineage-clickable">` firing `onNavigate(desc.forkSpaceId)` (unchanged behavior). Name = `resolveLocalName?.(desc.forkSpaceId) ?? truncSpaceId(desc.forkSpaceId)`.
  - **Descendant, not joined:** `border: 1px solid var(--border)`; footer `<span class="lineage-badge not-joined">not joined</span>` (muted `--faint`, no pill); rendered as `<span class="lineage-unknown" title="You're not a member of this fork.">` (unchanged — **zero `<button>`s**). Name = `truncSpaceId(desc.forkSpaceId)` (format `0x{first8}…`).
  - **Ancestors** (`lineage.parentLineage`): `border: 1px solid var(--border)` cards, `↳` prefix glyph, frozen `entry.name` + optional mono date; clickable `<button>` iff `localNavIds.has(entry.spaceId)`, else `<span class="lineage-unknown" title="You're not a member of this community.">`.
  - **Truncation row** (`…and {N} earlier ancestors`) and **empty hint** (`(no forks yet)`, `aria-hidden`) retained verbatim.
- **Connector feel** without SVG: a CSS rail via `::before` on the tree / rows (vertical hairline in `--line-soft` or clay-tan `color-mix`) plus the existing `↳`/`←` glyphs. Indentation via the existing `padding-left: calc(depth * 1.5rem)` stays.
- **Preserve byte-identical:** roles (`tree`/`treeitem`), `aria-current="page"` on self, `aria-label="Fork lineage tree"`, all copy ("You are here", "(no forks yet)", "…and N earlier ancestors", "an unknown member"), the `0x{first8}…` truncation, and the button-vs-span clickability gating.

### Surface 2 — Fork divider in `ChannelMessageFeed.svelte`

**File:** `src/lib/components/ChannelMessageFeed.svelte` — divider markup ~901–908, `.fork-divider` styles ~1474–1503, pre-fork tint at ~914 (`class:pre-fork`) + `.pre-fork-badge` ~1484–1488. Fed by `buildUnifiedTimeline()` in `src/lib/fork-timeline.ts`.
**`ForkDivider` shape — unchanged (do not rename):** `{ kind: 'fork-divider'; originalCommunityName: string; forkedAtMs: number }`.

**Restyle:** replace the plain centered `───── Forked from {name} on {date} ─────` line with the Commons **clay card band**:
- Container: `background: var(--gov-clay-soft)` (warm clay-cream), `border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised))` (clay-tan), radius ~10px, `--shadow-e1`, `margin: 4px 16px`.
- A `⑂` glyph in a ~26px chip: `background: var(--gov-clay)`, glyph `--text-bright`, radius ~7px.
- Title (Public Sans 600, `var(--gov-clay-deep)`): **"Forked from {originalCommunityName}"**.
- Mono sub-line (`--font-mono`, `--text-muted`): **"{date} · {N} messages carried"** where `date = new Date(row.forkedAtMs).toLocaleDateString()` and `N = snapshotMessages.length` (real; see §0.6). No reason quote, no "signed by N founders".
- **Preserve:** `role="separator"` and `aria-label="Forked from {row.originalCommunityName}"` byte-identical.
- **Pre-fork rows:** keep the existing `.channel-message.pre-fork` opacity, `.pre-fork-badge` ("from {originalCommunityName}"), and disabled reactions — token-polished only. Keep the existing opacity value (no test pins it; minimal diff to a ~1500-line component).
- **Out of scope for F:** the mock's separate "carried from {original}" hairline marker above the pre-fork block — it is redundant with the divider band's own "Forked from {name}" title, and adding it means further surgery on the large feed component for negligible fidelity gain. Noted as possible minor polish only.

### Surface 3 — `ForkConfirmDialog.svelte`

**File:** `src/lib/components/ForkConfirmDialog.svelte` (194 L). Substrate: `Modal.svelte` (`role="dialog"`, `aria-modal`, `use:trapFocus`) + `TypedConfirmationModal` for the also-leave path.
**Interface — unchanged:** `originalName`, `messageCount`, `onConfirm(opts: { name; silent; alsoLeave })`, `onCancel`.

**Restyle (Commons chrome; every pinned anchor preserved):**
- **Header:** a ~38px clay chip (`--gov-clay-soft` bg, `⑂` in `--gov-clay`) + title in `--font-display`. **Heading copy stays "Fork this community"** (test pins `/fork this community/i`; do **not** change to "Fork {name}") + sub-line "Branch a new community. The original is never affected." Keep `id={titleId}` / `aria-labelledby`.
- **Name field:** label "Name:" (pins `/name/i`), `id="fork-name"`, default value `{originalName} (fork)` (pins prefill "Cool Community (fork)"). Add a **sage focus ring**: `border-color: var(--accent)` + `box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 12%, transparent)` on focus.
- **Real controls, restyled (copy byte-identical):** the two checkboxes "Fork silently (don't tell other members)" (pins `/fork silently/i`) and "Also leave the original community" (pins `/also leave/i`), styled as Commons checkbox rows.
- **Honest "carries over" context:** keep the existing snapshot line — `>0` → "Snapshot will include ~{messageCount} messages." (pins `/1247 messages/`), `0` → "Snapshot will include your accessible message history (up to 5000 messages)." (pins the exact parenthetical) — and add **one** informational line: "A frozen snapshot of every channel is always included." No fabricated interactive checklist items.
- **Consent callout:** the mock's trust-blue box **remapped** to sage/primary (`--primary-soft` bg, `--primary-border` border, `--primary-deep` text), with accurate copy (the forker becomes the new community's owner — true; forking writes an immutable divider — true). **No "why" field.**
- **Actions:** "Cancel" outline (pins `/cancel/i`) + "Create fork" clay-filled `--gov-clay` on `--text-bright` (pins `/create fork/i`).
- **Also-leave second stage:** `TypedConfirmationModal` unchanged — title "Leave and fork community?", `requiredText="leave"`, input `aria-label` "Type to confirm", confirm "Confirm" (pins `/type.*leave/i`, `/type to confirm/i`, `/confirm/i`), disabled until exact match.
- **Payload unchanged:** `onConfirm({ name, silent, alsoLeave })`.

### Surface 4 — Settings → Lineage (Forks) section

**File:** `src/lib/components/CommunitySettingsPanel.svelte` — `.forks-section` at ~532–559 (also mounts `ForkConfirmDialog` at ~638–650).
**Props consumed (unchanged):** `onFork`, `lineage` (`{ originalCommunityName; forkedAtMs; snapshotMessageCount } | null`), `phase2Lineage: CommunityLineageDto | null`, `descendants`, `localNavIds`, `onForkLineageNavigate`, `resolveLocalCommunityName`.

**Restyle:** bring the `.forks-section` to the Commons card look (`--surface-raised`, `--border`, radius, `--shadow-e1`; header in `--font-display`). **Preserve byte-identical:** section label **"Forks"** (`getByText('Forks')`), the `.forks-explainer` copy substrings (`/Any member of a community can fork it at any time/`, `/…preserve continuity if members want to take/`), the `ForkLineageTree` mount (with `resolveLocalCommunityName` reaching it), and `button.fork-this-community` with text "Fork this community".

**Honest addition (§0-backed, Frame D):** when `phase2Lineage.forkedFrom` is set, render a **"This is a fork of {parent}"** callout above the tree — sage/primary tint (`--primary-soft` / `--primary-border` / `--primary-deep`), avatar chip, parent name resolved from `phase2Lineage.parentLineage` (nearest ancestor). Gated strictly on real data (root communities show nothing). **No** dedicated full-screen lineage route / "View full lineage tree →" (net-new; the tree stays embedded).

---

## §3 — Tests, guards, constraints

### Test lockstep (pinned selectors byte-identical)

Extend the component tests for the new markup (cards, badges, divider band, "fork of" callout) while keeping **every** pinned anchor byte-identical. Existing pins to preserve, by file:

- **`src/lib/components/__tests__/ForkLineageTree.test.ts`** — `role="treeitem"` count = ancestors + 1 + descendants; `[aria-current="page"]` self row; `aria-label="Fork lineage tree"`; copy "You are here", `/no forks yet/i`, `/and 2 earlier ancestors/i`, "an unknown member"; truncation `0x{first8}…`; locally-known → `<button>` fires `onNavigate` with exact hex; unknown → **zero** `<button>`s; `resolveLocalName` called only for locally-known descendants; resolver-null → truncated hex. **Add:** badge assertions (`● You are here`, `✓ Member`, `not joined`) keyed to the real `locallyKnown`/`localNavIds` signal.
- **`src/lib/components/__tests__/ForkConfirmDialog.test.ts`** — heading `/fork this community/i`; label `/name/i`; `/fork silently/i`; `/also leave/i`; snapshot `/1247 messages/` and `/accessible message history \(up to 5000 messages\)/i`; prefill "Cool Community (fork)"; buttons `/create fork/i`, `/cancel/i`, second-stage `/confirm/i`; `[role="dialog"]` Escape; `.modal-overlay` backdrop dismiss; second stage `/type.*leave/i`, input aria `/type to confirm/i`, requiredText "leave"; `onConfirm` payload exactly `{ name, silent, alsoLeave }`.
- **`src/lib/__tests__/fork-timeline.test.ts`** — logic-only; `ForkDivider` `{ kind, originalCommunityName, forkedAtMs }` and `TimelineMessage` `{ msg, isPreFork }` field names unchanged; insertion/positioning/HLC ordering untouched.
- **`src/lib/components/__tests__/CommunitySettingsPanel.test.ts`** — `getByText('Forks')`; explainer substrings; `.forks-section`; `button.fork-this-community` text "Fork this community"; `resolveLocalCommunityName` reaches `ForkLineageTree`.
- **`src/lib/components/__tests__/ChannelMessageFeed.test.ts`** — `article.channel-message.pre-fork` exists for pre-fork rows; `.reaction-toolbar` absent in a pre-fork article. **Add:** a divider-band test seeding both snapshot + live messages so a divider renders — assert the `⑂` band, `role="separator"`, `aria-label="Forked from {name}"`, and the real "{N} messages carried" count.

**Regression pins for the honest subset:** assert no fabricated member counts appear; assert the divider carried-count equals `snapshotMessages.length`; assert descendant "not joined" rows render zero buttons.

### Guards & allowlist

- **`src/style-token-guard.test.ts`** — none of the four files are in `src/style-token-allowlist.json`, and all are already fully tokenized. The restyle must introduce **zero** raw color literals in any `<style>` block (budget 0 → any hex/rgb/named color fails). Use `var(--…)` or the whitelisted `color-mix(in srgb, var(--x) N%, …)` idiom (and `transparent`, which the guard ignores). If any *other* allowlisted file is incidentally touched, ratchet **down** only via `UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts` (removals-only diff). Never regenerate to admit new hex.
- **`src/commons-hex-guard.test.ts`** — must stay empty; introduce none of the 8 forbidden Discord-palette hex anywhere (`.svelte`/`.ts`/`.css`/`.html`, including script/markup).

### Constraints (binding)

- **Frontend gates:** `npx tsc --noEmit && npx vitest run` (from repo root) must pass. No Rust changes (F is frontend-only).
- **Svelte 5 runes** throughout (`$props`, `$state`, `$derived`, `$effect` with cleanup).
- **One PR**, commit per task, **no worktrees** (`git checkout -b` in the main repo).
- **Untouched seams:** the `ForkDivider`/`TimelineMessage` field names and `fork-timeline.ts` logic; the `onConfirm({name,silent,alsoLeave})` contract; the `ForkLineageTree` prop interface; ZEB-605/606/607/608 surfaces (nav shell, governance panels, tabs).
- **No invented fork data** (§0). No new `app.css` hex tokens. No cross-repo / harmony-core changes (those are the follow-up ticket).

---

## §4 — Task shape (for the plan)

Four SDD tasks, one per surface, each independently testable:

1. **`ForkLineageTree` card-row restyle** — node cards, relationship coding, lineage badges, connector rail; extend `ForkLineageTree.test.ts`.
2. **Fork divider band** — clay card band + real messages-carried count in `ChannelMessageFeed`; add the divider-render test.
3. **`ForkConfirmDialog` Commons chrome** — clay header, sage focus ring, restyled real controls, remapped consent callout (no "why"); preserve `ForkConfirmDialog.test.ts` pins.
4. **Settings Lineage section** — `.forks-section` card chrome + honest "This is a fork of {parent}" callout; preserve `CommunitySettingsPanel.test.ts` pins.

Plus: **file the follow-up ticket** (§0) for the fork-reason data model + 2D genealogy graph.
