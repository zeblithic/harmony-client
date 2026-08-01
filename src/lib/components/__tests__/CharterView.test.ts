import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import CharterView from '../CharterView.svelte';
import type { CommunityMember } from '../../types';
import type { VotingAdapter } from '../../voting-adapter';
import type { Tier3PollSummary } from '../../types/voting';

const alice: CommunityMember = { address: 'aa'.repeat(20), displayName: 'Alice', power: 100, status: 'joined' };
const bob: CommunityMember = { address: 'bb'.repeat(20), displayName: 'Bob', power: 0, status: 'joined' };
const carol: CommunityMember = { address: 'cc'.repeat(20), displayName: 'Carol', power: 100, status: 'joined' };
const daveLeft: CommunityMember = { address: 'dd'.repeat(20), displayName: 'Dave', power: 0, status: 'left' };

function poll(overrides: Partial<Tier3PollSummary> = {}): Tier3PollSummary {
  return {
    pollId: 'p1',
    communityId: 'cid',
    proposalText: 'Adopt a code of conduct',
    proposer: 'ee'.repeat(20),
    stage: 'fi',
    pollCreateHlcMs: 1735689600000, // 2025-01-01T00:00:00Z
    sortitionSize: 5,
    winnerText: 'Adopted with amendments',
    privacyMode: 'pu',
    ...overrides,
  };
}

function makeAdapter(polls: Tier3PollSummary[] | Error): VotingAdapter {
  return {
    listTier3Polls: vi.fn(() =>
      polls instanceof Error ? Promise.reject(polls) : Promise.resolve(polls),
    ),
  } as unknown as VotingAdapter;
}

const baseProps = {
  communityId: 'cid',
  communityName: 'IPFS Crew',
  members: [alice, bob, carol, daveLeft],
  adminQuorum: 2,
  onProposeAmendment: vi.fn(),
};

describe('CharterView', () => {
  it('derives the plural amendment-count pill and joined-members-bound line', async () => {
    const adapter = makeAdapter([
      poll({ pollId: 'p1' }),
      poll({ pollId: 'p2', pollCreateHlcMs: 1738368000000 }), // 2025-02-01
      poll({ pollId: 'p3', stage: 'de' }), // in deliberation — NOT ratified
    ]);
    const { getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(getByText('✓ 2 ratified amendments')).toBeTruthy();
    });
    // daveLeft has status 'left' — only joined members are bound.
    expect(getByText('3 members bound')).toBeTruthy();
  });

  it('uses the singular form for exactly one amendment', async () => {
    const adapter = makeAdapter([poll()]);
    const { getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(getByText('✓ 1 ratified amendment')).toBeTruthy();
    });
  });

  it('zero-state shows "No amendments yet" and no on-record section', async () => {
    const adapter = makeAdapter([]);
    const { getByText, container } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(getByText('✓ No amendments yet')).toBeTruthy();
    });
    expect(container.querySelector('.on-record')).toBeNull();
  });

  it('renders all three articles gracefully when the poll fetch rejects', async () => {
    const adapter = makeAdapter(new Error('adapter not connected'));
    const { container, getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect((adapter.listTier3Polls as ReturnType<typeof vi.fn>).mock.calls.length).toBe(1);
    });
    expect(getByText('✓ …')).toBeTruthy(); // neutral not-loaded pill, never a fake zero
    expect(getByText(/Article I · Membership/)).toBeTruthy();
    expect(getByText(/Article II · How we decide/)).toBeTruthy();
    expect(getByText(/Article III · Amendment/)).toBeTruthy();
    expect(container.querySelector('.on-record')).toBeNull();
  });

  it('renders the generated preamble framing', async () => {
    const adapter = makeAdapter([]);
    const { getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    expect(getByText(/generated from its live governance state/)).toBeTruthy();
  });

  it('capability matrix has the 6 derived rows, an admin-only bottom row, and the v1 footnote', async () => {
    const adapter = makeAdapter([]);
    const { container } = render(CharterView, { props: { ...baseProps, adapter } });
    const rows = container.querySelectorAll('.capability-matrix tbody tr');
    expect(rows.length).toBe(6);
    // Invite is MEMBER-level (backend gate = POWER_THRESHOLDS.invite = 0) —
    // regression pin for the T3-review mis-tiering fix.
    const first = rows[0];
    expect(first.textContent).toContain('invite');
    expect(first.querySelectorAll('.cap')[0].textContent).toBe('●');
    const last = rows[5];
    expect(last.textContent).toContain('Set roles · change decision rules');
    const caps = last.querySelectorAll('.cap');
    expect(caps[0].textContent).toBe('—');
    expect(caps[1].textContent).toBe('—');
    expect(caps[2].textContent).toBe('●');
    // Honesty footnote — thresholds are GLOBAL v1 constants (spec §0.1).
    expect(container.textContent).toContain('Thresholds are platform-wide in v1.');
  });

  it('role cards show the real POWER_THRESHOLDS values', async () => {
    const adapter = makeAdapter([]);
    const { getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    expect(getByText('power 0')).toBeTruthy();
    expect(getByText('power ≥ 50')).toBeTruthy();
    expect(getByText('power ≥ 100')).toBeTruthy();
  });

  it('admin quorum card shows k of n from real data with a matching pip meter', async () => {
    const adapter = makeAdapter([]);
    const { container, getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    // n = joined members with power >= 100 (alice, carol); k = adminQuorum prop.
    expect(getByText('2 of 2')).toBeTruthy();
    expect(container.querySelectorAll('.quorum-card .pip').length).toBe(2);
    expect(container.querySelectorAll('.quorum-card .pip.filled').length).toBe(2);
    expect(getByText(/No single admin can act alone/)).toBeTruthy();
  });

  it('a quorum of 1 does NOT claim co-signing is required (PR #410 CodeRabbit)', async () => {
    const adapter = makeAdapter([]);
    const { getByText, queryByText } = render(CharterView, {
      props: { ...baseProps, adapter, adminQuorum: 1 },
    });
    expect(getByText(/Any single admin can enact admin actions on their own/)).toBeTruthy();
    expect(queryByText(/No single admin can act alone/)).toBeNull();
  });

  it('an unresolved quorum shows a neutral placeholder, never a fake value (PR #410 CodeRabbit)', async () => {
    const adapter = makeAdapter([]);
    const { container, queryByText } = render(CharterView, {
      props: { ...baseProps, adapter, adminQuorum: null },
    });
    const card = container.querySelector('.quorum-card')!;
    expect(card.querySelector('.quorum-value')?.textContent).toBe('…');
    // No meter and no false "must co-sign / can act alone" copy while unknown.
    expect(card.querySelectorAll('.pip').length).toBe(0);
    expect(queryByText(/No single admin can act alone/)).toBeNull();
    expect(queryByText(/must co-sign admin actions/)).toBeNull();
  });

  it('capability cells expose Can/Cannot to assistive tech (PR #410 CodeRabbit a11y)', async () => {
    const adapter = makeAdapter([]);
    const { container } = render(CharterView, { props: { ...baseProps, adapter } });
    const firstRow = container.querySelectorAll('.capability-matrix tbody tr')[0];
    const memberCell = firstRow.querySelectorAll('.cap')[0];
    // Member CAN post/vote/propose/invite (row 0).
    expect(memberCell.getAttribute('aria-label')).toBe('Can');
    // The glyph itself is hidden from the accessibility tree.
    expect(memberCell.querySelector('[aria-hidden="true"]')?.textContent).toBe('●');
    const lastRow = container.querySelectorAll('.capability-matrix tbody tr')[5];
    expect(lastRow.querySelectorAll('.cap')[0].getAttribute('aria-label')).toBe('Cannot');
  });

  it('a finalized poll with no winner text is not counted as a ratified amendment (PR #410 CodeRabbit)', async () => {
    // Defensive: winnerText is nullable at the DTO boundary; a null-winner
    // finalized poll must not inflate the pill (the record already renders no
    // "Ratified:" row for it).
    const adapter = makeAdapter([
      poll({ pollId: 'real', proposalText: 'Rename #general', winnerText: 'Renamed to #square' }),
      poll({ pollId: 'nowinner', proposalText: 'Ghost poll', winnerText: null }),
    ]);
    const { getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(getByText('✓ 1 ratified amendment')).toBeTruthy();
    });
  });

  it('Propose amendment fires the callback', async () => {
    const onProposeAmendment = vi.fn();
    const adapter = makeAdapter([]);
    const { getByRole } = render(CharterView, {
      props: { ...baseProps, adapter, onProposeAmendment },
    });
    await fireEvent.click(getByRole('button', { name: 'Propose amendment' }));
    expect(onProposeAmendment).toHaveBeenCalledTimes(1);
  });

  it('on-record rows render proposed-date, title, ratified outcome, and short proposer', async () => {
    const adapter = makeAdapter([poll()]);
    const { container, getByText } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(container.querySelector('.on-record')).toBeTruthy();
    });
    expect(getByText(/2025-01-01 · proposed/)).toBeTruthy();
    expect(getByText('Adopt a code of conduct')).toBeTruthy();
    expect(getByText('Ratified: Adopted with amendments')).toBeTruthy();
    expect(getByText('eeeeeeee…eeee')).toBeTruthy(); // shortAddr 8…4
  });

  it('a status-quo win is NOT counted as a ratified amendment and never leaks the sentinel', async () => {
    // A finalized poll whose runoff upheld the status quo: backend returns
    // winnerText === '<status quo>'. It must not inflate the amendment count
    // and must render as "Upheld: status quo", never "Ratified: <status quo>"
    // (final-review I-1 — finalized ≠ adopted).
    const adapter = makeAdapter([
      poll({ pollId: 'adopted', proposalText: 'Rename #general', winnerText: 'Renamed to #square' }),
      poll({ pollId: 'upheld', proposalText: 'Abolish moderation', winnerText: '<status quo>' }),
    ]);
    const { getByText, queryByText, container } = render(CharterView, {
      props: { ...baseProps, adapter },
    });
    await waitFor(() => {
      expect(container.querySelector('.on-record')).toBeTruthy();
    });
    // Only the adopted poll counts toward the pill.
    expect(getByText('✓ 1 ratified amendment')).toBeTruthy();
    // The upheld deliberation still appears in the record, honestly labeled.
    expect(getByText('Abolish moderation')).toBeTruthy();
    expect(getByText('Upheld: status quo')).toBeTruthy();
    // The raw backend sentinel must never surface as a ratified outcome.
    expect(queryByText('Ratified: <status quo>')).toBeNull();
    expect(container.textContent).not.toContain('Ratified: <status quo>');
  });

  it('orders same-ms finalized polls by logical then deviceId (ZEB-790)', async () => {
    const a = poll({
      pollId: 'p-a', proposalText: 'second-by-logical',
      pollCreateHlcMs: 1_700_000_020_000, pollCreateHlcLogical: 2, pollCreateHlcDeviceId: 'dev-a',
    });
    const b = poll({
      pollId: 'p-b', proposalText: 'first-by-logical',
      pollCreateHlcMs: 1_700_000_020_000, pollCreateHlcLogical: 1, pollCreateHlcDeviceId: 'dev-z',
    });
    const adapter = makeAdapter([a, b]);
    const { container } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(container.querySelector('.on-record')).toBeTruthy();
    });
    const text = container.textContent ?? '';
    expect(text.indexOf('first-by-logical')).toBeGreaterThan(-1);
    expect(text.indexOf('first-by-logical')).toBeLessThan(text.indexOf('second-by-logical'));
  });

  it('a community whose only finalized poll upheld the status quo shows no ratified amendments', async () => {
    const adapter = makeAdapter([
      poll({ pollId: 'upheld-only', proposalText: 'Adopt term limits', winnerText: '<status quo>' }),
    ]);
    const { getByText, container } = render(CharterView, { props: { ...baseProps, adapter } });
    await waitFor(() => {
      expect(container.querySelector('.on-record')).toBeTruthy();
    });
    expect(getByText('✓ No amendments yet')).toBeTruthy();
    expect(getByText('Upheld: status quo')).toBeTruthy();
  });
});
