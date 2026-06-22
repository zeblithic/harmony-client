# ZEB-536 Spec 2: Message Reactions — Frontend UI — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface reactions in the Svelte/Tauri chat feed — reaction chips (emoji + count, mine-highlight, click-to-toggle, reactor tooltip), a hover quick-react toolbar (👍/👎 + picker button), and a fixed picker-grid popover — kept live via the `channel-reaction-received` event, on top of the complete Spec-1 backend.

**Architecture:** Three layers. (1) `ChannelMessageService` gains a `channel-reaction-received` listener that mutates the cached message's `reactions[]` in place and notifies the channel's subscribers (the same fan-out new messages use, so the feed re-renders), plus a `reactToMessage` IPC facade and a `selfOwnerId` field for computing `mine` on live events. (2) `ChannelMessageFeed.svelte` renders chips + a hover toolbar + a picker popover and calls `reactToMessage`. (3) Styling reuses the existing Discord-like CSS vars in component-scoped `<style>`. No backend changes — Spec 1 is the contract.

**Tech Stack:** Svelte 5 (runes: `$state`, `$derived`, `$effect`, `$props`), TypeScript, Tauri IPC (via the `TauriAdapter` seam), Vitest + jsdom + `@testing-library/svelte` v5.

## Global Constraints

- **No backend changes.** Spec-1 (PR #314) is the contract; this branch is frontend-only. Do not edit anything under `src-tauri/`.
- **IPC arg keys are camelCase.** `reactToMessage` invokes `set_message_reaction` with exactly `{ communityId, channelId, messageId, emoji, add }` (the Rust side declares snake_case and Tauri auto-converts; a wrong key silently arrives as `undefined`).
- **Event name + payload (verbatim from backend):** `channel-reaction-received` with payload `{ communityId, channelId, messageId, reactor, emoji, add, at }` where `at` is `{ wallMs, logical, deviceId }`. `reactor` and every entry of a reaction's `reactors[]` are **owner-id hex** (`hex::encode` of a 16-byte `OwnerAddr` → 32 hex chars), the same space as `msg.author` and the component's `ownAddress` prop.
- **DTO reaction shape (already on `ChannelMessageDto.reactions?`):** `{ emoji: string; count: number; mine: boolean; reactors: string[] }`.
- **`selfOwnerId` = the component's `ownAddress` prop.** Confirmed same hex space as `reactor`/`reactors[]` (the feed's shipped `isSelf(author) => author === ownAddress` already relies on this). Used only to compute `mine` for *live* events; `list_channel_messages` supplies authoritative `mine` on load.
- **Live apply must NOT fire `onMessage`.** `ChannelMessageService.onMessage` triggers a community-roster refetch for unknown authors (`App.svelte:1489`); a reaction is not a new message. `applyReaction` notifies only the per-channel subscriber set (which drives the feed re-render).
- **No strict frontend LWW-by-HLC in v1.** `applyReaction` is a plain set add/remove; it ignores `p.at`. `list_channel_messages.reactions` is authoritative and reseeds on channel open, so out-of-order drift self-heals (follow-up: per-(reactor,emoji) HLC tracking).
- **Palette (v1):** quick-react (inline) `👍 👎`; picker grid `👍 👎 ✅ ❌ 👀 🎉 🙏 🚀 ❤️ 😄` (10 emoji).
- **Reuse CSS vars** (`--bg-secondary #2b2d31`, `--bg-tertiary #313338`, `--text-primary #f2f3f5`, `--text-secondary #b5bac1`, `--accent #5865f2`, `--accent-hover #4752c4`, `--border #3f4147`) — no new color system. Styles live in the component's `<style>` block (matches the existing message styles).
- **Teardown safety:** component reaction handlers are fire-and-forget (`void promise.catch(log)`) with **no component-state writes after an `await`/resume point**, so no post-teardown guard is needed; any `window` event listener is added and removed inside a `$effect` cleanup. (Per the project's Svelte teardown rule.)
- **Gate (run from repo root, NOT `src-tauri/`):** `npx tsc --noEmit` clean **and** `npx vitest run` clean. Branch: `zeb-536-spec2-reactions-ui` (stacked on `zeb-536-message-reactions`); commit per task; never `git add -A`.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/lib/channel-message-service.ts` | Per-channel cache + IPC facade. Add reaction listener, in-place `applyReaction`, `reactToMessage` facade, `selfOwnerId` field, factor `notifyChannelSubscribers`. | Modify |
| `src/lib/__tests__/channel-message-service.test.ts` | Service unit tests (mock `TauriAdapter`). Add a reactions `describe` block. | Modify |
| `src/lib/components/ChannelMessageFeed.svelte` | Feed UI. Add chips, hover toolbar, picker popover, handlers, styles; set `selfOwnerId`. | Modify |
| `src/lib/components/__tests__/ChannelMessageFeed.test.ts` | Component interaction tests (`render` + mock adapter). Add chips + toolbar/picker `describe` blocks. | Modify |

No new files. No `app.css` change (all needed CSS vars already exist; reaction styles are component-scoped).

---

## Task 1: Service layer — live reaction apply + `reactToMessage` facade

**Files:**
- Modify: `src/lib/channel-message-service.ts`
- Test: `src/lib/__tests__/channel-message-service.test.ts`

**Interfaces:**
- Consumes (from Spec-1 backend, already shipped): the `channel-reaction-received` Tauri event `{ communityId, channelId, messageId, reactor, emoji, add, at }`; the `set_message_reaction` IPC verb with snake_case params; `ChannelMessageDto.reactions?: { emoji; count; mine; reactors }[]` (already declared on the type, `channel-message-service.ts:30-34`).
- Produces (later tasks rely on these):
  - `service.selfOwnerId: string | null` — public field; the feed sets it to `ownAddress`.
  - `service.reactToMessage(communityId: string, channelId: string, messageId: string, emoji: string, add: boolean): Promise<void>` — IPC facade.
  - A `channel-reaction-received` listener that mutates `byChannel` in place and notifies per-channel subscribers (so the feed's existing `subscribeToChannel` callback re-renders).

- [ ] **Step 1: Write the failing tests**

Append this block to `src/lib/__tests__/channel-message-service.test.ts` (after the existing top-level `describe('ChannelMessageService', …)` block, before EOF). It reuses the file's existing `makeAdapter()` helper:

```ts
describe('ChannelMessageService reactions (ZEB-536 Spec 2)', () => {
  let service: ChannelMessageService;
  let adapter: ReturnType<typeof makeAdapter>;
  const CID = 'aa'.repeat(16);
  const CHID = 'bb'.repeat(16);
  const OWN = 'cc'.repeat(16);
  const OTHER = 'dd'.repeat(16);

  beforeEach(() => {
    service = new ChannelMessageService();
    adapter = makeAdapter();
  });

  // Seed one message ('m1') into the per-channel cache via listMessages.
  async function seedMessage(): Promise<void> {
    (adapter.invoke as any).mockResolvedValue([
      {
        messageId: 'm1',
        communityId: CID,
        channelId: CHID,
        author: OTHER,
        at: { wallMs: 100, logical: 0, deviceId: 'd' },
        body: [],
      },
    ]);
    await service.listMessages(CID, CHID, undefined, 100);
  }

  function fireReaction(over: Record<string, unknown> = {}): void {
    const handler = adapter.listeners.get('channel-reaction-received')!;
    handler({
      payload: {
        communityId: CID,
        channelId: CHID,
        messageId: 'm1',
        reactor: OTHER,
        emoji: '👍',
        add: true,
        at: { wallMs: 200, logical: 0, deviceId: 'd' },
        ...over,
      },
    });
  }

  it('connectAdapter installs the channel-reaction-received listener', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('channel-reaction-received')).toBe(true);
  });

  it('applyReaction add creates a chip entry with count 1', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction();
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions).toEqual([
      { emoji: '👍', count: 1, mine: false, reactors: [OTHER] },
    ]);
  });

  it('a second distinct reactor increments count to 2', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction({ reactor: OTHER });
    fireReaction({ reactor: OWN });
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0].count).toBe(2);
    expect(msg.reactions?.[0].reactors).toEqual([OTHER, OWN]);
  });

  it('mine reflects selfOwnerId membership in reactors', async () => {
    service.selfOwnerId = OWN;
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction({ reactor: OWN });
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0].mine).toBe(true);
  });

  it('remove decrements and drops the entry at zero reactors', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction({ reactor: OTHER, add: true });
    fireReaction({ reactor: OTHER, add: false });
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions).toEqual([]);
  });

  it('duplicate add (idempotent redelivery) does not double-count', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    fireReaction({ reactor: OTHER, add: true });
    fireReaction({ reactor: OTHER, add: true });
    const msg = service.getMessages(CID, CHID)[0];
    expect(msg.reactions?.[0].count).toBe(1);
  });

  it('a reaction for an unloaded message is a no-op (no throw, no cache entry)', async () => {
    await service.connectAdapter(adapter);
    expect(() => fireReaction({ messageId: 'ghost' })).not.toThrow();
    expect(service.getMessages(CID, CHID)).toEqual([]);
  });

  it('applyReaction notifies channel subscribers (feed re-render hook)', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    const cb = vi.fn();
    service.subscribeToChannel(CID, CHID, cb);
    fireReaction();
    expect(cb).toHaveBeenCalledTimes(1);
  });

  it('applyReaction does NOT fire onMessage (no spurious roster refetch)', async () => {
    await service.connectAdapter(adapter);
    await seedMessage();
    const onMessage = vi.fn();
    service.onMessage = onMessage;
    fireReaction();
    expect(onMessage).not.toHaveBeenCalled();
  });

  it('reactToMessage invokes set_message_reaction with camelCase args', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.reactToMessage(CID, CHID, 'm1', '👍', true);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: CID,
      channelId: CHID,
      messageId: 'm1',
      emoji: '👍',
      add: true,
    });
  });

  it('reactToMessage throws when the adapter is not connected', async () => {
    await expect(
      service.reactToMessage(CID, CHID, 'm1', '👍', true),
    ).rejects.toThrow(/adapter not connected/);
  });

  it('destroy removes the reaction listener', async () => {
    await service.connectAdapter(adapter);
    service.destroy();
    expect(adapter.listeners.has('channel-reaction-received')).toBe(false);
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run (from repo root): `npx vitest run src/lib/__tests__/channel-message-service.test.ts`
Expected: FAIL — the new `describe` block fails (e.g. `applyReaction` entries never appear because no `channel-reaction-received` listener exists; `service.reactToMessage` is not a function → TypeScript/runtime error). The pre-existing tests in the file still pass.

- [ ] **Step 3: Add the payload interface + `selfOwnerId` field**

In `src/lib/channel-message-service.ts`, after the existing `ChannelBackfillProgressPayload` interface (around line 43-48), add:

```ts
/**
 * ZEB-536 Spec 2 — payload of the `channel-reaction-received` event.
 * `reactor` and the message's reaction `reactors[]` are owner-id hex
 * (same space as `ChannelMessageDto.author`). `at` is ignored in v1
 * (no frontend LWW-by-HLC; list reseeds on channel open).
 */
interface ChannelReactionReceivedPayload {
  communityId: string;
  channelId: string;
  messageId: string;
  reactor: string;
  emoji: string;
  add: boolean;
  at: HlcDto;
}
```

In the `ChannelMessageService` class, add a public field next to the existing `onMessage` / `onBackfillProgress` callbacks (after line 78, before `private adapter`):

```ts
  /**
   * Owner-id hex of the local member, used to compute `mine` for live
   * reaction events. Set by the feed from its `ownAddress` prop. `null`
   * until set — live `mine` is then false (list supplies authoritative
   * `mine` on load).
   */
  selfOwnerId: string | null = null;
```

- [ ] **Step 4: Install the listener + factor the subscriber fan-out**

In `connectAdapter`, after the `channel-backfill-progress` listener is pushed (after line 107), add:

```ts
    const unlistenReaction = await adapter.listen('channel-reaction-received', (event) => {
      const p = event.payload as ChannelReactionReceivedPayload;
      this.applyReaction(p);
    });
    this.unlisteners.push(unlistenReaction);
```

Factor the per-channel subscriber loop out of `ingest` so `applyReaction` can reuse it. Replace the tail of `ingest` (the `const subs = this.subscribers.get(key); …` block, lines ~257-266) with a single call:

```ts
    try {
      this.onMessage?.(communityId, channelId, message);
    } catch (e) {
      console.error(`ChannelMessageService onMessage failed for ${key}:`, e);
    }
    this.notifyChannelSubscribers(key, message);
  }

  /** Fan out to this channel's subscribers only (no onMessage — used by
   *  both ingest and applyReaction; the latter must not trigger the
   *  onMessage roster-refetch path). */
  private notifyChannelSubscribers(key: string, message: ChannelMessageDto): void {
    const subs = this.subscribers.get(key);
    if (subs) {
      for (const cb of subs) {
        try {
          cb(message);
        } catch (e) {
          console.error(`ChannelMessageService subscriber failed for ${key}:`, e);
        }
      }
    }
  }
```

(The `try { this.onMessage?.(…) } catch {…}` block already exists in `ingest`; keep it, and have it be followed by the `notifyChannelSubscribers(key, message)` call instead of the inline loop.)

- [ ] **Step 5: Add `applyReaction` + `reactToMessage`**

Add these two methods to the class (place `applyReaction` near `ingest`; place `reactToMessage` near the other IPC facades like `postMessage`):

```ts
  /**
   * ZEB-536 Spec 2 — apply a live reaction event in place. Finds the
   * cached message by id (drops if not loaded — list will carry the
   * materialized reactions when it loads), then add/removes the reactor
   * from the emoji's `reactors` set, recomputes `count`/`mine`, and
   * notifies the channel's subscribers so the feed re-renders. Plain set
   * semantics — `at` is ignored (no frontend LWW in v1).
   */
  private applyReaction(p: ChannelReactionReceivedPayload): void {
    const key = chKey(p.communityId, p.channelId);
    const arr = this.byChannel.get(key);
    if (!arr) return;
    const msg = arr.find((m) => m.messageId === p.messageId);
    if (!msg) return;

    const reactions = msg.reactions ?? (msg.reactions = []);
    const idx = reactions.findIndex((r) => r.emoji === p.emoji);

    if (p.add) {
      let entry = idx >= 0 ? reactions[idx] : undefined;
      if (!entry) {
        entry = { emoji: p.emoji, count: 0, mine: false, reactors: [] };
        reactions.push(entry);
      }
      if (!entry.reactors.includes(p.reactor)) {
        entry.reactors.push(p.reactor);
      }
      entry.count = entry.reactors.length;
      entry.mine = this.selfOwnerId !== null && entry.reactors.includes(this.selfOwnerId);
    } else {
      if (idx < 0) return; // unknown emoji — nothing to remove
      const entry = reactions[idx];
      entry.reactors = entry.reactors.filter((a) => a !== p.reactor);
      if (entry.reactors.length === 0) {
        reactions.splice(idx, 1);
      } else {
        entry.count = entry.reactors.length;
        entry.mine = this.selfOwnerId !== null && entry.reactors.includes(this.selfOwnerId);
      }
    }

    this.notifyChannelSubscribers(key, msg);
  }

  /** Set or clear the local member's reaction on a message. Fire-and-the
   *  result returns to the feed via the channel-reaction-received event
   *  (the backend echoes local React events back through the same path). */
  async reactToMessage(
    communityId: string,
    channelId: string,
    messageId: string,
    emoji: string,
    add: boolean,
  ): Promise<void> {
    if (!this.adapter) throw new Error('ChannelMessageService.reactToMessage: adapter not connected');
    await this.adapter.invoke('set_message_reaction', {
      communityId,
      channelId,
      messageId,
      emoji,
      add,
    });
  }
```

In `destroy()`, add `this.selfOwnerId = null;` alongside the other resets (the reaction unlistener is already cleared because it was pushed to `this.unlisteners`).

- [ ] **Step 6: Run the tests to verify they pass**

Run: `npx vitest run src/lib/__tests__/channel-message-service.test.ts`
Expected: PASS — all new reaction tests green, and the pre-existing `ChannelMessageService` tests still green (the `ingest` refactor preserves behavior).

- [ ] **Step 7: Type-check**

Run: `npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 8: Commit**

```bash
git add src/lib/channel-message-service.ts src/lib/__tests__/channel-message-service.test.ts
git commit -F - <<'EOF'
feat(zeb-536): live reaction apply + reactToMessage in ChannelMessageService

channel-reaction-received listener mutates the cached message's
reactions in place (plain set semantics, no frontend LWW) and notifies
per-channel subscribers only (not onMessage). Adds selfOwnerId for live
`mine`, and the reactToMessage IPC facade. Spec 2 of ZEB-536.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
```

---

## Task 2: Feed UI — reaction chips (render, click-toggle, mine-highlight, reactor tooltip)

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte`
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts`

**Interfaces:**
- Consumes: `channelMessageService.reactToMessage(...)`, `channelMessageService.selfOwnerId` (Task 1); the existing `ownAddress` prop; the existing `authorLabel(addr)` helper (nickname ► card name ► hex ladder, `ChannelMessageFeed.svelte:324`); `ChannelMessageDto.reactions?`.
- Produces (Task 3 reuses): `reactionMine(msg, emoji): boolean` and `toggleReaction(msg, emoji): void`.

- [ ] **Step 1: Write the failing tests**

Append this block to `src/lib/components/__tests__/ChannelMessageFeed.test.ts` (it reuses the file's existing `setup()` helper, which sets `ownAddress: 'cc'.repeat(20)` and connects a mock adapter):

```ts
describe('ChannelMessageFeed reactions — chips (ZEB-536)', () => {
  beforeEach(() => { vi.useFakeTimers({ shouldAdvanceTime: true }); });
  afterEach(() => { vi.useRealTimers(); });

  // Seed one message carrying `reactions` straight from the message event.
  async function seedMessageWithReactions(
    reactions: Array<{ emoji: string; count: number; mine: boolean; reactors: string[] }>,
    propOverrides: Record<string, unknown> = {},
  ) {
    const ctx = await setup(propOverrides);
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'ee'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
          reactions,
        },
      },
    });
    return ctx;
  }

  it('renders a chip per reaction with emoji + count', async () => {
    const { container } = await seedMessageWithReactions([
      { emoji: '👍', count: 2, mine: false, reactors: ['ee'.repeat(20), 'ff'.repeat(20)] },
    ]);
    await waitFor(() => {
      const chip = container.querySelector('.reaction-chip');
      expect(chip).toBeTruthy();
      expect(chip?.textContent).toContain('👍');
      expect(chip?.textContent).toContain('2');
    });
  });

  it('adds the .mine class when reaction.mine is true', async () => {
    const { container } = await seedMessageWithReactions([
      { emoji: '👍', count: 1, mine: true, reactors: ['cc'.repeat(20)] },
    ]);
    await waitFor(() => {
      expect(container.querySelector('.reaction-chip.mine')).toBeTruthy();
    });
  });

  it('clicking a mine chip toggles it off (add:false)', async () => {
    const { adapter, container } = await seedMessageWithReactions([
      { emoji: '👍', count: 1, mine: true, reactors: ['cc'.repeat(20)] },
    ]);
    let chip: Element | null = null;
    await waitFor(() => {
      chip = container.querySelector('.reaction-chip');
      expect(chip).toBeTruthy();
    });
    await fireEvent.click(chip!);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      messageId: 'm1',
      emoji: '👍',
      add: false,
    });
  });

  it('clicking a not-mine chip adds my reaction (add:true)', async () => {
    const { adapter, container } = await seedMessageWithReactions([
      { emoji: '👍', count: 1, mine: false, reactors: ['ee'.repeat(20)] },
    ]);
    let chip: Element | null = null;
    await waitFor(() => {
      chip = container.querySelector('.reaction-chip');
      expect(chip).toBeTruthy();
    });
    await fireEvent.click(chip!);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      messageId: 'm1',
      emoji: '👍',
      add: true,
    });
  });

  it('a live channel-reaction-received event updates the chip count', async () => {
    const { adapter, container } = await seedMessageWithReactions([
      { emoji: '👍', count: 1, mine: false, reactors: ['ee'.repeat(20)] },
    ]);
    await waitFor(() =>
      expect(container.querySelector('.reaction-chip')?.textContent).toContain('1'),
    );
    const rh = adapter.listeners.get('channel-reaction-received')!;
    rh({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        messageId: 'm1',
        reactor: 'ff'.repeat(20),
        emoji: '👍',
        add: true,
        at: { wallMs: 2000, logical: 0, deviceId: 'd' },
      },
    });
    await waitFor(() =>
      expect(container.querySelector('.reaction-chip')?.textContent).toContain('2'),
    );
  });

  it('chip title lists reactor display names via resolveCard', async () => {
    const resolveCard = (hex: string) =>
      hex === 'ee'.repeat(20) ? ({ displayName: 'Ildwyn' } as any) : undefined;
    const { container } = await seedMessageWithReactions(
      [{ emoji: '👍', count: 1, mine: false, reactors: ['ee'.repeat(20)] }],
      { resolveCard },
    );
    await waitFor(() => {
      const chip = container.querySelector('.reaction-chip');
      expect(chip?.getAttribute('title')).toContain('Ildwyn');
    });
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: FAIL — no `.reaction-chip` element exists yet; click tests find no chip.

- [ ] **Step 3: Add the chip handlers + set `selfOwnerId`**

In `ChannelMessageFeed.svelte`'s `<script>`, inside the existing `onMount(() => { … })` (starts line 203), add as the first statement:

```ts
    // ZEB-536: live `mine` on reaction events needs the local owner id.
    // ownAddress is the same owner-id hex as reaction reactors[].
    channelMessageService.selfOwnerId = ownAddress;
```

Add these helper functions to the `<script>` (place them near `authorLabel`, after line 330):

```ts
  // ZEB-536 — is the local member currently reacting with `emoji` on `msg`?
  function reactionMine(msg: ChannelMessageDto, emoji: string): boolean {
    return msg.reactions?.some((r) => r.emoji === emoji && r.mine) ?? false;
  }

  // ZEB-536 — toggle the local member's reaction (chips + quick-react share
  // this). Fire-and-forget: no component-state write after the await, so no
  // teardown guard is needed; failures are logged, not surfaced (the chip
  // self-heals from the authoritative event / next list).
  function toggleReaction(msg: ChannelMessageDto, emoji: string): void {
    const add = !reactionMine(msg, emoji);
    void channelMessageService
      .reactToMessage(communityId, channelId, msg.messageId, emoji, add)
      .catch((e) => console.warn('reaction toggle failed', e));
  }

  // ZEB-536 — comma-joined reactor labels for a chip tooltip, reusing the
  // ZEB-432 author label ladder (nickname ► profile-card name ► short hex).
  function reactorNames(reactors: string[]): string {
    return reactors.map((addr) => authorLabel(addr)).join(', ');
  }
```

- [ ] **Step 4: Render the chips**

In the template, inside `.content-col`, immediately after the `{#if msg.kind === 'poll' …}{:else}<p class="body">…</p>{/if}` block and before the closing `</div>` of `.content-col` (after line 429), add:

```svelte
            {#if msg.reactions && msg.reactions.length > 0}
              <div class="reactions">
                {#each msg.reactions as r (r.emoji)}
                  <button
                    type="button"
                    class="reaction-chip"
                    class:mine={r.mine}
                    title={reactorNames(r.reactors)}
                    onclick={() => toggleReaction(msg, r.emoji)}
                  >
                    <span class="reaction-emoji" aria-hidden="true">{r.emoji}</span>
                    <span class="reaction-count">{r.count}</span>
                  </button>
                {/each}
              </div>
            {/if}
```

- [ ] **Step 5: Add chip styles**

In the component `<style>` block, add (e.g. after the `.body { … }` rule, line 513):

```css
  .reactions {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-top: 4px;
  }
  .reaction-chip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 1px 8px;
    font-size: 0.8rem;
    line-height: 1.4;
    color: var(--text-primary);
    cursor: pointer;
  }
  .reaction-chip:hover { background: var(--bg-tertiary); }
  .reaction-chip.mine {
    border-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 18%, transparent);
  }
  .reaction-chip:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .reaction-count { color: var(--text-secondary); }
  .reaction-chip.mine .reaction-count { color: var(--text-primary); }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: PASS — the new chips block is green and all pre-existing `ChannelMessageFeed` tests still pass.

- [ ] **Step 7: Type-check**

Run: `npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 8: Commit**

```bash
git add src/lib/components/ChannelMessageFeed.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -F - <<'EOF'
feat(zeb-536): render reaction chips in the channel feed

Chips per reaction (emoji + count), mine-highlight, click-to-toggle via
reactToMessage, reactor tooltip via the ZEB-432 author label ladder.
Sets selfOwnerId from ownAddress so live events compute `mine`.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
```

---

## Task 3: Feed UI — hover quick-react toolbar + picker-grid popover

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte`
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts`

**Interfaces:**
- Consumes: `reactionMine(msg, emoji)` and `toggleReaction(msg, emoji)` (Task 2); `channelMessageService.reactToMessage(...)` (Task 1).
- Produces: nothing consumed by later tasks (terminal UI task).

- [ ] **Step 1: Write the failing tests**

Append this block to `src/lib/components/__tests__/ChannelMessageFeed.test.ts`:

```ts
describe('ChannelMessageFeed reactions — toolbar + picker (ZEB-536)', () => {
  beforeEach(() => { vi.useFakeTimers({ shouldAdvanceTime: true }); });
  afterEach(() => { vi.useRealTimers(); });

  async function seedPlainMessage() {
    const ctx = await setup();
    const handler = ctx.adapter.listeners.get('channel-message-received')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        message: {
          messageId: 'm1',
          communityId: 'aa'.repeat(16),
          channelId: 'bb'.repeat(16),
          author: 'ee'.repeat(20),
          at: { wallMs: 1000, logical: 0, deviceId: 'd' },
          body: Array.from(new TextEncoder().encode('hi')),
        },
      },
    });
    await waitFor(() => expect(ctx.container.querySelector('.channel-message')).toBeTruthy());
    return ctx;
  }

  it('renders the quick-react buttons 👍 👎 in the toolbar', async () => {
    const { container } = await seedPlainMessage();
    const quick = container.querySelectorAll('.reaction-toolbar .quick-react');
    expect(quick.length).toBe(2);
    expect(quick[0].textContent).toContain('👍');
    expect(quick[1].textContent).toContain('👎');
  });

  it('clicking the 👍 quick-react adds the reaction (add:true)', async () => {
    const { adapter, container } = await seedPlainMessage();
    const thumb = container.querySelector('.reaction-toolbar .quick-react') as HTMLButtonElement;
    await fireEvent.click(thumb);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      messageId: 'm1',
      emoji: '👍',
      add: true,
    });
  });

  it('the picker toggle opens a grid of 10 emoji', async () => {
    const { container } = await seedPlainMessage();
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    await waitFor(() => {
      const picker = container.querySelector('.reaction-picker');
      expect(picker).toBeTruthy();
      expect(picker!.querySelectorAll('.picker-emoji').length).toBe(10);
    });
  });

  it('selecting a picker emoji adds it (add:true) and closes the picker', async () => {
    const { adapter, container } = await seedPlainMessage();
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    let party: HTMLButtonElement | null = null;
    await waitFor(() => {
      const btns = Array.from(container.querySelectorAll('.picker-emoji')) as HTMLButtonElement[];
      party = btns.find((b) => b.textContent?.includes('🎉')) ?? null;
      expect(party).toBeTruthy();
    });
    await fireEvent.click(party!);
    expect(adapter.invoke).toHaveBeenCalledWith('set_message_reaction', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      messageId: 'm1',
      emoji: '🎉',
      add: true,
    });
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeNull());
  });

  it('Escape closes the picker', async () => {
    const { container } = await seedPlainMessage();
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeTruthy());
    await fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeNull());
  });

  it('clicking outside closes the picker', async () => {
    const { container } = await seedPlainMessage();
    await fireEvent.click(container.querySelector('.picker-toggle') as HTMLButtonElement);
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeTruthy());
    await fireEvent.click(document.body);
    await waitFor(() => expect(container.querySelector('.reaction-picker')).toBeNull());
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: FAIL — no `.reaction-toolbar` / `.picker-toggle` / `.reaction-picker` elements exist.

- [ ] **Step 3: Add the palette consts, picker state, and handlers**

In `ChannelMessageFeed.svelte`'s `<script>`, add the palette constants near the other module constants (after `SCROLL_TOP_THRESHOLD_PX`, line 95):

```ts
  // ZEB-536 reaction palette (v1). The grid is a const array — trim toward
  // quick-react-only later if it feels bloated (spec §Design).
  const QUICK_REACTIONS = ['👍', '👎'];
  const PICKER_EMOJI = ['👍', '👎', '✅', '❌', '👀', '🎉', '🙏', '🚀', '❤️', '😄'];
```

Add picker state near the other `$state` declarations (after `posting`, line 87):

```ts
  // messageId whose picker popover is open, or null. Only one at a time.
  let pickerOpenFor = $state<string | null>(null);
```

Add the picker handlers near `toggleReaction` (Task 2):

```ts
  function togglePicker(messageId: string): void {
    pickerOpenFor = pickerOpenFor === messageId ? null : messageId;
  }

  // ZEB-536 — picker selection is an explicit add (spec §Design), unlike the
  // toggle semantics of chips/quick-react. Closes the popover.
  function pickFromPicker(msg: ChannelMessageDto, emoji: string): void {
    pickerOpenFor = null;
    void channelMessageService
      .reactToMessage(communityId, channelId, msg.messageId, emoji, true)
      .catch((e) => console.warn('reaction pick failed', e));
  }
```

Add a `$effect` that closes the picker on Escape / outside-click while it is open (place it after the existing `$effect` blocks, before `onMount`, around line 201):

```ts
  // Close the open reaction picker on Escape or an outside click. Listeners
  // are scoped to "a picker is open" and cleaned up on close/teardown. The
  // click that opened the picker targets a node inside `.reaction-toolbar`,
  // so it does not self-close.
  $effect(() => {
    if (pickerOpenFor === null) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') pickerOpenFor = null;
    };
    const onDocClick = (e: MouseEvent) => {
      const t = e.target as HTMLElement | null;
      if (!t || !t.closest('.reaction-toolbar')) pickerOpenFor = null;
    };
    window.addEventListener('keydown', onKey);
    window.addEventListener('click', onDocClick);
    return () => {
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('click', onDocClick);
    };
  });
```

- [ ] **Step 4: Render the toolbar + picker**

In the template, inside the `<article class="channel-message" …>` element, after the closing `</div>` of `.content-col` and before the closing `</article>` (after line 430), add:

```svelte
          <div class="reaction-toolbar" role="group" aria-label="Add reaction">
            {#each QUICK_REACTIONS as emoji}
              <button
                type="button"
                class="quick-react"
                class:active={reactionMine(msg, emoji)}
                aria-label={`React ${emoji}`}
                aria-pressed={reactionMine(msg, emoji)}
                onclick={() => toggleReaction(msg, emoji)}
              >{emoji}</button>
            {/each}
            <button
              type="button"
              class="picker-toggle"
              aria-label="More reactions"
              aria-haspopup="true"
              aria-expanded={pickerOpenFor === msg.messageId}
              onclick={() => togglePicker(msg.messageId)}
            >😊</button>
            {#if pickerOpenFor === msg.messageId}
              <div class="reaction-picker" role="menu" aria-label="Pick a reaction">
                {#each PICKER_EMOJI as emoji}
                  <button
                    type="button"
                    class="picker-emoji"
                    role="menuitem"
                    aria-label={`React ${emoji}`}
                    onclick={() => pickFromPicker(msg, emoji)}
                  >{emoji}</button>
                {/each}
              </div>
            {/if}
          </div>
```

- [ ] **Step 5: Add toolbar + picker styles**

The `.channel-message` rule needs `position: relative` so the absolutely-positioned toolbar anchors to the message. In the `<style>` block, change the existing rule (line 486-490):

```css
  .channel-message {
    display: flex;
    gap: 10px;
    padding: 6px 16px;
    position: relative;
  }
```

Then add (after the `.reaction-chip` styles from Task 2):

```css
  .reaction-toolbar {
    position: absolute;
    top: -10px;
    right: 14px;
    display: flex;
    align-items: center;
    gap: 2px;
    padding: 2px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 6px;
    box-shadow: 0 1px 4px rgba(0, 0, 0, 0.3);
    opacity: 0;
    pointer-events: none;
    transition: opacity 0.08s ease;
  }
  .channel-message:hover .reaction-toolbar,
  .reaction-toolbar:focus-within {
    opacity: 1;
    pointer-events: auto;
  }
  .quick-react,
  .picker-toggle,
  .picker-emoji {
    background: transparent;
    border: none;
    cursor: pointer;
    font-size: 0.95rem;
    line-height: 1;
    padding: 3px 5px;
    border-radius: 4px;
  }
  .quick-react:hover,
  .picker-toggle:hover,
  .picker-emoji:hover { background: var(--bg-tertiary); }
  .quick-react:focus-visible,
  .picker-toggle:focus-visible,
  .picker-emoji:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .quick-react.active {
    background: color-mix(in srgb, var(--accent) 22%, transparent);
  }
  .reaction-picker {
    position: absolute;
    top: 100%;
    right: 0;
    margin-top: 4px;
    display: grid;
    grid-template-columns: repeat(5, 1fr);
    gap: 2px;
    padding: 4px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 6px;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.4);
    z-index: 10;
  }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: PASS — toolbar/picker block green; chips block (Task 2) and all pre-existing tests still green.

- [ ] **Step 7: Full gate**

Run (from repo root): `npx tsc --noEmit && npx vitest run`
Expected: type-check clean; entire vitest suite green.

- [ ] **Step 8: Commit**

```bash
git add src/lib/components/ChannelMessageFeed.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -F - <<'EOF'
feat(zeb-536): hover quick-react toolbar + picker-grid popover

Per-message hover toolbar with 👍/👎 quick-react (toggle) and a 😊 picker
button opening a fixed 10-emoji grid (add-on-select). Escape / outside
click close the popover via a scoped $effect with cleanup.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
```

---

## Manual / fleet validation (after Task 3, before PR)

- `npm run tauri dev` (or the fleet build) on AVALON: hover a message → toolbar appears; click 👍 → chip appears with count 1 and mine-highlight; click it again → chip disappears; open the 😊 picker → 10-emoji grid; pick 🎉 → chip appears; Escape and outside-click close the picker; hover a chip → reactor names tooltip.
- Two-party live test with **Ildwyn** once the stacked branch is up: AVALON reacts → Ildwyn's chip count updates live via `channel-reaction-received` (and vice-versa); toggle-off converges on both sides.

(Note: the fleet `serve` node can stay up during this frontend work — Vite/`tsc`/`vitest` don't relink `harmony-app.exe`. See the `windows-dev-gotchas` memory: only `cargo build --all-targets` is blocked by a running `serve`.)

---

## Self-Review (completed during planning)

**1. Spec coverage** — every spec section maps to a task:
- Component 1 (service live apply: `applyReaction` + listener + `selfOwnerId` + `reactToMessage` + convergence-by-reseed) → **Task 1**.
- Component 2 (chips: click-toggle, mine-highlight, reactor tooltip) → **Task 2**; (hover toolbar + picker popover, palette, Esc/outside-close) → **Task 3**.
- Component 3 (styling via CSS vars) → folded into **Task 2** (chip styles) and **Task 3** (toolbar/picker styles), co-located with the markup they style (no test-less styling task).
- Testing (service unit tests + component interaction tests) → Task 1 / Tasks 2-3.
- Open Q1 (selfOwnerId source) → **resolved**: backend `reactor`/`reactors[]` are owner-id hex = `ownAddress` space (the feed's `isSelf` already relies on it); the feed sets `selfOwnerId = ownAddress`, no `ZenohService` coupling.
- Open Q2 (component-test harness) → **resolved**: harness present (`vitest.config.ts` jsdom, `@testing-library/svelte` v5, existing `ChannelMessageFeed.test.ts`); component interaction tests included.

**2. Placeholder scan** — no TBD/TODO; every code step shows complete code; every command has an expected result.

**3. Type consistency** — `reactToMessage(communityId, channelId, messageId, emoji, add)`, `applyReaction(p: ChannelReactionReceivedPayload)`, `reactionMine(msg, emoji)`, `toggleReaction(msg, emoji)`, `togglePicker(messageId)`, `pickFromPicker(msg, emoji)`, `reactorNames(reactors)`, `pickerOpenFor: string | null`, `selfOwnerId: string | null` — names and signatures are used identically across Tasks 1-3. IPC arg keys and the event payload field names match the backend verbatim (`{ communityId, channelId, messageId, emoji, add }` and `{ communityId, channelId, messageId, reactor, emoji, add, at }`).
