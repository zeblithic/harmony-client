# ZEB-336 Phase 1 — Profile / display-name model Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Separate the owner's canonical display name from a per-device label (fixing the bug where "rename this device" silently renames the owner), default the device label to the OS hostname, and add a skippable first-run name prompt — all on a branch that also carries the already-completed ZEB-333 / ZEB-337 / ZEB-334 work, for one consolidated PR.

**Architecture:** Frontend-only (Svelte 5 + Tauri v2). A new `device-label-service` (localStorage, mirrors `profile-service`) holds the owner-private per-device label; `DevicesPanel` stops overlaying `profile.displayName` onto the device row and reads the label store instead; a new isolated `NamePromptModal` is shown by `App` after onboarding. The Phase-2 feed device-hint is explicitly out of scope (see spec).

**Tech Stack:** Svelte 5 runes, TypeScript, `@tauri-apps/plugin-os` (`hostname()`), vitest + @testing-library/svelte, Tauri capabilities.

**Spec:** `docs/specs/2026-06-02-zeb-336-profile-display-name-model-design.md`

---

## File structure

**New files:**
- `src/lib/device-label-service.ts` — per-device label store (load/save + hostname default). One responsibility: persistence of the owner-private device label.
- `src/lib/device-label-service.test.ts` — unit tests for the store.
- `src/lib/components/NamePromptModal.svelte` — skippable first-run "what should we call you?" prompt. One responsibility: capture a display name, hand it to a parent callback.
- `src/lib/components/__tests__/NamePromptModal.test.ts` — component tests.

**Modified files:**
- `src-tauri/capabilities/default.json` — add `os:allow-hostname` (the `os:default` set excludes hostname).
- `src/lib/components/DevicesPanel.svelte` — split owner name / device label; rename writes the label, not the profile; hostname default.
- `src/lib/components/__tests__/DevicesPanel.test.ts` — update the 2 tests that encode the conflation; add 2 tests for the split + hostname default.
- `src/App.svelte` — render `NamePromptModal`; trigger it from `onMinted`; `handleNamePromptSave`.
- `src/lib/profile-service.ts` — clarify in comments that `displayName` is the owner's canonical name and `address` is a local placeholder id.

**Already-complete work carried on this branch (committed in Task 0, not re-implemented):** ZEB-337 (`message-service.ts`), ZEB-333+334 (`NavPanel.svelte`), ZEB-334 (`notes-service.ts`, `NotesView.svelte`, their tests, `App.svelte` wiring), and the two design specs.

---

## Task 0: Branch + baseline commits of the already-done work

**Files:** (no new code — staging only)
- Revert build-run artifacts: `src-tauri/Cargo.toml`, `src-tauri/gen/schemas/desktop-schema.json`, `src-tauri/gen/schemas/windows-schema.json`
- Commit existing: `src/lib/message-service.ts(+test)`, `src/lib/components/NavPanel.svelte`, `src/lib/notes-service.ts(+test)`, `src/lib/components/NotesView.svelte(+test)`, `src/App.svelte`, `docs/specs/2026-06-02-zeb-334-*.md`, `docs/specs/2026-06-02-zeb-336-*.md`

- [ ] **Step 1: Confirm the build artifacts are non-substantive, then revert them**

Run:
```bash
git diff --stat src-tauri/Cargo.toml src-tauri/gen/schemas/desktop-schema.json src-tauri/gen/schemas/windows-schema.json
git checkout -- src-tauri/Cargo.toml src-tauri/gen/schemas/desktop-schema.json src-tauri/gen/schemas/windows-schema.json
```
Expected: the diff is EOL/whitespace/tauri-version regeneration only (no intentional change); after checkout, `git status` no longer lists them. The legitimate capability change in Task 2 will regenerate `gen/schemas/capabilities.json` cleanly.

- [ ] **Step 2: Create the feature branch**

Run:
```bash
git checkout -b zeb-336-display-name-model
```
Expected: `Switched to a new branch 'zeb-336-display-name-model'`.

- [ ] **Step 3: Commit ZEB-337**

```bash
git add src/lib/message-service.ts src/lib/message-service.test.ts
git commit -m "ZEB-337: self-messages render the configured display name, not 'You'"
```

- [ ] **Step 4: Commit ZEB-333 + ZEB-334 nav**

```bash
git add src/lib/components/NavPanel.svelte
git commit -m "ZEB-333 + ZEB-334: grid nav (fixes 7-button overflow) + pinned Notes row"
```

- [ ] **Step 5: Commit ZEB-334 self-notes**

```bash
git add src/lib/notes-service.ts src/lib/notes-service.test.ts \
        src/lib/components/NotesView.svelte src/lib/components/__tests__/NotesView.test.ts \
        src/App.svelte
git commit -m "ZEB-334: local self-notes default (service + view + nav/App wiring)"
```

- [ ] **Step 6: Commit the design specs**

```bash
git add docs/specs/2026-06-02-zeb-334-self-notes-design.md \
        docs/specs/2026-06-02-zeb-336-profile-display-name-model-design.md \
        docs/plans/2026-06-02-zeb-336-profile-display-name-model.md
git commit -m "docs: ZEB-334 + ZEB-336 design specs and ZEB-336 plan"
```

> Note: leave `docs/plans/2026-05-19-zeb-147-mint-sync-floor-propagation-plan.md` untracked — it is a Mint plan from another effort, not part of this PR.

---

## Task 1: `device-label-service` (the per-device label store)

**Files:**
- Create: `src/lib/device-label-service.ts`
- Test: `src/lib/device-label-service.test.ts`

- [ ] **Step 1: Write the failing tests**

Create `src/lib/device-label-service.test.ts`:
```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';

// Mock plugin-os so resolveDefaultDeviceLabel doesn't hit a real Tauri runtime.
// (The service uses a dynamic import; vi.mock intercepts both static + dynamic.)
vi.mock('@tauri-apps/plugin-os', () => ({ hostname: vi.fn() }));
import { hostname } from '@tauri-apps/plugin-os';
import {
  loadDeviceLabel,
  saveDeviceLabel,
  resolveDefaultDeviceLabel,
} from './device-label-service';

const mockedHostname = hostname as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
  localStorage.clear();
  vi.resetAllMocks();
});

describe('device-label-service — ZEB-336', () => {
  it('round-trips a saved label', () => {
    saveDeviceLabel('KRILE');
    expect(loadDeviceLabel()).toBe('KRILE');
  });

  it('returns null when no label is stored', () => {
    expect(loadDeviceLabel()).toBeNull();
  });

  it('ignores empty / whitespace-only labels', () => {
    saveDeviceLabel('   ');
    expect(loadDeviceLabel()).toBeNull();
  });

  it('trims the label on save', () => {
    saveDeviceLabel('  KOYA  ');
    expect(loadDeviceLabel()).toBe('KOYA');
  });

  it('defaults to the OS hostname when available', async () => {
    mockedHostname.mockResolvedValue('KOYA-MBP');
    expect(await resolveDefaultDeviceLabel()).toBe('KOYA-MBP');
  });

  it('falls back to "This device" when hostname is null', async () => {
    mockedHostname.mockResolvedValue(null);
    expect(await resolveDefaultDeviceLabel()).toBe('This device');
  });

  it('falls back to "This device" when hostname() rejects', async () => {
    mockedHostname.mockRejectedValue(new Error('os:allow-hostname not granted'));
    expect(await resolveDefaultDeviceLabel()).toBe('This device');
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `npx vitest run src/lib/device-label-service.test.ts`
Expected: FAIL — `Cannot find module './device-label-service'`.

- [ ] **Step 3: Write the minimal implementation**

Create `src/lib/device-label-service.ts`:
```typescript
/**
 * ZEB-336 — per-device label store (owner-private).
 *
 * Distinct from the owner display name (which lives in `profile-service` and
 * is broadcast owner-canonically by `profile_card_broadcast`). A device label
 * names THIS machine ("KRILE") and, in Phase 1, never leaves the device.
 * Mirrors `profile-service`'s localStorage pattern. The persistence interface
 * is deliberately thin so Phase 2 can sync the roster across the owner's own
 * devices without touching callers.
 */

const STORAGE_KEY = 'harmony-device-label';

/** The stored per-device label, or null if none has been set/defaulted yet. */
export function loadDeviceLabel(): string | null {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    return v && v.trim() ? v : null;
  } catch {
    return null;
  }
}

/** Persist the per-device label. No-op for empty / whitespace-only input. */
export function saveDeviceLabel(label: string): void {
  const trimmed = label.trim();
  if (!trimmed) return;
  try {
    localStorage.setItem(STORAGE_KEY, trimmed);
  } catch {
    // localStorage unavailable (SSR / private-mode quota) — non-fatal.
  }
}

/**
 * A default label derived from the OS hostname, falling back to "This device"
 * when the hostname is unavailable (not in Tauri, plugin error, null result,
 * or `os:allow-hostname` not granted). Read-only: callers decide whether to
 * persist the result.
 */
export async function resolveDefaultDeviceLabel(): Promise<string> {
  try {
    const { hostname } = await import('@tauri-apps/plugin-os');
    const h = await hostname();
    if (h && h.trim()) return h.trim();
  } catch {
    // Not in Tauri, or os:allow-hostname not granted — use the fallback.
  }
  return 'This device';
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `npx vitest run src/lib/device-label-service.test.ts`
Expected: PASS (7 tests).

- [ ] **Step 5: Commit**

```bash
git add src/lib/device-label-service.ts src/lib/device-label-service.test.ts
git commit -m "ZEB-336: device-label-service — owner-private per-device label store"
```

---

## Task 2: Grant the `os:allow-hostname` capability

**Files:**
- Modify: `src-tauri/capabilities/default.json`

The `os:default` permission set grants everything *except* hostname. `resolveDefaultDeviceLabel()` calls `hostname()`, which is denied until we add the explicit permission.

- [ ] **Step 1: Read the current capabilities file**

Run: `cat src-tauri/capabilities/default.json`
Expected: a JSON object with a `"permissions"` array containing `"os:default"` (around line 12).

- [ ] **Step 2: Add the permission**

In `src-tauri/capabilities/default.json`, in the `"permissions"` array, add `"os:allow-hostname"` immediately after the existing `"os:default"` entry. Example (match the file's existing indentation and trailing-comma style):
```json
    "os:default",
    "os:allow-hostname",
```

- [ ] **Step 3: Verify it parses**

Run: `node -e "JSON.parse(require('fs').readFileSync('src-tauri/capabilities/default.json','utf8')); console.log('ok')"`
Expected: `ok`.

> The matching `gen/schemas/capabilities.json` regenerates on the next `tauri dev`/build (Task 7) and is committed there.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/capabilities/default.json
git commit -m "ZEB-336: grant os:allow-hostname (os:default excludes it) for device-label default"
```

---

## Task 3: Split owner name / device label in `DevicesPanel`

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte`
- Test: `src/lib/components/__tests__/DevicesPanel.test.ts`

This is the core fix. Today `applyLocalProfileOverlay` overlays one `profile.displayName` onto both the owner header and the this-device row, and `saveRename` writes a device rename into `profile.displayName` — so renaming the device renames the owner. We split them: owner header ← `profile.displayName`; this-device row ← the device-label store; rename ← `saveDeviceLabel`.

- [ ] **Step 1: Update the two existing tests that encode the conflation, and add two new tests**

In `src/lib/components/__tests__/DevicesPanel.test.ts`:

(a) After the existing `profile-service` mock block (around line 16), add a `device-label-service` mock:
```typescript
import {
  loadDeviceLabel,
  saveDeviceLabel,
  resolveDefaultDeviceLabel,
} from '../../device-label-service';

vi.mock('../../device-label-service', () => ({
  loadDeviceLabel: vi.fn(),
  saveDeviceLabel: vi.fn(),
  resolveDefaultDeviceLabel: vi.fn(),
}));
```

(b) Replace the test `'saving the rename calls profile-service.saveProfile'` (the whole `it(...)` block) with:
```typescript
  it('saving the rename persists the device label, not the owner profile', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22', displayName: 'KRILE', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue('KRILE');

    render(DevicesPanel);
    const renameBtn = await screen.findByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtn);
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: 'KRILE-prime' } });
    const saveBtn = screen.getByRole('button', { name: /save/i });
    await fireEvent.click(saveBtn);
    // ZEB-336: rename writes the per-device LABEL, never the owner profile.
    expect(saveDeviceLabel).toHaveBeenCalledWith('KRILE-prime');
    expect(saveProfile).not.toHaveBeenCalled();
  });
```

(c) Replace the entire describe block `'DevicesPanel — rename overlay survives refresh'` (the one asserting `matches.length).toBe(2)`) with:
```typescript
describe('DevicesPanel — owner name and device label are separated (ZEB-336)', () => {
  it('shows the owner name in the header and the device label in the row', async () => {
    // Backend returns placeholders for both; the local stores override them
    // INDEPENDENTLY — the owner name and device label are distinct values.
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1c8239b7dd809abcdef0123456789',
      ownerDisplayName: 'backend-placeholder',
      devices: [{
        deviceId: 'aa11bb22cc33dd44ee55ff6677889900',
        displayName: 'this device', // backend placeholder
        isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000,
        fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadProfile as ReturnType<typeof vi.fn>).mockReturnValue({ address: 'addr', displayName: 'zeblith' });
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue('KRILE');

    render(DevicesPanel);
    await screen.findByText('zeblith');           // owner header ← profile
    expect(screen.getByText('KRILE')).toBeInTheDocument();   // device row ← label store
    expect(screen.queryByText('backend-placeholder')).not.toBeInTheDocument();
  });

  it('renaming the device does not change the owner display name', async () => {
    // Regression guard for the conflation: pre-split this rename rewrote
    // profile.displayName (the owner name).
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'backend',
      devices: [{
        deviceId: 'aa11bb22', displayName: 'this device', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadProfile as ReturnType<typeof vi.fn>).mockReturnValue({ address: 'addr', displayName: 'zeblith' });
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue('KRILE');

    render(DevicesPanel);
    const renameBtn = await screen.findByRole('button', { name: /rename/i });
    await fireEvent.click(renameBtn);
    const input = screen.getByRole('textbox', { name: /device name/i });
    await fireEvent.input(input, { target: { value: 'KRILE-prime' } });
    await fireEvent.click(screen.getByRole('button', { name: /save/i }));
    expect(screen.getByText('KRILE-prime')).toBeInTheDocument(); // device row updated
    expect(screen.getByText('zeblith')).toBeInTheDocument();     // owner header unchanged
  });

  it('defaults the device label to the OS hostname when none is stored', async () => {
    mockedInvoke.mockResolvedValueOnce({
      ownerId: 'a4f1', ownerDisplayName: 'zeblith',
      devices: [{
        deviceId: 'aa11bb22', displayName: 'this device', isThisDevice: true,
        trustDecision: { kind: 'full', reason: null },
        enrolledAt: 1_700_000_000, fingerprint: 'aa11·bb22',
      }],
      canBackUp: true,
    });
    (loadDeviceLabel as ReturnType<typeof vi.fn>).mockReturnValue(null);
    (resolveDefaultDeviceLabel as ReturnType<typeof vi.fn>).mockResolvedValue('HOSTBOX');

    render(DevicesPanel);
    await screen.findByText('HOSTBOX');                 // resolved hostname shown
    expect(saveDeviceLabel).toHaveBeenCalledWith('HOSTBOX'); // persisted once
  });
});
```

- [ ] **Step 2: Run the DevicesPanel tests to verify the new/changed ones fail**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: FAIL — the new tests fail (component still couples the two; `saveDeviceLabel` never called) and the rewritten separation test fails (`KRILE` not found / `backend-placeholder` still present). The unchanged backup/a11y/pairing tests still pass.

- [ ] **Step 3: Implement the split in `DevicesPanel.svelte`**

(a) Update the imports near the top of the `<script>`. Change:
```svelte
  import { loadProfile, saveProfile } from '../profile-service';
```
to:
```svelte
  import { loadProfile } from '../profile-service';
  import { loadDeviceLabel, saveDeviceLabel, resolveDefaultDeviceLabel } from '../device-label-service';
```

(b) Replace the entire `applyLocalProfileOverlay` doc-comment + function (the block spanning roughly lines 22–53, from the `/**` above it through the closing `}`) with:
```svelte
  /**
   * ZEB-336: the owner header and the this-device row are DISTINCT names.
   *
   * - Owner header ← `profile.displayName` (the owner's canonical name, also
   *   broadcast owner-keyed by profile_card_broadcast).
   * - This-device row ← the per-device LABEL store (`device-label-service`),
   *   which is owner-private and never overlaid from the owner name.
   *
   * The backend has no access to localStorage, so it returns placeholders for
   * both; we overlay each from its own local store. Defensive: a missing store
   * value leaves the backend value in place rather than blanking the field.
   */
  function applyLocalOverlay(view: OwnerStateView | null): OwnerStateView | null {
    if (!view) return null;
    const ownerName = loadProfile()?.displayName;
    return {
      ...view,
      ...(ownerName ? { ownerDisplayName: ownerName } : {}),
      devices: view.devices.map((d) =>
        d.isThisDevice && deviceLabel ? { ...d, displayName: deviceLabel } : d,
      ),
    };
  }

  // Per-device label (owner-private). Seeded from the store; defaulted to the
  // OS hostname on first run in onMount.
  let deviceLabel = $state<string | null>(loadDeviceLabel());
```

(c) Update the `onChange` assignment (was `state = applyLocalProfileOverlay(svc.state)`):
```svelte
  svc.onChange = () => { state = applyLocalOverlay(svc.state); };
```

(d) Replace the `onMount(...)` block with one that defaults the device label:
```svelte
  onMount(async () => {
    try {
      await svc.refresh();
      // ZEB-336: if this device has no label yet, default it to the OS hostname
      // (persisted once so it's stable across restarts), then re-apply overlay.
      if (!deviceLabel) {
        const def = await resolveDefaultDeviceLabel();
        if (def) {
          saveDeviceLabel(def);
          deviceLabel = def;
          state = applyLocalOverlay(svc.state);
        }
      }
    } catch (e) {
      loadError = extractError(e);
    } finally {
      loading = false;
    }
  });
```

(e) Replace the `saveRename` function with the label-writing version (no profile write, no owner mutation):
```svelte
  function saveRename(deviceId: string) {
    const trimmed = renameDraft.trim();
    if (trimmed.length === 0) return;
    // ZEB-336: a device rename writes the per-device LABEL, never the owner
    // profile. (Pre-split this wrote profile.displayName, renaming the owner.)
    saveDeviceLabel(trimmed);
    deviceLabel = trimmed;
    if (state) {
      state = {
        ...state,
        devices: state.devices.map((d) =>
          d.deviceId === deviceId ? { ...d, displayName: trimmed } : d,
        ),
      };
    }
    renamingDeviceId = null;
  }
```

- [ ] **Step 4: Run the DevicesPanel tests to verify all pass**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: PASS (all describe blocks, including the unchanged backup/a11y/pairing ones).

- [ ] **Step 5: Type-check**

Run: `npx tsc --noEmit`
Expected: no errors. (Confirms `saveProfile` removal left no dangling reference and `deviceLabel` typing is sound.)

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "ZEB-336: split owner name from device label in DevicesPanel (fixes rename-renames-owner)"
```

---

## Task 4: `NamePromptModal` (skippable first-run name prompt)

**Files:**
- Create: `src/lib/components/NamePromptModal.svelte`
- Test: `src/lib/components/__tests__/NamePromptModal.test.ts`

- [ ] **Step 1: Write the failing tests**

Create `src/lib/components/__tests__/NamePromptModal.test.ts`:
```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import NamePromptModal from '../NamePromptModal.svelte';

describe('NamePromptModal — ZEB-336 first-run name', () => {
  it('renders the name input when open', () => {
    render(NamePromptModal, { props: { open: true, onSave: vi.fn(), onSkip: vi.fn() } });
    expect(screen.getByTestId('name-prompt-input')).toBeTruthy();
  });

  it('does not render when closed', () => {
    render(NamePromptModal, { props: { open: false, onSave: vi.fn(), onSkip: vi.fn() } });
    expect(screen.queryByTestId('name-prompt-input')).toBeNull();
  });

  it('Save calls onSave with the trimmed name', async () => {
    const onSave = vi.fn();
    render(NamePromptModal, { props: { open: true, onSave, onSkip: vi.fn() } });
    await fireEvent.input(screen.getByTestId('name-prompt-input'), { target: { value: '  Jake  ' } });
    await fireEvent.click(screen.getByTestId('name-prompt-save'));
    expect(onSave).toHaveBeenCalledWith('Jake');
  });

  it('disables Save when the name is empty / whitespace', async () => {
    render(NamePromptModal, { props: { open: true, onSave: vi.fn(), onSkip: vi.fn() } });
    await fireEvent.input(screen.getByTestId('name-prompt-input'), { target: { value: '   ' } });
    expect((screen.getByTestId('name-prompt-save') as HTMLButtonElement).disabled).toBe(true);
  });

  it('Skip calls onSkip and not onSave', async () => {
    const onSave = vi.fn();
    const onSkip = vi.fn();
    render(NamePromptModal, { props: { open: true, onSave, onSkip } });
    await fireEvent.click(screen.getByTestId('name-prompt-skip'));
    expect(onSkip).toHaveBeenCalled();
    expect(onSave).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/NamePromptModal.test.ts`
Expected: FAIL — `Cannot find module '../NamePromptModal.svelte'`.

- [ ] **Step 3: Write the component**

Create `src/lib/components/NamePromptModal.svelte`:
```svelte
<script lang="ts">
  /**
   * ZEB-336 — first-run "what should we call you?" prompt.
   *
   * Shown by App.svelte after onboarding (post-WelcomeModal) when the profile
   * display name is still the "Anonymous" default. This is NOT a hard gate —
   * it is skippable (Skip or Escape leaves "Anonymous", editable later in
   * Settings → Profile). Save hands the trimmed name to the parent, which owns
   * persistence + card re-seed + network publish.
   */
  import { trapFocus } from '../focus-trap';

  interface Props {
    open: boolean;
    onSave: (name: string) => void | Promise<void>;
    onSkip: () => void;
  }
  const { open, onSave, onSkip }: Props = $props();

  let name = $state('');
  let modalEl = $state<HTMLElement | null>(null);

  // Mirror WelcomeModal's focus trap so keyboard users stay within the dialog.
  $effect(() => {
    if (!open || modalEl === null) return;
    return trapFocus(modalEl);
  });

  function handleSave() {
    const trimmed = name.trim();
    if (!trimmed) return;
    void onSave(trimmed);
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter') { e.preventDefault(); handleSave(); }
    if (e.key === 'Escape') { e.preventDefault(); onSkip(); }
  }
</script>

{#if open}
  <div class="modal-backdrop" data-testid="name-prompt-backdrop" role="presentation">
    <div
      bind:this={modalEl}
      class="modal-content"
      data-testid="name-prompt-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="name-prompt-title"
      tabindex="-1"
      onkeydown={handleKeydown}
    >
      <h2 id="name-prompt-title">What should we call you?</h2>
      <p class="muted">
        This is the name people see on your messages. You can change it anytime
        in your profile.
      </p>
      <label for="name-prompt-input">Display name</label>
      <input
        id="name-prompt-input"
        data-testid="name-prompt-input"
        type="text"
        bind:value={name}
        placeholder="Anonymous"
        aria-label="Display name"
      />
      <div class="actions">
        <button
          class="primary"
          data-testid="name-prompt-save"
          onclick={handleSave}
          disabled={name.trim().length === 0}
        >
          Save
        </button>
        <button data-testid="name-prompt-skip" onclick={onSkip}>
          Skip for now
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--bg-secondary, #2a2a2a);
    color: var(--text-primary, #fff);
    padding: 1.5rem;
    border-radius: 8px;
    max-width: 460px;
    width: 90%;
  }
  .modal-content h2 { margin: 0 0 1rem; font-size: 1.25rem; }
  .muted { color: var(--text-secondary, #aaa); font-size: 0.9rem; margin: 0 0 1rem; line-height: 1.5; }
  label { display: block; margin-bottom: 0.4rem; font-size: 0.9rem; }
  input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.5rem;
    background: var(--bg-primary, #111);
    color: var(--text-primary, #fff);
    border: 1px solid var(--border, #444);
    border-radius: 4px;
    margin-bottom: 1rem;
  }
  .actions { display: flex; gap: 0.5rem; }
  .actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    border-radius: 4px;
    cursor: pointer;
  }
  .actions button.primary { background: var(--accent, #5865f2); border-color: var(--accent, #5865f2); }
  .actions button:disabled { opacity: 0.5; cursor: default; }
</style>
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/NamePromptModal.test.ts`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/NamePromptModal.svelte src/lib/components/__tests__/NamePromptModal.test.ts
git commit -m "ZEB-336: NamePromptModal — skippable first-run display-name prompt"
```

---

## Task 5: Wire `NamePromptModal` into `App.svelte`

**Files:**
- Modify: `src/App.svelte`

App is not unit-tested per-handler; this task is verified by `tsc` (Task 5 Step 4) and the live smoke (Task 7).

- [ ] **Step 1: Import the component**

After the existing `import WelcomeModal from './lib/components/WelcomeModal.svelte';` (around line 72), add:
```svelte
  import NamePromptModal from './lib/components/NamePromptModal.svelte';
```

- [ ] **Step 2: Add the open-state flag**

Near the existing `let showWelcomeModal = $state(false);` (around line 530), add:
```svelte
  // ZEB-336: first-run display-name prompt, shown after onboarding when the
  // profile name is still the "Anonymous" default.
  let showNamePrompt = $state(false);
```

- [ ] **Step 3: Trigger it from `onMinted`**

In the `onMinted` function, immediately after the `memberCardService.seedSelf({...})` call (after the closing `});` of seedSelf, before the `if (tauriAdapter)` block), add:
```svelte
    // ZEB-336: a freshly-minted owner has no name yet — prompt for one. Skippable.
    if (!myProfile.displayName || myProfile.displayName === 'Anonymous') {
      showNamePrompt = true;
    }
```

- [ ] **Step 4: Add the save handler**

Immediately after the `handleProfileSave` function (after its closing `}`, around line 389), add:
```svelte
  // ZEB-336: persist the first-run name through the normal profile-save path
  // (saves locally, re-seeds the self card, publishes to the network), then
  // close the prompt.
  async function handleNamePromptSave(name: string): Promise<void> {
    await handleProfileSave({ ...myProfile, displayName: name });
    showNamePrompt = false;
  }
```

- [ ] **Step 5: Render the modal**

After the existing `<WelcomeModal open={showWelcomeModal} {onMinted} />` (around line 2751), add:
```svelte
<NamePromptModal
  open={showNamePrompt}
  onSave={handleNamePromptSave}
  onSkip={() => { showNamePrompt = false; }}
/>
```

- [ ] **Step 6: Type-check**

Run: `npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 7: Commit**

```bash
git add src/App.svelte
git commit -m "ZEB-336: show first-run NamePromptModal after onboarding (App wiring)"
```

---

## Task 6: Clarify the owner-name / address seam in comments

**Files:**
- Modify: `src/lib/profile-service.ts`

Spec Phase-1 item 2 asks to make the seam explicit. The wire keying is already owner-correct (ZEB-341); this is a clarity-only change so future readers don't re-confuse the owner name with the per-device `address`.

- [ ] **Step 1: Update the `loadProfile` doc comment**

In `src/lib/profile-service.ts`, replace the `loadProfile` doc comment (lines 12–14, the `/** Load the local user's profile ... same address. */` block) with:
```typescript
/** Load the local user's profile from localStorage, or create a new one with
 *  a unique random address.
 *
 *  `displayName` is the OWNER's canonical name — it is broadcast owner-keyed by
 *  profile_card_broadcast and resolved by peers per ownerIdHex. It is NOT a
 *  per-device label (see device-label-service for that, ZEB-336).
 *
 *  `address` is a local placeholder id (random 16 bytes), NOT the owner
 *  identity (`owner_id`) and NOT a device key. It predates real key management
 *  and carries no identity-bearing role; treat it as a legacy local handle. */
```

- [ ] **Step 2: Type-check (comment-only change is a no-op for behavior)**

Run: `npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add src/lib/profile-service.ts
git commit -m "ZEB-336: clarify displayName is the owner's canonical name; address is a local placeholder"
```

---

## Task 7: Full gate + live smoke + capabilities regen

**Files:** none new (verification + possible `gen/schemas/capabilities.json` regen commit)

- [ ] **Step 1: Run the full frontend gate**

Run:
```bash
npx tsc --noEmit
npx vitest run
```
Expected: `tsc` clean; vitest all green (the prior suite + the new device-label-service (7), NamePromptModal (5), and updated DevicesPanel tests).

- [ ] **Step 2: Live smoke (headless identity launch — see memory `project_tauri_keychain_passphrase`)**

Launch with a throwaway passphrase identity so onboarding runs (agent-launched `tauri dev` can't reach Windows Credential Manager):
```powershell
$env:HARMONY_PASSPHRASE_FILE = "$PWD\.playwright-scratch\dev-passphrase.txt"
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=9222"
$env:RUST_MIN_STACK = "8388608"
npm run tauri dev
```
(Create `.playwright-scratch/dev-passphrase.txt` with any ≥1-char string first if absent. Delete `~/.harmony/identity.enc` first for a true first-run.)

Verify via the WebView2-CDP bridge (Playwright `connectOverCDP('http://localhost:9222')`; never `browser.close()` — `process.exit()` instead):
1. Onboard: `welcome-create-identity` → `welcome-skip-backup` → `welcome-skip-confirm`.
2. **`name-prompt-modal` appears** after onboarding. Type a name, click `name-prompt-save` — the feed/member surfaces show that name (not "Anonymous").
3. Open Settings → Devices. The device row shows a label defaulted to the **OS hostname** (this machine's name), and the owner header shows the name you just set — two different values.
4. Click **Rename** on the device, change it, Save. The device row updates; the **owner header name does NOT change** (the conflation fix).

Expected: all four behaviors hold. Capture a screenshot of the Devices panel showing distinct owner-name vs device-label.

- [ ] **Step 3: Commit the regenerated capabilities (if changed)**

Run: `git status --short src-tauri/gen/schemas/capabilities.json`
If the build regenerated it to include `os:allow-hostname`:
```bash
git add src-tauri/gen/schemas/capabilities.json
git commit -m "ZEB-336: regenerate capabilities schema with os:allow-hostname"
```
Expected: only the hostname permission added; no unrelated churn. (If other `gen/schemas/*` files changed from a local tauri-version mismatch, `git checkout` them — they are not part of this change.)

---

## Task 8: Open the consolidated PR

**Files:** none

- [ ] **Step 1: Push the branch**

Run:
```bash
git push -u origin zeb-336-display-name-model
```

- [ ] **Step 2: Create the PR**

Run (single-quoted heredoc so `$`/backticks stay literal):
```bash
gh pr create --title "ZEB-336 + ZEB-334/333/337: profile display-name model + self-notes + alpha first-impression fixes" --body @'
## Summary

Consolidated alpha first-impressions work, headlined by the **ZEB-336 Phase 1** display-name model.

### ZEB-336 — profile / display-name model (Phase 1)
- New `device-label-service`: an owner-private per-device label, defaulted to the OS hostname (adds the `os:allow-hostname` capability — `os:default` excludes it).
- `DevicesPanel` now keeps the **owner name** (`profile.displayName`) and the **device label** as distinct values. Fixes the bug where "rename this device" silently renamed the *owner*.
- New skippable first-run `NamePromptModal` shown after onboarding so testers set a real name instead of "Anonymous".
- Spec: `docs/specs/2026-06-02-zeb-336-profile-display-name-model-design.md`. The feed device-hint ("(on KRILE)") is deferred to Phase 2 (rides the owner-device roster sync; suppressed for single-device users).

### Also in this PR (already smoke-tested on Windows)
- **ZEB-337**: self-messages render the configured display name, not the literal "You".
- **ZEB-333**: nav switches to a grid so all 7 mode buttons fit (was overflowing, hiding Mint/Network).
- **ZEB-334**: local self-notes replace the misleading empty `#general` as the zero-community default. Multi-device sync tracked in ZEB-361.

## Testing
- `npx tsc --noEmit` clean; `npx vitest run` green (incl. new device-label-service, NamePromptModal, and updated DevicesPanel tests — two of which flipped from asserting the old conflation).
- Live smoke on Windows: first-run name prompt appears; device label defaults to hostname; renaming the device does not change the owner name.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
'@
```
Expected: PR URL printed. Bot reviews (Cursor / CodeRabbit / Greptile / etc.) trigger automatically.

- [ ] **Step 3: Record the PR number for the review loop**

Note the PR number from the URL. Proceed to the review-monitoring loop (address bot rounds → re-request review → repeat until convergence → Pushover-notify the user that it's ready for final review).

---

## Self-review notes (author)

- **Spec coverage:** Phase-1 items map to tasks — separate name/label (Task 3), owner-name canonical + seam (Tasks 3, 6), first-run name (Tasks 4, 5), device-label field + hostname default (Tasks 1, 2, 3). Feed-hint correctly deferred per the spec's Phase 2.
- **Type consistency:** `loadDeviceLabel` / `saveDeviceLabel` / `resolveDefaultDeviceLabel` are referenced with identical signatures across Tasks 1, 3, and the DevicesPanel tests. `applyLocalProfileOverlay` → `applyLocalOverlay` rename is applied at its definition and its single call site (`svc.onChange`, `onMount`).
- **Regression guard:** the two DevicesPanel tests that encoded the conflation are rewritten (not deleted) so the new separation is asserted positively, and `saveProfile` is asserted *not* called on rename.
- **No dead plumbing:** the Phase-2-only feed hint is excluded, so no `Message.deviceId` plumbing lands unused.
