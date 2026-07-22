import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import CommunitySettingsPanel from '../CommunitySettingsPanel.svelte';
import type { CommunityMember, CommunityLineageDto, ForkDescendantDto } from '../../types';

// ZEB-250: CommunitySettingsPanel now imports invoke (for pending-badge map)
// and mounts PendingAdminProposalsPanel (which also imports invoke + listen).
// Hoist mocks so all tests render cleanly; default to empty-list resolution.
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn().mockResolvedValue([]),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

const adminMember: CommunityMember = { address: 'a3f8c1d2', displayName: 'Alice', power: 100, status: 'joined' };
const modMember: CommunityMember = { address: 'cc99', displayName: 'Charlie', power: 50, status: 'joined' };
const plainMember: CommunityMember = { address: 'b1c4', displayName: 'Bob', power: 0, status: 'joined' };
const secondAdmin: CommunityMember = { address: 'dd11', displayName: 'Dana', power: 100, status: 'joined' };

const baseProps = {
  communityId: 'aabbccdd' + 'ee'.repeat(28),
  communityName: 'IPFS Crew',
  communityKind: 'invite-only' as const,
  members: [adminMember, modMember, plainMember],
  myAddress: adminMember.address,
  myPower: 100,
  isDegraded: false,
  sharedInProfile: false,
  onClose: vi.fn(),
  onKick: vi.fn(),
  onSetPower: vi.fn(),
  onLeave: vi.fn(),
  onGenerateInvite: vi.fn().mockResolvedValue('harmony://invite/...'),
  onToggleSharedInProfile: vi.fn().mockResolvedValue(undefined),
};

describe('CommunitySettingsPanel', () => {
  it('renders Info / Members / Invites / Danger sections', () => {
    const { getByText } = render(CommunitySettingsPanel, { props: baseProps });
    expect(getByText('Info')).toBeTruthy();
    expect(getByText('Members (3)')).toBeTruthy();
    expect(getByText('Invites')).toBeTruthy();
    expect(getByText(/Danger/)).toBeTruthy();
  });

  it('Info section shows community name, kind, member count, your role', () => {
    const { getByText, getAllByText } = render(CommunitySettingsPanel, { props: baseProps });
    // 'IPFS Crew' appears in subtitle + name field; we just need at least one
    expect(getAllByText('IPFS Crew').length).toBeGreaterThan(0);
    expect(getByText(/Invite-only/i)).toBeTruthy();
    expect(getByText(/3 joined/i)).toBeTruthy();
    // ADMIN appears as the role badge for Alice (caller); test by getAllByText to be flexible
    expect(getAllByText('ADMIN').length).toBeGreaterThan(0);
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
    const rows = container.querySelectorAll('.member-row');
    const bobRow = Array.from(rows).find((r) => r.textContent?.includes('Bob'));
    expect(bobRow?.querySelector('button.kick')).toBeTruthy();
    expect(bobRow?.querySelector('button.set-role')).toBeTruthy();
  });

  it("does NOT render kick/set-role buttons on the caller's own row", () => {
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
    const { container, getByRole } = render(CommunitySettingsPanel, { props: baseProps });
    const rows = container.querySelectorAll('.member-row');
    const bobRow = Array.from(rows).find((r) => r.textContent?.includes('Bob'))!;
    const kickBtn = bobRow.querySelector('button.kick') as HTMLButtonElement;
    await fireEvent.click(kickBtn);
    // Tighter query: get the destructive button by role + name (disambiguates
    // from the title text which is in an <h3>, not a button).
    expect(getByRole('button', { name: /Kick Bob/i })).toBeTruthy();
  });

  it('Leave with other admins opens tier-2 confirmation (not tier-3)', async () => {
    const otherAdmin: CommunityMember = { address: 'dd99', displayName: 'Diana', power: 100, status: 'joined' };
    const { getByText, queryByPlaceholderText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, members: [...baseProps.members, otherAdmin] },
    });
    await fireEvent.click(getByText(/Leave community/i));
    // Should NOT show typed-confirm input (other admin exists)
    expect(queryByPlaceholderText(/Type community name/i)).toBeNull();
  });

  it('Leave as only admin opens LastAdminWarningDialog with LEAVE token requirement', async () => {
    const { getByText, getByLabelText } = render(CommunitySettingsPanel, { props: baseProps });
    await fireEvent.click(getByText(/Leave community/i));
    // LastAdminWarningDialog renders a label "To proceed, type LEAVE below:"
    // Match the explicit token (not just "type ...") so the test fails if
    // the action-token branching swaps LEAVE for the wrong word.
    expect(getByLabelText(/To proceed, type LEAVE/)).toBeTruthy();
    // The warning title should mention "last admin"
    expect(getByText(/last admin/i)).toBeTruthy();
  });

  it('renders a search input in the Members section', () => {
    const { getByPlaceholderText } = render(CommunitySettingsPanel, { props: baseProps });
    expect(getByPlaceholderText('Search members...')).toBeTruthy();
  });

  it('filters members by displayName substring (case-insensitive)', async () => {
    const { container, getByPlaceholderText } = render(CommunitySettingsPanel, { props: baseProps });
    const input = getByPlaceholderText('Search members...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'bob' } });
    const visibleNames = Array.from(container.querySelectorAll('.member-row .name')).map((el) => el.textContent);
    expect(visibleNames.some((n) => n?.includes('Bob'))).toBe(true);
    expect(visibleNames.some((n) => n?.includes('Alice'))).toBe(false);
    expect(visibleNames.some((n) => n?.includes('Charlie'))).toBe(false);
  });

  it('clicking the panel backdrop dismisses the panel', async () => {
    const onClose = vi.fn();
    const { container } = render(CommunitySettingsPanel, { props: { ...baseProps, onClose } });
    const overlay = container.querySelector('.panel-overlay') as HTMLElement;
    await fireEvent.click(overlay);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('clicking inside the panel does NOT dismiss', async () => {
    const onClose = vi.fn();
    const { container } = render(CommunitySettingsPanel, { props: { ...baseProps, onClose } });
    const panel = container.querySelector('.panel') as HTMLElement;
    await fireEvent.click(panel);
    expect(onClose).not.toHaveBeenCalled();
  });

  it('Set role: Member↔Mod transition does NOT open a tier-2 confirmation (no admin threshold crossing)', async () => {
    const onSetPower = vi.fn();
    const { container, queryByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, onSetPower },
    });
    const rows = container.querySelectorAll('.member-row');
    const bobRow = Array.from(rows).find((r) => r.textContent?.includes('Bob'))!;
    await fireEvent.click(bobRow.querySelector('button.set-role') as HTMLButtonElement);

    // Set Bob (power 0) to power 50 — Member → Mod, no threshold crossing.
    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    await fireEvent.input(numberInput, { target: { value: '50' } });
    // Click the dialog's confirm button (.confirm-btn), not the row's
    // "Set role" trigger which has the same accessible name.
    await fireEvent.click(container.querySelector('.confirm-btn') as HTMLButtonElement);

    // No admin-threshold confirmation modal should appear.
    expect(queryByText(/Promote .* to admin\?/i)).toBeNull();
    expect(queryByText(/Demote .* from admin\?/i)).toBeNull();
    expect(onSetPower).toHaveBeenCalledWith('b1c4', 50);
  });

  it('Set role: promote-to-admin opens tier-2 confirmation before invoking onSetPower', async () => {
    const onSetPower = vi.fn();
    const { container, getByRole, queryByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, onSetPower },
    });
    const rows = container.querySelectorAll('.member-row');
    const charlieRow = Array.from(rows).find((r) => r.textContent?.includes('Charlie'))!;
    await fireEvent.click(charlieRow.querySelector('button.set-role') as HTMLButtonElement);

    // Promote Charlie (power 50) to admin (power 100) — crosses threshold up.
    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    await fireEvent.input(numberInput, { target: { value: '100' } });
    // Click the dialog's confirm button (.confirm-btn), not the row's
    // "Set role" trigger which has the same accessible name.
    await fireEvent.click(container.querySelector('.confirm-btn') as HTMLButtonElement);

    // SetPowerDialog should close, ConfirmationModal should appear instead.
    expect(queryByText(/Promote Charlie to admin\?/i)).toBeTruthy();
    expect(onSetPower).not.toHaveBeenCalled();

    // Accept the confirmation — only now should onSetPower fire.
    await fireEvent.click(getByRole('button', { name: /Promote to admin/i }));
    expect(onSetPower).toHaveBeenCalledWith('cc99', 100);
  });

  it('Set role: demote-from-admin opens tier-2 confirmation before invoking onSetPower', async () => {
    const onSetPower = vi.fn();
    const otherAdmin: CommunityMember = { address: 'dd99', displayName: 'Diana', power: 100, status: 'joined' };
    const { container, queryByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, members: [...baseProps.members, otherAdmin], onSetPower },
    });
    // Diana is power 100 — but the caller is also 100 and canSetPower
    // requires myPower > target.power, so Diana's row has no Set-role
    // button. We can't demote a peer admin in v1. Verify that.
    const rows = container.querySelectorAll('.member-row');
    const dianaRow = Array.from(rows).find((r) => r.textContent?.includes('Diana'))!;
    expect(dianaRow.querySelector('button.set-role')).toBeNull();
    expect(queryByText(/Demote Diana from admin\?/i)).toBeNull();
  });

  it('Set role: cancelling the tier-2 admin confirmation does NOT invoke onSetPower', async () => {
    const onSetPower = vi.fn();
    const { container, getByRole, queryByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, onSetPower },
    });
    const rows = container.querySelectorAll('.member-row');
    const charlieRow = Array.from(rows).find((r) => r.textContent?.includes('Charlie'))!;
    await fireEvent.click(charlieRow.querySelector('button.set-role') as HTMLButtonElement);

    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    await fireEvent.input(numberInput, { target: { value: '100' } });
    // Click the dialog's confirm button (.confirm-btn), not the row's
    // "Set role" trigger which has the same accessible name.
    await fireEvent.click(container.querySelector('.confirm-btn') as HTMLButtonElement);

    // Cancel the admin-threshold confirmation.
    await fireEvent.click(getByRole('button', { name: /Cancel/i }));
    expect(onSetPower).not.toHaveBeenCalled();
    expect(queryByText(/Promote Charlie to admin\?/i)).toBeNull();
  });

  it('filters members by address substring', async () => {
    const { container, getByPlaceholderText } = render(CommunitySettingsPanel, { props: baseProps });
    const input = getByPlaceholderText('Search members...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'cc99' } });
    const visibleNames = Array.from(container.querySelectorAll('.member-row .name')).map((el) => el.textContent);
    expect(visibleNames.some((n) => n?.includes('Charlie'))).toBe(true);
    expect(visibleNames.some((n) => n?.includes('Alice'))).toBe(false);
  });

  it('settings_panel_toggle_invokes_set_shared', async () => {
    const onToggle = vi.fn().mockResolvedValue(undefined);
    const { getByLabelText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, onToggleSharedInProfile: onToggle },
    });
    // The <input type="checkbox"> sits inside a <label> wrapping the
    // "Share this community in my public profile" text — Testing
    // Library's getByLabelText matches via the implicit label.
    const checkbox = getByLabelText(/share this community in my public profile/i);
    await fireEvent.click(checkbox);
    expect(onToggle).toHaveBeenCalledWith(true);
  });

  // ── ZEB-287 Phase 2: Forks section tests ──────────────────────────

  it('forks_section_always_renders_for_non_fork_community', () => {
    // No phase2Lineage prop → tree absent but section still renders.
    const { getByText } = render(CommunitySettingsPanel, { props: baseProps });
    expect(getByText('Forks')).toBeTruthy();
  });

  it('forks_section_renders_explainer_text_present', () => {
    const { getByText } = render(CommunitySettingsPanel, { props: baseProps });
    // Substring matches on the final explainer wording from spec §5.3.
    expect(getByText(/Any member of a community can fork it at any time/)).toBeTruthy();
    expect(getByText(/communities preserve continuity if members want to take/)).toBeTruthy();
  });

  it('fork_this_community_button_inside_forks_section', () => {
    // Pass an onFork callback so the button renders.
    const { container } = render(CommunitySettingsPanel, {
      props: { ...baseProps, onFork: vi.fn() },
    });

    const forksSection = container.querySelector('.forks-section');
    expect(forksSection).toBeTruthy();

    const forkButton = forksSection!.querySelector('button.fork-this-community');
    expect(forkButton).toBeTruthy();
    expect(forkButton?.textContent).toMatch(/Fork this community/);
  });

  it('shows the "This is a fork of {parent}" callout for a forked community', async () => {
    const parentHex = '22'.repeat(16);
    const lineage: CommunityLineageDto = {
      forkedFrom: parentHex,
      forkedAtWallMs: 1_700_000_000_000,
      parentLineage: [{ spaceId: parentHex, name: 'Origin Community', forkedAtWallMs: null, reason: null }],
      selfSpaceId: '33'.repeat(16),
      selfName: 'The Fork',
      forkReason: null,
    };
    const onNav = vi.fn();
    const { container } = render(CommunitySettingsPanel, {
      props: {
        ...baseProps,
        phase2Lineage: lineage,
        localNavIds: new Set([parentHex]),
        onForkLineageNavigate: onNav,
      },
    });
    const callout = container.querySelector('.fork-of-callout');
    expect(callout).toBeTruthy();
    expect(callout!.textContent).toContain('This is a fork of');
    expect(callout!.textContent).toContain('Origin Community');
    // "Open ↗" is present because the parent is locally known, and it navigates.
    const openBtn = callout!.querySelector('.fork-of-open') as HTMLButtonElement;
    expect(openBtn).toBeTruthy();
    await fireEvent.click(openBtn);
    expect(onNav).toHaveBeenCalledWith(parentHex);
  });

  it('omits the "fork of" callout for a root community', () => {
    const lineage: CommunityLineageDto = {
      forkedFrom: null, forkedAtWallMs: null, parentLineage: [],
      selfSpaceId: '11'.repeat(16), selfName: 'Root',
      forkReason: null,
    };
    const { container } = render(CommunitySettingsPanel, {
      props: { ...baseProps, phase2Lineage: lineage },
    });
    expect(container.querySelector('.fork-of-callout')).toBeNull();
  });

  it('ZEB-649: "View genealogy" opens the graph modal for a fork; hidden for a fork-less root', async () => {
    const parentHex = '22'.repeat(16);
    const lineage: CommunityLineageDto = {
      forkedFrom: parentHex,
      forkedAtWallMs: 1_700_000_000_000,
      parentLineage: [{ spaceId: parentHex, name: 'Origin Community', forkedAtWallMs: null, reason: null }],
      selfSpaceId: '33'.repeat(16),
      selfName: 'The Fork',
      forkReason: 'Treasury split',
    };
    const { container } = render(CommunitySettingsPanel, {
      props: { ...baseProps, phase2Lineage: lineage },
    });
    const btn = container.querySelector('.view-genealogy-btn') as HTMLButtonElement;
    expect(btn).toBeTruthy();
    expect(document.body.querySelector('[data-testid="fork-genealogy"]')).toBeNull();
    await fireEvent.click(btn);
    const graph = document.body.querySelector('[data-testid="fork-genealogy"]');
    expect(graph).toBeTruthy();
    expect(graph!.textContent).toContain('“Treasury split”');

    // Root community with no descendants: no genealogy affordance.
    const { container: rootContainer } = render(CommunitySettingsPanel, {
      props: {
        ...baseProps,
        phase2Lineage: {
          forkedFrom: null, forkedAtWallMs: null, parentLineage: [],
          selfSpaceId: '11'.repeat(16), selfName: 'Root',
          forkReason: null,
        },
      },
    });
    expect(rootContainer.querySelector('.view-genealogy-btn')).toBeNull();
  });

  it('shows the callout without an "Open" button when the parent is not locally known', () => {
    const parentHex = '22'.repeat(16);
    const lineage: CommunityLineageDto = {
      forkedFrom: parentHex,
      forkedAtWallMs: 1_700_000_000_000,
      parentLineage: [{ spaceId: parentHex, name: 'Origin Community', forkedAtWallMs: null, reason: null }],
      selfSpaceId: '33'.repeat(16),
      selfName: 'The Fork',
      forkReason: null,
    };
    const { container } = render(CommunitySettingsPanel, {
      props: { ...baseProps, phase2Lineage: lineage, localNavIds: new Set() },
    });
    const callout = container.querySelector('.fork-of-callout');
    expect(callout).toBeTruthy();
    // Parent not in localNavIds → no navigable "Open" affordance.
    expect(callout!.querySelector('.fork-of-open')).toBeNull();
  });

  it('falls back to a truncated id when the fork parent name is blank (PR #411 Qodo)', () => {
    const parentHex = 'ab'.repeat(16);
    const lineage: CommunityLineageDto = {
      forkedFrom: parentHex,
      forkedAtWallMs: 1_700_000_000_000,
      parentLineage: [{ spaceId: parentHex, name: '', forkedAtWallMs: null, reason: null }],
      selfSpaceId: '33'.repeat(16),
      selfName: 'The Fork',
      forkReason: null,
    };
    const { container } = render(CommunitySettingsPanel, {
      props: { ...baseProps, phase2Lineage: lineage, localNavIds: new Set() },
    });
    const name = container.querySelector('.fork-of-callout .fork-of-name');
    expect(name?.textContent).toContain('0xabababab…');
  });

  // ── ZEB-250: Admin governance section tests ───────────────────────────────

  it('admin_governance_section_renders_for_admin', async () => {
    const { getByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, myPower: 100, adminQuorum: 2 },
    });
    // Section label visible for admin caller.
    expect(getByText('Admin governance')).toBeTruthy();
    // Quorum info line rendered.
    await waitFor(() => {
      expect(getByText(/Current admin quorum: 2 of/)).toBeTruthy();
    });
    // Change quorum button present.
    expect(getByText('Change quorum…')).toBeTruthy();
  });

  it('admin_governance_section_hidden_for_non_admin', () => {
    const { queryByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, myPower: 0, myAddress: plainMember.address, adminQuorum: 2 },
    });
    // Section must be absent for a plain member.
    expect(queryByText('Admin governance')).toBeNull();
    expect(queryByText('Change quorum…')).toBeNull();
  });

  // ── ZEB-251: per-community power thresholds wiring ─────────────────────────

  it('admin_governance_section_shows_change_thresholds_button_for_admin', () => {
    const { getByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, myPower: 100 },
    });
    expect(getByText('Change thresholds…')).toBeTruthy();
  });

  it('admin_governance_section_hides_change_thresholds_button_for_non_admin', () => {
    const { queryByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, myPower: 0, myAddress: plainMember.address },
    });
    expect(queryByText('Change thresholds…')).toBeNull();
  });

  it('clicking Change thresholds… opens ChangeThresholdsDialog with current thresholds', async () => {
    const { getByText, getByLabelText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, myPower: 100, thresholds: { invite: 10, kick: 40, setPower: 90 } },
    });
    await fireEvent.click(getByText('Change thresholds…'));
    expect(getByLabelText('Change power thresholds')).toBeTruthy();
    expect((getByLabelText('Invite threshold number') as HTMLInputElement).value).toBe('10');
    expect((getByLabelText('Kick threshold number') as HTMLInputElement).value).toBe('40');
    expect((getByLabelText('Set-power threshold number') as HTMLInputElement).value).toBe('90');
  });

  it('per-community invite threshold overrides the POWER_THRESHOLDS default (ZEB-251)', () => {
    // Default invite threshold is 0 (everyone can invite). A customized
    // community raising invite to 50 must hide the Invites section for a
    // caller whose power (10) is below the custom threshold.
    const { queryByText } = render(CommunitySettingsPanel, {
      props: { ...baseProps, myPower: 10, thresholds: { invite: 50, kick: 50, setPower: 100 } },
    });
    expect(queryByText('Invites')).toBeNull();
  });

  it('pending_promotion_badge_renders_on_target_member_row', async () => {
    const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));
    const adminProposal = {
      event_id: 'bb'.repeat(16),
      proposer_addr: adminMember.address,
      proposer_display_name: 'Alice',
      proposal_kind: {
        kind: 'SetPower',
        target_addr: plainMember.address,
        target_display_name: 'Bob',
        level: 100,
      },
      proposed_at_wall_ms: Date.now(),
      signers_so_far: 1,
      quorum_required: 2,
      expired: false,
      effective: false,
      self_has_signed: false,
      signer_display_names: ['Alice'],
    };
    // Use a per-command implementation so only list_pending_admin_proposals
    // receives the proposal. Other IPC calls (e.g. list_pending_join_requests
    // from PendingJoinsPanel) must receive [] to avoid shape-mismatch errors.
    (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'list_pending_admin_proposals') return Promise.resolve([adminProposal]);
      return Promise.resolve([]);
    });

    const { container } = render(CommunitySettingsPanel, {
      props: { ...baseProps, myPower: 100 },
    });

    // Wait for the async $effect to resolve and update the DOM.
    await waitFor(() => {
      const bobRow = Array.from(container.querySelectorAll('.member-row')).find((r) =>
        r.textContent?.includes('Bob'),
      );
      expect(bobRow?.querySelector('.pending-badge')).toBeTruthy();
      expect(bobRow?.querySelector('.pending-badge')?.textContent).toMatch(
        /pending promotion to admin/,
      );
    });
  });

  it('resolveLocalCommunityName_prop_flows_through_to_ForkLineageTree', () => {
    // R4-1 regression: R3 added `resolveLocalCommunityName` to the type
    // signature but forgot to pull it out of $props() destructuring, so
    // ForkLineageTree received `undefined` and the descendant-name
    // resolution silently no-op'd. This test asserts the prop is actually
    // invoked with a locally-known descendant's hex.
    const forkHex = '33'.repeat(16);
    const selfHex = '11'.repeat(16);
    const lineage: CommunityLineageDto = {
      forkedFrom: null,
      forkedAtWallMs: null,
      parentLineage: [],
      selfSpaceId: selfHex,
      selfName: 'Root',
      forkReason: null,
    };
    const descendants: ForkDescendantDto[] = [
      {
        forkSpaceId: forkHex,
        forkerAddr: 'ab'.repeat(16),
        forkerDisplayName: 'Maya',
        forkedAtWallMs: 1_715_000_000_000,
        locallyKnown: true,
        reason: null,
      },
    ];
    const resolver = vi.fn((hex: string) =>
      hex === forkHex ? 'Resolved Descendant Name' : null,
    );

    const { getByText } = render(CommunitySettingsPanel, {
      props: {
        ...baseProps,
        phase2Lineage: lineage,
        descendants,
        localNavIds: new Set([forkHex]),
        resolveLocalCommunityName: resolver,
      },
    });

    // The resolver MUST be called by ForkLineageTree — this fails fast if
    // CommunitySettingsPanel's $props() destructuring drops the prop again.
    expect(resolver).toHaveBeenCalledWith(forkHex);
    // The resolved name must reach the DOM (end-to-end through the props chain).
    expect(getByText(/Resolved Descendant Name/)).toBeTruthy();
  });

  // ── ZEB-582: per-community relay opt-in toggle ────────────────────────────

  it('relay_toggle_reflects_get_community_relay_status', async () => {
    const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));
    (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'get_community_relay_status') return Promise.resolve(true);
      return Promise.resolve([]);
    });
    const { getByLabelText } = render(CommunitySettingsPanel, { props: baseProps });
    // The async status read flips the checkbox checked once it resolves.
    await waitFor(() => {
      const cb = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      expect(cb.checked).toBe(true);
    });
  });

  it('relay_toggle_invokes_set_community_relay_opt_in_with_camelCase_args', async () => {
    const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));
    const calls: Array<[string, unknown]> = [];
    (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string, args?: unknown) => {
      calls.push([cmd, args]);
      // Start opted-out so a click sets it ON.
      if (cmd === 'get_community_relay_status') return Promise.resolve(false);
      return Promise.resolve([]);
    });
    const { getByLabelText } = render(CommunitySettingsPanel, { props: baseProps });
    // Wait for the initial status read to settle (toggle enabled, unchecked).
    const cb = await waitFor(() => {
      const el = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      expect(el.disabled).toBe(false);
      expect(el.checked).toBe(false);
      return el;
    });
    await fireEvent.click(cb);
    // The setter must be called with the backend's snake_case→camelCase arg
    // names (communityIdHex / optedIn) — a wrong key silently arrives undefined.
    await waitFor(() => {
      const setCall = calls.find(([c]) => c === 'set_community_relay_opt_in');
      expect(setCall).toBeTruthy();
      expect(setCall![1]).toEqual({ communityIdHex: baseProps.communityId, optedIn: true });
    });
  });

  it('relay_toggle_stale_completion_does_not_clobber_after_community_switch', async () => {
    // CodeRabbit PR #357: switching communities while a set is in flight must
    // not let the old community's completion mutate the new community's toggle.
    const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));
    // A `deferred` object (not a `let`) so TS doesn't narrow the
    // closure-assigned reject handle to `never` at the call site.
    const deferred: { reject?: (e: unknown) => void } = {};
    (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'get_community_relay_status') return Promise.resolve(false);
      if (cmd === 'set_community_relay_opt_in') {
        // Community A's set stays pending until we reject it below.
        return new Promise((_resolve, reject) => {
          deferred.reject = reject;
        });
      }
      return Promise.resolve([]);
    });
    const { getByLabelText, queryByText, rerender } = render(CommunitySettingsPanel, {
      props: baseProps,
    });
    const cbA = await waitFor(() => {
      const el = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      expect(el.disabled).toBe(false);
      return el;
    });
    await fireEvent.click(cbA); // community A: set now in flight (pending)
    // The set must actually be in flight before we reject it — otherwise this
    // test could pass for the wrong reason (no error ever had a chance to
    // surface). Wait for the pending call to be observed (CodeRabbit #357).
    await waitFor(() => expect(deferred.reject).toBeDefined());
    // Switch to community B before A's set resolves.
    await rerender({ ...baseProps, communityId: '11'.repeat(32) });
    await waitFor(() => {
      const el = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      // B's fresh status read cleared A's pending → toggle usable, unchecked.
      expect(el.disabled).toBe(false);
      expect(el.checked).toBe(false);
    });
    // Fail community A's stale set — its error must NOT surface on community B.
    deferred.reject!(new Error('stale-A-failure'));
    // Flush the rejection through the microtask + timer queue so the `.catch`
    // actually runs before we assert its effect was dropped — a bare waitFor
    // could pass before the handler executes.
    await new Promise((r) => setTimeout(r, 0));
    await waitFor(() => {
      expect(queryByText(/stale-A-failure/)).toBeNull();
      const el = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      expect(el.disabled).toBe(false);
    });
  });

  it('relay_failed_status_load_does_not_show_previous_community_value', async () => {
    // CodeRabbit PR #357 (Major): if the new community's status read REJECTS,
    // the checkbox must not keep showing the prior community's opted-in value.
    const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));
    let failNext = false;
    (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      if (cmd === 'get_community_relay_status') {
        // Community A reads opted-in (true); community B's read fails.
        return failNext
          ? Promise.reject(new Error('relay-status-unavailable'))
          : Promise.resolve(true);
      }
      return Promise.resolve([]);
    });
    const { getByLabelText, rerender } = render(CommunitySettingsPanel, { props: baseProps });
    // A: opted-in → checkbox checked.
    await waitFor(() => {
      const el = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      expect(el.checked).toBe(true);
    });
    // Switch to B; its status read will reject.
    failNext = true;
    await rerender({ ...baseProps, communityId: '22'.repeat(32) });
    await waitFor(() => {
      const el = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      // Load finished (error path) → checkbox re-enabled, but must show B's
      // neutral default (false), NOT A's stale `true`.
      expect(el.disabled).toBe(false);
      expect(el.checked).toBe(false);
    });
  });

  it('relay_toggle_stale_completion_dropped_after_A_B_A_roundtrip', async () => {
    // CodeRabbit PR #357 (Major): community-id alone can't tell apart an
    // A → B → A round-trip. The original A set, completing after we've returned
    // to A, must still be dropped (relayStatusSeq generation guard), not
    // clobber the re-entered A's fresh state.
    const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));
    const deferred: { reject?: (e: unknown) => void } = {};
    (invoke as ReturnType<typeof vi.fn>).mockImplementation((cmd: string) => {
      // Every community reads opted-in (true) so the checkbox is checked.
      if (cmd === 'get_community_relay_status') return Promise.resolve(true);
      if (cmd === 'set_community_relay_opt_in') {
        return new Promise((_resolve, reject) => {
          deferred.reject = reject;
        });
      }
      return Promise.resolve([]);
    });
    const idA = baseProps.communityId;
    const { getByLabelText, queryByText, rerender } = render(CommunitySettingsPanel, {
      props: baseProps,
    });
    const cbA = await waitFor(() => {
      const el = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      expect(el.disabled).toBe(false);
      expect(el.checked).toBe(true);
      return el;
    });
    await fireEvent.click(cbA); // A: toggle OFF, set now in flight (pending)
    await waitFor(() => expect(deferred.reject).toBeDefined());
    // A → B …
    await rerender({ ...baseProps, communityId: '11'.repeat(32) });
    await waitFor(() => {
      const el = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      expect(el.disabled).toBe(false);
    });
    // … → A (same id, but a NEW load generation).
    await rerender({ ...baseProps, communityId: idA });
    await waitFor(() => {
      const el = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      expect(el.disabled).toBe(false);
      expect(el.checked).toBe(true); // re-entered A's fresh value
    });
    // The ORIGINAL A set finally fails. Without the generation guard, its
    // `cid === communityId` (A === A) check would pass and surface the error /
    // roll back the re-entered A. It must be dropped.
    deferred.reject!(new Error('stale-original-A-failure'));
    await new Promise((r) => setTimeout(r, 0));
    await waitFor(() => {
      expect(queryByText(/stale-original-A-failure/)).toBeNull();
      const el = getByLabelText(/Relay this community for offline members/i) as HTMLInputElement;
      expect(el.disabled).toBe(false);
      expect(el.checked).toBe(true); // unclobbered
    });
  });

  // ── ZEB-608: Commons restyle (D5) — pip meter + token migration ───────────

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
});
