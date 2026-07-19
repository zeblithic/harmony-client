// ZEB-359 — voice device picker settings section.
import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, screen, cleanup, fireEvent, waitFor } from '@testing-library/svelte';
import VoiceDeviceSettings from '../VoiceDeviceSettings.svelte';

afterEach(cleanup);

function makeService(opts: {
  input?: string | null;
  output?: string | null;
  inputs?: { deviceId: string; label: string }[];
  outputs?: { deviceId: string; label: string }[];
  supportsOutput?: boolean;
} = {}) {
  let input = opts.input ?? null;
  let output = opts.output ?? null;
  const subs = new Set<() => void>();
  const svc = {
    getInput: () => input,
    getOutput: () => output,
    setInput: vi.fn((id: string | null) => {
      input = id;
    }),
    setOutput: vi.fn((id: string | null) => {
      output = id;
    }),
    listDevices: vi.fn(async () => ({
      inputs: opts.inputs ?? [],
      outputs: opts.outputs ?? [],
    })),
    supportsOutputSelection: () => opts.supportsOutput ?? true,
    subscribe: (cb: () => void) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },
  };
  return { svc, fire: () => [...subs].forEach((cb) => cb()), subCount: () => subs.size };
}

describe('VoiceDeviceSettings', () => {
  it('lists devices with a System default first and reflects the saved choice', async () => {
    const { svc } = makeService({
      input: 'm2',
      inputs: [
        { deviceId: 'm1', label: 'Built-in Mic' },
        { deviceId: 'm2', label: 'USB Headset' },
      ],
      outputs: [{ deviceId: 's1', label: 'Speakers' }],
    });
    render(VoiceDeviceSettings, { audioDevices: svc as never });
    const inputSel = (await screen.findByLabelText('Microphone')) as HTMLSelectElement;
    await waitFor(() => expect(inputSel.options.length).toBe(3));
    expect(inputSel.options[0].textContent).toContain('System default');
    expect(inputSel.value).toBe('m2');
  });

  it('selecting a microphone persists via setInput; System default persists null', async () => {
    const { svc } = makeService({
      inputs: [{ deviceId: 'm1', label: 'Built-in Mic' }],
    });
    render(VoiceDeviceSettings, { audioDevices: svc as never });
    const inputSel = (await screen.findByLabelText('Microphone')) as HTMLSelectElement;
    await waitFor(() => expect(inputSel.options.length).toBe(2));
    await fireEvent.change(inputSel, { target: { value: 'm1' } });
    expect(svc.setInput).toHaveBeenCalledWith('m1');
    await fireEvent.change(inputSel, { target: { value: '' } });
    expect(svc.setInput).toHaveBeenCalledWith(null);
  });

  it('disables the output picker with a note when setSinkId is unsupported', async () => {
    const { svc } = makeService({ supportsOutput: false });
    render(VoiceDeviceSettings, { audioDevices: svc as never });
    const outSel = (await screen.findByLabelText('Speaker')) as HTMLSelectElement;
    expect(outSel.disabled).toBe(true);
    expect(
      screen.getByText(/output selection is not supported/i),
    ).toBeTruthy();
  });

  it('keeps an unplugged saved device selectable (marked unavailable)', async () => {
    const { svc } = makeService({
      input: 'gone-mic',
      inputs: [{ deviceId: 'm1', label: 'Built-in Mic' }],
    });
    render(VoiceDeviceSettings, { audioDevices: svc as never });
    const inputSel = (await screen.findByLabelText('Microphone')) as HTMLSelectElement;
    await waitFor(() => expect(inputSel.options.length).toBe(3));
    expect(inputSel.value).toBe('gone-mic');
    const labels = [...inputSel.options].map((o) => o.textContent ?? '');
    expect(labels.some((l) => /unavailable/i.test(l))).toBe(true);
  });

  it('re-enumerates on service change events and unsubscribes on destroy', async () => {
    const h = makeService({ inputs: [{ deviceId: 'm1', label: 'Mic' }] });
    const { unmount } = render(VoiceDeviceSettings, { audioDevices: h.svc as never });
    await screen.findByLabelText('Microphone');
    const callsBefore = h.svc.listDevices.mock.calls.length;
    h.fire();
    await waitFor(() =>
      expect(h.svc.listDevices.mock.calls.length).toBeGreaterThan(callsBefore),
    );
    unmount();
    expect(h.subCount()).toBe(0);
  });
});
