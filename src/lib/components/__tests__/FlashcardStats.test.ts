import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import FlashcardStats from '../FlashcardStats.svelte';
import { initialSessionStats, type SessionStats } from '../../flashcard-types';

describe('FlashcardStats', () => {
  it('renders all stat labels', () => {
    render(FlashcardStats, { props: { stats: initialSessionStats() } });
    expect(screen.getByText('Cards completed')).toBeTruthy();
    expect(screen.getByText('Perfect cards')).toBeTruthy();
    expect(screen.getByText('Express cards')).toBeTruthy();
    expect(screen.getByText('Best time')).toBeTruthy();
    expect(screen.getByText('Average time')).toBeTruthy();
    expect(screen.getByText('Previous time')).toBeTruthy();
    expect(screen.getByText('Combo')).toBeTruthy();
    expect(screen.getByText('Effective bitrate')).toBeTruthy();
  });

  it('displays zeroed stats', () => {
    render(FlashcardStats, { props: { stats: initialSessionStats() } });
    const values = screen.getAllByTestId('stat-value');
    expect(values[0].textContent).toBe('0');
  });

  it('displays populated stats', () => {
    const stats: SessionStats = {
      cardsCompleted: 5,
      perfectCards: 3,
      expressCards: 2,
      bestTimeMs: 1234,
      totalTimeMs: 6170,
      previousTimeMs: 1500,
      combo: 3,
      totalCreditedBits: 200,
    };
    render(FlashcardStats, { props: { stats } });
    const values = screen.getAllByTestId('stat-value');
    expect(values[0].textContent).toBe('5');
    expect(values[1].textContent).toBe('3');
    expect(values[2].textContent).toBe('2');
  });

  it('formats time values as seconds', () => {
    const stats: SessionStats = {
      ...initialSessionStats(),
      bestTimeMs: 2500,
      previousTimeMs: 3100,
    };
    render(FlashcardStats, { props: { stats } });
    expect(screen.getByText('2.50s')).toBeTruthy();
    expect(screen.getByText('3.10s')).toBeTruthy();
  });

  it('shows dash for null time values', () => {
    render(FlashcardStats, { props: { stats: initialSessionStats() } });
    expect(screen.getAllByText('—').length).toBeGreaterThanOrEqual(2);
  });

  it('calculates effective bitrate', () => {
    const stats: SessionStats = {
      ...initialSessionStats(),
      cardsCompleted: 1,
      totalCreditedBits: 80,
      totalTimeMs: 10000,
    };
    render(FlashcardStats, { props: { stats } });
    expect(screen.getByText('8.0 bps')).toBeTruthy();
  });
});
