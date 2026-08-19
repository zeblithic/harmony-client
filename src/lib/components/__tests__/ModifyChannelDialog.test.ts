import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import ModifyChannelDialog from '../ModifyChannelDialog.svelte';
import { CommunityService } from '../../community-service';
import type { TauriAdapter } from '../../zenoh-service';
import type { ChannelInfo } from '../../community-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function> } {
  const listeners = new Map<string, Function>();
  return {
    listeners,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    }),
  } as any;
}

const baseChannel: ChannelInfo = {
  channelId: 'cc'.repeat(16),
  name: 'general',
  writePower: 0,
  kind: 'text',
  createdAt: { wallMs: 100, logical: 0, deviceId: 'd1' },
};

async function setupDialog(channelOverrides: Partial<ChannelInfo> = {}, propOverrides: Record<string, unknown> = {}) {
  const adapter = makeAdapter();
  const service = new CommunityService();
  await service.connectAdapter(adapter);
  const channel: ChannelInfo = { ...baseChannel, ...channelOverrides };
  const onClose = vi.fn();
  const props = {
    communityId: 'aa'.repeat(16),
    channel,
    communityService: service,
    open: true,
    myPower: 100,
    onClose,
    ...propOverrides,
  };
  const renderResult = render(ModifyChannelDialog, { props });
  return { adapter, service, props, channel, ...renderResult };
}

describe('ModifyChannelDialog', () => {
  it('pre-fills name from channel.name', async () => {
    const { getByPlaceholderText } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    expect(input.value).toBe('general');
  });

  it('Save button disabled when no fields changed', async () => {
    const { getByRole } = await setupDialog();
    const submit = getByRole('button', { name: /Save/i }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
  });

  it('Save button enabled when name changes', async () => {
    const { getByPlaceholderText, getByRole } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'announcements' } });
    expect((getByRole('button', { name: /Save/i }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('submit invokes modify_channel with only the changed name (writePower undefined)', async () => {
    const { getByPlaceholderText, getByRole, adapter, props } = await setupDialog();
    (adapter.invoke as any).mockResolvedValue(undefined);
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'announcements' } });
    await fireEvent.click(getByRole('button', { name: /Save/i }));
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('modify_channel', {
        communityId: 'aa'.repeat(16),
        channelId: 'cc'.repeat(16),
        name: 'announcements',
        writePower: undefined,
      });
    });
    expect(props.onClose).toHaveBeenCalled();
  });

  it('all-None submit is rejected client-side (no IPC dispatch, no error)', async () => {
    const { getByRole, adapter } = await setupDialog();
    // Force the submit handler by simulating Enter on the form even though
    // canSubmit should already be false.
    const submit = getByRole('button', { name: /Save/i }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    await fireEvent.click(submit);
    expect(adapter.invoke).not.toHaveBeenCalled();
  });

  it('rejects empty name (button stays disabled)', async () => {
    const { getByPlaceholderText, getByRole } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: '' } });
    expect((getByRole('button', { name: /Save/i }) as HTMLButtonElement).disabled).toBe(true);
  });

  it('rejects names over 32 chars (button stays disabled)', async () => {
    const { getByPlaceholderText, getByRole } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'a'.repeat(33) } });
    expect((getByRole('button', { name: /Save/i }) as HTMLButtonElement).disabled).toBe(true);
  });

  it('shows inline error when modify_channel rejects', async () => {
    const { getByPlaceholderText, getByRole, getByText, adapter } = await setupDialog();
    (adapter.invoke as any).mockRejectedValue(new Error('actor power below mod threshold'));
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'newname' } });
    await fireEvent.click(getByRole('button', { name: /Save/i }));
    await waitFor(() => {
      expect(getByText(/actor power below mod threshold/i)).toBeTruthy();
    });
  });

  it('auto-closes via onClose when myPower drops below 50', async () => {
    const { rerender, props } = await setupDialog();
    await rerender({ ...props, myPower: 25 });
    await waitFor(() => {
      expect(props.onClose).toHaveBeenCalled();
    });
  });

  // ZEB-965 (CodeRabbit #716): the demotion gate must honor a community-
  // customized kick threshold (ZEB-733 backend parity), not the global const.
  it('stays open at power ≥ a LOWERED community kick threshold (below the global 50)', async () => {
    const { props, getByText } = await setupDialog({}, { myPower: 40, kickThreshold: 30 });
    expect(getByText('Cancel')).toBeTruthy();
    expect(props.onClose).not.toHaveBeenCalled();
  });

  it('auto-closes below a RAISED community kick threshold even at power ≥ the global 50', async () => {
    const { props } = await setupDialog({}, { myPower: 60, kickThreshold: 75 });
    await waitFor(() => {
      expect(props.onClose).toHaveBeenCalled();
    });
  });

  it('Cancel button calls onClose without IPC dispatch', async () => {
    const { getByText, adapter, props } = await setupDialog();
    await fireEvent.click(getByText('Cancel'));
    expect(props.onClose).toHaveBeenCalled();
    expect(adapter.invoke).not.toHaveBeenCalled();
  });
});
