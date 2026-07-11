import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import RemoveDeviceDialog from '../RemoveDeviceDialog.svelte';

function props(overrides: Record<string, unknown> = {}) {
  return {
    deviceName: 'Study Mac',
    isSelf: false,
    isSeedHolder: false,
    busy: false,
    error: null,
    onConfirm: vi.fn(),
    onCancel: vi.fn(),
    ...overrides,
  };
}

describe('RemoveDeviceDialog (ZEB-668 S2)', () => {
  it('disables confirm until the exact device name is typed', async () => {
    const p = props();
    render(RemoveDeviceDialog, { props: p });
    const confirm = screen.getByRole('button', { name: /remove device/i });
    expect(confirm).toBeDisabled();
    await fireEvent.input(screen.getByRole('textbox'), { target: { value: 'Study Mac' } });
    expect(confirm).toBeEnabled();
    await fireEvent.click(confirm);
    expect(p.onConfirm).toHaveBeenCalledWith('decommissioned');
  });

  it('passes the selected reason to onConfirm', async () => {
    const p = props();
    render(RemoveDeviceDialog, { props: p });
    await fireEvent.click(screen.getByRole('radio', { name: /compromised/i }));
    await fireEvent.input(screen.getByRole('textbox'), { target: { value: 'Study Mac' } });
    await fireEvent.click(screen.getByRole('button', { name: /remove device/i }));
    expect(p.onConfirm).toHaveBeenCalledWith('compromised');
  });

  it('shows the seed-holder warning only for the seed-holding self device', () => {
    render(RemoveDeviceDialog, { props: props({ isSelf: true, isSeedHolder: true }) });
    expect(screen.getByText(/holds your master key/i)).toBeInTheDocument();
  });

  it('hides the seed-holder warning for sibling removals', () => {
    render(RemoveDeviceDialog, { props: props() });
    expect(screen.queryByText(/holds your master key/i)).toBeNull();
  });

  it('states what is NOT severed (honesty rule)', () => {
    render(RemoveDeviceDialog, { props: props() });
    expect(screen.getByText(/not blocked yet/i)).toBeInTheDocument();
  });

  it('maps notMaster errors to friendly copy', () => {
    render(RemoveDeviceDialog, { props: props({ error: 'notMaster: nope' }) });
    expect(screen.getByText(/only the device holding your master key/i)).toBeInTheDocument();
  });

  it('maps lastDevice errors to friendly copy', () => {
    render(RemoveDeviceDialog, { props: props({ error: 'lastDevice: nope' }) });
    expect(screen.getByText(/only active device/i)).toBeInTheDocument();
  });

  it('disables confirm while busy even when the name matches', async () => {
    const p = props({ busy: true });
    render(RemoveDeviceDialog, { props: p });
    await fireEvent.input(screen.getByRole('textbox'), { target: { value: 'Study Mac' } });
    expect(screen.getByRole('button', { name: /remove device/i })).toBeDisabled();
  });

  it('cancel invokes onCancel', async () => {
    const p = props();
    render(RemoveDeviceDialog, { props: p });
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(p.onCancel).toHaveBeenCalled();
  });
});
