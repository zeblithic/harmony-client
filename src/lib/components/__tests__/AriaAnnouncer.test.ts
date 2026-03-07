import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import AriaAnnouncer from '../AriaAnnouncer.svelte';

describe('AriaAnnouncer', () => {
  it('renders with role="status"', () => {
    render(AriaAnnouncer, { props: { message: 'Node online' } });
    const el = screen.getByRole('status');
    expect(el).toBeTruthy();
  });

  it('displays the message text', () => {
    render(AriaAnnouncer, { props: { message: 'Node bravo went offline' } });
    const el = screen.getByRole('status');
    expect(el.textContent).toBe('Node bravo went offline');
  });

  it('has aria-live="polite"', () => {
    render(AriaAnnouncer, { props: { message: 'test' } });
    const el = screen.getByRole('status');
    expect(el.getAttribute('aria-live')).toBe('polite');
  });

  it('has aria-atomic="true"', () => {
    render(AriaAnnouncer, { props: { message: 'test' } });
    const el = screen.getByRole('status');
    expect(el.getAttribute('aria-atomic')).toBe('true');
  });

  it('is visually hidden', () => {
    render(AriaAnnouncer, { props: { message: 'test' } });
    const el = screen.getByRole('status');
    expect(el.classList.contains('sr-only')).toBe(true);
  });
});
