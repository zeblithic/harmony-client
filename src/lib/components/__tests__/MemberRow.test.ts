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

  it('Escape closes the kebab menu from anywhere in the row (ZEB-386)', async () => {
    const modMember = makeMember(50, 'joined', 'cc'.repeat(16));
    const { getByRole, queryByRole } = render(MemberRow, {
      props: {
        member: modMember,
        viewer: { addr: VIEWER_ADDR, power: 100, isLastAdmin: false },
        onaction: vi.fn(),
      },
    });

    await fireEvent.click(getByRole('button', { name: 'Member actions' }));
    expect(getByRole('menuitem', { name: 'Kick' })).toBeTruthy();

    // The Escape handler is a `menuOpen`-gated document listener (not a listener
    // on the row/wrapper), so a keydown dispatched anywhere must still close it.
    await fireEvent.keyDown(document.body, { key: 'Escape' });
    expect(queryByRole('menuitem', { name: 'Kick' })).toBeNull();
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

  it('Moderator viewer on OWN row: NO kebab button (cannot self-demote — backend admin-only)', () => {
    // Regression test for the self-demote gate (Cursor finding on commit ccba30f).
    // Backend `verify_event` for SetPower requires actor_power >= 100. A mod
    // (viewerPower=50) cannot issue setPowerLevel; the UI must not surface a
    // self-demote option that would always be rejected. Mods who want to step
    // down should use the community-leave flow instead.
    const selfMod = makeMember(50, 'joined', VIEWER_ADDR);
    const { queryByRole } = render(MemberRow, {
      props: {
        member: selfMod,
        viewer: { addr: VIEWER_ADDR, power: 50, isLastAdmin: false },
        onaction: vi.fn(),
      },
    });

    // Empty action list → no kebab affordance at all
    expect(queryByRole('button', { name: 'Member actions' })).toBeNull();
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

describe('MemberRow per-community thresholds (ZEB-942)', () => {
  // MemberRow's kebab gates must mirror the backend `verify_event`
  // (community_membership.rs): Kick needs actor_power >= thresholds.kick AND
  // strictly-greater than the target; non-admin-affecting SetPower needs
  // actor_power >= thresholds.setPower; admin-affecting SetPower (grant/remove
  // the fixed max tier) needs actor_power >= 100 regardless of a lowered
  // set_power; Unban needs actor_power >= thresholds.setPower.

  it('lowered kick: a sub-50 authorized moderator sees Kick (thresholds.kick=25)', async () => {
    // A member at power 30 is authorized to kick when kick threshold is 25.
    // The hard-coded `>= 50` gate would wrongly deny the control.
    const memberTarget = makeMember(0, 'joined', 'dd'.repeat(16));
    const { getByRole } = render(MemberRow, {
      props: {
        member: memberTarget,
        viewer: { addr: VIEWER_ADDR, power: 30, isLastAdmin: false },
        thresholds: { kick: 25, setPower: 100 },
        onaction: vi.fn(),
      },
    });

    await fireEvent.click(getByRole('button', { name: 'Member actions' }));
    expect(getByRole('menuitem', { name: 'Kick' })).toBeTruthy();
  });

  it('raised kick: a power-50 member does NOT see Kick (thresholds.kick=75)', () => {
    // The backend rejects a power-50 actor when kick threshold is 75; the UI
    // must not dangle a control that would always fail. With no other action
    // available, the kebab affordance is absent entirely.
    const memberTarget = makeMember(0, 'joined', 'dd'.repeat(16));
    const { queryByRole } = render(MemberRow, {
      props: {
        member: memberTarget,
        viewer: { addr: VIEWER_ADDR, power: 50, isLastAdmin: false },
        thresholds: { kick: 75, setPower: 100 },
        onaction: vi.fn(),
      },
    });

    expect(queryByRole('button', { name: 'Member actions' })).toBeNull();
  });

  it('lowered setPower: non-admin-affecting promote is enabled, but Promote to Admin stays gated at 100', async () => {
    // thresholds.setPower=50 authorizes a power-50 actor for non-admin-affecting
    // SetPower (promote a member to mod). But granting admin (level 100) is
    // admin-affecting and requires the fixed max tier (100) regardless of the
    // lowered set_power — so Promote to Admin must remain hidden.
    const memberTarget = makeMember(0, 'joined', 'dd'.repeat(16));
    const { getByRole, queryByRole } = render(MemberRow, {
      props: {
        member: memberTarget,
        viewer: { addr: VIEWER_ADDR, power: 50, isLastAdmin: false },
        thresholds: { kick: 50, setPower: 50 },
        onaction: vi.fn(),
      },
    });

    await fireEvent.click(getByRole('button', { name: 'Member actions' }));
    expect(getByRole('menuitem', { name: 'Promote to Moderator' })).toBeTruthy();
    expect(queryByRole('menuitem', { name: 'Promote to Admin' })).toBeNull();
  });

  it('lowered setPower: demoting a mod to member (non-admin-affecting) is enabled for a power-50 actor', async () => {
    // Demote-to-Member on a mod (target power 50 → 0) is NOT admin-affecting, so
    // thresholds.setPower=50 authorizes it. Kick stays hidden (not strictly
    // greater: 50 is not > 50).
    const modTarget = makeMember(50, 'joined', 'cc'.repeat(16));
    const { getByRole, queryByRole } = render(MemberRow, {
      props: {
        member: modTarget,
        viewer: { addr: VIEWER_ADDR, power: 50, isLastAdmin: false },
        thresholds: { kick: 50, setPower: 50 },
        onaction: vi.fn(),
      },
    });

    await fireEvent.click(getByRole('button', { name: 'Member actions' }));
    expect(getByRole('menuitem', { name: 'Demote to Member' })).toBeTruthy();
    expect(queryByRole('menuitem', { name: 'Kick' })).toBeNull();
  });
});

describe('MemberRow display-name resolution (ZEB-432)', () => {
  const ADDR = 'cc'.repeat(16);
  const member = makeMember(0, 'joined', ADDR);
  const viewer = { addr: VIEWER_ADDR, power: 0, isLastAdmin: false };

  it('resolves the local friend nickname OVER the profile-card name and hex', () => {
    const { getByText, queryByText } = render(MemberRow, {
      props: {
        member,
        viewer,
        resolveCard: (id: string) =>
          id === ADDR ? { displayName: 'ZEBbot', statusText: '' } : undefined,
        resolveNickname: (id: string) => (id === ADDR ? 'Jake-nick' : undefined),
      },
    });
    expect(getByText('Jake-nick')).toBeTruthy();
    // The card name must not win when a nickname exists.
    expect(queryByText('ZEBbot')).toBeNull();
  });

  it('falls back to the profile-card name when there is no nickname', () => {
    const { getByText } = render(MemberRow, {
      props: {
        member,
        viewer,
        resolveCard: (id: string) =>
          id === ADDR ? { displayName: 'ZEBbot', statusText: '' } : undefined,
        resolveNickname: () => undefined,
      },
    });
    expect(getByText('ZEBbot')).toBeTruthy();
  });

  it('falls back to truncated hex when neither nickname nor card resolves', () => {
    const { getByText } = render(MemberRow, {
      props: {
        member,
        viewer,
        resolveCard: () => undefined,
        resolveNickname: () => undefined,
      },
    });
    expect(getByText(ADDR.slice(0, 8))).toBeTruthy();
  });

  it('the owner-card popover carries the SIGNED card name, not the nickname (PR #240 review)', async () => {
    const onOpenCard = vi.fn();
    const { container } = render(MemberRow, {
      props: {
        member,
        viewer,
        resolveCard: (id: string) =>
          id === ADDR ? { displayName: 'ZEBbot', statusText: '' } : undefined,
        resolveNickname: (id: string) => (id === ADDR ? 'Jake-nick' : undefined),
        onOpenCard,
      },
    });
    const nameBtn = container.querySelector('.name-btn');
    expect(nameBtn?.textContent).toContain('Jake-nick'); // inline label = nickname
    await fireEvent.click(nameBtn!);
    expect(onOpenCard).toHaveBeenCalled();
    expect(onOpenCard.mock.calls[0][0].displayName).toBe('ZEBbot'); // popover = card name
  });
});

describe('MemberRow presence dot (ZEB-553)', () => {
  const PEER = 'cc'.repeat(16);

  it('shows self online even when the resolver reports offline (zenoh no self-loopback)', () => {
    const selfMember = makeMember(0, 'joined', VIEWER_ADDR);
    const { container } = render(MemberRow, {
      props: {
        member: selfMember,
        viewer: { addr: VIEWER_ADDR, power: 0, isLastAdmin: false },
        isOnline: () => false, // resolver never reports our own beacon
      },
    });
    expect(container.querySelector('.presence-dot.online')).not.toBeNull();
  });

  it('shows a non-self member offline when the resolver reports offline', () => {
    const peer = makeMember(0, 'joined', PEER);
    const { container } = render(MemberRow, {
      props: {
        member: peer,
        viewer: { addr: VIEWER_ADDR, power: 0, isLastAdmin: false },
        isOnline: () => false,
      },
    });
    expect(container.querySelector('.presence-dot')).not.toBeNull(); // dot still present
    expect(container.querySelector('.presence-dot.online')).toBeNull(); // but not lit
  });

  it('reflects the resolver for a non-self online member', () => {
    const peer = makeMember(0, 'joined', PEER);
    const { container } = render(MemberRow, {
      props: {
        member: peer,
        viewer: { addr: VIEWER_ADDR, power: 0, isLastAdmin: false },
        isOnline: (id: string) => id === PEER,
      },
    });
    expect(container.querySelector('.presence-dot.online')).not.toBeNull();
  });

  it('marks the presence dot with role="img" + an online/offline label (finding 15)', () => {
    const peer = makeMember(0, 'joined', PEER);
    const { container } = render(MemberRow, {
      props: {
        member: peer,
        viewer: { addr: VIEWER_ADDR, power: 0, isLastAdmin: false },
        isOnline: () => false,
      },
    });
    const dot = container.querySelector('.presence-dot');
    expect(dot?.getAttribute('role')).toBe('img');
    expect(dot?.getAttribute('aria-label')).toMatch(/offline/i);
  });

  it('renders the row as a listitem (finding 15)', () => {
    const peer = makeMember(0, 'joined', PEER);
    const { container } = render(MemberRow, {
      props: { member: peer, viewer: { addr: VIEWER_ADDR, power: 0, isLastAdmin: false } },
    });
    expect(container.querySelector('.member-row')?.getAttribute('role')).toBe('listitem');
  });
});

describe('MemberRow self-invisible dot (ZEB-600)', () => {
  const PEER = 'cc'.repeat(16);

  it('renders the self dot hollow (not online) when selfInvisible is true', () => {
    const selfMember = makeMember(0, 'joined', VIEWER_ADDR);
    const { container } = render(MemberRow, {
      props: {
        member: selfMember,
        viewer: { addr: VIEWER_ADDR, power: 0, isLastAdmin: false },
        isOnline: () => false,
        selfInvisible: true,
      },
    });
    const dot = container.querySelector('.presence-dot');
    expect(dot).not.toBeNull();
    expect(dot?.classList.contains('online')).toBe(false);
    expect(dot?.classList.contains('self-invisible')).toBe(true);
    expect(dot?.getAttribute('aria-label')).toMatch(/appearing offline/i);
  });

  it('keeps the self dot online when selfInvisible is false', () => {
    const selfMember = makeMember(0, 'joined', VIEWER_ADDR);
    const { container } = render(MemberRow, {
      props: {
        member: selfMember,
        viewer: { addr: VIEWER_ADDR, power: 0, isLastAdmin: false },
        isOnline: () => false,
        selfInvisible: false,
      },
    });
    const dot = container.querySelector('.presence-dot');
    expect(dot?.classList.contains('online')).toBe(true);
    expect(dot?.classList.contains('self-invisible')).toBe(false);
  });

  it('does not apply the invisible style to a non-self member', () => {
    const peer = makeMember(0, 'joined', PEER);
    const { container } = render(MemberRow, {
      props: {
        member: peer,
        viewer: { addr: VIEWER_ADDR, power: 0, isLastAdmin: false },
        isOnline: () => true,
        selfInvisible: true, // set, but must only affect the self row
      },
    });
    const dot = container.querySelector('.presence-dot');
    expect(dot?.classList.contains('self-invisible')).toBe(false);
    expect(dot?.classList.contains('online')).toBe(true);
  });
});
