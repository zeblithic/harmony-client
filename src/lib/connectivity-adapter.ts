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
  DiscoveredRecord,
  PkarrPublicationStatus,
  RedemptionOutcome,
  ResolutionProgressEvent,
  InvitePreviewDto,
} from './types/connectivity';
import type { RelayHealth, IrohRelayInfo } from './types/network-health';
import type { RedeemInviteResultDto } from './community-service';

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
  try {
    return await listen<ConnectivityReachabilityChangedPayload>(
      'connectivity-reachability-changed',
      (event) => callback(event.payload),
    );
  } catch (e) {
    // Match the error-normalization pattern used by the three IPC
    // helpers above so the panel's error surface displays a consistent
    // "connectivity-reachability-changed: ..." prefix (CodeRabbit
    // PR #157 round 1).
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity-reachability-changed: ${msg}`);
  }
}

// ---------------------------------------------------------------------------
// ZEB-323 Phase 2b: pkarr-backed discovery IPCs + events
// ---------------------------------------------------------------------------

/**
 * Attempt to redeem an invite via the cross-WAN iroh/pkarr path.
 *
 * Resolves with a `RedemptionOutcome` indicating whether the join succeeded,
 * the inviter was unreachable, or the call should fall back to Reticulum.
 * Rejects (throws) only on hard IPC-layer failures (process bridge broken,
 * etc.); expected soft failures are encoded in the outcome status instead.
 */
export async function redeemInviteIroh(inviteUrl: string): Promise<RedemptionOutcome> {
  try {
    return await invoke<RedemptionOutcome>('connectivity_redeem_invite_iroh', { inviteUrl });
  } catch (e) {
    // ZEB-885: the command rejects with a structured { code, message }. Preserve
    // it unchanged so the dialog can route its copy off the code; wrapping it in
    // a new Error here would stringify the object to "[object Object]" and drop
    // the code. Require a *string* `code` (matching toRedeemInviteError's shape
    // check) so a malformed object with a non-string code still takes the
    // context-wrapping path below rather than passing through to a useless
    // "[object Object]" message.
    if (e && typeof e === 'object' && typeof (e as { code?: unknown }).code === 'string') {
      throw e;
    }
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_redeem_invite_iroh: ${msg}`);
  }
}

/**
 * ZEB-650 slice 3: pure-local preview of a pasted invite URL — decode +
 * token-signature verify + expiry evaluation. Mints nothing, joins nothing,
 * no network; safe to call on every keystroke settle.
 */
export async function previewInvite(url: string): Promise<InvitePreviewDto> {
  try {
    return await invoke<InvitePreviewDto>('preview_invite', { url });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`preview_invite: ${msg}`);
  }
}

/**
 * Join an OPEN community over the cross-WAN iroh first-contact path.
 *
 * Resolves with a `RedeemInviteResultDto` whose optional `status` field
 * (camelCase wire key `status`, omitted when null) drives the UI:
 *   - `"joined"`    → the join completed; render the normal joined success.
 *   - `"searching"` → RETRYABLE cold-start: no beacon was reachable yet; the
 *                     node keeps retrying on its transport-epoch re-arm. Show a
 *                     non-blocking "still searching" state, NOT an error.
 *   - `"rejected"`  → the beacon explicitly rejected the join (banned / bad
 *                     capability). A distinct blocked state, NOT the spinner.
 *   - (field absent) → legacy/local redeem; render exactly as the LAN path.
 *
 * Rejects (throws) only on hard IPC-layer failures; the soft cold-start and
 * rejection outcomes are encoded in `status` rather than thrown.
 */
export async function openJoinIroh(inviteUrl: string): Promise<RedeemInviteResultDto> {
  try {
    return await invoke<RedeemInviteResultDto>('connectivity_open_join_iroh', { inviteUrl });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_open_join_iroh: ${msg}`);
  }
}

/**
 * Toggle case-B identity-keyed discoverability.
 *
 * When `enabled` is `true`, this device publishes its iroh routing to the
 * pkarr DHT under a key derived from the owner identity public key. Anyone
 * holding the identity address can then find this device cross-WAN. Persisted
 * across restarts via `connectivity-settings.json`.
 */
export async function setIdentityDiscoverable(enabled: boolean): Promise<void> {
  try {
    await invoke('connectivity_set_identity_discoverable', { enabled });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_set_identity_discoverable: ${msg}`);
  }
}

/**
 * Read the persisted case-B discoverability setting.
 *
 * Returns `false` (default) when the setting file has never been written.
 */
export async function getIdentityDiscoverable(): Promise<boolean> {
  try {
    return await invoke<boolean>('connectivity_get_identity_discoverable');
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_get_identity_discoverable: ${msg}`);
  }
}

/**
 * ZEB-600: set community-presence visibility.
 *
 * When `visible` is `false` ("appear offline"), this node stops publishing its
 * signed presence beacons, so no community sees it as online — while it keeps
 * receiving everyone else's beacons (privacy-complete invisible mode). The live
 * gate flips on the running node's next heartbeat and is persisted to
 * `connectivity-settings.json`, fail-closed to invisible on a corrupt file.
 */
export async function setPresenceVisibility(visible: boolean): Promise<void> {
  try {
    await invoke('set_presence_visibility', { visible });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`set_presence_visibility: ${msg}`);
  }
}

/**
 * ZEB-600: read the current presence visibility (`true` = visible/online).
 *
 * Prefers the live gate on a running node; falls back to the persisted setting
 * when no node is running yet, so the settings toggle can seed pre-connect.
 */
export async function getPresenceVisibility(): Promise<boolean> {
  try {
    return await invoke<boolean>('get_presence_visibility');
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`get_presence_visibility: ${msg}`);
  }
}

/**
 * Query the pkarr DHT for a peer's current iroh routing record given its
 * 64-byte identity public key as a lowercase hex string.
 *
 * Returns `null` when no valid record is found (peer offline or not
 * discoverable). Throws on hard resolution errors.
 */
export async function discoverIdentity(identityPubHex: string): Promise<DiscoveredRecord | null> {
  try {
    return await invoke<DiscoveredRecord | null>('connectivity_discover_identity', {
      identityPubHex,
    });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_discover_identity: ${msg}`);
  }
}

/**
 * Returns a snapshot of active pkarr publication handles by case.
 *
 * `inviteCount` — number of pending invite publications (case A).
 * `identityActive` — whether case-B identity-keyed publishing is live.
 * `communityCount` — number of per-community publications (case C).
 */
export async function pkarrPublicationStatus(): Promise<PkarrPublicationStatus> {
  try {
    return await invoke<PkarrPublicationStatus>('connectivity_pkarr_publication_status');
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`connectivity_pkarr_publication_status: ${msg}`);
  }
}

// ---------------------------------------------------------------------------
// ZEB-323 Phase 2b: event subscribers
// ---------------------------------------------------------------------------

/**
 * Subscribe to `connectivity-invite-resolution-progress` events.
 *
 * The backend emits these during an iroh invite-redemption attempt to let the
 * UI display stage-by-stage progress. Returns a teardown function — call it
 * from `onDestroy` to unregister the listener.
 *
 * Uses the same destroyed-flag pattern as Phase 1's `onReachabilityChanged`
 * to guard against the mount/unmount race window.
 */
export function onResolutionProgress(
  cb: (ev: ResolutionProgressEvent) => void,
): () => void {
  let unlisten: UnlistenFn | undefined;
  let destroyed = false;
  listen<ResolutionProgressEvent>('connectivity-invite-resolution-progress', (e) =>
    cb(e.payload),
  )
    .then((u) => {
      if (destroyed) {
        u();
      } else {
        unlisten = u;
      }
    })
    .catch((e) =>
      console.error(
        'connectivity-invite-resolution-progress listen failed:',
        e instanceof Error ? e.message : String(e),
      ),
    );
  return () => {
    destroyed = true;
    unlisten?.();
  };
}

/**
 * Subscribe to `connectivity-identity-discoverable-changed` events.
 *
 * Fired whenever `connectivity_set_identity_discoverable` IPC completes so
 * that any other panel showing the toggle can sync its state without polling.
 * Returns a teardown function.
 */
export function onIdentityDiscoverableChanged(cb: (enabled: boolean) => void): () => void {
  let unlisten: UnlistenFn | undefined;
  let destroyed = false;
  listen<{ enabled: boolean }>('connectivity-identity-discoverable-changed', (e) =>
    cb(e.payload.enabled),
  )
    .then((u) => {
      if (destroyed) {
        u();
      } else {
        unlisten = u;
      }
    })
    .catch((e) =>
      console.error(
        'connectivity-identity-discoverable-changed listen failed:',
        e instanceof Error ? e.message : String(e),
      ),
    );
  return () => {
    destroyed = true;
    unlisten?.();
  };
}

/**
 * ZEB-600: subscribe to `presence-visibility-changed` events.
 *
 * Fired whenever `set_presence_visibility` completes so any panel showing the
 * viewer's own presence (the "Appear offline" toggle, the member-list self-dot)
 * stays in sync without polling. Returns a teardown function.
 */
export function onPresenceVisibilityChanged(cb: (visible: boolean) => void): () => void {
  let unlisten: UnlistenFn | undefined;
  let destroyed = false;
  listen<{ visible: boolean }>('presence-visibility-changed', (e) => cb(e.payload.visible))
    .then((u) => {
      if (destroyed) {
        u();
      } else {
        unlisten = u;
      }
    })
    .catch((e) =>
      console.error(
        'presence-visibility-changed listen failed:',
        e instanceof Error ? e.message : String(e),
      ),
    );
  return () => {
    destroyed = true;
    unlisten?.();
  };
}

// ---------------------------------------------------------------------------
// ZEB-380: pkarr relay pool management IPCs
// ---------------------------------------------------------------------------

/**
 * Returns the current pkarr relay pool with per-relay health snapshots.
 *
 * Each entry is a `RelayHealth` DTO (url + state + lastOutcome + lastSuccessMs).
 */
export async function getPkarrRelays(): Promise<RelayHealth[]> {
  try {
    return await invoke<RelayHealth[]>('get_pkarr_relays');
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`get_pkarr_relays: ${msg}`);
  }
}

/**
 * Replaces the live pkarr relay pool with the provided URL list.
 *
 * The backend validates, persists, and hot-swaps the pool. On success it
 * emits `connectivity-relays-changed` so any listener can re-fetch. Throws
 * with a descriptive error string on invalid input (bad scheme, duplicate
 * URLs, exceeds cap of 8, etc.).
 */
export async function setPkarrRelays(relays: string[]): Promise<RelayHealth[]> {
  try {
    // Returns the NEW authoritative list (same shape as getPkarrRelays), so the
    // caller updates its view from the result with no separate refetch.
    return await invoke<RelayHealth[]>('set_pkarr_relays', { relays });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`set_pkarr_relays: ${msg}`);
  }
}

/**
 * Resets the pkarr relay pool to the backend's recommended default set
 * (`default_pkarr_relays()`), persisted + hot-swapped live. Server-authoritative —
 * the frontend does not need to know the default URL list.
 */
export async function resetPkarrRelays(): Promise<RelayHealth[]> {
  try {
    // Returns the NEW authoritative list (the recommended defaults), so the
    // caller updates its view from the result with no separate refetch.
    return await invoke<RelayHealth[]>('reset_pkarr_relays');
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`reset_pkarr_relays: ${msg}`);
  }
}

/**
 * ZEB-380: add a single relay to the persisted pool (server-authoritative
 * read-modify-write). The backend appends `url` to the CURRENT persisted list
 * and re-validates, so a stale client view can never clobber a fresher pool.
 */
export async function addPkarrRelay(url: string): Promise<RelayHealth[]> {
  try {
    // Returns the NEW authoritative list (same shape as getPkarrRelays), so the
    // caller updates its view from the result with no separate refetch.
    return await invoke<RelayHealth[]>('add_pkarr_relay', { url });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`add_pkarr_relay: ${msg}`);
  }
}

/**
 * ZEB-380: remove a single relay from the persisted pool (server-authoritative
 * read-modify-write). The backend filters the URL from the CURRENT persisted
 * list and re-validates; removing the last relay is rejected server-side.
 */
export async function removePkarrRelay(url: string): Promise<RelayHealth[]> {
  try {
    // Returns the NEW authoritative list (same shape as getPkarrRelays), so the
    // caller updates its view from the result with no separate refetch.
    return await invoke<RelayHealth[]>('remove_pkarr_relay', { url });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`remove_pkarr_relay: ${msg}`);
  }
}

// ---------------------------------------------------------------------------
// ZEB-624: iroh transport relay configuration IPCs
// ---------------------------------------------------------------------------
//
// Distinct from the pkarr relay pool above: the iroh wire carries no per-relay
// health, so every verb returns an `IrohRelayInfo { relays, custom }`.
// `custom === false` means the node is following iroh's recommended defaults
// (the returned `relays` shows them); `true` means a materialized custom list.
// The backend emits `iroh-relays-changed` after every successful mutation.

/**
 * Returns the current iroh transport relay configuration: the effective relay
 * URL list plus whether it is a user-materialized custom list (`custom`) or
 * iroh's recommended defaults (`!custom`).
 */
export async function getIrohRelays(): Promise<IrohRelayInfo> {
  try {
    return await invoke<IrohRelayInfo>('get_iroh_relays');
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`get_iroh_relays: ${msg}`);
  }
}

/**
 * Replaces the iroh relay configuration with the provided URL list. The backend
 * validates, persists, and hot-swaps the transport relay map, then emits
 * `iroh-relays-changed`. Returns the NEW authoritative `IrohRelayInfo` so the
 * caller updates its view from the result with no separate refetch.
 */
export async function setIrohRelays(relays: string[]): Promise<IrohRelayInfo> {
  try {
    return await invoke<IrohRelayInfo>('set_iroh_relays', { relays });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`set_iroh_relays: ${msg}`);
  }
}

/**
 * Adds a single relay to the iroh relay configuration (server-authoritative
 * read-modify-write). Adding to a defaults-following node materializes a custom
 * list. Returns the NEW authoritative `IrohRelayInfo`.
 */
export async function addIrohRelay(url: string): Promise<IrohRelayInfo> {
  try {
    return await invoke<IrohRelayInfo>('add_iroh_relay', { url });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`add_iroh_relay: ${msg}`);
  }
}

/**
 * Removes a single relay from the iroh relay configuration (server-authoritative
 * read-modify-write). Removing the last custom relay is rejected server-side
 * (the error tells the user to reset to recommended). Returns the NEW
 * authoritative `IrohRelayInfo`.
 */
export async function removeIrohRelay(url: string): Promise<IrohRelayInfo> {
  try {
    return await invoke<IrohRelayInfo>('remove_iroh_relay', { url });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`remove_iroh_relay: ${msg}`);
  }
}

/**
 * Resets the iroh relay configuration back to iroh's recommended defaults,
 * persisted + hot-swapped live. Returns the NEW authoritative `IrohRelayInfo`
 * (with `custom === false`).
 */
export async function resetIrohRelays(): Promise<IrohRelayInfo> {
  try {
    return await invoke<IrohRelayInfo>('reset_iroh_relays');
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`reset_iroh_relays: ${msg}`);
  }
}

/**
 * Subscribe to `connectivity-pkarr-fallback-fired` events.
 *
 * Emitted by the backend whenever `ReachabilityResolver::resolve_async`
 * falls back to a pkarr DHT lookup (case C). Each event carries:
 *   - `peerAddrShort` — first 12 hex chars of the peer's OwnerAddr
 *   - `communityId`   — hex community id for which the lookup was attempted
 *   - `hit`           — whether the lookup returned a valid record
 *
 * Returns a teardown function.
 */
export function onPkarrFallbackFired(
  cb: (ev: { peerAddrShort: string; communityId: string; hit: boolean }) => void,
): () => void {
  let unlisten: UnlistenFn | undefined;
  let destroyed = false;
  listen<{ peerAddrShort: string; communityId: string; hit: boolean }>(
    'connectivity-pkarr-fallback-fired',
    (e) => cb(e.payload),
  )
    .then((u) => {
      if (destroyed) {
        u();
      } else {
        unlisten = u;
      }
    })
    .catch((e) =>
      console.error(
        'connectivity-pkarr-fallback-fired listen failed:',
        e instanceof Error ? e.message : String(e),
      ),
    );
  return () => {
    destroyed = true;
    unlisten?.();
  };
}
