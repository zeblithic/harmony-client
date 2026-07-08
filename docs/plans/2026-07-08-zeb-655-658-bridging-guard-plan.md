# ZEB-655 + ZEB-658 — BridgingPanel restyle + guard named-color hardening (bundled)

> Two ZEB-603 Commons-H gap-fill children, bundled into one PR: same repo, zero file
> overlap (`BridgingPanel.svelte` vs `style-token-guard.test.ts` + `DiagnosticExportModal.svelte`).
> Bundling is a plus — ZEB-658 tightens the guard and re-scans all of `src/`, so it validates
> ZEB-655's fresh BridgingPanel changes under the stricter rule before merge.

**Goal:** Finish BridgingPanel's half-done Commons restyle (cards + CountChip), and close the
style-token-guard's named-color detection gap (`crimson` + ~30 common names), keeping main green.

**Architecture:** Frontend-only. One component rewrite + one added test (ZEB-655); one regex
expansion + detection unit-tests + one 2-literal fix (ZEB-658). No Rust/IPC/behavior change.

## Global Constraints
- **Budget-0**: every colour a `var(--*)` already in `app.css`; `style-token-allowlist.json`
  **byte-identical** (ratchets DOWN only, never up). ZEB-658 achieves this by *fixing* the two
  newly-caught literals rather than allowlisting them.
- **Radius/type rubric**: cards/panels 8px; IDs/timestamps → `--font-mono`; headers → `--font-display`.
- **Gates**: `npx tsc --noEmit` + `npx vitest run` (full) + `style-token-guard` green; allowlist diff empty.

---

### Task 1 (ZEB-655): BridgingPanel — panel card + recessed rows + CountChip

**File:** `src/lib/components/BridgingPanel.svelte`
**Test:** `src/lib/components/__tests__/BridgingPanel.test.ts` (3 existing tests must stay green)

**Design decision (flag in PR):** BridgingPanel is a self-titled `<aside>` rendered in
DeliberationView's **unframed** `.right-col` grid cell (its column-sibling `StatementVoteList`
uses the same bespoke panel-deep+row pattern). So the **panel owns the card chrome** and rows are
**recessed inset** (no nested shadow → avoids card-in-card, per the ProfileEditor #6 precedent).
This differs from the parent-framed `PendingAdminProposalsPanel` (flat panel / carded rows) because
the framing context differs. Heat-bar stays **clay** per the audit default (sage-vs-clay flagged).

**Markup changes:**
- Import `CountChip from './governance/CountChip.svelte'`.
- Meta row: replace the two bespoke chips
  ```svelte
  <span class="chip agree">👍 {s.agreeCount}</span>
  <span class="chip diversity">diversity {diversityPct(s)}%</span>
  ```
  with
  ```svelte
  <CountChip label="Agree" value={String(s.agreeCount)} tone="sage" />
  <CountChip label="Diversity" value={`${diversityPct(s)}%`} tone="neutral" />
  ```
  (sage = positive cross-cutting consensus; neutral = the diversity tally, per ZEB-657 §3 #9.)
- Author span: `<span class="author">by {authorShort(s.author)}</span>` (truncated ID → mono).

**Style changes** (replace the whole `<style>` block per below; convert rem→px grid):
- `.bridging-panel`: `background: var(--surface-raised); border: 1px solid var(--border);
  box-shadow: var(--shadow-e1); border-radius: 8px; padding: 12px;` (was `--panel-bg-deep`, 6px).
- `h5`: add `font-family: var(--font-display); margin: 0 0 4px; font-size: 0.95rem;`.
- `.subtitle`: keep `--text-faint`; `margin: 0 0 8px;`.
- `ol`: `gap: 8px;` (keep list-none / flex-column).
- `.card`: `background: var(--surface); border-radius: 8px; padding: 10px;` keep
  `position: relative; overflow: hidden;` (recessed inset; drop `--panel-bg`/4px; **no** border/shadow).
- `.heat-bar`: **unchanged** (clay color-mix gradient — token-driven, passes guard).
- `.content`, `.text`: unchanged.
- `.meta`: `gap: 8px; margin-top: 6px; align-items: center;` keep `--text-faint`.
- `.author`: `font-family: var(--font-mono); color: var(--text-faint); font-size: 0.75rem;`.
- **Remove** `.chip`, `.chip.agree`, `.chip.diversity`.
- `.empty`, `.error`: unchanged.

**Add test** (locks count→CountChip mapping) in `BridgingPanel.test.ts`:
```ts
it('renders agree as a sage CountChip and diversity as a neutral CountChip', () => {
  const { container } = render(BridgingPanel, { props: { scores: [score1], error: null } });
  const sage = container.querySelector('.count-chip.sage');
  const neutral = container.querySelector('.count-chip.neutral');
  expect(sage?.querySelector('.cc-value')?.textContent).toBe('10');   // score1.agreeCount
  expect(neutral?.querySelector('.cc-value')?.textContent).toBe('50%'); // diversityQ32 ≈ 0.5
});
```

**Invariants:** heat-bar sort/width logic untouched; 3 existing tests (empty copy, statement
text, error string) unchanged and green.

### Task 2 (ZEB-658): guard named-color hardening + the 2 literals it catches

**Files:** `src/style-token-guard.test.ts` (regex + detection tests),
`src/lib/components/DiagnosticExportModal.svelte` (fix the 2 newly-caught literals).

**Step 2a — expand the `NAMED` regex** (adds `crimson` + common names + light/dark red/green/blue):
```ts
const NAMED =
  /(?<=[:\s,(])(?:(?:dark|light)?(?:red|green|blue|gr[ae]y)|white|black|yellow|orange|purple|pink|crimson|coral|salmon|tomato|gold|khaki|olive|lime|teal|cyan|aqua|navy|indigo|violet|magenta|fuchsia|maroon|brown|tan|beige|ivory|silver|turquoise|orchid|plum|lavender)(?=[\s;,)}!])/gi;
```
(`isRawFunctionArgs` reuses `NAMED.source`, so color-mix arg detection strengthens automatically.)

**Step 2b — add detection unit-tests** in the same file (same-module, no export needed):
```ts
describe('named-color detection (ZEB-658)', () => {
  const wrap = (css: string) => `<style>${css}</style>`;
  it('counts crimson + lightgreen (previously-missed)', () => {
    expect(countRawColors(wrap('.a { color: crimson; }'))).toBe(1);
    expect(countRawColors(wrap('.a { color: lightgreen; }'))).toBe(1);
  });
  it('counts the newly-added common names', () => {
    for (const c of ['coral','salmon','tomato','gold','khaki','olive','teal','navy','indigo','maroon','turquoise','orchid','plum','lavender']) {
      expect(countRawColors(wrap(`.a { color: ${c}; }`)), c).toBe(1);
    }
  });
  it('still ignores compositional keywords + tokens', () => {
    expect(countRawColors(wrap('.a { color: transparent; background: currentcolor; }'))).toBe(0);
    expect(countRawColors(wrap('.a { color: var(--danger); }'))).toBe(0);
    expect(countRawColors(wrap('.a { background: color-mix(in srgb, var(--accent) 20%, transparent); }'))).toBe(0);
  });
  it('does not false-positive on tan()/white-space (value-boundary guard)', () => {
    expect(countRawColors(wrap('.a { transform: rotate(tan(45deg)); }'))).toBe(0);
    expect(countRawColors(wrap('.a { white-space: nowrap; }'))).toBe(0);
  });
  it('counts a color-mix carrying a raw named color', () => {
    expect(countRawColors(wrap('.a { background: color-mix(in srgb, teal 20%, white); }'))).toBe(1);
  });
});
```

**Step 2c — fix the 2 literals the expanded guard now catches** (blast radius = exactly this
file; comprehensive scan confirmed nothing else in `src/`):
`src/lib/components/DiagnosticExportModal.svelte`
- `.error { color: crimson; }` → `color: var(--danger);`
- `.toast { color: lightgreen; }` → `color: var(--success);`
(The file's allowlist budget of `1` comes from a separate `background: #111` hex in
`.markdown-preview` — out of scope, left as-is. After the fixes the file counts `1` again →
**allowlist byte-identical**.)

### Task 3: Gate + PR

- `npx tsc --noEmit` clean.
- `npx vitest run` — full suite green (incl. the new BridgingPanel test + guard detection tests +
  the guard's filesystem scan under the stricter regex).
- `git diff src/style-token-allowlist.json` **empty**.
- Commit (bundle both tickets, one commit), push, open PR, fire `@coderabbitai review` **once**.
- PR body: flag the panel-owns-card and heat-bar clay-vs-sage decisions for Jake's review.
