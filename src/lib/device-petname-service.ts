/**
 * ZEB-668 S4 — fleet-synced device petname IPC wrapper.
 *
 * Thin wrapper around the `set_device_petname` Tauri command. LWW per
 * device: the newest write wins fleet-wide. Empty string clears. Unlike the
 * ZEB-336 device-label store (localStorage, this machine only), a petname
 * names ANY of the owner's devices and replicates to all of them.
 *
 * Error extraction follows the project memory rule (CLAUDE.md): production
 * rejections are strings; test environments may surface Error objects.
 */

import { invoke } from '@tauri-apps/api/core';

/** Max petname length (chars, post-trim) — mirrors the backend cap. */
export const MAX_DEVICE_PETNAME_CHARS = 64;

/**
 * Set or clear the petname for any of the owner's devices.
 *
 * @param deviceVkHex - 64-hex SP1 device ID (DeviceView.deviceVkHex).
 * @param petname - New name; empty string clears.
 * @throws on Tauri invoke failure (unknown device, over-cap, node not running).
 */
export async function setDevicePetname(deviceVkHex: string, petname: string): Promise<void> {
  await invoke('set_device_petname', { deviceVkHex, petname });
}
