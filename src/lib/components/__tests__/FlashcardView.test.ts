import { render, screen, fireEvent } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, it, expect, vi } from 'vitest';
import FlashcardView from '../FlashcardView.svelte';

// AudioCapture depends on Web Audio + getUserMedia, neither of which exist
// in jsdom. Mock the module and expose the last-registered onFrame callback
// so the mid-hold tests can push synthetic PCM frames through processTick.
let capturedOnFrame: ((pcm: Float32Array) => void) | null = null;

vi.mock('../../voice/audio-capture', () => {
  return {
    AudioCapture: class MockAudioCapture {
      isActive() {
        return true;
      }
      async start(onFrame: (pcm: Float32Array) => void) {
        capturedOnFrame = onFrame;
      }
      async stop() {
        capturedOnFrame = null;
      }
    },
  };
});

function syllable(nibble: number) {
  return { nibble, consonant: '', vowel: '' };
}

/** Synthetic PCM frame — contents irrelevant because processPcm is mocked. */
function pcmFrame(len = 160): Float32Array {
  return new Float32Array(len);
}

function createMockService() {
  return {
    isReady: vi.fn().mockReturnValue(true),
    isCalibrated: vi.fn().mockReturnValue(true),
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
    validateRow: vi.fn().mockReturnValue({ matched: true, expected: [], heard: [] }),
    processPcm: vi.fn().mockReturnValue({ syllables: [] }),
    addCalibrationSample: vi.fn(),
    finalizeCalibration: vi.fn(),
    exportProfile: vi.fn().mockReturnValue('{}'),
    importProfile: vi.fn(),
    setCreatedEpochSecs: vi.fn(),
  };
}

describe('FlashcardView', () => {
  it('renders grid with challenge data', () => {
    const mockService = createMockService();
    render(FlashcardView, {
      props: {
        level: 1,
        expressMode: 'off',
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
        expressMode: 'off',
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
        expressMode: 'off',
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
        expressMode: 'off',
        stq8Service: createMockService(),
        onStatsUpdate: vi.fn(),
      },
    });
    expect(screen.getByRole('button', { name: /hint/i })).toBeTruthy();
  });

  it('disables PTT and shows calibrate hint when not calibrated', () => {
    const mockService = createMockService();
    mockService.isCalibrated.mockReturnValue(false);
    render(FlashcardView, {
      props: {
        level: 1,
        expressMode: 'off',
        stq8Service: mockService,
        onStatsUpdate: vi.fn(),
      },
    });
    const ptt = screen.getByRole('button', { name: /push to talk/i });
    expect((ptt as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText(/calibrate your voice/i)).toBeTruthy();
  });

  it('enables PTT and hides calibrate hint when calibrated', () => {
    render(FlashcardView, {
      props: {
        level: 1,
        expressMode: 'off',
        stq8Service: createMockService(),
        onStatsUpdate: vi.fn(),
      },
    });
    const ptt = screen.getByRole('button', { name: /push to talk/i });
    expect((ptt as HTMLButtonElement).disabled).toBe(false);
    expect(screen.queryByText(/calibrate your voice/i)).toBeNull();
  });

  describe('mid-hold classification (slice 4)', () => {
    beforeEach(() => {
      vi.useFakeTimers();
      capturedOnFrame = null;
    });

    afterEach(() => {
      vi.useRealTimers();
    });

    /**
     * Press PTT, let the mocked ensureCapture() resolve, and return the
     * button. Handler is async (awaits AudioCapture.start); microtasks
     * must be flushed before the first setInterval tick is wired up.
     */
    async function pressPtt() {
      const btn = screen.getByRole('button', { name: /push to talk/i });
      await fireEvent.mouseDown(btn);
      // Let handlePttStart's `await ensureCapture()` continue past the
      // mock's immediate resolve — vi.advanceTimersByTimeAsync also
      // flushes microtasks without advancing real time further.
      await vi.advanceTimersByTimeAsync(0);
      return btn;
    }

    it('drains pcmBuffer through processPcm on each 300 ms tick', async () => {
      const mockService = createMockService();
      mockService.generateChallenge.mockReturnValue({
        level: 'Novice',
        data: [0x00],
        rows: [[0x00]],
      });
      // Return nothing so the test only observes call count, not advance.
      mockService.processPcm.mockReturnValue({ syllables: [] });

      render(FlashcardView, {
        props: {
          level: 0,
          expressMode: 'off',
          stq8Service: mockService,
          onStatsUpdate: vi.fn(),
        },
      });

      await pressPtt();
      capturedOnFrame!(pcmFrame());
      await vi.advanceTimersByTimeAsync(300);
      expect(mockService.processPcm).toHaveBeenCalledTimes(1);

      // Second tick with a new frame — processPcm must be called again,
      // only on the new frame. A tick with no new frames is a no-op.
      capturedOnFrame!(pcmFrame());
      await vi.advanceTimersByTimeAsync(300);
      expect(mockService.processPcm).toHaveBeenCalledTimes(2);

      await vi.advanceTimersByTimeAsync(300);
      expect(mockService.processPcm).toHaveBeenCalledTimes(2);
    });

    it('advances rows mid-hold when the heard syllables match', async () => {
      const mockService = createMockService();
      // 1-byte Novice challenge: row [0x00] → nibbles [0x0, 0x0].
      mockService.generateChallenge.mockReturnValue({
        level: 'Novice',
        data: [0x00],
        rows: [[0x00]],
      });
      mockService.processPcm.mockReturnValue({
        syllables: [syllable(0x0), syllable(0x0)],
      });
      const onStatsUpdate = vi.fn();
      render(FlashcardView, {
        props: {
          level: 0,
          expressMode: 'off',
          stq8Service: mockService,
          onStatsUpdate,
        },
      });

      await pressPtt();
      capturedOnFrame!(pcmFrame());
      await vi.advanceTimersByTimeAsync(300);

      // Row matched → handleCardComplete → combo incremented → newChallenge.
      const lastCall = onStatsUpdate.mock.calls.at(-1)?.[0];
      expect(lastCall?.combo).toBe(1);
      expect(lastCall?.cardsCompleted).toBe(1);
    });

    it('shows the mismatch display and preserves combo on a mid-hold wrong syllable', async () => {
      const mockService = createMockService();
      mockService.generateChallenge.mockReturnValue({
        level: 'Novice',
        data: [0x00],
        rows: [[0x00]], // expected nibbles [0x0, 0x0]
      });
      // Heard nibbles [0xf, 0xf] — byte-level red.
      mockService.processPcm.mockReturnValue({
        syllables: [syllable(0xf), syllable(0xf)],
      });
      const onStatsUpdate = vi.fn();
      render(FlashcardView, {
        props: {
          level: 0,
          expressMode: 'off',
          stq8Service: mockService,
          // Seed combo=3 so we can assert it stays intact on mismatch
          // (design: "No penalty on wrong answers").
          initialStats: {
            cardsCompleted: 3,
            perfectCards: 3,
            expressCards: 0,
            bestTimeMs: 500,
            totalTimeMs: 1500,
            previousTimeMs: 500,
            combo: 3,
            totalCreditedBits: 24,
          },
          onStatsUpdate,
        },
      });

      await pressPtt();
      capturedOnFrame!(pcmFrame());
      await vi.advanceTimersByTimeAsync(300);

      // MismatchDisplay is rendered with role="status".
      expect(screen.getByRole('status')).toBeTruthy();
      // No card completion, no combo-reset update published.
      expect(onStatsUpdate).not.toHaveBeenCalled();
    });

    it('breaks combo and resets the row after 2 s of silence during hold', async () => {
      const mockService = createMockService();
      mockService.generateChallenge.mockReturnValue({
        level: 'Novice',
        data: [0x00],
        rows: [[0x00]],
      });
      mockService.processPcm.mockReturnValue({ syllables: [] });
      const onStatsUpdate = vi.fn();
      render(FlashcardView, {
        props: {
          level: 0,
          expressMode: 'off',
          stq8Service: mockService,
          initialStats: {
            cardsCompleted: 2,
            perfectCards: 2,
            expressCards: 0,
            bestTimeMs: 500,
            totalTimeMs: 1000,
            previousTimeMs: 500,
            combo: 2,
            totalCreditedBits: 16,
          },
          onStatsUpdate,
        },
      });

      await pressPtt();
      // 2100 ms covers 7 ticks; the one at t=2100 ms sees Date.now()-
      // lastAdvanceAt >= 2000 ms and fires handleRowTimeout.
      await vi.advanceTimersByTimeAsync(2100);

      const resetCall = onStatsUpdate.mock.calls.find(([s]) => s.combo === 0);
      expect(resetCall).toBeTruthy();
    });

    it('cancels an unfinished row and resets combo on release', async () => {
      const mockService = createMockService();
      mockService.generateChallenge.mockReturnValue({
        level: 'Apprentice',
        data: [0x00, 0xff],
        rows: [[0x00, 0xff]], // needs 4 nibbles
      });
      // Only 2 nibbles heard — partial row, not enough to advance.
      mockService.processPcm.mockReturnValue({
        syllables: [syllable(0x0), syllable(0x0)],
      });
      const onStatsUpdate = vi.fn();
      render(FlashcardView, {
        props: {
          level: 1,
          expressMode: 'off',
          stq8Service: mockService,
          initialStats: {
            cardsCompleted: 1,
            perfectCards: 1,
            expressCards: 0,
            bestTimeMs: 400,
            totalTimeMs: 400,
            previousTimeMs: 400,
            combo: 1,
            totalCreditedBits: 8,
          },
          onStatsUpdate,
        },
      });

      const btn = await pressPtt();
      capturedOnFrame!(pcmFrame());
      await vi.advanceTimersByTimeAsync(300);
      // heardNibbles now [0x0, 0x0] (partial); release before any more.
      await fireEvent.mouseUp(btn);
      await vi.advanceTimersByTimeAsync(0);

      const lastCall = onStatsUpdate.mock.calls.at(-1)?.[0];
      expect(lastCall?.combo).toBe(0);
    });
  });
});
