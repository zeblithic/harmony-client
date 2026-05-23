/**
 * ZEB-321 Phase 1: Tauri IPC bindings for connectivity diagnostics.
 *
 * The three IPC commands and the one event subscriber here are the entire
 * frontend surface of Phase 1 — consumed by `DiagnosticsPanel.svelte` (dev
 * mode only). No production UI exposes these yet; Phase 2 will wire peer
 * reachability into the connection-bar status indicator.
 *
 * Error-extraction follows the project's memory rule
 * (`feedback_tauri_error_extraction`): production Tauri rejections are
 * plain strings, but vitest's `mockIPC` emits real `Error` objects, so each
 * helper normalizes both shapes into a single `Error` with a prefixed
 * IPC-name for caller-side identification.
 */
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  ReachabilityRecord,
  PeerReachability,
  ConnectivityReachabilityChangedPayload,
} from './types/connectivity';

/**
 * Returns this device's reachability snapshot, or `null` when the iroh
 * transport isn't running yet (boot failed, or pre-`start_node`).
 */
export async function getMyReachabilityRecord(): Promise<ReachabilityRecord | null> {
  try {
    return await invoke<ReachabilityRecord | null>(
      'connectivity_get_my_reachability_record',
    );
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_get_my_reachability_record: ${msg}`);
  }
}

/**
 * Returns a snapshot of all peer reachability entries known to this device's
 * LWW resolver. Empty array when the resolver hasn't been installed yet or
 * no peers have published.
 *
 * The DTO is `PeerReachability` (object), NOT a `[string, record]` tuple
 * — the Rust DTO is `PeerReachabilityDto { owner_address, record }` with
 * `#[serde(rename_all = "camelCase")]`, so the wire payload is an object
 * with `ownerAddress` + `record`.
 */
export async function listPeerReachability(): Promise<PeerReachability[]> {
  try {
    return await invoke<PeerReachability[]>('connectivity_list_peer_reachability');
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_list_peer_reachability: ${msg}`);
  }
}

/**
 * Wakes the publisher loop so it runs its publish callback immediately.
 *
 * Returns `true` when a notify was actually fired, `false` when no publisher
 * is running (iroh boot failed, or no owner identity loaded yet). Callers
 * can ignore the return value; tests assert on it to validate the IPC path
 * reached the publisher.
 */
export async function forceRepublish(): Promise<boolean> {
  try {
    return await invoke<boolean>('connectivity_force_republish');
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_force_republish: ${msg}`);
  }
}

/**
 * Subscribes to the `connectivity-reachability-changed` Tauri event.
 *
 * The callback is invoked with the deserialized payload (an object carrying
 * the `actor` hex string — see `ConnectivityReachabilityChangedPayload`).
 * Returns the `UnlistenFn` that must be called on teardown (typical pattern:
 * stash in a `let unlisten` and call from `onDestroy`).
 */
export async function onReachabilityChanged(
  callback: (payload: ConnectivityReachabilityChangedPayload) => void,
): Promise<UnlistenFn> {
  return await listen<ConnectivityReachabilityChangedPayload>(
    'connectivity-reachability-changed',
    (event) => callback(event.payload),
  );
}
