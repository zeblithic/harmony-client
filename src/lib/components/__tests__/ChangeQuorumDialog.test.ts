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
