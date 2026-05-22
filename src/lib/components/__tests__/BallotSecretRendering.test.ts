/**
 * ZEB-295 Phase 6 Task 11 — frontend rendering tests for ballot-secret
 * (se-mode) ratification polls.
 *
 * Covers:
 *  1. StarRatificationBallot renders the lock-icon encryption banner when
 *     `detail.privacyMode === 'se'`.
 *  2. Tier3ProposalPanel renders the awaiting-tally committee-progress
 *     banner when an se-mode poll is in the ratification stage, the
 *     window has closed, and no winner has been finalized yet.
 *  3. Tier3ProposalPanel renders the 🔒 privacy chip in the poll-list
 *     row for se-mode polls (no chip for pu-mode rows).
 *
 * Pattern reference: shape-matches StarRatificationBallot.test.ts and
 * Tier3ProposalPanel.test.ts.
 */

import { render } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import StarRatificationBallot from '../StarRatificationBallot.svelte';
import Tier3ProposalPanel from '../Tier3ProposalPanel.svelte';
import { VotingAdapter } from '../../voting-adapter';
import type {
  Tier3PollExport,
  Tier3PollSummary,
} from '../../types/voting';

// Baseline se-mode export. Tests adjust `stage`, `pollCreateHlcMs`, and
// the encryptedTally* fields per scenario.
function seBaseExport(overrides: Partial<Tier3PollExport> = {}): Tier3PollExport {
  return {
    pollId: 'aa'.repeat(32),
    communityId: '11'.repeat(16),
    proposalText: 'Amend §3 (ballot-secret)',
    proposer: 'pp'.repeat(32),
    stage: 'ra',
    pollCreateHlcMs: 1_700_000_000_000,
    sortitionSize: 100,
    deliberationWindowSeconds: 1_209_600,
    draftingWindowSeconds: 604_800,
    ratificationWindowSeconds: 1_209_600,
    incentiveMode: 'd',
    miniPublic: ['mm'.repeat(32)],
    backupPool: [],
    declined: [],
    draftCandidates: [],
    ratificationCandidates: [
      { eventHash: 'aa'.repeat(32), text: 'Candidate A' },
      { eventHash: 'bb'.repeat(32), text: 'Candidate B' },
    ],
    myRole: 'observer',
    myDraftingApprovals: [],
    myRatificationScores: null,
    deliberationStatements: [],
    myDeliberationStatementCount: 0,
    myDeliberationVotes: [],
    winnerEventHash: null,
    runnerUpEventHash: null,
    privacyMode: 'se',
    encryptedTallyShareCount: 0,
    encryptedTallyThreshold: 2,
    encryptedTallyCommitteeSize: 3,
    ...overrides,
  };
}

function createPanelAdapterMock(
  summaries: Tier3PollSummary[] = [],
  detail: Tier3PollExport | null = null,
): VotingAdapter {
  const adapter = new VotingAdapter();
  vi.spyOn(adapter, 'listTier3Polls').mockResolvedValue(summaries);
  if (detail) {
    vi.spyOn(adapter, 'getTier3Poll').mockResolvedValue(detail);
  }
  vi.spyOn(adapter, 'createTier3Proposal').mockResolvedValue('pollid'.padEnd(64, '0'));
  vi.spyOn(adapter, 'subscribeTier3PollCreated').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3SortitionComplete').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3DraftingOpen').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3RatificationOpen').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3Finalized').mockReturnValue(() => {});
  vi.spyOn(adapter, 'subscribeTier3TallyShareApplied').mockReturnValue(() => {});
  return adapter;
}

describe('StarRatificationBallot — se-mode rendering', () => {
  it('renders the lock-icon encryption banner when privacyMode is "se"', () => {
    const adapter = new VotingAdapter();
    const { getByText } = render(StarRatificationBallot, {
      props: { detail: seBaseExport(), adapter, onCast: () => {} },
    });
    // Banner copy from the StarRatificationBallot template; assert on
    // the load-bearing phrase "encrypted to the community committee" so
    // a wording tweak (e.g. swapping in a hyphen) is unlikely to break.
    expect(getByText(/encrypted to the community committee/i)).toBeTruthy();
  });

  it('does NOT render the encryption banner in pu-mode', () => {
    const adapter = new VotingAdapter();
    const { queryByText } = render(StarRatificationBallot, {
      props: {
        detail: seBaseExport({ privacyMode: 'pu' }),
        adapter,
        onCast: () => {},
      },
    });
    expect(queryByText(/encrypted to the community committee/i)).toBeNull();
  });
});

describe('Tier3ProposalPanel — awaiting-tally state', () => {
  it('shows committee progress banner when at least one tally share has arrived', async () => {
    // CodeRabbit PR #155: gating switched from client-clock `Date.now() >=
    // pollCreateHlcMs + window_seconds*1000` (skew-vulnerable) to
    // backend-derived `encryptedTallyShareCount > 0`. A committee member
    // publishing a kd=ts share is itself proof the engine considers
    // ratification closed — no client clock required.
    const detail = seBaseExport({
      stage: 'ra',
      winnerEventHash: null,
      encryptedTallyShareCount: 1,
      encryptedTallyThreshold: 2,
      encryptedTallyCommitteeSize: 3,
    });
    const summary: Tier3PollSummary = {
      pollId: detail.pollId,
      communityId: detail.communityId,
      proposalText: detail.proposalText,
      proposer: detail.proposer,
      stage: 'ra',
      pollCreateHlcMs: detail.pollCreateHlcMs,
      sortitionSize: detail.sortitionSize,
      winnerText: null,
      privacyMode: 'se',
    };
    const adapter = createPanelAdapterMock([summary], detail);
    const { findByText, getByText } = render(Tier3ProposalPanel, {
      props: { communityId: detail.communityId, adapter, myAddr: 'zz'.repeat(32) },
    });
    // Click the list-row to expand into the detail pane.
    const row = await findByText(detail.proposalText);
    row.click();
    // Awaiting-tally banner appears once the detail loads. The literal
    // "1 / 2 of 3" pattern is load-bearing — it's the user-facing k/t-of-n
    // progress indicator.
    await new Promise((r) => setTimeout(r, 0));
    expect(
      await findByText(/1 \/ 2[\s\S]*of 3 committee members/i),
    ).toBeTruthy();
    expect(getByText(/Awaiting committee tally/i)).toBeTruthy();
  });

  it('does NOT show awaiting-tally banner when zero shares have arrived (clock-skew safe)', async () => {
    // CodeRabbit PR #155 regression test: even if the device clock is
    // ahead and the client thinks the window has closed, no shares ⇒ keep
    // the ballot UI up. The ballot component itself is rendered by the
    // `:else` branch — assert that the awaiting-tally copy is absent.
    const detail = seBaseExport({
      stage: 'ra',
      winnerEventHash: null,
      // Backend hasn't seen any committee shares yet — no clock-derived
      // signal can override this. Note pollCreateHlcMs is in the deep
      // past (1.7e12 ms ≈ 2023) so `Date.now() >= windowEnd` would have
      // returned true under the old gating; the new gating ignores it.
      encryptedTallyShareCount: 0,
      encryptedTallyThreshold: 2,
      encryptedTallyCommitteeSize: 3,
    });
    const summary: Tier3PollSummary = {
      pollId: detail.pollId,
      communityId: detail.communityId,
      proposalText: detail.proposalText,
      proposer: detail.proposer,
      stage: 'ra',
      pollCreateHlcMs: detail.pollCreateHlcMs,
      sortitionSize: detail.sortitionSize,
      winnerText: null,
      privacyMode: 'se',
    };
    const adapter = createPanelAdapterMock([summary], detail);
    const { findByText, queryByText } = render(Tier3ProposalPanel, {
      props: { communityId: detail.communityId, adapter, myAddr: 'zz'.repeat(32) },
    });
    const row = await findByText(detail.proposalText);
    row.click();
    await new Promise((r) => setTimeout(r, 0));
    // Wait for the detail pane to finish rendering, then assert the
    // banner is NOT present. We don't try to assert the ballot IS
    // present (that's StarRatificationBallot's test surface); we just
    // check the awaiting-tally copy is absent.
    await findByText(detail.proposalText, { selector: 'h4' });
    expect(queryByText(/Awaiting committee tally/i)).toBeNull();
  });
});

describe('Tier3ProposalPanel — privacy chip in poll list', () => {
  it('renders the 🔒 privacy chip for se-mode summaries', async () => {
    const summary: Tier3PollSummary = {
      pollId: 'aa'.repeat(32),
      communityId: '11'.repeat(16),
      proposalText: 'Ballot-secret poll',
      proposer: '22'.repeat(32),
      stage: 'ra',
      pollCreateHlcMs: 1_700_000_000_000,
      sortitionSize: 100,
      winnerText: null,
      privacyMode: 'se',
    };
    const adapter = createPanelAdapterMock([summary]);
    const { findByLabelText } = render(Tier3ProposalPanel, {
      props: { communityId: '11'.repeat(16), adapter, myAddr: 'zz'.repeat(32) },
    });
    // The chip uses aria-label="ballot-secret poll" — lookup by label so
    // the lock-glyph itself isn't load-bearing on the assertion.
    const chip = await findByLabelText(/ballot-secret poll/i);
    expect(chip).toBeTruthy();
    expect(chip.textContent).toContain('🔒');
  });

  it('does NOT render the privacy chip for pu-mode summaries', async () => {
    const summary: Tier3PollSummary = {
      pollId: 'aa'.repeat(32),
      communityId: '11'.repeat(16),
      proposalText: 'Public poll',
      proposer: '22'.repeat(32),
      stage: 'ra',
      pollCreateHlcMs: 1_700_000_000_000,
      sortitionSize: 100,
      winnerText: null,
      privacyMode: 'pu',
    };
    const adapter = createPanelAdapterMock([summary]);
    const { findByText, queryByLabelText } = render(Tier3ProposalPanel, {
      props: { communityId: '11'.repeat(16), adapter, myAddr: 'zz'.repeat(32) },
    });
    await findByText('Public poll');
    expect(queryByLabelText(/ballot-secret poll/i)).toBeNull();
  });
});
