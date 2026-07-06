# ZEB-608 — Commons E: Charter View + Settings Restyle — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A member-facing `CharterView` that GENERATES a community's constitution from live governance state, plus one small read-only Rust IPC (`get_community_governance`) and a Commons restyle of the Manage-community panel and its two governance dialogs.

**Architecture:** One new Rust IPC exposes the materialized `admin_quorum` to any Joined member (fixing the settings panel's latent always-shows-1 bug). Two new shared primitives (`RoleBadge`, `PipMeter`) join the ZEB-607 governance family. `CharterView` composes them into a doc-column document whose every number is traceable to `POWER_THRESHOLDS`, the roster, the new IPC, or finalized Tier-3 polls. `CommunityView` gains a fourth tab; the settings panel and both dialogs get chrome-only Commons treatment with all test-pinned selectors/copy byte-identical.

**Tech Stack:** Rust (Tauri 2, existing `lib.rs` IPC idiom), Svelte 5 runes, vitest + @testing-library/svelte, cargo-nextest.

**Spec:** `docs/specs/2026-07-06-zeb-608-commons-e-charter-design.md` (committed 2d3a62a9). Branch: `zeb-608-commons-e-charter` off main `50eb276e`.

## Global Constraints

- Frontend gates: `npx tsc --noEmit && npx vitest run` (repo root). Rust gates: `cd src-tauri && cargo fmt --all` then `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; tests via `scripts/test-select --context task` (repo root) for iteration, full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` only at the final sweep.
- No raw hex colors in Svelte `<style>` blocks. `src/style-token-allowlist.json` ratchets DOWN only — regenerate with `UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts` and the diff must be removal-only.
- Tauri IPC naming: Rust `snake_case` params, JS `camelCase` call sites (auto-converted at the boundary). DTOs serialize `#[serde(rename_all = "camelCase")]`.
- No invented data in CharterView — every number traceable to `POWER_THRESHOLDS`, the roster, the D1 IPC, or finalized Tier-3 polls. The footnote "Thresholds are platform-wide in v1." is mandatory copy (spec §0.1).
- ZEB-606/607 contracts untouched: existing tabs' behavior identical; `StatusPill`/`TallyBar`/`CountChip`/`GovConfirmModal` and `short-addr.ts` are consumed, never modified.
- Test lockstep (spec §3): these files MUST keep passing **unedited except where a task explicitly appends new tests**: `CommunitySettingsPanel.test.ts`, `SetPowerDialog.test.ts`, `ChangeQuorumDialog.test.ts`, `LastAdminWarningDialog.test.ts`, `CommunityView.test.ts`. Pinned strings that must stay byte-identical: `Manage community`, section labels (`Info`, `Public profile`, `Message relay`, `Members (N)`, `Invites`, `Join requests`, `Admin governance`, `Forks`, `Danger zone`), `Change quorum…`, `Search members...`, `Current admin quorum: {k} of {n}` copy shape, `MEMBER`/`MOD`/`ADMIN` badge text, `● Healthy` / `⚠ Degraded — pending events not yet visible`, aria-labels `Quorum slider` / `Quorum number` / `Power level slider` / `Power level`, the ChangeQuorumDialog N+1/survivability paragraph, and selectors `.member-row`, `button.kick`, `button.set-role`, `.pending-badge`, `.confirm-btn`, native `<dialog>`.
- New Tauri commands need ONLY a `generate_handler!` entry — `src-tauri/capabilities/default.json` does not list app commands (verified).
- Commit at the end of every task (and before any long gate). No worktrees — work directly on the branch.

---

### Task 1: `get_community_governance` IPC + client binding

**Files:**
- Modify: `src-tauri/src/lib.rs` (new DTO + pure helper + command near `:35485`; registration at `:52624`; test module near `:58021`)
- Modify: `src/lib/community-service.ts` (new method after `listCommunityForks`, ~`:570`)
- Test: inline `#[cfg(test)] mod get_community_governance_tests` in `lib.rs`; `src/lib/__tests__/community-service.test.ts`

**Interfaces:**
- Consumes: `MaterializedMembership { members, power_levels, admin_quorum }`, `MemberState`, `MemberStatus` from `crate::community_membership`; `engine_arc`/`materialize_now` idiom copied from `list_pending_admin_proposals` (`lib.rs:35547`).
- Produces: Rust `get_community_governance(community_id: String) -> Result<CommunityGovernanceDto, String>` (DTO wire shape `{"adminQuorum": u8}`); TS `CommunityService.getCommunityGovernance(communityId: string): Promise<{ adminQuorum: number }>`. Task 4 consumes the TS method.

- [ ] **Step 1: Write the failing Rust tests**

Add this module in `src-tauri/src/lib.rs` immediately BEFORE the line `// ── ZEB-250 Task 10: list_pending_admin_proposals unit tests ──────────────` (~`:58021`):

```rust
// ── ZEB-608 D1: get_community_governance unit tests ────────────────────────
//
// Exercise `compute_community_governance` directly (no NodeState / Tauri
// runtime). The IPC wrapper adds only hex-decode + engine lookup, both
// covered by every other community IPC.
#[cfg(test)]
mod get_community_governance_tests {
    use super::*;
    use crate::community_membership::{MaterializedMembership, MemberState, MemberStatus};
    use crate::owner_state_types::{Hlc, OwnerAddr};
    use std::collections::BTreeSet;

    fn member(status: MemberStatus) -> MemberState {
        MemberState {
            status,
            joined_at: Hlc {
                wall_ms: 100,
                logical: 0,
                device_id: "test".into(),
            },
            left_at: None,
            enrolled_device_keys: BTreeSet::new(),
        }
    }

    #[test]
    fn returns_materialized_quorum_for_power_zero_member() {
        // The charter is member-facing: a Joined member with NO power_levels
        // entry (power 0) must be able to read the quorum — this is the
        // deliberate difference from admin-gated list_pending_admin_proposals.
        let caller = OwnerAddr([0x01; 16]);
        let mut m = MaterializedMembership {
            admin_quorum: 3,
            ..Default::default()
        };
        m.members.insert(caller, member(MemberStatus::Joined));

        let dto = compute_community_governance(&m, caller).expect("readable at power 0");
        assert_eq!(dto.admin_quorum, 3);
    }

    #[test]
    fn default_quorum_is_one() {
        let caller = OwnerAddr([0x01; 16]);
        let mut m = MaterializedMembership::default();
        m.members.insert(caller, member(MemberStatus::Joined));

        let dto = compute_community_governance(&m, caller).expect("joined member");
        assert_eq!(dto.admin_quorum, 1);
    }

    #[test]
    fn rejects_non_member_caller() {
        let caller = OwnerAddr([0x02; 16]);
        let m = MaterializedMembership::default();

        let err = compute_community_governance(&m, caller).unwrap_err();
        assert!(err.contains("not a Joined member"), "got: {err}");
    }

    #[test]
    fn rejects_left_and_banned_members() {
        let left = OwnerAddr([0x03; 16]);
        let banned = OwnerAddr([0x04; 16]);
        let mut m = MaterializedMembership::default();
        m.members.insert(left, member(MemberStatus::Left));
        m.members.insert(banned, member(MemberStatus::Banned));

        assert!(compute_community_governance(&m, left).is_err());
        assert!(compute_community_governance(&m, banned).is_err());
    }

    #[test]
    fn dto_serializes_admin_quorum_camel_case() {
        // Pins the wire key the TS binding reads (e2e camelCase rule).
        let dto = CommunityGovernanceDto { admin_quorum: 2 };
        let json = serde_json::to_string(&dto).expect("serialize");
        assert_eq!(json, r#"{"adminQuorum":2}"#);
    }
}
```

- [ ] **Step 2: Run the Rust tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(get_community_governance)'`
Expected: COMPILE ERROR — `compute_community_governance` and `CommunityGovernanceDto` not found.

- [ ] **Step 3: Implement the DTO, pure helper, and IPC**

In `src-tauri/src/lib.rs`, insert immediately BEFORE the banner comment `// ── ZEB-250 Task 10: list_pending_admin_proposals IPC ─────────────────────` (~`:35485`):

```rust
// ── ZEB-608 D1: get_community_governance IPC ──────────────────────────────
//
// Member-facing read-only governance snapshot. Unlike
// `list_pending_admin_proposals` (admin-gated), ANY Joined member may read
// this — it powers the CharterView "Admin quorum" card and fixes the
// settings panel's always-shows-1 default (spec §0.2).

/// DTO returned by `get_community_governance`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityGovernanceDto {
    /// Current materialized admin quorum (ZEB-250). Default 1.
    pub admin_quorum: u8,
}

/// Pure extraction: caller must be a Joined member — any power level; the
/// charter is member-facing. Extracted for unit testing without NodeState.
pub fn compute_community_governance(
    materialized: &crate::community_membership::MaterializedMembership,
    caller: crate::owner_state_types::OwnerAddr,
) -> Result<CommunityGovernanceDto, String> {
    let caller_status = materialized.members.get(&caller).map(|m| m.status);
    if !matches!(
        caller_status,
        Some(crate::community_membership::MemberStatus::Joined)
    ) {
        return Err("get_community_governance: caller is not a Joined member".to_string());
    }
    Ok(CommunityGovernanceDto {
        admin_quorum: materialized.admin_quorum,
    })
}

/// ZEB-608 D1: read-only governance values for a community, readable by any
/// Joined member (no power gate — deliberately weaker than
/// `list_pending_admin_proposals`).
#[tauri::command]
async fn get_community_governance(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<CommunityGovernanceDto, String> {
    let id_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(id_bytes);

    let (registry, self_owner) = {
        let g = state_lock
            .lock()
            .map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.community_registry.clone().ok_or(OWNER_NOT_LOADED_MSG)?,
            g.dm_self_owner.ok_or(OWNER_NOT_LOADED_MSG)?,
        )
    };

    let engine_arc = registry.engine_arc(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;

    let admin_addr = engine_arc.admin_addr();
    let materialized = {
        let state = engine_arc.state();
        let g = state.lock().await;
        g.materialize_now(admin_addr)
    };
    compute_community_governance(&materialized, self_owner)
}
```

Then register the command: in the `generate_handler!` list, add `get_community_governance,` on its own line immediately after `list_pending_admin_proposals,` (`:52624`).

- [ ] **Step 4: Run the Rust tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(get_community_governance)'`
Expected: 5 tests PASS.

- [ ] **Step 5: Write the failing TS binding test**

Append inside the `describe('CommunityService', ...)` block of `src/lib/__tests__/community-service.test.ts`:

```typescript
  it('getCommunityGovernance invokes the IPC and returns the camelCase DTO (ZEB-608)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({ adminQuorum: 2 });
    const result = await service.getCommunityGovernance('aa'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledWith('get_community_governance', {
      communityId: 'aa'.repeat(16),
    });
    expect(result.adminQuorum).toBe(2);
  });
```

Run: `npx vitest run src/lib/__tests__/community-service.test.ts`
Expected: FAIL — `service.getCommunityGovernance is not a function`.

- [ ] **Step 6: Implement the binding**

In `src/lib/community-service.ts`, insert after the `listCommunityForks` method (ends ~`:570`), before `destroy()`:

```typescript
  /**
   * ZEB-608 D1: read-only governance values for a community. Readable by
   * ANY Joined member (no power gate) — powers the CharterView admin-quorum
   * card and the settings panel's real quorum display (fixes the
   * always-shows-1 default, spec §0.2).
   */
  async getCommunityGovernance(communityId: string): Promise<{ adminQuorum: number }> {
    return this.invoke<{ adminQuorum: number }>('get_community_governance', { communityId });
  }
```

- [ ] **Step 7: Run the frontend gates**

Run: `npx tsc --noEmit && npx vitest run src/lib/__tests__/community-service.test.ts`
Expected: PASS.

- [ ] **Step 8: Rust gates + commit**

```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
scripts/test-select --context task   # from repo root; paste the round=… bucket=… line into the report
git add src-tauri/src/lib.rs src/lib/community-service.ts src/lib/__tests__/community-service.test.ts
git commit -m "ZEB-608 T1: get_community_governance IPC + client binding"
```

---

### Task 2: `RoleBadge` + `PipMeter` primitives

**Files:**
- Create: `src/lib/components/governance/RoleBadge.svelte`
- Create: `src/lib/components/governance/PipMeter.svelte`
- Test: `src/lib/components/governance/__tests__/governance-primitives.test.ts` (append)

**Interfaces:**
- Consumes: `PowerRole` type (`'member' | 'mod' | 'admin'`) from `src/lib/types.ts:388`; Commons tokens `--status-drafting-fg/bg`, `--status-open-fg/bg`, `--status-passed-fg/bg`, `--vote-for`, `--vote-abstain` (all exist in `src/app.css`).
- Produces: `RoleBadge` props `{ role: PowerRole }` — renders `<span class="role-badge {role}">MEMBER|MOD|ADMIN</span>` (uppercase in MARKUP, not CSS-only — tests use `getByText('MEMBER')`). `PipMeter` props `{ filled: number, total: number, label?: string }` — renders `.pip-meter > .pip` spans, filled ones classed `.pip.filled`, `role="img"` with `aria-label` defaulting to `` `${filled} of ${total}` ``. Tasks 3, 5, 6 consume both.

- [ ] **Step 1: Write the failing tests**

Append to `src/lib/components/governance/__tests__/governance-primitives.test.ts` (add the two imports next to the existing component imports at the top):

```typescript
import RoleBadge from '../RoleBadge.svelte';
import PipMeter from '../PipMeter.svelte';
```

```typescript
describe('RoleBadge', () => {
  it.each([
    ['member', 'MEMBER'],
    ['mod', 'MOD'],
    ['admin', 'ADMIN'],
  ] as const)('renders %s with its variant class and uppercase label', (role, expected) => {
    const { container } = render(RoleBadge, { props: { role } });
    const badge = container.querySelector(`.role-badge.${role}`);
    expect(badge?.textContent).toBe(expected);
  });
});

describe('PipMeter', () => {
  it('renders total pips with the filled count marked and the label applied', () => {
    const { container } = render(PipMeter, {
      props: { filled: 2, total: 4, label: 'Admin quorum meter' },
    });
    expect(container.querySelectorAll('.pip').length).toBe(4);
    expect(container.querySelectorAll('.pip.filled').length).toBe(2);
    expect(screen.getByLabelText('Admin quorum meter')).toBeTruthy();
  });

  it('clamps filled above total and below zero', () => {
    const { container } = render(PipMeter, { props: { filled: 9, total: 3 } });
    expect(container.querySelectorAll('.pip.filled').length).toBe(3);
    const { container: c2 } = render(PipMeter, { props: { filled: -1, total: 3 } });
    expect(c2.querySelectorAll('.pip.filled').length).toBe(0);
  });

  it('collapses a degenerate total to a single empty pip and defaults the aria-label', () => {
    const { container } = render(PipMeter, { props: { filled: 2, total: 0 } });
    // total clamps to >= 1; filled then clamps to <= total.
    expect(container.querySelectorAll('.pip').length).toBe(1);
    render(PipMeter, { props: { filled: 1, total: 3 } });
    expect(screen.getByLabelText('1 of 3')).toBeTruthy();
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `npx vitest run src/lib/components/governance/__tests__/governance-primitives.test.ts`
Expected: FAIL — cannot resolve `../RoleBadge.svelte`.

- [ ] **Step 3: Implement RoleBadge**

Create `src/lib/components/governance/RoleBadge.svelte`:

```svelte
<script lang="ts">
  /**
   * ZEB-608 — Commons role badge (spec D2). Membership-tier badge for
   * member/mod/admin rows. Deliberately NOT a StatusPill variant: role
   * badges are mono, smaller, and membership-semantic (not
   * governance-lifecycle). Token pairs per the design: member = drafting,
   * mod = open, admin = passed.
   *
   * The label is uppercased in MARKUP (not CSS) — consumer tests pin the
   * literal text 'MEMBER' / 'MOD' / 'ADMIN' via getByText.
   */
  import type { PowerRole } from '../../types';

  let { role }: { role: PowerRole } = $props();
</script>

<span class="role-badge {role}">{role.toUpperCase()}</span>

<style>
  .role-badge {
    display: inline-block;
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 10px;
    line-height: 1.4;
    letter-spacing: 0.04em;
    padding: 2px 8px;
    border-radius: 20px;
    white-space: nowrap;
  }
  .member {
    color: var(--status-drafting-fg);
    background: var(--status-drafting-bg);
  }
  .mod {
    color: var(--status-open-fg);
    background: var(--status-open-bg);
  }
  .admin {
    color: var(--status-passed-fg);
    background: var(--status-passed-bg);
  }
</style>
```

- [ ] **Step 4: Implement PipMeter**

Create `src/lib/components/governance/PipMeter.svelte`:

```svelte
<script lang="ts">
  /**
   * ZEB-608 — Commons k-of-n pip meter (spec D2). Discrete quorum pips —
   * deliberately distinct from TallyBar (contiguous percentage fills):
   * a quorum is a count of people, not a percentage.
   */
  let {
    filled,
    total,
    label,
  }: {
    filled: number;
    total: number;
    label?: string;
  } = $props();

  // Degenerate inputs (0 admins mid-roster-load, quorum > admin count after
  // an admin leaves, NaN) must render a sane meter, never throw or paint
  // an impossible state: total >= 1, 0 <= filled <= total.
  let safeTotal = $derived(Number.isFinite(total) ? Math.max(1, Math.trunc(total)) : 1);
  let safeFilled = $derived(
    Number.isFinite(filled) ? Math.max(0, Math.min(safeTotal, Math.trunc(filled))) : 0,
  );
</script>

<div class="pip-meter" role="img" aria-label={label ?? `${safeFilled} of ${safeTotal}`}>
  {#each { length: safeTotal } as _, i (i)}
    <span class="pip" class:filled={i < safeFilled}></span>
  {/each}
</div>

<style>
  .pip-meter {
    display: flex;
    gap: 5px;
  }
  .pip {
    flex: 1;
    height: 7px;
    border-radius: 4px;
    background: var(--vote-abstain);
  }
  .pip.filled {
    background: var(--vote-for);
  }
</style>
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `npx vitest run src/lib/components/governance/__tests__/governance-primitives.test.ts`
Expected: PASS (existing 9 + new 6 = 15).

- [ ] **Step 6: Full frontend gate + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/lib/components/governance/RoleBadge.svelte src/lib/components/governance/PipMeter.svelte src/lib/components/governance/__tests__/governance-primitives.test.ts
git commit -m "ZEB-608 T2: RoleBadge + PipMeter governance primitives"
```

---

### Task 3: `CharterView`

**Files:**
- Create: `src/lib/components/CharterView.svelte`
- Test: `src/lib/components/__tests__/CharterView.test.ts` (new file)

**Interfaces:**
- Consumes: `RoleBadge`/`PipMeter` (Task 2, `./governance/…`), `POWER_THRESHOLDS` + `CommunityMember` from `../types`, `Tier3PollSummary` from `../types/voting` (`stage: 'so'|'de'|'dr'|'ra'|'fi'|'fa'`, `winnerText: string | null`, `pollCreateHlcMs: number`, `proposalText`, `proposer`, `pollId`), `shortAddr` from `../short-addr` (8…4 form), `VotingAdapter.listTier3Polls(communityId)` from `../voting-adapter`.
- Produces: `CharterView` props `{ communityId: string, communityName: string, members: CommunityMember[], adminQuorum: number, adapter: VotingAdapter, onProposeAmendment: () => void }`. Root element `<article class="charter-view">`. Task 4 mounts it.

- [ ] **Step 1: Write the failing tests**

Create `src/lib/components/__tests__/CharterView.test.ts`:

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import CharterView from '../CharterView.svelte';
import type { CommunityMember } from '../../types';
import type { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollSummary } from '../../types/voting';

const alice: CommunityMember = { address: 'aa'.repeat(20), displayName: 'Alice', power: 100, status: 'joined' };
const bob: CommunityMember = { address: 'bb'.repeat(20), displayName: 'Bob', power: 0, status: 'joined' };
const carol: CommunityMember = { address: 'cc'.repeat(20), displayName: 'Carol', power: 100, status: 'joined' };
const daveLeft: CommunityMember = { address: 'dd'.repeat(20), displayName: 'Dave', power: 0, status: 'left' };

function poll(overrides: Partial<Tier3PollSummary> = {}): Tier3PollSummary {
  return {
    pollId: 'p1',
    communityId: 'cid',
    proposalText: 'Adopt a code of conduct',
    proposer: 'ee'.repeat(20),
    stage: 'fi',
    pollCreateHlcMs: 1735689600000, // 2025-01-01T00:00:00Z
    sortitionSize: 5,
    winnerText: 'Adopted with amendments',
    privacyMode: 'pu',
    ...overrides,
  };
}

function makeAdapter(polls: Tier3PollSummary[] | Error): VotingAdapter {
  return {
    listTier3Polls: vi.fn(() =>
      polls instanceof Error ? Promise.reject(polls) : Promise.resolve(polls),
    ),
  } as unknown as VotingAdapter;
}

const baseProps = {
  communityId: 'cid',
  communityName: 'IPFS Crew',
  members: [alice, bob, carol, daveLeft],
  adminQuorum: 2,
  onProposeAmendment: vi.fn(),
};

describe('CharterView', () => {
  it('derives the plural amendment-count pill and joined-members-bound line', async () => {
    const adapter = makeAdapter([
      poll({ pollId: 'p1' }),
      poll({ pollId: 'p2', pollCreateHlcMs: 1738368000000 }), // 2025-02-01
      poll({ pollId: 'p3', stage: 'de' }), // in deliberation — NOT ratified
    ]);
    const { getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(getByText('✓ 2 ratified amendments')).toBeTruthy();
    });
    // daveLeft has status 'left' — only joined members are bound.
    expect(getByText('3 members bound')).toBeTruthy();
  });

  it('uses the singular form for exactly one amendment', async () => {
    const adapter = makeAdapter([poll()]);
    const { getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(getByText('✓ 1 ratified amendment')).toBeTruthy();
    });
  });

  it('zero-state shows "No amendments yet" and no on-record section', async () => {
    const adapter = makeAdapter([]);
    const { getByText, container } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(getByText('✓ No amendments yet')).toBeTruthy();
    });
    expect(container.querySelector('.on-record')).toBeNull();
  });

  it('renders all three articles gracefully when the poll fetch rejects', async () => {
    const adapter = makeAdapter(new Error('adapter not connected'));
    const { container, getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect((adapter.listTier3Polls as ReturnType<typeof vi.fn>).mock.calls.length).toBe(1);
    });
    expect(getByText('✓ …')).toBeTruthy(); // neutral not-loaded pill, never a fake zero
    expect(getByText(/Article I · Membership/)).toBeTruthy();
    expect(getByText(/Article II · How we decide/)).toBeTruthy();
    expect(getByText(/Article III · Amendment/)).toBeTruthy();
    expect(container.querySelector('.on-record')).toBeNull();
  });

  it('renders the generated preamble framing', async () => {
    const adapter = makeAdapter([]);
    const { getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    expect(getByText(/generated from its live governance state/)).toBeTruthy();
  });

  it('capability matrix has the 6 derived rows, an admin-only bottom row, and the v1 footnote', async () => {
    const adapter = makeAdapter([]);
    const { container } = render(CharterView, { props: { ...baseProps, adapter } });
    const rows = container.querySelectorAll('.capability-matrix tbody tr');
    expect(rows.length).toBe(6);
    const last = rows[5];
    expect(last.textContent).toContain('Set roles · change decision rules');
    const caps = last.querySelectorAll('.cap');
    expect(caps[0].textContent).toBe('—');
    expect(caps[1].textContent).toBe('—');
    expect(caps[2].textContent).toBe('●');
    // Honesty footnote — thresholds are GLOBAL v1 constants (spec §0.1).
    expect(container.textContent).toContain('Thresholds are platform-wide in v1.');
  });

  it('role cards show the real POWER_THRESHOLDS values', async () => {
    const adapter = makeAdapter([]);
    const { getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    expect(getByText('power 0')).toBeTruthy();
    expect(getByText('power ≥ 50')).toBeTruthy();
    expect(getByText('power ≥ 100')).toBeTruthy();
  });

  it('admin quorum card shows k of n from real data with a matching pip meter', async () => {
    const adapter = makeAdapter([]);
    const { container, getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    // n = joined members with power >= 100 (alice, carol); k = adminQuorum prop.
    expect(getByText('2 of 2')).toBeTruthy();
    expect(container.querySelectorAll('.quorum-card .pip').length).toBe(2);
    expect(container.querySelectorAll('.quorum-card .pip.filled').length).toBe(2);
    expect(getByText(/No single admin can act alone/)).toBeTruthy();
  });

  it('Propose amendment fires the callback', async () => {
    const onProposeAmendment = vi.fn();
    const adapter = makeAdapter([]);
    const { getByRole } = render(CharterView, {
      props: { ...baseProps, adapter, onProposeAmendment },
    });
    await fireEvent.click(getByRole('button', { name: 'Propose amendment' }));
    expect(onProposeAmendment).toHaveBeenCalledTimes(1);
  });

  it('on-record rows render proposed-date, title, ratified outcome, and short proposer', async () => {
    const adapter = makeAdapter([poll()]);
    const { container, getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(container.querySelector('.on-record')).toBeTruthy();
    });
    expect(getByText(/2025-01-01 · proposed/)).toBeTruthy();
    expect(getByText('Adopt a code of conduct')).toBeTruthy();
    expect(getByText('Ratified: Adopted with amendments')).toBeTruthy();
    expect(getByText('eeeeeeee…eeee')).toBeTruthy(); // shortAddr 8…4
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/CharterView.test.ts`
Expected: FAIL — cannot resolve `../CharterView.svelte`.

- [ ] **Step 3: Implement CharterView**

Create `src/lib/components/CharterView.svelte`:

```svelte
<script lang="ts">
  /**
   * ZEB-608 — CharterView (spec D3). GENERATES a community's constitution
   * from live governance state: POWER_THRESHOLDS, the member roster, the
   * get_community_governance quorum, and finalized Tier-3 polls. Nothing
   * rendered here is stored charter prose — every number is traceable to
   * real data (spec §5 "no invented data"; tier cards describe REAL
   * mechanics in prose, no invented percentages per §0.3).
   */
  import type { VotingAdapter } from '../voting-adapter';
  import type { Tier3PollSummary } from '../types/voting';
  import type { CommunityMember } from '../types';
  import { POWER_THRESHOLDS } from '../types';
  import { shortAddr } from '../short-addr';
  import PipMeter from './governance/PipMeter.svelte';
  import RoleBadge from './governance/RoleBadge.svelte';

  let {
    communityId,
    communityName,
    members,
    adminQuorum,
    adapter,
    onProposeAmendment,
  }: {
    communityId: string;
    communityName: string;
    members: CommunityMember[];
    /** Current materialized admin quorum (get_community_governance, D1). */
    adminQuorum: number;
    adapter: VotingAdapter;
    /** Fired by "Propose amendment" — the parent switches to the
     *  Constitutional tab (spec §0.6: create-form prefill is YAGNI v1). */
    onProposeAmendment: () => void;
  } = $props();

  let joinedMembers = $derived(members.filter((m) => m.status === 'joined'));
  let adminCount = $derived(
    joinedMembers.filter((m) => m.power >= POWER_THRESHOLDS.setPower).length,
  );

  // Finalized Tier-3 polls = the real amendment record (spec §0.4).
  // null = not yet loaded OR load failed — the header pill shows a neutral
  // '…' (never a fake zero) and Article III renders without the list.
  let polls = $state<Tier3PollSummary[] | null>(null);

  $effect(() => {
    const cid = communityId;
    polls = null;
    void adapter
      .listTier3Polls(cid)
      .then((list) => {
        if (cid !== communityId) return; // stale — community switched
        polls = list;
      })
      .catch(() => {
        if (cid !== communityId) return;
        polls = null;
      });
  });

  let ratified = $derived(
    (polls ?? [])
      .filter((p) => p.stage === 'fi')
      .slice()
      .sort((a, b) => a.pollCreateHlcMs - b.pollCreateHlcMs),
  );
  let ratifiedPillText = $derived(
    polls === null
      ? '✓ …'
      : ratified.length === 0
        ? '✓ No amendments yet'
        : `✓ ${ratified.length} ratified amendment${ratified.length === 1 ? '' : 's'}`,
  );

  // pollCreateHlcMs is CREATION time — the finalization HLC is not in the
  // summary (spec §0.4), so the record honestly labels the date "proposed".
  function proposedDate(hlcMs: number): string {
    return new Date(hlcMs).toISOString().slice(0, 10);
  }

  // Capability matrix (spec D3 Article I): derived from the REAL consumer
  // checks — invite ≥ 0; channel manage/moderate/join-approval ≥ kick (50);
  // set-roles/kick-admin/change-quorum ≥ setPower (100). ● = can, — = cannot.
  const MATRIX_ROWS: Array<{ action: string; member: boolean; mod: boolean; admin: boolean }> = [
    { action: 'Post, vote & propose', member: true, mod: true, admin: true },
    { action: 'Delegate & recall', member: true, mod: true, admin: true },
    { action: 'Fork the community', member: true, mod: true, admin: true },
    { action: 'Manage channels & invites', member: false, mod: true, admin: true },
    { action: 'Approve joins · remove members', member: false, mod: true, admin: true },
    { action: 'Set roles · change decision rules', member: false, mod: false, admin: true },
  ];
</script>

<article class="charter-view" aria-label={`${communityName} charter`}>
  <div class="doc-column">
    <header class="charter-header">
      <div class="header-main">
        <h1 class="charter-title">📜 {communityName} Charter</h1>
        <div class="meta-row">
          <span class="ratified-pill">{ratifiedPillText}</span>
          <span class="members-bound"
            >{joinedMembers.length} member{joinedMembers.length === 1 ? '' : 's'} bound</span
          >
        </div>
      </div>
      <button type="button" class="propose-btn" onclick={onProposeAmendment}>
        Propose amendment
      </button>
    </header>

    <section class="charter-section" aria-label="Preamble">
      <h2 class="eyebrow">Preamble</h2>
      <p class="preamble">
        This charter is {communityName}'s constitution, generated from its live governance
        state. Every clause below reflects the rules as they are enforced today, and every
        clause can be changed by the members it governs.
      </p>
    </section>

    <section class="charter-section" aria-label="Article I — Membership and roles">
      <h2 class="eyebrow">Article I · Membership &amp; roles</h2>
      <p class="lede">
        Roles are earned, granted, and revoked as a numeric power level. Three named bands:
      </p>
      <div class="role-cards">
        <div class="role-card">
          <RoleBadge role="member" />
          <span class="power-req">power {POWER_THRESHOLDS.invite}</span>
          <p class="role-desc">
            Full civic standing: posts, votes, proposes, delegates — and can fork.
          </p>
        </div>
        <div class="role-card">
          <RoleBadge role="mod" />
          <span class="power-req">power ≥ {POWER_THRESHOLDS.kick}</span>
          <p class="role-desc">
            Stewards the day-to-day space: channels, invites and join requests.
          </p>
        </div>
        <div class="role-card">
          <RoleBadge role="admin" />
          <span class="power-req">power ≥ {POWER_THRESHOLDS.setPower}</span>
          <p class="role-desc">
            Holds the keys that change the rules — always under quorum.
          </p>
        </div>
      </div>
      <table class="capability-matrix">
        <thead>
          <tr>
            <th class="action-col">Capability</th>
            <th>Member</th>
            <th>Mod</th>
            <th>Admin</th>
          </tr>
        </thead>
        <tbody>
          {#each MATRIX_ROWS as row (row.action)}
            <tr>
              <td class="action-col">{row.action}</td>
              <td class="cap" class:can={row.member}>{row.member ? '●' : '—'}</td>
              <td class="cap" class:can={row.mod}>{row.mod ? '●' : '—'}</td>
              <td class="cap" class:can={row.admin}>{row.admin ? '●' : '—'}</td>
            </tr>
          {/each}
        </tbody>
      </table>
      <p class="matrix-footnote">Thresholds are platform-wide in v1.</p>
    </section>

    <section class="charter-section" aria-label="Article II — How we decide">
      <h2 class="eyebrow">Article II · How we decide</h2>
      <p class="lede">Proposals move through three tiers. Higher stakes, higher bar.</p>
      <div class="tier-cards">
        <div class="tier-card">
          <h3 class="tier-name">Tier 1 · Poll</h3>
          <p class="tier-desc">
            Multi-option approval polls. Options, window and eligibility are set per poll.
            Non-binding sentiment.
          </p>
        </div>
        <div class="tier-card">
          <h3 class="tier-name">Tier 2 · Motion</h3>
          <p class="tier-desc">
            Binding conviction votes. Support accumulates over time (7-day half-life by
            default) toward a dynamic threshold; delegable, recallable.
          </p>
        </div>
        <div class="tier-card">
          <h3 class="tier-name">Tier 3 · Charter</h3>
          <p class="tier-desc">
            Amends how the community works. A sortition-selected mini-public deliberates,
            drafts and ratifies by STAR ballot.
          </p>
        </div>
      </div>
      <div class="quorum-card">
        <h3 class="quorum-heading">Admin quorum</h3>
        <span class="quorum-value">{adminQuorum} of {adminCount}</span>
        <PipMeter filled={adminQuorum} total={adminCount} label="Admin quorum meter" />
        <p class="quorum-caption">
          {adminQuorum} of {adminCount} admins must co-sign admin actions. No single admin can
          act alone.
        </p>
      </div>
    </section>

    <section class="charter-section" aria-label="Article III — Amendment">
      <h2 class="eyebrow">Article III · Amendment</h2>
      <div class="amend-callout">
        <p class="amend-text">
          ✎ No clause here is permanent. Any member may open a Tier-3 proposal to amend how
          {communityName} works; if it ratifies, the change is signed by the mini-public and
          recorded. Every ratified decision stays on the record.
        </p>
      </div>
      {#if ratified.length > 0}
        <section class="on-record" aria-label="On the record">
          <h3 class="or-heading">On the record</h3>
          <ul class="amendment-list">
            {#each ratified as p (p.pollId)}
              <li class="amendment-row">
                <span class="amendment-date">{proposedDate(p.pollCreateHlcMs)} · proposed</span>
                <span class="amendment-title">{p.proposalText}</span>
                {#if p.winnerText}
                  <span class="amendment-outcome">Ratified: {p.winnerText}</span>
                {/if}
                <span class="amendment-proposer">{shortAddr(p.proposer)}</span>
              </li>
            {/each}
          </ul>
        </section>
      {/if}
    </section>
  </div>
</article>

<style>
  .charter-view {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: 24px 20px 48px;
    background: var(--bg-primary);
  }
  .doc-column {
    max-width: 780px;
    margin: 0 auto;
    display: flex;
    flex-direction: column;
    gap: 26px;
  }
  .charter-header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
  }
  .charter-title {
    margin: 0 0 6px;
    font-family: var(--font-display);
    font-weight: 500;
    font-size: 2rem;
    line-height: 1.15;
    color: var(--text-primary);
  }
  .meta-row {
    display: flex;
    align-items: center;
    gap: 10px;
    font-family: var(--font-mono);
    font-size: 12px;
    color: var(--text-muted);
  }
  .ratified-pill {
    color: var(--primary-deep);
    background: var(--primary-soft);
    padding: 2px 10px;
    border-radius: 20px;
    font-weight: 600;
    white-space: nowrap;
  }
  .propose-btn {
    background: var(--gov-clay-soft);
    color: var(--gov-clay-deep);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    border-radius: 7px;
    padding: 7px 14px;
    font: inherit;
    font-size: 0.85rem;
    font-weight: 600;
    cursor: pointer;
    white-space: nowrap;
  }
  .propose-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .eyebrow {
    margin: 0 0 10px;
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .preamble {
    margin: 0;
    font-family: var(--font-display);
    font-style: italic;
    font-size: 15.5px;
    line-height: 1.65;
    color: var(--text-primary);
  }
  .lede {
    margin: 0 0 12px;
    font-size: 0.85rem;
    line-height: 1.5;
    color: var(--text-secondary);
  }
  .role-cards {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 10px;
    margin-bottom: 14px;
  }
  .role-card {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-items: flex-start;
  }
  .power-req {
    font-family: var(--font-mono);
    font-size: 0.7rem;
    color: var(--text-muted);
  }
  .role-desc {
    margin: 0;
    font-size: 0.75rem;
    line-height: 1.45;
    color: var(--text-secondary);
  }
  .capability-matrix {
    width: 100%;
    border-collapse: collapse;
    table-layout: fixed;
    font-size: 0.8rem;
  }
  .capability-matrix th,
  .capability-matrix td {
    border: 1px solid var(--border);
    padding: 6px 10px;
    text-align: center;
  }
  .capability-matrix th {
    background: var(--bg-secondary);
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .capability-matrix th:not(.action-col),
  .capability-matrix td:not(.action-col) {
    width: 92px;
  }
  .capability-matrix .action-col {
    text-align: left;
    color: var(--text-primary);
  }
  .cap {
    color: var(--vote-abstain);
    font-family: var(--font-mono);
  }
  .cap.can {
    color: var(--vote-for);
  }
  .matrix-footnote {
    margin: 8px 0 0;
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--text-muted);
  }
  .tier-cards {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-bottom: 14px;
  }
  .tier-card {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-left: 3px solid var(--gov-clay);
    border-radius: 8px;
    padding: 10px 14px;
  }
  .tier-name {
    margin: 0 0 4px;
    font-size: 0.85rem;
    font-weight: 600;
    color: var(--text-primary);
  }
  .tier-desc {
    margin: 0;
    font-size: 0.78rem;
    line-height: 1.5;
    color: var(--text-secondary);
  }
  .quorum-card {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: var(--shadow-e1);
    padding: 12px 14px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .quorum-heading {
    margin: 0;
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .quorum-value {
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 0.9rem;
    color: var(--text-primary);
  }
  .quorum-caption {
    margin: 0;
    font-size: 0.72rem;
    line-height: 1.45;
    color: var(--text-muted);
  }
  .amend-callout {
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    border-radius: 10px;
    padding: 12px 15px;
  }
  .amend-text {
    margin: 0;
    font-size: 0.85rem;
    line-height: 1.55;
    color: var(--primary-deep);
  }
  .on-record {
    margin-top: 14px;
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
  .amendment-list {
    margin: 0;
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .amendment-row {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 8px 12px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .amendment-date {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--text-muted);
  }
  .amendment-title {
    font-weight: 600;
    font-size: 0.82rem;
    color: var(--text-primary);
  }
  .amendment-outcome {
    font-size: 0.78rem;
    color: var(--vote-for);
  }
  .amendment-proposer {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--text-muted);
  }
</style>
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/CharterView.test.ts`
Expected: 10 tests PASS.

- [ ] **Step 5: Full frontend gate + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/lib/components/CharterView.svelte src/lib/components/__tests__/CharterView.test.ts
git commit -m "ZEB-608 T3: CharterView — generated constitution from live governance state"
```

---

### Task 4: CommunityView / App wiring + adminQuorum threading

**Files:**
- Modify: `src/lib/components/CommunityView.svelte` (import block ~`:20`; `activeView` union `:128`; new governance state near `:160`; tab nav `:350-373`; render branch `:443`; settings mount `:516-560`)
- Modify: `src/App.svelte:1042` (union member)
- Test: `src/lib/components/__tests__/CommunityView.test.ts` (extend `setup`, append tests)

**Interfaces:**
- Consumes: `CharterView` (Task 3 props `{ communityId, communityName, members, adminQuorum, adapter, onProposeAmendment }`), `CommunityService.getCommunityGovernance` (Task 1), existing `CommunitySettingsPanel.adminQuorum?: number` prop (defaults 1, never before wired — spec §0.2 latent bug).
- Produces: `activeView` union `'channels' | 'proposals' | 'tier3' | 'charter'` (bindable — App deep-links); "Charter" tab; `governance: { adminQuorum: number } | null` threaded as `adminQuorum={governance?.adminQuorum ?? 1}` to BOTH CharterView and CommunitySettingsPanel.

- [ ] **Step 1: Extend the test harness and write the failing tests**

In `src/lib/components/__tests__/CommunityView.test.ts`:

(a) Extend `setup` with an `invokeOverrides` param and a governance default — replace the current `setup` signature + mockImplementation (lines 80-86) with:

```typescript
async function setup(
  channelList: any[] = [general, announcements],
  propOverrides: Record<string, unknown> = {},
  invokeOverrides: Record<string, () => Promise<unknown>> = {},
) {
  const adapter = makeAdapter();
  (adapter.invoke as any).mockImplementation((cmd: string) => {
    if (cmd in invokeOverrides) return invokeOverrides[cmd]();
    if (cmd === 'list_channels') return Promise.resolve(channelList);
    if (cmd === 'list_channel_messages') return Promise.resolve([]);
    if (cmd === 'get_community_governance') return Promise.resolve({ adminQuorum: 1 });
    return Promise.resolve(undefined);
  });
```

(everything from `const communityService = new CommunityService();` down stays byte-identical).

(b) Add a stubbed-voting-adapter helper next to `makeVoiceSessionStub` (the Constitutional test at `:300` builds these stubs inline; the new tests need the same set):

```typescript
/** ZEB-608: VotingAdapter stub with the tier3 surface CommunityView's
 *  tabs touch (list + lifecycle subscriptions), unconnected-safe. */
function makeVotingAdapterStub(): VotingAdapter {
  const votingAdapter = new VotingAdapter();
  votingAdapter.listTier3Polls = vi.fn().mockResolvedValue([]);
  const noopUnsub = () => {};
  votingAdapter.subscribeTier3PollCreated = vi.fn().mockReturnValue(noopUnsub);
  votingAdapter.subscribeTier3SortitionComplete = vi.fn().mockReturnValue(noopUnsub);
  votingAdapter.subscribeTier3DraftingOpen = vi.fn().mockReturnValue(noopUnsub);
  votingAdapter.subscribeTier3RatificationOpen = vi.fn().mockReturnValue(noopUnsub);
  votingAdapter.subscribeTier3Finalized = vi.fn().mockReturnValue(noopUnsub);
  return votingAdapter;
}
```

(c) Append these tests inside `describe('CommunityView', ...)`:

```typescript
  it('Charter tab mounts CharterView when votingAdapter is provided (ZEB-608)', async () => {
    const { container, getByText } = await setup(undefined, {
      votingAdapter: makeVotingAdapterStub(),
    });
    await waitFor(() => {
      expect(getByText('Charter')).toBeTruthy();
    });
    await fireEvent.click(getByText('Charter'));
    await waitFor(() => {
      expect(container.querySelector('.charter-view')).toBeTruthy();
    });
  });

  it('activeView is externally drivable to charter (deep-link)', async () => {
    const { container } = await setup(undefined, {
      votingAdapter: makeVotingAdapterStub(),
      activeView: 'charter',
    });
    await waitFor(() => {
      expect(container.querySelector('.charter-view')).toBeTruthy();
    });
  });

  it('Propose amendment switches the view to the Constitutional tab', async () => {
    const { container, getByText, getByRole } = await setup(undefined, {
      votingAdapter: makeVotingAdapterStub(),
      activeView: 'charter',
    });
    await waitFor(() => {
      expect(container.querySelector('.charter-view')).toBeTruthy();
    });
    await fireEvent.click(getByRole('button', { name: 'Propose amendment' }));
    await waitFor(() => {
      expect(container.querySelector('.tier3-panel')).toBeTruthy();
    });
    expect(getByText('Constitutional').getAttribute('aria-pressed')).toBe('true');
  });

  it('threads the fetched admin quorum into the settings panel (fixes always-shows-1, ZEB-608 §0.2)', async () => {
    const { container, getByLabelText, getByText } = await setup(
      undefined,
      {},
      { get_community_governance: () => Promise.resolve({ adminQuorum: 2 }) },
    );
    await waitFor(() => {
      expect(container.querySelector('.channel-sub-sidebar')).toBeTruthy();
    });
    await fireEvent.click(getByLabelText(/Open community settings/i));
    await waitFor(() => {
      // adminMember (myPower 100) sees the admin-governance section with the
      // REAL fetched quorum, not the component default of 1.
      expect(getByText(/Current admin quorum: 2 of/)).toBeTruthy();
    });
  });
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `npx vitest run src/lib/components/__tests__/CommunityView.test.ts`
Expected: the 4 new tests FAIL (no Charter tab / no `.charter-view` / quorum shows `1 of`); all pre-existing tests still PASS.

- [ ] **Step 3: Implement the wiring**

In `src/lib/components/CommunityView.svelte`:

(a) Add the import next to `Tier3ProposalPanel` (~`:20`):

```typescript
  import CharterView from './CharterView.svelte';
```

(b) Extend the union at `:128` (and its doc comment):

```typescript
    /** ZEB-606: which middle-column view is active. Bindable so App can
     *  deep-link (nav proposals row / Assembly rail "View all"). Default
     *  'channels' preserves the ZEB-291 behavior for non-binding parents.
     *  ZEB-608 adds 'charter'. */
    activeView?: 'channels' | 'proposals' | 'tier3' | 'charter';
```

(c) Add governance state + fetch after the `preForkSnapshot` declaration (~`:160`):

```typescript
  // ZEB-608 D1: per-community governance snapshot (admin quorum). Loaded on
  // every community switch, stale-guarded like the other per-community loads.
  // null until the IPC resolves — consumers fall back to the pre-ZEB-608
  // default of 1, so a failed fetch degrades to the old behavior instead of
  // blanking the UI.
  let governance = $state<{ adminQuorum: number } | null>(null);

  $effect(() => {
    const cid = communityId;
    governance = null;
    void communityService
      .getCommunityGovernance(cid)
      .then((g) => {
        if (cid !== communityId) return; // stale — community switched
        governance = g ?? null;
      })
      .catch(() => {
        if (cid !== communityId) return;
        governance = null;
      });
  });
```

(d) Add the tab button after the Constitutional button (`:365-372`):

```svelte
        <button
          type="button"
          class="view-tab"
          class:active={activeView === 'charter'}
          aria-pressed={activeView === 'charter'}
          onclick={() => { activeView = 'charter'; }}
        >Charter</button>
```

(e) Add the render branch at the TOP of the middle-column chain (`:443`, before the tier3 branch):

```svelte
    {#if activeView === 'charter' && votingAdapter}
      <CharterView
        {communityId}
        {communityName}
        {members}
        adminQuorum={governance?.adminQuorum ?? 1}
        adapter={votingAdapter}
        onProposeAmendment={() => { activeView = 'tier3'; }}
      />
    {:else if activeView === 'tier3' && votingAdapter}
```

(the existing `{#if activeView === 'tier3' && votingAdapter}` becomes this `{:else if ...}`).

(f) Thread the quorum into the settings mount — inside `<CommunitySettingsPanel ...>` (`:517`), add after `{sharedInProfile}`:

```svelte
    adminQuorum={governance?.adminQuorum ?? 1}
```

In `src/App.svelte:1042`, extend the union:

```typescript
  let communityActiveView = $state<'channels' | 'proposals' | 'tier3' | 'charter'>('channels');
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/CommunityView.test.ts`
Expected: all PASS (pre-existing + 4 new).

- [ ] **Step 5: Full frontend gate + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/lib/components/CommunityView.svelte src/App.svelte src/lib/components/__tests__/CommunityView.test.ts
git commit -m "ZEB-608 T4: Charter tab wiring + real admin-quorum threading"
```

---

### Task 5: CommunitySettingsPanel restyle (D5 — chrome only)

**Files:**
- Modify: `src/lib/components/CommunitySettingsPanel.svelte` (markup: 3 badge sites + PipMeter + danger-zone class; style block)
- Modify: `src/style-token-allowlist.json` (regenerated — removal-only)
- Test: `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` (append 2 tests; existing ~40 MUST pass unedited)

**Interfaces:**
- Consumes: `RoleBadge` + `PipMeter` (Task 2). Embedded children (InviteLinkManager, PendingJoinsPanel, ForkLineageTree, ForkConfirmDialog, ConfirmationModal, LastAdminWarningDialog, PendingAdminProposalsPanel) are OUT of scope → ZEB-611; do not touch their files.
- Produces: no interface changes — chrome only. All 9 section labels, pinned selectors, copy, aria-labels, and gating stay byte-identical (Global Constraints list).

- [ ] **Step 1: Write the failing tests**

Append inside `describe('CommunitySettingsPanel', ...)` in `src/lib/components/__tests__/CommunitySettingsPanel.test.ts` (add `secondAdmin` next to the existing member fixtures at the top of the file):

```typescript
const secondAdmin: CommunityMember = { address: 'dd11', displayName: 'Dana', power: 100, status: 'joined' };
```

```typescript
  it('admin governance renders the quorum pip meter from real counts (ZEB-608)', () => {
    const { container } = render(CommunitySettingsPanel, {
      props: {
        ...baseProps,
        myPower: 100,
        adminQuorum: 2,
        members: [adminMember, secondAdmin, plainMember],
      },
    });
    // n = 2 joined admins → 2 pips; k = 2 → both filled.
    expect(container.querySelectorAll('.admin-governance-section .pip').length).toBe(2);
    expect(container.querySelectorAll('.admin-governance-section .pip.filled').length).toBe(2);
  });

  it('sync-status healthy row keeps its copy after the token migration (ZEB-608)', () => {
    const { getByText } = render(CommunitySettingsPanel, { props: baseProps });
    // Copy is pinned; the color moved from raw #7acc7a to var(--presence-online).
    expect(getByText('● Healthy')).toBeTruthy();
  });
```

Run: `npx vitest run src/lib/components/__tests__/CommunitySettingsPanel.test.ts`
Expected: the pip-meter test FAILS (no `.pip` elements); the healthy test passes already (copy unchanged) — it is a regression pin for the token swap.

- [ ] **Step 2: Markup changes**

In `src/lib/components/CommunitySettingsPanel.svelte`:

(a) Add imports next to `PendingAdminProposalsPanel` (~`:18`):

```typescript
  import RoleBadge from './governance/RoleBadge.svelte';
  import PipMeter from './governance/PipMeter.svelte';
```

(b) Info section "Your role" row (`:369`) — replace the span:

```svelte
        <div>
          <RoleBadge role={myRole} />
          (power {myPower})
        </div>
```

(c) Member row badge (`:460`) — replace the span:

```svelte
            <RoleBadge role={powerToRole(m.power)} />
```

(d) Admin governance — insert between the `.admin-quorum-info` paragraph and the `change-quorum-btn` (`:504-505`):

```svelte
        <PipMeter
          filled={currentAdminQuorum}
          total={currentAdminCount}
          label="Admin quorum meter"
        />
```

(the existing `Current admin quorum: {k} of {n} admins required…` copy stays byte-identical).

(e) Danger zone — add a class hook (label text unchanged):

```svelte
    <div class="section danger-zone">
      <div class="section-label">Danger zone</div>
```

- [ ] **Step 3: Style changes (converge on the PendingAdminProposalsPanel reference aesthetic)**

In the `<style>` block:

(a) Section eyebrows — replace the `.section-label` rule and add the danger variant:

```css
  .section-label {
    font-size: 10.5px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.1em;
    margin-bottom: 12px;
  }
  .danger-zone .section-label {
    color: var(--vote-against);
  }
```

(b) DELETE the four `.role-badge` rules (`:705-713` — base + three `[data-role]` variants; RoleBadge carries its own styles now).

(c) Token migration — replace `.healthy { color: #7acc7a; }` (`:714`) with:

```css
  .healthy { color: var(--presence-online); }
```

(d) Member rows as raised cards — replace the `.member-list`, `.member-row`, `.member-row:last-child` rules:

```css
  .member-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .member-row {
    display: flex;
    align-items: center;
    padding: 8px 10px;
    gap: 10px;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: var(--shadow-e1);
  }
```

(e) Set role / Kick as the design's borderless text-buttons — replace the `.set-role, .kick` group and the two per-class rules:

```css
  .set-role,
  .kick {
    font-size: 11px;
    font-weight: 600;
    padding: 2px 4px;
    border: none;
    background: none;
    border-radius: 3px;
    cursor: pointer;
  }
  .set-role { color: var(--vote-for); }
  .kick { color: var(--vote-against); }
```

(f) Leave button soft-danger treatment — replace `.leave-btn`:

```css
  .leave-btn {
    background: color-mix(in srgb, var(--vote-against) 8%, var(--surface-raised));
    color: var(--vote-against);
    border: 1px solid var(--danger-border-muted);
    padding: 6px 14px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.8rem;
    font-weight: 600;
  }
```

(g) Secondary buttons converge on raised-card chrome — in `.manage-members-btn`, `.fork-btn`, and `.change-quorum-btn`, change `background: var(--bg-tertiary);` → `background: var(--surface-raised);` and `border-radius: 4px;` → `border-radius: 7px;` (hover/focus rules unchanged).

(h) Panel header title in the civic display face — replace `.panel-title`:

```css
  .panel-title {
    color: var(--text-primary);
    margin: 0;
    font-family: var(--font-display);
    font-weight: 500;
    font-size: 1.15rem;
  }
```

(i) Search input on the input token — in `.search-input`, change `background: var(--bg-tertiary);` → `background: var(--input-bg);`.

- [ ] **Step 4: Ratchet the allowlist DOWN**

```bash
UPDATE_STYLE_TOKEN_ALLOWLIST=1 npx vitest run src/style-token-guard.test.ts
git diff src/style-token-allowlist.json
```

Expected diff: EXACTLY one removed line — `"lib/components/CommunitySettingsPanel.svelte": 1,`. If anything is ADDED, a raw color slipped into a `<style>` block — fix the style, never the allowlist.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `npx vitest run src/lib/components/__tests__/CommunitySettingsPanel.test.ts src/style-token-guard.test.ts src/commons-hex-guard.test.ts`
Expected: PASS — all pre-existing ~40 panel tests unedited, 2 new tests, both guards.

- [ ] **Step 6: Full frontend gate + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/lib/components/CommunitySettingsPanel.svelte src/lib/components/__tests__/CommunitySettingsPanel.test.ts src/style-token-allowlist.json
git commit -m "ZEB-608 T5: Manage-community Commons restyle — RoleBadge, PipMeter, #7acc7a ratcheted out"
```

---

### Task 6: Governance dialogs — SetPowerDialog bands (D6) + ChangeQuorumDialog chrome (D7)

**Files:**
- Modify: `src/lib/components/SetPowerDialog.svelte`
- Modify: `src/lib/components/ChangeQuorumDialog.svelte`
- Test: `src/lib/components/__tests__/SetPowerDialog.test.ts`, `src/lib/components/__tests__/ChangeQuorumDialog.test.ts` (append; existing tests MUST pass unedited)

**Interfaces:**
- Consumes: `RoleBadge` + `PipMeter` (Task 2), `POWER_THRESHOLDS`/`powerToRole`/`PowerRole` (already imported in SetPowerDialog). Parent contracts unchanged: `SetPowerDialog { targetName, targetAddress, currentPower, actorMaxPower?, onSubmit, onCancel }`; `ChangeQuorumDialog { communityId, currentQuorum, currentAdminCount, onClose }`; the cross-admin confirm stays in the PARENT panel (spec §0.8).
- Produces: no interface changes. ChangeQuorumDialog's confirm button relabels `Propose` → `Propose change` (still matches every existing `/Propose/i` query).

- [ ] **Step 1: Write the failing SetPowerDialog tests**

Append inside `describe('SetPowerDialog', ...)` in `src/lib/components/__tests__/SetPowerDialog.test.ts`:

```typescript
  it('renders the power band track with threshold-derived flex widths (ZEB-608 D6)', () => {
    const { container } = render(SetPowerDialog, { props: baseProps });
    const bands = container.querySelectorAll('.band');
    expect(bands.length).toBe(3);
    expect(bands[0].classList.contains('band-member')).toBe(true);
    expect(bands[1].classList.contains('band-mod')).toBe(true);
    expect(bands[2].classList.contains('band-admin')).toBe(true);
    // Widths derive from POWER_THRESHOLDS (member: 0→50, mod: 50→100).
    expect((bands[0] as HTMLElement).style.flexGrow).toBe('50');
    expect((bands[1] as HTMLElement).style.flexGrow).toBe('50');
  });

  it('helper line tracks the previewed role (ZEB-608 D6)', async () => {
    const { container } = render(SetPowerDialog, { props: baseProps });
    expect(container.querySelector('.role-help')?.textContent).toContain('Member can');
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
    await fireEvent.input(slider, { target: { value: '60' } });
    expect(container.querySelector('.role-help')?.textContent).toContain(
      'Moderator can manage channels, invites & join requests.',
    );
    await fireEvent.input(slider, { target: { value: '100' } });
    expect(container.querySelector('.role-help')?.textContent).toContain('Admin can');
  });
```

Run: `npx vitest run src/lib/components/__tests__/SetPowerDialog.test.ts`
Expected: 2 new tests FAIL; existing 8 PASS.

- [ ] **Step 2: Implement SetPowerDialog D6**

Replace `src/lib/components/SetPowerDialog.svelte` script additions + markup + style as follows.

Script — add after the `role` derivation (`:27`):

```typescript
  // ZEB-608 D6: helper copy keyed to the PREVIEWED role (design frame C1).
  const ROLE_HELP: Record<ReturnType<typeof powerToRole>, string> = {
    member: 'Member can post, vote, propose, delegate and fork.',
    mod: 'Moderator can manage channels, invites & join requests.',
    admin: 'Admin can set roles and change decision rules — under quorum.',
  };
```

Markup — three changes:

(a) Role preview (`:49-51`) — replace the span with RoleBadge + the helper line:

```svelte
  <div class="role-preview">
    <RoleBadge {role} />
  </div>
  <p class="role-help">{ROLE_HELP[role]}</p>
```

and add the import at the top of the script:

```typescript
  import RoleBadge from './governance/RoleBadge.svelte';
```

(b) Control row (`:53-65`) — wrap the range input in a stack with the band track beneath it (the number input, both aria-labels, and both bindings stay byte-identical):

```svelte
  <div class="control-row">
    <div class="slider-stack">
      <input type="range" min="0" max={safeMax} step="1" bind:value={power} class="slider" aria-label="Power level slider" />
      <!-- ZEB-608 D6: banded track — widths from POWER_THRESHOLDS. The admin
           band is a fixed end-cap: the admin threshold IS the scale max
           (setPower == max == 100), so its data-width is zero; the cap marks
           "admin sits at the top of the scale" without inventing a range. -->
      <div class="band-track" aria-hidden="true">
        <span class="band band-member" style="flex-grow: {POWER_THRESHOLDS.kick - POWER_THRESHOLDS.invite}"></span>
        <span class="band band-mod" style="flex-grow: {POWER_THRESHOLDS.setPower - POWER_THRESHOLDS.kick}"></span>
        <span class="band band-admin"></span>
      </div>
    </div>
    <input
      type="number"
      min="0"
      max={safeMax}
      step="1"
      bind:value={power}
      onblur={clampOnBlur}
      class="number-input"
      aria-label="Power level"
    />
  </div>
```

Style — apply these rule changes (all other rules keep their current values):

```css
  .role-preview { text-align: center; margin-bottom: 6px; }
  .role-help {
    text-align: center;
    font-size: 0.72rem;
    color: var(--text-secondary);
    margin: 0 0 12px;
  }
  .slider-stack {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .slider { width: 100%; }
  .band-track {
    display: flex;
    height: 6px;
    border-radius: 4px;
    overflow: hidden;
  }
  .band { min-height: 100%; }
  .band-member { background: var(--status-drafting-bg); }
  .band-mod { background: var(--gov-clay-soft); }
  .band-admin { flex: 0 0 12px; background: var(--primary-soft); }
  .number-input {
    width: 64px;
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 6px 8px;
    color: var(--vote-for);
    font-size: 0.9rem;
    font-weight: 600;
    text-align: center;
    font-family: var(--font-mono);
  }
  .cancel-btn {
    background: transparent;
    color: var(--text-secondary);
    border: 1px solid var(--border);
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .confirm-btn {
    background: var(--accent);
    color: var(--text-bright);
    border: 1px solid var(--accent);
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
    font-weight: 600;
  }
```

Also DELETE the old `.role-badge` + `[data-role]` rules (`:84-87`) and the old `.slider { flex: 1; }` rule (the stack owns the flex now). The `.thresholds` legend block (`:67-71` markup, `:102-113` styles) stays byte-identical — its `.threshold.mod`/`.threshold.admin` colors may be updated to `var(--gov-clay-deep)` / `var(--vote-for)` respectively (Commons tones), but the text content `0/Member`, `50/Mod`, `100/Admin` must not change.

- [ ] **Step 3: Run the SetPowerDialog tests**

Run: `npx vitest run src/lib/components/__tests__/SetPowerDialog.test.ts`
Expected: all 10 PASS (8 pre-existing unedited + 2 new). The MEMBER/MOD/ADMIN assertions now resolve against RoleBadge's markup-uppercased text.

- [ ] **Step 4: Write the failing ChangeQuorumDialog tests**

Append inside `describe('ChangeQuorumDialog', ...)` in `src/lib/components/__tests__/ChangeQuorumDialog.test.ts`:

```typescript
  it('renders the net-new self-referential quorum warning (ZEB-608 D7)', () => {
    const { container } = render(ChangeQuorumDialog, {
      props: { communityId: 'c-x', currentQuorum: 2, currentAdminCount: 4, onClose: vi.fn() },
    });
    const warning = container.querySelector('.quorum-warning');
    expect(warning?.textContent).toMatch(/itself an admin action/);
    expect(warning?.textContent).toMatch(/current 2-of-4 quorum/);
  });

  it('pip preview tracks the PROPOSED quorum (ZEB-608 D7)', async () => {
    const { container } = render(ChangeQuorumDialog, {
      props: { communityId: 'c-x', currentQuorum: 1, currentAdminCount: 4, onClose: vi.fn() },
    });
    expect(container.querySelectorAll('.pip').length).toBe(4);
    expect(container.querySelectorAll('.pip.filled').length).toBe(1);
    const number = screen.getByLabelText('Quorum number') as HTMLInputElement;
    await fireEvent.input(number, { target: { value: '3' } });
    expect(container.querySelectorAll('.pip.filled').length).toBe(3);
  });
```

Run: `npx vitest run src/lib/components/__tests__/ChangeQuorumDialog.test.ts`
Expected: 2 new tests FAIL; existing 6 PASS.

- [ ] **Step 5: Implement ChangeQuorumDialog D7**

In `src/lib/components/ChangeQuorumDialog.svelte` — the native `<dialog>`, `showModal()` flow, both aria-labels, the N+1/survivability paragraph, the validation, and the submitting guards all stay byte-identical.

(a) Add the import:

```typescript
  import PipMeter from './governance/PipMeter.svelte';
```

(b) Insert between the `.control-row` div and the `{#if errorMessage}` block:

```svelte
  <div class="quorum-preview">
    <PipMeter filled={proposedQuorum} total={currentAdminCount} label="Proposed quorum preview" />
  </div>

  <!-- Copy on ONE line: the test matches raw textContent (no whitespace
       normalization), so a line break inside the sentence would break it. -->
  <div class="quorum-warning">
    ⚖ This change is itself an admin action — it needs the current {currentQuorum}-of-{currentAdminCount} quorum to take effect.
  </div>
```

(c) Relabel the confirm button text `Propose` → `Propose change` (its disabled expression is unchanged).

(d) Replace the `<style>` block:

```css
  .change-quorum-dialog {
    padding: 1.5rem;
    min-width: 24rem;
    max-width: 30rem;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 10px;
    box-shadow: var(--shadow-e2);
    color: var(--text-primary);
  }
  .change-quorum-dialog::backdrop { background: var(--overlay); }
  h2 {
    margin: 0 0 0.5rem;
    font-family: var(--font-display);
    font-weight: 500;
    font-size: 1.2rem;
  }
  .control-row { display: flex; align-items: center; gap: 0.75rem; margin-block: 1rem; }
  .control-row input[type="range"] { flex: 1; }
  .control-row input[type="number"] {
    width: 5rem;
    background: var(--input-bg);
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 4px 8px;
    color: var(--text-primary);
    font-family: var(--font-mono);
  }
  .of-label { white-space: nowrap; font-size: 0.9rem; color: var(--text-muted); }
  .quorum-preview { margin-block: 0.75rem; }
  .quorum-warning {
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    color: var(--gov-clay-deep);
    border-radius: 7px;
    padding: 0.6rem 0.8rem;
    font-size: 0.8rem;
    line-height: 1.45;
    margin-block: 1rem;
  }
  .actions { display: flex; gap: 0.5rem; justify-content: flex-end; margin-top: 1rem; }
  .actions button {
    padding: 6px 14px;
    border-radius: 7px;
    font: inherit;
    cursor: pointer;
  }
  .actions button:disabled { cursor: not-allowed; opacity: 0.5; }
  .actions button:first-child {
    border: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
  }
  .actions button:last-child {
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--text-bright);
    font-weight: 600;
  }
  .error { color: var(--danger-deep); }
</style>
```

- [ ] **Step 6: Run the ChangeQuorumDialog tests**

Run: `npx vitest run src/lib/components/__tests__/ChangeQuorumDialog.test.ts`
Expected: all 8 PASS (6 pre-existing unedited — every `/Propose/i` query still matches "Propose change").

- [ ] **Step 7: Full frontend gate + commit**

```bash
npx tsc --noEmit && npx vitest run
git add src/lib/components/SetPowerDialog.svelte src/lib/components/ChangeQuorumDialog.svelte src/lib/components/__tests__/SetPowerDialog.test.ts src/lib/components/__tests__/ChangeQuorumDialog.test.ts
git commit -m "ZEB-608 T6: SetPowerDialog band track + ChangeQuorumDialog Commons chrome & quorum warning"
```

---

## Final sweep (after all tasks, before PR)

```bash
npx tsc --noEmit && npx vitest run
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

All four must be green — `scripts/test-select` is for iteration only; the full sweep is the backstop.
