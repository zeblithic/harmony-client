import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import TrustOverview from '../TrustOverview.svelte';
import { buildScore } from '../../trust-score';
import type { TrustEdge } from '../../trust-score';

function makeEdges(): TrustEdge[] {
  return [
    { source: 'local', target: 'peer-a', score: buildScore(3, 2, 1, 0), timestamp: Date.now() },
    { source: 'local', target: 'peer-b', score: buildScore(0, 1, 2, 3), timestamp: Date.now() },
    { source: 'peer-a', target: 'local', score: buildScore(2, 2, 2, 2), timestamp: Date.now() },
  ];
}

interface PeerInfo {
  address: string;
  displayName: string;
}

const peers: PeerInfo[] = [
  { address: 'peer-a', displayName: 'Alpha' },
  { address: 'peer-b', displayName: 'Bravo' },
];

describe('TrustOverview', () => {
  it('renders a grid element', () => {
    render(TrustOverview, {
      props: { localAddress: 'local', peers, edges: makeEdges() },
    });
    expect(screen.getByRole('grid')).toBeTruthy();
  });

  it('renders a row per peer', () => {
    render(TrustOverview, {
      props: { localAddress: 'local', peers, edges: makeEdges() },
    });
    const rows = screen.getAllByRole('row');
    // header + 2 data rows
    expect(rows.length).toBe(3);
  });

  it('displays peer names', () => {
    render(TrustOverview, {
      props: { localAddress: 'local', peers, edges: makeEdges() },
    });
    expect(screen.getByText('Alpha')).toBeTruthy();
    expect(screen.getByText('Bravo')).toBeTruthy();
  });

  it('shows my score for each peer', () => {
    render(TrustOverview, {
      props: { localAddress: 'local', peers, edges: makeEdges() },
    });
    // Check that scores appear as hex
    const hexValues = screen.getAllByText(/0x[0-9a-f]{2}/i);
    expect(hexValues.length).toBeGreaterThan(0);
  });

  it('shows their score for me when available', () => {
    render(TrustOverview, {
      props: { localAddress: 'local', peers, edges: makeEdges() },
    });
    // peer-a scored local, peer-b did not
    // Check that at least one em-dash exists (peer-b has no score for us)
    const rows = screen.getAllByRole('row');
    const bravoRow = rows.find((r) => r.textContent?.includes('Bravo'));
    expect(bravoRow?.textContent).toContain('\u2014');
  });

  it('has sortable column headers with aria-sort', () => {
    render(TrustOverview, {
      props: { localAddress: 'local', peers, edges: makeEdges() },
    });
    const headers = screen.getAllByRole('columnheader');
    expect(headers.length).toBeGreaterThan(0);
    const sorted = headers.find(
      (h) => h.getAttribute('aria-sort') === 'ascending',
    );
    expect(sorted).toBeTruthy();
  });

  it('sorts by my score when header is clicked', async () => {
    render(TrustOverview, {
      props: { localAddress: 'local', peers, edges: makeEdges() },
    });
    const btn = screen.getByRole('button', { name: /sort by my score/i });
    await fireEvent.click(btn);
    const rows = screen.getAllByRole('row');
    // After sorting ascending by my score, Alpha (score 27) comes before Bravo (score 228)
    expect(rows[1].textContent).toContain('Alpha');
  });
});
