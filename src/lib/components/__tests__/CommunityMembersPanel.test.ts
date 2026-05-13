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
