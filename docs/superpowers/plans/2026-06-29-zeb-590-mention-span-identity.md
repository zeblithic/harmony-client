# ZEB-590: Stable per-insertion span identity for tracked mentions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give each picked @-mention a stable span identity (the offset of its inserted `@label`) so reconciliation matches by position, and invalidate a pick whose span is edited/deleted — closing the delete-then-retype-identical-label corruption case left by ZEB-588.

**Architecture:** All new logic lives in the existing pure module `src/lib/mention-compose.ts` (no DOM, exhaustively unit-testable). `TrackedMention` gains a `start: number` offset; a new pure `shiftTrackedSpans(prevText, nextText, tracked)` shifts/keeps/invalidates each span against the single contiguous edit between two compose snapshots; `reconcileCompose` becomes position-anchored (matches the entry anchored at each offset, keeping the existing boundary guards as defense-in-depth). The Svelte consumer `src/lib/components/ChannelMessageFeed.svelte` gains a `prevComposeText` shadow and feeds every edit through `shiftTrackedSpans`.

**Tech Stack:** TypeScript, Vitest, Svelte 5 (runes). Frontend only — no Rust changes.

## Global Constraints

- **Fail-safe invariant (load-bearing):** the worst case is a mention that should have tokenized degrading to literal text; **a wrong id is never emitted.** Every ambiguity resolves toward plain text.
- **Span definition:** a pick's span is the half-open range `[start, start + 1 + label.length)`, matching exactly the substring `@${label}`. The trailing space `applyMentionPick` inserts (`@${label} `) is a **boundary**, not part of the span.
- **No wire/render changes:** `<@ownerId>` tokens + `mentions[]` array are unchanged; `mention-render.ts`, notifications, and the wire format are untouched.
- **Gates (the `frontend` CI job), both run from the repo root, must be green before PR:** `npx tsc --noEmit` and `npx vitest run`.
- **Branch:** `mention-span-identity` (already created, spec committed at `7409bc70`). Keep `ZEB-590` out of branch/commit titles; it goes in the PR body only.

---

## File Structure

- `src/lib/mention-compose.ts` — the pure module. Add `TrackedMention.start`; `applyMentionPick` sets it; add `shiftTrackedSpans`; rewrite `reconcileCompose` to be position-anchored.
- `src/lib/mention-compose.test.ts` — add a `shiftTrackedSpans` suite; thread `start` through the existing `applyMentionPick`/`reconcileCompose` cases; replace the now-impossible "longest label wins" case; add the position-anchored headline case.
- `src/lib/components/ChannelMessageFeed.svelte` — `prevComposeText` shadow + `shiftTrackedSpans` call in the input handler + reset/insert bookkeeping. Import `shiftTrackedSpans`.

---

## Task 1: Data model + `applyMentionPick.start` + `shiftTrackedSpans`

Adds the `start` field, makes `applyMentionPick` populate it, threads `start` through the existing test literals (so the suite compiles with the new required field — `reconcileCompose` is **not** touched in this task and its old label-rescan logic still passes), and introduces the pure `shiftTrackedSpans` function with its full test suite. The headline delete-then-retype regression lives here, because invalidation (overlap-drop on the delete) is the half of the fix that actually kills the stale pick.

**Files:**
- Modify: `src/lib/mention-compose.ts` (`TrackedMention` interface ~line 15; `applyMentionPick` ~line 43; add `shiftTrackedSpans` after `applyMentionPick`)
- Modify: `src/lib/mention-compose.test.ts` (`applyMentionPick` suite ~line 38; all `reconcileCompose` literals ~line 74; add a new `shiftTrackedSpans` suite)

**Interfaces:**
- Produces:
  - `interface TrackedMention { ownerId: string; label: string; start: number }`
  - `applyMentionPick(...)` return `.tracked.start === atIndex`
  - `shiftTrackedSpans(prevText: string, nextText: string, tracked: TrackedMention[]): TrackedMention[]`

- [ ] **Step 1: Update the `applyMentionPick` test for `start` (failing on the new field)**

In `src/lib/mention-compose.test.ts`, replace the first `applyMentionPick` case's `tracked` assertion so it expects `start`:

```ts
  it('replaces the @query range with "@label " and tracks the id', () => {
    const r = applyMentionPick('hey @ja', 4, 7, { ownerId: ID_A, label: 'Jake (Koya)' });
    expect(r.text).toBe('hey @Jake (Koya) ');
    expect(r.caret).toBe('hey @Jake (Koya) '.length);
    expect(r.tracked).toEqual({ ownerId: ID_A, label: 'Jake (Koya)', start: 4 });
  });
```

- [ ] **Step 2: Run it to verify it fails**

Run: `npx vitest run src/lib/mention-compose.test.ts -t "tracks the id"`
Expected: FAIL — `r.tracked` is `{ ownerId, label }` (no `start`), so `toEqual` mismatches.

- [ ] **Step 3: Add `start` to the interface and have `applyMentionPick` set it**

In `src/lib/mention-compose.ts`, change the interface:

```ts
export interface TrackedMention {
  ownerId: string;
  label: string;
  /** Offset of the '@' of this pick's inserted "@label" in the current compose
   *  text. Span = [start, start + 1 + label.length), matching "@label" exactly
   *  (the trailing space is a boundary). Maintained by shiftTrackedSpans across
   *  edits; reconcileCompose matches by this offset, not by label rescan. */
  start: number;
}
```

And in `applyMentionPick`, set `start` to the insertion offset:

```ts
  return {
    text: newText,
    caret: atIndex + insert.length,
    tracked: { ownerId: candidate.ownerId, label: candidate.label, start: atIndex },
  };
```

- [ ] **Step 4: Thread `start` through the existing `reconcileCompose` test literals (compile fix; behavior unchanged)**

`reconcileCompose` is untouched in this task, so its old label-rescan logic still produces the same results — but the literals must carry the now-required `start`. In `src/lib/mention-compose.test.ts`, set `start` to the offset of each `@label` in its test text. Replace the whole `reconcileCompose` describe block's bodies as follows (keep the `describe`/`it` titles):

```ts
  it('rewrites a tracked mention to a token + array', () => {
    expect(
      reconcileCompose('hey @Jake (Koya) !', [{ ownerId: ID_A, label: 'Jake (Koya)', start: 4 }]),
    ).toEqual({ body: `hey <@${ID_A}> !`, mentions: [ID_A] });
  });
  it('drops a pick whose label was edited away (degrades to text)', () => {
    expect(
      reconcileCompose('hey @Jak !', [{ ownerId: ID_A, label: 'Jake', start: 4 }]),
    ).toEqual({ body: 'hey @Jak !', mentions: [] });
  });
  it('does NOT tokenize a pick extended at the right edge (@JakeX)', () => {
    expect(
      reconcileCompose('@JakeX', [{ ownerId: ID_A, label: 'Jake', start: 0 }]),
    ).toEqual({ body: '@JakeX', mentions: [] });
    expect(
      reconcileCompose('@Jake2 hi', [{ ownerId: ID_A, label: 'Jake', start: 0 }]),
    ).toEqual({ body: '@Jake2 hi', mentions: [] });
  });
  it('does NOT tokenize a label merged into a word/email (left boundary)', () => {
    expect(
      reconcileCompose('mail a@Jake', [{ ownerId: ID_A, label: 'Jake', start: 6 }]),
    ).toEqual({ body: 'mail a@Jake', mentions: [] });
  });
  it('tokenizes a mention followed immediately by whitespace or end', () => {
    expect(
      reconcileCompose('@Jake', [{ ownerId: ID_A, label: 'Jake', start: 0 }]),
    ).toEqual({ body: `<@${ID_A}>`, mentions: [ID_A] });
    expect(
      reconcileCompose('@Jake\n', [{ ownerId: ID_A, label: 'Jake', start: 0 }]),
    ).toEqual({ body: `<@${ID_A}>\n`, mentions: [ID_A] });
  });
  it('two same-label distinct ids map left-to-right', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake', start: 0 },
      { ownerId: ID_B, label: 'Jake', start: 10 },
    ];
    expect(reconcileCompose('@Jake and @Jake', tracked)).toEqual({
      body: `<@${ID_A}> and <@${ID_B}>`,
      mentions: [ID_A, ID_B],
    });
  });
  it('dedupes a repeated same id in the mentions array', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake', start: 0 },
      { ownerId: ID_A, label: 'Jake', start: 6 },
    ];
    expect(reconcileCompose('@Jake @Jake', tracked)).toEqual({
      body: `<@${ID_A}> <@${ID_A}>`,
      mentions: [ID_A],
    });
  });
```

> Note: the `'no tracked mentions → body unchanged'` case (`reconcileCompose('plain text', [])`) needs no change (empty array). The `'longest label wins over a prefix label'` case is removed in Task 2 (position anchoring makes a same-offset prefix collision impossible by construction), so leave it as-is here **but** add `start: 0` to both its literals so the file compiles:

```ts
  it('longest label wins over a prefix label', () => {
    const tracked = [
      { ownerId: ID_A, label: 'Jake', start: 0 },
      { ownerId: ID_B, label: 'Jake (Koya)', start: 0 },
    ];
    expect(reconcileCompose('@Jake (Koya)', tracked)).toEqual({
      body: `<@${ID_B}>`,
      mentions: [ID_B],
    });
  });
```

(Old label-rescan logic ignores `start`, so this still passes in Task 1. Task 2 replaces it.)

- [ ] **Step 5: Run the full module suite to confirm green after the compile fix**

Run: `npx vitest run src/lib/mention-compose.test.ts`
Expected: PASS — all existing cases green with `start` present (reconcile logic unchanged), and the updated `applyMentionPick` case now green.

- [ ] **Step 6: Write the failing `shiftTrackedSpans` suite**

Append this describe block to `src/lib/mention-compose.test.ts` (and add `shiftTrackedSpans` to the import at the top of the file):

```ts
describe('shiftTrackedSpans', () => {
  const span = (start: number, label: string, ownerId = ID_A) => ({ ownerId, label, start });

  it('shifts a span when text is inserted before it', () => {
    // 'hi @Jake' → 'yo hi @Jake' : '@Jake' moves from offset 3 to 6.
    expect(shiftTrackedSpans('hi @Jake', 'yo hi @Jake', [span(3, 'Jake')])).toEqual([
      span(6, 'Jake'),
    ]);
  });
  it('keeps a span unchanged when text is appended after it', () => {
    // 'Jake' span ends exactly at the edit point (p == end) → kept.
    expect(shiftTrackedSpans('@Jake', '@Jake!!', [span(0, 'Jake')])).toEqual([span(0, 'Jake')]);
  });
  it('drops a span whose whole text was deleted', () => {
    expect(shiftTrackedSpans('hi @Jake !', 'hi  !', [span(3, 'Jake')])).toEqual([]);
  });
  it('drops a span edited in the middle', () => {
    expect(shiftTrackedSpans('@Jake', '@JXke', [span(0, 'Jake')])).toEqual([]);
  });
  it('delete-then-retype regression: deletion drops the pick, retype is plain text', () => {
    // Headline bug: the span is invalidated on delete; a later identical retype
    // has no tracked entry, so reconcile emits no id.
    const afterDelete = shiftTrackedSpans('@Jake ', '', [span(0, 'Jake')]);
    expect(afterDelete).toEqual([]);
    expect(reconcileCompose('@Jake ', afterDelete)).toEqual({ body: '@Jake ', mentions: [] });
  });
  it('with two mentions, an edit between them keeps the earlier and shifts the later', () => {
    // '@Al @Bob' → '@AlXX @Bob' : 'Al' stays at 0, 'Bob' shifts 4 → 6.
    const tracked = [span(0, 'Al'), span(4, 'Bob', ID_B)];
    expect(shiftTrackedSpans('@Al @Bob', '@AlXX @Bob', tracked)).toEqual([
      span(0, 'Al'),
      span(6, 'Bob', ID_B),
    ]);
  });
  it('drops a span when a paste-over-selection covers it, keeping the survivor', () => {
    // Select '@Bob' (offsets 4..7) and paste 'ZZZ' → 'Bob' invalidated, 'Al' kept.
    const tracked = [span(0, 'Al'), span(4, 'Bob', ID_B)];
    expect(shiftTrackedSpans('@Al @Bob end', '@Al ZZZ end', tracked)).toEqual([span(0, 'Al')]);
  });
  it('returns the spans unchanged on a no-op edit', () => {
    expect(shiftTrackedSpans('@Jake', '@Jake', [span(0, 'Jake')])).toEqual([span(0, 'Jake')]);
  });
});
```

- [ ] **Step 7: Run it to verify it fails**

Run: `npx vitest run src/lib/mention-compose.test.ts -t "shiftTrackedSpans"`
Expected: FAIL — `shiftTrackedSpans is not a function` (not yet exported).

- [ ] **Step 8: Implement `shiftTrackedSpans`**

In `src/lib/mention-compose.ts`, add this function immediately after `applyMentionPick`:

```ts
/** Maintain tracked-mention spans across a single compose edit.
 *
 * Derives the one contiguous edit between `prevText` and `nextText` (longest
 * common prefix `p` + capped common suffix `s`), then for each span
 * [start, start + 1 + label.length):
 *   1. edit entirely at/after the span (p >= end)           → keep unchanged
 *   2. edit entirely before the span (prevLen - s <= start) → shift by delta
 *   3. edit overlaps the span                               → drop (invalidate)
 * Order is preserved. Fail-safe: an edit that touches a span removes it, so the
 * span can never be matched against text it no longer covers. */
export function shiftTrackedSpans(
  prevText: string,
  nextText: string,
  tracked: TrackedMention[],
): TrackedMention[] {
  if (prevText === nextText) return tracked;
  const prevLen = prevText.length;
  const nextLen = nextText.length;
  // Longest common prefix.
  let p = 0;
  const maxP = Math.min(prevLen, nextLen);
  while (p < maxP && prevText[p] === nextText[p]) p++;
  // Longest common suffix, capped so prefix and suffix don't overlap.
  let s = 0;
  const maxS = Math.min(prevLen - p, nextLen - p);
  while (s < maxS && prevText[prevLen - 1 - s] === nextText[nextLen - 1 - s]) s++;
  const delta = nextLen - prevLen;
  const editEnd = prevLen - s; // exclusive end of the edited region in prevText
  const out: TrackedMention[] = [];
  for (const m of tracked) {
    const end = m.start + 1 + m.label.length;
    if (p >= end) {
      out.push(m); // case 1: edit at/after the span
    } else if (editEnd <= m.start) {
      const shifted = m.start + delta; // case 2: edit before the span
      if (shifted >= 0) out.push({ ...m, start: shifted });
    }
    // else: case 3 — overlap → drop
  }
  return out;
}
```

- [ ] **Step 9: Run the full module suite to confirm green**

Run: `npx vitest run src/lib/mention-compose.test.ts`
Expected: PASS — `shiftTrackedSpans` suite green, all prior cases still green.

- [ ] **Step 10: Type-check and commit**

Run: `npx tsc --noEmit`
Expected: no errors.

```bash
git add src/lib/mention-compose.ts src/lib/mention-compose.test.ts
git commit -m "Mentions: span identity for tracked picks (start offset + shiftTrackedSpans)"
```

---

## Task 2: Position-anchored `reconcileCompose`

Rewrites `reconcileCompose` to match each pick at its own anchored offset (`start`) instead of rescanning by label, removing the longest-label tiebreak and `consumed[]` bookkeeping. The boundary guards are retained as defense-in-depth. This is the hardening half of the fix: a stale entry can no longer claim an identical label typed at a different offset.

**Files:**
- Modify: `src/lib/mention-compose.ts` (`reconcileCompose` ~line 78)
- Modify: `src/lib/mention-compose.test.ts` (`reconcileCompose` suite: add the anchored headline case; replace the `'longest label wins'` case)

**Interfaces:**
- Consumes: `TrackedMention.start` (Task 1).
- Produces: `reconcileCompose(text: string, tracked: TrackedMention[]): { body: string; mentions: string[] }` — same signature, position-anchored semantics.

- [ ] **Step 1: Write the failing headline test (position anchoring)**

Add this case to the `reconcileCompose` describe block in `src/lib/mention-compose.test.ts`:

```ts
  it('tokenizes only the pick at its anchored offset, not an identical label elsewhere', () => {
    // The real pick is at offset 6; the user also typed a bare "@Jake" at
    // offset 0. Label-rescan would claim the offset-0 one (wrong); position
    // anchoring tokenizes only the actual pick (ZEB-590).
    expect(
      reconcileCompose('@Jake @Jake', [{ ownerId: ID_A, label: 'Jake', start: 6 }]),
    ).toEqual({ body: `@Jake <@${ID_A}>`, mentions: [ID_A] });
  });
```

- [ ] **Step 2: Run it to verify it fails**

Run: `npx vitest run src/lib/mention-compose.test.ts -t "tokenizes only the pick at its anchored offset"`
Expected: FAIL — old label-rescan logic tokenizes the offset-0 occurrence, yielding `body: '<@…> @Jake'`.

- [ ] **Step 3: Replace the `'longest label wins'` case with a single multi-word-label case**

Position anchoring makes a same-offset prefix collision impossible (each pick owns a distinct insertion offset), so the longest-label tiebreak no longer exists. In `src/lib/mention-compose.test.ts`, replace the whole `'longest label wins over a prefix label'` `it(...)` with:

```ts
  it('tokenizes a full multi-word label as one span', () => {
    expect(
      reconcileCompose('@Jake (Koya)', [{ ownerId: ID_B, label: 'Jake (Koya)', start: 0 }]),
    ).toEqual({ body: `<@${ID_B}>`, mentions: [ID_B] });
  });
```

- [ ] **Step 4: Rewrite `reconcileCompose` to be position-anchored**

In `src/lib/mention-compose.ts`, replace the entire `reconcileCompose` function (and update its doc comment) with:

```ts
/** Reconcile the textarea text + picks into the wire payload. Each pick owns the
 *  exact offset where its '@label' was inserted (`start`), so we match a pick
 *  ONLY at its anchor — a manually-retyped identical label elsewhere is never
 *  claimed. Boundary guards are retained as defense-in-depth: a span left
 *  adjacent to appended text (e.g. "@JakeX") still degrades to plain text rather
 *  than corrupting the body. Unmatched picks degrade to plain text; the mentions
 *  array dedupes in first-seen order. */
export function reconcileCompose(
  text: string,
  tracked: TrackedMention[],
): { body: string; mentions: string[] } {
  const byStart = new Map<number, TrackedMention>();
  for (const m of tracked) {
    if (!byStart.has(m.start)) byStart.set(m.start, m); // first-wins on a dup start
  }
  let body = '';
  const mentions: string[] = [];
  let i = 0;
  while (i < text.length) {
    const m = byStart.get(i);
    if (m && text.startsWith(`@${m.label}`, i)) {
      // Left boundary: '@' must start the text or follow whitespace.
      const leftOk = i === 0 || /\s/.test(text[i - 1]);
      // Right boundary: end-of-text or whitespace after the label.
      const j = i + 1 + m.label.length;
      const rightOk = j === text.length || /\s/.test(text[j]);
      if (leftOk && rightOk) {
        body += `<@${m.ownerId}>`;
        if (!mentions.includes(m.ownerId)) mentions.push(m.ownerId);
        i = j;
        continue;
      }
    }
    body += text[i];
    i++;
  }
  return { body, mentions };
}
```

- [ ] **Step 5: Run the full module suite to confirm green**

Run: `npx vitest run src/lib/mention-compose.test.ts`
Expected: PASS — headline anchored case green, multi-word-label case green, all other cases (now expressed via distinct `start`s) green.

- [ ] **Step 6: Type-check and commit**

Run: `npx tsc --noEmit`
Expected: no errors.

```bash
git add src/lib/mention-compose.ts src/lib/mention-compose.test.ts
git commit -m "Mentions: position-anchored reconcile (match each pick at its own offset)"
```

---

## Task 3: Component wiring (`ChannelMessageFeed.svelte`)

Feeds every compose edit through `shiftTrackedSpans` so spans stay aligned (and stale picks get invalidated) before send. Maintains a `prevComposeText` shadow holding the last text the shifter saw.

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte` (import ~top; `composeText` decl line 106; `refreshTrigger` lines 135-143; `pickMention` lines 145-158; channel-switch reset lines 182-187; post-send reset lines 421-424)

**Interfaces:**
- Consumes: `shiftTrackedSpans(prevText, nextText, tracked)` (Task 1).

- [ ] **Step 1: Import `shiftTrackedSpans`**

Find the existing import from `mention-compose` (it brings in `detectMentionTrigger`, `applyMentionPick`, `filterCandidates`, `reconcileCompose`, and the `TrackedMention`/`MentionCandidate` types) and add `shiftTrackedSpans` to the named imports. Verify the current import shape first:

Run: `grep -n "mention-compose" src/lib/components/ChannelMessageFeed.svelte`

Then add `shiftTrackedSpans` to that import list (alongside `reconcileCompose`).

- [ ] **Step 2: Declare the `prevComposeText` shadow**

Immediately after `let composeText = $state('');` (line 106), add:

```ts
  // ZEB-590: last compose text the span-shifter has seen. Kept in lockstep with
  // composeText so shiftTrackedSpans can diff each edit. A plain (non-$state)
  // shadow: it's never read in the template, only by the input handler.
  let prevComposeText = '';
```

- [ ] **Step 3: Shift spans on every edit in `refreshTrigger`**

In `refreshTrigger` (lines 135-143), after the `if (!el) { trigger = null; return; }` guard and before the `detectMentionTrigger` call, insert the shift + shadow update:

```ts
  function refreshTrigger() {
    const el = composeEl;
    if (!el) {
      trigger = null;
      return;
    }
    // ZEB-590: realign / invalidate tracked spans against the just-applied edit
    // BEFORE re-detecting the trigger, so a deleted-then-retyped label can't be
    // reclaimed by a stale pick.
    tracked = shiftTrackedSpans(prevComposeText, el.value, tracked);
    prevComposeText = el.value;
    trigger = detectMentionTrigger(el.value, el.selectionStart ?? el.value.length);
    acIndex = 0;
  }
```

- [ ] **Step 4: Keep the shadow synced on a programmatic pick in `pickMention`**

`pickMention` sets `composeText = r.text` programmatically (no input event fires), so the shadow must be advanced to the post-insert text — the new entry's `start` is already in post-insert coordinates. After `tracked = [...tracked, r.tracked];` (line 151), add:

```ts
    composeText = r.text;
    tracked = [...tracked, r.tracked];
    // ZEB-590: the next user edit must diff against the post-insert text.
    prevComposeText = r.text;
```

- [ ] **Step 5: Reset the shadow post-send (NOT on channel switch)**

In the send handler, `composeText` is cleared to `''` (line 421); reset the shadow alongside it. After `composeText = '';` add:

```ts
      composeText = '';
      prevComposeText = '';
      tracked = [];
```

Do **not** touch `prevComposeText` in the channel-switch `$effect` (lines 182-187). Unlike post-send, that effect deliberately preserves `composeText` as a draft; the shadow stays paired with it, and since `tracked` is reset to `[]` there the shadow is moot anyway. Add a one-line comment at the existing `tracked = [];` (line 185) noting this:

```ts
    // ZEB-588 (CodeRabbit): drop in-progress picks on a channel/community switch.
    // ZEB-590: prevComposeText is intentionally NOT reset here — composeText is a
    // preserved draft across switches, so the shadow stays paired with it; tracked
    // is empty here regardless, so no span can be mis-shifted.
    tracked = [];
```

- [ ] **Step 6: Type-check and run the frontend suite**

Run: `npx tsc --noEmit`
Expected: no errors.

Run: `npx vitest run`
Expected: PASS — the full frontend suite, including the `mention-compose` module tests. (No new Svelte-component test: the compose logic is fully covered by the pure-module tests, matching the module's existing strategy.)

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/ChannelMessageFeed.svelte
git commit -m "Mentions: wire span-shifting into the channel compose box"
```

---

## Task 4: Final gates + PR

- [ ] **Step 1: Full frontend gates from a clean state**

Run: `npx tsc --noEmit && npx vitest run`
Expected: both green (the two `frontend` CI gates).

- [ ] **Step 2: Push and open the PR**

```bash
git push -u origin mention-span-identity
```

Open the PR against `zeblithic/harmony-client` with `gh pr create --repo zeblithic/harmony-client`. Body must include:
- `Closes ZEB-590` (PR body only — never the branch/commit titles).
- The bug recap (delete-then-retype-identical-label → wrong id) and the two-part fix (invalidation via `shiftTrackedSpans` + position-anchored reconcile).
- The fail-safe invariant (never emit a wrong id; ambiguity degrades to plain text).
- A note that this corrects a minor spec wording inaccuracy: `prevComposeText` is **not** reset on channel switch (composeText is a preserved draft there, and tracked is already cleared) — only post-send.
- Scope note: stays within the plain-`<textarea>` model; the contenteditable-chips successor is ZEB-594.
- Test plan: `npx tsc --noEmit` + `npx vitest run` green; new `shiftTrackedSpans` suite + position-anchored reconcile cases.
- Spec `7409bc70` + this plan path.

- [ ] **Step 3: Trigger CodeRabbit immediately after push, then converge bots**

Post `@coderabbitai review` as an issue-comment on the PR right after it's open (Qodo + CodeAnt auto-run). Scan all three comment buckets (inline threads + issue-comments + reviews) each round; address findings; re-trigger CodeRabbit after each meaningful push. Ignore CodeAnt's plan-doc rule (known false positive). Never trigger Greptile. Jake is the sole merge gate.

---

## Self-Review

**1. Spec coverage** (against `docs/superpowers/specs/2026-06-29-zeb-590-mention-span-identity-design.md`):
- §1 data model (`start`, span `[start, start+1+label.length)`, `applyMentionPick` sets `start`) → Task 1 Steps 3.
- §2 `shiftTrackedSpans` (prefix/suffix derivation, three disjoint cases, order preserved) → Task 1 Steps 6-8.
- §3 position-anchored `reconcileCompose` (start→entry map, boundary guards retained, longest-label/`consumed[]` removed) → Task 2 Steps 3-4.
- §4 wiring (`prevComposeText` shadow, shift in input handler, sync on pick, reset post-send) → Task 3 Steps 2-5. **Refinement noted:** spec said reset on channel switch too; corrected to "post-send only" with rationale (preserved draft) — captured in Task 3 Step 5 and the PR body.
- §Testing (shift suite incl. delete-retype regression; `applyMentionPick.start`; position-anchored reconcile incl. the "label substring at a different offset is not tokenized" positive case) → Task 1 Step 6, Task 2 Step 1.
- §Gates (`tsc` + `vitest`) → Task 1 Step 10, Task 2 Step 6, Task 3 Step 6, Task 4 Step 1.

**2. Placeholder scan:** no TBD/TODO/"handle edge cases"; every code step shows complete code.

**3. Type consistency:** `shiftTrackedSpans(prevText, nextText, tracked): TrackedMention[]`, `TrackedMention.start: number`, `applyMentionPick().tracked.start`, and `reconcileCompose(text, tracked)` are named identically across Tasks 1-3 and the test literals. The `span(start, label, ownerId)` test helper matches the `{ ownerId, label, start }` field order via property names (order-independent).
