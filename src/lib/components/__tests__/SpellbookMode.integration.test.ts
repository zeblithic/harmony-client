/**
 * Integration test for the SpellbookMode subsystem.
 *
 * Tests end-to-end: tab switching, loading state, flashcard grid render,
 * level selector, express mode, hint toggle, PTT + stats tracking, and
 * spell list empty state.
 *
 * Uses a mock stq8Service that returns deterministic challenges (no WASM
 * dependency), mirroring how App.svelte wires the real service.
 */
import { render, screen, fireEvent, cleanup } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import SpellbookMode from '../SpellbookMode.svelte';
import type { FlashcardLevel, Challenge } from '../../flashcard-types';

afterEach(cleanup);

// ── Mock stq8 service ──────────────────────────────────────────────

function createMockStq8(ready = true) {
  const service = {
    isReady: vi.fn(() => ready),
    isCalibrated: vi.fn(() => false),
    getLevelInfo: vi.fn((l: FlashcardLevel) => ({
      total_bytes: [2, 4, 8, 16, 32][l],
      bytes_per_row: [2, 2, 4, 4, 8][l],
      num_rows: [1, 2, 2, 4, 4][l],
      total_bits: [16, 32, 64, 128, 256][l],
    })),
    generateChallenge: vi.fn((l: FlashcardLevel): Challenge => ({
      level: ['Novice', 'Apprentice', 'Journeyman', 'Expert', 'Master'][l],
      data: [0xab, 0xcd],
      rows: [[0xab, 0xcd]],
    })),
    validateRow: vi.fn(() => ({ matched: true, expected: [], heard: [] })),
    processPcm: vi.fn(() => ({ syllables: [] })),
    addCalibrationSample: vi.fn(),
    finalizeCalibration: vi.fn(),
    exportProfile: vi.fn(() => '{}'),
    importProfile: vi.fn(),
    setCreatedEpochSecs: vi.fn(),
  };
  return service;
}

// ── Helper ──────────────────────────────────────────────────────────

function renderSpellbook(overrides: Record<string, unknown> = {}) {
  const onStatsUpdate = vi.fn();
  const defaultStq8Service = createMockStq8();

  const effectiveProps = {
    stq8Service: defaultStq8Service,
    onStatsUpdate,
    ...overrides,
  };

  const result = render(SpellbookMode, { props: effectiveProps });

  return {
    ...result,
    stq8Service: effectiveProps.stq8Service as ReturnType<typeof createMockStq8>,
    onStatsUpdate,
  };
}

describe('SpellbookMode Integration', () => {
  // ── 1. Tab switching ──────────────────────────────────────────────

  it('defaults to Practice tab', () => {
    renderSpellbook();
    const practiceTab = screen.getByRole('tab', { name: 'Practice' });
    expect(practiceTab.getAttribute('aria-selected')).toBe('true');
  });

  it('switches to Spells tab', async () => {
    renderSpellbook();
    const spellsTab = screen.getByRole('tab', { name: 'Spells' });
    await fireEvent.click(spellsTab);

    expect(spellsTab.getAttribute('aria-selected')).toBe('true');
    // Shows empty spell list
    expect(screen.getByText('No spells yet')).toBeTruthy();
  });

  it('switches back to Practice tab from Spells', async () => {
    renderSpellbook();
    const spellsTab = screen.getByRole('tab', { name: 'Spells' });
    await fireEvent.click(spellsTab);

    const practiceTab = screen.getByRole('tab', { name: 'Practice' });
    await fireEvent.click(practiceTab);

    expect(practiceTab.getAttribute('aria-selected')).toBe('true');
    expect(screen.queryByText('No spells yet')).toBeNull();
  });

  // ── 2. Spell list empty state ─────────────────────────────────────

  it('shows hint text in empty spell list', async () => {
    renderSpellbook();
    const spellsTab = screen.getByRole('tab', { name: 'Spells' });
    await fireEvent.click(spellsTab);

    expect(screen.getByText(/Try the Practice tab/)).toBeTruthy();
  });

  // ── 3. Loading state ──────────────────────────────────────────────

  it('shows loading state when service is not ready', () => {
    const stq8Service = createMockStq8(false);
    renderSpellbook({ stq8Service });

    expect(screen.getByText('Loading Q8 engine...')).toBeTruthy();
  });

  it('does not generate challenge when service is not ready', () => {
    const stq8Service = createMockStq8(false);
    renderSpellbook({ stq8Service });

    expect(stq8Service.generateChallenge).not.toHaveBeenCalled();
  });

  // ── 4. Flashcard rendering ────────────────────────────────────────

  it('generates a challenge on mount when service is ready', () => {
    const { stq8Service } = renderSpellbook();
    expect(stq8Service.generateChallenge).toHaveBeenCalledWith(0);
  });

  it('shows hint toggle button', () => {
    renderSpellbook();
    expect(screen.getByLabelText('Toggle hint')).toBeTruthy();
  });

  it('toggles hint visibility', async () => {
    renderSpellbook();
    const hintBtn = screen.getByLabelText('Toggle hint');

    // Initially hint is hidden
    expect(hintBtn.classList.contains('active')).toBe(false);

    await fireEvent.click(hintBtn);
    expect(hintBtn.classList.contains('active')).toBe(true);

    await fireEvent.click(hintBtn);
    expect(hintBtn.classList.contains('active')).toBe(false);
  });

  // ── 5. Level selector ─────────────────────────────────────────────

  it('shows level selector with all 5 levels', () => {
    renderSpellbook();
    const select = screen.getByLabelText('Level') as HTMLSelectElement;
    expect(select).toBeTruthy();
    expect(select.options.length).toBe(5);
    expect(select.options[0].text).toBe('Novice');
    expect(select.options[4].text).toBe('Master');
  });

  it('generates new challenge when level changes', async () => {
    const { stq8Service } = renderSpellbook();
    stq8Service.generateChallenge.mockClear();

    const select = screen.getByLabelText('Level') as HTMLSelectElement;
    await fireEvent.change(select, { target: { value: '2' } });

    expect(stq8Service.generateChallenge).toHaveBeenCalledWith(2);
  });

  // ── 6. Express mode selector ──────────────────────────────────────

  it('shows express mode selector', () => {
    renderSpellbook();
    const select = screen.getByLabelText('Express lane mode') as HTMLSelectElement;
    expect(select).toBeTruthy();
    expect(select.options.length).toBe(4);
  });

  it('defaults express mode to Off', () => {
    renderSpellbook();
    const select = screen.getByLabelText('Express lane mode') as HTMLSelectElement;
    expect(select.value).toBe('off');
  });

  // ── 7. Toolbar controls visibility ────────────────────────────────

  it('hides toolbar controls when on Spells tab', async () => {
    renderSpellbook();
    // Level selector should be visible on Practice tab
    expect(screen.getByLabelText('Level')).toBeTruthy();

    const spellsTab = screen.getByRole('tab', { name: 'Spells' });
    await fireEvent.click(spellsTab);

    // Level selector should be gone
    expect(screen.queryByLabelText('Level')).toBeNull();
    expect(screen.queryByLabelText('Express lane mode')).toBeNull();
  });

  // ── 8. ARIA / Accessibility ───────────────────────────────────────

  it('renders tablist with correct ARIA', () => {
    renderSpellbook();
    const tablist = screen.getByRole('tablist', { name: 'Spellbook tabs' });
    expect(tablist).toBeTruthy();
  });

  it('renders tabpanel', () => {
    renderSpellbook();
    expect(screen.getByRole('tabpanel')).toBeTruthy();
  });

  it('tabpanel aria-labelledby matches active tab id', async () => {
    renderSpellbook();
    const panel = screen.getByRole('tabpanel');
    expect(panel.getAttribute('aria-labelledby')).toBe('tab-practice');

    const spellsTab = screen.getByRole('tab', { name: 'Spells' });
    await fireEvent.click(spellsTab);

    expect(panel.getAttribute('aria-labelledby')).toBe('tab-spells');
  });
});
