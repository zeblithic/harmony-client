import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import DoubleConfirmDialog from '../DoubleConfirmDialog.svelte';

describe('DoubleConfirmDialog', () => {
  it('shows first gate initially', () => {
    render(DoubleConfirmDialog, {
      props: {
        title: 'Publish',
        firstMessage: 'This will publish your file.',
        secondMessage: 'Are you really sure?',
        confirmLabel: 'Publish',
        onConfirm: vi.fn(),
        onCancel: vi.fn(),
      },
    });
    expect(screen.getByText('This will publish your file.')).toBeTruthy();
    expect(screen.queryByText('Are you really sure?')).toBeNull();
    expect(screen.getByRole('button', { name: 'Continue' })).toBeTruthy();
  });

  it('advances to second gate on Continue', async () => {
    render(DoubleConfirmDialog, {
      props: {
        title: 'Publish',
        firstMessage: 'This will publish your file.',
        secondMessage: 'Are you really sure?',
        confirmLabel: 'Publish',
        onConfirm: vi.fn(),
        onCancel: vi.fn(),
      },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
    expect(screen.getByText('Are you really sure?')).toBeTruthy();
    expect(screen.queryByText('This will publish your file.')).toBeNull();
    expect(screen.getByRole('button', { name: 'Publish' })).toBeTruthy();
  });

  it('calls onConfirm only after second gate', async () => {
    const onConfirm = vi.fn();
    render(DoubleConfirmDialog, {
      props: {
        title: 'Publish',
        firstMessage: 'This will publish your file.',
        secondMessage: 'Are you really sure?',
        confirmLabel: 'Publish',
        onConfirm,
        onCancel: vi.fn(),
      },
    });
    // Continue past gate 1
    await fireEvent.click(screen.getByRole('button', { name: 'Continue' }));
    expect(onConfirm).not.toHaveBeenCalled();
    // Confirm at gate 2
    await fireEvent.click(screen.getByRole('button', { name: 'Publish' }));
    expect(onConfirm).toHaveBeenCalledOnce();
  });

  it('Cancel at gate 1 calls onCancel', async () => {
    const onCancel = vi.fn();
    render(DoubleConfirmDialog, {
      props: {
        title: 'Publish',
        firstMessage: 'This will publish your file.',
        secondMessage: 'Are you really sure?',
        confirmLabel: 'Publish',
        onConfirm: vi.fn(),
        onCancel,
      },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onCancel).toHaveBeenCalledOnce();
  });
});
