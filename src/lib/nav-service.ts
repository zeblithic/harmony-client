import type { TauriAdapter } from './zenoh-service';
import type { NavNode, Profile } from './types';
import { navNodes as mockNavNodes, profileStore as mockProfileStore } from './mock-data';

/** Wire format for profile updates from the Rust backend. */
export interface ProfileUpdateEvent {
  address: string;
  displayName: string;
  statusText?: string;
  avatarUrl?: string;
}

/**
 * Manages navigation tree state and peer profile lookups.
 *
 * Seeds with mock data on construction. When connected via Tauri adapter,
 * listens for `profile-update` events to keep the profile map live, and
 * `nav-updated` events (future) to receive nav tree changes from the backend.
 */
export class NavService {
  nodes: NavNode[] = [];
  profiles: Map<string, Profile> = new Map();
  /** Called whenever nodes or profiles change so the UI can re-render. */
  onChange?: () => void;
  /** Hex-encoded own address — profile updates matching this are filtered. */
  ownAddress: string | null = null;

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];

  constructor() {
    this.nodes = [...mockNavNodes];
    this.profiles = new Map(mockProfileStore);
  }

  /** Connect a Tauri adapter and start listening for profile + nav updates. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    this.adapter = adapter;

    const unlistenProfile = await adapter.listen(
      'profile-update',
      (event) => {
        const wire = event.payload as ProfileUpdateEvent;
        // Filter own profile echoes
        if (this.ownAddress && wire.address === this.ownAddress) return;
        this.profiles.set(wire.address, {
          address: wire.address,
          displayName: wire.displayName,
          statusText: wire.statusText,
          avatarCid: wire.avatarUrl,
        });
        // Update DM node names to match latest displayName
        this.nodes = this.nodes.map((n) => {
          if (n.peer?.address === wire.address && n.name !== wire.displayName) {
            return { ...n, name: wire.displayName, peer: { ...n.peer, displayName: wire.displayName } };
          }
          return n;
        });
        this.onChange?.();
      },
    );
    this.unlisteners.push(unlistenProfile);
  }

  /** Look up a peer's status text by address. */
  profileLookup(address: string): string | undefined {
    return this.profiles.get(address)?.statusText;
  }

  /** Look up a full profile by address (for popovers). */
  getProfile(address: string): Profile | undefined {
    return this.profiles.get(address);
  }

  addUnlisten(fn: () => void): void {
    this.unlisteners.push(fn);
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
  }
}
