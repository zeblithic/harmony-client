# ZEB-650 Slice 1 (+ZEB-659) Implementation Plan — onboarding meta bundle

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship ZEB-650 slice 1 — data-backed onboarding/identity meta (backup
timestamps + day counts, DevicesPanel meta row, name-prompt identicon chip) —
plus the ZEB-659 Network-Viz dev-flag gate, in one pure-frontend PR.

**Architecture:** All data is already client-reachable: two additive
owner-scoped localStorage timestamps inside the existing ZEB-587 flags module;
derived facts from `OwnerStateView` (`enrolledAt` is Unix **seconds**); one
existing IPC (`list_owner_communities`) behind a new mockable seam module; a
build-time `import.meta.env.DEV` prop default. No Rust changes.

**Tech Stack:** Svelte 5 (runes), TypeScript, Vitest + @testing-library/svelte.

Spec: `docs/specs/2026-07-09-zeb-650-commons-g-deferred-data-design.md` §2.
Branch: `zeb-650-659-slice1-onboarding-meta` (exists; spec committed).

## Global Constraints

- Svelte 5 runes (`$props/$state/$derived/$effect`).
- **Budget-0 color tokens:** zero new hex/rgb/named colors in `<style>` blocks
  (`var(--x)`, `color-mix(in srgb, var(--x) N%, …)`, `transparent` allowed).
- Preserve every existing `data-testid`, accessible name, aria id, and copy pin
  byte-identical. New testids (exact): `devices-meta-keytype`,
  `devices-meta-enrolled`, `devices-meta-communities`,
  `devices-last-backed-up`, `backup-reminder-days`, `name-prompt-chip`.
- Owner-scoped storage keys via the existing `ownerKey()` idiom
  (`<base>:owner-<id>`). New key bases (exact):
  `harmony.onboarding.backupSkippedAt`,
  `harmony.onboarding.recoveryBackedUpAt` (both localStorage).
- Legacy degradation: owners with boolean flags but no timestamps → all new
  reads return `null`, all new UI renders nothing / keeps current copy.
  `isBackupReminderVisible` predicate unchanged.
- Gates per task: `npx tsc --noEmit` + targeted `npx vitest run <file>`.
  Finish: full `npx vitest run`.
- Commit per task with trailers:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` +
  `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

## File map

| File | Change |
|---|---|
| `src/lib/onboarding-backup-flags.ts` | + timestamp stamps & reads (T1) |
| `src/lib/onboarding-backup-flags.test.ts` | + timestamp tests (T1) |
| `src/lib/components/BackupReminderBanner.svelte` | + N-days copy (T2) |
| `src/lib/components/__tests__/BackupReminderBanner.test.ts` | + copy tests (T2) |
| `src/lib/owner-meta.ts` | **new** — communities-count seam (T3) |
| `src/lib/owner-meta.test.ts` | **new** (T3) |
| `src/lib/components/DevicesPanel.svelte` | meta row + last-backed-up + gap fix (T3) |
| `src/lib/components/__tests__/DevicesPanel.test.ts` | + seam mock + new tests (T3) |
| `src/lib/components/NamePromptModal.svelte` | + identicon chip (T4) |
| `src/lib/components/__tests__/NamePromptModal.test.ts` | + chip tests (T4) |
| `src/App.svelte` | pass `ownerIdHex={selfOwnerId}` (T4) |
| `src/lib/components/NavPanel.svelte` | + `showNetworkViz` gate (T5) |
| `src/lib/components/__tests__/NavPanel.test.ts` | + gate tests (T5) |

---

### Task 1: Backup timestamps in `onboarding-backup-flags.ts`

**Files:**
- Modify: `src/lib/onboarding-backup-flags.ts`
- Test: `src/lib/onboarding-backup-flags.test.ts`

**Interfaces:**
- Consumes: existing `ownerKey`, `markBackupSkipped`, `markRecoveryBackedUp`.
- Produces (used by T2/T3): `backupSkippedAtMs(ownerId: string): number | null`,
  `recoveryBackedUpAtMs(ownerId: string): number | null`,
  `daysSinceBackupSkipped(ownerId: string, nowMs?: number): number | null`.

- [ ] **Step 1: Write the failing tests** — append to
  `src/lib/onboarding-backup-flags.test.ts` (import the three new fns in the
  existing import block):

```typescript
describe('backup timestamps (ZEB-650 slice 1)', () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
  });

  it('markBackupSkipped stamps an owner-scoped skippedAt', () => {
    const before = Date.now();
    markBackupSkipped(A);
    const at = backupSkippedAtMs(A);
    expect(at).not.toBeNull();
    expect(at!).toBeGreaterThanOrEqual(before);
    expect(backupSkippedAtMs(B)).toBeNull();
  });

  it('markRecoveryBackedUp stamps an owner-scoped backedUpAt', () => {
    markRecoveryBackedUp(A);
    expect(recoveryBackedUpAtMs(A)).not.toBeNull();
    expect(recoveryBackedUpAtMs(B)).toBeNull();
  });

  it('legacy boolean-only flags read as null timestamps', () => {
    // Pre-timestamp writers only set the boolean key.
    localStorage.setItem(`harmony.onboarding.backupSkipped:owner-${A}`, 'true');
    expect(isBackupSkipped(A)).toBe(true);
    expect(backupSkippedAtMs(A)).toBeNull();
    expect(daysSinceBackupSkipped(A)).toBeNull();
  });

  it('corrupt stamp value reads as null', () => {
    localStorage.setItem(`harmony.onboarding.backupSkippedAt:owner-${A}`, 'garbage');
    expect(backupSkippedAtMs(A)).toBeNull();
  });

  it('daysSinceBackupSkipped floors whole days from injected now', () => {
    markBackupSkipped(A);
    const at = backupSkippedAtMs(A)!;
    expect(daysSinceBackupSkipped(A, at)).toBe(0);
    expect(daysSinceBackupSkipped(A, at + 86_399_000)).toBe(0);
    expect(daysSinceBackupSkipped(A, at + 86_400_000)).toBe(1);
    expect(daysSinceBackupSkipped(A, at + 7 * 86_400_000 + 5)).toBe(7);
  });

  it('clock skew (stamp in the future) clamps to 0, never negative', () => {
    markBackupSkipped(A);
    const at = backupSkippedAtMs(A)!;
    expect(daysSinceBackupSkipped(A, at - 86_400_000)).toBe(0);
  });

  it('re-backing-up updates the backedUpAt stamp', () => {
    localStorage.setItem(`harmony.onboarding.recoveryBackedUpAt:owner-${A}`, '5');
    markRecoveryBackedUp(A);
    expect(recoveryBackedUpAtMs(A)!).toBeGreaterThan(5);
  });
});
```

- [ ] **Step 2: Run to verify failure**

Run: `npx vitest run src/lib/onboarding-backup-flags.test.ts`
Expected: FAIL — `backupSkippedAtMs` is not exported.

- [ ] **Step 3: Implement** — in `src/lib/onboarding-backup-flags.ts`, add the
  key bases below the existing three, the stamp helpers below `writeFlag`, and
  one `writeStamp` line inside each existing marker:

```typescript
// ZEB-650 slice 1: additive timestamps beside the booleans. Legacy owners
// (boolean set, stamp absent) read as null — callers degrade to the
// pre-timestamp copy rather than fabricating a figure.
const SKIPPED_AT = 'harmony.onboarding.backupSkippedAt';
const BACKED_UP_AT = 'harmony.onboarding.recoveryBackedUpAt';

function writeStamp(base: string, ownerId: string, nowMs: number): void {
  try {
    localStorage.setItem(ownerKey(base, ownerId), String(nowMs));
  } catch (e) {
    console.debug('[zeb-650] onboarding stamp write failed:', e instanceof Error ? e.message : String(e));
  }
}

function readStamp(base: string, ownerId: string): number | null {
  try {
    const raw = localStorage.getItem(ownerKey(base, ownerId));
    if (raw === null) return null;
    const n = Number(raw);
    return Number.isFinite(n) && n >= 0 ? n : null;
  } catch {
    return null;
  }
}

export function backupSkippedAtMs(ownerId: string): number | null {
  return readStamp(SKIPPED_AT, ownerId);
}

export function recoveryBackedUpAtMs(ownerId: string): number | null {
  return readStamp(BACKED_UP_AT, ownerId);
}

/** Whole days (floored) since this owner skipped backup; null when no stamp
 *  exists (legacy owner or never skipped). Clamped at 0 under clock skew. */
export function daysSinceBackupSkipped(ownerId: string, nowMs: number = Date.now()): number | null {
  const at = backupSkippedAtMs(ownerId);
  if (at === null) return null;
  return Math.max(0, Math.floor((nowMs - at) / 86_400_000));
}
```

And extend the two markers:

```typescript
export function markBackupSkipped(ownerId: string): void {
  writeFlag('local', SKIPPED, ownerId);
  writeStamp(SKIPPED_AT, ownerId, Date.now());
}

export function markRecoveryBackedUp(ownerId: string): void {
  writeFlag('local', BACKED_UP, ownerId);
  writeStamp(BACKED_UP_AT, ownerId, Date.now());
}
```

- [ ] **Step 4: Verify pass**

Run: `npx vitest run src/lib/onboarding-backup-flags.test.ts && npx tsc --noEmit`
Expected: all tests PASS (existing + new), tsc clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/onboarding-backup-flags.ts src/lib/onboarding-backup-flags.test.ts
git commit -m "ZEB-650: owner-scoped backup timestamps beside the ZEB-587 booleans"
```

---

### Task 2: BackupReminderBanner "skipped N days ago" copy

**Files:**
- Modify: `src/lib/components/BackupReminderBanner.svelte`
- Test: `src/lib/components/__tests__/BackupReminderBanner.test.ts`

**Interfaces:**
- Consumes: `daysSinceBackupSkipped(ownerId)` from Task 1.

- [ ] **Step 1: Write the failing tests** — append to the test file (the file
  already defines `OWNER`, `skippedKey(id)`, and clears storage in
  `beforeEach`):

```typescript
describe('BackupReminderBanner day count (ZEB-650 slice 1)', () => {
  const skippedAtKey = (id: string) => `harmony.onboarding.backupSkippedAt:owner-${id}`;

  it('shows "skipped N days ago" when the stamp is old enough', () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    localStorage.setItem(skippedAtKey(OWNER), String(Date.now() - 3 * 86_400_000 - 60_000));
    const { getByTestId } = render(BackupReminderBanner, { props: { ownerId: OWNER } });
    expect(getByTestId('backup-reminder-days').textContent).toContain('3 days ago');
  });

  it('uses singular copy for exactly 1 day', () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    localStorage.setItem(skippedAtKey(OWNER), String(Date.now() - 86_400_000 - 60_000));
    const { getByTestId } = render(BackupReminderBanner, { props: { ownerId: OWNER } });
    expect(getByTestId('backup-reminder-days').textContent).toContain('1 day ago');
    expect(getByTestId('backup-reminder-days').textContent).not.toContain('1 days');
  });

  it('renders no day count on the skip day (0 days)', () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    localStorage.setItem(skippedAtKey(OWNER), String(Date.now()));
    const { queryByTestId } = render(BackupReminderBanner, { props: { ownerId: OWNER } });
    expect(queryByTestId('backup-reminder-days')).toBeNull();
  });

  it('renders no day count for a legacy owner without a stamp', () => {
    localStorage.setItem(skippedKey(OWNER), 'true');
    const { queryByTestId, getByTestId } = render(BackupReminderBanner, { props: { ownerId: OWNER } });
    expect(queryByTestId('backup-reminder-days')).toBeNull();
    // Base copy unchanged.
    expect(getByTestId('backup-reminder-banner').textContent).toContain("hasn't been backed up");
  });
});
```

- [ ] **Step 2: Verify failure**

Run: `npx vitest run src/lib/components/__tests__/BackupReminderBanner.test.ts`
Expected: FAIL — `backup-reminder-days` testid not found in the first two tests.

- [ ] **Step 3: Implement** — in `BackupReminderBanner.svelte`:

Add `daysSinceBackupSkipped` to the existing flags import, then below the
`visible` derived:

```typescript
// ZEB-650: day count derives from the owner-scoped skippedAt stamp; null
// (legacy owner) or 0 (skipped today) keeps the base copy unchanged.
const skippedDays = $derived(ownerId ? daysSinceBackupSkipped(ownerId) : null);
```

In the template, extend the `.warn` span:

```svelte
<span class="warn"><span class="icon" aria-hidden="true">🔑</span> Your identity hasn't been backed up.{#if skippedDays !== null && skippedDays >= 1}<span data-testid="backup-reminder-days"> You skipped backup {skippedDays} {skippedDays === 1 ? 'day' : 'days'} ago.</span>{/if}</span>
```

- [ ] **Step 4: Verify pass**

Run: `npx vitest run src/lib/components/__tests__/BackupReminderBanner.test.ts && npx tsc --noEmit`
Expected: all PASS (existing visibility/backup tests untouched), tsc clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/BackupReminderBanner.svelte src/lib/components/__tests__/BackupReminderBanner.test.ts
git commit -m "ZEB-650: honest 'skipped N days ago' figure on the backup reminder"
```

---

### Task 3: DevicesPanel meta row, last-backed-up line, backed-up gap fix

**Files:**
- Create: `src/lib/owner-meta.ts`
- Create: `src/lib/owner-meta.test.ts`
- Modify: `src/lib/components/DevicesPanel.svelte`
- Test: `src/lib/components/__tests__/DevicesPanel.test.ts`

**Interfaces:**
- Consumes: `markRecoveryBackedUp`, `recoveryBackedUpAtMs` (Task 1);
  `OwnerStateView.devices[].enrolledAt` (Unix **seconds**);
  existing IPC `list_owner_communities`.
- Produces: `fetchCommunitiesCount(): Promise<number | null>` in
  `src/lib/owner-meta.ts`.

**Why the seam module:** `DevicesPanel.test.ts` stubs the global `invoke` with
*ordered* `mockResolvedValueOnce` chains. A second mount-time `invoke` call
from the component would consume stubs meant for later calls and silently
break existing tests. A separate module mocked at the top of the test file
keeps every existing stub chain intact.

- [ ] **Step 1: Write `src/lib/owner-meta.ts`**

```typescript
/**
 * ZEB-650 slice 1 — derivable owner/identity meta facts.
 *
 * Kept OUT of DevicesPanel deliberately: its test file stubs the global
 * tauri `invoke` with ordered mockResolvedValueOnce chains, so any extra
 * component-level invoke call would consume stubs meant for later calls.
 * Mock this module as a unit there instead.
 */
import { invoke } from '@tauri-apps/api/core';

/** Number of communities this owner has persisted rows for, or null when the
 *  IPC fails or returns a non-array — callers omit the fact, never render 0. */
export async function fetchCommunitiesCount(): Promise<number | null> {
  try {
    const rows = await invoke<unknown[]>('list_owner_communities', {});
    return Array.isArray(rows) ? rows.length : null;
  } catch {
    return null;
  }
}
```

- [ ] **Step 2: Write `src/lib/owner-meta.test.ts` (failing first is moot for a
  new module — write test + module together, run once):**

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
import { invoke } from '@tauri-apps/api/core';
import { fetchCommunitiesCount } from './owner-meta';

const mockedInvoke = invoke as unknown as ReturnType<typeof vi.fn>;

describe('fetchCommunitiesCount', () => {
  beforeEach(() => vi.resetAllMocks());

  it('returns the row count', async () => {
    mockedInvoke.mockResolvedValueOnce([{}, {}, {}]);
    expect(await fetchCommunitiesCount()).toBe(3);
    expect(mockedInvoke).toHaveBeenCalledWith('list_owner_communities', {});
  });

  it('returns null on IPC failure', async () => {
    mockedInvoke.mockRejectedValueOnce(new Error('nope'));
    expect(await fetchCommunitiesCount()).toBeNull();
  });

  it('returns null on non-array payloads', async () => {
    mockedInvoke.mockResolvedValueOnce(undefined);
    expect(await fetchCommunitiesCount()).toBeNull();
  });
});
```

Run: `npx vitest run src/lib/owner-meta.test.ts` — Expected: PASS.

- [ ] **Step 3: Write the failing DevicesPanel tests** — at the top of
  `src/lib/components/__tests__/DevicesPanel.test.ts`, add the seam mock
  beside the existing `vi.mock` blocks:

```typescript
import { fetchCommunitiesCount } from '../../owner-meta';

vi.mock('../../owner-meta', () => ({
  fetchCommunitiesCount: vi.fn(),
}));

const mockedCommunitiesCount = fetchCommunitiesCount as unknown as ReturnType<typeof vi.fn>;
```

(The file-level `vi.resetAllMocks()` leaves it returning `undefined` for all
existing tests — the component treats that as "omit the fact".)

Append a new describe block. It needs a populated `OwnerStateView`; reuse the
file's existing populated-view fixture if one exists — otherwise use this one:

```typescript
const metaView = {
  ownerId: 'aaaa0000aaaa0000aaaa0000aaaa0000',
  ownerDisplayName: 'Jake',
  canBackUp: true,
  devices: [
    { deviceId: 'd1', displayName: 'Koya', isThisDevice: true, trustDecision: { kind: 'full', reason: null }, enrolledAt: 1_700_000_000, fingerprint: 'fp1', butlerPinned: false, deviceVkHex: 'vk1' },
    { deviceId: 'd2', displayName: 'Ildwyn', isThisDevice: false, trustDecision: { kind: 'full', reason: null }, enrolledAt: 1_600_000_000, fingerprint: 'fp2', butlerPinned: false, deviceVkHex: 'vk2' },
  ],
};

describe('DevicesPanel meta row + backup stamps (ZEB-650 slice 1)', () => {
  it('renders keytype, earliest enrollment date, and communities count', async () => {
    mockedInvoke.mockResolvedValueOnce(metaView);      // get_owner_state
    mockedCommunitiesCount.mockResolvedValue(4);
    render(DevicesPanel);
    const keytype = await screen.findByTestId('devices-meta-keytype');
    expect(keytype.textContent).toBe('ed25519');
    expect(screen.getByTestId('devices-meta-enrolled').textContent)
      .toContain(new Date(1_600_000_000 * 1000).toLocaleDateString()); // MIN of the two
    const communities = await screen.findByTestId('devices-meta-communities');
    expect(communities.textContent).toContain('4');
  });

  it('omits the communities fact when the count is unavailable', async () => {
    mockedInvoke.mockResolvedValueOnce(metaView);
    mockedCommunitiesCount.mockResolvedValue(null);
    render(DevicesPanel);
    await screen.findByTestId('devices-meta-keytype');
    expect(screen.queryByTestId('devices-meta-communities')).toBeNull();
  });

  it('shows last-backed-up only when a stamp exists', async () => {
    localStorage.setItem(
      `harmony.onboarding.recoveryBackedUpAt:owner-${metaView.ownerId}`,
      String(Date.UTC(2026, 0, 15)),
    );
    mockedInvoke.mockResolvedValueOnce(metaView);
    render(DevicesPanel);
    const line = await screen.findByTestId('devices-last-backed-up');
    expect(line.textContent).toContain(new Date(Date.UTC(2026, 0, 15)).toLocaleDateString());
    localStorage.clear();
  });

  it('commitBackup marks the owner backed up (gap fix)', async () => {
    localStorage.clear();
    mockedInvoke.mockResolvedValueOnce(metaView);          // get_owner_state (mount)
    render(DevicesPanel);
    const backupBtn = await screen.findByRole('button', { name: /back up owner identity/i });
    mockedInvoke.mockResolvedValueOnce('token-1');         // issue_owner_recovery_token
    await fireEvent.click(backupBtn);
    const pass = await screen.findByTestId('devices-backup-passphrase');
    await fireEvent.input(pass, { target: { value: 'a'.repeat(12) } });
    await fireEvent.input(screen.getByTestId('devices-backup-passphrase-confirm'), { target: { value: 'a'.repeat(12) } });
    mockedInvoke.mockResolvedValueOnce('path-token');      // request_export_save_path
    mockedInvoke.mockResolvedValueOnce({ identityHash: 'h', byteLen: 1, path: '/tmp/x.bin' }); // export
    await fireEvent.click(screen.getByTestId('devices-backup-save'));
    await screen.findByTestId('devices-backup-saved-path');
    expect(
      localStorage.getItem(`harmony.onboarding.recoveryArtifactBackedUp:owner-${metaView.ownerId}`),
    ).toBe('true');
    expect(
      localStorage.getItem(`harmony.onboarding.recoveryBackedUpAt:owner-${metaView.ownerId}`),
    ).not.toBeNull();
    localStorage.clear();
  });
});
```

**Fixture/testid caveat for the implementer:** before running, check the real
testids/labels used by the backup modal inputs and saved-path confirmation in
`DevicesPanel.svelte` (search `data-testid="devices-backup` around lines
490-545) and adjust the FOUR selectors in the gap-fix test to the actual pins
(passphrase, confirm, save button, saved-path). Do not rename any existing
testid. If a populated-view fixture already exists in the test file, prefer it
over `metaView` (keep the two-device / distinct-`enrolledAt` shape — add a
second device to the fixture inline in this describe block if needed).

- [ ] **Step 4: Verify failure**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: new describe FAILs (missing testids); all existing tests still PASS
(seam mock returns undefined → no behavior change).

- [ ] **Step 5: Implement in `DevicesPanel.svelte`**

Imports (extend existing blocks):

```typescript
import { fetchCommunitiesCount } from '../owner-meta';
import { markRecoveryBackedUp, recoveryBackedUpAtMs } from '../onboarding-backup-flags';
```

State + derived (below the existing `$state` declarations):

```typescript
// ZEB-650 meta facts. communitiesCount/lastBackedUpMs are plain $state (not
// $derived) because localStorage and the IPC aren't reactive — they're set on
// mount and updated at the exact mutation points below.
let communitiesCount = $state<number | null>(null);
let lastBackedUpMs = $state<number | null>(null);
const firstEnrolledMs = $derived(
  state !== null && state.devices.length > 0
    ? Math.min(...state.devices.map((d) => d.enrolledAt)) * 1000
    : null,
);
```

In `onMount`, after the existing `await svc.refresh()` try/catch resolves
(inside the `finally` is wrong — put it after the try/catch, guarded):

```typescript
if (svc.state !== null) {
  lastBackedUpMs = recoveryBackedUpAtMs(svc.state.ownerId);
  const n = await fetchCommunitiesCount();
  communitiesCount = typeof n === 'number' ? n : null;
}
```

In `commitBackup`: capture the owner at the top (first line of the function):

```typescript
// Capture before the awaits — `state` could change underneath us, and the
// backed-up flag must name the identity actually exported (ZEB-587 pattern).
const backupOwnerId = state?.ownerId ?? null;
```

…and at the success point, immediately after `backupSavedPath = info.path;`:

```typescript
// ZEB-650 gap fix: a Devices-panel backup now clears the reminder banner,
// exactly like BackupReminderBanner's own save path.
if (backupOwnerId) {
  markRecoveryBackedUp(backupOwnerId);
  lastBackedUpMs = recoveryBackedUpAtMs(backupOwnerId);
}
```

Template — inside `.owner-identity`, directly below the
`.owner-fingerprint` div:

```svelte
<div class="owner-meta" data-testid="devices-meta">
  <span class="meta-key" data-testid="devices-meta-keytype">ed25519</span>
  {#if firstEnrolledMs !== null}
    <span data-testid="devices-meta-enrolled">First device enrolled {new Date(firstEnrolledMs).toLocaleDateString()}</span>
  {/if}
  {#if communitiesCount !== null}
    <span data-testid="devices-meta-communities">{communitiesCount} {communitiesCount === 1 ? 'community' : 'communities'}</span>
  {/if}
</div>
{#if lastBackedUpMs !== null}
  <div class="owner-meta" data-testid="devices-last-backed-up">Last backed up {new Date(lastBackedUpMs).toLocaleDateString()}</div>
{/if}
```

Styles (token-only; mirror the ss-badge/mono grammar already in the file):

```css
.owner-meta {
  display: flex;
  gap: 0.75rem;
  align-items: center;
  font-size: 0.75rem;
  color: var(--text-secondary);
  margin-top: 0.25rem;
}
.owner-meta .meta-key {
  font-family: var(--font-mono);
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  font-size: 0.65rem;
  padding: 0.05rem 0.4rem;
  border: 1px solid var(--border-default);
  border-radius: 999px;
}
```

(If the file's existing `.ss-badge` rule uses different token names for the
pill border/text, copy those exact tokens instead — grammar consistency wins.)

- [ ] **Step 6: Verify pass**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts src/lib/owner-meta.test.ts && npx tsc --noEmit`
Expected: ALL pass, including every pre-existing DevicesPanel test.

- [ ] **Step 7: Commit**

```bash
git add src/lib/owner-meta.ts src/lib/owner-meta.test.ts src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "ZEB-650: DevicesPanel meta row + last-backed-up line; Devices backup now clears the reminder"
```

---

### Task 4: NamePromptModal identicon chip

**Files:**
- Modify: `src/lib/components/NamePromptModal.svelte`
- Modify: `src/App.svelte:4263-4267` (prop pass)
- Test: `src/lib/components/__tests__/NamePromptModal.test.ts`

**Interfaces:**
- Consumes: `Avatar.svelte` (`address: string`, `displayName?: string`,
  `size?: number` → identicon when no `avatarUrl`); App's `selfOwnerId`
  (`$state<string | null>`, assigned at `App.svelte:979`).
- Produces: new optional prop `ownerIdHex?: string | null` (default `null`).

- [ ] **Step 1: Write the failing tests** — append:

```typescript
describe('NamePromptModal identicon chip (ZEB-650 slice 1)', () => {
  const OWNER = 'aaaa0000aaaa0000aaaa0000aaaa0000';

  it('renders the chip with identicon + typed name when ownerIdHex is set', async () => {
    render(NamePromptModal, { props: { open: true, ownerIdHex: OWNER, onSave: vi.fn(), onSkip: vi.fn() } });
    const chip = screen.getByTestId('name-prompt-chip');
    expect(chip.querySelector('svg')).toBeTruthy(); // identicon renders inline SVG
    expect(chip.textContent).toContain('Anonymous'); // empty input falls back
    await fireEvent.input(screen.getByTestId('name-prompt-input'), { target: { value: 'Jake' } });
    expect(screen.getByTestId('name-prompt-chip').textContent).toContain('Jake');
    expect(chip.textContent).toContain('self-sovereign');
  });

  it('renders no chip when ownerIdHex is null', () => {
    render(NamePromptModal, { props: { open: true, ownerIdHex: null, onSave: vi.fn(), onSkip: vi.fn() } });
    expect(screen.queryByTestId('name-prompt-chip')).toBeNull();
  });

  it('renders no chip when the prop is omitted (default)', () => {
    render(NamePromptModal, { props: { open: true, onSave: vi.fn(), onSkip: vi.fn() } });
    expect(screen.queryByTestId('name-prompt-chip')).toBeNull();
  });
});
```

- [ ] **Step 2: Verify failure**

Run: `npx vitest run src/lib/components/__tests__/NamePromptModal.test.ts`
Expected: FAIL — `name-prompt-chip` not found / unknown prop.

- [ ] **Step 3: Implement** — `NamePromptModal.svelte`:

```typescript
import Avatar from './Avatar.svelte';

interface Props {
  open: boolean;
  /** ZEB-650: owner id hex for the identicon chip; null pre-resolution. */
  ownerIdHex?: string | null;
  onSave: (name: string) => void | Promise<void>;
  onSkip: () => void;
}
const { open, ownerIdHex = null, onSave, onSkip }: Props = $props();

const chipName = $derived(name.trim() || 'Anonymous');
```

(Note: `name` is declared after the props — keep the `chipName` derived below
the existing `let name = $state('');` line.)

Template — between the `<input …>` and `<div class="actions">`:

```svelte
{#if ownerIdHex}
  <div class="profile-chip" data-testid="name-prompt-chip">
    <Avatar address={ownerIdHex} displayName={chipName} size={40} />
    <div class="chip-text">
      <span class="chip-name">{chipName}</span>
      <span class="chip-badge">● self-sovereign</span>
    </div>
  </div>
{/if}
```

Styles (token-only):

```css
.profile-chip {
  display: flex;
  align-items: center;
  gap: 0.6rem;
  padding: 0.5rem 0.6rem;
  border: 1px solid var(--border-default);
  border-radius: 8px;
  margin-bottom: 1rem;
  background: var(--bg-primary);
}
.chip-text { display: flex; flex-direction: column; gap: 0.1rem; }
.chip-name { font-weight: 600; font-size: 0.95rem; }
.chip-badge {
  font-family: var(--font-mono);
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  font-size: 0.6rem;
  color: var(--text-secondary);
}
```

`App.svelte:4263` — add the prop:

```svelte
<NamePromptModal
  open={showNamePrompt}
  ownerIdHex={selfOwnerId}
  onSave={handleNamePromptSave}
  onSkip={() => { showNamePrompt = false; }}
/>
```

- [ ] **Step 4: Verify pass**

Run: `npx vitest run src/lib/components/__tests__/NamePromptModal.test.ts && npx tsc --noEmit`
Expected: all PASS (existing tests use the omitted-prop default), tsc clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/NamePromptModal.svelte src/lib/components/__tests__/NamePromptModal.test.ts src/App.svelte
git commit -m "ZEB-650: identicon profile chip on the first-run name prompt"
```

---

### Task 5: ZEB-659 — Network Viz dev-flag gate + final gates

**Files:**
- Modify: `src/lib/components/NavPanel.svelte:14-53` (props), `:206`
  (`openNetworkWindow` guard), `:454-461` (button)
- Test: `src/lib/components/__tests__/NavPanel.test.ts`

**Interfaces:**
- Produces: new optional prop `showNetworkViz?: boolean`
  (default `import.meta.env.DEV`).

- [ ] **Step 1: Write the failing tests** — append (reuse the file's
  `testNodes` fixture and whatever minimal-props render pattern its existing
  tests use — match it exactly; the shape below assumes
  `{ nodes: testNodes, collapsed: false }` suffices):

```typescript
describe('Network Viz dev-flag gate (ZEB-659)', () => {
  it('hides the Network Viz button when showNetworkViz is false', () => {
    render(NavPanel, { props: { nodes: testNodes, collapsed: false, showNetworkViz: false } });
    expect(screen.queryByRole('button', { name: /open network visualization/i })).toBeNull();
  });

  it('shows the Network Viz button when showNetworkViz is true', () => {
    render(NavPanel, { props: { nodes: testNodes, collapsed: false, showNetworkViz: true } });
    expect(screen.getByRole('button', { name: /open network visualization/i })).toBeTruthy();
  });
});
```

- [ ] **Step 2: Verify failure**

Run: `npx vitest run src/lib/components/__tests__/NavPanel.test.ts`
Expected: the `showNetworkViz: false` test FAILs (button still renders).

- [ ] **Step 3: Implement** — `NavPanel.svelte`:

Destructure (add after `showConnectionStatus = false,`):

```typescript
    // ZEB-659: the network-viz window renders MockNetworkDataService's
    // fabricated topology — dev tool only until it has real data.
    showNetworkViz = import.meta.env.DEV,
```

Type block (after `showConnectionStatus?: boolean;`):

```typescript
    showNetworkViz?: boolean;
```

Guard in `openNetworkWindow` (first line):

```typescript
    if (!showNetworkViz) return;
```

Wrap the button (`:454-461`):

```svelte
      {#if showNetworkViz}
        <button
          type="button"
          class="nav-action-btn"
          aria-label="Open network visualization"
          onclick={openNetworkWindow}
        >
          Network Viz
        </button>
      {/if}
```

- [ ] **Step 4: Verify pass**

Run: `npx vitest run src/lib/components/__tests__/NavPanel.test.ts && npx tsc --noEmit`
Expected: all PASS (vitest runs with `DEV === true`, so existing tests that
touch the button keep passing via the default).

- [ ] **Step 5: Full frontend gate**

Run: `npx tsc --noEmit && npx vitest run`
Expected: clean tsc; full suite green (~3400+ tests).

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/NavPanel.svelte src/lib/components/__tests__/NavPanel.test.ts
git commit -m "ZEB-659: gate the mock-only Network Viz window behind import.meta.env.DEV"
```

---

## Finish

- [ ] Push branch; open PR titled
  "ZEB-650 slice 1 + ZEB-659: onboarding meta bundle — backup timestamps,
  DevicesPanel meta, name-prompt chip, Network-Viz dev gate" with
  "Closes ZEB-659" and "Part of ZEB-650" (ZEB-650 stays open for slices 2-3 —
  do NOT write "Closes ZEB-650").
- [ ] Fire `@coderabbitai review` once at PR-open; converge all three comment
  buckets, one commit + one push per round; CI green AND bots converged →
  merge-ready ping.
