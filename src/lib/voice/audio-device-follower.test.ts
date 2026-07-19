// ZEB-359 — audio-device follower tests: the shared "react to device-pref /
// hot-plug changes while media is live" logic used by both VoiceSession and
// CallSession.

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { followAudioDevices } from './audio-device-follower';

function flush(): Promise<void> {
  return new Promise((r) => setTimeout(r, 0));
}

function makeHarness() {
  let input: string | null = null;
  let output: string | null = null;
  let inputs: { deviceId: string; label: string }[] = [];
  const subscribers = new Set<() => void>();

  const prefs = {
    getInput: () => input,
    getOutput: () => output,
    listDevices: vi.fn(async () => ({ inputs, outputs: [] })),
    subscribe: (cb: () => void) => {
      subscribers.add(cb);
      return () => subscribers.delete(cb);
    },
  };
  const sender = { switchInputDevice: vi.fn(async () => {}) };
  const mixer = { setOutputDevice: vi.fn(async () => {}) };
  const onMicError = vi.fn();

  return {
    prefs,
    sender,
    mixer,
    onMicError,
    setInput(id: string | null) {
      input = id;
      for (const cb of subscribers) cb();
    },
    setOutput(id: string | null) {
      output = id;
      for (const cb of subscribers) cb();
    },
    setDevices(ids: string[]) {
      inputs = ids.map((deviceId, i) => ({ deviceId, label: `Mic ${i}` }));
    },
    fireDeviceChange() {
      for (const cb of subscribers) cb();
    },
    follow(overrides: { sender?: unknown; mixer?: unknown } = {}) {
      return followAudioDevices({
        prefs,
        getSender: () =>
          ('sender' in overrides ? overrides.sender : sender) as never,
        getMixer: () => ('mixer' in overrides ? overrides.mixer : mixer) as never,
        onMicError,
      });
    },
  };
}

describe('followAudioDevices', () => {
  let h: ReturnType<typeof makeHarness>;

  beforeEach(() => {
    h = makeHarness();
  });

  it('routes an output pref change to the live mixer', async () => {
    const stop = h.follow();
    h.setOutput('spk-2');
    await flush();
    expect(h.mixer.setOutputDevice).toHaveBeenCalledWith('spk-2');
    stop();
  });

  it('does not touch the mixer when the output pref is unchanged', async () => {
    const stop = h.follow();
    h.fireDeviceChange();
    await flush();
    expect(h.mixer.setOutputDevice).not.toHaveBeenCalled();
    stop();
  });

  it('restarts capture when the input pref changes', async () => {
    const stop = h.follow();
    h.setDevices(['mic-b']);
    h.setInput('mic-b');
    await flush();
    expect(h.sender.switchInputDevice).toHaveBeenCalledTimes(1);
    stop();
  });

  it('ignores a devicechange that does not affect the preferred input', async () => {
    h.setDevices(['mic-a', 'mic-b']);
    h.setInput('mic-a');
    const stop = h.follow();
    await flush();
    h.setDevices(['mic-a', 'mic-b', 'webcam-mic']);
    h.fireDeviceChange();
    await flush();
    expect(h.sender.switchInputDevice).not.toHaveBeenCalled();
    stop();
  });

  it('restarts capture when the preferred input is unplugged (fallback to default)', async () => {
    h.setDevices(['mic-a']);
    h.setInput('mic-a');
    const stop = h.follow();
    await flush(); // baseline presence recorded
    h.setDevices([]);
    h.fireDeviceChange();
    await flush();
    expect(h.sender.switchInputDevice).toHaveBeenCalledTimes(1);
    stop();
  });

  it('restarts capture again when the preferred input is replugged', async () => {
    h.setDevices(['mic-a']);
    h.setInput('mic-a');
    const stop = h.follow();
    await flush();
    h.setDevices([]);
    h.fireDeviceChange();
    await flush();
    h.setDevices(['mic-a']);
    h.fireDeviceChange();
    await flush();
    expect(h.sender.switchInputDevice).toHaveBeenCalledTimes(2);
    stop();
  });

  it('reports a failed capture restart via onMicError instead of throwing', async () => {
    h.sender.switchInputDevice.mockRejectedValueOnce(new Error('mic gone'));
    const stop = h.follow();
    h.setDevices(['mic-b']);
    h.setInput('mic-b');
    await flush();
    expect(h.onMicError).toHaveBeenCalledTimes(1);
    stop();
  });

  it('tolerates a missing sender (listen-only session)', async () => {
    const stop = h.follow({ sender: null });
    h.setDevices(['mic-b']);
    h.setInput('mic-b');
    await flush();
    expect(h.onMicError).not.toHaveBeenCalled();
    stop();
  });

  it('stops reacting after unfollow', async () => {
    const stop = h.follow();
    stop();
    h.setOutput('spk-9');
    h.setDevices(['mic-b']);
    h.setInput('mic-b');
    await flush();
    expect(h.mixer.setOutputDevice).not.toHaveBeenCalled();
    expect(h.sender.switchInputDevice).not.toHaveBeenCalled();
  });
});
