import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import TownHallMotionCard from '../TownHallMotionCard.svelte';
import type { PollMeta } from '../../types/voting';

const CID = 'aa'.repeat(16);
const CHID = 'bb'.repeat(16);

function hexToBytes(hex: string): number[] {
  const out: number[] = [];
  for (let i = 0; i < hex.length; i += 2) out.push(parseInt(hex.slice(i, i + 2), 16));
  return out;
}

function pollMeta(pollIdHex: string, overrides: Partial<PollMeta> = {}): PollMeta {
  return {
    poll_id: hexToBytes(pollIdHex),
    community_id: hexToBytes(CID),
    creator: hexToBytes('cc'.repeat(16)),
    tier: 1,
    eligibility: { min_power: 0 },
    lifecycle: 'Open',
    created_at: { w: 1000, l: 0, d: 'd1' },
    opens_at: { w: 1000, l: 0, d: 'd1' },
    closes_at: { w: 301000, l: 0, d: 'd1' },
    channel_id: hexToBytes(CHID),
    ...overrides,
  } as PollMeta;
}

function makeAdapter(over: Record<string, unknown> = {}) {
  const meta = pollMeta('11'.repeat(32));
  return {
    createTier1Poll: vi.fn(async () => '11'.repeat(32)),
    getPoll: vi.fn(async () => ({
      meta,
      tally: { counts: [0, 0], ballot_count: 0 },
      options: ['Aye — Adopt the charter', 'Nay — Adopt the charter'],
    })),
    listActivePolls: vi.fn(async () => []),
    subscribePollCreated: vi.fn(() => () => {}),
    subscribeBallotCast: vi.fn(() => () => {}),
    castTier1Ballot: vi.fn(async () => {}),
    createTier2Proposal: vi.fn(async () => '22'.repeat(32)),
    ...over,
  };
}

const base = { communityId: CID, channelId: CHID, presentCount: 5, quorum: 3, canAct: true };

describe('TownHallMotionCard (ZEB-612 S5)', () => {
  it('idle state shows the call-to-motion affordance with a title input', () => {
    render(TownHallMotionCard, { props: { ...base, adapter: makeAdapter() as never } });
    expect(screen.getByText('⚖ Call this to a motion')).toBeInTheDocument();
    expect(screen.getByLabelText('Motion title')).toBeInTheDocument();
    expect(screen.getByTestId('motion-call')).toBeDisabled(); // empty title
  });

  it('present ≥ quorum: Call creates a 300s Tier-1 poll with Aye/Nay titled options and renders it live', async () => {
    const adapter = makeAdapter();
    render(TownHallMotionCard, { props: { ...base, adapter: adapter as never } });
    await fireEvent.input(screen.getByLabelText('Motion title'), {
      target: { value: 'Adopt the charter' },
    });
    await fireEvent.click(screen.getByTestId('motion-call'));
    await waitFor(() => {
      expect(adapter.createTier1Poll).toHaveBeenCalledWith({
        communityId: CID,
        channelId: CHID,
        options: ['Aye — Adopt the charter', 'Nay — Adopt the charter'],
        windowSeconds: 300,
        minPower: 0,
        quorum: 3,
      });
    });
    // Live card: embedded PollMessage renders the titled options.
    await waitFor(() => {
      expect(screen.getByText('Aye — Adopt the charter')).toBeInTheDocument();
    });
    expect(screen.getByTestId('motion-live-meta')).toHaveTextContent('live · 5 present');
  });

  it('present < quorum: Call shows the DRAFT card (no poll created)', async () => {
    const adapter = makeAdapter();
    render(TownHallMotionCard, {
      props: { ...base, presentCount: 1, quorum: 3, adapter: adapter as never },
    });
    await fireEvent.input(screen.getByLabelText('Motion title'), {
      target: { value: 'Adopt the charter' },
    });
    await fireEvent.click(screen.getByTestId('motion-call'));
    expect(adapter.createTier1Poll).not.toHaveBeenCalled();
    expect(screen.getByTestId('motion-draft-badge')).toHaveTextContent('DRAFT');
    expect(screen.getByTestId('motion-quorum')).toHaveTextContent('1 / 3');
    expect(
      screen.getByText('Not enough present for a live vote — open it as an async proposal instead.'),
    ).toBeInTheDocument();
  });

  it('DRAFT → Open as async proposal creates the standard Tier-2 proposal and links to proposals', async () => {
    const adapter = makeAdapter();
    const onOpenProposals = vi.fn();
    render(TownHallMotionCard, {
      props: { ...base, presentCount: 1, quorum: 3, adapter: adapter as never, onOpenProposals },
    });
    await fireEvent.input(screen.getByLabelText('Motion title'), {
      target: { value: 'Adopt the charter' },
    });
    await fireEvent.click(screen.getByTestId('motion-call'));
    await fireEvent.click(screen.getByTestId('motion-open-async'));
    await waitFor(() => {
      expect(adapter.createTier2Proposal).toHaveBeenCalledWith({
        communityId: CID,
        channelId: CHID,
        proposalText: 'Adopt the charter',
      });
    });
    await fireEvent.click(screen.getByTestId('motion-view-proposals'));
    expect(onOpenProposals).toHaveBeenCalled();
  });

  it('adopts an existing open Tier-1 poll on this channel at mount (peer-created motion)', async () => {
    const meta = pollMeta('11'.repeat(32));
    const adapter = makeAdapter({ listActivePolls: vi.fn(async () => [meta]) });
    render(TownHallMotionCard, { props: { ...base, adapter: adapter as never } });
    await waitFor(() => {
      expect(screen.getByText('Aye — Adopt the charter')).toBeInTheDocument();
    });
  });

  it('surfaces a create failure in-card as an alert', async () => {
    const adapter = makeAdapter({
      createTier1Poll: vi.fn(async () => {
        throw new Error('creator not eligible');
      }),
    });
    render(TownHallMotionCard, { props: { ...base, adapter: adapter as never } });
    await fireEvent.input(screen.getByLabelText('Motion title'), {
      target: { value: 'Adopt the charter' },
    });
    await fireEvent.click(screen.getByTestId('motion-call'));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent(/creator not eligible/);
  });
});
