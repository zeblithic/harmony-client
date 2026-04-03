import type { TauriAdapter } from './zenoh-service';

/**
 * Resolves avatar CIDs to displayable blob URLs via content transport.
 *
 * Fetches raw bytes from the Zenoh mesh using the `fetch_content` Tauri
 * command and creates object URLs for rendering in <img> tags. Results
 * are cached so each CID is fetched at most once.
 */
export class AvatarResolver {
  /** Called when a new CID has been resolved so the UI can re-render. */
  onChange?: () => void;

  private adapter: TauriAdapter | null = null;
  private cache = new Map<string, string>();
  private pending = new Set<string>();
  private failed = new Set<string>();

  connectAdapter(adapter: TauriAdapter): void {
    this.adapter = adapter;
  }

  /** Return the resolved blob URL for a CID, or undefined if not yet resolved.
   *  Automatically kicks off a fetch if the CID hasn't been seen before. */
  resolve(cid: string): string | undefined {
    const cached = this.cache.get(cid);
    if (cached) return cached;
    if (!this.pending.has(cid) && !this.failed.has(cid)) {
      this.fetchCid(cid);
    }
    return undefined;
  }

  private async fetchCid(cid: string): Promise<void> {
    if (!this.adapter) return;
    this.pending.add(cid);
    try {
      const bytes = (await this.adapter.invoke('fetch_content', { cid })) as number[];
      const blob = new Blob([new Uint8Array(bytes)]);
      const url = URL.createObjectURL(blob);
      this.cache.set(cid, url);
      this.onChange?.();
    } catch (err) {
      console.warn(`Avatar fetch failed for CID ${cid}:`, err);
      this.failed.add(cid);
    } finally {
      this.pending.delete(cid);
    }
  }

  destroy(): void {
    for (const url of this.cache.values()) {
      URL.revokeObjectURL(url);
    }
    this.cache.clear();
    this.pending.clear();
    this.failed.clear();
  }
}
