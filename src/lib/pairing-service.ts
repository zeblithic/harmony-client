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
    this.state = (await invoke<PairingState>('get_pairing_state'));
    this.unlistener = await listen('pairing-state-changed', (event) => {
      this.state = event.payload as PairingState;
      this.onChange?.();
    });
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

export function extractError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
