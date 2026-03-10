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
});
