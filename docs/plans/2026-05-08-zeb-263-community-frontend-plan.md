# ZEB-263 Phase 5 Community Frontend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the seven Phase 3 + Phase 4 community IPCs to UI: nav-tree integration, four dialogs (create / redeem / settings / set-power), invite-link manager, three-tier confirmation policy, error UX with diagnostic disclosure, and full test coverage. After this lands, ZEB-217 closes.

**Architecture:** All-frontend work. New service `community-service.ts` mirrors `MessageService` shape (IPC wrapper + event listener cache). NavService extended to handle `kind: 'community'`. NavPanel grows a global "+" FAB. App.svelte routes community-node-clicks to a new right-pane overview placeholder + `CommunitySettingsPanel` modal.

**Tech Stack:** Svelte 5 (runes), TypeScript, Tauri IPC, Vitest + @testing-library/svelte, jsdom.

**Spec:** `docs/specs/2026-05-08-zeb-263-community-frontend-design.md` (commit `3696130`).

**Branch:** `zeb-263-community-frontend` cut from `origin/main` at `26007ce` (merge of PR #90, ZEB-260). Spec commit at `3696130`.

**Per-task verification gates (HARD):**
- `cargo fmt --all -- --check` (no Rust changes expected, but gate still runs)
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `npx tsc --noEmit`
- `npx vitest run`

All five must pass before each task's commit. Use `${PIPESTATUS[0]}` or `set -o pipefail` if piping any of these — pipe exit codes lie.

**Subagent rules:**
- DO NOT use `Monitor` for `cargo test` or `npx vitest run`. Wait synchronously.
- All Tauri IPC params are snake_case at the boundary; Rust parameters auto-convert.
- Tauri error extraction: `e instanceof Error ? e.message : String(e)` everywhere errors are surfaced.

---

### Task 0: Pre-flight + green-baseline confirmation

**Files:** None modified.

- [ ] **Step 1: Confirm branch state**

```bash
git branch --show-current
# Expected: zeb-263-community-frontend

git log --oneline -3
# Expected: top commit is 3696130 docs(zeb-263): Phase 5 community frontend design spec

git status --short
# Expected: empty (clean working tree)
```

- [ ] **Step 2: Fetch origin and verify lineage**

```bash
git fetch origin
git merge-base --is-ancestor origin/main HEAD && echo "lineage ok" || echo "LINEAGE BROKEN"
# Expected: "lineage ok"
```

If "LINEAGE BROKEN" — stop and escalate. Per user memory rule: branch must stay on origin/main lineage.

- [ ] **Step 3: Run baseline gate suite (Rust)**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test
# Expected: all green
cd ..
```

If any gate fails on baseline, that's pre-existing breakage on `main`. Per user memory: test drift is our fault. File a follow-up ticket and fix it before starting Phase 5 work.

- [ ] **Step 4: Run baseline gate suite (frontend)**

```bash
npx tsc --noEmit
# Expected: 0 errors

npx vitest run
# Expected: all tests pass
```

- [ ] **Step 5: No commit (verification only)**

Task 0 produces no artifact. Proceed to Task 1.

---

### Task 1: Types + community-service skeleton

**Files:**
- Modify: `src/lib/types.ts`
- Create: `src/lib/community-service.ts`
- Test: `src/lib/__tests__/community-service.test.ts` (new dir if not present)
- Test: `src/lib/__tests__/power-role.test.ts`

- [ ] **Step 1: Write the failing test for `powerToRole` + `POWER_THRESHOLDS`**

`src/lib/__tests__/power-role.test.ts`:

```typescript
import { describe, it, expect } from 'vitest';
import { POWER_THRESHOLDS, powerToRole } from '../types';

describe('POWER_THRESHOLDS', () => {
  it('mirrors backend community_membership.rs:1108 values', () => {
    expect(POWER_THRESHOLDS.invite).toBe(0);
    expect(POWER_THRESHOLDS.kick).toBe(50);
    expect(POWER_THRESHOLDS.setPower).toBe(100);
    expect(POWER_THRESHOLDS.max).toBe(100);
  });
});

describe('powerToRole', () => {
  it('returns "member" for power 0', () => {
    expect(powerToRole(0)).toBe('member');
  });

  it('returns "member" for power 49 (just below kick threshold)', () => {
    expect(powerToRole(49)).toBe('member');
  });

  it('returns "mod" for power 50 (kick threshold)', () => {
    expect(powerToRole(50)).toBe('mod');
  });

  it('returns "mod" for power 99 (just below admin threshold)', () => {
    expect(powerToRole(99)).toBe('mod');
  });

  it('returns "admin" for power 100', () => {
    expect(powerToRole(100)).toBe('admin');
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

```bash
npx vitest run src/lib/__tests__/power-role.test.ts
# Expected: FAIL — POWER_THRESHOLDS / powerToRole not exported from types
```

- [ ] **Step 3: Add types and helper to `types.ts`**

Append to `src/lib/types.ts`:

```typescript
// ── Community types (ZEB-263) ─────────────────────────────────────

export interface Community {
  id: string;          // hex-encoded community_id (32 chars)
  name: string;
  kind: 'open' | 'invite-only';
  myPower: number;     // 0-100
  memberCount: number;
}

export interface CommunityMember {
  address: string;
  displayName?: string;
  power: number;       // 0-100
  status: 'joined' | 'invited' | 'banned';
  joinedAt?: number;
}

// Mirrors backend POWER_THRESHOLDS in src-tauri/src/community_membership.rs:1108.
export const POWER_THRESHOLDS = {
  invite: 0,
  kick: 50,
  setPower: 100,
  max: 100,
} as const;

export type PowerRole = 'member' | 'mod' | 'admin';

export function powerToRole(power: number): PowerRole {
  if (power >= POWER_THRESHOLDS.setPower) return 'admin';
  if (power >= POWER_THRESHOLDS.kick) return 'mod';
  return 'member';
}
```

Also add `'community'` to the existing `NavNodeType` union:

```typescript
// Find the existing NavNodeType in types.ts and add 'community':
export type NavNodeType = 'folder' | 'channel' | 'dm' | 'group-chat' | 'community';
```

- [ ] **Step 4: Run test to verify it passes**

```bash
npx vitest run src/lib/__tests__/power-role.test.ts
# Expected: 6 tests pass
```

- [ ] **Step 5: Write the failing test for community-service skeleton**

`src/lib/__tests__/community-service.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { CommunityService } from '../community-service';
import type { TauriAdapter } from '../zenoh-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

describe('CommunityService', () => {
  let service: CommunityService;
  let adapter: ReturnType<typeof makeAdapter>;

  beforeEach(() => {
    service = new CommunityService();
    adapter = makeAdapter();
  });

  it('connectAdapter installs community-members-changed + community-state-sync-degraded listeners', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('community-members-changed')).toBe(true);
    expect(adapter.listeners.has('community-state-sync-degraded')).toBe(true);
  });

  it('createCommunity calls invoke with snake_case args', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('aabbccdd');
    const id = await service.createCommunity('Test', 'invite-only');
    expect(adapter.invoke).toHaveBeenCalledWith('create_community', expect.objectContaining({ name: 'Test' }));
    expect(id).toBe('aabbccdd');
  });

  it('redeemInvite calls invoke with the URL string', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('eeff0011');
    const id = await service.redeemInvite('harmony://invite/v1?ci=...');
    expect(adapter.invoke).toHaveBeenCalledWith('redeem_invite', { url: 'harmony://invite/v1?ci=...' });
    expect(id).toBe('eeff0011');
  });

  it('listCommunityMembers caches per-community result', async () => {
    await service.connectAdapter(adapter);
    const fakeRoster = [{ address: 'a3f8c1d2', displayName: 'Alice', power: 100, status: 'joined' }];
    (adapter.invoke as any).mockResolvedValue(fakeRoster);

    const r1 = await service.listMembers('aabbccdd');
    const r2 = await service.listMembers('aabbccdd');

    expect(r1).toEqual(fakeRoster);
    expect(r2).toEqual(fakeRoster);
    // Cached: only one IPC call
    expect(adapter.invoke).toHaveBeenCalledTimes(1);
  });

  it('community-members-changed for a community invalidates its cache', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([]);
    await service.listMembers('aabbccdd');
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    // Simulate event
    const handler = adapter.listeners.get('community-members-changed')!;
    handler({ payload: { communityId: 'aabbccdd' } });

    await service.listMembers('aabbccdd');
    // Re-fetched after event
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
  });

  it('community-state-sync-degraded sets degraded flag', async () => {
    await service.connectAdapter(adapter);
    expect(service.isDegraded('aabbccdd')).toBe(false);

    const handler = adapter.listeners.get('community-state-sync-degraded')!;
    handler({ payload: { communityId: 'aabbccdd', degraded: true } });

    expect(service.isDegraded('aabbccdd')).toBe(true);
  });
});
```

- [ ] **Step 6: Run test to verify it fails**

```bash
npx vitest run src/lib/__tests__/community-service.test.ts
# Expected: FAIL — module 'community-service' not found
```

- [ ] **Step 7: Implement `community-service.ts`**

Create `src/lib/community-service.ts`:

```typescript
import type { TauriAdapter } from './zenoh-service';
import type { CommunityMember } from './types';

interface MembersChangedPayload { communityId: string; }
interface DegradedPayload { communityId: string; degraded: boolean; }

export class CommunityService {
  /** Called whenever member rosters or degraded state changes. */
  onChange?: () => void;

  private adapter: TauriAdapter | null = null;
  private memberCache: Map<string, CommunityMember[]> = new Map();
  private degraded: Map<string, boolean> = new Map();
  private unlisteners: Array<() => void> = [];

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlistenMembers = await adapter.listen(
      'community-members-changed',
      (event) => {
        const p = event.payload as MembersChangedPayload;
        this.memberCache.delete(p.communityId);
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenMembers);

    const unlistenDegraded = await adapter.listen(
      'community-state-sync-degraded',
      (event) => {
        const p = event.payload as DegradedPayload;
        this.degraded.set(p.communityId, p.degraded);
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenDegraded);
  }

  async createCommunity(name: string, kind: 'open' | 'invite-only'): Promise<string> {
    return this.invoke<string>('create_community', { name, kind });
  }

  async redeemInvite(url: string): Promise<string> {
    return this.invoke<string>('redeem_invite', { url });
  }

  async leaveCommunity(communityId: string): Promise<void> {
    await this.invoke<void>('leave_community', { communityId });
  }

  async kickMember(communityId: string, targetAddr: string): Promise<void> {
    await this.invoke<void>('kick_from_community', { communityId, targetAddr });
  }

  async setPowerLevel(communityId: string, targetAddr: string, newPower: number): Promise<void> {
    await this.invoke<void>('set_power_level', { communityId, targetAddr, newPower });
  }

  async generateInvite(communityId: string): Promise<string> {
    return this.invoke<string>('generate_invite', {
      communityId,
      inviteeHint: null,
      expiresAt: null,
    });
  }

  async listMembers(communityId: string): Promise<CommunityMember[]> {
    const cached = this.memberCache.get(communityId);
    if (cached) return cached;
    const fresh = await this.invoke<CommunityMember[]>('list_community_members', { communityId });
    this.memberCache.set(communityId, fresh);
    return fresh;
  }

  isDegraded(communityId: string): boolean {
    return this.degraded.get(communityId) ?? false;
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
    this.memberCache.clear();
    this.degraded.clear();
  }

  private async invoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
    if (!this.adapter) throw new Error(`CommunityService.${cmd}: adapter not connected`);
    return this.adapter.invoke(cmd, args) as Promise<T>;
  }
}
```

- [ ] **Step 8: Run all frontend gates**

```bash
npx tsc --noEmit
# Expected: 0 errors

npx vitest run
# Expected: all tests pass (existing + 2 new files)
```

- [ ] **Step 9: Run Rust gates**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test && cd ..
# Expected: all green (no Rust changes, gates still run)
```

- [ ] **Step 10: Commit**

```bash
git add src/lib/types.ts src/lib/community-service.ts src/lib/__tests__/community-service.test.ts src/lib/__tests__/power-role.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-263): community types + service skeleton

Adds Community/CommunityMember/PowerRole types and POWER_THRESHOLDS
constant mirroring backend community_membership.rs. Adds 'community'
to NavNodeType. Introduces CommunityService — thin IPC wrapper with
per-community member-roster cache invalidated on
community-members-changed events, plus degraded-flag tracking from
community-state-sync-degraded events.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: NavService rewrite

**Files:**
- Modify: `src/lib/nav-service.ts`
- Modify: `src/lib/nav-service.test.ts` (existing — extend)

- [ ] **Step 1: Write failing tests for community-kind handling**

Append to `src/lib/nav-service.test.ts`:

```typescript
describe('NavService — community kind (ZEB-263)', () => {
  it('addOrUpdateNavSpace creates a community NavNode for kind: "community"', () => {
    const svc = new NavService();
    svc.nodes = []; // clear seeded mock data
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'aabbccdd' + 'ee'.repeat(28),
      kind: 'community',
      name: 'Test Crew',
      parentId: null,
    });

    expect(svc.nodes).toHaveLength(1);
    const node = svc.nodes[0];
    expect(node.type).toBe('community');
    expect(node.name).toBe('Test Crew');
    expect(node.parentId).toBeNull();
    expect(node.expanded).toBe(true);
    expect(node.peer).toBeUndefined();
  });

  it('addOrUpdateNavSpace silently ignores kind: "channel"', () => {
    const svc = new NavService();
    svc.nodes = [];
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'cc'.repeat(32),
      kind: 'channel',
      name: 'general',
      parentId: 'aabb' + 'cc'.repeat(28),
    });

    expect(svc.nodes).toHaveLength(0);
  });

  it('community node can have parentId set (placement inside user folder)', () => {
    const svc = new NavService();
    svc.nodes = [];
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'aabbccdd' + 'ee'.repeat(28),
      kind: 'community',
      name: 'Crew',
      parentId: 'folder-1',
    });

    expect(svc.nodes[0].parentId).toBe('folder-1');
  });

  it('removed action drops community node', () => {
    const svc = new NavService();
    svc.nodes = [];
    const id = 'aabbccdd' + 'ee'.repeat(28);
    svc.addOrUpdateNavSpace({ action: 'added', spaceId: id, kind: 'community', name: 'Crew' });
    expect(svc.nodes).toHaveLength(1);
    svc.addOrUpdateNavSpace({ action: 'removed', spaceId: id, kind: 'community', name: 'Crew' });
    expect(svc.nodes).toHaveLength(0);
  });

  it('existing dm/group-dm path still works unchanged (regression)', () => {
    const svc = new NavService();
    svc.nodes = [];
    svc.addOrUpdateNavSpace({
      action: 'added',
      spaceId: 'dd'.repeat(32),
      kind: 'dm',
      name: 'Bob',
      members: ['bob_addr', 'self_addr'],
    });

    expect(svc.nodes).toHaveLength(1);
    expect(svc.nodes[0].type).toBe('dm');
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
npx vitest run src/lib/nav-service.test.ts
# Expected: 5 tests fail — addOrUpdateNavSpace not defined OR community kind not handled
```

- [ ] **Step 3: Rename `addOrUpdateDmSpace` to `addOrUpdateNavSpace` + add community branch**

In `src/lib/nav-service.ts`:

1. Rename method `addOrUpdateDmSpace` → `addOrUpdateNavSpace` (search-replace all references in this file).
2. Update the listener registration that calls it (likely line ~102).
3. Add the community branch BEFORE the existing dm/group-dm short-circuit return:

```typescript
// Inside addOrUpdateNavSpace, before the "if (kind !== 'dm' && kind !== 'group-dm') return;" line:

if (kind === 'community') {
  if (action === 'removed') {
    const before = this.nodes.length;
    this.nodes = this.nodes.filter((n) => n.id !== spaceId);
    if (this.nodes.length !== before) this.onChange?.();
    return;
  }

  const newNode: NavNode = {
    id: spaceId,
    type: 'community',
    name,
    parentId: parentId ?? null,
    expanded: true, // default expanded; user can collapse
    unreadCount: 0,
    unreadLevel: 'none',
    peer: undefined,
  };

  if (action === 'added') {
    const existing = this.nodes.find((n) => n.id === spaceId);
    if (existing) {
      // Preserve user-applied state on duplicate add (cold-replay)
      this.nodes = this.nodes.map((n) =>
        n.id === spaceId
          ? { ...newNode, parentId: existing.parentId, expanded: existing.expanded }
          : n
      );
    } else {
      this.nodes = [...this.nodes, newNode];
    }
  } else if (action === 'modified') {
    let found = false;
    this.nodes = this.nodes.map((n) => {
      if (n.id !== spaceId) return n;
      found = true;
      return { ...n, name }; // preserve parentId/expanded
    });
    if (!found) this.nodes = [...this.nodes, newNode];
  }

  this.onChange?.();
  return;
}
```

4. Update the existing comment at line ~117 to reflect the new behavior:

```typescript
// Phase 5 (ZEB-263) handles dm/group-dm/community kinds.
// Channel kind is reserved for the channel-introduction phase
// and silently ignored here.
if (kind !== 'dm' && kind !== 'group-dm') return;
```

- [ ] **Step 4: Run tests to verify pass**

```bash
npx vitest run src/lib/nav-service.test.ts
# Expected: all tests pass (5 new + existing regressions)
```

- [ ] **Step 5: Search for any external callers of the old method name**

```bash
grep -rn "addOrUpdateDmSpace" src/
# Expected: no results (the rename is complete)
```

If results found, update them.

- [ ] **Step 6: Run all gates**

```bash
npx tsc --noEmit && npx vitest run
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test && cd ..
# Expected: all green
```

- [ ] **Step 7: Commit**

```bash
git add src/lib/nav-service.ts src/lib/nav-service.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-263): NavService handles kind: community

Renames addOrUpdateDmSpace to addOrUpdateNavSpace and adds the
community branch — creates a community NavNode with type:'community',
expanded by default, no peer attachment, parentId honored for
placement inside user folders. kind:'channel' continues to be
silently ignored pending the channel-introduction phase. Existing
dm/group-dm semantics unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Confirmation primitives

**Files:**
- Create: `src/lib/components/ConfirmationModal.svelte`
- Create: `src/lib/components/TypedConfirmationModal.svelte`
- Test: `src/lib/components/__tests__/ConfirmationModal.test.ts`
- Test: `src/lib/components/__tests__/TypedConfirmationModal.test.ts`

- [ ] **Step 1: Write failing test for ConfirmationModal**

`src/lib/components/__tests__/ConfirmationModal.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import ConfirmationModal from '../ConfirmationModal.svelte';

describe('ConfirmationModal', () => {
  const baseProps = {
    title: 'Kick Bob from IPFS Crew?',
    description: 'Bob will be banned from rejoining.',
    confirmLabel: 'Kick Bob',
    danger: true,
    onConfirm: vi.fn(),
    onCancel: vi.fn(),
  };

  it('renders title, description, and both buttons', () => {
    const { getByText } = render(ConfirmationModal, { props: baseProps });
    expect(getByText('Kick Bob from IPFS Crew?')).toBeTruthy();
    expect(getByText('Bob will be banned from rejoining.')).toBeTruthy();
    expect(getByText('Kick Bob')).toBeTruthy();
    expect(getByText('Cancel')).toBeTruthy();
  });

  it('confirm button is on the LEFT (offset from row-end-right triggers)', () => {
    const { getByText } = render(ConfirmationModal, { props: baseProps });
    const confirmBtn = getByText('Kick Bob').closest('button')!;
    const cancelBtn = getByText('Cancel').closest('button')!;
    // The confirm button must come BEFORE the cancel button in DOM order
    // so it lays out to the left visually (with default LTR flexbox).
    expect(confirmBtn.compareDocumentPosition(cancelBtn) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it('clicking confirm calls onConfirm', async () => {
    const onConfirm = vi.fn();
    const { getByText } = render(ConfirmationModal, { props: { ...baseProps, onConfirm } });
    await fireEvent.click(getByText('Kick Bob'));
    expect(onConfirm).toHaveBeenCalled();
  });

  it('clicking cancel calls onCancel', async () => {
    const onCancel = vi.fn();
    const { getByText } = render(ConfirmationModal, { props: { ...baseProps, onCancel } });
    await fireEvent.click(getByText('Cancel'));
    expect(onCancel).toHaveBeenCalled();
  });

  it('Escape key cancels', async () => {
    const onCancel = vi.fn();
    const { container } = render(ConfirmationModal, { props: { ...baseProps, onCancel } });
    await fireEvent.keyDown(container, { key: 'Escape' });
    expect(onCancel).toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

```bash
npx vitest run src/lib/components/__tests__/ConfirmationModal.test.ts
# Expected: FAIL — component file not found
```

- [ ] **Step 3: Implement ConfirmationModal.svelte**

`src/lib/components/ConfirmationModal.svelte`:

```svelte
<script lang="ts">
  interface Props {
    title: string;
    description: string;
    confirmLabel: string;
    danger?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  }

  let { title, description, confirmLabel, danger = false, onConfirm, onCancel }: Props = $props();

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') onCancel();
  }
</script>

<svelte:window on:keydown={handleKeydown} />

<div class="modal-backdrop" onclick={onCancel} role="presentation">
  <div class="modal" onclick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">
    <h3 class="modal-title">{title}</h3>
    <p class="modal-description">{description}</p>

    <div class="action-row">
      <button class="confirm" class:danger onclick={onConfirm}>{confirmLabel}</button>
      <div class="spacer"></div>
      <button class="cancel" onclick={onCancel}>Cancel</button>
    </div>
  </div>
</div>

<style>
  .modal-backdrop {
    position: fixed; inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex; align-items: center; justify-content: center;
    z-index: 1000;
  }
  .modal {
    background: var(--surface, #1e1e1e);
    border-radius: 8px;
    padding: 20px;
    max-width: 420px;
    width: 90%;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.6);
  }
  .modal-title { margin: 0 0 10px 0; font-size: 15px; }
  .modal-description { margin: 0 0 20px 0; font-size: 13px; color: var(--text-muted, #ccc); }
  .action-row { display: flex; gap: 8px; align-items: center; }
  .spacer { flex: 1; }
  .confirm.danger { background: #cc4a4a; color: white; border-color: #cc4a4a; }
</style>
```

- [ ] **Step 4: Run test to verify it passes**

```bash
npx vitest run src/lib/components/__tests__/ConfirmationModal.test.ts
# Expected: 5 tests pass
```

- [ ] **Step 5: Write failing test for TypedConfirmationModal**

`src/lib/components/__tests__/TypedConfirmationModal.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import TypedConfirmationModal from '../TypedConfirmationModal.svelte';

describe('TypedConfirmationModal', () => {
  const baseProps = {
    title: 'Leave IPFS Crew (you are the only admin)',
    description: 'If you leave, no one can promote new admins.',
    requiredText: 'IPFS Crew',
    confirmLabel: 'Leave anyway',
    onConfirm: vi.fn(),
    onCancel: vi.fn(),
  };

  it('confirm button starts disabled', () => {
    const { getByText } = render(TypedConfirmationModal, { props: baseProps });
    const btn = getByText('Leave anyway').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it('partial typed string keeps button disabled', async () => {
    const { getByText, getByPlaceholderText } = render(TypedConfirmationModal, { props: baseProps });
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'IPFS Cre' } });
    const btn = getByText('Leave anyway').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it('case-mismatched string keeps button disabled', async () => {
    const { getByText, getByPlaceholderText } = render(TypedConfirmationModal, { props: baseProps });
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'ipfs crew' } });
    const btn = getByText('Leave anyway').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it('exact match enables the confirm button', async () => {
    const { getByText, getByPlaceholderText } = render(TypedConfirmationModal, { props: baseProps });
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'IPFS Crew' } });
    const btn = getByText('Leave anyway').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
  });

  it('confirm fires onConfirm when typed string matches and button clicked', async () => {
    const onConfirm = vi.fn();
    const { getByText, getByPlaceholderText } = render(TypedConfirmationModal, {
      props: { ...baseProps, onConfirm },
    });
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'IPFS Crew' } });
    await fireEvent.click(getByText('Leave anyway'));
    expect(onConfirm).toHaveBeenCalled();
  });
});
```

- [ ] **Step 6: Run test to verify it fails**

```bash
npx vitest run src/lib/components/__tests__/TypedConfirmationModal.test.ts
# Expected: FAIL — component file not found
```

- [ ] **Step 7: Implement TypedConfirmationModal.svelte**

`src/lib/components/TypedConfirmationModal.svelte`:

```svelte
<script lang="ts">
  interface Props {
    title: string;
    description: string;
    requiredText: string;
    confirmLabel: string;
    onConfirm: () => void;
    onCancel: () => void;
  }

  let { title, description, requiredText, confirmLabel, onConfirm, onCancel }: Props = $props();
  let typed = $state('');
  let matches = $derived(typed.trimEnd() === requiredText);

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') onCancel();
  }
</script>

<svelte:window on:keydown={handleKeydown} />

<div class="modal-backdrop" onclick={onCancel} role="presentation">
  <div class="modal" onclick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">
    <h3 class="modal-title">⚠ {title}</h3>
    <p class="modal-description">{description}</p>

    <p class="prompt">
      Type <strong class="required">{requiredText}</strong> to confirm:
    </p>
    <input
      type="text"
      bind:value={typed}
      placeholder="Type community name exactly..."
      class="typed-input"
    />

    <div class="action-row">
      <button class="confirm danger" disabled={!matches} onclick={onConfirm}>
        {confirmLabel}
      </button>
      <div class="spacer"></div>
      <button class="cancel" onclick={onCancel}>Cancel</button>
    </div>
  </div>
</div>

<style>
  .modal-backdrop {
    position: fixed; inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex; align-items: center; justify-content: center;
    z-index: 1000;
  }
  .modal {
    background: var(--surface, #1e1e1e);
    border-radius: 8px;
    padding: 20px;
    max-width: 480px;
    width: 90%;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.6);
  }
  .modal-title { margin: 0 0 10px 0; font-size: 15px; color: #cc7a7a; }
  .modal-description { margin: 0 0 16px 0; font-size: 12px; color: #ccc; }
  .prompt { margin: 0 0 6px 0; font-size: 13px; color: #ccc; }
  .required { font-family: monospace; background: #2a2a2a; padding: 1px 6px; border-radius: 3px; color: #eee; }
  .typed-input { width: 100%; margin-bottom: 16px; font-family: monospace; font-size: 13px; padding: 6px 8px; }
  .action-row { display: flex; gap: 8px; align-items: center; }
  .spacer { flex: 1; }
  .confirm.danger { background: #cc4a4a; color: white; border-color: #cc4a4a; }
  .confirm.danger:disabled { background: #553333; color: #888; opacity: 0.6; cursor: not-allowed; }
</style>
```

- [ ] **Step 8: Run all tests**

```bash
npx vitest run src/lib/components/__tests__/TypedConfirmationModal.test.ts src/lib/components/__tests__/ConfirmationModal.test.ts
# Expected: 10 tests pass
```

- [ ] **Step 9: Run all gates**

```bash
npx tsc --noEmit && npx vitest run
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test && cd ..
# Expected: all green
```

- [ ] **Step 10: Commit**

```bash
git add src/lib/components/ConfirmationModal.svelte src/lib/components/TypedConfirmationModal.svelte src/lib/components/__tests__/ConfirmationModal.test.ts src/lib/components/__tests__/TypedConfirmationModal.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-263): tier-2 + tier-3 confirmation modals

ConfirmationModal: tier-2 click-confirm with destructive button on
the LEFT and Cancel on the RIGHT, anchoring opposite to row-end-right
triggers so a stray repeat-tap lands on Cancel. TypedConfirmationModal:
tier-3 typed-string confirmation, button stays disabled until typed
value matches exactly (case-sensitive, trim-trailing-whitespace).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: CreateCommunityDialog + RedeemInviteDialog

**Files:**
- Create: `src/lib/components/CreateCommunityDialog.svelte`
- Create: `src/lib/components/RedeemInviteDialog.svelte`
- Create: `src/lib/redeem-invite-errors.ts` (the variant→summary mapping helper)
- Test: `src/lib/components/__tests__/CreateCommunityDialog.test.ts`
- Test: `src/lib/components/__tests__/RedeemInviteDialog.test.ts`
- Test: `src/lib/__tests__/redeem-invite-errors.test.ts`

- [ ] **Step 1: Write the redeem-invite-errors helper test**

`src/lib/__tests__/redeem-invite-errors.test.ts`:

```typescript
import { describe, it, expect } from 'vitest';
import { mapRedeemInviteError } from '../redeem-invite-errors';

describe('mapRedeemInviteError', () => {
  it('maps BootstrapMissing to "incomplete" summary', () => {
    const r = mapRedeemInviteError('BootstrapMissing: invite-only payload missing admin bootstrap');
    expect(r.summary).toContain('incomplete');
    expect(r.tag).toBe('bootstrap_missing');
  });

  it('maps BootstrapSignatureInvalid to "invalid" summary', () => {
    const r = mapRedeemInviteError('BootstrapSignatureInvalid: ed25519 verify failed');
    expect(r.summary).toContain('signature is invalid');
    expect(r.tag).toBe('bootstrap_signature_invalid');
  });

  it('maps BootstrapAddressMismatch to "malformed" summary', () => {
    const r = mapRedeemInviteError('BootstrapAddressMismatch: identity_pub address != admin_addr');
    expect(r.summary).toContain('malformed');
    expect(r.tag).toBe('bootstrap_address_mismatch');
  });

  it('maps BootstrapActorMismatch', () => {
    const r = mapRedeemInviteError('BootstrapActorMismatch: bootstrap.actor != admin_addr');
    expect(r.tag).toBe('bootstrap_actor_mismatch');
  });

  it('maps BootstrapCommunityMismatch', () => {
    const r = mapRedeemInviteError('BootstrapCommunityMismatch');
    expect(r.summary).toContain('different community');
    expect(r.tag).toBe('bootstrap_community_mismatch');
  });

  it('maps BootstrapKindInvalid', () => {
    const r = mapRedeemInviteError('BootstrapKindInvalid: kind != Join');
    expect(r.tag).toBe('bootstrap_kind_invalid');
  });

  it('maps BootstrapInvalidPubkey', () => {
    const r = mapRedeemInviteError('BootstrapInvalidPubkey: ed25519 from_bytes failed');
    expect(r.tag).toBe('bootstrap_invalid_pubkey');
  });

  it('maps BootstrapInsertFailed', () => {
    const r = mapRedeemInviteError('BootstrapInsertFailed(Foo)');
    expect(r.summary).toContain("Couldn't bootstrap");
    expect(r.tag).toBe('bootstrap_insert_failed');
  });

  it('maps timeout', () => {
    const r = mapRedeemInviteError('redeem_invite timed out after 15s');
    expect(r.summary).toContain('offline');
    expect(r.tag).toBe('inviter_timeout');
  });

  it('maps already-member', () => {
    const r = mapRedeemInviteError('already a member of community aabbccdd');
    expect(r.summary).toContain("already in");
    expect(r.tag).toBe('already_member');
  });

  it('maps malformed URL', () => {
    const r = mapRedeemInviteError('invalid invite URL: missing scheme');
    expect(r.summary).toContain("doesn't look like");
    expect(r.tag).toBe('malformed_url');
  });

  it('falls through to network failure', () => {
    const r = mapRedeemInviteError('network unreachable');
    expect(r.summary).toContain('network');
    expect(r.tag).toBe('network_failure');
  });
});
```

- [ ] **Step 2: Run test to verify failure**

```bash
npx vitest run src/lib/__tests__/redeem-invite-errors.test.ts
# Expected: FAIL — module not found
```

- [ ] **Step 3: Implement redeem-invite-errors.ts**

`src/lib/redeem-invite-errors.ts`:

```typescript
export interface RedeemInviteUserError {
  summary: string;
  hint: string;
  tag: string;
  raw: string;
}

const VARIANT_PATTERNS: Array<{
  match: RegExp;
  summary: string;
  hint: string;
  tag: string;
}> = [
  { match: /BootstrapMissing/i, summary: 'Invite link is incomplete.', hint: 'Ask the inviter to regenerate the link from a recent client build.', tag: 'bootstrap_missing' },
  { match: /BootstrapInvalidPubkey/i, summary: 'Invite link is malformed.', hint: "The embedded admin key isn't valid. Ask the inviter to regenerate.", tag: 'bootstrap_invalid_pubkey' },
  { match: /BootstrapAddressMismatch/i, summary: 'Invite link is malformed.', hint: "Embedded admin keys don't agree with each other. Ask the inviter to regenerate.", tag: 'bootstrap_address_mismatch' },
  { match: /BootstrapActorMismatch/i, summary: 'Invite link is malformed.', hint: 'Bootstrap event was signed by someone other than the admin. Ask the inviter to regenerate.', tag: 'bootstrap_actor_mismatch' },
  { match: /BootstrapCommunityMismatch/i, summary: 'Invite link points to a different community than the one it advertises.', hint: 'The inviter may have a corrupted client. Ask them to reinstall and regenerate.', tag: 'bootstrap_community_mismatch' },
  { match: /BootstrapSignatureInvalid/i, summary: 'Invite link signature is invalid.', hint: 'Either the link was tampered with in transit, or the inviter\'s client is buggy. Ask the inviter to regenerate via a different channel.', tag: 'bootstrap_signature_invalid' },
  { match: /BootstrapKindInvalid/i, summary: 'Invite link contains the wrong event type.', hint: 'Likely a malformed client. Ask the inviter to regenerate.', tag: 'bootstrap_kind_invalid' },
  { match: /BootstrapInsertFailed/i, summary: "Couldn't bootstrap the community on this device.", hint: 'Most likely transient — retry. See diagnostic below.', tag: 'bootstrap_insert_failed' },
  { match: /timed out|timeout/i, summary: 'Inviter is offline — try again later.', hint: 'The community admin needs to be reachable when you redeem. Retry once they\'re back online.', tag: 'inviter_timeout' },
  { match: /already a member/i, summary: "You're already in this community.", hint: '', tag: 'already_member' },
  { match: /invalid invite URL|missing scheme|parse fail/i, summary: "That URL doesn't look like a Harmony invite.", hint: 'Make sure you copied the full URL starting with harmony://invite/.', tag: 'malformed_url' },
];

const FALLBACK = {
  summary: "Couldn't reach the network.",
  hint: 'Check your connection and retry.',
  tag: 'network_failure',
};

export function mapRedeemInviteError(raw: string): RedeemInviteUserError {
  for (const p of VARIANT_PATTERNS) {
    if (p.match.test(raw)) {
      return { summary: p.summary, hint: p.hint, tag: p.tag, raw };
    }
  }
  return { ...FALLBACK, raw };
}
```

- [ ] **Step 4: Run helper test → verify pass**

```bash
npx vitest run src/lib/__tests__/redeem-invite-errors.test.ts
# Expected: 12 tests pass
```

- [ ] **Step 5: Write failing test for CreateCommunityDialog**

`src/lib/components/__tests__/CreateCommunityDialog.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import CreateCommunityDialog from '../CreateCommunityDialog.svelte';

describe('CreateCommunityDialog', () => {
  it('renders name input + kind toggle + Create button', () => {
    const { getByPlaceholderText, getByText } = render(CreateCommunityDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    expect(getByPlaceholderText('Community name')).toBeTruthy();
    expect(getByText('Open')).toBeTruthy();
    expect(getByText('Invite-only')).toBeTruthy();
    expect(getByText('Create')).toBeTruthy();
  });

  it('default kind is invite-only', () => {
    const { getByLabelText } = render(CreateCommunityDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const inviteOnly = getByLabelText('Invite-only') as HTMLInputElement;
    expect(inviteOnly.checked).toBe(true);
  });

  it('Create button disabled when name is empty', () => {
    const { getByText } = render(CreateCommunityDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const btn = getByText('Create').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it('Create button enables when name is non-empty', async () => {
    const { getByText, getByPlaceholderText } = render(CreateCommunityDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const input = getByPlaceholderText('Community name') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'Test Crew' } });
    const btn = getByText('Create').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
  });

  it('submit calls onSubmit with name + kind', async () => {
    const onSubmit = vi.fn();
    const { getByText, getByPlaceholderText } = render(CreateCommunityDialog, {
      props: { onSubmit, onCancel: vi.fn() },
    });
    await fireEvent.input(getByPlaceholderText('Community name'), { target: { value: 'Test Crew' } });
    await fireEvent.click(getByText('Create'));
    expect(onSubmit).toHaveBeenCalledWith('Test Crew', 'invite-only');
  });

  it('switching to Open and submitting passes "open" kind', async () => {
    const onSubmit = vi.fn();
    const { getByText, getByLabelText, getByPlaceholderText } = render(CreateCommunityDialog, {
      props: { onSubmit, onCancel: vi.fn() },
    });
    await fireEvent.input(getByPlaceholderText('Community name'), { target: { value: 'Open Crew' } });
    await fireEvent.click(getByLabelText('Open'));
    await fireEvent.click(getByText('Create'));
    expect(onSubmit).toHaveBeenCalledWith('Open Crew', 'open');
  });

  it('cancel calls onCancel', async () => {
    const onCancel = vi.fn();
    const { getByText } = render(CreateCommunityDialog, {
      props: { onSubmit: vi.fn(), onCancel },
    });
    await fireEvent.click(getByText('Cancel'));
    expect(onCancel).toHaveBeenCalled();
  });
});
```

- [ ] **Step 6: Run test → verify failure**

```bash
npx vitest run src/lib/components/__tests__/CreateCommunityDialog.test.ts
# Expected: FAIL — component not found
```

- [ ] **Step 7: Implement CreateCommunityDialog.svelte**

`src/lib/components/CreateCommunityDialog.svelte`:

```svelte
<script lang="ts">
  interface Props {
    onSubmit: (name: string, kind: 'open' | 'invite-only') => void;
    onCancel: () => void;
    error?: string | null;
    pending?: boolean;
  }

  let { onSubmit, onCancel, error = null, pending = false }: Props = $props();
  let name = $state('');
  let kind = $state<'open' | 'invite-only'>('invite-only');
  let canSubmit = $derived(name.trim().length > 0 && !pending);

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') onCancel();
  }

  function handleSubmit() {
    if (!canSubmit) return;
    onSubmit(name.trim(), kind);
  }
</script>

<svelte:window on:keydown={handleKeydown} />

<div class="modal-backdrop" onclick={onCancel} role="presentation">
  <div class="modal" onclick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">
    <h3>New community</h3>

    <input
      type="text"
      placeholder="Community name"
      bind:value={name}
      class="name-input"
      disabled={pending}
    />

    <div class="kind-row">
      <label>
        <input type="radio" name="kind" value="open" bind:group={kind} disabled={pending} />
        Open
        <span class="hint">Anyone with the URL can join</span>
      </label>
      <label>
        <input type="radio" name="kind" value="invite-only" bind:group={kind} disabled={pending} />
        Invite-only
        <span class="hint">Each invite link works once</span>
      </label>
    </div>

    {#if error}
      <div class="error-banner">{error}</div>
    {/if}

    <div class="action-row">
      <button onclick={onCancel} disabled={pending}>Cancel</button>
      <div class="spacer"></div>
      <button class="primary" onclick={handleSubmit} disabled={!canSubmit}>
        {pending ? 'Creating...' : 'Create'}
      </button>
    </div>
  </div>
</div>

<style>
  .modal-backdrop { position: fixed; inset: 0; background: rgba(0,0,0,0.5); display:flex; align-items:center; justify-content:center; z-index: 1000; }
  .modal { background: var(--surface, #1e1e1e); border-radius: 8px; padding: 20px; max-width: 420px; width: 90%; box-shadow: 0 8px 24px rgba(0,0,0,0.6); }
  h3 { margin: 0 0 16px 0; }
  .name-input { width: 100%; padding: 8px 10px; margin-bottom: 16px; font-size: 14px; }
  .kind-row { display: flex; flex-direction: column; gap: 8px; margin-bottom: 16px; }
  .kind-row label { display: flex; align-items: center; gap: 8px; font-size: 13px; cursor: pointer; }
  .hint { color: #888; font-size: 11px; margin-left: auto; }
  .error-banner { background: #2a1a1a; border: 1px solid #553333; color: #cc7a7a; padding: 8px 10px; border-radius: 4px; font-size: 12px; margin-bottom: 12px; }
  .action-row { display: flex; gap: 8px; align-items: center; }
  .spacer { flex: 1; }
  .primary { background: #4a7cff; color: white; border-color: #4a7cff; }
  .primary:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
```

- [ ] **Step 8: Run test → verify pass**

```bash
npx vitest run src/lib/components/__tests__/CreateCommunityDialog.test.ts
# Expected: 7 tests pass
```

- [ ] **Step 9: Write failing test for RedeemInviteDialog**

`src/lib/components/__tests__/RedeemInviteDialog.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import RedeemInviteDialog from '../RedeemInviteDialog.svelte';

describe('RedeemInviteDialog', () => {
  it('renders URL input and Redeem button', () => {
    const { getByPlaceholderText, getByText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    expect(getByPlaceholderText(/harmony:\/\/invite/)).toBeTruthy();
    expect(getByText('Redeem')).toBeTruthy();
  });

  it('Redeem button disabled until URL contains harmony://invite/', async () => {
    const { getByText, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn() },
    });
    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    let btn = getByText('Redeem').closest('button') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: 'not a url' } });
    expect(btn.disabled).toBe(true);

    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=...' } });
    expect(btn.disabled).toBe(false);
  });

  it('Submit calls onSubmit with the URL', async () => {
    const onSubmit = vi.fn();
    const { getByText, getByPlaceholderText } = render(RedeemInviteDialog, {
      props: { onSubmit, onCancel: vi.fn() },
    });
    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    await fireEvent.input(input, { target: { value: 'harmony://invite/v1?ci=foo' } });
    await fireEvent.click(getByText('Redeem'));
    expect(onSubmit).toHaveBeenCalledWith('harmony://invite/v1?ci=foo');
  });

  it('shows pending spinner when pending=true', () => {
    const { container } = render(RedeemInviteDialog, {
      props: { onSubmit: vi.fn(), onCancel: vi.fn(), pending: true },
    });
    expect(container.querySelector('.spinner')).toBeTruthy();
  });

  it('renders friendly summary + hint when error provided', () => {
    const { getByText } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        error: 'BootstrapSignatureInvalid: ed25519 verify failed',
      },
    });
    expect(getByText(/signature is invalid/i)).toBeTruthy();
  });

  it('disclosure expands to show variant + tag', () => {
    const { getByText, container } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        error: 'BootstrapSignatureInvalid: ed25519 verify failed',
      },
    });
    // Disclosure toggle visible
    expect(getByText(/Show details/i)).toBeTruthy();
    // Tag visible inside disclosure
    expect(container.textContent).toContain('bootstrap_signature_invalid');
  });

  it('preserves URL on error for retry', () => {
    const { getByPlaceholderText } = render(RedeemInviteDialog, {
      props: {
        onSubmit: vi.fn(),
        onCancel: vi.fn(),
        error: 'timed out after 15s',
        initialUrl: 'harmony://invite/v1?ci=foo',
      },
    });
    const input = getByPlaceholderText(/harmony:\/\/invite/) as HTMLTextAreaElement;
    expect(input.value).toBe('harmony://invite/v1?ci=foo');
  });
});
```

- [ ] **Step 10: Run test → verify failure**

```bash
npx vitest run src/lib/components/__tests__/RedeemInviteDialog.test.ts
# Expected: FAIL — component not found
```

- [ ] **Step 11: Implement RedeemInviteDialog.svelte**

`src/lib/components/RedeemInviteDialog.svelte`:

```svelte
<script lang="ts">
  import { mapRedeemInviteError } from '../redeem-invite-errors';

  interface Props {
    onSubmit: (url: string) => void;
    onCancel: () => void;
    error?: string | null;
    pending?: boolean;
    initialUrl?: string;
  }

  let { onSubmit, onCancel, error = null, pending = false, initialUrl = '' }: Props = $props();
  let url = $state(initialUrl);
  let canSubmit = $derived(url.includes('harmony://invite/') && !pending);
  let mapped = $derived(error ? mapRedeemInviteError(error) : null);

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape' && !pending) onCancel();
  }

  function handleSubmit() {
    if (!canSubmit) return;
    onSubmit(url.trim());
  }
</script>

<svelte:window on:keydown={handleKeydown} />

<div class="modal-backdrop" onclick={pending ? null : onCancel} role="presentation">
  <div class="modal" onclick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">
    <h3>Redeem invite link</h3>

    {#if mapped}
      <div class="error-banner">
        <p class="summary">{mapped.summary}</p>
        {#if mapped.hint}<p class="hint">{mapped.hint}</p>{/if}
        <details>
          <summary>Show details</summary>
          <div class="diagnostic">
            <div>Telemetry tag: <code>{mapped.tag}</code></div>
            <div>Raw error: <code>{mapped.raw}</code></div>
          </div>
        </details>
      </div>
    {/if}

    <textarea
      placeholder="harmony://invite/v1?..."
      bind:value={url}
      class="url-input"
      rows="3"
      disabled={pending}
    ></textarea>

    {#if pending}
      <div class="pending-row">
        <div class="spinner"></div>
        <span>Verifying invite...</span>
      </div>
    {/if}

    <div class="action-row">
      <button onclick={onCancel} disabled={pending}>Cancel</button>
      <div class="spacer"></div>
      <button class="primary" onclick={handleSubmit} disabled={!canSubmit}>
        Redeem
      </button>
    </div>
  </div>
</div>

<style>
  .modal-backdrop { position: fixed; inset: 0; background: rgba(0,0,0,0.5); display:flex; align-items:center; justify-content:center; z-index: 1000; }
  .modal { background: var(--surface, #1e1e1e); border-radius: 8px; padding: 20px; max-width: 480px; width: 90%; box-shadow: 0 8px 24px rgba(0,0,0,0.6); }
  h3 { margin: 0 0 16px 0; }
  .url-input { width: 100%; padding: 8px 10px; margin-bottom: 12px; font-family: monospace; font-size: 12px; resize: vertical; }
  .error-banner { background: #2a1a1a; border: 1px solid #553333; padding: 10px 12px; border-radius: 4px; margin-bottom: 12px; }
  .error-banner .summary { margin: 0 0 4px 0; color: #cc7a7a; font-size: 13px; }
  .error-banner .hint { margin: 0 0 8px 0; color: #aaa; font-size: 12px; }
  .error-banner details { font-size: 11px; color: #888; }
  .error-banner details summary { cursor: pointer; }
  .diagnostic { padding: 8px 0 0 0; font-family: monospace; }
  .diagnostic code { color: #ddd; }
  .pending-row { display: flex; align-items: center; gap: 8px; margin-bottom: 12px; color: #aaa; font-size: 12px; }
  .spinner { width: 12px; height: 12px; border: 2px solid #4a7cff; border-top-color: transparent; border-radius: 50%; animation: spin 0.8s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
  .action-row { display: flex; gap: 8px; align-items: center; }
  .spacer { flex: 1; }
  .primary { background: #4a7cff; color: white; border-color: #4a7cff; }
  .primary:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
```

- [ ] **Step 12: Run all tests + gates**

```bash
npx vitest run
# Expected: all tests pass
npx tsc --noEmit
# Expected: 0 errors
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test && cd ..
# Expected: all green
```

- [ ] **Step 13: Commit**

```bash
git add src/lib/components/CreateCommunityDialog.svelte src/lib/components/RedeemInviteDialog.svelte src/lib/redeem-invite-errors.ts src/lib/components/__tests__/CreateCommunityDialog.test.ts src/lib/components/__tests__/RedeemInviteDialog.test.ts src/lib/__tests__/redeem-invite-errors.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-263): create + redeem invite dialogs

CreateCommunityDialog: name input + open/invite-only toggle (default
invite-only) → calls create_community on submit. RedeemInviteDialog:
URL paste field + spinner during pending IPC + friendly error banner
with expandable diagnostic disclosure (variant + reason_tag from
ZEB-260). New helper redeem-invite-errors.ts maps the 12 backend
rejection patterns to user-facing summaries.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: SetPowerDialog + InviteLinkManager

**Files:**
- Create: `src/lib/components/SetPowerDialog.svelte`
- Create: `src/lib/components/InviteLinkManager.svelte`
- Test: `src/lib/components/__tests__/SetPowerDialog.test.ts`
- Test: `src/lib/components/__tests__/InviteLinkManager.test.ts`

- [ ] **Step 1: Write failing test for SetPowerDialog**

`src/lib/components/__tests__/SetPowerDialog.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import SetPowerDialog from '../SetPowerDialog.svelte';

describe('SetPowerDialog', () => {
  const baseProps = {
    targetName: 'Bob',
    targetAddress: 'b1c4...88af',
    currentPower: 0,
    onSubmit: vi.fn(),
    onCancel: vi.fn(),
  };

  it('renders slider and number input synced to currentPower', () => {
    const { container } = render(SetPowerDialog, { props: { ...baseProps, currentPower: 25 } });
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    expect(slider.value).toBe('25');
    expect(numberInput.value).toBe('25');
  });

  it('typing in number input updates the slider', async () => {
    const { container } = render(SetPowerDialog, { props: baseProps });
    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
    await fireEvent.input(numberInput, { target: { value: '75' } });
    expect(slider.value).toBe('75');
  });

  it('moving slider updates number input', async () => {
    const { container } = render(SetPowerDialog, { props: baseProps });
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    await fireEvent.input(slider, { target: { value: '60' } });
    expect(numberInput.value).toBe('60');
  });

  it('role badge shows MEMBER for power < 50', () => {
    const { getByText } = render(SetPowerDialog, { props: { ...baseProps, currentPower: 30 } });
    expect(getByText('MEMBER')).toBeTruthy();
  });

  it('role badge shows MOD for power 50-99', () => {
    const { getByText } = render(SetPowerDialog, { props: { ...baseProps, currentPower: 75 } });
    expect(getByText('MOD')).toBeTruthy();
  });

  it('role badge shows ADMIN for power 100', () => {
    const { getByText } = render(SetPowerDialog, { props: { ...baseProps, currentPower: 100 } });
    expect(getByText('ADMIN')).toBeTruthy();
  });

  it('clamps out-of-range typed value on blur', async () => {
    const { container } = render(SetPowerDialog, { props: baseProps });
    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    await fireEvent.input(numberInput, { target: { value: '500' } });
    await fireEvent.blur(numberInput);
    expect(numberInput.value).toBe('100');
  });

  it('submit calls onSubmit with current power value', async () => {
    const onSubmit = vi.fn();
    const { container, getByText } = render(SetPowerDialog, { props: { ...baseProps, onSubmit } });
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
    await fireEvent.input(slider, { target: { value: '50' } });
    await fireEvent.click(getByText('Set role'));
    expect(onSubmit).toHaveBeenCalledWith(50);
  });
});
```

- [ ] **Step 2: Run test → verify failure**

```bash
npx vitest run src/lib/components/__tests__/SetPowerDialog.test.ts
# Expected: FAIL — component not found
```

- [ ] **Step 3: Implement SetPowerDialog.svelte**

`src/lib/components/SetPowerDialog.svelte`:

```svelte
<script lang="ts">
  import { powerToRole, POWER_THRESHOLDS } from '../types';

  interface Props {
    targetName: string;
    targetAddress: string;
    currentPower: number;
    onSubmit: (power: number) => void;
    onCancel: () => void;
  }

  let { targetName, targetAddress, currentPower, onSubmit, onCancel }: Props = $props();
  let power = $state(currentPower);
  let role = $derived(powerToRole(power));

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') onCancel();
  }

  function clampOnBlur() {
    if (power < 0) power = 0;
    if (power > POWER_THRESHOLDS.max) power = POWER_THRESHOLDS.max;
  }
</script>

<svelte:window on:keydown={handleKeydown} />

<div class="modal-backdrop" onclick={onCancel} role="presentation">
  <div class="modal" onclick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">
    <h3>Set {targetName}'s role</h3>
    <p class="subtitle"><code>{targetAddress}</code> · currently {powerToRole(currentPower)} (power {currentPower})</p>

    <div class="role-preview">
      <span class="role-badge" data-role={role}>{role.toUpperCase()}</span>
    </div>

    <div class="control-row">
      <input type="range" min="0" max="100" step="1" bind:value={power} class="slider" />
      <input type="number" min="0" max="100" step="1" bind:value={power} onblur={clampOnBlur} class="number-input" />
    </div>

    <div class="thresholds">
      <span class="threshold member"><span class="bar">|</span>0<br/>Member</span>
      <span class="threshold mod"><span class="bar">|</span>50<br/>Mod</span>
      <span class="threshold admin"><span class="bar">|</span>100<br/>Admin</span>
    </div>

    <div class="action-row">
      <button onclick={onCancel}>Cancel</button>
      <div class="spacer"></div>
      <button class="primary" onclick={() => onSubmit(power)}>Set role</button>
    </div>
  </div>
</div>

<style>
  .modal-backdrop { position: fixed; inset: 0; background: rgba(0,0,0,0.5); display:flex; align-items:center; justify-content:center; z-index: 1100; }
  .modal { background: var(--surface, #1e1e1e); border-radius: 8px; padding: 20px; max-width: 460px; width: 90%; box-shadow: 0 8px 24px rgba(0,0,0,0.6); }
  h3 { margin: 0 0 4px 0; font-size: 16px; }
  .subtitle { margin: 0 0 16px 0; color: #888; font-size: 12px; }
  .role-preview { text-align: center; margin-bottom: 12px; }
  .role-badge { padding: 3px 14px; border-radius: 12px; font-size: 12px; font-weight: bold; }
  .role-badge[data-role="member"] { background: #666; color: white; }
  .role-badge[data-role="mod"] { background: #ffb84a; color: #1a1a1a; }
  .role-badge[data-role="admin"] { background: #4a7cff; color: white; }
  .control-row { display: flex; align-items: center; gap: 14px; margin-bottom: 6px; }
  .slider { flex: 1; }
  .number-input { width: 64px; background: #0d0d0d; border: 1px solid #4a7cff; border-radius: 4px; padding: 6px 8px; color: #eee; font-size: 14px; text-align: center; font-family: monospace; }
  .thresholds { display: flex; justify-content: space-between; padding: 0 4px; margin-bottom: 24px; font-size: 10px; }
  .threshold { text-align: center; }
  .threshold.member { color: #aaa; }
  .threshold.mod { color: #ffb84a; }
  .threshold.admin { color: #4a7cff; }
  .threshold .bar { display: block; }
  .action-row { display: flex; gap: 8px; align-items: center; }
  .spacer { flex: 1; }
  .primary { background: #4a7cff; color: white; border-color: #4a7cff; }
</style>
```

- [ ] **Step 4: Run test → verify pass**

```bash
npx vitest run src/lib/components/__tests__/SetPowerDialog.test.ts
# Expected: 8 tests pass
```

- [ ] **Step 5: Write failing test for InviteLinkManager**

`src/lib/components/__tests__/InviteLinkManager.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import InviteLinkManager from '../InviteLinkManager.svelte';

describe('InviteLinkManager', () => {
  it('initial state shows Generate button + explanation', () => {
    const { getByText } = render(InviteLinkManager, {
      props: { kind: 'invite-only', onGenerate: vi.fn().mockResolvedValue('harmony://invite/...') },
    });
    expect(getByText(/Generate invite link/i)).toBeTruthy();
  });

  it('clicking Generate calls onGenerate and renders the URL', async () => {
    const onGenerate = vi.fn().mockResolvedValue('harmony://invite/v1?ci=foo');
    const { getByText, container } = render(InviteLinkManager, {
      props: { kind: 'invite-only', onGenerate },
    });
    await fireEvent.click(getByText(/Generate invite link/i));
    // Wait for promise resolution (microtask flush)
    await new Promise((r) => setTimeout(r, 0));
    expect(onGenerate).toHaveBeenCalled();
    expect(container.textContent).toContain('harmony://invite/v1?ci=foo');
  });

  it('Copy button uses clipboard API', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    const onGenerate = vi.fn().mockResolvedValue('harmony://invite/v1?ci=foo');
    const { getByText } = render(InviteLinkManager, {
      props: { kind: 'invite-only', onGenerate },
    });
    await fireEvent.click(getByText(/Generate invite link/i));
    await new Promise((r) => setTimeout(r, 0));
    await fireEvent.click(getByText(/Copy/i));
    expect(writeText).toHaveBeenCalledWith('harmony://invite/v1?ci=foo');
  });

  it('Regenerate replaces the visible URL', async () => {
    const onGenerate = vi
      .fn()
      .mockResolvedValueOnce('harmony://invite/v1?ci=first')
      .mockResolvedValueOnce('harmony://invite/v1?ci=second');
    const { getByText, container } = render(InviteLinkManager, {
      props: { kind: 'invite-only', onGenerate },
    });
    await fireEvent.click(getByText(/Generate invite link/i));
    await new Promise((r) => setTimeout(r, 0));
    expect(container.textContent).toContain('first');
    await fireEvent.click(getByText(/Regenerate/i));
    await new Promise((r) => setTimeout(r, 0));
    expect(container.textContent).toContain('second');
    expect(container.textContent).not.toContain('first');
  });

  it('shows different warning text for open vs invite-only', () => {
    const { getByText, rerender } = render(InviteLinkManager, {
      props: { kind: 'invite-only', onGenerate: vi.fn() },
    });
    expect(getByText(/admin bootstrap signature/i)).toBeTruthy();

    rerender({ kind: 'open', onGenerate: vi.fn() });
    expect(getByText(/Anyone with this URL/i)).toBeTruthy();
  });
});
```

- [ ] **Step 6: Run test → verify failure**

```bash
npx vitest run src/lib/components/__tests__/InviteLinkManager.test.ts
# Expected: FAIL — component not found
```

- [ ] **Step 7: Implement InviteLinkManager.svelte**

`src/lib/components/InviteLinkManager.svelte`:

```svelte
<script lang="ts">
  interface Props {
    kind: 'open' | 'invite-only';
    onGenerate: () => Promise<string>;
  }

  let { kind, onGenerate }: Props = $props();
  let url = $state<string | null>(null);
  let pending = $state(false);
  let copied = $state(false);

  async function handleGenerate() {
    pending = true;
    try {
      url = await onGenerate();
    } finally {
      pending = false;
    }
  }

  async function handleCopy() {
    if (!url) return;
    await navigator.clipboard.writeText(url);
    copied = true;
    setTimeout(() => (copied = false), 2000);
  }
</script>

<div class="invite-manager">
  {#if !url}
    <p class="explanation">Generate a one-time invite link to share via DM, email, or any side channel.</p>
    <button class="primary" onclick={handleGenerate} disabled={pending}>
      {pending ? 'Generating...' : '+ Generate invite link'}
    </button>
  {:else}
    {#if kind === 'invite-only'}
      <p class="warning">Don't post publicly — it embeds your admin bootstrap signature. Each link can only be redeemed once.</p>
    {:else}
      <p class="warning">Anyone with this URL can join. The same link works indefinitely.</p>
    {/if}

    <div class="url-row">
      <code class="url">{url}</code>
      <button class="primary copy" onclick={handleCopy}>
        {copied ? '✓ Copied' : '📋 Copy'}
      </button>
    </div>

    <div class="actions">
      <button onclick={handleGenerate} disabled={pending}>↻ Regenerate</button>
    </div>
  {/if}
</div>

<style>
  .invite-manager { font-size: 13px; }
  .explanation { color: #aaa; font-size: 12px; margin: 0 0 12px 0; }
  .warning { color: #ffb84a; font-size: 12px; margin: 0 0 12px 0; }
  .url-row { display: flex; align-items: center; gap: 10px; background: #0d0d0d; border: 1px solid #333; border-radius: 6px; padding: 10px 12px; margin-bottom: 12px; }
  .url { flex: 1; font-size: 11px; color: #aaa; font-family: monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .actions { display: flex; gap: 8px; }
  .primary { background: #4a7cff; color: white; border-color: #4a7cff; padding: 6px 14px; }
  .primary.copy { padding: 4px 10px; font-size: 11px; }
</style>
```

- [ ] **Step 8: Run all gates**

```bash
npx tsc --noEmit && npx vitest run
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test && cd ..
# Expected: all green
```

- [ ] **Step 9: Commit**

```bash
git add src/lib/components/SetPowerDialog.svelte src/lib/components/InviteLinkManager.svelte src/lib/components/__tests__/SetPowerDialog.test.ts src/lib/components/__tests__/InviteLinkManager.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-263): set-power dialog + invite-link manager

SetPowerDialog: bidirectionally-synced slider + number input (per
accessibility feedback — sliders alone exclude users without precise
motor control), threshold annotations at 0/50/100, live role badge
preview. InviteLinkManager: generate-on-demand + copy-to-clipboard +
regenerate, with kind-specific warning copy ("don't post publicly"
for invite-only, "anyone can join" for open).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: CommunitySettingsPanel

**Files:**
- Create: `src/lib/components/CommunitySettingsPanel.svelte`
- Test: `src/lib/components/__tests__/CommunitySettingsPanel.test.ts`

- [ ] **Step 1: Write failing tests (large suite — see all tests in single file)**

`src/lib/components/__tests__/CommunitySettingsPanel.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import CommunitySettingsPanel from '../CommunitySettingsPanel.svelte';
import type { CommunityMember } from '../../types';

const adminMember: CommunityMember = { address: 'a3f8c1d2...', displayName: 'Alice', power: 100, status: 'joined' };
const modMember: CommunityMember = { address: 'cc99...', displayName: 'Charlie', power: 50, status: 'joined' };
const plainMember: CommunityMember = { address: 'b1c4...', displayName: 'Bob', power: 0, status: 'joined' };

const baseProps = {
  communityId: 'aabbccdd' + 'ee'.repeat(28),
  communityName: 'IPFS Crew',
  communityKind: 'invite-only' as const,
  members: [adminMember, modMember, plainMember],
  myAddress: adminMember.address,
  myPower: 100,
  isDegraded: false,
  onClose: vi.fn(),
  onKick: vi.fn(),
  onSetPower: vi.fn(),
  onLeave: vi.fn(),
  onGenerateInvite: vi.fn().mockResolvedValue('harmony://invite/...'),
};

describe('CommunitySettingsPanel', () => {
  it('renders Info / Members / Invites / Danger sections', () => {
    const { getByText } = render(CommunitySettingsPanel, { props: baseProps });
    expect(getByText('Info')).toBeTruthy();
    expect(getByText(/Members/)).toBeTruthy();
    expect(getByText('Invites')).toBeTruthy();
    expect(getByText(/Danger/)).toBeTruthy();
  });

  it('Info section shows community name, kind, member count, your role', () => {
    const { getByText } = render(CommunitySettingsPanel, { props: baseProps });
    expect(getByText('IPFS Crew')).toBeTruthy();
    expect(getByText(/Invite-only/i)).toBeTruthy();
    expect(getByText(/3 joined/i)).toBeTruthy();
    expect(getByText('ADMIN')).toBeTruthy();
  });

  it('shows degraded sync status when isDegraded is true', () => {
    const { getByText } = render(CommunitySettingsPanel, { props: { ...baseProps, isDegraded: true } });
    expect(getByText(/Degraded/i)).toBeTruthy();
  });

  it('shows healthy sync status by default', () => {
    const { getByText } = render(CommunitySettingsPanel, { props: baseProps });
    expect(getByText(/Healthy/i)).toBeTruthy();
  });

  it('renders kick + set-role buttons for non-self members when caller has power', () => {
    const { container } = render(CommunitySettingsPanel, { props: baseProps });
    // Bob's row should have both buttons (caller is admin, target is plain member)
    const rows = container.querySelectorAll('.member-row');
    const bobRow = Array.from(rows).find((r) => r.textContent?.includes('Bob'));
    expect(bobRow?.querySelector('button.kick')).toBeTruthy();
    expect(bobRow?.querySelector('button.set-role')).toBeTruthy();
  });

  it('does NOT render kick/set-role buttons on the caller\'s own row', () => {
    const { container } = render(CommunitySettingsPanel, { props: baseProps });
    const aliceRow = Array.from(container.querySelectorAll('.member-row')).find((r) =>
      r.textContent?.includes('Alice'),
    );
    expect(aliceRow?.querySelector('button.kick')).toBeFalsy();
    expect(aliceRow?.querySelector('button.set-role')).toBeFalsy();
  });

  it('does NOT render kick when caller power <= target power', () => {
    const { container } = render(CommunitySettingsPanel, {
      props: { ...baseProps, myPower: 50, myAddress: modMember.address },
    });
    const rows = container.querySelectorAll('.member-row');
    const aliceRow = Array.from(rows).find((r) => r.textContent?.includes('Alice'));
    // Charlie (power 50) cannot kick Alice (power 100)
    expect(aliceRow?.querySelector('button.kick')).toBeFalsy();
  });

  it('does NOT render any action buttons when caller is plain member', () => {
    const { container } = render(CommunitySettingsPanel, {
      props: { ...baseProps, myPower: 0, myAddress: plainMember.address },
    });
    expect(container.querySelectorAll('button.kick').length).toBe(0);
    expect(container.querySelectorAll('button.set-role').length).toBe(0);
  });

  it('Kick button opens tier-2 confirmation', async () => {
    const { container, getByText } = render(CommunitySettingsPanel, { props: baseProps });
    const rows = container.querySelectorAll('.member-row');
    const bobRow = Array.from(rows).find((r) => r.textContent?.includes('Bob'))!;
    const kickBtn = bobRow.querySelector('button.kick') as HTMLButtonElement;
    await fireEvent.click(kickBtn);
    expect(getByText(/Kick Bob/i)).toBeTruthy();
  });

  it('Leave with other admins opens tier-2 confirmation', async () => {
    const otherAdmin: CommunityMember = { address: 'dd99...', displayName: 'Diana', power: 100, status: 'joined' };
    const { getByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, members: [...baseProps.members, otherAdmin] },
    });
    await fireEvent.click(getByText(/Leave community/i));
    // Should NOT trigger typed-confirm (other admin exists)
    expect(getByText(/Leave IPFS Crew/i)).toBeTruthy();
  });

  it('Leave as only admin opens tier-3 typed-confirmation', async () => {
    const { getByText, getByPlaceholderText } = render(CommunitySettingsPanel, { props: baseProps });
    await fireEvent.click(getByText(/Leave community/i));
    expect(getByPlaceholderText(/Type community name/i)).toBeTruthy();
  });
});
```

- [ ] **Step 2: Run test → verify failure**

```bash
npx vitest run src/lib/components/__tests__/CommunitySettingsPanel.test.ts
# Expected: FAIL — component not found
```

- [ ] **Step 3: Implement CommunitySettingsPanel.svelte**

`src/lib/components/CommunitySettingsPanel.svelte`:

```svelte
<script lang="ts">
  import { POWER_THRESHOLDS, powerToRole, type CommunityMember } from '../types';
  import ConfirmationModal from './ConfirmationModal.svelte';
  import TypedConfirmationModal from './TypedConfirmationModal.svelte';
  import SetPowerDialog from './SetPowerDialog.svelte';
  import InviteLinkManager from './InviteLinkManager.svelte';

  interface Props {
    communityId: string;
    communityName: string;
    communityKind: 'open' | 'invite-only';
    members: CommunityMember[];
    myAddress: string;
    myPower: number;
    isDegraded: boolean;
    onClose: () => void;
    onKick: (targetAddr: string) => void;
    onSetPower: (targetAddr: string, newPower: number) => void;
    onLeave: () => void;
    onGenerateInvite: () => Promise<string>;
  }

  let {
    communityId, communityName, communityKind, members, myAddress, myPower,
    isDegraded, onClose, onKick, onSetPower, onLeave, onGenerateInvite,
  }: Props = $props();

  let kickTarget = $state<CommunityMember | null>(null);
  let setPowerTarget = $state<CommunityMember | null>(null);
  let leaveOpen = $state(false);

  let joinedMembers = $derived(members.filter((m) => m.status === 'joined'));
  let adminCount = $derived(joinedMembers.filter((m) => m.power >= POWER_THRESHOLDS.setPower).length);
  let amOnlyAdmin = $derived(myPower >= POWER_THRESHOLDS.setPower && adminCount === 1);
  let myRole = $derived(powerToRole(myPower));

  function canKick(target: CommunityMember): boolean {
    return target.address !== myAddress
      && myPower >= POWER_THRESHOLDS.kick
      && myPower > target.power;
  }

  function canSetPower(target: CommunityMember): boolean {
    return target.address !== myAddress
      && myPower >= POWER_THRESHOLDS.setPower
      && myPower > target.power;
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape' && !kickTarget && !setPowerTarget && !leaveOpen) onClose();
  }
</script>

<svelte:window on:keydown={handleKeydown} />

<div class="modal-backdrop" onclick={onClose} role="presentation">
  <div class="modal" onclick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">

    <div class="header">
      <div>
        <h3>Manage community</h3>
        <div class="subtitle">{communityName}</div>
      </div>
      <button class="close-btn" onclick={onClose} aria-label="Close">✕</button>
    </div>

    <div class="section">
      <div class="section-label">Info</div>
      <div class="info-grid">
        <div class="key">Name</div><div>{communityName}</div>
        <div class="key">Type</div><div>{communityKind === 'invite-only' ? '🔒 Invite-only' : '🌐 Open'}</div>
        <div class="key">Members</div><div>{joinedMembers.length} joined</div>
        <div class="key">Your role</div>
        <div>
          <span class="role-badge" data-role={myRole}>{myRole.toUpperCase()}</span>
          (power {myPower})
        </div>
        <div class="key">Sync status</div>
        <div class={isDegraded ? 'degraded' : 'healthy'}>
          {isDegraded ? '⚠ Degraded — pending events not yet visible' : '● Healthy'}
        </div>
      </div>
    </div>

    <div class="section">
      <div class="section-label">Members ({joinedMembers.length})</div>
      <div class="member-list">
        {#each joinedMembers as m (m.address)}
          <div class="member-row">
            <div class="avatar">{(m.displayName ?? m.address).slice(0, 1).toUpperCase()}</div>
            <div class="member-name">
              <div class="name">{m.displayName ?? m.address.slice(0, 8)}{m.address === myAddress ? ' (you)' : ''}</div>
              <div class="addr">{m.address}</div>
            </div>
            <span class="role-badge" data-role={powerToRole(m.power)}>{powerToRole(m.power).toUpperCase()}</span>
            {#if canSetPower(m)}
              <button class="set-role" onclick={() => (setPowerTarget = m)}>Set role</button>
            {/if}
            {#if canKick(m)}
              <button class="kick" onclick={() => (kickTarget = m)}>Kick</button>
            {/if}
          </div>
        {/each}
      </div>
    </div>

    {#if myPower >= POWER_THRESHOLDS.invite}
      <div class="section">
        <div class="section-label">Invites</div>
        <InviteLinkManager kind={communityKind} {onGenerate={onGenerateInvite}} />
      </div>
    {/if}

    <div class="section">
      <div class="section-label">Danger zone</div>
      <button class="danger" onclick={() => (leaveOpen = true)}>Leave community</button>
      {#if amOnlyAdmin}
        <p class="hint">As the only admin, leaving will leave the community without an admin until another member is promoted.</p>
      {/if}
    </div>

  </div>
</div>

{#if kickTarget}
  <ConfirmationModal
    title={`Kick ${kickTarget.displayName ?? kickTarget.address.slice(0, 8)} from ${communityName}?`}
    description="They will be banned from rejoining. A future admin can re-invite them, but kick events can't be undone."
    confirmLabel={`Kick ${kickTarget.displayName ?? 'member'}`}
    danger={true}
    onConfirm={() => { onKick(kickTarget!.address); kickTarget = null; }}
    onCancel={() => (kickTarget = null)}
  />
{/if}

{#if setPowerTarget}
  <SetPowerDialog
    targetName={setPowerTarget.displayName ?? setPowerTarget.address.slice(0, 8)}
    targetAddress={setPowerTarget.address}
    currentPower={setPowerTarget.power}
    onSubmit={(newPower) => { onSetPower(setPowerTarget!.address, newPower); setPowerTarget = null; }}
    onCancel={() => (setPowerTarget = null)}
  />
{/if}

{#if leaveOpen && amOnlyAdmin}
  <TypedConfirmationModal
    title={`Leave ${communityName} (you're the only admin)`}
    description="If you leave, no one can promote new admins, kick disruptive members, or generate new invite links. The community CRDT will persist on the network but become permanently ungoverned. Promote another member to admin first if you want to hand off control."
    requiredText={communityName}
    confirmLabel="Leave anyway"
    onConfirm={() => { onLeave(); leaveOpen = false; }}
    onCancel={() => (leaveOpen = false)}
  />
{:else if leaveOpen}
  <ConfirmationModal
    title={`Leave ${communityName}?`}
    description="You will lose access. You can rejoin via invite later if available."
    confirmLabel="Leave"
    danger={true}
    onConfirm={() => { onLeave(); leaveOpen = false; }}
    onCancel={() => (leaveOpen = false)}
  />
{/if}

<style>
  .modal-backdrop { position: fixed; inset: 0; background: rgba(0,0,0,0.5); display:flex; align-items:flex-start; justify-content:center; z-index: 900; overflow-y: auto; padding: 32px 16px; }
  .modal { background: var(--surface, #1e1e1e); border-radius: 8px; max-width: 640px; width: 100%; box-shadow: 0 8px 24px rgba(0,0,0,0.6); }
  .header { padding: 16px 20px; border-bottom: 1px solid #333; display: flex; align-items: center; justify-content: space-between; }
  .header h3 { margin: 0; }
  .subtitle { color: #888; font-size: 12px; }
  .close-btn { font-size: 18px; padding: 4px 10px; background: transparent; }
  .section { padding: 18px 20px; border-bottom: 1px solid #2a2a2a; }
  .section:last-child { border-bottom: none; }
  .section-label { font-size: 11px; color: #888; text-transform: uppercase; letter-spacing: 0.5px; margin-bottom: 12px; }
  .info-grid { display: grid; grid-template-columns: 120px 1fr; gap: 10px 16px; font-size: 13px; color: #ccc; }
  .info-grid .key { color: #888; }
  .role-badge { padding: 1px 7px; border-radius: 8px; font-size: 9px; font-weight: bold; }
  .role-badge[data-role="member"] { background: #666; color: white; }
  .role-badge[data-role="mod"] { background: #ffb84a; color: #1a1a1a; }
  .role-badge[data-role="admin"] { background: #4a7cff; color: white; }
  .healthy { color: #7acc7a; }
  .degraded { color: #ffb84a; }
  .member-list { display: flex; flex-direction: column; }
  .member-row { display: flex; align-items: center; padding: 6px 6px; gap: 10px; border-bottom: 1px solid #2a2a2a; }
  .member-row:last-child { border-bottom: none; }
  .avatar { width: 28px; height: 28px; border-radius: 50%; background: #4a7cff; color: white; display: flex; align-items: center; justify-content: center; font-size: 12px; font-weight: bold; }
  .member-name { flex: 1; }
  .member-name .name { color: #eee; font-size: 13px; }
  .member-name .addr { font-size: 10px; color: #888; font-family: monospace; }
  .set-role, .kick { font-size: 10px; padding: 2px 7px; }
  .set-role { color: #aaa; background: #2a2a2a; border-color: #444; }
  .kick { color: #cc7a7a; background: #2a1a1a; border-color: #553333; }
  .danger { color: #ff7a7a; border-color: #7a3a3a; padding: 6px 14px; }
  .hint { font-size: 11px; color: #666; margin: 8px 0 0 0; }
</style>
```

- [ ] **Step 4: Run test → verify pass**

```bash
npx vitest run src/lib/components/__tests__/CommunitySettingsPanel.test.ts
# Expected: 11 tests pass
```

- [ ] **Step 5: Run all gates**

```bash
npx tsc --noEmit && npx vitest run
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test && cd ..
# Expected: all green
```

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/CommunitySettingsPanel.svelte src/lib/components/__tests__/CommunitySettingsPanel.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-263): community settings panel

CommunitySettingsPanel: 4-section modal (Info / Members / Invites /
Danger). Member rows render Set-role and Kick action buttons gated
by POWER_THRESHOLDS comparisons against caller's power; never on
caller's own row. Leave routes to TypedConfirmationModal when caller
is the only admin (community would become ungoverned), else to the
tier-2 ConfirmationModal. Sync-status surfaces degraded events for
pre-1.0 prototype visibility.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: NavPanel FAB + App.svelte wiring

**Files:**
- Modify: `src/lib/components/NavPanel.svelte`
- Modify: `src/App.svelte`
- Test: `src/lib/components/__tests__/NavPanel.test.ts` (new — NavPanel may not have existing tests)

- [ ] **Step 1: Write failing test for NavPanel FAB + fan-out menu**

`src/lib/components/__tests__/NavPanel.test.ts` (or extend existing):

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import NavPanel from '../NavPanel.svelte';

const baseProps = {
  nodes: [],
  selectedId: null,
  onSelect: vi.fn(),
  onNewDm: vi.fn(),
  onNewGroupDm: vi.fn(),
  onNewCommunity: vi.fn(),
  onRedeemInvite: vi.fn(),
  appMode: 'messages' as const,
  setAppMode: vi.fn(),
};

describe('NavPanel — FAB + fan-out menu (ZEB-263)', () => {
  it('renders the "+" FAB button', () => {
    const { getByLabelText } = render(NavPanel, { props: baseProps });
    expect(getByLabelText(/Create new/i)).toBeTruthy();
  });

  it('clicking "+" opens the fan-out menu', async () => {
    const { getByLabelText, getByText } = render(NavPanel, { props: baseProps });
    await fireEvent.click(getByLabelText(/Create new/i));
    expect(getByText(/New direct message/i)).toBeTruthy();
    expect(getByText(/New group DM/i)).toBeTruthy();
    expect(getByText(/New community/i)).toBeTruthy();
    expect(getByText(/Redeem invite/i)).toBeTruthy();
  });

  it('clicking "New community" calls onNewCommunity', async () => {
    const onNewCommunity = vi.fn();
    const { getByLabelText, getByText } = render(NavPanel, {
      props: { ...baseProps, onNewCommunity },
    });
    await fireEvent.click(getByLabelText(/Create new/i));
    await fireEvent.click(getByText(/New community/i));
    expect(onNewCommunity).toHaveBeenCalled();
  });

  it('clicking "Redeem invite" calls onRedeemInvite', async () => {
    const onRedeemInvite = vi.fn();
    const { getByLabelText, getByText } = render(NavPanel, {
      props: { ...baseProps, onRedeemInvite },
    });
    await fireEvent.click(getByLabelText(/Create new/i));
    await fireEvent.click(getByText(/Redeem invite/i));
    expect(onRedeemInvite).toHaveBeenCalled();
  });

  it('Escape closes the popover', async () => {
    const { getByLabelText, queryByText, container } = render(NavPanel, { props: baseProps });
    await fireEvent.click(getByLabelText(/Create new/i));
    expect(queryByText(/New community/i)).toBeTruthy();
    await fireEvent.keyDown(container, { key: 'Escape' });
    expect(queryByText(/New community/i)).toBeNull();
  });

  it('clicking outside closes the popover', async () => {
    const { getByLabelText, queryByText } = render(NavPanel, { props: baseProps });
    await fireEvent.click(getByLabelText(/Create new/i));
    expect(queryByText(/New community/i)).toBeTruthy();
    await fireEvent.click(document.body);
    expect(queryByText(/New community/i)).toBeNull();
  });

  it('renders community-kind nodes as collapsible-folder-like', () => {
    const { container } = render(NavPanel, {
      props: {
        ...baseProps,
        nodes: [
          { id: 'aabb', type: 'community', name: 'IPFS Crew', parentId: null, expanded: true, unreadCount: 0, unreadLevel: 'none' },
        ],
      },
    });
    const node = container.querySelector('[data-node-type="community"]');
    expect(node).toBeTruthy();
    expect(node?.querySelector('.chevron')).toBeTruthy();
  });
});
```

- [ ] **Step 2: Run test → verify failure**

```bash
npx vitest run src/lib/components/__tests__/NavPanel.test.ts
# Expected: FAIL — props/structure don't match
```

- [ ] **Step 3: Add FAB + fan-out menu to NavPanel.svelte**

Modify `src/lib/components/NavPanel.svelte`. Inside the script, add:

```typescript
// Add to existing script section:
let menuOpen = $state(false);
let menuButtonEl = $state<HTMLButtonElement | null>(null);

interface Props {
  // ... existing props ...
  onNewDm: () => void;
  onNewGroupDm: () => void;
  onNewCommunity: () => void;
  onRedeemInvite: () => void;
}

function handleDocClick(e: MouseEvent) {
  if (!menuOpen) return;
  if (menuButtonEl?.contains(e.target as Node)) return;
  menuOpen = false;
}

function handleKeydown(e: KeyboardEvent) {
  if (e.key === 'Escape' && menuOpen) menuOpen = false;
}

$effect(() => {
  if (menuOpen) {
    document.addEventListener('click', handleDocClick);
    return () => document.removeEventListener('click', handleDocClick);
  }
});
```

Inside the existing toolbar (after the mode-toggle buttons), add:

```svelte
<svelte:window on:keydown={handleKeydown} />

<span class="divider"></span>
<button
  bind:this={menuButtonEl}
  class="fab-btn"
  aria-label="Create new"
  aria-expanded={menuOpen}
  onclick={() => (menuOpen = !menuOpen)}
>+</button>

{#if menuOpen}
  <div class="fab-popover" role="menu">
    <button role="menuitem" onclick={() => { menuOpen = false; onNewDm(); }}>💬 New direct message</button>
    <button role="menuitem" onclick={() => { menuOpen = false; onNewGroupDm(); }}>👥 New group DM</button>
    <hr />
    <button role="menuitem" onclick={() => { menuOpen = false; onNewCommunity(); }}>🏛️ New community</button>
    <button role="menuitem" onclick={() => { menuOpen = false; onRedeemInvite(); }}>🔗 Redeem invite link</button>
  </div>
{/if}
```

For community-node rendering, add to the existing tree-rendering loop a branch for `node.type === 'community'`:

```svelte
{#if node.type === 'community'}
  <div class="nav-node community" data-node-type="community" onclick={() => onSelect(node.id)}>
    <span class="chevron" onclick={(e) => { e.stopPropagation(); toggleExpand(node.id); }}>
      {node.expanded ? '▾' : '▸'}
    </span>
    <span class="kind-icon">{communityKindIcon(node)}</span>
    <span class="name">{node.name}</span>
    {#if node.memberCount > 0}<span class="badge">{node.memberCount}</span>{/if}
  </div>
{/if}
```

(The implementer should refer to existing nav-rendering structure for exact integration patterns; the overall shape of NavPanel.svelte may already use specific DOM patterns and should be preserved.)

Add corresponding CSS:

```css
.divider { width: 1px; height: 18px; background: #444; margin: 0 4px; }
.fab-btn { font-size: 14px; padding: 4px 10px; background: #4a7cff; color: white; }
.fab-popover { position: absolute; right: 12px; top: 42px; background: #222; border: 1px solid #4a7cff; border-radius: 6px; padding: 4px; min-width: 200px; box-shadow: 0 4px 12px rgba(0,0,0,0.5); z-index: 50; display: flex; flex-direction: column; }
.fab-popover button { padding: 8px 12px; background: transparent; border: none; color: #eee; text-align: left; cursor: pointer; border-radius: 3px; }
.fab-popover button:hover { background: #333; }
.fab-popover hr { border: none; border-top: 1px solid #444; margin: 4px 0; }
```

- [ ] **Step 4: Run NavPanel test → verify pass**

```bash
npx vitest run src/lib/components/__tests__/NavPanel.test.ts
# Expected: 7 tests pass
```

- [ ] **Step 5: Wire dialogs + service in App.svelte**

Modify `src/App.svelte`:

1. Import the new components and service:

```typescript
import CreateCommunityDialog from './lib/components/CreateCommunityDialog.svelte';
import RedeemInviteDialog from './lib/components/RedeemInviteDialog.svelte';
import CommunitySettingsPanel from './lib/components/CommunitySettingsPanel.svelte';
import { CommunityService } from './lib/community-service';
```

2. Add state:

```typescript
const communityService = new CommunityService();
let showCreateCommunity = $state(false);
let showRedeemInvite = $state(false);
let createPending = $state(false);
let createError = $state<string | null>(null);
let redeemPending = $state(false);
let redeemError = $state<string | null>(null);
let redeemUrl = $state('');
let selectedCommunityId = $state<string | null>(null);
let communityMembers = $state<CommunityMember[]>([]);
```

3. On Tauri-connect (in the existing connect-effect), add:

```typescript
await communityService.connectAdapter(adapter);
communityService.onChange = async () => {
  if (selectedCommunityId) {
    communityMembers = await communityService.listMembers(selectedCommunityId);
  }
};
```

4. Wire NavPanel callbacks:

```svelte
<NavPanel
  ...
  onNewDm={() => (showDmCreate = true)}
  onNewGroupDm={() => (showDmCreate = true)}
  onNewCommunity={() => (showCreateCommunity = true)}
  onRedeemInvite={() => (showRedeemInvite = true)}
/>
```

5. Render the dialogs:

```svelte
{#if showCreateCommunity}
  <CreateCommunityDialog
    pending={createPending}
    error={createError}
    onSubmit={async (name, kind) => {
      createPending = true; createError = null;
      try {
        const id = await communityService.createCommunity(name, kind);
        showCreateCommunity = false;
        selectedCommunityId = id;
        communityMembers = await communityService.listMembers(id);
      } catch (e) {
        createError = e instanceof Error ? e.message : String(e);
      } finally { createPending = false; }
    }}
    onCancel={() => (showCreateCommunity = false)}
  />
{/if}

{#if showRedeemInvite}
  <RedeemInviteDialog
    pending={redeemPending}
    error={redeemError}
    initialUrl={redeemUrl}
    onSubmit={async (url) => {
      redeemPending = true; redeemError = null; redeemUrl = url;
      try {
        const id = await communityService.redeemInvite(url);
        showRedeemInvite = false;
        redeemUrl = '';
        selectedCommunityId = id;
        communityMembers = await communityService.listMembers(id);
      } catch (e) {
        redeemError = e instanceof Error ? e.message : String(e);
      } finally { redeemPending = false; }
    }}
    onCancel={() => { showRedeemInvite = false; redeemUrl = ''; redeemError = null; }}
  />
{/if}
```

6. Render the right-pane overview placeholder when a community is selected (the implementer should integrate this with the existing right-pane routing logic):

```svelte
{#if selectedNode?.type === 'community'}
  <div class="community-overview">
    <h3>{selectedNode.name}</h3>
    <p>{communityKind === 'invite-only' ? '🔒 Invite-only' : '🌐 Open'} · {communityMembers.length} members</p>
    <p class="muted">No channels yet — channels arrive in a later phase. Until then, manage members and invites here.</p>
    <button onclick={() => (showSettingsPanel = true)}>Manage community</button>
  </div>
{/if}

{#if showSettingsPanel && selectedCommunityId}
  <CommunitySettingsPanel
    communityId={selectedCommunityId}
    communityName={selectedNode!.name}
    communityKind={communityKind}
    members={communityMembers}
    myAddress={ownAddress}
    myPower={myPowerInCommunity}
    isDegraded={communityService.isDegraded(selectedCommunityId)}
    onClose={() => (showSettingsPanel = false)}
    onKick={async (target) => { await communityService.kickMember(selectedCommunityId!, target); }}
    onSetPower={async (target, power) => { await communityService.setPowerLevel(selectedCommunityId!, target, power); }}
    onLeave={async () => {
      await communityService.leaveCommunity(selectedCommunityId!);
      selectedCommunityId = null;
      showSettingsPanel = false;
    }}
    onGenerateInvite={() => communityService.generateInvite(selectedCommunityId!)}
  />
{/if}
```

7. Cleanup on unmount:

```typescript
$effect(() => () => communityService.destroy());
```

- [ ] **Step 6: Run all gates**

```bash
npx tsc --noEmit && npx vitest run
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test && cd ..
# Expected: all green
```

If `tsc` complains about prop shapes or missing service methods, address inline — the wiring above is illustrative; exact integration with App.svelte's existing event flow and `selectedNode` derivation may need adaptation.

- [ ] **Step 7: Commit**

```bash
git add src/lib/components/NavPanel.svelte src/lib/components/__tests__/NavPanel.test.ts src/App.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-263): NavPanel FAB + App wiring

NavPanel grows a global "+" FAB right of the existing mode-toggle
row, opening a 4-item fan-out menu (DM / Group DM / Community /
Redeem invite) split by a divider between DM and community
sections. Community-kind nav nodes render as collapsible-folder-like
with a kind icon and member-count badge. App.svelte mounts
CommunityService, wires all 4 dialogs + CommunitySettingsPanel, and
routes community-node-clicks to a right-pane overview placeholder
with a Manage button that opens the settings modal.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: Final verification + push + PR

**Files:** None modified.

- [ ] **Step 1: Final all-gate sweep**

```bash
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test && cd ..
npx tsc --noEmit
npx vitest run
# Expected: all green across all gates
```

If anything fails: STOP. Fix in a new commit on the branch (do not amend prior commits).

- [ ] **Step 2: Manual smoke (best-effort if hardware available)**

If two Tauri devices are available:

1. Device A (Alice): Click "+" → "New community" → name "Smoke Test", kind invite-only → Create.
2. Device A: Click the new community node → click "Manage" → click "+ Generate invite link" → copy URL.
3. Device A → Device B: transfer the URL via any side channel.
4. Device B (Bob): Click "+" → "Redeem invite" → paste URL → Redeem. Spinner shows during round-trip; community appears in nav.
5. Device A: settings panel auto-refreshes via `community-members-changed` → Bob in member list with MEMBER badge.
6. Device A: click "Set role" on Bob → slider to 50 (or type 50) → confirm → Bob now MOD.
7. Device A: click "Kick" on Bob → tier-2 confirm → confirm → Bob's community node disappears on Device B.

If only one device available, verify all UI states render correctly without round-trip; document the limitation in the PR body.

- [ ] **Step 3: Push branch**

```bash
git push -u origin zeb-263-community-frontend
# Expected: pushed; branch tracks origin/zeb-263-community-frontend
```

- [ ] **Step 4: Create PR**

```bash
gh pr create --title "ZEB-263 Phase 5: community frontend (NavService kinds + dialogs + admin UI)" --body "$(cat <<'EOF'
## Summary

Phase 5 (final) of ZEB-217 Sub-C — surfaces the Phase 3 + Phase 4 community IPCs to UI. After this lands, ZEB-217 closes.

- 8 new components + 1 new service (`community-service.ts`)
- 4 modified files (`types.ts`, `nav-service.ts`, `NavPanel.svelte`, `App.svelte`)
- Three-tier confirmation policy (no-confirm / click-confirm-at-offset-position / typed-confirmation)
- Hybrid power-level UX (Member/Mod/Admin badges + slider+number-input set-power dialog with bidirectional sync)
- 12 redeem-invite error variants mapped to friendly summaries with diagnostic disclosure (uses ZEB-260's `reason_tag()`)
- No backend changes

Builds atop:
- ZEB-262 / PR #89 (Phase 4 backend invite-only / kick / set-power)
- ZEB-260 / PR #90 (Phase 4 cold-cache bootstrap fix)

Spec: `docs/specs/2026-05-08-zeb-263-community-frontend-design.md` (commit `3696130`).
Plan: `docs/plans/2026-05-08-zeb-263-community-frontend-plan.md`.

## Test plan

- [ ] All vitest tests pass (10 component test files added/extended)
- [ ] All Rust gates pass: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`
- [ ] Frontend type-check passes: `npx tsc --noEmit`
- [ ] Manual two-device smoke: create invite-only community on A → mint URL → redeem on B → admin sees joiner in roster → kick via tier-2 confirm → joiner's community node disappears
- [ ] Tier-3 typed confirmation: as only admin, attempt to leave → typed-confirm modal blocks until exact community name typed
- [ ] Sync-degraded surfacing: disconnect network → "⚠ Degraded" appears in Info section → reconnect → returns to "● Healthy"

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Report PR URL**

```bash
gh pr view --json url --jq .url
# Print the URL so the human can review.
```

After PR is opened, this branch's work is complete pending human review and merge.

---

## Open implementation questions (from spec Appendix A)

The implementer should resolve these during execution and document any clarifications in PR review:

1. **NavService API rename versioning** — verify no external callers of `addOrUpdateDmSpace` exist outside Phase 4's wiring (Step 5 of Task 2).
2. **Default community placement on create** — new communities land at root (parentId = null); user can drag-and-drop into a folder afterward. Verify the existing folder-placement IPC supports moving community nodes (post-Task 7 manual smoke).
3. **`list_community_members` filter semantics** — confirm whether the IPC returns all members (joined/invited/banned) or only joined. The CommunitySettingsPanel filters to `status === 'joined'` for member-count display; if the backend already filters, the client filter is a no-op (still safe to keep for defensive coherence).
4. **Avatar resolution for community members** — community members reuse `NavService.profiles` map keyed by address. The Phase 5 panel does not need to add a new resolution path.
5. **Empty-state when no communities exist** — if implementer wants to add a "Create your first community" CTA on the empty NavPanel, that's polish-out-of-scope; don't block the PR on it.
