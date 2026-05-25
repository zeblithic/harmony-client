// ZEB-329 — Tauri IPC wrappers + event subscriber + pure helpers.

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  NetworkHealthSnapshot,
  SelfTestReport,
  NatClass,
} from './types/network-health';

const EVENT_NAME = 'network-health-changed';

export async function snapshot(): Promise<NetworkHealthSnapshot> {
  return await invoke<NetworkHealthSnapshot>('network_health_snapshot');
}

export async function runSelfTest(): Promise<SelfTestReport> {
  return await invoke<SelfTestReport>('network_health_run_self_test');
}

export async function exportPayload(includeFullIds: boolean): Promise<string> {
  return await invoke<string>('network_health_export_payload', {
    includeFullIds,
  });
}

export async function onNetworkHealthChanged(
  cb: () => void
): Promise<UnlistenFn> {
  return await listen<unknown>(EVENT_NAME, () => cb());
}

// Pure helpers (testable in isolation)

export function explainNatClass(n: NatClass): { headline: string; detail: string } {
  switch (n) {
    case 'fullCone':
      return {
        headline: 'Direct connections work',
        detail: 'Open NAT — peers can connect to you directly. Best speed.',
      };
    case 'restrictedCone':
      return {
        headline: 'Direct connections mostly work',
        detail:
          'Restricted-cone NAT — peers you contact first can reach you back; new inbound is blocked until you initiate.',
      };
    case 'portRestricted':
      return {
        headline: 'Some direct connections work',
        detail:
          'Port-restricted NAT — direct connections work only with peers you contact first, and only on the exact port pair.',
      };
    case 'symmetric':
      return {
        headline: 'Direct connections do not work',
        detail:
          'Symmetric NAT — every peer needs to go through the relay. Slower but functional.',
      };
    case 'unknown':
      return {
        headline: 'Network type not yet determined',
        detail:
          'Harmony is still measuring your network. Connection mode for peers tells the real story.',
      };
  }
}

export function redactAddr(addr: string, full: boolean): string {
  if (!addr || addr.length < 8) return '(unknown)';
  if (full) return addr;
  return `${addr.slice(0, 8)}…`;
}
