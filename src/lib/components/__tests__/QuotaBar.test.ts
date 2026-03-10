import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import QuotaBar from '../QuotaBar.svelte';

describe('QuotaBar', () => {
  it('renders usage text', () => {
    render(QuotaBar, { props: { usedBytes: 5_000_000_000, totalBytes: 10_000_000_000, onCleanupClick: vi.fn() } });
    expect(screen.getByText(/5\.0 GB/)).toBeTruthy();
    expect(screen.getByText(/10\.0 GB/)).toBeTruthy();
  });

  it('has descriptive aria-label on button', () => {
    render(QuotaBar, { props: { usedBytes: 3_000_000_000, totalBytes: 10_000_000_000, onCleanupClick: vi.fn() } });
    const btn = screen.getByRole('button');
    expect(btn.getAttribute('aria-label')).toContain('3.0 GB');
    expect(btn.getAttribute('aria-label')).toContain('10.0 GB');
    expect(btn.getAttribute('aria-label')).toContain('30%');
  });

  it('calls onCleanupClick when clicked', async () => {
    const onClick = vi.fn();
    render(QuotaBar, { props: { usedBytes: 5_000_000_000, totalBytes: 10_000_000_000, onCleanupClick: onClick } });
    await fireEvent.click(screen.getByRole('button'));
    expect(onClick).toHaveBeenCalledOnce();
  });

  it('shows warning color when usage exceeds 85%', () => {
    const { container } = render(QuotaBar, { props: { usedBytes: 9_000_000_000, totalBytes: 10_000_000_000, onCleanupClick: vi.fn() } });
    expect(container.querySelector('.quota-fill.warning')).toBeTruthy();
  });
});
