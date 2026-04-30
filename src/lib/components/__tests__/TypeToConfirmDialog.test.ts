import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import TypeToConfirmDialog from '../TypeToConfirmDialog.svelte';

describe('TypeToConfirmDialog', () => {
  it('confirm button is disabled until text matches', () => {
    render(TypeToConfirmDialog, {
      props: {
        title: 'Burn File',
        message: 'Type the file name to confirm permanent deletion.',
        confirmText: 'my-file.txt',
        confirmLabel: 'Burn',
        onConfirm: vi.fn(),
        onCancel: vi.fn(),
      },
    });
    const confirmBtn = screen.getByRole('button', { name: 'Burn' });
    expect(confirmBtn.hasAttribute('disabled')).toBe(true);
  });

  it('enables confirm button when typed text matches', async () => {
    render(TypeToConfirmDialog, {
      props: {
        title: 'Burn File',
        message: 'Type the file name to confirm permanent deletion.',
        confirmText: 'my-file.txt',
        confirmLabel: 'Burn',
        onConfirm: vi.fn(),
        onCancel: vi.fn(),
      },
    });
    const input = screen.getByRole('textbox', { name: 'Type to confirm' });
    await fireEvent.input(input, { target: { value: 'my-file.txt' } });
    const confirmBtn = screen.getByRole('button', { name: 'Burn' });
    expect(confirmBtn.hasAttribute('disabled')).toBe(false);
  });

  it('match is case-sensitive', async () => {
    render(TypeToConfirmDialog, {
      props: {
        title: 'Burn File',
        message: 'Type the file name to confirm permanent deletion.',
        confirmText: 'my-file.txt',
        confirmLabel: 'Burn',
        onConfirm: vi.fn(),
        onCancel: vi.fn(),
      },
    });
    const input = screen.getByRole('textbox', { name: 'Type to confirm' });
    await fireEvent.input(input, { target: { value: 'My-File.txt' } });
    const confirmBtn = screen.getByRole('button', { name: 'Burn' });
    expect(confirmBtn.hasAttribute('disabled')).toBe(true);
  });

  it('renders inside <Modal> with correct a11y attributes and focuses the input', async () => {
    const { getByRole, getByLabelText } = render(TypeToConfirmDialog, {
      props: {
        title: 'Confirm',
        message: 'Type to confirm',
        confirmText: 'DELETE',
        confirmLabel: 'Delete',
        onConfirm: () => {},
        onCancel: () => {},
      },
    });
    const dialog = getByRole('dialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(dialog.getAttribute('aria-labelledby')).toMatch(/^dialog-title-/);
    // Verify input is first-focused (preserves the existing UX).
    const input = getByLabelText('Type to confirm');
    expect(document.activeElement).toBe(input);
  });
});
