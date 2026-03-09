import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import SensitivityBadge from '../SensitivityBadge.svelte';

describe('SensitivityBadge', () => {
  it('renders "Public" with globe icon for public sensitivity', () => {
    render(SensitivityBadge, { props: { sensitivity: 'public' } });
    expect(screen.getByText(/Public/)).toBeTruthy();
    // Globe emoji 🌐
    expect(screen.getByText(/\u{1F310}/u)).toBeTruthy();
  });

  it('renders lock icon for private sensitivity', () => {
    render(SensitivityBadge, { props: { sensitivity: 'private' } });
    expect(screen.getByText(/Private/)).toBeTruthy();
    // Lock emoji 🔒
    expect(screen.getByText(/\u{1F512}/u)).toBeTruthy();
  });

  it('applies correct class for each sensitivity level', () => {
    const levels = ['public', 'private', 'intimate', 'confidential'] as const;
    for (const level of levels) {
      const { container, unmount } = render(SensitivityBadge, {
        props: { sensitivity: level },
      });
      const badge = container.querySelector('.sensitivity-badge');
      expect(badge).toBeTruthy();
      expect(badge!.classList.contains(level)).toBe(true);
      unmount();
    }
  });

  it('renders "Intimate" with lock icon for intimate sensitivity', () => {
    render(SensitivityBadge, { props: { sensitivity: 'intimate' } });
    expect(screen.getByText(/Intimate/)).toBeTruthy();
    expect(screen.getByText(/\u{1F512}/u)).toBeTruthy();
  });

  it('renders "Confidential" with lock icon for confidential sensitivity', () => {
    render(SensitivityBadge, { props: { sensitivity: 'confidential' } });
    expect(screen.getByText(/Confidential/)).toBeTruthy();
    expect(screen.getByText(/\u{1F512}/u)).toBeTruthy();
  });
});
