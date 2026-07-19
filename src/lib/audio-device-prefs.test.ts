// ZEB-359 — audio device preference service tests.

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { AudioDevicePrefs } from './audio-device-prefs';

interface StoredMap {
  [k: string]: string;
}

function makeStorage(seed: StoredMap = {}) {
  const map: StoredMap = { ...seed };
  return {
    map,
    getItem: (k: string) => (k in map ? map[k] : null),
    setItem: (k: string, v: string) => {
      map[k] = v;
    },
    removeItem: (k: string) => {
      delete map[k];
    },
  };
}

type DeviceChangeCb = () => void;

function makeMedia(devices: Partial<MediaDeviceInfo>[] = []) {
  const listeners = new Set<DeviceChangeCb>();
  return {
    listeners,
    enumerateDevices: vi.fn(async () => devices as MediaDeviceInfo[]),
    addEventListener: vi.fn((_t: string, cb: DeviceChangeCb) => {
      listeners.add(cb);
    }),
    removeEventListener: vi.fn((_t: string, cb: DeviceChangeCb) => {
      listeners.delete(cb);
    }),
    fireDeviceChange() {
      for (const cb of listeners) cb();
    },
  };
}

describe('AudioDevicePrefs', () => {
  let storage: ReturnType<typeof makeStorage>;

  beforeEach(() => {
    storage = makeStorage();
  });

  it('defaults to system default (null) for both directions', () => {
    const p = new AudioDevicePrefs({ storage, media: null });
    expect(p.getInput()).toBeNull();
    expect(p.getOutput()).toBeNull();
  });

  it('persists selections and re-reads them in a fresh instance', () => {
    const p = new AudioDevicePrefs({ storage, media: null });
    p.setInput('mic-abc');
    p.setOutput('spk-xyz');
    const q = new AudioDevicePrefs({ storage, media: null });
    expect(q.getInput()).toBe('mic-abc');
    expect(q.getOutput()).toBe('spk-xyz');
  });

  it('null selection clears back to system default and persists that', () => {
    const p = new AudioDevicePrefs({ storage, media: null });
    p.setInput('mic-abc');
    p.setInput(null);
    const q = new AudioDevicePrefs({ storage, media: null });
    expect(q.getInput()).toBeNull();
  });

  it('tolerates corrupted stored JSON (falls back to defaults)', () => {
    storage.map['harmony-voice-devices'] = '{not json';
    const p = new AudioDevicePrefs({ storage, media: null });
    expect(p.getInput()).toBeNull();
    expect(p.getOutput()).toBeNull();
  });

  it('tolerates a throwing storage (private mode / quota) without throwing', () => {
    const throwing = {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('quota');
      },
      removeItem: () => {
        throw new Error('denied');
      },
    };
    const p = new AudioDevicePrefs({ storage: throwing, media: null });
    expect(p.getInput()).toBeNull();
    expect(() => p.setInput('mic')).not.toThrow();
  });

  it('notifies subscribers on pref changes; unsubscribe stops notifications', () => {
    const p = new AudioDevicePrefs({ storage, media: null });
    const cb = vi.fn();
    const un = p.subscribe(cb);
    p.setInput('mic-1');
    p.setOutput('spk-1');
    expect(cb).toHaveBeenCalledTimes(2);
    un();
    p.setInput('mic-2');
    expect(cb).toHaveBeenCalledTimes(2);
  });

  it('a throwing subscriber does not block the others nor the setter (PR #495 R1)', () => {
    const p = new AudioDevicePrefs({ storage, media: null });
    const bad = vi.fn(() => {
      throw new Error('subscriber exploded');
    });
    const good = vi.fn();
    p.subscribe(bad);
    p.subscribe(good);
    expect(() => p.setInput('mic-1')).not.toThrow();
    expect(bad).toHaveBeenCalledTimes(1);
    expect(good).toHaveBeenCalledTimes(1);
    // The pref still landed.
    expect(p.getInput()).toBe('mic-1');
  });

  it('does not notify when setting the same value again', () => {
    const p = new AudioDevicePrefs({ storage, media: null });
    p.setInput('mic-1');
    const cb = vi.fn();
    p.subscribe(cb);
    p.setInput('mic-1');
    expect(cb).not.toHaveBeenCalled();
  });

  it('rebroadcasts devicechange to subscribers and detaches on destroy', () => {
    const media = makeMedia();
    const p = new AudioDevicePrefs({ storage, media });
    const cb = vi.fn();
    p.subscribe(cb);
    expect(media.addEventListener).toHaveBeenCalledWith(
      'devicechange',
      expect.any(Function),
    );
    media.fireDeviceChange();
    expect(cb).toHaveBeenCalledTimes(1);
    p.destroy();
    media.fireDeviceChange();
    expect(cb).toHaveBeenCalledTimes(1);
  });

  it('listDevices filters audio kinds and applies generic label fallbacks', async () => {
    const media = makeMedia([
      { kind: 'audioinput', deviceId: 'm1', label: 'Blue Yeti' },
      { kind: 'audioinput', deviceId: 'm2', label: '' },
      { kind: 'videoinput', deviceId: 'cam', label: 'FaceTime HD' },
      { kind: 'audiooutput', deviceId: 's1', label: '' },
    ]);
    const p = new AudioDevicePrefs({ storage, media });
    const set = await p.listDevices();
    expect(set.inputs).toEqual([
      { deviceId: 'm1', label: 'Blue Yeti' },
      { deviceId: 'm2', label: 'Microphone 2' },
    ]);
    expect(set.outputs).toEqual([{ deviceId: 's1', label: 'Speaker 1' }]);
  });

  it('listDevices returns empty sets when enumeration is unavailable', async () => {
    const p = new AudioDevicePrefs({ storage, media: null });
    const set = await p.listDevices();
    expect(set.inputs).toEqual([]);
    expect(set.outputs).toEqual([]);
  });

  it('supportsOutputSelection reflects the injected detector', () => {
    const yes = new AudioDevicePrefs({
      storage,
      media: null,
      detectOutputSelection: () => true,
    });
    const no = new AudioDevicePrefs({
      storage,
      media: null,
      detectOutputSelection: () => false,
    });
    expect(yes.supportsOutputSelection()).toBe(true);
    expect(no.supportsOutputSelection()).toBe(false);
  });
});
