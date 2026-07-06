// ZEB-605 done-when pin: no Discord palette hex anywhere under src/.
// Complements style-token-guard.test.ts, which only scans Svelte <style>
// blocks — this catches TS color logic, canvas fills, and template markup.
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
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

// Anchored to this file's own location (src/) so the guard works regardless of
// the directory vitest is invoked from — mirrors style-token-guard.test.ts.
const SRC_ROOT = dirname(fileURLToPath(import.meta.url));

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
    for (const file of walk(SRC_ROOT)) {
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
