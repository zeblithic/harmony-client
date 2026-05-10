import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import CreateChannelDialog from '../CreateChannelDialog.svelte';
import { CommunityService } from '../../community-service';
import type { TauriAdapter } from '../../zenoh-service';

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

async function setupDialog(overrides: Record<string, unknown> = {}) {
  const adapter = makeAdapter();
  const service = new CommunityService();
  await service.connectAdapter(adapter);
  const onClose = vi.fn();
  const onCreated = vi.fn();
  const props = {
    communityId: 'aa'.repeat(16),
    communityService: service,
    open: true,
    myPower: 100,
    onClose,
    onCreated,
    ...overrides,
  };
  const renderResult = render(CreateChannelDialog, { props });
  return { adapter, service, props, ...renderResult };
}

describe('CreateChannelDialog', () => {
  it('renders nothing when open=false', async () => {
    const { container } = await setupDialog({ open: false });
    expect(container.querySelector('[role="dialog"]')).toBeNull();
  });

  it('renders the form when open=true', async () => {
    const { getByPlaceholderText } = await setupDialog();
    expect(getByPlaceholderText(/Channel name/i)).toBeTruthy();
  });

  it('Create button disabled while name is empty', async () => {
    const { getByRole } = await setupDialog();
    const submit = getByRole('button', { name: /Create/i }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
  });

  it('Create button enabled when name has content', async () => {
    const { getByPlaceholderText, getByRole } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'general' } });
    const submit = getByRole('button', { name: /Create/i }) as HTMLButtonElement;
    expect(submit.disabled).toBe(false);
  });

  it('rejects names over 32 chars (button stays disabled)', async () => {
    const { getByPlaceholderText, getByRole } = await setupDialog();
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'a'.repeat(33) } });
    const submit = getByRole('button', { name: /Create/i }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
  });

  it('submit invokes createChannel with name + writePower=0 (v2 default)', async () => {
    const { getByPlaceholderText, getByRole, adapter, props } = await setupDialog();
    (adapter.invoke as any).mockResolvedValue('cc'.repeat(16));
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'announcements' } });
    await fireEvent.click(getByRole('button', { name: /Create/i }));
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('create_channel', {
        communityId: 'aa'.repeat(16),
        name: 'announcements',
        writePower: 0,
      });
    });
    expect(props.onCreated).toHaveBeenCalledWith('cc'.repeat(16));
    expect(props.onClose).toHaveBeenCalled();
  });

  it('shows inline error when createChannel rejects', async () => {
    const { getByPlaceholderText, getByRole, getByText, adapter } = await setupDialog();
    (adapter.invoke as any).mockRejectedValue(new Error('channel name is empty or exceeds 32 chars'));
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'whatever' } });
    await fireEvent.click(getByRole('button', { name: /Create/i }));
    await waitFor(() => {
      expect(getByText(/channel name is empty or exceeds 32 chars/i)).toBeTruthy();
    });
  });

  it('Cancel button calls onClose without dispatching IPC', async () => {
    const { getByText, props, adapter } = await setupDialog();
    await fireEvent.click(getByText('Cancel'));
    expect(props.onClose).toHaveBeenCalled();
    expect(adapter.invoke).not.toHaveBeenCalled();
  });

  it('auto-closes via onClose when myPower drops below 50', async () => {
    const { rerender, props } = await setupDialog();
    await rerender({ ...props, myPower: 25 });
    await waitFor(() => {
      expect(props.onClose).toHaveBeenCalled();
    });
  });
});
