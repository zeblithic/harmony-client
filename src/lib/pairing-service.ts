import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

export type PairingRole = 'inviter' | 'joiner';

export interface DiscoveredPeer {
  sessionId: string;
  role: PairingRole;
  displayName: string;
  ownerIdIfInviter: string | null;
  ephemeralPubkeyHex: string;
  joinerEd25519VerifyHex: string | null;
  seenAtUnix: number;
}

export type PairingState =
  | { kind: 'idle' }
  | { kind: 'discovering'; role: PairingRole; ephemeralPubkeyHex: string; sessionId: string }
  | { kind: 'discovered'; peers: DiscoveredPeer[] }
  | { kind: 'handshaking'; peerSessionId: string; sasDigits: string }
  | { kind: 'waitingPeerConfirm'; peerSessionId: string }
  | { kind: 'awaitingQuorumCosign' }
  | { kind: 'enrolling' }
  | { kind: 'complete'; deviceIdHex: string }
  | { kind: 'failed'; reason: string };

export class PairingService {
  state: PairingState = { kind: 'idle' };
  onChange?: () => void;
  private unlistener: (() => void) | null = null;

  async init(): Promise<void> {
    // Idempotent on re-init: drop any previous subscription before
    // registering a new one. Without this, calling init() twice would
    // overwrite this.unlistener and leak the prior subscription.
    this.unlistener?.();
    this.unlistener = null;

    // Listener-first pattern: if init() fetched the snapshot before
    // registering the listener, any backend transition that landed during
    // that window would be lost (the wizard would skip past Discovering →
    // Discovered, etc). Subscribe first, capture whether any event landed
    // during the snapshot fetch, then only apply the snapshot if no event
    // arrived — otherwise the live event payload is authoritative.
    let sawEvent = false;
    const unlisten = await listen<PairingState>('pairing-state-changed', (event) => {
      sawEvent = true;
      this.state = event.payload;
      this.onChange?.();
    });
    this.unlistener = unlisten;

    // If the snapshot fetch throws, the listener we just registered would
    // leak — Tauri keeps it active until the page reloads. Roll back on
    // error so dispose() / next init() are still well-defined.
    try {
      const snapshot = await invoke<PairingState>('get_pairing_state');
      if (!sawEvent) {
        this.state = snapshot;
        this.onChange?.();
      }
    } catch (err) {
      unlisten();
      this.unlistener = null;
      throw err;
    }
  }

  dispose(): void {
    this.unlistener?.();
    this.unlistener = null;
  }

  /**
   * ZEB-668 S6: re-fetch the backend snapshot. On Complete this is more
   * than a read — `get_pairing_state_inner` folds the freshly-persisted
   * enrollment from `owner_state.cbor` into the resident trust doc
   * (ZEB-668 S1), which is what the Devices panel renders from while the
   * node runs. Calling this at completion is what makes the new device
   * visible (and petname-addressable by row lookup) without a restart.
   */
  async refreshSnapshot(): Promise<void> {
    this.state = await invoke<PairingState>('get_pairing_state');
    this.onChange?.();
  }

  async startInviter(displayName: string): Promise<void> {
    await invoke('start_inviter_pairing', { displayName });
  }

  async startJoiner(displayName: string): Promise<void> {
    await invoke('start_joiner_pairing', { displayName });
  }

  async selectPeer(peerSessionId: string): Promise<void> {
    await invoke('select_pairing_peer', { peerSessionId });
  }

  async confirmSas(): Promise<void> {
    await invoke('confirm_pairing_sas');
  }

  async cancel(): Promise<void> {
    await invoke('cancel_pairing');
  }
}

// Re-export the canonical extractError from owner-service so wizards
// can keep importing it from this module without a divergent copy. PR #63
// review: a duplicated helper would silently drift from the original
// (e.g. when the Tauri error format changes).
export { extractError } from './owner-service';
