import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import RecoveryConfigDialog from '../RecoveryConfigDialog.svelte';
import type { CommunityMember } from '../../types';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

const { invoke } = vi.mocked(await import('@tauri-apps/api/core'));

const DAY_MS = 86_400_000;

function member(addr: string, name: string, power = 10): CommunityMember {
  return { address: addr, displayName: name, power, status: 'joined' };
}

const MEMBERS: CommunityMember[] = [
  member('01'.repeat(16), 'ada', 100),
  member('11'.repeat(16), 'bob'),
  member('12'.repeat(16), 'cyn'),
  member('13'.repeat(16), 'dee'),
];

function renderDialog(
  existing: { designateAddrs: string[]; threshold: number; vetoWindowMs: number } | null = null,
) {
  const onClose = vi.fn();
  const onSaved = vi.fn();
  const utils = render(RecoveryConfigDialog, {
    props: {
      communityId: 'c0'.repeat(16),
      joinedMembers: MEMBERS,
      myAddress: '01'.repeat(16),
      existing,
      onClose,
      onSaved,
    },
  });
  return { ...utils, onClose, onSaved };
}

describe('RecoveryConfigDialog', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('disables save until at least one designate is selected', async () => {
    renderDialog();
    const save = screen.getByRole('button', { name: /Save recovery settings/ });
    expect(save).toBeDisabled();
    await fireEvent.click(screen.getByRole('option', { name: /@bob/ }));
    expect(save).not.toBeDisabled();
  });

  it('marks admins as discouraged designate picks', () => {
    renderDialog();
    const adminRow = screen.getByRole('option', { name: /@ada/ });
    expect(adminRow.textContent).toContain('admin — can already act');
  });

  it('clamps the threshold to the selected designate count', async () => {
    renderDialog();
    await fireEvent.click(screen.getByRole('option', { name: /@bob/ }));
    await fireEvent.click(screen.getByRole('option', { name: /@cyn/ }));
    const threshold = screen.getByLabelText('Recovery threshold') as HTMLInputElement;
    await fireEvent.input(threshold, { target: { value: '2' } });
    expect(threshold.value).toBe('2');
    // Deselect one designate → threshold clamps back to 1.
    await fireEvent.click(screen.getByRole('option', { name: /@cyn/ }));
    await waitFor(() => expect(threshold.value).toBe('1'));
  });

  it('submits designates, threshold, and the window converted to ms', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue({ kind: 'Completed' });
    const { onClose, onSaved } = renderDialog();
    await fireEvent.click(screen.getByRole('option', { name: /@bob/ }));
    await fireEvent.click(screen.getByRole('option', { name: /@cyn/ }));
    const threshold = screen.getByLabelText('Recovery threshold') as HTMLInputElement;
    await fireEvent.input(threshold, { target: { value: '2' } });
    const windowDays = screen.getByLabelText('Veto window in days') as HTMLInputElement;
    await fireEvent.input(windowDays, { target: { value: '14' } });

    await fireEvent.click(screen.getByRole('button', { name: /Save recovery settings/ }));
    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('set_recovery_designates', {
        communityId: 'c0'.repeat(16),
        designateAddrs: ['11'.repeat(16), '12'.repeat(16)],
        threshold: 2,
        vetoWindowMs: 14 * DAY_MS,
      });
      expect(onSaved).toHaveBeenCalledWith({ kind: 'Completed' });
      expect(onClose).toHaveBeenCalled();
    });
  });

  it('pre-fills from an existing config', () => {
    renderDialog({
      designateAddrs: ['11'.repeat(16)],
      threshold: 1,
      vetoWindowMs: 21 * DAY_MS,
    });
    const windowDays = screen.getByLabelText('Veto window in days') as HTMLInputElement;
    expect(windowDays.value).toBe('21');
    const bobRow = screen.getByRole('option', { name: /@bob/ });
    expect(bobRow.getAttribute('aria-selected')).toBe('true');
  });

  it('surfaces IPC rejection as an inline error and stays open', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockRejectedValue(
      new Error('set_recovery_designates: designate is not a Joined member'),
    );
    const { onClose } = renderDialog();
    await fireEvent.click(screen.getByRole('option', { name: /@bob/ }));
    await fireEvent.click(screen.getByRole('button', { name: /Save recovery settings/ }));
    await waitFor(() => {
      expect(screen.getByRole('alert').textContent).toContain('not a Joined member');
    });
    expect(onClose).not.toHaveBeenCalled();
  });
});
