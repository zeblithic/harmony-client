import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import SpellbookMode from '../SpellbookMode.svelte';

function createMockService() {
  return {
    isReady: vi.fn().mockReturnValue(true),
    isCalibrated: vi.fn().mockReturnValue(false),
    getLevelInfo: vi.fn().mockReturnValue({
      total_bytes: 1, bytes_per_row: 1, num_rows: 1, total_bits: 8,
    }),
    generateChallenge: vi.fn().mockReturnValue({
      level: 'Novice', data: [0x42], rows: [[0x42]],
    }),
    validateRow: vi.fn().mockReturnValue({ matched: true, expected: [], heard: [] }),
    processPcm: vi.fn().mockReturnValue({ syllables: [] }),
    addCalibrationSample: vi.fn(),
    finalizeCalibration: vi.fn(),
    exportProfile: vi.fn().mockReturnValue('{}'),
    importProfile: vi.fn(),
    setCreatedEpochSecs: vi.fn(),
  };
}

describe('SpellbookMode', () => {
  it('renders Spells and Practice tabs', () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    expect(screen.getByRole('tab', { name: /spells/i })).toBeTruthy();
    expect(screen.getByRole('tab', { name: /practice/i })).toBeTruthy();
  });

  it('shows Practice tab by default', () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    const practiceTab = screen.getByRole('tab', { name: /practice/i });
    expect(practiceTab.getAttribute('aria-selected')).toBe('true');
  });

  it('switches to Spells tab on click', async () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    await fireEvent.click(screen.getByRole('tab', { name: /spells/i }));
    expect(screen.getByText(/no spells yet/i)).toBeTruthy();
  });

  it('renders level selector', () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    expect(screen.getByLabelText(/level/i)).toBeTruthy();
  });

  it('renders express lane toggle', () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    expect(screen.getByLabelText(/express lane/i)).toBeTruthy();
  });

  it('has accessible tablist', () => {
    render(SpellbookMode, {
      props: { stq8Service: createMockService() },
    });
    expect(screen.getByRole('tablist')).toBeTruthy();
  });
});
