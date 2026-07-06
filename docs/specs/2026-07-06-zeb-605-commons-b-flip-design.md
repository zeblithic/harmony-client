# ZEB-605 — Commons B: the flip (design)

**Ticket:** [ZEB-605](https://linear.app/zeblith/issue/ZEB-605) · parent ZEB-603 · depends on Commons A (ZEB-604, PRs #386/#388, done)
**Branch:** `zeb-605-commons-b-flip` off main @ `bda09473`

## Goal

Re-value the token layer to the Commons palette (light default + warm dark), self-host the three
Commons font families for offline Tauri rendering, and add a follow-system theme with an
owner-scoped manual override in Settings → Appearance. Done when: app renders Commons in both
themes with no raw Discord hex anywhere; system-follow and manual override both work and persist
per-owner; fonts load offline.

## Scope decisions (settled here)

1. **The flip is NOT a literal paste of `docs/design/commons/tokens.css`.** Track A sweep 2 added
   ~70 tokens to `src/app.css` that tokens.css does not define. Pasting verbatim would leave every
   `var()` referencing them unresolved. §1 carries the authoritative remap table: every current
   app.css token gets a Commons light AND dark value.
2. **Fonts via `@fontsource` npm packages** (SIL OFL), not committed woff2 and not Google Fonts CDN.
   Rationale: binaries stay out of the repo (no LFS configured; woff2 would be the largest tracked
   binaries by 10×), files exist in node_modules at Vite transform time (test-safe for all 255
   frontend test files — a missing src-relative `url()` would throw at transform), and the woff2 is
   fingerprinted into `dist/assets/` at build so runtime is fully offline. No `index.html` edits;
   the Tauri config has no CSP block, so no `font-src` obstacle exists.
3. **Guard-blind TypeScript/canvas Discord hex is in scope** (done-when says "no raw Discord hex
   anywhere", and the style-token guard only scans Svelte `<style>` blocks). §4 introduces a palette
   resolver so canvas/graph code consumes tokens. **Non-Discord palettes are out of scope**: the
   QuotaSummary flat-UI category colors and the amber-family raw colors already on the guard
   allowlist are not Discord hex; they stay for Commons H (gap-fill audit) to converge.
4. **Type-scale adoption is split**: this PR defines `--font-display/--font-ui/--font-mono`, flips
   `:root` to `var(--font-ui)`, and mechanically sweeps hardcoded monospace stacks (30+ sites) to
   `var(--font-mono)` — that is what makes IBM Plex Mono actually appear on IDs/tallies per the
   design. Applying Newsreader to specific headers needs per-surface judgment and belongs to the
   Commons C–I restyle tracks.
5. **Harmony mark**: new `HarmonyMark.svelte` (inline SVG lifted verbatim from the reference; its
   four hexes are fixed brand constants, deliberately not themeable tokens), mounted in
   `WelcomeModal`'s header. The nav-header brand lockup is Commons C scope (shell restyle) — the
   ticket's step 4 is "if needed", and no in-app brand element exists today to update.
6. **Network window** (`NetworkApp`, second Vite entry): gets pre-paint theme apply + fonts (it
   already imports app.css) but no Settings surface and no owner-scoped read — it follows the
   device hint at launch and then live-follows `prefers-color-scheme` (PR #407 R1); no owner
   preference is read there. Documented limitation; converges when settings sync exists.

## §1 Token flip (`src/app.css`)

Structure after the flip: one `:root` block (Commons **light** values for every existing token +
the new Commons tokens), then `:root[data-theme="dark"]` re-valuing every color token (dimensions,
radii, and font stacks are theme-invariant and appear only in `:root`). `background-color` at
`:root` switches from `var(--bg-primary)` to `var(--paper)` per tokens.css.

Tokens already covered by tokens.css (both themes) adopt its values verbatim: `--bg-primary`,
`--bg-secondary`, `--bg-tertiary`, `--bg-hover`, `--text-primary/-secondary/-muted`, `--accent`,
`--accent-hover`, `--danger`, `--border`, `--overlay`, `--info`, `--success`, `--warning`, the five
dimension tokens, plus new: `--paper`, `--surface-raised`, `--line-soft`, `--faint`,
`--primary-deep/-soft/-border`, `--gov-clay/-soft/-deep`, `--vote-for/-against/-abstain`,
`--tally-track`, the ten `--status-*` pairs, `--font-*`, `--radius-chip/-input/-card`,
`--shadow-e1/e2/e3` (dark gets its own e1/e2/e3: same geometry, `rgba(0,0,0,.4/.5/.6)`).

**Remap table for the sweep-2 tokens.** Method: converge drifted families onto the small Commons
set (the sweep-2 comment explicitly deferred convergence to this flip); tint fills use the Commons
hue at the original alpha weight; light-theme "hover/sunken" relationships invert (hover = darker
on light, lighter on dark). Four hues are invented where Commons has no anchor, derived by
desaturating/warm-shifting the original (marked ✦).

| Token | Commons light | Commons dark |
|---|---|---|
| `--bg-tertiary-hover` | `#e0d9c8` | `#3a352d` |
| `--border-default` | `#d8d2c2` | `#453f35` |
| `--buddy-bg` | `rgba(70,107,76,0.08)` | `rgba(127,168,134,0.10)` |
| `--buddy-bg-hover` | `rgba(70,107,76,0.14)` | `rgba(127,168,134,0.16)` |
| `--chip-bg` | `#e6e0d0` | `#322e27` |
| `--chip-fg` | `#4b4f44` | `#c4bdab` |
| `--color-bg` | `#ffffff` | `#2b2823` |
| `--color-bg-warning` | `#f1e2cc` | `#3a2f1f` |
| `--color-error` | `#b1402f` | `#d98377` |
| `--color-text-secondary` | `#767a6c` | `#8f897a` |
| `--fg-error` | `#b1402f` | `#d98377` |
| `--hover-bg` | `rgba(32,36,28,0.04)` | `rgba(240,236,226,0.05)` |
| `--input-bg` | `#ffffff` | `#191713` |
| `--muted` | `#767a6c` | `#8f897a` |
| `--panel-bg` | `#efeadf` | `#201e19` |
| `--share-bg` | `rgba(70,107,76,0.08)` | `rgba(127,168,134,0.10)` |
| `--share-bg-hover` | `rgba(70,107,76,0.14)` | `rgba(127,168,134,0.16)` |
| `--success-bg` | `rgba(70,107,76,0.12)` | `rgba(127,168,134,0.15)` |
| `--surface` | `#fbf9f4` | `#26231e` |
| `--surface-active` | `#e9e3d6` | `#322e27` |
| `--surface-highlight` | `rgba(185,116,44,0.10)` | `rgba(211,148,80,0.12)` |
| `--surface-hover` | `#efeadf` | `#2b2823` |
| `--text-danger` | `#b1402f` | `#d98377` |
| `--text-link` | `#4a6fa5` | `#8ab0d8` |
| `--text-warning` | `#b9742c` | `#d39450` |
| `--toast-bg` | `rgba(32,36,28,0.95)` | `rgba(20,18,14,0.95)` |
| `--toast-fg` | `#f0ece2` | `#f0ece2` |
| `--warn-border` | `#d9b982` | `#6b5633` |
| `--warn-fg` | `#5a4321` | `#e2b888` |
| `--danger-muted` | `#b1402f` | `#d98377` |
| `--danger-alt` | `#b1402f` | `#d98377` |
| `--danger-deep` | `#7d2a1e` | `#e2a49a` |
| `--danger-vivid` | `#b1402f` | `#d98377` |
| `--danger-text-muted` | `#a06055` ✦ | `#b98d84` ✦ |
| `--danger-border-muted` | `#dcc0b8` ✦ | `#4a332e` ✦ |
| `--mail-danger` | `#b1402f` | `#d98377` |
| `--mail-error-text` | `#a06055` ✦ | `#c99087` ✦ |
| `--warning-bright` | `#b9742c` | `#e2b888` |
| `--role-mod` | `#b9742c` | `#d39450` |
| `--success-deep` | `#2f4a35` | `#97bd9d` |
| `--success-gov` | `#466b4c` | `#7fa886` |
| `--success-alt` | `#466b4c` | `#7fa886` |
| `--presence-online` | `#466b4c` | `#7fa886` |
| `--gov-purple` | `#7d6ba0` ✦ | `#b3a3d1` ✦ |
| `--sortition-bg` | `rgba(125,107,160,0.08)` | `rgba(179,163,209,0.10)` |
| `--flashcard-correct` | `#466b4c` | `#7fa886` |
| `--flashcard-hint` | `#b9742c` | `#e2b888` |
| `--library-accent` | `#4a6fa5` | `#8ab0d8` |
| `--cat-orange` | `#c56a46` | `#e0946f` |
| `--cat-yellow` | `#b9742c` | `#d39450` |
| `--cat-blue` | `#4a6fa5` | `#8ab0d8` |
| `--cat-purple` | `#7d6ba0` ✦ | `#b3a3d1` ✦ |
| `--net-ok-bg` | `#e4ece2` | `#2a342a` |
| `--net-ok-fg` | `#2f4a35` | `#97bd9d` |
| `--net-ok-deep` | `#2f4a35` | `#7fa886` |
| `--net-warn-bg` | `#f1e2cc` | `#3a2f1f` |
| `--net-warn-fg` | `#5a4321` | `#e2b888` |
| `--net-danger-bg` | `rgba(177,64,47,0.06)` | `rgba(217,131,119,0.08)` |
| `--net-danger-fg` | `#b1402f` | `#d98377` |
| `--text-bright` | `#ffffff` | `#f0ece2` |
| `--text-faint` | `#a39e8e` | `#6f6a5c` |
| `--text-dim` | `#767a6c` | `#8f897a` |
| `--text-chip` | `#4b4f44` | `#c4bdab` |
| `--text-doc` | `#4b4f44` | `#c4bdab` |
| `--text-inverse-dark` | `#20241c` | `#1c1a16` |
| `--panel-bg-deep` | `#e6e0d0` | `#191713` |
| `--chip-bg-active` | `#ddd5c2` | `#3a352d` |
| `--color-border-soft` | `#ece7da` | `#322e27` |
| `--bg-hover-subtle` | `rgba(32,36,28,0.05)` | `rgba(240,236,226,0.06)` |
| `--bg-highlight-faint` | `rgba(32,36,28,0.03)` | `rgba(240,236,226,0.03)` |
| `--border-bright` | `rgba(32,36,28,0.25)` | `rgba(240,236,226,0.30)` |
| `--shadow-soft` | `rgba(40,30,10,0.10)` | `rgba(0,0,0,0.35)` |
| `--shadow-mid` | `rgba(40,30,10,0.16)` | `rgba(0,0,0,0.45)` |
| `--shadow-strong` | `rgba(40,30,10,0.24)` | `rgba(0,0,0,0.55)` |
| `--shadow-heavy` | `rgba(40,30,10,0.38)` | `rgba(0,0,0,0.70)` |

Notes: the `--shadow-*` tiers are color-only values consumed inside component box-shadows — they
warm-shift per theme; the Commons `--shadow-e1/e2/e3` (full shadow values) land as new tokens
alongside. `--net-ok-deep: green` and `--net-danger-fg: crimson` lose their named-color values.
The ratchet guard never scans app.css, so none of this trips it; no file's raw-color count changes
anywhere in this PR (the §2 mono sweep edits `<style>` font-family lines only, which the guard
does not count), so the allowlist stays as-is.

## §2 Self-hosted fonts

Packages (exact weights from the reference Google Fonts URL, latin subset):

- `@fontsource-variable/newsreader` — variable `wght` 400–600 **with the `opsz` 6..72 axis**
  (import the `opsz`-axis CSS subpath; if the package layout doesn't expose opsz+wght combined,
  fall back to static `@fontsource/newsreader` 400/500/600 and note the lost optical sizing in the
  PR body). Italic is NOT bundled — the reference mock itself renders faux-italic (its URL requests
  no italic axis); we match it.
- `@fontsource/public-sans` — 400, 500, 600, 700.
- `@fontsource/ibm-plex-mono` — 400, 500, 600.

Import site: JS imports at the top of `src/App.svelte` and `src/NetworkApp.svelte`, immediately
before `import './app.css'` (the @fontsource-documented pattern; both Vite entries covered; inert
in jsdom). `:root` gains the three `--font-*` tokens (§1) and `font-family: var(--font-ui)`.

Mono sweep: every hardcoded `font-family` monospace stack in Svelte `<style>` blocks
(`monospace`, `'Courier New'`, `ui-monospace, ...` — 30+ sites) becomes
`font-family: var(--font-mono)`. The four existing defensive `var(--font-mono, fallback)` sites
keep working and now resolve to IBM Plex Mono; the sweep normalizes them to drop the fallbacks.
Font-family values are not colors, so the ratchet guard is unaffected.

## §3 Theme service + Settings → Appearance

New `src/lib/theme-service.ts` (toast.ts singleton-store pattern; no adapter — pure
localStorage + DOM):

- `type ThemePreference = 'system' | 'light' | 'dark'`; resolved theme = `'light' | 'dark'`.
- Applying sets `document.documentElement.dataset.theme` to the **resolved** theme; CSS keys only
  on `[data-theme="dark"]`, light is `:root` default.
- **Persistence, two keys** (ZEB-586 owner-scoped pattern from profile-service/
  onboarding-backup-flags, try/catch-guarded storage access):
  - `harmony-theme:owner-<ownerId>` — the **preference** (source of truth, owner-scoped).
  - `harmony-theme:last-applied` — the last **resolved** theme, deliberately device-scoped: it
    exists only so the next launch can apply a theme before `selfOwnerId` resolves (owner state
    arrives post-paint via `get_owner_state`). Without it, a returning dark-override user flashes
    light on every boot.
- **Boot sequence (two-phase):**
  1. `initThemePrePaint()` called from `src/main.ts` and `src/network-main.ts` before `mount()`:
     applies `last-applied` hint if present, else `matchMedia('(prefers-color-scheme: dark)')`,
     and installs the system-follow listener (PR #407 R1 amendment — so the hint-only network
     window live-follows the OS; the listener re-applies only while the preference is `'system'`).
  2. `connectOwnerTheme(ownerId)` called from App.svelte where `selfOwnerId` lands (all three
     owner-resolution sites: the boot IIFE path, the mint path, and the
     owner-loads-after-start_node path found during implementation): reads the owner preference
     (default `'system'`), applies, and ensures the same system-follow listener.
- `setThemePreference(pref)`: persists under the connected owner's key (no owner → apply only,
  persist nothing, matching the loadProfile contract), applies, updates the exported
  `themePreference: Readable<ThemePreference>` store.
- **Settings UI:** new `AppearanceSettings.svelte` mounted as a new `appearance` tab in
  `SettingsPanel.svelte` (extend the `SettingsTab` union at :31, `TABS` at :73, add the
  stay-mounted `hidden`-toggled tabpanel per the ZEB-545 convention — no `{#if}`) and the
  duplicated union in `App.svelte:115`. The control is a 3-option `role="radiogroup"`
  (System / Light / Dark) following `CodecToggle.svelte`'s keyboard model (arrows/Home/End/
  Space/Enter, roving tabindex).
- **Test setup:** `src/test-setup.ts` gains a `window.matchMedia` stub (jsdom lacks it; any code
  path touching the service would throw in tests otherwise).

## §4 Discord hex in TypeScript/canvas code

New `src/lib/theme-colors.ts`:

- `COMMONS_FALLBACK: Record<string, string>` — the Commons **light** hex for each token the module
  serves (the one sanctioned raw-hex site; these are Commons constants, not Discord).
- `tokenColor(name: string): string` — `getComputedStyle(document.documentElement)
  .getPropertyValue(name)`, trimmed; empty (jsdom, or token missing) → fallback. Cached per
  resolved theme; cache invalidated by a hook the theme service calls on every apply.
- Call sites converted (each maps its Discord hexes to existing tokens — `--accent`, `--success*`,
  `--warning`, `--danger`, `--text-muted`, `--info`, `--cat-*`, `--presence-online`, `--bg-*`):
  `src/lib/graph-utils.ts` (node-type map + status/heat lerp — lerp interpolates between resolved
  hexes, unchanged math), `src/lib/trust-score.ts` (tier colors), `src/lib/nav-utils.ts`
  (`NAV_PALETTE` → the four `--cat-*` tokens), `NetworkGraph.svelte` canvas fills,
  `ConnectionBar.svelte` status map, `NodeDetail.svelte` gauge colors + the one inline-style
  `var(--text-muted, #72767d)` fallback (drop the fallback), `Sparkline.svelte`,
  `LinkDetail.svelte`.
- Tests (`graph-utils.test.ts` 19 asserts, `trust-score.test.ts` 9, `Sparkline.test.ts` 2, and any
  ChannelMessageFeed hits) update in lockstep: in jsdom `tokenColor` deterministically returns
  `COMMONS_FALLBACK`, so assertions target the fallback constants (imported, not re-typed).
- Sweep gate: after conversion, a repo grep for the eight Discord palette hexes
  (`#5865f2 #57f287 #43b581 #faa61a #ed4245 #72767d #1e1f22 #b5bac1`, any case) over `src/` must
  return zero hits outside `docs/`.

**Implementation amendment (T4 review).** `--accent`, `--presence-online`, and `--success-gov`
resolve to one shared value per theme (`#466b4c` light / sage in dark), so the three ex-blurple
(`#5865f2`) sites where color was the *only* discriminator against a green sibling — trust tier
≥2.5 (`trustScoreColor`), capability `inference` (`capabilityColor`), and the `heatToColor`
`isLocal` branch — map to `--info` (navy `#4a6fa5` light / `#8ab0d8` dark) instead of `--accent`.
`--info` is distinct from every green in both themes. Other `--accent` uses are unchanged;
`NAV_PALETTE` slot 0 (`--accent`, the ex-`#43b581` green slot) sits beside slot 1 (`--cat-blue`,
ex-`#5865f2`), which already differ, so no nav collision exists.

## §5 Harmony mark

`src/lib/components/HarmonyMark.svelte`: the reference SVG verbatim (viewBox `0 0 92 92`, three
circles sage `#466b4c` / navy `#283450` / clay `#c56a46`, optional center dot `#20241c`), props
`size` (default 24) and `withDot` (default false), stroke-width 5 below 40px / 4 at 40px+ per the
reference variants. Hexes live in markup attributes (guard-blind by design) with a comment marking
them as fixed brand constants — they do not change with theme. Mounted in `WelcomeModal.svelte`
beside the "Welcome to Harmony" heading (`withDot`, size 58 per the reference header variant).

## Testing & gates

- Unit: theme-service (resolve matrix, owner-key isolation à la backup-service.test.ts, no-owner
  no-persist, hint write-through, matchMedia-follow only under `system`); theme-colors (fallback
  determinism, cache invalidation on apply); HarmonyMark render; AppearanceSettings radiogroup
  (selection + keyboard); SettingsPanel tab-count/order update (`TAB_LABELS` in its test).
- Updated in lockstep: graph-utils / trust-score / Sparkline color asserts → fallback constants.
- Full frontend gate: `npx tsc --noEmit` + `npx vitest run` (255 files; the ratchet guard passes
  because no `<style>` block's raw-color count changes — font-family edits are not counted).
- Rust side untouched → no cargo gates beyond CI's own runs.
- Manual/GUI validation is Jake's visual pass post-PR (screenshots in PR body if feasible via
  `tauri dev` — NOT while any fleet/app instance shares the tree).

## Behavior changes & risks

1. **Default appearance flips dark→light** for every existing profile (no persisted preference
   exists yet; the default is follow-system). A dark-OS user sees Commons dark; a light-OS user
   sees Commons light instead of Discord graphite. Intended — this IS the reskin.
2. **First-boot paint** before any hint exists follows the OS, so the very first frame after this
   upgrade may repaint once when the (defaulted-`system`) owner preference loads — subsequent
   boots are flash-free via the hint.
3. Perceived-contrast regressions on surfaces the design never covered are possible (the remap
   table is judgment for ~70 tokens); the four ✦ invented hues are the highest-risk aesthetic
   calls. All are single-line value edits to revert/tune.
4. Rollback: revert the PR — app.css values, additive service/components, and package.json font
   deps all come out cleanly; no data migration (stale localStorage keys are harmless).
