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
const NAMED =
  /(?<=[:\s,(])(?:white|black|red|green|blue|yellow|orange|purple|pink|(?:dark|light)?gr[ae]y)(?=[\s;,)}!])/gi;

// A color function only counts as raw if its arguments carry raw color
// components. `color-mix(in srgb, var(--accent) 15%, transparent)` is
// token-driven styling — the encouraged idiom, not debt.
function isRawFunctionArgs(args: string): boolean {
  const cleaned = args
    .replace(/var\(\s*--[\w-]+\s*\)/g, '')
    .replace(/\d+(?:\.\d+)?%/g, '')
    .replace(/\bin\s+[\w-]+/g, '');
  return /[0-9#]/.test(cleaned) || new RegExp(NAMED.source, 'i').test(cleaned);
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
