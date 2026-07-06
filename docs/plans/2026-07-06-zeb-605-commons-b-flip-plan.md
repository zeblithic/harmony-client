# ZEB-605 Commons B Flip Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-value the app to the Commons palette (light + warm dark), self-host the three Commons
font families, and add a follow-system theme with an owner-scoped override in Settings →
Appearance.

**Architecture:** Pure frontend PR. `src/app.css` carries both theme value sets (`:root` light,
`:root[data-theme="dark"]` dark); a new `theme-service.ts` applies `dataset.theme` in two phases
(device hint pre-paint, owner-scoped preference post-boot); a new `theme-colors.ts` resolves tokens
for canvas/TS drawing code; fonts arrive as `@fontsource` npm packages bundled by Vite.

**Tech Stack:** Svelte 5 (runes), Vite 7, vitest 4 + @testing-library/svelte, @fontsource.

**Spec:** `docs/specs/2026-07-06-zeb-605-commons-b-flip-design.md` — the remap table there is the
authoritative source for every token value; this plan embeds the resulting complete files.

## Global Constraints

- Branch `zeb-605-commons-b-flip`. ONE commit per task (amend fixups into it). No worktrees.
- Frontend gates per task, from the **repo root**: `npx tsc --noEmit` then `npx vitest run`.
  Both must be clean. No Rust files change in this plan → no cargo gates.
- `npm install` (which updates `package-lock.json`) happens ONLY in Task 1; commit both manifest
  files there.
- **No Google Fonts CDN / no `index.html` `<link>` tags** — the app must render offline.
- The style-token ratchet guard (`src/style-token-guard.test.ts`) must pass WITHOUT regenerating
  `src/style-token-allowlist.json`. No step in this plan legitimately changes a `<style>` block's
  raw-color count; if the guard fails, stop and report — do not regenerate.
- Discord palette hexes (case-insensitive): `#5865f2 #57f287 #43b581 #faa61a #ed4245 #72767d
  #1e1f22 #b5bac1`. After Task 4 none may remain under `src/`; Task 5 lands the permanent guard.
- Svelte 5 runes syntax (`$state`, `$derived`, `$props`, `$effect`) — match surrounding files.

---

### Task 1: @fontsource packages + app.css Commons flip

**Files:**
- Modify: `package.json`, `package-lock.json` (via npm install)
- Modify: `src/App.svelte` (imports, top of `<script>`)
- Modify: `src/NetworkApp.svelte` (imports)
- Modify: `src/app.css` (full rewrite of the token layer)

**Interfaces:**
- Consumes: `docs/design/commons/tokens.css` values (already merged into the code below).
- Produces: every CSS token in both themes; `--font-display/-ui/-mono` tokens; fonts registered
  under family names `'Newsreader Variable'`, `'Public Sans'`, `'IBM Plex Mono'`. Task 2 keys the
  dark block on `:root[data-theme="dark"]`.

- [ ] **Step 1: Install font packages**

```bash
npm install @fontsource-variable/newsreader @fontsource/public-sans @fontsource/ibm-plex-mono
```

- [ ] **Step 2: Identify the exact import paths**

Inspect the installed packages:

```bash
ls node_modules/@fontsource-variable/newsreader/*.css
ls node_modules/@fontsource/public-sans/{400,500,600,700}.css
ls node_modules/@fontsource/ibm-plex-mono/{400,500,600}.css
grep -l "opsz" node_modules/@fontsource-variable/newsreader/*.css | head -5
```

Rule: for Newsreader pick the CSS file that declares BOTH `wght` and `opsz` axes (expected:
`opsz.css`; verify with the grep — its `@font-face` should carry a `font-stretch`-style
`font-variation-settings` or an `opsz`-named woff2). If no combined opsz+wght file exists, use the
default `index.css` (wght-only variable) and note the lost optical sizing in your report. For
Public Sans and IBM Plex Mono the per-weight files above are the imports (latin subsets are the
package default).

- [ ] **Step 3: Add imports to both Vite entries**

In `src/App.svelte`, immediately BEFORE the existing `import './app.css';` (line 2):

```ts
// ZEB-605: self-hosted Commons fonts (bundled woff2 — offline; no CDN).
import '@fontsource-variable/newsreader/opsz.css';
import '@fontsource/public-sans/400.css';
import '@fontsource/public-sans/500.css';
import '@fontsource/public-sans/600.css';
import '@fontsource/public-sans/700.css';
import '@fontsource/ibm-plex-mono/400.css';
import '@fontsource/ibm-plex-mono/500.css';
import '@fontsource/ibm-plex-mono/600.css';
```

Same block in `src/NetworkApp.svelte` before its `import './app.css';` (line 2). Adjust the
Newsreader path per Step 2 if needed.

- [ ] **Step 4: Rewrite the token layer in `src/app.css`**

Replace everything from `:root {` through the closing brace of the current `:root` block (lines
1–128) with the two blocks below. The trailing resets (`*, *::before…`, `html, body`, scrollbar
rules) stay exactly as they are.

```css
:root {
  /* =========================================================================
     Commons LIGHT (ZEB-605 flip). Dark re-values every color token in
     :root[data-theme="dark"] below; dimensions/radii/fonts are theme-invariant.
     Reference colors via variables — src/style-token-guard.test.ts rejects
     new raw color literals in Svelte <style> blocks.
     Remap rationale: docs/specs/2026-07-06-zeb-605-commons-b-flip-design.md §1.
     ========================================================================= */
  --bg-primary: #fbf9f4;
  --bg-secondary: #efeadf;
  --bg-tertiary: #e9e3d6;
  --text-primary: #20241c;
  --text-secondary: #4b4f44;
  --text-muted: #767a6c;
  --accent: #466b4c;
  --accent-hover: #2f4a35;
  --danger: #b1402f;
  --border: #e3ddcf;

  /* Semantic color tokens (ZEB-604) */
  --bg-hover: #e9e3d6;
  --overlay: rgba(32, 27, 15, 0.45);
  --info: #4a6fa5;
  --success: #466b4c;
  --warning: #b9742c;

  /* Commons core (new in ZEB-605) */
  --paper: #f4f1ea; /* deepest backdrop behind the layout grid */
  --surface-raised: #ffffff;
  --line-soft: #ece7da;
  --faint: #a39e8e;
  --primary-deep: #2f4a35;
  --primary-soft: #e4ece2;
  --primary-border: #c9d6c6;
  --gov-clay: #b9742c;
  --gov-clay-soft: #f1e2cc;
  --gov-clay-deep: #5a4321;
  --vote-for: #466b4c;
  --vote-against: #b1402f;
  --vote-abstain: #cdc6b4;
  --tally-track: #eadfc8;
  --status-drafting-fg: #5a5345;
  --status-drafting-bg: #e6e0d0;
  --status-open-fg: #5a4321;
  --status-open-bg: #f1e2cc;
  --status-passed-fg: #ffffff;
  --status-passed-bg: #466b4c;
  --status-failed-fg: #ffffff;
  --status-failed-bg: #b1402f;
  --status-recalled-fg: #7d2a1e;
  --status-recalled-bg: #f3ddd7;

  /* Adopted seams (ZEB-604 sweep 2; Commons values per the ZEB-605 remap table) */
  --bg-tertiary-hover: #e0d9c8;
  --border-default: #d8d2c2;
  --buddy-bg: rgba(70, 107, 76, 0.08);
  --buddy-bg-hover: rgba(70, 107, 76, 0.14);
  --chip-bg: #e6e0d0;
  --chip-fg: #4b4f44;
  --color-bg: #ffffff; /* mint namespace */
  --color-bg-warning: #f1e2cc;
  --color-error: #b1402f;
  --color-text-secondary: #767a6c;
  --fg-error: #b1402f;
  --hover-bg: rgba(32, 36, 28, 0.04);
  --input-bg: #ffffff;
  --muted: #767a6c;
  --panel-bg: #efeadf;
  --share-bg: rgba(70, 107, 76, 0.08);
  --share-bg-hover: rgba(70, 107, 76, 0.14);
  --success-bg: rgba(70, 107, 76, 0.12);
  --surface: #fbf9f4;
  --surface-active: #e9e3d6;
  --surface-highlight: rgba(185, 116, 44, 0.1);
  --surface-hover: #efeadf;
  --text-danger: #b1402f;
  --text-link: #4a6fa5;
  --text-warning: #b9742c;
  --toast-bg: rgba(32, 36, 28, 0.95);
  --toast-fg: #f0ece2;
  --warn-border: #d9b982;
  --warn-fg: #5a4321;

  /* Status/role families (converged onto the Commons set per the flip decision) */
  --danger-muted: #b1402f;
  --danger-alt: #b1402f;
  --danger-deep: #7d2a1e;
  --danger-vivid: #b1402f;
  --danger-text-muted: #a06055;
  --danger-border-muted: #dcc0b8;
  --mail-danger: #b1402f;
  --mail-error-text: #a06055;
  --warning-bright: #b9742c;
  --role-mod: #b9742c;
  --success-deep: #2f4a35;
  --success-gov: #466b4c;
  --success-alt: #466b4c;
  --presence-online: #466b4c;
  --gov-purple: #7d6ba0;
  --sortition-bg: rgba(125, 107, 160, 0.08);
  --flashcard-correct: #466b4c;
  --flashcard-hint: #b9742c;
  --library-accent: #4a6fa5;
  --cat-orange: #c56a46;
  --cat-yellow: #b9742c;
  --cat-blue: #4a6fa5;
  --cat-purple: #7d6ba0;
  --net-ok-bg: #e4ece2;
  --net-ok-fg: #2f4a35;
  --net-ok-deep: #2f4a35;
  --net-warn-bg: #f1e2cc;
  --net-warn-fg: #5a4321;
  --net-danger-bg: rgba(177, 64, 47, 0.06);
  --net-danger-fg: #b1402f;

  /* Text extras */
  --text-bright: #ffffff;
  --text-faint: #a39e8e;
  --text-dim: #767a6c;
  --text-chip: #4b4f44;
  --text-doc: #4b4f44;
  --text-inverse-dark: #20241c;

  /* Surface extras */
  --panel-bg-deep: #e6e0d0;
  --chip-bg-active: #ddd5c2;
  --color-border-soft: #ece7da; /* mint namespace */

  /* Subtle fills & borders */
  --bg-hover-subtle: rgba(32, 36, 28, 0.05);
  --bg-highlight-faint: rgba(32, 36, 28, 0.03);
  --border-bright: rgba(32, 36, 28, 0.25);

  /* Shadow color tiers (consumed inside component box-shadows) + Commons elevation */
  --shadow-soft: rgba(40, 30, 10, 0.1);
  --shadow-mid: rgba(40, 30, 10, 0.16);
  --shadow-strong: rgba(40, 30, 10, 0.24);
  --shadow-heavy: rgba(40, 30, 10, 0.38);
  --shadow-e1: 0 1px 3px rgba(40, 30, 10, 0.07);
  --shadow-e2: 0 2px 10px rgba(40, 30, 10, 0.1);
  --shadow-e3: 0 8px 28px rgba(40, 30, 10, 0.16);

  /* Type (families self-hosted via @fontsource, ZEB-605) */
  --font-display: 'Newsreader Variable', 'Newsreader', Georgia, serif;
  --font-ui: 'Public Sans', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
  --font-mono: 'IBM Plex Mono', ui-monospace, 'SF Mono', monospace;

  --avatar-size-micro: 24px;
  --avatar-size-mini: 20px;
  --nav-width: 240px;
  --nav-width-collapsed: 56px;
  --breakpoint-collapse: 768px;

  font-family: var(--font-ui);
  font-size: 14px;
  line-height: 1.4;
  color: var(--text-primary);
  background-color: var(--paper);
}

/* =============================================================================
   Commons DARK — warm, brown-tinted near-black (never Discord graphite).
   Applied by theme-service.ts setting document.documentElement.dataset.theme.
   ============================================================================= */
:root[data-theme='dark'] {
  --bg-primary: #26231e;
  --bg-secondary: #201e19;
  --bg-tertiary: #322e27;
  --text-primary: #f0ece2;
  --text-secondary: #c4bdab;
  --text-muted: #8f897a;
  --accent: #7fa886;
  --accent-hover: #97bd9d;
  --danger: #d98377;
  --border: #38342c;

  --bg-hover: #322e27;
  --overlay: rgba(0, 0, 0, 0.55);
  --info: #8ab0d8;
  --success: #7fa886;
  --warning: #d39450;

  --paper: #1c1a16;
  --surface-raised: #2b2823;
  --line-soft: #322e27;
  --faint: #6f6a5c;
  --primary-deep: #7fa886;
  --primary-soft: #2a342a;
  --primary-border: #3c4a3d;
  --gov-clay: #d39450;
  --gov-clay-soft: #3a2f1f;
  --gov-clay-deep: #e2b888;
  --vote-for: #7fa886;
  --vote-against: #d98377;
  --vote-abstain: #4a4639;
  --tally-track: #322a1f;
  --status-drafting-fg: #c4bdab;
  --status-drafting-bg: #322e27;
  --status-open-fg: #e2b888;
  --status-open-bg: #3a2f1f;
  --status-passed-fg: #14160f;
  --status-passed-bg: #7fa886;
  --status-failed-fg: #1c1a16;
  --status-failed-bg: #d98377;
  --status-recalled-fg: #e2b888;
  --status-recalled-bg: #3a2f1f;

  --bg-tertiary-hover: #3a352d;
  --border-default: #453f35;
  --buddy-bg: rgba(127, 168, 134, 0.1);
  --buddy-bg-hover: rgba(127, 168, 134, 0.16);
  --chip-bg: #322e27;
  --chip-fg: #c4bdab;
  --color-bg: #2b2823;
  --color-bg-warning: #3a2f1f;
  --color-error: #d98377;
  --color-text-secondary: #8f897a;
  --fg-error: #d98377;
  --hover-bg: rgba(240, 236, 226, 0.05);
  --input-bg: #191713;
  --muted: #8f897a;
  --panel-bg: #201e19;
  --share-bg: rgba(127, 168, 134, 0.1);
  --share-bg-hover: rgba(127, 168, 134, 0.16);
  --success-bg: rgba(127, 168, 134, 0.15);
  --surface: #26231e;
  --surface-active: #322e27;
  --surface-highlight: rgba(211, 148, 80, 0.12);
  --surface-hover: #2b2823;
  --text-danger: #d98377;
  --text-link: #8ab0d8;
  --text-warning: #d39450;
  --toast-bg: rgba(20, 18, 14, 0.95);
  --toast-fg: #f0ece2;
  --warn-border: #6b5633;
  --warn-fg: #e2b888;

  --danger-muted: #d98377;
  --danger-alt: #d98377;
  --danger-deep: #e2a49a;
  --danger-vivid: #d98377;
  --danger-text-muted: #b98d84;
  --danger-border-muted: #4a332e;
  --mail-danger: #d98377;
  --mail-error-text: #c99087;
  --warning-bright: #e2b888;
  --role-mod: #d39450;
  --success-deep: #97bd9d;
  --success-gov: #7fa886;
  --success-alt: #7fa886;
  --presence-online: #7fa886;
  --gov-purple: #b3a3d1;
  --sortition-bg: rgba(179, 163, 209, 0.1);
  --flashcard-correct: #7fa886;
  --flashcard-hint: #e2b888;
  --library-accent: #8ab0d8;
  --cat-orange: #e0946f;
  --cat-yellow: #d39450;
  --cat-blue: #8ab0d8;
  --cat-purple: #b3a3d1;
  --net-ok-bg: #2a342a;
  --net-ok-fg: #97bd9d;
  --net-ok-deep: #7fa886;
  --net-warn-bg: #3a2f1f;
  --net-warn-fg: #e2b888;
  --net-danger-bg: rgba(217, 131, 119, 0.08);
  --net-danger-fg: #d98377;

  --text-bright: #f0ece2;
  --text-faint: #6f6a5c;
  --text-dim: #8f897a;
  --text-chip: #c4bdab;
  --text-doc: #c4bdab;
  --text-inverse-dark: #1c1a16;

  --panel-bg-deep: #191713;
  --chip-bg-active: #3a352d;
  --color-border-soft: #322e27;

  --bg-hover-subtle: rgba(240, 236, 226, 0.06);
  --bg-highlight-faint: rgba(240, 236, 226, 0.03);
  --border-bright: rgba(240, 236, 226, 0.3);

  --shadow-soft: rgba(0, 0, 0, 0.35);
  --shadow-mid: rgba(0, 0, 0, 0.45);
  --shadow-strong: rgba(0, 0, 0, 0.55);
  --shadow-heavy: rgba(0, 0, 0, 0.7);
  --shadow-e1: 0 1px 3px rgba(0, 0, 0, 0.4);
  --shadow-e2: 0 2px 10px rgba(0, 0, 0, 0.5);
  --shadow-e3: 0 8px 28px rgba(0, 0, 0, 0.6);
}
```

- [ ] **Step 5: Sanity-check the token sets match**

Every token name present in the old `:root` must still exist in the new light block, and every
color token in the light block must be re-valued in the dark block (dimensions, radii, `--font-*`
excluded). Verify mechanically:

```bash
node -e "
const css = require('fs').readFileSync('src/app.css','utf8');
const blocks = css.split(/:root\[data-theme='dark'\]/);
const names = (s) => new Set([...s.matchAll(/(--[a-z0-9-]+):/g)].map((m) => m[1]));
const light = names(blocks[0]), dark = names(blocks[1]);
const nonColor = new Set(['--avatar-size-micro','--avatar-size-mini','--nav-width','--nav-width-collapsed','--breakpoint-collapse','--font-display','--font-ui','--font-mono','--radius-chip','--radius-input','--radius-card']);
const missing = [...light].filter((n) => !dark.has(n) && !nonColor.has(n));
console.log('light', light.size, 'dark', dark.size, 'missing-in-dark', missing);
process.exit(missing.length ? 1 : 0);
"
```

Expected: `missing-in-dark []`.

- [ ] **Step 6: Run gates**

```bash
npx tsc --noEmit && npx vitest run
```

Expected: clean. The ratchet guard passes untouched (app.css is not scanned).

- [ ] **Step 7: Commit**

```bash
git add package.json package-lock.json src/app.css src/App.svelte src/NetworkApp.svelte
git commit -m "ZEB-605 T1: Commons token flip (light+dark) + self-hosted @fontsource families"
```

---

### Task 2: theme-service + pre-paint init + matchMedia stub

**Files:**
- Create: `src/lib/theme-service.ts`
- Create: `src/lib/theme-service.test.ts`
- Modify: `src/main.ts`, `src/network-main.ts` (pre-paint call)
- Modify: `src/App.svelte` (connectOwnerTheme at both owner-landing sites)
- Modify: `src/test-setup.ts` (matchMedia stub)

**Interfaces:**
- Consumes: `document.documentElement.dataset.theme` keyed by Task 1's dark block; owner id from
  App.svelte (`ownerState.ownerId` string).
- Produces (Tasks 3/4 rely on these exact exports):
  `type ThemePreference = 'system' | 'light' | 'dark'`;
  `initThemePrePaint(): void`; `connectOwnerTheme(ownerId: string): void`;
  `setThemePreference(pref: ThemePreference): void`;
  `themePreference: Readable<ThemePreference>`;
  `THEME_APPLIED_EVENT = 'harmony:theme-applied'` (CustomEvent on `document`, detail =
  `'light' | 'dark'`); `_resetThemeServiceForTest(): void`.

- [ ] **Step 1: matchMedia stub in `src/test-setup.ts`**

Append after the existing localStorage/dialog polyfills:

```ts
// jsdom lacks matchMedia; theme-service (ZEB-605) queries prefers-color-scheme.
// Static stub resolves 'system' to light; tests needing control stub their own.
if (typeof window !== 'undefined' && typeof window.matchMedia !== 'function') {
  (window as unknown as { matchMedia: (query: string) => MediaQueryList }).matchMedia = (
    query: string
  ) =>
    ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }) as unknown as MediaQueryList;
}
```

- [ ] **Step 2: Write the failing tests — `src/lib/theme-service.test.ts`**

```ts
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { get } from 'svelte/store';
import {
  THEME_APPLIED_EVENT,
  _resetThemeServiceForTest,
  connectOwnerTheme,
  initThemePrePaint,
  resolveTheme,
  setThemePreference,
  themePreference,
} from './theme-service';

const OWNER = 'aaaa0000aaaa0000';
const OTHER = 'bbbb1111bbbb1111';

beforeEach(() => {
  localStorage.clear();
  _resetThemeServiceForTest();
  delete document.documentElement.dataset.theme;
});

describe('resolveTheme', () => {
  it('passes explicit prefs through and resolves system via matchMedia', () => {
    expect(resolveTheme('light')).toBe('light');
    expect(resolveTheme('dark')).toBe('dark');
    expect(resolveTheme('system')).toBe('light'); // global stub: matches=false
  });
});

describe('pre-paint init', () => {
  it('applies the device hint when present', () => {
    localStorage.setItem('harmony-theme:last-applied', 'dark');
    initThemePrePaint();
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('falls back to the system theme with no hint', () => {
    initThemePrePaint();
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  it('ignores a corrupt hint', () => {
    localStorage.setItem('harmony-theme:last-applied', 'blurple');
    initThemePrePaint();
    expect(document.documentElement.dataset.theme).toBe('light');
  });
});

describe('owner-scoped preference', () => {
  it('persists under the connected owner key only', () => {
    connectOwnerTheme(OWNER);
    setThemePreference('dark');
    expect(localStorage.getItem(`harmony-theme:owner-${OWNER}`)).toBe('dark');
    expect(localStorage.getItem(`harmony-theme:owner-${OTHER}`)).toBeNull();
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('connectOwnerTheme loads that owner’s stored pref; missing → system', () => {
    localStorage.setItem(`harmony-theme:owner-${OWNER}`, 'dark');
    connectOwnerTheme(OWNER);
    expect(document.documentElement.dataset.theme).toBe('dark');
    _resetThemeServiceForTest();
    connectOwnerTheme(OTHER);
    expect(get(themePreference)).toBe('system');
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  it('corrupt stored preference degrades to system', () => {
    localStorage.setItem(`harmony-theme:owner-${OWNER}`, 'blurple');
    connectOwnerTheme(OWNER);
    expect(get(themePreference)).toBe('system');
  });

  it('no connected owner: applies but persists no owner-scoped key', () => {
    setThemePreference('dark');
    expect(document.documentElement.dataset.theme).toBe('dark');
    const ownerKeys = Object.keys(localStorage).filter((k) => k.includes(':owner-'));
    expect(ownerKeys).toEqual([]);
  });

  it('writes the last-applied device hint on every apply', () => {
    connectOwnerTheme(OWNER);
    setThemePreference('dark');
    expect(localStorage.getItem('harmony-theme:last-applied')).toBe('dark');
    setThemePreference('light');
    expect(localStorage.getItem('harmony-theme:last-applied')).toBe('light');
  });

  it('emits THEME_APPLIED_EVENT with the resolved theme', () => {
    const seen: string[] = [];
    const listener = (e: Event) => seen.push((e as CustomEvent).detail as string);
    document.addEventListener(THEME_APPLIED_EVENT, listener);
    connectOwnerTheme(OWNER);
    setThemePreference('dark');
    document.removeEventListener(THEME_APPLIED_EVENT, listener);
    expect(seen).toContain('dark');
  });
});

describe('system follow', () => {
  it('re-applies on matchMedia change only while preference is system', () => {
    let matches = false;
    const listeners = new Set<() => void>();
    vi.stubGlobal('matchMedia', (query: string) => ({
      matches,
      media: query,
      onchange: null,
      addEventListener: (_: string, cb: () => void) => listeners.add(cb),
      removeEventListener: (_: string, cb: () => void) => listeners.delete(cb),
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }));
    try {
      connectOwnerTheme(OWNER); // pref defaults to 'system'
      expect(document.documentElement.dataset.theme).toBe('light');
      matches = true;
      listeners.forEach((cb) => cb());
      expect(document.documentElement.dataset.theme).toBe('dark');
      setThemePreference('light');
      matches = false;
      listeners.forEach((cb) => cb());
      // Explicit preference wins; the flip back to matches=false must not re-apply.
      expect(document.documentElement.dataset.theme).toBe('light');
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
```

- [ ] **Step 3: Run to verify failure**

```bash
npx vitest run src/lib/theme-service.test.ts
```

Expected: FAIL — module `./theme-service` not found.

- [ ] **Step 4: Implement `src/lib/theme-service.ts`**

```ts
// ZEB-605: follow-system theme with an owner-scoped manual override.
//
// Persistence is two keys:
//  - `harmony-theme:owner-<ownerId>` — the preference (source of truth). Owner-
//    scoped per the ZEB-586 pattern (see profile-service.ts): WebView
//    localStorage is bundle-scoped, not profile-scoped, so a fixed key would
//    leak the preference across identities on the same box.
//  - `harmony-theme:last-applied` — the last RESOLVED theme, deliberately
//    device-scoped: owner state arrives post-first-paint (get_owner_state),
//    so the next launch needs a pre-paint hint to avoid flashing the default.
import { writable, type Readable, type Writable } from 'svelte/store';

export type ThemePreference = 'system' | 'light' | 'dark';
export type ResolvedTheme = 'light' | 'dark';

export const THEME_APPLIED_EVENT = 'harmony:theme-applied';

const PREF_KEY_BASE = 'harmony-theme';
const LAST_APPLIED_KEY = 'harmony-theme:last-applied';
const DARK_QUERY = '(prefers-color-scheme: dark)';

function ownerKey(ownerId: string): string {
  return `${PREF_KEY_BASE}:owner-${ownerId}`;
}

function isPreference(v: unknown): v is ThemePreference {
  return v === 'system' || v === 'light' || v === 'dark';
}

function systemTheme(): ResolvedTheme {
  try {
    return window.matchMedia(DARK_QUERY).matches ? 'dark' : 'light';
  } catch {
    return 'light';
  }
}

export function resolveTheme(pref: ThemePreference): ResolvedTheme {
  return pref === 'system' ? systemTheme() : pref;
}

const preferenceWritable: Writable<ThemePreference> = writable('system');
let connectedOwnerId: string | null = null;
let currentPreference: ThemePreference = 'system';
let mediaCleanup: (() => void) | null = null;

function applyResolved(theme: ResolvedTheme): void {
  document.documentElement.dataset.theme = theme;
  try {
    localStorage.setItem(LAST_APPLIED_KEY, theme);
  } catch {
    // Sandboxed WebView storage failure is non-fatal; the theme still applies.
  }
  document.dispatchEvent(new CustomEvent(THEME_APPLIED_EVENT, { detail: theme }));
}

function applyPreference(pref: ThemePreference): void {
  currentPreference = pref;
  preferenceWritable.set(pref);
  applyResolved(resolveTheme(pref));
}

/** Pre-paint apply. Called from main.ts / network-main.ts BEFORE mount(). */
export function initThemePrePaint(): void {
  let hint: string | null = null;
  try {
    hint = localStorage.getItem(LAST_APPLIED_KEY);
  } catch {
    hint = null;
  }
  applyResolved(hint === 'dark' || hint === 'light' ? hint : systemTheme());
}

/** Owner identity resolved: load the owner's preference and start following
 *  the system while (and only while) the preference is 'system'. */
export function connectOwnerTheme(ownerId: string): void {
  connectedOwnerId = ownerId;
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(ownerKey(ownerId));
  } catch {
    stored = null;
  }
  applyPreference(isPreference(stored) ? stored : 'system');
  if (mediaCleanup === null) {
    try {
      const mql = window.matchMedia(DARK_QUERY);
      const onChange = () => {
        if (currentPreference === 'system') {
          applyResolved(systemTheme());
        }
      };
      mql.addEventListener('change', onChange);
      mediaCleanup = () => mql.removeEventListener('change', onChange);
    } catch {
      mediaCleanup = null;
    }
  }
}

/** Settings entry point. Persists only when an owner is connected (matching
 *  the loadProfile no-owner contract). */
export function setThemePreference(pref: ThemePreference): void {
  if (connectedOwnerId !== null) {
    try {
      localStorage.setItem(ownerKey(connectedOwnerId), pref);
    } catch {
      // non-fatal
    }
  }
  applyPreference(pref);
}

export const themePreference: Readable<ThemePreference> = preferenceWritable;

/** Test-only reset. */
export function _resetThemeServiceForTest(): void {
  connectedOwnerId = null;
  currentPreference = 'system';
  preferenceWritable.set('system');
  if (mediaCleanup !== null) {
    mediaCleanup();
    mediaCleanup = null;
  }
}
```

- [ ] **Step 5: Run tests to verify pass**

```bash
npx vitest run src/lib/theme-service.test.ts
```

Expected: PASS (all cases).

- [ ] **Step 6: Wire pre-paint + owner connection**

`src/main.ts` — before `mount(...)`:

```ts
import { initThemePrePaint } from './lib/theme-service';

initThemePrePaint();
```

`src/network-main.ts` — same two lines before its `mount(...)`.

`src/App.svelte` — add to the imports: `import { connectOwnerTheme } from './lib/theme-service';`
Then at BOTH owner-landing sites:
1. Boot path (~line 1793, right after `selfOwnerId = ownerState.ownerId;`):
   `connectOwnerTheme(ownerState.ownerId);`
2. Mint path (~line 952, right after `selfOwnerId = result.state.ownerId;`):
   `connectOwnerTheme(result.state.ownerId);`

- [ ] **Step 7: Run gates**

```bash
npx tsc --noEmit && npx vitest run
```

- [ ] **Step 8: Commit**

```bash
git add src/lib/theme-service.ts src/lib/theme-service.test.ts src/main.ts src/network-main.ts src/App.svelte src/test-setup.ts
git commit -m "ZEB-605 T2: theme-service — pre-paint hint, owner-scoped preference, system follow"
```

---

### Task 3: Settings → Appearance tab

**Files:**
- Create: `src/lib/components/AppearanceSettings.svelte`
- Create: `src/lib/components/__tests__/AppearanceSettings.test.ts`
- Modify: `src/lib/components/SettingsPanel.svelte` (union :31, TABS :73-79, new tabpanel)
- Modify: `src/App.svelte` (duplicated union :115)
- Modify: `src/lib/components/__tests__/SettingsPanel.test.ts` (TAB_LABELS + stub)

**Interfaces:**
- Consumes: Task 2's `themePreference`, `setThemePreference`, `ThemePreference`.
- Produces: `appearance` member of the `SettingsTab` union (SettingsPanel + App.svelte copies).

- [ ] **Step 1: Write the failing component test —
  `src/lib/components/__tests__/AppearanceSettings.test.ts`**

```ts
import '@testing-library/jest-dom/vitest';
import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, describe, expect, it } from 'vitest';
import { _resetThemeServiceForTest } from '../../theme-service';
import AppearanceSettings from '../AppearanceSettings.svelte';

beforeEach(() => {
  localStorage.clear();
  _resetThemeServiceForTest();
  delete document.documentElement.dataset.theme;
});

describe('AppearanceSettings', () => {
  it('renders a three-option radiogroup defaulting to System', () => {
    render(AppearanceSettings);
    const group = screen.getByRole('radiogroup', { name: /theme/i });
    const options = screen.getAllByRole('radio');
    expect(group).toBeInTheDocument();
    expect(options).toHaveLength(3);
    expect(screen.getByRole('radio', { name: /system/i })).toHaveAttribute(
      'aria-checked',
      'true'
    );
  });

  it('selecting Dark applies the dark theme and reflects selection', async () => {
    render(AppearanceSettings);
    await fireEvent.click(screen.getByRole('radio', { name: /dark/i }));
    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(screen.getByRole('radio', { name: /dark/i })).toHaveAttribute(
      'aria-checked',
      'true'
    );
    expect(screen.getByRole('radio', { name: /system/i })).toHaveAttribute(
      'aria-checked',
      'false'
    );
  });

  it('selecting Light applies the light theme', async () => {
    render(AppearanceSettings);
    await fireEvent.click(screen.getByRole('radio', { name: /dark/i }));
    await fireEvent.click(screen.getByRole('radio', { name: /light/i }));
    expect(document.documentElement.dataset.theme).toBe('light');
  });
});
```

- [ ] **Step 2: Run to verify failure**

```bash
npx vitest run src/lib/components/__tests__/AppearanceSettings.test.ts
```

Expected: FAIL — component not found.

- [ ] **Step 3: Implement `src/lib/components/AppearanceSettings.svelte`**

Structure below is normative; for the keyboard handler mirror
`CodecToggle.svelte`'s radiogroup model EXACTLY (ArrowLeft/ArrowRight/ArrowUp/ArrowDown move +
select, Home/End jump, Space/Enter select, roving tabindex on the active option). Style with
existing tokens only (no raw colors — the ratchet guard scans this file).

```svelte
<script lang="ts">
  import {
    setThemePreference,
    themePreference,
    type ThemePreference,
  } from '../theme-service';

  const OPTIONS: { value: ThemePreference; label: string; hint: string }[] = [
    { value: 'system', label: 'System', hint: 'Follow the operating system appearance' },
    { value: 'light', label: 'Light', hint: 'Commons light' },
    { value: 'dark', label: 'Dark', hint: 'Commons warm dark' },
  ];

  let current = $state<ThemePreference>('system');
  $effect(() => themePreference.subscribe((v) => (current = v)));

  function select(value: ThemePreference): void {
    setThemePreference(value);
  }

  // onKeydown(event, index): CodecToggle keyboard model — see Step 3 note.
</script>

<section class="appearance-settings">
  <h3>Appearance</h3>
  <div class="setting-row">
    <div class="setting-text">
      <span class="setting-label" id="theme-label">Theme</span>
      <span class="setting-hint">
        System follows your OS setting. Your choice is saved per identity on this device.
      </span>
    </div>
    <div class="theme-options" role="radiogroup" aria-labelledby="theme-label">
      {#each OPTIONS as option, i (option.value)}
        <button
          type="button"
          role="radio"
          aria-checked={current === option.value}
          tabindex={current === option.value ? 0 : -1}
          class="theme-option"
          class:selected={current === option.value}
          title={option.hint}
          onclick={() => select(option.value)}
          onkeydown={(e) => onKeydown(e, i)}
        >
          {option.label}
        </button>
      {/each}
    </div>
  </div>
</section>

<style>
  /* Tokens only. Model row/typography styling on NetworkDiscoverabilitySettings.svelte;
     segmented-control styling on CodecToggle.svelte (borders var(--border),
     selected fill var(--primary-soft), selected text var(--primary-deep)). */
</style>
```

- [ ] **Step 4: Wire the tab into `SettingsPanel.svelte` and `App.svelte`**

1. `SettingsPanel.svelte:31`: add `'appearance'` to the `SettingsTab` union.
2. `TABS` array (:73-79): add `{ id: 'appearance', label: 'Appearance' }` — place it directly
   after `profile` so identity-adjacent things stay grouped.
3. Add the tabpanel alongside the existing ones (stay-mounted, `hidden`-toggled — the ZEB-545
   convention, no `{#if}`):

```svelte
<div
  class="tab-content"
  role="tabpanel"
  id="settings-tabpanel-appearance"
  aria-labelledby="settings-tab-appearance"
  hidden={activeTab !== 'appearance'}
>
  <AppearanceSettings />
</div>
```

4. Import `AppearanceSettings` in SettingsPanel's script block.
5. `App.svelte:115`: add `'appearance'` to the duplicated `settingsTab` union.

- [ ] **Step 5: Update `SettingsPanel.test.ts`**

Add `Appearance` to `TAB_LABELS` (:30) in its expected position, and add
`AppearanceSettings` to the inner-panel stub mocks (:8-13, `./settings-panel-stub.svelte`
pattern) so the panel renders inert.

- [ ] **Step 6: Run the two test files, then gates**

```bash
npx vitest run src/lib/components/__tests__/AppearanceSettings.test.ts src/lib/components/__tests__/SettingsPanel.test.ts
npx tsc --noEmit && npx vitest run
```

Expected: PASS; full suite clean (ratchet guard: the new component's `<style>` uses tokens only).

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/AppearanceSettings.svelte src/lib/components/__tests__/AppearanceSettings.test.ts src/lib/components/SettingsPanel.svelte src/lib/components/__tests__/SettingsPanel.test.ts src/App.svelte
git commit -m "ZEB-605 T3: Settings → Appearance tab with System/Light/Dark radiogroup"
```

---

### Task 4: theme-colors resolver + Discord hex out of TS/canvas code

**Files:**
- Create: `src/lib/theme-colors.ts`
- Create: `src/lib/theme-colors.test.ts`
- Modify: `src/lib/graph-utils.ts`, `src/lib/trust-score.ts`, `src/lib/nav-utils.ts`,
  `src/lib/components/NetworkGraph.svelte`, `src/lib/components/ConnectionBar.svelte`,
  `src/lib/components/NodeDetail.svelte`, `src/lib/components/Sparkline.svelte`,
  `src/lib/components/LinkDetail.svelte`
- Modify (lockstep): `src/lib/graph-utils.test.ts`, `src/lib/trust-score.test.ts`,
  `src/lib/components/__tests__/Sparkline.test.ts`, plus any other test asserting the old hexes
  (`rg -il '#5865f2|#43b581|#ed4245|#faa61a|#72767d|#57f287|#fee75c|#eb459e' src --glob '*.test.ts'`)

**Interfaces:**
- Consumes: Task 2's `THEME_APPLIED_EVENT`.
- Produces: `tokenColor(name: string): string` and `COMMONS_FALLBACK` — the only sanctioned
  raw-hex table for these tokens.

- [ ] **Step 1: Write the failing resolver test — `src/lib/theme-colors.test.ts`**

```ts
import { beforeEach, describe, expect, it } from 'vitest';
import { COMMONS_FALLBACK, tokenColor, _clearTokenColorCacheForTest } from './theme-colors';
import { THEME_APPLIED_EVENT } from './theme-service';

beforeEach(() => {
  _clearTokenColorCacheForTest();
});

describe('tokenColor', () => {
  it('resolves via the fallback table in jsdom (getComputedStyle yields empty)', () => {
    expect(tokenColor('--accent')).toBe(COMMONS_FALLBACK['--accent']);
    expect(tokenColor('--danger')).toBe('#b1402f');
  });

  it('unknown token resolves to black rather than empty string', () => {
    expect(tokenColor('--no-such-token')).toBe('#000000');
  });

  it('caches and invalidates on THEME_APPLIED_EVENT', () => {
    document.documentElement.style.setProperty('--accent', '#123456');
    // jsdom's getComputedStyle DOES surface inline custom properties.
    document.dispatchEvent(new CustomEvent(THEME_APPLIED_EVENT, { detail: 'light' }));
    expect(tokenColor('--accent')).toBe('#123456');
    document.documentElement.style.removeProperty('--accent');
    expect(tokenColor('--accent')).toBe('#123456'); // still cached
    document.dispatchEvent(new CustomEvent(THEME_APPLIED_EVENT, { detail: 'dark' }));
    expect(tokenColor('--accent')).toBe(COMMONS_FALLBACK['--accent']); // cache dropped
  });
});
```

(If jsdom's `getComputedStyle` does not surface inline custom properties in the installed
version, replace the middle assertion's setup with a spy on `getComputedStyle` — keep the
cache/invalidsation semantics identical. Note the adjustment in your report.)

- [ ] **Step 2: Run to verify failure, then implement `src/lib/theme-colors.ts`**

```ts
// ZEB-605: resolve CSS custom-property colors for canvas/TS drawing code that
// cannot consume var(--…) directly (canvas fillStyle, color lerp math).
//
// COMMONS_FALLBACK carries the Commons LIGHT values — the single sanctioned
// raw-hex site for these tokens. jsdom returns '' for stylesheet-defined
// custom properties, so tests resolve deterministically to these constants.
import { THEME_APPLIED_EVENT } from './theme-service';

export const COMMONS_FALLBACK: Record<string, string> = {
  '--accent': '#466b4c',
  '--info': '#4a6fa5',
  '--warning': '#b9742c',
  '--warning-bright': '#b9742c',
  '--danger': '#b1402f',
  '--success-gov': '#466b4c',
  '--success-deep': '#2f4a35',
  '--presence-online': '#466b4c',
  '--text-muted': '#767a6c',
  '--text-faint': '#a39e8e',
  '--text-secondary': '#4b4f44',
  '--bg-primary': '#fbf9f4',
  '--paper': '#f4f1ea',
  '--flashcard-hint': '#b9742c',
  '--gov-purple': '#7d6ba0',
  '--cat-orange': '#c56a46',
  '--cat-yellow': '#b9742c',
  '--cat-blue': '#4a6fa5',
  '--cat-purple': '#7d6ba0',
};

const cache = new Map<string, string>();
let listenerInstalled = false;

function ensureInvalidationListener(): void {
  if (listenerInstalled || typeof document === 'undefined') {
    return;
  }
  document.addEventListener(THEME_APPLIED_EVENT, () => cache.clear());
  listenerInstalled = true;
}

export function tokenColor(name: string): string {
  ensureInvalidationListener();
  const cached = cache.get(name);
  if (cached !== undefined) {
    return cached;
  }
  let value = '';
  try {
    value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  } catch {
    value = '';
  }
  if (value === '') {
    value = COMMONS_FALLBACK[name] ?? '#000000';
  }
  cache.set(name, value);
  return value;
}

/** Test-only. */
export function _clearTokenColorCacheForTest(): void {
  cache.clear();
}
```

Run: `npx vitest run src/lib/theme-colors.test.ts` — expected PASS.

- [ ] **Step 3: Convert the eight call sites**

Per-site hex→token mapping (exhaustive; every replacement goes through `tokenColor(...)` resolved
AT DRAW/CALL TIME, not at module top-level — module-level resolution would freeze the first
theme's values):

| Old hex | Token |
|---|---|
| `#5865f2` (blurple) | `--accent` |
| `#57f287` (bright green) | `--success-gov` |
| `#fee75c` (yellow) | `--flashcard-hint` |
| `#eb459e` (pink) | `--cat-purple` |
| `#43b581` (green) | `--presence-online` |
| `#faa61a` (orange) | `--warning` |
| `#ed4245` (red) | `--danger` |
| `#72767d` (grey) | `--text-muted` |
| `#4f545c` (dark grey) | `--text-faint` |
| `#1e1f22` / `#1a1b1e` (canvas bg) | `--bg-primary` / `--paper` |
| `#b5bac1` (canvas text) | `--text-secondary` |

> **T4 review amendment:** `#5865f2 → --accent` is the default; the three sites where `#5865f2`
> collided with a green sibling in both themes (trust ≥2.5, capability `inference`, heat `isLocal`)
> use `--info` instead — see design spec §4 amendment.

Example transform (graph-utils.ts node-type map):

```ts
// BEFORE
const NODE_COLORS: Record<NodeType, string> = { community: '#5865f2', /* … */ };
// AFTER
import { tokenColor } from './theme-colors';
function nodeColor(type: NodeType): string {
  const TOKENS: Record<NodeType, string> = { community: '--accent', /* … */ };
  return tokenColor(TOKENS[type]);
}
```

Site notes:
- `graph-utils.ts` heat/status lerp: keep the interpolation math; lerp between
  `tokenColor(...)`-resolved endpoint hexes.
- `nav-utils.ts` `NAV_PALETTE`: entries become `['--accent', '--cat-blue', '--cat-purple',
  '--cat-orange']` resolved through `tokenColor` at pick time. Check `NAV_PALETTE`'s consumers
  first (`rg -n "NAV_PALETTE" src`); preserve the exported API shape where it is consumed
  elsewhere (a `navPaletteColor(index)` helper is acceptable — report the final shape).
- `NodeDetail.svelte`: also fix the template inline style `var(--text-muted, #72767d)` → drop the
  raw fallback (`var(--text-muted)`).
- Canvas components (`NetworkGraph`, `Sparkline`, `ConnectionBar`, `LinkDetail`): resolve inside
  the draw/render function so a theme switch repaints correctly on the next frame.

- [ ] **Step 4: Update tests in lockstep**

Assertions on old hexes now target the fallback constants — import `COMMONS_FALLBACK` (or
`tokenColor`) rather than re-typing hex strings, e.g.
`expect(statusColor('online')).toBe(COMMONS_FALLBACK['--presence-online'])`.

- [ ] **Step 5: Sweep-verify zero Discord hex outside app.css history**

```bash
rg -in '#5865f2|#57f287|#43b581|#faa61a|#ed4245|#72767d|#1e1f22|#b5bac1' src/
```

Expected: no output.

- [ ] **Step 6: Gates + commit**

```bash
npx tsc --noEmit && npx vitest run
git add -A src/
git commit -m "ZEB-605 T4: theme-colors resolver — Discord hex out of canvas/TS color logic"
```

---

### Task 5: mono-stack sweep + HarmonyMark + permanent hex guard

**Files:**
- Modify: every `.svelte` file whose `<style>` sets a monospace `font-family` (~30 sites; enumerate
  with the Step 1 command)
- Create: `src/lib/components/HarmonyMark.svelte`
- Create: `src/lib/components/__tests__/HarmonyMark.test.ts`
- Modify: `src/lib/components/WelcomeModal.svelte` (~line 197)
- Create: `src/commons-hex-guard.test.ts`

**Interfaces:**
- Consumes: `--font-mono` token (Task 1).
- Produces: `HarmonyMark` component (props `size?: number = 24`, `withDot?: boolean = false`).

- [ ] **Step 1: Enumerate and sweep monospace stacks**

```bash
rg -n "font-family:\s*[^;]*(monospace|ui-monospace|Courier|SF Mono|Menlo|Consolas)" src --glob '*.svelte'
```

For every hit inside a `<style>` block: replace the whole declaration with
`font-family: var(--font-mono);`. For the defensive variants
(`var(--font-mono, monospace)` etc. in App.svelte:3937, FriendsPanel.svelte:1292/:1380,
DelegationWidget.svelte:345): drop the fallback → `var(--font-mono)`. Do NOT touch `font-family`
declarations that are not monospace stacks. Font-family lines are not colors, so the ratchet
guard's counts are unaffected even in allowlisted files.

- [ ] **Step 2: Write the failing HarmonyMark test —
  `src/lib/components/__tests__/HarmonyMark.test.ts`**

```ts
import '@testing-library/jest-dom/vitest';
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import HarmonyMark from '../HarmonyMark.svelte';

describe('HarmonyMark', () => {
  it('renders three brand circles, no dot by default, at default size 24', () => {
    const { container } = render(HarmonyMark);
    const svg = container.querySelector('svg');
    expect(svg).toHaveAttribute('width', '24');
    expect(svg).toHaveAttribute('viewBox', '0 0 92 92');
    expect(container.querySelectorAll('circle')).toHaveLength(3);
    expect(container.querySelector('circle[stroke="#466b4c"]')).toHaveAttribute(
      'stroke-width',
      '5'
    );
  });

  it('renders the center dot and thinner stroke at header size', () => {
    const { container } = render(HarmonyMark, { props: { size: 58, withDot: true } });
    expect(container.querySelectorAll('circle')).toHaveLength(4);
    expect(container.querySelector('circle[stroke="#466b4c"]')).toHaveAttribute(
      'stroke-width',
      '4'
    );
  });
});
```

- [ ] **Step 3: Implement `src/lib/components/HarmonyMark.svelte`**

```svelte
<script lang="ts">
  // The Harmony mark (ZEB-605), lifted from the Commons reference headers.
  // The four hexes are fixed BRAND constants — deliberately not theme tokens;
  // they live in markup attributes, which the style-token guard does not scan.
  let { size = 24, withDot = false }: { size?: number; withDot?: boolean } = $props();
  const strokeWidth = $derived(size >= 40 ? 4 : 5);
</script>

<svg width={size} height={size} viewBox="0 0 92 92" aria-hidden="true" class="harmony-mark">
  <circle cx="46" cy="34" r="22" fill="none" stroke="#466b4c" stroke-width={strokeWidth} />
  <circle cx="32" cy="56" r="22" fill="none" stroke="#283450" stroke-width={strokeWidth} />
  <circle cx="60" cy="56" r="22" fill="none" stroke="#c56a46" stroke-width={strokeWidth} />
  {#if withDot}
    <circle cx="46" cy="49" r="4" fill="#20241c" />
  {/if}
</svg>

<style>
  .harmony-mark {
    flex-shrink: 0;
  }
</style>
```

Run: `npx vitest run src/lib/components/__tests__/HarmonyMark.test.ts` — expected PASS.

- [ ] **Step 4: Mount in `WelcomeModal.svelte`**

At the "Welcome to Harmony" heading (~:197), place the mark before/above the `<h2>` per the
reference header lockup:

```svelte
<HarmonyMark size={58} withDot={true} />
<h2 id="welcome-title">Welcome to Harmony</h2>
```

(plus the import; wrap in a flex row/column matching the modal's existing layout — visual
judgment allowed, keep it minimal).

- [ ] **Step 5: Land the permanent guard — `src/commons-hex-guard.test.ts`**

```ts
// ZEB-605 done-when pin: no Discord palette hex anywhere under src/.
// Complements style-token-guard.test.ts, which only scans Svelte <style>
// blocks — this catches TS color logic, canvas fills, and template markup.
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

const DISCORD_HEX = [
  '#5865f2',
  '#57f287',
  '#43b581',
  '#faa61a',
  '#ed4245',
  '#72767d',
  '#1e1f22',
  '#b5bac1',
];

const SELF = 'commons-hex-guard.test.ts';

function walk(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const p = join(dir, entry);
    if (statSync(p).isDirectory()) {
      walk(p, out);
    } else if (/\.(svelte|ts|css|html)$/.test(entry) && !p.endsWith(SELF)) {
      out.push(p);
    }
  }
  return out;
}

describe('commons hex guard (ZEB-605)', () => {
  it('no Discord palette hex survives under src/', () => {
    const offenders: string[] = [];
    for (const file of walk('src')) {
      const text = readFileSync(file, 'utf8').toLowerCase();
      for (const hex of DISCORD_HEX) {
        if (text.includes(hex)) {
          offenders.push(`${file}: ${hex}`);
        }
      }
    }
    expect(offenders).toEqual([]);
  });
});
```

(Resolve the `src` root the same way `style-token-guard.test.ts` does — mirror its path setup if
it uses something other than a bare relative path.)

- [ ] **Step 6: Gates + commit**

```bash
npx tsc --noEmit && npx vitest run
git add -A src/
git commit -m "ZEB-605 T5: mono-stack sweep to var(--font-mono), HarmonyMark, Discord-hex guard"
```

---

## Final sweep (controller, after all tasks)

- `npx tsc --noEmit && npx vitest run` — full frontend gate, clean.
- `rg -in '#5865f2|#57f287|#43b581|#faa61a|#ed4245|#72767d|#1e1f22|#b5bac1' src/` — empty.
- Rust untouched: `git diff origin/main --stat -- src-tauri/` — empty.
- Whole-branch adversarial review (superpowers:requesting-code-review), then PR.
