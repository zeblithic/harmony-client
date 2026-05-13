import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, screen } from '@testing-library/svelte';
import LastAdminWarningDialog from '../LastAdminWarningDialog.svelte';

const baseProps = {
  open: true,
  action: 'demote' as const,
  communityName: 'IPFS Crew',
  onConfirm: vi.fn().mockResolvedValue(undefined),
  onCancel: vi.fn(),
};

describe('LastAdminWarningDialog', () => {
  it('Proceed disabled until exact token typed (case-sensitive, action=demote)', async () => {
    render(LastAdminWarningDialog, { props: baseProps });

    const proceedBtn = screen.getByRole('button', { name: 'Proceed' }) as HTMLButtonElement;
    expect(proceedBtn.disabled).toBe(true);

    // Lowercase 'demote' does NOT match token 'DEMOTE' — still disabled
    const input = screen.getByRole('textbox');
    await fireEvent.input(input, { target: { value: 'demote' } });
    expect(proceedBtn.disabled).toBe(true);

    // Exact uppercase 'DEMOTE' — now enabled
    await fireEvent.input(input, { target: { value: 'DEMOTE' } });
    expect(proceedBtn.disabled).toBe(false);
  });

  it('LEAVE token required for action=leave; Proceed calls onConfirm', async () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    render(LastAdminWarningDialog, {
      props: { ...baseProps, action: 'leave', onConfirm },
    });

    const input = screen.getByRole('textbox');
    await fireEvent.input(input, { target: { value: 'LEAVE' } });

    const proceedBtn = screen.getByRole('button', { name: 'Proceed' });
    await fireEvent.click(proceedBtn);

    expect(onConfirm).toHaveBeenCalledOnce();
  });

  it('Cancel does not fire onConfirm, fires onCancel', async () => {
    const onConfirm = vi.fn();
    const onCancel = vi.fn();
    render(LastAdminWarningDialog, { props: { ...baseProps, onConfirm, onCancel } });

    await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));

    expect(onConfirm).not.toHaveBeenCalled();
    expect(onCancel).toHaveBeenCalledOnce();
  });
});
