import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import QuotaBar from '../QuotaBar.svelte';

// ZEB-612 S3: no overall storage quota exists — the bar shows real used
// bytes with no invented denominator, plus the real pinned budget meter.
describe('QuotaBar', () => {
  const props = (over: Record<string, unknown> = {}) => ({
    usedBytes: 5_000_000_000,
    pinnedUsedBytes: 10_000_000,
    pinnedBudgetBytes: 50_000_000 as number | null,
    onCleanupClick: vi.fn(),
    ...over,
  });

  it('shows used bytes with no invented total', () => {
    render(QuotaBar, { props: props() });
    expect(screen.getByText(/5\.0 GB stored locally/)).toBeTruthy();
    expect(screen.queryByText(/10\.0 GB/)).toBeNull();
  });

  it('renders the pinned meter when the budget is known', () => {
    render(QuotaBar, { props: props() });
    expect(screen.getByText(/Pinned 10\.0 MB of 50\.0 MB/)).toBeTruthy();
  });

  it('hides the pinned meter when the budget is unknown (demo / IPC failure)', () => {
    const { container } = render(QuotaBar, { props: props({ pinnedBudgetBytes: null }) });
    expect(screen.queryByText(/Pinned/)).toBeNull();
    expect(container.querySelector('.quota-track')).toBeNull();
  });

  it('calls onCleanupClick when clicked', async () => {
    const onClick = vi.fn();
    render(QuotaBar, { props: props({ onCleanupClick: onClick }) });
    await fireEvent.click(screen.getByRole('button'));
    expect(onClick).toHaveBeenCalledOnce();
  });

  it('shows warning color when pinned usage exceeds 85% of the budget', () => {
    const { container } = render(
      QuotaBar,
      { props: props({ pinnedUsedBytes: 45_000_000 }) },
    );
    expect(container.querySelector('.quota-fill.warning')).toBeTruthy();
  });
});
