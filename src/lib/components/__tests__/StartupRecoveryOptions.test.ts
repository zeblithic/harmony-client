import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import StartupRecoveryOptions from '../StartupRecoveryOptions.svelte';

// The child OwnerRestoreWizard imports `invoke` directly from the Tauri core;
// mock it so mounting the wizard (restore mode) never reaches a real IPC. The
// component under test uses the INJECTED `invoke` prop, not this — but the
// default binding resolves here too.
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

/** An `invoke` that resolves per command; records calls for assertions. */
function makeInvoke(overrides: Record<string, unknown> = {}) {
  return vi.fn((cmd: string) => {
    if (cmd in overrides) {
      const v = overrides[cmd];
      return v instanceof Error ? Promise.reject(v) : Promise.resolve(v);
    }
    return Promise.resolve(null);
  });
}

beforeEach(() => {
  vi.clearAllMocks();
});

describe('StartupRecoveryOptions (ZEB-835 / ZEB-836)', () => {
  it('starts collapsed and reveals both remedies behind "Still stuck?"', async () => {
    const { getByTestId, queryByTestId } = render(StartupRecoveryOptions, {
      props: { invoke: makeInvoke(), reload: vi.fn() },
    });

    // Collapsed: only the quiet disclosure link — no reset/restore surfaced.
    expect(getByTestId('startup-still-stuck')).toBeTruthy();
    expect(queryByTestId('startup-recovery-options')).toBeNull();

    await fireEvent.click(getByTestId('startup-still-stuck'));
    expect(getByTestId('startup-recovery-options')).toBeTruthy();
    expect(getByTestId('startup-restore')).toBeTruthy();
    expect(getByTestId('startup-reset')).toBeTruthy();
  });

  it('reset is gated: no invoke until the confirm checkbox is checked', async () => {
    const invoke = makeInvoke();
    const reload = vi.fn();
    const { getByTestId } = render(StartupRecoveryOptions, { props: { invoke, reload } });

    await fireEvent.click(getByTestId('startup-still-stuck'));
    await fireEvent.click(getByTestId('startup-reset'));

    // Confirm gate shown; go-button disabled; clicking it while unchecked is a no-op.
    const go = getByTestId('startup-reset-go') as HTMLButtonElement;
    expect(go.disabled).toBe(true);
    await fireEvent.click(go);
    expect(invoke).not.toHaveBeenCalled();
    expect(reload).not.toHaveBeenCalled();
  });

  it('confirmed reset invokes reset_local_identity then reloads into onboarding', async () => {
    const invoke = makeInvoke({ reset_local_identity: '/home/u/.harmony/_reset-backup-1700' });
    const reload = vi.fn();
    const { getByTestId } = render(StartupRecoveryOptions, { props: { invoke, reload } });

    await fireEvent.click(getByTestId('startup-still-stuck'));
    await fireEvent.click(getByTestId('startup-reset'));
    await fireEvent.click(getByTestId('startup-reset-confirm'));

    const go = getByTestId('startup-reset-go') as HTMLButtonElement;
    expect(go.disabled).toBe(false);
    await fireEvent.click(go);

    expect(invoke).toHaveBeenCalledWith('reset_local_identity');
    expect(reload).toHaveBeenCalledTimes(1);
  });

  it('a failed reset surfaces the error and does NOT reload', async () => {
    const invoke = makeInvoke({ reset_local_identity: new Error('disk full') });
    const reload = vi.fn();
    const { getByTestId } = render(StartupRecoveryOptions, { props: { invoke, reload } });

    await fireEvent.click(getByTestId('startup-still-stuck'));
    await fireEvent.click(getByTestId('startup-reset'));
    await fireEvent.click(getByTestId('startup-reset-confirm'));
    await fireEvent.click(getByTestId('startup-reset-go'));

    expect(reload).not.toHaveBeenCalled();
    expect(getByTestId('startup-reset-error').textContent).toContain('disk full');
  });

  it('restore reads the on-disk owner-id (for force overwrite) and opens the wizard', async () => {
    const invoke = makeInvoke({ owner_id_on_disk: 'aa0b1838deadbeef' });
    const { getByTestId } = render(StartupRecoveryOptions, {
      props: { invoke, reload: vi.fn() },
    });

    await fireEvent.click(getByTestId('startup-still-stuck'));
    await fireEvent.click(getByTestId('startup-restore'));

    // owner_id_on_disk is consulted so a same-owner phrase force-overwrites the
    // broken owner_state.cbor rather than being refused by the overwrite guard.
    expect(invoke).toHaveBeenCalledWith('owner_id_on_disk');
    // The wizard is now mounted (its recovery-phrase textarea resolves).
    expect(getByTestId('owner-restore-words')).toBeTruthy();
  });
});
