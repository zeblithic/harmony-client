import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import NetworkStatusBar from '../NetworkStatusBar.svelte';

describe('NetworkStatusBar', () => {
  it('displays node count', () => {
    render(NetworkStatusBar, { props: { nodeCount: 12, linkCount: 18, healthySummary: '10 healthy, 2 degraded' } });
    expect(screen.getByText(/12 nodes/)).toBeTruthy();
  });

  it('displays link count', () => {
    render(NetworkStatusBar, { props: { nodeCount: 12, linkCount: 18, healthySummary: '' } });
    expect(screen.getByText(/18 links/)).toBeTruthy();
  });

  it('displays health summary', () => {
    render(NetworkStatusBar, { props: { nodeCount: 5, linkCount: 8, healthySummary: '4 healthy, 1 offline' } });
    expect(screen.getByText(/4 healthy, 1 offline/)).toBeTruthy();
  });
});
