import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import OwnerRestoreWizard from '../OwnerRestoreWizard.svelte';

// The wizard calls `invoke` directly from @tauri-apps/api/core (ZEB-454).
const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

const WORDS = Array.from({ length: 24 }, (_, i) => `word${i + 1}`).join(' ');
const WORD_ARRAY = WORDS.split(' ');

/** Dispatch invoke by command: preview returns `previewOwnerId`, restore returns it. */
function mockInvoke(previewOwnerId: string) {
  invokeMock.mockImplementation((cmd: string) => {
    if (cmd === 'preview_owner_mnemonic_identity') return Promise.resolve(previewOwnerId);
    if (cmd === 'restore_owner_mnemonic_from_words') return Promise.resolve(previewOwnerId);
    return Promise.reject(new Error(`unexpected command ${cmd}`));
  });
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe('OwnerRestoreWizard (ZEB-454)', () => {
  // ZEB-610 Commons chrome regression guard: the 24-word entry stays a Commons
  // field — the `owner-restore-words` textarea still resolves and carries the
  // `.words` field class the restyle targets. Pins the input-only surface.
  it('renders the recovery-phrase entry as a Commons textarea field', () => {
    const { getByTestId } = render(OwnerRestoreWizard, {
      props: { currentOwnerId: null, onRestored: vi.fn(), onCancel: vi.fn() },
    });
    const words = getByTestId('owner-restore-words');
    expect(words.tagName).toBe('TEXTAREA');
    expect(words.classList.contains('words')).toBe(true);
  });

  it('fresh install: restores non-destructively (force=false), no typed gate', async () => {
    mockInvoke('aabbccdd0011');
    const onRestored = vi.fn();
    const { getByTestId, findByTestId, queryByTestId } = render(OwnerRestoreWizard, {
      props: { currentOwnerId: null, onRestored, onCancel: vi.fn() },
    });

    await fireEvent.input(getByTestId('owner-restore-words'), { target: { value: WORDS } });
    await fireEvent.click(getByTestId('owner-restore-continue'));

    const confirmBtn = (await findByTestId('owner-restore-confirm')) as HTMLButtonElement;
    // Non-destructive: no typed-confirm gate, and the button is immediately ready.
    expect(queryByTestId('owner-restore-typed-confirm')).toBeNull();
    expect(confirmBtn.disabled).toBe(false);

    await fireEvent.click(confirmBtn);
    await vi.waitFor(() => expect(onRestored).toHaveBeenCalledWith('aabbccdd0011'));
    expect(invokeMock).toHaveBeenCalledWith('restore_owner_mnemonic_from_words', {
      words: WORD_ARRAY,
      force: false,
    });
  });

  it('same owner: gates restore behind the typed owner-id prefix, force=true', async () => {
    mockInvoke('aabbccdd0011');
    const onRestored = vi.fn();
    const { getByTestId, findByTestId } = render(OwnerRestoreWizard, {
      props: { currentOwnerId: 'aabbccdd0011', onRestored, onCancel: vi.fn() },
    });

    await fireEvent.input(getByTestId('owner-restore-words'), { target: { value: WORDS } });
    await fireEvent.click(getByTestId('owner-restore-continue'));

    const confirmBtn = (await findByTestId('owner-restore-confirm')) as HTMLButtonElement;
    // Destructive same-owner re-adoption: gated until the prefix is typed.
    expect(confirmBtn.disabled).toBe(true);
    await fireEvent.input(getByTestId('owner-restore-typed-confirm'), {
      target: { value: 'aabbccdd' },
    });
    expect(confirmBtn.disabled).toBe(false);

    await fireEvent.click(confirmBtn);
    await vi.waitFor(() => expect(onRestored).toHaveBeenCalled());
    expect(invokeMock).toHaveBeenCalledWith('restore_owner_mnemonic_from_words', {
      words: WORD_ARRAY,
      force: true,
    });
  });

  it('different owner: refuses and never calls restore', async () => {
    mockInvoke('ffffffff9999'); // preview derives a DIFFERENT owner than current
    const onRestored = vi.fn();
    const { getByTestId, findByTestId, queryByTestId } = render(OwnerRestoreWizard, {
      props: { currentOwnerId: 'aabbccdd0011', onRestored, onCancel: vi.fn() },
    });

    await fireEvent.input(getByTestId('owner-restore-words'), { target: { value: WORDS } });
    await fireEvent.click(getByTestId('owner-restore-continue'));

    const err = await findByTestId('owner-restore-error');
    expect(err.textContent).toMatch(/different identity/i);
    // Confirm step never reached; restore command never issued.
    expect(queryByTestId('owner-restore-confirm')).toBeNull();
    expect(onRestored).not.toHaveBeenCalled();
    expect(
      invokeMock.mock.calls.some((c) => c[0] === 'restore_owner_mnemonic_from_words'),
    ).toBe(false);
  });
});
