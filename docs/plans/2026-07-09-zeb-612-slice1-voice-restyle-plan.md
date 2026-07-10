# ZEB-612 Slice 1: VoiceChannelView Commons Restyle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Faithful Commons restyle of the shipped `VoiceChannelView.svelte` per spec §2 (`docs/specs/2026-07-09-zeb-612-commons-i-town-hall-vines-files-design.md`) — **zero behavior change**.

**Architecture:** Single-component restyle. All states already exist (join-muted pane, PTT hold, mod-silenced, channel-full, roster grid/list, mod controls); this slice changes copy, badges, tints, radii, and reveals — never handlers, session calls, or state logic. Tests pin the new copy/anatomy first (TDD), then markup/CSS changes make them pass while every existing test stays green.

**Tech Stack:** Svelte 5 (runes), vitest + @testing-library/svelte, Commons tokens in `src/app.css` (no new tokens).

## Global Constraints

- **Zero behavior change:** no edits to handlers, session method calls, `$state`/`$derived` logic, event wiring, or `GRID_MAX`. Only markup structure, classes, copy, and `<style>`.
- **style-token-guard budget-0:** colors only via existing `var(--*)` (and `color-mix(in srgb, var(--*) N%, transparent)`). `VoiceChannelView.svelte` has no allowlist entry; do not add one.
- Exact design copy (spec §2, TH frames B/C):
  - join hint: `You'll join muted — unmute when you're ready.` (already present — must not change)
  - channel-full: `Voice channel full — try again later.` (already present — must not change)
  - mod-silenced note: `🛡 You've been muted by a moderator. Your talk controls are disabled until they unmute you.`
  - PTT held label: `🎙 Transmitting… (hold Space)`
  - PTT hold title: `Release to go quiet. Replaces the mute toggle while PTT mode is on.`
  - deafen idle label: `🎧 Deafen` (deafened stays `🔕 Deafened`)
  - mod-muted tile sub-label: `mod-muted`
- Preserved test pins (must stay passing unchanged unless a step explicitly updates them): `role="alert"` + `/voice channel full/i`; `data-testid` values `voice-mic-blocked`, `voice-stage`, `voice-grid`, `voice-tile`, `voice-list`, `voice-list-row`, `mod-muted-badge`, `mod-mute`, `mod-remove`, `mod-remove-confirm`, `self-mod-muted`, `self-kicked`, `ptt-hold`, `voice-reconnecting`; aria-labels on mod buttons and `Hold to talk (or hold Space)`; `speaking` class on tiles/rows.
- Gates per task: `npx tsc --noEmit` + `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts`; final: full `npx vitest run` + `npx vitest run src/style-token-guard.test.ts`.
- Commits on `zeb-612-slice1-voice-restyle`; trailers:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`

**Files (whole plan):**
- Modify: `src/lib/components/VoiceChannelView.svelte`
- Test: `src/lib/components/__tests__/VoiceChannelView.test.ts`

---

### Task 1: State banners + join pane

**Files:**
- Modify: `src/lib/components/VoiceChannelView.svelte` (template ~:123-155; styles `.voice-error`, `.voice-mod-note`, `.voice-mic-blocked`, `.voice-join-pane`, `.btn-primary`, `.voice-count`)
- Test: `src/lib/components/__tests__/VoiceChannelView.test.ts`

**Interfaces:**
- Consumes: existing `fakeSession`/`base`/`roster` helpers in the test file.
- Produces: `.voice-full-note` class (channel-full banner, still `role="alert"`); extended mod-silenced copy; join-pane glyph markup (`data-testid="join-glyph"`). Task 2/3 don't depend on these names.

- [ ] **Step 1: Write the failing tests** — append a new describe block at the end of `VoiceChannelView.test.ts`:

```typescript
describe('VoiceChannelView (ZEB-612 slice 1): Commons restyle — banners + join pane', () => {
  it('channel-full banner is its own clay note (not the danger error class), still an alert', () => {
    const session = fakeSession({ phase: 'idle', channelFull: true });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const alert = screen.getByRole('alert');
    expect(alert).toHaveTextContent(/voice channel full — try again later/i);
    expect(alert.className).toMatch(/voice-full-note/);
    expect(alert.className).not.toMatch(/voice-error/);
  });

  it('join errors still use the danger error class', async () => {
    // Pin that only channel-full moved off .voice-error — real errors keep it.
    const session = fakeSession({ phase: 'idle' });
    (session.join as ReturnType<typeof vi.fn>).mockRejectedValueOnce(new Error('boom'));
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByRole('button', { name: /join/i }));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent(/boom/);
    expect(alert.className).toMatch(/voice-error/);
  });

  it('mod-silenced note carries the full Commons copy', () => {
    const session = fakeSession({ phase: 'connected', selfModMuted: true });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const note = screen.getByTestId('self-mod-muted');
    expect(note).toHaveTextContent(/You've been muted by a moderator/);
    expect(note).toHaveTextContent(/talk controls are disabled until they unmute you/i);
  });

  it('join pane shows the room glyph and keeps the join-muted hint verbatim', () => {
    const session = fakeSession({ phase: 'idle' });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('join-glyph')).toBeInTheDocument();
    expect(
      screen.getByText("You'll join muted — unmute when you're ready.")
    ).toBeInTheDocument();
  });
});
```

- [ ] **Step 2: Run the new tests, verify they fail**

Run: `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts -t 'ZEB-612 slice 1'`
Expected: 4 failing — `voice-full-note` class missing, extended copy missing, `join-glyph` missing. (`join errors still use the danger error class` may already pass; that's fine — it's a pin.)

- [ ] **Step 3: Template changes** in `VoiceChannelView.svelte`:

Replace the channel-full/error block (`:130-136`):

```svelte
  {#if $voiceState.channelFull}
    <!-- Soft-cap bounce (ZEB-353): the join was reactively refused because the
         channel was full. Session is back at idle; this explains why. -->
    <div class="voice-full-note" role="alert">Voice channel full — try again later.</div>
  {:else if error}
    <div class="voice-error" role="alert">{error}</div>
  {/if}
```

Replace the self-mod-muted note (`:228-232`):

```svelte
    {#if $voiceState.selfModMuted}
      <div class="voice-mod-note" role="status" data-testid="self-mod-muted">
        🛡 You've been muted by a moderator. Your talk controls are disabled until they unmute you.
      </div>
    {/if}
```

Replace the join pane (`:148-154`):

```svelte
  {#if $voiceState.phase === 'idle'}
    <div class="voice-join-pane">
      <span class="join-glyph" data-testid="join-glyph" aria-hidden="true">🔊</span>
      <span class="join-name">{channelName}</span>
      <button class="btn-primary" onclick={onJoin} disabled={joining}>
        {joining ? 'Joining…' : 'Join Voice'}
      </button>
      <p class="hint">You'll join muted — unmute when you're ready.</p>
    </div>
  {:else}
```

- [ ] **Step 4: Style changes** in the same file's `<style>`:

Replace `.voice-error` and `.voice-mic-blocked` blocks and add `.voice-full-note`; replace `.voice-mod-note`; extend the join-pane styles; make the header count mono:

```css
  .voice-count {
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--text-secondary);
    white-space: nowrap;
  }

  .voice-error {
    background: var(--bg-tertiary);
    border: 1px solid var(--danger);
    color: var(--danger);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }

  /* Soft-cap bounce: a chosen-limit refusal, not a failure — gov-clay soft
     surface per TH frame C, distinct from the danger .voice-error. */
  .voice-full-note {
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 45%, transparent);
    color: var(--gov-clay-deep);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }

  .voice-mic-blocked {
    background: color-mix(in srgb, var(--warning) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--warning) 40%, transparent);
    color: var(--warning);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }
```

```css
  .voice-join-pane {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    color: var(--text-secondary);
  }
  .join-glyph {
    font-size: 2rem;
    line-height: 1;
  }
  .join-name {
    font-size: 1rem;
    font-weight: 600;
    color: var(--text-primary);
    margin-bottom: 4px;
  }
  .btn-primary {
    border: none;
    padding: 8px 22px;
    border-radius: 5px;
    background: var(--accent);
    color: var(--on-accent);
    font-size: 0.9rem;
    cursor: pointer;
  }
```

(`.btn-primary:hover`/`:disabled` and `.hint` stay as they are.)

Replace `.voice-mod-note` (bottom of the style block) — clay-danger recalled family per TH frame C, replacing the warning tint:

```css
  .voice-mod-note {
    background: var(--status-recalled-bg);
    border: 1px solid color-mix(in srgb, var(--danger) 35%, transparent);
    color: var(--danger);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 0 16px 8px;
    font-size: 0.85rem;
  }
```

- [ ] **Step 5: Run the component suite**

Run: `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts`
Expected: all pass (26 existing + 4 new). The existing channel-full test uses `getByRole('alert')` and copy only — still green.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/VoiceChannelView.svelte src/lib/components/__tests__/VoiceChannelView.test.ts
git commit -m "ZEB-612 S1: Commons banners + join pane for VoiceChannelView

Channel-full moves to a gov-clay note (chosen-limit, not danger); mod-silenced
note gets the full frame-C copy on the recalled surface; join pane gains the
room glyph/name; count goes mono. Zero behavior change.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

### Task 2: Roster tiles + compact list

**Files:**
- Modify: `src/lib/components/VoiceChannelView.svelte` (template ~:156-226; styles `.voice-tile`, `.mute-glyph`, `.mod-badge`, `.mod-controls`, list rows)
- Test: `src/lib/components/__tests__/VoiceChannelView.test.ts`

**Interfaces:**
- Consumes: `roster(n)` helper; existing testids (`voice-tile`, `mod-muted-badge`, `mod-mute`, `mod-remove`).
- Produces: `mod-sub` sub-label (`data-testid="mod-sub"`); double-ring `.voice-tile.speaking` box-shadow; hover/focus-reveal `.mod-controls`. Task 3 doesn't depend on these.

- [ ] **Step 1: Write the failing tests** — append inside a new describe block:

```typescript
describe('VoiceChannelView (ZEB-612 slice 1): Commons restyle — roster', () => {
  const modMutedRoster = [{
    ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64),
    muted: false, speaking: false, modMuted: true, power: 0,
  }];

  it('mod-muted tile shows the "mod-muted" sub-label', () => {
    const session = fakeSession({ phase: 'connected', roster: modMutedRoster });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('mod-sub')).toHaveTextContent('mod-muted');
  });

  it('mod-muted list row also shows the sub-label past the grid cap', () => {
    const big = [...roster(13)];
    big[3] = { ...big[3], modMuted: true };
    const session = fakeSession({ phase: 'connected', roster: big });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('voice-list')).toBeInTheDocument();
    expect(screen.getByTestId('mod-sub')).toHaveTextContent('mod-muted');
  });

  it('mod controls remain clickable under the hover-reveal treatment', async () => {
    // Reveal is CSS-only (opacity); handlers must be unaffected in jsdom.
    const session = fakeSession({ phase: 'connected', selfPower: 60,
      roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: false, power: 0 }] });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    await fireEvent.click(screen.getByTestId('mod-mute'));
    expect(session.moderate).toHaveBeenCalledWith('b'.repeat(32), 'mute');
  });
});
```

- [ ] **Step 2: Run, verify the two `mod-sub` tests fail**

Run: `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts -t 'restyle — roster'`
Expected: 2 fail (`mod-sub` testid missing); the clickability pin passes.

- [ ] **Step 3: Template changes.** In **both** the grid tile (`:159-190`) and the list row (`:195-222`), replace the two badge lines:

```svelte
              {#if m.muted && !m.modMuted}<span class="mute-glyph" aria-label="muted">🔇</span>{/if}
              {#if m.modMuted}
                <span class="mod-badge" data-testid="mod-muted-badge" title="Muted by a moderator" aria-label="muted by a moderator">🛡</span>
                <span class="mod-sub" data-testid="mod-sub">mod-muted</span>
              {/if}
```

(The only changes: badge glyph `🛡️🔇` → `🛡`, plus the new `mod-sub` span. The grid tile and the list row get identical markup — Svelte scopes each `#each` correctly; note the list row will render `mod-sub` after the name, which is the intended reading order.)

- [ ] **Step 4: Style changes.**

Replace `.voice-tile.speaking`, `.voice-tile .mute-glyph`, `.mod-badge`, `.mod-controls`, and add `.mod-sub` + name-mute treatment:

```css
  /* Speaking ring per TH frame A/B: double ring — paper gap then accent. */
  .voice-tile.speaking {
    box-shadow:
      0 0 0 2.5px var(--bg-primary),
      0 0 0 5px var(--accent);
  }
  /* Muted member: badge chip bottom-right of the avatar, name goes muted. */
  .voice-tile .mute-glyph {
    position: absolute;
    top: 6px;
    right: 6px;
    font-size: 0.7rem;
    line-height: 1;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 2px 4px;
  }
  .voice-tile:has(.mute-glyph) .name,
  .voice-list-row:has(.mute-glyph) .name {
    color: var(--text-muted);
  }
```

```css
  .mod-controls {
    display: flex;
    gap: 4px;
    margin-top: 4px;
    /* Frame B: "Hover a tile to Mute / Remove (mods)" — reveal on hover or
       keyboard focus. Opacity keeps the buttons clickable and focusable. */
    opacity: 0;
    transition: opacity 0.12s ease;
  }
  .voice-tile:hover .mod-controls,
  .voice-tile:focus-within .mod-controls,
  .voice-list-row:hover .mod-controls,
  .voice-list-row:focus-within .mod-controls {
    opacity: 1;
  }
  .mod-btn { border: 1px solid var(--border); background: var(--bg-tertiary); color: var(--text-secondary);
    font-size: 0.7rem; padding: 2px 6px; border-radius: 3px; cursor: pointer; }
  .mod-btn:hover { color: var(--text-primary); }
  .mod-btn.danger { color: var(--danger); border-color: var(--danger); }
  .mod-badge {
    position: absolute;
    top: 6px;
    left: 6px;
    font-size: 0.7rem;
    line-height: 1;
    background: var(--status-recalled-bg);
    border: 1px solid color-mix(in srgb, var(--danger) 30%, transparent);
    border-radius: 999px;
    padding: 2px 4px;
  }
  .mod-sub {
    font-size: 0.7rem;
    color: var(--danger);
    line-height: 1;
  }
```

- [ ] **Step 5: Run the component suite**

Run: `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts`
Expected: all pass (existing `mod-muted-badge` pins check testid/title/aria only, not the glyph text).

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/VoiceChannelView.svelte src/lib/components/__tests__/VoiceChannelView.test.ts
git commit -m "ZEB-612 S1: Commons roster tiles — double speaking ring, badge chips, hover-reveal mod controls

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

### Task 3: Control bar + PTT treatment, full gates

**Files:**
- Modify: `src/lib/components/VoiceChannelView.svelte` (template ~:239-298; styles `.ctrl`, `.ptt-hold`, `.btn-danger`)
- Test: `src/lib/components/__tests__/VoiceChannelView.test.ts`

**Interfaces:**
- Consumes: existing `ptt-hold` testid + `Hold to talk (or hold Space)` aria-label (both preserved).
- Produces: nothing downstream — final task.

- [ ] **Step 1: Write the failing tests** — append:

```typescript
describe('VoiceChannelView (ZEB-612 slice 1): Commons restyle — control bar', () => {
  it('held PTT shows the transmitting label with the Space hint', () => {
    const session = fakeSession({ phase: 'connected', pttMode: true, pttHeld: true, muted: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByTestId('ptt-hold')).toHaveTextContent('🎙 Transmitting… (hold Space)');
  });

  it('unheld PTT keeps the hold-to-talk label and explains release behavior via title', () => {
    const session = fakeSession({ phase: 'connected', pttMode: true, pttHeld: false, muted: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    const hold = screen.getByTestId('ptt-hold');
    expect(hold).toHaveTextContent('🎙 Hold to Talk');
    expect(hold).toHaveAttribute('title', 'Release to go quiet. Replaces the mute toggle while PTT mode is on.');
  });

  it('deafen control uses the headphones glyph when not deafened', () => {
    const session = fakeSession({ phase: 'connected', deafened: false });
    render(VoiceChannelView, { props: { session: session as never, ...base } });
    expect(screen.getByRole('button', { name: 'Deafen' })).toHaveTextContent('🎧 Deafen');
  });
});
```

- [ ] **Step 2: Run, verify they fail**

Run: `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts -t 'restyle — control bar'`
Expected: 3 fail (`Talking…` vs `Transmitting…`, missing title, `🔈` vs `🎧`).

- [ ] **Step 3: Template changes.**

PTT hold button (`:250-263`) — label + title only, handlers untouched:

```svelte
        <button
          class="ctrl ptt-hold"
          class:active={$voiceState.pttHeld}
          aria-pressed={$voiceState.pttHeld}
          data-testid="ptt-hold"
          onpointerdown={pttDown}
          onpointerup={pttUp}
          onpointerleave={pttUp}
          onpointercancel={pttUp}
          aria-label="Hold to talk (or hold Space)"
          title="Release to go quiet. Replaces the mute toggle while PTT mode is on."
          disabled={silenced}
        >
          {$voiceState.pttHeld ? '🎙 Transmitting… (hold Space)' : '🎙 Hold to Talk'}
        </button>
```

Deafen button (`:288-296`) — glyph only:

```svelte
      <button
        class="ctrl"
        class:restrictive={$voiceState.deafened}
        aria-pressed={$voiceState.deafened}
        onclick={toggleDeafen}
        aria-label="Deafen"
      >
        {$voiceState.deafened ? '🔕 Deafened' : '🎧 Deafen'}
      </button>
```

- [ ] **Step 4: Style changes.**

Replace `.ctrl`, `.ptt-hold`, `.btn-danger` radii/treatment:

```css
  .ctrl {
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-secondary);
    padding: 6px 14px;
    border-radius: 5px;
    font-size: 0.85rem;
    cursor: pointer;
  }
```

```css
  /* Hold-to-talk: suppress touch scroll/selection so a press-hold-release
     gesture stays a clean PTT hold on touch devices. Frame C: the held state
     is a full-width accent control with a soft glow ring. */
  .ptt-hold {
    touch-action: none;
    user-select: none;
    flex: 1;
  }
  .ptt-hold.active {
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 35%, transparent);
  }
  .btn-danger {
    margin-left: auto;
    border: none;
    background: var(--danger);
    color: var(--on-accent);
    padding: 6px 16px;
    border-radius: 5px;
    font-size: 0.85rem;
    cursor: pointer;
  }
```

- [ ] **Step 5: Run component suite + type check**

Run: `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts && npx tsc --noEmit`
Expected: all pass (existing PTT tests assert testid/aria/handler calls, never the label text), tsc clean.

- [ ] **Step 6: Full gates**

Run: `npx vitest run`
Expected: full frontend suite green (incl. `src/style-token-guard.test.ts` — no new literals were introduced).

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/VoiceChannelView.svelte src/lib/components/__tests__/VoiceChannelView.test.ts
git commit -m "ZEB-612 S1: Commons control bar — transmitting PTT treatment, headphones deafen glyph, Commons radii

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

---

## Post-plan (session workflow, not tasks)

PR to `zeblithic/harmony-client` titled `ZEB-612 slice 1: VoiceChannelView Commons restyle — frames B/C anatomy, zero behavior change`, body "Part of ZEB-612" + summary + honesty notes (none apply to this slice); fire `@coderabbitai review` once at open; attach PR to ZEB-612 in Linear; converge bots/CI per the standing loop.
