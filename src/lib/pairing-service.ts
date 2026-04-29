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
  | { kind: 'enrolling' }
  | { kind: 'complete'; deviceIdHex: string }
  | { kind: 'failed'; reason: string };

export class PairingService {
  state: PairingState = { kind: 'idle' };
  onChange?: () => void;
  private unlistener: (() => void) | null = null;

  async init(): Promise<void> {
    // Listener-first pattern: if init() fetched the snapshot before
    // registering the listener, any backend transition that landed during
    // that window would be lost (the wizard would skip past Discovering →
    // Discovered, etc). Subscribe first, capture whether any event landed
    // during the snapshot fetch, then only apply the snapshot if no event
    // arrived — otherwise the live event payload is authoritative.
    let sawEvent = false;
    this.unlistener = await listen<PairingState>('pairing-state-changed', (event) => {
      sawEvent = true;
      this.state = event.payload;
      this.onChange?.();
    });

    const snapshot = await invoke<PairingState>('get_pairing_state');
    if (!sawEvent) {
      this.state = snapshot;
      this.onChange?.();
    }
  }

  dispose(): void {
    this.unlistener?.();
    this.unlistener = null;
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
