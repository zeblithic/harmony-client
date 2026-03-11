import { render, screen } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import FlashcardView from '../FlashcardView.svelte';

function createMockService() {
  return {
    isReady: vi.fn().mockReturnValue(true),
    getLevelInfo: vi.fn().mockReturnValue({
      total_bytes: 2,
      bytes_per_row: 2,
      num_rows: 1,
      total_bits: 16,
    }),
    generateChallenge: vi.fn().mockReturnValue({
      level: 'Apprentice',
      data: [0x00, 0xff],
      rows: [[0x00, 0xff]],
    }),
  };
}

describe('FlashcardView', () => {
  it('renders grid with challenge data', () => {
    const mockService = createMockService();
    render(FlashcardView, {
      props: {
        level: 1,
        expressLane: false,
        stq8Service: mockService,
        onStatsUpdate: vi.fn(),
      },
    });
    expect(mockService.generateChallenge).toHaveBeenCalledWith(1);
    expect(screen.getByRole('grid')).toBeTruthy();
  });

  it('renders PTT button', () => {
    render(FlashcardView, {
      props: {
        level: 1,
        expressLane: false,
        stq8Service: createMockService(),
        onStatsUpdate: vi.fn(),
      },
    });
    expect(screen.getByRole('button', { name: /push to talk/i })).toBeTruthy();
  });

  it('shows loading state when service not ready', () => {
    const mockService = createMockService();
    mockService.isReady.mockReturnValue(false);
    render(FlashcardView, {
      props: {
        level: 1,
        expressLane: false,
        stq8Service: mockService,
        onStatsUpdate: vi.fn(),
      },
    });
    expect(screen.getByText(/loading/i)).toBeTruthy();
  });

  it('shows hint bar toggle', () => {
    render(FlashcardView, {
      props: {
        level: 1,
        expressLane: false,
        stq8Service: createMockService(),
        onStatsUpdate: vi.fn(),
      },
    });
    expect(screen.getByRole('button', { name: /hint/i })).toBeTruthy();
  });
});
