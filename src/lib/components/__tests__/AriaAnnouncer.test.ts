import { render, screen } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import AriaAnnouncer from '../AriaAnnouncer.svelte';

describe('AriaAnnouncer', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('renders with role="status"', () => {
    render(AriaAnnouncer, { props: { message: 'Node online' } });
    const el = screen.getByRole('status');
    expect(el).toBeTruthy();
  });

  it('announces first message immediately (leading-edge throttle)', async () => {
    render(AriaAnnouncer, { props: { message: 'Node bravo went offline' } });
    const el = screen.getByRole('status');
    await vi.advanceTimersByTimeAsync(0);
    expect(el.textContent?.trim()).toBe('Node bravo went offline');
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
