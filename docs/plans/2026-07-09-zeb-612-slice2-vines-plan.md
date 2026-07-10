# ZEB-612 Slice 2 — Vines: interactive full-bleed feed + honest publish dialog + hydration gap-fix

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rework the Vines surface per spec §3 (`docs/specs/2026-07-09-zeb-612-commons-i-town-hall-vines-files-design.md`): VineFeed becomes a vertical scroll-snap full-bleed feed with center-detection autoplay (single `playingId`, in-feed muted-loop video via `resolveVideo(cid)` blobs), VinePlayer is retired, VinePublishDialog gets the Commons anatomy + an honest client-side ≤6s publish gate, and VineService hydrates the Rust-persisted feed cache at startup.

**Architecture:** All state additions are frontend-only. The feed owns `playingId` (rAF-throttled center detection over `data-vine-id` rows), a cid→blob-URL map bounded to the playing card ± 1 neighbor (URLs revoked outside the window), and a sticky cid→duration map fed by `loadedmetadata`. VineCard becomes a presentational full-bleed card. `VineService.hydrate()` pulls `list_vine_videos` once after `loadFollowed()` and classifies rows by the live `followedAddresses` set (the DTO carries no `source` field; current-follow-set classification matches the service's existing follow/unfollow move semantics).

**Tech Stack:** Svelte 5 runes, vitest + @testing-library/svelte (jsdom), Tauri IPC via `TauriAdapter`.

## Plan-time verification results (spec §3 open items)

1. **`VinePlayer.svelte` consumers:** only `VineFeed.svelte` (plus its own test). → Delete component + `VinePlayer.test.ts`; move the ReshareConfirmDialog flow to feed level.
2. **Hydration ground truth:** `vine_feed_cache.rs` persists descriptors + reactions + viewed to `vine_feed.json` (ZEB-147); `vine-received` is emitted **only** on `DescriptorOutcome::Inserted` (event_loop.rs:7432), so after a restart the cache reloads but the frontend feed renders empty (re-arrivals are absorbed). `hydrate()` fixes descriptors AND viewed. Reactions have no list IPC — out of S2 scope (spin-off ticket, filed during this slice).
3. **Wire shapes (camelCase, verified):** `list_vine_videos` → `Vec<VineVideoDto>` = `{ id, creatorAddress, creatorName, createdAt (unix s), videoCid, title?, reshareOf?, viewed, originalCreatorAddress?, originalCreatorName? }` — **no `source`**. Live `vine-received` payload additionally carries `source: 'followed'|'discover'` and `viewed` (the frontend type currently omits `viewed` — that's half the gap). `mark_vine_viewed` takes `{ vineId }`.
4. **Style-token guard:** per-file ratchet against `src/style-token-allowlist.json`. Current entries: `VinePlayer.svelte: 3`, `VinePublishDialog.svelte: 1`. Deleting VinePlayer + tokenizing the dialog overlay requires the allowlist regen step (Task 6). New dark-stage colors enter as semantic tokens in `src/app.css` (guard doesn't scan it — that's where colors belong).

## Global Constraints

- **Copy, verbatim (spec §3):**
  - Publish header: `Share a vine`; subtitle: `≤ 6 seconds · loops forever`.
  - Over-limit block: `This clip is {X.X}s — vines are 6 seconds or less. Trim it and re-ingest.` (X.X = `toFixed(1)`).
  - Picked line (duration known, ≤6s): `{fileName} · {X.X}s ✓ · ingested to content store`.
  - Sovereign note: `Publishes to your sovereign identity and replicates peer-to-peer. There's no central server to take it down.` (🔑 glyph; **no** "only you can delete it" — returns with ZEB-670).
  - Reshare attribution: `↻ {resharer} reshared · view original by {orig}`.
  - Duration badge: `↻ {m:ss}` in `--font-mono`.
- **Honesty ledger (spec §8):** no loop/play counts, no trim UI, no fake ✓ when duration unmeasured. The ≤6s gate is honest-client and **fail-open** on measurement failure (documented in code).
- **Radius idiom:** pills fully rounded (999px), buttons/inputs 5px, cards/banners 8px.
- **style-token-guard budget-0** for VineFeed/VineCard (no raw color literals; use `var(--*)` + `color-mix(... var(--*) ..., transparent)`); new tokens only in `src/app.css` (both theme blocks).
- **Zero contract change** to: `onPublish(videoCid, title?)`, reshare attribution propagation (`resolveOriginalCreator`), follow/unfollow semantics, self-reshare guard location (App.svelte), empty-state/CTA logic (ZEB-554/CodeAnt #333 pins).
- **IPC keys are camelCase** (`vineId`, `videoCid`, `createdAt` unix **seconds**).
- **jsdom caveats:** `HTMLMediaElement.play/pause/load` unimplemented (guard with try/catch + `?.`), `URL.createObjectURL/revokeObjectURL` absent (tests stub; prod code uses `URL.revokeObjectURL?.(url)` never bare), `scrollIntoView` absent (`el.scrollIntoView?.()`), rects all-zero (center detection resolves to index 0 → first card plays on mount — tests rely on this).
- **Gates per task:** `npx tsc --noEmit` + targeted `npx vitest run <files>`; Task 6 runs the full `npx vitest run`. No Rust changes in this slice → no cargo gates locally (CI still runs them).
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

## File Structure

- Modify: `src/app.css` (two semantic tokens, both theme blocks)
- Modify: `src/lib/vine-utils.ts` + `src/lib/vine-utils.test.ts` (3 pure helpers)
- Create: `src/lib/video-metadata.ts` + `src/lib/video-metadata.test.ts` (duration probe)
- Modify: `src/lib/vine-service.ts` + `src/lib/vine-service.test.ts` (wire `viewed`, `hydrate()`)
- Rewrite: `src/lib/components/VineCard.svelte` + `__tests__/VineCard.test.ts` (full-bleed)
- Rewrite: `src/lib/components/VineFeed.svelte` + `__tests__/VineFeed.test.ts` (scroll-snap + autoplay)
- Rewrite: `__tests__/VineFeed.integration.test.ts` (feed-as-player flows)
- Delete: `src/lib/components/VinePlayer.svelte`, `__tests__/VinePlayer.test.ts`
- Modify: `src/lib/components/VinePublishDialog.svelte` + `__tests__/VinePublishDialog.test.ts`
- Modify: `src/App.svelte` (hydrate call + `resolveVideo` prop to dialog)
- Regenerate: `src/style-token-allowlist.json`

---

### Task 1: Media-stage tokens + pure vine-utils helpers

**Files:**
- Modify: `src/app.css` (`:root` Commons-core section ~line 40; dark block ~line 205)
- Modify: `src/lib/vine-utils.ts`
- Test: `src/lib/vine-utils.test.ts`

**Interfaces:**
- Produces: `pickCenterIndex(centers: number[], viewportCenter: number): number` (nearest center, first wins ties, `-1` for empty), `formatVineDuration(seconds: number): string` (`m:ss`, rounds to nearest whole second, `0:00` floor for non-finite/negative), `isOwnOriginalVine(vine: VineVideo, ownAddress?: string): boolean` (extracted verbatim from VinePlayer's `isOwnOriginal`), CSS tokens `--media-stage` / `--on-media-stage`.

- [ ] **Step 1: Write the failing tests** — append to `src/lib/vine-utils.test.ts`:

```typescript
import { pickCenterIndex, formatVineDuration, isOwnOriginalVine } from './vine-utils';

describe('pickCenterIndex (ZEB-612 S2)', () => {
  it('returns -1 for an empty list', () => {
    expect(pickCenterIndex([], 300)).toBe(-1);
  });

  it('returns 0 for a single card', () => {
    expect(pickCenterIndex([120], 300)).toBe(0);
  });

  it('picks the center nearest the viewport center', () => {
    expect(pickCenterIndex([100, 290, 500], 300)).toBe(1);
  });

  it('breaks ties toward the earlier card (stable under all-zero jsdom rects)', () => {
    expect(pickCenterIndex([250, 350], 300)).toBe(0);
    expect(pickCenterIndex([0, 0, 0], 0)).toBe(0);
  });
});

describe('formatVineDuration (ZEB-612 S2)', () => {
  it('formats sub-minute durations as m:ss', () => {
    expect(formatVineDuration(6)).toBe('0:06');
    expect(formatVineDuration(5.96)).toBe('0:06');
    expect(formatVineDuration(5.4)).toBe('0:05');
  });

  it('formats minute-plus durations', () => {
    expect(formatVineDuration(65)).toBe('1:05');
  });

  it('floors non-finite and negative inputs to 0:00', () => {
    expect(formatVineDuration(Number.NaN)).toBe('0:00');
    expect(formatVineDuration(Number.POSITIVE_INFINITY)).toBe('0:00');
    expect(formatVineDuration(-3)).toBe('0:00');
  });
});

describe('isOwnOriginalVine (ZEB-612 S2 — extracted from VinePlayer)', () => {
  const base = {
    id: 'v1', creatorName: 'Me', createdAt: 1, videoCid: 'cid', viewed: false,
  };
  it('true for a self-magic original', () => {
    expect(isOwnOriginalVine({ ...base, creatorAddress: 'self' })).toBe(true);
  });
  it('true for a hex-keyed own original when ownAddress matches (FIX 2, PR #120)', () => {
    expect(isOwnOriginalVine({ ...base, creatorAddress: 'aabb' }, 'aabb')).toBe(true);
  });
  it('false for someone else\'s original', () => {
    expect(isOwnOriginalVine({ ...base, creatorAddress: 'ccdd' }, 'aabb')).toBe(false);
  });
  it('false for own RESHARE (reshares of own content are re-resharable)', () => {
    expect(isOwnOriginalVine({ ...base, creatorAddress: 'self', reshareOf: 'orig' })).toBe(false);
  });
});
```

- [ ] **Step 2: Run to verify failure** — `npx vitest run src/lib/vine-utils.test.ts` → FAIL (exports missing).

- [ ] **Step 3: Implement** — append to `src/lib/vine-utils.ts`:

```typescript
/**
 * Index of the card center nearest the viewport center (ZEB-612 S2 feed
 * autoplay). Ties break toward the earlier card, which also makes jsdom's
 * all-zero rects deterministically pick index 0 (first card plays on mount).
 * Returns -1 for an empty list.
 */
export function pickCenterIndex(centers: number[], viewportCenter: number): number {
  let best = -1;
  let bestDist = Number.POSITIVE_INFINITY;
  for (let i = 0; i < centers.length; i++) {
    const d = Math.abs(centers[i] - viewportCenter);
    if (d < bestDist) {
      bestDist = d;
      best = i;
    }
  }
  return best;
}

/** "m:ss" badge text for the honest duration pill ("↻ 0:06"). */
export function formatVineDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '0:00';
  const total = Math.round(seconds);
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}:${String(s).padStart(2, '0')}`;
}

/**
 * Whether `vine` is the local user's own ORIGINAL (not a reshare) — the
 * only case where the Reshare verb is suppressed. Extracted verbatim from
 * VinePlayer's `isOwnOriginal` (FIX 2, PR #120 round 1): hex-keyed
 * self-authored vines that arrived before `ownAddress` was set weren't
 * remapped to the magic 'self' value, so both signals are checked.
 */
export function isOwnOriginalVine(vine: VineVideo, ownAddress?: string): boolean {
  return (
    !vine.reshareOf
    && (vine.creatorAddress === 'self'
      || (ownAddress != null && vine.creatorAddress === ownAddress))
  );
}
```

- [ ] **Step 4: Add the tokens** — in `src/app.css`, at the end of the "Commons core" group in `:root` (after `--tally-track`), add:

```css
  /* ZEB-612 S2: Vines full-bleed media stage. The video letterbox is
     near-black in BOTH themes (per the drawn VFI reference — video sits on
     a dark stage regardless of app theme); values are warm-tinted to match
     the palette. --on-media-stage is the text/glyph color on that stage. */
  --media-stage: #14120d;
  --on-media-stage: #f4f1ea;
```

and the same two declarations (identical values, same comment reduced to `/* ZEB-612 S2: theme-invariant media stage — see :root. */`) in the `:root[data-theme="dark"]` block after its `--tally-track`.

- [ ] **Step 5: Run tests + typecheck** — `npx vitest run src/lib/vine-utils.test.ts && npx tsc --noEmit` → PASS.

- [ ] **Step 6: Commit** — `git add -A && git commit -m "ZEB-612 S2: media-stage tokens + pure feed helpers (pickCenterIndex, formatVineDuration, isOwnOriginalVine)"` (with trailers).

---

### Task 2: `video-metadata.ts` duration probe

**Files:**
- Create: `src/lib/video-metadata.ts`
- Test: `src/lib/video-metadata.test.ts`

**Interfaces:**
- Produces: `probeVideoDuration(url: string): Promise<number>` — resolves the metadata duration of a blob URL via a detached `<video preload="metadata">`; rejects on decode error. Injected into VinePublishDialog as the default `probeDuration` prop (tests stub the prop; this module's own test stubs `document.createElement`).

- [ ] **Step 1: Write the failing test** — `src/lib/video-metadata.test.ts`:

```typescript
import { describe, it, expect, vi, afterEach } from 'vitest';
import { probeVideoDuration } from './video-metadata';

/** Minimal fake <video> — jsdom never fires media events, so drive them by hand. */
function fakeVideoElement() {
  const el = {
    preload: '',
    src: '',
    onloadedmetadata: null as (() => void) | null,
    onerror: null as (() => void) | null,
    duration: Number.NaN,
    removeAttribute: vi.fn(),
    load: vi.fn(),
  };
  return el;
}

afterEach(() => vi.restoreAllMocks());

describe('probeVideoDuration (ZEB-612 S2)', () => {
  it('resolves the duration once metadata loads', async () => {
    const el = fakeVideoElement();
    vi.spyOn(document, 'createElement').mockReturnValue(el as unknown as HTMLElement);
    const p = probeVideoDuration('blob:fake');
    expect(el.src).toBe('blob:fake');
    expect(el.preload).toBe('metadata');
    el.duration = 5.8;
    el.onloadedmetadata!();
    await expect(p).resolves.toBe(5.8);
    expect(el.removeAttribute).toHaveBeenCalledWith('src');
  });

  it('rejects when the element errors (undecodable container)', async () => {
    const el = fakeVideoElement();
    vi.spyOn(document, 'createElement').mockReturnValue(el as unknown as HTMLElement);
    const p = probeVideoDuration('blob:bad');
    el.onerror!();
    await expect(p).rejects.toThrow('could not read video metadata');
  });
});
```

- [ ] **Step 2: Run to verify failure** — `npx vitest run src/lib/video-metadata.test.ts` → FAIL (module missing).

- [ ] **Step 3: Implement** — `src/lib/video-metadata.ts`:

```typescript
/**
 * Read a video blob URL's duration from container metadata (ZEB-612 S2).
 *
 * Used by the VinePublishDialog's honest ≤6s gate and injectable there as
 * the `probeDuration` prop — jsdom implements no media pipeline, so tests
 * stub the prop rather than this DOM glue.
 */
export function probeVideoDuration(url: string): Promise<number> {
  return new Promise((resolve, reject) => {
    const video = document.createElement('video');
    video.preload = 'metadata';
    const cleanup = () => {
      video.onloadedmetadata = null;
      video.onerror = null;
      video.removeAttribute('src');
      try {
        video.load();
      } catch {
        // jsdom: HTMLMediaElement.load is not implemented.
      }
    };
    video.onloadedmetadata = () => {
      const d = video.duration;
      cleanup();
      resolve(d);
    };
    video.onerror = () => {
      cleanup();
      reject(new Error('could not read video metadata'));
    };
    video.src = url;
  });
}
```

- [ ] **Step 4: Run tests + typecheck** — `npx vitest run src/lib/video-metadata.test.ts && npx tsc --noEmit` → PASS.

- [ ] **Step 5: Commit** — `git add -A && git commit -m "ZEB-612 S2: probeVideoDuration metadata probe"` (with trailers).

---

### Task 3: VineService — honor wire `viewed` + `hydrate()` from the persisted cache

**Files:**
- Modify: `src/lib/vine-service.ts`
- Test: `src/lib/vine-service.test.ts`

**Interfaces:**
- Consumes: `list_vine_videos` IPC (camelCase DTO, no `source`, guaranteed `viewed`).
- Produces: `VineDescriptorEvent.viewed?: boolean`; `VineService.hydrate(): Promise<void>` (call AFTER `loadFollowed()` — classification reads `followedAddresses`). App wiring lands in Task 7.

- [ ] **Step 1: Write the failing tests** — append a describe to `src/lib/vine-service.test.ts` (uses the existing `createMockAdapter` from `./test-utils`; check its `invoke` mock shape at the top of the file and match the existing stubbing idiom):

```typescript
describe('hydrate (ZEB-612 S2 — restart survival for the Rust-persisted cache)', () => {
  const dto = (over: Partial<VineDescriptorEvent & { viewed: boolean }> = {}) => ({
    id: 'h1', creatorAddress: 'peer-a', creatorName: 'A', createdAt: 100,
    videoCid: 'cid-h1', viewed: false, ...over,
  });

  it('is a no-op without an adapter', async () => {
    const fresh = new VineService({ seedMockData: false });
    await expect(fresh.hydrate()).resolves.toBeUndefined();
  });

  it('populates feeds from list_vine_videos, classified by the follow set', async () => {
    const svc2 = new VineService({ seedMockData: false });
    const { adapter } = createMockAdapter();
    await svc2.connectAdapter(adapter);
    svc2.followedAddresses.add('peer-followed');
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) =>
      cmd === 'list_vine_videos'
        ? Promise.resolve([
            dto({ id: 'h1', creatorAddress: 'peer-followed' }),
            dto({ id: 'h2', creatorAddress: 'peer-stranger', videoCid: 'cid-h2' }),
          ])
        : Promise.resolve(null));
    await svc2.hydrate();
    expect(svc2.followedVines.map(v => v.id)).toEqual(['h1']);
    expect(svc2.discoverVines.map(v => v.id)).toEqual(['h2']);
  });

  it('restores the persisted viewed-set (the ZEB-612 gap-fix)', async () => {
    const svc2 = new VineService({ seedMockData: false });
    const { adapter } = createMockAdapter();
    await svc2.connectAdapter(adapter);
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) =>
      cmd === 'list_vine_videos'
        ? Promise.resolve([dto({ id: 'h1', viewed: true }), dto({ id: 'h2', videoCid: 'c2' })])
        : Promise.resolve(null));
    await svc2.hydrate();
    expect(svc2.viewedIds.has('h1')).toBe(true);
    expect(svc2.viewedIds.has('h2')).toBe(false);
  });

  it('merges viewed-state for ids already received live, without duplicating them', async () => {
    const svc2 = new VineService({ seedMockData: false });
    const { adapter, emit } = createMockAdapter();
    await svc2.connectAdapter(adapter);
    emit('vine-received', {
      id: 'live-1', creatorAddress: 'p', creatorName: 'P',
      createdAt: 5, videoCid: 'c', source: 'discover',
    });
    expect(svc2.viewedIds.has('live-1')).toBe(false);
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) =>
      cmd === 'list_vine_videos'
        ? Promise.resolve([dto({ id: 'live-1', creatorAddress: 'p', viewed: true })])
        : Promise.resolve(null));
    await svc2.hydrate();
    expect(svc2.vines.filter(v => v.id === 'live-1').length).toBe(1);
    expect(svc2.viewedIds.has('live-1')).toBe(true);
  });

  it('remaps own-address rows to self (discover, viewed), matching the live path', async () => {
    const svc2 = new VineService({ seedMockData: false });
    const { adapter } = createMockAdapter();
    await svc2.connectAdapter(adapter);
    svc2.ownAddress = 'me-hex';
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) =>
      cmd === 'list_vine_videos'
        ? Promise.resolve([dto({ id: 'mine', creatorAddress: 'me-hex' })])
        : Promise.resolve(null));
    await svc2.hydrate();
    expect(svc2.discoverVines[0]?.creatorAddress).toBe('self');
    expect(svc2.viewedIds.has('mine')).toBe(true);
  });

  it('fires onChange exactly once for a batch', async () => {
    const svc2 = new VineService({ seedMockData: false });
    const { adapter } = createMockAdapter();
    await svc2.connectAdapter(adapter);
    const onChange = vi.fn();
    svc2.onChange = onChange;
    (adapter.invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) =>
      cmd === 'list_vine_videos'
        ? Promise.resolve([dto({ id: 'h1' }), dto({ id: 'h2', videoCid: 'c2' })])
        : Promise.resolve(null));
    await svc2.hydrate();
    expect(onChange).toHaveBeenCalledTimes(1);
  });
});

describe('live vine-received viewed flag (ZEB-612 S2)', () => {
  it('honors viewed:true on the wire (previously dropped)', async () => {
    const svc2 = new VineService({ seedMockData: false });
    const { adapter, emit } = createMockAdapter();
    await svc2.connectAdapter(adapter);
    emit('vine-received', {
      id: 'w1', creatorAddress: 'p', creatorName: 'P',
      createdAt: 5, videoCid: 'c', source: 'discover', viewed: true,
    });
    expect(svc2.viewedIds.has('w1')).toBe(true);
  });
});
```

- [ ] **Step 2: Run to verify failure** — `npx vitest run src/lib/vine-service.test.ts` → new tests FAIL (`hydrate` missing; viewed flag dropped).

- [ ] **Step 3: Implement** — in `src/lib/vine-service.ts`:

1. Add to `VineDescriptorEvent` (after `source`):

```typescript
  /**
   * Local viewed-state, joined from the Rust cache's persisted viewed-set.
   * Present on both the live `vine-received` payload and `list_vine_videos`
   * rows; older frontends ignored it (ZEB-612 S2 gap-fix).
   */
  viewed?: boolean;
```

2. In `wireToVine`, change the last field from `viewed: isSelf` to:

```typescript
      viewed: wire.viewed === true || isSelf,
```

3. Add `hydrate()` after `loadFollowed()`:

```typescript
  /**
   * ZEB-612 S2: pull the Rust-persisted feed cache (`vine_feed.json`,
   * ZEB-147) into the frontend. The backend emits `vine-received` only on
   * FIRST insert, so after a restart the reloaded cache never re-emits —
   * without this pull the feed boots empty and the persisted viewed-set is
   * invisible. Call AFTER `loadFollowed()`: the DTO carries no `source`
   * field, so rows are classified by the live follow set (which matches
   * the follow()/unfollow() move semantics this service already applies).
   * Already-seen ids only merge viewed-state (live events may have raced
   * ahead of this call). Throws on IPC failure — the App-level tryConnect
   * wrapper logs it.
   */
  async hydrate(): Promise<void> {
    if (!this.adapter) return;
    const rows = (await this.adapter.invoke('list_vine_videos', {})) as Array<
      VineDescriptorEvent & { viewed: boolean }
    >;
    const viewed = new Set(this.viewedIds);
    const followedAdd: VineVideo[] = [];
    const discoverAdd: VineVideo[] = [];
    for (const wire of rows) {
      if (wire.viewed) viewed.add(wire.id);
      if (this.seenIds.has(wire.id)) continue;
      this.seenIds.add(wire.id);
      const vine = this.wireToVine(wire);
      if (vine.viewed) viewed.add(vine.id);
      if (this.followedAddresses.has(wire.creatorAddress)) {
        followedAdd.push(vine);
      } else {
        discoverAdd.push(vine);
      }
    }
    const viewedGrew = viewed.size !== this.viewedIds.size;
    if (followedAdd.length > 0) this.followedVines = [...this.followedVines, ...followedAdd];
    if (discoverAdd.length > 0) this.discoverVines = [...this.discoverVines, ...discoverAdd];
    if (viewedGrew) this.viewedIds = viewed;
    if (viewedGrew || followedAdd.length > 0 || discoverAdd.length > 0) this.onChange?.();
  }
```

- [ ] **Step 4: Run tests + typecheck** — `npx vitest run src/lib/vine-service.test.ts && npx tsc --noEmit` → PASS (full file: pre-existing tests must stay green).

- [ ] **Step 5: Commit** — `git add -A && git commit -m "ZEB-612 S2: VineService.hydrate() + honor wire viewed flag (restart-proof feed + viewed-set)"` (with trailers).

---

### Task 4: VineCard — full-bleed rework

**Files:**
- Rewrite: `src/lib/components/VineCard.svelte`
- Rewrite: `src/lib/components/__tests__/VineCard.test.ts`

**Interfaces:**
- Consumes: `formatVineDuration`, `vineCreatorLabel`, `vineOriginalCreatorLabel` (vine-utils), `Avatar`, `relativeTime`.
- Produces (props contract consumed by Task 5's VineFeed):

```typescript
{
  vine: VineVideo;
  isPlaying?: boolean;                       // feed's single playing card
  isViewed?: boolean;
  videoUrl?: string | null;                  // present only inside the lazy window
  duration?: number | null;                  // sticky, cached by the feed per cid
  onActivate?: (vine: VineVideo) => void;    // click/Enter/Space → center + play
  onDuration?: (cid: string, seconds: number) => void;  // loadedmetadata report
  showFollowButton?: boolean;
  isFollowed?: boolean;
  onFollow?: (address: string, name: string) => void;
  onUnfollow?: (address: string) => void;
  reactionCount?: number;
  likedByMe?: boolean;
  onToggleLike?: (vine: VineVideo) => void;
  reshareCount?: number;
  canReshare?: boolean;                      // feed applies isOwnOriginalVine guard
  resharing?: boolean;                       // in-flight for THIS vine
  onReshare?: (vine: VineVideo) => void;     // feed opens ReshareConfirmDialog
  onViewOriginal?: (vineId: string) => void;
}
```

- [ ] **Step 1: Write the failing tests** — replace `src/lib/components/__tests__/VineCard.test.ts` with:

```typescript
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import VineCard from '../VineCard.svelte';
import type { VineVideo } from '../../types';

const vine: VineVideo = {
  id: 'vine-01',
  creatorAddress: 'a1b2c3d4',
  creatorName: 'Alice',
  createdAt: 1700000000,
  videoCid: 'cid-abc',
  title: 'First vine',
  viewed: false,
};

function props(over: Record<string, unknown> = {}) {
  return { vine, onActivate: vi.fn(), ...over };
}

describe('VineCard (ZEB-612 S2 full-bleed)', () => {
  it('renders creator, title, and timestamp', () => {
    render(VineCard, props());
    expect(screen.getByText('Alice')).toBeTruthy();
    expect(screen.getByText('First vine')).toBeTruthy();
  });

  it('activates on click and on Enter (feed centers + plays it)', async () => {
    const onActivate = vi.fn();
    render(VineCard, props({ onActivate }));
    const card = screen.getByRole('button', { name: /First vine by Alice/ });
    await fireEvent.click(card);
    await fireEvent.keyDown(card, { key: 'Enter' });
    expect(onActivate).toHaveBeenCalledTimes(2);
  });

  it('mounts a muted looping video when a blob URL is supplied', () => {
    render(VineCard, props({ videoUrl: 'blob:fake-1' }));
    const video = screen.getByTestId('stage-video') as HTMLVideoElement;
    expect(video.getAttribute('src')).toBe('blob:fake-1');
    expect(video.hasAttribute('loop')).toBe(true);
    expect(video.muted).toBe(true);
  });

  it('renders the ▶ placeholder without a blob URL (outside the lazy window)', () => {
    render(VineCard, props());
    expect(screen.queryByTestId('stage-video')).toBeNull();
    expect(screen.getByText('▶')).toBeTruthy();
  });

  it('shows the ❚❚ paused glyph only when not playing', async () => {
    const { rerender } = render(VineCard, props({ isPlaying: false }));
    expect(screen.getByText('❚❚')).toBeTruthy();
    await rerender(props({ isPlaying: true }));
    expect(screen.queryByText('❚❚')).toBeNull();
  });

  it('reports duration from loadedmetadata (honest badge source)', async () => {
    const onDuration = vi.fn();
    render(VineCard, props({ videoUrl: 'blob:fake-1', onDuration }));
    const video = screen.getByTestId('stage-video') as HTMLVideoElement;
    Object.defineProperty(video, 'duration', { value: 6.0, configurable: true });
    await fireEvent(video, new Event('loadedmetadata'));
    expect(onDuration).toHaveBeenCalledWith('cid-abc', 6.0);
  });

  it('renders the mono duration pill when duration is known', () => {
    render(VineCard, props({ duration: 6 }));
    expect(screen.getByTestId('duration-pill')).toHaveTextContent('↻ 0:06');
  });

  it('omits the duration pill when unknown (no fabricated duration)', () => {
    render(VineCard, props());
    expect(screen.queryByTestId('duration-pill')).toBeNull();
  });

  it('shows the clay unviewed dot only when unviewed', async () => {
    const { rerender } = render(VineCard, props({ isViewed: false }));
    expect(screen.getByLabelText('Unviewed')).toBeTruthy();
    await rerender(props({ isViewed: true }));
    expect(screen.queryByLabelText('Unviewed')).toBeNull();
  });

  it('renders reshare attribution with the view-original verb', async () => {
    const onViewOriginal = vi.fn();
    const reshare: VineVideo = {
      ...vine, id: 'vine-rs', reshareOf: 'vine-orig',
      creatorName: 'Bob', creatorAddress: 'bbbb',
      originalCreatorAddress: 'a1b2c3d4', originalCreatorName: 'Alice',
    };
    render(VineCard, props({ vine: reshare, onViewOriginal }));
    expect(screen.getByText(/↻ Bob reshared ·/)).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'view original by Alice' }));
    expect(onViewOriginal).toHaveBeenCalledWith('vine-orig');
  });

  it('offers the Reshare verb when canReshare, with in-flight state', async () => {
    const onReshare = vi.fn();
    const { rerender } = render(VineCard, props({ canReshare: true, onReshare }));
    await fireEvent.click(screen.getByRole('button', { name: 'Reshare vine' }));
    expect(onReshare).toHaveBeenCalledWith(vine);
    await rerender(props({ canReshare: true, onReshare, resharing: true }));
    expect(screen.getByRole('button', { name: 'Reshare vine' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Reshare vine' })).toHaveTextContent('Resharing…');
  });

  it('shows a static reshare-count chip when the verb is unavailable (own original)', () => {
    render(VineCard, props({ canReshare: false, reshareCount: 3 }));
    expect(screen.queryByRole('button', { name: 'Reshare vine' })).toBeNull();
    expect(screen.getByLabelText('reshare count 3')).toBeTruthy();
  });

  it('hides the reshare count on reshares themselves (counts credit originals)', () => {
    const reshare: VineVideo = { ...vine, id: 'r1', reshareOf: 'orig' };
    render(VineCard, props({ vine: reshare, canReshare: true, onReshare: vi.fn(), reshareCount: 5 }));
    expect(screen.queryByLabelText('reshare count 5')).toBeNull();
  });

  it('like button toggles and stops propagation to onActivate', async () => {
    const onToggleLike = vi.fn();
    const onActivate = vi.fn();
    render(VineCard, props({ onToggleLike, onActivate, reactionCount: 2, likedByMe: false }));
    await fireEvent.click(screen.getByRole('button', { name: 'Like First vine' }));
    expect(onToggleLike).toHaveBeenCalledWith(vine);
    expect(onActivate).not.toHaveBeenCalled();
  });

  it('follow button follows/unfollows without activating the card', async () => {
    const onFollow = vi.fn();
    const onActivate = vi.fn();
    render(VineCard, props({ showFollowButton: true, isFollowed: false, onFollow, onActivate }));
    await fireEvent.click(screen.getByRole('button', { name: 'Follow Alice' }));
    expect(onFollow).toHaveBeenCalledWith('a1b2c3d4', 'Alice');
    expect(onActivate).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run to verify failure** — `npx vitest run src/lib/components/__tests__/VineCard.test.ts` → FAIL.

- [ ] **Step 3: Rewrite the component** — `src/lib/components/VineCard.svelte`:

```svelte
<script lang="ts">
  import type { VineVideo } from '../types';
  import Avatar from './Avatar.svelte';
  import { relativeTime } from '../file-utils';
  import { vineCreatorLabel, vineOriginalCreatorLabel, formatVineDuration } from '../vine-utils';

  let {
    vine, isPlaying = false, isViewed, videoUrl = null, duration = null,
    onActivate, onDuration,
    showFollowButton = false, isFollowed = false, onFollow, onUnfollow,
    reactionCount = 0, likedByMe = false, onToggleLike,
    reshareCount = 0, canReshare = false, resharing = false, onReshare,
    onViewOriginal,
  }: {
    vine: VineVideo;
    /** True when this card is the feed's single playing card. */
    isPlaying?: boolean;
    isViewed?: boolean;
    /** Blob URL — present only while the card is inside the feed's lazy window. */
    videoUrl?: string | null;
    /** Known duration in seconds (feed caches per CID once metadata loads). */
    duration?: number | null;
    /** Bring this card to center + play (click / Enter / Space). */
    onActivate?: (vine: VineVideo) => void;
    /** Reports the mounted video's metadata duration (honest badge source). */
    onDuration?: (cid: string, seconds: number) => void;
    showFollowButton?: boolean;
    isFollowed?: boolean;
    onFollow?: (address: string, name: string) => void;
    onUnfollow?: (address: string) => void;
    reactionCount?: number;
    likedByMe?: boolean;
    onToggleLike?: (vine: VineVideo) => void;
    reshareCount?: number;
    /** Whether the Reshare verb is offered (feed applies the own-original guard). */
    canReshare?: boolean;
    /** True while the feed's confirm→publish is in flight for THIS vine. */
    resharing?: boolean;
    /** Ask the feed to open the reshare confirmation for this vine. */
    onReshare?: (vine: VineVideo) => void;
    onViewOriginal?: (vineId: string) => void;
  } = $props();

  let videoEl = $state<HTMLVideoElement | null>(null);

  let viewed = $derived(isViewed ?? vine.viewed);
  let showReshareCount = $derived(!vine.reshareOf && reshareCount > 0);
  let timeStr = $derived(relativeTime(vine.createdAt * 1000));
  // ZEB-561: never render a blank creator/resharer.
  let creatorLabel = $derived(vineCreatorLabel(vine.creatorName, vine.creatorAddress));
  let originalLabel = $derived(vineOriginalCreatorLabel(vine));

  // Imperative play/pause on playing-state transitions. The `autoplay`
  // attribute only acts at element load, so a paused neighbor promoted to
  // the playing card needs an explicit play(). jsdom implements neither —
  // guard both (play may return undefined instead of a promise).
  $effect(() => {
    const el = videoEl;
    if (!el) return;
    try {
      if (isPlaying) void el.play()?.catch(() => {});
      else el.pause();
    } catch {
      // jsdom: HTMLMediaElement.play/pause are not implemented.
    }
  });

  function handleLoadedMetadata(e: Event) {
    const el = e.currentTarget as HTMLVideoElement;
    if (Number.isFinite(el.duration)) onDuration?.(vine.videoCid, el.duration);
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter' || e.key === ' ') {
      // Don't intercept keyboard events from child buttons (follow/like/…).
      if (e.target instanceof HTMLButtonElement) return;
      e.preventDefault();
      onActivate?.(vine);
    }
  }

  function handleFollowClick(e: MouseEvent) {
    e.stopPropagation();
    if (isFollowed) {
      onUnfollow?.(vine.creatorAddress);
    } else {
      onFollow?.(vine.creatorAddress, creatorLabel);
    }
  }

  function handleLikeClick(e: MouseEvent) {
    e.stopPropagation();
    onToggleLike?.(vine);
  }

  function handleReshareClick(e: MouseEvent) {
    e.stopPropagation();
    onReshare?.(vine);
  }

  function handleViewOriginal(e: MouseEvent) {
    e.stopPropagation();
    if (vine.reshareOf) onViewOriginal?.(vine.reshareOf);
  }
</script>

<div
  class="vine-card"
  class:playing={isPlaying}
  class:viewed={viewed}
  role="button"
  tabindex="0"
  aria-label="{vine.title ?? 'Untitled vine'} by {creatorLabel}"
  onclick={() => onActivate?.(vine)}
  onkeydown={handleKeyDown}
>
  <div class="stage">
    {#if videoUrl}
      <!-- svelte-ignore a11y_media_has_caption -->
      <video
        class="stage-video"
        src={videoUrl}
        muted
        loop
        playsinline
        bind:this={videoEl}
        onloadedmetadata={handleLoadedMetadata}
        aria-label="Vine video"
        data-testid="stage-video"
      ></video>
    {:else}
      <span class="stage-placeholder" aria-hidden="true">▶</span>
    {/if}
    {#if !isPlaying}
      <span class="paused-glyph" aria-hidden="true">❚❚</span>
    {/if}
    {#if duration != null}
      <span class="duration-pill" data-testid="duration-pill">↻ {formatVineDuration(duration)}</span>
    {/if}
    {#if !viewed}
      <span class="unviewed-dot" aria-label="Unviewed"></span>
    {/if}
  </div>

  <div class="meta">
    <div class="creator-row">
      <Avatar address={vine.creatorAddress} size={26} displayName={creatorLabel} />
      <span class="creator-name">{creatorLabel}</span>
      <span class="timestamp">{timeStr}</span>
      {#if showFollowButton}
        <button
          type="button"
          class="follow-btn"
          class:following={isFollowed}
          aria-label={isFollowed ? `Unfollow ${creatorLabel}` : `Follow ${creatorLabel}`}
          onclick={handleFollowClick}
        >
          {isFollowed ? 'Following' : 'Follow'}
        </button>
      {/if}
    </div>
    {#if vine.title}
      <p class="vine-title">{vine.title}</p>
    {/if}
    {#if vine.reshareOf}
      <span class="attribution-row">
        <span aria-hidden="true">↻</span> {creatorLabel} reshared ·
        {#if onViewOriginal}
          <button
            type="button"
            class="attribution-link"
            onclick={handleViewOriginal}
            aria-label="view original by {originalLabel}"
          >view original by {originalLabel}</button>
        {:else}
          view original by {originalLabel}
        {/if}
      </span>
    {/if}
    <div class="action-rail">
      {#if onToggleLike}
        <button
          type="button"
          class="rail-btn"
          class:liked={likedByMe}
          onclick={handleLikeClick}
          aria-label={likedByMe ? `Unlike ${vine.title ?? 'vine'}` : `Like ${vine.title ?? 'vine'}`}
        >
          {likedByMe ? '❤️' : '🤍'}{#if reactionCount > 0}<span class="rail-count">{reactionCount}</span>{/if}
        </button>
      {:else if reactionCount > 0}
        <span class="rail-chip">🤍 <span class="rail-count">{reactionCount}</span></span>
      {/if}
      {#if canReshare && onReshare}
        <button
          type="button"
          class="rail-btn"
          onclick={handleReshareClick}
          disabled={resharing}
          aria-label="Reshare vine"
        >
          <span aria-hidden="true">↻</span> {resharing ? 'Resharing…' : 'Reshare'}{#if showReshareCount}<span class="rail-count">{reshareCount}</span>{/if}
        </button>
      {:else if showReshareCount}
        <span class="rail-chip" aria-label="reshare count {reshareCount}">
          <span aria-hidden="true">↻</span> <span class="rail-count">{reshareCount}</span>
        </span>
      {/if}
    </div>
  </div>
</div>

<style>
  .vine-card {
    position: relative;
    height: 100%;
    display: flex;
    flex-direction: column;
    justify-content: flex-end;
    background: var(--media-stage);
    border-radius: 8px;
    overflow: hidden;
    cursor: pointer;
  }

  .vine-card:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .stage {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: filter 0.2s ease;
  }

  /* Paused cards dim; viewed cards dim further while paused (spec §3). */
  .vine-card:not(.playing) .stage {
    filter: brightness(0.82);
  }

  .vine-card.viewed:not(.playing) .stage {
    filter: brightness(0.6);
  }

  .stage-video {
    width: 100%;
    height: 100%;
    object-fit: contain;
  }

  .stage-placeholder {
    color: color-mix(in srgb, var(--on-media-stage) 45%, transparent);
    font-size: 3rem;
  }

  .paused-glyph {
    position: absolute;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    font-size: 2rem;
    color: var(--on-media-stage);
    opacity: 0.85;
    pointer-events: none;
  }

  .duration-pill {
    position: absolute;
    top: 10px;
    left: 12px;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--on-media-stage);
    background: color-mix(in srgb, var(--media-stage) 72%, transparent);
    padding: 2px 10px;
    border-radius: 999px;
  }

  .unviewed-dot {
    position: absolute;
    top: 12px;
    right: 12px;
    width: 10px;
    height: 10px;
    background: var(--gov-clay);
    border-radius: 50%;
  }

  .meta {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 14px 16px;
    background: linear-gradient(
      transparent,
      color-mix(in srgb, var(--media-stage) 88%, transparent)
    );
    color: var(--on-media-stage);
  }

  .creator-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .creator-name {
    color: var(--on-media-stage);
    font-weight: 600;
    font-size: 0.85rem;
  }

  .timestamp {
    color: color-mix(in srgb, var(--on-media-stage) 62%, transparent);
    font-family: var(--font-mono);
    font-size: 0.72rem;
  }

  .vine-title {
    color: color-mix(in srgb, var(--on-media-stage) 88%, transparent);
    font-size: 0.85rem;
    margin: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .attribution-row {
    color: color-mix(in srgb, var(--on-media-stage) 75%, transparent);
    font-size: 0.75rem;
  }

  .attribution-link {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    color: var(--on-media-stage);
    font-size: 0.75rem;
    cursor: pointer;
    text-decoration: underline;
  }

  .attribution-link:hover {
    opacity: 0.85;
  }

  .attribution-link:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .follow-btn {
    margin-left: auto;
    font-size: 0.7rem;
    font-weight: 600;
    padding: 2px 12px;
    border-radius: 999px;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
    background: var(--accent);
    color: var(--on-accent);
    border: 1px solid var(--accent);
  }

  .follow-btn:hover {
    opacity: 0.85;
  }

  .follow-btn.following {
    background: transparent;
    color: color-mix(in srgb, var(--on-media-stage) 70%, transparent);
    border-color: color-mix(in srgb, var(--on-media-stage) 45%, transparent);
  }

  .follow-btn.following:hover {
    border-color: var(--danger-vivid);
    color: var(--danger-vivid);
  }

  .action-rail {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 4px;
  }

  .rail-btn {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    background: transparent;
    border: 1px solid color-mix(in srgb, var(--on-media-stage) 28%, transparent);
    color: var(--on-media-stage);
    font-size: 0.78rem;
    padding: 4px 12px;
    border-radius: 999px;
    cursor: pointer;
    transition: background 0.15s;
  }

  .rail-btn:hover:not(:disabled) {
    background: color-mix(in srgb, var(--on-media-stage) 12%, transparent);
  }

  .rail-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .rail-btn.liked {
    border-color: color-mix(in srgb, var(--danger-vivid) 45%, transparent);
  }

  .rail-chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    color: color-mix(in srgb, var(--on-media-stage) 70%, transparent);
    font-size: 0.75rem;
  }

  .rail-count {
    font-family: var(--font-mono);
    font-size: 0.72rem;
  }
</style>
```

- [ ] **Step 4: Run tests + typecheck** — `npx vitest run src/lib/components/__tests__/VineCard.test.ts && npx tsc --noEmit` → VineCard tests PASS (VineFeed tests will break — fixed by Task 5; do NOT run the full suite here).

- [ ] **Step 5: Commit** — `git add -A && git commit -m "ZEB-612 S2: VineCard full-bleed rework (stage video, honest duration pill, on-card actions)"` (with trailers).

---

### Task 5: VineFeed — scroll-snap feed with center-detection autoplay

**Files:**
- Rewrite: `src/lib/components/VineFeed.svelte`
- Rewrite: `src/lib/components/__tests__/VineFeed.test.ts`

**Interfaces:**
- Consumes: Task 4's VineCard props, `pickCenterIndex` / `isOwnOriginalVine` (Task 1), `ReshareConfirmDialog` (`{ vine, onConfirm, onCancel }`).
- Produces: same external props contract as today **except** `onMarkViewed` now fires when a card becomes the playing card, and `playTarget` scrolls the feed (tab-switching + clearing the unviewed filter as needed) instead of opening an overlay. `VinePlayer` import removed (deleted in Task 6).

**Behavior pins (each carries a test):**
1. First card (newest) becomes the playing card on mount → `onMarkViewed` fires for it (jsdom all-zero rects → index 0).
2. Exactly one playing card; clicking another card moves `playing` to it.
3. Lazy window: with `resolveVideo` present, only playing ± 1 cards mount `<video>`; blob URLs for evicted cards are revoked.
4. Under the Unviewed filter, the card that became viewed *by playing while the filter was active* stays listed (`unviewedPin`); switching filters clears the pin (all-caught-up state still reachable).
5. `playTarget` → consumed via `onPlayTargetConsumed`, switches tab via `onTabChange` when the target lives on the other tab, clears the unviewed filter if it would hide the target, sets playing + marks viewed.
6. Reshare: card verb → `ReshareConfirmDialog` → confirm calls `onReshare(vine)`; failure surfaces a `role="alert"` feed-level error; `resharing` state reaches the card.
7. "N new" pill is clay (`gov-clay` classes asserted indirectly via its testid/class name only — no computed-style assertions in jsdom).
8. Empty-state/CTA logic byte-identical to today (ZEB-554 pins keep passing).

- [ ] **Step 1: Rewrite the tests.** Replace `src/lib/components/__tests__/VineFeed.test.ts`. Keep the existing fixture block (`vines`, `makeViewedIds`) verbatim, then:
  - **Keep as-is (assertions unchanged):** 'renders feed title', 'shows unviewed count badge', 'hides unviewed badge when all viewed', 'shows empty state when no vines', the whole ZEB-554 CTA describe, 'renders Following and Discover tabs', 'shows followed vines when Following tab active', 'shows discover vines when Discover tab active', 'shows empty state with nudge in Following when no followed vines', 'calls onTabChange when Discover tab clicked', 'shows follow button on cards in Discover tab', 'shows Following badge on cards in Following tab for followed creators', 'passes reaction data to vine cards', 'calls onToggleLike when card like is clicked', 'renders reshare counts derived from the local feed (single-pass index)', 'has accessible feed list', 'renders all vine cards'.
  - **Adapt:** 'sorts vines newest first' asserts `[data-vine-id]` order = `['vine-03','vine-02','vine-01']` via `container.querySelectorAll`. 'filters to unviewed when filter tab is clicked' unchanged in spirit (vine-02 hidden) but must pass `onMarkViewed: vi.fn()` to avoid unhandled mount callback noise.
  - **Delete:** 'opens player when a card is clicked', 'closes player when close button is clicked', 'calls onMarkViewed when a vine is opened', 'forwards onViewOriginal to VinePlayer attribution link' (player is gone).
  - **Add** (complete code):

```typescript
describe('center-detection autoplay (ZEB-612 S2)', () => {
  it('the first (newest) card plays on mount and is marked viewed', async () => {
    const onMarkViewed = vi.fn();
    const { container } = render(VineFeed, {
      followedVines: vines, viewedIds: makeViewedIds(), onMarkViewed,
    });
    await waitFor(() => expect(onMarkViewed).toHaveBeenCalledWith('vine-03'));
    const playing = container.querySelectorAll('.vine-card.playing');
    expect(playing.length).toBe(1);
  });

  it('clicking a card moves the single playing slot to it', async () => {
    const onMarkViewed = vi.fn();
    const { container } = render(VineFeed, {
      followedVines: vines, viewedIds: makeViewedIds(), onMarkViewed,
    });
    await fireEvent.click(screen.getByRole('button', { name: /First vine by Alice/ }));
    expect(onMarkViewed).toHaveBeenCalledWith('vine-01');
    const row = container.querySelector('[data-vine-id="vine-01"]');
    expect(row?.querySelector('.vine-card.playing')).toBeTruthy();
    expect(container.querySelectorAll('.vine-card.playing').length).toBe(1);
  });
});

describe('lazy video window (ZEB-612 S2)', () => {
  it('mounts <video> only for the playing card and its neighbors', async () => {
    const resolveVideo = vi.fn(async (cid: string) => `blob:fake-${cid}`);
    const five: VineVideo[] = [1, 2, 3, 4, 5].map(n => ({
      id: `vine-0${n}`, creatorAddress: `addr-${n}`, creatorName: `C${n}`,
      createdAt: 1700000000 + n * 100, videoCid: `cid-${n}`, viewed: false,
    }));
    const { container } = render(VineFeed, {
      followedVines: five, viewedIds: new Set<string>(), resolveVideo, onMarkViewed: vi.fn(),
    });
    // Newest-first order: vine-05 plays (index 0); window = vine-05 + vine-04.
    await waitFor(() => expect(container.querySelectorAll('[data-testid="stage-video"]').length).toBe(2));
    expect(resolveVideo).toHaveBeenCalledWith('cid-5');
    expect(resolveVideo).toHaveBeenCalledWith('cid-4');
    expect(resolveVideo).not.toHaveBeenCalledWith('cid-1');
  });

  it('revokes blob URLs when cards leave the window', async () => {
    const revoke = vi.fn();
    vi.stubGlobal('URL', Object.assign(Object.create(URL), { revokeObjectURL: revoke }));
    try {
      const resolveVideo = vi.fn(async (cid: string) => `blob:fake-${cid}`);
      const five: VineVideo[] = [1, 2, 3, 4, 5].map(n => ({
        id: `vine-0${n}`, creatorAddress: `addr-${n}`, creatorName: `C${n}`,
        createdAt: 1700000000 + n * 100, videoCid: `cid-${n}`, viewed: false,
      }));
      const { container } = render(VineFeed, {
        followedVines: five, viewedIds: new Set<string>(), resolveVideo, onMarkViewed: vi.fn(),
      });
      await waitFor(() => expect(container.querySelectorAll('[data-testid="stage-video"]').length).toBe(2));
      // Jump to the oldest card: window becomes vine-01 + vine-02 → cid-5/cid-4 evicted.
      await fireEvent.click(screen.getByRole('button', { name: /by C1/ }));
      await waitFor(() => expect(revoke).toHaveBeenCalledWith('blob:fake-cid-5'));
      expect(revoke).toHaveBeenCalledWith('blob:fake-cid-4');
    } finally {
      vi.unstubAllGlobals();
    }
  });
});

describe('unviewed filter pin (ZEB-612 S2)', () => {
  it('keeps the card that became viewed BY playing under the active Unviewed filter', async () => {
    const onMarkViewed = vi.fn();
    render(VineFeed, { followedVines: vines, viewedIds: makeViewedIds(), onMarkViewed });
    await fireEvent.click(screen.getByRole('button', { name: /Unviewed/ }));
    // vine-01 is unviewed → still listed; activate it under the filter.
    await fireEvent.click(screen.getByRole('button', { name: /First vine by Alice/ }));
    // Parent marks it viewed and pushes the new set down:
    // (rerender with the updated viewedIds prop, as App.svelte would)
    expect(onMarkViewed).toHaveBeenCalledWith('vine-01');
  });

  it('still reaches the all-caught-up state when every vine is viewed', async () => {
    const allViewed = vines.map(v => ({ ...v, viewed: true }));
    render(VineFeed, {
      followedVines: allViewed,
      viewedIds: new Set(allViewed.map(v => v.id)),
      onMarkViewed: vi.fn(),
      onPublish: vi.fn(),
    });
    await fireEvent.click(screen.getByRole('button', { name: /Unviewed/ }));
    expect(screen.getByText('All caught up — no unviewed vines.')).toBeTruthy();
  });
});

describe('playTarget navigation (ZEB-612 S2 — feed is the player)', () => {
  it('consumes the target, plays it, and marks it viewed', async () => {
    const onMarkViewed = vi.fn();
    const onPlayTargetConsumed = vi.fn();
    const { container, rerender } = render(VineFeed, {
      followedVines: vines, viewedIds: makeViewedIds(),
      onMarkViewed, onPlayTargetConsumed, playTarget: null,
    });
    await rerender({
      followedVines: vines, viewedIds: makeViewedIds(),
      onMarkViewed, onPlayTargetConsumed, playTarget: vines[0],
    });
    await waitFor(() => expect(onPlayTargetConsumed).toHaveBeenCalled());
    expect(onMarkViewed).toHaveBeenCalledWith('vine-01');
    const row = container.querySelector('[data-vine-id="vine-01"]');
    expect(row?.querySelector('.vine-card.playing')).toBeTruthy();
  });

  it('switches to the tab that owns the target', async () => {
    const onTabChange = vi.fn();
    const discoverOnly = [{ ...vines[0], id: 'disc-1' }];
    const { rerender } = render(VineFeed, {
      followedVines: vines, discoverVines: discoverOnly, viewedIds: makeViewedIds(),
      activeTab: 'following' as const, onTabChange,
      onMarkViewed: vi.fn(), onPlayTargetConsumed: vi.fn(), playTarget: null,
    });
    await rerender({
      followedVines: vines, discoverVines: discoverOnly, viewedIds: makeViewedIds(),
      activeTab: 'following' as const, onTabChange,
      onMarkViewed: vi.fn(), onPlayTargetConsumed: vi.fn(), playTarget: discoverOnly[0],
    });
    await waitFor(() => expect(onTabChange).toHaveBeenCalledWith('discover'));
  });
});

describe('feed-level reshare (ZEB-612 S2 — replaces the player flow)', () => {
  it('card verb → confirm dialog → onReshare', async () => {
    const onReshare = vi.fn().mockResolvedValue(undefined);
    render(VineFeed, {
      followedVines: [vines[0]], viewedIds: new Set<string>(),
      onReshare, onMarkViewed: vi.fn(),
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Reshare vine' }));
    // ReshareConfirmDialog is up — confirm it.
    await fireEvent.click(screen.getByRole('button', { name: /^Reshare$/ }));
    await waitFor(() => expect(onReshare).toHaveBeenCalledWith(expect.objectContaining({ id: 'vine-01' })));
  });

  it('cancel closes the dialog without resharing', async () => {
    const onReshare = vi.fn();
    render(VineFeed, {
      followedVines: [vines[0]], viewedIds: new Set<string>(),
      onReshare, onMarkViewed: vi.fn(),
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Reshare vine' }));
    await fireEvent.click(screen.getByRole('button', { name: /Cancel/ }));
    expect(onReshare).not.toHaveBeenCalled();
  });

  it('surfaces a reshare failure as a feed-level alert', async () => {
    const onReshare = vi.fn().mockRejectedValue(new Error('publish failed: not connected'));
    render(VineFeed, {
      followedVines: [vines[0]], viewedIds: new Set<string>(),
      onReshare, onMarkViewed: vi.fn(),
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Reshare vine' }));
    await fireEvent.click(screen.getByRole('button', { name: /^Reshare$/ }));
    await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent(/publish failed/));
  });

  it('suppresses the verb on own originals (isOwnOriginalVine guard)', () => {
    const own: VineVideo = { ...vines[0], id: 'own-1', creatorAddress: 'self' };
    render(VineFeed, {
      followedVines: [own], viewedIds: new Set<string>(),
      onReshare: vi.fn(), onMarkViewed: vi.fn(),
    });
    expect(screen.queryByRole('button', { name: 'Reshare vine' })).toBeNull();
  });
});
```

  Add `waitFor` to the testing-library import. **Check ReshareConfirmDialog's actual button labels before finalizing the reshare tests** (read the component; if its confirm button is not literally "Reshare"/"Cancel", use its real accessible names).

- [ ] **Step 2: Run to verify failure** — `npx vitest run src/lib/components/__tests__/VineFeed.test.ts` → FAIL.

- [ ] **Step 3: Rewrite the component** — `src/lib/components/VineFeed.svelte`. Script section:

```svelte
<script lang="ts">
  import type { VineVideo } from '../types';
  import VineCard from './VineCard.svelte';
  import ReshareConfirmDialog from './ReshareConfirmDialog.svelte';
  import { pickCenterIndex, isOwnOriginalVine } from '../vine-utils';

  type FeedFilter = 'all' | 'unviewed';
  type FeedTab = 'following' | 'discover';

  let {
    followedVines = [],
    discoverVines = [],
    viewedIds,
    activeTab = 'following' as FeedTab,
    followedAddresses = new Set<string>(),
    onTabChange,
    onMarkViewed,
    onPublish,
    onReshare,
    onFollow,
    onUnfollow,
    resolveVideo,
    getReaction,
    onToggleLike,
    onViewOriginal,
    playTarget = null,
    onPlayTargetConsumed,
    ownAddress,
  }: {
    followedVines?: VineVideo[];
    discoverVines?: VineVideo[];
    viewedIds: Set<string>;
    activeTab?: FeedTab;
    followedAddresses?: Set<string>;
    onTabChange?: (tab: FeedTab) => void;
    /** Fires when a card BECOMES the playing card (autoplay-on-view = viewed). */
    onMarkViewed?: (id: string) => void;
    onPublish?: () => void;
    onReshare?: (vine: VineVideo) => Promise<void> | void;
    onFollow?: (address: string, name: string) => void;
    onUnfollow?: (address: string) => void;
    resolveVideo?: (cid: string) => Promise<string>;
    getReaction?: (vineId: string) => { count: number; likedByMe: boolean };
    onToggleLike?: (vine: VineVideo) => void;
    onViewOriginal?: (vineId: string) => void;
    /**
     * Parent-controlled "navigate to this vine" request (attribution links,
     * App.svelte handleViewOriginal). The feed IS the player now: consuming
     * a target scrolls its card to the snap position and plays it, switching
     * tab / clearing the unviewed filter when they would hide it. Consumed →
     * onPlayTargetConsumed nulls the slot for the next click.
     */
    playTarget?: VineVideo | null;
    onPlayTargetConsumed?: () => void;
    /** Local node's hex address — own-original reshare suppression (FIX 2, PR #120). */
    ownAddress?: string;
  } = $props();

  let feedFilter = $state<FeedFilter>('all');
  let playingId = $state<string | null>(null);
  /**
   * Card kept visible under the Unviewed filter even after becoming viewed —
   * set when a card starts playing WHILE the filter is active, so the list
   * doesn't yank it away mid-playback. Cleared on filter/tab switches so the
   * strict unviewed view (and its all-caught-up state) stays reachable.
   */
  let unviewedPin = $state<string | null>(null);
  /** cid → blob URL, bounded to the lazy window (playing ± 1). */
  let videoUrls = $state(new Map<string, string>());
  /** cid → seconds; sticky once any mounted video reports metadata. */
  let durations = $state(new Map<string, number>());
  let reshareTarget = $state<VineVideo | null>(null);
  let resharingId = $state<string | null>(null);
  let reshareError = $state('');
  let feedListEl = $state<HTMLDivElement | null>(null);
  /** Card id to scroll to once it exists in the rendered list. */
  let pendingScrollId = $state<string | null>(null);
  let scrollScheduled = false;
  /** cids with an in-flight resolveVideo call (non-reactive bookkeeping). */
  const pendingCids = new Set<string>();

  let activeVines = $derived(
    activeTab === 'following' ? followedVines : discoverVines
  );

  let sortedVines = $derived(
    [...activeVines].sort((a, b) => b.createdAt - a.createdAt)
  );

  let filteredVines = $derived(
    activeTab === 'following' && feedFilter === 'unviewed'
      ? sortedVines.filter(v => !viewedIds.has(v.id) || v.id === unviewedPin)
      : sortedVines
  );

  let unviewedCount = $derived(
    followedVines.filter(v => !viewedIds.has(v.id)).length
  );

  // True "all caught up": the user HAS followed vines but none are unviewed.
  // (ZEB-554 / CodeAnt PR #333 — see the empty-state CTA tests.)
  let allCaughtUp = $derived(
    activeTab === 'following' && feedFilter === 'unviewed' && followedVines.length > 0
  );

  // Single-pass reshare-count index (FIX 5, PR #120 round 1).
  let reshareCountMap = $derived.by(() => {
    const map = new Map<string, number>();
    for (const v of followedVines) {
      if (v.reshareOf) map.set(v.reshareOf, (map.get(v.reshareOf) ?? 0) + 1);
    }
    for (const v of discoverVines) {
      if (v.reshareOf) map.set(v.reshareOf, (map.get(v.reshareOf) ?? 0) + 1);
    }
    return map;
  });

  // ── Center-detection autoplay ───────────────────────────────────────

  function setPlaying(id: string) {
    if (playingId === id) return;
    playingId = id;
    // Playing under the active Unviewed filter pins the card (it's about to
    // become viewed); anywhere else the pin is stale — drop it.
    unviewedPin =
      activeTab === 'following' && feedFilter === 'unviewed' ? id : null;
    onMarkViewed?.(id);
  }

  function recomputePlaying() {
    const list = feedListEl;
    if (!list) return;
    const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-vine-id]'));
    if (rows.length === 0) {
      playingId = null;
      return;
    }
    const listRect = list.getBoundingClientRect();
    const viewportCenter = listRect.top + listRect.height / 2;
    const centers = rows.map(r => {
      const rect = r.getBoundingClientRect();
      return rect.top + rect.height / 2;
    });
    const idx = pickCenterIndex(centers, viewportCenter);
    if (idx >= 0) setPlaying(rows[idx].dataset.vineId!);
  }

  function onFeedScroll() {
    if (scrollScheduled) return;
    scrollScheduled = true;
    requestAnimationFrame(() => {
      scrollScheduled = false;
      recomputePlaying();
    });
  }

  // Re-detect on mount and whenever the rendered list changes. Effects run
  // post-render, so the [data-vine-id] rows are queryable. jsdom rects are
  // all zeros → index 0 wins the tie → the first (newest) card plays on
  // mount. setPlaying's identity guard makes re-runs converge.
  $effect(() => {
    void filteredVines;
    if (!feedListEl) return;
    recomputePlaying();
  });

  function activateCard(vine: VineVideo) {
    setPlaying(vine.id);
    pendingScrollId = vine.id;
  }

  // Parent-driven navigation: the feed is the player now.
  $effect(() => {
    if (!playTarget) return;
    const target = playTarget;
    onPlayTargetConsumed?.();
    const inFollowed = followedVines.some(v => v.id === target.id);
    const inDiscover = discoverVines.some(v => v.id === target.id);
    if (!inFollowed && !inDiscover) return;
    const wantTab: FeedTab = inFollowed ? 'following' : 'discover';
    if (activeTab !== wantTab) onTabChange?.(wantTab);
    if (wantTab === 'following' && feedFilter === 'unviewed' && viewedIds.has(target.id)) {
      feedFilter = 'all';
    }
    setPlaying(target.id);
    pendingScrollId = target.id;
  });

  // Scroll the pending card into the snap position once it's rendered
  // (after a tab switch the row appears a tick later; re-run on list change).
  $effect(() => {
    void filteredVines;
    const id = pendingScrollId;
    if (!id || !feedListEl) return;
    const el = feedListEl.querySelector<HTMLElement>(`[data-vine-id="${CSS.escape(id)}"]`);
    if (el) {
      el.scrollIntoView?.({ block: 'start' });
      pendingScrollId = null;
    }
  });

  // ── Lazy blob window (playing ± 1) ──────────────────────────────────

  let windowCids = $derived.by(() => {
    const idx = filteredVines.findIndex(v => v.id === playingId);
    const around = idx === -1 ? [0, 1] : [idx - 1, idx, idx + 1];
    const set = new Set<string>();
    for (const i of around) {
      const v = filteredVines[i];
      if (v) set.add(v.videoCid);
    }
    return set;
  });

  $effect(() => {
    const want = windowCids;
    // Revoke and drop URLs whose cards left the window.
    let next: Map<string, string> | null = null;
    for (const [cid, url] of videoUrls) {
      if (!want.has(cid)) {
        URL.revokeObjectURL?.(url);
        (next ??= new Map(videoUrls)).delete(cid);
      }
    }
    if (next) videoUrls = next;
    const resolver = resolveVideo;
    if (!resolver) return;
    for (const cid of want) {
      if (videoUrls.has(cid) || pendingCids.has(cid)) continue;
      pendingCids.add(cid);
      resolver(cid)
        .then(url => {
          pendingCids.delete(cid);
          // The window may have moved while the fetch was in flight.
          if (!windowCids.has(cid)) {
            URL.revokeObjectURL?.(url);
            return;
          }
          videoUrls = new Map(videoUrls).set(cid, url);
        })
        .catch(() => {
          pendingCids.delete(cid);
        });
    }
  });

  // Revoke everything on unmount (body reads nothing reactive → runs once).
  $effect(() => () => {
    for (const url of videoUrls.values()) URL.revokeObjectURL?.(url);
  });

  function reportDuration(cid: string, seconds: number) {
    if (durations.get(cid) === seconds) return;
    durations = new Map(durations).set(cid, seconds);
  }

  // ── Feed-level reshare (moved from the retired VinePlayer) ─────────

  function requestReshare(vine: VineVideo) {
    if (resharingId) return;
    reshareError = '';
    reshareTarget = vine;
  }

  async function confirmReshare() {
    const vine = reshareTarget;
    reshareTarget = null;
    if (!vine || !onReshare) return;
    resharingId = vine.id;
    try {
      await onReshare(vine);
    } catch (err) {
      reshareError = err instanceof Error ? err.message : 'Reshare failed';
    } finally {
      resharingId = null;
    }
  }
</script>
```

Template — header/tab-bar/filter-bar/empty-state stay structurally as today (same copy, same conditional logic); the list becomes:

```svelte
{#if reshareError}
  <p class="reshare-error" role="alert">{reshareError}</p>
{/if}

{#if filteredVines.length === 0}
  <!-- unchanged empty-state block -->
{:else}
  <div
    class="feed-list"
    role="list"
    aria-label="Vine feed"
    bind:this={feedListEl}
    onscroll={onFeedScroll}
  >
    {#each filteredVines as vine (vine.id)}
      {@const reaction = getReaction?.(vine.id)}
      <div class="feed-item" role="listitem" data-vine-id={vine.id}>
        <VineCard
          {vine}
          isPlaying={playingId === vine.id}
          isViewed={viewedIds.has(vine.id)}
          videoUrl={videoUrls.get(vine.videoCid) ?? null}
          duration={durations.get(vine.videoCid) ?? null}
          onActivate={activateCard}
          onDuration={reportDuration}
          showFollowButton={vine.creatorAddress !== 'self'}
          isFollowed={followedAddresses.has(vine.creatorAddress)}
          {onFollow}
          {onUnfollow}
          reactionCount={reaction?.count ?? 0}
          likedByMe={reaction?.likedByMe ?? false}
          {onToggleLike}
          reshareCount={reshareCountMap.get(vine.id) ?? 0}
          canReshare={!!onReshare && !isOwnOriginalVine(vine, ownAddress)}
          resharing={resharingId === vine.id}
          onReshare={requestReshare}
          {onViewOriginal}
        />
      </div>
    {/each}
  </div>
{/if}

{#if reshareTarget}
  <ReshareConfirmDialog
    vine={reshareTarget}
    onConfirm={confirmReshare}
    onCancel={() => { reshareTarget = null; }}
  />
{/if}
```

Also: filter-tab buttons' `onclick` becomes `onclick={() => { feedFilter = 'all'; unviewedPin = null; }}` / `onclick={() => { feedFilter = 'unviewed'; unviewedPin = null; }}`, and the tab buttons add `unviewedPin = null;` before `onTabChange?.(…)`.

Style changes (rest of the existing `<style>` stays):

```css
  .unviewed-count {
    color: var(--gov-clay-deep);
    font-size: 0.75rem;
    font-weight: 600;
    font-family: var(--font-mono);
    background: var(--gov-clay-soft);
    padding: 2px 10px;
    border-radius: 999px;
  }

  .create-btn { /* radius 6px → 5px (button idiom) */ border-radius: 5px; }
  .empty-state-cta { border-radius: 5px; }
  .filter-tab { border-radius: 999px; }

  .reshare-error {
    color: var(--danger);
    font-size: 0.78rem;
    margin: 0;
    padding: 4px 16px;
  }

  .feed-list {
    flex: 1;
    overflow-y: auto;
    scroll-snap-type: y mandatory;
    scroll-behavior: smooth;
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 0 16px 16px;
  }

  @media (prefers-reduced-motion: reduce) {
    .feed-list {
      scroll-behavior: auto;
    }
  }

  .feed-item {
    flex: 0 0 auto;
    height: 100%;
    scroll-snap-align: start;
    scroll-snap-stop: always;
  }
```

Remove the `VinePlayer` import and the `{#if activeVine}` render block, plus the now-dead `activeVine`/`playerList`/`activeIndex`/`openPlayer`/`closePlayer`/`nextVine`/`previousVine` state and functions.

- [ ] **Step 4: Run tests + typecheck** — `npx vitest run src/lib/components/__tests__/VineFeed.test.ts src/lib/components/__tests__/VineCard.test.ts && npx tsc --noEmit` → PASS.

- [ ] **Step 5: Commit** — `git add -A && git commit -m "ZEB-612 S2: VineFeed scroll-snap rework — center-detection autoplay, lazy blob window, feed-level reshare"` (with trailers).

---

### Task 6: Retire VinePlayer + integration-test rewrite + allowlist regen + full gate

**Files:**
- Delete: `src/lib/components/VinePlayer.svelte`, `src/lib/components/__tests__/VinePlayer.test.ts`
- Rewrite: `src/lib/components/__tests__/VineFeed.integration.test.ts`
- Regenerate: `src/style-token-allowlist.json`

- [ ] **Step 1: Delete** — `git rm src/lib/components/VinePlayer.svelte src/lib/components/__tests__/VinePlayer.test.ts`. Verify no dangling references: `grep -rn "VinePlayer" src --include='*.svelte' --include='*.ts'` → expect ZERO hits.

- [ ] **Step 2: Rewrite the integration file.** In `VineFeed.integration.test.ts`, keep the fixture/setup block and:
  - **Keep (unchanged assertions):** 'renders vine cards sorted newest first' (adapt to `[data-vine-id]` order if it asserted card text order), 'shows creator names on cards', 'shows vine titles', 'shows unviewed count when there are unviewed vines', 'hides unviewed count when all are viewed', 'defaults to All filter', 'filters to unviewed vines when Unviewed tab is clicked', 'shows empty state when no vines exist', 'shows create button and fires onPublish', 'hides create button when onPublish is not provided', 'renders vine feed as a list with listitem roles'.
  - **Adapt:** 'shows attribution row for reshared vines' → assert the new copy shape (`/reshared ·/` + `view original by`). 'shows empty state when all vines are viewed in Unviewed filter' → must still pass (pin cleared on filter switch). 'marks vine as viewed when player opens' → becomes 'marks the newest vine viewed on mount (autoplay-on-view)' asserting `onMarkViewed` called with the newest id. 'vine cards are keyboard accessible' → Enter now activates (asserts `onMarkViewed` for that card instead of a player opening).
  - **Delete:** 'opens player when a vine card is clicked', 'closes player when close button is clicked', 'closes player on Escape key', 'navigates to next vine with ArrowRight', 'navigates to previous vine with ArrowLeft', 'shows reshare button in player'.
  - **Adapt the App-level reshare wiring describe** (the load-bearing attribution tests): each test opened the player then clicked its Reshare. New flow per test: `await fireEvent.click(screen.getByRole('button', { name: 'Reshare vine' }))` on the target card (scope with `within(container.querySelector('[data-vine-id="…"]')!)` when multiple cards are resharable), then confirm via the ReshareConfirmDialog, then the existing publish-payload assertions **unchanged** (they pin the attribution contract). The guard tests ('own ORIGINAL → publish NOT called') now assert the verb is absent: `expect(within(row).queryByRole('button', { name: 'Reshare vine' })).toBeNull()`.

- [ ] **Step 3: Run the two feed files** — `npx vitest run src/lib/components/__tests__/VineFeed.integration.test.ts src/lib/components/__tests__/VineFeed.test.ts` → PASS.

- [ ] **Step 4: Regenerate the style-token allowlist** (VinePlayer's 3 literals leave the tree):

```bash
UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts
git diff src/style-token-allowlist.json   # expect: VinePlayer entry removed
```

(The VinePublishDialog entry drops in Task 7 — its literal is tokenized there; regen again then.)

- [ ] **Step 5: Full frontend gate** — `npx tsc --noEmit && npx vitest run` → all green.

- [ ] **Step 6: Commit** — `git add -A && git commit -m "ZEB-612 S2: retire VinePlayer (feed is the player) + integration-test rework + allowlist ratchet"` (with trailers).

---

### Task 7: VinePublishDialog restyle + honest ≤6s gate + App wiring

**Files:**
- Modify: `src/lib/components/VinePublishDialog.svelte`
- Modify: `src/lib/components/__tests__/VinePublishDialog.test.ts`
- Modify: `src/App.svelte` (~line 2170: hydrate; ~line 3758: `resolveVideo` prop)
- Regenerate: `src/style-token-allowlist.json` (dialog overlay tokenized)

**Interfaces:**
- Consumes: `probeVideoDuration` (Task 2), `VineService.hydrate()` (Task 3), App's `resolveVideoFn`.
- Produces: new optional dialog props `resolveVideo?: (cid: string) => Promise<string>` and `probeDuration?: (url: string) => Promise<number>` (default `probeVideoDuration`). `onPublish` contract unchanged.

**Gate semantics (pin in code comment + tests):**
- Measured at pick time (picker path) and at submit time (pasted-CID path, if not yet measured).
- `> 6.0s` → block: error copy verbatim, Publish disabled while `overLimit`.
- `≤ 6.0s` → picked line `{fileName} · {X.X}s ✓ · ingested to content store`.
- **Fail-open** when unmeasurable (no `resolveVideo`, blob fetch fails, undecodable): publish proceeds, no ✓/duration shown. The gate is honest-client courtesy, not security.
- Advanced-CID input clears BOTH `pickedFileName` and `pickedDuration` (a stale measurement must never gate/pass a different CID).

- [ ] **Step 1: Update + extend the tests.** In `VinePublishDialog.test.ts`: change every `{ name: 'Publish' }` to `{ name: 'Publish vine' }`; keep all existing tests otherwise (they don't pass `resolveVideo`, so the gate is dormant). Add:

```typescript
describe('honest ≤6s gate (ZEB-612 S2)', () => {
  const gateProps = (durations: Record<string, number | Error>, over: Record<string, unknown> = {}) =>
    props({
      onPickVideo: vi.fn().mockResolvedValue({ cid: 'cid-x', fileName: 'clip.mp4' }),
      resolveVideo: vi.fn(async (cid: string) => `blob:for-${cid}`),
      probeDuration: vi.fn(async (url: string) => {
        const d = durations[url];
        if (d instanceof Error) throw d;
        return d as number;
      }),
      ...over,
    });

  it('a ≤6s pick shows the honest picked line and enables Publish', async () => {
    render(VinePublishDialog, gateProps({ 'blob:for-cid-x': 5.8 }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() =>
      expect(screen.getByTestId('picked-file')).toHaveTextContent('clip.mp4 · 5.8s ✓ · ingested to content store'));
    expect(screen.getByRole('button', { name: 'Publish vine' })).not.toBeDisabled();
  });

  it('a >6s pick blocks with the exact trim copy and disables Publish', async () => {
    render(VinePublishDialog, gateProps({ 'blob:for-cid-x': 9.3 }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent(
      'This clip is 9.3s — vines are 6 seconds or less. Trim it and re-ingest.'));
    expect(screen.getByRole('button', { name: 'Publish vine' })).toBeDisabled();
  });

  it('replacing an over-long clip with a short one clears the block', async () => {
    const onPickVideo = vi.fn()
      .mockResolvedValueOnce({ cid: 'cid-long', fileName: 'long.mp4' })
      .mockResolvedValueOnce({ cid: 'cid-short', fileName: 'short.mp4' });
    render(VinePublishDialog, gateProps(
      { 'blob:for-cid-long': 9.3, 'blob:for-cid-short': 4.0 }, { onPickVideo }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByRole('alert')).toBeTruthy());
    await fireEvent.click(screen.getByRole('button', { name: 'Replace clip' }));
    await waitFor(() =>
      expect(screen.getByTestId('picked-file')).toHaveTextContent('short.mp4 · 4.0s ✓'));
    expect(screen.getByRole('button', { name: 'Publish vine' })).not.toBeDisabled();
  });

  it('gates a pasted Advanced CID at submit (>6s → onPublish NOT called)', async () => {
    const onPublish = vi.fn();
    render(VinePublishDialog, gateProps({ 'blob:for-pastedcid': 7.7 }, { onPublish, onPickVideo: undefined }));
    await fireEvent.input(screen.getByLabelText('Video CID'), { target: { value: 'pastedcid' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Publish vine' }));
    await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent(/7\.7s/));
    expect(onPublish).not.toHaveBeenCalled();
  });

  it('fails OPEN when the probe errors (honesty courtesy, not security)', async () => {
    const onPublish = vi.fn().mockResolvedValue(undefined);
    render(VinePublishDialog, gateProps(
      { 'blob:for-cid-x': new Error('undecodable') }, { onPublish }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByTestId('picked-file')).toHaveTextContent('clip.mp4'));
    expect(screen.getByTestId('picked-file')).not.toHaveTextContent('✓');
    await fireEvent.click(screen.getByRole('button', { name: 'Publish vine' }));
    await waitFor(() => expect(onPublish).toHaveBeenCalledWith('cid-x', undefined));
  });

  it('editing the Advanced CID clears a stale measured duration', async () => {
    const onPublish = vi.fn().mockResolvedValue(undefined);
    render(VinePublishDialog, gateProps(
      { 'blob:for-cid-x': 9.3, 'blob:for-othercid': 3.0 }, { onPublish }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByRole('alert')).toBeTruthy());
    await fireEvent.input(screen.getByLabelText('Video CID'), { target: { value: 'othercid' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Publish vine' }));
    await waitFor(() => expect(onPublish).toHaveBeenCalledWith('othercid', undefined));
  });
});

describe('Commons copy (ZEB-612 S2)', () => {
  it('renders the header, subtitle, and the true-claims-only sovereign note', () => {
    render(VinePublishDialog, props());
    expect(screen.getByRole('dialog', { name: 'Share a vine' })).toBeTruthy();
    expect(screen.getByText('≤ 6 seconds · loops forever')).toBeTruthy();
    expect(screen.getByText(
      "Publishes to your sovereign identity and replicates peer-to-peer. There's no central server to take it down.")).toBeTruthy();
    // The delete claim stays out until ZEB-670 ships the verb.
    expect(screen.queryByText(/only you can delete it/)).toBeNull();
  });

  it('labels the text field Caption', () => {
    render(VinePublishDialog, props());
    expect(screen.getByText('Caption')).toBeTruthy();
  });
});
```

  Also stub `URL.revokeObjectURL` for the gate describe if jsdom lacks it (add at describe top: `beforeAll(() => { (URL as { revokeObjectURL?: unknown }).revokeObjectURL ??= vi.fn(); });` — check whether an existing global test setup already provides it).

- [ ] **Step 2: Run to verify failure** — `npx vitest run src/lib/components/__tests__/VinePublishDialog.test.ts` → new tests FAIL.

- [ ] **Step 3: Implement the dialog changes.** Script: add the two props, `MAX_VINE_SECONDS = 6`, `pickedDuration` state, `overLimit` derived, `overLimitCopy()`, `measureDuration()` (fail-open, revokes via `URL.revokeObjectURL?.(url)`), extend `handleChooseVideo` and `handleSubmit` exactly as below; template: new header (glyph chip 🎞 + title + subtitle), picked-file line variants, `Caption` label, sovereign note, `Publish vine` button, Advanced `oninput` clearing both stale fields; style: overlay → `var(--overlay)`, dialog-card radius 8px, buttons/inputs radius 5px, choose-video zone dashed `--primary-border` on `--primary-soft` radius 8px, sovereign note `--primary-soft` bg / `--primary-border` border / 8px radius, over-limit picked-file border `--danger`.

Key script code:

```typescript
  import { probeVideoDuration } from '../video-metadata';

  // …props gain:
  //   resolveVideo?: (cid: string) => Promise<string>;
  //   probeDuration?: (url: string) => Promise<number>;
  // with `probeDuration = probeVideoDuration` as the destructuring default.

  /** Honest-client cap (spec §3): vines loop at six seconds or less. */
  const MAX_VINE_SECONDS = 6;

  let pickedDuration = $state<number | null>(null);
  let overLimit = $derived(pickedDuration != null && pickedDuration > MAX_VINE_SECONDS);

  function overLimitCopy(seconds: number): string {
    return `This clip is ${seconds.toFixed(1)}s — vines are 6 seconds or less. Trim it and re-ingest.`;
  }

  /**
   * Honest ≤6s gate (ZEB-612 S2): resolve the CID to a blob and read the
   * container's metadata duration. Same honest-client posture as voice
   * moderation — the network never enforces this; our client refuses to
   * publish dishonest data. Fail-OPEN on measurement failure (no resolver
   * wired, fetch failed, undecodable container): the gate is an honesty
   * courtesy, not security, and blocking legitimate publishes over a probe
   * failure would make nothing truer. Unmeasured clips show no ✓/duration.
   */
  async function measureDuration(cid: string): Promise<number | null> {
    if (!resolveVideo) return null;
    let url: string | null = null;
    try {
      url = await resolveVideo(cid);
      return await probeDuration(url);
    } catch {
      return null;
    } finally {
      if (url) URL.revokeObjectURL?.(url);
    }
  }

  async function handleChooseVideo() {
    if (!onPickVideo || ingesting || publishing) return;
    error = '';
    ingesting = true;
    try {
      const result = await onPickVideo();
      if (!result) return; // user cancelled the picker — leave state as-is
      videoCid = result.cid;
      pickedFileName = result.fileName;
      pickedDuration = await measureDuration(result.cid);
      if (pickedDuration != null && pickedDuration > MAX_VINE_SECONDS) {
        error = overLimitCopy(pickedDuration);
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      ingesting = false;
    }
  }

  async function handleSubmit() {
    const cid = videoCid.trim();
    if (!cid) {
      error = 'Choose a video first (or paste a Video CID under Advanced)';
      return;
    }
    if (publishing || ingesting) return;
    error = '';
    publishing = true;
    try {
      // Pasted/Advanced CIDs haven't been measured yet — gate at submit.
      if (pickedDuration == null) {
        pickedDuration = await measureDuration(cid);
      }
      if (pickedDuration != null && pickedDuration > MAX_VINE_SECONDS) {
        error = overLimitCopy(pickedDuration);
        return;
      }
      await onPublish(cid, title.trim() || undefined);
      videoCid = '';
      title = '';
      pickedFileName = '';
      pickedDuration = null;
      onClose();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      publishing = false;
    }
  }
```

Key template fragments:

```svelte
    <header class="dialog-header">
      <span class="header-glyph" aria-hidden="true">🎞</span>
      <div class="header-text">
        <h3>Share a vine</h3>
        <p class="header-sub">≤ 6 seconds · loops forever</p>
      </div>
      <button type="button" class="close-btn" onclick={onClose} aria-label="Close">✕</button>
    </header>
```

(dialog `aria-label` becomes `"Share a vine"`.) Picked-file block:

```svelte
          {#if pickedFileName}
            <div class="picked-file" class:over-limit={overLimit} data-testid="picked-file">
              <span class="picked-name" title={pickedFileName}>
                {#if pickedDuration != null && !overLimit}
                  {pickedFileName} · {pickedDuration.toFixed(1)}s ✓ · ingested to content store
                {:else if overLimit}
                  {pickedFileName} · {pickedDuration?.toFixed(1)}s
                {:else}
                  {pickedFileName} · ingested to content store
                {/if}
              </span>
              <button
                type="button"
                class="link-btn"
                onclick={handleChooseVideo}
                disabled={ingesting || publishing}
              >Replace clip</button>
            </div>
          {:else}
```

Caption label: `<span class="field-label">Caption <span class="optional">(optional)</span></span>` (input/placeholder/counter unchanged). Advanced input: `oninput={() => { pickedFileName = ''; pickedDuration = null; }}`. Before `.dialog-actions`:

```svelte
      <div class="sovereign-note">
        <span aria-hidden="true">🔑</span>
        <p>Publishes to your sovereign identity and replicates peer-to-peer. There's no central server to take it down.</p>
      </div>
```

Submit button: `>{publishing ? 'Publishing…' : 'Publish vine'}</button>` with `disabled={publishing || ingesting || !videoCid.trim() || overLimit}`.

New/changed styles:

```css
  .dialog-overlay { background: var(--overlay); }         /* was rgba(0,0,0,0.7) */
  .dialog-card { border-radius: 8px; }                    /* was 12px */
  .header-glyph {
    width: 34px; height: 34px;
    display: flex; align-items: center; justify-content: center;
    background: var(--primary-soft);
    border-radius: 8px;
    font-size: 1rem;
  }
  .header-text { display: flex; flex-direction: column; gap: 2px; }
  .header-sub { color: var(--text-muted); font-size: 0.72rem; margin: 0; font-weight: 400; }
  .field-input { border-radius: 5px; }                    /* was 6px */
  .choose-video-btn {
    border: 1.5px dashed var(--primary-border);
    background: var(--primary-soft);
    border-radius: 8px;
  }
  .picked-file.over-limit { border-color: var(--danger); }
  .sovereign-note {
    display: flex;
    gap: 9px;
    align-items: flex-start;
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    border-radius: 8px;
    padding: 10px 12px;
  }
  .sovereign-note p { margin: 0; color: var(--text-secondary); font-size: 0.75rem; line-height: 1.5; }
  .btn { border-radius: 5px; }                            /* was 6px */
```

- [ ] **Step 4: App wiring** — in `src/App.svelte`:
  1. After `await tryConnect('vine.loadFollowed', vineService.loadFollowed());` (line ~2170) add:

```typescript
      // ZEB-612 S2: pull the Rust-persisted feed cache (descriptors +
      // viewed-set) — vine-received only fires on FIRST insert, so a
      // restarted app never re-receives what the cache already holds.
      // Must run after loadFollowed: hydrate classifies rows by the
      // follow set (the list DTO carries no source field).
      await tryConnect('vine.hydrate', vineService.hydrate());
```

  2. The `<VinePublishDialog … />` render (line ~3758) gains `resolveVideo={resolveVideoFn}`.

- [ ] **Step 5: Regenerate the allowlist** (dialog's literal tokenized):

```bash
UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts
git diff src/style-token-allowlist.json   # expect: VinePublishDialog entry removed
```

- [ ] **Step 6: Full gate** — `npx tsc --noEmit && npx vitest run` → all green.

- [ ] **Step 7: Commit** — `git add -A && git commit -m "ZEB-612 S2: publish dialog Commons restyle + honest ≤6s gate + feed hydration wiring"` (with trailers).

---

## Post-plan checklist (controller, not tasks)

1. Full `npx vitest run` + `npx tsc --noEmit` one final time on the branch head.
2. Open PR: title `ZEB-612 slice 2: Vines — scroll-snap autoplay feed, honest publish gate, restart-proof hydration`, body includes "Part of ZEB-612" (NO closing keyword), honesty notes (§8 rows exercised: duration badge honest source, no trim UI, reworded sovereign note, fail-open gate), and a note that vine *reactions* still don't survive restart (needs a Rust list IPC — spin-off ticket filed).
3. File the reactions-rehydration spin-off ticket in Linear (use the assigned ID in the PR body).
4. Fire `@coderabbitai review` ONCE at PR-open. Converge rounds follow the standing protocol.

## Self-review notes

- **Spec coverage:** §3 feed (snap ✓, autoplay/playingId ✓, dim+❚❚ ✓, lazy window + revocation ✓, no endless-cycling — real feed ends in existing empty states ✓, tabs/filter/N-new ✓, attribution ✓, hearts ✓, reshare counts ✓, viewed-on-play ✓, duration badge ✓, playTarget scroll ✓, VinePlayer retired ✓, hydrate gap-fix ✓); §3 publish (header/subtitle ✓, caption ≤140 ✓, 100MB hint ✓, advanced CID ✓, ≤6s gate ✓, no trim chrome ✓, sovereign rewording ✓).
- **Type consistency:** `onDuration(cid, seconds)` (Task 4) ↔ `reportDuration(cid, seconds)` (Task 5); `onActivate(vine)` ↔ `activateCard(vine)`; `canReshare`/`resharing`/`onReshare(vine)` card props ↔ feed's `requestReshare`; `hydrate()` (Task 3) ↔ App wiring (Task 7); `probeDuration` prop default ↔ Task 2 export.
- **Known intentional behaviors (pre-empt bot findings):** fail-open gate (documented rationale); `unviewedPin` (playing card survives the filter it just violated); classification-by-current-follow-set in hydrate (matches follow()/unfollow() move semantics, not the cache's first-arrival tag); mount marks the newest card viewed (autoplay-on-view IS viewing); `URL.revokeObjectURL?.()` optional-call (jsdom).
