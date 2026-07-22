import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { vi, describe, it, expect, beforeEach } from 'vitest';
import ChangeThresholdsDialog from '../ChangeThresholdsDialog.svelte';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

describe('ChangeThresholdsDialog', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('slider_and_number_input_sync_bidirectionally', async () => {
    render(ChangeThresholdsDialog, {
      props: {
        communityId: 'c-x',
        currentThresholds: { invite: 0, kick: 50, setPower: 100 },
        onClose: vi.fn(),
      },
    });
    const slider = screen.getByLabelText('Invite threshold slider') as HTMLInputElement;
    const number = screen.getByLabelText('Invite threshold number') as HTMLInputElement;

    // Change slider → number input updates.
    await fireEvent.input(slider, { target: { value: '10' } });
    expect(number.value).toBe('10');

    // Change number input → slider updates.
    await fireEvent.input(number, { target: { value: '20' } });
    expect(slider.value).toBe('20');
  });

  it('propose_invokes_propose_change_thresholds_ipc_with_new_values', async () => {
    const { invoke } = await import('@tauri-apps/api/core');
    const mockInvoke = invoke as ReturnType<typeof vi.fn>;
    mockInvoke.mockResolvedValueOnce({ kind: 'Completed' });

    const onClose = vi.fn();
    render(ChangeThresholdsDialog, {
      props: {
        communityId: 'c-abc',
        currentThresholds: { invite: 0, kick: 50, setPower: 100 },
        onClose,
      },
    });

    // Edit the invite input to 25 so Propose is enabled (differs from current).
    const inviteNumber = screen.getByLabelText('Invite threshold number') as HTMLInputElement;
    await fireEvent.input(inviteNumber, { target: { value: '25' } });

    const btn = screen.getByRole('button', { name: /Propose/i });
    await fireEvent.click(btn);

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith('propose_change_thresholds', {
        communityId: 'c-abc',
        invite: 25,
        kick: 50,
        setPower: 100,
      });
    });
    expect(onClose).toHaveBeenCalled();
  });

  it('propose_button_disabled_when_ordering_invalid', async () => {
    render(ChangeThresholdsDialog, {
      props: {
        communityId: 'c-x',
        currentThresholds: { invite: 0, kick: 50, setPower: 100 },
        onClose: vi.fn(),
      },
    });

    const btn = screen.getByRole('button', { name: /Propose/i });
    // Starts disabled — unchanged from currentThresholds.
    expect(btn).toBeDisabled();

    // Make kick < invite (ordering-invalid): should stay disabled.
    const inviteNumber = screen.getByLabelText('Invite threshold number') as HTMLInputElement;
    await fireEvent.input(inviteNumber, { target: { value: '60' } });
    expect(btn).toBeDisabled();

    // Restore a valid, different value.
    await fireEvent.input(inviteNumber, { target: { value: '10' } });
    expect(btn).not.toBeDisabled();
  });

  it('propose_button_disabled_for_fractional_or_negative_values', async () => {
    // A u8 IPC boundary can't take -1 or 2.5; the dialog must reject them
    // rather than enable submit and fail server-side (ZEB-251 / Qodo).
    render(ChangeThresholdsDialog, {
      props: {
        communityId: 'c-x',
        currentThresholds: { invite: 0, kick: 50, setPower: 100 },
        onClose: vi.fn(),
      },
    });
    const btn = screen.getByRole('button', { name: /Propose/i });
    const inviteNumber = screen.getByLabelText('Invite threshold number') as HTMLInputElement;

    // Fractional → disabled (ordering alone would have accepted 2.5 ≤ 50 ≤ 100).
    await fireEvent.input(inviteNumber, { target: { value: '2.5' } });
    expect(btn).toBeDisabled();

    // Negative → disabled.
    await fireEvent.input(inviteNumber, { target: { value: '-1' } });
    expect(btn).toBeDisabled();

    // Whole number in range → enabled.
    await fireEvent.input(inviteNumber, { target: { value: '10' } });
    expect(btn).not.toBeDisabled();
  });

  it('cancel_button_does_not_close_while_submitting', async () => {
    const { invoke } = await import('@tauri-apps/api/core');
    const mockInvoke = invoke as ReturnType<typeof vi.fn>;
    let resolveInvoke: (v: { kind: string }) => void = () => {};
    mockInvoke.mockReturnValueOnce(
      new Promise((res) => {
        resolveInvoke = res;
      })
    );

    const onClose = vi.fn();
    render(ChangeThresholdsDialog, {
      props: {
        communityId: 'c-x',
        currentThresholds: { invite: 0, kick: 50, setPower: 100 },
        onClose,
      },
    });

    const inviteNumber = screen.getByLabelText('Invite threshold number') as HTMLInputElement;
    await fireEvent.input(inviteNumber, { target: { value: '10' } });
    const proposeBtn = screen.getByRole('button', { name: /Propose/i });
    await fireEvent.click(proposeBtn);

    const cancelBtn = screen.getByRole('button', { name: /Cancel/i });
    expect(cancelBtn).toBeDisabled();
    await fireEvent.click(cancelBtn);
    expect(onClose).not.toHaveBeenCalled();

    resolveInvoke({ kind: 'Completed' });
    await waitFor(() => {
      expect(onClose).toHaveBeenCalled();
    });
  });

  it('renders the self-referential admin-action warning', () => {
    const { container } = render(ChangeThresholdsDialog, {
      props: {
        communityId: 'c-x',
        currentThresholds: { invite: 0, kick: 50, setPower: 100 },
        onClose: vi.fn(),
      },
    });
    expect(container.textContent).toMatch(/itself an admin action/);
  });
});
