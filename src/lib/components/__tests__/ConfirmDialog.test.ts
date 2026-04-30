import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ConfirmDialog from '../ConfirmDialog.svelte';

describe('ConfirmDialog', () => {
  it('renders title and message', () => {
    render(ConfirmDialog, {
      props: {
        title: 'Delete File',
        message: 'Are you sure you want to delete this file?',
        confirmLabel: 'Delete',
        onConfirm: vi.fn(),
        onCancel: vi.fn(),
      },
    });
    expect(screen.getByText('Delete File')).toBeTruthy();
    expect(screen.getByText('Are you sure you want to delete this file?')).toBeTruthy();
  });

  it('has role="dialog" with aria-modal="true"', () => {
    render(ConfirmDialog, {
      props: {
        title: 'Confirm',
        message: 'Proceed?',
        confirmLabel: 'Yes',
        onConfirm: vi.fn(),
        onCancel: vi.fn(),
      },
    });
    const dialog = screen.getByRole('dialog');
    expect(dialog).toBeTruthy();
    expect(dialog.getAttribute('aria-modal')).toBe('true');
  });

  it('calls onConfirm when confirm button clicked', async () => {
    const onConfirm = vi.fn();
    render(ConfirmDialog, {
      props: {
        title: 'Confirm',
        message: 'Proceed?',
        confirmLabel: 'Yes',
        onConfirm,
        onCancel: vi.fn(),
      },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Yes' }));
    expect(onConfirm).toHaveBeenCalledOnce();
  });

  it('calls onCancel when cancel button clicked', async () => {
    const onCancel = vi.fn();
    render(ConfirmDialog, {
      props: {
        title: 'Confirm',
        message: 'Proceed?',
        confirmLabel: 'Yes',
        onConfirm: vi.fn(),
        onCancel,
      },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onCancel).toHaveBeenCalledOnce();
  });

  it('applies destructive class when destructive prop is true', () => {
    render(ConfirmDialog, {
      props: {
        title: 'Delete',
        message: 'This is permanent.',
        confirmLabel: 'Delete',
        destructive: true,
        onConfirm: vi.fn(),
        onCancel: vi.fn(),
      },
    });
    const confirmBtn = screen.getByRole('button', { name: 'Delete' });
    expect(confirmBtn.classList.contains('destructive')).toBe(true);
  });

  it('renders inside <Modal> with correct a11y attributes', () => {
    const { getByRole } = render(ConfirmDialog, {
      props: {
        title: 'Confirm',
        message: 'Are you sure?',
        confirmLabel: 'Yes',
        onConfirm: () => {},
        onCancel: () => {},
      },
    });
    const dialog = getByRole('dialog');
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(dialog.getAttribute('aria-labelledby')).toMatch(/^dialog-title-/);
  });
});
