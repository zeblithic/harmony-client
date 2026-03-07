import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import TrustBadge from '../TrustBadge.svelte';
import { buildScore } from '../../trust-score';

describe('TrustBadge', () => {
  it('renders a span element', () => {
    render(TrustBadge, { props: { score: buildScore(2, 2, 2, 2) } });
    const badge = screen.getByRole('img');
    expect(badge).toBeTruthy();
  });

  // jsdom normalizes hex colors to rgb() when setting element.style
  it('shows gray for unscored (null)', () => {
    render(TrustBadge, { props: { score: null } });
    const badge = screen.getByRole('img');
    expect(badge.style.background).toContain('rgb(114, 118, 125)');
  });

  it('shows red for low trust', () => {
    render(TrustBadge, { props: { score: buildScore(0, 0, 0, 0) } });
    const badge = screen.getByRole('img');
    expect(badge.style.background).toContain('rgb(237, 66, 69)');
  });

  it('shows amber for cautious', () => {
    render(TrustBadge, { props: { score: buildScore(1, 1, 1, 1) } });
    const badge = screen.getByRole('img');
    expect(badge.style.background).toContain('rgb(250, 166, 26)');
  });

  it('shows green for trusted', () => {
    render(TrustBadge, { props: { score: buildScore(2, 2, 2, 2) } });
    const badge = screen.getByRole('img');
    expect(badge.style.background).toContain('rgb(67, 181, 129)');
  });

  it('shows accent blue for highly trusted', () => {
    render(TrustBadge, { props: { score: buildScore(3, 3, 3, 3) } });
    const badge = screen.getByRole('img');
    expect(badge.style.background).toContain('rgb(88, 101, 242)');
  });

  it('has accessible aria-label', () => {
    render(TrustBadge, { props: { score: buildScore(3, 3, 3, 3) } });
    const badge = screen.getByRole('img');
    expect(badge.getAttribute('aria-label')).toBe('highly trusted');
  });

  it('aria-label says unscored for null', () => {
    render(TrustBadge, { props: { score: null } });
    const badge = screen.getByRole('img');
    expect(badge.getAttribute('aria-label')).toBe('unscored');
  });
});
