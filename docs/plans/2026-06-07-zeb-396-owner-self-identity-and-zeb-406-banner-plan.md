# ZEB-396 (owner self-identity) + ZEB-406 (backup-banner overlay) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A freshly-minted community owner can immediately use every moderation affordance (create/rename/delete channel, kick, set-power), and the backup-reminder banner no longer clips page headers or intercepts clicks on the top toolbar.

**Architecture:** The community world is keyed by **owner_id**; `App.svelte` was feeding community self-identity comparisons the **node address** (`myAddress` from `get_node_addr`), which never matches the owner_id-keyed roster, so the owner's power resolved to 0 and all moderation hid. Fix: thread `selfOwnerId` (owner_id, already in `App.svelte`) into the community world via one extracted, unit-tested helper (`myCommunityPower`) plus `CommunityView`'s `ownAddress` prop (downstream consumers inherit transitively). Separately, take the backup banner out of `position: fixed` and render it in normal flow inside a flex-column app-shell.

**Tech Stack:** Svelte 5 runes (`$state`/`$derived`/`$props`), TypeScript, vitest + @testing-library/svelte, Playwright over CDP (live Tauri WebView2 on Ildwyn).

**Branch:** `zeb-396-owner-self-identity` (off `main` @ 19724ea; design committed f086d65).

---

## File structure

- `src/lib/community-self-power.ts` (**create**) — pure `selfCommunityPower(members, selfOwnerId)` helper. One responsibility: resolve the viewer's own power from an owner_id-keyed roster.
- `src/lib/community-self-power.test.ts` (**create**) — unit tests for the helper.
- `src/App.svelte` (**modify**) — use the helper for `myCommunityPower`; pass `selfOwnerId` as `CommunityView`'s `ownAddress`; wrap `<Layout>` + banner in `.app-shell`; drop `.backup-banner-overlay`.
- `src/lib/components/Layout.svelte` (**modify**) — `.layout` fills the shell instead of `height: 100vh`.
- `.playwright-scratch/zeb396-verify.mjs` (**create**, gitignored) — live verification driver.

---

### Task 1: Extract + unit-test the self-power resolver (ZEB-396 core)

**Files:**
- Create: `src/lib/community-self-power.ts`
- Test: `src/lib/community-self-power.test.ts`

- [ ] **Step 1: Write the failing test**

Create `src/lib/community-self-power.test.ts`:
```ts
import { describe, it, expect } from 'vitest';
import { selfCommunityPower } from './community-self-power';

const member = (address: string, power: number) => ({ address, power });

describe('selfCommunityPower (ZEB-396)', () => {
  it('returns the matching owner_id row power', () => {
    const members = [member('cb7026bb877c6e580a5a35e5a4e1f857', 100), member('aa11', 0)];
    expect(selfCommunityPower(members, 'cb7026bb877c6e580a5a35e5a4e1f857')).toBe(100);
  });

  it('returns 0 when selfOwnerId is null (owner identity not loaded yet)', () => {
    expect(selfCommunityPower([member('cb7026bb877c6e580a5a35e5a4e1f857', 100)], null)).toBe(0);
  });

  it('returns 0 when matched against the wrong identity (e.g. the node address)', () => {
    // a node address (get_node_addr) never matches the owner_id-keyed roster — the original bug.
    expect(selfCommunityPower([member('cb7026bb877c6e580a5a35e5a4e1f857', 100)], 'a888ba9ecd0635acea2af590a70f02a8')).toBe(0);
  });

  it('returns 0 for an empty roster', () => {
    expect(selfCommunityPower([], 'cb7026bb877c6e580a5a35e5a4e1f857')).toBe(0);
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/community-self-power.test.ts`
Expected: FAIL — `Failed to resolve import "./community-self-power"` (module does not exist yet).

- [ ] **Step 3: Write minimal implementation**

Create `src/lib/community-self-power.ts`:
```ts
import type { CommunityMember } from './types';

/**
 * ZEB-396: resolve the viewer's own power level within a community.
 *
 * The community roster (`listCommunityMembers` → MemberInfoDto.addr) is keyed by
 * **owner_id** (32-char lowercase hex of the OwnerAddr). The viewer's self-identity
 * in the community world is therefore `selfOwnerId` (from get_owner_state), NOT the
 * node/transport address (`get_node_addr`). Matching against the node address — the
 * prior bug — never resolved, so the owner's power fell through to 0 and every
 * moderation affordance (create/rename/delete channel, kick, set-power) was hidden.
 *
 * Returns 0 when owner identity hasn't loaded yet (`selfOwnerId === null`) or when
 * the viewer has no materialized roster entry.
 */
export function selfCommunityPower(
  members: Pick<CommunityMember, 'address' | 'power'>[],
  selfOwnerId: string | null,
): number {
  if (!selfOwnerId) return 0;
  return members.find((m) => m.address === selfOwnerId)?.power ?? 0;
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `npx vitest run src/lib/community-self-power.test.ts`
Expected: PASS (4 passed).

- [ ] **Step 5: Commit**

```bash
git add src/lib/community-self-power.ts src/lib/community-self-power.test.ts
git commit -m "ZEB-396: extract selfCommunityPower helper (owner_id-keyed self-power)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Wire the helper + owner_id self-identity into App.svelte (ZEB-396)

**Files:**
- Modify: `src/App.svelte` (`myCommunityPower` ≈ line 869-871; import; `CommunityView ownAddress` ≈ line 2522)

- [ ] **Step 1: Add the import**

In `src/App.svelte`, add near the other `./lib/...` imports (e.g. just after the `MemberCardService` import line ~81):
```ts
import { selfCommunityPower } from './lib/community-self-power';
```

- [ ] **Step 2: Replace the broken derivation**

Replace the existing block (≈ lines 867-871):
```js
  // resolves later than the first roster load (race fixed in PR #91
  // review). Never assign to this directly.
  let myCommunityPower = $derived(
    communityMembers.find((m) => m.address === myAddress)?.power ?? 0,
  );
```
with:
```js
  // ZEB-396: the roster is owner_id-keyed; self-power must match selfOwnerId
  // (owner_id), NOT myAddress (node address from get_node_addr). The $derived
  // recomputes when selfOwnerId resolves after start_node. Never assign directly.
  let myCommunityPower = $derived(selfCommunityPower(communityMembers, selfOwnerId));
```

- [ ] **Step 3: Pass owner_id as CommunityView's self-identity**

Change the `<CommunityView>` prop (≈ line 2522) from:
```svelte
        ownAddress={myAddress}
```
to:
```svelte
        ownAddress={selfOwnerId ?? ''}
```
Leave the other two `ownAddress={myAddress}` sites unchanged — VineFeed (≈ 2713) and ProfilePopover (≈ 2881) are the vine / Reticulum-profile worlds, which are correctly node-addr-keyed.

- [ ] **Step 4: Verify types + existing suite stay green**

Run: `npx tsc --noEmit`
Expected: no errors (`selfOwnerId` is `string | null`; `selfOwnerId ?? ''` is `string`, matching `CommunityView`'s `ownAddress: string`).

Run: `npx vitest run`
Expected: PASS — full suite green (no component test regressions; the downstream comparisons were already correct, only the value App feeds them changed).

- [ ] **Step 5: Commit**

```bash
git add src/App.svelte
git commit -m "ZEB-396: thread selfOwnerId (owner_id) as community self-identity

myCommunityPower + CommunityView.ownAddress now key on selfOwnerId, fixing
owner moderation (create/rename/delete channel, kick, set-power), the '(you)'
marker, self-sort, own-message detection, and voting self-identity. Node-addr
myAddress is retained for the DM/vine/nav/profile worlds.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Render the backup banner in normal flow (ZEB-406)

**Files:**
- Modify: `src/App.svelte` (wrap `<Layout>` + banner in `.app-shell`; remove `.backup-banner-overlay`)
- Modify: `src/lib/components/Layout.svelte` (`.layout` fills the shell)

- [ ] **Step 1: Open the app-shell and move the banner above Layout**

In `src/App.svelte`, immediately **before** the `<Layout ...>` open tag (≈ line 2476), insert:
```svelte
<div class="app-shell">
  {#if ownerIdentityState === 'present'}
    <BackupReminderBanner />
  {/if}
```

- [ ] **Step 2: Close the app-shell after Layout**

Immediately **after** the `</Layout>` close tag (≈ line 2873), insert:
```svelte
</div>
```

- [ ] **Step 3: Delete the old fixed-overlay banner block**

Remove the old banner block (≈ lines 3203-3210):
```svelte
<!-- ZEB-338: backup-reminder banner. Self-gates on backup-skipped state; only
     shown once an owner identity is loaded so it never stacks behind the mint
     gate or the startup-error overlay. -->
{#if ownerIdentityState === 'present'}
  <div class="backup-banner-overlay">
    <BackupReminderBanner />
  </div>
{/if}
```

- [ ] **Step 4: Swap the CSS — add `.app-shell`, remove `.backup-banner-overlay`**

In `src/App.svelte`'s `<style>`, remove the `.backup-banner-overlay` rule (≈ lines 3354-3360):
```css
  .backup-banner-overlay {
    position: fixed;
    top: 0;
    left: 0;
    right: 0;
    z-index: 40;
  }
```
and add (near the top of the style block):
```css
  /* ZEB-406: app-shell hosts the optional backup banner in normal flow above the
     main layout, so the banner reserves height instead of overlaying + intercepting
     the top toolbar. */
  .app-shell {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
```

- [ ] **Step 5: Make `.layout` fill the shell instead of the viewport**

In `src/lib/components/Layout.svelte`, in the base `.layout` rule (≈ lines 245-253), replace:
```css
    height: 100vh;
```
with:
```css
    flex: 1 1 auto;
    min-height: 0;
```
(Leave `display: grid`, `position: relative`, the grid-template, and all `.layout.*` variant rules unchanged — `.layout` is the direct flex child of `.app-shell` and now fills the height the banner doesn't take.)

- [ ] **Step 6: Verify the suite stays green**

Run: `npx tsc --noEmit`
Expected: no errors.

Run: `npx vitest run src/lib/components/Layout.test.ts src/lib/media-panel-prefs.test.ts`
Expected: PASS — WS-C Layout/media-panel tests still green (they assert DOM/aria/logic, not pixel height).

- [ ] **Step 7: Commit**

```bash
git add src/App.svelte src/lib/components/Layout.svelte
git commit -m "ZEB-406: render backup banner in normal flow (app-shell), not a fixed overlay

The banner no longer overlays + intercepts the top toolbar (search, + create)
or clips page headers; it reserves its own height above a flex-filled layout.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Live verification on Ildwyn (Playwright/CDP) — the integration proof

**Files:**
- Create: `.playwright-scratch/zeb396-verify.mjs` (gitignored — NEVER commit)

This is the integration test for Tasks 1-3 (App-level wiring + CSS layout aren't unit-testable in jsdom). The dev app is running; Vite HMR picks up the changes. Reuse the existing `ZEB394 Probe` community (owner = this identity).

- [ ] **Step 1: Write the verification driver**

Create `.playwright-scratch/zeb396-verify.mjs`:
```js
// ZEB-396 + ZEB-406 live verification: owner sees create-channel affordance,
// banner no longer intercepts the "+" toolbar, headers not clipped.
import { chromium } from 'playwright';

const browser = await chromium.connectOverCDP('http://127.0.0.1:9222');
const allPages = browser.contexts().flatMap((c) => c.pages());
const page = allPages.find((p) => { try { const u = new URL(p.url()); return u.port === '5173' && (u.pathname === '/' || u.pathname.endsWith('index.html')); } catch { return false; } }) || allPages.find((p) => p.url().includes('5173') && !p.url().includes('network.html'));
const wait = (ms) => page.waitForTimeout(ms);

await page.reload({ waitUntil: 'domcontentloaded' });
await page.waitForSelector('.layout', { timeout: 20000 });
await wait(2500);

// ZEB-406: banner must NOT intercept the top toolbar. elementFromPoint at the
// fab's center should be the fab (or its child), never the banner overlay.
const bannerCheck = await page.evaluate(() => {
  const fab = document.querySelector('.fab-btn');
  if (!fab) return { fab: false };
  const b = fab.getBoundingClientRect();
  const top = document.elementFromPoint(b.x + b.width / 2, b.y + b.height / 2);
  const bannerVisible = !!document.querySelector('.backup-banner');
  const overlayPresent = !!document.querySelector('.backup-banner-overlay');
  return { fab: true, topAtFab: top ? `${top.tagName.toLowerCase()}.${(top.className||'').toString().split(' ')[0]}` : null, bannerVisible, overlayPresent };
});
console.log('banner/fab:', JSON.stringify(bannerCheck));

// Click the fab WITHOUT dismissing the banner (real hit-tested click).
await page.evaluate(() => { const b = Array.from(document.querySelectorAll('button')).find((x) => x.getAttribute('aria-label') === 'Messages'); b?.click(); });
await wait(500);
let fabClickable = true;
try { await page.click('.fab-btn', { timeout: 4000 }); await page.keyboard.press('Escape'); }
catch { fabClickable = false; }
console.log('fab clickable without dismiss:', fabClickable);

// ZEB-396: enter ZEB394 Probe community, assert create-channel button now renders.
const enteredAndPower = await page.evaluate(async () => {
  const node = Array.from(document.querySelectorAll('.nav-tree-container button, .nav-panel button'))
    .find((b) => (b.textContent || '').includes('ZEB394 Probe'));
  node?.click();
  await new Promise((r) => setTimeout(r, 1500));
  return {
    inView: !!document.querySelector('.community-view'),
    createBtn: !!document.querySelector('.create-channel-btn'),
    youMarker: (document.querySelector('.members-panel')?.textContent || '').includes('(you)')
      || !!document.querySelector('.members-panel .self'),
  };
});
console.log('community:', JSON.stringify(enteredAndPower));

console.log('\n=== VERDICT ===');
console.log('ZEB-406 banner not intercepting fab:', bannerCheck.topAtFab && !bannerCheck.topAtFab.includes('banner') && fabClickable);
console.log('ZEB-396 owner sees create-channel:', enteredAndPower.createBtn);
process.exit(0);
```

- [ ] **Step 2: Run it**

Run: `node .playwright-scratch/zeb396-verify.mjs`
Expected:
- `topAtFab` is `button.fab-btn` (not `*.backup-banner*`) and `fab clickable without dismiss: true`.
- `community: { inView: true, createBtn: true, youMarker: true }` — the create-channel affordance now renders for the owner (was `false` pre-fix), and self is marked.

- [ ] **Step 3: Screenshot + eyeball**

If any assertion is false, capture `await page.screenshot(...)`, read it, and diagnose before proceeding. (No commit — scratch files are gitignored and never committed.)

---

### Task 5: Full gate, push, PR

**Files:** none (CI gates + PR)

- [ ] **Step 1: Run the full frontend gate**

Run: `npx tsc --noEmit && npx vitest run`
Expected: tsc clean; full vitest suite green (includes the new `community-self-power.test.ts` + all existing).

- [ ] **Step 2: Confirm the diff is intentional and scoped**

Run: `git status -s && git --no-pager diff --stat main...HEAD`
Expected: only `src/lib/community-self-power.ts(.test.ts)`, `src/App.svelte`, `src/lib/components/Layout.svelte`, and `docs/specs/...` + `docs/plans/...`. NOTHING under `.playwright-scratch/`, `gen/schemas/`, or stray docs.

- [ ] **Step 3: Commit the plan doc**

```bash
git add docs/plans/2026-06-07-zeb-396-owner-self-identity-and-zeb-406-banner-plan.md
git commit -m "ZEB-396/ZEB-406: implementation plan

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

- [ ] **Step 4: Push + open PR**

```bash
git push -u origin zeb-396-owner-self-identity
gh pr create --title "ZEB-396 + ZEB-406: owner self-identity (owner_id) + backup-banner in normal flow" --body "<see body below>"
```
PR body covers: root cause (owner_id vs node-addr, with the live values), the two fixes, testing (unit + live), and **coordination note**: shares `App.svelte` with PR #202 (ZEB-404) in a different region; clean 3-way merge expected. End with the Claude Code attribution line.

- [ ] **Step 5: Monitor + address bot review**

Watch CodeRabbit / Cursor Bugbot / Qodo / CodeAnt (Greptile excluded). Address real findings with fixes + regression tests; decline false positives with rationale. Re-verify CI green.

---

## Self-review

**Spec coverage:**
- Part A `myCommunityPower` fix → Task 1 (helper) + Task 2 (wire). ✓
- Part A "all community sites" (you-marker, self-sort, viewerPower, last-admin, own-message, voting) → Task 2 prop threading (transitive) + Task 4 live `youMarker`/`createBtn`. ✓
- Part A "leave myAddress for DM/vine/nav/profile" → Task 2 Step 3 explicit. ✓
- Part B banner-in-flow → Task 3. ✓
- Testing (unit + live) → Tasks 1, 4. ✓
- Coordination with PR #202 → Task 5 Step 4. ✓

**Placeholder scan:** No TBD/TODO; all code blocks concrete; `<see body below>` in Task 5 Step 4 is described inline (root cause + two fixes + coordination), acceptable as a body outline.

**Type consistency:** `selfCommunityPower(members, selfOwnerId)` signature identical across Tasks 1-2; `Pick<CommunityMember,'address'|'power'>[]` matches `communityMembers: CommunityMember[]`; `selfOwnerId: string | null` matches the `App.svelte` declaration; `ownAddress: string` satisfied by `selfOwnerId ?? ''`.
