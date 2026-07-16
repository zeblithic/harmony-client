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

  it('states the feed, DM, and friend/PEX cutoffs (honesty rule)', () => {
    render(RemoveDeviceDialog, { props: props() });
    // ZEB-685 shipped the DM-only cutoff; ZEB-680 shipped the friend/PEX cutoff.
    expect(screen.getByText(/stop accepting new posts/i)).toBeInTheDocument();
    expect(screen.getByText(/direct[\s\S]*messages also stop being accepted/i)).toBeInTheDocument();
    expect(
      screen.getByText(
        /can no longer add a new friend, introduce itself, or vouch for an introduction/i,
      ),
    ).toBeInTheDocument();
    // The stale "lands in follow-up work" clause is gone now that ZEB-685 shipped.
    expect(screen.queryByText(/follow-up work/i)).toBeNull();
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

  it('gates Cancel behind busy (mid-flight close would strand the next confirm)', async () => {
    const p = props({ busy: true });
    render(RemoveDeviceDialog, { props: p });
    const cancel = screen.getByRole('button', { name: /cancel/i });
    expect(cancel).toBeDisabled();
    await fireEvent.click(cancel);
    expect(p.onCancel).not.toHaveBeenCalled();
  });

  it('cancel invokes onCancel', async () => {
    const p = props();
    render(RemoveDeviceDialog, { props: p });
    await fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(p.onCancel).toHaveBeenCalled();
  });
});

describe('RemoveDeviceDialog replace mode (ZEB-668 S6)', () => {
  it('renders the replace title, hides reason radios, and locks decommissioned', async () => {
    const p = props({ mode: 'replace' });
    render(RemoveDeviceDialog, { props: p });
    expect(screen.getByText('Replace Study Mac?')).toBeInTheDocument();
    expect(screen.queryAllByRole('radio')).toHaveLength(0);
    const confirm = screen.getByRole('button', { name: /remove & continue/i });
    expect(confirm).toBeDisabled();
    await fireEvent.input(screen.getByRole('textbox'), { target: { value: 'Study Mac' } });
    await fireEvent.click(confirm);
    expect(p.onConfirm).toHaveBeenCalledWith('decommissioned');
  });

  it('states removal-first semantics and that the master key is unchanged (§7 scope note)', () => {
    render(RemoveDeviceDialog, { props: props({ mode: 'replace' }) });
    expect(screen.getByText(/removed first|removes .* first/i)).toBeInTheDocument();
    expect(screen.getByText(/even if you cancel pairing/i)).toBeInTheDocument();
    expect(screen.getByText(/owner identity and master key are unchanged/i)).toBeInTheDocument();
  });

  it('keeps the honesty copy in replace mode', () => {
    render(RemoveDeviceDialog, { props: props({ mode: 'replace' }) });
    expect(screen.getByText(/stop accepting new posts/i)).toBeInTheDocument();
    expect(
      screen.getByText(
        /can no longer add a new friend, introduce itself, or vouch for an introduction/i,
      ),
    ).toBeInTheDocument();
  });

  it('still requires the exact device name in replace mode', async () => {
    const p = props({ mode: 'replace' });
    render(RemoveDeviceDialog, { props: p });
    await fireEvent.input(screen.getByRole('textbox'), { target: { value: 'wrong' } });
    expect(screen.getByRole('button', { name: /remove & continue/i })).toBeDisabled();
    expect(p.onConfirm).not.toHaveBeenCalled();
  });
});

// ── ZEB-677 S3: quorum (master-less co-sign) removal mode ─────────────────────

describe('RemoveDeviceDialog — quorum mode (ZEB-677 S3)', () => {
  it('shows the co-sign copy and Request removal label when quorum is set', async () => {
    const p = props({ quorum: true });
    render(RemoveDeviceDialog, { props: p });
    expect(screen.getByTestId('remove-quorum-copy').textContent).toMatch(/asked to co-sign/i);
    const confirm = screen.getByRole('button', { name: /request removal/i });
    expect(confirm).toBeDisabled();
    // Typed-confirm tier unchanged.
    await fireEvent.input(screen.getByRole('textbox'), { target: { value: 'Study Mac' } });
    expect(confirm).toBeEnabled();
    await fireEvent.click(confirm);
    expect(p.onConfirm).toHaveBeenCalledWith('decommissioned');
  });

  it('hides the co-sign copy and keeps the direct label without quorum', () => {
    render(RemoveDeviceDialog, { props: props() });
    expect(screen.queryByTestId('remove-quorum-copy')).toBeNull();
    expect(screen.getByRole('button', { name: /remove device/i })).toBeInTheDocument();
  });

  it('maps quorum backend prefixes to friendly copy', () => {
    render(
      RemoveDeviceDialog,
      { props: props({ quorum: true, error: 'noQuorum: no other active device' }) },
    );
    expect(screen.getByRole('alert').textContent).toMatch(/needs your recovery phrase/i);
  });
});
