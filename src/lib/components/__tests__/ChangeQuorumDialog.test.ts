import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { vi, describe, it, expect, beforeEach } from 'vitest';
import ChangeQuorumDialog from '../ChangeQuorumDialog.svelte';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));

describe('ChangeQuorumDialog', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('slider_and_number_input_sync_bidirectionally', async () => {
    render(ChangeQuorumDialog, {
      props: {
        communityId: 'c-x',
        currentQuorum: 1,
        currentAdminCount: 3,
        onClose: vi.fn(),
      },
    });
    const slider = screen.getByLabelText('Quorum slider') as HTMLInputElement;
    const number = screen.getByLabelText('Quorum number') as HTMLInputElement;

    // Change slider → number input updates.
    await fireEvent.input(slider, { target: { value: '2' } });
    expect(number.value).toBe('2');

    // Change number input → slider updates.
    await fireEvent.input(number, { target: { value: '3' } });
    expect(slider.value).toBe('3');
  });

  it('propose_button_disabled_when_quorum_outside_valid_range', async () => {
    const { rerender } = render(ChangeQuorumDialog, {
      props: {
        communityId: 'c-x',
        currentQuorum: 2,
        currentAdminCount: 3,
        onClose: vi.fn(),
      },
    });

    // proposedQuorum starts at currentQuorum (2) → disabled because equal to currentQuorum.
    const btn = screen.getByRole('button', { name: /Propose/i });
    expect(btn).toBeDisabled();

    // Set proposedQuorum below 1 by re-rendering with currentAdminCount=0 won't work,
    // so we test via the number input: type 0 which is below min(1).
    const number = screen.getByLabelText('Quorum number') as HTMLInputElement;
    await fireEvent.input(number, { target: { value: '0' } });
    expect(btn).toBeDisabled();

    // Type a value above currentAdminCount.
    await fireEvent.input(number, { target: { value: '5' } });
    expect(btn).toBeDisabled();

    // Restore a valid, different value.
    await fireEvent.input(number, { target: { value: '3' } });
    expect(btn).not.toBeDisabled();

    void rerender;
  });

  it('propose_invokes_propose_change_quorum_ipc_with_new_value', async () => {
    const { invoke } = await import('@tauri-apps/api/core');
    const mockInvoke = invoke as ReturnType<typeof vi.fn>;
    mockInvoke.mockResolvedValueOnce({ kind: 'Completed' });

    const onClose = vi.fn();
    render(ChangeQuorumDialog, {
      props: {
        communityId: 'c-abc',
        currentQuorum: 1,
        currentAdminCount: 3,
        onClose,
      },
    });

    // Change to a value different from currentQuorum so Propose is enabled.
    const number = screen.getByLabelText('Quorum number') as HTMLInputElement;
    await fireEvent.input(number, { target: { value: '2' } });

    const btn = screen.getByRole('button', { name: /Propose/i });
    await fireEvent.click(btn);

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith(
        'propose_change_quorum',
        expect.objectContaining({ communityId: 'c-abc', newQuorum: 2 })
      );
    });
    expect(onClose).toHaveBeenCalled();
  });

  it('cancel_button_does_not_close_while_submitting', async () => {
    // R3 CodeRabbit finding: Escape / Cancel mid-submission should not close
    // the dialog (would leave the IPC in flight + optimistic UI confused).
    const { invoke } = await import('@tauri-apps/api/core');
    const mockInvoke = invoke as ReturnType<typeof vi.fn>;
    // Make invoke hang so submitting stays true.
    let resolveInvoke: (v: { kind: string }) => void = () => {};
    mockInvoke.mockReturnValueOnce(
      new Promise((res) => {
        resolveInvoke = res;
      })
    );

    const onClose = vi.fn();
    render(ChangeQuorumDialog, {
      props: {
        communityId: 'c-x',
        currentQuorum: 1,
        currentAdminCount: 3,
        onClose,
      },
    });

    const number = screen.getByLabelText('Quorum number') as HTMLInputElement;
    await fireEvent.input(number, { target: { value: '2' } });
    const proposeBtn = screen.getByRole('button', { name: /Propose/i });
    await fireEvent.click(proposeBtn);

    // submitting is now true (invoke is pending). The Cancel button should
    // be disabled (existing behavior), AND clicking it programmatically
    // should not trigger onClose because of the new guard.
    const cancelBtn = screen.getByRole('button', { name: /Cancel/i });
    expect(cancelBtn).toBeDisabled();
    // Even if disabled were bypassed (programmatic click), onClose should
    // not fire. Fire click anyway to exercise the guard.
    await fireEvent.click(cancelBtn);
    expect(onClose).not.toHaveBeenCalled();

    // Now resolve the IPC; the propose() finally-block resets submitting,
    // and propose() calls handleClose() which DOES close (internal call,
    // no guard). onClose should fire.
    resolveInvoke({ kind: 'Completed' });
    await waitFor(() => {
      expect(onClose).toHaveBeenCalled();
    });
  });

  it('dialog_cancel_event_does_not_close_while_submitting', async () => {
    // R4 CodeRabbit follow-up: complement the Cancel-button test with explicit
    // coverage of the native dialog `cancel` event path (what Escape fires).
    // handleUserCancel should preventDefault when submitting, leaving the
    // dialog open and onClose unfired.
    const { invoke } = await import('@tauri-apps/api/core');
    const mockInvoke = invoke as ReturnType<typeof vi.fn>;
    let resolveInvoke: (v: { kind: string }) => void = () => {};
    mockInvoke.mockReturnValueOnce(
      new Promise((res) => {
        resolveInvoke = res;
      })
    );

    const onClose = vi.fn();
    const { container } = render(ChangeQuorumDialog, {
      props: {
        communityId: 'c-x',
        currentQuorum: 1,
        currentAdminCount: 3,
        onClose,
      },
    });

    const number = screen.getByLabelText('Quorum number') as HTMLInputElement;
    await fireEvent.input(number, { target: { value: '2' } });
    const proposeBtn = screen.getByRole('button', { name: /Propose/i });
    await fireEvent.click(proposeBtn);

    // Find the dialog element and dispatch a cancellable `cancel` event
    // (the same event the browser fires when the user presses Escape).
    const dialogEl = container.querySelector('dialog') as HTMLDialogElement;
    expect(dialogEl).toBeTruthy();
    const cancelEvent = new Event('cancel', { cancelable: true });
    dialogEl.dispatchEvent(cancelEvent);

    // Guard should have called preventDefault and skipped handleClose.
    expect(cancelEvent.defaultPrevented).toBe(true);
    expect(onClose).not.toHaveBeenCalled();

    // Resolving the IPC lets propose()'s internal handleClose fire normally.
    resolveInvoke({ kind: 'Completed' });
    await waitFor(() => {
      expect(onClose).toHaveBeenCalled();
    });
  });

  it('explainer_text_present_for_survivability_recommendation', () => {
    const { container } = render(ChangeQuorumDialog, {
      props: {
        communityId: 'c-x',
        currentQuorum: 2,
        currentAdminCount: 4,
        onClose: vi.fn(),
      },
    });

    // The explainer must mention the N+1 survivability advice.
    expect(container.textContent).toMatch(/N\+1/);
    expect(container.textContent).toMatch(/survivability/i);
  });
});
