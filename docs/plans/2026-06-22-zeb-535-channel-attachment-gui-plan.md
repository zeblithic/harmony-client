# Channel Artifact Attachment GUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render CAS channel attachments as download chips in the message feed and add a compose-time file-attach affordance — the deferred GUI half of in-channel artifact sharing.

**Architecture:** Frontend-only. A new self-contained `MessageAttachments.svelte` renders the per-message chips and drives per-attachment download via the existing `ChannelMessageService.downloadArtifact` facade + the `@tauri-apps/plugin-dialog` `save` dialog. `ChannelMessageFeed.svelte` gains a one-line render insertion plus a composer attach affordance backed by `ingestArtifact`. A pure `mimeCategoryIcon` helper maps mime → glyph. No Rust, no new IPC.

**Tech Stack:** Svelte 5 (runes: `$props`/`$state`), TypeScript, Vitest + `@testing-library/svelte`, `@tauri-apps/plugin-dialog`.

**Spec:** `docs/specs/2026-06-22-zeb-535-channel-attachment-gui-design.md`

**Branch:** `cas-channel-artifact-gui` (off main `13fe1007`).

**Test/gate commands (frontend-only — no Rust touched):**
- Single file: `npx vitest run src/lib/components/__tests__/MessageAttachments.test.ts`
- Full frontend gate (matches CI `frontend` job): `npx tsc --noEmit && npx vitest run` (from repo root)

**Reference facts (verified on main):**
- `ChannelAttachmentDto = { cid: string; mime: string; name: string; size: number; encrypted: boolean }` — `src/lib/channel-message-service.ts:40`.
- `ChannelMessageService.postMessage(communityId, channelId, body, replyTo?, mentions?, attachments?)` — signs `attachments` (`:122`).
- `ChannelMessageService.ingestArtifact(communityId, sourcePath, opts?) → ChannelAttachmentDto` (`:160`).
- `ChannelMessageService.downloadArtifact(communityId, channelId, attachment, destPath, maxBytes?) → number` (`:184`).
- `formatBytes(bytes)` + `const CATEGORY_ICONS` (image `🖼`, text `📄`, music `♪`, video `▶`) in `src/lib/file-utils.ts`.
- `ContentCategory = 'music' | 'video' | 'text' | 'image' | 'software' | 'dataset' | 'bundle'` — `src/lib/types.ts:212`.
- `ChannelMessageFeed.svelte` state vars at `:84-90` (`composeText`, `composeError`, `posting`, `composeEl`); `handleCompose` at `:274`; message-body markup at `:427-429`; composer markup at `:436-450`; styles from `:453`.
- Existing feed test harness `makeAdapter()` + `setup()` (with `vi.useFakeTimers`) — `src/lib/components/__tests__/ChannelMessageFeed.test.ts:1-43`.
- `formatBytes(2048)` ⇒ `'2.0 KB'`; `formatBytes(10)` ⇒ `'10 B'`.

---

### Task 1: `mimeCategoryIcon` helper

**Files:**
- Modify: `src/lib/file-utils.ts` (add export below `categoryIcon`, ~`:34`)
- Test: `src/lib/file-utils.test.ts` (existing)

- [ ] **Step 1: Write the failing test**

Append to `src/lib/file-utils.test.ts` (add `mimeCategoryIcon` to the existing import from `./file-utils`):

```typescript
describe('mimeCategoryIcon', () => {
  it('maps mime prefixes to category glyphs', () => {
    expect(mimeCategoryIcon('image/png')).toBe('🖼');      // 🖼
    expect(mimeCategoryIcon('IMAGE/JPEG')).toBe('🖼');     // case-insensitive
    expect(mimeCategoryIcon('text/plain')).toBe('📄');     // 📄
    expect(mimeCategoryIcon('audio/mpeg')).toBe('♪');           // ♪
    expect(mimeCategoryIcon('video/mp4')).toBe('▶');            // ▶
  });
  it('falls back to the document glyph for unknown mimes', () => {
    expect(mimeCategoryIcon('application/octet-stream')).toBe('📄');
    expect(mimeCategoryIcon('')).toBe('📄');
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/file-utils.test.ts`
Expected: FAIL — `mimeCategoryIcon is not a function` / not exported.

- [ ] **Step 3: Write minimal implementation**

In `src/lib/file-utils.ts`, immediately after the `categoryIcon` function (~`:34`):

```typescript
/** Map a MIME type to a category glyph. `categoryIcon` only accepts the
 *  ContentCategory enum, so this bridges a raw mime (as carried on a
 *  ChannelAttachmentDto) to the same icon set. Unknown mimes get the
 *  generic document glyph. */
export function mimeCategoryIcon(mime: string): string {
  const m = mime.toLowerCase();
  if (m.startsWith('image/')) return CATEGORY_ICONS.image;
  if (m.startsWith('audio/')) return CATEGORY_ICONS.music;
  if (m.startsWith('video/')) return CATEGORY_ICONS.video;
  if (m.startsWith('text/')) return CATEGORY_ICONS.text;
  return CATEGORY_ICONS.text;
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `npx vitest run src/lib/file-utils.test.ts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/file-utils.ts src/lib/file-utils.test.ts
git commit -m "feat(channel-attachments): mimeCategoryIcon helper

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: `MessageAttachments.svelte` render + download

**Files:**
- Create: `src/lib/components/MessageAttachments.svelte`
- Test: `src/lib/components/__tests__/MessageAttachments.test.ts` (new)

- [ ] **Step 1: Write the failing test**

Create `src/lib/components/__tests__/MessageAttachments.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import MessageAttachments from '../MessageAttachments.svelte';
import type { ChannelAttachmentDto } from '../../channel-message-service';

// vi.mock is hoisted to the top of the file; vi.hoisted makes the spy
// available at factory-call time (repo pattern — see WelcomeModal.test.ts).
const { saveMock } = vi.hoisted(() => ({ saveMock: vi.fn() }));
vi.mock('@tauri-apps/plugin-dialog', () => ({
  save: saveMock,
  open: vi.fn(),
}));

function att(over: Partial<ChannelAttachmentDto> = {}): ChannelAttachmentDto {
  return { cid: 'cid1', mime: 'text/plain', name: 'log.txt', size: 2048, encrypted: true, ...over };
}
function makeService(downloadArtifact = vi.fn().mockResolvedValue(2048)) {
  return { downloadArtifact } as any;
}
function props(over: Record<string, unknown> = {}) {
  return { communityId: 'c', channelId: 'ch', attachments: [att()], channelMessageService: makeService(), ...over };
}

describe('MessageAttachments', () => {
  beforeEach(() => { saveMock.mockReset(); });

  it('renders a chip per attachment with name, size, icon, lock', () => {
    const { container } = render(MessageAttachments, { props: props() });
    expect(container.textContent).toContain('log.txt');
    expect(container.textContent).toContain('2.0 KB');
    expect(container.querySelector('.att-lock')).not.toBeNull();
    expect(container.querySelectorAll('.attachment-chip').length).toBe(1);
  });

  it('omits the lock badge when not encrypted', () => {
    const { container } = render(MessageAttachments, { props: props({ attachments: [att({ encrypted: false })] }) });
    expect(container.querySelector('.att-lock')).toBeNull();
  });

  it('download: save → downloadArtifact called with chosen path', async () => {
    saveMock.mockResolvedValue('/tmp/out.txt');
    const service = makeService();
    const a = att();
    const { container } = render(MessageAttachments, { props: props({ attachments: [a], channelMessageService: service }) });
    await fireEvent.click(container.querySelector('.att-download')!);
    await waitFor(() => {
      expect(service.downloadArtifact).toHaveBeenCalledWith('c', 'ch', a, '/tmp/out.txt');
    });
  });

  it('cancel (save → null) does not call downloadArtifact', async () => {
    saveMock.mockResolvedValue(null);
    const service = makeService();
    const { container } = render(MessageAttachments, { props: props({ channelMessageService: service }) });
    await fireEvent.click(container.querySelector('.att-download')!);
    await Promise.resolve();
    expect(service.downloadArtifact).not.toHaveBeenCalled();
  });

  it('download error → error message + retry re-invokes', async () => {
    saveMock.mockResolvedValue('/tmp/out.txt');
    const downloadArtifact = vi.fn()
      .mockRejectedValueOnce(new Error('peer offline'))
      .mockResolvedValueOnce(2048);
    const { container } = render(MessageAttachments, { props: props({ channelMessageService: makeService(downloadArtifact) }) });
    await fireEvent.click(container.querySelector('.att-download')!);
    await waitFor(() => expect(container.querySelector('.att-error')?.textContent).toContain('peer offline'));
    await fireEvent.click(container.querySelector('.att-download')!);
    await waitFor(() => expect(downloadArtifact).toHaveBeenCalledTimes(2));
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/MessageAttachments.test.ts`
Expected: FAIL — cannot resolve `../MessageAttachments.svelte`.

- [ ] **Step 3: Write minimal implementation**

Create `src/lib/components/MessageAttachments.svelte`:

```svelte
<script lang="ts">
  import { save } from '@tauri-apps/plugin-dialog';
  import type { ChannelAttachmentDto, ChannelMessageService } from '../channel-message-service';
  import { formatBytes, mimeCategoryIcon } from '../file-utils';

  let { communityId, channelId, attachments, channelMessageService }: {
    communityId: string;
    channelId: string;
    attachments: ChannelAttachmentDto[];
    channelMessageService: ChannelMessageService;
  } = $props();

  type DownloadState = 'idle' | 'downloading' | 'saved' | 'error';
  // Per-cid state so each attachment downloads independently.
  let states = $state<Record<string, DownloadState>>({});
  let errors = $state<Record<string, string>>({});

  function stateOf(cid: string): DownloadState {
    return states[cid] ?? 'idle';
  }

  function filtersFor(name: string): { filters?: { name: string; extensions: string[] }[] } {
    const dot = name.lastIndexOf('.');
    if (dot <= 0 || dot === name.length - 1) return {};
    const ext = name.slice(dot + 1).toLowerCase();
    return { filters: [{ name: ext.toUpperCase(), extensions: [ext] }] };
  }

  async function download(att: ChannelAttachmentDto) {
    if (stateOf(att.cid) === 'downloading') return;
    let destPath: string | null;
    try {
      destPath = await save({ defaultPath: att.name, ...filtersFor(att.name) });
    } catch {
      // Treat a dialog backend error like a cancel — nothing downloaded.
      return;
    }
    if (!destPath) return; // user cancelled
    states = { ...states, [att.cid]: 'downloading' };
    errors = { ...errors, [att.cid]: '' };
    try {
      await channelMessageService.downloadArtifact(communityId, channelId, att, destPath);
      states = { ...states, [att.cid]: 'saved' };
    } catch (e) {
      // Tauri IPC rejections are raw strings in prod, Error in tests.
      states = { ...states, [att.cid]: 'error' };
      errors = { ...errors, [att.cid]: e instanceof Error ? e.message : String(e) };
    }
  }
</script>

<div class="attachments">
  {#each attachments as att (att.cid)}
    <div class="attachment-chip" class:error={stateOf(att.cid) === 'error'}>
      <span class="att-icon" aria-hidden="true">{mimeCategoryIcon(att.mime)}</span>
      <span class="att-name" title={att.name}>{att.name}</span>
      <span class="att-size">{formatBytes(att.size)}</span>
      {#if att.encrypted}
        <span class="att-lock" title="Encrypted" aria-label="Encrypted">&#x1F512;</span>
      {/if}
      <button
        type="button"
        class="att-download"
        onclick={() => download(att)}
        disabled={stateOf(att.cid) === 'downloading'}
        aria-label={stateOf(att.cid) === 'error' ? `Retry download ${att.name}` : `Download ${att.name}`}
      >
        {#if stateOf(att.cid) === 'downloading'}&#x2026;
        {:else if stateOf(att.cid) === 'saved'}&#x2713;
        {:else if stateOf(att.cid) === 'error'}&#x21BB;
        {:else}&#x2913;{/if}
      </button>
    </div>
    {#if stateOf(att.cid) === 'error'}
      <div class="att-error" role="alert">{errors[att.cid]}</div>
    {/if}
  {/each}
</div>

<style>
  .attachments { display: flex; flex-direction: column; gap: 4px; margin-top: 4px; }
  .attachment-chip {
    display: flex;
    align-items: center;
    gap: 8px;
    max-width: 420px;
    padding: 6px 8px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-size: 0.8rem;
  }
  .attachment-chip.error { border-color: #d83c3e; }
  .att-icon { flex: 0 0 auto; }
  .att-name {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-primary);
  }
  .att-size { flex: 0 0 auto; color: var(--text-secondary); }
  .att-lock { flex: 0 0 auto; }
  .att-download {
    flex: 0 0 auto;
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    cursor: pointer;
    padding: 2px 8px;
    font: inherit;
  }
  .att-download:hover:not(:disabled) { background: rgba(255, 255, 255, 0.06); }
  .att-download:disabled { opacity: 0.6; cursor: default; }
  .att-error { color: #d83c3e; font-size: 0.72rem; padding: 0 8px; max-width: 420px; }
</style>
```

- [ ] **Step 4: Run test to verify it passes**

Run: `npx vitest run src/lib/components/__tests__/MessageAttachments.test.ts`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/MessageAttachments.svelte src/lib/components/__tests__/MessageAttachments.test.ts
git commit -m "feat(channel-attachments): MessageAttachments download-chip component

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Render attachments in the message feed

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte` (import + one insertion at `:429`)
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts`

- [ ] **Step 1: Write the failing test**

First, add the dialog mock at the TOP of `ChannelMessageFeed.test.ts` (after the imports, before `makeAdapter`). The feed now transitively renders `MessageAttachments`, which imports `save`; the composer (Task 4) imports `open` — mock both now:

```typescript
// vi.mock is hoisted; vi.hoisted makes the spies available at factory-call
// time (repo pattern — see WelcomeModal.test.ts).
const { openMock, saveMock } = vi.hoisted(() => ({ openMock: vi.fn(), saveMock: vi.fn() }));
vi.mock('@tauri-apps/plugin-dialog', () => ({
  open: openMock,
  save: saveMock,
}));
```

Then add this test inside the `describe('ChannelMessageFeed', …)` block:

```typescript
it('renders MessageAttachments for a message carrying attachments', async () => {
  const { adapter, container } = await setup();
  const handler = adapter.listeners.get('channel-message-received')!;
  handler({
    payload: {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      message: {
        messageId: 'm1',
        communityId: 'aa'.repeat(16),
        channelId: 'bb'.repeat(16),
        author: 'cc'.repeat(20),
        at: { wallMs: 1000, logical: 0, deviceId: 'd' },
        body: Array.from(new TextEncoder().encode('see attached')),
        attachments: [{ cid: 'k1', mime: 'text/plain', name: 'ci.log', size: 1234, encrypted: true }],
      },
    },
  });
  await waitFor(() => {
    expect(container.querySelector('.attachment-chip')).not.toBeNull();
    expect(container.textContent).toContain('ci.log');
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts -t "renders MessageAttachments"`
Expected: FAIL — no `.attachment-chip` (feed doesn't render attachments yet).

- [ ] **Step 3: Write minimal implementation**

In `src/lib/components/ChannelMessageFeed.svelte`, add the import after line 8 (`import PollMessage …`):

```svelte
  import MessageAttachments from './MessageAttachments.svelte';
```

Then insert the render block immediately after the body `{/if}` at line 429 (still inside `.content-col`, before its closing `</div>` at line 430). The region becomes:

```svelte
            {:else}
              <p class="body">{bodyToText(msg.body)}</p>
            {/if}
            {#if msg.attachments && msg.attachments.length > 0}
              <MessageAttachments
                {communityId}
                {channelId}
                attachments={msg.attachments}
                {channelMessageService}
              />
            {/if}
          </div>
```

- [ ] **Step 4: Run test to verify it passes**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: PASS (all existing tests + the new one).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/ChannelMessageFeed.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -m "feat(channel-attachments): render attachments in the channel feed

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Compose-time attach affordance + send

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte` (imports, state, handlers, composer markup, styles)
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts`

- [ ] **Step 1: Write the failing tests**

Add these tests inside the `describe('ChannelMessageFeed', …)` block. (`openMock`/`saveMock` were added in Task 3.) Override `invoke` per test so `ingest_channel_artifact` returns descriptors with distinct cids:

```typescript
function withIngest(adapter: any, opts: { reject?: Error } = {}) {
  let n = 0;
  (adapter.invoke as any).mockImplementation((cmd: string) => {
    if (cmd === 'list_channel_messages') return Promise.resolve([]);
    if (cmd === 'request_channel_backfill') return Promise.resolve(undefined);
    if (cmd === 'ingest_channel_artifact') {
      if (opts.reject) return Promise.reject(opts.reject);
      return Promise.resolve({ cid: 'cid' + n++, mime: 'text/plain', name: 'f.txt', size: 5, encrypted: true });
    }
    if (cmd === 'post_channel_message') return Promise.resolve('mid' + 'a'.repeat(29));
    return Promise.resolve(undefined);
  });
}

it('attach button ingests picked files into pending chips', async () => {
  openMock.mockResolvedValue(['/tmp/a.txt', '/tmp/b.txt']);
  const { adapter, container } = await setup();
  withIngest(adapter);
  await fireEvent.click(container.querySelector('.attach-btn')!);
  await waitFor(() => {
    expect(adapter.invoke).toHaveBeenCalledWith('ingest_channel_artifact', expect.objectContaining({ sourcePath: '/tmp/a.txt' }));
    expect(container.querySelectorAll('.pending-chip').length).toBe(2);
  });
});

it('removing a pending attachment drops its chip', async () => {
  openMock.mockResolvedValue('/tmp/a.txt');
  const { adapter, container } = await setup();
  withIngest(adapter);
  await fireEvent.click(container.querySelector('.attach-btn')!);
  await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(1));
  await fireEvent.click(container.querySelector('.pending-remove')!);
  await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(0));
});

it('send includes pendingAttachments and clears them', async () => {
  openMock.mockResolvedValue('/tmp/a.txt');
  const { adapter, container } = await setup();
  withIngest(adapter);
  await fireEvent.click(container.querySelector('.attach-btn')!);
  await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(1));
  const textarea = container.querySelector('textarea.compose-input') as HTMLTextAreaElement;
  await fireEvent.input(textarea, { target: { value: 'here it is' } });
  await fireEvent.keyDown(textarea, { key: 'Enter' });
  await waitFor(() => {
    expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', expect.objectContaining({
      body: Array.from(new TextEncoder().encode('here it is')),
      attachments: [{ cid: 'cid0', mime: 'text/plain', name: 'f.txt', size: 5, encrypted: true }],
    }));
  });
  expect(container.querySelectorAll('.pending-chip').length).toBe(0);
});

it('allows sending with empty body when an attachment is pending', async () => {
  openMock.mockResolvedValue('/tmp/a.txt');
  const { adapter, container } = await setup();
  withIngest(adapter);
  await fireEvent.click(container.querySelector('.attach-btn')!);
  await waitFor(() => expect(container.querySelectorAll('.pending-chip').length).toBe(1));
  const textarea = container.querySelector('textarea.compose-input') as HTMLTextAreaElement;
  await fireEvent.keyDown(textarea, { key: 'Enter' });
  await waitFor(() => {
    expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', expect.objectContaining({
      body: [],
      attachments: [{ cid: 'cid0', mime: 'text/plain', name: 'f.txt', size: 5, encrypted: true }],
    }));
  });
});

it('surfaces an ingest error on the compose error line', async () => {
  openMock.mockResolvedValue('/tmp/a.txt');
  const { adapter, container } = await setup();
  withIngest(adapter, { reject: new Error('artifact too large') });
  await fireEvent.click(container.querySelector('.attach-btn')!);
  await waitFor(() => {
    expect(container.querySelector('.compose-error')?.textContent).toContain('artifact too large');
    expect(container.querySelectorAll('.pending-chip').length).toBe(0);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts -t "attach"`
Expected: FAIL — no `.attach-btn` element exists yet.

- [ ] **Step 3: Write the implementation**

In `src/lib/components/ChannelMessageFeed.svelte`:

(a) Add to the import on line 3 so `ChannelAttachmentDto` is in scope, and add `formatBytes`/`mimeCategoryIcon` + the dialog `open`:

```svelte
  import type { ChannelMessageDto, HlcDto, ChannelAttachmentDto } from '../channel-message-service';
```

and after the `MessageAttachments` import (added in Task 3):

```svelte
  import { open } from '@tauri-apps/plugin-dialog';
  import { formatBytes, mimeCategoryIcon } from '../file-utils';
```

(b) Add state after line 87 (`let posting = $state(false);`):

```svelte
  let pendingAttachments = $state<ChannelAttachmentDto[]>([]);
  let ingesting = $state(false);
```

(c) Add handlers immediately after `handleCompose` (after line 290). Also update `handleCompose`'s guard + send call. Replace the existing `handleCompose` body (lines 274-290) with:

```svelte
  async function handleCompose(e: KeyboardEvent) {
    if (e.key !== 'Enter') return;
    if (e.shiftKey) return; // newline; let browser handle
    e.preventDefault();
    const text = composeText.trim();
    if ((!text && pendingAttachments.length === 0) || posting || ingesting) return;
    posting = true;
    composeError = null;
    try {
      await channelMessageService.postMessage(
        communityId,
        channelId,
        text,
        undefined,
        undefined,
        pendingAttachments.length > 0 ? pendingAttachments : undefined,
      );
      composeText = '';
      pendingAttachments = [];
    } catch (e) {
      composeError = e instanceof Error ? e.message : String(e);
    } finally {
      posting = false;
    }
  }

  async function pickAttachments() {
    if (ingesting || posting) return;
    let selected: string | string[] | null;
    try {
      selected = await open({ multiple: true });
    } catch {
      return; // dialog backend error — treat like cancel
    }
    if (!selected) return;
    const paths = Array.isArray(selected) ? selected : [selected];
    ingesting = true;
    composeError = null;
    try {
      for (const path of paths) {
        const att = await channelMessageService.ingestArtifact(communityId, path);
        pendingAttachments = [...pendingAttachments, att];
      }
    } catch (e) {
      composeError = e instanceof Error ? e.message : String(e);
    } finally {
      ingesting = false;
    }
  }

  function removePending(cid: string) {
    pendingAttachments = pendingAttachments.filter((a) => a.cid !== cid);
  }
```

(d) Replace the composer markup (lines 436-450) with:

```svelte
  <div class="compose">
    {#if composeError}
      <div class="compose-error" role="alert">{composeError}</div>
    {/if}
    {#if pendingAttachments.length > 0}
      <div class="pending-attachments">
        {#each pendingAttachments as att (att.cid)}
          <div class="pending-chip">
            <span class="att-icon" aria-hidden="true">{mimeCategoryIcon(att.mime)}</span>
            <span class="att-name" title={att.name}>{att.name}</span>
            <span class="att-size">{formatBytes(att.size)}</span>
            <button
              type="button"
              class="pending-remove"
              onclick={() => removePending(att.cid)}
              aria-label={`Remove ${att.name}`}
            >&times;</button>
          </div>
        {/each}
      </div>
    {/if}
    <div class="compose-row">
      <button
        type="button"
        class="attach-btn"
        onclick={pickAttachments}
        disabled={posting || ingesting}
        aria-label="Attach file"
        title="Attach file"
      >{ingesting ? '…' : '📎'}</button>
      <textarea
        bind:this={composeEl}
        bind:value={composeText}
        onkeydown={handleCompose}
        class="compose-input"
        placeholder={`Message #${channelName}`}
        rows="2"
        aria-label="Channel message"
        disabled={posting}
      ></textarea>
    </div>
  </div>
```

(e) Add styles before the closing `</style>`:

```svelte
  .compose-row { display: flex; align-items: flex-end; gap: 8px; }
  .attach-btn {
    flex: 0 0 auto;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    cursor: pointer;
    padding: 8px 10px;
    font-size: 1rem;
    line-height: 1;
  }
  .attach-btn:hover:not(:disabled) { background: rgba(255, 255, 255, 0.06); }
  .attach-btn:disabled { opacity: 0.6; cursor: default; }
  .pending-attachments { display: flex; flex-wrap: wrap; gap: 6px; margin-bottom: 8px; }
  .pending-chip {
    display: flex;
    align-items: center;
    gap: 6px;
    max-width: 260px;
    padding: 4px 8px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 12px;
    font-size: 0.75rem;
  }
  .pending-chip .att-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-primary);
  }
  .pending-chip .att-size { color: var(--text-secondary); }
  .pending-remove {
    background: transparent;
    border: none;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 1rem;
    line-height: 1;
    padding: 0 2px;
  }
  .pending-remove:hover { color: var(--text-primary); }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: PASS — all existing tests (incl. the original "compose Enter posts…", "Shift+Enter", "does not post empty") plus the 5 new attach tests. The original empty-post test still passes because with no pending attachments the guard still rejects whitespace-only.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/ChannelMessageFeed.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -m "feat(channel-attachments): compose-time file attach affordance

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: Full frontend gate

**Files:** none (verification only).

- [ ] **Step 1: Type-check the whole frontend**

Run (repo root): `npx tsc --noEmit`
Expected: exit 0, no errors.

- [ ] **Step 2: Run the full vitest suite**

Run (repo root): `npx vitest run`
Expected: all test files pass (the existing ~231 files + the new `MessageAttachments.test.ts`).

- [ ] **Step 3: Confirm no Rust was touched (gate sanity)**

Run: `git diff --name-only origin/main | grep -E '^src-tauri/' || echo "no rust changes — rust CI jobs trivially green"`
Expected: prints the "no rust changes" line. (If any `src-tauri/` file appears, run the Rust gate from `src-tauri/`: `cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures`.)

- [ ] **Step 4: Commit (only if Step 1/2 surfaced fixes)**

If tsc/vitest required fixes, commit them:

```bash
git add -A
git commit -m "fix(channel-attachments): gate fixes

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- Download-chip render (icon/name/size/lock/download) → Task 2 ✅
- mime→icon → Task 1 ✅
- Render insertion in feed (single line, above reactions row) → Task 3 ✅
- Compose attach (picker → ingest → pending chips → remove) → Task 4 ✅
- Send with attachments + empty-body-with-attachment allowed + ingest-in-flight gating → Task 4 ✅
- Per-chip download state + retry + cancel no-op → Task 2 ✅
- Error handling (ingest error on compose line; download error per-chip) → Task 2 + Task 4 ✅
- No Rust / no new IPC → confirmed (Task 5 Step 3) ✅
- Tests vitest-only → all tasks ✅

**Placeholder scan:** none — every code step shows complete code.

**Type consistency:** `ChannelAttachmentDto` shape, `postMessage(...,attachments?)` arity, `downloadArtifact(communityId, channelId, att, destPath)` arity, `ingestArtifact(communityId, sourcePath)` arity, `mimeCategoryIcon`/`formatBytes` names — all match the verified facts and are used identically across Tasks 1-4. Class hooks (`.attachment-chip`, `.att-download`, `.att-error`, `.att-lock`, `.attach-btn`, `.pending-chip`, `.pending-remove`) match between component markup and tests.
