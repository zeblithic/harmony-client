/**
 * ZEB-292 Phase 3 — DelegationWidget vitest coverage.
 *
 * Widget is a pure render+actions surface: the parent
 * (CommunityProposalsPanel) owns the `currentDelegate` state and
 * passes it as a prop. The widget only fires set/revoke IPCs and
 * renders the picker / confirmation dialogs.
 */
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import DelegationWidget from '../DelegationWidget.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type { CommunityMember } from '../../types';
import type { Tier2ProposalExport } from '../../types/voting';

const COMMUNITY_ID = 'bb'.repeat(16);
const ME = 'aa'.repeat(16);
const BOB = 'cc'.repeat(16);
const CAROL = 'dd'.repeat(16);

function makeMember(addr: string, displayName: string, power = 1): CommunityMember {
  return { address: addr, displayName, power, status: 'joined' };
}

function makeProposal(overrides: Partial<Tier2ProposalExport> = {}): Tier2ProposalExport {
  return {
    proposal_id: 'ff'.repeat(32),
    community_id: COMMUNITY_ID,
    proposal_text: 'sample',
    lifecycle: 'Open',
    total_conviction_ms: '100',
    threshold_conviction_ms: '1000',
    half_life_seconds: 7 * 86400,
    auto_exec: { kk: 'n' },
    total_supply: 10,
    voter_count: 1,
    your_signal: undefined,
    threshold_reached_at_ms: undefined,
    ...overrides,
  };
}

describe('DelegationWidget', () => {
  let adapter: VotingAdapter;
  let delegateMock: ReturnType<typeof vi.fn<(cid: string, d: string) => Promise<void>>>;
  let undelegateMock: ReturnType<typeof vi.fn<(cid: string) => Promise<void>>>;
  let listProposalsMock: ReturnType<typeof vi.fn<(cid: string) => Promise<Tier2ProposalExport[]>>>;

  const members: CommunityMember[] = [
    makeMember(ME, 'me'),
    makeMember(BOB, 'bob'),
    makeMember(CAROL, 'carol'),
  ];

  beforeEach(() => {
    adapter = new VotingAdapter();
    delegateMock = vi
      .fn<(cid: string, d: string) => Promise<void>>()
      .mockResolvedValue(undefined);
    undelegateMock = vi
      .fn<(cid: string) => Promise<void>>()
      .mockResolvedValue(undefined);
    listProposalsMock = vi
      .fn<(cid: string) => Promise<Tier2ProposalExport[]>>()
      .mockResolvedValue([]);
    adapter.delegateTier2 = delegateMock;
    adapter.undelegateTier2 = undelegateMock;
    adapter.listTier2Proposals = listProposalsMock;
  });

  it('renders "Voting directly" when currentDelegate is null', () => {
    render(DelegationWidget, {
      props: {
        communityId: COMMUNITY_ID,
        adapter,
        myAddr: ME,
        communityMembers: members,
        currentDelegate: null,
      },
    });
    expect(screen.getByText(/voting directly/i)).toBeInTheDocument();
  });

  it('renders the delegate display name when currentDelegate is set', () => {
    render(DelegationWidget, {
      props: {
        communityId: COMMUNITY_ID,
        adapter,
        myAddr: ME,
        communityMembers: members,
        currentDelegate: BOB,
      },
    });
    // The picker `<option>` also contains "bob"; assert on the
    // status-line `<strong>` specifically so we don't match the
    // option element by accident.
    expect(screen.getByText('Delegated to', { exact: false })).toHaveTextContent('bob');
  });

  it('delegate-on-pick fires delegateTier2 IPC with the picked address', async () => {
    render(DelegationWidget, {
      props: {
        communityId: COMMUNITY_ID,
        adapter,
        myAddr: ME,
        communityMembers: members,
        currentDelegate: null,
      },
    });
    const select = (await screen.findByLabelText(/delegate to/i)) as HTMLSelectElement;
    await fireEvent.change(select, { target: { value: BOB } });
    await fireEvent.click(screen.getByRole('button', { name: /^delegate$/i }));
    await waitFor(() => {
      expect(delegateMock).toHaveBeenCalledWith(COMMUNITY_ID, BOB);
    });
  });

  it('revoke with no active proposals shows the click-confirm bar', async () => {
    listProposalsMock.mockResolvedValueOnce([]);
    render(DelegationWidget, {
      props: {
        communityId: COMMUNITY_ID,
        adapter,
        myAddr: ME,
        communityMembers: members,
        currentDelegate: BOB,
      },
    });
    await fireEvent.click(screen.getByText(/revoke delegation/i));
    expect(
      await screen.findByRole('alertdialog', { name: /confirm revoke/i }),
    ).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: /confirm revoke/i }));
    await waitFor(() => {
      expect(undelegateMock).toHaveBeenCalledWith(COMMUNITY_ID);
    });
  });

  it('revoke with high-participation active proposal shows typed-confirm', async () => {
    // 3 voters of 10 total supply = 30% participation, > 25% threshold.
    listProposalsMock.mockResolvedValueOnce([
      makeProposal({ lifecycle: 'Open', total_supply: 10, voter_count: 3 }),
    ]);
    render(DelegationWidget, {
      props: {
        communityId: COMMUNITY_ID,
        adapter,
        myAddr: ME,
        communityMembers: members,
        currentDelegate: BOB,
      },
    });
    await fireEvent.click(screen.getByText(/revoke delegation/i));
    const dialog = await screen.findByRole('dialog', { name: /type-to-confirm revoke/i });
    expect(dialog).toBeInTheDocument();
    // Typing wrong word should NOT enable the confirm button.
    const input = screen.getByPlaceholderText('revoke') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'no' } });
    expect(screen.getByRole('button', { name: /confirm revoke/i })).toBeDisabled();
    // Typing "revoke" enables and fires.
    await fireEvent.input(input, { target: { value: 'revoke' } });
    await fireEvent.click(screen.getByRole('button', { name: /confirm revoke/i }));
    await waitFor(() => {
      expect(undelegateMock).toHaveBeenCalledWith(COMMUNITY_ID);
    });
  });

  it('resets busy on community switch so the widget recovers when IPC was in-flight', async () => {
    // Cursor R8 regression: an in-flight delegate IPC at the moment of
    // community switch must not leave `busy` stuck on `true` in the new
    // community. The generation guard correctly suppresses the stale
    // callback's success-effects, but `busy` is widget-local UI state
    // that the community-swap $effect now resets explicitly.
    let resolveDelegate: () => void = () => {};
    delegateMock.mockImplementationOnce(
      () =>
        new Promise<void>((res) => {
          resolveDelegate = () => res();
        }),
    );
    const { rerender } = render(DelegationWidget, {
      props: {
        communityId: COMMUNITY_ID,
        adapter,
        myAddr: ME,
        communityMembers: members,
        currentDelegate: null,
      },
    });
    const select = screen.getByLabelText(/delegate to/i) as HTMLSelectElement;
    await fireEvent.change(select, { target: { value: BOB } });
    await fireEvent.click(screen.getByRole('button', { name: /^delegate$/i }));
    await waitFor(() => {
      expect(delegateMock).toHaveBeenCalledWith(COMMUNITY_ID, BOB);
    });
    // While IPC is pending, the button label flips to "Applying…" + disabled.
    expect(screen.getByRole('button', { name: /applying/i })).toBeDisabled();
    // Swap to a different community mid-IPC.
    const OTHER_COMMUNITY = '99'.repeat(16);
    await rerender({
      communityId: OTHER_COMMUNITY,
      adapter,
      myAddr: ME,
      communityMembers: members,
      currentDelegate: null,
    });
    // Without the busy reset, the button would still read "Applying…"
    // and stay disabled forever in the new community. The fix restores
    // the original "Delegate" label and re-enables once a member is picked.
    const select2 = screen.getByLabelText(/delegate to/i) as HTMLSelectElement;
    await fireEvent.change(select2, { target: { value: BOB } });
    expect(screen.getByRole('button', { name: /^delegate$/i })).not.toBeDisabled();
    // Resolve the orphaned IPC for cleanliness.
    resolveDelegate();
  });

  it('member picker excludes the caller and non-joined members', () => {
    render(DelegationWidget, {
      props: {
        communityId: COMMUNITY_ID,
        adapter,
        myAddr: ME,
        communityMembers: [
          makeMember(ME, 'me'),
          makeMember(BOB, 'bob'),
          { ...makeMember(CAROL, 'carol'), status: 'banned' },
        ],
        currentDelegate: null,
      },
    });
    const select = screen.getByLabelText(/delegate to/i) as HTMLSelectElement;
    const opts = Array.from(select.options).map((o) => o.value);
    expect(opts).toContain(BOB);
    expect(opts).not.toContain(ME);
    expect(opts).not.toContain(CAROL);
  });
});
