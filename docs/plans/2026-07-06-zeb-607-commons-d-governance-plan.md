# ZEB-607 Commons D — Governance Surfaces Restyle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the Commons vocabulary (status pills, tally anatomy, count chips, confirm modals, doc-column, signed-vote toasts) across all governance surfaces, reconciled with the real voting models per spec `docs/specs/2026-07-06-zeb-607-commons-d-governance-design.md`.

**Architecture:** A small set of shared primitives under `src/lib/components/governance/` becomes the restyle spine; each existing surface then migrates to those primitives + Commons tokens without changing its behavior contracts (event subscriptions, race guards, optimistic casts). One net-new UI state: the proposals panel's doc-column ballot detail, composed from real DTO fields only.

**Tech Stack:** Svelte 5 (runes), TypeScript, vitest + @testing-library/svelte, CSS custom properties (all tokens already in `src/app.css` from ZEB-605).

## Global Constraints

- Frontend gates from repo root: `npx tsc --noEmit && npx vitest run` (~3160 tests). Run per task.
- `style-token-guard` forbids raw hex in Svelte `<style>` blocks. The allowlist (`src/style-token-allowlist.json`) ratchets DOWN only: after REMOVING a listed literal, regenerate via `UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run` and verify the diff shows only removals. Never add new raw color literals.
- `src/lib/components/__tests__/AssemblyRail.test.ts` and `MessagesRail.test.ts` MUST pass UNCHANGED (ZEB-606 contracts). In particular `getByText('Devin Ross')` requires the card's override-pill copy to keep `<strong>{delegateName}</strong>`, and `getByText('No open proposals')` / `getByText('View all proposals →')` must keep rendering.
- `setupDelegateOnBehalfToast` / `formatDelegateOnBehalfMessage` / the local `shortAddr` in `src/lib/voting-toast-wiring.ts` are copy-LOCKED by ZEB-298 Task 10 — do not modify or migrate them.
- Behavior contracts untouched: ZEB-319 event-driven Tier3ProposalPanel (no polling), load-token/generation race guards, optimistic signal handling, `hideText`-style prop additions must be optional with defaults so existing mounts compile unchanged.
- Design tokens: use ONLY existing tokens (verified present in both theme blocks): `--gov-clay --gov-clay-soft --gov-clay-deep --vote-for --vote-against --vote-abstain --tally-track --status-{drafting,open,passed,failed,recalled}-{fg,bg} --gov-purple --sortition-bg --primary-deep --primary-soft --primary-border --danger --danger-deep --danger-border-muted --warning --paper --surface-raised --border --line-soft --muted --faint --text-muted --overlay --panel-bg --input-bg --accent --text-bright --text-primary --text-secondary --font-mono --font-display --shadow-e1 --shadow-e2` plus the four `--verdict-*` aliases Task 1 adds.
- KEEP `--gov-purple`/`--sortition-bg` on sortition/encryption surfaces (spec D4).
- Motion: tally fills `transition: width .35s ease`; no other animation added.
- Commit per task; no worktrees; branch `zeb-607-commons-d-governance`.

**Spec amendments locked here (supersede the spec's wording):**
1. The `--verdict-*` aliases are defined ONCE in `:root` — `var()` references resolve at use time against the cascaded theme values, so a dark-theme block would be redundant.
2. The conviction card's proxied footer action is **"Vote directly"** (the real per-proposal override verb, test-pinned). There is NO card-level Recall — community-scoped recall lives only in DelegationWidget (spec §0.6 governs over the D5 anatomy line).

---

### Task 1: Shared governance primitives, verdict aliases, signed-vote toasts

**Files:**
- Create: `src/lib/short-addr.ts`
- Create: `src/lib/components/governance/StatusPill.svelte`
- Create: `src/lib/components/governance/TallyBar.svelte`
- Create: `src/lib/components/governance/CountChip.svelte`
- Create: `src/lib/components/governance/GovConfirmModal.svelte`
- Modify: `src/app.css` (add 4 alias tokens after `--status-recalled-bg` in the `:root` block, ~line 51)
- Modify: `src/lib/voting-toast-wiring.ts` (append 3 exported helpers; touch nothing existing)
- Test: `src/lib/__tests__/short-addr.test.ts`, `src/lib/components/governance/__tests__/governance-primitives.test.ts`, `src/lib/__tests__/signed-vote-toasts.test.ts`

**Interfaces (Produces — later tasks import these exactly):**
- `shortAddr(hex: string): string` — `'aabbccdd…ff11'` form (first 8 + '…' + last 4) when `hex.length > 16`, else unchanged.
- `shortId(hex: string): string` — `'aabbccdd…'` form (first 8 + '…') when `hex.length > 8`, else unchanged.
- `StatusPill` props: `{ variant: 'drafting'|'open'|'passing'|'failing'|'passed'|'failed'|'archived'|'recalled'; label?: string; ariaLabel?: string }`.
- `TallyBar` props: `{ segments: Array<{ pct: number; token: string }>; height?: number; label?: string }` — `token` is a CSS custom-property name like `'--vote-for'`.
- `CountChip` props: `{ label: string; value: string; tone?: 'sage'|'clay'|'neutral' }`.
- `GovConfirmModal` props: `{ title: string; confirmLabel?: string; cancelLabel?: string; severity?: 'click'|'typed'; typedMatch?: string; busy?: boolean; onConfirm: () => void; onCancel: () => void; children?: Snippet }`.
- `showSignalCastToast(support: boolean): void`, `showDelegationToast(delegateName: string): void`, `showRecallToast(): void` from `voting-toast-wiring.ts`.

- [ ] **Step 1: Write failing tests**

`src/lib/__tests__/short-addr.test.ts`:
```typescript
import { describe, it, expect } from 'vitest';
import { shortAddr, shortId } from '../short-addr';

describe('short-addr', () => {
  it('shortAddr renders first-8…last-4 for long hex', () => {
    expect(shortAddr('ab'.repeat(16))).toBe('abababab…abab');
  });
  it('shortAddr passes short strings through', () => {
    expect(shortAddr('abcd1234')).toBe('abcd1234');
  });
  it('shortId renders first-8… for long hex', () => {
    expect(shortId('cd'.repeat(32))).toBe('cdcdcdcd…');
  });
  it('shortId passes 8-char strings through', () => {
    expect(shortId('deadbeef')).toBe('deadbeef');
  });
});
```

`src/lib/components/governance/__tests__/governance-primitives.test.ts`:
```typescript
import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StatusPill from '../StatusPill.svelte';
import TallyBar from '../TallyBar.svelte';
import CountChip from '../CountChip.svelte';
import GovConfirmModal from '../GovConfirmModal.svelte';

describe('StatusPill', () => {
  it('renders the default label per variant with the variant class', () => {
    const { container } = render(StatusPill, { props: { variant: 'open' } });
    const pill = container.querySelector('.status-pill.open');
    expect(pill?.textContent).toBe('● Open');
  });
  it('label prop overrides the default and ariaLabel lands on the pill', () => {
    render(StatusPill, {
      props: { variant: 'passing', label: 'Threshold reached', ariaLabel: 'Lifecycle' },
    });
    const pill = screen.getByLabelText('Lifecycle');
    expect(pill.textContent).toBe('Threshold reached');
    expect(pill.classList.contains('passing')).toBe(true);
  });
});

describe('TallyBar', () => {
  it('renders one fill per segment with clamped width and token background', () => {
    const { container } = render(TallyBar, {
      props: {
        segments: [
          { pct: 68, token: '--vote-for' },
          { pct: 140, token: '--vote-against' },
        ],
        label: 'Live tally',
      },
    });
    const fills = container.querySelectorAll('.tally-fill');
    expect(fills.length).toBe(2);
    expect((fills[0] as HTMLElement).style.width).toBe('68%');
    expect((fills[1] as HTMLElement).style.width).toBe('100%'); // clamped
    expect((fills[0] as HTMLElement).style.background).toContain('--vote-for');
    expect(screen.getByLabelText('Live tally')).toBeTruthy();
  });
});

describe('CountChip', () => {
  it('renders label + value with the tone class', () => {
    const { container } = render(CountChip, {
      props: { label: 'Threshold', value: '68% reached', tone: 'sage' },
    });
    expect(container.querySelector('.count-chip.sage')).toBeTruthy();
    expect(screen.getByText('Threshold')).toBeTruthy();
    expect(screen.getByText('68% reached')).toBeTruthy();
  });
});

describe('GovConfirmModal', () => {
  it('click severity: confirm fires immediately', async () => {
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    render(GovConfirmModal, {
      props: { title: 'Confirm thing', confirmLabel: 'Do it', onConfirm, onCancel },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Do it' }));
    expect(onConfirm).toHaveBeenCalledTimes(1);
    await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });
  it('typed severity: confirm disabled until the match string is typed', async () => {
    const onConfirm = vi.fn();
    render(GovConfirmModal, {
      props: {
        title: 'Confirm revoke',
        confirmLabel: 'Confirm revoke',
        severity: 'typed',
        typedMatch: 'revoke',
        onConfirm,
        onCancel: vi.fn(),
      },
    });
    const confirmBtn = screen.getByRole('button', { name: 'Confirm revoke' });
    expect((confirmBtn as HTMLButtonElement).disabled).toBe(true);
    const input = screen.getByLabelText('Type the word revoke to confirm');
    await fireEvent.input(input, { target: { value: '  ReVoKe ' } });
    expect((confirmBtn as HTMLButtonElement).disabled).toBe(false);
    await fireEvent.click(confirmBtn);
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });
  it('busy disables both buttons', () => {
    render(GovConfirmModal, {
      props: { title: 'T', busy: true, onConfirm: vi.fn(), onCancel: vi.fn() },
    });
    expect((screen.getByRole('button', { name: 'Confirm' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: 'Cancel' }) as HTMLButtonElement).disabled).toBe(true);
  });
});
```

`src/lib/__tests__/signed-vote-toasts.test.ts`:
```typescript
import { describe, it, expect, beforeEach } from 'vitest';
import { get } from 'svelte/store';
import { toastStore, toastsStore } from '../stores/toast';
import {
  showSignalCastToast,
  showDelegationToast,
  showRecallToast,
} from '../voting-toast-wiring';

describe('signed-vote toasts (ZEB-607 D6)', () => {
  beforeEach(() => {
    for (const t of get(toastsStore)) toastStore.dismiss(t.id);
  });
  it('support cast', () => {
    showSignalCastToast(true);
    const toasts = get(toastsStore);
    expect(toasts[toasts.length - 1].message).toBe('✓ Support signaled · signed with your key');
    expect(toasts[toasts.length - 1].durationMs).toBe(2100);
  });
  it('support withdrawn', () => {
    showSignalCastToast(false);
    expect(get(toastsStore).at(-1)?.message).toBe('✓ Support withdrawn · signed with your key');
  });
  it('delegation', () => {
    showDelegationToast('Heating WG');
    expect(get(toastsStore).at(-1)?.message).toBe('↪ Proxied to Heating WG');
  });
  it('recall', () => {
    showRecallToast();
    expect(get(toastsStore).at(-1)?.message).toBe(
      '↩ Delegation recalled — your vote is yours again',
    );
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/lib/__tests__/short-addr.test.ts src/lib/components/governance/__tests__/governance-primitives.test.ts src/lib/__tests__/signed-vote-toasts.test.ts`
Expected: FAIL (modules not found / exports missing).

- [ ] **Step 3: Implement**

`src/lib/short-addr.ts`:
```typescript
/**
 * ZEB-607 — shared hex-address abbreviation. Two forms:
 *   shortAddr: first 8 + '…' + last 4 (roster/proposer rows — the
 *              SortitionRevealView/DraftingPanel convention)
 *   shortId:   first 8 + '…' (ID pills, author chips)
 *
 * NOTE: `voting-toast-wiring.ts` keeps its own local shortAddr — its
 * message format is locked by ZEB-298 Task 10. Do not migrate it here.
 */
export function shortAddr(hex: string): string {
  return hex.length > 16 ? `${hex.slice(0, 8)}…${hex.slice(-4)}` : hex;
}

export function shortId(hex: string): string {
  return hex.length > 8 ? `${hex.slice(0, 8)}…` : hex;
}
```

`src/lib/components/governance/StatusPill.svelte`:
```svelte
<script lang="ts" module>
  export type StatusPillVariant =
    | 'drafting'
    | 'open'
    | 'passing'
    | 'failing'
    | 'passed'
    | 'failed'
    | 'archived'
    | 'recalled';

  const DEFAULT_LABELS: Record<StatusPillVariant, string> = {
    drafting: 'Drafting',
    open: '● Open',
    passing: 'Passing',
    failing: 'Failing',
    passed: '✓ Passed',
    failed: '✕ Failed',
    archived: 'Archived',
    recalled: 'Recalled',
  };
</script>

<script lang="ts">
  /**
   * ZEB-607 — Commons status pill (spec D3). Variant → token-pair
   * mapping is the single source of governance status colors; labels
   * default per variant and are overridable (e.g. lifecycle copy the
   * tests pin, or tier3StageLabel strings).
   */
  let {
    variant,
    label,
    ariaLabel,
  }: {
    variant: StatusPillVariant;
    label?: string;
    ariaLabel?: string;
  } = $props();
</script>

<span class="status-pill {variant}" aria-label={ariaLabel}>{label ?? DEFAULT_LABELS[variant]}</span>

<style>
  .status-pill {
    display: inline-block;
    font-weight: 600;
    font-size: 11px;
    line-height: 1.3;
    padding: 4px 11px;
    border-radius: 20px;
    white-space: nowrap;
  }
  .drafting,
  .archived {
    color: var(--status-drafting-fg);
    background: var(--status-drafting-bg);
  }
  .open {
    color: var(--status-open-fg);
    background: var(--status-open-bg);
  }
  .passing {
    color: var(--verdict-passing-fg);
    background: var(--verdict-passing-bg);
  }
  .failing {
    color: var(--verdict-failing-fg);
    background: var(--verdict-failing-bg);
  }
  .passed {
    color: var(--status-passed-fg);
    background: var(--status-passed-bg);
  }
  .failed {
    color: var(--status-failed-fg);
    background: var(--status-failed-bg);
  }
  .recalled {
    color: var(--status-recalled-fg);
    background: var(--status-recalled-bg);
  }
</style>
```

`src/lib/components/governance/TallyBar.svelte`:
```svelte
<script lang="ts">
  /**
   * ZEB-607 — Commons tally bar (spec D2). Flex segments on
   * --tally-track; each fill animates width .35s ease (the design's
   * only sanctioned motion). `token` is a CSS custom-property NAME
   * ('--vote-for') resolved at render via var().
   */
  let {
    segments,
    height = 8,
    label,
  }: {
    segments: Array<{ pct: number; token: string }>;
    height?: number;
    label?: string;
  } = $props();

  function clamp(pct: number): number {
    return Math.max(0, Math.min(100, pct));
  }
</script>

<div class="tally-track" style="height: {height}px" role="img" aria-label={label ?? 'Tally'}>
  {#each segments as seg, i (i)}
    <span class="tally-fill" style="width: {clamp(seg.pct)}%; background: var({seg.token})"></span>
  {/each}
</div>

<style>
  .tally-track {
    display: flex;
    background: var(--tally-track);
    border-radius: 4px;
    overflow: hidden;
  }
  .tally-fill {
    height: 100%;
    transition: width 0.35s ease;
  }
</style>
```

`src/lib/components/governance/CountChip.svelte`:
```svelte
<script lang="ts">
  /**
   * ZEB-607 — Commons count chip (spec D2): the design's
   * quorum/conviction chip anatomy — soft box, small label, mono value.
   */
  let {
    label,
    value,
    tone = 'neutral',
  }: {
    label: string;
    value: string;
    tone?: 'sage' | 'clay' | 'neutral';
  } = $props();
</script>

<div class="count-chip {tone}">
  <span class="cc-label">{label}</span>
  <span class="cc-value">{value}</span>
</div>

<style>
  .count-chip {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 6px 10px;
    border-radius: 6px;
    background: var(--status-drafting-bg);
    min-width: 0;
  }
  .cc-label {
    font-size: 0.6rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-muted);
  }
  .cc-value {
    font-family: var(--font-mono);
    font-size: 0.82rem;
    color: var(--text-primary);
  }
  .sage {
    background: var(--primary-soft);
  }
  .sage .cc-value {
    color: var(--primary-deep);
  }
  .clay {
    background: var(--gov-clay-soft);
  }
  .clay .cc-value {
    color: var(--gov-clay-deep);
  }
</style>
```

`src/lib/components/governance/GovConfirmModal.svelte`:
```svelte
<script lang="ts">
  /**
   * ZEB-607 — shared governance confirm modal (spec D2). Replaces the
   * three verbatim .confirm-modal copies (Tier3ProposalPanel,
   * StatementComposer, StarRatificationBallot) and hosts
   * DelegationWidget's typed-"revoke" severity tier
   * (feedback_severe_action_confirmation: click = reversible,
   * typed = irreversible-by-consequence).
   */
  import type { Snippet } from 'svelte';

  let {
    title,
    confirmLabel = 'Confirm',
    cancelLabel = 'Cancel',
    severity = 'click',
    typedMatch = 'revoke',
    busy = false,
    onConfirm,
    onCancel,
    children,
  }: {
    title: string;
    confirmLabel?: string;
    cancelLabel?: string;
    severity?: 'click' | 'typed';
    typedMatch?: string;
    busy?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
    children?: Snippet;
  } = $props();

  let typedInput = $state('');
  let confirmEnabled = $derived(
    !busy &&
      (severity === 'click' ||
        typedInput.trim().toLowerCase() === typedMatch.toLowerCase()),
  );
</script>

<div class="confirm-modal" role="dialog" aria-modal="true" aria-label={title}>
  <div class="confirm-card">
    <p class="confirm-title">{title}</p>
    {#if children}
      {@render children()}
    {/if}
    {#if severity === 'typed'}
      <input
        class="typed-input"
        type="text"
        bind:value={typedInput}
        placeholder={typedMatch}
        aria-label={`Type the word ${typedMatch} to confirm`}
        disabled={busy}
      />
    {/if}
    <div class="confirm-actions">
      <button type="button" class="cancel" onclick={onCancel} disabled={busy}>
        {cancelLabel}
      </button>
      <button type="button" class="confirm" onclick={onConfirm} disabled={!confirmEnabled}>
        {confirmLabel}
      </button>
    </div>
  </div>
</div>

<style>
  .confirm-modal {
    position: fixed;
    inset: 0;
    background: var(--overlay);
    display: grid;
    place-items: center;
    z-index: 100;
  }
  .confirm-card {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    box-shadow: var(--shadow-e2);
    padding: 1.25rem 1.5rem;
    border-radius: 10px;
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    max-width: 480px;
  }
  .confirm-title {
    margin: 0;
    font-weight: 600;
    color: var(--text-primary);
  }
  .typed-input {
    padding: 6px 10px;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: var(--input-bg);
    color: var(--text-primary);
    font: inherit;
    max-width: 160px;
  }
  .typed-input:focus {
    outline: 1px solid var(--accent);
    outline-offset: 1px;
  }
  .confirm-actions {
    display: flex;
    gap: 0.5rem;
    justify-content: flex-end;
  }
  .confirm-actions button {
    padding: 6px 14px;
    border-radius: 7px;
    font: inherit;
    cursor: pointer;
  }
  .confirm-actions button:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }
  .cancel {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
  }
  .confirm {
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--text-bright);
    font-weight: 600;
  }
</style>
```

`src/app.css` — insert immediately after the `--status-recalled-bg: #f3ddd7;` line in the `:root` block (do NOT touch the dark block):
```css
  /* ZEB-607: live-verdict pill aliases. Pure var() refs — custom
     properties substitute at use time against the cascaded theme
     values, so these track light/dark automatically and need no
     dark-block duplicates. Tied/quorum verdicts reuse the open and
     drafting pairs directly (spec D3). */
  --verdict-passing-fg: var(--primary-deep);
  --verdict-passing-bg: var(--primary-soft);
  --verdict-failing-fg: var(--danger-deep);
  --verdict-failing-bg: var(--status-recalled-bg);
```

`src/lib/voting-toast-wiring.ts` — append at end of file (touch nothing above):
```typescript
/** ZEB-607 D6 — signed-vote feedback toasts. ~2.1s dwell matches the
 *  design prototype; deliberately shorter than the 5s delegate-on-
 *  behalf toast above (whose copy is locked by ZEB-298 Task 10). */
const SIGNED_TOAST_MS = 2100;

export function showSignalCastToast(support: boolean): void {
  toastStore.show(
    support
      ? '✓ Support signaled · signed with your key'
      : '✓ Support withdrawn · signed with your key',
    SIGNED_TOAST_MS,
  );
}

export function showDelegationToast(delegateName: string): void {
  toastStore.show(`↪ Proxied to ${delegateName}`, SIGNED_TOAST_MS);
}

export function showRecallToast(): void {
  toastStore.show('↩ Delegation recalled — your vote is yours again', SIGNED_TOAST_MS);
}
```

- [ ] **Step 4: Run the new tests + full gates**

Run: `npx vitest run src/lib/__tests__/short-addr.test.ts src/lib/components/governance/__tests__/governance-primitives.test.ts src/lib/__tests__/signed-vote-toasts.test.ts`
Expected: PASS.
Run: `npx tsc --noEmit && npx vitest run`
Expected: PASS (no existing behavior touched).

- [ ] **Step 5: Commit**

```bash
git add src/lib/short-addr.ts src/lib/components/governance src/app.css src/lib/voting-toast-wiring.ts src/lib/__tests__/short-addr.test.ts src/lib/__tests__/signed-vote-toasts.test.ts
git commit -m "ZEB-607 T1: governance primitives (StatusPill/TallyBar/CountChip/GovConfirmModal), short-addr util, verdict aliases, signed-vote toasts"
```

---

### Task 2: ConvictionProposalCard — Commons anatomy

**Files:**
- Modify: `src/lib/components/ConvictionProposalCard.svelte` (template + style rewrite; script additions only)
- Modify: `src/lib/components/__tests__/ConvictionProposalCard.test.ts` (button-copy lockstep)
- Verify unchanged-pass: `src/lib/components/__tests__/AssemblyRail.test.ts`, `MessagesRail.test.ts`

**Interfaces:**
- Consumes (Task 1): `StatusPill` (`variant`,`label`,`ariaLabel`), `TallyBar`, `CountChip`, `shortId`, `showSignalCastToast`.
- Produces: new optional prop `hideText?: boolean` (default `false`) — Task 3's detail vote-column mounts `<ConvictionProposalCard … hideText />`. All existing props/behavior unchanged.

**Behavior invariants (do not change):** optimistic `optimisticSignal` flip/rollback, `$effect` re-sync from `proposal.your_signal`, `canSignal` gate, `showOverridePill` gate, exactly ONE `<button>` rendered in the plain signal state (tests use singular `getByRole('button')`), override-pill copy keeps `<strong>{delegateName ?? 'your delegate'}</strong>` and the "Vote directly" button (AssemblyRail + card tests pin these).

- [ ] **Step 1: Update the card**

Script changes (keep everything else, including all comments):
1. Add imports:
```typescript
  import { shortId } from '../short-addr';
  import { showSignalCastToast } from '../voting-toast-wiring';
  import StatusPill, { type StatusPillVariant } from './governance/StatusPill.svelte';
  import TallyBar from './governance/TallyBar.svelte';
  import CountChip from './governance/CountChip.svelte';
```
2. Add `hideText = false,` to the `$props()` destructure with prop type `/** ZEB-607: detail vote-column mounts the card next to the doc column, which already shows the text. */ hideText?: boolean;`.
3. After the `lifecycleLabel` derived, add:
```typescript
  /** ZEB-607 D3: lifecycle → Commons pill variant. */
  let lifecycleVariant = $derived.by((): StatusPillVariant => {
    switch (proposal.lifecycle) {
      case 'Open':
        return 'open';
      case 'ThresholdReached':
        return 'passing';
      case 'Finalized':
        return 'passed';
      default:
        return 'archived';
    }
  });
  let halfLifeDays = $derived(Math.round(proposal.half_life_seconds / 86_400));
```
4. In `toggleSignal`, on the success path (immediately after the `await adapter.signalTier2(...)` line, before the comment about the signal-cast event), add:
```typescript
      showSignalCastToast(nextSupport); // ZEB-607 D6: signed-vote feedback
```

Replace the entire template with:
```svelte
<article
  class="conviction-proposal-card"
  data-proposal-id={proposal.proposal_id}
  aria-label="Conviction proposal"
>
  <header class="cp-header">
    <span class="cp-id-pill" aria-label="Proposal id">{shortId(proposal.proposal_id)}</span>
    <StatusPill variant={lifecycleVariant} label={lifecycleLabel} ariaLabel="Lifecycle" />
    <span class="cp-half-life" aria-label="Half-life">half-life {halfLifeDays}d</span>
  </header>

  {#if !hideText}
    <p class="cp-text">{proposal.proposal_text}</p>
  {/if}

  <div class="cp-bar-wrap" aria-label="Conviction progress">
    <TallyBar
      segments={[{ pct: pctFilled, token: pctFilled >= 100 ? '--gov-clay' : '--vote-for' }]}
      label="Conviction vs threshold"
    />
    <span class="cp-bar-pct" aria-label="Percent of threshold">
      {pctFilled.toFixed(1)}%
    </span>
  </div>

  <div class="cp-chips">
    <CountChip tone="sage" label="Threshold" value={`${pctFilled.toFixed(0)}% reached`} />
    <CountChip
      tone="clay"
      label="Supporters"
      value={`${proposal.voter_count} / ${proposal.total_supply}`}
    />
  </div>

  {#if showOverridePill}
    <!-- ZEB-292 Phase 3 override affordance, restyled as the Commons
         proxied footer (spec D5 + amendment 2: the action is "Vote
         directly" — the real per-proposal override verb; community-
         scoped Recall lives in DelegationWidget only). -->
    <div class="cp-override-pill" role="status" aria-label="Delegate signaling on your behalf">
      <span class="cp-override-text">
        Your conviction follows <strong>{delegateName ?? 'your delegate'}</strong> on this proposal.
      </span>
      <button
        type="button"
        class="cp-override-btn"
        disabled={signaling}
        onclick={toggleSignal}
      >
        Vote directly
      </button>
      {#if signalError}
        <span class="cp-error" role="alert">Override failed: {signalError}</span>
      {/if}
    </div>
  {:else if canSignal}
    <div class="cp-signal-row">
      <button
        type="button"
        class="cp-signal-btn"
        class:supporting={optimisticSignal === true}
        disabled={signaling}
        aria-pressed={optimisticSignal === true}
        onclick={toggleSignal}
      >
        {optimisticSignal === true ? 'Withdraw support' : '▲ Support'}
      </button>
      {#if signalError}
        <span class="cp-error" role="alert">Signal failed: {signalError}</span>
      {/if}
    </div>
  {/if}
</article>
```

Replace the entire `<style>` block with:
```css
  .conviction-proposal-card {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 14px 16px;
    border: 1px solid var(--border);
    border-left: 3px solid var(--gov-clay);
    border-radius: 8px;
    background: var(--surface-raised);
    box-shadow: var(--shadow-e1);
    max-width: 520px;
  }
  .cp-header {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .cp-id-pill {
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 0.62rem;
    color: var(--text-bright);
    background: var(--gov-clay);
    padding: 2px 6px;
    border-radius: 3px;
  }
  .cp-half-life {
    margin-left: auto;
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--faint);
  }
  .cp-text {
    margin: 0;
    color: var(--text-primary);
    font-size: 0.95rem;
    line-height: 1.5;
    white-space: pre-wrap;
  }
  .cp-bar-wrap {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .cp-bar-wrap > :global(.tally-track) {
    flex: 1;
  }
  .cp-bar-pct {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    font-size: 0.8rem;
    color: var(--text-muted);
    min-width: 48px;
    text-align: right;
  }
  .cp-chips {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .cp-signal-row {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .cp-signal-btn {
    padding: 8px 16px;
    border: 1px solid var(--vote-for);
    border-radius: 7px;
    background: var(--vote-for);
    color: var(--status-passed-fg);
    font: inherit;
    font-weight: 600;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .cp-signal-btn:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }
  .cp-signal-btn.supporting {
    background: var(--surface-raised);
    color: var(--vote-for);
    border-color: var(--primary-border);
    font-weight: 600;
  }
  .cp-error {
    color: var(--danger);
    font-size: 0.85rem;
  }
  .cp-override-pill {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    padding: 8px 12px;
    border-top: 1px solid var(--line-soft);
    background: var(--paper);
    border-radius: 0 0 6px 6px;
    margin: 2px -6px -4px;
  }
  .cp-override-text {
    flex: 1 1 auto;
    color: var(--text-muted);
    font-size: 0.8rem;
  }
  .cp-override-text strong {
    color: var(--vote-for);
  }
  .cp-override-btn {
    padding: 4px 12px;
    border: 1px solid var(--primary-border);
    background: transparent;
    color: var(--vote-for);
    border-radius: 7px;
    font: inherit;
    font-size: 0.8rem;
    font-weight: 600;
    cursor: pointer;
  }
  .cp-override-btn:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }
```

- [ ] **Step 2: Lockstep the card tests**

In `src/lib/components/__tests__/ConvictionProposalCard.test.ts`, apply EXACTLY these copy changes (behavior assertions unchanged):
- `it('shows "Signal support" …` → rename to `'shows "▲ Support" when the caller has not yet signaled'`; `expect(btn.textContent).toContain('Signal support')` → `toContain('▲ Support')`.
- `it('shows "Withdraw signal" …` → `'shows "Withdraw support" when the caller is currently supporting'`; `toContain('Withdraw signal')` → `toContain('Withdraw support')`.
- `'calls signalTier2(proposal_id, true) when clicking Signal support'` — title only; body unchanged.
- `'calls signalTier2(proposal_id, false) when clicking Withdraw signal'` → title `…Withdraw support`; body unchanged.
- Rollback test: `expect(screen.getByRole('button').textContent).toContain('Signal support')` → `toContain('▲ Support')`.
- Override tests: `{ name: /withdraw signal/i }` → `{ name: /withdraw support/i }`; `{ name: /signal support/i }` → `{ name: /▲ support/i }` (two occurrences).
- Line 55/94 lifecycle assertions (`getByLabelText('Lifecycle').textContent).toBe('Open')` / `('Finalized')`) stay UNCHANGED — the card passes `lifecycleLabel` explicitly so pill text is identical.

- [ ] **Step 3: Run gates**

Run: `npx vitest run src/lib/components/__tests__/ConvictionProposalCard.test.ts src/lib/components/__tests__/AssemblyRail.test.ts src/lib/components/__tests__/MessagesRail.test.ts src/lib/components/__tests__/CommunityProposalsPanel.test.ts`
Expected: PASS with AssemblyRail/MessagesRail test files showing ZERO diff (`git diff --stat` must not list them).
Run: `npx tsc --noEmit && npx vitest run`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/lib/components/ConvictionProposalCard.svelte src/lib/components/__tests__/ConvictionProposalCard.test.ts
git commit -m "ZEB-607 T2: ConvictionProposalCard Commons anatomy — ID pill, status pill, tally bar, chips, Support grammar, cast toast"
```

---

### Task 3: CommunityProposalsPanel — hub restyle + doc-column ballot detail

**Files:**
- Modify: `src/lib/components/CommunityProposalsPanel.svelte`
- Modify: `src/lib/components/__tests__/CommunityProposalsPanel.test.ts` (ADD detail-state tests; existing tests must keep passing)

**Interfaces:**
- Consumes (T1/T2): `StatusPill`, `shortId`, `convictionPercent` (already exported from `../types/voting`), `ConvictionProposalCard` `hideText` prop.
- Produces: nothing consumed later.

**Behavior invariants:** the `$effect` (community-swap reset + 4 subscriptions), both load-token spaces, the create-form gating, and DelegationWidget/DelegationGraph mounts are UNTOUCHED except for the two additions named below.

- [ ] **Step 1: Add the detail state (script)**

Add imports: `import { convictionPercent } from '../types/voting';`, `import { shortId } from '../short-addr';`, `import StatusPill from './governance/StatusPill.svelte';`.

Add state + deriveds after `let graphOpen = $state(false);`:
```typescript
  /** ZEB-607: doc-column ballot detail (spec D5). Holds the open
   *  proposal's id; the DTO itself is derived from the live list so
   *  event-driven refetches keep the detail fresh for free. Falls back
   *  to the hub if the proposal leaves the list. */
  let selectedProposalId = $state<string | null>(null);
  let selectedProposal = $derived(
    selectedProposalId
      ? (proposals?.find((p) => p.proposal_id === selectedProposalId) ?? null)
      : null,
  );
  let selectedPct = $derived(
    selectedProposal
      ? convictionPercent(
          selectedProposal.total_conviction_ms,
          selectedProposal.threshold_conviction_ms,
        )
      : 0,
  );
```

In the community-swap `$effect`, extend the reset block (`proposals = null; loadError = null; myDelegate = null;`) with one line: `selectedProposalId = null;`.

- [ ] **Step 2: Rework the template**

Wrap the EXISTING section content (DelegationWidget, dg-section details, form, proposals-list — all unchanged) in an `{:else}` branch of a new top-level conditional, and give each listed card an "Open ballot →" affordance. The section becomes:

```svelte
<section class="community-proposals" aria-label="Community proposals">
  {#if selectedProposal}
    <button type="button" class="detail-back" onclick={() => (selectedProposalId = null)}>
      ← All proposals
    </button>
    <div class="detail-grid">
      <article class="doc-col" aria-label="Proposal document">
        <div class="doc-breadcrumb">
          <span class="doc-id-pill">{shortId(selectedProposal.proposal_id)}</span>
          <StatusPill
            variant={selectedProposal.lifecycle === 'Open'
              ? 'open'
              : selectedProposal.lifecycle === 'ThresholdReached'
                ? 'passing'
                : selectedProposal.lifecycle === 'Finalized'
                  ? 'passed'
                  : 'archived'}
            label={selectedProposal.lifecycle === 'ThresholdReached'
              ? 'Threshold reached'
              : undefined}
          />
        </div>
        <p class="doc-text">{selectedProposal.proposal_text}</p>
        <section class="on-record" aria-label="On the record">
          <h5 class="or-heading">On the record</h5>
          <dl class="or-rows">
            <dt>Method</dt>
            <dd>conviction · half-life {Math.round(selectedProposal.half_life_seconds / 86_400)}d</dd>
            <dt>Threshold</dt>
            <dd>{selectedPct.toFixed(1)}% reached</dd>
            <dt>Signed by</dt>
            <dd class="or-keys">✓ {selectedProposal.voter_count} keys</dd>
          </dl>
          <p class="or-note">
            Every vote is signed by its caster's key and replicated peer-to-peer. No server can
            alter the tally.
          </p>
        </section>
      </article>
      <aside class="vote-col" aria-label="Ballot">
        <h5 class="vote-heading">Live tally</h5>
        <ConvictionProposalCard
          {communityId}
          proposal={selectedProposal}
          {adapter}
          {myDelegate}
          delegateName={myDelegateName}
          hideText
        />
      </aside>
    </div>
  {:else}
    <!-- existing content, verbatim: DelegationWidget mount, dg-section
         details block, new-proposal form, proposals-list — with ONE
         change inside the {#each}: wrap each card in a .hub-item div and
         add the open-ballot link. -->
    …
      {#each proposals as proposal (proposal.proposal_id)}
        <div class="hub-item">
          <ConvictionProposalCard
            {communityId}
            {proposal}
            {adapter}
            {myDelegate}
            delegateName={myDelegateName}
          />
          <button
            type="button"
            class="hub-open-link"
            onclick={() => (selectedProposalId = proposal.proposal_id)}
          >
            Open ballot →
          </button>
        </div>
      {/each}
    …
  {/if}
</section>
```

(The `…` above means: reproduce the existing markup exactly as it is in the file today — this plan changes only the `{#each}` body and adds the outer `{#if}/{:else}`.)

- [ ] **Step 3: Add styles** (append to the existing `<style>` block; also add `container-type: inline-size;` to the existing `.community-proposals` rule)

```css
  .detail-back {
    align-self: flex-start;
    border: none;
    background: none;
    color: var(--vote-for);
    font: inherit;
    font-size: 0.85rem;
    font-weight: 600;
    cursor: pointer;
    padding: 0;
  }
  .detail-grid {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 300px;
    gap: 28px;
    align-items: start;
    max-width: 860px;
  }
  @container (max-width: 719px) {
    .detail-grid {
      grid-template-columns: 1fr;
    }
  }
  .doc-col {
    display: flex;
    flex-direction: column;
    gap: 14px;
    min-width: 0;
  }
  .doc-breadcrumb {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .doc-id-pill {
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 0.62rem;
    color: var(--text-bright);
    background: var(--gov-clay);
    padding: 2px 6px;
    border-radius: 3px;
  }
  .doc-text {
    margin: 0;
    color: var(--text-primary);
    font-size: 0.95rem;
    line-height: 1.65;
    white-space: pre-wrap;
  }
  .on-record {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 12px 15px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .or-heading {
    margin: 0;
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .or-rows {
    margin: 0;
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 4px 12px;
    font-size: 0.8rem;
  }
  .or-rows dt {
    color: var(--text-muted);
  }
  .or-rows dd {
    margin: 0;
    font-family: var(--font-mono);
    color: var(--text-primary);
  }
  .or-keys {
    color: var(--vote-for);
  }
  .or-note {
    margin: 0;
    font-size: 0.72rem;
    line-height: 1.45;
    color: var(--text-muted);
  }
  .vote-col {
    display: flex;
    flex-direction: column;
    gap: 8px;
    min-width: 0;
  }
  .vote-heading {
    margin: 0;
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .hub-item {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .hub-open-link {
    align-self: flex-start;
    border: none;
    background: none;
    padding: 0;
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--gov-clay);
    cursor: pointer;
  }
```

- [ ] **Step 4: Add detail-state tests**

Append to `CommunityProposalsPanel.test.ts`, reusing the file's existing adapter-mock + render helpers (match its established idiom for props/mock construction; do not build a new mock pattern):

```typescript
describe('ballot detail (ZEB-607)', () => {
  it('opens the doc-column detail from "Open ballot →" and returns via back link', async () => {
    // Arrange: mock adapter whose listTier2Proposals resolves one Open
    // proposal (voter_count: 3, total_supply: 10, half_life_seconds:
    // 7 * 86400) — use the file's existing proposal factory.
    // 1. render panel; await the proposal text visible
    await waitFor(() => expect(screen.getByText(/open ballot/i)).toBeTruthy());
    await fireEvent.click(screen.getByText(/open ballot/i));
    // Doc column + on-record from real DTO fields:
    expect(screen.getByText('On the record')).toBeTruthy();
    expect(screen.getByText('✓ 3 keys')).toBeTruthy();
    expect(screen.getByText(/half-life 7d/)).toBeTruthy();
    expect(screen.getByText(/No server can alter the tally/)).toBeTruthy();
    // Hub chrome hidden in detail:
    expect(screen.queryByLabelText('Delegation')).toBeNull();
    // Back:
    await fireEvent.click(screen.getByText('← All proposals'));
    await waitFor(() => expect(screen.getByText(/open ballot/i)).toBeTruthy());
    expect(screen.queryByText('On the record')).toBeNull();
  });

  it('falls back to the hub when the selected proposal leaves the list', async () => {
    // Open detail, then emit a lifecycle event with the mock's list
    // swapped to [] (file's existing event-emitter helper) and assert
    // the on-record block is gone after refetch.
  });
});
```
(Second test: implement fully using the file's existing emit/refetch helpers — the assertion is `await waitFor(() => expect(screen.queryByText('On the record')).toBeNull())`.)

- [ ] **Step 5: Run gates**

Run: `npx vitest run src/lib/components/__tests__/CommunityProposalsPanel.test.ts src/lib/components/__tests__/ConvictionProposalCard.test.ts`
Expected: PASS (all pre-existing panel tests green without edits).
Run: `npx tsc --noEmit && npx vitest run`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/CommunityProposalsPanel.svelte src/lib/components/__tests__/CommunityProposalsPanel.test.ts
git commit -m "ZEB-607 T3: proposals panel — hub open-ballot links + doc-column detail with On-the-record block"
```

---

### Task 4: PollMessage, DelegationWidget (+toasts, typed modal), DelegationGraph

**Files:**
- Modify: `src/lib/components/PollMessage.svelte` (tokens + pill only)
- Modify: `src/lib/components/DelegationWidget.svelte`
- Modify: `src/lib/components/DelegationGraph.svelte` (one style rule)
- Modify: `src/style-token-allowlist.json` (regen — DelegationGraph entry drops)
- Tests: `PollMessage.test.ts`, `DelegationWidget.test.ts`, `DelegationGraph.test.ts` must pass — expected WITHOUT edits; if a pinned selector/copy moved, lockstep minimally and record it in the task report.

**Interfaces:**
- Consumes (T1): `StatusPill`, `shortId`, `GovConfirmModal`, `showDelegationToast`, `showRecallToast`.

- [ ] **Step 1: PollMessage — Commons tokens**

Script: add `import StatusPill from './governance/StatusPill.svelte';`.
Template: replace the lifecycle span
```svelte
    <span class="poll-lifecycle" class:open={isOpen} aria-label="Lifecycle">
      {state?.meta.lifecycle ?? meta.lifecycle}
    </span>
```
with
```svelte
    <StatusPill
      variant={isOpen ? 'open' : 'archived'}
      label={state?.meta.lifecycle ?? meta.lifecycle}
      ariaLabel="Lifecycle"
    />
```
(Explicit `label` keeps the exact lifecycle strings tests/text pin; `.poll-lifecycle` CSS rules are then dead — delete them.)
Style edits (exact rule replacements):
- `.poll-option-bar` → `background: var(--tally-track); height: 6px; border-radius: 3px;` (keep grid placement/overflow lines).
- `.poll-option-bar-fill` → `background: var(--vote-for); transition: width 0.35s ease;` (keep display/height).
- `.poll-option-btn:hover:not(:disabled)` → `border-color: var(--vote-for);`
- `.poll-option-btn.selected` → `border-color: var(--vote-for); background: color-mix(in srgb, var(--vote-for) 10%, var(--bg-primary));`
- Delete `.poll-lifecycle` and `.poll-lifecycle.open` rules.

- [ ] **Step 2: DelegationWidget — Commons grammar + typed modal + toasts**

Script:
1. Imports: `import GovConfirmModal from './governance/GovConfirmModal.svelte';`, `import { shortId } from '../short-addr';`, `import { showDelegationToast, showRecallToast } from '../voting-toast-wiring';`.
2. `delegateName` derived: replace `` `${currentDelegate.slice(0, 8)}…` `` with `shortId(currentDelegate)`.
3. In `setDelegate`, after the `if (genAtStart !== generation) return;` success line and before `pendingDelegate = '';`, add:
```typescript
      const targetName =
        communityMembers.find((m) => m.address === target)?.displayName ?? shortId(target);
      showDelegationToast(targetName); // ZEB-607 D6
```
4. In `confirmRevoke`, after the success-path `confirmState = 'none'; typedInput = '';` lines, add `showRecallToast(); // ZEB-607 D6`.
5. `typedInput` state stays (the modal manages its own input; widget keeps the variable only if still referenced — after Step 3 it is NOT: delete `let typedInput = $state('');`, its reset in the `$effect` (`typedInput = '';`), the `confirmState === 'typed' && typedInput…` guard line in `confirmRevoke` (the modal's disabled gate already enforces the match), and `typedInput = '';` in `cancelRevoke`).

Template:
- Replace the `{:else if confirmState === 'typed'}` block with:
```svelte
    {:else if confirmState === 'typed'}
      <GovConfirmModal
        title="Type-to-confirm revoke"
        confirmLabel="Confirm revoke"
        severity="typed"
        typedMatch="revoke"
        busy={busy}
        onConfirm={() => void confirmRevoke()}
        onCancel={cancelRevoke}
      >
        <p class="dw-typed-copy">
          Your delegate is carrying significant weight on at least one active proposal.
          Revoking now will change those tallies. Type <strong>revoke</strong> to confirm.
        </p>
      </GovConfirmModal>
    {/if}
```
- Replace `<span class="dw-addr-tail" aria-hidden="true">({currentDelegate.slice(0, 8)}…)</span>` with `<span class="dw-addr-tail" aria-hidden="true">({shortId(currentDelegate)})</span>`.

Style edits:
- `.dw-revoke` → `border: 1px solid var(--danger-border-muted); color: var(--vote-against);` (keep the rest).
- `.dw-confirm-bar, .dw-confirm-typed` selector → `.dw-confirm-bar` only (typed block is gone); change its `border: 1px solid var(--danger)` → `var(--danger-border-muted)`.
- Delete `.dw-typed-input` rules; add `.dw-typed-copy { margin: 0; font-size: 0.85rem; color: var(--text-primary); }`.
- `.dw-apply, .dw-confirm` unchanged (`--accent` filled per spec D4).
- `.delegation-widget` add `background: var(--paper);` replacing `var(--bg-secondary)`.

Run `npx vitest run src/lib/components/__tests__/DelegationWidget.test.ts` — the typed-path tests query `Confirm revoke` button and the typed input by its `Type the word revoke to confirm` aria-label, both preserved by GovConfirmModal. If any other pin fails, lockstep minimally + report.

- [ ] **Step 3: DelegationGraph — token the local-node fill**

Replace
```css
  .dg-node-local {
    fill: #facc15;
    stroke-width: 2;
  }
```
with
```css
  .dg-node-local {
    fill: var(--warning);
    stroke-width: 2;
  }
```
Then regenerate the ratchet: `UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts` and verify `git diff src/style-token-allowlist.json` shows ONLY the `DelegationGraph.svelte` entry removed.

- [ ] **Step 4: Run gates**

Run: `npx tsc --noEmit && npx vitest run`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/PollMessage.svelte src/lib/components/DelegationWidget.svelte src/lib/components/DelegationGraph.svelte src/style-token-allowlist.json src/lib/components/__tests__
git commit -m "ZEB-607 T4: PollMessage tally tokens, DelegationWidget Commons grammar + typed-revoke modal + toasts, DelegationGraph warning token"
```

---

### Task 5: Tier-3 chrome — LifecycleStatus, ProposalPanel, Drafting, Deliberation, SortitionReveal, ParticipationToggle

**Files:**
- Modify: `src/lib/components/Tier3LifecycleStatus.svelte` (CSS ONLY — `.stage-chip`/`.current` classes and all copy are test-pinned)
- Modify: `src/lib/components/Tier3ProposalPanel.svelte`
- Modify: `src/lib/components/DraftingPanel.svelte`
- Modify: `src/lib/components/DeliberationView.svelte`
- Modify: `src/lib/components/SortitionRevealView.svelte`
- Modify: `src/lib/components/MiniPublicParticipationToggle.svelte`
- Modify: `src/style-token-allowlist.json` (regen — Tier3ProposalPanel entry drops)
- Tests: all six components' test files must pass; expected WITHOUT edits (no copy or class-name changes in this task). If one fails, lockstep minimally + report.

**Interfaces:** Consumes (T1): `GovConfirmModal`, `shortAddr`.

- [ ] **Step 1: Tier3LifecycleStatus** — replace the `<style>` block with:
```css
  .stage-chips {
    display: flex;
    list-style: none;
    gap: 0.25rem;
    padding: 0;
    margin: 0;
    font-size: 11px;
  }
  .stage-chip {
    padding: 4px 11px;
    border-radius: 20px;
    font-weight: 600;
    background: var(--status-drafting-bg);
    color: var(--status-drafting-fg);
  }
  .stage-chip.past {
    opacity: 0.6;
  }
  .stage-chip.current {
    background: var(--status-open-bg);
    color: var(--status-open-fg);
  }
  .failed-badge {
    color: var(--vote-against);
    font-weight: 600;
  }
  .finalized-badge {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    color: var(--vote-for);
  }
```

- [ ] **Step 2: Tier3ProposalPanel**

Script: add `import GovConfirmModal from './governance/GovConfirmModal.svelte';`.
Template: replace the `{#if confirmingCreate}` block with:
```svelte
  {#if confirmingCreate}
    <GovConfirmModal
      title="Confirm new Tier 3 proposal"
      confirmLabel={creating ? 'Creating…' : 'Confirm'}
      busy={creating}
      onConfirm={submitCreate}
      onCancel={() => (confirmingCreate = false)}
    >
      <p class="confirm-summary">
        "{proposalText.slice(0, 120)}{proposalText.length > 120 ? '…' : ''}"
      </p>
    </GovConfirmModal>
  {/if}
```
Style edits:
- Delete `.confirm-modal`, `.confirm-card`, `.confirm-actions`, `.confirm-actions button:last-child` rules (now in GovConfirmModal); KEEP `.confirm-summary` (add `margin: 0; color: var(--text-secondary);`).
- `.tier3-panel` h2: add rule `h2 { font-family: var(--font-display); font-weight: 500; }`.
- `.poll-row-button.selected` → `background: var(--primary-soft);`.
- `.privacy-chip` → `background: var(--sortition-bg); color: var(--gov-purple);` (removes the raw `rgba(170, 130, 255, 0.12)` literal — sortition purple RETAINED per spec D4).
- `.badge.winner` → `background: var(--status-passed-bg); color: var(--status-passed-fg);`.
- `.badge.runner-up` → `background: var(--status-drafting-bg); color: var(--status-drafting-fg);`.
- `.error` → `color: var(--danger);`; `.failed-detail` → `color: var(--vote-against);`.
- `.awaiting-tally` unchanged (already sortition purple).
Regen ratchet: `UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts`; verify diff = Tier3ProposalPanel entry removed only.

- [ ] **Step 3: DraftingPanel** — script: replace the local `shortAddr` with:
```typescript
  import { shortAddr as shortHex } from '../short-addr';
  function shortAddr(hex: string | null): string {
    if (!hex) return 'system';
    return shortHex(hex);
  }
```
Style: `.error` → `color: var(--danger);`; `h5` add rule `h5 { font-size: 0.68rem; font-weight: 600; text-transform: uppercase; letter-spacing: 0.08em; color: var(--text-muted); }`.

- [ ] **Step 4: DeliberationView** — style: add
```css
  h4 {
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
    border-bottom: 1px solid var(--line-soft);
    padding-bottom: 6px;
  }
```

- [ ] **Step 5: SortitionRevealView** — script: replace local `shortAddr` with `import { shortAddr } from '../short-addr';` (identical behavior — delete the local function). Style: `.backup-banner` → `background: var(--primary-soft); color: var(--primary-deep);`; `.roster li.self` → `background: var(--primary-soft);`.

- [ ] **Step 6: MiniPublicParticipationToggle** — style: button → `color: var(--vote-against); border: 1px solid var(--danger-border-muted);`; `.error` → `color: var(--danger);`.

- [ ] **Step 7: Run gates**

Run: `npx vitest run src/lib/components/__tests__/Tier3LifecycleStatus.test.ts src/lib/components/__tests__/Tier3ProposalPanel.test.ts src/lib/components/__tests__/DraftingPanel.test.ts src/lib/components/__tests__/SortitionRevealView.test.ts src/lib/components/__tests__/StatementVoteList.test.ts`
Expected: PASS without test edits.
Run: `npx tsc --noEmit && npx vitest run`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/lib/components/Tier3LifecycleStatus.svelte src/lib/components/Tier3ProposalPanel.svelte src/lib/components/DraftingPanel.svelte src/lib/components/DeliberationView.svelte src/lib/components/SortitionRevealView.svelte src/lib/components/MiniPublicParticipationToggle.svelte src/style-token-allowlist.json
git commit -m "ZEB-607 T5: Tier-3 chrome — stage pills, shared confirm modal, sortition-purple token, vote-semantic colors"
```

---

### Task 6: Tier-3 ballots + admin panel — StatementComposer, StatementVoteList, StarRatificationBallot, BridgingPanel, PendingAdminProposalsPanel

**Files:**
- Modify: `src/lib/components/StatementComposer.svelte`
- Modify: `src/lib/components/StatementVoteList.svelte`
- Modify: `src/lib/components/StarRatificationBallot.svelte`
- Modify: `src/lib/components/BridgingPanel.svelte`
- Modify: `src/lib/components/PendingAdminProposalsPanel.svelte`
- Modify: `src/style-token-allowlist.json` (regen — StatementComposer entry drops)
- Tests: the five components' test files must pass; expected WITHOUT edits except where noted. Lockstep minimally + report otherwise.

**Interfaces:** Consumes (T1): `GovConfirmModal`, `TallyBar`, `shortId`.

- [ ] **Step 1: StatementComposer**

Script: add `import GovConfirmModal from './governance/GovConfirmModal.svelte';`.
Template: replace the `{#if confirming}` block with:
```svelte
{#if confirming}
  <GovConfirmModal
    title="Confirm statement submission"
    onConfirm={confirmSubmit}
    onCancel={() => (confirming = false)}
  >
    <blockquote class="preview">{text}</blockquote>
    <p class="caveat">Statements are immutable — once submitted, you cannot edit or retract.</p>
  </GovConfirmModal>
{/if}
```
Style: delete `.confirm-modal`, `.confirm-card`, `.actions`, `.actions button:last-child` rules; KEEP `.preview`/`.caveat`. Replace `.cap-warning { color: #d9b438; …}` with `color: var(--warning);`; `.error` → `color: var(--danger);`.
Regen ratchet; verify diff = StatementComposer entry removed only.
NOTE: `StatementComposer.test.ts` drives the confirm flow — its Cancel/Confirm button queries keep matching (labels unchanged: 'Cancel'/'Confirm'). Run it; lockstep only if a structural query (`.confirm-card` selector etc.) fails.

- [ ] **Step 2: StatementVoteList**

Script: replace `authorShort` body with `import { shortId } from '../short-addr';` and `const authorShort = shortId;` (top-level import + alias; delete the local function).
Template: in the read-only `{:else}` chips branch, add a three-bucket tally ABOVE the chips row (the one surface where 3 buckets genuinely exist — spec D1):
```svelte
          {@const total = s.agreeCount + s.disagreeCount + s.passCount}
          {#if total > 0}
            <TallyBar
              height={5}
              label="Statement votes"
              segments={[
                { pct: (s.agreeCount / total) * 100, token: '--vote-for' },
                { pct: (s.disagreeCount / total) * 100, token: '--vote-against' },
                { pct: (s.passCount / total) * 100, token: '--vote-abstain' },
              ]}
            />
          {/if}
```
with `import TallyBar from './governance/TallyBar.svelte';` added to the script. Place it as the first child inside the `.chips` container's parent (immediately before `<div class="chips">`), wrapped in a `<div class="chips-tally">` with style `.chips-tally { margin-top: 0.4rem; }`.
Style: `.chip.agree` → `color: var(--vote-for);`; `.chip.disagree` → `color: var(--vote-against);`; `.tri-button button.active` → `border-color: var(--vote-for);`; `.error` → `color: var(--danger);`.

- [ ] **Step 3: StarRatificationBallot**

Script: add `import GovConfirmModal from './governance/GovConfirmModal.svelte';`.
Template: replace the `{#if confirming}` block with:
```svelte
{#if confirming}
  <GovConfirmModal
    title="Confirm ratification ballot"
    onConfirm={confirmCast}
    onCancel={() => (confirming = false)}
  >
    <ul class="ballot-summary">
      {#each detail.ratificationCandidates as c, i}
        <li><strong>{scores[i]}</strong> — {c.text}</li>
      {/each}
    </ul>
    <p class="caveat">You can re-cast later if the ratification window is still open.</p>
  </GovConfirmModal>
{/if}
```
Style: delete `.confirm-modal`, `.confirm-card`, `.confirm-actions`, `.confirm-actions button:last-child`; KEEP `.ballot-summary`/`.caveat`. `.encryption-banner` UNCHANGED (sortition purple retained). `.success` → `color: var(--vote-for);`; `.error` → `color: var(--danger);`.
NOTE: `StarRatificationBallot.test.ts` exercises the confirm flow — 'Cancel'/'Confirm' labels unchanged; run it, lockstep only on structural-query failure.

- [ ] **Step 4: BridgingPanel**

Style: `.heat-bar` gradient → clay (spec §2.14: heat = conviction-adjacent emphasis, not agree-green):
```css
  .heat-bar { position: absolute; left: 0; top: 0; bottom: 0; background: linear-gradient(to right, color-mix(in srgb, var(--gov-clay) 18%, transparent), color-mix(in srgb, var(--gov-clay) 0%, transparent)); z-index: 0; }
```
`.chip.agree` → `color: var(--vote-for);`; `.error` → `color: var(--danger);`.
Script: replace `authorShort` with the `shortId` alias (same as Step 2).

- [ ] **Step 5: PendingAdminProposalsPanel**

Style block becomes:
```css
  .admin-proposals-panel { margin-block: 1rem; }
  .proposal-card {
    border: 1px solid var(--border);
    border-left: 3px solid var(--gov-clay);
    border-radius: 8px;
    background: var(--surface-raised);
    box-shadow: var(--shadow-e1);
    padding: 0.75rem;
    margin-block: 0.5rem;
  }
  .summary { font-weight: 600; }
  .meta { font-family: var(--font-mono); font-size: 0.75rem; color: var(--muted); margin-block: 0.25rem; }
  .reason { font-style: italic; margin-block: 0.25rem; }
  .error { color: var(--danger-deep); }
  .effective { opacity: 0.7; border-left-color: var(--vote-for); }
  .expired { opacity: 0.5; border-left-color: var(--vote-abstain); }
  button {
    padding: 6px 14px;
    border: 1px solid var(--vote-for);
    border-radius: 7px;
    background: var(--vote-for);
    color: var(--status-passed-fg);
    font: inherit;
    font-weight: 600;
    cursor: pointer;
  }
  button:disabled { cursor: not-allowed; opacity: 0.6; background: var(--surface-raised); color: var(--text-muted); border-color: var(--border); }
```
(No markup/copy changes — `PendingAdminProposalsPanel.test.ts` passes untouched.)

- [ ] **Step 6: Run gates**

Run: `npx vitest run src/lib/components/__tests__/StatementComposer.test.ts src/lib/components/__tests__/StatementVoteList.test.ts src/lib/components/__tests__/StarRatificationBallot.test.ts src/lib/components/__tests__/BridgingPanel.test.ts src/lib/components/__tests__/PendingAdminProposalsPanel.test.ts`
Expected: PASS.
Run: `npx tsc --noEmit && npx vitest run`
Expected: PASS — full suite. Also `git diff --stat` across the branch must show ZERO changes to `src/Layout.svelte`, `src-tauri/`, `AssemblyRail.test.ts`, `MessagesRail.test.ts`.

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/StatementComposer.svelte src/lib/components/StatementVoteList.svelte src/lib/components/StarRatificationBallot.svelte src/lib/components/BridgingPanel.svelte src/lib/components/PendingAdminProposalsPanel.svelte src/style-token-allowlist.json
git commit -m "ZEB-607 T6: Tier-3 ballots + admin panel — shared modals, 3-bucket statement tally, clay heat bar, warning token"
```

---

## Post-plan

Final whole-branch review (most capable model) → PR → converge per standing protocol. Full gates: `npx tsc --noEmit && npx vitest run` from repo root.
