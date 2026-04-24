import { render, screen, fireEvent } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, it, expect, vi } from 'vitest';
import FlashcardView from '../FlashcardView.svelte';

// AudioCapture depends on Web Audio + getUserMedia, neither of which exist
// in jsdom. Mock the module and expose the last-registered onFrame callback
// so the mid-hold tests can push synthetic PCM frames through processTick.
// The mock also exposes a pendingStart handle — by default start() resolves
// immediately, but tests can flip `autoResolveStart = false` to hold the
// promise and exercise press/release ordering around the await.
let capturedOnFrame: ((pcm: Float32Array) => void) | null = null;
let pendingStart: { resolve: () => void } | null = null;
let autoResolveStart = true;
let rejectStart = false;

vi.mock('../../voice/audio-capture', () => {
  return {
    AudioCapture: class MockAudioCapture {
      isActive() {
        return true;
      }
      start(onFrame: (pcm: Float32Array) => void): Promise<void> {
        capturedOnFrame = onFrame;
        if (rejectStart) {
          return Promise.reject(new Error('mock: getUserMedia denied'));
        }
        return new Promise((resolve) => {
          pendingStart = { resolve };
          if (autoResolveStart) resolve();
        });
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
      pendingStart = null;
      autoResolveStart = true;
      rejectStart = false;
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

    it('does not start the mid-hold tick if PTT is released before ensureCapture resolves', async () => {
      // Regression for the race flagged by Qodo/CodeAnt/Cursor Bugbot on
      // PR #49: PttButton fires onPttStart synchronously, so handlePttStop
      // can run between the sync prelude of handlePttStart and its post-
      // await startTick(). Without the pttActive guard after await, a late-
      // resolving ensureCapture would start an orphan setInterval that
      // fires handleRowTimeout at 2 s and zeroes the user's combo for no
      // reason.
      autoResolveStart = false;
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
            bestTimeMs: 400,
            totalTimeMs: 800,
            previousTimeMs: 400,
            combo: 2,
            totalCreditedBits: 16,
          },
          onStatsUpdate,
        },
      });

      const btn = screen.getByRole('button', { name: /push to talk/i });
      await fireEvent.mouseDown(btn);
      // Release before start() resolves. handlePttStop sets pttActive=false
      // and lastAdvanceAt=null; the awaited continuation in handlePttStart
      // must see pttActive=false and skip startTick entirely.
      await fireEvent.mouseUp(btn);
      pendingStart!.resolve();
      pendingStart = null;
      await vi.advanceTimersByTimeAsync(0);

      // Advance past the 2 s momentum window. A stale tick would fire
      // processTick here (calling processPcm) and, on an empty buffer,
      // call checkTimeout → handleRowTimeout → publish combo=0.
      await vi.advanceTimersByTimeAsync(2100);

      expect(mockService.processPcm).not.toHaveBeenCalled();
      expect(onStatsUpdate).not.toHaveBeenCalled();
    });

    it('does not start the mid-hold tick when ensureCapture fails', async () => {
      // Regression for CodeRabbit PR #49 finding: ensureCapture swallows
      // getUserMedia/AudioContext errors (logs + sets captureError) and
      // returns normally with audioCapture still null. Before the guard
      // widening, handlePttStart still called startTick after the await,
      // so a user who denied mic access would silently hold PTT for 2 s
      // and then see their combo zeroed by a handleRowTimeout fired on
      // empty buffers — a phantom timeout for a mic that never came up.
      rejectStart = true;
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
            cardsCompleted: 5,
            perfectCards: 5,
            expressCards: 0,
            bestTimeMs: 300,
            totalTimeMs: 1500,
            previousTimeMs: 300,
            combo: 5,
            totalCreditedBits: 40,
          },
          onStatsUpdate,
        },
      });

      const btn = screen.getByRole('button', { name: /push to talk/i });
      await fireEvent.mouseDown(btn);
      // ensureCapture catches the rejection and resolves normally; its
      // continuation in handlePttStart then sees audioCapture=null and
      // must skip startTick. Flush microtasks so the rejection + catch
      // path runs fully.
      await vi.advanceTimersByTimeAsync(0);

      // Advance past the momentum window. A ticker would fire
      // handleRowTimeout here and publish a combo=0 stats update.
      await vi.advanceTimersByTimeAsync(2100);

      expect(mockService.processPcm).not.toHaveBeenCalled();
      expect(onStatsUpdate).not.toHaveBeenCalled();
    });

    it('preserves combo when PTT is released within the mid-hold mismatch window', async () => {
      // Regression for CodeRabbit PR #49 finding: the 300 ms red-flash
      // rowState from a mid-hold mismatch persists across release, and
      // the previous hadAttempt check (heardNibbles OR rowStates.!completed)
      // treated it as an abandoned attempt on release. Per the design
      // rule "No penalty on wrong answers", a release that coincides
      // with mismatch state should not cost the user their combo — the
      // mistake + quick release is exactly the shape the no-penalty
      // rule exists to forgive.
      const mockService = createMockService();
      mockService.generateChallenge.mockReturnValue({
        level: 'Novice',
        data: [0x00],
        rows: [[0x00]], // expected nibbles [0, 0]
      });
      // Heard wildly wrong nibbles → mid-hold mismatch fires.
      mockService.processPcm.mockReturnValue({
        syllables: [syllable(0xf), syllable(0xf)],
      });
      const onStatsUpdate = vi.fn();
      render(FlashcardView, {
        props: {
          level: 0,
          expressMode: 'off',
          stq8Service: mockService,
          initialStats: {
            cardsCompleted: 4,
            perfectCards: 4,
            expressCards: 0,
            bestTimeMs: 300,
            totalTimeMs: 1200,
            previousTimeMs: 300,
            combo: 4,
            totalCreditedBits: 32,
          },
          onStatsUpdate,
        },
      });

      const btn = await pressPtt();
      capturedOnFrame!(pcmFrame());
      await vi.advanceTimersByTimeAsync(300);
      // Mismatch fired — MismatchDisplay is up, red flash is on the grid.
      expect(screen.getByRole('status')).toBeTruthy();

      // Release inside the 300 ms red-flash window.
      await fireEvent.mouseUp(btn);
      await vi.advanceTimersByTimeAsync(0);

      const resetCall = onStatsUpdate.mock.calls.find(([s]) => s.combo === 0);
      expect(resetCall).toBeFalsy();
    });

    it('points the mismatch caret at the first red byte in express mode, skipping yellow bytes', async () => {
      // Regression for Cursor Bugbot PR #49 finding: in express mode an
      // earlier byte can strictly differ but pass express matching
      // (yellow/accepted), while a later byte is the true red failure.
      // findFirstNibbleDiff alone would point at the yellow byte's
      // differing nibble — misleading feedback. The caret must land on
      // the first RED byte's failing nibble instead.
      const mockService = createMockService();
      // Two-byte Apprentice row so byte 0 and byte 1 can differ independently.
      mockService.generateChallenge.mockReturnValue({
        level: 'Apprentice',
        data: [0x01, 0x02],
        rows: [[0x01, 0x02]],
      });
      // Heard nibbles:
      //   byte 0: (0,1) → (0, 5). Nibble 1 strictly differs: exp U (1),
      //           heard JU (5). In 'vowel' mode vowels match → yellow.
      //   byte 1: (0,2) → (0, 7). Nibble 3 strictly differs: exp E (2),
      //           heard JI (7). Vowels differ (E vs I) → red.
      // The caret should land on the low nibble of byte 1 (index 3), not
      // on nibble 1 of the yellow byte.
      mockService.processPcm.mockReturnValue({
        syllables: [syllable(0), syllable(5), syllable(0), syllable(7)],
      });
      render(FlashcardView, {
        props: {
          level: 1,
          expressMode: 'vowel',
          stq8Service: mockService,
          onStatsUpdate: vi.fn(),
        },
      });

      await pressPtt();
      capturedOnFrame!(pcmFrame());
      await vi.advanceTimersByTimeAsync(300);

      // Inspect the caret column via MismatchDisplay's rendered text. The
      // caret column for nibble 3 is LABEL_WIDTH(10) + byteIdx(1)*5 +
      // syllableIdx(1)*2 = 17. Strict-first-diff would have placed it at
      // column 10 + 0*5 + 1*2 = 12.
      const pre = screen.getByLabelText(/mismatch feedback/i);
      const caretLine = pre.textContent?.split('\n').at(-1) ?? '';
      expect(caretLine).toBe(' '.repeat(17) + '^^');
    });

    it('seeds the momentum timer after capture is live so slow ensureCapture does not trigger phantom timeout', async () => {
      // Regression for CodeRabbit PR #49 follow-up: lastAdvanceAt was
      // previously seeded before `await ensureCapture()`. If the first-
      // use permission prompt or AudioContext construction took longer
      // than 2 s, the first post-capture tick would immediately trip
      // handleRowTimeout against a stale-by-3-seconds baseline and
      // zero the user's combo before they'd made a sound.
      autoResolveStart = false;
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
            cardsCompleted: 3,
            perfectCards: 3,
            expressCards: 0,
            bestTimeMs: 300,
            totalTimeMs: 900,
            previousTimeMs: 300,
            combo: 3,
            totalCreditedBits: 24,
          },
          onStatsUpdate,
        },
      });

      const btn = screen.getByRole('button', { name: /push to talk/i });
      await fireEvent.mouseDown(btn);
      // Simulate a slow permission prompt — 3 s with no capture.
      await vi.advanceTimersByTimeAsync(3000);
      // Capture finally comes up.
      pendingStart!.resolve();
      pendingStart = null;
      await vi.advanceTimersByTimeAsync(0);

      // First tick post-capture at +300 ms. With the pre-fix timing the
      // tick would see Date.now() - lastAdvanceAt >= 2 s and fire
      // handleRowTimeout, publishing combo=0.
      await vi.advanceTimersByTimeAsync(300);

      const resetCall = onStatsUpdate.mock.calls.find(([s]) => s.combo === 0);
      expect(resetCall).toBeFalsy();
    });

    it('resets combo when a partial attempt first appears during the release flush', async () => {
      // Regression for CodeRabbit PR #49 follow-up: if a short hold
      // releases before any mid-hold tick fires, the release's final
      // flush is the first (and only) processPcm call. Partial syllables
      // it produces were being ignored by the pre-flush-only hadAttempt
      // check, so combo failed to reset despite the row being abandoned.
      const mockService = createMockService();
      mockService.generateChallenge.mockReturnValue({
        level: 'Apprentice',
        data: [0x00, 0x00],
        rows: [[0x00, 0x00]], // 4 nibbles expected
      });
      // Flush classifies 1 partial nibble — doesn't complete a byte,
      // doesn't trigger mismatch, doesn't complete the card.
      mockService.processPcm.mockReturnValue({
        syllables: [syllable(0x0)],
      });
      const onStatsUpdate = vi.fn();
      render(FlashcardView, {
        props: {
          level: 1,
          expressMode: 'off',
          stq8Service: mockService,
          initialStats: {
            cardsCompleted: 2,
            perfectCards: 2,
            expressCards: 0,
            bestTimeMs: 400,
            totalTimeMs: 800,
            previousTimeMs: 400,
            combo: 2,
            totalCreditedBits: 16,
          },
          onStatsUpdate,
        },
      });

      const btn = await pressPtt();
      capturedOnFrame!(pcmFrame());
      // Release without advancing the tick timer — the flush is the
      // only processPcm call.
      await fireEvent.mouseUp(btn);
      await vi.advanceTimersByTimeAsync(0);

      const resetCall = onStatsUpdate.mock.calls.find(([s]) => s.combo === 0);
      expect(resetCall).toBeTruthy();
    });

    it('preserves the new red-flash row state when a second mismatch fires 300 ms after the first', async () => {
      // Regression for Cursor Bugbot PR #49 finding: handleRowMismatch's
      // setTimeout captures failedRowIndex and unconditionally filters
      // !completed rowStates 300 ms later. Mid-hold ticks are 300 ms
      // apart, so two consecutive wrong-syllable ticks produce back-
      // to-back mismatches whose stale-timeout fires at the exact moment
      // the new red-flash rowState is mounted — stripping it instantly.
      const mockService = createMockService();
      mockService.generateChallenge.mockReturnValue({
        level: 'Novice',
        data: [0x00],
        rows: [[0x00]],
      });
      // Every tick produces the same wrong syllables → another mismatch.
      mockService.processPcm.mockReturnValue({
        syllables: [syllable(0xf), syllable(0xf)],
      });
      render(FlashcardView, {
        props: {
          level: 0,
          expressMode: 'off',
          stq8Service: mockService,
          onStatsUpdate: vi.fn(),
        },
      });

      await pressPtt();
      // Tick 1 at +300 ms — first mismatch, red flash, setTimeout for +600 ms.
      capturedOnFrame!(pcmFrame());
      await vi.advanceTimersByTimeAsync(300);
      // Tick 2 at +600 ms — second mismatch, new red flash. Without the
      // fix, the first mismatch's setTimeout would also fire at +600 ms
      // and wipe the new flash.
      capturedOnFrame!(pcmFrame());
      await vi.advanceTimersByTimeAsync(300);

      // At least one byte-cell should still be rendered with the `red`
      // class (the new mismatch's red flash).
      const redCells = screen
        .queryAllByTestId('byte-cell')
        .filter((el) => el.classList.contains('red'));
      expect(redCells.length).toBeGreaterThan(0);
    });

    it('preserves combo when the release flush completes a card with overshoot nibbles', async () => {
      // Regression for Cursor Bugbot's "carry nibbles break combo" finding:
      // when the final flush itself completes a card, heardNibbles retains
      // the carry for the next card's row 0. The hadAttempt check must
      // consult pre-flush state so that post-flush carry (from the just-
      // completed card) doesn't undo handleCardComplete's combo increment.
      const mockService = createMockService();
      mockService.generateChallenge.mockReturnValue({
        level: 'Novice',
        data: [0x00],
        rows: [[0x00]],
      });
      // 4 syllables: [0x0,0x0] completes card 1, [0xc,0xc] are overshoot.
      mockService.processPcm.mockReturnValue({
        syllables: [
          syllable(0x0),
          syllable(0x0),
          syllable(0xc),
          syllable(0xc),
        ],
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

      const btn = await pressPtt();
      // Feed a frame but do not advance timers — the tick never fires
      // mid-hold, so the flush on release is what completes the card.
      capturedOnFrame!(pcmFrame());
      await fireEvent.mouseUp(btn);
      await vi.advanceTimersByTimeAsync(0);

      const lastCall = onStatsUpdate.mock.calls.at(-1)?.[0];
      expect(lastCall?.cardsCompleted).toBe(1);
      expect(lastCall?.combo).toBe(1);
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
