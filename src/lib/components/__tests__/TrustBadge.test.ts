import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import TrustBadge from '../TrustBadge.svelte';
import { buildScore } from '../../trust-score';
import { COMMONS_FALLBACK } from '../../theme-colors';

// jsdom normalizes the hex fill to rgb() on element.style; derive the expected
// rgb from the sanctioned fallback table rather than re-typing color literals.
function hexToRgb(hex: string): string {
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  return `rgb(${r}, ${g}, ${b})`;
}

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
    expect(badge.style.background).toContain(hexToRgb(COMMONS_FALLBACK['--text-muted']));
  });

  it('shows red for low trust', () => {
    render(TrustBadge, { props: { score: buildScore(0, 0, 0, 0) } });
    const badge = screen.getByRole('img');
    expect(badge.style.background).toContain(hexToRgb(COMMONS_FALLBACK['--danger']));
  });

  it('shows amber for cautious', () => {
    render(TrustBadge, { props: { score: buildScore(1, 1, 1, 1) } });
    const badge = screen.getByRole('img');
    expect(badge.style.background).toContain(hexToRgb(COMMONS_FALLBACK['--warning']));
  });

  it('shows green for trusted', () => {
    render(TrustBadge, { props: { score: buildScore(2, 2, 2, 2) } });
    const badge = screen.getByRole('img');
    expect(badge.style.background).toContain(hexToRgb(COMMONS_FALLBACK['--presence-online']));
  });

  it('shows info for highly trusted', () => {
    render(TrustBadge, { props: { score: buildScore(3, 3, 3, 3) } });
    const badge = screen.getByRole('img');
    expect(badge.style.background).toContain(hexToRgb(COMMONS_FALLBACK['--info']));
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
