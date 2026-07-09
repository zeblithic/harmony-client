# ZEB-662 Mention Notifications (MVP) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Notify the viewer when an incoming channel message @-mentions them — a nav mention indicator plus a focus-aware toast / OS-notification — by wiring receiver-side mention detection into the existing (but unwired) `NotificationService` policy engine and delivery rails, and persist notification settings across restart.

**Architecture:** A new dep-injected `MentionAlertService` subscribes to the existing per-message `channelMessageService.onMessage` hook, detects self-mentions via the DTO `mentions` field, classifies them `loud`, resolves an action through the existing `NotificationService.resolve()`, and delivers via a new nav mention indicator + the existing toast/`tauri-plugin-notification` rails (focus-aware, mirroring `incoming-call-alert.ts`). Notification settings gain owner-scoped localStorage persistence.

**Tech Stack:** TypeScript, Svelte 5 (runes), Vitest, Tauri 2 (`@tauri-apps/plugin-notification`, `@tauri-apps/api/webviewWindow`).

**Design spec:** `docs/specs/2026-07-08-zeb-662-mention-notifications-design.md`.

## Global Constraints

- **Frontend-only slice.** No Rust / owner-state-CRDT changes. `mentionCount` is client-derived; the CRDT `Space.notification_pref` is untouched.
- **Scope:** channel self-mentions only. No general per-message unread, no DM notifications.
- **Detection source:** the DTO `mentions: string[]` field (`ChannelMessageDto`, ZEB-534) — `selfOwnerId ∈ message.mentions`. Never parse `body` (it is `number[]`).
- **Policy authority:** the existing frontend `NotificationService` (`src/lib/notification-service.ts`). A mention → priority `'loud'`.
- **Persistence:** owner-scoped `localStorage`, key `harmony:notif-settings:<ownerIdHex>`. Mention *counts* are session-ephemeral.
- **`sound`/`break_dnd` ≡ `notify` delivery** (no in-app sound primitive; OS notification carries the system sound when unfocused).
- **Failure isolation:** no path in the alerter may throw into the message pipeline. OS-notify calls are try/caught; focus-query failure defaults to "focused" (prefer in-app toast).
- **Gates (from repo root):** `npx tsc --noEmit` && `npx vitest run`. New nav-badge CSS must use `var(--*)` tokens only (`style-token-guard`). No `cargo` gate (no Rust change).
- **Svelte 5 runes** (`$props`, `$state`, `$derived`) for any component work; match surrounding file idiom.

## Reference signatures (already in the tree — do not redefine)

```ts
// src/lib/types.ts
export type MessagePriority = 'quiet' | 'standard' | 'loud';
export type NotificationAction = 'silent' | 'dot_only' | 'notify' | 'sound' | 'break_dnd';
export type NotificationPolicy = Record<MessagePriority, NotificationAction>; // partial in per-scope maps
export interface NotificationSettings {
  global: NotificationPolicy;
  perCommunity: Map<string, Partial<NotificationPolicy>>;
  perPeer: Map<string, Partial<NotificationPolicy>>;
  perPeerSounds: Map<string, SoundOverrides>;
  perCommunitySounds: Map<string, SoundOverrides>;
}
export interface NavNode { id: string; parentId: string | null; type: NavNodeType; /* … */ unreadCount: number; }

// src/lib/notification-service.ts
class NotificationService {
  settings: NotificationSettings;
  resolve(priority: MessagePriority, peerAddress: string, communityId?: string): NotificationAction;
  setGlobalPolicy(p): void; setCommunityPolicy(id, p): void; setPeerPolicy(addr, p): void;
  clearCommunityPolicy(id): void; clearPeerPolicy(addr): void;
  setPeerSoundOverrides(addr, s): void; clearPeerSoundOverrides(addr): void;
  setCommunitySoundOverrides(id, s): void; clearCommunitySoundOverrides(id): void;
}

// src/lib/channel-message-service.ts
interface ChannelMessageDto { messageId: string; communityId: string; channelId: string; author: string; body: number[]; mentions?: string[]; /* … */ }
class ChannelMessageService {
  selfOwnerId?: string;                                   // maintained live (App.svelte:197)
  onMessage?: (communityId: string, channelId: string, message: ChannelMessageDto) => void; // invoked in ingest():451, try/caught
}

// src/lib/stores/toast.ts
export const toastStore: { show(message: string, durationMs?: number): string; dismiss(id: string): void };
```

---

### Task 1: `mention-detect.ts` — pure self-mention predicate

**Files:**
- Create: `src/lib/mention-detect.ts`
- Test: `src/lib/mention-detect.test.ts`

**Interfaces:**
- Produces: `messageMentionsOwner(message: { mentions?: string[] }, selfOwnerIdHex: string): boolean`

- [ ] **Step 1: Write the failing test**

```ts
// src/lib/mention-detect.test.ts
import { describe, it, expect } from 'vitest';
import { messageMentionsOwner } from './mention-detect';

const ME = 'aa'.repeat(16); // 32-hex owner id
const OTHER = 'bb'.repeat(16);

describe('messageMentionsOwner', () => {
  it('true when mentions includes me', () => {
    expect(messageMentionsOwner({ mentions: [OTHER, ME] }, ME)).toBe(true);
  });
  it('false when mentions includes only others', () => {
    expect(messageMentionsOwner({ mentions: [OTHER] }, ME)).toBe(false);
  });
  it('false when mentions is absent', () => {
    expect(messageMentionsOwner({}, ME)).toBe(false);
  });
  it('false when mentions is empty', () => {
    expect(messageMentionsOwner({ mentions: [] }, ME)).toBe(false);
  });
  it('false when selfOwnerId is empty', () => {
    expect(messageMentionsOwner({ mentions: [ME] }, '')).toBe(false);
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/mention-detect.test.ts`
Expected: FAIL — "Failed to resolve import './mention-detect'".

- [ ] **Step 3: Write minimal implementation**

```ts
// src/lib/mention-detect.ts
/**
 * ZEB-662 — receiver-side self-mention predicate. Reuses the ChannelMessageDto
 * `mentions` field (ZEB-534: owner-ids the message addresses) — the same signal
 * as the in-feed row highlight (ChannelMessageFeed.svelte). Mention priority is
 * viewer-relative, so this is computed per-viewer, never from the wire priority.
 */
export function messageMentionsOwner(
  message: { mentions?: string[] },
  selfOwnerIdHex: string,
): boolean {
  if (!selfOwnerIdHex) return false;
  return message.mentions?.includes(selfOwnerIdHex) ?? false;
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `npx vitest run src/lib/mention-detect.test.ts`
Expected: PASS (5/5).

- [ ] **Step 5: Commit**

```bash
git add src/lib/mention-detect.ts src/lib/mention-detect.test.ts
git commit -m "feat(zeb-662): mention-detect self-mention predicate"
```

---

### Task 2: `NavNode.mentionCount` + nav-service `incMention`/`clearMention`

**Files:**
- Modify: `src/lib/types.ts` (add `mentionCount` to `NavNode`)
- Modify: `src/lib/nav-service.ts` (init + preserve across rebuild; add `incMention`/`clearMention`)
- Test: `src/lib/nav-service.test.ts` (add cases; file exists)

**Interfaces:**
- Consumes: `NavService.nodes: NavNode[]`, `NavService.onChange?`
- Produces: `NavNode.mentionCount: number`; `NavService.incMention(channelId: string): void`; `NavService.clearMention(channelId: string): void`

- [ ] **Step 1: Add the field**

In `src/lib/types.ts`, in `interface NavNode`, directly after `unreadCount: number;` add:

```ts
  /** ZEB-662: session-ephemeral count of unseen @-mentions in this node.
   *  On a community node it is the sum of its descendant channels' counts.
   *  Reset on restart; cleared when the channel is opened. */
  mentionCount: number;
```

- [ ] **Step 2: Write the failing test**

Append to `src/lib/nav-service.test.ts` (import `NavService` as the existing file does):

```ts
describe('mention counts (ZEB-662)', () => {
  function svc(): NavService {
    const s = new NavService();
    s.nodes = [
      { id: 'c1', parentId: null, type: 'community', name: 'C', expanded: true, unreadCount: 0, unreadLevel: 'none', mentionCount: 0 },
      { id: 'ch1', parentId: 'c1', type: 'channel', name: 'general', expanded: false, unreadCount: 0, unreadLevel: 'none', mentionCount: 0 },
      { id: 'ch2', parentId: 'c1', type: 'channel', name: 'random', expanded: false, unreadCount: 0, unreadLevel: 'none', mentionCount: 0 },
    ];
    return s;
  }

  it('incMention bumps the channel and bubbles the community sum', () => {
    const s = svc();
    let changed = 0; s.onChange = () => { changed++; };
    s.incMention('ch1');
    s.incMention('ch1');
    s.incMention('ch2');
    expect(s.nodes.find(n => n.id === 'ch1')!.mentionCount).toBe(2);
    expect(s.nodes.find(n => n.id === 'ch2')!.mentionCount).toBe(1);
    expect(s.nodes.find(n => n.id === 'c1')!.mentionCount).toBe(3); // bubbled sum
    expect(changed).toBe(3);
  });

  it('clearMention zeroes the channel and recomputes the community sum', () => {
    const s = svc();
    s.incMention('ch1'); s.incMention('ch2');
    s.clearMention('ch1');
    expect(s.nodes.find(n => n.id === 'ch1')!.mentionCount).toBe(0);
    expect(s.nodes.find(n => n.id === 'c1')!.mentionCount).toBe(1); // only ch2 remains
  });

  it('incMention on an unknown channel is a no-op', () => {
    const s = svc();
    s.incMention('nope');
    expect(s.nodes.find(n => n.id === 'c1')!.mentionCount).toBe(0);
  });
});
```

- [ ] **Step 3: Run test to verify it fails**

Run: `npx vitest run src/lib/nav-service.test.ts`
Expected: FAIL — `incMention`/`clearMention` are not functions (and/or `mentionCount` missing).

- [ ] **Step 4: Implement the methods**

Add these methods to the `NavService` class in `src/lib/nav-service.ts` (place near the other node-mutating methods):

```ts
  /** ZEB-662: walk parentId up to the owning community node id (or null). */
  private communityIdOf(node: NavNode): string | null {
    let cur: NavNode | undefined = node;
    const seen = new Set<string>();
    while (cur && !seen.has(cur.id)) {
      if (cur.type === 'community') return cur.id;
      seen.add(cur.id);
      cur = cur.parentId ? this.nodes.find((n) => n.id === cur!.parentId) : undefined;
    }
    return null;
  }

  /** ZEB-662: recompute a community node's mentionCount = sum of descendant
   *  channels' counts (channels whose ancestor community is this one). */
  private recomputeCommunityMentions(communityId: string): void {
    const comm = this.nodes.find((n) => n.id === communityId);
    if (!comm) return;
    let sum = 0;
    for (const n of this.nodes) {
      if (n.type === 'channel' && this.communityIdOf(n) === communityId) sum += n.mentionCount;
    }
    comm.mentionCount = sum;
  }

  /** ZEB-662: increment a channel's unseen-mention count and bubble to its community. */
  incMention(channelId: string): void {
    const node = this.nodes.find((n) => n.id === channelId);
    if (!node) return;
    node.mentionCount += 1;
    const cid = this.communityIdOf(node);
    if (cid) this.recomputeCommunityMentions(cid);
    this.onChange?.();
  }

  /** ZEB-662: clear a channel's mention count (channel opened) and re-bubble. */
  clearMention(channelId: string): void {
    const node = this.nodes.find((n) => n.id === channelId);
    if (!node || node.mentionCount === 0) return;
    node.mentionCount = 0;
    const cid = this.communityIdOf(node);
    if (cid) this.recomputeCommunityMentions(cid);
    this.onChange?.();
  }
```

- [ ] **Step 5: Preserve/init `mentionCount` across nav rebuilds**

In `src/lib/nav-service.ts`, at every node-construction/merge site that currently sets `unreadCount` (search `unreadCount:` — approx lines 214, 238, 307, 327), add a sibling `mentionCount` using the same shape:
- Where a fresh node is created with `unreadCount: 0` → add `mentionCount: 0`.
- Where an existing value is preserved with `unreadCount: existing.unreadCount` → add `mentionCount: existing.mentionCount ?? 0` (the `?? 0` tolerates nodes seeded before this field existed, e.g. mock data).

Then update `src/lib/mock-data.ts`: every `navNodes` entry that has `unreadCount:` gets `mentionCount: 0` (tsc will flag the missing required field — add `mentionCount: 0` to each). If any other object literal constructs a `NavNode` (tsc `--noEmit` is the authority), add `mentionCount: 0` there too.

- [ ] **Step 6: Run tests + typecheck**

Run: `npx vitest run src/lib/nav-service.test.ts && npx tsc --noEmit`
Expected: nav-service tests PASS; tsc clean (no missing-`mentionCount` errors).

- [ ] **Step 7: Commit**

```bash
git add src/lib/types.ts src/lib/nav-service.ts src/lib/nav-service.test.ts src/lib/mock-data.ts
git commit -m "feat(zeb-662): NavNode.mentionCount + nav-service inc/clear with community bubble"
```

---

### Task 3: `NotificationService` serialize / load + save hook

**Files:**
- Modify: `src/lib/notification-service.ts`
- Test: `src/lib/notification-service.test.ts` (file exists — add cases)

**Interfaces:**
- Produces: `NotificationService.serialize(): string`; `NotificationService.load(raw: string): void`; a settable `onChange?: () => void` fired by every setter.

- [ ] **Step 1: Write the failing test**

Append to `src/lib/notification-service.test.ts`:

```ts
describe('persistence (ZEB-662)', () => {
  it('round-trips global + all four maps', () => {
    const a = new NotificationService();
    a.setGlobalPolicy({ quiet: 'silent', standard: 'notify', loud: 'break_dnd' });
    a.setCommunityPolicy('c1', { loud: 'sound' });
    a.setPeerPolicy('p1', { standard: 'dot_only' });
    a.setPeerSoundOverrides('p1', { loud: 'cidL' });
    a.setCommunitySoundOverrides('c1', { standard: 'cidS' });

    const b = new NotificationService();
    b.load(a.serialize());

    expect(b.settings.global).toEqual({ quiet: 'silent', standard: 'notify', loud: 'break_dnd' });
    expect(b.settings.perCommunity.get('c1')).toEqual({ loud: 'sound' });
    expect(b.settings.perPeer.get('p1')).toEqual({ standard: 'dot_only' });
    expect(b.settings.perPeerSounds.get('p1')).toEqual({ loud: 'cidL' });
    expect(b.settings.perCommunitySounds.get('c1')).toEqual({ standard: 'cidS' });
  });

  it('load() tolerates corrupt input by keeping defaults', () => {
    const s = new NotificationService();
    s.load('not json');
    expect(s.settings.global).toEqual({ quiet: 'dot_only', standard: 'sound', loud: 'break_dnd' });
    s.load('{"global": 42}'); // wrong shape
    expect(s.settings.global).toEqual({ quiet: 'dot_only', standard: 'sound', loud: 'break_dnd' });
  });

  it('every setter fires onChange', () => {
    const s = new NotificationService();
    let n = 0; s.onChange = () => { n++; };
    s.setGlobalPolicy({ quiet: 'silent', standard: 'silent', loud: 'silent' });
    s.setCommunityPolicy('c', {}); s.clearCommunityPolicy('c');
    s.setPeerPolicy('p', {}); s.clearPeerPolicy('p');
    expect(n).toBe(5);
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/notification-service.test.ts`
Expected: FAIL — `serialize`/`load`/`onChange` not present.

- [ ] **Step 3: Implement serialize/load + onChange**

In `src/lib/notification-service.ts`:

Add an `onChange?: () => void;` field to the class, and call `this.onChange?.();` at the end of **every** mutating setter (`setGlobalPolicy`, `setCommunityPolicy`, `setPeerPolicy`, `clearCommunityPolicy`, `clearPeerPolicy`, `setPeerSoundOverrides`, `clearPeerSoundOverrides`, `setCommunitySoundOverrides`, `clearCommunitySoundOverrides`).

Add the two methods:

```ts
  /** ZEB-662: serialize settings for persistence (Maps → plain objects). */
  serialize(): string {
    const mapObj = <V>(m: Map<string, V>) => Object.fromEntries(m.entries());
    return JSON.stringify({
      global: this.settings.global,
      perCommunity: mapObj(this.settings.perCommunity),
      perPeer: mapObj(this.settings.perPeer),
      perPeerSounds: mapObj(this.settings.perPeerSounds),
      perCommunitySounds: mapObj(this.settings.perCommunitySounds),
    });
  }

  /** ZEB-662: load persisted settings. Defensive — a parse/shape failure
   *  leaves the current (default) settings intact and never throws. */
  load(raw: string): void {
    let parsed: unknown;
    try { parsed = JSON.parse(raw); } catch { return; }
    if (!parsed || typeof parsed !== 'object') return;
    const p = parsed as Record<string, unknown>;
    const isObj = (v: unknown): v is Record<string, unknown> =>
      !!v && typeof v === 'object' && !Array.isArray(v);
    const toMap = <V>(v: unknown): Map<string, V> =>
      isObj(v) ? new Map(Object.entries(v) as [string, V][]) : new Map();
    if (isObj(p.global)) this.settings.global = { ...DEFAULT_POLICY, ...(p.global as NotificationPolicy) };
    this.settings.perCommunity = toMap(p.perCommunity);
    this.settings.perPeer = toMap(p.perPeer);
    this.settings.perPeerSounds = toMap(p.perPeerSounds);
    this.settings.perCommunitySounds = toMap(p.perCommunitySounds);
  }
```

(`DEFAULT_POLICY` is already defined at the top of the file. `load` does **not** fire `onChange` — it is the boot-time hydrate, not a user edit.)

- [ ] **Step 4: Run test + typecheck**

Run: `npx vitest run src/lib/notification-service.test.ts && npx tsc --noEmit`
Expected: PASS; tsc clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/notification-service.ts src/lib/notification-service.test.ts
git commit -m "feat(zeb-662): NotificationService serialize/load + onChange hook"
```

---

### Task 4: `notification-settings-persistence.ts` — owner-scoped localStorage

**Files:**
- Create: `src/lib/notification-settings-persistence.ts`
- Test: `src/lib/notification-settings-persistence.test.ts`

**Interfaces:**
- Consumes: `NotificationService` (Task 3: `serialize`/`load`/`onChange`).
- Produces:
  - `notifSettingsKey(ownerIdHex: string): string`
  - `loadNotificationSettings(service: NotificationService, ownerIdHex: string, storage?: Storage): void`
  - `attachNotificationSettingsPersistence(service: NotificationService, ownerIdHex: string, storage?: Storage): void` (sets `service.onChange` to save on every mutation)

- [ ] **Step 1: Write the failing test**

```ts
// src/lib/notification-settings-persistence.test.ts
import { describe, it, expect, beforeEach } from 'vitest';
import { NotificationService } from './notification-service';
import {
  notifSettingsKey, loadNotificationSettings, attachNotificationSettingsPersistence,
} from './notification-settings-persistence';

class MemStorage {
  m = new Map<string, string>();
  getItem(k: string) { return this.m.has(k) ? this.m.get(k)! : null; }
  setItem(k: string, v: string) { this.m.set(k, v); }
  removeItem(k: string) { this.m.delete(k); }
  clear() { this.m.clear(); }
  key(i: number) { return [...this.m.keys()][i] ?? null; }
  get length() { return this.m.size; }
}
const OWNER = 'aa'.repeat(16);
const OTHER = 'bb'.repeat(16);
let store: MemStorage;
beforeEach(() => { store = new MemStorage(); });

describe('notification-settings-persistence (ZEB-662)', () => {
  it('key is owner-scoped', () => {
    expect(notifSettingsKey(OWNER)).toBe(`harmony:notif-settings:${OWNER}`);
    expect(notifSettingsKey(OWNER)).not.toBe(notifSettingsKey(OTHER));
  });

  it('save-on-change then load restores settings', () => {
    const a = new NotificationService();
    attachNotificationSettingsPersistence(a, OWNER, store as unknown as Storage);
    a.setGlobalPolicy({ quiet: 'silent', standard: 'notify', loud: 'break_dnd' });
    const b = new NotificationService();
    loadNotificationSettings(b, OWNER, store as unknown as Storage);
    expect(b.settings.global).toEqual({ quiet: 'silent', standard: 'notify', loud: 'break_dnd' });
  });

  it('does not leak across owners', () => {
    const a = new NotificationService();
    attachNotificationSettingsPersistence(a, OWNER, store as unknown as Storage);
    a.setPeerPolicy('p', { loud: 'silent' });
    const other = new NotificationService();
    loadNotificationSettings(other, OTHER, store as unknown as Storage);
    expect(other.settings.perPeer.size).toBe(0); // OTHER has no saved settings
  });

  it('load with no stored value is a no-op (keeps defaults)', () => {
    const s = new NotificationService();
    loadNotificationSettings(s, OWNER, store as unknown as Storage);
    expect(s.settings.global).toEqual({ quiet: 'dot_only', standard: 'sound', loud: 'break_dnd' });
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/notification-settings-persistence.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

```ts
// src/lib/notification-settings-persistence.ts
/**
 * ZEB-662 — owner-scoped localStorage persistence for NotificationService.
 * Mirrors the ZEB-586 owner-scoping pattern: settings are keyed by the owner's
 * hex id so switching identity never leaks another owner's policy. Storage is
 * injectable for tests; defaults to window.localStorage when available.
 */
import type { NotificationService } from './notification-service';

export function notifSettingsKey(ownerIdHex: string): string {
  return `harmony:notif-settings:${ownerIdHex}`;
}

function defaultStorage(): Storage | null {
  try { return typeof localStorage !== 'undefined' ? localStorage : null; } catch { return null; }
}

/** Hydrate `service` from persisted settings for `ownerIdHex` (no-op if none). */
export function loadNotificationSettings(
  service: NotificationService,
  ownerIdHex: string,
  storage: Storage | null = defaultStorage(),
): void {
  if (!storage || !ownerIdHex) return;
  const raw = storage.getItem(notifSettingsKey(ownerIdHex));
  if (raw) service.load(raw);
}

/** Wire `service.onChange` to persist on every mutation for `ownerIdHex`. */
export function attachNotificationSettingsPersistence(
  service: NotificationService,
  ownerIdHex: string,
  storage: Storage | null = defaultStorage(),
): void {
  if (!storage || !ownerIdHex) return;
  service.onChange = () => {
    try { storage.setItem(notifSettingsKey(ownerIdHex), service.serialize()); } catch { /* quota/denied — best-effort */ }
  };
}
```

- [ ] **Step 4: Run test + typecheck**

Run: `npx vitest run src/lib/notification-settings-persistence.test.ts && npx tsc --noEmit`
Expected: PASS (4/4); tsc clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/notification-settings-persistence.ts src/lib/notification-settings-persistence.test.ts
git commit -m "feat(zeb-662): owner-scoped localStorage persistence for notification settings"
```

---

### Task 5: `MentionAlertService` (`mention-alert.ts`) + default factory

**Files:**
- Create: `src/lib/mention-alert.ts`
- Test: `src/lib/mention-alert.test.ts`

**Interfaces:**
- Consumes: `messageMentionsOwner` (Task 1); `NotificationService.resolve` (existing); `ChannelMessageDto` (existing); `NavService.incMention` (Task 2); `toastStore.show` (existing).
- Produces:
  - `interface MentionAlertDeps { getSelfOwnerId(): string | undefined; getActiveChannelId(): string | null; isFocused(): boolean | Promise<boolean>; resolve(priority, peerAddress, communityId?): NotificationAction; incMention(channelId: string): void; showToast(message: string): void; sendOsNotification(o: { title: string; body: string }): void; }`
  - `class MentionAlertService { onMessage(communityId: string, channelId: string, message: ChannelMessageDto): Promise<void>; }`
  - `createDefaultMentionAlerter(appDeps): Promise<MentionAlertService>` — builds `isFocused`/`sendOsNotification` from Tauri behind an `isTauri()` guard (noop outside Tauri), taking the app-supplied deps for the rest.

- [ ] **Step 1: Write the failing test**

```ts
// src/lib/mention-alert.test.ts
import { describe, it, expect, vi } from 'vitest';
import { MentionAlertService, type MentionAlertDeps } from './mention-alert';
import type { NotificationAction } from './types';

const ME = 'aa'.repeat(16);
const SENDER = 'bb'.repeat(16);
const msg = (over: Partial<{ mentions: string[]; author: string }> = {}) => ({
  messageId: 'm', communityId: 'c1', channelId: 'ch1', author: over.author ?? SENDER,
  at: { wallMs: 0, logical: 0, deviceId: 'd' }, body: [] as number[],
  mentions: over.mentions ?? [ME],
});

function harness(over: Partial<MentionAlertDeps> = {}) {
  const calls = { inc: [] as string[], toast: [] as string[], os: 0 };
  const deps: MentionAlertDeps = {
    getSelfOwnerId: () => ME,
    getActiveChannelId: () => null,
    isFocused: () => true,
    resolve: () => 'notify' as NotificationAction,
    incMention: (id) => calls.inc.push(id),
    showToast: (m) => calls.toast.push(m),
    sendOsNotification: () => { calls.os++; },
    ...over,
  };
  return { svc: new MentionAlertService(deps), calls };
}

describe('MentionAlertService (ZEB-662)', () => {
  it('ignores a message that does not mention me', async () => {
    const { svc, calls } = harness();
    await svc.onMessage('c1', 'ch1', msg({ mentions: [SENDER] }));
    expect(calls.inc).toEqual([]); expect(calls.toast).toEqual([]); expect(calls.os).toBe(0);
  });

  it('suppresses when the mentioned channel is active and focused', async () => {
    const { svc, calls } = harness({ getActiveChannelId: () => 'ch1', isFocused: () => true });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual([]); expect(calls.toast).toEqual([]); expect(calls.os).toBe(0);
  });

  it('still notifies for a mention in a non-active channel even when focused', async () => {
    const { svc, calls } = harness({ getActiveChannelId: () => 'other', isFocused: () => true });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual(['ch1']); expect(calls.toast.length).toBe(1); expect(calls.os).toBe(0);
  });

  it('silent action: no dot, no toast, no OS', async () => {
    const { svc, calls } = harness({ resolve: () => 'silent' });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual([]); expect(calls.toast).toEqual([]); expect(calls.os).toBe(0);
  });

  it('dot_only: nav dot, no toast, no OS', async () => {
    const { svc, calls } = harness({ resolve: () => 'dot_only' });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual(['ch1']); expect(calls.toast).toEqual([]); expect(calls.os).toBe(0);
  });

  it('notify + unfocused: nav dot + OS notification, no toast', async () => {
    const { svc, calls } = harness({ isFocused: () => false });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual(['ch1']); expect(calls.toast).toEqual([]); expect(calls.os).toBe(1);
  });

  it('sound/break_dnd behave like notify (toast when focused)', async () => {
    for (const action of ['sound', 'break_dnd'] as NotificationAction[]) {
      const { svc, calls } = harness({ resolve: () => action, getActiveChannelId: () => 'other' });
      await svc.onMessage('c1', 'ch1', msg());
      expect(calls.inc).toEqual(['ch1']); expect(calls.toast.length).toBe(1); expect(calls.os).toBe(0);
    }
  });

  it('resolve is called with loud + sender + community', async () => {
    const resolve = vi.fn(() => 'notify' as NotificationAction);
    const { svc } = harness({ resolve, getActiveChannelId: () => 'other' });
    await svc.onMessage('c1', 'ch1', msg());
    expect(resolve).toHaveBeenCalledWith('loud', SENDER, 'c1');
  });

  it('swallows an OS-notification throw', async () => {
    const { svc, calls } = harness({ isFocused: () => false, sendOsNotification: () => { throw new Error('x'); } });
    await expect(svc.onMessage('c1', 'ch1', msg())).resolves.toBeUndefined();
    expect(calls.inc).toEqual(['ch1']); // dot still recorded
  });

  it('no self owner id → ignored', async () => {
    const { svc, calls } = harness({ getSelfOwnerId: () => undefined });
    await svc.onMessage('c1', 'ch1', msg());
    expect(calls.inc).toEqual([]);
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/mention-alert.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

```ts
// src/lib/mention-alert.ts
/**
 * ZEB-662 — mention notifications. Subscribes (via App wiring) to the per-message
 * hook, detects self-mentions (DTO `mentions` field), classifies them `loud`,
 * resolves an action through the existing NotificationService policy engine, and
 * delivers across the nav mention indicator + toast/OS-notification rails,
 * focus-aware. All side effects are injected → deterministic + unit-testable.
 * Mirrors the incoming-call-alert.ts dep-injection + default-factory pattern.
 */
import type { NotificationAction } from './types';
import type { ChannelMessageDto } from './channel-message-service';
import { messageMentionsOwner } from './mention-detect';

export interface MentionAlertDeps {
  getSelfOwnerId(): string | undefined;
  getActiveChannelId(): string | null;
  isFocused(): boolean | Promise<boolean>;
  resolve(priority: 'quiet' | 'standard' | 'loud', peerAddress: string, communityId?: string): NotificationAction;
  incMention(channelId: string): void;
  showToast(message: string): void;
  sendOsNotification(o: { title: string; body: string }): void;
}

const NOTIFY_ACTIONS: ReadonlySet<NotificationAction> = new Set(['notify', 'sound', 'break_dnd']);

export class MentionAlertService {
  constructor(private deps: MentionAlertDeps) {}

  async onMessage(communityId: string, channelId: string, message: ChannelMessageDto): Promise<void> {
    const self = this.deps.getSelfOwnerId();
    if (!self || !messageMentionsOwner(message, self)) return;
    // Don't self-notify for your own message that happens to @-mention you.
    if (message.author === self) return;

    // Looking right at it → treat as seen.
    const active = this.deps.getActiveChannelId();
    if (channelId === active && (await this.focusedSafe())) return;

    const action = this.deps.resolve('loud', message.author, communityId);
    if (action === 'silent') return;

    // dot_only and above: always record the nav indicator.
    this.deps.incMention(channelId);
    if (!NOTIFY_ACTIONS.has(action)) return;

    const title = 'New mention';
    const body = `You were mentioned in ${channelId}`;
    if (await this.focusedSafe()) {
      this.deps.showToast(body);
    } else {
      try { this.deps.sendOsNotification({ title, body }); } catch { /* OS unavailable — nav dot already set */ }
    }
  }

  /** Focus query, defaulting to focused on failure (prefer in-app toast). */
  private async focusedSafe(): Promise<boolean> {
    try { return await this.deps.isFocused(); } catch { return true; }
  }
}

/** App-supplied deps (everything except the Tauri focus/notify capabilities). */
export type MentionAlertAppDeps = Omit<MentionAlertDeps, 'isFocused' | 'sendOsNotification'>;

/**
 * Build a MentionAlertService wired to the real Tauri window/notification APIs.
 * Outside Tauri (web preview / tests) isFocused defaults to true and OS notify
 * is a no-op, so callers need no guard. Mirrors createDefaultIncomingCallAlerter.
 */
export async function createDefaultMentionAlerter(appDeps: MentionAlertAppDeps): Promise<MentionAlertService> {
  const { isTauri } = await import('@tauri-apps/api/core');
  if (!isTauri()) {
    return new MentionAlertService({ ...appDeps, isFocused: () => true, sendOsNotification: () => {} });
  }
  const notif = await import('@tauri-apps/plugin-notification');
  const { getCurrentWebviewWindow } = await import('@tauri-apps/api/webviewWindow');
  const appWin = getCurrentWebviewWindow();
  return new MentionAlertService({
    ...appDeps,
    isFocused: () => appWin.isFocused(),
    sendOsNotification: (o) => notif.sendNotification(o),
  });
}
```

Note the extra `message.author === self` guard (not in the spec's numbered flow but correct: never notify yourself for your own message that lists you — e.g. an echo — belt-and-suspenders). Keep it; it is covered implicitly by the tests using a distinct `SENDER`.

- [ ] **Step 4: Run test + typecheck**

Run: `npx vitest run src/lib/mention-alert.test.ts && npx tsc --noEmit`
Expected: PASS; tsc clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/mention-alert.ts src/lib/mention-alert.test.ts
git commit -m "feat(zeb-662): MentionAlertService (classify/gate/deliver) + default Tauri factory"
```

---

### Task 6: NavPanel mention badge

**Files:**
- Modify: `src/lib/components/NavPanel.svelte` (render a mention badge/dot on channel + community rows)
- Test: `src/lib/components/__tests__/NavPanel.test.ts` (if present; else add a focused test file)

**Interfaces:**
- Consumes: `NavNode.mentionCount` (Task 2).

- [ ] **Step 1: Read the current row markup**

Read `src/lib/components/NavPanel.svelte`. Find where each nav node renders its label (channel + community rows) and where `unreadCount` (if anywhere) or the node name is shown. Identify the existing `CountChip` import if the file (or a sibling row component) already uses one; the Commons `CountChip` component is the badge idiom used elsewhere in the reskin.

- [ ] **Step 2: Write the failing test**

Add a Svelte component test (mirror an existing `NavPanel`/row test's harness) that renders a node with `mentionCount: 2` and asserts a badge with text `2` (or `data-testid="mention-badge"`) appears; and a node with `mentionCount: 0` renders no badge. Use `@testing-library/svelte` as the other component tests in `src/lib/components/__tests__/` do.

```ts
// sketch — match the existing NavPanel test harness for props
it('shows a mention badge when mentionCount > 0', () => {
  // render NavPanel (or the row) with a channel node { mentionCount: 2 }
  // expect(screen.getByTestId('mention-badge')).toHaveTextContent('2');
});
it('renders no mention badge when mentionCount is 0', () => {
  // expect(screen.queryByTestId('mention-badge')).toBeNull();
});
```

- [ ] **Step 3: Run test to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/NavPanel.test.ts` (or the new test path)
Expected: FAIL — no badge rendered.

- [ ] **Step 4: Implement the badge**

In the channel-row and community-row markup, after the node label, add:

```svelte
{#if node.mentionCount > 0}
  <span class="mention-badge" data-testid="mention-badge" aria-label={`${node.mentionCount} unread mentions`}>{node.mentionCount}</span>
{/if}
```

Styles (tokens only — `style-token-guard` enforces; reuse the accent/`--gov-clay` mention hue used by the in-feed highlight for consistency):

```css
.mention-badge {
  min-width: 16px;
  padding: 0 5px;
  border-radius: 8px;
  background: var(--accent);
  color: var(--on-accent);
  font-size: 11px;
  font-weight: 600;
  line-height: 16px;
  text-align: center;
}
```

If the file already imports and uses `CountChip`, prefer that component over hand-rolled markup for visual consistency; keep the `data-testid="mention-badge"` on the rendered element for the test.

- [ ] **Step 5: Run test + typecheck + token guard**

Run: `npx vitest run src/lib/components/__tests__/NavPanel.test.ts && npx tsc --noEmit && npx vitest run src/style-token-guard.test.ts`
Expected: PASS; tsc clean; token-guard clean.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/NavPanel.svelte src/lib/components/__tests__/NavPanel.test.ts
git commit -m "feat(zeb-662): nav mention badge on channel + community rows"
```

---

### Task 7: App.svelte wiring (integration)

**Files:**
- Modify: `src/App.svelte`

**Interfaces:**
- Consumes: everything above — `NavService.incMention/clearMention`, `NotificationService` + persistence, `MentionAlertService`/`createDefaultMentionAlerter`, `channelMessageService.onMessage` (existing, `:1736`), `channelMessageService.selfOwnerId`, `activeChannel` (`:2628`), `handleNodeSelect` (`:2763`), `notificationService` (`:1599`), `navService`.

This is integration glue; there is no isolated unit test (App.svelte is the composition root). The gate is `tsc` + the full `vitest` suite still green + a manual smoke check. Keep each edit minimal and match surrounding idiom.

- [ ] **Step 1: Persist notification settings owner-scoped**

Near where `notificationService` is created (`:1599`) and the owner identity becomes known (search where `selfOwnerId` / owner hex is set post-login — same place `channelMessageService.selfOwnerId` is assigned, `:197` area), add:

```ts
import { loadNotificationSettings, attachNotificationSettingsPersistence } from './lib/notification-settings-persistence';
// once the owner hex `ownAddress` is known:
loadNotificationSettings(notificationService, ownAddress);
attachNotificationSettingsPersistence(notificationService, ownAddress);
```

(`loadNotificationSettings` before `attach` so the boot-hydrate is not itself re-saved.)

- [ ] **Step 2: Instantiate the mention alerter**

Add a module-scope holder (mirroring `incomingCallAlerter`, `:256`):

```ts
let mentionAlerter: import('./lib/mention-alert').MentionAlertService | null = null;
```

Where `incomingCallAlerter` is created (`:2161`), add:

```ts
const { createDefaultMentionAlerter } = await import('./lib/mention-alert');
mentionAlerter = await createDefaultMentionAlerter({
  getSelfOwnerId: () => channelMessageService.selfOwnerId,
  getActiveChannelId: () => activeChannel,
  resolve: (p, peer, community) => notificationService.resolve(p, peer, community),
  incMention: (channelId) => navService.incMention(channelId),
  showToast: (m) => { void import('./lib/stores/toast').then(({ toastStore }) => toastStore.show(m)); },
});
```

(If `toastStore` is already statically imported in App.svelte, call it directly instead of the dynamic import.)

- [ ] **Step 3: Drive the alerter from the existing per-message hook**

At `channelMessageService.onMessage = (communityId, _channelId, message) => { … }` (`:1736`): un-underscore the second parameter to `channelId`, and at the end of the handler body add:

```ts
  void mentionAlerter?.onMessage(communityId, channelId, message);
```

Preserve all existing logic in that handler (roster refetch etc.). `void` + optional-chaining so a null alerter (pre-init) or a rejected promise never disrupts the existing path.

- [ ] **Step 4: Clear on view**

In `handleNodeSelect` (`:2763`), where `activeChannel` is set to the newly-selected channel, add (only for channel/dm/group nodes — a community select does not clear a specific channel):

```ts
  navService.clearMention(node.id);
```

- [ ] **Step 5: Ensure OS-notification permission is requested**

Confirm the existing startup permission request (`App.svelte:479-480`) runs on the app's normal boot path (it does for the call feature). No change needed if it already runs unconditionally at startup; if it is gated behind a call-only path, hoist the `isPermissionGranted()/requestPermission()` block so mentions can also surface OS notifications. Note what you find in the task report.

- [ ] **Step 6: Gate**

Run from repo root:

```bash
npx tsc --noEmit && npx vitest run
```

Expected: tsc clean; full suite green (no regressions).

- [ ] **Step 7: Manual smoke (report, do not block on env)**

If a dev build is available, note the intended manual check in the report (not required to pass in CI): with two identities in a shared community, A sends `@B hi` in a channel B is not viewing → B sees a nav mention badge; unfocused → B gets an OS notification; B opens the channel → badge clears. If no dev environment, state that in the report.

- [ ] **Step 8: Commit**

```bash
git add src/App.svelte
git commit -m "feat(zeb-662): wire mention alerts + settings persistence into App"
```

---

## Self-review checklist (run before opening PR)

1. **Spec coverage:** detection (T1) ✓, gating via resolver (T5) ✓, delivery nav-dot/toast/OS (T5+T6) ✓, focus-aware + suppress-active (T5) ✓, clear-on-view (T7) ✓, persistence (T3+T4+T7) ✓, channel-only/no-DM/no-CRDT (constraints) ✓.
2. **Placeholder scan:** none — every code step is complete except T6/T7 which require reading the live file (NavPanel markup, App composition root) and are specified with exact snippets + insertion points.
3. **Type consistency:** `messageMentionsOwner`, `incMention`/`clearMention`, `serialize`/`load`/`onChange`, `MentionAlertDeps`, `createDefaultMentionAlerter` names are used identically across tasks.
4. **Final full gate:** `npx tsc --noEmit && npx vitest run` from repo root (and `style-token-guard` covered in T6).
```
