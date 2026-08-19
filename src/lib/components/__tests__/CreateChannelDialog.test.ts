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

  it('submit invokes createChannel with name + writePower=0 + kind=text (v2 default)', async () => {
    const { getByPlaceholderText, getByRole, adapter, props } = await setupDialog();
    (adapter.invoke as any).mockResolvedValue('cc'.repeat(16));
    const input = getByPlaceholderText(/Channel name/i) as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'announcements' } });
    await fireEvent.click(getByRole('button', { name: /^Create/i }));
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('create_channel', {
        communityId: 'aa'.repeat(16),
        name: 'announcements',
        writePower: 0,
        kind: 'text',
      });
    });
    expect(props.onCreated).toHaveBeenCalledWith('cc'.repeat(16));
    expect(props.onClose).toHaveBeenCalled();
  });

  it('creates a voice channel when Voice is selected', async () => {
    const { getByPlaceholderText, getByRole, adapter } = await setupDialog();
    (adapter.invoke as any).mockResolvedValue('dd'.repeat(16));
    await fireEvent.click(getByRole('button', { name: /voice/i }));
    await fireEvent.input(getByPlaceholderText(/Channel name/i), {
      target: { value: 'hangout' },
    });
    await fireEvent.click(getByRole('button', { name: /^Create/i }));
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('create_channel', {
        communityId: 'aa'.repeat(16),
        name: 'hangout',
        writePower: 0,
        kind: 'voice',
      });
    });
  });

  it('creates a townhall channel when Town Hall is selected (ZEB-612)', async () => {
    const { getByPlaceholderText, getByRole, adapter } = await setupDialog();
    (adapter.invoke as any).mockResolvedValue('ff'.repeat(16));
    await fireEvent.click(getByRole('button', { name: /town hall/i }));
    await fireEvent.input(getByPlaceholderText(/Channel name/i), {
      target: { value: 'assembly' },
    });
    await fireEvent.click(getByRole('button', { name: /^Create/i }));
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('create_channel', {
        communityId: 'aa'.repeat(16),
        name: 'assembly',
        writePower: 0,
        kind: 'townhall',
      });
    });
  });

  it('defaults to a text channel', async () => {
    const { getByPlaceholderText, getByRole, adapter } = await setupDialog();
    (adapter.invoke as any).mockResolvedValue('ee'.repeat(16));
    await fireEvent.input(getByPlaceholderText(/Channel name/i), {
      target: { value: 'general' },
    });
    await fireEvent.click(getByRole('button', { name: /^Create/i }));
    await waitFor(() => {
      expect(adapter.invoke).toHaveBeenCalledWith('create_channel', {
        communityId: 'aa'.repeat(16),
        name: 'general',
        writePower: 0,
        kind: 'text',
      });
    });
  });

  it('Voice/Text toggle reflects selection via aria-pressed', async () => {
    const { getByRole } = await setupDialog();
    const textBtn = getByRole('button', { name: /text/i });
    const voiceBtn = getByRole('button', { name: /voice/i });
    expect(textBtn.getAttribute('aria-pressed')).toBe('true');
    expect(voiceBtn.getAttribute('aria-pressed')).toBe('false');
    await fireEvent.click(voiceBtn);
    expect(textBtn.getAttribute('aria-pressed')).toBe('false');
    expect(voiceBtn.getAttribute('aria-pressed')).toBe('true');
  });

  it('resets kind to text when reopened after selecting Voice (mounted instance)', async () => {
    const { getByRole, rerender, props } = await setupDialog();
    await fireEvent.click(getByRole('button', { name: /voice/i }));
    expect(getByRole('button', { name: /voice/i }).getAttribute('aria-pressed')).toBe('true');
    // The dialog instance stays mounted; only `open` toggles. Closing then
    // reopening must reset the kind selection (regression: Qodo "kind not reset on cancel").
    await rerender({ ...props, open: false });
    await rerender({ ...props, open: true });
    expect(getByRole('button', { name: /text/i }).getAttribute('aria-pressed')).toBe('true');
    expect(getByRole('button', { name: /voice/i }).getAttribute('aria-pressed')).toBe('false');
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

  // ZEB-965 (CodeRabbit #716): the demotion gate must honor a community-
  // customized kick threshold (ZEB-733 backend parity), not the global const.
  it('stays open at power ≥ a LOWERED community kick threshold (below the global 50)', async () => {
    const { props, getByPlaceholderText } = await setupDialog({ myPower: 40, kickThreshold: 30 });
    expect(getByPlaceholderText(/Channel name/i)).toBeTruthy();
    expect(props.onClose).not.toHaveBeenCalled();
  });

  it('auto-closes below a RAISED community kick threshold even at power ≥ the global 50', async () => {
    const { props } = await setupDialog({ myPower: 60, kickThreshold: 75 });
    await waitFor(() => {
      expect(props.onClose).toHaveBeenCalled();
    });
  });

  it('keeps the v2 write-power control present but hidden (ZEB-517)', async () => {
    const { getByLabelText } = await setupDialog();
    // The slider + number-input pair must exist from day one (slider-pairing
    // rule) so v3 can reveal it without re-adding markup...
    const slider = getByLabelText('Write-power threshold slider');
    const numberInput = getByLabelText('Write-power threshold');
    expect(slider).toBeTruthy();
    expect(numberInput).toBeTruthy();
    // ...but in v2 the whole row stays hidden. ZEB-517: a bare
    // `.control-row { display: flex }` rule defeats the UA `[hidden]`
    // stylesheet, so `hidden` is made authoritative via
    // `.control-row[hidden] { display: none }`. Guard that the row keeps the
    // `hidden` attribute — removing it (the v3 trigger) would re-leak the
    // control to v2 users.
    const row = slider.closest('.control-row') as HTMLElement | null;
    expect(row).not.toBeNull();
    expect(row?.hasAttribute('hidden')).toBe(true);
  });
});
