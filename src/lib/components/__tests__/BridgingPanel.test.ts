import { render } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import BridgingPanel from '../BridgingPanel.svelte';
import type { BridgingScoreExport } from '../../types/voting';

const score1: BridgingScoreExport = {
  statementEventHash: 'aa'.repeat(32),
  statementText: 'Top bridging',
  author: '33'.repeat(32),
  agreeCount: 10,
  disagreeCount: 2,
  passCount: 1,
  diversityQ32: '2147483648', // ≈ 0.5
  bridgingScoreQ64: '21474836480',
};

const score2: BridgingScoreExport = {
  statementEventHash: 'bb'.repeat(32),
  statementText: 'Second bridging',
  author: '44'.repeat(32),
  agreeCount: 8,
  disagreeCount: 3,
  passCount: 1,
  diversityQ32: '1073741824',
  bridgingScoreQ64: '8589934592',
};

describe('BridgingPanel', () => {
  it('renders empty-state copy when scores is empty', () => {
    const { getByText } = render(BridgingPanel, { props: { scores: [], error: null } });
    expect(getByText(/Bridging scores will appear once/i)).toBeTruthy();
  });

  it('renders top-N cards when scores present', () => {
    const { getByText } = render(BridgingPanel, { props: { scores: [score1, score2], error: null } });
    expect(getByText('Top bridging')).toBeTruthy();
    expect(getByText('Second bridging')).toBeTruthy();
  });

  it('renders error string when error prop set', () => {
    const { getByText } = render(BridgingPanel, { props: { scores: [], error: 'IPC failed' } });
    expect(getByText(/IPC failed/i)).toBeTruthy();
  });
});
