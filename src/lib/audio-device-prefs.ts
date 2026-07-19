/**
 * ZEB-359 — audio input/output device preferences.
 *
 * One service owns: the persisted device choice (localStorage, this device
 * only — `null` = system default), enumeration of available devices, change
 * notification (both pref edits and OS `devicechange` hot-plug events), and
 * output-selection capability detection.
 *
 * Capability note: playback runs through an AudioContext (voice-mixer.ts), so
 * output routing needs `AudioContext.setSinkId()` — present in Chromium
 * WebView2 (Windows), absent in WKWebView (macOS). Callers must treat
 * `supportsOutputSelection() === false` as "output follows the system default"
 * and keep the picker disabled rather than broken.
 *
 * Input selection needs no capability gate: the `deviceId: {ideal}` constraint
 * is universally supported and silently falls back to the OS default when the
 * preferred device is unplugged — exactly the fallback the ticket asks for.
 */

const STORAGE_KEY = 'harmony-voice-devices';

export interface AudioDeviceInfo {
  deviceId: string;
  label: string;
}

export interface AudioDeviceSet {
  inputs: AudioDeviceInfo[];
  outputs: AudioDeviceInfo[];
}

/** Subset of `Storage` used, injectable for tests. */
type StorageLike = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;

/** Subset of `MediaDevices` used, injectable for tests. */
export interface MediaDevicesLike {
  enumerateDevices(): Promise<MediaDeviceInfo[]>;
  addEventListener(type: 'devicechange', cb: () => void): void;
  removeEventListener(type: 'devicechange', cb: () => void): void;
}

interface StoredPrefs {
  input: string | null;
  output: string | null;
}

function defaultStorage(): StorageLike | null {
  try {
    return typeof localStorage !== 'undefined' ? localStorage : null;
  } catch {
    return null;
  }
}

function defaultMedia(): MediaDevicesLike | null {
  return typeof navigator !== 'undefined' && navigator.mediaDevices
    ? navigator.mediaDevices
    : null;
}

function defaultDetectOutputSelection(): boolean {
  return (
    typeof AudioContext !== 'undefined' && 'setSinkId' in AudioContext.prototype
  );
}

export interface AudioDevicePrefsOptions {
  storage?: StorageLike | null;
  media?: MediaDevicesLike | null;
  detectOutputSelection?: () => boolean;
}

export class AudioDevicePrefs {
  private storage: StorageLike | null;
  private media: MediaDevicesLike | null;
  private detect: () => boolean;
  private input: string | null = null;
  private output: string | null = null;
  private subscribers = new Set<() => void>();
  private deviceChangeCb: (() => void) | null = null;

  constructor(opts: AudioDevicePrefsOptions = {}) {
    this.storage = opts.storage !== undefined ? opts.storage : defaultStorage();
    this.media = opts.media !== undefined ? opts.media : defaultMedia();
    this.detect = opts.detectOutputSelection ?? defaultDetectOutputSelection;
    const stored = this.load();
    this.input = stored.input;
    this.output = stored.output;
  }

  /** Preferred capture device, or null for the system default. */
  getInput(): string | null {
    return this.input;
  }

  /** Preferred playback device, or null for the system default. */
  getOutput(): string | null {
    return this.output;
  }

  setInput(id: string | null): void {
    if (id === this.input) return;
    this.input = id;
    this.save();
    this.notify();
  }

  setOutput(id: string | null): void {
    if (id === this.output) return;
    this.output = id;
    this.save();
    this.notify();
  }

  /**
   * Notify `cb` on any pref change or OS device hot-plug. The `devicechange`
   * listener attaches lazily on first subscribe so a headless construction
   * (tests, early boot) costs nothing.
   */
  subscribe(cb: () => void): () => void {
    this.subscribers.add(cb);
    if (this.media && !this.deviceChangeCb) {
      this.deviceChangeCb = () => this.notify();
      this.media.addEventListener('devicechange', this.deviceChangeCb);
    }
    return () => {
      this.subscribers.delete(cb);
    };
  }

  /** Enumerate audio devices. Empty sets when enumeration is unavailable. */
  async listDevices(): Promise<AudioDeviceSet> {
    if (!this.media) return { inputs: [], outputs: [] };
    let all: MediaDeviceInfo[];
    try {
      all = await this.media.enumerateDevices();
    } catch {
      return { inputs: [], outputs: [] };
    }
    const inputs: AudioDeviceInfo[] = [];
    const outputs: AudioDeviceInfo[] = [];
    for (const d of all) {
      // Labels are blank until the user has granted mic permission (browser
      // privacy rule); fall back to a stable generic name so the picker stays
      // usable before the first voice join.
      if (d.kind === 'audioinput') {
        inputs.push({
          deviceId: d.deviceId,
          label: d.label || `Microphone ${inputs.length + 1}`,
        });
      } else if (d.kind === 'audiooutput') {
        outputs.push({
          deviceId: d.deviceId,
          label: d.label || `Speaker ${outputs.length + 1}`,
        });
      }
    }
    return { inputs, outputs };
  }

  /** Whether the platform can route AudioContext output (`setSinkId`). */
  supportsOutputSelection(): boolean {
    return this.detect();
  }

  destroy(): void {
    if (this.media && this.deviceChangeCb) {
      this.media.removeEventListener('devicechange', this.deviceChangeCb);
      this.deviceChangeCb = null;
    }
    this.subscribers.clear();
  }

  private load(): StoredPrefs {
    try {
      const raw = this.storage?.getItem(STORAGE_KEY);
      if (!raw) return { input: null, output: null };
      const parsed = JSON.parse(raw) as Partial<StoredPrefs>;
      return {
        input: typeof parsed.input === 'string' ? parsed.input : null,
        output: typeof parsed.output === 'string' ? parsed.output : null,
      };
    } catch {
      // Corrupt JSON / storage unavailable — system defaults, non-fatal.
      return { input: null, output: null };
    }
  }

  private save(): void {
    try {
      this.storage?.setItem(
        STORAGE_KEY,
        JSON.stringify({ input: this.input, output: this.output }),
      );
    } catch {
      // localStorage unavailable (private-mode quota) — non-fatal; the choice
      // still applies for this session.
    }
  }

  private notify(): void {
    for (const cb of [...this.subscribers]) cb();
  }
}
