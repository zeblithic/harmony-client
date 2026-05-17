import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import PollMessage from '../PollMessage.svelte';
import { VotingAdapter } from '../../voting-adapter';
import { createMockAdapter } from '../../test-utils';
import type { PollMeta, PollStateExport } from '../../types/voting';

function makeMeta(overrides: Partial<PollMeta> = {}): PollMeta {
  return {
    poll_id: 'aa'.repeat(32),
    community_id: 'bb'.repeat(16),
    creator: 'cc'.repeat(16),
    tier: 1,
    eligibility: { mp: 0 },
    lifecycle: 'Open',
    created_at: { w: 100, l: 0, d: 'dev' },
    opens_at: { w: 100, l: 0, d: 'dev' },
    closes_at: { w: 1000, l: 0, d: 'dev' },
    channel_id: 'dd'.repeat(16),
    ...overrides,
  };
}

function makeState(overrides: Partial<PollStateExport> = {}): PollStateExport {
  return {
    meta: makeMeta(),
    tally: { counts: [2, 1], ballot_count: 3 },
    your_ballot: undefined,
    options: ['Yes', 'No'],
    ...overrides,
  };
}

describe('PollMessage', () => {
  let adapter: VotingAdapter;
  let getPollMock: ReturnType<typeof vi.fn<(pollId: string) => Promise<PollStateExport>>>;
  let castMock: ReturnType<typeof vi.fn<(pollId: string, approvedIndices: number[]) => Promise<void>>>;

  beforeEach(() => {
    adapter = new VotingAdapter();
    getPollMock = vi.fn<(pollId: string) => Promise<PollStateExport>>();
    castMock = vi
      .fn<(pollId: string, approvedIndices: number[]) => Promise<void>>()
      .mockResolvedValue(undefined);
    // Patch the adapter's methods directly so we don't need a real
    // Tauri adapter wired up — the component only ever calls these
    // two public methods after mount.
    adapter.getPoll = getPollMock;
    adapter.castTier1Ballot = castMock;
  });

  it('renders option labels from PollStateExport.options once fetched', async () => {
    const state = makeState({ options: ['Yes', 'No', 'Maybe'], tally: { counts: [3, 1, 0], ballot_count: 4 } });
    getPollMock.mockResolvedValue(state);

    render(PollMessage, {
      props: {
        pollId: 'aa'.repeat(32),
        meta: makeMeta(),
        adapter,
      },
    });

    // Header tier badge appears immediately from meta (pre-fetch).
    expect(screen.getByLabelText('Tier').textContent).toBe('Approval');

    // After getPoll resolves, the option labels and at-least-one
    // option button render.
    await waitFor(() => {
      expect(screen.getByText('Yes')).toBeTruthy();
    });
    expect(screen.getByText('No')).toBeTruthy();
    expect(screen.getByText('Maybe')).toBeTruthy();

    const buttons = screen.getAllByRole('button');
    expect(buttons.length).toBeGreaterThanOrEqual(1);
    // All option buttons should be enabled for an Open poll.
    for (const b of buttons) {
      expect((b as HTMLButtonElement).disabled).toBe(false);
    }
  });

  it('disables option buttons when the poll lifecycle is Closed', async () => {
    const state = makeState({
      meta: makeMeta({ lifecycle: 'Closed' }),
      tally: { counts: [5, 2], ballot_count: 7 },
    });
    getPollMock.mockResolvedValue(state);

    render(PollMessage, {
      props: {
        pollId: 'aa'.repeat(32),
        meta: makeMeta({ lifecycle: 'Closed' }),
        adapter,
      },
    });

    await waitFor(() => {
      expect(screen.getByText('Yes')).toBeTruthy();
    });

    const buttons = screen.getAllByRole('button');
    expect(buttons.length).toBeGreaterThanOrEqual(1);
    for (const b of buttons) {
      expect((b as HTMLButtonElement).disabled).toBe(true);
    }
    expect(screen.getByText('Voting closed')).toBeTruthy();
  });

  it('calls castTier1Ballot with the toggled option index when clicked', async () => {
    const state = makeState({ tally: { counts: [0, 0], ballot_count: 0 } });
    getPollMock.mockResolvedValue(state);

    render(PollMessage, {
      props: {
        pollId: 'aa'.repeat(32),
        meta: makeMeta(),
        adapter,
      },
    });

    await waitFor(() => {
      expect(screen.getByText('Yes')).toBeTruthy();
    });

    // Click the first option ("Yes").
    const yesBtn = screen.getByRole('button', { name: /Yes/ });
    await fireEvent.click(yesBtn);

    await waitFor(() => {
      expect(castMock).toHaveBeenCalled();
    });
    expect(castMock).toHaveBeenCalledWith('aa'.repeat(32), [0]);
  });

  it('does NOT send an empty ballot when the last selected option is deselected', async () => {
    // Voter already has their ballot at [0]; clicking option 0 again
    // would deselect it, producing an empty ballot that the backend
    // rejects as EmptyBallot. The component must silently no-op.
    const state = makeState({
      tally: { counts: [1, 0], ballot_count: 1 },
      your_ballot: [0],
    });
    getPollMock.mockResolvedValue(state);

    render(PollMessage, {
      props: { pollId: 'aa'.repeat(32), meta: makeMeta(), adapter },
    });

    await waitFor(() => {
      expect(screen.getByText('Yes')).toBeTruthy();
    });

    const yesBtn = screen.getByRole('button', { name: /Yes/ });
    await fireEvent.click(yesBtn);

    // Give any in-flight promise chain a chance to resolve.
    await new Promise((r) => setTimeout(r, 0));
    expect(castMock).not.toHaveBeenCalled();
  });

  it('does NOT send an approve-all ballot when every option would be selected', async () => {
    // Voter has [0]; clicking [1] would make their next ballot [0, 1]
    // which equals options.length — backend rejects as AbstentionBallot.
    const state = makeState({
      tally: { counts: [1, 0], ballot_count: 1 },
      your_ballot: [0],
    });
    getPollMock.mockResolvedValue(state);

    render(PollMessage, {
      props: { pollId: 'aa'.repeat(32), meta: makeMeta(), adapter },
    });

    await waitFor(() => {
      expect(screen.getByText('No')).toBeTruthy();
    });

    const noBtn = screen.getByRole('button', { name: /No/ });
    await fireEvent.click(noBtn);

    await new Promise((r) => setTimeout(r, 0));
    expect(castMock).not.toHaveBeenCalled();
  });
});

describe('VotingAdapter (consumer-side IPC contract)', () => {
  it('listens to all three voting events on connectAdapter', async () => {
    const va = new VotingAdapter();
    const { adapter } = createMockAdapter();
    await va.connectAdapter(adapter);
    expect(adapter.listen).toHaveBeenCalledWith('voting-poll-created', expect.any(Function));
    expect(adapter.listen).toHaveBeenCalledWith('voting-ballot-cast', expect.any(Function));
    expect(adapter.listen).toHaveBeenCalledWith('voting-poll-closed', expect.any(Function));
  });

  it('forwards voting-ballot-cast payloads to onBallotCast', async () => {
    const va = new VotingAdapter();
    const { adapter, emit } = createMockAdapter();
    await va.connectAdapter(adapter);
    const handler = vi.fn();
    va.onBallotCast = handler;
    const payload = { pollId: 'aa'.repeat(32), voter: 'bb'.repeat(16), approvedCount: 1 };
    emit('voting-ballot-cast', payload);
    expect(handler).toHaveBeenCalledWith(payload);
  });

  it('castTier1Ballot invokes voting_cast_tier1_ballot with camelCase args', async () => {
    const va = new VotingAdapter();
    const { adapter } = createMockAdapter();
    await va.connectAdapter(adapter);
    await va.castTier1Ballot('aa'.repeat(32), [0, 2]);
    expect(adapter.invoke).toHaveBeenCalledWith('voting_cast_tier1_ballot', {
      pollId: 'aa'.repeat(32),
      approvedIndices: [0, 2],
    });
  });
});
