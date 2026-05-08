import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import CommunitySettingsPanel from '../CommunitySettingsPanel.svelte';
import type { CommunityMember } from '../../types';

const adminMember: CommunityMember = { address: 'a3f8c1d2', displayName: 'Alice', power: 100, status: 'joined' };
const modMember: CommunityMember = { address: 'cc99', displayName: 'Charlie', power: 50, status: 'joined' };
const plainMember: CommunityMember = { address: 'b1c4', displayName: 'Bob', power: 0, status: 'joined' };

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

  it('Leave as only admin opens tier-3 typed-confirmation', async () => {
    const { getByText, getByPlaceholderText } = render(CommunitySettingsPanel, { props: baseProps });
    await fireEvent.click(getByText(/Leave community/i));
    expect(getByPlaceholderText(/Type community name/i)).toBeTruthy();
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

  it('filters members by address substring', async () => {
    const { container, getByPlaceholderText } = render(CommunitySettingsPanel, { props: baseProps });
    const input = getByPlaceholderText('Search members...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'cc99' } });
    const visibleNames = Array.from(container.querySelectorAll('.member-row .name')).map((el) => el.textContent);
    expect(visibleNames.some((n) => n?.includes('Charlie'))).toBe(true);
    expect(visibleNames.some((n) => n?.includes('Alice'))).toBe(false);
  });
});
