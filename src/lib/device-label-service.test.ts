import { describe, it, expect, vi, beforeEach } from 'vitest';

// Mock plugin-os so resolveDefaultDeviceLabel doesn't hit a real Tauri runtime.
// (The service uses a dynamic import; vi.mock intercepts both static + dynamic.)
vi.mock('@tauri-apps/plugin-os', () => ({ hostname: vi.fn() }));
import { hostname } from '@tauri-apps/plugin-os';
import {
  loadDeviceLabel,
  saveDeviceLabel,
  resolveDefaultDeviceLabel,
} from './device-label-service';

const mockedHostname = hostname as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
  localStorage.clear();
  vi.resetAllMocks();
});

describe('device-label-service — ZEB-336', () => {
  it('round-trips a saved label', () => {
    saveDeviceLabel('KRILE');
    expect(loadDeviceLabel()).toBe('KRILE');
  });

  it('returns null when no label is stored', () => {
    expect(loadDeviceLabel()).toBeNull();
  });

  it('ignores empty / whitespace-only labels', () => {
    saveDeviceLabel('   ');
    expect(loadDeviceLabel()).toBeNull();
  });

  it('trims the label on save', () => {
    saveDeviceLabel('  KOYA  ');
    expect(loadDeviceLabel()).toBe('KOYA');
  });

  it('defaults to the OS hostname when available', async () => {
    mockedHostname.mockResolvedValue('KOYA-MBP');
    expect(await resolveDefaultDeviceLabel()).toBe('KOYA-MBP');
  });

  it('falls back to "This device" when hostname is null', async () => {
    mockedHostname.mockResolvedValue(null);
    expect(await resolveDefaultDeviceLabel()).toBe('This device');
  });

  it('falls back to "This device" when hostname() rejects', async () => {
    mockedHostname.mockRejectedValue(new Error('os:allow-hostname not granted'));
    expect(await resolveDefaultDeviceLabel()).toBe('This device');
  });
});

describe('clearDeviceLabel (ZEB-668 S4, PR #454 round 1)', () => {
  it('removes the stored label so the ladder cannot resurrect it', async () => {
    const { saveDeviceLabel, loadDeviceLabel, clearDeviceLabel } = await import(
      './device-label-service'
    );
    saveDeviceLabel('KRILE');
    expect(loadDeviceLabel()).toBe('KRILE');
    clearDeviceLabel();
    expect(loadDeviceLabel()).toBeNull();
  });
});
