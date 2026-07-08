import { describe, expect, it } from 'vitest';
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

/**
 * Style-token ratchet guard (ZEB-604).
 *
 * All colors in Svelte `<style>` blocks must go through CSS custom properties
 * (`var(--…)`) so the Commons design flip (ZEB-605) is a pure token-value
 * swap. This test counts raw color literals (hex, rgb/rgba, hsl/hsla) per
 * component and compares against `style-token-allowlist.json` — the frozen
 * remainder of the pre-ZEB-604 backlog, which sweep PRs ratchet down to zero.
 *
 * - Adding a raw color to any file fails this test: use an existing token in
 *   `src/app.css` or add a new semantic token there instead.
 * - Removing raw colors also fails (deliberately, to keep the ratchet tight):
 *   regenerate the allowlist with
 *   `UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts`
 *   and commit the shrunken file.
 *
 * Scope: `<style>` blocks only. Colors in `<script>` sections and template
 * inline styles are out of scope for the ZEB-604 sweep (tracked per-surface
 * under ZEB-611 where they are load-bearing).
 */

// Anchored to this file's own location so the guard works regardless of the
// directory vitest is invoked from.
const SRC_ROOT = dirname(fileURLToPath(import.meta.url));
const ALLOWLIST_PATH = join(SRC_ROOT, 'style-token-allowlist.json');

const STYLE_BLOCK = /<style[^>]*>([\s\S]*?)<\/style>/g;
const CSS_COMMENT = /\/\*[\s\S]*?\*\//g;
// Hex, raw color functions, and common named colors in value position.
// `transparent`/`currentcolor` are deliberately not counted — they are
// compositional keywords, not themeable colors. Named-color matching requires
// a value-ish left boundary so selectors like `.text-red` and properties like
// `white-space` don't false-positive.
const COLOR_FN =
  /\b(?:rgb|rgba|hsl|hsla|oklch|oklab|lab|lch|hwb|color-mix)\(((?:[^()]|\([^()]*\))*)\)/gi;
const HEX = /#[0-9a-f]{3,8}\b/gi;
// ZEB-658: expanded to catch `crimson` (+ light/dark red/green/blue and ~25 other
// common CSS named colors) after the audit found 5 uncounted `crimson` literals
// slipping the budget-0 ratchet. Value-position boundaries keep English-word
// colors (`tan`, `plum`, `lime`) from false-positiving on property names.
const NAMED =
  /(?<=[:\s,(])(?:(?:dark|light)?(?:red|green|blue|gr[ae]y)|white|black|yellow|orange|purple|pink|crimson|coral|salmon|tomato|gold|khaki|olive|lime|teal|cyan|aqua|navy|indigo|violet|magenta|fuchsia|maroon|brown|tan|beige|ivory|silver|turquoise|orchid|plum|lavender)(?=[\s;,)}!])/gi;

// A color function only counts as raw if its arguments carry raw color
// components. `color-mix(in srgb, var(--accent) 15%, transparent)` is
// token-driven styling — the encouraged idiom, not debt.
function isRawFunctionArgs(args: string): boolean {
  const cleaned = args
    .replace(/var\(\s*--[\w-]+\s*\)/g, '')
    .replace(/\d+(?:\.\d+)?%/g, '')
    .replace(/\bin\s+[\w-]+/g, '');
  // NAMED requires a value-boundary on its left, which holds when scanning full
  // CSS but not when a named color sits at index 0 of this cleaned fragment
  // (e.g. `color-mix(teal 20%, var(--accent))` → cleaned `teal , `). Prepend a
  // boundary space so a leading raw color is still detected — otherwise the
  // whole function goes uncounted and a raw color slips the ratchet.
  return /[0-9#]/.test(cleaned) || new RegExp(NAMED.source, 'i').test(` ${cleaned}`);
}

function svelteFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...svelteFiles(full));
    else if (entry.name.endsWith('.svelte')) out.push(full);
  }
  return out;
}

function countRawColors(source: string): number {
  let count = 0;
  for (const block of source.matchAll(STYLE_BLOCK)) {
    const css = block[1].replace(CSS_COMMENT, '');
    // Functions first (raw ones counted once, then removed so their inner
    // hex/named components aren't double-counted), then bare hex/named.
    const rest = css.replace(COLOR_FN, (_m, args: string) => {
      if (isRawFunctionArgs(args)) count += 1;
      return ' ';
    });
    count += [...rest.matchAll(HEX)].length + [...rest.matchAll(NAMED)].length;
  }
  return count;
}

function currentCounts(): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const file of svelteFiles(SRC_ROOT).sort()) {
    const n = countRawColors(readFileSync(file, 'utf8'));
    if (n > 0) {
      const rel = file.slice(SRC_ROOT.length + 1).split('\\').join('/');
      counts[rel] = n;
    }
  }
  return counts;
}

describe('style token guard', () => {
  it('Svelte <style> blocks introduce no raw color literals beyond the allowlist', () => {
    const actual = currentCounts();

    if (process.env.UPDATE_STYLE_TOKEN_ALLOWLIST) {
      writeFileSync(ALLOWLIST_PATH, `${JSON.stringify(actual, null, 2)}\n`);
      return;
    }

    const allowed: Record<string, number> = JSON.parse(readFileSync(ALLOWLIST_PATH, 'utf8'));
    const problems: string[] = [];
    for (const [file, n] of Object.entries(actual)) {
      const budget = allowed[file] ?? 0;
      if (n > budget) {
        problems.push(`${file}: ${n} raw color literal(s), allowlist permits ${budget} — use var(--…) tokens from src/app.css`);
      } else if (n < budget) {
        problems.push(`${file}: ${n} raw color literal(s), allowlist expects ${budget} — nice, now tighten the ratchet: UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts`);
      }
    }
    for (const file of Object.keys(allowed)) {
      if (!(file in actual)) {
        problems.push(`${file}: fully tokenized (or removed) but still allowlisted — tighten the ratchet: UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts`);
      }
    }

    expect(problems, `\n${problems.join('\n')}\n`).toEqual([]);
  });
});

describe('named-color detection (ZEB-658)', () => {
  const wrap = (css: string) => `<style>${css}</style>`;

  it('counts crimson + lightgreen (previously missed by the regex)', () => {
    expect(countRawColors(wrap('.a { color: crimson; }'))).toBe(1);
    expect(countRawColors(wrap('.a { color: lightgreen; }'))).toBe(1);
  });

  it('counts the newly-added common named colors', () => {
    for (const c of [
      'coral', 'salmon', 'tomato', 'gold', 'khaki', 'olive', 'teal', 'navy',
      'indigo', 'maroon', 'turquoise', 'orchid', 'plum', 'lavender',
    ]) {
      expect(countRawColors(wrap(`.a { color: ${c}; }`)), c).toBe(1);
    }
  });

  it('still ignores compositional keywords and token references', () => {
    expect(countRawColors(wrap('.a { color: transparent; background: currentcolor; }'))).toBe(0);
    expect(countRawColors(wrap('.a { color: var(--danger); }'))).toBe(0);
    expect(countRawColors(wrap('.a { background: color-mix(in srgb, var(--accent) 20%, transparent); }'))).toBe(0);
  });

  it('does not false-positive on tan()/white-space (value-boundary guard)', () => {
    expect(countRawColors(wrap('.a { transform: rotate(tan(45deg)); }'))).toBe(0);
    expect(countRawColors(wrap('.a { white-space: nowrap; }'))).toBe(0);
  });

  it('counts a color-mix that carries a raw named color', () => {
    expect(countRawColors(wrap('.a { background: color-mix(in srgb, teal 20%, white); }'))).toBe(1);
  });

  it('counts a leading named color in function args (index-0 boundary guard)', () => {
    // The named color is the sole non-token arg and sits at index 0 of the
    // cleaned fragment — must still be detected (regression for the reused
    // NAMED lookbehind, Qodo #420). Without the prepended boundary this was 0,
    // letting a raw color slip the budget-0 ratchet.
    expect(countRawColors(wrap('.a { background: color-mix(teal 20%, var(--accent)); }'))).toBe(1);
  });
});
