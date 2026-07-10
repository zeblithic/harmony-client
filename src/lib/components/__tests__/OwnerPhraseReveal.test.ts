import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import OwnerPhraseReveal from '../OwnerPhraseReveal.svelte';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
import { invoke } from '@tauri-apps/api/core';
const mockInvoke = vi.mocked(invoke);

// Realistic 32-hex owner id — the redaction tests below depend on this
// being long enough to trip /[0-9a-f]{32,}/ if it ever rendered.
const OWNER = 'a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6';
const WORDS = [
  'abandon', 'ability', 'able', 'about', 'above', 'absent',
  'absorb', 'abstract', 'absurd', 'abuse', 'access', 'accident',
  'account', 'accuse', 'achieve', 'acid', 'acoustic', 'acquire',
  'across', 'act', 'action', 'actor', 'actress', 'actual',
];
const backedUpKey = (id: string) =>
  `harmony.onboarding.recoveryArtifactBackedUp:owner-${id}`;

beforeEach(() => {
  mockInvoke.mockReset();
  localStorage.clear();
  sessionStorage.clear();
});

async function revealWords(utils: { getByTestId: (id: string) => HTMLElement }) {
  await fireEvent.click(utils.getByTestId('phrase-reveal-open'));
  await fireEvent.click(utils.getByTestId('phrase-reveal-confirm'));
  await Promise.resolve();
  await Promise.resolve();
}

describe('OwnerPhraseReveal (ZEB-650 slice 2, Option A)', () => {
  it('renders collapsed with no words and NO IPC call on mount', () => {
    const { getByTestId, queryByTestId, container } = render(OwnerPhraseReveal, {
      props: { ownerId: OWNER },
    });
    expect(getByTestId('phrase-reveal-open')).toBeTruthy();
    expect(queryByTestId('phrase-grid')).toBeNull();
    expect(mockInvoke).not.toHaveBeenCalled();
    expect(container.textContent).not.toContain('abandon');
  });

  it('opening shows the warning but still fires no IPC', async () => {
    const { getByTestId } = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await fireEvent.click(getByTestId('phrase-reveal-open'));
    expect(getByTestId('phrase-reveal-warning')).toBeTruthy();
    expect(mockInvoke).not.toHaveBeenCalled();
  });

  it('confirm fires the IPC exactly once and renders the blurred 24-word grid', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    expect(mockInvoke).toHaveBeenCalledTimes(1);
    expect(mockInvoke).toHaveBeenCalledWith('export_owner_mnemonic_words');
    const grid = utils.getByTestId('phrase-grid');
    expect(grid.querySelectorAll('li').length).toBe(24);
    expect(grid.classList.contains('blurred')).toBe(true);
  });

  // Blur is only visual — the DOM itself must hold masked placeholders until
  // the explicit Reveal, or screen readers / find-in-page / DOM inspection
  // see the words early (CodeRabbit PR #437).
  it('words are absent from the DOM until the explicit unblur', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    expect(utils.container.textContent).not.toContain('abandon');
    expect(utils.getByTestId('phrase-grid').textContent).toContain('••••••');
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    expect(utils.container.textContent).toContain('abandon');
  });

  it('unblur reveals the grid; copy + checkbox appear only then', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    expect(utils.queryByTestId('phrase-copy')).toBeNull();
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    expect(utils.getByTestId('phrase-grid').classList.contains('blurred')).toBe(false);
    expect(utils.getByTestId('phrase-copy')).toBeTruthy();
    expect(utils.getByTestId('phrase-written-down')).toBeTruthy();
  });

  it('ownerId mismatch discards the words and shows an error — nothing renders', async () => {
    mockInvoke.mockResolvedValue({
      words: WORDS,
      ownerId: 'ffffffffffffffffffffffffffffffff',
    });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    expect(utils.queryByTestId('phrase-grid')).toBeNull();
    expect(utils.getByTestId('phrase-reveal-error').textContent).toContain('does not match');
    expect(utils.container.textContent).not.toContain('abandon');
  });

  it('IPC failure shows the backend error inline (wiped-seed case reads naturally)', async () => {
    mockInvoke.mockRejectedValue(
      new Error('Master seed has been wiped from this device — backup is no longer possible.'),
    );
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    expect(utils.getByTestId('phrase-reveal-error').textContent).toContain('wiped');
    expect(utils.queryByTestId('phrase-grid')).toBeNull();
  });

  // The seed↔owner-state mismatch backend error embeds two 32-hex owner ids —
  // they must be redacted before the message reaches the DOM (Qodo PR #437).
  it('redacts 32+ hex runs embedded in backend error messages', async () => {
    const seedId = 'ab12'.repeat(8);
    const stateId = 'cd34'.repeat(8);
    mockInvoke.mockRejectedValue(
      new Error(
        `master seed / owner-state mismatch: seed derives owner-id ${seedId} but owner_state.cbor records ${stateId} — refusing to export`,
      ),
    );
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    const err = utils.getByTestId('phrase-reveal-error').textContent ?? '';
    expect(err).toContain('mismatch');
    expect(err).toContain('[redacted]');
    expect(utils.container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
  });

  it("'I've written these words down' marks the owner-scoped backed-up flag", async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    expect(localStorage.getItem(backedUpKey(OWNER))).toBeNull();
    await fireEvent.click(utils.getByTestId('phrase-written-down'));
    expect(localStorage.getItem(backedUpKey(OWNER))).toBe('true');
  });

  it('mere reveal does NOT count as backed up', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    expect(localStorage.getItem(backedUpKey(OWNER))).toBeNull();
  });

  it('hide collapses and clears the words from the DOM', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    await fireEvent.click(utils.getByTestId('phrase-reveal-hide'));
    expect(utils.queryByTestId('phrase-grid')).toBeNull();
    expect(utils.container.textContent).not.toContain('abandon');
    expect(utils.getByTestId('phrase-reveal-open')).toBeTruthy();
  });

  // ── Redaction invariant (spec §3.3): dto.ownerId must never render ──
  it('never renders a 32+ hex run at ANY phase (dto.ownerId stays out of the DOM)', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    expect(utils.container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
    await revealWords(utils);
    expect(utils.container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    expect(utils.container.innerHTML).not.toMatch(/[0-9a-f]{32,}/i);
  });

  // The other half of the lifetime invariant: unmount (not just Hide) must
  // leave no words behind (CodeRabbit PR #437).
  it('unmount removes the revealed words from the document', async () => {
    mockInvoke.mockResolvedValue({ words: WORDS, ownerId: OWNER });
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await revealWords(utils);
    await fireEvent.click(utils.getByTestId('phrase-reveal-unblur'));
    expect(document.body.textContent).toContain('abandon');
    utils.unmount();
    expect(document.body.textContent).not.toContain('abandon');
  });

  // Escape can dismiss the host modal while the export IPC is in flight —
  // the late resolution must be discarded, not stored (Qodo PR #437).
  it('discards an IPC resolution that lands after unmount', async () => {
    let resolveIpc!: (dto: { words: string[]; ownerId: string }) => void;
    mockInvoke.mockReturnValue(
      new Promise((resolve) => {
        resolveIpc = resolve;
      }),
    );
    const utils = render(OwnerPhraseReveal, { props: { ownerId: OWNER } });
    await fireEvent.click(utils.getByTestId('phrase-reveal-open'));
    await fireEvent.click(utils.getByTestId('phrase-reveal-confirm'));
    utils.unmount(); // modal dismissed mid-flight
    resolveIpc({ words: WORDS, ownerId: OWNER });
    await Promise.resolve();
    await Promise.resolve();
    expect(document.body.textContent).not.toContain('abandon');
  });
});
