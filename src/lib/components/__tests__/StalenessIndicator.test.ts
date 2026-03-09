import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import StalenessIndicator from '../StalenessIndicator.svelte';

describe('StalenessIndicator', () => {
  it('renders nothing for score below 0.3', () => {
    const { container } = render(StalenessIndicator, { props: { score: 0.1 } });
    expect(container.querySelector('.staleness-dot')).toBeNull();
  });

  it('renders yellow dot for score between 0.3 and 0.6', () => {
    const { container } = render(StalenessIndicator, { props: { score: 0.45 } });
    const dot = container.querySelector('.staleness-dot');
    expect(dot).toBeTruthy();
    expect(dot!.classList.contains('yellow')).toBe(true);
    expect(screen.getByLabelText(/slightly stale/i)).toBeTruthy();
  });

  it('renders orange dot for score between 0.6 and 0.85', () => {
    const { container } = render(StalenessIndicator, { props: { score: 0.7 } });
    const dot = container.querySelector('.staleness-dot');
    expect(dot).toBeTruthy();
    expect(dot!.classList.contains('orange')).toBe(true);
    expect(screen.getByLabelText(/moderately stale/i)).toBeTruthy();
  });

  it('renders red dot for score at or above 0.85', () => {
    const { container } = render(StalenessIndicator, { props: { score: 0.9 } });
    const dot = container.querySelector('.staleness-dot');
    expect(dot).toBeTruthy();
    expect(dot!.classList.contains('red')).toBe(true);
    expect(screen.getByLabelText(/very stale/i)).toBeTruthy();
  });

  it('renders nothing when pinned regardless of score', () => {
    const { container } = render(StalenessIndicator, { props: { score: 0.95, pinned: true } });
    expect(container.querySelector('.staleness-dot')).toBeNull();
  });
});
