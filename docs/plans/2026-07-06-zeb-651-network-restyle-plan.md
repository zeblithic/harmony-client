# ZEB-651 Commons H: Network mode restyle — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring Commons card/pill anatomy to the `/network` surfaces — card-wrap NetworkHealthView's sections, and replace the byte-duplicated relay badges + the peer-incompat alarm badge with one shared `NetworkStatusPill` component.

**Architecture:** Frontend-only Svelte 5 restyle. One net-new shared component (`NetworkStatusPill.svelte`) reusing the Commons `StatusPill` *anatomy* (20px pill) with network-domain variants mapped to the existing `--net-*` tokens; two consumers refactored to use it; NetworkHealthView additionally gets card scaffolding + display/mono fonts + a danger attention-card. No Rust, no IPC, no behavior change.

**Tech Stack:** Svelte 5 runes, existing CSS custom-property token layer (`src/app.css`), Vitest + @testing-library/svelte.

## Global Constraints

- **Budget-0 tokens.** Use ONLY `var(--*)` tokens already defined in `src/app.css`. Introduce ZERO raw color literals (no hex/rgb/hsl/named colors) in `<style>` blocks. Do NOT modify `src/style-token-allowlist.json`. The style-token-guard must stay green with no allowlist change.
- **Every replacement token must be verified defined in `src/app.css`.** A typo'd `var(--token)` renders nothing and NO test catches it. The tokens this plan uses are all confirmed present: `--net-ok-bg`, `--net-ok-fg`, `--net-warn-bg`, `--net-warn-fg`, `--net-danger-bg`, `--net-danger-fg`, `--surface-raised`, `--border`, `--shadow-e1`, `--font-display`, `--font-mono`.
- **Preserve test invariants exactly** (pinned by the two existing suites): `data-testid` values `nh-relay-badge`, `relay-badge`, `nh-peer-incompat`, `nh-transport-disabled`, `nh-transport-disabled-reason`; the peer-incompat `role="alert"` + `title`; and exact label text `Healthy`, `Cooling down (Ns)` (matches `/Cooling down \(\d+s\)/`), `⚠ incompatible`.
- **Do NOT touch `NodeDetail.svelte`.** Its metric-tiles → CountChip conversion is deferred (blocked on the ZEB-657 CountChip danger-tone decision).
- **Gates:** from the repo root, `npx tsc --noEmit && npx vitest run` must pass. Scope Vitest to the changed files during iterative dev (`npx vitest run src/lib/components/__tests__/NetworkStatusPill.test.ts src/lib/components/__tests__/NetworkHealthView.test.ts src/lib/components/__tests__/NetworkDiscoverabilitySettings.test.ts`); the final gate is the full `npx vitest run`.
- **Test-harness idiom** (match existing sibling tests): `import { render, screen } from '@testing-library/svelte';` + `import { describe, it, expect } from 'vitest';`; render with `render(Component, { props: {...} })`; assert with `toBeTruthy()` / `.getAttribute()` / `.classList.contains()` — do NOT assume jest-dom matchers like `toBeInTheDocument`.

---

### Task 1: `NetworkStatusPill.svelte` shared component + tests

**Files:**
- Create: `src/lib/components/NetworkStatusPill.svelte`
- Test: `src/lib/components/__tests__/NetworkStatusPill.test.ts`

**Interfaces:**
- Produces: `NetworkStatusPill` (default export). Props: `variant: 'healthy' | 'cooling' | 'incompat'` (required), `label: string` (required), plus forwarded `HTMLAttributes<HTMLSpanElement>` (so `data-testid`, `title`, `role`, `aria-label` pass through to the rendered `<span>`). Also exports the type `NetworkStatusVariant` from the module script.

- [ ] **Step 1: Write the component**

Create `src/lib/components/NetworkStatusPill.svelte`:

```svelte
<script lang="ts" module>
  /**
   * ZEB-651 — shared network-status pill. Reuses the Commons StatusPill
   * anatomy (20px pill, 11px / 600) with network-domain variants, replacing
   * the byte-duplicated relay `.badge*` in NetworkHealthView and
   * NetworkDiscoverabilitySettings plus the `.peer-incompat` alarm badge.
   *
   * Deliberately separate from governance/StatusPill.svelte, which owns
   * governance status colors only. Variant → token pairs live here so the
   * network `--net-*` semantics stay out of the governance enum.
   */
  export type NetworkStatusVariant = 'healthy' | 'cooling' | 'incompat';
</script>

<script lang="ts">
  import type { HTMLAttributes } from 'svelte/elements';

  let {
    variant,
    label,
    ...rest
  }: {
    variant: NetworkStatusVariant;
    label: string;
  } & HTMLAttributes<HTMLSpanElement> = $props();
</script>

<span class="net-pill {variant}" {...rest}>{label}</span>

<style>
  .net-pill {
    display: inline-block;
    font-weight: 600;
    font-size: 11px;
    line-height: 1.3;
    padding: 2px 10px;
    border-radius: 20px;
    white-space: nowrap;
  }
  .healthy {
    background: var(--net-ok-bg);
    color: var(--net-ok-fg);
  }
  .cooling {
    background: var(--net-warn-bg);
    color: var(--net-warn-fg);
  }
  .incompat {
    background: var(--net-danger-bg);
    color: var(--net-danger-fg);
    border: 1px solid var(--net-danger-fg);
  }
</style>
```

- [ ] **Step 2: Write the tests**

Create `src/lib/components/__tests__/NetworkStatusPill.test.ts`:

```ts
import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import NetworkStatusPill from '../NetworkStatusPill.svelte';

describe('NetworkStatusPill', () => {
  it('renders the label text', () => {
    render(NetworkStatusPill, { props: { variant: 'healthy', label: 'Healthy' } });
    expect(screen.getByText('Healthy')).toBeTruthy();
  });

  it('applies the variant class', () => {
    const { container } = render(NetworkStatusPill, {
      props: { variant: 'cooling', label: 'Cooling down (5s)' },
    });
    const pill = container.querySelector('.net-pill');
    expect(pill).toBeTruthy();
    expect(pill!.classList.contains('cooling')).toBe(true);
  });

  it('forwards data-testid, role and title to the span', () => {
    render(NetworkStatusPill, {
      props: {
        variant: 'incompat',
        label: '⚠ incompatible',
        'data-testid': 'nh-peer-incompat',
        role: 'alert',
        title: 'protocol mismatch',
      },
    });
    const pill = screen.getByTestId('nh-peer-incompat');
    expect(pill.getAttribute('role')).toBe('alert');
    expect(pill.getAttribute('title')).toBe('protocol mismatch');
    expect(pill.textContent).toContain('⚠ incompatible');
    expect(pill.classList.contains('incompat')).toBe(true);
  });
});
```

- [ ] **Step 3: Run the tests**

Run: `npx vitest run src/lib/components/__tests__/NetworkStatusPill.test.ts`
Expected: PASS (3 tests).

- [ ] **Step 4: Type check**

Run: `npx tsc --noEmit`
Expected: clean (no errors).

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/NetworkStatusPill.svelte src/lib/components/__tests__/NetworkStatusPill.test.ts
git commit -m "ZEB-651: add shared NetworkStatusPill (healthy/cooling/incompat)"
```

---

### Task 2: NetworkHealthView — badge consolidation + card anatomy + fonts + attention card

**Files:**
- Modify: `src/lib/components/NetworkHealthView.svelte`
- Verify (no change expected): `src/lib/components/__tests__/NetworkHealthView.test.ts`

**Interfaces:**
- Consumes: `NetworkStatusPill` from Task 1 (`variant`, `label`, forwarded attrs).

- [ ] **Step 1: Import the shared pill**

In the `<script lang="ts">` block of `src/lib/components/NetworkHealthView.svelte`, add to the imports (next to the existing `import DiagnosticExportModal from './DiagnosticExportModal.svelte';`):

```ts
import NetworkStatusPill from './NetworkStatusPill.svelte';
```

- [ ] **Step 2: Add a relay-badge label helper**

In the same `<script>`, add this function (near `relayOutcomeLabel`):

```ts
function relayBadgeLabel(relay: RelayHealth, nowMs: number): string {
  if (relay.state.kind === 'healthy') return 'Healthy';
  const secsLeft = Math.max(0, Math.ceil((relay.state.untilMs - nowMs) / 1000));
  return `Cooling down (${secsLeft}s)`;
}
```

`RelayHealth` is already imported in this file's `import type { ... } from '../types/network-health';` block — add `RelayHealth` to that list if it is not already present.

- [ ] **Step 3: Replace the peer-incompat span**

In the peers `{#each}` block, replace the existing `<span class="peer-incompat" ...>⚠ incompatible</span>` element (currently lines ~290-295) with:

```svelte
<NetworkStatusPill
  variant="incompat"
  label="⚠ incompatible"
  role="alert"
  title={p.protocolIncompatReason}
  data-testid="nh-peer-incompat"
/>
```

- [ ] **Step 4: Replace the pkarr relay badge block**

In the `.pkarr-relays` `{#each}` block, replace the entire `{#if relay.state.kind === 'healthy'} <span class="badge badge-healthy" ...>Healthy</span> {:else} <span class="badge badge-cooling" ...>Cooling down (…)</span> {/if}` block (currently lines ~418-427) with a single element:

```svelte
<NetworkStatusPill
  variant={relay.state.kind === 'healthy' ? 'healthy' : 'cooling'}
  label={relayBadgeLabel(relay, now)}
  data-testid="nh-relay-badge"
/>
```

- [ ] **Step 5: Add card anatomy + font + attention-card CSS**

In the `<style>` block:

(a) Add the card rule for the five content sections:

```css
.my-network,
.peers,
.dynamic-dials,
.self-test,
.pkarr-relays {
  background: var(--surface-raised);
  border: 1px solid var(--border);
  border-radius: 8px;
  box-shadow: var(--shadow-e1);
  padding: 16px;
  margin-bottom: 16px;
}
```

(b) Add display + mono font rules:

```css
.network-health h1,
.network-health h2 {
  font-family: var(--font-display);
}
.network-health code {
  font-family: var(--font-mono);
}
```

(c) Replace the existing `.transport-disabled` rule with the Commons danger attention-card:

```css
.transport-disabled {
  background: var(--surface-raised);
  border: 1px solid var(--border);
  border-left: 3px solid var(--net-danger-fg);
  border-radius: 8px;
  box-shadow: var(--shadow-e1);
  padding: 0.75rem 1rem;
  margin-bottom: 1rem;
}
```

Keep the existing `.transport-disabled h2 { color: var(--net-danger-fg); margin-top: 0; }` and `.transport-disabled .reason { font-family: var(--font-mono); word-break: break-word; }` rules unchanged.

- [ ] **Step 6: Remove the now-dead badge CSS**

Delete these rules (their spans are gone): `.peer-incompat { … }`, `.badge { … }`, `.badge-healthy { … }`, `.badge-cooling { … }`. Leave every other rule (`.status-*`, `.info-hover`, `.error`, `.self-test-steps`, `.dial-*`, `.pkarr-relays ul`, `.pkarr-relays li`, `.muted`) intact.

- [ ] **Step 7: Run the existing suite (must stay green unchanged)**

Run: `npx vitest run src/lib/components/__tests__/NetworkHealthView.test.ts`
Expected: PASS — all tests, including the `nh-peer-incompat` (text `⚠ incompatible`, `role="alert"`, `title`) and `nh-relay-badge` (`Healthy` / `Cooling down (Ns)`) assertions, unchanged.

- [ ] **Step 8: Type check + token guard**

Run: `npx tsc --noEmit`
Expected: clean.
Run: `npx vitest run src/style-token-guard.test.ts`
Expected: PASS with no allowlist change (no new literals were introduced).

- [ ] **Step 9: Commit**

```bash
git add src/lib/components/NetworkHealthView.svelte
git commit -m "ZEB-651: NetworkHealthView — card sections, display/mono fonts, attention card, shared pill"
```

---

### Task 3: NetworkDiscoverabilitySettings — relay-badge consolidation

**Files:**
- Modify: `src/lib/components/NetworkDiscoverabilitySettings.svelte`
- Verify (no change expected): `src/lib/components/__tests__/NetworkDiscoverabilitySettings.test.ts`

**Interfaces:**
- Consumes: `NetworkStatusPill` from Task 1.

- [ ] **Step 1: Import the shared pill**

In the `<script lang="ts">` block of `src/lib/components/NetworkDiscoverabilitySettings.svelte`, add to the imports:

```ts
import NetworkStatusPill from './NetworkStatusPill.svelte';
```

- [ ] **Step 2: Replace the relay-badge span**

In the relay `{#each}` list, replace the existing badge span (currently lines ~380-385):

```svelte
<span
  class="relay-badge {relay.state.kind === 'healthy' ? 'badge-healthy' : 'badge-cooling'}"
  data-testid="relay-badge"
>
  {relayStateLabel(relay)}
</span>
```

with:

```svelte
<NetworkStatusPill
  variant={relay.state.kind === 'healthy' ? 'healthy' : 'cooling'}
  label={relayStateLabel(relay)}
  data-testid="relay-badge"
/>
```

`relayStateLabel(relay)` already returns `'Healthy'` / `'Cooling down (Ns)'` — leave it unchanged.

- [ ] **Step 3: Remove the now-dead badge CSS**

Delete these rules from the `<style>` block: `.relay-badge { … }`, `.badge-healthy { … }`, `.badge-cooling { … }`. Leave every other rule intact (this component is a settings sub-panel and gets NO card treatment — only the badge is consolidated).

- [ ] **Step 4: Run the existing suite (must stay green unchanged)**

Run: `npx vitest run src/lib/components/__tests__/NetworkDiscoverabilitySettings.test.ts`
Expected: PASS — including `relay-badge` testid, `Healthy`, and `/Cooling down \(\d+s\)/` assertions, unchanged.

- [ ] **Step 5: Type check**

Run: `npx tsc --noEmit`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/NetworkDiscoverabilitySettings.svelte
git commit -m "ZEB-651: NetworkDiscoverabilitySettings — relay badge → shared NetworkStatusPill"
```

---

## Final gate (after all tasks)

- [ ] Full frontend gate from repo root: `npx tsc --noEmit && npx vitest run` — expected all green (~3238+ tests; the new NetworkStatusPill suite adds 3).
- [ ] Confirm `git diff origin/main -- src/style-token-allowlist.json` is EMPTY (no allowlist change).
- [ ] Whole-branch review (superpowers:requesting-code-review), then open the PR.
