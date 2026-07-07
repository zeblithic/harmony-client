# Commons G — Onboarding & Identity/Backup Restyle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restyle the self-sovereign onboarding, identity, and backup surfaces to the Commons design system — honestly (real data only), frontend-only.

**Architecture:** One new shared primitive (`WizardProgress`) plus in-place restyles of the existing onboarding/identity components. No new data model, no backend, no service-layer changes — the Commons palette already lives in `src/app.css` token *values*, so this track is typography (`--font-display` headings), layout (card anatomy), idiom (pip rail, badges, clay containment), and one literal tokenization. Every touched component keeps its existing markup contract (testids, accessible names, aria ids) byte-identical.

**Tech Stack:** Svelte 5 (runes: `$props/$state/$derived`), TypeScript, Vitest + @testing-library/svelte. CSS custom properties (Commons tokens in `src/app.css`).

## Global Constraints

- Frontend gates (run from repo root): `npx tsc --noEmit && npx vitest run`. Both must pass before every commit.
- **Budget-0 tokens:** zero raw hex/rgb/hsl/named colors in `<style>` blocks. Allowed: `var(--token)`, `color-mix(in srgb, var(--token) N%, <other>)`, `transparent`. Guard: `src/style-token-guard.test.ts` (per-file count vs `src/style-token-allowlist.json`) + `src/commons-hex-guard.test.ts` (8 forbidden Discord hex must appear nowhere). The allowlist ratchets **down only** — Task 5 removes its one `BackupReminderBanner` entry via `UPDATE_STYLE_TOKEN_ALLOWLIST=1`.
- **Clay containment:** clay tokens (`--gov-clay`, `--gov-clay-soft`, `--gov-clay-deep`) appear ONLY in `WelcomeModal` stage `backup` (+ its pip) and `BackupReminderBanner`. Every other surface uses sage/primary.
- **Preserve byte-identical** (any restyle that changes these fails a test): all `data-testid` values, accessible names/labels, aria ids, the `·` middot in fingerprints, SAS triplet-whitespace, and the `WelcomeModal` redaction invariant (`container.innerHTML` must never contain a `[0-9a-f]{32,}` run).
- Svelte 5 runes throughout. One PR, commit per task, branch off latest `origin/main` (`zeb-610-commons-g-onboarding-identity` @ `beb036f5`), no worktrees.
- **Read-only seams — DO NOT modify:** `src/lib/owner-gate.ts`, `owner-service.ts`, `pairing-service.ts`, `owner-restore-logic.ts`, `recovery-policy.ts`, `onboarding-backup-flags.ts`, `types/onboarding.ts`; `Modal.svelte`, `HarmonyMark.svelte`, focus-trap actions; all `src-tauri/**`; every IPC contract; `owner-gate.test.ts`.
- Spec: `docs/specs/2026-07-06-zeb-610-commons-g-onboarding-identity-design.md` (the honesty ledger §0 is binding — do not render dropped/deferred elements).

---

### Task 1: `WizardProgress.svelte` primitive

**Files:**
- Create: `src/lib/components/WizardProgress.svelte`
- Test: `src/lib/components/__tests__/WizardProgress.test.ts`

**Interfaces:**
- Consumes: nothing (leaf primitive).
- Produces: a component with props
  `{ steps: { label: string; accent: 'sage' | 'clay' }[]; activeIndex: number; showCounter?: boolean }`
  (`showCounter` defaults `true`). Task 2 mounts it as
  `<WizardProgress steps={WIZARD_STEPS} activeIndex={i} showCounter={stage !== 'explain'} />`
  where `WIZARD_STEPS = [{label:'Welcome',accent:'sage'},{label:'Create',accent:'sage'},{label:'Back up',accent:'clay'}]`.
  Renders the pips as `<li class="wizard-progress-pip" [is-active] [accent-clay]>` and, when `showCounter`, a `.wizard-progress-counter` reading `Step {activeIndex+1} of {steps.length}`.

- [ ] **Step 1: Write the failing test**

`src/lib/components/__tests__/WizardProgress.test.ts`:

```ts
import { render } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import WizardProgress from '../WizardProgress.svelte';

const steps = [
  { label: 'Welcome', accent: 'sage' as const },
  { label: 'Create', accent: 'sage' as const },
  { label: 'Back up', accent: 'clay' as const },
];

describe('WizardProgress', () => {
  it('renders one pip per step', () => {
    const { container } = render(WizardProgress, { props: { steps, activeIndex: 0 } });
    expect(container.querySelectorAll('.wizard-progress-pip')).toHaveLength(3);
  });

  it('marks only the active step pip active', () => {
    const { container } = render(WizardProgress, { props: { steps, activeIndex: 1 } });
    const pips = container.querySelectorAll('.wizard-progress-pip');
    expect(pips[1].classList.contains('is-active')).toBe(true);
    expect(pips[0].classList.contains('is-active')).toBe(false);
    expect(pips[2].classList.contains('is-active')).toBe(false);
  });

  it('applies the clay accent class on a clay step', () => {
    const { container } = render(WizardProgress, { props: { steps, activeIndex: 2 } });
    const pips = container.querySelectorAll('.wizard-progress-pip');
    expect(pips[2].classList.contains('accent-clay')).toBe(true);
  });

  it('shows the step counter by default', () => {
    const { queryByText } = render(WizardProgress, { props: { steps, activeIndex: 1 } });
    expect(queryByText('Step 2 of 3')).toBeTruthy();
  });

  it('hides the counter when showCounter is false', () => {
    const { queryByText } = render(WizardProgress, {
      props: { steps, activeIndex: 0, showCounter: false },
    });
    expect(queryByText(/Step \d of \d/)).toBeNull();
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/WizardProgress.test.ts`
Expected: FAIL — "Failed to resolve import '../WizardProgress.svelte'".

- [ ] **Step 3: Write the component**

`src/lib/components/WizardProgress.svelte`:

```svelte
<script lang="ts">
  let {
    steps,
    activeIndex,
    showCounter = true,
  }: {
    steps: { label: string; accent: 'sage' | 'clay' }[];
    activeIndex: number;
    showCounter?: boolean;
  } = $props();
</script>

<div class="wizard-progress" data-testid="wizard-progress">
  {#if showCounter}
    <span class="wizard-progress-counter">Step {activeIndex + 1} of {steps.length}</span>
  {/if}
  <ol class="wizard-progress-pips" aria-hidden="true">
    {#each steps as step, i (i)}
      <li
        class="wizard-progress-pip"
        class:is-active={i === activeIndex}
        class:accent-clay={step.accent === 'clay'}
      ></li>
    {/each}
  </ol>
</div>

<style>
  .wizard-progress {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 10px;
  }
  .wizard-progress-counter {
    font-family: var(--font-mono);
    font-size: 0.69rem;
    font-weight: 600;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--text-muted);
  }
  .wizard-progress-pips {
    display: flex;
    align-items: center;
    gap: 7px;
    list-style: none;
    padding: 0;
    margin: 0;
  }
  .wizard-progress-pip {
    width: 6px;
    height: 6px;
    border-radius: 3px;
    background: var(--faint);
    transition: width 0.2s ease, background 0.2s ease;
  }
  .wizard-progress-pip.is-active {
    width: 24px;
    background: var(--accent);
  }
  .wizard-progress-pip.is-active.accent-clay {
    background: var(--gov-clay);
  }
</style>
```

- [ ] **Step 4: Run tests + gates**

Run: `npx vitest run src/lib/components/__tests__/WizardProgress.test.ts && npx tsc --noEmit && npx vitest run src/style-token-guard.test.ts src/commons-hex-guard.test.ts`
Expected: all PASS (WizardProgress is not allowlisted → its `<style>` must count 0 raw literals; it does).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/WizardProgress.svelte src/lib/components/__tests__/WizardProgress.test.ts
git commit -m "feat(zeb-610): WizardProgress pip-rail primitive"
```

---

### Task 2: `WelcomeModal` steps 1–3 restyle

**Files:**
- Modify: `src/lib/components/WelcomeModal.svelte` (405 lines; stages `explain`/`minting`/`backup`/`skip-confirm`)
- Test: `src/lib/components/__tests__/WelcomeModal.test.ts` (extend; do not weaken existing assertions)

**Interfaces:**
- Consumes: `WizardProgress` (Task 1) — mount with `WIZARD_STEPS = [{label:'Welcome',accent:'sage'},{label:'Create',accent:'sage'},{label:'Back up',accent:'clay'}]`, `activeIndex` = 0 for `explain`, 1 for `minting`, 2 for `backup`; `showCounter={currentStage !== 'explain'}`. `HarmonyMark` (existing, read-only).
- Produces: nothing consumed downstream.

**Preserve byte-identical (existing `WelcomeModal.test.ts` pins):** testids `welcome-modal`, `welcome-modal-backdrop`, `welcome-create-identity`, `welcome-join-existing`, `welcome-restore-mnemonic`, `welcome-save-backup`, `welcome-skip-backup`, `welcome-skip-confirm`, `welcome-backup-passphrase`, `welcome-mint-error`, `welcome-already-exists-reload`; the mint-error copy `/couldn.t create your identity/i` with raw detail inside `<details>`; the **redaction invariant** (no `[0-9a-f]{32,}` run in `innerHTML`); the hard-gate behavior (no Escape/backdrop dismiss — do NOT convert to shared `Modal`).

- [ ] **Step 1: Write the failing test (new assertions only)**

Add to `src/lib/components/__tests__/WelcomeModal.test.ts` a describe block `Commons chrome (ZEB-610)`. Use the file's existing mount/helper pattern (read it first for the render harness + how it advances to the `backup` stage — typically render, click `welcome-create-identity`, resolve the mocked `mint`). Assertions:

```ts
// The wizard progress rail is present on the welcome stage.
it('renders the wizard pip rail on the welcome stage', () => {
  const { getByTestId } = renderWelcome(); // existing helper
  expect(getByTestId('wizard-progress')).toBeTruthy();
});

// The backup (Step 3) stage shows the real encrypted-file passphrase field,
// NOT a recovery-phrase word grid (honesty ledger §0.1).
it('backup stage offers the encrypted-file passphrase, not a phrase grid', async () => {
  const { getByTestId, queryByText } = await advanceToBackupStage(); // existing/local helper
  expect(getByTestId('welcome-backup-passphrase')).toBeTruthy();
  // No 12/24-word mnemonic grid is rendered.
  expect(queryByText(/recovery phrase · 12 words/i)).toBeNull();
});

// Redaction invariant still holds after restyle.
it('never leaks a 32+ hex run in the DOM after restyle', async () => {
  const { container } = await advanceToBackupStage();
  expect(container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
});
```

If the file lacks an `advanceToBackupStage` helper, write a local one mirroring the existing backup-stage tests (they already reach `welcome-backup-passphrase`).

- [ ] **Step 2: Run test to verify the new ones fail (rail) / pass-vacuously (guards)**

Run: `npx vitest run src/lib/components/__tests__/WelcomeModal.test.ts`
Expected: the `wizard-progress` assertion FAILS (rail not yet mounted); the redaction/no-grid assertions may already pass (there is no grid today) — that is fine, they are regression guards for the restyle.

- [ ] **Step 3: Restyle the component**

Read `src/lib/components/WelcomeModal.svelte` in full first. Apply, preserving all pins above:

1. **Import + mount the pip rail.** `import WizardProgress from './WizardProgress.svelte';` Define `const WIZARD_STEPS = [{ label: 'Welcome', accent: 'sage' as const }, { label: 'Create', accent: 'sage' as const }, { label: 'Back up', accent: 'clay' as const }];` and `const wizardIndex = $derived(stage === 'backup' ? 2 : stage === 'minting' ? 1 : 0);`. Render `<WizardProgress steps={WIZARD_STEPS} activeIndex={wizardIndex} showCounter={stage !== 'explain'} />` near the bottom of the `explain`/`minting`/`backup` stage bodies (not on `skip-confirm`/`joining`/`restore`).
2. **Card chrome + typography.** Headings (welcome wordmark, "Create your identity", "Save your recovery kit") → `font-family: var(--font-display)`. Body/labels/buttons stay `--font-ui`. The modal card uses `--paper`/`--surface-raised` bg, `--border-default`, `--shadow-e3`, radius ~10px. Do NOT add faux window traffic-lights.
3. **Stage `explain` (sage):** `<HarmonyMark>` + wordmark; sage-italic tagline; calm body; sage-filled "Get started"; keep the existing join/restore affordances (`welcome-join-existing`, `welcome-restore-mnemonic`) restyled as sage links/buttons.
4. **Stage `minting` (sage):** `--font-display` heading; honest body (keypair minted locally; no email/password). **Do not** add a handle input or a fabricated `did:harmony:` string (honesty ledger §0.2/§0.3). Sage-filled CTA — keep its existing accessible text/testid.
5. **Stage `backup` (Step 3 — the ONE clay stage):** a `--gov-clay-soft` warning callout with a `color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised))` border, `--gov-clay-deep` text, 🔑 — "you hold the only copy…". The real encrypted-file flow: the `welcome-backup-passphrase` input (sage focus ring: `border-color: var(--accent); box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 12%, transparent)`), the `welcome-save-backup` action, a sage keychain-stored note, and `welcome-skip-backup`. Continue/primary buttons stay **sage** (clay is only the callout + the pip). **No phrase grid.** If (and only if) a middot fingerprint helper already exists and you surface the owner fingerprint post-mint, use that `xxxx·xxxx` format — never the raw 32-hex `ownerId` (would break the redaction invariant); otherwise omit it.
6. **Tokenize the stray literal:** replace `.error { color: crimson }` with `color: var(--fg-error)` (or `var(--danger)` if `--fg-error` is absent — check `src/app.css`).
7. Keep `skip-confirm` copy byte-identical; restyle only its chrome (sage buttons, clay only if it echoes the backup warning — prefer sage).

- [ ] **Step 4: Run tests + gates**

Run: `npx vitest run src/lib/components/__tests__/WelcomeModal.test.ts && npx tsc --noEmit && npx vitest run src/style-token-guard.test.ts src/commons-hex-guard.test.ts`
Expected: all PASS. `WelcomeModal` is not allowlisted → its `<style>` must count 0 raw literals (the `crimson` tokenization + no new hex ensures this).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/WelcomeModal.svelte src/lib/components/__tests__/WelcomeModal.test.ts
git commit -m "feat(zeb-610): Commons welcome wizard (steps 1-3) + pip rail"
```

---

### Task 3: Redeem + Done + startup-error Commons pass

**Files:**
- Modify: `src/lib/components/RedeemInviteDialog.svelte`, `src/lib/components/NamePromptModal.svelte`, `src/App.svelte` (startup-error overlay ~3850-3881 + `.redeem-status-banner` styles ~3971-3994)
- Test: extend `src/lib/components/__tests__/RedeemInviteDialog.test.ts` and `NamePromptModal.test.ts` (whichever exist — read them first to enumerate their pins)

**Interfaces:** none new. Preserve every `data-testid`, role, and copy string those test files pin (read them; do not guess). App startup-error overlay: preserve `role="alertdialog"`, testids `startup-error-backdrop`/`-modal`/`-detail`/`-retry`, headline **"Couldn't start Harmony"**.

- [ ] **Step 1: Write the failing test (new assertions only)**

For `RedeemInviteDialog.test.ts`, add (adapting to its existing render harness + how it injects a resolved invite preview):

```ts
// Honesty ledger §0.4: no fabricated member/channel counts on the preview.
it('does not render invented member or channel counts', async () => {
  const { queryByText } = await renderResolvedInvite(); // existing/local helper
  expect(queryByText(/\d+\s+members/i)).toBeNull();
  expect(queryByText(/\d+\s+channels/i)).toBeNull();
});
```

If `RedeemInviteDialog` has no test file, create `src/lib/components/__tests__/RedeemInviteDialog.test.ts` with a minimal render that mounts the resolved-preview state and asserts the community name renders and the counts do not. For `NamePromptModal`, add one assertion that its primary/skip testids still resolve after restyle (regression guard).

- [ ] **Step 2: Run to verify fail/guard**

Run: `npx vitest run src/lib/components/__tests__/RedeemInviteDialog.test.ts src/lib/components/__tests__/NamePromptModal.test.ts`
Expected: the counts-absent assertion passes only after Step 3 removes any counts (fails now if the current code shows them; if the current code never showed counts, it is a regression guard and passes).

- [ ] **Step 3: Restyle**

1. **`RedeemInviteDialog`:** resolved-community preview → Commons card (`--surface-raised`, `--border`, radius 10, `--shadow-e1`): sage avatar chip, community name in `--font-display`, a sage **"✓ signature verified"** inviter tick, inviter display name via a nullable fallback (`nonEmpty(name) ?? 'an unknown member'` idiom from `src/lib/display-label.ts`). **Remove** any member/channel count markup. Sage-filled join CTA + muted "later" skip. Keep all redeem testids + the `redeem_invite` wiring untouched.
2. **`NamePromptModal`:** Commons success/profile chip + display-name input; `--font-display` heading; preserve every testid + the Enter-scoped-to-input behavior; copy byte-identical.
3. **App startup-error overlay:** Commons pass on the hand-rolled `.modal-overlay`/`.modal-content` (sage/neutral, `--font-display` headline) — preserve `role="alertdialog"`, the four testids, and the headline text. Restyle `.redeem-status-banner` to a sage/`--primary-soft` tint. No raw hex (`App.svelte` `<style>` is not allowlisted → budget 0).

- [ ] **Step 4: Run tests + gates**

Run: `npx vitest run src/lib/components/__tests__/RedeemInviteDialog.test.ts src/lib/components/__tests__/NamePromptModal.test.ts && npx tsc --noEmit && npx vitest run src/style-token-guard.test.ts src/commons-hex-guard.test.ts`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/RedeemInviteDialog.svelte src/lib/components/NamePromptModal.svelte src/App.svelte src/lib/components/__tests__/RedeemInviteDialog.test.ts src/lib/components/__tests__/NamePromptModal.test.ts
git commit -m "feat(zeb-610): Commons redeem preview, name prompt, startup-error"
```

---

### Task 4: `DevicesPanel` Commons restyle

**Files:**
- Modify: `src/lib/components/DevicesPanel.svelte` (768 lines)
- Test: extend `src/lib/components/__tests__/DevicesPanel.test.ts` (1033 lines — the heaviest pin set; do not weaken)

**Interfaces:** none new. Restyle the three REAL sections only (owner header, device rows, add-device footer) + hosted backup/mint modals. Add **no** rotation/revoke/danger-zone chrome (honesty ledger §0.5).

**Preserve byte-identical:** `/bind this device/i`, `/join existing identity/i`; mint modal `/will create your owner identity/i`, confirm `/^create owner identity/i` (anchored — label must START "Create owner identity"), `/cancel/i`; `getByText('zeblith')`, fingerprint `/a4f1·c823/i` (**`·` middot**), `/back up owner identity/i`; `/this device/i`, `/trusted/i`, `/aa11·bb22/i`; `/add another device/i`; rename `/rename/i` + textbox aria-label `/device name/i` + Save/Cancel (asserts `saveDeviceLabel`, not `saveProfile`); backup modal `role="dialog"`, inputs **"Passphrase"**/**"Confirm passphrase"**/**"Comment"**, `/save backup/i`, errors `/do not match/i` `/at most 256 bytes/i` `/at least 12 characters/i`, success echoes `ExportInfo.path` verbatim; butler checkbox name `/set {name} as always-on butler/i` → `invoke('set_butler_pin', { deviceId: vkHex })`.

- [ ] **Step 1: Write the failing test (new assertion only)**

Add to `DevicesPanel.test.ts` (adapt to its existing populated-state harness):

```ts
// Honesty ledger §0.5: the self-sovereign badge is honest (always true for the
// owner). Rotation / revoke / danger-zone must NOT be invented.
it('shows a self-sovereign badge but no rotation/revoke/danger chrome', async () => {
  const { getByText, queryByText } = await renderPopulated(); // existing helper
  expect(getByText(/self-sovereign/i)).toBeTruthy();
  expect(queryByText(/rotate keys/i)).toBeNull();
  expect(queryByText(/^revoke$/i)).toBeNull();
  expect(queryByText(/delete this identity/i)).toBeNull();
});
```

- [ ] **Step 2: Run to verify fail**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts`
Expected: FAIL on `self-sovereign` (badge not yet added); the negative assertions pass (chrome never existed).

- [ ] **Step 3: Restyle**

Read `src/lib/components/DevicesPanel.svelte` in full first. Apply:

1. **Owner header → Commons card:** `--surface-raised` + `--border`, radius 10, `--shadow-e1`; sage avatar; owner name in `--font-display`; add a **"● self-sovereign"** badge — a `<span>` mono 600 uppercase, `color: var(--primary-deep); background: var(--primary-soft); padding: 2px 8px; border-radius: 20px` (RoleBadge grammar). Keep the existing name text (`getByText('zeblith')`) and the `·`-middot fingerprint (`/a4f1·c823/i`) untouched. `ed25519` + membership count as mono meta if already rendered; do not fabricate a creation date.
2. **Device rows → cards:** `--surface-raised`, hairline `--line-soft`; keep trust badge (`/trusted/i`), mono enrolled-date + fingerprint (`/aa11·bb22/i`), rename control (aria-label "Device name"), butler-pin checkbox (name template unchanged). "this device" → sage pill. **Remap** the cool trust-blue device-icon chip (`#e8eef0`) to `background: var(--bg-tertiary)` (trust-blue rejection).
3. **Footer:** "＋ Add another device" as a sage button (accessible name unchanged).
4. **Hosted backup/mint modals** (`Modal`-based): Commons pass — `--font-display` `h3`, sage-filled confirm; keep the anchored `/^create owner identity/i` label and all backup-modal input labels/errors byte-identical.
5. No raw hex anywhere (`DevicesPanel` not allowlisted → budget 0).

- [ ] **Step 4: Run tests + gates**

Run: `npx vitest run src/lib/components/__tests__/DevicesPanel.test.ts && npx tsc --noEmit && npx vitest run src/style-token-guard.test.ts src/commons-hex-guard.test.ts`
Expected: all PASS (full existing pin set + the new badge assertion).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/DevicesPanel.svelte src/lib/components/__tests__/DevicesPanel.test.ts
git commit -m "feat(zeb-610): Commons DevicesPanel (identity header, devices, backup)"
```

---

### Task 5: `BackupReminderBanner` clay restyle + allowlist ratchet-down

**Files:**
- Modify: `src/lib/components/BackupReminderBanner.svelte` (170 lines), `src/style-token-allowlist.json`
- Test: extend `src/lib/components/__tests__/BackupReminderBanner.test.ts`

**Interfaces:** none new. Preserve `role="status"` + testids `backup-reminder-banner`/`-backup-now`/`-dismiss`/`-passphrase`/`-save`/`-error`; the owner-scoped visibility matrix; dismiss → sessionStorage key; save → localStorage `recoveryArtifactBackedUp` key.

- [ ] **Step 1: Write the failing test (new assertion) + capture guard baseline**

Add to `BackupReminderBanner.test.ts`:

```ts
// After tokenization the amber literal is gone; the clay banner still renders.
it('renders the clay backup banner with its action + dismiss', () => {
  const { getByTestId } = renderVisibleBanner(); // existing helper (owner with skipped backup)
  expect(getByTestId('backup-reminder-banner')).toBeTruthy();
  expect(getByTestId('backup-reminder-backup-now')).toBeTruthy();
  expect(getByTestId('backup-reminder-dismiss')).toBeTruthy();
});
```

- [ ] **Step 2: Run to verify current state green**

Run: `npx vitest run src/lib/components/__tests__/BackupReminderBanner.test.ts src/style-token-guard.test.ts`
Expected: PASS at the current allowlist count (1 for this file).

- [ ] **Step 3: Restyle + tokenize + ratchet down**

1. Replace `background: #4a3a1a` (line ~147) with a Commons clay fill: `background: var(--gov-clay-soft);` and set the strip border to `1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised))`, title/body in `var(--gov-clay-deep)`, the 🔑 icon, a **clay-filled** `--gov-clay` "Back up now" (text `--text-bright`), and a clay "✕" dismiss.
2. Replace `.error { color: crimson }` with `color: var(--fg-error)` (or `var(--danger)` if absent).
3. Keep copy honest: retain the real **days-since** figure; if any sub-copy asserts "you've joined a new community since then," soften to the days-only claim (§0.6). No test pins the copy.
4. **Remove the allowlist entry** for `BackupReminderBanner.svelte`: regenerate with `UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts` (the file now counts 0 raw literals, so its entry drops out). Verify `git diff src/style-token-allowlist.json` shows only the removal of the `BackupReminderBanner` entry (count 1 → gone) — no additions.

- [ ] **Step 4: Run tests + gates**

Run: `npx vitest run src/lib/components/__tests__/BackupReminderBanner.test.ts && npx tsc --noEmit && npx vitest run src/style-token-guard.test.ts src/commons-hex-guard.test.ts`
Expected: all PASS with the reduced allowlist.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/BackupReminderBanner.svelte src/style-token-allowlist.json src/lib/components/__tests__/BackupReminderBanner.test.ts
git commit -m "feat(zeb-610): Commons clay backup banner + tokenize amber literal"
```

---

### Task 6: Pairing + Restore Commons chrome

**Files:**
- Modify: `src/lib/components/PairingInviter.svelte`, `PairingJoiner.svelte`, `OwnerRestoreWizard.svelte`
- Test: extend the three existing `__tests__` files (do not weaken)

**Interfaces:** none new. Restyle the `Modal`-based bodies; do not restructure the SAS/restore flows.

**Preserve byte-identical:** `PairingInviter` — `invoke('start_inviter_pairing', {displayName})`, SAS regex `/987\s*654|987654/` (keep digits as text, only whitespace between triplets), peer `displayName` verbatim, `/cancel/i`→`cancel_pairing`, `aria-labelledby="invite-heading"`. `PairingJoiner` — label **"Give this device a name"** (`/give this device a name/i`), `/start pairing/i`→`start_joiner_pairing`, SAS `/012\s*845|012845/`, `aria-labelledby="join-heading"`, complete copy `/this device is now part of the owner identity/i`, Escape routing (terminal `complete`→`onComplete` with no `cancel_pairing`; non-terminal→`onClose`+`cancel_pairing`). `OwnerRestoreWizard` — testids `owner-restore-words`/`-continue`/`-confirm`/`-typed-confirm`/`-error`, 24-word gate, `preview_owner_mnemonic_identity`, same-owner typed-prefix (8-char) → `force:true`, `restore_owner_mnemonic_from_words {words, force}`, `/different identity/i`.

- [ ] **Step 1: Write the failing test (regression guards)**

Add one assertion to each existing test file that a restyled visual class is present without disturbing pinned behavior — e.g. in `PairingInviter.test.ts`:

```ts
it('renders the SAS in a mono display block', () => {
  const { container } = renderHandshaking(); // existing helper reaching the sasDigits state
  expect(container.querySelector('.sas-display')).toBeTruthy();
});
```

Mirror lightly for `PairingJoiner` (`.sas-display`) and `OwnerRestoreWizard` (the `owner-restore-words` textarea still resolves). Keep these guards minimal — the pinned behavior tests already carry the weight.

- [ ] **Step 2: Run to verify current state**

Run: `npx vitest run src/lib/components/__tests__/PairingInviter.test.ts src/lib/components/__tests__/PairingJoiner.test.ts src/lib/components/__tests__/OwnerRestoreWizard.test.ts`
Expected: PASS (the `.sas-display` class already exists; these are regression guards for the restyle).

- [ ] **Step 3: Restyle**

1. **`PairingInviter` / `PairingJoiner`:** `--font-display` `h3` headings (keep `id="invite-heading"`/`id="join-heading"`); the `.sas-display` block styled as a Commons mono card (`--surface-raised`/`--bg-tertiary`, `--border`, radius, letter-spaced `--font-mono`, sage accent); sage-filled primary + outline cancel; peer rows as sage-bordered selectable cards. Do not alter the digit rendering (keep whitespace-only between triplets).
2. **`OwnerRestoreWizard`:** `--font-display` heading; the 24-word textarea + `{wordCount}/24` counter as a Commons field; the same-owner typed-prefix confirm styled as a sage input with a clay-free warning (this is not the Step-3 backup — keep it sage/`--danger` per existing danger idiom, not `--gov-clay`); preserve all testids + the `/different identity/i` phrasing.
3. No raw hex (none of the three are allowlisted → budget 0). Keep the existing `color-mix(... transparent)` idioms.

- [ ] **Step 4: Run tests + gates (full sweep)**

Run: `npx tsc --noEmit && npx vitest run`
Expected: entire frontend suite PASS (all files), including `style-token-guard` (allowlist now smaller) and `commons-hex-guard`.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/PairingInviter.svelte src/lib/components/PairingJoiner.svelte src/lib/components/OwnerRestoreWizard.svelte src/lib/components/__tests__/PairingInviter.test.ts src/lib/components/__tests__/PairingJoiner.test.ts src/lib/components/__tests__/OwnerRestoreWizard.test.ts
git commit -m "feat(zeb-610): Commons chrome for pairing + owner-restore"
```

---

## Post-tasks

- Whole-branch review on the most capable model (SDD final review).
- Open the PR; trigger CodeRabbit immediately (`@coderabbitai review`) in parallel with Qodo + CI.
- File the §6 follow-up ticket: "Owner recovery phrase in the GUI — export IPC + Step-3 phrase display + phrase-restore parity" (parent ZEB-603).
