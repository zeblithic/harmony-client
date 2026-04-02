import type { TauriAdapter } from './zenoh-service';
import type { VineVideo } from './types';
import { vineVideos as mockVines } from './mock-data';

/** Wire format for vine descriptors from the Rust backend. */
export interface VineDescriptorEvent {
  id: string;
  creatorAddress: string;
  creatorName: string;
  createdAt: number;
  videoCid: string;
  title?: string;
  reshareOf?: string;
}

/**
 * Manages real-time vine feed over Zenoh pub/sub.
 *
 * When connected, vine descriptors flow via Tauri IPC events (`vine-received`).
 * When disconnected (or in browser dev mode), seeds with mock data so the
 * UI is never empty. Call `connectAdapter()` to upgrade from offline to live.
 */
export class VineService {
  vines: VineVideo[] = [];
  /** Called whenever the vine list or viewed state changes so the UI can re-render. */
  onChange?: () => void;
  /** Hex-encoded node address — set after Zenoh connects so we can
   *  identify self-published vines in the echo. */
  ownAddress: string | null = null;
  /** Display name to include in outgoing vines. */
  ownDisplayName = 'You';
  /** Locally tracked viewed vine IDs. */
  viewedIds = new Set<string>();

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private seenIds = new Set<string>();

  constructor() {
    // Seed with mock data — real vines append on top.
    this.vines = [...mockVines];
    for (const v of this.vines) {
      this.seenIds.add(v.id);
      if (v.viewed) this.viewedIds.add(v.id);
    }
  }

  /** Connect a Tauri adapter and start listening for vine descriptors. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    this.adapter = adapter;
    const unlisten = await adapter.listen(
      'vine-received',
      (event) => {
        const wire = event.payload as VineDescriptorEvent;
        if (this.seenIds.has(wire.id)) return;
        this.seenIds.add(wire.id);
        const vine = this.wireToVine(wire);
        this.vines = [...this.vines, vine];
        this.onChange?.();
      },
    ) as unknown as () => void;
    this.unlisteners.push(unlisten);
  }

  /** Publish a vine via Tauri command. */
  async publish(
    videoCid: string,
    title?: string,
    reshareOf?: string,
  ): Promise<void> {
    if (this.adapter) {
      try {
        await this.adapter.invoke('publish_vine', {
          vine: { videoCid, title, reshareOf, creatorName: this.ownDisplayName },
        });
        return; // Backend will echo via subscription → vine-received event
      } catch (err: unknown) {
        const msg = err instanceof Error ? err.message : String(err);
        // Only fall back locally when genuinely disconnected; re-throw real errors.
        if (!msg.includes('not connected') && !msg.includes('event loop')) {
          throw err;
        }
      }
    }

    // Offline fallback: append locally so the UI stays responsive.
    const id = `vine-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
    this.seenIds.add(id);
    const vine: VineVideo = {
      id,
      creatorAddress: 'self',
      creatorName: this.ownDisplayName,
      createdAt: Math.floor(Date.now() / 1000),
      videoCid,
      title,
      reshareOf,
      viewed: true,
    };
    this.vines = [...this.vines, vine];
    this.onChange?.();
  }

  /** Mark a vine as viewed locally (and notify backend). */
  markViewed(id: string): void {
    if (this.viewedIds.has(id)) return;
    this.viewedIds = new Set([...this.viewedIds, id]);
    this.onChange?.();
    if (this.adapter) {
      this.adapter.invoke('mark_vine_viewed', { vineId: id }).catch(() => {});
    }
  }

  /** Convert wire format to frontend VineVideo type. */
  private wireToVine(wire: VineDescriptorEvent): VineVideo {
    // Self-published vines echo back via Zenoh — map to 'self'/ownDisplayName.
    const isSelf = this.ownAddress != null && wire.creatorAddress === this.ownAddress;

    return {
      id: wire.id,
      creatorAddress: isSelf ? 'self' : wire.creatorAddress,
      creatorName: isSelf
        ? this.ownDisplayName
        : wire.creatorName || wire.creatorAddress.slice(0, 8),
      createdAt: wire.createdAt,
      videoCid: wire.videoCid,
      title: wire.title,
      reshareOf: wire.reshareOf,
      viewed: false,
    };
  }

  /** Register an external unlisten handle (e.g. zenoh-status listener)
   *  so it gets cleaned up alongside the service. */
  addUnlisten(fn: () => void): void {
    this.unlisteners.push(fn);
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
  }
}
