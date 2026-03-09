import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import RecommendationCard from '../RecommendationCard.svelte';
import type { CleanupRecommendation } from '../../types';

const baseRec: CleanupRecommendation = {
  cid: 'cid-training-data',
  name: 'sensor-readings.parquet',
  category: 'dataset',
  sizeBytes: 250_000_000,
  reason: 'stale',
  stalenessScore: 0.85,
  spaceRecoverable: 250_000_000,
  confidence: 0.92,
};

describe('RecommendationCard', () => {
  it('renders file name', () => {
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction: vi.fn(), onToggle: vi.fn() },
    });
    expect(screen.getByText('sensor-readings.parquet')).toBeTruthy();
  });

  it('renders category icon', () => {
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction: vi.fn(), onToggle: vi.fn() },
    });
    // dataset = 📊
    expect(screen.getByText('\uD83D\uDCCA')).toBeTruthy();
  });

  it('renders formatted size', () => {
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction: vi.fn(), onToggle: vi.fn() },
    });
    expect(screen.getByText('250.0 MB')).toBeTruthy();
  });

  it('renders reason badge', () => {
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction: vi.fn(), onToggle: vi.fn() },
    });
    expect(screen.getByText('stale')).toBeTruthy();
  });

  it('renders space recoverable', () => {
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction: vi.fn(), onToggle: vi.fn() },
    });
    expect(screen.getByText(/250\.0 MB recoverable/)).toBeTruthy();
  });

  it('renders staleness score bar', () => {
    const { container } = render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction: vi.fn(), onToggle: vi.fn() },
    });
    const bar = container.querySelector('.staleness-bar-fill');
    expect(bar).toBeTruthy();
    expect(bar!.getAttribute('style')).toContain('85%');
  });

  it('renders five action buttons', () => {
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction: vi.fn(), onToggle: vi.fn() },
    });
    expect(screen.getByRole('button', { name: /burn/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /archive/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /release/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /publish/i })).toBeTruthy();
    expect(screen.getByRole('button', { name: /pin/i })).toBeTruthy();
  });

  it('fires onAction with cid and action when Burn clicked and confirmed', async () => {
    const onAction = vi.fn();
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction, onToggle: vi.fn() },
    });
    await fireEvent.click(screen.getByRole('button', { name: /burn/i }));
    // Burn now requires confirmation
    expect(onAction).not.toHaveBeenCalled();
    const dialog = screen.getByRole('dialog');
    const confirmBtn = dialog.querySelector('.confirm-btn') as HTMLElement;
    await fireEvent.click(confirmBtn);
    expect(onAction).toHaveBeenCalledWith('cid-training-data', 'burn');
  });

  it('fires onAction with cid and action when Archive clicked', async () => {
    const onAction = vi.fn();
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction, onToggle: vi.fn() },
    });
    await fireEvent.click(screen.getByRole('button', { name: /archive/i }));
    expect(onAction).toHaveBeenCalledWith('cid-training-data', 'archive');
  });

  it('fires onAction with cid and action when Release clicked', async () => {
    const onAction = vi.fn();
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction, onToggle: vi.fn() },
    });
    await fireEvent.click(screen.getByRole('button', { name: /release/i }));
    expect(onAction).toHaveBeenCalledWith('cid-training-data', 'release');
  });

  it('fires onAction with cid and action when Publish clicked', async () => {
    const onAction = vi.fn();
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction, onToggle: vi.fn() },
    });
    await fireEvent.click(screen.getByRole('button', { name: /publish/i }));
    expect(onAction).toHaveBeenCalledWith('cid-training-data', 'publish');
  });

  it('fires onAction with cid and action when Pin clicked', async () => {
    const onAction = vi.fn();
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction, onToggle: vi.fn() },
    });
    await fireEvent.click(screen.getByRole('button', { name: /pin/i }));
    expect(onAction).toHaveBeenCalledWith('cid-training-data', 'pin');
  });

  it('renders checkbox and fires onToggle when toggled', async () => {
    const onToggle = vi.fn();
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction: vi.fn(), onToggle },
    });
    const checkbox = screen.getByRole('checkbox');
    expect(checkbox).toBeTruthy();
    await fireEvent.click(checkbox);
    expect(onToggle).toHaveBeenCalledWith('cid-training-data');
  });

  it('reflects checked state on checkbox', () => {
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: true, onAction: vi.fn(), onToggle: vi.fn() },
    });
    const checkbox = screen.getByRole('checkbox') as HTMLInputElement;
    expect(checkbox.checked).toBe(true);
  });

  it('shows suggestion text', () => {
    render(RecommendationCard, {
      props: { recommendation: baseRec, checked: false, onAction: vi.fn(), onToggle: vi.fn() },
    });
    expect(screen.getByText(/publish to preserve it forever/i)).toBeTruthy();
  });

  it('renders over-replicated reason badge', () => {
    const overRep: CleanupRecommendation = { ...baseRec, reason: 'over-replicated' };
    render(RecommendationCard, {
      props: { recommendation: overRep, checked: false, onAction: vi.fn(), onToggle: vi.fn() },
    });
    expect(screen.getByText('over-replicated')).toBeTruthy();
  });

  it('renders duplicate-of-public reason badge', () => {
    const dupRec: CleanupRecommendation = { ...baseRec, reason: 'duplicate-of-public' };
    render(RecommendationCard, {
      props: { recommendation: dupRec, checked: false, onAction: vi.fn(), onToggle: vi.fn() },
    });
    expect(screen.getByText('duplicate-of-public')).toBeTruthy();
  });
});
