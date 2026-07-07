import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import NetworkStatusPill from '../NetworkStatusPill.svelte';

describe('NetworkStatusPill', () => {
  it('renders the label text', () => {
    render(NetworkStatusPill, { props: { variant: 'healthy', label: 'Healthy' } });
    expect(screen.getByText('Healthy')).toBeTruthy();
  });

  it('applies the variant class', () => {
    const { container } = render(NetworkStatusPill, {
      props: { variant: 'cooling', label: 'Cooling down (5s)' },
    });
    const pill = container.querySelector('.net-pill');
    expect(pill).toBeTruthy();
    expect(pill!.classList.contains('cooling')).toBe(true);
  });

  it('forwards data-testid, role and title to the span', () => {
    render(NetworkStatusPill, {
      props: {
        variant: 'incompat',
        label: '⚠ incompatible',
        'data-testid': 'nh-peer-incompat',
        role: 'alert',
        title: 'protocol mismatch',
      },
    });
    const pill = screen.getByTestId('nh-peer-incompat');
    expect(pill.getAttribute('role')).toBe('alert');
    expect(pill.getAttribute('title')).toBe('protocol mismatch');
    expect(pill.textContent).toContain('⚠ incompatible');
    expect(pill.classList.contains('incompat')).toBe(true);
  });
});
