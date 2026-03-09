import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import CleanupView from '../CleanupView.svelte';
import type { CleanupRecommendation, QuotaStatus } from '../../types';

const quota: QuotaStatus = {
  usedBytes: 1_879_867_048,
  totalBytes: 10_000_000_000,
  byCategory: {
    bundle: 0,
    text: 47_048,
    image: 8_820_000,
    music: 35_000_000,
    video: 1_500_000_000,
    dataset: 250_000_000,
    software: 85_000_000,
  },
};

const recommendations: CleanupRecommendation[] = [
  {
    cid: 'cid-training-data',
    name: 'sensor-readings.parquet',
    category: 'dataset',
    sizeBytes: 250_000_000,
    reason: 'stale',
    stalenessScore: 0.85,
    spaceRecoverable: 250_000_000,
    confidence: 0.92,
  },
  {
    cid: 'cid-video-lecture',
    name: 'distributed-systems-lecture.mp4',
    category: 'video',
    sizeBytes: 1_500_000_000,
    reason: 'stale',
    stalenessScore: 0.7,
    spaceRecoverable: 1_500_000_000,
    confidence: 0.78,
  },
  {
    cid: 'cid-family-photo',
    name: 'family-reunion-2025.jpg',
    category: 'image',
    sizeBytes: 8_500_000,
    reason: 'over-replicated',
    stalenessScore: 0.55,
    spaceRecoverable: 8_500_000,
    confidence: 0.65,
  },
];

function baseProps() {
  return {
    quota,
    recommendations,
    onAction: vi.fn(),
    onBulkBurn: vi.fn(),
    onBulkArchive: vi.fn(),
    onBulkRelease: vi.fn(),
    onBulkPublish: vi.fn(),
  };
}

describe('CleanupView', () => {
  // ── QuotaSummary integration ────────────────────────────────────

  it('renders quota summary with used/total', () => {
    render(CleanupView, { props: baseProps() });
    expect(screen.getByText(/1\.9 GB/)).toBeTruthy();
    expect(screen.getByText(/10\.0 GB/)).toBeTruthy();
  });

  it('renders category breakdown bars', () => {
    const { container } = render(CleanupView, { props: baseProps() });
    const segments = container.querySelectorAll('.category-segment');
    // One per non-zero category: text, image, music, video, dataset, software = 6
    // bundle is 0 so excluded
    expect(segments.length).toBeGreaterThanOrEqual(5);
  });

  // ── Recommendation cards ────────────────────────────────────────

  it('renders all recommendation cards', () => {
    render(CleanupView, { props: baseProps() });
    expect(screen.getByText('sensor-readings.parquet')).toBeTruthy();
    expect(screen.getByText('distributed-systems-lecture.mp4')).toBeTruthy();
    expect(screen.getByText('family-reunion-2025.jpg')).toBeTruthy();
  });

  it('fires onAction when a card Burn button is clicked and confirmed', async () => {
    const onAction = vi.fn();
    render(CleanupView, { props: { ...baseProps(), onAction } });
    // Each recommendation card has a Burn button
    const burnButtons = screen.getAllByRole('button', { name: /burn/i });
    await fireEvent.click(burnButtons[0]);
    // Per-card burn now requires confirmation
    expect(onAction).not.toHaveBeenCalled();
    const dialog = screen.getByRole('dialog');
    const confirmBtn = dialog.querySelector('.confirm-btn') as HTMLElement;
    await fireEvent.click(confirmBtn);
    expect(onAction).toHaveBeenCalledWith('cid-training-data', 'burn');
  });

  // ── Select all ──────────────────────────────────────────────────

  it('has a select-all checkbox', () => {
    render(CleanupView, { props: baseProps() });
    const selectAll = screen.getByRole('checkbox', { name: /select all/i });
    expect(selectAll).toBeTruthy();
  });

  it('selects all items when select-all is clicked', async () => {
    const { container } = render(CleanupView, { props: baseProps() });
    const selectAll = screen.getByRole('checkbox', { name: /select all/i });
    await fireEvent.click(selectAll);
    const checkboxes = container.querySelectorAll('input[type="checkbox"]');
    // All should be checked (select-all + 3 cards = 4)
    for (const cb of checkboxes) {
      expect((cb as HTMLInputElement).checked).toBe(true);
    }
  });

  it('deselects all when select-all is unchecked', async () => {
    const { container } = render(CleanupView, { props: baseProps() });
    const selectAll = screen.getByRole('checkbox', { name: /select all/i });
    // Select all, then deselect
    await fireEvent.click(selectAll);
    await fireEvent.click(selectAll);
    const checkboxes = container.querySelectorAll('input[type="checkbox"]');
    for (const cb of checkboxes) {
      expect((cb as HTMLInputElement).checked).toBe(false);
    }
  });

  // ── Bulk action bar ─────────────────────────────────────────────

  it('does not show bulk action bar when nothing is selected', () => {
    render(CleanupView, { props: baseProps() });
    expect(screen.queryByText(/items selected/)).toBeNull();
  });

  it('shows bulk action bar when items are selected', async () => {
    const { container } = render(CleanupView, { props: baseProps() });
    // Click the first recommendation card's checkbox
    const checkboxes = container.querySelectorAll('input[type="checkbox"]');
    // checkboxes[0] is select-all, checkboxes[1] is first card
    await fireEvent.click(checkboxes[1]);
    expect(screen.getByText(/1 item selected/)).toBeTruthy();
  });

  it('shows correct recoverable space in bulk bar', async () => {
    const { container } = render(CleanupView, { props: baseProps() });
    // Select first recommendation (250 MB)
    const checkboxes = container.querySelectorAll('input[type="checkbox"]');
    await fireEvent.click(checkboxes[1]);
    // The bulk bar shows "1 item selected — 250.0 MB recoverable"
    expect(screen.getByText(/1 item selected.*250\.0 MB recoverable/)).toBeTruthy();
  });

  it('shows recoverable for multiple selections', async () => {
    const { container } = render(CleanupView, { props: baseProps() });
    const selectAll = screen.getByRole('checkbox', { name: /select all/i });
    await fireEvent.click(selectAll);
    // 250MB + 1500MB + 8.5MB = 1758.5MB = 1.8 GB
    expect(screen.getByText(/3 items selected/)).toBeTruthy();
    expect(screen.getByText(/1\.8 GB recoverable/)).toBeTruthy();
  });

  it('shows bulk action buttons when items are selected', async () => {
    const { container } = render(CleanupView, { props: baseProps() });
    const selectAll = screen.getByRole('checkbox', { name: /select all/i });
    await fireEvent.click(selectAll);
    expect(screen.getByRole('button', { name: /burn all/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /archive all/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /release all/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /publish all/i })).toBeTruthy();
  });

  it('fires onBulkBurn with selected CIDs after confirmation', async () => {
    const onBulkBurn = vi.fn();
    const { container } = render(CleanupView, { props: { ...baseProps(), onBulkBurn } });
    const selectAll = screen.getByRole('checkbox', { name: /select all/i });
    await fireEvent.click(selectAll);
    await fireEvent.click(screen.getByRole('button', { name: /burn all/i }));
    // Burn All now requires confirmation
    expect(onBulkBurn).not.toHaveBeenCalled();
    const dialog = screen.getByRole('dialog');
    const confirmBtn = dialog.querySelector('.confirm-btn') as HTMLElement;
    await fireEvent.click(confirmBtn);
    expect(onBulkBurn).toHaveBeenCalledWith([
      'cid-training-data',
      'cid-video-lecture',
      'cid-family-photo',
    ]);
  });

  it('fires onBulkArchive with selected CIDs after confirmation', async () => {
    const onBulkArchive = vi.fn();
    const { container } = render(CleanupView, { props: { ...baseProps(), onBulkArchive } });
    const selectAll = screen.getByRole('checkbox', { name: /select all/i });
    await fireEvent.click(selectAll);
    await fireEvent.click(screen.getByRole('button', { name: /archive all/i }));
    expect(onBulkArchive).not.toHaveBeenCalled();
    const dialog = screen.getByRole('dialog');
    const confirmBtn = dialog.querySelector('.confirm-btn') as HTMLElement;
    await fireEvent.click(confirmBtn);
    expect(onBulkArchive).toHaveBeenCalledWith([
      'cid-training-data',
      'cid-video-lecture',
      'cid-family-photo',
    ]);
  });

  it('fires onBulkRelease with selected CIDs after confirmation', async () => {
    const onBulkRelease = vi.fn();
    const { container } = render(CleanupView, { props: { ...baseProps(), onBulkRelease } });
    const selectAll = screen.getByRole('checkbox', { name: /select all/i });
    await fireEvent.click(selectAll);
    await fireEvent.click(screen.getByRole('button', { name: /release all/i }));
    expect(onBulkRelease).not.toHaveBeenCalled();
    const dialog = screen.getByRole('dialog');
    const confirmBtn = dialog.querySelector('.confirm-btn') as HTMLElement;
    await fireEvent.click(confirmBtn);
    expect(onBulkRelease).toHaveBeenCalledWith([
      'cid-training-data',
      'cid-video-lecture',
      'cid-family-photo',
    ]);
  });

  it('fires onBulkPublish with selected CIDs after confirmation', async () => {
    const onBulkPublish = vi.fn();
    const { container } = render(CleanupView, { props: { ...baseProps(), onBulkPublish } });
    const selectAll = screen.getByRole('checkbox', { name: /select all/i });
    await fireEvent.click(selectAll);
    await fireEvent.click(screen.getByRole('button', { name: /publish all/i }));
    expect(onBulkPublish).not.toHaveBeenCalled();
    const dialog = screen.getByRole('dialog');
    const confirmBtn = dialog.querySelector('.confirm-btn') as HTMLElement;
    await fireEvent.click(confirmBtn);
    expect(onBulkPublish).toHaveBeenCalledWith([
      'cid-training-data',
      'cid-video-lecture',
      'cid-family-photo',
    ]);
  });

  it('only sends selected CIDs (not all)', async () => {
    const onBulkBurn = vi.fn();
    const { container } = render(CleanupView, { props: { ...baseProps(), onBulkBurn } });
    // Select only the second card checkbox (index 2, after select-all at 0 and first card at 1)
    const checkboxes = container.querySelectorAll('input[type="checkbox"]');
    await fireEvent.click(checkboxes[2]);
    await fireEvent.click(screen.getByRole('button', { name: /burn all/i }));
    // Confirm the burn dialog
    const dialog = screen.getByRole('dialog');
    const confirmBtn = dialog.querySelector('.confirm-btn') as HTMLElement;
    await fireEvent.click(confirmBtn);
    expect(onBulkBurn).toHaveBeenCalledWith(['cid-video-lecture']);
  });

  // ── Empty state ─────────────────────────────────────────────────

  it('shows empty message when no recommendations', () => {
    render(CleanupView, { props: { ...baseProps(), recommendations: [] } });
    expect(screen.getByText(/no cleanup recommendations/i)).toBeTruthy();
  });

  // ── Toggling individual checkbox updates select-all state ───────

  it('unchecks select-all when one item is deselected', async () => {
    const { container } = render(CleanupView, { props: baseProps() });
    const selectAll = screen.getByRole('checkbox', { name: /select all/i }) as HTMLInputElement;
    await fireEvent.click(selectAll);
    expect(selectAll.checked).toBe(true);
    // Deselect first card
    const checkboxes = container.querySelectorAll('input[type="checkbox"]');
    await fireEvent.click(checkboxes[1]);
    expect(selectAll.checked).toBe(false);
  });
});
