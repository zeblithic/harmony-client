# ZEB-644 / ZEB-645 / ZEB-646 — Commons a11y & polish bundle

**Parent epic:** ZEB-603 (Commons design system). **Branch:** `zeb-644-646-commons-a11y-polish`.
Three Low-priority review-debt follow-ups surfaced by the ZEB-605/606 whole-branch reviews,
bundled into one PR (all harmony-client, all a11y/polish). Design decisions approved by Jake
(on-accent swatch sign-off + segmented downgrade), 2026-07-08.

## Global constraints

- **budget-0 / style-token-guard:** no raw color literals in `.svelte <style>` blocks — only
  `var(--*)` or `color-mix` of vars. `src/style-token-allowlist.json` ratchets DOWN only,
  byte-identical. The guard scans `.svelte` files only; `src/app.css` is the sanctioned token
  layer (raw hex allowed there).
- **Gates:** `npx tsc --noEmit && npx vitest run` (repo root) + `style-token-guard` test. No Rust
  touched. Cargo gates unaffected.
- **No color literals enter `.svelte`:** the ZEB-644 sweep swaps `var(--x)` → `var(--on-accent)`
  (var→var), so the allowlist is byte-identical.

---

## ZEB-644 — foreground-on-accent contrast (`--on-accent` token)

### Decision (approved via swatch sign-off)

Text on `--accent` fills is sub-AA — **2.30:1** in dark (near-white `#f0ece2` on `#7fa886`),
**2.61:1** at the light sites that used `--text-primary`. Fix = a dedicated `--on-accent`
foreground token, **accent color unchanged**:

| theme | `--on-accent` | on `--accent` | on `--accent-hover` |
|-------|---------------|---------------|---------------------|
| light | `#ffffff` | `#466b4c` → **6.05:1** ✓ | `#2f4a35` → >6:1 ✓ |
| dark  | `#14160f` | `#7fa886` → **6.74:1** ✓ | `#97bd9d` → **8.76:1** ✓ |

These are the existing `--status-passed-fg` values (whose `-bg` *is* the accent) — a pairing the
design already validated. One token covers accent + accent-hover in both themes (verified: dark
on-accent is near-black, so the lighter accent-hover is *higher* contrast, not lower).

### Edits

1. **`src/app.css`** — add `--on-accent` to both theme blocks (near `--accent`):
   - `:root` (light): `--on-accent: #ffffff;`
   - `:root[data-theme='dark']`: `--on-accent: #14160f;`
2. **Sweep (77 sites)** — every solid `background: var(--accent)` fill that carries text/an icon
   glyph, change its foreground `color:` → `var(--on-accent)`. Enumerated by the 2026-07-08
   audit (41 `--text-bright`, 34 `--text-primary`, 2 `--bg-primary`). Categories: primary/confirm
   buttons, active/selected toggles & filter/section buttons, count & role badges
   (`.unread-badge`, `.tier-badge[data-power="100"]`), avatar-initial fills (`.chip-avatar`,
   `.community-chip`, `.owner-avatar`, `.node-avatar-self`, `.fork-of-avatar`), accept/join
   buttons, banners. The 2 `--bg-primary` sites (DelegationWidget `.dw-apply/.dw-confirm`,
   CommunityProposalsPanel `.np-submit`) already pass but are swept for single-source consistency.
3. **Add-color (4 sites)** — hover/active states that turn the fill accent but inherit
   `--text-primary` from their base rule (changing the base would recolor the non-accent state,
   so ADD a line to the state rule): NavPanel `.icon-button:hover`, MentionAutocomplete
   `.option.active button, .option button:hover`, MoreMenu `.more-icon-button:hover`,
   FeedbackModal `.actions button.primary`.
4. **Not touched:** `color-mix(... var(--accent) ...)` translucent tints (text sits on
   mostly-surface, contrast fine); `color: var(--accent)` used as text on non-accent grounds;
   accent used as border/bar/dot/progress/toggle-track (no text of its own). The 3
   `--accent-hover` `:hover` fills inherit the swept base color — no separate edit.

### Gate note
No `.svelte` color literals added → allowlist byte-identical. Contrast is CSS-value-only; the
regression net is the existing component suite + the guard.

---

## ZEB-645 — theme-switch repaint staleness + AppearanceSettings keyboard test

### Decision

`tokenColor()` (`theme-colors.ts`) clears its cache on `THEME_APPLIED_EVENT`, but `$derived`s
that call it have no reactive dependency on the theme, so they keep the previous theme's color
until their next data tick. Fix = a reactive `appliedTheme` store, referenced (dependency-touch)
in each theme-dependent derived so a theme flip invalidates it.

### Edits

1. **`src/lib/theme-service.ts`** — add `appliedThemeWritable: Writable<ResolvedTheme>` (init
   `'light'`); in `applyResolved(theme)` set it **after** `dispatchEvent(THEME_APPLIED_EVENT)`
   (so the cache is already cleared when consumers re-run); export
   `appliedTheme: Readable<ResolvedTheme>`. Reset in `_resetThemeServiceForTest`. (`writable`'s
   `safe_not_equal` guard means same-theme re-applies don't notify → no spurious repaints.)
2. **Components** — import `appliedTheme`, add a `void $appliedTheme;` dependency-touch (comment:
   `// re-resolve token colors on theme flip (ZEB-645)`) to the theme-dependent derived/template
   expression:
   - `Sparkline.svelte` — `strokeColor` derived.
   - `ConnectionBar.svelte` — convert the inline `statusColor(status)` template call to a
     `statusColorValue` derived that touches `$appliedTheme`; bind `{statusColorValue}` in markup.
   - `NodeDetail.svelte` — `statusColor`, `cpuColor`, `memColor`, `diskColor` deriveds (all call
     `tokenColor` via `heatToColor`/`sparklineColor`), and the inline `linkUtilizationColor(...)`
     link-stat color → derived-per-link or touch.
   - `LinkDetail.svelte` — `utilizationColor` derived + the inline `tokenColor('--accent')` passed
     to the latency Sparkline → a `latencyColor` derived that touches `$appliedTheme`.
   - `NavNodeRow.svelte` — the color-band `navPaletteColor(colorIdx)` inline call in the
     `{#each colorAncestry}` → a `bandColors` derived (`$appliedTheme` touch +
     `colorAncestry.map(navPaletteColor)`); use `bandColors[i]` in markup.
   - `TrustBadge.svelte` — `color` derived (`trustScoreColor`).
   - `NetworkGraph.svelte` — **no change** (per-frame canvas rAF already repaints).
3. **Tests:**
   - `src/lib/theme-service.test.ts` — assert `get(appliedTheme)` reflects the resolved theme
     after `setThemePreference('dark')` / `('light')` and after a `system` resolve.
   - `src/lib/components/__tests__/AppearanceSettings.test.ts` — add keyboard-nav coverage:
     ArrowRight/ArrowDown advances + selects, ArrowLeft/ArrowUp retreats (wrapping), Home/End
     jump to first/last, Space/Enter select the focused option; assert `aria-checked` +
     `dataset.theme` follow. Mirrors the CodecToggle model in `AppearanceSettings.svelte`.

---

## ZEB-646 — MessagesRail rail-tabs → segmented (`aria-pressed`)

### Decision (approved)

The rail switcher is a 2-item mode toggle, not tabs over labeled panels: no `role="tabpanel"`,
the tab bar is a sibling of `AssemblyRail`/`MediaFeed`, and the toggle vanishes entirely outside
community contexts. Downgrade to the segmented `aria-pressed` idiom (matches the nav footer
mode-toggle); drop the partial APG tabs pattern.

### Edits

1. **`src/lib/components/MessagesRail.svelte`** — container `role="tablist"` → `role="group"`
   (keep `aria-label="Right rail content"`); each `<button role="tab" aria-selected={…}>` →
   `<button aria-pressed={…}>` (drop `role="tab"`). CSS (`.rail-tabs`/`.rail-tab`/`.active`)
   unchanged.
2. **`src/lib/components/__tests__/MessagesRail.test.ts`** — `getByRole('tab', { name })` →
   `getByRole('button', { name })`; `queryByRole('tab', …)` absence checks →
   `queryByRole('button', { name: '⚖ Assembly' })` is null (name-scoped so other buttons don't
   match). Assert `aria-pressed` reflects the active segment.

---

## Execution order (avoids concurrent edits to NavNodeRow/ConnectionBar)

1. `app.css` token add (this session).
2. ZEB-644 sweep via one implementer subagent (exact audit as spec) → commit → review diff.
3. ZEB-645 inline (theme-service + 6 components + 2 tests).
4. ZEB-646 inline (MessagesRail + test).
5. Full gate: `npx tsc --noEmit && npx vitest run` + style-token-guard.
6. Whole-branch review → fix → PR → `@coderabbitai review` once at open.
