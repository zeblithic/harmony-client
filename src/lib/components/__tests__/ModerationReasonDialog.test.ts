import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, screen } from '@testing-library/svelte';
import ModerationReasonDialog from '../ModerationReasonDialog.svelte';

const baseProps = {
  open: true,
  action: 'kick' as const,
  targetName: 'Bob',
  communityName: 'IPFS Crew',
  onConfirm: vi.fn(),
  onCancel: vi.fn(),
};

describe('ModerationReasonDialog', () => {
  it('Kick with reason: onConfirm called with the trimmed reason string', async () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    render(ModerationReasonDialog, { props: { ...baseProps, onConfirm } });

    const textarea = screen.getByPlaceholderText(/e.g., repeated spam/i);
    await fireEvent.input(textarea, { target: { value: 'spam' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Kick' }));

    expect(onConfirm).toHaveBeenCalledWith('spam');
  });

  it('Kick with blank reason: onConfirm called with null', async () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    render(ModerationReasonDialog, { props: { ...baseProps, onConfirm } });

    // Do not type anything in the textarea
    await fireEvent.click(screen.getByRole('button', { name: 'Kick' }));

    expect(onConfirm).toHaveBeenCalledWith(null);
  });

  it('IPC rejection surfaces inline alert', async () => {
    const onConfirm = vi.fn().mockRejectedValue(new Error('insufficient power'));
    render(ModerationReasonDialog, { props: { ...baseProps, onConfirm } });

    await fireEvent.click(screen.getByRole('button', { name: 'Kick' }));

    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('insufficient power');
  });

  it('Cancel does not fire onConfirm, fires onCancel', async () => {
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    render(ModerationReasonDialog, { props: { ...baseProps, onConfirm, onCancel } });

    await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));

    expect(onConfirm).not.toHaveBeenCalled();
    expect(onCancel).toHaveBeenCalledOnce();
  });
});
