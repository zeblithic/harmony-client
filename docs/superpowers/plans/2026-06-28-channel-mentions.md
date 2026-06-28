# Channel @-Mentions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a user @-mention a member while composing a community channel message, and render mentions as resolved, human-readable names with a self-mention highlight.

**Architecture:** Two new pure-function modules hold all logic (compose reconcile + render tokenize); a dumb dropdown component renders candidates; `ChannelMessageFeed` wires compose (trigger→pick→track→reconcile-on-send) and render (segment list + self highlight); `App.svelte` passes the member roster. The wire format (`mentions[]` array + inline `<@ownerIdHex>` body tokens) and the nickname→profile→hex ladder already exist.

**Tech Stack:** Svelte 5 (runes), TypeScript, Vitest + @testing-library/svelte. Frontend only — no Rust.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-06-28-zeb-588-mentions-design.md`. Scope: community channels only.
- Mention token = `<@<ownerIdHex>>`, ownerIdHex = 32 lowercase hex; regex `/<@([0-9a-f]{32})>/g`.
- Resolution ladder (ONE definition): local nickname → broadcast profile displayName → `ownerId.slice(0,8)`, empty/whitespace treated as absent.
- Render must be XSS-safe: only Svelte `{}` text interpolation, never `innerHTML`.
- Gates: `npx tsc --noEmit` (repo root) and `npx vitest run` must pass. Run a single test file with `npx vitest run <path>`.
- Keep ZEB IDs out of commit subjects (body refs only). Commit Co-Authored-By + Claude-Session trailers per repo convention.

---

### Task 1: `mention-render.ts` — pure render helpers

**Files:**
- Create: `src/lib/mention-render.ts`
- Test: `src/lib/mention-render.test.ts`

**Interfaces:**
- Produces:
  - `type BodySegment = { type: 'text'; text: string } | { type: 'mention'; ownerId: string }`
  - `tokenizeBody(text: string): BodySegment[]`
  - `resolveMentionLabel(ownerId: string, resolveNickname?: (id: string) => string | undefined, resolveCard?: (id: string) => { displayName: string } | undefined): string`

- [ ] **Step 1: Write the failing test** — `src/lib/mention-render.test.ts`

```ts
import { describe, it, expect } from 'vitest';
import { tokenizeBody, resolveMentionLabel } from './mention-render';

const ID_A = 'a'.repeat(32);
const ID_B = 'b'.repeat(32);

describe('tokenizeBody', () => {
  it('returns a single text segment when there are no tokens', () => {
    expect(tokenizeBody('hello world')).toEqual([{ type: 'text', text: 'hello world' }]);
  });

  it('returns empty array for empty string', () => {
    expect(tokenizeBody('')).toEqual([]);
  });

  it('splits a token in the middle', () => {
    expect(tokenizeBody(`hey <@${ID_A}> there`)).toEqual([
      { type: 'text', text: 'hey ' },
      { type: 'mention', ownerId: ID_A },
      { type: 'text', text: ' there' },
    ]);
  });

  it('handles a token at the start and end', () => {
    expect(tokenizeBody(`<@${ID_A}>!`)).toEqual([
      { type: 'mention', ownerId: ID_A },
      { type: 'text', text: '!' },
    ]);
    expect(tokenizeBody(`hi <@${ID_A}>`)).toEqual([
      { type: 'text', text: 'hi ' },
      { type: 'mention', ownerId: ID_A },
    ]);
  });

  it('handles adjacent tokens and multiple distinct ids', () => {
    expect(tokenizeBody(`<@${ID_A}><@${ID_B}>`)).toEqual([
      { type: 'mention', ownerId: ID_A },
      { type: 'mention', ownerId: ID_B },
    ]);
  });

  it('does not treat a malformed near-token as a mention', () => {
    // wrong length (31 hex) → left as text
    const short = 'a'.repeat(31);
    expect(tokenizeBody(`x <@${short}> y`)).toEqual([{ type: 'text', text: `x <@${short}> y` }]);
  });
});

describe('resolveMentionLabel', () => {
  const nick = (id: string) => (id === ID_A ? 'NickA' : undefined);
  const card = (id: string) => (id === ID_A ? { displayName: 'CardA' } : id === ID_B ? { displayName: 'CardB' } : undefined);

  it('prefers local nickname', () => {
    expect(resolveMentionLabel(ID_A, nick, card)).toBe('NickA');
  });

  it('falls back to broadcast displayName', () => {
    expect(resolveMentionLabel(ID_B, nick, card)).toBe('CardB');
  });

  it('falls back to short hex when nothing resolves', () => {
    expect(resolveMentionLabel(ID_A, undefined, undefined)).toBe('aaaaaaaa');
  });

  it('treats empty/whitespace nickname or name as absent', () => {
    expect(resolveMentionLabel(ID_A, () => '  ', () => ({ displayName: '' }))).toBe('aaaaaaaa');
    expect(resolveMentionLabel(ID_A, () => '   ', () => ({ displayName: 'Real' }))).toBe('Real');
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/mention-render.test.ts`
Expected: FAIL — cannot resolve `./mention-render`.

- [ ] **Step 3: Write minimal implementation** — `src/lib/mention-render.ts`

```ts
export type BodySegment =
  | { type: 'text'; text: string }
  | { type: 'mention'; ownerId: string };

/** Split a wire body into alternating text/mention segments by the
 *  /<@([0-9a-f]{32})>/g token. No tokens → one text segment ('' → []). */
export function tokenizeBody(text: string): BodySegment[] {
  const segments: BodySegment[] = [];
  const re = /<@([0-9a-f]{32})>/g;
  let lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    if (m.index > lastIndex) {
      segments.push({ type: 'text', text: text.slice(lastIndex, m.index) });
    }
    segments.push({ type: 'mention', ownerId: m[1] });
    lastIndex = m.index + m[0].length;
  }
  if (lastIndex < text.length) {
    segments.push({ type: 'text', text: text.slice(lastIndex) });
  }
  return segments;
}

function present(v: string | undefined): string | undefined {
  return v && v.trim() ? v : undefined;
}

/** The single shared resolution ladder: local nickname → broadcast profile
 *  displayName → ownerId.slice(0,8). Returns the BARE label (no leading '@'). */
export function resolveMentionLabel(
  ownerId: string,
  resolveNickname?: (id: string) => string | undefined,
  resolveCard?: (id: string) => { displayName: string } | undefined,
): string {
  return (
    present(resolveNickname?.(ownerId)) ??
    present(resolveCard?.(ownerId)?.displayName) ??
    ownerId.slice(0, 8)
  );
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `npx vitest run src/lib/mention-render.test.ts`
Expected: PASS (all cases).

- [ ] **Step 5: Commit**

```bash
git add src/lib/mention-render.ts src/lib/mention-render.test.ts
git commit -m "feat(mentions): pure render helpers — tokenizeBody + resolveMentionLabel"
```

---

### Task 2: `mention-compose.ts` — pure compose helpers

**Files:**
- Create: `src/lib/mention-compose.ts`
- Test: `src/lib/mention-compose.test.ts`

**Interfaces:**
- Produces:
  - `interface MentionCandidate { ownerId: string; label: string }`
  - `interface TrackedMention { ownerId: string; label: string }`
  - `detectMentionTrigger(text: string, caret: number): { query: string; atIndex: number } | null`
  - `applyMentionPick(text: string, atIndex: number, caret: number, candidate: MentionCandidate): { text: string; caret: number; tracked: TrackedMention }`
  - `filterCandidates(candidates: MentionCandidate[], query: string, limit?: number): MentionCandidate[]`
  - `reconcileCompose(text: string, tracked: TrackedMention[]): { body: string; mentions: string[] }`

- [ ] **Step 1: Write the failing test** — `src/lib/mention-compose.test.ts`

```ts
import { describe, it, expect } from 'vitest';
import {
  detectMentionTrigger,
  applyMentionPick,
  filterCandidates,
  reconcileCompose,
  type MentionCandidate,
} from './mention-compose';

const ID_A = 'a'.repeat(32);
const ID_B = 'b'.repeat(32);

describe('detectMentionTrigger', () => {
  it('detects a trigger at the start', () => {
    expect(detectMentionTrigger('@ja', 3)).toEqual({ query: 'ja', atIndex: 0 });
  });
  it('detects a trigger after whitespace', () => {
    expect(detectMentionTrigger('hey @ja', 7)).toEqual({ query: 'ja', atIndex: 4 });
  });
  it('returns null for an email-like @ (not at a word boundary)', () => {
    expect(detectMentionTrigger('a@b', 3)).toBeNull();
  });
  it('returns null when whitespace sits between @ and caret', () => {
    expect(detectMentionTrigger('@jo bar', 7)).toBeNull();
  });
  it('returns null when there is no @', () => {
    expect(detectMentionTrigger('hello', 5)).toBeNull();
  });
  it('uses the nearest @ and respects its boundary', () => {
    // second @ is not at a boundary → null
    expect(detectMentionTrigger('@a@b', 4)).toBeNull();
    // nearest @ after a space → trigger
    expect(detectMentionTrigger('@a @b', 5)).toEqual({ query: 'b', atIndex: 3 });
  });
  it('an empty query (just typed @) is a trigger', () => {
    expect(detectMentionTrigger('hi @', 4)).toEqual({ query: '', atIndex: 3 });
  });
});

describe('applyMentionPick', () => {
  it('replaces the @query range with "@label " and tracks the id', () => {
    const r = applyMentionPick('hey @ja', 4, 7, { ownerId: ID_A, label: 'Jake (Koya)' });
    expect(r.text).toBe('hey @Jake (Koya) ');
    expect(r.caret).toBe('hey @Jake (Koya) '.length);
    expect(r.tracked).toEqual({ ownerId: ID_A, label: 'Jake (Koya)' });
  });
  it('keeps trailing text after the caret', () => {
    const r = applyMentionPick('@j end', 0, 2, { ownerId: ID_A, label: 'Jay' });
    expect(r.text).toBe('@Jay  end');
  });
});

describe('filterCandidates', () => {
  const cands: MentionCandidate[] = [
    { ownerId: ID_A, label: 'Jake (Koya)' },
    { ownerId: ID_B, label: 'Jasmine' },
    { ownerId: 'c'.repeat(32), label: 'Mike Jakeson' },
  ];
  it('returns all (capped) for an empty query', () => {
    expect(filterCandidates(cands, '', 2)).toHaveLength(2);
  });
  it('case-insensitive substring match', () => {
    expect(filterCandidates(cands, 'jas').map((c) => c.label)).toEqual(['Jasmine']);
  });
  it('prefix matches sort ahead of mid-string matches', () => {
    expect(filterCandidates(cands, 'jak').map((c) => c.label)).toEqual(['Jake (Koya)', 'Mike Jakeson']);
  });
  it('respects the limit', () => {
    expect(filterCandidates(cands, 'ja', 1)).toHaveLength(1);
  });
});

describe('reconcileCompose', () => {
  it('no tracked mentions → body unchanged, empty mentions', () => {
    expect(reconcileCompose('plain text', [])).toEqual({ body: 'plain text', mentions: [] });
  });
  it('rewrites a tracked mention to a token + array', () => {
    expect(reconcileCompose('hey @Jake (Koya) !', [{ ownerId: ID_A, label: 'Jake (Koya)' }])).toEqual({
      body: `hey <@${ID_A}> !`,
      mentions: [ID_A],
    });
  });
  it('drops a pick whose label was edited away (degrades to text)', () => {
    expect(reconcileCompose('hey @Jak !', [{ ownerId: ID_A, label: 'Jake' }])).toEqual({
      body: 'hey @Jak !',
      mentions: [],
    });
  });
  it('longest label wins over a prefix label', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake' },
      { ownerId: ID_B, label: 'Jake (Koya)' },
    ];
    expect(reconcileCompose('@Jake (Koya)', tracked)).toEqual({
      body: `<@${ID_B}>`,
      mentions: [ID_B],
    });
  });
  it('two same-label distinct ids map left-to-right', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake' },
      { ownerId: ID_B, label: 'Jake' },
    ];
    expect(reconcileCompose('@Jake and @Jake', tracked)).toEqual({
      body: `<@${ID_A}> and <@${ID_B}>`,
      mentions: [ID_A, ID_B],
    });
  });
  it('dedupes a repeated same id in the mentions array', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake' },
      { ownerId: ID_A, label: 'Jake' },
    ];
    expect(reconcileCompose('@Jake @Jake', tracked)).toEqual({
      body: `<@${ID_A}> <@${ID_A}>`,
      mentions: [ID_A],
    });
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/mention-compose.test.ts`
Expected: FAIL — cannot resolve `./mention-compose`.

- [ ] **Step 3: Write minimal implementation** — `src/lib/mention-compose.ts`

```ts
export interface MentionCandidate {
  ownerId: string;
  label: string;
}
export interface TrackedMention {
  ownerId: string;
  label: string;
}

/** Detect an active @-trigger at the caret. The '@' must be at start-of-text or
 *  preceded by whitespace; everything from '@' to the caret must be
 *  non-whitespace. Returns the query (after '@') and the '@' index, or null. */
export function detectMentionTrigger(
  text: string,
  caret: number,
): { query: string; atIndex: number } | null {
  for (let i = caret - 1; i >= 0; i--) {
    const ch = text[i];
    if (ch === '@') {
      const before = i === 0 ? '' : text[i - 1];
      if (i === 0 || /\s/.test(before)) {
        return { query: text.slice(i + 1, caret), atIndex: i };
      }
      return null; // '@' not at a word boundary (e.g. email)
    }
    if (/\s/.test(ch)) return null; // whitespace before any '@' → no trigger
  }
  return null;
}

/** Replace the '@query' range [atIndex, caret) with '@<label> ' and return the
 *  new text/caret + the TrackedMention to append. */
export function applyMentionPick(
  text: string,
  atIndex: number,
  caret: number,
  candidate: MentionCandidate,
): { text: string; caret: number; tracked: TrackedMention } {
  const insert = `@${candidate.label} `;
  const newText = text.slice(0, atIndex) + insert + text.slice(caret);
  return {
    text: newText,
    caret: atIndex + insert.length,
    tracked: { ownerId: candidate.ownerId, label: candidate.label },
  };
}

/** Filter+rank the roster: case-insensitive substring on label; prefix matches
 *  first (stable partition); capped to `limit`. */
export function filterCandidates(
  candidates: MentionCandidate[],
  query: string,
  limit = 8,
): MentionCandidate[] {
  const q = query.trim().toLowerCase();
  if (q === '') return candidates.slice(0, limit);
  const matches = candidates.filter((c) => c.label.toLowerCase().includes(q));
  const prefix = matches.filter((c) => c.label.toLowerCase().startsWith(q));
  const rest = matches.filter((c) => !c.label.toLowerCase().startsWith(q));
  return [...prefix, ...rest].slice(0, limit);
}

/** Reconcile the textarea text + picks into the wire payload. Single
 *  left-to-right scan; at each index, among UNCONSUMED tracked entries whose
 *  '@<label>' matches there, take the longest (prefix safety), tie-broken by
 *  insertion order (FIFO → same-label entries map left-to-right). Unmatched
 *  picks degrade to plain text. */
export function reconcileCompose(
  text: string,
  tracked: TrackedMention[],
): { body: string; mentions: string[] } {
  const consumed = new Array(tracked.length).fill(false);
  const order = tracked
    .map((_, idx) => idx)
    .sort((a, b) => {
      const d = tracked[b].label.length - tracked[a].label.length;
      return d !== 0 ? d : a - b; // longer first; original order tiebreak
    });
  let body = '';
  const mentions: string[] = [];
  let i = 0;
  while (i < text.length) {
    let pick = -1;
    for (const idx of order) {
      if (consumed[idx]) continue;
      if (text.startsWith(`@${tracked[idx].label}`, i)) {
        pick = idx;
        break;
      }
    }
    if (pick >= 0) {
      consumed[pick] = true;
      body += `<@${tracked[pick].ownerId}>`;
      if (!mentions.includes(tracked[pick].ownerId)) mentions.push(tracked[pick].ownerId);
      i += tracked[pick].label.length + 1; // skip '@' + label
    } else {
      body += text[i];
      i++;
    }
  }
  return { body, mentions };
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `npx vitest run src/lib/mention-compose.test.ts`
Expected: PASS (all cases).

- [ ] **Step 5: Commit**

```bash
git add src/lib/mention-compose.ts src/lib/mention-compose.test.ts
git commit -m "feat(mentions): pure compose helpers — trigger/pick/filter/reconcile"
```

---

### Task 3: `MentionAutocomplete.svelte` — dumb dropdown

**Files:**
- Create: `src/lib/components/MentionAutocomplete.svelte`
- Test: `src/lib/components/__tests__/MentionAutocomplete.test.ts`

**Interfaces:**
- Consumes: `MentionCandidate` from `mention-compose.ts`.
- Produces: a component with props `{ candidates: MentionCandidate[]; activeIndex: number; onPick: (c: MentionCandidate) => void }`. Renders nothing when `candidates` is empty. Each row has `data-testid="mention-option"`; the active row also has `data-active="true"`.

- [ ] **Step 1: Write the failing test** — `src/lib/components/__tests__/MentionAutocomplete.test.ts`

```ts
import { render, fireEvent, screen } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import MentionAutocomplete from '../MentionAutocomplete.svelte';

const A = { ownerId: 'a'.repeat(32), label: 'Jake (Koya)' };
const B = { ownerId: 'b'.repeat(32), label: 'Jasmine' };

describe('MentionAutocomplete', () => {
  beforeEach(() => vi.clearAllMocks());

  it('renders one row per candidate', () => {
    render(MentionAutocomplete, { props: { candidates: [A, B], activeIndex: 0, onPick: vi.fn() } });
    expect(screen.getAllByTestId('mention-option')).toHaveLength(2);
    expect(screen.getByText('Jake (Koya)')).toBeTruthy();
  });

  it('marks the active row', () => {
    render(MentionAutocomplete, { props: { candidates: [A, B], activeIndex: 1, onPick: vi.fn() } });
    const rows = screen.getAllByTestId('mention-option');
    expect(rows[1].getAttribute('data-active')).toBe('true');
    expect(rows[0].getAttribute('data-active')).not.toBe('true');
  });

  it('renders nothing when there are no candidates', () => {
    const { container } = render(MentionAutocomplete, { props: { candidates: [], activeIndex: 0, onPick: vi.fn() } });
    expect(container.querySelector('[data-testid="mention-option"]')).toBeNull();
  });

  it('calls onPick with the clicked candidate', async () => {
    const onPick = vi.fn();
    render(MentionAutocomplete, { props: { candidates: [A, B], activeIndex: 0, onPick } });
    await fireEvent.click(screen.getByText('Jasmine'));
    expect(onPick).toHaveBeenCalledWith(B);
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/MentionAutocomplete.test.ts`
Expected: FAIL — cannot resolve `../MentionAutocomplete.svelte`.

- [ ] **Step 3: Write minimal implementation** — `src/lib/components/MentionAutocomplete.svelte`

```svelte
<script lang="ts">
  import type { MentionCandidate } from '../mention-compose';

  interface Props {
    candidates: MentionCandidate[];
    activeIndex: number;
    onPick: (c: MentionCandidate) => void;
  }
  const { candidates, activeIndex, onPick }: Props = $props();
</script>

{#if candidates.length > 0}
  <ul class="mention-autocomplete" role="listbox" data-testid="mention-autocomplete">
    {#each candidates as c, i (c.ownerId)}
      <li
        role="option"
        aria-selected={i === activeIndex}
        data-testid="mention-option"
        data-active={i === activeIndex ? 'true' : undefined}
        class="option"
        class:active={i === activeIndex}
      >
        <button type="button" tabindex="-1" onmousedown={(e) => { e.preventDefault(); onPick(c); }}>
          {c.label}
        </button>
      </li>
    {/each}
  </ul>
{/if}

<style>
  .mention-autocomplete {
    position: absolute;
    bottom: 100%;
    left: 0;
    margin: 0 0 0.25rem;
    padding: 0.25rem;
    list-style: none;
    background: var(--bg-secondary, #2a2a2a);
    border: 1px solid var(--border, #444);
    border-radius: 6px;
    max-height: 12rem;
    overflow-y: auto;
    z-index: 50;
    min-width: 12rem;
  }
  .option button {
    display: block;
    width: 100%;
    text-align: left;
    padding: 0.3rem 0.5rem;
    background: transparent;
    border: none;
    color: var(--text-primary, #fff);
    border-radius: 4px;
    cursor: pointer;
    font: inherit;
  }
  .option.active button,
  .option button:hover {
    background: var(--accent, #5865f2);
  }
</style>
```

Note: use `onmousedown` + `preventDefault` (not `onclick`) so the textarea does not lose focus/selection before the pick is applied.

- [ ] **Step 4: Run test to verify it passes**

Run: `npx vitest run src/lib/components/__tests__/MentionAutocomplete.test.ts`
Expected: PASS. (The click test fires a click, which Testing Library dispatches after mousedown; `onmousedown` handler still runs. If the click test is flaky, switch the test to `fireEvent.mouseDown`.)

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/MentionAutocomplete.svelte src/lib/components/__tests__/MentionAutocomplete.test.ts
git commit -m "feat(mentions): MentionAutocomplete presentational dropdown"
```

---

### Task 4: `ChannelMessageFeed` render integration

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte` (render path + `authorLabel` refactor + self-highlight)
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts` (add cases; create the file if absent)

**Interfaces:**
- Consumes: `tokenizeBody`, `resolveMentionLabel` from `mention-render.ts`. Existing props `ownAddress: string`, `resolveCard?`, `resolveNickname?`, and message objects `{ body: number[]; mentions?: string[]; author: string; ... }`.

**Before writing tests:** open `ChannelMessageFeed.svelte` and locate (a) the existing `authorLabel(author)` function and its `nonEmpty` helper, (b) `bodyToText(body)`, (c) the render line `<p class="body">{bodyToText(msg.body)}</p>`, and (d) the message row container element + its `{#each}` over messages. Confirm `ownAddress` is the self owner-id hex.

- [ ] **Step 1: Write the failing test** — add to `src/lib/components/__tests__/ChannelMessageFeed.test.ts`

```ts
// Mentions render — relies on the real mention-render module (no mock).
// Mount ChannelMessageFeed with one message whose body contains a <@id> token
// and assert the rendered output shows a styled @Name resolved via the ladder,
// and that a self-mention marks the row.
//
// NOTE: ChannelMessageFeed has many required props + service deps. Use the
// existing test's mount helper / mocks if this file already exists; otherwise
// build the minimal prop set the component requires (see its Props block) and
// mock channel-message-service like the sibling component tests do.
//
// Pseudocode of the two assertions to add:
//   const ID = 'a'.repeat(32);
//   body bytes = TextEncoder().encode(`hi <@${ID}>`)
//   resolveCard = (id) => id === ID ? { displayName: 'Jake' } : undefined
//   render with messages=[{ ..., author: OTHER, body: [...bytes], mentions: [ID] }], ownAddress = ID
//   expect a [data-testid="mention"] element with text '@Jake'
//   expect that element to have class 'self' (ownAddress === ID)
//   expect the message row to have class 'mentions-me' (ownAddress ∈ mentions)
```

Because `ChannelMessageFeed` is a large component with many deps, write these as concrete tests by copying the mount/mocks from the nearest existing `ChannelMessageFeed` test (or, if none exists, from `DevicesPanel.test.ts`'s mocking style). The two behaviours to pin:
1. a `<@id>` token renders an element `data-testid="mention"` whose text is `@` + the ladder-resolved label;
2. when `ownAddress` is in a message's `mentions`, the row element carries class `mentions-me`, and a mention of `ownAddress` carries class `self`.

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: FAIL — no `data-testid="mention"` element / no `mentions-me` class yet.

- [ ] **Step 3: Implement the render changes** in `src/lib/components/ChannelMessageFeed.svelte`

3a. Add imports at the top of the `<script>`:

```ts
import { tokenizeBody, resolveMentionLabel } from '../mention-render';
```

3b. Refactor `authorLabel` to use the shared ladder (DRY — one ladder). Replace its body with:

```ts
function authorLabel(author: string): string {
  return resolveMentionLabel(author, resolveNickname, resolveCard);
}
```

(Delete the now-unused `nonEmpty` helper ONLY if nothing else references it — grep first; if other call sites use it, leave it.)

3c. Replace the body render. Change:

```svelte
<p class="body">{bodyToText(msg.body)}</p>
```

to:

```svelte
<p class="body">{#each tokenizeBody(bodyToText(msg.body)) as seg}{#if seg.type === 'mention'}<span
        class="mention"
        class:self={seg.ownerId === ownAddress}
        data-testid="mention">@{resolveMentionLabel(seg.ownerId, resolveNickname, resolveCard)}</span>{:else}{seg.text}{/if}{/each}</p>
```

(The `{#each}`/`{#if}` are written without surrounding whitespace so no stray spaces are injected between segments.)

3d. Add the self-mention row class. On the message-row container element inside the messages `{#each}`, add:

```svelte
class:mentions-me={msg.mentions?.includes(ownAddress)}
```

3e. Add scoped styles in the component `<style>`:

```css
.mention {
  color: var(--accent, #5865f2);
  background: color-mix(in srgb, var(--accent, #5865f2) 15%, transparent);
  border-radius: 3px;
  padding: 0 0.15rem;
  font-weight: 500;
}
.mention.self {
  background: color-mix(in srgb, var(--accent, #5865f2) 35%, transparent);
}
.mentions-me {
  /* subtle row highlight when the viewer is mentioned */
  background: color-mix(in srgb, var(--accent, #5865f2) 8%, transparent);
  border-left: 2px solid var(--accent, #5865f2);
}
```

(Match the existing row selector's nesting; if the row already has a background on hover/selected, place `.mentions-me` so it composes sensibly.)

- [ ] **Step 4: Run test to verify it passes**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: PASS. Then `npx tsc --noEmit` (expect clean).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/ChannelMessageFeed.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -m "feat(mentions): render <@id> tokens as resolved styled mentions + self highlight"
```

---

### Task 5: `ChannelMessageFeed` compose integration + `App.svelte` roster

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte` (compose: trigger/pick/track/reconcile + autocomplete + key handling + new prop)
- Modify: `src/App.svelte` (build + pass `mentionCandidates`)
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts` (compose cases)

**Interfaces:**
- Consumes: `detectMentionTrigger`, `applyMentionPick`, `filterCandidates`, `reconcileCompose`, `MentionCandidate`, `TrackedMention` from `mention-compose.ts`; `MentionAutocomplete` component. Existing: `composeText` state, `handleCompose()` send path, `channelMessageService.postMessage(communityId, channelId, body, replyTo, mentions, attachments)`.

**Before writing tests:** locate in `ChannelMessageFeed.svelte` (a) `composeText = $state('')`, (b) the `<textarea>` element + its keydown handler (`handleCompose` fires on Enter), (c) the exact `postMessage(...)` call inside `handleCompose`, and (d) the Props block (to add `mentionCandidates`).

- [ ] **Step 1: Write the failing test** — add to `ChannelMessageFeed.test.ts`

Pin these compose behaviours (build on the same mount/mocks as Task 4; mock `channelMessageService.postMessage` to capture args):
1. Typing `@` then a query shows `data-testid="mention-autocomplete"` populated from `mentionCandidates`.
2. Clicking a candidate inserts `@Label ` into the textarea (assert textarea value).
3. Pressing Enter after a pick sends: `postMessage` called with `body` containing `<@id>` and `mentions: [id]` (assert the captured 5th/6th args).
4. A plain message with no picks sends `mentions` undefined/absent (unchanged behaviour).

```ts
// Sketch (adapt mount to the component's real prop set):
//   const ID = 'a'.repeat(32);
//   const postMessage = vi.fn().mockResolvedValue('msgid');
//   mock channel-message-service so channelMessageService.postMessage = postMessage
//   render(ChannelMessageFeed, { props: { ..., mentionCandidates: [{ ownerId: ID, label: 'Jake' }] } })
//   const ta = screen.getByTestId('channel-compose'); // confirm the real testid/role
//   await fireEvent.input(ta, { target: { value: '@Ja' } });   // also set selectionStart
//   expect(screen.getByTestId('mention-autocomplete')).toBeTruthy();
//   await fireEvent.mouseDown(screen.getByText('Jake'));
//   expect((ta as HTMLTextAreaElement).value).toContain('@Jake ');
//   await fireEvent.keyDown(ta, { key: 'Enter' });
//   const args = postMessage.mock.calls.at(-1);
//   expect(args[2]).toContain(`<@${ID}>`); // body
//   expect(args[4]).toEqual([ID]);          // mentions
```

If the textarea's `data-testid` / send trigger differ, use the real selectors discovered in the "Before writing tests" step.

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: FAIL — no autocomplete renders; `postMessage` still gets `undefined` mentions.

- [ ] **Step 3: Implement compose wiring** in `ChannelMessageFeed.svelte`

3a. Imports:

```ts
import {
  detectMentionTrigger,
  applyMentionPick,
  filterCandidates,
  reconcileCompose,
  type MentionCandidate,
  type TrackedMention,
} from '../mention-compose';
import MentionAutocomplete from './MentionAutocomplete.svelte';
```

3b. Add the prop to the Props block: `mentionCandidates?: MentionCandidate[];` (destructure with default `mentionCandidates = []`).

3c. Add compose state + derived autocomplete:

```ts
let composeEl = $state<HTMLTextAreaElement | null>(null);
let tracked = $state<TrackedMention[]>([]);
let trigger = $state<{ query: string; atIndex: number } | null>(null);
let acIndex = $state(0);
const acCandidates = $derived(trigger ? filterCandidates(mentionCandidates, trigger.query) : []);
const acOpen = $derived(acCandidates.length > 0);
```

3d. Recompute the trigger on input/caret change. In the textarea's `oninput` (and `onkeyup`/`onclick` for caret moves), call:

```ts
function refreshTrigger() {
  const el = composeEl;
  if (!el) { trigger = null; return; }
  trigger = detectMentionTrigger(composeText, el.selectionStart ?? composeText.length);
  acIndex = 0;
}
```

3e. Pick handler:

```ts
function pickMention(c: MentionCandidate) {
  const el = composeEl;
  if (!el || !trigger) return;
  const caret = el.selectionStart ?? composeText.length;
  const r = applyMentionPick(composeText, trigger.atIndex, caret, c);
  composeText = r.text;
  tracked = [...tracked, r.tracked];
  trigger = null;
  // restore caret after Svelte updates the value
  queueMicrotask(() => { el.focus(); el.setSelectionRange(r.caret, r.caret); });
}
```

3f. Intercept keys while the dropdown is open. In the textarea keydown handler, BEFORE the existing Enter-sends logic:

```ts
if (acOpen) {
  if (e.key === 'ArrowDown') { e.preventDefault(); acIndex = (acIndex + 1) % acCandidates.length; return; }
  if (e.key === 'ArrowUp') { e.preventDefault(); acIndex = (acIndex - 1 + acCandidates.length) % acCandidates.length; return; }
  if (e.key === 'Enter') { e.preventDefault(); pickMention(acCandidates[acIndex]); return; }
  if (e.key === 'Escape') { e.preventDefault(); trigger = null; return; }
}
```

3g. In `handleCompose` (send), reconcile before posting. Replace the existing `postMessage(communityId, channelId, composeText, undefined, undefined, ...)` call with:

```ts
const { body, mentions } = reconcileCompose(composeText, tracked);
const messageId = await channelMessageService.postMessage(
  communityId, channelId, body, replyToId /* existing var or undefined */, mentions, pendingAttachments,
);
```

and after a successful send clear `tracked = []` alongside the existing `composeText = ''`.

3h. Render the dropdown next to the textarea (inside a `position: relative` wrapper around the compose area):

```svelte
{#if acOpen}
  <MentionAutocomplete candidates={acCandidates} activeIndex={acIndex} onPick={pickMention} />
{/if}
```

and bind the textarea: `bind:this={composeEl}` plus `oninput={refreshTrigger}` `onkeyup={refreshTrigger}` `onclick={refreshTrigger}` (keep the existing `bind:value={composeText}` and keydown handler).

- [ ] **Step 4: Wire the roster in `src/App.svelte`**

Find where `<ChannelMessageFeed ... />` is rendered. Build candidates from the existing selected-community member roster (the same source feeding the members panel; it exposes `address` + a resolvable name) and the existing `resolveNickname`/`resolveCard`:

```svelte
<ChannelMessageFeed
  ...existing props...
  mentionCandidates={selectedCommunityMembers.map((m) => ({
    ownerId: m.address,
    label: resolveMentionLabel(m.address, resolveNicknameFn, resolveCardFn),
  }))}
/>
```

Use the existing member-roster state variable and the existing nickname/card resolver functions (the same ones already passed as `resolveNickname`/`resolveCard` to the feed). Import `resolveMentionLabel` from `./lib/mention-render`. If the roster is a `Map`/cache, adapt the `.map(...)` accordingly. Exclude self if desired (optional; harmless to include).

- [ ] **Step 5: Run tests + gates**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: PASS.
Run: `npx tsc --noEmit` → clean.
Run: `npx vitest run` → full suite green.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/ChannelMessageFeed.svelte src/App.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -m "feat(mentions): compose @-autocomplete + reconcile-on-send + roster wiring"
```

---

## Final verification (before PR)

- [ ] `npx tsc --noEmit` clean.
- [ ] `npx vitest run` — full suite green.
- [ ] Manual sanity (if running the app): type `@`, pick a member, send; the message shows a styled `@Name`; mentioning yourself highlights the row.
- [ ] Open PR (branch `channel-mentions`), title without a ZEB id, body references ZEB-588; trigger CodeRabbit per cadence.
