// ZEB-329 — Tauri IPC wrappers + event subscriber + pure helpers.

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  NetworkHealthSnapshot,
  SelfTestReport,
  NatClass,
  CommunityRelayHealth,
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

// ── ZEB-803: community-relay stall assessment ──────────────────────

/**
 * Serving cadence: the acceptor serves each peer roughly every
 * `COMMUNITY_RELAY_AD_REFRESH_MS` (~7m30s). Mirrored here rather than imported
 * because the Rust constant does not cross the IPC boundary.
 */
export const COMMUNITY_RELAY_SERVE_CADENCE_MS = 7.5 * 60 * 1000;

/**
 * How many missed cadences before we call it stalled. Three, not one: a single
 * miss is ordinary jitter (a peer offline, a slow pass), and the observed
 * incident ran 46 minutes — roughly six cadences — so three is comfortably
 * inside the real signal while staying outside the noise.
 */
export const COMMUNITY_RELAY_STALL_CADENCES = 3;

export type RelayDirectionVerdict =
  /** Wired and behaving. */
  | { state: 'ok' }
  /** Wired, but has never done the thing — normal on a fresh node. */
  | { state: 'idle'; detail: string }
  /** Wired, previously working, now silent past the threshold. */
  | { state: 'stalled'; detail: string; sinceMs: number };

/**
 * Assess the SERVING direction. Pure, so it is unit-testable without a node.
 *
 * `nowMs` is passed in rather than read from the clock: the caller already
 * holds `snapshot.capturedAtMs`, and using it keeps the verdict consistent with
 * the data it describes instead of drifting against a later wall-clock read.
 */
export function assessRelayServing(
  relay: CommunityRelayHealth,
  nowMs: number,
  cadenceMs: number = COMMUNITY_RELAY_SERVE_CADENCE_MS
): RelayDirectionVerdict {
  const { lastServedMs, pullsServed } = relay.serving;
  if (lastServedMs === null || lastServedMs === undefined) {
    // Never served anyone. On a node nobody relays through this is the correct
    // steady state, so it is NOT an alarm — but it is also not health, and
    // saying so is the difference between "quiet" and "broken".
    return {
      state: 'idle',
      detail: 'no peer has ever pulled from this node',
    };
  }
  const age = nowMs - lastServedMs;
  const threshold = cadenceMs * COMMUNITY_RELAY_STALL_CADENCES;
  if (age > threshold) {
    return {
      state: 'stalled',
      detail: `served ${pullsServed} pulls, then nothing for ${Math.round(age / 60000)} minutes`,
      sinceMs: age,
    };
  }
  return { state: 'ok' };
}

/**
 * Assess the PULLING direction.
 *
 * Ordering matters here. A dead pull loop is checked FIRST, because when the
 * loop is gone every other counter is frozen at whatever it last held — and a
 * frozen `sessionsOk` reads as "recently fine". Reporting the freshness of a
 * stopped clock is precisely the failure this whole section exists to prevent.
 */
export function assessRelayPulling(
  relay: CommunityRelayHealth,
  nowMs: number,
  cadenceMs: number = COMMUNITY_RELAY_SERVE_CADENCE_MS
): RelayDirectionVerdict {
  const { passesRun, lastPassMs, sessionsOk, sessionsFailed, passesNoRelay } =
    relay.pulling;
  const threshold = cadenceMs * COMMUNITY_RELAY_STALL_CADENCES;

  if (lastPassMs === null || lastPassMs === undefined) {
    return { state: 'idle', detail: 'the pull loop has not run a pass yet' };
  }
  const passAge = nowMs - lastPassMs;
  if (passAge > threshold) {
    // The loop itself stopped. This outranks every other signal.
    return {
      state: 'stalled',
      detail: `the pull loop stopped running (${passesRun} passes, last ${Math.round(passAge / 60000)} minutes ago)`,
      sinceMs: passAge,
    };
  }

  // Loop alive. Is it accomplishing anything?
  if (sessionsOk === 0 && (sessionsFailed > 0 || passesNoRelay > 0)) {
    // Distinguish the two causes, because the remedies are unrelated: a stale
    // or absent relay announce is a discovery problem, a failing session is a
    // transport problem.
    const detail =
      passesNoRelay > sessionsFailed
        ? `no fresh relay advertised for a joined community (${passesNoRelay} passes)`
        : `every pull session failed (${sessionsFailed})`;
    return { state: 'stalled', detail, sinceMs: passAge };
  }
  return { state: 'ok' };
}
