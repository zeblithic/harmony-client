import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import MemberRow from '../MemberRow.svelte';
import type { CommunityMember } from '../../types';

function makeMember(
  power: number,
  status: CommunityMember['status'] = 'joined',
  address = 'aa'.repeat(16),
): CommunityMember {
  return {
    address,
    power,
    status,
    joinedAt: 1700000000000,
  };
}

const VIEWER_ADDR = 'bb'.repeat(16);

describe('MemberRow kebab-matrix', () => {
  it('Admin viewer on Moderator target: sees Kick, Promote to Admin, Demote to Member', async () => {
    const modMember = makeMember(50, 'joined', 'cc'.repeat(16));
    const onaction = vi.fn();
    const { getByRole, queryByRole } = render(MemberRow, {
      props: {
        member: modMember,
        viewer: { addr: VIEWER_ADDR, power: 100, isLastAdmin: false },
        onaction,
      },
    });

    const kebabBtn = getByRole('button', { name: 'Member actions' });
    await fireEvent.click(kebabBtn);

    expect(getByRole('menuitem', { name: 'Kick' })).toBeTruthy();
    expect(getByRole('menuitem', { name: 'Promote to Admin' })).toBeTruthy();
    expect(getByRole('menuitem', { name: 'Demote to Member' })).toBeTruthy();
    // No Unban on a joined member
    expect(queryByRole('menuitem', { name: 'Unban' })).toBeNull();
  });

  it('Moderator viewer on Member target: sees only Kick', async () => {
    const memberTarget = makeMember(0, 'joined', 'dd'.repeat(16));
    const { getByRole, queryByRole } = render(MemberRow, {
      props: {
        member: memberTarget,
        viewer: { addr: VIEWER_ADDR, power: 50, isLastAdmin: false },
        onaction: vi.fn(),
      },
    });

    const kebabBtn = getByRole('button', { name: 'Member actions' });
    await fireEvent.click(kebabBtn);

    expect(getByRole('menuitem', { name: 'Kick' })).toBeTruthy();
    expect(queryByRole('menuitem', { name: 'Promote to Moderator' })).toBeNull();
    expect(queryByRole('menuitem', { name: 'Promote to Admin' })).toBeNull();
    expect(queryByRole('menuitem', { name: 'Demote to Member' })).toBeNull();
  });

  it('Moderator viewer on Admin target: NO kebab button rendered', () => {
    const adminTarget = makeMember(100, 'joined', 'ee'.repeat(16));
    const { queryByRole } = render(MemberRow, {
      props: {
        member: adminTarget,
        viewer: { addr: VIEWER_ADDR, power: 50, isLastAdmin: false },
        onaction: vi.fn(),
      },
    });

    // Moderator (power 50) cannot act on Admin (power 100) — no kebab
    expect(queryByRole('button', { name: 'Member actions' })).toBeNull();
  });

  it('Member viewer on any target: NO kebab button rendered', () => {
    const anyTarget = makeMember(50, 'joined', 'ff'.repeat(16));
    const { queryByRole } = render(MemberRow, {
      props: {
        member: anyTarget,
        viewer: { addr: VIEWER_ADDR, power: 0, isLastAdmin: false },
        onaction: vi.fn(),
      },
    });

    expect(queryByRole('button', { name: 'Member actions' })).toBeNull();
  });

  it('Admin viewer on Banned target: sees Unban, does NOT see Kick', async () => {
    const bannedTarget = makeMember(0, 'banned', '11'.repeat(16));
    const { getByRole, queryByRole } = render(MemberRow, {
      props: {
        member: bannedTarget,
        viewer: { addr: VIEWER_ADDR, power: 100, isLastAdmin: false },
        onaction: vi.fn(),
      },
    });

    const kebabBtn = getByRole('button', { name: 'Member actions' });
    await fireEvent.click(kebabBtn);

    expect(getByRole('menuitem', { name: 'Unban' })).toBeTruthy();
    expect(queryByRole('menuitem', { name: 'Kick' })).toBeNull();
  });

  it('Admin viewer on PEER Admin target: sees Demote actions but NOT Kick', async () => {
    // Regression test for the peer-admin demote gate (Qodo bug report on PR #117).
    // SetPower requires only `actor_power >= 100`; kick requires `actor_power
    // > target_power` (strictly-greater). So admin-on-admin can demote but
    // not kick.
    const peerAdmin = makeMember(100, 'joined', '22'.repeat(16));
    const { getByRole, queryByRole } = render(MemberRow, {
      props: {
        member: peerAdmin,
        viewer: { addr: VIEWER_ADDR, power: 100, isLastAdmin: false },
        onaction: vi.fn(),
      },
    });

    const kebabBtn = getByRole('button', { name: 'Member actions' });
    await fireEvent.click(kebabBtn);

    expect(getByRole('menuitem', { name: 'Demote to Moderator' })).toBeTruthy();
    expect(getByRole('menuitem', { name: 'Demote to Member' })).toBeTruthy();
    // Kick is gated strictly-greater; peer admin cannot kick peer admin
    expect(queryByRole('menuitem', { name: 'Kick' })).toBeNull();
  });

  it("Admin viewer on own row when last admin: sees Demote to Moderator action", async () => {
    const selfMember = makeMember(100, 'joined', VIEWER_ADDR);
    const { getByRole } = render(MemberRow, {
      props: {
        member: selfMember,
        viewer: { addr: VIEWER_ADDR, power: 100, isLastAdmin: true },
        onaction: vi.fn(),
      },
    });

    const kebabBtn = getByRole('button', { name: 'Member actions' });
    await fireEvent.click(kebabBtn);

    // Demotion actions are available at the row level; last-admin gate is at the panel level
    expect(getByRole('menuitem', { name: 'Demote to Moderator' })).toBeTruthy();
  });
});
