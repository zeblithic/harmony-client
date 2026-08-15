import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import CommunityMembersPanel from '../CommunityMembersPanel.svelte';
import type { CommunityMember } from '../../types';

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const COMMUNITY_ID = 'aabbccdd' + 'ee'.repeat(28);
const OWN_ADDRESS = 'alice'.padEnd(32, '0');

const alice: CommunityMember = {
  address: OWN_ADDRESS,
  displayName: 'Alice',
  power: 100,
  status: 'joined',
  joinedAt: 1700000000000,
};

const bob: CommunityMember = {
  address: 'bob'.padEnd(32, '0'),
  displayName: 'Bob',
  power: 50,
  status: 'joined',
  joinedAt: 1700000001000,
};

const eve: CommunityMember = {
  address: 'eve'.padEnd(32, '0'),
  displayName: 'Eve',
  power: 0,
  status: 'banned',
  joinedAt: 1700000002000,
};

const allMembers = [alice, bob, eve];

// ---------------------------------------------------------------------------
// Mock service factory
// ---------------------------------------------------------------------------

function makeService(
  members: CommunityMember[] = allMembers,
  membersError?: Error,
) {
  return {
    onMembersChanged: undefined as ((communityId: string) => void) | undefined,
    listCommunityMembers: membersError
      ? vi.fn().mockRejectedValue(membersError)
      : vi.fn().mockResolvedValue(members),
    listRecentModerationEvents: vi.fn().mockResolvedValue([]),
    kickFromCommunity: vi.fn(),
    unbanFromCommunity: vi.fn(),
    setPowerLevel: vi.fn(),
  };
}

const baseProps = () => ({
  communityId: COMMUNITY_ID,
  communityName: 'IPFS Crew',
  communityService: makeService(),
  ownAddress: OWN_ADDRESS,
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('CommunityMembersPanel', () => {
  it('renders active members sorted by power descending (Alice before Bob)', async () => {
    render(CommunityMembersPanel, { props: baseProps() });

    // Wait for async load to complete — Alice is the viewer so name renders as "Alice (you)"
    await screen.findByText(/Alice/);
    const activeList = screen.getByRole('list', { name: 'Active members' });
    const names = Array.from(activeList.querySelectorAll('.name')).map((el) => el.textContent ?? '');

    const aliceIdx = names.findIndex((n) => n.includes('Alice'));
    const bobIdx = names.findIndex((n) => n.includes('Bob'));

    expect(aliceIdx).toBeGreaterThanOrEqual(0);
    expect(bobIdx).toBeGreaterThanOrEqual(0);
    expect(aliceIdx).toBeLessThan(bobIdx);
  });

  it('shows an online count in the header (self + isOnline-true members)', async () => {
    // alice is self (always online); mark bob online via isOnline → 2 online.
    const isOnline = (addr: string) => addr === bob.address;
    const { container } = render(CommunityMembersPanel, { props: { ...baseProps(), isOnline } });
    await screen.findByText(/Alice/);
    expect(container.querySelector('.panel-title')?.textContent ?? '').toMatch(/2 online/);
  });

  it('excludes self from the online count when selfInvisible is on', async () => {
    // Same as above but the viewer is invisible → self is NOT counted → 1 online.
    const isOnline = (addr: string) => addr === bob.address;
    const { container } = render(CommunityMembersPanel, {
      props: { ...baseProps(), isOnline, selfInvisible: true },
    });
    await screen.findByText(/Alice/);
    expect(container.querySelector('.panel-title')?.textContent ?? '').toMatch(/1 online/);
  });

  it('sorts invisible self below actually-online members', async () => {
    // alice is self+invisible; bob is online. Online bob must float above alice.
    const isOnline = (addr: string) => addr === bob.address;
    render(CommunityMembersPanel, {
      props: { ...baseProps(), isOnline, selfInvisible: true },
    });
    await screen.findByText(/Alice/);
    const activeList = screen.getByRole('list', { name: 'Active members' });
    const names = Array.from(activeList.querySelectorAll('.name')).map((el) => el.textContent ?? '');
    const bobIdx = names.findIndex((n) => n.includes('Bob'));
    const aliceIdx = names.findIndex((n) => n.includes('Alice'));
    expect(bobIdx).toBeGreaterThanOrEqual(0);
    expect(aliceIdx).toBeGreaterThanOrEqual(0);
    expect(bobIdx).toBeLessThan(aliceIdx); // online Bob above invisible-self Alice
  });

  it('sorts online members before offline (online-first)', async () => {
    const carol: CommunityMember = {
      address: 'carol'.padEnd(32, '0'),
      displayName: 'Carol',
      power: 10,
      status: 'joined',
      joinedAt: 1700000003000,
    };
    // Backend order bob(offline) then carol(online); no self in this list so
    // presence alone drives the sort.
    const isOnline = (addr: string) => addr === carol.address;
    render(CommunityMembersPanel, {
      props: {
        ...baseProps(),
        communityService: makeService([bob, carol]),
        ownAddress: 'nobody'.padEnd(32, '0'),
        isOnline,
      },
    });
    await screen.findByText(/Carol/);
    const activeList = screen.getByRole('list', { name: 'Active members' });
    const names = Array.from(activeList.querySelectorAll('.name')).map((el) => el.textContent ?? '');
    const carolIdx = names.findIndex((n) => n.includes('Carol'));
    const bobIdx = names.findIndex((n) => n.includes('Bob'));
    expect(carolIdx).toBeGreaterThanOrEqual(0);
    expect(bobIdx).toBeGreaterThanOrEqual(0);
    expect(carolIdx).toBeLessThan(bobIdx); // online Carol floats above offline Bob
  });

  it('Banned section visible (with count) when banned members exist', async () => {
    render(CommunityMembersPanel, { props: baseProps() });

    // Eve is banned — the <details> summary should appear
    await screen.findByText(/Banned \(1\)/i);
  });

  it('Search filter shows Alice and hides Bob when typing "alice"', async () => {
    render(CommunityMembersPanel, { props: baseProps() });

    // Wait for load — Alice is the viewer so name renders as "Alice (you)"
    await screen.findByText(/Alice/);

    const searchInput = screen.getByRole('searchbox', { name: /filter members/i });
    await fireEvent.input(searchInput, { target: { value: 'alice' } });

    const activeList = screen.getByRole('list', { name: 'Active members' });
    const names = Array.from(activeList.querySelectorAll('.name')).map((el) => el.textContent ?? '');

    expect(names.some((n) => n.includes('Alice'))).toBe(true);
    expect(names.some((n) => n.includes('Bob'))).toBe(false);
  });

  it('threads per-community thresholds into member rows — lowered kick shows Kick to a sub-50 moderator (ZEB-942)', async () => {
    // Viewer at power 30 with a lowered kick threshold of 25 is authorized to
    // kick per the backend. The control must appear on the target row. If the
    // panel failed to forward `thresholds`, MemberRow would fall back to the
    // default kick=50 and hide the control — so this pins the
    // CommunityView → panel → MemberRow wiring, not just MemberRow's own logic.
    const viewer30: CommunityMember = {
      address: OWN_ADDRESS,
      displayName: 'Alice',
      power: 30,
      status: 'joined',
      joinedAt: 1700000000000,
    };
    const dave: CommunityMember = {
      address: 'dave'.padEnd(32, '0'),
      displayName: 'Dave',
      power: 0,
      status: 'joined',
      joinedAt: 1700000001000,
    };
    render(CommunityMembersPanel, {
      props: {
        ...baseProps(),
        communityService: makeService([viewer30, dave]),
        thresholds: { kick: 25, setPower: 100 },
      },
    });

    await screen.findByText(/Dave/);
    // Only Dave's row is actionable (self-row has no actions at power 30), so
    // there is exactly one kebab. Its presence already proves authorization;
    // open it to confirm the specific control is Kick.
    await fireEvent.click(screen.getByRole('button', { name: 'Member actions' }));
    expect(screen.getByRole('menuitem', { name: 'Kick' })).toBeTruthy();
  });

  it('IPC error surfaces as role="alert" in the panel', async () => {
    const service = makeService([], new Error('network timeout'));
    render(CommunityMembersPanel, {
      props: {
        communityId: COMMUNITY_ID,
        communityName: 'IPFS Crew',
        communityService: service,
        ownAddress: OWN_ADDRESS,
      },
    });

    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('network timeout');
  });
});
