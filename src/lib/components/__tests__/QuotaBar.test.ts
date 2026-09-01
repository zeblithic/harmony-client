import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import QuotaBar from '../QuotaBar.svelte';

// ZEB-612 S3: no overall storage quota exists — the bar shows real used
// bytes with no invented denominator, plus the real pinned budget meter.
describe('QuotaBar', () => {
  const props = (over: Record<string, unknown> = {}) => ({
    usedBytes: 5_000_000_000,
    pinnedUsedBytes: 10_000_000,
    pinnedBudgetBytes: 50_000_000 as number | null,
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

  it('shows warning color when pinned usage exceeds 85% of the budget', () => {
    const { container } = render(
      QuotaBar,
      { props: props({ pinnedUsedBytes: 45_000_000 }) },
    );
    expect(container.querySelector('.quota-fill.warning')).toBeTruthy();
  });

  it('treats a known zero budget with usage as fully over budget, not 0%', () => {
    // pinnedBudgetBytes === 0 is a valid state distinct from null (unknown);
    // rendering it as a healthy empty bar would hide an over-quota state.
    const { container } = render(
      QuotaBar,
      { props: props({ pinnedBudgetBytes: 0, pinnedUsedBytes: 1_000 }) },
    );
    const fill = container.querySelector('.quota-fill.warning') as HTMLElement;
    expect(fill).toBeTruthy();
    expect(fill.style.width).toBe('100%');
  });
});
