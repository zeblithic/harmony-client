/**
 * ZEB-418 P2 D17 — butler pin IPC wrapper.
 *
 * Thin wrapper around the `set_butler_pin` Tauri command. Single-select
 * semantics: pinning a device clears any previous pin server-side (LWW);
 * calling with `null` explicitly clears the pin.
 *
 * Error extraction follows the project memory rule (CLAUDE.md): production
 * rejections are strings; test environments may surface Error objects.
 */

import { invoke } from '@tauri-apps/api/core';

/**
 * Set or clear the pinned butler device.
 *
 * @param deviceId - 64-hex SP1 device ID to pin, or `null` to clear.
 * @throws on Tauri invoke failure (unknown device, node not running, etc.).
 */
export async function setButlerPin(deviceId: string | null): Promise<void> {
  await invoke('set_butler_pin', { deviceId });
}

/** Memory-rule-compliant error extraction for butler-pin rejections. */
export function extractButlerPinError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
